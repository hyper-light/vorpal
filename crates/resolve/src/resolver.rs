//! The resolution algorithm (§3.3): scope-aware, confidence-scored, never faking edges.

use vorpal_kg::{EdgeType, NodeId};

use crate::reference::{RefForm, RefKind, Reference};
use crate::table::{Symbol, SymbolTable};

/// How sure a resolution is (0–100). Ordered so `<= AMBIGUOUS` flags approximate edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Confidence(pub u8);

impl Confidence {
  /// No definition is visible — unresolved.
  pub const NONE: Confidence = Confidence(0);
  /// Multiple candidates; the edge is approximate (labeled, not faked).
  pub const AMBIGUOUS: Confidence = Confidence(40);
  /// A single visible exported definition in another file.
  pub const CROSS_FILE: Confidence = Confidence(90);
  /// A single definition in the same file — the strongest binding.
  pub const LOCAL: Confidence = Confidence(100);
}

/// The outcome of resolving one reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
  /// The chosen definition, if any (approximate resolutions still pick a deterministic target).
  pub target: Option<NodeId>,
  pub edge: EdgeType,
  pub confidence: Confidence,
  /// How many definitions carried the referenced name (for transparency).
  pub candidates: usize,
}

/// A resolved reference ready to become a graph edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedEdge {
  pub from: NodeId,
  pub to: NodeId,
  pub edge: EdgeType,
  pub confidence: u8,
}

/// Counts from a resolution batch. `external + masked` is the total left without an edge.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResolveStats {
  /// Confidently resolved (single visible definition).
  pub resolved: u64,
  /// Resolved but approximate (multiple candidates; labeled edge).
  pub ambiguous: u64,
  /// No definition with this name exists in the indexed tree — the reference targets the
  /// standard library or a dependency outside the corpus. Honest, expected, and not a defect.
  pub external: u64,
  /// Definitions with the name exist, but none is safely attributable: not visible from the
  /// use site, or the reference is a member access whose name evidence is non-unique.
  pub masked: u64,
}

impl ResolveStats {
  /// References that produced no edge (external + masked).
  pub fn unresolved(&self) -> u64 {
    self.external + self.masked
  }
}

impl std::ops::AddAssign for ResolveStats {
  fn add_assign(&mut self, other: ResolveStats) {
    self.resolved += other.resolved;
    self.ambiguous += other.ambiguous;
    self.external += other.external;
    self.masked += other.masked;
  }
}

/// The reference resolver. Stateless today; a home for future config (per-language visibility).
#[derive(Debug, Default, Clone, Copy)]
pub struct Resolver;

impl Resolver {
  pub fn new() -> Self {
    Self
  }

  /// Resolve one reference against the table (§3.3):
  /// 1. path-form imports resolve against indexed file nodes (exact path match — cannot fake);
  /// 2. a grammar-provided qualifier (`Kg::load`, `self.helper()`) restricts candidates to
  ///    members of that owner (or, for static paths, definitions in the module of that name) —
  ///    and when the qualifier matches nothing, [`RefForm::Static`]/[`RefForm::Method`]
  ///    references never fall back to a blind multi-candidate guess;
  /// 3. same-file definitions win (local binding);
  /// 4. across files, exported definitions are visible — plus, for Rust, private definitions
  ///    in an ancestor module file (child modules see parent privates);
  /// 5. a single visible definition binds; multiple bind approximately only where the form
  ///    tolerates it (bare names); none is unresolved.
  pub fn resolve(&self, table: &SymbolTable, reference: &Reference) -> Resolution {
    let edge = reference.kind.edge();
    if reference.kind == RefKind::Import {
      if let Some(target) = resolve_import_path(table, reference) {
        return Resolution {
          target: Some(target),
          edge,
          confidence: Confidence::CROSS_FILE,
          candidates: 1,
        };
      }
    }
    let candidates = table.candidates(&reference.name);
    if candidates.is_empty() {
      return Resolution {
        target: None,
        edge,
        confidence: Confidence::NONE,
        candidates: 0,
      };
    }

    if let Some(qualifier) = reference.qualifier.as_deref() {
      let refined: Vec<&Symbol> = candidates
        .iter()
        .filter(|s| qualifier_matches(s, qualifier, reference.form))
        .collect();
      if !refined.is_empty() {
        // The qualifier corroborates these candidates; among them, a multi-way tie (e.g. two
        // `impl Kg` blocks defining `load`) is genuine ambiguity — labeled, tolerated.
        return finish(&refined, reference, edge, candidates.len(), true);
      }
      if reference.form != RefForm::Bare {
        // The grammar names an owner/namespace and nothing in the tree matches it: the target
        // is outside the corpus (e.g. `Vec::new`). Falling back to bare-name candidates would
        // fake an edge to a coincidentally-named definition.
        return Resolution {
          target: None,
          edge,
          confidence: Confidence::NONE,
          candidates: candidates.len(),
        };
      }
    }

    let all: Vec<&Symbol> = candidates.iter().collect();
    // Bare names may take a labeled approximate pick on a tie; member accesses on untyped
    // values carry no evidence beyond the name, so only a unique match binds.
    let guess_on_tie = reference.form != RefForm::Method;
    finish(&all, reference, edge, candidates.len(), guess_on_tie)
  }
}

/// Shared tail of resolution: local-first, then cross-file visibility, then pick.
fn finish(
  set: &[&Symbol],
  reference: &Reference,
  edge: EdgeType,
  candidates: usize,
  guess_on_tie: bool,
) -> Resolution {
  let local: Vec<&Symbol> = set
    .iter()
    .copied()
    .filter(|s| s.path == *reference.from_path)
    .collect();
  if !local.is_empty() {
    return pick(&local, edge, Confidence::LOCAL, candidates, guess_on_tie);
  }

  let visible: Vec<&Symbol> = set
    .iter()
    .copied()
    .filter(|s| s.exported || privately_visible(&s.path, &reference.from_path))
    .collect();
  if visible.is_empty() {
    // Definitions exist, but all are private to other files → not visible here.
    return Resolution {
      target: None,
      edge,
      confidence: Confidence::NONE,
      candidates,
    };
  }
  pick(
    &visible,
    edge,
    Confidence::CROSS_FILE,
    candidates,
    guess_on_tie,
  )
}

/// Whether `symbol` is a plausible target for qualifier `q`: its containing definition carries
/// that name, or — for static paths only — it is a top-level definition in a module file named
/// `q` (`util::helper` → `…/util.rs`). Method receivers never module-match: a variable that
/// happens to share a file's name is coincidence, not namespace evidence.
fn qualifier_matches(symbol: &Symbol, q: &str, form: RefForm) -> bool {
  match symbol.owner.as_deref() {
    Some(owner) => owner == q,
    None => form == RefForm::Static && module_stem_matches(&symbol.path, q),
  }
}

/// Whether `path` is the file of module `q`: its stem is `q`, or it is a directory-carrier file
/// (`mod.rs` / `lib.rs` / `main.rs` / `index.*` / `__init__.py`) inside a directory named `q`.
fn module_stem_matches(path: &str, q: &str) -> bool {
  let file = path.rsplit('/').next().unwrap_or(path);
  let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
  if stem == q {
    return true;
  }
  matches!(stem, "mod" | "lib" | "main" | "index" | "__init__")
    && parent_dir(path).rsplit('/').next() == Some(q)
}

/// Language-structural visibility for non-exported definitions — only where the language's
/// privacy domain is derivable from the file tree:
///
/// - **Rust**: a private item is visible to its module and all descendant modules, and the
///   module tree mirrors the file tree — `dir/name.rs` owns `dir/name/**`, and the carrier
///   files `mod.rs`/`lib.rs`/`main.rs` own their directory's subtree.
/// - **Java**: package-private means visible throughout the package, and a package is a
///   directory — same-directory `.java` files see each other's package-private definitions.
///
/// Everything else stays file-local: JS/TS module-locals genuinely end at the file, C `static`
/// is translation-unit-local, and module/assembly-scoped visibilities (Kotlin `internal`,
/// C# `internal`, Swift `internal`) are not derivable from paths — masking those is the
/// conservative, honest outcome.
fn privately_visible(def_path: &str, from_path: &str) -> bool {
  if let Some(stem_path) = def_path.strip_suffix(".rs") {
    if !from_path.ends_with(".rs") {
      return false;
    }
    let subtree_root = match stem_path.rsplit('/').next().unwrap_or(stem_path) {
      "mod" | "lib" | "main" => parent_dir(def_path),
      _ => stem_path,
    };
    return !subtree_root.is_empty()
      && from_path
        .strip_prefix(subtree_root)
        .is_some_and(|rest| rest.starts_with('/'));
  }
  if def_path.ends_with(".java") && from_path.ends_with(".java") {
    return parent_dir(def_path) == parent_dir(from_path);
  }
  false
}

/// Path-form import resolution: `./util` imported from `src/a.ts` tries the indexed file nodes
/// at `src/util` and `src/util.ts` (the importer's own extension). Exact matches only — a miss
/// falls through to symbol-name resolution and, failing that, stays honestly unresolved.
///
/// Only path-shaped names (containing `/` or `.`) are attempted: a bare symbol import like
/// Java's `Helper` must not be hijacked to a coincidentally-named `Helper.java` file node when
/// precise symbol resolution is available.
fn resolve_import_path(table: &SymbolTable, reference: &Reference) -> Option<NodeId> {
  if !reference.name.contains(['/', '.']) {
    return None;
  }
  if let Some(id) = table.file(&reference.name) {
    return Some(id);
  }
  let joined = join_normalize(parent_dir(&reference.from_path), &reference.name);
  if let Some(id) = table.file(&joined) {
    return Some(id);
  }
  let ext = extension(&reference.from_path)?;
  table.file(&format!("{joined}.{ext}"))
}

fn parent_dir(path: &str) -> &str {
  path.rfind('/').map(|i| &path[..i]).unwrap_or("")
}

/// Join a relative segment path onto a directory, resolving `.` and `..` textually. An absolute
/// `rel` ignores `dir`; an absolute `dir` keeps its leading slash through the split/join.
fn join_normalize(dir: &str, rel: &str) -> String {
  let mut parts: Vec<&str> = if rel.starts_with('/') || dir.is_empty() {
    Vec::new()
  } else {
    dir.split('/').collect()
  };
  for segment in rel.split('/') {
    match segment {
      "" | "." => {}
      ".." => {
        parts.pop();
      }
      other => parts.push(other),
    }
  }
  let joined = parts.join("/");
  if rel.starts_with('/') {
    format!("/{joined}")
  } else {
    joined
  }
}

fn extension(path: &str) -> Option<&str> {
  path
    .rsplit('/')
    .next()
    .and_then(|file| file.rsplit_once('.').map(|(_, ext)| ext))
}

/// Choose a target from a visible set: unique → the given confidence; a tie takes a
/// deterministic min-id target at `AMBIGUOUS` when the reference's form tolerates it, and no
/// edge at all when it does not.
fn pick(
  set: &[&Symbol],
  edge: EdgeType,
  unique: Confidence,
  candidates: usize,
  guess_on_tie: bool,
) -> Resolution {
  if set.len() == 1 {
    Resolution {
      target: Some(set[0].id),
      edge,
      confidence: unique,
      candidates,
    }
  } else if guess_on_tie {
    let target = set.iter().min_by_key(|s| s.id.raw()).map(|s| s.id);
    Resolution {
      target,
      edge,
      confidence: Confidence::AMBIGUOUS,
      candidates,
    }
  } else {
    Resolution {
      target: None,
      edge,
      confidence: Confidence::NONE,
      candidates,
    }
  }
}

/// Below this many references the fan-out overhead outweighs the win and the batch resolves
/// serially.
const MIN_REFS_PER_SHARD: usize = 4096;

/// Resolve a batch of references, emitting a labeled edge per resolvable reference.
///
/// Large batches shard across threads (§7.5): resolution of one reference is a pure read of
/// the immutable table, so contiguous chunks resolve independently and their edge lists
/// concatenate in chunk order — the output is identical to the serial loop, edge for edge, in
/// order (pinned by test).
pub fn resolve_all(
  table: &SymbolTable,
  references: &[Reference],
  resolver: &Resolver,
) -> (Vec<ResolvedEdge>, ResolveStats) {
  if references.len() <= MIN_REFS_PER_SHARD {
    return resolve_chunk(table, references, resolver);
  }
  use rayon::prelude::*;
  let threads = rayon::current_num_threads().max(1);
  let chunk_size = references
    .len()
    .div_ceil(threads * 2)
    .max(MIN_REFS_PER_SHARD);
  let shards: Vec<(Vec<ResolvedEdge>, ResolveStats)> = references
    .par_chunks(chunk_size)
    .map(|chunk| resolve_chunk(table, chunk, resolver))
    .collect();
  let mut edges = Vec::with_capacity(references.len());
  let mut stats = ResolveStats::default();
  for (shard_edges, shard_stats) in shards {
    edges.extend(shard_edges);
    stats += shard_stats;
  }
  (edges, stats)
}

/// The serial kernel: resolve one contiguous run of references in order.
fn resolve_chunk(
  table: &SymbolTable,
  references: &[Reference],
  resolver: &Resolver,
) -> (Vec<ResolvedEdge>, ResolveStats) {
  let mut edges = Vec::new();
  let mut stats = ResolveStats::default();
  for reference in references {
    let resolution = resolver.resolve(table, reference);
    match resolution.target {
      Some(to) => {
        if resolution.confidence <= Confidence::AMBIGUOUS {
          stats.ambiguous += 1;
        } else {
          stats.resolved += 1;
        }
        edges.push(ResolvedEdge {
          from: reference.from,
          to,
          edge: resolution.edge,
          confidence: resolution.confidence.0,
        });
      }
      None => {
        if resolution.candidates == 0 {
          stats.external += 1;
        } else {
          stats.masked += 1;
        }
      }
    }
  }
  (edges, stats)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::reference::{RefForm, RefKind, Reference};
  use crate::table::{Symbol, SymbolTable};
  use vorpal_kg::SymbolKind;

  fn symbol(id: u64, path: &str, exported: bool, owner: Option<&str>) -> Symbol {
    Symbol {
      id: NodeId::new(id),
      kind: SymbolKind::Function,
      path: path.to_owned(),
      exported,
      owner: owner.map(str::to_owned),
    }
  }

  fn call(name: &str, from_path: &str) -> Reference {
    Reference::new(NodeId::new(0), from_path, name, RefKind::Call)
  }

  #[test]
  fn qualifier_binds_to_owner_and_never_falls_back_to_a_guess() {
    let mut table = SymbolTable::new();
    table.insert("new", symbol(1, "a.rs", true, Some("Kg")));
    table.insert("new", symbol(2, "b.rs", true, Some("Manifest")));

    // `Kg::new()` → exactly the Kg member, despite two visible `new`s.
    let r = Resolver::new().resolve(
      &table,
      &call("new", "c.rs")
        .with_qualifier(Some("Kg".into()))
        .with_form(RefForm::Static),
    );
    assert_eq!(r.target, Some(NodeId::new(1)));
    assert_eq!(r.confidence, Confidence::CROSS_FILE);

    // `Vec::new()` → Vec is not in the tree; a static path must NOT guess by bare name.
    let r = Resolver::new().resolve(
      &table,
      &call("new", "c.rs")
        .with_qualifier(Some("Vec".into()))
        .with_form(RefForm::Static),
    );
    assert_eq!(r.target, None);
    assert_eq!(r.candidates, 2, "candidates reported for transparency");
  }

  #[test]
  fn static_path_matches_module_file_stem() {
    let mut table = SymbolTable::new();
    table.insert("helper", symbol(1, "src/util.rs", true, None));
    table.insert("helper", symbol(2, "src/other.rs", true, None));

    let r = Resolver::new().resolve(
      &table,
      &call("helper", "src/a.rs")
        .with_qualifier(Some("util".into()))
        .with_form(RefForm::Static),
    );
    assert_eq!(r.target, Some(NodeId::new(1)));

    // Method receivers must not module-match: a variable named `util` is not evidence.
    let r = Resolver::new().resolve(
      &table,
      &call("helper", "src/a.rs")
        .with_qualifier(Some("util".into()))
        .with_form(RefForm::Method),
    );
    assert_eq!(r.target, None, "receiver text is not namespace evidence");
  }

  #[test]
  fn method_form_requires_a_unique_visible_candidate() {
    let mut table = SymbolTable::new();
    table.insert("map", symbol(1, "a.rs", true, Some("Chart")));
    table.insert("map", symbol(2, "b.rs", true, Some("Grid")));

    // `x.map()` with two visible candidates: no edge, counted as masked.
    let r = Resolver::new().resolve(&table, &call("map", "c.rs").with_form(RefForm::Method));
    assert_eq!(r.target, None);

    // Bare `map()` keeps the labeled approximate pick.
    let r = Resolver::new().resolve(&table, &call("map", "c.rs"));
    assert_eq!(r.target, Some(NodeId::new(1)));
    assert_eq!(r.confidence, Confidence::AMBIGUOUS);

    // A unique candidate binds for methods too.
    let mut unique = SymbolTable::new();
    unique.insert("map", symbol(7, "a.rs", true, Some("Chart")));
    let r = Resolver::new().resolve(&unique, &call("map", "c.rs").with_form(RefForm::Method));
    assert_eq!(r.target, Some(NodeId::new(7)));
  }

  #[test]
  fn rust_child_modules_see_parent_file_privates() {
    let mut table = SymbolTable::new();
    table.insert(
      "print_text_to",
      symbol(1, "src/outline/output.rs", false, None),
    );

    // `output/tests.rs` is a descendant module of `output.rs` → private is visible.
    let r = Resolver::new().resolve(
      &table,
      &call("print_text_to", "src/outline/output/tests.rs"),
    );
    assert_eq!(r.target, Some(NodeId::new(1)));
    assert_eq!(r.confidence, Confidence::CROSS_FILE);

    // A sibling file is not.
    let r = Resolver::new().resolve(&table, &call("print_text_to", "src/outline/other.rs"));
    assert_eq!(r.target, None);

    // Carrier files own their directory subtree.
    let mut lib = SymbolTable::new();
    lib.insert("internal", symbol(2, "crates/x/src/lib.rs", false, None));
    let r = Resolver::new().resolve(&lib, &call("internal", "crates/x/src/deep/nested.rs"));
    assert_eq!(r.target, Some(NodeId::new(2)));
    let r = Resolver::new().resolve(&lib, &call("internal", "crates/y/src/a.rs"));
    assert_eq!(r.target, None);
  }

  #[test]
  fn java_package_privates_are_visible_within_their_directory() {
    let mut table = SymbolTable::new();
    table.insert("Helper", symbol(1, "src/com/x/Helper.java", false, None));

    // Same package (directory) → visible.
    let r = Resolver::new().resolve(&table, &call("Helper", "src/com/x/Main.java"));
    assert_eq!(r.target, Some(NodeId::new(1)));

    // Another package → package-private stays masked.
    let r = Resolver::new().resolve(&table, &call("Helper", "src/com/y/Main.java"));
    assert_eq!(r.target, None);

    // Directory scoping never leaks across languages.
    let r = Resolver::new().resolve(&table, &call("Helper", "src/com/x/main.rs"));
    assert_eq!(r.target, None);
  }

  #[test]
  fn sharded_resolve_all_matches_the_serial_loop_edge_for_edge() {
    // A table with every outcome class: unique resolutions, ambiguous ties, masked privates,
    // owner-qualified members, and plenty of external misses.
    let mut table = SymbolTable::new();
    for i in 0..64u64 {
      table.insert(&format!("unique_{i}"), symbol(i, "defs.rs", true, None));
      table.insert(&format!("dup_{i}"), symbol(1000 + i, "a.rs", true, None));
      table.insert(&format!("dup_{i}"), symbol(2000 + i, "b.rs", true, None));
      table.insert(
        &format!("hidden_{i}"),
        symbol(3000 + i, "priv.rs", false, None),
      );
      table.insert(
        &format!("method_{i}"),
        symbol(4000 + i, "owners.rs", true, Some("Owner")),
      );
    }

    // Well past the sharding threshold, mixing every reference shape.
    let mut references = Vec::new();
    for round in 0..1500u64 {
      let i = round % 64;
      references.push(call(&format!("unique_{i}"), "use.rs"));
      references.push(call(&format!("dup_{i}"), "use.rs"));
      references.push(call(&format!("hidden_{i}"), "use.rs"));
      references.push(call(&format!("nowhere_{round}"), "use.rs"));
      references.push(
        call(&format!("method_{i}"), "use.rs")
          .with_qualifier(Some("Owner".into()))
          .with_form(RefForm::Static),
      );
    }
    assert!(references.len() > 4096, "must exercise the sharded path");

    let resolver = Resolver::new();
    let (sharded_edges, sharded_stats) = resolve_all(&table, &references, &resolver);

    // The serial specification: one resolver call per reference, in order.
    let mut serial_edges = Vec::new();
    let mut serial_stats = ResolveStats::default();
    for reference in &references {
      let resolution = resolver.resolve(&table, reference);
      match resolution.target {
        Some(to) => {
          if resolution.confidence <= Confidence::AMBIGUOUS {
            serial_stats.ambiguous += 1;
          } else {
            serial_stats.resolved += 1;
          }
          serial_edges.push(ResolvedEdge {
            from: reference.from,
            to,
            edge: resolution.edge,
            confidence: resolution.confidence.0,
          });
        }
        None => {
          if resolution.candidates == 0 {
            serial_stats.external += 1;
          } else {
            serial_stats.masked += 1;
          }
        }
      }
    }

    assert_eq!(sharded_stats, serial_stats);
    assert_eq!(sharded_edges, serial_edges, "edges must match in order");
  }

  #[test]
  fn stats_split_external_from_masked() {
    let mut table = SymbolTable::new();
    table.insert("hidden", symbol(1, "a.rs", false, None));
    let refs = vec![
      call("nowhere", "b.rs"), // no candidates at all → external
      call("hidden", "b.rs"),  // exists, not visible → masked
      call("hidden", "a.rs"),  // same file → resolved
    ];
    let (edges, stats) = resolve_all(&table, &refs, &Resolver::new());
    assert_eq!(edges.len(), 1);
    assert_eq!(stats.resolved, 1);
    assert_eq!(stats.external, 1);
    assert_eq!(stats.masked, 1);
    assert_eq!(stats.unresolved(), 2);
  }
}

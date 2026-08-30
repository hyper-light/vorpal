//! The resolution algorithm (§3.3): scope-aware, confidence-scored, never faking edges.

use vorpal_kg::{EdgeType, NodeId};

use crate::intern::{Interner, NameId};
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
  /// Typed-receiver resolution (G-M2): the candidate set was narrowed by the receiver's
  /// inferred type — stronger than a bare-name pick, weaker than an explicit local binding.
  pub const TYPE_BOUND: Confidence = Confidence(85);
  /// A single visible exported definition in another file.
  pub const CROSS_FILE: Confidence = Confidence(90);
  /// A single definition in the same file — the strongest binding.
  pub const LOCAL: Confidence = Confidence(100);

  /// The categorical [`ResolutionGrade`] of this confidence.
  pub fn grade(self) -> ResolutionGrade {
    ResolutionGrade::from_confidence(self)
  }
}

/// The explicit, ordinal grade of a resolution — the categorical view of [`Confidence`] a
/// consumer branches on to tell a proven binding from a best guess, without interpreting a raw
/// score. This is the single grade vocabulary shared by the CLI, MCP, and bindings (§3.3, §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolutionGrade {
  /// No definition was visible: no edge (or a name-only membership).
  Unresolved,
  /// Several definitions carried the name; the target is a deterministic best guess, not a bound
  /// reference — approximate, and labeled as such.
  Heuristic,
  /// A single visible exported definition in another file: constrained to one candidate, but not
  /// a lexical binding (no import/scope proof).
  Constrained,
  /// A single definition in the same file: an exact lexical binding.
  Exact,
}

impl ResolutionGrade {
  /// Map a confidence score to its grade using the [`Confidence`] tier constants.
  pub fn from_confidence(c: Confidence) -> Self {
    if c >= Confidence::LOCAL {
      Self::Exact
    } else if c >= Confidence::TYPE_BOUND {
      // The constrained floor moved 90 → 85 when TYPE_BOUND landed (G-M0). No existing edge
      // lives in (40, 90), so no historical label shifts.
      Self::Constrained
    } else if c > Confidence::NONE {
      Self::Heuristic
    } else {
      Self::Unresolved
    }
  }

  /// A stable lowercase label for output and machine consumers.
  pub fn label(self) -> &'static str {
    match self {
      Self::Exact => "exact",
      Self::Constrained => "constrained",
      Self::Heuristic => "heuristic",
      Self::Unresolved => "unresolved",
    }
  }
}

/// *Why* a resolution chose its target — the exact resolver branch that produced the edge,
/// persisted per edge occurrence so every relation can answer "why does this exist?" (§5).
/// Stored on disk as its `u8` tag; unknown future tags render as `unknown`, never fail.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveReason {
  /// No edge was produced (unresolved/masked) — never persisted.
  None = 0,
  /// A path-form import matched an indexed file node exactly.
  ImportPath = 1,
  /// A grammar-provided qualifier corroborated a single candidate.
  Qualified = 2,
  /// A grammar-provided qualifier corroborated several; deterministic tie pick (approximate).
  QualifiedTie = 3,
  /// A single definition in the referencing file (the strongest binding).
  Local = 4,
  /// Several same-file definitions; deterministic tie pick (approximate).
  LocalTie = 5,
  /// A single cross-file definition visible here (exported, or structurally private-visible).
  VisibleExport = 6,
  /// Several visible cross-file candidates; deterministic tie pick (approximate).
  VisibleTie = 7,
  /// A bare name bound through the referencing file's own import: the file's qualified import
  /// of this name resolved to a single corroborated target, and no local definition shadows
  /// it. The strongest cross-file evidence a bare use can carry.
  ImportBound = 8,
  /// Typed-receiver resolution (G-M2): the receiver's type came from an explicit annotation.
  ReceiverAnnotated = 9,
  /// The receiver's type came from a constructor-shaped initializer.
  ReceiverConstructed = 10,
  /// The receiver's type came from a typed parameter binding.
  ReceiverParamTyped = 11,
  /// The receiver's type came from a typed field on the enclosing type.
  ReceiverFieldTyped = 12,
  /// Type narrowing left several candidates; deterministic tie pick (approximate).
  ReceiverTypedTie = 13,
}

impl ResolveReason {
  pub fn from_tag(tag: u8) -> Self {
    match tag {
      1 => Self::ImportPath,
      2 => Self::Qualified,
      3 => Self::QualifiedTie,
      4 => Self::Local,
      5 => Self::LocalTie,
      6 => Self::VisibleExport,
      7 => Self::VisibleTie,
      8 => Self::ImportBound,
      9 => Self::ReceiverAnnotated,
      10 => Self::ReceiverConstructed,
      11 => Self::ReceiverParamTyped,
      12 => Self::ReceiverFieldTyped,
      13 => Self::ReceiverTypedTie,
      _ => Self::None,
    }
  }

  /// Stable lowercase label for output and machine consumers.
  pub fn label(self) -> &'static str {
    match self {
      Self::None => "unknown",
      Self::ImportPath => "import-path",
      Self::Qualified => "qualifier-match",
      Self::QualifiedTie => "qualifier-tie",
      Self::Local => "same-file",
      Self::LocalTie => "same-file-tie",
      Self::VisibleExport => "visible-export",
      Self::VisibleTie => "visible-tie",
      Self::ImportBound => "import-bound",
      Self::ReceiverAnnotated => "receiver-annotated",
      Self::ReceiverConstructed => "receiver-constructed",
      Self::ReceiverParamTyped => "receiver-param-typed",
      Self::ReceiverFieldTyped => "receiver-field-typed",
      Self::ReceiverTypedTie => "receiver-typed-tie",
    }
  }
}

/// Cap on retained alternative-candidate identities per occurrence: enough to explain any
/// realistic tie, bounded so a 500-way `init` collision cannot bloat the sidecar.
pub const MAX_RETAINED_ALTERNATIVES: usize = 8;

/// The outcome of resolving one reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
  /// The chosen definition, if any (approximate resolutions still pick a deterministic target).
  pub target: Option<NodeId>,
  pub edge: EdgeType,
  pub confidence: Confidence,
  /// How many definitions carried the referenced name (for transparency).
  pub candidates: usize,
  /// The resolver branch that produced (or declined) the target.
  pub reason: ResolveReason,
  /// The alternatives the chosen target beat *in the final set* (tie picks retain the tie
  /// set minus the target, capped at [`MAX_RETAINED_ALTERNATIVES`]); unique picks have none —
  /// their eliminations are already explained by `reason`. `(ids, count)`.
  pub alternatives: ([u32; MAX_RETAINED_ALTERNATIVES], u8),
}

/// A resolved reference ready to become a graph edge, carrying its evidence: the source span
/// of the referencing occurrence, the resolver branch that bound it, and how many candidates
/// carried the name — everything the persisted evidence sidecar retains per edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedEdge {
  pub from: NodeId,
  pub to: NodeId,
  pub edge: EdgeType,
  pub confidence: u8,
  /// Byte span of the referencing occurrence within `from`'s file.
  pub span: (u32, u32),
  pub reason: ResolveReason,
  /// Candidate count at resolve time (saturated to `u32`).
  pub candidates: u32,
  /// Low 32 bits of xxh3 of the referenced name — the absence-query key, carried on edges too.
  pub name_hash: u32,
  /// Retained final-set alternatives (tie picks), `(ids, count)` — see [`Resolution`].
  pub alternatives: ([u32; MAX_RETAINED_ALTERNATIVES], u8),
}

/// A reference that produced **no** edge, retained as evidence (IMPROVEMENTS 07-29 §4): the
/// honest-resolution story completed — "why is there no edge here?" is answerable from the
/// sidecar instead of only aggregate counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnresolvedEvidence {
  pub from: NodeId,
  /// Low 32 bits of xxh3 of the referenced name (the name itself lives in source, not here).
  pub name_hash: u32,
  /// The edge type this reference *would* have produced.
  pub etype: EdgeType,
  pub span: (u32, u32),
  pub candidates: u32,
  /// `true` = external (no definition anywhere in the tree); `false` = masked (definitions
  /// exist but none is safely attributable).
  pub external: bool,
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

impl ResolveStats {
  /// Every reference this stats block accounts for, across all four outcomes.
  pub fn total(&self) -> u64 {
    self.resolved + self.ambiguous + self.external + self.masked
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
  pub fn resolve<'i>(
    &self,
    interner: &'i Interner,
    table: &SymbolTable<'i>,
    reference: &Reference<'i>,
  ) -> Resolution {
    self.resolve_with(interner, table, reference, &mut ResolveScratch::default())
  }

  /// [`Resolver::resolve`] with caller-owned scratch buffers — batch resolution reuses one
  /// scratch across a whole chunk instead of allocating fresh candidate vectors per
  /// reference (~4 allocations × millions of references otherwise).
  fn resolve_with<'i>(
    &self,
    interner: &'i Interner,
    table: &SymbolTable<'i>,
    reference: &Reference<'i>,
    scratch: &mut ResolveScratch<'i>,
  ) -> Resolution {
    let edge = reference.kind.edge();
    if reference.kind == RefKind::Import {
      if let Some(target) = resolve_import_path(interner, table, reference) {
        return Resolution {
          target: Some(target),
          edge,
          confidence: Confidence::CROSS_FILE,
          candidates: 1,
          reason: ResolveReason::ImportPath,
          alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
        };
      }
    }
    let candidates = table.candidates(reference.name);
    // A bare name in a file whose own import provably bound it (the qualified import
    // resolved to a single corroborated target — ties never seed bindings, so ambiguity
    // cannot launder through here) resolves to that import's target, unless a local
    // definition shadows the import. This check precedes the empty-candidates return
    // because an ALIASED import's local name has no candidates of its own (`from x import
    // y as z`: nothing anywhere defines `z`), yet `z()` is exactly the call the binding
    // proves. Confidence stays cross-file: the use inherits the import's certainty.
    if reference.form == RefForm::Bare {
      if let Some(target) = table.import_binding(reference.from_path, reference.name) {
        if !candidates.iter().any(|s| s.path == reference.from_path) {
          return Resolution {
            target: Some(target),
            edge,
            confidence: Confidence::CROSS_FILE,
            candidates: candidates.len(),
            reason: ResolveReason::ImportBound,
            alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
          };
        }
      }
    }
    if candidates.is_empty() {
      return Resolution {
        target: None,
        edge,
        confidence: Confidence::NONE,
        candidates: 0,
        reason: ResolveReason::None,
        alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
      };
    }

    if let Some(qualifier) = reference.qualifier {
      scratch.refined.clear();
      scratch.refined.extend(
        candidates
          .iter()
          .filter(|s| qualifier_matches(interner, s, qualifier, reference.form))
          .copied(),
      );
      if !scratch.refined.is_empty() {
        // The qualifier corroborates these candidates; among them, a multi-way tie (e.g. two
        // `impl Kg` blocks defining `load`) is genuine ambiguity — labeled, tolerated.
        return finish(
          interner,
          &scratch.refined,
          reference,
          edge,
          candidates.len(),
          true,
          true,
          &mut scratch.local,
          &mut scratch.visible,
        );
      }
      if reference.form == RefForm::MethodHinted {
        // The receiver text corroborated no owner — it was an opaque value name after all.
        // Fall through to plain Method semantics below: the hint may upgrade a resolution,
        // never veto one.
      } else if reference.form != RefForm::Bare {
        // The grammar names an owner/namespace and nothing in the tree matches it: the target
        // is outside the corpus (e.g. `Vec::new`). Falling back to bare-name candidates would
        // fake an edge to a coincidentally-named definition.
        return Resolution {
          target: None,
          edge,
          confidence: Confidence::NONE,
          candidates: candidates.len(),
          reason: ResolveReason::None,
          alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
        };
      }
    }

    // Bare names may take a labeled approximate pick on a tie; member accesses on untyped
    // values (hinted or not) carry no proof beyond the name, so only a unique match binds.
    let guess_on_tie = !matches!(reference.form, RefForm::Method | RefForm::MethodHinted);
    finish(
      interner,
      candidates,
      reference,
      edge,
      candidates.len(),
      guess_on_tie,
      false,
      &mut scratch.local,
      &mut scratch.visible,
    )
  }
}

/// Reusable candidate buffers for [`Resolver::resolve_with`] — cleared per reference, sized
/// by the largest candidate set a chunk encounters.
#[derive(Default)]
struct ResolveScratch<'i> {
  refined: Vec<Symbol<'i>>,
  local: Vec<Symbol<'i>>,
  visible: Vec<Symbol<'i>>,
}

/// Shared tail of resolution: local-first, then cross-file visibility, then pick.
#[allow(clippy::too_many_arguments)] // resolution kernel: scratch buffers ride as args by design
fn finish<'i>(
  interner: &'i Interner,
  set: &[Symbol<'i>],
  reference: &Reference<'i>,
  edge: EdgeType,
  candidates: usize,
  guess_on_tie: bool,
  via_qualifier: bool,
  local: &mut Vec<Symbol<'i>>,
  visible: &mut Vec<Symbol<'i>>,
) -> Resolution {
  local.clear();
  local.extend(
    set
      .iter()
      .filter(|s| s.path == reference.from_path)
      .copied(),
  );
  if !local.is_empty() {
    return pick(local, edge, Confidence::LOCAL, candidates, guess_on_tie, via_qualifier);
  }

  // Resolve the reference's path text once per reference, not once per candidate.
  let from_path_text = interner.text_of(reference.from_path);
  visible.clear();
  visible.extend(
    set
      .iter()
      .filter(|s| s.exported || privately_visible(interner.text_of(s.path), from_path_text))
      .copied(),
  );
  if visible.is_empty() {
    // Definitions exist, but all are private to other files → not visible here.
    return Resolution {
      target: None,
      edge,
      confidence: Confidence::NONE,
      candidates,
      reason: ResolveReason::None,
      alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
    };
  }
  pick(
    visible,
    edge,
    Confidence::CROSS_FILE,
    candidates,
    guess_on_tie,
    via_qualifier,
  )
}

/// Whether `symbol` is a plausible target for qualifier `q`: its containing definition carries
/// that name, or — for static paths only — it is a top-level definition in a module file named
/// `q` (`util::helper` → `…/util.rs`). Method receivers never module-match: a variable that
/// happens to share a file's name is coincidence, not namespace evidence.
fn qualifier_matches<'i>(
  interner: &'i Interner,
  symbol: &Symbol<'i>,
  q: NameId<'i>,
  form: RefForm,
) -> bool {
  match symbol.owner {
    Some(owner) => owner == q,
    None => {
      form == RefForm::Static
        && module_stem_matches(interner.text_of(symbol.path), interner.text_of(q))
    }
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
fn resolve_import_path<'i>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  reference: &Reference<'i>,
) -> Option<NodeId> {
  let name = interner.text_of(reference.name);
  if !name.contains(['/', '.']) {
    return None;
  }
  if let Some(id) = table.file(interner, name) {
    return Some(id);
  }
  let from_path = interner.text_of(reference.from_path);
  let joined = join_normalize(parent_dir(from_path), name);
  if let Some(id) = table.file(interner, &joined) {
    return Some(id);
  }
  let ext = extension(from_path)?;
  table.file(interner, &format!("{joined}.{ext}"))
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
  set: &[Symbol],
  edge: EdgeType,
  unique: Confidence,
  candidates: usize,
  guess_on_tie: bool,
  via_qualifier: bool,
) -> Resolution {
  if set.len() == 1 {
    // The reason names the branch that bound the target: qualifier corroboration when it
    // narrowed the set, else the visibility tier the unique candidate came from.
    let reason = if via_qualifier {
      ResolveReason::Qualified
    } else if unique >= Confidence::LOCAL {
      ResolveReason::Local
    } else {
      ResolveReason::VisibleExport
    };
    Resolution {
      target: Some(set[0].id),
      edge,
      confidence: unique,
      candidates,
      reason,
      alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
    }
  } else if guess_on_tie {
    let reason = if via_qualifier {
      ResolveReason::QualifiedTie
    } else if unique >= Confidence::LOCAL {
      ResolveReason::LocalTie
    } else {
      ResolveReason::VisibleTie
    };
    let target = set.iter().min_by_key(|s| s.id.raw()).map(|s| s.id);
    // Retain the tie set the pick beat — the alternatives a "why this target?" answer names.
    let mut alts = [0u32; MAX_RETAINED_ALTERNATIVES];
    let mut alt_count = 0u8;
    for symbol in set {
      if Some(symbol.id) == target || (alt_count as usize) >= MAX_RETAINED_ALTERNATIVES {
        continue;
      }
      alts[alt_count as usize] = symbol.id.raw() as u32;
      alt_count += 1;
    }
    Resolution {
      target,
      edge,
      confidence: Confidence::AMBIGUOUS,
      candidates,
      reason,
      alternatives: (alts, alt_count),
    }
  } else {
    Resolution {
      target: None,
      edge,
      confidence: Confidence::NONE,
      candidates,
      reason: ResolveReason::None,
      alternatives: ([0; MAX_RETAINED_ALTERNATIVES], 0),
    }
  }
}

/// Resolve the qualifier-carrying import references and install, per importing file, the
/// bindings they prove: `(file path, imported name) → target node`. Only single-target,
/// constrained-or-better, symbol-form resolutions seed a binding — a tied import proves
/// nothing (so ambiguity can never launder into a binding), and a path-form match targets a
/// *file* node, which a bare name must not inherit. When one file imports the same name twice
/// the later import wins, matching rebinding semantics in the languages that allow it. Runs
/// serially: qualified imports are ~0.1% of references, and the main pass re-resolves them
/// identically for edge emission (bindings never alter `RefForm::Static` resolution).
///
/// Call between [`SymbolTable::finalize`] and the main resolution pass; returns the number of
/// bindings installed.
pub fn seed_import_bindings<'i>(
  interner: &'i Interner,
  table: &mut SymbolTable<'i>,
  qualified_imports: &[Reference<'i>],
  resolver: &Resolver,
) -> usize {
  let mut bindings: std::collections::HashMap<(NameId<'i>, NameId<'i>), NodeId> =
    std::collections::HashMap::new();
  let mut scratch = ResolveScratch::default();
  for reference in qualified_imports {
    let resolution = resolver.resolve_with(interner, table, reference, &mut scratch);
    let Some(target) = resolution.target else {
      continue;
    };
    if resolution.confidence < Confidence::CROSS_FILE {
      continue;
    }
    if !matches!(
      resolution.reason,
      ResolveReason::Qualified | ResolveReason::Local | ResolveReason::VisibleExport
    ) {
      continue;
    }
    // An aliased import rebinds under its LOCAL name — that is what bare uses in the file
    // say, so the binding keys on the alias when one exists.
    let bound_name = reference.alias.unwrap_or(reference.name);
    bindings.insert((reference.from_path, bound_name), target);
  }
  let count = bindings.len();
  table.set_import_bindings(bindings);
  count
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
pub fn resolve_all<'i>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  references: &[Reference<'i>],
  resolver: &Resolver,
) -> (Vec<ResolvedEdge>, ResolveStats) {
  if references.len() <= MIN_REFS_PER_SHARD {
    let (edges, _unresolved, stats) = resolve_chunk(interner, table, references, resolver);
    return (edges, stats);
  }
  use rayon::prelude::*;
  let threads = rayon::current_num_threads().max(1);
  let chunk_size = references
    .len()
    .div_ceil(threads * 2)
    .max(MIN_REFS_PER_SHARD);
  let shards: Vec<(Vec<ResolvedEdge>, Vec<UnresolvedEvidence>, ResolveStats)> = references
    .par_chunks(chunk_size)
    .map(|chunk| resolve_chunk(interner, table, chunk, resolver))
    .collect();
  // Reserve what actually resolved, not one slot per reference — at kernel scale roughly
  // half of all references yield edges, and the difference is ~80 MB of dead reservation.
  let total: usize = shards.iter().map(|(edges, _, _)| edges.len()).sum();
  let mut edges = Vec::with_capacity(total);
  let mut stats = ResolveStats::default();
  for (shard_edges, _unresolved, shard_stats) in shards {
    edges.extend(shard_edges);
    stats += shard_stats;
  }
  (edges, stats)
}

/// [`resolve_all`] over a [`crate::RefSpill`] instead of an in-RAM slice: chunks stream off
/// disk through a bounded channel into a worker pool, and edge lists concatenate in chunk
/// order — output identical to `resolve_all` on the same references (chunking is invisible:
/// resolution is a pure per-reference read of the immutable table). In-flight memory is a
/// few chunks, not the whole reference stream.
pub fn resolve_all_spilled<'i>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  spill: &crate::RefSpill<'i>,
  resolver: &Resolver,
) -> std::io::Result<(Vec<ResolvedEdge>, ResolveStats)> {
  let mut edges = Vec::new();
  let stats = resolve_all_spilled_into(
    interner,
    table,
    spill,
    resolver,
    |edge| edges.push(*edge),
    |_| {},
  )?;
  Ok((edges, stats))
}

/// [`resolve_all_spilled`] delivering edges through `sink` — in exactly the order the
/// collected form would have held them — instead of materializing the edge vector (~90 MB
/// alive under the seal at kernel scale). Chunks stream to a worker pool through a bounded
/// channel; the sink runs on the calling thread, fed by a rolling in-order drain of
/// finished chunks (the absorb-holdback pattern).
pub fn resolve_all_spilled_into<'i>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  spill: &crate::RefSpill<'i>,
  resolver: &Resolver,
  mut sink: impl FnMut(&ResolvedEdge),
  mut unresolved_sink: impl FnMut(&UnresolvedEvidence),
) -> std::io::Result<ResolveStats> {
  type ChunkOut = (
    usize,
    Vec<ResolvedEdge>,
    Vec<UnresolvedEvidence>,
    ResolveStats,
  );
  let threads = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(1);
  let (work_tx, work_rx) = crossbeam_channel::bounded::<(usize, Vec<Reference<'i>>)>(threads * 2);
  let (out_tx, out_rx) = crossbeam_channel::unbounded::<ChunkOut>();

  let mut stats = ResolveStats::default();
  {
    let sink = &mut sink;
    let unresolved_sink = &mut unresolved_sink;
    let stats = &mut stats;
    std::thread::scope(|scope| -> std::io::Result<()> {
      for _ in 0..threads {
        let work_rx = work_rx.clone();
        let out_tx = out_tx.clone();
        scope.spawn(move || {
          while let Ok((index, chunk)) = work_rx.recv() {
            let (edges, unresolved, stats) = resolve_chunk(interner, table, &chunk, resolver);
            if out_tx.send((index, edges, unresolved, stats)).is_err() {
              break;
            }
          }
        });
      }
      drop(work_rx);
      drop(out_tx);

      type Held = (Vec<ResolvedEdge>, Vec<UnresolvedEvidence>, ResolveStats);
      let mut holdback: std::collections::BTreeMap<usize, Held> = std::collections::BTreeMap::new();
      let mut next_out = 0usize;
      // Superlinearity probe over drained references (D7): chunk granularity, one tick per
      // drained chunk — far off the per-reference hot path.
      let mut scaling = vorpal_kg::ScalingProbe::new("link");
      let mut refs_done: u64 = 0;

      for (sent, chunk) in spill.chunks()?.enumerate() {
        if work_tx.send((sent, chunk?)).is_err() {
          break;
        }
        while let Ok((index, chunk_edges, chunk_unresolved, chunk_stats)) = out_rx.try_recv() {
          holdback.insert(index, (chunk_edges, chunk_unresolved, chunk_stats));
        }
        while let Some((chunk_edges, chunk_unresolved, chunk_stats)) = holdback.remove(&next_out) {
          for edge in &chunk_edges {
            sink(edge);
          }
          for row in &chunk_unresolved {
            unresolved_sink(row);
          }
          refs_done += chunk_stats.total();
          scaling.tick(refs_done);
          *stats += chunk_stats;
          next_out += 1;
        }
      }
      scaling.finish(refs_done);
      drop(work_tx);

      while let Ok((index, chunk_edges, chunk_unresolved, chunk_stats)) = out_rx.recv() {
        holdback.insert(index, (chunk_edges, chunk_unresolved, chunk_stats));
        while let Some((chunk_edges, chunk_unresolved, chunk_stats)) = holdback.remove(&next_out) {
          for edge in &chunk_edges {
            sink(edge);
          }
          for row in &chunk_unresolved {
            unresolved_sink(row);
          }
          *stats += chunk_stats;
          next_out += 1;
        }
      }
      Ok(())
    })?;
  }
  Ok(stats)
}

/// The serial kernel: resolve one contiguous run of references in order.
fn resolve_chunk<'i>(
  interner: &'i Interner,
  table: &SymbolTable<'i>,
  references: &[Reference<'i>],
  resolver: &Resolver,
) -> (Vec<ResolvedEdge>, Vec<UnresolvedEvidence>, ResolveStats) {
  let mut edges = Vec::new();
  let mut unresolved = Vec::new();
  let mut stats = ResolveStats::default();
  let mut scratch = ResolveScratch::default();
  for reference in references {
    let resolution = resolver.resolve_with(interner, table, reference, &mut scratch);
    let name_hash =
      xxhash_rust::xxh3::xxh3_64(interner.text_of(reference.name).as_bytes()) as u32;
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
          span: reference.evidence,
          reason: resolution.reason,
          candidates: resolution.candidates.min(u32::MAX as usize) as u32,
          name_hash,
          alternatives: resolution.alternatives,
        });
      }
      None => {
        let external = resolution.candidates == 0;
        if external {
          stats.external += 1;
        } else {
          stats.masked += 1;
        }
        unresolved.push(UnresolvedEvidence {
          from: reference.from,
          name_hash,
          etype: resolution.edge,
          span: reference.evidence,
          candidates: resolution.candidates.min(u32::MAX as usize) as u32,
          external,
        });
      }
    }
  }
  (edges, unresolved, stats)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::intern::Interner;
  use crate::reference::{RefForm, RefKind, Reference};
  use crate::table::{Symbol, SymbolTable};
  use vorpal_kg::SymbolKind;

  /// One shared session for the whole test binary: tests only ever intern a bounded
  /// vocabulary, and `'static` ids keep the assertions free of lifetime plumbing.
  fn itn() -> &'static Interner {
    static INTERNER: std::sync::OnceLock<Interner> = std::sync::OnceLock::new();
    INTERNER.get_or_init(Interner::new)
  }

  fn symbol(id: u64, path: &str, exported: bool, owner: Option<&str>) -> Symbol<'static> {
    Symbol {
      id: NodeId::new(id),
      kind: SymbolKind::Function,
      path: itn().intern(path),
      exported,
      owner: owner.map(|o| itn().intern(o)),
    }
  }

  fn call(name: &str, from_path: &str) -> Reference<'static> {
    Reference::new(itn(), NodeId::new(0), from_path, name, RefKind::Call)
  }

  #[test]
  fn qualifier_binds_to_owner_and_never_falls_back_to_a_guess() {
    let mut table = SymbolTable::new();
    table.insert(itn(), "new", symbol(1, "a.rs", true, Some("Kg")));
    table.insert(itn(), "new", symbol(2, "b.rs", true, Some("Manifest")));

    // `Kg::new()` → exactly the Kg member, despite two visible `new`s.
    table.finalize();
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("new", "c.rs")
        .with_qualifier(itn(), Some("Kg".into()))
        .with_form(RefForm::Static),
    );
    assert_eq!(r.target, Some(NodeId::new(1)));
    assert_eq!(r.confidence, Confidence::CROSS_FILE);

    // `Vec::new()` → Vec is not in the tree; a static path must NOT guess by bare name.
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("new", "c.rs")
        .with_qualifier(itn(), Some("Vec".into()))
        .with_form(RefForm::Static),
    );
    assert_eq!(r.target, None);
    assert_eq!(r.candidates, 2, "candidates reported for transparency");
  }

  #[test]
  fn static_path_matches_module_file_stem() {
    let mut table = SymbolTable::new();
    table.insert(itn(), "helper", symbol(1, "src/util.rs", true, None));
    table.insert(itn(), "helper", symbol(2, "src/other.rs", true, None));

    table.finalize();
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("helper", "src/a.rs")
        .with_qualifier(itn(), Some("util".into()))
        .with_form(RefForm::Static),
    );
    assert_eq!(r.target, Some(NodeId::new(1)));

    // Method receivers must not module-match: a variable named `util` is not evidence.
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("helper", "src/a.rs")
        .with_qualifier(itn(), Some("util".into()))
        .with_form(RefForm::Method),
    );
    assert_eq!(r.target, None, "receiver text is not namespace evidence");
  }

  #[test]
  fn import_bindings_bind_bare_uses_but_ties_never_seed() {
    // `helper` is exported by BOTH util.rs and other.rs; the importing file's
    // `use crate::util::helper` disambiguates.
    let mut table = SymbolTable::new();
    table.insert(itn(), "helper", symbol(1, "src/util.rs", true, None));
    table.insert(itn(), "helper", symbol(2, "src/other.rs", true, None));
    table.finalize();

    let import = |from_path: &str, qualifier: &str| {
      Reference::new(itn(), NodeId::new(9), from_path, "helper", RefKind::Import)
        .with_qualifier(itn(), Some(qualifier.into()))
        .with_form(RefForm::Static)
    };
    let seeded = seed_import_bindings(
      itn(),
      &mut table,
      &[import("src/a.rs", "util")],
      &Resolver::new(),
    );
    assert_eq!(seeded, 1);

    // The bare call in the importing file inherits the import-proven target...
    let r = Resolver::new().resolve(itn(), &table, &call("helper", "src/a.rs"));
    assert_eq!(r.target, Some(NodeId::new(1)));
    assert_eq!(r.confidence, Confidence::CROSS_FILE);
    assert_eq!(r.reason, ResolveReason::ImportBound);
    // ...while the same call in a file with no import stays a labelled blind tie.
    let r = Resolver::new().resolve(itn(), &table, &call("helper", "src/b.rs"));
    assert_eq!(r.reason, ResolveReason::VisibleTie);
    assert_eq!(r.confidence, Confidence::AMBIGUOUS);

    // A local definition shadows the file's own import.
    let mut shadowed = SymbolTable::new();
    shadowed.insert(itn(), "helper", symbol(1, "src/util.rs", true, None));
    shadowed.insert(itn(), "helper", symbol(3, "src/a.rs", false, None));
    shadowed.finalize();
    seed_import_bindings(itn(), &mut shadowed, &[import("src/a.rs", "util")], &Resolver::new());
    let r = Resolver::new().resolve(itn(), &shadowed, &call("helper", "src/a.rs"));
    assert_eq!(r.target, Some(NodeId::new(3)));
    assert_eq!(r.reason, ResolveReason::Local);

    // An aliased import rebinds under its LOCAL name: the binding keys on the alias, and a
    // bare use of that alias — a name with ZERO candidates of its own — still resolves
    // import-bound (the consult precedes the empty-candidates return).
    let mut aliased = SymbolTable::new();
    aliased.insert(itn(), "helper", symbol(1, "src/util.rs", true, None));
    aliased.finalize();
    let aliased_import = Reference::new(itn(), NodeId::new(9), "src/a.rs", "helper", RefKind::Import)
      .with_qualifier(itn(), Some("util".into()))
      .with_form(RefForm::Static)
      .with_alias_ref(itn(), Some("h"));
    let seeded = seed_import_bindings(itn(), &mut aliased, &[aliased_import], &Resolver::new());
    assert_eq!(seeded, 1);
    let r = Resolver::new().resolve(itn(), &aliased, &call("h", "src/a.rs"));
    assert_eq!(r.target, Some(NodeId::new(1)), "alias call binds the original");
    assert_eq!(r.reason, ResolveReason::ImportBound);
    assert_eq!(r.candidates, 0, "the alias itself is defined nowhere");
    // The ORIGINAL name did not get a binding in the aliased form — `helper()` in that file
    // resolves through normal visibility, not the alias binding.
    let r = Resolver::new().resolve(itn(), &aliased, &call("helper", "src/a.rs"));
    assert_eq!(r.reason, ResolveReason::VisibleExport);

    // Two module files share the qualifier's stem: the import itself is a qualifier TIE, and
    // a tied import must seed nothing — ambiguity never launders into a binding.
    let mut tied = SymbolTable::new();
    tied.insert(itn(), "helper", symbol(1, "src/util.rs", true, None));
    tied.insert(itn(), "helper", symbol(2, "vendor/util.rs", true, None));
    tied.finalize();
    let seeded = seed_import_bindings(itn(), &mut tied, &[import("src/a.rs", "util")], &Resolver::new());
    assert_eq!(seeded, 0, "a tied import proves nothing");
    let r = Resolver::new().resolve(itn(), &tied, &call("helper", "src/a.rs"));
    assert_eq!(r.reason, ResolveReason::VisibleTie, "no binding, so the tie stays labelled");
  }

  #[test]
  fn receiver_hints_upgrade_but_never_veto() {
    let mut table = SymbolTable::new();
    table.insert(itn(), "draw", symbol(1, "a.rs", true, Some("Chart")));
    table.insert(itn(), "draw", symbol(2, "b.rs", true, Some("Grid")));
    table.insert(itn(), "render", symbol(3, "a.rs", true, Some("Chart")));
    table.finalize();

    // The hint names an owner: corroborated exactly like a static qualifier, even among
    // several same-named members.
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("draw", "c.rs")
        .with_qualifier(itn(), Some("Grid".into()))
        .with_form(RefForm::MethodHinted),
    );
    assert_eq!(r.target, Some(NodeId::new(2)));
    assert_eq!(r.reason, ResolveReason::Qualified);

    // The hint names nothing (an opaque variable): fall back to Method semantics — a unique
    // member binds…
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("render", "c.rs")
        .with_qualifier(itn(), Some("mystery".into()))
        .with_form(RefForm::MethodHinted),
    );
    assert_eq!(r.target, Some(NodeId::new(3)), "hint must not veto the unique member");
    assert_eq!(r.reason, ResolveReason::VisibleExport);

    // …and a non-unique one stays unbound (no blind guessing), exactly like plain Method.
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("draw", "c.rs")
        .with_qualifier(itn(), Some("mystery".into()))
        .with_form(RefForm::MethodHinted),
    );
    assert_eq!(r.target, None);

    // A hint must never module-stem match: a variable sharing a file's name is coincidence.
    let mut files = SymbolTable::new();
    files.insert(itn(), "helper", symbol(7, "src/util.rs", true, None));
    files.insert(itn(), "helper", symbol(8, "src/other.rs", true, None));
    files.finalize();
    let r = Resolver::new().resolve(
      itn(),
      &files,
      &call("helper", "c.rs")
        .with_qualifier(itn(), Some("util".into()))
        .with_form(RefForm::MethodHinted),
    );
    assert_eq!(r.target, None, "hinted receivers get owner matching only");
  }

  #[test]
  fn method_form_requires_a_unique_visible_candidate() {
    let mut table = SymbolTable::new();
    table.insert(itn(), "map", symbol(1, "a.rs", true, Some("Chart")));
    table.insert(itn(), "map", symbol(2, "b.rs", true, Some("Grid")));

    // `x.map()` with two visible candidates: no edge, counted as masked.
    table.finalize();
    let r = Resolver::new().resolve(itn(), &table, &call("map", "c.rs").with_form(RefForm::Method));
    assert_eq!(r.target, None);

    // Bare `map()` keeps the labeled approximate pick.
    let r = Resolver::new().resolve(itn(), &table, &call("map", "c.rs"));
    assert_eq!(r.target, Some(NodeId::new(1)));
    assert_eq!(r.confidence, Confidence::AMBIGUOUS);

    // A unique candidate binds for methods too.
    let mut unique = SymbolTable::new();
    unique.insert(itn(), "map", symbol(7, "a.rs", true, Some("Chart")));
    unique.finalize();
    let r = Resolver::new().resolve(itn(), &unique, &call("map", "c.rs").with_form(RefForm::Method));
    assert_eq!(r.target, Some(NodeId::new(7)));
  }

  #[test]
  fn rust_child_modules_see_parent_file_privates() {
    let mut table = SymbolTable::new();
    table.insert(
      itn(),
      "print_text_to",
      symbol(1, "src/outline/output.rs", false, None),
    );

    // `output/tests.rs` is a descendant module of `output.rs` → private is visible.
    table.finalize();
    let r = Resolver::new().resolve(
      itn(),
      &table,
      &call("print_text_to", "src/outline/output/tests.rs"),
    );
    assert_eq!(r.target, Some(NodeId::new(1)));
    assert_eq!(r.confidence, Confidence::CROSS_FILE);

    // A sibling file is not.
    table.finalize();
    let r = Resolver::new().resolve(itn(), &table, &call("print_text_to", "src/outline/other.rs"));
    assert_eq!(r.target, None);

    // Carrier files own their directory subtree.
    let mut lib = SymbolTable::new();
    lib.insert(itn(), "internal", symbol(2, "crates/x/src/lib.rs", false, None));
    lib.finalize();
    let r = Resolver::new().resolve(itn(), &lib, &call("internal", "crates/x/src/deep/nested.rs"));
    assert_eq!(r.target, Some(NodeId::new(2)));
    let r = Resolver::new().resolve(itn(), &lib, &call("internal", "crates/y/src/a.rs"));
    assert_eq!(r.target, None);
  }

  #[test]
  fn java_package_privates_are_visible_within_their_directory() {
    let mut table = SymbolTable::new();
    table.insert(itn(), "Helper", symbol(1, "src/com/x/Helper.java", false, None));

    // Same package (directory) → visible.
    table.finalize();
    let r = Resolver::new().resolve(itn(), &table, &call("Helper", "src/com/x/Main.java"));
    assert_eq!(r.target, Some(NodeId::new(1)));

    // Another package → package-private stays masked.
    let r = Resolver::new().resolve(itn(), &table, &call("Helper", "src/com/y/Main.java"));
    assert_eq!(r.target, None);

    // Directory scoping never leaks across languages.
    let r = Resolver::new().resolve(itn(), &table, &call("Helper", "src/com/x/main.rs"));
    assert_eq!(r.target, None);
  }

  #[test]
  fn sharded_resolve_all_matches_the_serial_loop_edge_for_edge() {
    // A table with every outcome class: unique resolutions, ambiguous ties, masked privates,
    // owner-qualified members, and plenty of external misses.
    let mut table = SymbolTable::new();
    for i in 0..64u64 {
      table.insert(itn(), &format!("unique_{i}"), symbol(i, "defs.rs", true, None));
      table.insert(itn(), &format!("dup_{i}"), symbol(1000 + i, "a.rs", true, None));
      table.insert(itn(), &format!("dup_{i}"), symbol(2000 + i, "b.rs", true, None));
      table.insert(
        itn(),
        &format!("hidden_{i}"),
        symbol(3000 + i, "priv.rs", false, None),
      );
      table.insert(
        itn(),
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
          .with_qualifier(itn(), Some("Owner".into()))
          .with_form(RefForm::Static),
      );
    }
    assert!(references.len() > 4096, "must exercise the sharded path");

    let resolver = Resolver::new();
    table.finalize();
    let (sharded_edges, sharded_stats) = resolve_all(itn(), &table, &references, &resolver);

    // The serial specification: one resolver call per reference, in order.
    let mut serial_edges = Vec::new();
    let mut serial_stats = ResolveStats::default();
    for reference in &references {
      let resolution = resolver.resolve(itn(), &table, reference);
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
            span: reference.evidence,
            reason: resolution.reason,
            candidates: resolution.candidates.min(u32::MAX as usize) as u32,
            name_hash: xxhash_rust::xxh3::xxh3_64(itn().text_of(reference.name).as_bytes())
              as u32,
            alternatives: resolution.alternatives,
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
    table.insert(itn(), "hidden", symbol(1, "a.rs", false, None));
    let refs = vec![
      call("nowhere", "b.rs"), // no candidates at all → external
      call("hidden", "b.rs"),  // exists, not visible → masked
      call("hidden", "a.rs"),  // same file → resolved
    ];
    table.finalize();
    let (edges, stats) = resolve_all(itn(), &table, &refs, &Resolver::new());
    assert_eq!(edges.len(), 1);
    assert_eq!(stats.resolved, 1);
    assert_eq!(stats.external, 1);
    assert_eq!(stats.masked, 1);
    assert_eq!(stats.unresolved(), 2);
  }
}

#[cfg(test)]
mod grade_tests {
  use super::{Confidence, ResolutionGrade};

  #[test]
  fn grade_maps_each_confidence_tier() {
    assert_eq!(Confidence::LOCAL.grade(), ResolutionGrade::Exact);
    assert_eq!(Confidence::CROSS_FILE.grade(), ResolutionGrade::Constrained);
    assert_eq!(Confidence::AMBIGUOUS.grade(), ResolutionGrade::Heuristic);
    assert_eq!(Confidence::NONE.grade(), ResolutionGrade::Unresolved);
    // Ordered strongest-last so `>=` comparisons rank grades.
    assert!(ResolutionGrade::Exact > ResolutionGrade::Heuristic);
    assert_eq!(ResolutionGrade::Exact.label(), "exact");
  }
}

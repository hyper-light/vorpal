//! The resolution algorithm (§3.3): scope-aware, confidence-scored, never faking edges.

use vorpal_kg::{EdgeType, NodeId};

use crate::reference::{RefKind, Reference};
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

/// Counts from a resolution batch.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResolveStats {
  /// Confidently resolved (single visible definition).
  pub resolved: u64,
  /// Resolved but approximate (multiple candidates).
  pub ambiguous: u64,
  /// No visible definition — no edge emitted.
  pub unresolved: u64,
}

/// The reference resolver. Stateless today; a home for future config (per-language visibility).
#[derive(Debug, Default, Clone, Copy)]
pub struct Resolver;

impl Resolver {
  pub fn new() -> Self {
    Self
  }

  /// Resolve one reference against the table (§3.3):
  /// 1. path-form imports resolve against indexed file nodes (exact path match — cannot fake),
  /// 2. same-file definitions win (local binding),
  /// 3. else only exported definitions are visible across files,
  /// 4. a single visible definition is confident; multiple is approximate; none is unresolved.
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

    let local: Vec<&Symbol> = candidates
      .iter()
      .filter(|s| s.path == reference.from_path)
      .collect();
    if !local.is_empty() {
      return pick(&local, edge, Confidence::LOCAL, candidates.len());
    }

    let visible: Vec<&Symbol> = candidates.iter().filter(|s| s.exported).collect();
    if visible.is_empty() {
      // Definitions exist, but all are private to other files → not visible here.
      return Resolution {
        target: None,
        edge,
        confidence: Confidence::NONE,
        candidates: candidates.len(),
      };
    }
    pick(&visible, edge, Confidence::CROSS_FILE, candidates.len())
  }
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

/// Choose a target from a visible set: unique → the given confidence; many → deterministic
/// min-id target at `AMBIGUOUS` (approximate, labeled).
fn pick(set: &[&Symbol], edge: EdgeType, unique: Confidence, candidates: usize) -> Resolution {
  if set.len() == 1 {
    Resolution {
      target: Some(set[0].id),
      edge,
      confidence: unique,
      candidates,
    }
  } else {
    let target = set.iter().min_by_key(|s| s.id.raw()).map(|s| s.id);
    Resolution {
      target,
      edge,
      confidence: Confidence::AMBIGUOUS,
      candidates,
    }
  }
}

/// Resolve a batch of references, emitting a labeled edge per resolvable reference.
pub fn resolve_all(
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
      None => stats.unresolved += 1,
    }
  }
  (edges, stats)
}

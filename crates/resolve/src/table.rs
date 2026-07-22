//! The symbol table: name → candidate definitions, built from the KG's definition nodes.
//!
//! Storage is CSR-shaped: building appends `(name, symbol)` pairs to one flat vector (so
//! sharded builds absorb by memcpy, with no per-shard maps), and [`SymbolTable::finalize`]
//! groups the pairs by name with one stable sort — per-name candidate order is exactly
//! insertion order. The map-of-vecs form this replaces cost ~50–100 bytes of per-name
//! overhead *per shard* (hot names recur in every shard) and re-paid it during table
//! absorption; at kernel scale that was a ~600 MB spike for ~58 MB of payload.

use std::collections::HashMap;

use vorpal_kg::{Kg, NodeId, SymbolKind};

use crate::intern::{self, NameId};

/// A definition candidate for resolution — plain old data over interned strings (~24 bytes;
/// the owned-`String` form cost ~130 bytes per symbol × millions at kernel scale).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Symbol {
  pub id: NodeId,
  pub kind: SymbolKind,
  /// The file the definition lives in (interned path) — used for intra-file scoping.
  pub path: NameId,
  /// Whether the definition is visible across files.
  pub exported: bool,
  /// The containing definition's name for members (`Kg` for `Kg.load`), `None` for top-level
  /// items — the target side of qualified-reference matching (§3.3).
  pub owner: Option<NameId>,
}

/// Maps a name to every definition with that name (§3.3 candidate set), plus an exact-path map
/// of file nodes for path-form import resolution. Build with [`SymbolTable::insert`] (and
/// [`SymbolTable::absorb`] for sharded builds), then call [`SymbolTable::finalize`] once
/// before resolving.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SymbolTable {
  /// Insertion-ordered `(name, symbol)` pairs, drained by `finalize`.
  pending: Vec<(NameId, Symbol)>,
  /// Post-finalize: symbols grouped contiguously by name, insertion order within a name.
  grouped: Vec<Symbol>,
  /// Post-finalize: name → `(start, len)` into `grouped`.
  ranges: HashMap<NameId, (u32, u32)>,
  files: HashMap<NameId, NodeId>,
}

impl SymbolTable {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn insert(&mut self, name: &str, symbol: Symbol) {
    debug_assert!(self.grouped.is_empty(), "insert after finalize");
    self.pending.push((intern::intern(name), symbol));
  }

  /// Pre-size the pair vector for a known upper bound of inserts — sharded builds know their
  /// row-range length, and skipping Vec doubling keeps ~2× growth slack off the peak.
  pub fn reserve(&mut self, additional: usize) {
    self.pending.reserve_exact(additional);
  }

  /// [`SymbolTable::insert`], except definitions whose name was **never interned** are
  /// skipped — and the probe itself never grows the interner.
  ///
  /// Sound only when every reference that will ever query this table was constructed (and
  /// therefore interned its name) *before* the table is built — the link phase's situation,
  /// where the full reference stream is committed first. Under that ordering, a definition
  /// whose name no reference interned can never be probed by [`SymbolTable::candidates`]:
  /// candidates are keyed by the *reference's* `NameId`, and `peek` returning `None` proves
  /// no such id exists. At kernel scale, most definitions (unreferenced statics, macros,
  /// generated stubs) never enter the table at all — and, decisively, their names never
  /// enter the interner: ~1.4 M leaked strings plus shard-map entries that existed only to
  /// key rows nothing would ever look up.
  pub fn insert_if_referenced(&mut self, name: &str, symbol: Symbol) {
    debug_assert!(self.grouped.is_empty(), "insert after finalize");
    if let Some(id) = intern::peek(name) {
      self.pending.push((id, symbol));
    }
  }

  /// Register a file node by its exact ingested path (the target of path-form imports).
  pub fn insert_file(&mut self, path: &str, id: NodeId) {
    self.files.insert(intern::intern(path), id);
  }

  /// The file node at exactly `path`, if indexed. Probes never grow the interner: a joined
  /// path nothing interned cannot be in the table either.
  pub fn file(&self, path: &str) -> Option<NodeId> {
    self.files.get(&intern::peek(path)?).copied()
  }

  /// Merge another table's entries after this one's — the ordered-absorption step of a §7.5
  /// sharded table build, a plain append of the flat pair vector. Absorbing row-range shards
  /// in row order reproduces the serial insertion order exactly. (File paths and canonical
  /// identities are disjoint across shards by construction.)
  pub fn absorb(&mut self, other: SymbolTable) {
    debug_assert!(
      self.grouped.is_empty() && other.grouped.is_empty(),
      "absorb after finalize"
    );
    self.pending.extend(other.pending);
    self.files.extend(other.files);
  }

  /// Group the inserted pairs by name — counting scatter, no sort: count per name, assign
  /// each name's slice at its **first appearance** (so the layout is a pure function of
  /// insertion order — deterministic, hash-map iteration order never observed), then scatter
  /// each pair to its slot. Per-name candidate order is insertion order, exactly as a stable
  /// sort would give, at O(n) with no sort temp. Must run once, after every
  /// `insert`/`absorb` and before `candidates`.
  pub fn finalize(&mut self) {
    let pending = std::mem::take(&mut self.pending);
    Self::group(pending.iter().copied(), pending.len(), self);
  }

  /// Shared grouping kernel: `pairs` yielded in insertion order, twice-iterable via clone.
  fn group(
    pairs: impl Iterator<Item = (NameId, Symbol)> + Clone,
    total: usize,
    into: &mut SymbolTable,
  ) {
    if total == 0 {
      return;
    }
    // Pass 1: count occurrences per name.
    into.ranges.reserve(total / 4);
    for (name, _) in pairs.clone() {
      into.ranges.entry(name).or_insert((u32::MAX, 0)).1 += 1;
    }
    // Pass 2: assign each name's start at first appearance; scatter to the running slot.
    // (`u32::MAX` marks "start not yet assigned"; `grouped` is pre-filled with an arbitrary
    // symbol and fully overwritten by construction.)
    let mut cursor = 0u32;
    let first = pairs
      .clone()
      .next()
      .expect("total > 0 implies a first pair")
      .1;
    into.grouped = vec![first; total];
    for (name, symbol) in pairs {
      let range = into.ranges.get_mut(&name).expect("counted in pass 1");
      if range.0 == u32::MAX {
        range.0 = cursor;
        cursor += range.1;
        range.1 = 0;
      }
      into.grouped[(range.0 + range.1) as usize] = symbol;
      range.1 += 1;
    }
  }

  /// Build a finalized table straight from per-shard tables in absorb order, without ever
  /// concatenating their pair vectors — the count pass reads the shards in place and the
  /// scatter writes directly into the final layout. Equal to `absorb`-then-`finalize` by
  /// construction (same iteration order, same grouping kernel).
  pub fn from_shards(shards: Vec<SymbolTable>) -> Self {
    let mut table = SymbolTable::new();
    let total = shards.iter().map(|s| s.pending.len()).sum();
    {
      let pairs = shards.iter().flat_map(|s| s.pending.iter().copied());
      Self::group(pairs, total, &mut table);
    }
    for shard in shards {
      debug_assert!(
        shard.grouped.is_empty(),
        "from_shards takes unfinalized shards"
      );
      table.files.extend(shard.files);
    }
    table
  }

  /// Build a table from every node in a sealed [`Kg`]. `File` nodes go to the path map (targets
  /// of path-form imports); import/alias nodes are wiring, not definitions, and are never
  /// candidates; every other definition goes to the name candidate set, with its containment
  /// parent (when not the file) recorded as `owner`. The returned table is finalized.
  pub fn from_kg(kg: &Kg) -> Self {
    let mut table = Self::new();
    for i in 0..kg.node_count() as u64 {
      let id = NodeId::new(i);
      if let Some(node) = kg.node(id) {
        if node.kind == SymbolKind::File {
          table.insert_file(node.path, id);
          continue;
        }
        if node.kind == SymbolKind::Import {
          continue;
        }
        let owner = kg.container_of(id).and_then(|cid| {
          let container = kg.node(cid)?;
          (container.kind != SymbolKind::File).then(|| intern::intern(container.name))
        });
        table.insert(
          node.name,
          Symbol {
            id,
            kind: node.kind,
            path: intern::intern(node.path),
            exported: node.exported,
            owner,
          },
        );
      }
    }
    table.finalize();
    table
  }

  /// Every definition carrying `name` (the candidate set for resolution), in insertion order.
  pub fn candidates(&self, name: NameId) -> &[Symbol] {
    debug_assert!(
      self.pending.is_empty(),
      "finalize the table before resolving"
    );
    match self.ranges.get(&name) {
      Some(&(start, len)) => &self.grouped[start as usize..(start + len) as usize],
      None => &[],
    }
  }

  /// Total distinct names in the table (post-finalize).
  pub fn names(&self) -> usize {
    self.ranges.len()
  }
}

//! The symbol table: name → candidate definitions, built from the KG's definition nodes.
//!
//! Storage is CSR-shaped: building appends `(name, symbol)` pairs to one flat vector (so
//! sharded builds absorb by memcpy, with no per-shard maps), and [`SymbolTable::finalize`]
//! groups the pairs by name with one stable sort — per-name candidate order is exactly
//! insertion order. The map-of-vecs form this replaces cost ~50–100 bytes of per-name
//! overhead *per shard* (hot names recur in every shard) and re-paid it during table
//! absorption; at kernel scale that was a ~600 MB spike for ~58 MB of payload.

use rustc_hash::FxHashMap;
use vorpal_kg::{Kg, NodeId, SymbolKind};

use crate::intern::{Interner, NameId};

/// Post-finalize `name → (start, len)` map, direct-indexed by the id's `(shard, slot)`
/// decomposition: the per-shard slot is the dense insertion index the interner mints — a
/// perfect hash we already own — so a `candidates()` probe is two dependent loads, no
/// hashing, no probe sequence (~6.8M probes per link ran through SipHash before). Slots grow
/// on touch; `len == 0` marks an absent name. Contents are a pure function of the pair
/// sequence, so serial and sharded builds still compare equal.
#[derive(Debug, Default, Clone, PartialEq)]
struct DenseRanges {
  shards: Vec<Vec<(u32, u32)>>,
  names: usize,
}

impl DenseRanges {
  fn slot_mut(&mut self, name: NameId<'_>) -> &mut (u32, u32) {
    if self.shards.is_empty() {
      self.shards = vec![Vec::new(); crate::intern::SHARDS];
    }
    let (shard, slot) = name.shard_slot();
    let vec = &mut self.shards[shard];
    if vec.len() <= slot {
      vec.resize(slot + 1, (u32::MAX, 0));
    }
    &mut vec[slot]
  }

  fn get(&self, name: NameId<'_>) -> Option<(u32, u32)> {
    let (shard, slot) = name.shard_slot();
    let value = *self.shards.get(shard)?.get(slot)?;
    (value.1 > 0).then_some(value)
  }
}

/// A definition candidate for resolution — plain old data over interned strings (~24 bytes;
/// the owned-`String` form cost ~130 bytes per symbol × millions at kernel scale).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Symbol<'i> {
  pub id: NodeId,
  pub kind: SymbolKind,
  /// The file the definition lives in (interned path) — used for intra-file scoping.
  pub path: NameId<'i>,
  /// Whether the definition is visible across files.
  pub exported: bool,
  /// The containing definition's name for members (`Kg` for `Kg.load`), `None` for top-level
  /// items — the target side of qualified-reference matching (§3.3).
  pub owner: Option<NameId<'i>>,
}

/// Maps a name to every definition with that name (§3.3 candidate set), plus an exact-path map
/// of file nodes for path-form import resolution. Build with [`SymbolTable::insert`] (and
/// [`SymbolTable::absorb`] for sharded builds), then call [`SymbolTable::finalize`] once
/// before resolving.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SymbolTable<'i> {
  /// Insertion-ordered `(name, symbol)` pairs, drained by `finalize`.
  pending: Vec<(NameId<'i>, Symbol<'i>)>,
  /// Post-finalize: symbols grouped contiguously by name, insertion order within a name.
  grouped: Vec<Symbol<'i>>,
  /// Post-finalize: name → `(start, len)` into `grouped`, direct-indexed (see [`DenseRanges`]).
  ranges: DenseRanges,
  files: FxHashMap<NameId<'i>, NodeId>,
  /// Import-proven per-file bindings: `(importing file's path, imported name)` → the node the
  /// file's import statement resolved to at constrained-or-better, single-target confidence
  /// (seeded by [`crate::seed_import_bindings`]). Consulted for bare-name references after
  /// local definitions (which shadow imports) and before global visibility. Probed, never
  /// iterated, so map order is never observed.
  import_bindings: FxHashMap<(NameId<'i>, NameId<'i>), NodeId>,
}

impl<'i> SymbolTable<'i> {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn insert(&mut self, interner: &'i Interner, name: &str, symbol: Symbol<'i>) {
    debug_assert!(self.grouped.is_empty(), "insert after finalize");
    self.pending.push((interner.intern(name), symbol));
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
  pub fn insert_if_referenced(&mut self, interner: &'i Interner, name: &str, symbol: Symbol<'i>) {
    debug_assert!(self.grouped.is_empty(), "insert after finalize");
    if let Some(id) = interner.peek(name) {
      self.pending.push((id, symbol));
    }
  }

  /// Register a file node by its exact ingested path (the target of path-form imports).
  pub fn insert_file(&mut self, interner: &'i Interner, path: &str, id: NodeId) {
    self.files.insert(interner.intern(path), id);
  }

  /// The file node at exactly `path`, if indexed. Probes never grow the interner: a joined
  /// path nothing interned cannot be in the table either.
  pub fn file(&self, interner: &'i Interner, path: &str) -> Option<NodeId> {
    self.files.get(&interner.peek(path)?).copied()
  }

  /// Install the import-binding map (see the field's doc). Called once by the link phase's
  /// pre-pass, between `finalize` and the main resolution pass.
  pub fn set_import_bindings(&mut self, bindings: FxHashMap<(NameId<'i>, NameId<'i>), NodeId>) {
    self.import_bindings = bindings;
  }

  /// The node `path`'s import of `name` provably resolved to, if any.
  pub fn import_binding(&self, path: NameId<'i>, name: NameId<'i>) -> Option<NodeId> {
    self.import_bindings.get(&(path, name)).copied()
  }

  /// Merge another table's entries after this one's — the ordered-absorption step of a §7.5
  /// sharded table build, a plain append of the flat pair vector. Absorbing row-range shards
  /// in row order reproduces the serial insertion order exactly. (File paths and canonical
  /// identities are disjoint across shards by construction.)
  pub fn absorb(&mut self, other: SymbolTable<'i>) {
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
    pairs: impl Iterator<Item = (NameId<'i>, Symbol<'i>)> + Clone,
    total: usize,
    into: &mut SymbolTable<'i>,
  ) {
    if total == 0 {
      return;
    }
    // Pass 1: count occurrences per name (dense slots grow on first touch).
    for (name, _) in pairs.clone() {
      into.ranges.slot_mut(name).1 += 1;
    }
    // Pass 2: assign each name's start at first appearance; scatter to the running slot.
    // (`u32::MAX` marks "start not yet assigned"; `grouped` is pre-filled with an arbitrary
    // symbol and fully overwritten by construction.)
    let mut cursor = 0u32;
    let mut names = 0usize;
    let first = pairs
      .clone()
      .next()
      .expect("total > 0 implies a first pair")
      .1;
    into.grouped = vec![first; total];
    for (name, symbol) in pairs {
      let range = into.ranges.slot_mut(name);
      if range.0 == u32::MAX {
        range.0 = cursor;
        cursor += range.1;
        range.1 = 0;
        names += 1;
      }
      into.grouped[(range.0 + range.1) as usize] = symbol;
      range.1 += 1;
    }
    into.ranges.names = names;
  }

  /// Build a finalized table straight from per-shard tables in absorb order, without ever
  /// concatenating their pair vectors — the count pass reads the shards in place and the
  /// scatter writes directly into the final layout. Equal to `absorb`-then-`finalize` by
  /// construction (same iteration order, same grouping kernel).
  pub fn from_shards(shards: Vec<SymbolTable<'i>>) -> Self {
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
  pub fn from_kg(interner: &'i Interner, kg: &Kg) -> Self {
    let mut table = Self::new();
    for i in 0..kg.node_count() as u64 {
      let id = NodeId::new(i);
      if let Some(node) = kg.node(id) {
        if node.kind == SymbolKind::File {
          table.insert_file(interner, node.path, id);
          continue;
        }
        if node.kind == SymbolKind::Import {
          continue;
        }
        let owner = kg.container_of(id).and_then(|cid| {
          let container = kg.node(cid)?;
          (container.kind != SymbolKind::File).then(|| interner.intern(container.name))
        });
        table.insert(
          interner,
          node.name,
          Symbol {
            id,
            kind: node.kind,
            path: interner.intern(node.path),
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
  pub fn candidates(&self, name: NameId<'i>) -> &[Symbol<'i>] {
    debug_assert!(
      self.pending.is_empty(),
      "finalize the table before resolving"
    );
    match self.ranges.get(name) {
      Some((start, len)) => &self.grouped[start as usize..(start + len) as usize],
      None => &[],
    }
  }

  /// Total distinct names in the table (post-finalize).
  pub fn names(&self) -> usize {
    self.ranges.names
  }
}

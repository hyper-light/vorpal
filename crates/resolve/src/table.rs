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
  /// Basename → every indexed file carrying it, as `(full path text, node)`, sorted by
  /// path at finalize — the candidate pool for root-relative import suffix matching
  /// ([`SymbolTable::file_by_suffix`]).
  file_suffixes: FxHashMap<&'i str, Vec<(&'i str, NodeId)>>,
  /// Include-root support learned from the corpus's own import stream
  /// ([`SymbolTable::learn_include_roots`]): directory prefix → how many import
  /// occurrences it satisfies. The `-I` set inferred from data — probed for suffix
  /// tie-breaking, never iterated.
  root_support: FxHashMap<&'i str, u32>,
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
    let interned = interner.intern(path);
    let text = interner.text_of(interned);
    let basename = text.rsplit('/').next().unwrap_or(text);
    self.file_suffixes.entry(basename).or_default().push((text, id));
    self.files.insert(interned, id);
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

  /// Learn include-root support from the corpus's own import stream: for every
  /// path-form import, each directory prefix `R` such that `R/name` is an indexed
  /// file earns one occurrence. The result is the `-I` set inferred from data —
  /// the roots that explain the corpus's import vocabulary (a kernel's `include/`
  /// dwarfs `tools/include/` exactly because it satisfies far more of the stream).
  /// Call after `finalize`, before resolution; both link drivers do.
  pub fn learn_include_roots(
    &mut self,
    interner: &'i Interner,
    imports: &[crate::reference::Reference<'i>],
  ) {
    // Derived from THIS import stream alone: a retained (maintained) table re-learns on
    // every link, and support must equal what a from-scratch table learns from the same
    // alive set — accumulation across links would let historical edits sway rung 2.
    self.root_support.clear();
    for reference in imports {
      if reference.kind != crate::reference::RefKind::Import {
        continue;
      }
      let name = interner.text_of(reference.name);
      let Some((dir, basename)) = name.rsplit_once('/') else {
        continue;
      };
      if dir.is_empty() {
        continue;
      }
      let Some(bucket) = self.file_suffixes.get(basename) else {
        continue;
      };
      for &(path, _) in bucket {
        if let Some(at) = suffix_boundary(path, name) {
          if let Some(root) = path.get(..at) {
            let entry = self.root_support.entry(root).or_insert(0);
            *entry = entry.saturating_add(1);
          }
        }
      }
    }
  }

  /// The learned include-root support, sorted by root: what [`Self::learn_include_roots`]
  /// derived from this link's import stream. Persisted beside the reach graph so a scoped
  /// compose can break suffix ties exactly as the full build did.
  pub fn include_root_support(&self) -> Vec<(&'i str, u32)> {
    let mut out: Vec<(&'i str, u32)> = self.root_support.iter().map(|(&r, &n)| (r, n)).collect();
    out.sort_unstable();
    out
  }

  /// Install a persisted include-root support map in place of learning one — the scoped
  /// compose's path. A session sees only its own files' imports, which can never reproduce
  /// corpus-wide support; the prior generation's map is exactly what a full build over the
  /// same tree learns while the session's imports are unchanged, and a session whose imports
  /// changed fails the reach-row check regardless.
  pub fn set_include_root_support<'a>(
    &mut self,
    interner: &'i Interner,
    entries: impl IntoIterator<Item = (&'a str, u32)>,
  ) {
    self.root_support.clear();
    for (root, count) in entries {
      self.root_support.insert(interner.text_of(interner.intern(root)), count);
    }
  }

  /// Resolve a root-relative import (`linux/export.h`-shaped: at least one directory
  /// component) to the indexed file it names, by path suffix. Disambiguation is an
  /// evidence hierarchy, every rung corpus-derived:
  ///
  /// 1. **Nearest prefix** — the candidate sharing the most leading path components
  ///    with the importer wins (`tools/` files bind `tools/include/`, an arch tree
  ///    binds its own headers) — locality trumps popularity.
  /// 2. **Root support** — among prefix-ties, the candidate under the root that
  ///    satisfies more of the corpus's import stream ([`Self::learn_include_roots`])
  ///    wins: a main-tree file's `<linux/export.h>` binds `include/`, not the
  ///    `tools/include/` shadow copy.
  /// 3. **Still tied → `None`** — approximate edges are labeled, never faked.
  ///
  /// Bare basenames (no directory component) carry no structural evidence and are
  /// never suffix-matched: the relative probes either already resolved them or the
  /// file genuinely is not where the include convention says.
  pub fn file_by_suffix(
    &self,
    interner: &'i Interner,
    name: &str,
    from_path: NameId<'i>,
  ) -> Option<(NodeId, NameId<'i>)> {
    let (dir, basename) = name.rsplit_once('/')?;
    if dir.is_empty() {
      return None;
    }
    let bucket = self.file_suffixes.get(basename)?;
    let from_text = interner.text_of(from_path);
    let mut best: Option<(usize, u32, &'i str, NodeId)> = None;
    let mut tied = false;
    for &(path, id) in bucket {
      let Some(at) = suffix_boundary(path, name) else {
        continue;
      };
      let shared = shared_components(from_text, path);
      let support = path
        .get(..at)
        .and_then(|root| self.root_support.get(root))
        .copied()
        .unwrap_or(0);
      match best {
        Some((s, p, _, _)) if (shared, support) < (s, p) => {}
        Some((s, p, _, _)) if (shared, support) == (s, p) => tied = true,
        _ => {
          best = Some((shared, support, path, id));
          tied = false;
        }
      }
    }
    if tied {
      return None;
    }
    best.map(|(_, _, path, id)| (id, interner.intern(path)))
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
    for (basename, entries) in other.file_suffixes {
      self.file_suffixes.entry(basename).or_default().extend(entries);
    }
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
    self.seal_file_suffixes();
  }

  /// Sort every suffix bucket by path text — candidate scan order becomes a pure
  /// function of the file set (hash-map iteration order is never observed).
  fn seal_file_suffixes(&mut self) {
    for bucket in self.file_suffixes.values_mut() {
      bucket.sort_unstable_by(|a, b| a.0.cmp(b.0));
      bucket.dedup_by(|a, b| a.0 == b.0);
    }
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
      for (basename, entries) in shard.file_suffixes {
        table.file_suffixes.entry(basename).or_default().extend(entries);
      }
    }
    table.seal_file_suffixes();
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
        if !node.kind.is_resolution_candidate() {
          // The candidate law lives on SymbolKind (one definition for every
          // table feed) — see `SymbolKind::is_resolution_candidate`.
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

  /// Replace one name's candidate run in place (the retained daemon's table maintenance):
  /// the new run appends at the tail of `grouped` and the name's slot repoints to it; the
  /// old run becomes garbage (tracked by the caller via [`SymbolTable::grouped_len`] deltas
  /// and retired by a full rebuild past a threshold). Candidate ORDER within the name is the
  /// caller's contract — pass the run in canonical (path-major, row-ascending) order and
  /// `candidates()` behaves exactly as a from-scratch build's.
  pub fn replace_candidates(&mut self, name: NameId<'i>, run: &[Symbol<'i>]) {
    debug_assert!(self.pending.is_empty(), "maintenance only after finalize");
    let slot = self.ranges.slot_mut(name);
    let had = slot.1 > 0;
    if run.is_empty() {
      if had {
        self.ranges.names -= 1;
      }
      *self.ranges.slot_mut(name) = (u32::MAX, 0);
      return;
    }
    let start = self.grouped.len() as u32;
    self.grouped.extend_from_slice(run);
    *self.ranges.slot_mut(name) = (start, run.len() as u32);
    if !had {
      self.ranges.names += 1;
    }
  }

  /// Update (or insert) the file-node entry for `path` — an edited file's node id moves.
  pub fn update_file(&mut self, path: NameId<'i>, id: NodeId) {
    self.files.insert(path, id);
  }

  /// Drop the file-node entry for `path` — the file was deleted; path-form imports of it
  /// must stop resolving instead of pointing at a retired row.
  pub fn remove_file(&mut self, path: NameId<'i>) {
    self.files.remove(&path);
  }

  /// Current length of the grouped candidate store — tail growth from
  /// [`SymbolTable::replace_candidates`] minus nothing (garbage is never reclaimed in
  /// place); callers difference this against live candidate counts to decide when a full
  /// rebuild is cheaper than the accumulated waste.
  pub fn grouped_len(&self) -> usize {
    self.grouped.len()
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

/// A finalized table with its interner brand erased, so a retained daemon can own it beside
/// the `Interner` that built it (SUBSECOND.md Phase 3 — the same lifetime-free discipline as
/// `RefStore`). Sound because the table stores interned IDS only — `NameId` is an index plus
/// a phantom brand, never a pointer — and the brand exists solely to prevent cross-interner
/// id confusion. [`RetainedSymbolTable::borrow_mut`] restores the brand; callers uphold the
/// one rule that matters: **rebind only with the interner that built the table.**
pub struct RetainedSymbolTable(SymbolTable<'static>);

impl RetainedSymbolTable {
  pub fn erase(table: SymbolTable<'_>) -> Self {
    // SAFETY: `SymbolTable` transitively contains no references — `NameId<'i>` is
    // `(NonZeroU32, PhantomData<&'i str>)` — so the only effect of the transmute is the
    // phantom brand. The public contract above confines rebinding to the originating
    // interner, which is exactly the invariant the brand encodes.
    Self(unsafe { std::mem::transmute::<SymbolTable<'_>, SymbolTable<'static>>(table) })
  }

  pub fn borrow_mut<'i>(&mut self, _interner: &'i Interner) -> &mut SymbolTable<'i> {
    // SAFETY: inverse of `erase` under the same no-references argument; `_interner` pins
    // the caller to naming the session the ids belong to.
    unsafe {
      std::mem::transmute::<&mut SymbolTable<'static>, &mut SymbolTable<'i>>(&mut self.0)
    }
  }
}

/// Where `path` ends with `/name`: the byte offset of that `/`, else `None`.
/// Pure byte comparison — never a char-boundary panic (`/` is ASCII, so a match
/// proves the boundary).
fn suffix_boundary(path: &str, name: &str) -> Option<usize> {
  let at = path.len().checked_sub(name.len() + 1)?;
  let bytes = path.as_bytes();
  (bytes[at] == b'/' && &bytes[at + 1..] == name.as_bytes()).then_some(at)
}

/// Leading path components two paths share (`a/b/x.c` vs `a/b/y/z.h` → 2).
fn shared_components(a: &str, b: &str) -> usize {
  a.split('/')
    .zip(b.split('/'))
    .take_while(|(x, y)| x == y)
    .count()
}

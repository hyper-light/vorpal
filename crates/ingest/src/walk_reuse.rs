//! Walk reuse (incremental saves): the extraction walk's outputs, snapshotted beside the
//! cached parse tree, so an edited giant re-walks only its dirty top-level region.
//!
//! The tree cache (see [`crate::tree_cache`]) already makes the *reparse* of a save
//! incremental; the residual cost is the extraction walk itself — on a multi-megabyte
//! generated source, millions of reference rows re-derive from unchanged definitions on
//! every save. This module snapshots the walk's PRE-finalize outputs (pre-adoption
//! outline items, pre-dedup reference rows, binders, finished signatures) in a compact
//! owned form. The next save maps the edit to a dirty top-level byte region, re-walks
//! only that region, splices retained rows around the fresh ones (byte positions
//! shifted, entity ids remapped by span), and runs the unchanged file-global finalize
//! laws over the merged whole — the same rows a full walk computes, at the cost of the
//! dirty region only.
//!
//! Correctness stance: reuse is gated hard (see the eligibility checks at the call
//! site) and every splice invariant is checked at runtime — any violation falls back to
//! the full walk, and the product-byte oracle pins reuse == full across edit shapes.
//!
//! Representation: almost every captured string is a slice of the retained source, so
//! rows store byte spans ([`Snip::Span`]) instead of owned copies — capture allocates
//! per *rendered* string only, and resolving against the next save's source is
//! allocation-free (`Cow::Borrowed` either into the new source or into the snapshot's
//! own text).

use std::borrow::Cow;
use std::ops::Range;

use vorpal_outline::model::{
  EntryRole, OutlineEntry, OutlineItem, OutlineMember, SourcePosition, SourceRange, SymbolType,
};
use vorpal_resolve::{RefForm, RefKind};

use crate::references::{ArgClass, Pending, RawArg, RawRef, RawRequest};
use crate::typefacts::RawBinding;

/// Span-or-text: the snapshot spelling of a `Cow<'t, str>`. `Span` offsets index the
/// source the snapshot was captured against; `Text` carries strings that were rendered
/// or composed rather than sliced (template signatures, joined paths).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Snip {
  Span(u32, u32),
  Text(Box<str>),
}

impl Snip {
  fn capture(s: &str, src: &str) -> Snip {
    let base = src.as_ptr() as usize;
    let p = s.as_ptr() as usize;
    // Pointer-range containment proves `s` is a subslice of `src`; the offsets are then
    // exact by construction. Anything else (rendered strings, grammar-static kind names)
    // is carried as text.
    if p >= base && p.saturating_add(s.len()) <= base.saturating_add(src.len()) {
      Snip::Span((p - base) as u32, (p - base + s.len()) as u32)
    } else {
      Snip::Text(s.into())
    }
  }

  /// Resolve against the NEW source, shifting span offsets by `shift` (the edit's byte
  /// delta for suffix-side rows, 0 for prefix-side). `None` = the shifted span fell
  /// outside the new source or off a char boundary — a splice-invariant violation the
  /// caller turns into a full-walk fallback.
  fn resolve<'t>(&'t self, src: &'t str, shift: i64) -> Option<Cow<'t, str>> {
    match self {
      Snip::Span(a, b) => {
        let a = usize::try_from(i64::from(*a) + shift).ok()?;
        let b = usize::try_from(i64::from(*b) + shift).ok()?;
        src.get(a..b).map(Cow::Borrowed)
      }
      Snip::Text(t) => Some(Cow::Borrowed(&**t)),
    }
  }

  fn heap_bytes(&self) -> usize {
    match self {
      Snip::Span(..) => 0,
      Snip::Text(t) => t.len(),
    }
  }
}

fn capture_opt(s: &Option<Cow<'_, str>>, src: &str) -> Option<Snip> {
  s.as_deref().map(|s| Snip::capture(s, src))
}

fn resolve_opt<'t>(s: &'t Option<Snip>, src: &'t str, shift: i64) -> Option<Option<Cow<'t, str>>> {
  match s {
    None => Some(None),
    Some(snip) => snip.resolve(src, shift).map(Some),
  }
}

// ---- Reference rows -----------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct SnapArg {
  index: u16,
  class: ArgClass,
  kw_name: Option<Snip>,
  expr: Option<Snip>,
}

#[derive(Debug, Clone)]
pub(crate) struct SnapRef {
  from: u32,
  name: Snip,
  kind: RefKind,
  start: u32,
  end: u32,
  qualifier: Option<Snip>,
  form: RefForm,
  alias: Option<Snip>,
  receiver: Option<Snip>,
  args: Vec<SnapArg>,
}

#[derive(Debug, Clone)]
pub(crate) enum SnapPending {
  Ready(SnapRef),
  TypeUse { from: u32, name: Snip, start: u32, end: u32 },
  ImplUse { from: u32, name: Snip, start: u32, end: u32 },
}

impl SnapPending {
  /// The row's anchor byte offset — the emitting node's start, used to classify the row
  /// as prefix / dirty / suffix relative to the dirty region.
  pub(crate) fn start(&self) -> u32 {
    match self {
      SnapPending::Ready(r) => r.start,
      SnapPending::TypeUse { start, .. } | SnapPending::ImplUse { start, .. } => *start,
    }
  }

  pub(crate) fn end(&self) -> u32 {
    match self {
      SnapPending::Ready(r) => r.end,
      SnapPending::TypeUse { end, .. } | SnapPending::ImplUse { end, .. } => *end,
    }
  }

  fn capture(p: &Pending<'_>, src: &str) -> SnapPending {
    match p {
      Pending::Ready(r) => SnapPending::Ready(SnapRef {
        from: r.from.raw() as u32,
        name: Snip::capture(&r.name, src),
        kind: r.kind,
        start: r.start,
        end: r.end,
        qualifier: capture_opt(&r.qualifier, src),
        form: r.form,
        alias: capture_opt(&r.alias, src),
        receiver: capture_opt(&r.receiver, src),
        args: r
          .args
          .iter()
          .map(|a| SnapArg {
            index: a.index,
            class: a.class,
            kw_name: capture_opt(&a.kw_name, src),
            expr: capture_opt(&a.expr, src),
          })
          .collect(),
      }),
      Pending::TypeUse { from, name, start, end } => SnapPending::TypeUse {
        from: from.raw() as u32,
        name: Snip::capture(name, src),
        start: *start,
        end: *end,
      },
      Pending::ImplUse { from, name, start, end } => SnapPending::ImplUse {
        from: from.raw() as u32,
        name: Snip::capture(name, src),
        start: *start,
        end: *end,
      },
    }
  }

  /// Rebuild the borrowed row against the new source. `shift` moves byte positions;
  /// `remap(old_entity) -> Option<new_entity>` re-attributes. `None` = fallback.
  fn resolve<'t>(
    &'t self,
    src: &'t str,
    shift: i64,
    remap: &impl Fn(u32) -> Option<u32>,
  ) -> Option<Pending<'t>> {
    let move_pos = |p: u32| u32::try_from(i64::from(p) + shift).ok();
    Some(match self {
      SnapPending::Ready(r) => Pending::Ready(RawRef {
        from: vorpal_kg::NodeId::new(u64::from(remap(r.from)?)),
        name: r.name.resolve(src, shift)?,
        kind: r.kind,
        start: move_pos(r.start)?,
        end: move_pos(r.end)?,
        qualifier: resolve_opt(&r.qualifier, src, shift)?,
        form: r.form,
        alias: resolve_opt(&r.alias, src, shift)?,
        receiver: resolve_opt(&r.receiver, src, shift)?,
        args: r
          .args
          .iter()
          .map(|a| {
            Some(RawArg {
              index: a.index,
              class: a.class,
              kw_name: resolve_opt(&a.kw_name, src, shift)?,
              expr: resolve_opt(&a.expr, src, shift)?,
            })
          })
          .collect::<Option<Vec<_>>>()?,
      }),
      SnapPending::TypeUse { from, name, start, end } => Pending::TypeUse {
        from: vorpal_kg::NodeId::new(u64::from(remap(*from)?)),
        name: name.resolve(src, shift)?,
        start: move_pos(*start)?,
        end: move_pos(*end)?,
      },
      SnapPending::ImplUse { from, name, start, end } => Pending::ImplUse {
        from: vorpal_kg::NodeId::new(u64::from(remap(*from)?)),
        name: name.resolve(src, shift)?,
        start: move_pos(*start)?,
        end: move_pos(*end)?,
      },
    })
  }

  fn heap_bytes(&self) -> usize {
    match self {
      SnapPending::Ready(r) => {
        r.name.heap_bytes()
          + r.qualifier.as_ref().map_or(0, Snip::heap_bytes)
          + r.alias.as_ref().map_or(0, Snip::heap_bytes)
          + r.receiver.as_ref().map_or(0, Snip::heap_bytes)
          + r.args.len() * std::mem::size_of::<SnapArg>()
          + r
            .args
            .iter()
            .map(|a| {
              a.kw_name.as_ref().map_or(0, Snip::heap_bytes)
                + a.expr.as_ref().map_or(0, Snip::heap_bytes)
            })
            .sum::<usize>()
      }
      SnapPending::TypeUse { name, .. } | SnapPending::ImplUse { name, .. } => name.heap_bytes(),
    }
  }
}

#[derive(Debug, Clone)]
pub(crate) struct SnapBinder {
  scope: (u32, u32),
  name: Snip,
}

impl SnapBinder {
  pub(crate) fn start(&self) -> u32 {
    self.scope.0
  }

  pub(crate) fn end(&self) -> u32 {
    self.scope.1
  }

  fn resolve<'t>(&'t self, src: &'t str, shift: i64) -> Option<(Range<usize>, Cow<'t, str>)> {
    let a = usize::try_from(i64::from(self.scope.0) + shift).ok()?;
    let b = usize::try_from(i64::from(self.scope.1) + shift).ok()?;
    Some((a..b, self.name.resolve(src, shift)?))
  }
}

// ---- Outline items ------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct SnapEntry {
  role: EntryRole,
  symbol_type: SymbolType,
  name: Snip,
  start_byte: u32,
  end_byte: u32,
  signature: Snip,
  ast_kind: Snip,
}

impl SnapEntry {
  fn capture(entry: &OutlineEntry<'_>, src: &str) -> SnapEntry {
    SnapEntry {
      role: entry.role,
      symbol_type: entry.symbol_type,
      name: Snip::capture(&entry.name, src),
      start_byte: entry.range.byte_offset.start as u32,
      end_byte: entry.range.byte_offset.end as u32,
      signature: Snip::capture(&entry.signature, src),
      ast_kind: Snip::capture(&entry.ast_kind, src),
    }
  }

  fn resolve<'t>(&'t self, src: &'t str, shift: i64, lines: &LineIndex) -> Option<OutlineEntry<'t>> {
    let start = usize::try_from(i64::from(self.start_byte) + shift).ok()?;
    let end = usize::try_from(i64::from(self.end_byte) + shift).ok()?;
    if end > src.len() {
      return None;
    }
    Some(OutlineEntry {
      role: self.role,
      symbol_type: self.symbol_type,
      name: self.name.resolve(src, shift)?,
      range: SourceRange {
        byte_offset: start..end,
        start: lines.position(src, start),
        end: lines.position(src, end),
      },
      signature: self.signature.resolve(src, shift)?,
      ast_kind: self.ast_kind.resolve(src, shift)?,
    })
  }

  fn heap_bytes(&self) -> usize {
    self.name.heap_bytes() + self.signature.heap_bytes() + self.ast_kind.heap_bytes()
  }
}

#[derive(Debug, Clone)]
pub(crate) struct SnapItem {
  entry: SnapEntry,
  is_import: bool,
  is_exported: bool,
  members: Vec<(SnapEntry, bool)>,
  member_of: Option<Box<str>>,
}

impl SnapItem {
  pub(crate) fn start(&self) -> u32 {
    self.entry.start_byte
  }

  pub(crate) fn end(&self) -> u32 {
    self.entry.end_byte
  }

  pub(crate) fn capture(item: &OutlineItem<'_>, member_of: &Option<String>, src: &str) -> SnapItem {
    SnapItem {
      entry: SnapEntry::capture(&item.entry, src),
      is_import: item.is_import,
      is_exported: item.is_exported,
      members: item
        .members
        .iter()
        .map(|m| (SnapEntry::capture(&m.entry, src), m.is_public))
        .collect(),
      member_of: member_of.as_deref().map(Box::from),
    }
  }

  pub(crate) fn resolve<'t>(
    &'t self,
    src: &'t str,
    shift: i64,
    lines: &LineIndex,
  ) -> Option<(OutlineItem<'t>, Option<String>)> {
    Some((
      OutlineItem {
        entry: self.entry.resolve(src, shift, lines)?,
        is_import: self.is_import,
        is_exported: self.is_exported,
        members: self
          .members
          .iter()
          .map(|(entry, is_public)| {
            Some(OutlineMember {
              entry: entry.resolve(src, shift, lines)?,
              is_public: *is_public,
            })
          })
          .collect::<Option<Vec<_>>>()?,
      },
      self.member_of.as_deref().map(String::from),
    ))
  }

  fn heap_bytes(&self) -> usize {
    self.entry.heap_bytes()
      + self.members.len() * std::mem::size_of::<(SnapEntry, bool)>()
      + self.members.iter().map(|(e, _)| e.heap_bytes()).sum::<usize>()
      + self.member_of.as_deref().map_or(0, str::len)
  }
}

// ---- Line index ---------------------------------------------------------------------------

/// Newline offsets of one source, for rebuilding `SourcePosition`s exactly as the live
/// extractor computes them: line = tree-sitter row (count of `\n` before the offset),
/// column = CHARACTER count from the line start (`Content::get_char_column`'s loop).
pub(crate) struct LineIndex {
  /// Byte offset one past each `\n`, prefixed with 0 — `starts[i]` begins line `i`.
  starts: Vec<u32>,
}

impl LineIndex {
  pub(crate) fn new(src: &str) -> LineIndex {
    let mut starts = Vec::with_capacity(src.len() / 32 + 1);
    starts.push(0u32);
    starts.extend(
      memchr::memchr_iter(b'\n', src.as_bytes()).map(|at| (at + 1) as u32),
    );
    LineIndex { starts }
  }

  fn position(&self, src: &str, offset: usize) -> SourcePosition {
    let line = self.starts.partition_point(|&s| s as usize <= offset) - 1;
    let line_start = self.starts[line] as usize;
    // Char count, mirroring `get_char_column`: count non-continuation bytes.
    let column = src.as_bytes()[line_start..offset]
      .iter()
      .filter(|&&b| (b & 0xC0) != 0x80)
      .count();
    SourcePosition { line, column }
  }
}

// ---- The snapshot -------------------------------------------------------------------------

/// Everything one file's extraction walk produced, in splice-ready pre-finalize form.
/// Lives beside the cached parse tree; superseded (recaptured) on every save.
pub(crate) struct WalkSnapshot {
  /// xxh3 of the source these rows describe — must match the cache entry's source.
  pub(crate) source_xxh3: u64,
  /// The extraction identity (grammar generation ⊕ rules digest) the rows were derived
  /// under; a changed grammar or rule set discards the snapshot.
  pub(crate) identity: u64,
  /// PRE-adoption outline items with their `memberOf` owner names, file order.
  pub(crate) items: Vec<SnapItem>,
  /// PRE-finalize reference rows, emission order.
  pub(crate) pending: Vec<SnapPending>,
  /// Type-parameter binders: (declaring span, name).
  pub(crate) binders: Vec<SnapBinder>,
  /// FINISHED signatures (entity-indexed against `spans`); sketches are content-local,
  /// so retained entities keep them verbatim.
  pub(crate) signatures: Vec<crate::product::ProductSignature>,
  /// The old definition layout: `spans[id] = (start, end)` for entity `id` (0 = file).
  /// Retained rows re-attribute by looking their old span up in the new layout.
  pub(crate) spans: Vec<(u32, u32)>,
}

/// Capture the pre-adoption item pairs of one walk (either a full walk's collection or
/// the reuse path's merged splice) into snapshot form.
pub(crate) fn capture_items(
  collected: &[(OutlineItem<'_>, Option<String>)],
  src: &str,
) -> Vec<SnapItem> {
  collected
    .iter()
    .map(|(item, member_of)| SnapItem::capture(item, member_of, src))
    .collect()
}

/// Capture the reference walk's pre-finalize rows.
pub(crate) fn capture_pending(pending: &[Pending<'_>], src: &str) -> Vec<SnapPending> {
  pending.iter().map(|p| SnapPending::capture(p, src)).collect()
}

pub(crate) fn capture_binders(
  binders: &[(Range<usize>, Cow<'_, str>)],
  src: &str,
) -> Vec<SnapBinder> {
  binders
    .iter()
    .map(|(scope, name)| SnapBinder {
      scope: (scope.start as u32, scope.end as u32),
      name: Snip::capture(name, src),
    })
    .collect()
}

/// The definition layout in snapshot form: `out[id] = (start, end)`. Layout ids are dense
/// (0 = file, then items/members in order), so index = id.
pub(crate) fn capture_spans(spans: &[(Range<usize>, vorpal_kg::NodeId)]) -> Vec<(u32, u32)> {
  let mut out = vec![(0u32, 0u32); spans.len()];
  for (range, id) in spans {
    if let Some(slot) = out.get_mut(id.raw() as usize) {
      *slot = (
        range.start.min(u32::MAX as usize) as u32,
        range.end.min(u32::MAX as usize) as u32,
      );
    }
  }
  out
}

impl WalkSnapshot {
  /// Approximate resident size, charged against the tree cache's byte budget so snapshot
  /// mass is bounded by the same knob that bounds retained sources.
  pub(crate) fn approx_bytes(&self) -> usize {
    std::mem::size_of::<WalkSnapshot>()
      + self.items.len() * std::mem::size_of::<SnapItem>()
      + self.items.iter().map(SnapItem::heap_bytes).sum::<usize>()
      + self.pending.len() * std::mem::size_of::<SnapPending>()
      + self.pending.iter().map(SnapPending::heap_bytes).sum::<usize>()
      + self.binders.len() * std::mem::size_of::<SnapBinder>()
      + self
        .binders
        .iter()
        .map(|b| b.name.heap_bytes())
        .sum::<usize>()
      + self.signatures.len() * std::mem::size_of::<crate::product::ProductSignature>()
      + self.spans.len() * std::mem::size_of::<(u32, u32)>()
  }

}

/// The dirty byte region of one incremental save, in both coordinate systems: OLD-source
/// offsets classify snapshot rows, NEW-source offsets select the fresh subtrees to walk.
/// Invariants (by construction in [`compute_dirty`]): `old.start == new.start <= prefix`,
/// `old.end >= old_len - suffix`, `new.end == old.end + delta`.
#[derive(Debug, Clone)]
pub(crate) struct DirtyRegion {
  pub(crate) old: Range<u32>,
  pub(crate) new: Range<u32>,
  /// `new_len - old_len` — how far suffix-side positions shift.
  pub(crate) delta: i64,
}

/// Map the incremental parse's delta to the dirty top-level region: the spanning textual
/// edit, unioned with every tree-sitter changed range (mapped to old coordinates), then
/// expanded to whole snapshot-item spans until no item straddles the boundary. `None` =
/// the geometry doesn't fit u32 bookkeeping (never for real sources; files above 4 GiB
/// don't reach extraction).
pub(crate) fn compute_dirty(
  snap: &WalkSnapshot,
  delta: &vorpal_core::tree_sitter::IncrementalDelta,
) -> Option<DirtyRegion> {
  let old_len = u32::try_from(delta.old_len).ok()?;
  let new_len = u32::try_from(delta.new_len).ok()?;
  let prefix = u32::try_from(delta.prefix).ok()?;
  let suffix = u32::try_from(delta.suffix).ok()?;
  let shift = i64::from(new_len) - i64::from(old_len);
  // Seed: the spanning edit window in OLD coordinates.
  let mut a0 = prefix.min(old_len);
  let mut b0 = old_len.saturating_sub(suffix).max(a0);
  // Union in tree-sitter's changed ranges (NEW coordinates → OLD): a range at or before
  // the window keeps its offsets, one at or after shifts back by the delta, and anything
  // inside the window is already covered by the seed.
  let new_window_end = i64::from(new_len) - i64::from(suffix);
  for r in &delta.changed {
    let start = i64::try_from(r.start).ok()?;
    let end = i64::try_from(r.end).ok()?;
    let old_start = if start <= i64::from(prefix) {
      start
    } else if start >= new_window_end {
      start - shift
    } else {
      i64::from(a0)
    };
    let old_end = if end <= i64::from(prefix) {
      end
    } else if end >= new_window_end {
      end - shift
    } else {
      i64::from(b0)
    };
    a0 = a0.min(u32::try_from(old_start.max(0)).ok()?);
    b0 = b0.max(u32::try_from(old_end.max(0)).ok()?.min(old_len));
  }
  // Expand to whole top-level item spans (fixpoint: expansion can pull in overlapping
  // items, which can pull the boundary further).
  loop {
    let mut grew = false;
    for item in &snap.items {
      let (start, end) = (item.start(), item.end());
      if start < b0 && end > a0 {
        if start < a0 {
          a0 = start;
          grew = true;
        }
        if end > b0 {
          b0 = end;
          grew = true;
        }
      }
    }
    if !grew {
      break;
    }
  }
  let a1 = a0; // a0 <= prefix, so the offset is identical in new coordinates
  let b1 = u32::try_from(i64::from(b0) + shift).ok()?.min(new_len);
  Some(DirtyRegion {
    old: a0..b0,
    new: a1..b1,
    delta: shift,
  })
}

/// Split the snapshot's PRE-adoption items around the dirty region, resolving retained
/// items against the new source (suffix side shifted, positions rebuilt against `lines`).
/// Returns `(prefix, suffix)`; `None` on a straddling item (the expansion missed — a
/// splice-invariant violation) or an out-of-bounds resolve.
pub(crate) fn split_items<'t>(
  snap: &'t WalkSnapshot,
  new_src: &'t str,
  dirty: &DirtyRegion,
  lines: &LineIndex,
) -> Option<SplitItems<'t>> {
  let mut prefix = Vec::new();
  let mut suffix = Vec::new();
  for item in &snap.items {
    if item.end() <= dirty.old.start {
      prefix.push(item.resolve(new_src, 0, lines)?);
    } else if item.start() >= dirty.old.end {
      suffix.push(item.resolve(new_src, dirty.delta, lines)?);
    } else if item.start() >= dirty.old.start && item.end() <= dirty.old.end {
      // Inside the dirty region: superseded by the fresh walk.
    } else {
      return None;
    }
  }
  Some((prefix, suffix))
}

pub(crate) type SplitItems<'t> = (
  Vec<(OutlineItem<'t>, Option<String>)>,
  Vec<(OutlineItem<'t>, Option<String>)>,
);

/// Old entity id → new entity id, by definition span: a retained definition keeps its
/// byte span (shifted on the suffix side), so its old span looked up in the NEW layout
/// names its new id. Built once per reuse; `None` if the new layout has duplicate spans
/// (two entities over identical bytes — ambiguous, fall back).
pub(crate) fn entity_remap(
  snap: &WalkSnapshot,
  new_spans: &[(Range<usize>, vorpal_kg::NodeId)],
  dirty: &DirtyRegion,
) -> Option<EntityRemap> {
  let mut by_span = std::collections::HashMap::with_capacity(new_spans.len());
  for (range, id) in new_spans.iter().skip(1) {
    let key = (range.start as u64, range.end as u64);
    if by_span.insert(key, id.raw() as u32).is_some() {
      return None;
    }
  }
  Some(EntityRemap {
    old_spans: snap.spans.clone(),
    by_span,
    dirty: dirty.clone(),
  })
}

pub(crate) struct EntityRemap {
  old_spans: Vec<(u32, u32)>,
  by_span: std::collections::HashMap<(u64, u64), u32>,
  dirty: DirtyRegion,
}

impl EntityRemap {
  pub(crate) fn map(&self, old_id: u32) -> Option<u32> {
    if old_id == 0 {
      return Some(0); // the file node is id 0 in every layout
    }
    let &(start, end) = self.old_spans.get(old_id as usize)?;
    let shift = if end <= self.dirty.old.start {
      0
    } else if start >= self.dirty.old.end {
      self.dirty.delta
    } else {
      return None; // a dirty entity has no retained counterpart
    };
    let new_start = u64::try_from(i64::from(start) + shift).ok()?;
    let new_end = u64::try_from(i64::from(end) + shift).ok()?;
    self.by_span.get(&(new_start, new_end)).copied()
  }
}

/// Retained reference rows and binders, resolved and remapped, split at the dirty
/// region: the caller walks the dirty subtrees fresh and splices between the halves.
pub(crate) struct RetainedRows<'t> {
  pub(crate) prefix_pending: Vec<Pending<'t>>,
  pub(crate) suffix_pending: Vec<Pending<'t>>,
  pub(crate) prefix_binders: Vec<(Range<usize>, Cow<'t, str>)>,
  pub(crate) suffix_binders: Vec<(Range<usize>, Cow<'t, str>)>,
}

/// `None` on any splice-invariant violation: a row straddling the dirty boundary, a
/// shifted span escaping the new source, a retained row attributed to a vanished entity.
pub(crate) fn split_rows<'t>(
  snap: &'t WalkSnapshot,
  new_src: &'t str,
  dirty: &DirtyRegion,
  remap: &EntityRemap,
) -> Option<RetainedRows<'t>> {
  let remap_fn = |id: u32| remap.map(id);
  let mut out = RetainedRows {
    prefix_pending: Vec::new(),
    suffix_pending: Vec::new(),
    prefix_binders: Vec::new(),
    suffix_binders: Vec::new(),
  };
  for row in &snap.pending {
    if row.end() <= dirty.old.start {
      out.prefix_pending.push(row.resolve(new_src, 0, &remap_fn)?);
    } else if row.start() >= dirty.old.end {
      out
        .suffix_pending
        .push(row.resolve(new_src, dirty.delta, &remap_fn)?);
    } else if row.start() >= dirty.old.start && row.end() <= dirty.old.end {
      // Dirty: superseded by the fresh regional walk.
    } else {
      return None;
    }
  }
  for binder in &snap.binders {
    if binder.end() <= dirty.old.start {
      out.prefix_binders.push(binder.resolve(new_src, 0)?);
    } else if binder.start() >= dirty.old.end {
      out.suffix_binders.push(binder.resolve(new_src, dirty.delta)?);
    } else if binder.start() >= dirty.old.start && binder.end() <= dirty.old.end {
      // Dirty: superseded.
    } else {
      return None;
    }
  }
  Some(out)
}

/// The retained entities' signatures under their NEW ids; dirty entities are absent (the
/// fresh regional signer re-signs them). `None` on a remap failure.
pub(crate) fn retained_signatures(
  snap: &WalkSnapshot,
  dirty: &DirtyRegion,
  remap: &EntityRemap,
) -> Option<Vec<crate::product::ProductSignature>> {
  let mut out = Vec::with_capacity(snap.signatures.len());
  for sig in &snap.signatures {
    let &(start, end) = snap.spans.get(sig.entity_index as usize)?;
    if end <= dirty.old.start || start >= dirty.old.end {
      out.push(crate::product::ProductSignature {
        entity_index: remap.map(sig.entity_index)?,
        shingles: sig.shingles,
        sketch: sig.sketch,
      });
    }
  }
  Some(out)
}

/// Compile-time proof that the MVP's empty-gate types stay walk outputs — if either grows
/// a genuinely region-local role for C, the gate at the call site must be revisited.
#[allow(dead_code)]
fn _gate_types(_: &RawBinding<'_>, _: &RawRequest<'_>) {}

/// Extractions that spliced retained rows around a dirty region (telemetry + oracle
/// non-vacuity: tests assert the counter moved, so a silently-dead reuse path can never
/// masquerade as a passing oracle).
pub(crate) static SPLICES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Reuse attempts that fell back to the full walk after the item stage succeeded.
pub(crate) static FALLBACKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod tests {
  use super::*;

  /// THE splice oracle: a chain of saves through the tree cache — where extraction
  /// captures a snapshot on one save and SPLICES retained rows on the next — must
  /// produce products byte-identical to fresh whole-file extractions at every step.
  /// Asserts the splice counter moved, so a silently-dead reuse path can never pass.
  fn splice_oracle(path: &str, versions: &[String]) {
    use crate::tree_cache::grep_cached_unpoliced;
    use vorpal_core::Language as _;
    let extractor = crate::OutlineExtractor::new().expect("rules compile");
    let lang = vorpal_lang_registry::SgLang::from_path(path).expect("lang");
    let (first, rest) = versions.split_first().expect("at least one version");
    // Two-touch prime, then extract v0 so a snapshot lands beside the retained tree.
    let _ = grep_cached_unpoliced(lang, path, first);
    let _ = grep_cached_unpoliced(lang, path, first);
    let _ = extractor
      .extract_product_via(path, first, grep_cached_unpoliced)
      .expect("prime extraction");
    let mut spliced_any = false;
    for (step, version) in rest.iter().enumerate() {
      let before = SPLICES.load(std::sync::atomic::Ordering::Relaxed);
      let via_cache = extractor
        .extract_product_via(path, version, grep_cached_unpoliced)
        .expect("cached extraction");
      let fresh = extractor
        .extract_product(&format!("fresh-{step}-{path}"), version)
        .expect("fresh extraction");
      let mut a = Vec::new();
      let mut b = Vec::new();
      crate::product::encode_product_into(&via_cache, &mut a);
      crate::product::encode_product_into(&fresh, &mut b);
      assert_eq!(a, b, "{path} step {step}: spliced product diverged from fresh");
      spliced_any |= SPLICES.load(std::sync::atomic::Ordering::Relaxed) > before;
    }
    assert!(
      spliced_any,
      "{path}: no save in the chain actually spliced — the oracle would be vacuous"
    );
    // Leave the shared cache clean for sibling tests.
    crate::tree_cache::evict_for_tests(path);
  }

  const C_BASE: &str = r#"
#include <stdio.h>
#include "util.h"

#define LIMIT 64
#define twice(x) ((x) + (x))

static int counter = 0;

struct pair { int x; int y; };

typedef struct pair pair_t;

union blob { int i; float f; };

enum mode { IDLE, BUSY };

static inline int add(int a, int b) {
  return a + b;
}

int scale(pair_t p, int k) {
  struct pair q = { p.x * k, p.y * k };
  return add(q.x, q.y) + twice(k);
}

int main(void) {
  pair_t p = { 1, 2 };
  printf("%d %d\n", scale(p, LIMIT), counter);
  return 0;
}
"#;

  #[test]
  fn splice_oracle_edit_shapes() {
    let base = C_BASE.to_string();
    let cases: Vec<(&str, String)> = vec![
      (
        "body-edit",
        base.replace("return a + b;", "return a * b + counter;"),
      ),
      ("prepend", format!("// leading comment\n{base}")),
      (
        "append",
        format!("{base}\nint tail(void) {{ return LIMIT; }}\n"),
      ),
      (
        "add-definition-mid",
        base.replace(
          "int main(void) {",
          "static int helper(int v) { return add(v, v); }\n\nint main(void) {",
        ),
      ),
      (
        "delete-definition",
        base.replace("union blob { int i; float f; };\n\n", ""),
      ),
      (
        "rename-definition",
        base.replace("int scale(", "int rescale(").replace("scale(p, LIMIT)", "rescale(p, LIMIT)"),
      ),
      ("whitespace-only", base.replace("int main", "int  main")),
      ("identical", base.clone()),
      (
        "two-distant-edits",
        base
          .replace("counter = 0", "counter = 42")
          .replace("return 0;", "return counter;"),
      ),
      (
        "edit-first-definition",
        base.replace("#define LIMIT 64", "#define LIMIT 128"),
      ),
      (
        "struct-member-edit",
        base.replace("struct pair { int x; int y; };", "struct pair { int x; int y; int z; };"),
      ),
    ];
    for (name, edited) in cases {
      splice_oracle(&format!("splice-{name}.c"), &[C_BASE.to_string(), edited]);
    }
  }

  #[test]
  fn splice_oracle_chained_saves() {
    // Multi-save chains: each step splices against the snapshot the PREVIOUS step
    // captured (including snapshots captured BY the splice path itself).
    let v0 = C_BASE.to_string();
    let v1 = v0.replace("return a + b;", "return a + b + 1;");
    let v2 = v1.replace("#define LIMIT 64", "#define LIMIT 96");
    let v3 = format!("{v2}\nint chained(void) {{ return twice(3); }}\n");
    let v4 = v3.replace("int chained(void) { return twice(3); }", "");
    splice_oracle("splice-chain.c", &[v0, v1, v2, v3, v4]);
  }

  #[test]
  fn splice_oracle_vendored_giant() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = repo.join("grammars/tree-sitter-c/src/parser.c");
    let Ok(source) = std::fs::read_to_string(&path) else {
      return;
    };
    let edited = source.replace(
      "static const char * const ts_symbol_names",
      "/* touched */\nstatic const char * const ts_symbol_names",
    );
    assert_ne!(source, edited);
    let reverted = source.clone();
    splice_oracle("splice-giant.c", &[source, edited, reverted]);
  }

  #[test]
  fn snip_captures_slices_as_spans_and_foreign_strings_as_text() {
    let src = String::from("int add(int a, int b) { return a + b; }");
    let slice = &src[4..7];
    match Snip::capture(slice, &src) {
      Snip::Span(4, 7) => {}
      other => panic!("expected Span(4,7), got {other:?}"),
    }
    let rendered = String::from("add()");
    match Snip::capture(&rendered, &src) {
      Snip::Text(t) => assert_eq!(&*t, "add()"),
      other => panic!("expected Text, got {other:?}"),
    }
  }

  #[test]
  fn snip_resolves_with_shift_and_rejects_out_of_bounds() {
    let old = String::from("aaa bbb ccc");
    let new = String::from("aaa XX bbb ccc");
    // "bbb" at 4..7 in old; the edit inserted "XX " at 4 (delta +3).
    let snip = Snip::capture(&old[4..7], &old);
    assert_eq!(snip.resolve(&new, 3).as_deref(), Some("bbb"));
    assert_eq!(snip.resolve(&new, 0).as_deref(), Some("XX "));
    assert!(snip.resolve(&new, 1000).is_none());
    assert!(snip.resolve(&new, -100).is_none());
  }

  #[test]
  fn line_index_positions_match_char_column_semantics() {
    let src = "abc\ndéf ghi\nx";
    let lines = LineIndex::new(src);
    assert_eq!(lines.position(src, 0), SourcePosition { line: 0, column: 0 });
    assert_eq!(lines.position(src, 3), SourcePosition { line: 0, column: 3 });
    assert_eq!(lines.position(src, 4), SourcePosition { line: 1, column: 0 });
    // 'é' is two bytes; the char column after "dé" is 2, at byte offset 4 + 3.
    assert_eq!(lines.position(src, 7), SourcePosition { line: 1, column: 2 });
    let last = src.len();
    assert_eq!(lines.position(src, last), SourcePosition { line: 2, column: 1 });
  }
}

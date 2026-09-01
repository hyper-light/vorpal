//! Scoped re-resolution for the defs-stable compose (SUBSECOND.md P4.5c-2): re-derive ONE
//! edited file's resolution outcomes against a PRIOR sealed generation, without replaying
//! the corpus.
//!
//! The theorem this leans on (verified in `apply_parts`/`ingest_file_with_spans`): a file's
//! node rows are exactly `[File][item][member…]` in layout order, and every identity column
//! (name, path, signature, kind, exported, eid, content_hash = hash(entity_path, signature))
//! is BODY-INVARIANT. So a defs-stable edit — same items, members, imports, signatures,
//! params, returns; bodies changed — keeps every file's node count and order, which keeps
//! every dense id in the universe, which keeps every OTHER file's evidence/edge bytes. Only
//! the edited file's own outcomes need re-deriving, and they can be re-derived against the
//! prior generation because the candidate universe is unchanged BY THE SAME PREMISE.
//!
//! Drift-freedom is structural, not aspirational: references are built by the SAME
//! `reference_from_view` the pipeline uses, resolved by the SAME `resolve_batch` chunk
//! kernel, joined by the SAME `join_call_edge`, matched by the SAME `match_requests`. The
//! partial symbol table mirrors `build_symbol_table_over` rule-for-rule (Import skipping,
//! File registration, peek-or-sentinel owners, id-ascending candidate runs) — and the
//! scoped-vs-scratch oracle test holds the mirror to the pipeline's answers.

use std::collections::HashSet;
use std::io;

use vorpal_kg::{Kg, NodeId, SymbolKind};
use vorpal_resolve::{
  ChainReturns, Interner, Reference, RefForm, RefKind, Resolver, SymbolTable, resolve_batch,
  seed_import_bindings,
};

use crate::pipeline::{
  ArgJoin, ArgRec, ParamTable, is_python_path, join_call_edge, reference_from_view,
  unmatchable_owner,
};
use crate::product::{ProductView, decode_product_view};
use crate::requests::{ReqRow, match_requests};
use crate::similar::SigRow;

/// What one file's scoped re-resolution yields, in the exact shapes and orders the sealed
/// artifacts need:
/// - `evidence`: the file's rows (edge AND no-edge outcomes) — the evidence saver's
///   canonical total-order sort makes emission order irrelevant;
/// - `edges`: the file's resolution edges in reference order with `DATA_FLOWS` spliced
///   directly after each first (caller, callee) CALLS edge — the per-src slab segment;
/// - `request_edges`: the request/notify tail segment (per-src order = site order);
/// - `flows`: the file's dataflow rows (sealed ids);
/// - `sigs`: the file's fresh sketch rows (the sigs family's replacement run).
pub struct ScopedOutcome {
  pub evidence: Vec<vorpal_kg::EvidenceRow>,
  pub edges: Vec<(u32, u32, vorpal_kg::EdgeType)>,
  pub request_edges: Vec<(u32, u32, vorpal_kg::EdgeType)>,
  pub flows: Vec<vorpal_kg::DataflowRow>,
  pub sigs: Vec<SigRow>,
  pub stats: crate::ResolveStats,
}

/// Fetches a packed product's bytes by its manifest path — the compose passes the prior
/// generation's pack reader; the oracle test passes a closure over its fixture.
pub trait ProductSource {
  fn product(&self, path: &str) -> Option<Vec<u8>>;
}

impl<F: Fn(&str) -> Option<Vec<u8>>> ProductSource for F {
  fn product(&self, path: &str) -> Option<Vec<u8>> {
    self(path)
  }
}

/// Defs-stability: the eligibility ladder for the scoped compose, checked between the
/// prior and fresh product views of ONE changed file. `None` = eligible. Every rejection
/// names its rung — the caller phase-stamps it, and the full pipeline takes over.
///
/// Bodies, references, sketches, and request sites are FREE to differ (re-derived here);
/// everything that shapes the shared universe is not:
/// - grammar identity and error accounting (parse-health gating inputs);
/// - the definition set sans ranges (names, kinds, signatures, export, import wiring,
///   member structure) — the ordinal-stability premise itself;
/// - per-entity params (callers' argument binding reads the callee's ledger);
/// - returns (the rets ledger is a GLOBAL name-keyed chain input).
pub fn views_defs_stable_reject(
  old: &ProductView<'_>,
  new: &ProductView<'_>,
) -> Option<&'static str> {
  if old.grammar_digest != new.grammar_digest {
    return Some("grammar digest");
  }
  if old.error_nodes != new.error_nodes || old.error_bytes != new.error_bytes {
    return Some("error accounting");
  }
  if old.error_spans.len() != new.error_spans.len() {
    return Some("error span count");
  }
  if !items_equal_sans_ranges(&old.items, &new.items) {
    return Some("definition set");
  }
  if old.entity_params != new.entity_params {
    return Some("entity params");
  }
  if old.returns != new.returns {
    return Some("returns");
  }
  None
}

fn items_equal_sans_ranges(
  old: &[vorpal_outline::model::OutlineItem<'_>],
  new: &[vorpal_outline::model::OutlineItem<'_>],
) -> bool {
  if old.len() != new.len() {
    return false;
  }
  old.iter().zip(new).all(|(a, b)| {
    a.entry.name == b.entry.name
      && a.entry.signature == b.entry.signature
      && a.entry.symbol_type == b.entry.symbol_type
      && a.is_exported == b.is_exported
      && a.is_import == b.is_import
      && a.members.len() == b.members.len()
      && a.members.iter().zip(&b.members).all(|(ma, mb)| {
        ma.entry.name == mb.entry.name
          && ma.entry.signature == mb.entry.signature
          && ma.entry.symbol_type == mb.entry.symbol_type
          && ma.is_public == mb.is_public
      })
  })
}

/// Re-resolve one defs-stable file against the prior generation. `file_key` names the file
/// in the identity space; `path` is its manifest spelling (what references intern);
/// `view` is the FRESH product. `products` serves OTHER files' packed products for the
/// bounded chain/param closure; `decode_cap` bounds that closure — past it the caller
/// escalates to the full pipeline (loudly), never approximates.
///
/// The caller must hand a `kg` whose name index is present (`Kg::load` picks up
/// `names.idx`; `build_names_index` otherwise) — `nodes_named` is the candidate source.
#[allow(clippy::too_many_arguments)] // the one scoped entry: every input is load-bearing
pub fn scoped_resolve_file(
  interner: &Interner,
  kg: &Kg,
  map: &vorpal_kg::NodeIdMap,
  resolver: &Resolver,
  products: &dyn ProductSource,
  path: &str,
  file_key: u64,
  view: &ProductView<'_>,
  layout_ords: &[u64],
  decode_cap: usize,
) -> io::Result<ScopedOutcome> {
  // --- the file's dense range: base + row count, from the nodes TOC file table ---
  let Some(&(_, base, rows)) = map.files().iter().find(|&&(key, _, _)| key == file_key) else {
    return Err(io::Error::other("scoped: file outside the prior universe"));
  };
  // Layout parity: [File] + items + members, mapped through the writer's OWN duplicate
  // collapse (`layout_ords`, from the caller's scratch ingest of this same product) — a C
  // declaration+definition pair shares one row, so layout length may EXCEED the row
  // count; every mapped ordinal must land inside it.
  let layout_len = 1 + view.items.iter().map(|item| 1 + item.members.len()).sum::<usize>();
  if layout_ords.len() != layout_len {
    return Err(io::Error::other(format!(
      "scoped: layout mapping length {} != fresh layout {layout_len}",
      layout_ords.len(),
    )));
  }
  if layout_ords.iter().any(|&ord| ord >= u64::from(rows)) {
    return Err(io::Error::other("scoped: layout mapping outside the prior node rows"));
  }
  let ord_of = |index: u32| -> io::Result<u64> {
    layout_ords
      .get(index as usize)
      .copied()
      .ok_or_else(|| io::Error::other("scoped: entity index outside the file layout"))
  };

  // --- pass 1 (interning parity with the pipeline's two-pass shape): references first ---
  let path_id = interner.intern(path);
  let mut references: Vec<Reference<'_>> = Vec::with_capacity(view.refs.len());
  let mut args: Vec<ArgRec> = Vec::new();
  let mut req_rows: Vec<ReqRow> = Vec::new();
  for r in &view.refs {
    let from = NodeId::new(base + ord_of(r.from_entity_index)?);
    if crate::product::tag_refkind(r.kind) == vorpal_resolve::RefKind::Call && r.args_len() > 0
    {
      let has_receiver = r.receiver.is_some();
      for arg in r.args() {
        if arg.class <= 2 {
          args.push(ArgRec {
            from,
            span: (r.start, r.end),
            index: arg.index,
            class: arg.class,
            has_receiver,
            expr: arg.expr.map(Box::from),
            kw: arg.kw_name.map(Box::from),
          });
        }
      }
    }
    references.push(reference_from_view(interner, from, path_id, r));
  }
  for req in &view.requests {
    let Ok(from_ord) = ord_of(req.from_entity_index) else {
      continue; // corrupt product index — the apply kernel drops these identically
    };
    req_rows.push(ReqRow {
      from: base + from_ord,
      method: Box::from(req.method),
      path: Box::from(req.path),
      span: (req.start, req.end),
    });
  }

  // --- the name closure: every table lookup the file's resolution can perform ---
  let mut names: HashSet<&str> = HashSet::new();
  let mut call_names: HashSet<&str> = HashSet::new();
  let mut chain_keys: HashSet<&str> = HashSet::new();
  for r in &view.refs {
    names.insert(r.name);
    if let Some(receiver_type) = r.receiver_type {
      names.insert(receiver_type);
      // The resolver's ONE chain consult (`resolver.rs`, ReceiverChained) fires for
      // Method/MethodHinted refs and keys on the ref's receiver_type — nothing else
      // ever reads the rets ledger, so nothing else bounds the decode set.
      if matches!(
        crate::product::tag_refform(r.form),
        vorpal_resolve::RefForm::Method | vorpal_resolve::RefForm::MethodHinted
      ) {
        chain_keys.insert(receiver_type);
      }
    }
    if crate::product::tag_refkind(r.kind) == vorpal_resolve::RefKind::Call {
      call_names.insert(r.name);
    }
  }

  // --- bounded closure, split by CONSUMER (each half decodes only what its reader can
  // ever consult): rets ← files defining a chain key; params ← PYTHON files defining a
  // called name (`is_python_path` gates the ledger at ingest — a C corpus decodes
  // nothing at all here, measured 542 dead decodes/1.4 s before this split).
  let mut closure_paths: HashSet<String> = HashSet::new();
  for name in &chain_keys {
    for id in kg.nodes_named(name) {
      let Some(node) = kg.node(id) else { continue };
      if node.kind == SymbolKind::Import || node.kind == SymbolKind::File {
        continue;
      }
      if node.path != path {
        closure_paths.insert(node.path.to_string());
      }
    }
  }
  for name in &call_names {
    for id in kg.nodes_named(name) {
      let Some(node) = kg.node(id) else { continue };
      if node.kind == SymbolKind::Import || node.kind == SymbolKind::File {
        continue;
      }
      if node.path != path && is_python_path(node.path) {
        closure_paths.insert(node.path.to_string());
      }
    }
  }
  if closure_paths.len() > decode_cap {
    return Err(io::Error::other(format!(
      "scoped: closure of {} files exceeds the decode cap ({decode_cap})",
      closure_paths.len(),
    )));
  }
  vorpal_kg::phase_stamp(&format!(
    "scoped: closure {} files over {} call names",
    closure_paths.len(),
    call_names.len(),
  ));
  let mut rets_rows: Vec<(&str, &str)> = Vec::new();
  let mut param_rows: Vec<(u64, Box<[Box<str>]>)> = Vec::new();
  let mut decoded: Vec<(String, Vec<u8>)> = Vec::with_capacity(closure_paths.len() + 1);
  let mut sorted_paths: Vec<String> = closure_paths.into_iter().collect();
  sorted_paths.sort_unstable();
  for other in sorted_paths {
    let Some(bytes) = products.product(&other) else {
      return Err(io::Error::other(format!("scoped: no packed product for {other}")));
    };
    decoded.push((other, bytes));
  }
  // The edited file's own ledgers come from the FRESH view (its packed product is prior).
  let mut decoded_views: Vec<(&str, u64, ProductView<'_>)> = Vec::new();
  for (other, bytes) in &decoded {
    let other_view = decode_product_view(bytes)
      .map_err(|err| io::Error::other(format!("scoped: product decode ({other}): {err}")))?;
    let rel_key = map
      .files()
      .iter()
      .find(|&&(_, start, _)| {
        kg.node(NodeId::new(start)).is_some_and(|file| file.path == other.as_str())
      })
      .map(|&(key, start, _)| (key, start));
    let Some((_, other_base)) = rel_key else {
      return Err(io::Error::other(format!("scoped: {other} missing from the universe")));
    };
    decoded_views.push((other.as_str(), other_base, other_view));
  }
  for (other_path, other_base, other_view) in &decoded_views {
    for (name, ret) in &other_view.returns {
      rets_rows.push((name, ret));
    }
    if is_python_path(other_path) {
      for (entity_index, params) in other_view.entity_params.iter() {
        let list: Box<[Box<str>]> =
          params.iter().map(|(name, _)| Box::from(*name)).collect();
        if !list.is_empty() {
          param_rows.push((other_base + u64::from(*entity_index), list));
        }
      }
    }
  }
  // The edited file's own rets/params (self-calls, self-chains) from the fresh view.
  for (name, ret) in &view.returns {
    rets_rows.push((name, ret));
  }
  if is_python_path(path) {
    for (entity_index, params) in view.entity_params.iter() {
      let list: Box<[Box<str>]> = params.iter().map(|(name, _)| Box::from(*name)).collect();
      if !list.is_empty() {
        param_rows.push((base + u64::from(*entity_index), list));
      }
    }
  }

  vorpal_kg::phase_stamp("scoped: closure decoded");
  // The chain ledger interns its return-type names NOW, before the table build — the
  // linkers' own order. Owner slots resolve by peek, so every name a comparison can
  // reach (F's ref fields interned above; rets values interned here) must be in the
  // interner FIRST; anything outside that set sentinels identically in both worlds.
  let chain = (!rets_rows.is_empty()).then(|| ChainReturns::build(interner, rets_rows));

  // --- the partial symbol table: rule-for-rule with build_symbol_table_over ---
  let mut table = SymbolTable::new();
  for &(_, start, _) in map.files() {
    let id = NodeId::new(start);
    if let Some(file) = kg.node(id) {
      table.insert_file(interner, file.path, id);
    }
  }
  for name in &names {
    for id in kg.nodes_named(name) {
      let Some(node) = kg.node(id) else { continue };
      if node.kind == SymbolKind::File || node.kind == SymbolKind::Import {
        continue;
      }
      // Owner parity: peek-or-sentinel, exactly like the bulk build — an owner name no
      // reference interned can never match a qualifier, but member-ness must survive.
      let owner = kg.container_of(id).and_then(|cid| {
        let container = kg.node(cid)?;
        (container.kind != SymbolKind::File)
          .then(|| interner.peek(container.name).unwrap_or_else(|| unmatchable_owner(interner)))
      });
      table.insert(
        interner,
        node.name,
        vorpal_resolve::Symbol {
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

  // --- import-binding pre-pass, the file's own bindings only (bindings key on from_path) ---
  let qualified: Vec<Reference<'_>> = references
    .iter()
    .filter(|r| r.kind == RefKind::Import && r.form == RefForm::Static)
    .copied()
    .collect();
  seed_import_bindings(interner, &mut table, &qualified, resolver);

  // --- resolution: the pipeline's own chunk kernel, chain-aware ---
  vorpal_kg::phase_stamp("scoped: table ready");
  let (resolved, unresolved, stats) =
    resolve_batch(interner, &table, &references, resolver, chain.as_ref());
  vorpal_kg::phase_stamp(&format!("scoped: resolved {} refs", references.len()));

  // --- emission: the per-src slab segment, mirror of the linkers' shared shape ---
  let arg_join = ArgJoin::from_records(args);
  let param_table = ParamTable::from_rows(param_rows);
  let mut evidence: Vec<vorpal_kg::EvidenceRow> =
    Vec::with_capacity(resolved.len() + unresolved.len());
  let mut edges: Vec<(u32, u32, vorpal_kg::EdgeType)> = Vec::with_capacity(resolved.len());
  let mut flows: Vec<vorpal_kg::DataflowRow> = Vec::new();
  let mut flow_pairs: HashSet<(u64, u64)> = HashSet::new();
  for edge in &resolved {
    let (from, to) = (edge.from.raw() as u32, edge.to.raw() as u32);
    edges.push((from, to, edge.edge.with_confidence(edge.confidence)));
    if edge.edge.base() == vorpal_kg::EdgeType::CALLS {
      join_call_edge(
        edge.from.raw(),
        edge.to.raw(),
        edge.span,
        edge.confidence,
        &arg_join,
        &param_table,
        &mut flow_pairs,
        &mut flows,
        |flow_etype| edges.push((from, to, flow_etype)),
      );
    }
    let (alt_ids, alt_count) = edge.alternatives;
    evidence.push(vorpal_kg::EvidenceRow {
      from,
      to,
      name_hash: edge.name_hash,
      etype: edge.edge.base().0,
      reason: edge.reason as u8,
      confidence: edge.confidence,
      outcome: vorpal_kg::EvidenceOutcome::Edge,
      candidates: edge.candidates,
      span_start: edge.span.0,
      span_end: edge.span.1,
      alternatives: alt_ids[..alt_count as usize].to_vec(),
    });
  }
  for miss in &unresolved {
    evidence.push(vorpal_kg::EvidenceRow {
      from: miss.from.raw() as u32,
      to: vorpal_kg::NO_EDGE,
      name_hash: miss.name_hash,
      etype: miss.etype.base().0,
      reason: 0,
      confidence: 0,
      outcome: if miss.external {
        vorpal_kg::EvidenceOutcome::External
      } else {
        vorpal_kg::EvidenceOutcome::Masked
      },
      candidates: miss.candidates,
      span_start: miss.span.0,
      span_end: miss.span.1,
      alternatives: Vec::new(),
    });
  }

  // --- request/notify tail: the file's sites against the GLOBAL route set (defs-stable
  // keeps it identical to the prior build's), id-ascending like the bulk collection ---
  let mut request_edges: Vec<(u32, u32, vorpal_kg::EdgeType)> = Vec::new();
  if !req_rows.is_empty() {
    let mut routes: Vec<(u64, String)> = Vec::new();
    for id in 0..kg.node_count() as u64 {
      let node_id = NodeId::new(id);
      if let Some(node) = kg.node(node_id) {
        if matches!(node.kind, SymbolKind::Route | SymbolKind::Channel) {
          routes.push((id, node.name.to_string()));
        }
      }
    }
    let (matched, _report) = match_requests(&routes, &req_rows);
    for &(from, to, confidence) in &matched.requests {
      request_edges.push((
        from as u32,
        to as u32,
        vorpal_kg::EdgeType::REQUESTS.with_confidence(confidence),
      ));
    }
    for &(from, to, confidence) in &matched.notifies {
      request_edges.push((
        from as u32,
        to as u32,
        vorpal_kg::EdgeType::NOTIFIES.with_confidence(confidence),
      ));
    }
  }

  // --- fresh sketch rows: the sigs family's replacement run for this file ---
  let sigs: Vec<SigRow> = view
    .signatures
    .iter()
    .filter_map(|sig| {
      let ord = layout_ords.get(sig.entity_index as usize).copied()?;
      let sketch = <[u8; crate::signature::BINS]>::try_from(sig.sketch).ok()?;
      Some(SigRow {
        node: base + ord,
        shingles: sig.shingles,
        sketch,
      })
    })
    .collect();

  Ok(ScopedOutcome {
    evidence,
    edges,
    request_edges,
    flows,
    sigs,
    stats,
  })
}

/// The global near-clone pair set of a sealed graph, extracted from its adjacency:
/// every `SIMILAR_TO` edge once (`a < b`), with its confidence label. This is the PRIOR
/// side of the scoped pairing diff — read from the sealed truth rather than re-paired,
/// so the diff can never disagree with the bytes it is patching.
pub fn similar_pairs_of_kg(kg: &Kg) -> Vec<(u64, u64, u8)> {
  // Delegates to the graph-side zero-allocation walk (`Kg::similar_pairs`): the
  // per-node `out_neighbors` Vec was ~9M transient allocations at kernel scale.
  kg.similar_pairs()
}

/// The scoped pairing repair (P4.5c-2, slice ii). Near-clone pairing is GLOBAL — LSH
/// banding, star caps, and partner limits act over the ENTIRE sketch ledger, and the
/// candidate ceiling makes even bucket enumeration order-dependent — so the repair
/// re-pairs the full row set with the edited file's run swapped in, in the ledger's
/// canonical (bucket, file, ordinal) order (exactly `SigStore::rows` order, exactly the
/// order the pipeline's stream feeds). The DIFF against the prior pair set names every
/// source node whose similar segment must be rewritten; an empty diff — the common body
/// edit, near-clone of nothing — leaves every other bucket's edges byte-carried.
pub struct SimilarRepair {
  /// The new global pair set, `(a, b, confidence)` with `a < b`, sorted.
  pub fresh_pairs: Vec<(u64, u64, u8)>,
  /// Every endpoint of every added, removed, or relabeled pair — ascending, deduped.
  pub changed_srcs: Vec<u32>,
  /// The full swapped row set in canonical order — the sigs family's new content, handed
  /// back so the compose persists exactly what was paired.
  pub swapped_rows: Vec<SigRow>,
}

/// `live_files` is the MANIFEST's live entry count — the bucket law's one input
/// (never derived from node-bearing files, which drift under parse-health Exclude).
pub fn scoped_similar_repair(
  map: &vorpal_kg::NodeIdMap,
  live_files: usize,
  prior_rows: &[SigRow],
  prior_pairs: &[(u64, u64, u8)],
  file_key: u64,
  fresh_file_sigs: &[SigRow],
) -> io::Result<SimilarRepair> {
  // Swap the edited file's run in place: prior rows are canonically sorted and a file's
  // rows are contiguous at its (bucket, key) position, so splicing the fresh run at the
  // old run's position preserves the global feed order the ceiling depends on.
  let mut rows: Vec<SigRow> = Vec::with_capacity(prior_rows.len() + fresh_file_sigs.len());
  let mut spliced = false;
  for row in prior_rows {
    let Some((key, _)) = map.locate(
      u32::try_from(row.node)
        .map_err(|_| io::Error::other("scoped: sig row outside the dense space"))?,
    ) else {
      return Err(io::Error::other("scoped: sig row outside the prior universe"));
    };
    if key == file_key {
      if !spliced {
        rows.extend(fresh_file_sigs.iter().cloned());
        spliced = true;
      }
      continue; // the old run is replaced wholesale
    }
    rows.push(row.clone());
  }
  if !spliced {
    // The file had no signed definitions before: its fresh run enters at its canonical
    // (bucket, key) position among the survivors.
    let position = rows
      .partition_point(|row| {
        map
          .locate(row.node as u32)
          .map(|(key, _)| {
            let buckets = u64::from(vorpal_kg::identity::bucket_count_for(live_files));
            let row_bucket = key & (buckets - 1);
            let file_bucket = file_key & (buckets - 1);
            (row_bucket, key) < (file_bucket, file_key)
          })
          .unwrap_or(false)
      });
    let tail = rows.split_off(position);
    rows.extend(fresh_file_sigs.iter().cloned());
    rows.extend(tail);
  }
  let (mut fresh_pairs, _report, swapped_rows) = crate::similar::similar_pairs(rows);
  fresh_pairs.sort_unstable();

  // Symmetric difference on (a, b, confidence): a relabeled pair appears on both sides
  // with different confidences, contributing its endpoints exactly once each side.
  let mut changed: Vec<u32> = Vec::new();
  let (mut i, mut j) = (0usize, 0usize);
  while i < prior_pairs.len() || j < fresh_pairs.len() {
    match (prior_pairs.get(i), fresh_pairs.get(j)) {
      (Some(old), Some(new)) if old == new => {
        i += 1;
        j += 1;
      }
      (Some(old), Some(new)) if old < new => {
        changed.push(old.0 as u32);
        changed.push(old.1 as u32);
        i += 1;
      }
      (Some(_), Some(new)) => {
        changed.push(new.0 as u32);
        changed.push(new.1 as u32);
        j += 1;
      }
      (Some(old), None) => {
        changed.push(old.0 as u32);
        changed.push(old.1 as u32);
        i += 1;
      }
      (None, Some(new)) => {
        changed.push(new.0 as u32);
        changed.push(new.1 as u32);
        j += 1;
      }
      (None, None) => unreachable!("loop condition"),
    }
  }
  changed.sort_unstable();
  changed.dedup();
  Ok(SimilarRepair {
    fresh_pairs,
    changed_srcs: changed,
    swapped_rows,
  })
}

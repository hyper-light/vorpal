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
  decode_cap: usize,
) -> io::Result<ScopedOutcome> {
  // --- the file's dense range: base + row count, from the nodes TOC file table ---
  let Some(&(_, base, rows)) = map.files().iter().find(|&&(key, _, _)| key == file_key) else {
    return Err(io::Error::other("scoped: file outside the prior universe"));
  };
  // Layout parity: [File] + items + members. A mismatch means the ladder mis-judged —
  // refuse rather than mis-attribute a single reference.
  let layout_len = 1 + view.items.iter().map(|item| 1 + item.members.len()).sum::<usize>();
  if layout_len as u32 != rows {
    return Err(io::Error::other("scoped: layout drift against the prior node rows"));
  }

  // --- pass 1 (interning parity with the pipeline's two-pass shape): references first ---
  let path_id = interner.intern(path);
  let mut references: Vec<Reference<'_>> = Vec::with_capacity(view.refs.len());
  let mut args: Vec<ArgRec> = Vec::new();
  let mut req_rows: Vec<ReqRow> = Vec::new();
  for r in &view.refs {
    if r.from_entity_index >= rows {
      return Err(io::Error::other("scoped: reference outside the file layout"));
    }
    let from = NodeId::new(base + u64::from(r.from_entity_index));
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
    if req.from_entity_index >= rows {
      continue; // corrupt product index — the apply kernel drops these identically
    }
    req_rows.push(ReqRow {
      from: base + u64::from(req.from_entity_index),
      method: Box::from(req.method),
      path: Box::from(req.path),
      span: (req.start, req.end),
    });
  }

  // --- the name closure: every table lookup the file's resolution can perform ---
  let mut names: HashSet<&str> = HashSet::new();
  let mut call_names: HashSet<&str> = HashSet::new();
  for r in &view.refs {
    names.insert(r.name);
    if let Some(receiver_type) = r.receiver_type {
      names.insert(receiver_type);
    }
    if crate::product::tag_refkind(r.kind) == vorpal_resolve::RefKind::Call {
      call_names.insert(r.name);
    }
  }

  // --- bounded closure: decode the files defining any called name, for rets + params ---
  // Chain resolution consults rets[NAME] only for names the file calls, and every rets
  // entry for such a name lives in a file defining it. Params bind arguments only at
  // RESOLVED callees — a subset of the same files. One decode round covers both.
  let mut closure_paths: HashSet<String> = HashSet::new();
  for name in &call_names {
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
  if closure_paths.len() > decode_cap {
    return Err(io::Error::other(format!(
      "scoped: closure of {} files exceeds the decode cap ({decode_cap})",
      closure_paths.len(),
    )));
  }
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
      names.insert(ret);
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
    names.insert(ret);
  }
  if is_python_path(path) {
    for (entity_index, params) in view.entity_params.iter() {
      let list: Box<[Box<str>]> = params.iter().map(|(name, _)| Box::from(*name)).collect();
      if !list.is_empty() {
        param_rows.push((base + u64::from(*entity_index), list));
      }
    }
  }

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
  let chain = (!rets_rows.is_empty()).then(|| ChainReturns::build(interner, rets_rows));
  let (resolved, unresolved, stats) =
    resolve_batch(interner, &table, &references, resolver, chain.as_ref());

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
    .filter(|sig| sig.entity_index < rows)
    .filter_map(|sig| {
      let sketch = <[u8; crate::signature::BINS]>::try_from(sig.sketch).ok()?;
      Some(SigRow {
        node: base + u64::from(sig.entity_index),
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

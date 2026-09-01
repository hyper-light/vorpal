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

use std::collections::{HashMap, HashSet};
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

// ---------------------------------------------------------------------------------------
// The session core: ONE resolution path for the defs-stable (single-file, prior universe)
// and defs-changed (multi-file, overlay universe) composes. The universe abstraction owns
// exactly the three questions resolution asks of the world — candidates by name, the file
// registry, the route set — every other behavior is shared code, so the two lanes cannot
// drift from each other, and the scoped oracle holds them both to the pipeline.
// ---------------------------------------------------------------------------------------

/// One candidate definition as the table sees it, in the SESSION's dense space.
pub(crate) struct CandidateFacts {
  pub id: u64,
  pub kind: SymbolKind,
  pub path: String,
  pub exported: bool,
  /// The containing definition's NAME (never a File) — owner ids resolve by peek at
  /// table build, exactly like `build_symbol_table_over`.
  pub owner: Option<String>,
}

pub(crate) trait UniverseView {
  /// Candidates for `name`, ascending by id — the bulk build's insertion order.
  fn candidates_named(&self, name: &str) -> Vec<CandidateFacts>;
  /// Every file's `(File-node id, path)` in dense order.
  fn all_file_entries(&self) -> Vec<(u64, String)>;
  /// Every Route/Channel definition `(id, name)`, ascending by id.
  fn routes(&self) -> Vec<(u64, String)>;
  /// A file's dense start by its path (the closure's param-ledger anchor).
  fn file_start_by_path(&self, path: &str) -> Option<u64>;
}

/// The prior sealed generation as-is: the defs-stable lane's universe.
pub(crate) struct PriorUniverse<'k> {
  kg: &'k Kg,
  map: &'k vorpal_kg::NodeIdMap,
}

impl UniverseView for PriorUniverse<'_> {
  fn candidates_named(&self, name: &str) -> Vec<CandidateFacts> {
    let mut facts = Vec::new();
    for id in self.kg.nodes_named(name) {
      let Some(node) = self.kg.node(id) else { continue };
      if node.kind == SymbolKind::File || node.kind == SymbolKind::Import {
        continue;
      }
      let owner = self.kg.container_of(id).and_then(|cid| {
        let container = self.kg.node(cid)?;
        (container.kind != SymbolKind::File).then(|| container.name.to_string())
      });
      facts.push(CandidateFacts {
        id: id.raw(),
        kind: node.kind,
        path: node.path.to_string(),
        exported: node.exported,
        owner,
      });
    }
    facts
  }

  fn all_file_entries(&self) -> Vec<(u64, String)> {
    let mut entries = Vec::with_capacity(self.map.files().len());
    for &(_, start, _) in self.map.files() {
      if let Some(file) = self.kg.node(NodeId::new(start)) {
        entries.push((start, file.path.to_string()));
      }
    }
    entries
  }

  fn routes(&self) -> Vec<(u64, String)> {
    let mut routes = Vec::new();
    for id in 0..self.kg.node_count() as u64 {
      if let Some(node) = self.kg.node(NodeId::new(id)) {
        if matches!(node.kind, SymbolKind::Route | SymbolKind::Channel) {
          routes.push((id, node.name.to_string()));
        }
      }
    }
    routes
  }

  fn file_start_by_path(&self, path: &str) -> Option<u64> {
    self
      .map
      .files()
      .iter()
      .find(|&&(_, start, _)| {
        self.kg.node(NodeId::new(start)).is_some_and(|file| file.path == path)
      })
      .map(|&(_, start, _)| start)
  }
}

/// The prior universe with ONE file's definitions swapped for a scratch seal's, every
/// dense id past the old block shifted by the row delta — the defs-changed lane. The
/// shift law is total: bucket-major order is a single sequence, so
/// `translate(x) = x + (x ≥ old_end) · delta`, and (file_key, ordinal) coordinates of
/// every OTHER file are untouched.
pub(crate) struct OverlayUniverse<'k> {
  pub prior: &'k Kg,
  pub map: &'k vorpal_kg::NodeIdMap,
  pub file_key: u64,
  pub file_path: &'k str,
  pub old_start: u64,
  pub old_end: u64,
  pub delta: i64,
  /// The edited file's scratch single-file seal: row ordinals ARE the new file-local
  /// ordinals; dense ids = `old_start + ordinal` (files before it never shift).
  pub fresh: &'k Kg,
}

impl OverlayUniverse<'_> {
  pub(crate) fn translate(&self, prior_dense: u64) -> Option<u64> {
    if (self.old_start..self.old_end).contains(&prior_dense) {
      return None; // the old block is replaced wholesale
    }
    if prior_dense >= self.old_end {
      return Some((prior_dense as i64 + self.delta) as u64);
    }
    Some(prior_dense)
  }
}

impl UniverseView for OverlayUniverse<'_> {
  fn candidates_named(&self, name: &str) -> Vec<CandidateFacts> {
    let mut facts = Vec::new();
    for id in self.prior.nodes_named(name) {
      let Some(new_id) = self.translate(id.raw()) else { continue };
      let Some(node) = self.prior.node(id) else { continue };
      if node.kind == SymbolKind::File || node.kind == SymbolKind::Import {
        continue;
      }
      let owner = self.prior.container_of(id).and_then(|cid| {
        let container = self.prior.node(cid)?;
        (container.kind != SymbolKind::File).then(|| container.name.to_string())
      });
      facts.push(CandidateFacts {
        id: new_id,
        kind: node.kind,
        path: node.path.to_string(),
        exported: node.exported,
        owner,
      });
    }
    for ord in 0..self.fresh.node_count() as u64 {
      let id = NodeId::new(ord);
      let Some(node) = self.fresh.node(id) else { continue };
      if node.name != name || node.kind == SymbolKind::File || node.kind == SymbolKind::Import
      {
        continue;
      }
      let owner = self.fresh.container_of(id).and_then(|cid| {
        let container = self.fresh.node(cid)?;
        (container.kind != SymbolKind::File).then(|| container.name.to_string())
      });
      facts.push(CandidateFacts {
        id: self.old_start + ord,
        kind: node.kind,
        path: self.file_path.to_string(),
        exported: node.exported,
        owner,
      });
    }
    facts.sort_by_key(|f| f.id);
    facts
  }

  fn all_file_entries(&self) -> Vec<(u64, String)> {
    let mut entries = Vec::with_capacity(self.map.files().len());
    for &(key, start, _) in self.map.files() {
      if key == self.file_key {
        entries.push((self.old_start, self.file_path.to_string()));
        continue;
      }
      let Some(new_start) = self.translate(start) else { continue };
      if let Some(file) = self.prior.node(NodeId::new(start)) {
        entries.push((new_start, file.path.to_string()));
      }
    }
    entries
  }

  fn routes(&self) -> Vec<(u64, String)> {
    let mut routes = Vec::new();
    for id in 0..self.prior.node_count() as u64 {
      let Some(new_id) = self.translate(id) else { continue };
      if let Some(node) = self.prior.node(NodeId::new(id))
        && matches!(node.kind, SymbolKind::Route | SymbolKind::Channel)
      {
        routes.push((new_id, node.name.to_string()));
      }
    }
    for ord in 0..self.fresh.node_count() as u64 {
      if let Some(node) = self.fresh.node(NodeId::new(ord))
        && matches!(node.kind, SymbolKind::Route | SymbolKind::Channel)
      {
        routes.push((self.old_start + ord, node.name.to_string()));
      }
    }
    routes.sort_by_key(|r| r.0);
    routes
  }

  fn file_start_by_path(&self, path: &str) -> Option<u64> {
    if path == self.file_path {
      return Some(self.old_start);
    }
    self
      .map
      .files()
      .iter()
      .find(|&&(_, start, _)| {
        self.prior.node(NodeId::new(start)).is_some_and(|file| file.path == path)
      })
      .and_then(|&(_, start, _)| self.translate(start))
  }
}

/// One file of a scoped session, in the SESSION's dense space.
pub struct SessionFile<'a> {
  pub path: String,
  /// The file's dense start in the session space.
  pub base: u64,
  /// The file's row count in the session space (post-edit for the edited file).
  pub rows: u32,
  pub view: &'a ProductView<'a>,
  pub layout_ords: &'a [u64],
}

struct CollectedFile<'i> {
  references: Vec<Reference<'i>>,
  args: Vec<ArgRec>,
  req_rows: Vec<ReqRow>,
  sigs: Vec<SigRow>,
}

/// The whole session: collect → bounded closure → chain → table → bindings → per-file
/// resolve + emit. Outcomes come back in `files` order, each in the session dense space.
fn resolve_session(
  interner: &Interner,
  universe: &dyn UniverseView,
  resolver: &Resolver,
  products: &dyn ProductSource,
  files: &[SessionFile<'_>],
  decode_cap: usize,
) -> io::Result<Vec<ScopedOutcome>> {
  // --- pass 1 per file (interning parity: references first) + the session name sets ---
  let mut collected: Vec<CollectedFile<'_>> = Vec::with_capacity(files.len());
  let mut names: HashSet<&str> = HashSet::new();
  let mut call_names: HashSet<&str> = HashSet::new();
  let mut chain_keys: HashSet<&str> = HashSet::new();
  for file in files {
    let layout_len =
      1 + file.view.items.iter().map(|item| 1 + item.members.len()).sum::<usize>();
    if file.layout_ords.len() != layout_len {
      return Err(io::Error::other(format!(
        "scoped: layout mapping length {} != fresh layout {layout_len}",
        file.layout_ords.len(),
      )));
    }
    if file.layout_ords.iter().any(|&ord| ord >= u64::from(file.rows)) {
      return Err(io::Error::other("scoped: layout mapping outside the file's rows"));
    }
    let ord_of = |index: u32| -> io::Result<u64> {
      file
        .layout_ords
        .get(index as usize)
        .copied()
        .ok_or_else(|| io::Error::other("scoped: entity index outside the file layout"))
    };
    let path_id = interner.intern(&file.path);
    let mut references: Vec<Reference<'_>> = Vec::with_capacity(file.view.refs.len());
    let mut args: Vec<ArgRec> = Vec::new();
    let mut req_rows: Vec<ReqRow> = Vec::new();
    for r in &file.view.refs {
      let from = NodeId::new(file.base + ord_of(r.from_entity_index)?);
      if crate::product::tag_refkind(r.kind) == vorpal_resolve::RefKind::Call
        && r.args_len() > 0
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
    for req in &file.view.requests {
      let Ok(from_ord) = ord_of(req.from_entity_index) else {
        continue; // corrupt product index — the apply kernel drops these identically
      };
      req_rows.push(ReqRow {
        from: file.base + from_ord,
        method: Box::from(req.method),
        path: Box::from(req.path),
        span: (req.start, req.end),
      });
    }
    let sigs: Vec<SigRow> = file
      .view
      .signatures
      .iter()
      .filter_map(|sig| {
        let ord = file.layout_ords.get(sig.entity_index as usize).copied()?;
        let sketch = <[u8; crate::signature::BINS]>::try_from(sig.sketch).ok()?;
        Some(SigRow {
          node: file.base + ord,
          shingles: sig.shingles,
          sketch,
        })
      })
      .collect();
    collected.push(CollectedFile {
      references,
      args,
      req_rows,
      sigs,
    });
  }

  // --- bounded closure, split by CONSUMER (each half decodes only what its reader can
  // ever consult): rets ← files defining a chain key; params ← PYTHON files defining a
  // called name (`is_python_path` gates the ledger at ingest — a C corpus decodes
  // nothing at all here, measured 542 dead decodes/1.4 s before this split). Session
  // files never decode: their views are already in hand.
  let session_paths: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
  let mut closure_paths: HashSet<String> = HashSet::new();
  for name in &chain_keys {
    for facts in universe.candidates_named(name) {
      if !session_paths.contains(facts.path.as_str()) {
        closure_paths.insert(facts.path);
      }
    }
  }
  for name in &call_names {
    for facts in universe.candidates_named(name) {
      if !session_paths.contains(facts.path.as_str()) && is_python_path(&facts.path) {
        closure_paths.insert(facts.path);
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
    "scoped: closure {} files over {} call names ({} session files)",
    closure_paths.len(),
    call_names.len(),
    files.len(),
  ));
  let mut rets_rows: Vec<(&str, &str)> = Vec::new();
  let mut param_rows: Vec<(u64, Box<[Box<str>]>)> = Vec::new();
  let mut decoded: Vec<(String, Vec<u8>)> = Vec::with_capacity(closure_paths.len());
  let mut sorted_paths: Vec<String> = closure_paths.into_iter().collect();
  sorted_paths.sort_unstable();
  for other in sorted_paths {
    let Some(bytes) = products.product(&other) else {
      return Err(io::Error::other(format!("scoped: no packed product for {other}")));
    };
    decoded.push((other, bytes));
  }
  let mut decoded_views: Vec<(&str, u64, ProductView<'_>)> = Vec::new();
  for (other, bytes) in &decoded {
    let other_view = decode_product_view(bytes)
      .map_err(|err| io::Error::other(format!("scoped: product decode ({other}): {err}")))?;
    let Some(other_base) = universe.file_start_by_path(other) else {
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
  // The session files' own ledgers come from their in-hand views (the edited file's is
  // FRESH; dirty files' are the pack's, decoded by the caller).
  for file in files {
    for (name, ret) in &file.view.returns {
      rets_rows.push((name, ret));
    }
    if is_python_path(&file.path) {
      for (entity_index, params) in file.view.entity_params.iter() {
        let list: Box<[Box<str>]> =
          params.iter().map(|(name, _)| Box::from(*name)).collect();
        if !list.is_empty() {
          param_rows.push((file.base + u64::from(*entity_index), list));
        }
      }
    }
  }

  vorpal_kg::phase_stamp("scoped: closure decoded");
  // The chain ledger interns its return-type names NOW, before the table build — the
  // linkers' own order. Owner slots resolve by peek, so every name a comparison can
  // reach (session ref fields interned above; rets values interned here) must be in
  // the interner FIRST; anything outside that set sentinels identically in both worlds.
  let chain = (!rets_rows.is_empty()).then(|| ChainReturns::build(interner, rets_rows));

  // --- the partial symbol table: rule-for-rule with build_symbol_table_over ---
  let mut table = SymbolTable::new();
  for (id, path) in universe.all_file_entries() {
    table.insert_file(interner, &path, NodeId::new(id));
  }
  for name in &names {
    for facts in universe.candidates_named(name) {
      // Owner parity: peek-or-sentinel, exactly like the bulk build — an owner name no
      // reference interned can never match a qualifier, but member-ness must survive.
      let owner = facts.owner.as_deref().map(|owner_name| {
        interner.peek(owner_name).unwrap_or_else(|| unmatchable_owner(interner))
      });
      table.insert(
        interner,
        name,
        vorpal_resolve::Symbol {
          id: NodeId::new(facts.id),
          kind: facts.kind,
          path: interner.intern(&facts.path),
          exported: facts.exported,
          owner,
        },
      );
    }
  }
  table.finalize();

  // --- import-binding pre-pass over EVERY session file (bindings key on from_path,
  // exactly the bulk's one-table shape) ---
  let qualified: Vec<Reference<'_>> = collected
    .iter()
    .flat_map(|c| c.references.iter())
    .filter(|r| r.kind == RefKind::Import && r.form == RefForm::Static)
    .copied()
    .collect();
  seed_import_bindings(interner, &mut table, &qualified, resolver);

  // --- per-file resolution + emission through the pipeline's own kernels ---
  vorpal_kg::phase_stamp("scoped: table ready");
  let param_table = ParamTable::from_rows(param_rows);
  let mut routes: Option<Vec<(u64, String)>> = None;
  let mut outcomes = Vec::with_capacity(files.len());
  for collected_file in collected {
    let CollectedFile {
      references,
      args,
      req_rows,
      sigs,
    } = collected_file;
    let (resolved, unresolved, stats) =
      resolve_batch(interner, &table, &references, resolver, chain.as_ref());
    let arg_join = ArgJoin::from_records(args);
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
    let mut request_edges: Vec<(u32, u32, vorpal_kg::EdgeType)> = Vec::new();
    if !req_rows.is_empty() {
      let routes = routes.get_or_insert_with(|| universe.routes());
      let (matched, _report) = match_requests(routes, &req_rows);
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
    outcomes.push(ScopedOutcome {
      evidence,
      edges,
      request_edges,
      flows,
      sigs,
      stats,
    });
  }
  Ok(outcomes)
}

/// Re-resolve one defs-stable file against the prior generation — a one-file session
/// over [`PriorUniverse`]. `products` serves the bounded chain/param closure;
/// `decode_cap` bounds it (past it the caller escalates to the full pipeline, loudly).
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
  let Some(&(_, base, rows)) = map.files().iter().find(|&&(key, _, _)| key == file_key)
  else {
    return Err(io::Error::other("scoped: file outside the prior universe"));
  };
  let universe = PriorUniverse { kg, map };
  let files = [SessionFile {
    path: path.to_string(),
    base,
    rows,
    view,
    layout_ords,
  }];
  let mut outcomes = resolve_session(interner, &universe, resolver, products, &files, decode_cap)?;
  outcomes
    .pop()
    .ok_or_else(|| io::Error::other("scoped: session produced no outcome"))
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

/// The defs-changed ladder (P4.5c-3): what may NOT change when the definition set does.
/// Bodies, references, sketches, request sites, and the definitions themselves are free;
/// the parse identity and error accounting are not (slice-1 posture — error-delta edits
/// escalate, recorded in SUBSECOND.md), and Route/Channel definition changes escalate at
/// the caller (request matching is URL-keyed; the usage family cannot bound its dirty
/// set).
pub fn views_defs_changed_reject(
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
  None
}

/// Every definition name of the edited file whose row EVIDENCE moved between the prior
/// block and the fresh seal: the per-name sequence of
/// `(ordinal, kind, signature, exported, eid)` differs — adds, removes, renames,
/// signature/export changes, AND pure ordinal shifts (a def added above an unchanged one
/// moves its `(file_key, ordinal)` coordinate, so every referrer's bytes move even
/// though semantics do not). `usage[name]` over this set IS the byte-impact closure.
pub fn affected_def_names(
  prior: &Kg,
  old_start: u64,
  old_rows: u32,
  fresh: &Kg,
) -> Vec<String> {
  type RowFacts = (u64, SymbolKind, String, bool, Option<String>);
  let mut old_by_name: HashMap<String, Vec<RowFacts>> = HashMap::new();
  for ord in 0..u64::from(old_rows) {
    if let Some(node) = prior.node(NodeId::new(old_start + ord)) {
      old_by_name.entry(node.name.to_string()).or_default().push((
        ord,
        node.kind,
        node.signature.to_string(),
        node.exported,
        node.external_id.map(|eid| eid.to_string()),
      ));
    }
  }
  let mut new_by_name: HashMap<String, Vec<RowFacts>> = HashMap::new();
  for ord in 0..fresh.node_count() as u64 {
    if let Some(node) = fresh.node(NodeId::new(ord)) {
      new_by_name.entry(node.name.to_string()).or_default().push((
        ord,
        node.kind,
        node.signature.to_string(),
        node.exported,
        node.external_id.map(|eid| eid.to_string()),
      ));
    }
  }
  let mut affected: Vec<String> = Vec::new();
  for (name, old_rows) in &old_by_name {
    if new_by_name.get(name) != Some(old_rows) {
      affected.push(name.clone());
    }
  }
  for name in new_by_name.keys() {
    if !old_by_name.contains_key(name) {
      affected.push(name.clone());
    }
  }
  affected.sort_unstable();
  affected.dedup();
  affected
}

/// Whether any definition among the prior block or the fresh seal is a Route/Channel —
/// the defs-changed escalation for URL-keyed request matching whenever such a def is
/// AFFECTED (the caller intersects with `affected_def_names`; a stable route is fine).
pub fn def_kinds_of(kg: &Kg, start: u64, rows: u32) -> HashMap<String, Vec<SymbolKind>> {
  let mut kinds: HashMap<String, Vec<SymbolKind>> = HashMap::new();
  for ord in 0..u64::from(rows) {
    if let Some(node) = kg.node(NodeId::new(start + ord))
      && node.kind != SymbolKind::File
    {
      kinds.entry(node.name.to_string()).or_default().push(node.kind);
    }
  }
  kinds
}

/// One dirty file's inputs for the defs-changed session — the caller decodes the pack
/// product and derives the layout bridge through the writer's own collapse
/// (`Ingestor::ingest_product_mapped`).
pub struct DirtyFileInput<'a> {
  pub path: String,
  pub file_key: u64,
  pub view: &'a ProductView<'a>,
  pub layout_ords: &'a [u64],
}

/// Resolve the defs-changed closure: the edited file (against its fresh scratch seal)
/// plus every usage-dirty file, all in the SUCCESSOR dense space (the shift law:
/// `translate(x) = x + (x ≥ old_end) · delta`). Outcomes come back edited-first, then in
/// `dirty` order. The c2-i machinery underneath is unchanged — one session, one table,
/// the pipeline's own kernels.
#[allow(clippy::too_many_arguments)] // the one defs-changed entry: every input is load-bearing
pub fn resolve_defs_changed(
  interner: &Interner,
  prior_kg: &Kg,
  prior_map: &vorpal_kg::NodeIdMap,
  resolver: &Resolver,
  products: &dyn ProductSource,
  edited: &DirtyFileInput<'_>,
  fresh_kg: &Kg,
  dirty: &[DirtyFileInput<'_>],
  decode_cap: usize,
) -> io::Result<Vec<ScopedOutcome>> {
  let Some(&(_, old_start, old_rows)) = prior_map
    .files()
    .iter()
    .find(|&&(key, _, _)| key == edited.file_key)
  else {
    return Err(io::Error::other("scoped: edited file outside the prior universe"));
  };
  let new_rows = u32::try_from(fresh_kg.node_count())
    .map_err(|_| io::Error::other("scoped: fresh seal beyond the row space"))?;
  let universe = OverlayUniverse {
    prior: prior_kg,
    map: prior_map,
    file_key: edited.file_key,
    file_path: &edited.path,
    old_start,
    old_end: old_start + u64::from(old_rows),
    delta: i64::from(new_rows) - i64::from(old_rows),
    fresh: fresh_kg,
  };
  let mut files: Vec<SessionFile<'_>> = Vec::with_capacity(1 + dirty.len());
  files.push(SessionFile {
    path: edited.path.clone(),
    base: old_start,
    rows: new_rows,
    view: edited.view,
    layout_ords: edited.layout_ords,
  });
  for input in dirty {
    let Some(&(_, prior_start, rows)) = prior_map
      .files()
      .iter()
      .find(|&&(key, _, _)| key == input.file_key)
    else {
      return Err(io::Error::other("scoped: dirty file outside the prior universe"));
    };
    let Some(base) = universe.translate(prior_start) else {
      return Err(io::Error::other("scoped: dirty file overlaps the edited block"));
    };
    files.push(SessionFile {
      path: input.path.clone(),
      base,
      rows,
      view: input.view,
      layout_ords: input.layout_ords,
    });
  }
  resolve_session(interner, &universe, resolver, products, &files, decode_cap)
}

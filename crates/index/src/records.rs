//! Typed query results (IMPROVEMENTS #7): the single serde record schema every machine
//! surface serializes — MCP `structuredContent` today, the Node/Python index sessions next
//! (#8). The rendered-text surfaces stay byte-stable for humans; these records are the
//! contract for programs, so every field is explicit (ids, durable ids, grades, spans) and
//! nothing requires parsing prose.
//!
//! Selector-driven queries return [`Selected`], preserving the ambiguity semantics of the
//! rendered surfaces: `NoMatch` and `Ambiguous` are answers, not errors — an ambiguous name
//! hands back the candidate records to refine with (`path`/`kind`/`id`/`eid`).

use serde::Serialize;

use vorpal_kg::{Kg, NodeId};

use crate::{GraphTarget, resolve_target};

/// One graph node, fully identified: dense id (this generation), durable external id
/// (`eid:<32 hex>`, cross-generation), and the definition's location.
#[derive(Serialize, Clone, Debug)]
pub struct NodeRecord {
  pub id: u64,
  pub external_id: Option<String>,
  pub name: String,
  pub kind: String,
  pub path: String,
  pub exported: bool,
  /// Definition byte range in `path`; `[0, 0]` when unknown (File nodes, pre-span segments).
  pub span: [u32; 2],
  pub signature: String,
  /// Conservative path classification: source | test | vendored | generated.
  pub class: String,
  /// Total in/out edges (containment included) — populated on identity listings (`node`),
  /// where degree is the question; absent elsewhere to keep pages lean.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub in_degree: Option<u64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub out_degree: Option<u64>,
}

/// A node related to the query target through one edge, with the edge's resolution grade
/// (`structural` for containment edges, else exact/constrained/heuristic). For `similar_to`
/// edges the confidence IS the estimated similarity, surfaced as a percentage.
#[derive(Serialize, Debug)]
pub struct RelatedRecord {
  #[serde(flatten)]
  pub node: NodeRecord,
  pub grade: String,
  /// Estimated Jaccard similarity x 100 (`similar_to` edges only).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub similarity: Option<u8>,
}

/// One step of a relation-restricted traversal: the reached node, its BFS depth, the node it
/// was first reached from, and the edge that reached it.
#[derive(Serialize, Debug)]
pub struct ReachRecord {
  /// For `data_flows` hops with a sidecar: the arguments flowing along this hop, rendered
  /// `expr→param#k` (empty otherwise — absence of a sidecar is stated by the tool text).
  #[serde(skip_serializing_if = "Vec::is_empty", default)]
  pub flow_exprs: Vec<String>,
  #[serde(flatten)]
  pub node: NodeRecord,
  pub depth: u32,
  pub via: u64,
  pub relation: String,
  pub grade: String,
  /// Orientation of the stored edge relative to the BFS tree: `"in"` = it points from this
  /// node toward `via`. Constant for pure in/out traversals; per-hop under direction=both.
  pub edge_direction: String,
}

/// One evidence-sidecar occurrence: an emitted edge (`to` set) or a retained no-edge outcome
/// (`to` null; `outcome` external/masked) — the typed form of `why`.
#[derive(Serialize, Debug)]
pub struct EvidenceRecord {
  pub from: u64,
  pub to: Option<u64>,
  pub relation: String,
  pub outcome: String,
  pub grade: String,
  pub reason: String,
  pub candidates: u32,
  pub span: [u32; 2],
}

/// One hybrid-search hit with its fused score and per-channel provenance.
#[derive(Serialize, Debug)]
pub struct SearchHitRecord {
  #[serde(flatten)]
  pub node: NodeRecord,
  pub score: f32,
  /// Channels that placed this hit, each with its 1-based rank in that channel.
  pub channels: Vec<ChannelRank>,
}

#[derive(Serialize, Debug)]
pub struct ChannelRank {
  pub channel: &'static str,
  pub rank: usize,
  /// Multi-phrase (`"…" AND "…"`) queries only: which phrase (0-based) this channel
  /// placement came from. `None` on single-phrase queries — and serialization omits it,
  /// so single-phrase JSON stays byte-identical to the pre-conjunction surface.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub phrase: Option<usize>,
}

/// Why a conjunctive (multi-phrase AND) query answered the way it did: the parsed phrases,
/// each phrase's fused-pool size at the intersection depth, and — when the intersection
/// emptied — which phrase (0-based, evaluated left-to-right) eliminated every remaining
/// candidate. Absence of results is an answer with a stated reason, never silence.
#[derive(Serialize, Debug)]
pub struct MultiPhraseReport {
  pub phrases: Vec<String>,
  pub per_phrase_pool: Vec<usize>,
  /// The fused-pool depth of the final round: intersection pools deepen iteratively
  /// (`(k·4).max(50)`, ×4 per round, cap 3200) while the conjunction is starved, so a
  /// shallow truncation can never fake an empty intersection.
  pub intersection_depth: usize,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub eliminated_by: Option<usize>,
}

/// A search answer with its conjunction provenance: `hits` is the ranking every surface
/// renders; `multi_phrase` is present iff the query parsed as the `"…" AND "…"` syntax
/// (see `parse_and_phrases`). Single-phrase callers use the `hits`-only shim
/// (`search_records_filtered`) and see no change.
#[derive(Serialize, Debug)]
pub struct SearchReport {
  pub hits: Vec<SearchHitRecord>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub multi_phrase: Option<MultiPhraseReport>,
}

/// One definition's source text, sliced from its persisted byte span and digest-verified
/// against the generation that recorded it — the selector-driven twin of `fetch_span`.
/// One data-flow row answered to queries (G-M3): a traceable argument at a resolved call
/// from `from_name` into `to_name`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowRecord {
  pub from_id: u64,
  pub from_name: String,
  pub to_id: u64,
  pub to_name: String,
  pub to_path: String,
  pub arg_index: u16,
  pub param_index: u16,
  /// var | field-access | call-result.
  pub class: &'static str,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub expr: Option<String>,
  pub span: (u32, u32),
}

fn flow_class_name(class: u8) -> &'static str {
  match class {
    0 => "var",
    1 => "field-access",
    2 => "call-result",
    _ => "other",
  }
}

/// Outgoing data-flow rows for one selected definition, from the `dataflow.bin` sidecar.
/// Absent sidecar (an older generation) answers empty with `sidecar_present = false` — the
/// caller says so instead of implying "no flows exist".
pub fn flow_records(
  kg: &vorpal_kg::Kg,
  kg_dir: &std::path::Path,
  target: &crate::GraphTarget,
) -> Result<(Vec<FlowRecord>, bool), String> {
  let matches = crate::resolve_target(kg, target).map_err(|err| err.to_string())?;
  let store = vorpal_kg::DataflowStore::load(kg_dir).map_err(|err| err.to_string())?;
  let present = !store.is_empty();
  let mut records = Vec::new();
  for &id in &matches {
    for flow in store.flows_from(id.raw() as u32) {
      let to = vorpal_kg::NodeId::new(flow.to as u64);
      let (to_name, to_path) = kg
        .node(to)
        .map(|v| (v.name.to_string(), v.path.to_string()))
        .unwrap_or_default();
      let from_name = kg
        .node(id)
        .map(|v| v.name.to_string())
        .unwrap_or_default();
      records.push(FlowRecord {
        from_id: id.raw(),
        from_name,
        to_id: flow.to as u64,
        to_name,
        to_path,
        arg_index: flow.arg_index,
        param_index: flow.param_index,
        class: flow_class_name(flow.class),
        expr: flow.expr.map(str::to_string),
        span: flow.span,
      });
    }
  }
  Ok((records, present))
}

/// One runtime-observed call for a selected definition (from the `observed.bin` sidecar,
/// ingested with `vorpal-index ingest-traces`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedRecord {
  /// `out` = the selection was seen calling the counterpart; `in` = the reverse.
  pub direction: &'static str,
  pub counterpart_id: u64,
  pub counterpart_name: String,
  pub counterpart_path: String,
  /// Summed sample/occurrence count across the ingested stacks.
  pub count: u64,
  /// Whether the static graph already carries a `calls` edge for this pair — `false` is
  /// the interesting case: dynamic dispatch or a function pointer static resolution can
  /// never prove.
  pub in_static_graph: bool,
}

fn static_calls(kg: &vorpal_kg::Kg, from: vorpal_kg::NodeId, to: u32) -> bool {
  kg.out_neighbors(from)
    .into_iter()
    .any(|(t, e)| t.raw() as u32 == to && e.base() == vorpal_kg::EdgeType::CALLS)
}

/// Observed calls touching one selected definition, both directions. Absent or stale
/// sidecar (a rebuild renumbers nodes) answers empty with `sidecar_present = false` — the
/// caller says "not ingested for this generation" instead of implying "never ran".
pub fn observed_records(
  kg: &vorpal_kg::Kg,
  kg_dir: &std::path::Path,
  target: &crate::GraphTarget,
) -> Result<(Vec<ObservedRecord>, bool), String> {
  let matches = crate::resolve_target(kg, target).map_err(|err| err.to_string())?;
  let store = vorpal_kg::observed::ObservedStore::load(kg_dir, kg.node_segment_stamp())
    .map_err(|err| err.to_string())?;
  let present = !store.is_empty();
  let mut records = Vec::new();
  let mut describe = |direction: &'static str, counterpart: u32, count: u64, statically: bool| {
    let id = vorpal_kg::NodeId::new(counterpart as u64);
    let (name, path) = kg
      .node(id)
      .map(|v| (v.name.to_string(), v.path.to_string()))
      .unwrap_or_default();
    records.push(ObservedRecord {
      direction,
      counterpart_id: counterpart as u64,
      counterpart_name: name,
      counterpart_path: path,
      count,
      in_static_graph: statically,
    });
  };
  for &id in &matches {
    for (to, count) in store.observed_from(id.raw() as u32) {
      describe("out", to, count, static_calls(kg, id, to));
    }
    for (from, count) in store.observed_into(id.raw() as u32) {
      let statically = static_calls(kg, vorpal_kg::NodeId::new(from as u64), id.raw() as u32);
      describe("in", from, count, statically);
    }
  }
  Ok((records, present))
}

#[derive(Serialize, Debug)]
pub struct SnippetRecord {
  #[serde(flatten)]
  pub node: NodeRecord,
  /// 1-based line of the (context-expanded) snippet start in `path`.
  pub line: usize,
  /// `verified` (bytes match the indexed digest) or `unverified` (generation carries no
  /// digest for this file). A changed file is an error, never a mislabeled snippet.
  pub verification: String,
  pub body: String,
  /// Full span length in bytes when `body` was clamped by `max_bytes`.
  pub truncated_from: Option<u64>,
}

/// How a snippet query failed: staleness is structurally distinguished so surfaces can keep
/// their stable error codes (`stale-source` on MCP) without string matching.
#[derive(Debug)]
pub enum SnippetError {
  /// The file changed since the pinned generation indexed it.
  Stale(String),
  /// Anything else (selector error, spanless node, unreadable file).
  Other(String),
}

/// Selector-driven snippet extraction: resolve `target`, slice each match's span from its
/// file (expanded by `context_lines` whole lines on each side, clamped to `max_bytes`),
/// verifying bytes against `artifacts_dir`'s product pack. Ambiguity semantics match every
/// other selector verb; with `merge_all`, each match yields its own snippet.
pub fn snippet_records(
  kg: &Kg,
  artifacts_dir: Option<&std::path::Path>,
  target: &GraphTarget,
  context_lines: usize,
  max_bytes: usize,
) -> Result<Selected<SnippetRecord>, SnippetError> {
  let matches = resolve_target(kg, target).map_err(|err| SnippetError::Other(err.to_string()))?;
  if matches.is_empty() {
    return Ok(Selected::NoMatch);
  }
  if matches.len() > 1 && !target.merge_all {
    return Ok(Selected::Ambiguous(
      matches.iter().filter_map(|&id| node_record(kg, id)).collect(),
    ));
  }
  // One pack handle for the whole selection (process-cached per generation), and one read
  // per distinct file: `--all` over N same-file overloads costs one read, not N. Matches
  // arrive in ascending id order (= file-grouped), so a single-entry cache is a full dedup.
  let pack = artifacts_dir.and_then(crate::cached_pack);
  let mut cached: Option<(String, Vec<u8>, &'static str)> = None;
  let mut records = Vec::new();
  for &id in &matches {
    let Some(node) = node_record(kg, id) else {
      continue;
    };
    let [start, end] = node.span;
    if end <= start {
      return Err(SnippetError::Other(format!(
        "node {} ({}) carries no source span (File node, or an index built before spans \
         were persisted — rebuild the index)",
        node.id, node.name
      )));
    }
    let (bytes, verification) = match &cached {
      Some((path, bytes, verification)) if path == &node.path => (bytes, *verification),
      _ => {
        let read = crate::read_indexed_source_with(pack.as_deref(), &node.path)
          .map_err(SnippetError::Other)?;
        let (bytes, verification) = match read {
          crate::IndexedRead::Verified(bytes) => (bytes, "verified"),
          crate::IndexedRead::Unverified(bytes) => (bytes, "unverified"),
          crate::IndexedRead::Changed => {
            return Err(SnippetError::Stale(format!(
              "{} changed since this generation indexed it — span offsets are stale; rebuild \
               the index",
              node.path
            )));
          }
        };
        cached = Some((node.path.clone(), bytes, verification));
        match &cached {
          Some((_, bytes, verification)) => (bytes, *verification),
          None => return Err(SnippetError::Other("snippet cache write failed".to_string())),
        }
      }
    };
    let end = (end as usize).min(bytes.len());
    let start = (start as usize).min(end);
    // Whole-lines contract: the span's own first/last lines complete, then `context_lines`
    // more on each side. `line_start` maps any offset to the start of its line.
    let line_start =
      |at: usize| bytes[..at].iter().rposition(|&b| b == b'\n').map_or(0, |nl| nl + 1);
    let mut from = line_start(start);
    for _ in 0..context_lines {
      if from == 0 {
        break;
      }
      from = line_start(from - 1); // step onto the previous line's newline, then to its start
    }
    let mut to = end;
    for _ in 0..=context_lines {
      match bytes[to..].iter().position(|&b| b == b'\n') {
        Some(nl) => to += nl + 1,
        None => {
          to = bytes.len();
          break;
        }
      }
    }
    let full = to - from;
    let clamped_to = to.min(from + max_bytes);
    let line = bytes[..from].iter().filter(|&&b| b == b'\n').count() + 1;
    records.push(SnippetRecord {
      node,
      line,
      verification: verification.to_string(),
      body: String::from_utf8_lossy(&bytes[from..clamped_to]).into_owned(),
      truncated_from: (clamped_to < to).then_some(full as u64),
    });
  }
  Ok(Selected::Hits(records))
}

/// The rendered form of one snippet page: `path:line  name [Kind] (verification)` header
/// then the body — the same shape `fetch_span` prints, per selected node.
pub fn render_snippets(records: &[SnippetRecord]) -> String {
  use std::fmt::Write;
  let mut out = String::new();
  for record in records {
    let _ = write!(
      out,
      "{}:{}  {} [{}] ({})\n{}",
      record.node.path, record.line, record.node.name, record.node.kind, record.verification,
      record.body
    );
    if !record.body.ends_with('\n') {
      out.push('\n');
    }
    if let Some(full) = record.truncated_from {
      let _ = writeln!(out, "(truncated: {} of {full} bytes)", record.body.len());
    }
  }
  out
}

/// Pattern listing: every node whose NAME matches the regex, ascending id (deterministic).
/// A listing like the `node` verb — matches are the answer, never auto-selected for edge
/// queries. The regex engine's own literal prefilter does the heavy lifting; `names.idx`
/// cannot help here (it is hash-sorted, not lexicographic).
pub fn pattern_records(kg: &Kg, pattern: &str) -> Result<Vec<NodeRecord>, String> {
  use rayon::prelude::*;
  let regex = regex::Regex::new(pattern).map_err(|err| format!("bad pattern: {err}"))?;
  let node_count = kg.node_count() as u64;
  Ok(
    (0..node_count)
      .into_par_iter()
      .filter_map(|row| {
        let id = NodeId::new(row);
        if regex.is_match(kg.node_name(id)?) {
          node_record(kg, id)
        } else {
          None
        }
      })
      .collect(),
  )
}

/// One dead-code candidate: a definition with **no semantic in-edges anywhere in the graph**
/// (no calls/references/imports/implements/of_type/overrides — containment edges don't count
/// as liveness), surviving the referenced-name and parse-damage suppressions.
#[derive(Serialize, Debug)]
pub struct DeadRecord {
  #[serde(flatten)]
  pub node: NodeRecord,
  /// Total in-edges including structural containment (context: 1 = just its `defines`).
  pub in_degree: u64,
}

/// The dead-code scan's answer: one page of candidates plus the honesty envelope — the
/// full candidate total, how many were suppressed and why, and whether name suppression
/// was even available.
#[derive(Serialize, Debug)]
pub struct DeadPage {
  /// The requested page of candidates (ascending node id over the whole candidate set).
  pub records: Vec<DeadRecord>,
  pub total: usize,
  pub start: usize,
  pub end: usize,
  /// Candidates whose NAME appears in evidence (some occurrence referenced it — resolved,
  /// external, masked, or a namesake tie): extraction saw a use that resolution could not
  /// pin to this node, so calling it dead would be a guess. Function-pointer tables and
  /// dynamic dispatch land here.
  pub suppressed_referenced: u64,
  /// Candidates in files whose parse damage exceeds the ratio threshold: their edges may be
  /// missing because the parse is, not because the code is dead.
  pub suppressed_damaged: u64,
  /// Whether the evidence sidecar existed; without it, `suppressed_referenced` is 0 because
  /// suppression was UNAVAILABLE, not because nothing was referenced.
  pub name_suppression: bool,
}

/// Filters for the dead-code scan.
#[derive(Default, Clone, Debug)]
pub struct DeadFilter {
  /// One symbol kind; absent = the default definition set (Function, Method, Class, Struct,
  /// Enum, Interface, Constructor).
  pub kind: Option<String>,
  pub path_prefix: Option<String>,
  pub path_suffix: Option<String>,
  pub exported_only: bool,
  /// Exclude test-classified paths — a test's local helpers are dead in production terms
  /// anyway, and test entry points are runner-invoked (always in-degree 0).
  pub exclude_tests: bool,
}

/// Parse-damage suppression threshold: candidates in files with more than this fraction of
/// bytes inside ERROR nodes are reported as suppressed, not dead.
const DEAD_DAMAGE_RATIO: f64 = 0.10;

/// Whole-graph dead-definition scan, page-materialized. Deterministic: candidates in
/// ascending node id; `page` selects which slice becomes full records (at kernel scale
/// ~430K raw candidates survive the edge scan — building NodeRecords for all of them was
/// 120 ms of the original 250; a page costs microseconds).
pub fn dead_records_page(
  kg: &Kg,
  artifacts_dir: Option<&std::path::Path>,
  filter: &DeadFilter,
  page: PageRequest<'_>,
) -> Result<DeadPage, String> {
  use rayon::prelude::*;

  let mut allowed = [false; 256];
  match filter.kind.as_deref() {
    Some(text) => {
      let kind =
        vorpal_kg::SymbolKind::parse(text).ok_or_else(|| format!("unknown symbol kind '{text}'"))?;
      allowed[kind.tag() as usize] = true;
    }
    None => {
      use vorpal_kg::SymbolKind as K;
      for kind in [K::Function, K::Method, K::Class, K::Struct, K::Enum, K::Interface, K::Constructor]
      {
        allowed[kind.tag() as usize] = true;
      }
    }
  }

  // Semantic (liveness) relations, as base tags; defines/has_method/has_field are containment.
  let semantic = {
    let mut mask = [false; 256];
    for edge in [
      vorpal_kg::EdgeType::CALLS,
      vorpal_kg::EdgeType::REFERENCES,
      vorpal_kg::EdgeType::IMPORTS,
      vorpal_kg::EdgeType::IMPLEMENTS,
      vorpal_kg::EdgeType::OF_TYPE,
      vorpal_kg::EdgeType::OVERRIDES,
    ] {
      mask[edge.0 as usize & 0xff] = true;
    }
    mask
  };

  // Pass 1 (parallel): cheapest gates first — the u8 kind tag, then the in-edge type slice
  // (both allocation-free; most definitions have semantic in-edges and die here) — and only
  // then the heap-string view for the path/export filters. At kernel scale this ordering is
  // the difference between 2.7M three-string view materializations and ~400K.
  let needs_view = filter.exported_only
    || filter.exclude_tests
    || filter.path_prefix.is_some()
    || filter.path_suffix.is_some();
  let node_count = kg.node_count() as u64;
  let kind_tags = kg.kind_tag_lookup();
  let candidates: Vec<u64> = (0..node_count)
    .into_par_iter()
    .filter(|&row| {
      let id = NodeId::new(row);
      let tag = match &kind_tags {
        Some(tags) => tags.get(row),
        None => kg.node_kind(id).map(|kind| kind.tag()),
      };
      let Some(tag) = tag else { return false };
      if !allowed[tag as usize] {
        return false;
      }
      if kg
        .in_edge_types_of(id)
        .iter()
        .any(|&packed| semantic[vorpal_kg::EdgeType(packed).base().0 as usize & 0xff])
      {
        return false;
      }
      if !needs_view {
        return true;
      }
      let Some(view) = kg.node(id) else { return false };
      if filter.exported_only && !view.exported {
        return false;
      }
      if filter.exclude_tests && crate::path_class(view.path) == crate::PathClass::Test {
        return false;
      }
      if let Some(prefix) = filter.path_prefix.as_deref() {
        if !view.path.starts_with(prefix) {
          return false;
        }
      }
      match filter.path_suffix.as_deref() {
        Some(suffix) => view.path.ends_with(suffix),
        None => true,
      }
    })
    .collect();

  // Pass 2: referenced-name suppression — any evidence occurrence carrying this name's hash
  // (any outcome) means extraction saw a use; absence of an edge is then attribution
  // failure, not death. Conservative by design. Membership structure: collect + parallel
  // sort + dedup, then binary search — at kernel scale (~6.9M occurrences) this builds in
  // tens of ms where a SipHash set build was the scan's dominant cost.
  let mut referenced: Vec<u32> = Vec::new();
  let name_suppression = kg.for_each_evidence_name_hash(|hash| {
    referenced.push(hash);
  });
  referenced.par_sort_unstable();
  referenced.dedup();
  let mut suppressed_referenced = 0u64;
  let mut suppressed_damaged = 0u64;
  let pack = artifacts_dir.and_then(crate::cached_pack);
  // Suppression works on the cheap accessors (name for the hash, path for the damage
  // lookup) — full records are built only for the requested page below. Per-file damage is
  // computed once per distinct candidate path (candidates are id-ordered = path-grouped).
  let mut damage_cache: Option<(String, bool)> = None;
  let mut survivors: Vec<u64> = Vec::new();
  for row in candidates {
    let id = NodeId::new(row);
    if name_suppression {
      let Some(name) = kg.node_name(id) else { continue };
      let hash = xxhash_rust::xxh3::xxh3_64(name.as_bytes()) as u32;
      if referenced.binary_search(&hash).is_ok() {
        suppressed_referenced += 1;
        continue;
      }
    }
    if let Some(pack) = pack.as_deref() {
      let Some(path) = kg.node_path(id) else { continue };
      if damage_cache.as_ref().is_none_or(|(cached, _)| cached != path) {
        let damaged = pack.get(path).is_some_and(|bytes| {
          let error_bytes = vorpal_ingest::peek_product_error_bytes(bytes).unwrap_or(0);
          let size = vorpal_ingest::peek_product_stamps(bytes).map_or(0, |(size, _)| size);
          size > 0 && (error_bytes as f64 / size as f64) > DEAD_DAMAGE_RATIO
        });
        damage_cache = Some((path.to_string(), damaged));
      }
      if damage_cache.as_ref().is_some_and(|&(_, damaged)| damaged) {
        suppressed_damaged += 1;
        continue;
      }
    }
    survivors.push(row);
  }
  let PageBounds { start, end, total } = page_bounds(survivors.len(), page.cursor, page.limit)?;
  let records = survivors[start..end]
    .iter()
    .filter_map(|&row| {
      let id = NodeId::new(row);
      Some(DeadRecord {
        node: node_record(kg, id)?,
        in_degree: kg.in_degree(id) as u64,
      })
    })
    .collect();
  Ok(DeadPage {
    records,
    total,
    start,
    end,
    suppressed_referenced,
    suppressed_damaged,
    name_suppression,
  })
}

/// Rendered dead-code page: the honesty head (whole-scan totals), then one line per record
/// in this page. Text callers pass a 200-record page; the full set pages through records.
pub fn render_dead(report: &DeadPage) -> String {
  use std::fmt::Write;
  let mut out = String::new();
  let _ = writeln!(
    out,
    "{} dead candidates ({} suppressed: name referenced somewhere; {} suppressed: file parse-damaged{})",
    report.total,
    report.suppressed_referenced,
    report.suppressed_damaged,
    if report.name_suppression { "" } else { "; no evidence sidecar — name suppression unavailable" },
  );
  for record in &report.records {
    let _ = writeln!(
      out,
      "{} [{}] {}  (in-degree {})",
      record.node.name, record.node.kind, record.node.path, record.in_degree
    );
  }
  if report.end < report.total {
    let _ = writeln!(
      out,
      "… {} more — refine (--kind/--prefix/--path/--exported) or page the records surface",
      report.total - report.end
    );
  }
  out
}

/// One file's parse-coverage row: how much of it the parser actually understood. The cheap
/// whole-bank overview (header peeks only — no product decode); per-span/per-entity detail
/// stays with the `health` surface.
#[derive(Serialize, Debug)]
pub struct CoverageRecord {
  pub path: String,
  pub error_nodes: u64,
  pub error_bytes: u64,
  pub size: u64,
  /// error_bytes / size (0.0 when size unknown).
  pub ratio: f64,
}

/// Coverage overview + honesty head.
#[derive(Serialize, Debug)]
pub struct CoverageReport {
  /// Damaged files only, worst ratio first (ties: path) — clean files are counted, not listed.
  pub records: Vec<CoverageRecord>,
  pub total_files: u64,
  pub damaged_files: u64,
  pub total_error_bytes: u64,
}

/// Sweep the generation's product bank: one header peek per file, fanned across the pool
/// (peeks fault mapped pages — exactly the work that parallelizes). Absence of a bank
/// yields an empty report with totals 0 — callers state that, never "everything parsed".
pub fn coverage_records(artifacts_dir: Option<&std::path::Path>) -> CoverageReport {
  use rayon::prelude::*;
  let mut records = Vec::new();
  let mut total_files = 0u64;
  let mut total_error_bytes = 0u64;
  if let Some(pack) = artifacts_dir.and_then(crate::cached_pack) {
    let entries: Vec<(&str, &[u8])> = pack.entries().collect();
    total_files = entries.len() as u64;
    records = entries
      .par_iter()
      .filter_map(|&(path, bytes)| {
        let error_nodes = vorpal_ingest::peek_product_error_nodes(bytes).unwrap_or(0);
        let error_bytes = vorpal_ingest::peek_product_error_bytes(bytes).unwrap_or(0);
        if error_nodes == 0 && error_bytes == 0 {
          return None;
        }
        let size = vorpal_ingest::peek_product_stamps(bytes).map_or(0, |(size, _)| size);
        Some(CoverageRecord {
          path: path.to_string(),
          error_nodes: error_nodes as u64,
          error_bytes,
          size,
          ratio: if size > 0 { error_bytes as f64 / size as f64 } else { 0.0 },
        })
      })
      .collect();
    total_error_bytes = records.iter().map(|r| r.error_bytes).sum();
  }
  records.sort_by(|a, b| {
    b.ratio
      .partial_cmp(&a.ratio)
      .unwrap_or(std::cmp::Ordering::Equal)
      .then_with(|| a.path.cmp(&b.path))
  });
  let damaged_files = records.len() as u64;
  CoverageReport {
    records,
    total_files,
    damaged_files,
    total_error_bytes,
  }
}

/// Rendered coverage overview, capped like the other whole-tree listings.
pub fn render_coverage(report: &CoverageReport) -> String {
  use std::fmt::Write;
  const TEXT_CAP: usize = 100;
  let mut out = String::new();
  if report.total_files == 0 {
    return "no product bank in this generation — coverage unavailable (not proof of clean parses)\n".to_string();
  }
  let _ = writeln!(
    out,
    "{} of {} files carry parse damage ({} error bytes total); worst first:",
    report.damaged_files, report.total_files, report.total_error_bytes
  );
  for record in report.records.iter().take(TEXT_CAP) {
    let _ = writeln!(
      out,
      "{:>6.2}%  {} ({} error nodes, {} of {} bytes)",
      record.ratio * 100.0,
      record.path,
      record.error_nodes,
      record.error_bytes,
      record.size
    );
  }
  if report.records.len() > TEXT_CAP {
    let _ = writeln!(out, "… {} more — page the records surface, or `health` for span detail", report.records.len() - TEXT_CAP);
  }
  out
}

/// What this generation's graph contains, by vocabulary: the introspection surface that
/// teaches a caller (agent or human) what is queryable before it guesses — kinds, relations,
/// grades, and tier state, with counts.
#[derive(Serialize, Debug)]
pub struct SchemaReport {
  /// Generation content id (the `gen/<id>` dir name), when resolved from a generation dir.
  pub generation: Option<String>,
  pub nodes: u64,
  pub edges: u64,
  pub files: u64,
  /// Node counts per symbol kind — count-descending, then name, so hubs read first.
  pub kinds: Vec<CountRow>,
  /// Directed edge counts per relation — count-descending, then name.
  pub relations: Vec<CountRow>,
  /// The resolution-grade vocabulary, best first (traversal floors accept these).
  pub grades: Vec<String>,
  /// Warm search tiers present in this generation (absent tiers change latency, never answers).
  pub ann_tier: bool,
  pub postings_tier: bool,
}

#[derive(Serialize, Debug)]
pub struct CountRow {
  pub name: String,
  pub count: u64,
}

fn count_rows<T>(counts: Vec<(T, u64)>, name_of: impl Fn(&T) -> String) -> Vec<CountRow> {
  let mut rows: Vec<CountRow> = counts
    .into_iter()
    .map(|(item, count)| CountRow {
      name: name_of(&item),
      count,
    })
    .collect();
  rows.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
  rows
}

/// Compute the schema report: one pass over the kind column, one over the edge-type column.
pub fn schema_report(kg: &Kg, generation_dir: Option<&std::path::Path>) -> SchemaReport {
  let kinds = kg.node_count_by_kind();
  let files = kinds
    .iter()
    .find(|(kind, _)| *kind == vorpal_kg::SymbolKind::File)
    .map_or(0, |&(_, count)| count);
  SchemaReport {
    generation: generation_dir
      .and_then(|dir| dir.file_name())
      .map(|name| name.to_string_lossy().into_owned()),
    nodes: kg.node_count() as u64,
    edges: kg.edge_count(),
    files,
    kinds: count_rows(kinds, |kind| format!("{kind:?}")),
    relations: count_rows(kg.edge_count_by_type(), |edge| edge.name().to_string()),
    grades: ["exact", "constrained", "heuristic", "structural"]
      .map(String::from)
      .to_vec(),
    ann_tier: generation_dir.is_some_and(|dir| dir.join("ann.bin").is_file()),
    postings_tier: generation_dir.is_some_and(|dir| dir.join("postings.bin").is_file()),
  }
}

/// The rendered schema — compact, count-annotated, one vocabulary per line.
pub fn render_schema(report: &SchemaReport) -> String {
  use std::fmt::Write;
  let mut out = String::new();
  let generation = report.generation.as_deref().unwrap_or("(in-memory)");
  let _ = writeln!(
    out,
    "generation {generation}: {} nodes · {} edges · {} files",
    report.nodes, report.edges, report.files
  );
  let list = |rows: &[CountRow]| {
    rows
      .iter()
      .map(|row| format!("{} {}", row.name, row.count))
      .collect::<Vec<_>>()
      .join(" · ")
  };
  let _ = writeln!(out, "kinds: {}", list(&report.kinds));
  let _ = writeln!(out, "relations: {}", list(&report.relations));
  let _ = writeln!(out, "grades: {}", report.grades.join(" > "));
  let _ = writeln!(
    out,
    "tiers: ann {} · postings {}",
    if report.ann_tier { "warm" } else { "cold" },
    if report.postings_tier { "warm" } else { "cold" }
  );
  out
}

/// The outcome of a selector-driven record query.
#[derive(Debug)]
pub enum Selected<T> {
  /// Nothing matches the selector.
  NoMatch,
  /// Several definitions match and `merge_all` was not set: refine with these candidates.
  Ambiguous(Vec<NodeRecord>),
  /// The query ran; here are its records (possibly empty — a bound target with no relations).
  Hits(Vec<T>),
}

/// Resolved slice bounds for one page of a deterministic record vector — the cursor/limit
/// contract every paged surface shares (MCP tools, `--format json` CLI verbs). `cursor` is
/// an opaque `o:<offset>` into the record order; `limit` caps the page (default 100, max
/// 1000). Errors are plain messages for the surface to wrap in its own error shape.
/// A page request as surfaces receive it: the raw cursor + limit pair.
#[derive(Clone, Copy, Default)]
pub struct PageRequest<'a> {
  pub cursor: Option<&'a str>,
  pub limit: Option<u64>,
}

pub struct PageBounds {
  pub start: usize,
  pub end: usize,
  pub total: usize,
}

pub fn page_bounds(
  total: usize,
  cursor: Option<&str>,
  limit: Option<u64>,
) -> Result<PageBounds, String> {
  let offset = match cursor {
    None => 0usize,
    Some(cursor) => cursor
      .strip_prefix("o:")
      .and_then(|n| n.parse().ok())
      .ok_or_else(|| format!("malformed cursor '{cursor}' (want o:<offset>)"))?,
  };
  let limit = limit.unwrap_or(100).clamp(1, 1000) as usize;
  let start = offset.min(total);
  let end = start.saturating_add(limit).min(total);
  Ok(PageBounds { start, end, total })
}

/// One page of records as the shared structured envelope: `{outcome, records, total,
/// truncated, nextCursor?}`. Truncation is always declared; recomputing the vector per page
/// keeps callers stateless, and record-order determinism is what makes the pages coherent.
pub fn paged_value<T: Serialize>(
  records: &[T],
  cursor: Option<&str>,
  limit: Option<u64>,
  outcome: &str,
) -> Result<serde_json::Value, String> {
  let PageBounds { start, end, total } = page_bounds(records.len(), cursor, limit)?;
  let page: Vec<serde_json::Value> = records[start..end]
    .iter()
    .map(|record| serde_json::to_value(record).unwrap_or(serde_json::Value::Null))
    .collect();
  let mut data = serde_json::json!({
    "outcome": outcome,
    "records": page,
    "total": total,
    "truncated": end < total,
  });
  if end < total {
    data["nextCursor"] = serde_json::json!(format!("o:{end}"));
  }
  Ok(data)
}

/// [`paged_value`] over a selector outcome: `no-match` and `ambiguous` are answers (the
/// ambiguous candidates page like any records), never errors.
pub fn selected_value<T: Serialize>(
  selected: Selected<T>,
  cursor: Option<&str>,
  limit: Option<u64>,
) -> Result<serde_json::Value, String> {
  match selected {
    Selected::NoMatch => Ok(serde_json::json!({
      "outcome": "no-match", "records": [], "total": 0, "truncated": false
    })),
    Selected::Ambiguous(candidates) => paged_value(&candidates, cursor, limit, "ambiguous"),
    Selected::Hits(hits) => paged_value(&hits, cursor, limit, "hits"),
  }
}

/// The typed view of one node, if it exists.
pub fn node_record(kg: &Kg, id: NodeId) -> Option<NodeRecord> {
  let view = kg.node(id)?;
  Some(NodeRecord {
    id: id.raw(),
    external_id: view.external_id.map(|e| format!("eid:{e:032x}")),
    name: view.name.to_string(),
    kind: format!("{:?}", view.kind),
    path: view.path.to_string(),
    exported: view.exported,
    span: [view.span.0, view.span.1],
    signature: view.signature.to_string(),
    class: crate::path_class(view.path).label().to_string(),
    in_degree: None,
    out_degree: None,
  })
}

/// The typed candidate listing for a selector — the record twin of the `node` verb. Listing
/// never treats multiple matches as ambiguity: the matches ARE the answer.
pub fn listing_records(kg: &Kg, target: &GraphTarget) -> Result<Vec<NodeRecord>, String> {
  let matches = resolve_target(kg, target).map_err(|err| err.to_string())?;
  Ok(
    matches
      .iter()
      .filter_map(|&id| {
        let mut record = node_record(kg, id)?;
        record.in_degree = Some(kg.in_degree(id) as u64);
        record.out_degree = Some(kg.out_degree(id) as u64);
        Some(record)
      })
      .collect(),
  )
}

/// The typed twin of the edge verbs (`callers`/`references`/`importers`/`implementors`/
/// `typeusers`): nodes with an incoming edge of the verb's relation to the selected target,
/// each carrying its edge grade. Ordering matches the rendered surface (ascending node id,
/// best grade per node).
pub fn related_records(
  kg: &Kg,
  verb: &str,
  target: &GraphTarget,
) -> Result<Selected<RelatedRecord>, String> {
  let edge = match verb {
    "callers" => vorpal_kg::EdgeType::CALLS,
    "refs" | "references" => vorpal_kg::EdgeType::REFERENCES,
    "importers" => vorpal_kg::EdgeType::IMPORTS,
    "implementors" => vorpal_kg::EdgeType::IMPLEMENTS,
    "typeusers" => vorpal_kg::EdgeType::OF_TYPE,
    "similar" => vorpal_kg::EdgeType::SIMILAR_TO,
    other => return Err(format!("unknown graph verb '{other}'")),
  };
  let matches = resolve_target(kg, target).map_err(|err| err.to_string())?;
  if matches.is_empty() {
    return Ok(Selected::NoMatch);
  }
  if matches.len() > 1 && !target.merge_all {
    return Ok(Selected::Ambiguous(
      matches.iter().filter_map(|&id| node_record(kg, id)).collect(),
    ));
  }
  let mut hits: Vec<(NodeId, u8)> = Vec::new();
  for &target_id in &matches {
    for (from, confidence) in kg.incoming_with_confidence(target_id, edge) {
      hits.push((from, confidence));
    }
  }
  hits.sort_unstable_by_key(|&(n, c)| (n.raw(), std::cmp::Reverse(c)));
  hits.dedup_by_key(|&mut (n, _)| n);
  Ok(Selected::Hits(
    hits
      .into_iter()
      .filter_map(|(id, confidence)| {
        Some(RelatedRecord {
          node: node_record(kg, id)?,
          grade: crate::confidence_label(confidence).to_string(),
          similarity: (edge.base() == vorpal_kg::EdgeType::SIMILAR_TO).then_some(confidence),
        })
      })
      .collect(),
  ))
}

/// One page of a selector-driven query whose full record set would be expensive to
/// materialize: the page's records plus the bounds that locate it in the deterministic
/// whole. `NoMatch`/`Ambiguous` keep the usual meanings.
#[derive(Debug)]
pub enum SelectedPage<T> {
  NoMatch,
  Ambiguous(Vec<NodeRecord>),
  Page {
    records: Vec<T>,
    total: usize,
    start: usize,
    end: usize,
  },
}

/// The structured envelope for a pre-sliced page — identical shape to [`paged_value`],
/// without re-serializing anything outside the page.
pub fn selected_page_value<T: Serialize>(
  selected: SelectedPage<T>,
  cursor: Option<&str>,
  limit: Option<u64>,
) -> Result<serde_json::Value, String> {
  match selected {
    SelectedPage::NoMatch => Ok(serde_json::json!({
      "outcome": "no-match", "records": [], "total": 0, "truncated": false
    })),
    SelectedPage::Ambiguous(candidates) => paged_value(&candidates, cursor, limit, "ambiguous"),
    SelectedPage::Page {
      records,
      total,
      end,
      ..
    } => {
      let page: Vec<serde_json::Value> = records
        .iter()
        .map(|record| serde_json::to_value(record).unwrap_or(serde_json::Value::Null))
        .collect();
      let mut data = serde_json::json!({
        "outcome": "hits",
        "records": page,
        "total": total,
        "truncated": end < total,
      });
      if end < total {
        data["nextCursor"] = serde_json::json!(format!("o:{end}"));
      }
      Ok(data)
    }
  }
}

/// [`reach_records`] materializing NodeRecords **only for one page**: the BFS still runs
/// whole (steps are 16-byte Copy values — the deterministic vector the cursor indexes),
/// but the heap-string record construction is paid per page, not per closure. An
/// undirected kernel walk reaches 200K+ nodes; building 200K records to emit 100 was the
/// dominant cost of the paged surface.
#[allow(clippy::too_many_arguments)] // one traversal surface; every input is load-bearing
pub fn reach_records_page(
  kg: &Kg,
  flows_dir: Option<&std::path::Path>,
  target: &GraphTarget,
  dir: vorpal_kg::Direction,
  relations: &[vorpal_kg::EdgeType],
  max_depth: Option<u32>,
  min_confidence: u8,
  page: PageRequest<'_>,
) -> Result<SelectedPage<ReachRecord>, String> {
  let matches = resolve_target(kg, target).map_err(|err| err.to_string())?;
  if matches.is_empty() {
    return Ok(SelectedPage::NoMatch);
  }
  if matches.len() > 1 && !target.merge_all {
    return Ok(SelectedPage::Ambiguous(
      matches.iter().filter_map(|&id| node_record(kg, id)).collect(),
    ));
  }
  let mut steps = Vec::new();
  for &seed in &matches {
    steps.extend(kg.reachable_via_paths(seed, dir, relations, max_depth, min_confidence));
  }
  // The flow sidecar joins per data_flows hop (G-M5): loaded once, absent-tolerant.
  let flow_store = crate::flow_store_for(flows_dir, relations);
  let PageBounds { start, end, total } = page_bounds(steps.len(), page.cursor, page.limit)?;
  let records = steps[start..end]
    .iter()
    .filter_map(|step| {
      Some(ReachRecord {
        node: node_record(kg, NodeId::new(step.node as u64))?,
        depth: step.depth,
        via: step.via.0 as u64,
        relation: step.via.1.name().to_string(),
        grade: crate::confidence_label(step.via.1.confidence()).to_string(),
        edge_direction: if step.inbound { "in" } else { "out" }.to_string(),
        flow_exprs: crate::flow_exprs_for_hop(
          flow_store.as_ref(),
          step.via.0,
          step.node,
          step.inbound,
        ),
      })
    })
    .collect();
  Ok(SelectedPage::Page {
    records,
    total,
    start,
    end,
  })
}

/// One graph-ranked structural-search hit: the enclosing definition of ≥1 pattern match,
/// with its match count and semantic in-degree (the ranking signal — heavily-used code
/// outranks dead-weight lookalikes, the same prior hybrid search uses).
#[derive(Serialize, Debug)]
pub struct CodeMatchRecord {
  #[serde(flatten)]
  pub node: NodeRecord,
  pub matches: u32,
  pub semantic_in_degree: u64,
  /// 1-based line of the first match in the file.
  pub first_line: u32,
}

/// The whole answer, with the honesty margins.
#[derive(Serialize, Debug)]
pub struct CodeSearchReport {
  pub records: Vec<CodeMatchRecord>,
  /// Files whose current bytes no longer match the generation (skipped, not guessed).
  pub stale_files: u64,
  /// Files that vanished since indexing or are not valid UTF-8 (skipped, and said so).
  pub unreadable_files: u64,
  /// Files scanned (post lang/prefix filters).
  pub scanned_files: u64,
  pub total_matches: u64,
}

/// Structural pattern search fused with the graph (ADOPTION B3): run the ast-grep pattern
/// over the generation's OWN file set (digest-verified against the product pack — changed
/// files are counted stale and skipped, never half-trusted), attribute each match to its
/// innermost enclosing definition via the index spans, and rank definitions by semantic
/// in-degree. Their `search_code` shells out to grep; this is in-process, structural, and
/// generation-coherent.
pub fn code_search(
  kg: &Kg,
  artifacts_dir: Option<&std::path::Path>,
  pattern: &str,
  lang_filter: Option<&str>,
  path_prefix: Option<&str>,
  k: usize,
) -> Result<CodeSearchReport, String> {
  use rayon::prelude::*;
  use vorpal_ingest::SgLang;
  use vorpal_language::{Language, LanguageExt};

  let runs = crate::cached_runs(kg, artifacts_dir);
  let pack = artifacts_dir.and_then(crate::cached_pack);
  // Copyable borrow for the per-chunk closures.
  let pack_ref = pack.as_deref();
  let semantic = {
    let mut mask = [false; 256];
    for edge in [
      vorpal_kg::EdgeType::CALLS,
      vorpal_kg::EdgeType::REFERENCES,
      vorpal_kg::EdgeType::IMPORTS,
      vorpal_kg::EdgeType::IMPLEMENTS,
      vorpal_kg::EdgeType::OF_TYPE,
      vorpal_kg::EdgeType::OVERRIDES,
    ] {
      mask[edge.0 as usize & 0xff] = true;
    }
    mask
  };

  enum FileOutcome {
    /// Read + parsed + matched (defs may be empty — matchless is still scanned).
    Scanned(Vec<(u64, u32, u32)>),
    /// Current bytes no longer match the generation — never half-trusted.
    Stale,
    /// Vanished since indexing, or not valid UTF-8 — excluded, and SAID so.
    Unreadable,
  }

  // Pattern compilation is ~a parse of the pattern itself: per-chunk, per-language reuse
  // keeps it to a few hundred compiles across a monorepo instead of one per file (the
  // difference between 69 s and parse-bound wall time on the kernel's 63K C files).
  let per_file: Vec<Option<FileOutcome>> = runs
    .par_chunks(256)
    .flat_map_iter(|chunk| {
      let mut compiled: Vec<(SgLang, vorpal_core::matcher::Pattern)> = Vec::new();
      chunk.iter().map(move |run| {
      let lang = SgLang::from_path(&run.path)?;
      if let Some(filter) = lang_filter {
        if !format!("{lang:?}").eq_ignore_ascii_case(filter) && lang.to_string() != filter {
          return None;
        }
      }
      if let Some(prefix) = path_prefix {
        if !run.path.starts_with(prefix) {
          return None;
        }
      }
      let matcher = match compiled.iter().position(|(l, _)| *l == lang) {
        Some(at) => &compiled[at].1,
        None => {
          let pattern = vorpal_core::matcher::Pattern::try_new(pattern, lang).ok()?;
          compiled.push((lang, pattern));
          &compiled[compiled.len() - 1].1
        }
      };
      let bytes = match crate::read_indexed_source_with(pack_ref, &run.path) {
        Ok(crate::IndexedRead::Verified(bytes)) => bytes,
        Ok(crate::IndexedRead::Unverified(bytes)) => bytes,
        Ok(crate::IndexedRead::Changed) => return Some(FileOutcome::Stale),
        Err(_) => return Some(FileOutcome::Unreadable),
      };
      let Ok(source) = String::from_utf8(bytes) else {
        return Some(FileOutcome::Unreadable);
      };
      let grep = lang.grep(&source);
      let mut match_starts: Vec<u32> = grep
        .root()
        .find_all(matcher)
        .map(|hit| hit.range().start as u32)
        .collect();
      if match_starts.is_empty() {
        // Scanned, clean, matchless — counted as scanned (silence must be attributable).
        return Some(FileOutcome::Scanned(Vec::new()));
      }
      match_starts.sort_unstable();
      // Attribute to the innermost containing definition span within this file's run.
      let spans: Vec<(u32, u32, u64)> = (run.start..run.start + run.len as u64)
        .filter_map(|id| {
          let view = kg.node(NodeId::new(id))?;
          if view.kind == vorpal_kg::SymbolKind::File || view.span.1 <= view.span.0 {
            return None;
          }
          Some((view.span.0, view.span.1, id))
        })
        .collect();
      let mut defs: std::collections::BTreeMap<u64, (u32, u32)> = std::collections::BTreeMap::new();
      for &start in &match_starts {
        let owner = spans
          .iter()
          .filter(|&&(s, e, _)| s <= start && start < e)
          .min_by_key(|&&(s, e, _)| e - s)
          .map(|&(.., id)| id)
          .unwrap_or(run.start); // no containing definition → the File node
        let entry = defs.entry(owner).or_insert((0, start));
        entry.0 += 1;
        entry.1 = entry.1.min(start);
      }
      Some(FileOutcome::Scanned(
        defs
          .into_iter()
          .map(|(id, (count, first))| {
            let line = source.as_bytes()[..first as usize]
              .iter()
              .filter(|&&b| b == b'\n')
              .count() as u32
              + 1;
            (id, count, line)
          })
          .collect(),
      ))
      })
    })
    .collect();

  let mut stale_files = 0u64;
  let mut unreadable_files = 0u64;
  let mut scanned_files = 0u64;
  let mut total_matches = 0u64;
  let mut hits: Vec<(u64, u32, u32)> = Vec::new();
  for file in per_file.into_iter().flatten() {
    match file {
      FileOutcome::Stale => stale_files += 1,
      FileOutcome::Unreadable => unreadable_files += 1,
      FileOutcome::Scanned(defs) => {
        scanned_files += 1;
        for (id, count, first) in defs {
          total_matches += u64::from(count);
          hits.push((id, count, first));
        }
      }
    }
  }

  // Rank: semantic in-degree desc, then match count desc, then id — computed only for
  // definitions that actually matched.
  let mut ranked: Vec<(u64, u32, u32, u64)> = hits
    .into_iter()
    .map(|(id, count, first)| {
      let in_semantic = kg
        .in_edge_types_of(NodeId::new(id))
        .iter()
        .filter(|&&packed| semantic[vorpal_kg::EdgeType(packed).base().0 as usize & 0xff])
        .count() as u64;
      (id, count, first, in_semantic)
    })
    .collect();
  ranked.sort_unstable_by(|a, b| {
    b.3
      .cmp(&a.3)
      .then_with(|| b.1.cmp(&a.1))
      .then_with(|| a.0.cmp(&b.0))
  });
  ranked.truncate(k.clamp(1, 1000));

  let records = ranked
    .into_iter()
    .filter_map(|(id, matches, first, in_semantic)| {
      Some(CodeMatchRecord {
        node: node_record(kg, NodeId::new(id))?,
        matches,
        semantic_in_degree: in_semantic,
        first_line: first,
      })
    })
    .collect();
  Ok(CodeSearchReport {
    records,
    stale_files,
    unreadable_files,
    scanned_files,
    total_matches,
  })
}

/// Rendered code-search page.
pub fn render_code_search(report: &CodeSearchReport) -> String {
  use std::fmt::Write;
  let mut out = String::new();
  let _ = writeln!(
    out,
    "{} matches across {} scanned files ({} stale, {} unreadable — skipped, not guessed); top definitions by in-degree:",
    report.total_matches, report.scanned_files, report.stale_files, report.unreadable_files
  );
  for record in &report.records {
    let _ = writeln!(
      out,
      "{:>6}  {} [{}] {}  ({} match{}, first at line {})",
      record.semantic_in_degree,
      record.node.name,
      record.node.kind,
      record.node.path,
      record.matches,
      if record.matches == 1 { "" } else { "es" },
      record.first_line
    );
  }
  out
}

/// TOON ("token-oriented object notation") rendering: declare columns once, then one row
/// per record — path prefixes grouped so directories print once and rows carry basenames.
/// Built for agent token budgets: on homogeneous result sets the per-row cost collapses to
/// the values themselves. Lossless for scalars; nested values inline as compact JSON; tabs
/// and newlines escape. Records arrive as serialized `Value`s so one renderer serves every
/// record type on every surface.
/// The longest common ABSOLUTE directory prefix across a page's `path` values (never
/// including a basename), or `None` when paths are relative, mixed, or share too little
/// to be worth a header line. Factoring it out once cuts the single largest token cost in
/// tabular renders — kernel-scale pages repeat a ~30-40 byte prefix on every row.
fn common_abs_root(rows: &[serde_json::Value]) -> Option<String> {
  let mut paths = rows
    .iter()
    .filter_map(|row| row.get("path").and_then(serde_json::Value::as_str));
  let first = paths.next()?;
  if !first.starts_with('/') {
    return None;
  }
  let mut root: Vec<&str> = first.split('/').collect();
  root.pop(); // never include the basename
  for path in paths {
    if !path.starts_with('/') {
      return None;
    }
    let segments: Vec<&str> = path.split('/').collect();
    let keep = root
      .iter()
      .zip(segments.iter().take(segments.len().saturating_sub(1)))
      .take_while(|(a, b)| a == b)
      .count();
    root.truncate(keep);
    if root.len() <= 1 {
      return None;
    }
  }
  if root.len() <= 2 {
    return None; // a single top-level segment is not worth the header line
  }
  Some(root.join("/") + "/")
}

pub fn toon_from_values(rows: &[serde_json::Value]) -> String {
  use std::fmt::Write;
  if rows.is_empty() {
    return "(no records)\n".to_string();
  }
  // Column order: identity-first, then everything else in first-seen order (serde_json
  // maps iterate sorted, which is deterministic — the priority list restores readability).
  const PRIORITY: [&str; 6] = ["change", "name", "kind", "path", "depth", "grade"];
  let mut columns: Vec<String> = Vec::new();
  for lead in PRIORITY {
    if rows.iter().any(|row| row.get(lead).is_some()) {
      columns.push(lead.to_string());
    }
  }
  for row in rows {
    if let Some(map) = row.as_object() {
      for key in map.keys() {
        if !columns.iter().any(|c| c == key) {
          columns.push(key.clone());
        }
      }
    }
  }
  let cell = |row: &serde_json::Value, column: &str| -> String {
    // LEAN-style cell economies (adopted after measuring): T/F/_ beat true/false/null in
    // every tokenizer, and they cost nothing to keep lossless (booleans and nulls are
    // typed in the records, never strings).
    let value = match row.get(column) {
      None | Some(serde_json::Value::Null) => return "_".to_string(),
      Some(value) => value,
    };
    let text = match value {
      serde_json::Value::Bool(true) => return "T".to_string(),
      serde_json::Value::Bool(false) => return "F".to_string(),
      serde_json::Value::String(text) => text.clone(),
      other => other.to_string(),
    };
    let mut text = text.replace('\t', "\\t").replace('\n', "\\n");
    if text.is_empty() {
      text.push('_');
    }
    text
  };
  let mut out = String::new();
  let _ = writeln!(out, "cols: {}", columns.join("\t"));
  let root = common_abs_root(rows);
  if let Some(root) = &root {
    let _ = writeln!(out, "root: {root}");
  }
  let mut current_dir: Option<String> = None;
  for row in rows {
    let full_path = row.get("path").and_then(serde_json::Value::as_str).unwrap_or("");
    let (dir, base) = full_path.rsplit_once('/').unwrap_or(("", full_path));
    if !dir.is_empty() && current_dir.as_deref() != Some(dir) {
      match root.as_deref().and_then(|r| format!("{dir}/").strip_prefix(r).map(str::to_string)) {
        Some(rel) if rel.is_empty() => {
          let _ = writeln!(out, "./");
        }
        Some(rel) => {
          let _ = writeln!(out, "{rel}");
        }
        None => {
          let _ = writeln!(out, "{dir}/");
        }
      }
      current_dir = Some(dir.to_string());
    }
    let mut first = true;
    for column in &columns {
      if !first {
        out.push('\t');
      }
      first = false;
      if column == "path" && !dir.is_empty() {
        out.push_str(base);
      } else {
        out.push_str(&cell(row, column));
      }
    }
    out.push('\n');
  }
  out
}

/// LEAN (LLM-Efficient Adaptive Notation) rendering of one record page — the tabular-array
/// profile of the published spec: `records[N]:` header declaring count + tab-separated
/// columns once, two-space-indented tab-delimited rows, `T`/`F`/`_` for booleans and null,
/// bare strings quoted only when they would parse as something else or carry specials
/// (RFC-4180 quote doubling; `\n`/`\\` escapes). Benchmarked leaner than TOON-style output
/// at equal retrieval accuracy; both formats are offered — measure on your own pages.
pub fn lean_from_values(rows: &[serde_json::Value]) -> String {
  use std::fmt::Write;
  if rows.is_empty() {
    return "records[0]:\n".to_string();
  }
  const PRIORITY: [&str; 6] = ["change", "name", "kind", "path", "depth", "grade"];
  // LEAN is the MINIMAL rendering: identity + ranking columns only. The auxiliary fat —
  // `signature` (~60 B/row), `external_id` (36 B/row), `span` — lives in the default
  // text, `toon` (lossless), `ids` (durable handles), and `snippet`/`fetch_span`;
  // measured before the cut, "lean" cost 2.3x the DEFAULT rendering on kernel pages.
  const OMIT: [&str; 3] = ["signature", "span", "external_id"];
  let mut columns: Vec<String> = Vec::new();
  for lead in PRIORITY {
    if rows.iter().any(|row| row.get(lead).is_some()) {
      columns.push(lead.to_string());
    }
  }
  for row in rows {
    if let Some(map) = row.as_object() {
      for key in map.keys() {
        if !OMIT.contains(&key.as_str()) && !columns.iter().any(|c| c == key) {
          columns.push(key.clone());
        }
      }
    }
  }
  fn lean_cell(value: Option<&serde_json::Value>) -> String {
    let value = match value {
      None | Some(serde_json::Value::Null) => return "_".to_string(),
      Some(value) => value,
    };
    match value {
      serde_json::Value::Bool(true) => "T".to_string(),
      serde_json::Value::Bool(false) => "F".to_string(),
      serde_json::Value::Number(number) => number.to_string(),
      serde_json::Value::String(text) => lean_string(text),
      other => lean_string(&other.to_string()),
    }
  }
  fn lean_string(text: &str) -> String {
    let looks_reserved = matches!(text, "T" | "F" | "_");
    let looks_numeric = !text.is_empty() && text.parse::<f64>().is_ok();
    let has_specials = text.contains(['\t', '\n', '\\', '"']);
    let edge_space = text.starts_with(char::is_whitespace) || text.ends_with(char::is_whitespace);
    if text.is_empty() || looks_reserved || looks_numeric || has_specials || edge_space {
      let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\"\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t");
      format!("\"{escaped}\"")
    } else {
      text.to_string()
    }
  }
  let mut out = String::new();
  let _ = writeln!(out, "records[{}]:{}", rows.len(), columns.join("\t"));
  let root = common_abs_root(rows);
  if let Some(root) = &root {
    let _ = writeln!(out, "root: {root}");
  }
  for row in rows {
    out.push_str("  ");
    let mut first = true;
    for column in &columns {
      if !first {
        out.push('\t');
      }
      first = false;
      if column == "path"
        && let Some(root) = &root
        && let Some(rel) = row
          .get("path")
          .and_then(serde_json::Value::as_str)
          .and_then(|p| p.strip_prefix(root.as_str()))
      {
        out.push_str(&lean_cell(Some(&serde_json::Value::String(rel.to_string()))));
        continue;
      }
      out.push_str(&lean_cell(row.get(column.as_str())));
    }
    out.push('\n');
  }
  out
}

/// The bare-identity rendering: one durable id per line (`eid:<hex>`, falling back to the
/// dense id) — for piping into further queries.
pub fn ids_from_values(rows: &[serde_json::Value]) -> String {
  use std::fmt::Write;
  let mut out = String::new();
  for row in rows {
    match row.get("external_id").and_then(serde_json::Value::as_str) {
      Some(eid) => {
        let _ = writeln!(out, "{eid}");
      }
      None => {
        let _ = writeln!(out, "id:{}", row.get("id").and_then(serde_json::Value::as_u64).unwrap_or(0));
      }
    }
  }
  out
}

/// One module row of the architecture summary (module = defining file's directory).
#[derive(Serialize, Debug)]
pub struct ModuleRow {
  pub module: String,
  pub files: u64,
  pub definitions: u64,
  /// Imports arriving from OTHER modules (how much the codebase leans on this module).
  pub imported_by_others: u64,
  /// Imports this module makes into other modules.
  pub imports_others: u64,
}

/// A hub: a definition ranked by semantic in-degree (calls/references/of_type/implements/
/// imports/overrides — containment excluded).
#[derive(Serialize, Debug)]
pub struct HubRecord {
  #[serde(flatten)]
  pub node: NodeRecord,
  pub semantic_in_degree: u64,
}

/// One `calls`-graph community: its size, its most-called member, and the module that
/// holds most of it.
#[derive(Serialize, Debug)]
pub struct ClusterRow {
  pub community: u32,
  pub members: u64,
  /// The member with the highest semantic in-degree — the cluster's face.
  pub representative: NodeRecord,
  /// The directory holding the plurality of members.
  pub dominant_module: String,
}

/// The orientation summary an agent asks for first: where the mass is, what everything
/// leans on, and where execution enters.
#[derive(Serialize, Debug)]
pub struct ArchitectureReport {
  /// Modules by definition count (desc, then name) — capped by `top`.
  pub modules: Vec<ModuleRow>,
  /// Definitions by semantic in-degree (desc, then id) — capped by `top`.
  pub hubs: Vec<HubRecord>,
  /// Exported definitions nothing semantic reaches: entry-point candidates — capped by `top`.
  pub entries: Vec<NodeRecord>,
  pub total_modules: u64,
  /// `calls`-graph communities by size (desc, then id) — capped by `top`; empty with
  /// `clusters_note` set when the sidecar has not been built for this generation.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub clusters: Vec<ClusterRow>,
  pub total_clusters: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub clusters_note: Option<String>,
}

/// Compute the summary: one parallel pass over every node's in-edge types (semantic
/// in-degree → hubs + entries), one pass over IMPORTS out-edges (module matrix), one
/// node_path sweep for per-module tallies. Deterministic ordering throughout.
pub fn architecture_report(
  kg: &Kg,
  artifacts_dir: Option<&std::path::Path>,
  top: usize,
) -> ArchitectureReport {
  use rayon::prelude::*;
  let semantic = {
    let mut mask = [false; 256];
    for edge in [
      vorpal_kg::EdgeType::CALLS,
      vorpal_kg::EdgeType::REFERENCES,
      vorpal_kg::EdgeType::IMPORTS,
      vorpal_kg::EdgeType::IMPLEMENTS,
      vorpal_kg::EdgeType::OF_TYPE,
      vorpal_kg::EdgeType::OVERRIDES,
    ] {
      mask[edge.0 as usize & 0xff] = true;
    }
    mask
  };
  let n = kg.node_count() as u64;
  let kind_tags = kg.kind_tag_lookup();

  // Per-node semantic in-degree (u32; saturating — a >4B-edge node is already a hub).
  let in_semantic: Vec<u32> = (0..n)
    .into_par_iter()
    .map(|row| {
      kg.in_edge_types_of(NodeId::new(row))
        .iter()
        .filter(|&&packed| semantic[vorpal_kg::EdgeType(packed).base().0 as usize & 0xff])
        .count() as u32
    })
    .collect();

  // Modules from the FILE RUNS, not the node table: one iteration per file (72K at kernel
  // scale) instead of per node (2.7M) — the run carries the path and its definition count
  // (`len - 1`), and the File node is `run.start` for the import margins.
  let runs = crate::cached_runs(kg, artifacts_dir);
  let mut modules: std::collections::BTreeMap<&str, ModuleRow> = std::collections::BTreeMap::new();
  fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
  }
  let blank = |module: &str| ModuleRow {
    module: module.to_string(),
    files: 0,
    definitions: 0,
    imported_by_others: 0,
    imports_others: 0,
  };
  for run in runs.iter() {
    let module = dir_of(&run.path);
    let mut cross: Vec<&str> = Vec::new();
    for (target, edge) in kg.out_neighbors(NodeId::new(run.start)) {
      if edge.base() != vorpal_kg::EdgeType::IMPORTS {
        continue;
      }
      let Some(target_path) = kg.node_path(target) else { continue };
      let target_module = dir_of(target_path);
      if target_module != module {
        cross.push(target_module);
      }
    }
    let entry = modules.entry(module).or_insert_with(|| blank(module));
    entry.files += 1;
    entry.definitions += u64::from(run.len.saturating_sub(1));
    entry.imports_others += cross.len() as u64;
    for target_module in cross {
      modules
        .entry(target_module)
        .or_insert_with(|| blank(target_module))
        .imported_by_others += 1;
    }
  }
  let total_modules = modules.len() as u64;
  let mut modules: Vec<ModuleRow> = modules.into_values().collect();
  modules.sort_by(|a, b| {
    b.definitions
      .cmp(&a.definitions)
      .then_with(|| a.module.cmp(&b.module))
  });
  modules.truncate(top);

  // Hubs: top-N semantic in-degree. One partial pass keeps this O(n log top).
  let mut hub_ids: Vec<(u32, u64)> = in_semantic
    .iter()
    .enumerate()
    .filter(|&(_, &d)| d > 0)
    .map(|(row, &d)| (d, row as u64))
    .collect();
  // Partial select: only the top slice ever sorts (1.5M nonzero rows at kernel scale).
  let cmp = |a: &(u32, u64), b: &(u32, u64)| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1));
  if hub_ids.len() > top {
    hub_ids.select_nth_unstable_by(top - 1, cmp);
    hub_ids.truncate(top);
  }
  hub_ids.sort_unstable_by(cmp);
  let hubs = hub_ids
    .into_iter()
    .filter_map(|(d, row)| {
      Some(HubRecord {
        node: node_record(kg, NodeId::new(row))?,
        semantic_in_degree: d as u64,
      })
    })
    .collect();

  // Entries: exported, semantically unreached, callable kinds — id order, capped.
  let callable = [
    vorpal_kg::SymbolKind::Function.tag(),
    vorpal_kg::SymbolKind::Method.tag(),
  ];
  let mut entries = Vec::new();
  for row in 0..n {
    if entries.len() >= top {
      break;
    }
    if in_semantic[row as usize] != 0 {
      continue;
    }
    let id = NodeId::new(row);
    let tag = match &kind_tags {
      Some(tags) => tags.get(row),
      None => kg.node_kind(id).map(|kind| kind.tag()),
    };
    if !tag.is_some_and(|tag| callable.contains(&tag)) {
      continue;
    }
    let Some(record) = node_record(kg, id) else { continue };
    if record.exported && record.class == "source" {
      entries.push(record);
    }
  }

  // Clusters from the warm-time community sidecar. Community ids are dense, so sizing is
  // one integer pass; only the `top` largest multi-member communities are then walked for
  // a face (highest semantic in-degree member) and a plurality module — no strings touched
  // for the millions of rows outside them. An unbuilt sidecar is stated, never rendered as
  // "no communities".
  let (clusters, total_clusters, clusters_note) = match kg.communities() {
    None => (
      Vec::new(),
      0,
      Some(
        "communities not built for this generation — a search warm (or the daemon's \
         background warm) builds them"
          .to_string(),
      ),
    ),
    Some(table) => {
      let is_file = |row: usize| match &kind_tags {
        Some(tags) => tags.get(row as u64) == Some(vorpal_kg::SymbolKind::File.tag()),
        None => kg.node_kind(NodeId::new(row as u64)) == Some(vorpal_kg::SymbolKind::File),
      };
      let community_count = table.iter().copied().max().map_or(0, |m| m as usize + 1);
      let mut members = vec![0u64; community_count];
      for (row, &community) in table.iter().enumerate() {
        if !is_file(row) {
          members[community as usize] += 1;
        }
      }
      let total = members.iter().filter(|&&m| m > 1).count() as u64;
      let mut ranked: Vec<(u64, u32)> = members
        .iter()
        .enumerate()
        .filter(|(_, m)| **m > 1)
        .map(|(community, &m)| (m, community as u32))
        .collect();
      ranked.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
      ranked.truncate(top);
      // slot[community] = rank within the top list, for the single gathering pass.
      let mut slot = vec![u32::MAX; community_count];
      for (rank, &(_, community)) in ranked.iter().enumerate() {
        slot[community as usize] = rank as u32;
      }
      struct Face<'a> {
        best: u64,
        best_degree: u32,
        modules: std::collections::HashMap<&'a str, u64>,
      }
      let mut faces: Vec<Option<Face>> = (0..ranked.len()).map(|_| None).collect();
      for (row, &community) in table.iter().enumerate() {
        let rank = slot[community as usize];
        if rank == u32::MAX || is_file(row) {
          continue;
        }
        let degree = in_semantic[row];
        let face = faces[rank as usize].get_or_insert_with(|| Face {
          best: row as u64,
          best_degree: degree,
          modules: std::collections::HashMap::new(),
        });
        if degree > face.best_degree {
          face.best_degree = degree;
          face.best = row as u64;
        }
        if let Some(path) = kg.node_path(NodeId::new(row as u64)) {
          *face.modules.entry(dir_of(path)).or_insert(0) += 1;
        }
      }
      let clusters = ranked
        .into_iter()
        .zip(faces)
        .filter_map(|((count, community), face)| {
          let face = face?;
          let representative = node_record(kg, NodeId::new(face.best))?;
          let mut modules: Vec<(&str, u64)> = face.modules.into_iter().collect();
          modules.sort_unstable_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(y.0)));
          Some(ClusterRow {
            community,
            members: count,
            representative,
            dominant_module: modules.first().map(|(m, _)| (*m).to_string()).unwrap_or_default(),
          })
        })
        .collect();
      (clusters, total, None)
    }
  };

  ArchitectureReport {
    modules,
    hubs,
    entries,
    total_modules,
    clusters,
    total_clusters,
    clusters_note,
  }
}

/// Rendered architecture summary.
pub fn render_architecture(report: &ArchitectureReport) -> String {
  use std::fmt::Write;
  let mut out = String::new();
  let _ = writeln!(out, "modules ({} total; top by definitions):", report.total_modules);
  for row in &report.modules {
    let _ = writeln!(
      out,
      "  {}  {} defs · {} files · imported-by-others {} · imports-others {}",
      row.module, row.definitions, row.files, row.imported_by_others, row.imports_others
    );
  }
  let _ = writeln!(out, "hubs (top by semantic in-degree):");
  for hub in &report.hubs {
    let _ = writeln!(
      out,
      "  {:>7}  {} [{}] {}",
      hub.semantic_in_degree, hub.node.name, hub.node.kind, hub.node.path
    );
  }
  let _ = writeln!(out, "entry-point candidates (exported, semantically unreached):");
  for entry in &report.entries {
    let _ = writeln!(out, "  {} [{}] {}", entry.name, entry.kind, entry.path);
  }
  match &report.clusters_note {
    Some(note) => {
      let _ = writeln!(out, "clusters: {note}");
    }
    None => {
      let _ = writeln!(
        out,
        "clusters ({} calls-graph communities; top by size):",
        report.total_clusters
      );
      for row in &report.clusters {
        let _ = writeln!(
          out,
          "  #{:<6} {:>6} members  {} [{}] {}  ({})",
          row.community,
          row.members,
          row.representative.name,
          row.representative.kind,
          row.representative.path,
          row.dominant_module
        );
      }
    }
  }
  out
}

/// One node-level generation difference.
#[derive(Serialize, Debug)]
pub struct DiffRecord {
  /// added | removed | modified (by durable eid; modified = same eid, new content hash).
  pub change: String,
  #[serde(flatten)]
  pub node: NodeRecord,
}

/// One page of a generation diff, with whole-diff totals.
#[derive(Serialize, Debug)]
pub struct DiffPage {
  pub records: Vec<DiffRecord>,
  pub total: usize,
  pub start: usize,
  pub end: usize,
  pub from_generation: String,
  pub to_generation: String,
  pub files_unchanged: usize,
  pub files_added: usize,
  pub files_removed: usize,
  pub files_changed: usize,
  /// (relation, from-count, to-count).
  pub relations: Vec<(String, u64, u64)>,
}

/// Page-materialize a [`crate::gendiff::GenDiff`]: removed nodes render from `from`,
/// added/modified from `to`.
pub fn diff_page(
  from: &Kg,
  to: &Kg,
  diff: crate::gendiff::GenDiff,
  page: PageRequest<'_>,
) -> Result<DiffPage, String> {
  let PageBounds { start, end, total } = page_bounds(diff.changes.len(), page.cursor, page.limit)?;
  let records = diff.changes[start..end]
    .iter()
    .filter_map(|change| {
      let (label, kg, id) = match change {
        crate::gendiff::NodeChange::Added(id) => ("added", to, *id),
        crate::gendiff::NodeChange::Removed(id) => ("removed", from, *id),
        crate::gendiff::NodeChange::Modified(id) => ("modified", to, *id),
      };
      Some(DiffRecord {
        change: label.to_string(),
        node: node_record(kg, id)?,
      })
    })
    .collect();
  Ok(DiffPage {
    records,
    total,
    start,
    end,
    from_generation: diff.from_generation,
    to_generation: diff.to_generation,
    files_unchanged: diff.files_unchanged,
    files_added: diff.files_added,
    files_removed: diff.files_removed,
    files_changed: diff.files_changed,
    relations: diff.relation_deltas,
  })
}

/// Rendered diff page: totals head, relation deltas, then one line per change.
pub fn render_diff(report: &DiffPage) -> String {
  use std::fmt::Write;
  let mut out = String::new();
  let _ = writeln!(
    out,
    "{} → {}: {} files changed, {} added, {} removed, {} unchanged; {} node changes",
    report.from_generation,
    report.to_generation,
    report.files_changed,
    report.files_added,
    report.files_removed,
    report.files_unchanged,
    report.total
  );
  for (name, from, to) in &report.relations {
    if from != to {
      let _ = writeln!(out, "relation {name}: {from} → {to}");
    }
  }
  for record in &report.records {
    let _ = writeln!(
      out,
      "{:<9} {} [{}] {}",
      record.change, record.node.name, record.node.kind, record.node.path
    );
  }
  if report.end < report.total {
    let _ = writeln!(out, "… {} more — page the records surface", report.total - report.end);
  }
  out
}

/// One page of a change-impact query, with the whole-scan honesty head.
#[derive(Serialize, Debug)]
pub struct ImpactPage {
  /// This page of impacted nodes (min-hop BFS order over the whole seed set).
  pub records: Vec<ReachRecord>,
  pub total: usize,
  pub start: usize,
  pub end: usize,
  /// Changed paths git reported.
  pub changed_files: usize,
  /// Changed paths with no File node in this generation (deleted, unindexed, or renamed):
  /// their impact is NOT included — stated, never silently dropped.
  pub missing_files: usize,
  /// BFS seeds (changed File nodes + their definitions).
  pub seeds: usize,
}

/// The impact closure for pre-resolved seeds, page-materialized like every whole-graph
/// surface. Depth is min-hop from any seed by construction (one multi-seed BFS).
#[allow(clippy::too_many_arguments)]
pub fn impact_page(
  kg: &Kg,
  seeds: &[NodeId],
  relations: &[vorpal_kg::EdgeType],
  max_depth: Option<u32>,
  min_confidence: u8,
  counts: (usize, usize),
  page: PageRequest<'_>,
) -> Result<ImpactPage, String> {
  let steps = kg.reachable_via_paths_multi(
    seeds,
    vorpal_kg::Direction::In,
    relations,
    max_depth,
    min_confidence,
  );
  let PageBounds { start, end, total } = page_bounds(steps.len(), page.cursor, page.limit)?;
  let records = steps[start..end]
    .iter()
    .filter_map(|step| {
      Some(ReachRecord {
        node: node_record(kg, NodeId::new(step.node as u64))?,
        depth: step.depth,
        via: step.via.0 as u64,
        relation: step.via.1.name().to_string(),
        grade: crate::confidence_label(step.via.1.confidence()).to_string(),
        edge_direction: if step.inbound { "in" } else { "out" }.to_string(),
        // Impact semantics are blast-radius, not argument tracing — no sidecar join here.
        flow_exprs: Vec::new(),
      })
    })
    .collect();
  Ok(ImpactPage {
    records,
    total,
    start,
    end,
    changed_files: counts.0,
    missing_files: counts.1,
    seeds: seeds.len(),
  })
}

/// Rendered impact page: the blast-radius head, then one line per impacted node.
pub fn render_impact(report: &ImpactPage) -> String {
  use std::fmt::Write;
  let mut out = String::new();
  let _ = writeln!(
    out,
    "{} changed files ({} not in this index) → {} seed definitions → {} impacted nodes:",
    report.changed_files, report.missing_files, report.seeds, report.total
  );
  for record in &report.records {
    let _ = writeln!(
      out,
      "depth {}  {} [{}] {}  ({}, {})",
      record.depth, record.node.name, record.node.kind, record.node.path, record.relation,
      record.grade
    );
  }
  if report.end < report.total {
    let _ = writeln!(
      out,
      "… {} more — page the records surface, or bound --depth / raise --min-grade",
      report.total - report.end
    );
  }
  if report.total == 0 && report.changed_files > 0 {
    let _ = writeln!(
      out,
      "(no inbound edges reach the changed definitions under those relations/grade)"
    );
  }
  out
}

/// The typed twin of `reachable`: BFS steps in deterministic order, each step carrying its
/// parent and the (grade-labeled) edge that reached it.
pub fn reach_records(
  kg: &Kg,
  target: &GraphTarget,
  dir: vorpal_kg::Direction,
  relations: &[vorpal_kg::EdgeType],
  max_depth: Option<u32>,
  min_confidence: u8,
) -> Result<Selected<ReachRecord>, String> {
  let matches = resolve_target(kg, target).map_err(|err| err.to_string())?;
  if matches.is_empty() {
    return Ok(Selected::NoMatch);
  }
  if matches.len() > 1 && !target.merge_all {
    return Ok(Selected::Ambiguous(
      matches.iter().filter_map(|&id| node_record(kg, id)).collect(),
    ));
  }
  let mut records = Vec::new();
  for &seed in &matches {
    for step in kg.reachable_via_paths(seed, dir, relations, max_depth, min_confidence) {
      let Some(node) = node_record(kg, NodeId::new(step.node as u64)) else {
        continue;
      };
      records.push(ReachRecord {
        node,
        depth: step.depth,
        via: step.via.0 as u64,
        relation: step.via.1.name().to_string(),
        grade: crate::confidence_label(step.via.1.confidence()).to_string(),
        edge_direction: if step.inbound { "in" } else { "out" }.to_string(),
        // The unpaged variant serves dir-less callers; annotations need the sidecar.
        flow_exprs: Vec::new(),
      });
    }
  }
  Ok(Selected::Hits(records))
}

#[cfg(test)]
mod paging_tests {
  use super::*;

  #[test]
  fn lean_tabular_profile_matches_the_spec() {
    let rows = vec![
      serde_json::json!({"name": "alpha", "kind": "Function", "path": "src/x.rs", "exported": true, "grade": null}),
      serde_json::json!({"name": "42", "kind": "T", "path": "src/y.rs", "exported": false, "grade": "exact"}),
    ];
    let lean = lean_from_values(&rows);
    let lines: Vec<&str> = lean.lines().collect();
    assert_eq!(lines[0], "records[2]:name\tkind\tpath\tgrade\texported");
    assert_eq!(lines[1], "  alpha\tFunction\tsrc/x.rs\t_\tT");
    // Numeric-looking and keyword-colliding strings quote; booleans abbreviate.
    assert_eq!(lines[2], "  \"42\"\t\"T\"\tsrc/y.rs\texact\tF");
    assert_eq!(lean_from_values(&[]), "records[0]:\n");
  }

  #[test]
  fn toon_declares_columns_groups_dirs_and_escapes() {
    let rows = vec![
      serde_json::json!({"name": "alpha", "kind": "Function", "path": "src/a/x.rs", "grade": "exact"}),
      serde_json::json!({"name": "beta\ttabbed", "kind": "Struct", "path": "src/a/y.rs", "grade": "exact"}),
      serde_json::json!({"name": "gamma", "kind": "Field", "path": "src/b/z.rs", "grade": null}),
    ];
    let toon = toon_from_values(&rows);
    let lines: Vec<&str> = toon.lines().collect();
    assert_eq!(lines[0], "cols: name\tkind\tpath\tgrade");
    assert_eq!(lines[1], "src/a/");
    assert_eq!(lines[2], "alpha\tFunction\tx.rs\texact");
    assert!(lines[3].starts_with("beta\\ttabbed\t"), "tab escaped: {:?}", lines[3]);
    assert_eq!(lines[4], "src/b/");
    assert!(lines[5].ends_with("\t_"), "null renders as _: {:?}", lines[5]);
    assert_eq!(toon_from_values(&[]), "(no records)\n");

    let ids = ids_from_values(&[
      serde_json::json!({"external_id": "eid:00ff", "id": 7}),
      serde_json::json!({"id": 9}),
    ]);
    assert_eq!(ids, "eid:00ff\nid:9\n");
  }

  #[test]
  fn lean_omits_fat_columns_and_factors_absolute_roots() {
    let rows = vec![
      serde_json::json!({"name": "a", "kind": "Function", "path": "/repo/src/fs/x.c",
        "signature": "static int a(void)", "span": [1, 2], "external_id": "eid:ff", "id": 1}),
      serde_json::json!({"name": "b", "kind": "Function", "path": "/repo/src/mm/y.c",
        "signature": "static int b(void)", "span": [3, 4], "external_id": "eid:aa", "id": 2}),
    ];
    let lean = lean_from_values(&rows);
    let lines: Vec<&str> = lean.lines().collect();
    assert_eq!(lines[0], "records[2]:name\tkind\tpath\tid");
    assert_eq!(lines[1], "root: /repo/src/");
    assert_eq!(lines[2], "  a\tFunction\tfs/x.c\t1");
    assert!(!lean.contains("signature") && !lean.contains("eid:"), "fat columns omitted");

    let toon = toon_from_values(&rows);
    let tlines: Vec<&str> = toon.lines().collect();
    assert_eq!(tlines[1], "root: /repo/src/");
    assert_eq!(tlines[2], "fs/");
    // toon stays LOSSLESS: signature/eid columns survive there.
    assert!(toon.contains("eid:ff") && toon.contains("static int a(void)"));

    // Relative paths (tests, non-canonical callers): no root line, cells untouched.
    let rel = vec![serde_json::json!({"name": "n", "path": "src/x.rs"})];
    assert!(!lean_from_values(&rel).contains("root:"));
  }

  #[test]
  fn page_bounds_contract() {
    // Defaults: offset 0, limit 100.
    let b = page_bounds(250, None, None).unwrap();
    assert_eq!((b.start, b.end, b.total), (0, 100, 250));
    // Cursor resumes; limit clamps to [1, 1000].
    let b = page_bounds(250, Some("o:200"), Some(0)).unwrap();
    assert_eq!((b.start, b.end), (200, 201));
    let b = page_bounds(5000, Some("o:100"), Some(9999)).unwrap();
    assert_eq!((b.start, b.end), (100, 1100));
    // Past-the-end cursor yields an empty page, never an error.
    let b = page_bounds(10, Some("o:50"), None).unwrap();
    assert_eq!((b.start, b.end, b.total), (10, 10, 10));
    // Malformed cursors are errors, never a silent first page.
    assert!(page_bounds(10, Some("page-2"), None).is_err());
    assert!(page_bounds(10, Some("o:-1"), None).is_err());
  }

  #[test]
  fn paged_value_declares_truncation() {
    let rows: Vec<u32> = (0..7).collect();
    let v = paged_value(&rows, None, Some(3), "hits").unwrap();
    assert_eq!(v["total"], 7);
    assert_eq!(v["truncated"], true);
    assert_eq!(v["nextCursor"], "o:3");
    assert_eq!(v["records"].as_array().unwrap().len(), 3);
    let last = paged_value(&rows, Some("o:6"), Some(3), "hits").unwrap();
    assert_eq!(last["truncated"], false);
    assert!(last.get("nextCursor").is_none());
  }
}

/// The typed twin of `why`: the retained evidence occurrences from `from_id` — the edge form
/// (`to` given) or the absence form (`name` given: no-edge outcomes for that referenced
/// name, plus any real edges to nodes carrying it, so a partial answer is never mistaken for
/// none).
pub fn evidence_records(
  kg: &Kg,
  from_id: u64,
  to_id: Option<u64>,
  name: Option<&str>,
) -> Vec<EvidenceRecord> {
  let from = NodeId::new(from_id);
  let mut rows = Vec::new();
  match (to_id, name) {
    (Some(to), _) => {
      rows.extend(
        kg.evidence_from(from)
          .into_iter()
          .filter(|row| row.to as u64 == to),
      );
    }
    (None, Some(name)) => {
      let name_hash = xxhash_rust::xxh3::xxh3_64(name.as_bytes()) as u32;
      rows.extend(kg.evidence_absences(from, name_hash));
      rows.extend(kg.evidence_from(from).into_iter().filter(|row| {
        row.outcome == vorpal_kg::EvidenceOutcome::Edge
          && kg
            .node(NodeId::new(row.to as u64))
            .is_some_and(|view| view.name == name)
      }));
    }
    (None, None) => {}
  }
  rows
    .into_iter()
    .map(|row| EvidenceRecord {
      from: row.from as u64,
      to: (row.outcome == vorpal_kg::EvidenceOutcome::Edge).then_some(row.to as u64),
      relation: vorpal_kg::EdgeType(row.etype).name().to_string(),
      outcome: match row.outcome {
        vorpal_kg::EvidenceOutcome::Edge => "edge",
        vorpal_kg::EvidenceOutcome::External => "external",
        vorpal_kg::EvidenceOutcome::Masked => "masked",
      }
      .to_string(),
      grade: vorpal_ingest::Confidence(row.confidence).grade().label().to_string(),
      reason: vorpal_ingest::ResolveReason::from_tag(row.reason).label().to_string(),
      candidates: row.candidates,
      span: [row.span_start, row.span_end],
    })
    .collect()
}

#[cfg(test)]
mod dead_bench {
  use super::*;

  /// Sub-step timing against a real index: `VORPAL_BENCH_INDEX=<dir> cargo test --release
  /// -p vorpal-index --lib dead_bench -- --ignored --nocapture`.
  #[test]
  #[ignore = "diagnostic: localizes dead-scan cost on a real index"]
  fn where_does_dead_spend() {
    use rayon::prelude::*;
    let Ok(dir) = std::env::var("VORPAL_BENCH_INDEX") else { return };
    let kg = Kg::load(std::path::Path::new(&dir)).unwrap();
    let t = std::time::Instant::now();
    let filter = DeadFilter::default();
    let report = dead_records_page(&kg, None, &filter, PageRequest::default()).unwrap();
    println!("whole (no pack): {:?} ({} candidates)", t.elapsed(), report.total);

    let mut allowed = [false; 256];
    for kind in [
      vorpal_kg::SymbolKind::Function,
      vorpal_kg::SymbolKind::Method,
      vorpal_kg::SymbolKind::Class,
      vorpal_kg::SymbolKind::Struct,
      vorpal_kg::SymbolKind::Enum,
      vorpal_kg::SymbolKind::Interface,
      vorpal_kg::SymbolKind::Constructor,
    ] {
      allowed[kind.tag() as usize] = true;
    }
    let semantic = {
      let mut mask = [false; 256];
      for edge in [
        vorpal_kg::EdgeType::CALLS,
        vorpal_kg::EdgeType::REFERENCES,
        vorpal_kg::EdgeType::IMPORTS,
        vorpal_kg::EdgeType::IMPLEMENTS,
        vorpal_kg::EdgeType::OF_TYPE,
        vorpal_kg::EdgeType::OVERRIDES,
      ] {
        mask[edge.0 as usize & 0xff] = true;
      }
      mask
    };
    let n = kg.node_count() as u64;
    let tags = kg.kind_tag_lookup().unwrap();

    let t = std::time::Instant::now();
    let kind_only: u64 = (0..n)
      .into_par_iter()
      .filter(|&r| tags.get(r).is_some_and(|tag| allowed[tag as usize]))
      .count() as u64;
    println!("kind gate only: {:?} ({kind_only} pass)", t.elapsed());

    let t = std::time::Instant::now();
    let survivors: Vec<u64> = (0..n)
      .into_par_iter()
      .filter(|&r| {
        tags.get(r).is_some_and(|tag| allowed[tag as usize])
          && !kg
            .in_edge_types_of(NodeId::new(r))
            .iter()
            .any(|&p| semantic[vorpal_kg::EdgeType(p).base().0 as usize & 0xff])
      })
      .collect();
    println!("+ in-edge scan: {:?} ({} pass)", t.elapsed(), survivors.len());

    let t = std::time::Instant::now();
    let viewed = survivors.iter().filter(|&&r| kg.node(NodeId::new(r)).is_some()).count();
    println!("+ survivor views (serial): {:?} ({viewed})", t.elapsed());

    let t = std::time::Instant::now();
    let mut hashes: Vec<u32> = Vec::new();
    kg.for_each_evidence_name_hash(|h| hashes.push(h));
    println!("name-hash collect: {:?} ({} rows)", t.elapsed(), hashes.len());
    let t = std::time::Instant::now();
    hashes.par_sort_unstable();
    hashes.dedup();
    println!("sort+dedup: {:?} ({} distinct)", t.elapsed(), hashes.len());

    let t = std::time::Instant::now();
    let built = survivors
      .iter()
      .filter_map(|&r| node_record(&kg, NodeId::new(r)))
      .count();
    println!("record build for survivors: {:?} ({built})", t.elapsed());
  }
}

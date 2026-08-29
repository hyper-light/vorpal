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
}

/// A node related to the query target through one edge, with the edge's resolution grade
/// (`structural` for containment edges, else exact/constrained/heuristic).
#[derive(Serialize, Debug)]
pub struct RelatedRecord {
  #[serde(flatten)]
  pub node: NodeRecord,
  pub grade: String,
}

/// One step of a relation-restricted traversal: the reached node, its BFS depth, the node it
/// was first reached from, and the edge that reached it.
#[derive(Serialize, Debug)]
pub struct ReachRecord {
  #[serde(flatten)]
  pub node: NodeRecord,
  pub depth: u32,
  pub via: u64,
  pub relation: String,
  pub grade: String,
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
}

/// One definition's source text, sliced from its persisted byte span and digest-verified
/// against the generation that recorded it — the selector-driven twin of `fetch_span`.
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
    if cached.as_ref().is_none_or(|(path, ..)| path != &node.path) {
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
    }
    let (_, bytes, verification) = cached.as_ref().expect("just populated");
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
  })
}

/// The typed candidate listing for a selector — the record twin of the `node` verb. Listing
/// never treats multiple matches as ambiguity: the matches ARE the answer.
pub fn listing_records(kg: &Kg, target: &GraphTarget) -> Result<Vec<NodeRecord>, String> {
  let matches = resolve_target(kg, target).map_err(|err| err.to_string())?;
  Ok(matches.iter().filter_map(|&id| node_record(kg, id)).collect())
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
        })
      })
      .collect(),
  ))
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
      });
    }
  }
  Ok(Selected::Hits(records))
}

#[cfg(test)]
mod paging_tests {
  use super::*;

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

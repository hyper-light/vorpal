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

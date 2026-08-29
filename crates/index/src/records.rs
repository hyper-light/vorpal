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
  let kind_tags = kg.kind_tags();
  let candidates: Vec<u64> = (0..node_count)
    .into_par_iter()
    .filter(|&row| {
      let id = NodeId::new(row);
      let tag = match kind_tags {
        Some(tags) => tags.get(row as usize).copied(),
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
pub fn reach_records_page(
  kg: &Kg,
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
    let tags = kg.kind_tags().unwrap();

    let t = std::time::Instant::now();
    let kind_only: u64 = (0..n).into_par_iter().filter(|&r| allowed[tags[r as usize] as usize]).count() as u64;
    println!("kind gate only: {:?} ({kind_only} pass)", t.elapsed());

    let t = std::time::Instant::now();
    let survivors: Vec<u64> = (0..n)
      .into_par_iter()
      .filter(|&r| {
        allowed[tags[r as usize] as usize]
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

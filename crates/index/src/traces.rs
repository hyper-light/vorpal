//! Runtime-trace ingestion (ADOPTION #26): folded stacks (`main;foo;bar 42` — perf +
//! stackcollapse, py-spy, inferno) become observed caller→callee rows in the
//! `observed.bin` sidecar. Observed evidence proves calls static resolution never can —
//! dynamic dispatch, function pointers — and it lives BESIDE the static graph, never
//! inside it: the generation id is untouched, and a rebuild invalidates the sidecar
//! (re-ingest the traces) rather than silently carrying stale node ids.
//!
//! Frame resolution is deliberately conservative: a frame links only when its normalized
//! name matches exactly one callable definition (falling back to the last `.`/`::`
//! segment for qualified frames). Unknown and ambiguous frames are counted and sampled on
//! the report; a chain never links ACROSS an unresolved frame — that would claim a direct
//! call the trace does not show.

use std::collections::HashMap;
use std::path::Path;

use vorpal_kg::{Kg, SymbolKind};

/// What one ingestion did — every number a user needs to trust (or distrust) the result.
#[derive(Debug, Default, Clone)]
pub struct TraceReport {
  /// Folded stack lines read (blank lines skipped).
  pub stacks: u64,
  /// Lines that carried no trailing count and were skipped.
  pub malformed: u64,
  /// Adjacent frame pairs seen (weighted occurrences are summed into rows, not here).
  pub pairs: u64,
  /// Distinct observed caller→callee rows written.
  pub rows: u64,
  /// Distinct frame names that matched no definition (external, inlined, unparsed).
  pub unknown_frames: u64,
  /// Distinct frame names that matched more than one definition — refused, never guessed.
  pub ambiguous_frames: u64,
  /// Up to eight unknown/ambiguous frame names, for a human to eyeball.
  pub samples: Vec<String>,
}

enum Resolution {
  Node(u32),
  Unknown,
  Ambiguous,
}

/// Strip the decorations profilers append: `tcp_v4_rcv+0x1a`, `sym [kernel.kallsyms]`,
/// py-spy's `func (file.py:12)`.
fn normalize(frame: &str) -> &str {
  let mut f = frame.trim();
  if let Some((head, _)) = f.split_once("+0x") {
    f = head;
  }
  if let Some((head, _)) = f.split_once(" [") {
    f = head;
  }
  if let Some((head, _)) = f.split_once(" (") {
    f = head;
  }
  f.trim()
}

fn callable_matches(kg: &Kg, name: &str) -> Vec<u32> {
  kg
    .select(&vorpal_kg::SymbolSelector {
      id: None,
      name: Some(name),
      path_suffix: None,
      kind: None,
      external_id: None,
    })
    .into_iter()
    .filter(|&id| {
      kg.node(id).is_some_and(|view| {
        matches!(
          view.kind,
          SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor
        )
      })
    })
    .map(|id| id.raw() as u32)
    .collect()
}

fn resolve(kg: &Kg, frame: &str) -> Resolution {
  let name = normalize(frame);
  if name.is_empty() {
    return Resolution::Unknown;
  }
  let mut matches = callable_matches(kg, name);
  if matches.is_empty() {
    // Qualified frames (`module.func`, `Type::method`): try the trailing segment.
    if let Some(last) = name.rsplit(['.', ':']).next().filter(|l| *l != name && !l.is_empty()) {
      matches = callable_matches(kg, last);
    }
  }
  match matches.len() {
    0 => Resolution::Unknown,
    1 => Resolution::Node(matches[0]),
    _ => Resolution::Ambiguous,
  }
}

/// Ingest one folded-stacks file into `index_dir`'s CURRENT generation. Returns the
/// report; the sidecar lands only when at least one row resolved.
pub fn ingest_traces(index_dir: &Path, folded: &Path) -> Result<TraceReport, Box<dyn std::error::Error>> {
  let kg = Kg::load(index_dir)?;
  let gen_dir = crate::resolve_index_dir(index_dir);
  let text = std::fs::read_to_string(folded)
    .map_err(|err| format!("read folded stacks {}: {err}", folded.display()))?;
  let mut report = TraceReport::default();
  let mut cache: HashMap<&str, Resolution> = HashMap::new();
  let mut counts: HashMap<(u32, u32), u64> = HashMap::new();
  let mut problem_names: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
  for line in text.lines() {
    let line = line.trim();
    if line.is_empty() {
      continue;
    }
    report.stacks += 1;
    // `frame;frame;frame COUNT` — the count is the last whitespace-separated token.
    let Some((stack, count_text)) = line.rsplit_once(' ') else {
      report.malformed += 1;
      continue;
    };
    let Ok(count) = count_text.trim().parse::<u64>() else {
      report.malformed += 1;
      continue;
    };
    let mut previous: Option<u32> = None;
    for frame in stack.split(';') {
      let resolution = cache
        .entry(frame)
        .or_insert_with(|| resolve(&kg, frame));
      match resolution {
        Resolution::Node(node) => {
          if let Some(from) = previous {
            report.pairs += 1;
            *counts.entry((from, *node)).or_insert(0) += count;
          }
          previous = Some(*node);
        }
        Resolution::Unknown | Resolution::Ambiguous => {
          problem_names.insert(frame);
          // A gap breaks the chain: linking across it would claim an unobserved call.
          previous = None;
        }
      }
    }
  }
  for (frame, resolution) in &cache {
    match resolution {
      Resolution::Unknown => report.unknown_frames += 1,
      Resolution::Ambiguous => report.ambiguous_frames += 1,
      Resolution::Node(_) => {
        let _ = frame;
      }
    }
  }
  report.samples = problem_names
    .into_iter()
    .take(8)
    .map(str::to_string)
    .collect();
  report.rows = counts.len() as u64;
  if report.rows > 0 {
    let rows: Vec<vorpal_kg::observed::ObservedRow> = counts
      .into_iter()
      .map(|((from, to), count)| vorpal_kg::observed::ObservedRow { from, to, count })
      .collect();
    vorpal_kg::observed::save_observed(&gen_dir, kg.node_segment_stamp(), rows)?;
  }
  Ok(report)
}

/// Rendered ingestion report.
pub fn render_trace_report(report: &TraceReport) -> String {
  let mut out = format!(
    "observed: {} rows from {} stacks ({} frame pairs); {} unknown and {} ambiguous frame \
     names skipped",
    report.rows, report.stacks, report.pairs, report.unknown_frames, report.ambiguous_frames
  );
  if report.malformed > 0 {
    out.push_str(&format!("; {} malformed lines", report.malformed));
  }
  if !report.samples.is_empty() {
    out.push_str(&format!("; e.g. {}", report.samples.join(", ")));
  }
  out.push('\n');
  if report.rows == 0 {
    out.push_str("nothing ingested — no adjacent frame pair resolved to definitions\n");
  }
  out
}

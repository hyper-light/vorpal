//! `text_search`: grep-shaped regex search over the generation's own files, through the same
//! candidate path as `code_search` — the text tier names the files that can hold every literal
//! the regex requires, the prefilter skips files it proves matchless, and the regex verifies
//! the rest. Records carry `path:line:column`, the line's text, and the innermost definition
//! the match sits in (the graph's spans), so an agent gets symbols, not just lines. A regex
//! with no three-byte literal (or a case-insensitive one) scans every file and says so.

use std::path::Path;

use serde::Serialize;
use vorpal_core::matcher::{Prefilter, regex_required_literals};
use vorpal_kg::identity::FileKey;
use vorpal_kg::trigramstore::Verdict;
use vorpal_kg::{Kg, NodeId};

/// One `text_search` request.
pub struct TextQuery<'a> {
  pub pattern: &'a str,
  pub case_insensitive: bool,
  pub lang: Option<&'a str>,
  pub prefix: Option<&'a str>,
  pub max_results: usize,
  /// Scope the scan to one symbol's definition span(s): every definition of that name (the
  /// graph's spans), read from their files, nothing else. No candidates, no walk.
  pub symbol: Option<&'a str>,
}

/// One matching line.
#[derive(Serialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextMatchRecord {
  pub path: String,
  pub line: u32,
  pub column: u32,
  pub text: String,
  /// The innermost definition containing the match, when the graph has one.
  pub symbol: Option<String>,
  pub kind: Option<String>,
}

/// The answer with its honesty margins.
#[derive(Serialize, Debug)]
pub struct TextSearchReport {
  pub records: Vec<TextMatchRecord>,
  pub total_matches: u64,
  pub matched_files: u64,
  pub truncated: bool,
  /// Files in scope after the lang/prefix filters.
  pub candidate_files: u64,
  /// Files the text tier proved cannot hold every required literal — never read.
  pub pruned_files: u64,
  /// Files read but proved matchless by the literal prefilter — never regex-scanned.
  pub prefiltered_files: u64,
  /// Files the regex actually scanned.
  pub scanned_files: u64,
  pub stale_files: u64,
  pub unreadable_files: u64,
  /// `trigram` when the tier pruned, `full-scan` when it could not (and why).
  pub index: String,
  pub index_reason: Option<String>,
  /// `fresh`, `partial(live/total)`, or `absent`.
  pub text_index: String,
}

pub const MAX_RESULTS_CAP: usize = 10_000;

/// Run `q` over the generation under `artifacts_dir` (the graph's file set), digest-verified
/// against the product pack: files whose bytes changed since the generation are counted stale
/// and skipped, never half-trusted.
pub fn text_search(
  kg: &Kg,
  artifacts_dir: Option<&Path>,
  q: &TextQuery<'_>,
) -> Result<TextSearchReport, String> {
  use rayon::prelude::*;
  use vorpal_ingest::SgLang;
  use vorpal_language::Language;

  let regex = regex::bytes::RegexBuilder::new(q.pattern)
    .case_insensitive(q.case_insensitive)
    .build()
    .map_err(|err| format!("bad regex: {err}"))?;
  if let Some(symbol) = q.symbol {
    return symbol_scoped(kg, artifacts_dir, q, symbol, &regex);
  }
  let literals: Vec<String> = if q.case_insensitive {
    Vec::new()
  } else {
    regex_required_literals(q.pattern)
  };
  let literal_refs: Vec<&str> = literals.iter().map(String::as_str).collect();
  let prefilter = Prefilter::from_literals(literal_refs.clone());

  let runs = crate::cached_runs(kg, artifacts_dir);
  let pack = artifacts_dir.and_then(crate::cached_pack);
  let pack_ref = pack.as_deref();
  let text = match (artifacts_dir, pack_ref) {
    (Some(dir), Some(pack)) => crate::trigrams::cached(dir, pack),
    _ => None,
  };
  let text_status = text.as_ref().map_or_else(|| "absent".to_string(), |t| t.status());
  if let Some(dir) = artifacts_dir
    && text.as_ref().is_none_or(|t| !t.is_fresh())
  {
    crate::trigrams::request_heal(dir);
  }
  // Candidates: one AND-plan per literal branch, unioned — `TODO|FIXME` prunes to the files
  // holding either instead of scanning everything (the required-literal intersection of an
  // alternation is usually empty).
  let branches: Vec<Vec<String>> = if q.case_insensitive {
    vec![Vec::new()]
  } else {
    vorpal_core::matcher::regex_literal_branches(q.pattern)
  };
  let candidates = text
    .as_ref()
    .and_then(|t| t.candidates_for_branches(&branches));
  let (index, index_reason) = match (&candidates, &text) {
    (Some(_), _) => ("trigram".to_string(), None),
    (None, None) => ("full-scan".to_string(), Some("no text index for this generation yet".to_string())),
    (None, Some(_)) if q.case_insensitive => ("full-scan".to_string(), Some("case-insensitive patterns carry no exact literal".to_string())),
    (None, Some(_)) => ("full-scan".to_string(), Some("the regex requires no literal of three bytes or more".to_string())),
  };
  let lang_ok = |path: &str| -> bool {
    match q.lang {
      None => true,
      Some(filter) => SgLang::from_path(path)
        .is_some_and(|lang| format!("{lang:?}").eq_ignore_ascii_case(filter) || lang.to_string() == filter),
    }
  };

  enum Outcome {
    Pruned,
    Prefiltered,
    Stale,
    Unreadable,
    Scanned(Vec<TextMatchRecord>),
  }
  vorpal_kg::phase_stamp("text_search: candidates ready");
  let text_ref = text.as_deref();
  let run_index = match (artifacts_dir, pack_ref) {
    (Some(dir), Some(pack)) => Some(crate::trigrams::cached_run_index(dir, &runs, pack)),
    _ => None,
  };
  // A complete candidate set names the files to read; only those are visited (per-thread
  // read buffers, no per-file allocation). Otherwise every run is tested.
  let admitted_runs: Option<Vec<u32>> = match (&candidates, &run_index) {
    (Some(set), Some(index)) if set.is_complete() => {
      let mut idx: Vec<u32> = set.admitted().iter().filter_map(|k| index.run_of(*k)).collect();
      idx.sort_unstable();
      Some(idx)
    }
    _ => None,
  };
  let scan_run = |run: &crate::annfiles::FileRun, check_verdict: bool, buf: &mut Vec<u8>| -> Option<Outcome> {
      if !lang_ok(&run.path) {
        return None;
      }
      if let Some(prefix) = q.prefix
        && !run.path.starts_with(prefix)
      {
        return None;
      }
      if check_verdict
        && let (Some(set), Some(text), Some(pack)) = (&candidates, text_ref, pack_ref)
      {
        let key = FileKey::of(pack.stored_key(&run.path)).0;
        if text.verdict(set, key) == Verdict::Pruned {
          return Some(Outcome::Pruned);
        }
      }
      match crate::read_indexed_source_into(pack_ref, &run.path, buf) {
        Ok(crate::IndexedReadVerdict::Verified) | Ok(crate::IndexedReadVerdict::Unverified) => {}
        Ok(crate::IndexedReadVerdict::Changed) => return Some(Outcome::Stale),
        Err(_) => return Some(Outcome::Unreadable),
      }
      let bytes: &[u8] = buf;
      if !prefilter.may_match_bytes(bytes) {
        return Some(Outcome::Prefiltered);
      }
      let mut records: Vec<TextMatchRecord> = Vec::new();
      let mut spans: Option<Vec<(u32, u32, u64)>> = None;
      let mut line_no: u32 = 1;
      let mut line_start: usize = 0;
      let mut cursor: usize = 0;
      let mut last_line_reported: Option<usize> = None;
      for found in regex.find_iter(&bytes) {
        let start = found.start();
        // advance the line counter to `start`
        while cursor < start {
          if bytes[cursor] == b'\n' {
            line_no += 1;
            line_start = cursor + 1;
          }
          cursor += 1;
        }
        if last_line_reported == Some(line_start) {
          continue; // one record per matching line, like grep
        }
        last_line_reported = Some(line_start);
        let line_end = bytes[start..]
          .iter()
          .position(|&b| b == b'\n')
          .map_or(bytes.len(), |p| start + p);
        let line_text: String = String::from_utf8_lossy(&bytes[line_start..line_end])
          .chars()
          .take(200)
          .collect();
        let spans = spans.get_or_insert_with(|| {
          (run.start..run.start + run.len as u64)
            .filter_map(|id| {
              let node = NodeId::new(id);
              if kg.node_kind(node)? == vorpal_kg::SymbolKind::File {
                return None;
              }
              let (start, end) = kg.node_span(node)?;
              (end > start).then_some((start, end, id))
            })
            .collect()
        });
        let owner = spans
          .iter()
          .filter(|&&(s, e, _)| (s as usize) <= start && start < e as usize)
          .min_by_key(|&&(s, e, _)| e - s)
          .and_then(|&(.., id)| kg.node(NodeId::new(id)))
          .map(|view| (view.name.to_string(), format!("{:?}", view.kind)));
        records.push(TextMatchRecord {
          path: run.path.clone(),
          line: line_no,
          column: (start - line_start) as u32 + 1,
          text: line_text,
          symbol: owner.as_ref().map(|(name, _)| name.clone()),
          kind: owner.map(|(_, kind)| kind),
        });
      }
      Some(Outcome::Scanned(records))
  };
  // Read buffers come from a pool, one per concurrently scanning worker, reused across files
  // and (through the process-wide pool) across queries — rayon's per-split init would hand
  // out a fresh buffer per split and re-fault it on every query.
  let scan_pooled = |run: &crate::annfiles::FileRun, check_verdict: bool| -> Option<Outcome> {
    let mut buf = crate::trigrams::take_read_buffer();
    let out = scan_run(run, check_verdict, &mut buf);
    crate::trigrams::give_read_buffer(buf);
    out
  };
  let (per_file, population): (Vec<Option<Outcome>>, u64) = match &admitted_runs {
    Some(idx) => {
      // The in-scope population, for the pruned count: per-language totals from the run
      // index when there is no prefix (no per-query walk of every run), a walk otherwise.
      let population = match (&run_index, q.prefix) {
        (Some(index), None) => index.count_where(|lang| match (q.lang, lang) {
          (None, _) => true,
          (Some(filter), Some(lang)) => format!("{lang:?}").eq_ignore_ascii_case(filter) || lang.to_string() == filter,
          (Some(_), None) => false,
        }),
        _ => runs
          .iter()
          .filter(|run| lang_ok(&run.path) && q.prefix.is_none_or(|p| run.path.starts_with(p)))
          .count() as u64,
      };
      (
        idx
          .par_iter()
          .with_min_len(32)
          .map(|&i| scan_pooled(&runs[i as usize], false))
          .collect(),
        population,
      )
    }
    None => (
      runs
        .par_iter()
        .with_min_len(256)
        .map(|run| scan_pooled(run, true))
        .collect(),
      0,
    ),
  };

  vorpal_kg::phase_stamp("text_search: scan done");
  let mut report = TextSearchReport {
    records: Vec::new(),
    total_matches: 0,
    matched_files: 0,
    truncated: false,
    candidate_files: 0,
    pruned_files: 0,
    prefiltered_files: 0,
    scanned_files: 0,
    stale_files: 0,
    unreadable_files: 0,
    index,
    index_reason,
    text_index: text_status,
  };
  let mut all: Vec<TextMatchRecord> = Vec::new();
  let mut seen = 0u64;
  for outcome in per_file.into_iter().flatten() {
    seen += 1;
    report.candidate_files += 1;
    match outcome {
      Outcome::Pruned => report.pruned_files += 1,
      Outcome::Prefiltered => report.prefiltered_files += 1,
      Outcome::Stale => report.stale_files += 1,
      Outcome::Unreadable => report.unreadable_files += 1,
      Outcome::Scanned(records) => {
        report.scanned_files += 1;
        if !records.is_empty() {
          report.matched_files += 1;
        }
        report.total_matches += records.len() as u64;
        all.extend(records);
      }
    }
  }
  if admitted_runs.is_some() {
    // Candidate-driven: files in scope that were never visited were pruned by the tier.
    report.pruned_files = population.saturating_sub(seen);
    report.candidate_files = population;
  }
  all.sort();
  let cap = q.max_results.clamp(1, MAX_RESULTS_CAP);
  if all.len() > cap {
    all.truncate(cap);
    report.truncated = true;
  }
  report.records = all;
  Ok(report)
}

/// [`text_search`] inside one symbol's definition span(s): the graph names every definition
/// called `symbol` with its file and byte span, so only those bytes are scanned. Records
/// carry the definition as their symbol. Files whose bytes changed since the generation are
/// stale (the span no longer describes them) and are counted, never guessed at.
fn symbol_scoped(
  kg: &Kg,
  artifacts_dir: Option<&Path>,
  q: &TextQuery<'_>,
  symbol: &str,
  regex: &regex::bytes::Regex,
) -> Result<TextSearchReport, String> {
  use vorpal_language::Language;
  let target = crate::GraphTarget {
    name: symbol.to_string(),
    id: None,
    external_id: None,
    path_suffix: q.prefix.map(str::to_string),
    kind: None,
    merge_all: true,
    show_ids: false,
  };
  let nodes = crate::resolve_target(kg, &target).map_err(|err| err.to_string())?;
  // (path, start, end, name, kind) per definition, grouped by path in id order.
  let mut spans: Vec<(String, u32, u32, String, String)> = Vec::new();
  for id in nodes {
    let Some(view) = kg.node(id) else { continue };
    if view.kind == vorpal_kg::SymbolKind::File || view.span.1 <= view.span.0 {
      continue;
    }
    if let Some(lang) = q.lang
      && !vorpal_ingest::SgLang::from_path(view.path).is_some_and(|l| format!("{l:?}").eq_ignore_ascii_case(lang) || l.to_string() == lang)
    {
      continue;
    }
    spans.push((view.path.to_string(), view.span.0, view.span.1, view.name.to_string(), format!("{:?}", view.kind)));
  }
  spans.sort();
  let pack = artifacts_dir.and_then(crate::cached_pack);
  let mut report = TextSearchReport {
    records: Vec::new(),
    total_matches: 0,
    matched_files: 0,
    truncated: false,
    candidate_files: 0,
    pruned_files: 0,
    prefiltered_files: 0,
    scanned_files: 0,
    stale_files: 0,
    unreadable_files: 0,
    index: "symbol-scoped".to_string(),
    index_reason: None,
    text_index: String::new(),
  };
  let mut all: Vec<TextMatchRecord> = Vec::new();
  let mut buf = crate::trigrams::take_read_buffer();
  let mut i = 0;
  while i < spans.len() {
    let path = spans[i].0.clone();
    let mut j = i;
    while j < spans.len() && spans[j].0 == path {
      j += 1;
    }
    report.candidate_files += 1;
    match crate::read_indexed_source_into(pack.as_deref(), &path, &mut buf) {
      Ok(crate::IndexedReadVerdict::Verified) | Ok(crate::IndexedReadVerdict::Unverified) => {}
      Ok(crate::IndexedReadVerdict::Changed) => {
        report.stale_files += 1;
        i = j;
        continue;
      }
      Err(_) => {
        report.unreadable_files += 1;
        i = j;
        continue;
      }
    }
    report.scanned_files += 1;
    let bytes: &[u8] = &buf;
    let mut file_matched = false;
    for (_, start, end, name, kind) in &spans[i..j] {
      let (start, end) = (*start as usize, (*end as usize).min(bytes.len()));
      if start >= end {
        continue;
      }
      let base_line = memchr::memchr_iter(b'\n', &bytes[..start]).count() as u32 + 1;
      let mut last_line_start: Option<usize> = None;
      for found in regex.find_iter(&bytes[start..end]) {
        let at = start + found.start();
        let line_start = bytes[..at].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
        if last_line_start == Some(line_start) {
          continue;
        }
        last_line_start = Some(line_start);
        let line_no = base_line + memchr::memchr_iter(b'\n', &bytes[start..at]).count() as u32;
        let line_end = bytes[at..].iter().position(|&b| b == b'\n').map_or(bytes.len(), |p| at + p);
        all.push(TextMatchRecord {
          path: path.clone(),
          line: line_no,
          column: (at - line_start) as u32 + 1,
          text: String::from_utf8_lossy(&bytes[line_start..line_end]).chars().take(200).collect(),
          symbol: Some(name.clone()),
          kind: Some(kind.clone()),
        });
        file_matched = true;
      }
    }
    if file_matched {
      report.matched_files += 1;
    }
    i = j;
  }
  crate::trigrams::give_read_buffer(buf);
  all.sort();
  all.dedup();
  report.total_matches = all.len() as u64;
  let cap = q.max_results.clamp(1, MAX_RESULTS_CAP);
  if all.len() > cap {
    all.truncate(cap);
    report.truncated = true;
  }
  report.records = all;
  Ok(report)
}

/// Textual mentions of `name` the graph did not attribute: every whole-word occurrence in
/// the indexed files (through the text tier) minus the files in `attributed` — the graph
/// hits' files and the definitions' own. A necessary condition made checkable: if this list
/// is empty and nothing was stale or truncated, no file outside the graph's answer spells
/// the name, so a rename or a delete has no unresolved reference to chase.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionReport {
  pub records: Vec<TextMatchRecord>,
  pub unattributed_files: u64,
  pub attributed_files: u64,
  pub candidate_files: u64,
  pub pruned_files: u64,
  pub scanned_files: u64,
  pub stale_files: u64,
  pub truncated: bool,
  pub index: String,
  /// True when the list is the complete answer: nothing stale, nothing truncated.
  pub complete: bool,
}

pub fn unattributed_mentions(
  kg: &Kg,
  artifacts_dir: Option<&Path>,
  name: &str,
  attributed: &std::collections::HashSet<&str>,
  max_results: usize,
) -> Result<MentionReport, String> {
  let pattern = format!(r"\b{}\b", regex::escape(name));
  let q = TextQuery {
    pattern: &pattern,
    case_insensitive: false,
    lang: None,
    prefix: None,
    max_results: MAX_RESULTS_CAP,
    symbol: None,
  };
  let report = text_search(kg, artifacts_dir, &q)?;
  let mut records: Vec<TextMatchRecord> = report
    .records
    .into_iter()
    .filter(|r| !attributed.contains(r.path.as_str()))
    .collect();
  let mut files: Vec<&str> = records.iter().map(|r| r.path.as_str()).collect();
  files.sort_unstable();
  files.dedup();
  let unattributed_files = files.len() as u64;
  let truncated = report.truncated || records.len() > max_results;
  records.truncate(max_results.max(1));
  Ok(MentionReport {
    records,
    unattributed_files,
    attributed_files: attributed.len() as u64,
    candidate_files: report.candidate_files,
    pruned_files: report.pruned_files,
    scanned_files: report.scanned_files,
    stale_files: report.stale_files,
    truncated,
    index: report.index,
    complete: report.stale_files == 0 && !truncated,
  })
}

/// The text block: a grep-shaped line per record, headed by the honesty margins.
pub fn render_text_search(report: &TextSearchReport) -> String {
  use std::fmt::Write;
  let mut out = String::new();
  let _ = writeln!(
    out,
    "{} matching lines in {} files ({} in scope: {} pruned by the text index [{}], {} prefiltered, {} scanned, {} stale, {} unreadable; {}{})",
    report.total_matches,
    report.matched_files,
    report.candidate_files,
    report.pruned_files,
    report.text_index,
    report.prefiltered_files,
    report.scanned_files,
    report.stale_files,
    report.unreadable_files,
    report.index,
    report
      .index_reason
      .as_deref()
      .map(|r| format!(": {r}"))
      .unwrap_or_default()
  );
  for record in &report.records {
    match &record.symbol {
      Some(symbol) => {
        let _ = writeln!(out, "{}:{}:{}  {}    [{}]", record.path, record.line, record.column, record.text, symbol);
      }
      None => {
        let _ = writeln!(out, "{}:{}:{}  {}", record.path, record.line, record.column, record.text);
      }
    }
  }
  if report.truncated {
    let _ = writeln!(out, "(truncated at {} lines)", report.records.len());
  }
  out
}

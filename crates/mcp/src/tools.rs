//! Structural-engine MCP tools (IMPROVEMENTS §8): the matcher half of vorpal, served next
//! to the graph half — `structural_search` runs an ast-grep pattern over the watched tree,
//! `fetch_span` returns a graph node's defining source verbatim.

use std::path::{Path, PathBuf};

use vorpal_core::tree_sitter::LanguageExt as _;
use vorpal_core::{Language as _, Pattern};
use vorpal_language::SupportLang;

/// Run `pattern` (parsed under `lang`) over every matching-language file beneath `root`,
/// honoring ignore files, and render up to `limit` matches as `path:line  matched-text`
/// lines. Deterministic: files are visited in sorted path order.
pub fn structural_search(
  root: &Path,
  pattern: &str,
  lang: &str,
  path_suffix: Option<&str>,
  limit: usize,
) -> Result<String, String> {
  let lang: SupportLang = lang
    .parse()
    .map_err(|_| format!("unknown language '{lang}'"))?;
  let pattern = Pattern::try_new(pattern, lang).map_err(|err| format!("bad pattern: {err}"))?;

  let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
    .build()
    .flatten()
    .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
    .map(|entry| entry.into_path())
    .filter(|path| SupportLang::from_path(path) == Some(lang))
    .filter(|path| path_suffix.is_none_or(|suffix| path.to_string_lossy().ends_with(suffix)))
    .collect();
  files.sort();

  let mut out = String::new();
  let mut hits = 0usize;
  'files: for path in files {
    let Ok(source) = std::fs::read_to_string(&path) else {
      continue;
    };
    let grep = lang.grep(&source);
    for found in grep.root().find_all(&pattern) {
      let line = found.start_pos().line() + 1;
      let text = found.text();
      let snippet: String = text
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
      out.push_str(&format!("{}:{line}  {snippet}\n", path.display()));
      hits += 1;
      if hits >= limit {
        out.push_str(&format!("(truncated at {limit} matches)\n"));
        break 'files;
      }
    }
  }
  if hits == 0 {
    return Ok("(no matches)".to_string());
  }
  Ok(out)
}

/// Return the defining source of node `id`: the persisted byte span sliced from the file,
/// clamped to `max_bytes`. Paths are stored as walked at index time, so they resolve
/// relative to the daemon's working directory exactly as the indexer saw them.
pub fn fetch_span(kg: &vorpal_kg::Kg, id: u64, max_bytes: usize) -> Result<String, String> {
  let view = kg
    .node(vorpal_kg::NodeId::new(id))
    .ok_or_else(|| format!("no node with id {id}"))?;
  let (start, end) = view.span;
  if end <= start {
    return Err(format!(
      "node {id} ({}) carries no source span (File node, or an index built before spans were persisted — re-run the 'index' tool)",
      view.name
    ));
  }
  let bytes = std::fs::read(view.path).map_err(|err| format!("read {}: {err}", view.path))?;
  let end = (end as usize).min(bytes.len());
  let start = (start as usize).min(end);
  let clamped = end.min(start + max_bytes);
  let body = String::from_utf8_lossy(&bytes[start..clamped]);
  let line = bytes[..start].iter().filter(|&&b| b == b'\n').count() + 1;
  let mut out = format!("{}:{line}  {} [{:?}]\n", view.path, view.name, view.kind);
  out.push_str(&body);
  if clamped < end {
    out.push_str(&format!(
      "\n(truncated: {} of {} bytes)",
      clamped - start,
      end - start
    ));
  }
  Ok(out)
}

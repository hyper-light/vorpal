//! Structural-engine MCP tools (IMPROVEMENTS §8): the matcher half of vorpal, served next
//! to the graph half — `structural_search` runs an ast-grep pattern over the watched tree,
//! `rule_search` runs the **full YAML rule model** (rule/constraints/utils/transform/fix)
//! with dry-run fix rendering, `ast_dump` prints a parse tree for rule authoring, and
//! `fetch_span` returns a graph node's defining source, digest-verified.

use std::path::{Path, PathBuf};

use vorpal_core::tree_sitter::LanguageExt as _;
use vorpal_core::{Language as _, Matcher as _, Pattern};
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

/// Run one or more full YAML rules (`---`-separated) over the watched tree: the complete
/// rule model — composite rules, relational rules, `constraints`, `utils`, `transform` —
/// not just a bare pattern. When a rule carries `fix`, each match renders its **dry-run**
/// replacement (nothing on disk changes). Deterministic: sorted path order, document order
/// within a file.
pub fn rule_search(
  root: &Path,
  rule_yaml: &str,
  path_suffix: Option<&str>,
  limit: usize,
) -> Result<String, String> {
  let configs = vorpal_config::from_yaml_string::<SupportLang>(rule_yaml, &Default::default())
    .map_err(|err| format!("bad rule: {err}"))?;
  if configs.is_empty() {
    return Err("no rule documents in input".to_string());
  }

  let mut out = String::new();
  let mut hits = 0usize;
  for config in &configs {
    let lang = config.language;
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
      .build()
      .flatten()
      .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
      .map(|entry| entry.into_path())
      .filter(|path| SupportLang::from_path(path) == Some(lang))
      .filter(|path| path_suffix.is_none_or(|suffix| path.to_string_lossy().ends_with(suffix)))
      .collect();
    files.sort();

    'files: for path in files {
      let Ok(source) = std::fs::read_to_string(&path) else {
        continue;
      };
      let grep = lang.grep(&source);
      for found in grep.root().find_all(&config.matcher) {
        let line = found.start_pos().line() + 1;
        let snippet: String = found
          .text()
          .lines()
          .next()
          .unwrap_or("")
          .chars()
          .take(200)
          .collect();
        out.push_str(&format!(
          "[{}] {}:{line}  {snippet}\n",
          config.id,
          path.display()
        ));
        for fixer in &config.fixer {
          let edit = found.replace_by(fixer);
          let inserted: String = String::from_utf8_lossy(&edit.inserted_text)
            .chars()
            .take(200)
            .collect();
          out.push_str(&format!("  fix (dry-run) → {inserted}\n"));
        }
        hits += 1;
        if hits >= limit {
          out.push_str(&format!("(truncated at {limit} matches)\n"));
          break 'files;
        }
      }
    }
  }
  if hits == 0 {
    return Ok("(no matches)".to_string());
  }
  Ok(out)
}

/// Parse `source` under `lang` and print the named-node tree — kind, byte span, and leaf
/// text — the ground truth a rule author needs to pick `kind`/field targets. Capped at
/// `max_nodes` nodes with an explicit truncation note.
pub fn ast_dump(source: &str, lang: &str, max_nodes: usize) -> Result<String, String> {
  let lang: SupportLang = lang
    .parse()
    .map_err(|_| format!("unknown language '{lang}'"))?;
  let grep = lang.grep(source);
  let root = grep.root();
  let mut out = String::new();
  let mut printed = 0usize;
  // (node, depth) DFS; children pushed in reverse for document order.
  let mut stack = vec![(root, 0usize)];
  while let Some((node, depth)) = stack.pop() {
    if node.is_named() {
      if printed >= max_nodes {
        out.push_str(&format!("(truncated at {max_nodes} nodes)\n"));
        break;
      }
      let range = node.range();
      let leaf = node.children().all(|c| !c.is_named());
      let text: String = if leaf {
        let t: String = node.text().chars().take(60).collect();
        format!("  {t:?}")
      } else {
        String::new()
      };
      out.push_str(&format!(
        "{}{} [{}..{}]{}\n",
        "  ".repeat(depth),
        node.kind(),
        range.start,
        range.end,
        text
      ));
      printed += 1;
      let children: Vec<_> = node.children().collect();
      for child in children.into_iter().rev() {
        stack.push((child, depth + 1));
      }
    } else {
      let children: Vec<_> = node.children().collect();
      for child in children.into_iter().rev() {
        stack.push((child, depth));
      }
    }
  }
  if printed == 0 {
    return Ok("(no named nodes — is the source empty?)".to_string());
  }
  Ok(out)
}

/// Return the defining source of node `id`: the persisted byte span sliced from the file,
/// clamped to `max_bytes`. Paths are stored as walked at index time, so they resolve
/// relative to the daemon's working directory exactly as the indexer saw them.
///
/// The read is **digest-verified against the pinned generation** (IMPROVEMENTS #7): when the
/// file's current bytes no longer match what this generation indexed, the persisted offsets
/// are stale and slicing them would return bytes inconsistent with the node — the tool
/// refuses instead of guessing. Generations without pack digests slice the current file,
/// labeled as unverified.
pub fn fetch_span(
  kg: &vorpal_kg::Kg,
  artifacts_dir: Option<&Path>,
  id: u64,
  max_bytes: usize,
) -> Result<String, String> {
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
  let (bytes, verdict) = match vorpal_index::read_indexed_source(artifacts_dir, view.path)? {
    vorpal_index::IndexedRead::Verified(bytes) => (bytes, "source verified"),
    vorpal_index::IndexedRead::Unverified(bytes) => (bytes, "current file contents — unverified"),
    vorpal_index::IndexedRead::Changed => {
      return Err(format!(
        "{} changed since this generation indexed it — span offsets are stale; re-run the 'index' tool",
        view.path
      ));
    }
  };
  let end = (end as usize).min(bytes.len());
  let start = (start as usize).min(end);
  let clamped = end.min(start + max_bytes);
  let body = String::from_utf8_lossy(&bytes[start..clamped]);
  let line = bytes[..start].iter().filter(|&&b| b == b'\n').count() + 1;
  let mut out = format!(
    "{}:{line}  {} [{:?}] ({verdict})\n",
    view.path, view.name, view.kind
  );
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

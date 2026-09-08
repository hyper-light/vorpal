//! Structural-engine MCP tools (IMPROVEMENTS §8): the matcher half of vorpal, served next
//! to the graph half — `structural_search` runs an ast-grep pattern over the watched tree,
//! `rule_search` runs the **full YAML rule model** (rule/constraints/utils/transform/fix)
//! with dry-run fix rendering, `ast_dump` prints a parse tree for rule authoring, and
//! `fetch_span` returns a graph node's defining source, digest-verified.
//!
//! Both searches share one candidate path with `code_search`: the text tier names the files
//! that can hold every required literal (a file it does not know, or whose bucket is stale,
//! stays a candidate), the literal prefilter skips the parse for files it proves matchless,
//! and only then does tree-sitter run. The walk is parallel; results are sorted by
//! `(path, line, column)` so pages are deterministic whatever the thread count.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use vorpal_core::matcher::{Prefilter, PatternSpec};
use vorpal_core::tree_sitter::LanguageExt as _;
use vorpal_core::{Language as _, Pattern};
use vorpal_ingest::SgLang;

/// The committed generation's text tier, when the watched tree is the one it indexed.
pub struct TextTier {
  pub pack: std::sync::Arc<vorpal_ingest::PackReader>,
  pub index: std::sync::Arc<vorpal_index::trigrams::TextIndex>,
}

/// One structural match.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StructuralHit {
  pub path: String,
  pub line: u32,
  pub column: u32,
  pub kind: String,
  pub text: String,
}

/// One rule match, with its dry-run fixes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RuleHit {
  pub rule: String,
  pub path: String,
  pub line: u32,
  pub column: u32,
  pub text: String,
  pub fixes: Vec<String>,
}

/// What a walk did, for the report's honesty margins.
#[derive(Serialize, Debug, Clone, Copy, Default)]
pub struct WalkStats {
  pub candidate_files: u64,
  pub pruned_files: u64,
  pub prefiltered_files: u64,
  pub scanned_files: u64,
  /// Files parsed chunk-scoped (see `vorpal_index::chunks`).
  pub chunk_parsed_files: u64,
  /// Files answered from the product's call references (see `vorpal_index::callsite`).
  pub callsite_files: u64,
}

/// Read `path` into `buf` (cleared first), reserving from the file's size so one read fills it.
pub(crate) fn read_into(path: &Path, buf: &mut Vec<u8>) -> std::io::Result<()> {
  use std::io::Read;
  buf.clear();
  let mut file = std::fs::File::open(path)?;
  if let Ok(meta) = file.metadata() {
    buf.reserve(meta.len() as usize);
  }
  file.read_to_end(buf)?;
  Ok(())
}

fn first_line(text: &str) -> String {
  text.lines().next().unwrap_or("").chars().take(200).collect()
}

type WalkKey = (PathBuf, SgLang, Option<String>);
type WalkCache = Mutex<Vec<(WalkKey, u64, std::sync::Arc<Vec<PathBuf>>)>>;
static WALKS: std::sync::OnceLock<WalkCache> = std::sync::OnceLock::new();
const WALK_CACHE_CAP: usize = 16;

/// [`files_of`], remembered per `(root, lang, suffix)` at `epoch` — the daemon's watcher
/// event count at the time of the call, so any event after the walk (a level that only
/// rises) misses the cache and walks again; `None` (no watcher) never caches. The kernel
/// walk is 94k directory entries and ~500k allocations per call — the whole floor of a
/// `structural_search` once the matching itself is milliseconds. The list is shared
/// (`Arc`) because the scan runs on rayon workers while the cache keeps its copy: genuine
/// cross-thread sharing of one immutable list.
fn files_of_cached(root: &Path, lang: SgLang, path_suffix: Option<&str>, epoch: Option<u64>) -> std::sync::Arc<Vec<PathBuf>> {
  let Some(epoch) = epoch else {
    return std::sync::Arc::new(files_of(root, lang, path_suffix));
  };
  let key: WalkKey = (root.to_path_buf(), lang, path_suffix.map(str::to_string));
  let cache_ref = WALKS.get_or_init(|| Mutex::new(Vec::new()));
  {
    let guard = cache_ref.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((_, _, files)) = guard.iter().find(|(k, e, _)| *k == key && *e == epoch) {
      vorpal_kg::phase_stamp("structural: walk cache hit");
      return files.clone();
    }
  }
  vorpal_kg::phase_stamp("structural: walk cache miss, walking");
  let files = std::sync::Arc::new(files_of(root, lang, path_suffix));
  vorpal_kg::phase_stamp("structural: walk done");
  let mut guard = cache_ref.lock().unwrap_or_else(|p| p.into_inner());
  guard.retain(|(_, e, _)| *e == epoch);
  guard.push((key, epoch, files.clone()));
  if guard.len() > WALK_CACHE_CAP {
    guard.remove(0);
  }
  files
}

/// Every file of `lang` beneath `root` (ignore files honored), path-sorted.
fn files_of(root: &Path, lang: SgLang, path_suffix: Option<&str>) -> Vec<PathBuf> {
  let found: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
  let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
  ignore::WalkBuilder::new(root)
    .threads(threads)
    .build_parallel()
    .run(|| {
      Box::new(|entry| {
        if let Ok(entry) = entry
          && entry.file_type().is_some_and(|t| t.is_file())
        {
          let path = entry.into_path();
          if SgLang::from_path(&path) == Some(lang)
            && path_suffix.is_none_or(|suffix| path.to_string_lossy().ends_with(suffix))
          {
            found.lock().unwrap_or_else(|p| p.into_inner()).push(path);
          }
        }
        ignore::WalkState::Continue
      })
    });
  let mut files = found.into_inner().unwrap_or_else(|p| p.into_inner());
  files.sort();
  files
}

/// Run `matcher` over `files` in parallel with the shared candidate path, collecting
/// `(path, matches)`; `hit` renders one match.
/// The record builder for a span (`path, source, start, end, node kind`): what the
/// reference shortcut and the chunk memo build records from, since neither has a match node.
pub type SpanBuilder<'a, T> = &'a (dyn Fn(&Path, &str, usize, usize, &'static str) -> T + Sync);

/// The parse-free answers a scan may use: the reference shortcut for a call shape (see
/// `vorpal_index::callsite`) and the chunk memo (see `vorpal_index::chunks`) keyed by
/// `memo_key`, both building records through `build`. Only for matchers whose records need
/// nothing but the span and kind — a rule with a `fix` needs the match environment and
/// passes `None`.
pub struct SpanShortcut<'a, T> {
  pub build: SpanBuilder<'a, T>,
  pub call_shape: Option<&'a vorpal_index::callsite::CallShape>,
  pub memo_key: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
fn scan_files<M, T, F>(
  files: &[PathBuf],
  lang: SgLang,
  matcher: &M,
  prefilter: &Prefilter,
  tier: Option<&TextTier>,
  candidates: Option<&vorpal_kg::trigramstore::CandidateSet>,
  hit: F,
  shortcut: Option<SpanShortcut<'_, T>>,
) -> (Vec<T>, WalkStats)
where
  M: vorpal_core::Matcher + Sync,
  T: Send,
  F: Fn(&Path, vorpal_core::NodeMatch<'_, vorpal_core::tree_sitter::StrDoc<SgLang>>) -> T + Sync,
{
  use rayon::prelude::*;
  use vorpal_kg::trigramstore::Verdict;
  enum Outcome<T> {
    Pruned,
    Prefiltered,
    /// Records, chunk-scoped flag, answered-from-call-references flag.
    Scanned(Vec<T>, bool, bool),
    Unreadable,
  }
  let per_file: Vec<Outcome<T>> = files
    .par_iter()
    .map(|path| {
      let spelled = path.to_string_lossy();
      if let (Some(tier), Some(set)) = (tier, candidates) {
        let key = vorpal_kg::identity::FileKey::of(tier.pack.stored_key(&spelled)).0;
        if tier.index.verdict(set, key) == Verdict::Pruned {
          return Outcome::Pruned;
        }
      }
      let mut bytes = vorpal_index::trigrams::take_read_buffer();
      if crate::tools::read_into(path, &mut bytes).is_err() {
        vorpal_index::trigrams::give_read_buffer(bytes);
        return Outcome::Unreadable;
      }
      if !prefilter.may_match_bytes(&bytes) {
        vorpal_index::trigrams::give_read_buffer(bytes);
        return Outcome::Prefiltered;
      }
      // A call shape reads its matches off the product's call references (the same
      // whole-file parse the reference arm runs) — verified bytes only, no parse.
      let verified = tier.and_then(|tier| {
        let product = tier.pack.get(&spelled)?;
        (vorpal_ingest::peek_product_digest(product) == Some(xxhash_rust::xxh3::xxh3_64(&bytes))).then_some(product)
      });
      if let (Some(SpanShortcut { build, call_shape: Some(shape), .. }), Some(product)) = (&shortcut, verified)
        && let Some(rows) = vorpal_ingest::peek_product_refs(product)
      {
        let mut opaque = false;
        let mut spans: Vec<(u32, u32)> = Vec::new();
        for row in rows {
          if row.kind != vorpal_ingest::CALL_REF_TAG || row.name != shape.callee {
            continue;
          }
          if vorpal_index::callsite::shape_has_error(row.call_shape) {
            opaque = true;
            break;
          }
          if row.form == vorpal_ingest::BARE_FORM_TAG && !row.has_qualifier && shape.admits(row.call_shape) {
            spans.push((row.start, row.end));
          }
        }
        if !opaque {
          let Ok(source) = String::from_utf8(bytes) else {
            return Outcome::Unreadable;
          };
          let found: Vec<T> = spans
            .iter()
            .map(|&(start, end)| build(path, &source, start as usize, end as usize, shape.kind))
            .collect();
          vorpal_index::trigrams::give_read_buffer(source.into_bytes());
          return Outcome::Scanned(found, false, true);
        }
      }
      // Chunk-scoped parse when the bytes are exactly the indexed bytes (digest match):
      // only the top-level statements holding the anchor literal are parsed, one parse
      // per chunk (see `vorpal_index::chunks`).
      let mut chunk_scratch = vorpal_index::chunks::take_scratch();
      let chunked = verified.is_some_and(|product| {
        let Some(cuts) = vorpal_ingest::peek_product_cuts(product) else {
          return false;
        };
        if prefilter.anchor_positions(&bytes, &mut chunk_scratch.positions).is_none() {
          return false;
        }
        vorpal_index::chunks::chunks_with_anchors(&bytes, cuts, &mut chunk_scratch)
      });
      let Ok(mut source) = String::from_utf8(bytes) else {
        vorpal_index::chunks::give_scratch(chunk_scratch);
        return Outcome::Unreadable;
      };
      let mut found: Vec<T> = Vec::new();
      if !chunked {
        // Whole-file parse, memoized as one chunk keyed by the file's bytes for matchers
        // whose records need only the span (see `SpanShortcut`): a repeat replays it.
        let memo = shortcut.as_ref().and_then(|sc| sc.memo_key.map(|key| (key, sc.build)));
        let file_key = (memo.is_some() && verified.is_some()).then(|| vorpal_index::chunks::chunk_key(source.as_bytes()));
        let ts_lang = lang.get_ts_language();
        let mut triples: Vec<u32> = Vec::new();
        if let (Some((memo_key, build)), Some(file_key)) = (memo, file_key)
          && vorpal_index::chunks::memo_get(memo_key, file_key, &mut triples)
        {
          for t in triples.chunks_exact(3) {
            let kind = ts_lang.node_kind_for_id(t[2] as u16).unwrap_or("");
            found.push(build(path, &source, t[0] as usize, t[1] as usize, kind));
          }
        } else {
          let grep = lang.grep(&source);
          for m in grep.root().find_all(matcher) {
            let r = m.range();
            triples.extend([r.start as u32, r.end as u32, u32::from(m.kind_id())]);
            found.push(hit(path, m));
          }
          if let (Some((memo_key, _)), Some(file_key)) = (memo, file_key) {
            vorpal_index::chunks::memo_put(memo_key, file_key, &triples);
          }
        }
        vorpal_index::trigrams::give_read_buffer(source.into_bytes());
        vorpal_index::chunks::give_scratch(chunk_scratch);
        return Outcome::Scanned(found, false, false);
      }
      // The chunk memo (start, end, kind id) per verified chunk, for matchers whose records
      // need only the span: a memo hit builds records with no parse.
      let memo = shortcut.as_ref().and_then(|sc| sc.memo_key.map(|key| (key, sc.build)));
      let ts_lang = lang.get_ts_language();
      let mut points = vorpal_index::chunks::PointCursor::default();
      let mut triples: Vec<u32> = Vec::new();
      for i in 0..chunk_scratch.chunks.len() {
        let (start, end) = chunk_scratch.chunks[i];
        let chunk_key = memo.map(|_| vorpal_index::chunks::chunk_key(&source.as_bytes()[start..end]));
        if let (Some((memo_key, build)), Some(chunk_key)) = (memo, chunk_key) {
          triples.clear();
          if vorpal_index::chunks::memo_get(memo_key, chunk_key, &mut triples) {
            for t in triples.chunks_exact(3) {
              let kind = ts_lang.node_kind_for_id(t[2] as u16).unwrap_or("");
              found.push(build(path, &source, start + t[0] as usize, start + t[1] as usize, kind));
            }
            continue;
          }
        }
        let range = points.range(source.as_bytes(), start, end);
        let Ok(tree) = vorpal_core::tree_sitter::parse_ranges(&source, &lang, &[range]) else {
          found.clear();
          let grep = lang.grep(&source);
          found.extend(grep.root().find_all(matcher).map(|m| hit(path, m)));
          vorpal_index::trigrams::give_read_buffer(source.into_bytes());
          vorpal_index::chunks::give_scratch(chunk_scratch);
          return Outcome::Scanned(found, false, false);
        };
        let root = vorpal_core::Vorpal::doc(vorpal_core::tree_sitter::StrDoc::from_parts(source, lang, tree));
        triples.clear();
        for m in root.root().find_all(matcher) {
          let r = m.range();
          if !(start <= r.start && r.start < end) {
            continue;
          }
          triples.extend([(r.start - start) as u32, (r.end - start) as u32, u32::from(m.kind_id())]);
          found.push(hit(path, m));
        }
        source = root.into_doc().src;
        if let (Some((memo_key, _)), Some(chunk_key)) = (memo, chunk_key) {
          vorpal_index::chunks::memo_put(memo_key, chunk_key, &triples);
        }
      }
      vorpal_index::trigrams::give_read_buffer(source.into_bytes());
      vorpal_index::chunks::give_scratch(chunk_scratch);
      Outcome::Scanned(found, true, false)
    })
    .collect();
  let mut stats = WalkStats {
    candidate_files: files.len() as u64,
    ..WalkStats::default()
  };
  let mut hits = Vec::new();
  for outcome in per_file {
    match outcome {
      Outcome::Pruned => stats.pruned_files += 1,
      Outcome::Prefiltered => stats.prefiltered_files += 1,
      Outcome::Unreadable => {}
      Outcome::Scanned(found, chunked, callsite) => {
        stats.scanned_files += 1;
        stats.chunk_parsed_files += u64::from(chunked);
        stats.callsite_files += u64::from(callsite);
        hits.extend(found);
      }
    }
  }
  (hits, stats)
}

/// Build the pattern for a search tool: an explicit `selector`/`context` when given, else the
/// smart constructor (C-family call shapes re-parse in statement context).
pub fn search_pattern(spec: &PatternSpec<'_>, lang: SgLang) -> Result<Pattern, String> {
  match spec.selector {
    Some(selector) => Pattern::contextual(spec.context.unwrap_or(spec.pattern), selector, lang),
    None => Pattern::try_new_smart(spec.pattern, lang),
  }
  .map_err(|err| format!("bad pattern: {err}"))
}

/// Run `pattern` (parsed under `lang`) over every matching-language file beneath `root`,
/// honoring ignore files. Deterministic: hits sorted by `(path, line, column)`.
pub fn structural_search(
  root: &Path,
  spec: &PatternSpec<'_>,
  lang: &str,
  path_suffix: Option<&str>,
  tier: Option<&TextTier>,
  walk_epoch: Option<u64>,
) -> Result<(Vec<StructuralHit>, WalkStats), String> {
  let lang: SgLang = lang
    .parse()
    .map_err(|_| format!("unknown language '{lang}'"))?;
  let pattern = search_pattern(spec, lang)?;
  let prefilter = Prefilter::for_pattern(&pattern);
  let candidates = tier.and_then(|t| t.index.candidates_for_literals(&pattern.required_literals()));
  let files = files_of_cached(root, lang, path_suffix, walk_epoch);
  vorpal_kg::phase_stamp("structural: files listed");
  let shape = vorpal_index::callsite::shape_of(spec, lang);
  // A record from a call reference's span: line and column counted from the bytes (the
  // column is in characters, as `start_pos().column` counts), text from the span.
  let from_span = |path: &Path, source: &str, start: usize, end: usize, kind: &'static str| -> StructuralHit {
    let bytes = source.as_bytes();
    let line = bytes[..start].iter().filter(|&&b| b == b'\n').count() as u32 + 1;
    let line_start = bytes[..start].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
    let column = source[line_start..start].chars().count() as u32 + 1;
    StructuralHit {
      path: path.display().to_string(),
      line,
      column,
      kind: kind.to_string(),
      text: first_line(&source[start..end.min(source.len())]),
    }
  };
  let (mut hits, stats) = scan_files(
    &files,
    lang,
    &pattern,
    &prefilter,
    tier,
    candidates.as_ref(),
    |path, found| StructuralHit {
      path: path.display().to_string(),
      line: found.start_pos().line() as u32 + 1,
      column: found.start_pos().column(&found) as u32 + 1,
      kind: found.kind().to_string(),
      text: first_line(&found.text()),
    },
    Some(SpanShortcut {
      build: &from_span,
      call_shape: shape.as_ref(),
      // A different salt from `code_search`'s starts-only memo of the same pattern.
      memo_key: Some(vorpal_index::chunks::pattern_key(&format!("structural:{}", lang), spec.pattern, spec.selector, spec.context)),
    }),
  );
  vorpal_kg::phase_stamp("structural: scan done");
  hits.sort();
  Ok((hits, stats))
}

/// Run one or more full YAML rules (`---`-separated) over the watched tree: the complete
/// rule model — composite rules, relational rules, `constraints`, `utils`, `transform` —
/// not just a bare pattern. When a rule carries `fix`, each match renders its **dry-run**
/// replacement (nothing on disk changes). Deterministic: sorted `(path, line, column)` within
/// each rule, rules in document order.
pub fn rule_search(
  root: &Path,
  rule_yaml: &str,
  path_suffix: Option<&str>,
  tier: Option<&TextTier>,
  walk_epoch: Option<u64>,
) -> Result<(Vec<RuleHit>, WalkStats), String> {
  let configs = vorpal_config::from_yaml_string::<SgLang>(rule_yaml, &Default::default())
    .map_err(|err| format!("bad rule: {err}"))?;
  if configs.is_empty() {
    return Err("no rule documents in input".to_string());
  }
  // A rule that is exactly `rule: {pattern: P}` — no fix, constraints, utils, transform,
  // rewriters — has records that need only the span: it takes the reference shortcut and
  // the chunk memo like `structural_search`. Read off the YAML beside the compiled configs
  // (document order is the same).
  let bare_patterns: Vec<Option<String>> = serde_yaml::Deserializer::from_str(rule_yaml)
    .map(|doc| {
      let v = serde_yaml::Value::deserialize(doc).ok()?;
      let m = v.as_mapping()?;
      let key = |k: &str| serde_yaml::Value::String(k.to_string());
      for forbidden in ["fix", "constraints", "utils", "transform", "rewriters"] {
        if m.contains_key(key(forbidden)) {
          return None;
        }
      }
      let rule = m.get(key("rule"))?.as_mapping()?;
      if rule.len() != 1 {
        return None;
      }
      rule.get(key("pattern"))?.as_str().map(str::to_string)
    })
    .collect();
  let mut all: Vec<RuleHit> = Vec::new();
  let mut total = WalkStats::default();
  for (config, bare) in configs.iter().zip(bare_patterns.iter().chain(std::iter::repeat(&None))) {
    let lang = config.language;
    let literals = config.matcher.required_literals();
    let prefilter = Prefilter::from_literals(literals.clone());
    let candidates = tier.and_then(|t| t.index.candidates_for_literals(&literals));
    let files = files_of_cached(root, lang, path_suffix, walk_epoch);
    let bare_spec = bare.as_deref().map(PatternSpec::plain);
    let shape = bare_spec.as_ref().and_then(|spec| vorpal_index::callsite::shape_of(spec, lang));
    let rule_id = config.id.clone();
    let from_span = |path: &Path, source: &str, start: usize, end: usize, _kind: &'static str| -> RuleHit {
      let bytes = source.as_bytes();
      let line = bytes[..start].iter().filter(|&&b| b == b'\n').count() as u32 + 1;
      let line_start = bytes[..start].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
      RuleHit {
        rule: rule_id.clone(),
        path: path.display().to_string(),
        line,
        column: source[line_start..start].chars().count() as u32 + 1,
        text: first_line(&source[start..end.min(source.len())]),
        fixes: Vec::new(),
      }
    };
    let shortcut = bare_spec.as_ref().map(|spec| SpanShortcut {
      build: &from_span as SpanBuilder<'_, RuleHit>,
      call_shape: shape.as_ref(),
      memo_key: Some(vorpal_index::chunks::pattern_key(&format!("rule:{}", lang), spec.pattern, None, None)),
    });
    let (mut hits, stats) = scan_files(&files, lang, &config.matcher, &prefilter, tier, candidates.as_ref(), |path, found| {
      let fixes = config
        .fixer
        .iter()
        .map(|fixer| {
          let edit = found.replace_by(fixer);
          String::from_utf8_lossy(&edit.inserted_text).chars().take(200).collect()
        })
        .collect();
      RuleHit {
        rule: config.id.clone(),
        path: path.display().to_string(),
        line: found.start_pos().line() as u32 + 1,
        column: found.start_pos().column(&found) as u32 + 1,
        text: first_line(&found.text()),
        fixes,
      }
    }, shortcut);
    hits.sort();
    all.extend(hits);
    total.candidate_files += stats.candidate_files;
    total.pruned_files += stats.pruned_files;
    total.prefiltered_files += stats.prefiltered_files;
    total.chunk_parsed_files += stats.chunk_parsed_files;
    total.callsite_files += stats.callsite_files;
    total.scanned_files += stats.scanned_files;
  }
  Ok((all, total))
}

/// The text block for a page of structural hits.
pub fn render_structural(hits: &[StructuralHit], stats: &WalkStats, total: usize) -> String {
  let mut out = format!(
    "{total} matches ({} files: {} pruned by the text index, {} prefiltered, {} parsed, {} chunk-scoped)\n",
    stats.candidate_files, stats.pruned_files, stats.prefiltered_files, stats.scanned_files, stats.chunk_parsed_files
  );
  for hit in hits {
    out.push_str(&format!("{}:{}:{}  {}\n", hit.path, hit.line, hit.column, hit.text));
  }
  if total == 0 {
    out.push_str("(no matches)\n");
  }
  out
}

/// The text block for a page of rule hits.
pub fn render_rules(hits: &[RuleHit], stats: &WalkStats, total: usize) -> String {
  let mut out = format!(
    "{total} matches ({} files: {} pruned by the text index, {} prefiltered, {} parsed, {} chunk-scoped)\n",
    stats.candidate_files, stats.pruned_files, stats.prefiltered_files, stats.scanned_files, stats.chunk_parsed_files
  );
  for hit in hits {
    out.push_str(&format!("[{}] {}:{}:{}  {}\n", hit.rule, hit.path, hit.line, hit.column, hit.text));
    for fix in &hit.fixes {
      out.push_str(&format!("  fix (dry-run) → {fix}\n"));
    }
  }
  if total == 0 {
    out.push_str("(no matches)\n");
  }
  out
}

/// Parse `source` under `lang` and print the named-node tree — kind, byte span, and leaf
/// text — the ground truth a rule author needs to pick `kind`/field targets. Capped at
/// `max_nodes` nodes with an explicit truncation note.
pub fn ast_dump(source: &str, lang: &str, max_nodes: usize) -> Result<String, String> {
  let lang: SgLang = lang
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
/// How a [`fetch_span`] request failed — staleness is structurally distinguished so the MCP
/// envelope can carry its stable error code without string matching.
pub enum FetchSpanError {
  /// The file changed since the pinned generation indexed it (`stale-source`).
  Stale(String),
  /// Anything else (missing node, no span, unreadable file).
  Other(String),
}

pub fn fetch_span(
  kg: &vorpal_kg::Kg,
  artifacts_dir: Option<&Path>,
  id: u64,
  max_bytes: usize,
) -> Result<String, FetchSpanError> {
  let view = kg
    .node(vorpal_kg::NodeId::new(id))
    .ok_or_else(|| FetchSpanError::Other(format!("no node with id {id}")))?;
  let (start, end) = view.span;
  if end <= start {
    return Err(FetchSpanError::Other(format!(
      "node {id} ({}) carries no source span (File node, or an index built before spans were persisted — re-run the 'index' tool)",
      view.name
    )));
  }
  let read =
    vorpal_index::read_indexed_source(artifacts_dir, view.path).map_err(FetchSpanError::Other)?;
  let (bytes, verdict) = match read {
    vorpal_index::IndexedRead::Verified(bytes) => (bytes, "source verified"),
    vorpal_index::IndexedRead::Unverified(bytes) => (bytes, "current file contents — unverified"),
    vorpal_index::IndexedRead::Changed => {
      return Err(FetchSpanError::Stale(format!(
        "{} changed since this generation indexed it — span offsets are stale; re-run the 'index' tool",
        view.path
      )));
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

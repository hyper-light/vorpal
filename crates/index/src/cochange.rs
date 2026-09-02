//! Git co-change (temporal coupling) edges — ADOPTION #27's second half. Files that keep
//! changing together in recent history are coupled whether or not any static edge says so;
//! the pass reads `git log --name-only` over the last N non-merge commits, counts file
//! pairs, and emits symmetric `CHANGES_WITH` edges between File nodes at seal.
//!
//! Bounded and honest by construction: N commits (default 2,000; `VORPAL_COCHANGE_COMMITS`
//! overrides, `0` disables), commits touching more than 50 files — raw, before filtering to
//! the indexed set — are skipped (bulk moves and reformat sweeps couple nothing), a pair
//! needs at least two co-changes, every file keeps at most its 8 strongest partners, and
//! the distinct-pair table has a ceiling past which the pass declines whole rather than
//! answer from a partial count. Paths are interned to `u32` ids for the count, so the
//! table costs 12 bytes a pair, never two strings. Not a git repository, no `git`, or no
//! history: zero edges and a stated reason — never a silent nothing.
//!
//! Contract note: with this pass on, a generation's content id folds the repository's
//! recent HISTORY, not only its tree — a shallow clone seals a different id than a full one.
//! docs/INDEX_FORMAT.md states the policy; the switch is the environment variable above.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

/// Default history window.
pub const DEFAULT_COMMITS: usize = 2_000;
/// Commits touching more files than this (before indexed-set filtering) are skipped.
const MAX_FILES_PER_COMMIT: usize = 50;
/// Minimum co-changes for a pair to become an edge.
const MIN_COUNT: u32 = 2;
/// Strongest partners kept per file.
const MAX_PARTNERS: usize = 8;
/// Distinct pairs the count table may hold (~48 MB at the 12-byte entry) before the pass
/// declines — stated on the report, never a truncated table.
const MAX_PAIRS: usize = 4_000_000;

/// One co-change edge: manifest paths of both files plus the packed confidence.
pub struct CochangeEdge {
  pub a: String,
  pub b: String,
  pub confidence: u8,
}

/// The pass outcome: edges (symmetric pairs listed once, `a < b`) and a human note when
/// nothing could be computed.
pub struct Cochange {
  pub edges: Vec<CochangeEdge>,
  pub commits_read: usize,
  pub note: Option<String>,
}

fn skipped(note: String) -> Cochange {
  Cochange {
    edges: Vec::new(),
    commits_read: 0,
    note: Some(note),
  }
}

fn commits_from_env() -> usize {
  std::env::var("VORPAL_COCHANGE_COMMITS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(DEFAULT_COMMITS)
}

fn git_output(src: &Path, args: &[&str]) -> Result<String, String> {
  let out = Command::new("git")
    .arg("-C")
    .arg(src)
    .args(args)
    .output()
    .map_err(|err| format!("git unavailable: {err}"))?;
  if !out.status.success() {
    return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
  }
  Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The pass in flight: either a cache hit already holding the edges, an early refusal, or
/// a `git log` child running CONCURRENTLY with extraction (it is I/O and single-threaded
/// tree diffing — a serial 1.1 s at kernel scale when run after the stream, ~free beside it).
pub struct Pending {
  commits: usize,
  head: String,
  prefix: String,
  cache_path: std::path::PathBuf,
  state: PendingState,
}

enum PendingState {
  Skipped(String),
  /// Raw counted pairs (git-relative paths) from the HEAD-keyed cache.
  Cached(Vec<RawPair>),
  /// The child plus the thread draining its stdout — without the drain, git would block on
  /// a full pipe after 64 KB and do its diffing only when joined, defeating the overlap.
  Running(std::process::Child, std::thread::JoinHandle<std::io::Result<String>>),
}

/// A co-changed pair as git reports it: toplevel-relative paths and the count. Manifest-
/// independent, which is what makes it cacheable across builds that index different sets.
struct RawPair {
  a: String,
  b: String,
  count: u32,
}

const CACHE_MAGIC: &str = "vorpal-cochange/1";

/// Whether the co-change derivation's inputs are provably unchanged since the prior
/// build — the composes' carry premise for CHANGES_WITH edges (respan and defs-stable
/// both carry them byte-identically). True when: co-change is disabled; or the tree is
/// not a git repository (both builds derive nothing); or the HEAD-keyed cache the prior
/// build read or wrote matches the CURRENT (head, commit-window) exactly — a commit's
/// ancestry is immutable, so header equality pins the pair set. Anything unprovable is
/// `false`: the caller declines and the full pipeline re-derives honestly.
pub(crate) fn inputs_unchanged(src: &Path, cache_path: &Path) -> bool {
  let commits = commits_from_env();
  if commits == 0 {
    return true;
  }
  if git_output(src, &["rev-parse", "--show-toplevel"]).is_err() {
    return true;
  }
  let Ok(head) = git_output(src, &["rev-parse", "HEAD"]) else {
    return false;
  };
  let Ok(text) = std::fs::read_to_string(cache_path) else {
    return false;
  };
  let Some(header) = text.lines().next() else {
    return false;
  };
  let mut parts = header.split(' ');
  parts.next() == Some(CACHE_MAGIC)
    && parts.next() == Some(head.trim())
    && parts.next().and_then(|c| c.parse::<usize>().ok()) == Some(commits)
}

/// Begin the pass: resolve the repository, consult the HEAD-keyed cache, else spawn
/// `git log`. Cheap (two `git rev-parse` calls) — call before the extraction stream.
pub fn start(src: &Path, cache_path: &Path) -> Pending {
  let commits = commits_from_env();
  let mut pending = Pending {
    commits,
    head: String::new(),
    prefix: String::new(),
    cache_path: cache_path.to_path_buf(),
    state: PendingState::Skipped(String::new()),
  };
  if commits == 0 {
    pending.state =
      PendingState::Skipped("co-change disabled (VORPAL_COCHANGE_COMMITS=0)".to_string());
    return pending;
  }
  let toplevel = match git_output(src, &["rev-parse", "--show-toplevel"]) {
    Ok(text) => text.trim().to_string(),
    Err(reason) => {
      pending.state =
        PendingState::Skipped(format!("co-change skipped: {}", short_reason(&reason)));
      return pending;
    }
  };
  pending.head = git_output(src, &["rev-parse", "HEAD"])
    .map(|t| t.trim().to_string())
    .unwrap_or_default();
  // Map git's toplevel-relative paths onto the index's path strings: the indexed root is
  // `toplevel/<prefix>` and manifest paths are `<src as given>/<rest>`.
  let src_canon = src
    .canonicalize()
    .map(|p| p.to_string_lossy().into_owned())
    .unwrap_or_default();
  let toplevel_canon = Path::new(&toplevel)
    .canonicalize()
    .map(|p| p.to_string_lossy().into_owned())
    .unwrap_or(toplevel.clone());
  pending.prefix = src_canon
    .strip_prefix(&toplevel_canon)
    .map(|rest| rest.trim_start_matches('/').to_string())
    .unwrap_or_default();

  if !pending.head.is_empty() {
    if let Some(edges) = load_cache(cache_path, &pending.head, commits) {
      pending.state = PendingState::Cached(edges);
      return pending;
    }
  }
  let count_arg = commits.to_string();
  match Command::new("git")
    .arg("-C")
    .arg(src)
    .args([
      "log",
      "--name-only",
      "--no-merges",
      "--format=%x01",
      "-n",
      &count_arg,
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
  {
    Ok(mut child) => match child.stdout.take() {
      Some(mut stdout) => {
        let drain = std::thread::spawn(move || {
          use std::io::Read;
          let mut text = String::new();
          stdout.read_to_string(&mut text).map(|_| text)
        });
        pending.state = PendingState::Running(child, drain);
      }
      None => {
        pending.state = PendingState::Skipped("co-change skipped: git stdout unavailable".to_string());
      }
    },
    Err(err) => {
      pending.state = PendingState::Skipped(format!("co-change skipped: git unavailable: {err}"));
    }
  }
  pending
}

/// Complete the pass against the indexed paths (the exact strings the File nodes carry).
/// Call after the extraction stream; joins the child, counts, caches the raw pairs, and
/// finalizes them against this build's manifest.
pub fn finish<'a>(
  pending: Pending,
  src: &Path,
  manifest_paths: impl Iterator<Item = &'a str>,
) -> Cochange {
  let commits = pending.commits;
  let raw: Vec<RawPair> = match pending.state {
    PendingState::Skipped(note) => return skipped(note),
    PendingState::Cached(raw) => raw,
    PendingState::Running(mut child, drain) => {
      let text = match drain.join() {
        Ok(Ok(text)) => text,
        Ok(Err(err)) => return skipped(format!("co-change skipped: reading git: {err}")),
        Err(_) => return skipped("co-change skipped: git reader thread failed".to_string()),
      };
      match child.wait() {
        Ok(status) if status.success() => {}
        Ok(_) => {
          let mut reason = String::new();
          if let Some(mut err) = child.stderr.take() {
            use std::io::Read;
            let _ = err.read_to_string(&mut reason);
          }
          return skipped(format!("co-change skipped: {}", short_reason(reason.trim())));
        }
        Err(err) => return skipped(format!("co-change skipped: git failed: {err}")),
      }
      let (raw, commits_read) = match count_pairs(&text, commits) {
        Ok(counted) => counted,
        Err(note) => return skipped(note),
      };
      if commits_read == 0 {
        return skipped("co-change skipped: no git history".to_string());
      }
      if !pending.head.is_empty() {
        // Best-effort: a cache write failure only costs the next build a `git log`.
        let _ = save_cache(&pending.cache_path, &pending.head, commits, &raw);
      }
      raw
    }
  };
  let edges = finalize(raw, src, &pending.prefix, manifest_paths);
  Cochange {
    edges,
    commits_read: commits,
    note: None,
  }
}

/// Count co-changed pairs over git's own paths (interned to `u32` ids for the table) and
/// keep those at or above the floor — manifest-independent, so the result caches.
fn count_pairs(log: &str, commits: usize) -> Result<(Vec<RawPair>, usize), String> {
  let mut interned: Vec<&str> = Vec::new();
  let mut index_of: HashMap<&str, u32> = HashMap::new();
  let mut pairs: HashMap<(u32, u32), u32> = HashMap::new();
  let mut commits_read = 0usize;
  let mut files: Vec<u32> = Vec::new();
  for block in log.split('\u{1}') {
    let lines: Vec<&str> = block
      .lines()
      .map(str::trim)
      .filter(|l| !l.is_empty())
      .collect();
    if block.trim().is_empty() {
      continue;
    }
    commits_read += 1;
    // The sweep bound is judged on the RAW commit: a 300-file reformat that happens to
    // touch 30 indexed files still couples nothing.
    if lines.len() > MAX_FILES_PER_COMMIT {
      continue;
    }
    files.clear();
    for line in &lines {
      let id = match index_of.get(line) {
        Some(&id) => id,
        None => {
          let id = interned.len() as u32;
          interned.push(line);
          index_of.insert(line, id);
          id
        }
      };
      files.push(id);
    }
    files.sort_unstable();
    files.dedup();
    if files.len() < 2 {
      continue;
    }
    for i in 0..files.len() {
      for j in i + 1..files.len() {
        let key = (files[i], files[j]);
        if !pairs.contains_key(&key) && pairs.len() >= MAX_PAIRS {
          return Err(format!(
            "co-change skipped: more than {MAX_PAIRS} distinct file pairs in the last \
             {commits} commits (lower VORPAL_COCHANGE_COMMITS)"
          ));
        }
        *pairs.entry(key).or_insert(0) += 1;
      }
    }
  }
  let mut raw: Vec<RawPair> = pairs
    .into_iter()
    .filter(|(_, count)| *count >= MIN_COUNT)
    .map(|((a, b), count)| RawPair {
      a: interned[a as usize].to_string(),
      b: interned[b as usize].to_string(),
      count,
    })
    .collect();
  raw.sort_by(|x, y| x.a.cmp(&y.a).then_with(|| x.b.cmp(&y.b)));
  Ok((raw, commits_read))
}

/// Map raw pairs onto this build's indexed paths, keep each file's strongest partners,
/// and emit symmetric edges once each — deterministic ordering throughout.
fn finalize<'a>(
  raw: Vec<RawPair>,
  src: &Path,
  prefix: &str,
  manifest_paths: impl Iterator<Item = &'a str>,
) -> Vec<CochangeEdge> {
  let interned: Vec<&str> = manifest_paths.collect();
  let index_of: HashMap<&str, u32> = interned
    .iter()
    .enumerate()
    .map(|(i, p)| (*p, i as u32))
    .collect();
  let to_id = |git_path: &str| -> Option<u32> {
    let rest = if prefix.is_empty() {
      git_path
    } else {
      git_path.strip_prefix(prefix)?.strip_prefix('/')?
    };
    let candidate = src.join(rest);
    index_of.get(candidate.to_string_lossy().as_ref()).copied()
  };
  let mut counts: HashMap<(u32, u32), u32> = HashMap::new();
  let mut by_file: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
  for pair in &raw {
    let (Some(a), Some(b)) = (to_id(&pair.a), to_id(&pair.b)) else {
      continue;
    };
    let (a, b) = (a.min(b), a.max(b));
    counts.insert((a, b), pair.count);
    by_file.entry(a).or_default().push((b, pair.count));
    by_file.entry(b).or_default().push((a, pair.count));
  }
  let mut kept: HashSet<(u32, u32)> = HashSet::new();
  for (&file, partners) in by_file.iter_mut() {
    partners.sort_by(|x, y| {
      y.1
        .cmp(&x.1)
        .then_with(|| interned[x.0 as usize].cmp(interned[y.0 as usize]))
    });
    for &(partner, _) in partners.iter().take(MAX_PARTNERS) {
      kept.insert((file.min(partner), file.max(partner)));
    }
  }
  let mut edges: Vec<CochangeEdge> = kept
    .into_iter()
    .map(|(a, b)| {
      let count = counts.get(&(a, b)).copied().unwrap_or(MIN_COUNT);
      CochangeEdge {
        a: interned[a as usize].to_string(),
        b: interned[b as usize].to_string(),
        confidence: count.saturating_mul(20).min(100) as u8,
      }
    })
    .collect();
  edges.sort_by(|x, y| x.a.cmp(&y.a).then_with(|| x.b.cmp(&y.b)));
  edges
}

/// The HEAD-keyed cache: `magic head commits` then `count\ta\tb` lines over git-relative
/// paths. A commit's ancestry is immutable, so (HEAD, window) fully determines the pairs;
/// mapping onto the indexed set happens at load, so the cache survives manifest changes.
fn load_cache(path: &Path, head: &str, commits: usize) -> Option<Vec<RawPair>> {
  let text = std::fs::read_to_string(path).ok()?;
  let mut lines = text.lines();
  let header = lines.next()?;
  let mut parts = header.split(' ');
  if parts.next()? != CACHE_MAGIC || parts.next()? != head {
    return None;
  }
  if parts.next()?.parse::<usize>().ok()? != commits {
    return None;
  }
  let mut raw = Vec::new();
  for line in lines {
    let mut fields = line.split('\t');
    let count: u32 = fields.next()?.parse().ok()?;
    let a = fields.next()?.to_string();
    let b = fields.next()?.to_string();
    raw.push(RawPair { a, b, count });
  }
  Some(raw)
}

fn save_cache(path: &Path, head: &str, commits: usize, raw: &[RawPair]) -> std::io::Result<()> {
  use std::io::Write;
  let tmp = path.with_extension("cache.tmp");
  {
    let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    writeln!(out, "{CACHE_MAGIC} {head} {commits}")?;
    for pair in raw {
      writeln!(out, "{}\t{}\t{}", pair.count, pair.a, pair.b)?;
    }
    out.flush()?;
  }
  std::fs::rename(&tmp, path)
}

fn short_reason(reason: &str) -> String {
  let first = reason.lines().next().unwrap_or("").trim();
  if first.contains("not a git repository") {
    "not a git repository".to_string()
  } else if first.is_empty() {
    "git failed".to_string()
  } else {
    first.to_string()
  }
}

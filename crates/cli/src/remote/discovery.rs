//! Streaming-mode discovery reconstruction (invariant I1, docs/REMOTE.md §3.3).
//!
//! A byte-streaming node cannot run our walker, and a naive enumeration (tar-style) examines a
//! *different* file set than a local `vorpal scan` — vendored deps, build output, everything
//! gitignored. To uphold I1 the coordinator reconstructs the walk: the node ships a **raw**
//! enumeration (every entry, no filtering) plus the ignore-relevant metadata (ignore-file
//! contents, `.git` presence, ancestor chains), and this module replays the `ignore` crate's own
//! matcher semantics over that data, using the crate's real `Gitignore`/`Override`/`Types`
//! matchers for all glob evaluation.
//!
//! The matcher *composition* mirrors `ignore` v0.4.27 `dir.rs`/`walk.rs` exactly:
//!
//! * overrides first — a match (either polarity) is final; with any positive override, an
//!   unmatched **file** is ignored, dirs pass so descent can continue;
//! * per-category first-match walking the directory chain deepest-first — `.ignore` beats
//!   `.gitignore` beats `.git/info/exclude` beats the global file, each category resolving
//!   independently; git-flavored categories apply only when a `.git`/`.jj` exists somewhere in
//!   the chain (`require_git` defaults on) and stop at a nested-repo boundary (`saw_git`);
//! * ancestor directories above each walk root participate (matched against the canonicalized
//!   root — `parents(true)`), and file types filter files only;
//! * hidden filtering applies last and **only when nothing else matched**, so a whitelist can
//!   rescue a dotfile; ignored directories are pruned outright (no whitelist resurrection below
//!   them); walk roots themselves are exempt from all filtering.
//!
//! Any drift between this replay and the real walker is a correctness bug caught by the
//! differential test (`streaming_discovery_matches_walkparallel`), which runs both over the same
//! fixtures.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::Override;
use ignore::types::Types;

use vorpal_wire::NoIgnore;

use crate::utils::{IgnoreFile, WalkIgnore};

/// The walk-configuration half of reconstruction, mirroring `NoIgnore::walk` +
/// `InputArgs::walk_basic`.
pub struct WalkConfig {
  pub overrides: Override,
  /// Language file types (`None` = no restriction, as in `InputArgs::walk`).
  pub types: Option<Types>,
  pub filter_hidden: bool,
  pub match_parents: bool,
  pub use_dot_ignore: bool,
  pub use_git_ignore: bool,
  pub use_git_exclude: bool,
  pub use_git_global: bool,
}

impl WalkConfig {
  /// Build from wire `--no-ignore` selectors (the form a non-loopback node ships), deriving the
  /// per-category settings from the **single** source shared with the real walk
  /// (`utils::NoIgnore::effective`) so stream-mode discovery and `vorpal scan` cannot diverge on
  /// `--no-ignore` semantics (I1). Symlink-following is handled by the live walker
  /// (`reconstruct_local`), not by matching, so it is not part of this config.
  ///
  /// The loopback path uses [`WalkConfig::from_walk_ignore`] with the real CLI flags directly;
  /// this wire-based entry is exercised by the discovery parity tests and is the seam R1's
  /// shipped-snapshot reconstruction will use.
  #[cfg_attr(not(test), allow(dead_code))]
  pub fn from_flags(overrides: Override, types: Option<Types>, no_ignore: &[NoIgnore]) -> Self {
    let ignore_files: Vec<IgnoreFile> = no_ignore.iter().copied().map(wire_to_ignore_file).collect();
    Self::from_walk_ignore(overrides, types, crate::utils::NoIgnore::disregard(&ignore_files).effective())
  }

  /// Build directly from resolved [`WalkIgnore`] settings.
  pub fn from_walk_ignore(overrides: Override, types: Option<Types>, e: WalkIgnore) -> Self {
    Self {
      overrides,
      types,
      filter_hidden: e.hidden,
      match_parents: e.parents,
      use_dot_ignore: e.dot_ignore,
      use_git_ignore: e.git_ignore,
      use_git_exclude: e.git_exclude,
      use_git_global: e.git_global,
    }
  }
}

/// Map a wire `--no-ignore` selector back to the CLI's `IgnoreFile` (they are the same set).
#[cfg_attr(not(test), allow(dead_code))]
fn wire_to_ignore_file(n: NoIgnore) -> IgnoreFile {
  match n {
    NoIgnore::Hidden => IgnoreFile::Hidden,
    NoIgnore::Dot => IgnoreFile::Dot,
    NoIgnore::Exclude => IgnoreFile::Exclude,
    NoIgnore::Global => IgnoreFile::Global,
    NoIgnore::Parent => IgnoreFile::Parent,
    NoIgnore::Vcs => IgnoreFile::Vcs,
  }
}

/// One directory's ignore-relevant facts, as shipped by (or enumerated for) a node.
#[derive(Debug, Clone, Default)]
pub struct DirFacts {
  /// `.git` (any file type) or `.jj` exists here — feeds `require_git` and nested-repo gating.
  pub has_git: bool,
  pub gitignore: Option<String>,
  pub dot_ignore: Option<String>,
  /// Contents of `<git-commondir>/info/exclude` for a repo rooted here.
  pub git_exclude: Option<String>,
}

/// The global gitignore source. Loopback uses the local machine's (it *is* the node's); remote
/// transports ship the node's file content (`Content`) or opt out (`None`).
#[allow(dead_code)] // `None`/`Content` are used by non-loopback transports (R1+).
pub enum GlobalIgnore {
  None,
  LocalMachine,
  Content(String),
}

#[cfg(unix)]
fn dir_identity(meta: &fs::Metadata) -> (u64, u64) {
  use std::os::unix::fs::MetadataExt;
  (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn dir_identity(_meta: &fs::Metadata) -> (u64, u64) {
  (0, 0)
}

fn read_file_opt(path: &Path) -> Option<String> {
  fs::read_to_string(path).ok()
}

/// Read an ignore file the way `GitignoreBuilder::add` consumes it: line by line, **stopping at
/// the first line that is not valid UTF-8** (the crate's `BufRead::lines` loop breaks there,
/// keeping the patterns before it). A whole-file `read_to_string` would instead drop the entire
/// file on one bad byte — a different pattern set than the real walker compiled (I1).
fn read_ignore_text(path: &Path) -> Option<String> {
  let bytes = fs::read(path).ok()?;
  match String::from_utf8(bytes) {
    Ok(text) => Some(text),
    Err(err) => {
      let bytes = err.into_bytes();
      let mut kept = String::new();
      for line in bytes.split(|&b| b == b'\n') {
        match std::str::from_utf8(line) {
          Ok(l) => {
            kept.push_str(l);
            kept.push('\n');
          }
          Err(_) => break,
        }
      }
      Some(kept)
    }
  }
}

/// Gather one directory's ignore facts, mirroring `Ignore::add_child_path` (including the
/// worktree `gitdir:`/commondir resolution for `info/exclude`).
fn read_dir_facts(dir: &Path) -> DirFacts {
  let git = dir.join(".git");
  let git_meta = fs::metadata(&git).ok();
  let has_git = git_meta.is_some() || dir.join(".jj").exists();
  let git_exclude = git_meta.and_then(|m| {
    let git_dir = if m.is_file() {
      // Worktree: `.git` is a file containing `gitdir: <path>`.
      let contents = read_file_opt(&git)?;
      let rel = contents.strip_prefix("gitdir:")?.trim();
      let gitdir = if Path::new(rel).is_absolute() { PathBuf::from(rel) } else { dir.join(rel) };
      // A worktree gitdir may point at a commondir indirection.
      match read_file_opt(&gitdir.join("commondir")) {
        Some(common) => {
          let common = common.trim();
          if Path::new(common).is_absolute() {
            PathBuf::from(common)
          } else {
            gitdir.join(common)
          }
        }
        None => gitdir,
      }
    } else {
      git
    };
    read_ignore_text(&git_dir.join("info").join("exclude"))
  });
  DirFacts {
    has_git,
    gitignore: read_ignore_text(&dir.join(".gitignore")),
    dot_ignore: read_ignore_text(&dir.join(".ignore")),
    git_exclude,
  }
}

/// Whether the walker's hidden filter would consider this entry hidden: a dot-prefixed name on
/// every platform, plus the `FILE_ATTRIBUTE_HIDDEN` attribute on Windows (`pathutil::is_hidden`).
fn entry_is_hidden(name: &std::ffi::OsStr, symlink_meta: &fs::Metadata) -> bool {
  #[cfg(windows)]
  {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    if symlink_meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0 {
      return true;
    }
  }
  #[cfg(not(windows))]
  let _ = symlink_meta;
  name.as_encoded_bytes().first() == Some(&b'.')
}

/// The mutable state carried down the live walk: the deepest-first `Level` stack, the ancestor
/// levels above the root, the compiled global matcher, and the symlink-loop identity chain.
struct WalkState<'a> {
  cfg: &'a WalkConfig,
  ancestors: &'a [Level],
  global: &'a Option<Gitignore>,
  /// Canonicalized walk root, for rebasing parent-matcher queries (`absolute_base`).
  canonical: Option<PathBuf>,
  /// Display base — the root exactly as given on the CLI; output paths join onto it.
  root: PathBuf,
  follow_links: bool,
}

/// Walk one directory live, **pruning skipped directories before descent** (the real walker never
/// reads inside an ignored dir). Reads each surviving directory once, applying the same matcher
/// composition as `Ignore::matched`. `levels` is the deepest-first chain to and including
/// `abs_dir`; `chain` is the symlink-loop identity set (root + ancestor dirs).
fn descend(
  st: &WalkState<'_>,
  rel_dir: &Path,
  levels: &mut Vec<Level>,
  chain: &mut Vec<(u64, u64)>,
  out: &mut Vec<PathBuf>,
) {
  let abs_dir = st.root.join(rel_dir);
  let Ok(read) = fs::read_dir(&abs_dir) else {
    return;
  };
  for entry in read.flatten() {
    let name = entry.file_name();
    let abs = abs_dir.join(&name);
    let rel = rel_dir.join(&name);
    let Ok(symlink_meta) = fs::symlink_metadata(&abs) else {
      continue;
    };
    let hidden = entry_is_hidden(&name, &symlink_meta);
    let (is_dir, is_file) = if symlink_meta.file_type().is_symlink() {
      if st.follow_links {
        match fs::metadata(&abs) {
          Ok(target) => {
            let loops = target.is_dir() && chain.contains(&dir_identity(&target));
            if loops {
              // The walker reports a loop error and does not descend; nothing is yielded.
              (false, false)
            } else {
              (target.is_dir(), target.is_file())
            }
          }
          // Broken link: the walker surfaces an error, yields nothing.
          Err(_) => (false, false),
        }
      } else {
        // Unfollowed symlink: matched as a non-dir, yielded, then dropped (not a file).
        (false, false)
      }
    } else {
      (symlink_meta.is_dir(), symlink_meta.is_file())
    };
    let joined = st.root.join(&rel);
    let query = strip_dot_slash(&joined);
    let abs_query = st.canonical.as_ref().map(|base| base.join(&rel));
    if is_skipped(query, abs_query.as_deref(), is_dir, hidden, levels, st.ancestors, st.global, st.cfg)
    {
      // Pruned: an ignored directory is never descended (the perf crux) and an ignored file is
      // never emitted.
      continue;
    }
    if is_dir {
      let facts = read_dir_facts(&abs);
      levels.push(facts_to_level(&joined, &facts, st.cfg));
      let pushed = fs::metadata(&abs).ok().map(|m| dir_identity(&m));
      if let Some(id) = pushed {
        chain.push(id);
      }
      descend(st, &rel, levels, chain, out);
      if pushed.is_some() {
        chain.pop();
      }
      levels.pop();
    } else if is_file {
      out.push(display_path(&joined));
    }
  }
}

// ---------------------------------------------------------------------------
// Reconstruction
// ---------------------------------------------------------------------------

struct Level {
  /// Whether git-flavored categories are still live at and above this level (nested-repo gate:
  /// a `.git` *below* stops `.gitignore`s *above* from applying).
  has_git: bool,
  dot_ignore: Option<Gitignore>,
  gitignore: Option<Gitignore>,
  git_exclude: Option<Gitignore>,
}

fn build_matcher(root: &Path, file_name: &str, content: &str) -> Option<Gitignore> {
  let mut builder = GitignoreBuilder::new(root);
  let from = Some(root.join(file_name));
  for (i, line) in content.lines().enumerate() {
    // Match `GitignoreBuilder::add` exactly: the first line is stripped of a UTF-8 BOM
    // (`trim_start_matches`, i.e. all leading BOMs) — Windows editors commonly write one, and
    // without the strip the first pattern compiles as `\u{feff}pattern` and never matches (I1).
    let line = if i == 0 { line.trim_start_matches('\u{feff}') } else { line };
    // I/O-shaped errors are ignored by the crate; malformed globs are skipped the same way.
    let _ = builder.add_line(from.clone(), line);
  }
  builder.build().ok()
}

fn facts_to_level(dir_path: &Path, facts: &DirFacts, cfg: &WalkConfig) -> Level {
  Level {
    has_git: facts.has_git,
    dot_ignore: match (&facts.dot_ignore, cfg.use_dot_ignore) {
      (Some(c), true) => build_matcher(dir_path, ".ignore", c),
      _ => None,
    },
    gitignore: match (&facts.gitignore, cfg.use_git_ignore) {
      (Some(c), true) => build_matcher(dir_path, ".gitignore", c),
      _ => None,
    },
    git_exclude: match (&facts.git_exclude, cfg.use_git_exclude) {
      (Some(c), true) => build_matcher(dir_path, "info/exclude", c),
      _ => None,
    },
  }
}

fn global_matcher(source: &GlobalIgnore, cfg: &WalkConfig) -> Option<Gitignore> {
  if !cfg.use_git_global {
    return None;
  }
  match source {
    GlobalIgnore::None => None,
    GlobalIgnore::LocalMachine => Some(Gitignore::global().0),
    GlobalIgnore::Content(content) => {
      let cwd = std::env::current_dir().ok()?;
      build_matcher(&cwd, "global-gitignore", content)
    }
  }
}

/// Match one entry against the reconstructed stack, returning the walker's skip decision.
/// `levels` is the relative dir chain deepest-first; `ancestors` continues upward.
#[allow(clippy::too_many_arguments)]
fn is_skipped(
  query: &Path,
  abs_query: Option<&Path>,
  is_dir: bool,
  hidden: bool,
  levels: &[Level],
  ancestors: &[Level],
  global: &Option<Gitignore>,
  cfg: &WalkConfig,
) -> bool {
  // 1. Overrides: any hit is final; unmatched files are ignored when positive globs exist.
  if !cfg.overrides.is_empty() {
    match cfg.overrides.matched(query, is_dir) {
      Match::Ignore(_) => return true,
      Match::Whitelist(_) => return false,
      Match::None => {}
    }
  }
  // 2. The per-category chains (Ignore::matched_ignore).
  let any_git = levels.iter().chain(ancestors).any(|l| l.has_git);
  let mut m_ignore: Match<()> = Match::None;
  let mut m_gi: Match<()> = Match::None;
  let mut m_exclude: Match<()> = Match::None;
  let mut saw_git = false;
  let mut check = |level: &Level, path: &Path, saw_git: &mut bool| {
    if m_ignore.is_none() {
      if let Some(m) = &level.dot_ignore {
        m_ignore = m.matched(path, is_dir).map(|_| ());
      }
    }
    if any_git && !*saw_git {
      if m_gi.is_none() {
        if let Some(m) = &level.gitignore {
          m_gi = m.matched(path, is_dir).map(|_| ());
        }
      }
      if m_exclude.is_none() {
        if let Some(m) = &level.git_exclude {
          m_exclude = m.matched(path, is_dir).map(|_| ());
        }
      }
    }
    *saw_git = *saw_git || level.has_git;
  };
  for level in levels.iter().rev() {
    check(level, query, &mut saw_git);
  }
  if cfg.match_parents {
    if let Some(abs) = abs_query {
      for level in ancestors {
        check(level, abs, &mut saw_git);
      }
    }
  }
  let m_global: Match<()> = match (any_git, global) {
    (true, Some(g)) => g.matched(query, is_dir).map(|_| ()),
    _ => Match::None,
  };
  let ignore_verdict = m_ignore.or(m_gi).or(m_exclude).or(m_global);
  let mut whitelisted = false;
  match ignore_verdict {
    Match::Ignore(_) => return true,
    Match::Whitelist(_) => whitelisted = true,
    Match::None => {}
  }
  // 3. File types (files only; dirs always pass).
  if let Some(types) = &cfg.types {
    match types.matched(query, is_dir) {
      Match::Ignore(_) => return true,
      Match::Whitelist(_) => whitelisted = true,
      Match::None => {}
    }
  }
  // 4. Hidden — only when nothing whitelisted the entry. The verdict comes from the enumerator
  // (dot-prefix everywhere; plus the HIDDEN attribute on Windows), mirroring `pathutil::is_hidden`.
  if !whitelisted && cfg.filter_hidden && hidden {
    return true;
  }
  false
}

/// Reconstruct the walk over one root **live**, reading the filesystem as it descends and pruning
/// ignored directories before entering them, returning the surviving **files** with the exact
/// display paths a local walk yields (root-joined, one leading `./` stripped). For loopback the
/// "node" is the local machine, so this both enumerates and matches in one pass.
pub fn reconstruct_local(
  root: &Path,
  follow_links: bool,
  cfg: &WalkConfig,
  global: &GlobalIgnore,
) -> Result<Vec<PathBuf>> {
  // Roots are always stat-followed: the walker builds root entries via `fs::metadata`
  // (`DirEntryRaw::from_path`), so an explicit symlink root scans its target even without
  // `--follow`.
  match fs::metadata(root) {
    Err(e) => {
      // The walker reports root errors on stderr and continues; mirror it.
      eprintln!("ERROR: {e}");
      return Ok(vec![]);
    }
    // Explicit file roots bypass all filtering.
    Ok(m) if !m.is_dir() => return Ok(vec![display_path(root)]),
    Ok(_) => {}
  }

  let canonical = root.canonicalize().ok();
  let global = global_matcher(global, cfg);
  // Ancestor matchers above the root (matched against the canonicalized root — `parents(true)`).
  let mut ancestors: Vec<Level> = Vec::new();
  if let Some(canonical) = &canonical {
    let mut cur = canonical.parent();
    while let Some(dir) = cur {
      let mut level = facts_to_level(dir, &read_dir_facts(dir), cfg);
      // Mirror `add_parents`: ancestor `has_git` is only consulted when gitignore matching is
      // enabled (the crate skips the check otherwise).
      if !cfg.use_git_ignore {
        level.has_git = false;
      }
      ancestors.push(level);
      cur = dir.parent();
    }
  }

  let st = WalkState {
    cfg,
    ancestors: &ancestors,
    global: &global,
    canonical: canonical.clone(),
    root: root.to_path_buf(),
    follow_links,
  };
  let mut levels = vec![facts_to_level(root, &read_dir_facts(root), cfg)];
  // Seed the loop chain with the root's own identity: `check_symlink_loop` compares a followed
  // link against every ancestor level *including the walk root*, so `self -> .` is a loop, not a
  // second copy of the tree.
  let mut chain = Vec::new();
  if let Ok(m) = fs::metadata(root) {
    chain.push(dir_identity(&m));
  }
  let mut out = Vec::new();
  descend(&st, Path::new(""), &mut levels, &mut chain, &mut out);
  Ok(out)
}

/// Mirror `Ignore::matched`'s leading-`./` strip for matcher queries.
fn strip_dot_slash(path: &Path) -> &Path {
  path.strip_prefix("./").unwrap_or(path)
}

/// Mirror `filter_result`'s display-path normalization (one `./` stripped).
fn display_path(path: &Path) -> PathBuf {
  strip_dot_slash(path).to_path_buf()
}

// ---------------------------------------------------------------------------
// Snapshot-based reconstruction (for transports that ship a pre-enumerated tree)
// ---------------------------------------------------------------------------

/// A pre-enumerated view of one walk root, as a remote (non-loopback) transport ships it. The
/// loopback path walks the local FS live (`reconstruct_local`); a remote node instead ships this
/// snapshot and the coordinator reconstructs from it — reusing the **same matcher core**
/// (`is_skipped`/`facts_to_level`), so I1 filtering parity is inherited.
#[derive(Default)]
pub struct RemoteSnapshot {
  /// The root exactly as given on the CLI (display paths derive from it).
  pub root: PathBuf,
  pub kind: RootKind,
  /// The node's canonicalized root (its physical `pwd -P`), for ancestor-matcher queries.
  pub canonical: Option<PathBuf>,
  /// Entries under each root-relative directory ("" = the root itself).
  pub children: std::collections::BTreeMap<PathBuf, Vec<SnapEntry>>,
  /// Ignore facts per root-relative directory.
  pub dirs: std::collections::BTreeMap<PathBuf, DirFacts>,
  /// Ancestor directories of the canonical root (immediate parent first), with their facts.
  pub ancestors: Vec<(PathBuf, DirFacts)>,
}

/// One entry in a shipped snapshot. `hidden` is the node-computed hidden-filter verdict (a
/// remote node reports it because it is platform-dependent).
#[derive(Clone, Debug)]
pub struct SnapEntry {
  pub rel: PathBuf,
  pub is_dir: bool,
  pub is_file: bool,
  pub hidden: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RootKind {
  #[default]
  Dir,
  File,
  /// Missing/unreadable on the node — reported like the walker reports it (error, no files).
  Error(String),
}

/// Reconstruct surviving files from a shipped snapshot, using the identical matcher composition as
/// the live local walk. Symlink-follow loop detection is not modeled here (the snapshot enumerator
/// does not follow links), matching the default no-`--follow` behavior.
pub fn reconstruct_snapshot(
  snapshot: &RemoteSnapshot,
  cfg: &WalkConfig,
  global: &GlobalIgnore,
) -> Vec<PathBuf> {
  match &snapshot.kind {
    RootKind::Error(e) => {
      eprintln!("ERROR: {e}");
      return vec![];
    }
    RootKind::File => return vec![display_path(&snapshot.root)],
    RootKind::Dir => {}
  }
  let global = global_matcher(global, cfg);
  let ancestors: Vec<Level> = snapshot
    .ancestors
    .iter()
    .map(|(dir, facts)| {
      let mut level = facts_to_level(dir, facts, cfg);
      if !cfg.use_git_ignore {
        level.has_git = false;
      }
      level
    })
    .collect();
  let mut out = Vec::new();
  let root_facts = snapshot.dirs.get(Path::new("")).cloned().unwrap_or_default();
  let mut levels = vec![facts_to_level(&snapshot.root, &root_facts, cfg)];
  walk_snapshot(snapshot, Path::new(""), &mut levels, &ancestors, &global, cfg, &mut out);
  out
}

#[allow(clippy::too_many_arguments)]
fn walk_snapshot(
  snapshot: &RemoteSnapshot,
  rel_dir: &Path,
  levels: &mut Vec<Level>,
  ancestors: &[Level],
  global: &Option<Gitignore>,
  cfg: &WalkConfig,
  out: &mut Vec<PathBuf>,
) {
  let Some(children) = snapshot.children.get(rel_dir) else {
    return;
  };
  for entry in children {
    let joined = snapshot.root.join(&entry.rel);
    let query = strip_dot_slash(&joined);
    let abs_query = snapshot.canonical.as_ref().map(|base| base.join(&entry.rel));
    if is_skipped(query, abs_query.as_deref(), entry.is_dir, entry.hidden, levels, ancestors, global, cfg)
    {
      continue;
    }
    if entry.is_dir {
      let facts = snapshot.dirs.get(&entry.rel).cloned().unwrap_or_default();
      levels.push(facts_to_level(&joined, &facts, cfg));
      walk_snapshot(snapshot, &entry.rel, levels, ancestors, global, cfg, out);
      levels.pop();
    } else if entry.is_file {
      out.push(display_path(&joined));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use ignore::WalkBuilder;
  use ignore::overrides::OverrideBuilder;
  use std::collections::BTreeSet;
  use std::fs;

  /// Ground truth: what the real parallel walker (with vorpal's exact builder wiring) yields.
  fn real_walk(
    root: &Path,
    no_ignore: &[NoIgnore],
    globs: &[&str],
    follow: bool,
  ) -> BTreeSet<PathBuf> {
    use std::sync::Mutex;
    let mut builder = WalkBuilder::new(root);
    let has = |flag: NoIgnore| no_ignore.contains(&flag);
    let vcs = has(NoIgnore::Vcs);
    builder
      .hidden(!has(NoIgnore::Hidden))
      .parents(!has(NoIgnore::Parent))
      .ignore(!has(NoIgnore::Dot))
      .git_global(!vcs && !has(NoIgnore::Global))
      .git_ignore(!vcs)
      .git_exclude(!vcs && !has(NoIgnore::Exclude))
      .follow_links(follow)
      .threads(2);
    if !globs.is_empty() {
      let cwd = std::env::current_dir().unwrap();
      let mut ov = OverrideBuilder::new(cwd);
      for g in globs {
        ov.add(g).unwrap();
      }
      builder.overrides(ov.build().unwrap());
    }
    let results = Mutex::new(BTreeSet::new());
    builder.build_parallel().run(|| {
      Box::new(|result| {
        if let Ok(entry) = result {
          if entry.file_type().is_some_and(|t| t.is_file()) {
            let p = entry.into_path();
            let p = p.strip_prefix("./").map(|p| p.to_path_buf()).unwrap_or(p);
            results.lock().unwrap().insert(p);
          }
        }
        ignore::WalkState::Continue
      })
    });
    results.into_inner().unwrap()
  }

  fn reconstructed(
    root: &Path,
    no_ignore: &[NoIgnore],
    globs: &[&str],
    follow: bool,
  ) -> BTreeSet<PathBuf> {
    let overrides = if globs.is_empty() {
      Override::empty()
    } else {
      let cwd = std::env::current_dir().unwrap();
      let mut ov = OverrideBuilder::new(cwd);
      for g in globs {
        ov.add(g).unwrap();
      }
      ov.build().unwrap()
    };
    let cfg = WalkConfig::from_flags(overrides, None, no_ignore);
    // LocalMachine mirrors exactly what the real walker consults (`Gitignore::global()`), so the
    // comparison stays hermetic regardless of this machine's global gitignore contents.
    reconstruct_local(root, follow, &cfg, &GlobalIgnore::LocalMachine)
      .unwrap()
      .into_iter()
      .collect()
  }

  fn assert_parity(root: &Path, no_ignore: &[NoIgnore], globs: &[&str], follow: bool) {
    let real = real_walk(root, no_ignore, globs, follow);
    let mine = reconstructed(root, no_ignore, globs, follow);
    assert_eq!(
      real, mine,
      "reconstructed file set diverges from WalkParallel (no_ignore={no_ignore:?}, globs={globs:?}, follow={follow})"
    );
  }

  fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
  }

  /// A fixture with nested gitignores, whitelists, dir-only rules, `.ignore` overriding
  /// `.gitignore`, hidden files, an ignored directory with a would-be-whitelisted child, and a
  /// nested git repo boundary.
  fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".git")).unwrap(); // require_git wants a repo
    write(root, ".gitignore", "target/\n*.log\n!keep.log\nsub/deep.rs\n");
    write(root, "main.rs", "fn main() {}\n");
    write(root, "kept.rs", "fn kept() {}\n");
    write(root, "a.log", "log\n");
    write(root, "keep.log", "kept log\n");
    write(root, ".hidden.rs", "fn hidden() {}\n");
    write(root, "target/build.rs", "fn ignored_dir() {}\n");
    write(root, "target/sub/x.rs", "fn nested_in_ignored() {}\n");
    write(root, "sub/deep.rs", "fn parent_ignored() {}\n");
    write(root, "sub/ok.rs", "fn ok() {}\n");
    write(root, "sub/.gitignore", "local.rs\n!deep.rs\n"); // whitelist cannot beat parent rule order? (first-match per category: deeper file wins)
    write(root, "sub/local.rs", "fn locally_ignored() {}\n");
    write(root, "dotted/.ignore", "byignore.rs\n");
    write(root, "dotted/byignore.rs", "fn dot_ignored() {}\n");
    write(root, "dotted/stays.rs", "fn stays() {}\n");
    // .ignore whitelist beating a .gitignore ignore (category precedence)
    write(root, "prec/.gitignore", "both.rs\n");
    write(root, "prec/.ignore", "!both.rs\n");
    write(root, "prec/both.rs", "fn precedence() {}\n");
    // nested repo: its own .gitignore applies, the outer one stops at the boundary
    fs::create_dir_all(root.join("nested/.git")).unwrap();
    write(root, "nested/.gitignore", "inner.rs\n");
    write(root, "nested/inner.rs", "fn inner_ignored() {}\n");
    write(root, "nested/outer.log", "outer pattern does not cross the boundary\n");
    dir
  }

  #[test]
  fn matches_walkparallel_on_defaults() {
    let dir = fixture();
    assert_parity(dir.path(), &[], &[], false);
  }

  #[test]
  fn matches_walkparallel_with_no_ignore_flags() {
    let dir = fixture();
    for flags in [
      vec![NoIgnore::Hidden],
      vec![NoIgnore::Vcs],
      vec![NoIgnore::Dot],
      vec![NoIgnore::Hidden, NoIgnore::Vcs, NoIgnore::Dot],
      vec![NoIgnore::Parent],
    ] {
      assert_parity(dir.path(), &flags, &[], false);
    }
  }

  #[test]
  fn matches_walkparallel_with_globs() {
    let dir = fixture();
    assert_parity(dir.path(), &[], &["*.rs"], false);
    assert_parity(dir.path(), &[], &["!sub/**"], false);
    assert_parity(dir.path(), &[], &["*.rs", "!prec/**"], false);
  }

  #[test]
  fn matches_walkparallel_under_parent_gitignore() {
    // The walk root is a subdirectory; its ancestors' .gitignore must apply (parents=true).
    let dir = fixture();
    write(dir.path(), "area/.keep", "");
    write(dir.path(), "area/app.rs", "fn app() {}\n");
    write(dir.path(), "area/note.log", "ignored by the ROOT .gitignore\n");
    assert_parity(&dir.path().join("area"), &[], &[], false);
    assert_parity(&dir.path().join("area"), &[NoIgnore::Parent], &[], false);
  }

  #[test]
  fn matches_walkparallel_without_git_repo() {
    // require_git: without a .git anywhere, .gitignore files must NOT apply.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "*.rs\n");
    write(root, "still_scanned.rs", "fn x() {}\n");
    write(root, ".ignore", "by_dot.rs\n"); // .ignore applies regardless of git
    write(root, "by_dot.rs", "fn y() {}\n");
    assert_parity(root, &[], &[], false);
  }

  #[test]
  fn matches_walkparallel_with_symlinks() {
    let dir = fixture();
    let root = dir.path();
    write(root, "linked/real.rs", "fn real() {}\n");
    #[cfg(unix)]
    {
      std::os::unix::fs::symlink(root.join("linked/real.rs"), root.join("file_link.rs")).unwrap();
      std::os::unix::fs::symlink(root.join("linked"), root.join("dir_link")).unwrap();
    }
    assert_parity(root, &[], &[], false);
    assert_parity(root, &[], &[], true);
  }

  #[test]
  fn matches_walkparallel_with_bom_gitignore() {
    // Windows editors commonly write a UTF-8 BOM; the crate strips it on the first line.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, ".gitignore", "\u{feff}target/\n*.log\n");
    write(root, "keep.rs", "fn k() {}\n");
    write(root, "target/gone.rs", "fn g() {}\n");
    write(root, "x.log", "log\n");
    assert_parity(root, &[], &[], false);
  }

  #[test]
  fn matches_walkparallel_with_invalid_utf8_gitignore_line() {
    // The crate keeps patterns before the first invalid-UTF-8 line and drops the rest.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    let mut bytes = b"*.log\n".to_vec();
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    bytes.extend_from_slice(b"\nkept-after-bad-line/\n");
    fs::write(root.join(".gitignore"), bytes).unwrap();
    write(root, "a.log", "log\n");
    write(root, "kept-after-bad-line/b.rs", "fn b() {}\n");
    write(root, "c.rs", "fn c() {}\n");
    assert_parity(root, &[], &[], false);
  }

  #[cfg(unix)]
  #[test]
  fn symlink_walk_roots_are_followed() {
    // Explicit roots are stat-followed by the walker even without --follow: a symlink root to a
    // directory scans its target; to a file, scans the file.
    let dir = fixture();
    let root = dir.path();
    let dir_link = root.join("root_link");
    std::os::unix::fs::symlink(root.join("sub"), &dir_link).unwrap();
    assert_parity(&dir_link, &[], &[], false);
    let file_link = root.join("file_root_link.rs");
    std::os::unix::fs::symlink(root.join("main.rs"), &file_link).unwrap();
    assert_parity(&file_link, &[], &[], false);
  }

  #[cfg(unix)]
  #[test]
  fn self_referential_symlink_is_a_loop_not_a_duplicate() {
    // `self -> .` under --follow: the walker reports a loop and yields nothing beneath the link;
    // reconstruction must not emit a second copy of the tree.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, "a.rs", "fn a() {}\n");
    std::os::unix::fs::symlink(root, root.join("self")).unwrap();
    assert_parity(root, &[], &[], true);
    let files = reconstructed(root, &[], &[], true);
    assert!(
      files.iter().all(|p| !p.to_string_lossy().contains("/self/")),
      "no duplicated tree under the self link: {files:?}"
    );
  }

  #[test]
  fn file_root_bypasses_all_filtering() {
    let dir = fixture();
    // a.log is gitignored, but an explicit file root is always scanned.
    let root = dir.path().join("a.log");
    let real = real_walk(&root, &[], &[], false);
    let mine = reconstructed(&root, &[], &[], false);
    assert_eq!(real, mine);
    assert_eq!(mine.len(), 1);
  }
}

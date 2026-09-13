//! Scope: the caller's working radius — directory or file prefixes to stay inside
//! (`within`), prefixes to leave out (`except`), and row facets (path class, language,
//! symbol kind, visibility, files changed since a git ref).
//!
//! A scope is a VIEW over an answer, never a change to the graph or to traversal: rows
//! whose defining file lies outside it are dropped from the page and counted, so a
//! complete answer stays complete as a number (`outsideScope`) instead of as rows an agent
//! feels obliged to visit. Traversals still pass through out-of-scope nodes — an in-scope
//! caller two hops away through an out-of-scope one is still reported, with its `via`.
//!
//! Entries are spelled the way a person types them (`fs/`, `drivers/net`, `mm/slab.c`, or
//! an absolute path) and resolved once against the index's source root, because node paths
//! are absolute and canonical. Matching is segment-exact: `fs` admits `fs/read_write.c` and
//! never `fsnotify/…`; a file entry admits exactly that file.

use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::Serialize;
use vorpal_kg::{Kg, NodeId};

/// A resolved scope: the entries as given (echoed back on every scoped answer) and the
/// absolute prefixes they resolve to, trailing slashes removed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PathScope {
  pub within: Vec<String>,
  #[serde(skip_serializing_if = "Vec::is_empty", default)]
  pub except: Vec<String>,
  /// Path classes admitted (`source`, `test`, `vendored`, `generated`); empty = all.
  #[serde(skip_serializing_if = "Vec::is_empty", default)]
  pub classes: Vec<String>,
  #[serde(skip_serializing_if = "Option::is_none", default)]
  pub kind: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none", default)]
  pub lang: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none", default)]
  pub exported: Option<bool>,
  /// A git ref (or `"worktree"`): only files changed since it.
  #[serde(rename = "changedSince", skip_serializing_if = "Option::is_none", default)]
  pub changed_since: Option<String>,
  /// How many files `changed_since` resolved to.
  #[serde(rename = "changedFiles", skip_serializing_if = "Option::is_none", default)]
  pub changed_files: Option<usize>,
  #[serde(skip)]
  prefixes: Vec<String>,
  #[serde(skip)]
  excludes: Vec<String>,
  /// Anchor-relative entries (`@file`, `@dir`, `@package`) not yet bound to a symbol.
  #[serde(skip)]
  deferred: Vec<String>,
  #[serde(skip)]
  class_set: Vec<crate::PathClass>,
  #[serde(skip)]
  kind_set: Option<vorpal_kg::SymbolKind>,
  #[serde(skip)]
  lang_set: Option<String>,
  /// Absolute paths changed since `changed_since`, sorted for binary search.
  #[serde(skip)]
  changed: Option<Vec<String>>,
}

/// `(entries as spelled, absolute prefixes, deferred anchor-relative entries)`.
type ResolvedEntries = (Vec<String>, Vec<String>, Vec<String>);

/// A scope as a caller states it, before resolution against a source root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeSpec {
  pub within: Vec<String>,
  pub except: Vec<String>,
  pub classes: Vec<String>,
  pub kind: Option<String>,
  pub lang: Option<String>,
  pub exported: Option<bool>,
  pub changed_since: Option<String>,
}

impl ScopeSpec {
  pub fn is_empty(&self) -> bool {
    self.within.is_empty()
      && self.except.is_empty()
      && self.classes.is_empty()
      && self.kind.is_none()
      && self.lang.is_none()
      && self.exported.is_none()
      && self.changed_since.is_none()
  }
}

/// Manifest files that mark a package boundary for `@package`, nearest first walking up
/// from the anchor's directory. Kernel-style trees mark every subsystem with a Makefile
/// or Kbuild, so `@package` there is the subsystem directory.
const PACKAGE_MANIFESTS: &[&str] = &[
  "Cargo.toml",
  "package.json",
  "go.mod",
  "pyproject.toml",
  "setup.py",
  "pom.xml",
  "build.gradle",
  "build.gradle.kts",
  "CMakeLists.txt",
  "Kbuild",
  "Makefile",
];

fn parse_class(text: &str) -> Result<crate::PathClass, String> {
  match text.trim().to_ascii_lowercase().as_str() {
    "source" => Ok(crate::PathClass::Source),
    "test" | "tests" => Ok(crate::PathClass::Test),
    "vendored" | "vendor" => Ok(crate::PathClass::Vendored),
    "generated" => Ok(crate::PathClass::Generated),
    other => Err(format!("unknown path class '{other}' (source, test, vendored, generated)")),
  }
}

impl PathScope {
  /// Resolve `entries` against `root` (the index's source root, canonical). A relative
  /// entry needs a root, and every entry must name a directory or file that exists —
  /// otherwise it is an error naming the entry, never a silent empty answer later. Paths
  /// are canonicalized because the build canonicalizes its root (`/tmp/...` must meet
  /// `/private/tmp/...`).
  pub fn resolve(entries: &[String], root: Option<&Path>) -> Result<Self, String> {
    Self::from_spec(
      &ScopeSpec {
        within: entries.to_vec(),
        ..ScopeSpec::default()
      },
      root,
      None,
    )
  }

  /// Resolve one entry list to absolute prefixes. `@file` / `@dir` / `@package` bind to
  /// `anchor` (the defining file of the symbol asked about) when one is given and are
  /// returned as deferred otherwise.
  fn resolve_entries(
    entries: &[String],
    root: Option<&Path>,
    anchor: Option<&str>,
    what: &str,
  ) -> Result<ResolvedEntries, String> {
    let mut prefixes = Vec::with_capacity(entries.len());
    let mut spelled_entries = Vec::with_capacity(entries.len());
    let mut deferred = Vec::new();
    for raw in entries {
      let entry = raw.trim();
      if entry.is_empty() {
        return Err(format!("{what} entry is empty"));
      }
      let lexical: PathBuf = if let Some(relative) = entry.strip_prefix('@') {
        let Some(anchor) = anchor else {
          deferred.push(entry.to_string());
          continue;
        };
        let file = Path::new(anchor);
        let dir = file.parent().unwrap_or(file);
        match relative {
          "file" => file.to_path_buf(),
          "dir" => dir.to_path_buf(),
          "package" => package_root(dir, root).ok_or_else(|| {
            format!("scope entry '@package': no package manifest above {}", dir.display())
          })?,
          other => {
            return Err(format!(
              "unknown anchor-relative scope entry '@{other}' (one of: @file, @dir, @package)"
            ));
          }
        }
      } else if Path::new(entry).is_absolute() {
        PathBuf::from(entry)
      } else {
        let Some(root) = root else {
          return Err(format!(
            "{what} entry '{entry}' is relative and this index has no source root — pass an \
             absolute path"
          ));
        };
        root.join(entry)
      };
      let resolved = lexical.canonicalize().map_err(|_| match root {
        Some(root) if !Path::new(entry).is_absolute() => {
          format!("{what} entry '{entry}' names nothing under {}", root.display())
        }
        _ => format!("{what} entry '{entry}' names nothing"),
      })?;
      let mut spelled = resolved.to_string_lossy().into_owned();
      while spelled.len() > 1 && spelled.ends_with('/') {
        spelled.pop();
      }
      if !prefixes.contains(&spelled) {
        prefixes.push(spelled);
        spelled_entries.push(entry.to_string());
      }
    }
    Ok((spelled_entries, prefixes, deferred))
  }

  /// Resolve a full spec against `root`, binding anchor-relative entries to `anchor`
  /// (a symbol's defining file) when given. Facets are validated here so a typo is an
  /// error at the call that made it. `changed_since` runs git once, now.
  pub fn from_spec(spec: &ScopeSpec, root: Option<&Path>, anchor: Option<&str>) -> Result<Self, String> {
    let root = root.map(|r| r.canonicalize().unwrap_or_else(|_| r.to_path_buf()));
    let (within, prefixes, deferred) = Self::resolve_entries(&spec.within, root.as_deref(), anchor, "scope")?;
    let (except, excludes, deferred_except) =
      Self::resolve_entries(&spec.except, root.as_deref(), anchor, "except")?;
    if !deferred_except.is_empty() {
      return Err("anchor-relative entries (@file, @dir, @package) are for `within`, not `except`".to_string());
    }
    let class_set = spec.classes.iter().map(|c| parse_class(c)).collect::<Result<Vec<_>, _>>()?;
    let kind_set = match spec.kind.as_deref() {
      Some(text) => Some(
        vorpal_kg::SymbolKind::parse(text).ok_or_else(|| format!("unknown symbol kind '{text}'"))?,
      ),
      None => None,
    };
    let lang_set = match spec.lang.as_deref() {
      Some(text) => Some(
        vorpal_ingest::canonical_language(text).ok_or_else(|| format!("unknown language '{text}'"))?,
      ),
      None => None,
    };
    let (changed, changed_files) = match spec.changed_since.as_deref() {
      Some(reference) => {
        let Some(root) = &root else {
          return Err("changed_since needs a source root (a default-layout index)".to_string());
        };
        let since = (reference != "worktree").then_some(reference);
        // git reports an untracked directory as one entry: expand it to its files, and
        // never count the index's own directory (or git's) as a change.
        let mut paths: Vec<String> = Vec::new();
        for rel in crate::impact::changed_paths(root, since)? {
          let abs = root.join(rel.trim_end_matches('/'));
          if abs.is_dir() {
            push_files_under(&abs, &mut paths);
          } else {
            paths.push(abs.to_string_lossy().into_owned());
          }
        }
        paths.sort();
        paths.dedup();
        let count = paths.len();
        (Some(paths), Some(count))
      }
      None => (None, None),
    };
    Ok(Self {
      within,
      except,
      classes: spec.classes.clone(),
      kind: spec.kind.clone(),
      lang: spec.lang.clone(),
      exported: spec.exported,
      changed_since: spec.changed_since.clone(),
      changed_files,
      prefixes,
      excludes,
      deferred,
      class_set,
      kind_set,
      lang_set,
      changed,
    })
  }

  /// The spec this scope was resolved from (for re-binding to a new anchor).
  pub fn spec(&self) -> ScopeSpec {
    ScopeSpec {
      within: self.within.iter().cloned().chain(self.deferred.iter().cloned()).collect(),
      except: self.except.clone(),
      classes: self.classes.clone(),
      kind: self.kind.clone(),
      lang: self.lang.clone(),
      exported: self.exported,
      changed_since: self.changed_since.clone(),
    }
  }

  /// Anchor-relative entries still waiting for a symbol (`@file`, `@dir`, `@package`).
  pub fn deferred(&self) -> &[String] {
    &self.deferred
  }

  /// Bind the deferred entries to `anchor`, keeping every other facet.
  pub fn bind(&self, root: Option<&Path>, anchor: &str) -> Result<Self, String> {
    if self.deferred.is_empty() {
      return Ok(self.clone());
    }
    let mut bound = Self::from_spec(&self.spec(), root, Some(anchor))?;
    // `changed_since` was resolved when the scope was set; keep that answer rather than
    // running git again per call.
    bound.changed = self.changed.clone();
    bound.changed_files = self.changed_files;
    Ok(bound)
  }

  /// Facets the semantic tier cannot generate candidates for (class, language, changed
  /// set, kind, visibility): a query with any of them keeps its overfetch.
  pub fn has_facets_beyond_prefixes(&self) -> bool {
    !self.class_set.is_empty()
      || self.lang_set.is_some()
      || self.changed.is_some()
      || self.kind_set.is_some()
      || self.exported.is_some()
  }

  /// Row admission for a node: the path facets plus kind and visibility.
  pub fn admits_node(&self, path: &str, kind: vorpal_kg::SymbolKind, exported: bool) -> bool {
    if self.kind_set.is_some_and(|k| k != kind) {
      return false;
    }
    if self.exported.is_some_and(|want| want != exported) {
      return false;
    }
    self.admits(path)
  }

  /// [`PathScope::resolve`] with exclusions: rows under an `except` prefix are dropped even
  /// when an include admits them. An empty `entries` with excludes means "everything but".
  pub fn resolve_with_except(
    entries: &[String],
    except: &[String],
    root: Option<&Path>,
  ) -> Result<Self, String> {
    Self::from_spec(
      &ScopeSpec {
        within: entries.to_vec(),
        except: except.to_vec(),
        ..ScopeSpec::default()
      },
      root,
      None,
    )
  }

  /// The absolute prefixes this scope excludes.
  pub fn excludes(&self) -> &[String] {
    &self.excludes
  }

  /// Nothing set at all: admits everything (the unscoped view). A scope whose only
  /// entries are still deferred is NOT empty — it must be bound before use.
  pub fn is_empty(&self) -> bool {
    self.prefixes.is_empty()
      && self.excludes.is_empty()
      && self.deferred.is_empty()
      && !self.has_facets_beyond_prefixes()
  }

  /// The absolute prefixes this scope resolved to.
  pub fn prefixes(&self) -> &[String] {
    &self.prefixes
  }

  /// File-level admission: segment-exact prefix test (`path` is a `within` prefix or lies
  /// below one, and under no `except` prefix), then the path facets (class, language,
  /// changed set).
  pub fn admits(&self, path: &str) -> bool {
    if self.excludes.iter().any(|prefix| under(path, prefix)) {
      return false;
    }
    if !(self.prefixes.is_empty() || self.prefixes.iter().any(|prefix| under(path, prefix))) {
      return false;
    }
    if !self.class_set.is_empty() && !self.class_set.contains(&crate::path_class(path)) {
      return false;
    }
    if let Some(lang) = &self.lang_set
      && vorpal_ingest::language_name_of(path).as_deref() != Some(lang.as_str())
    {
      return false;
    }
    if let Some(changed) = &self.changed
      && changed.binary_search_by(|p| p.as_str().cmp(path)).is_err()
    {
      return false;
    }
    true
  }

  /// Keep the rows the scope admits; return them with the count of rows it excluded.
  pub fn split<T>(&self, rows: Vec<T>, path_of: impl Fn(&T) -> &str) -> (Vec<T>, usize) {
    if self.is_empty() {
      return (rows, 0);
    }
    let before = rows.len();
    let kept: Vec<T> = rows.into_iter().filter(|row| self.admits(path_of(row))).collect();
    let outside = before - kept.len();
    (kept, outside)
  }
}

/// The dense node-id ranges a path scope covers in one generation, sorted and disjoint.
///
/// Dense ids are contiguous per file and files are path-sorted within each bucket (the
/// pack's canonical v2 order), so one directory prefix is at most one contiguous run of
/// ids per bucket — a handful of ranges for the whole scope, and an exact row count. The
/// semantic and body channels use these to generate candidates inside the scope instead
/// of overfetching repository-wide and filtering afterwards.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeRanges {
  ranges: Vec<Range<u64>>,
  rows: u64,
}

impl ScopeRanges {
  fn from_ranges(mut ranges: Vec<Range<u64>>) -> Self {
    ranges.retain(|r| r.end > r.start);
    ranges.sort_by_key(|r| (r.start, r.end));
    let mut merged: Vec<Range<u64>> = Vec::with_capacity(ranges.len());
    for r in ranges {
      match merged.last_mut() {
        Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
        _ => merged.push(r),
      }
    }
    let rows = merged.iter().map(|r| r.end - r.start).sum();
    Self { ranges: merged, rows }
  }

  /// One contiguous id range (a calibration probe, or a whole-tier view).
  pub fn single(range: Range<u64>) -> Self {
    Self::from_ranges(vec![range])
  }

  /// Rows (dense ids) inside the scope — the population a candidate generator must cover.
  pub fn rows(&self) -> u64 {
    self.rows
  }

  pub fn ranges(&self) -> &[Range<u64>] {
    &self.ranges
  }

  pub fn is_empty(&self) -> bool {
    self.rows == 0
  }

  /// Membership by binary search over the sorted disjoint ranges.
  pub fn contains(&self, id: u64) -> bool {
    let i = self.ranges.partition_point(|r| r.end <= id);
    self.ranges.get(i).is_some_and(|r| r.start <= id)
  }
}

/// One generation's file table: each file's path and dense-id block, plus a path-sorted
/// permutation for prefix lookups. Built once per generation and cached by the searcher;
/// a scope resolves to ranges with one binary search per entry and one push per file it
/// covers. Nothing here assumes the dense layout is path-sorted: when it is (the pack's
/// canonical order within a bucket), adjacent files merge into a few runs; when it is
/// not, the ranges are simply more numerous and still exact.
#[derive(Debug)]
pub struct FileTable {
  paths: Vec<String>,
  starts: Vec<u64>,
  ends: Vec<u64>,
  /// File indices sorted by path.
  order: Vec<u32>,
}

impl FileTable {
  pub fn build(kg: &Kg) -> Self {
    let file_tag = vorpal_kg::SymbolKind::File.tag();
    let mut paths = Vec::new();
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    // File rows open blocks: the block of file `i` runs to the next file row, or to the
    // stripe's end (one stripe per bucket, one for a flat graph).
    let stripes: Vec<(Vec<u64>, u64)> = match kg.kind_tag_stripes() {
      Some(stripes) => stripes
        .into_iter()
        .map(|(base, tags)| {
          let rows: Vec<u64> = tags
            .iter()
            .enumerate()
            .filter(|(_, tag)| **tag == file_tag)
            .map(|(i, _)| base + i as u64)
            .collect();
          (rows, base + tags.len() as u64)
        })
        .collect(),
      None => {
        let count = kg.node_count() as u64;
        let rows: Vec<u64> = (0..count)
          .filter(|&id| kg.node_kind(NodeId::new(id)) == Some(vorpal_kg::SymbolKind::File))
          .collect();
        vec![(rows, count)]
      }
    };
    for (rows, stripe_end) in stripes {
      for (i, &row) in rows.iter().enumerate() {
        let path = kg
          .node(NodeId::new(row))
          .map(|view| view.path.to_string())
          .unwrap_or_default();
        paths.push(path);
        starts.push(row);
        ends.push(rows.get(i + 1).copied().unwrap_or(stripe_end));
      }
    }
    Self::from_parts(paths, starts, ends)
  }

  fn from_parts(paths: Vec<String>, starts: Vec<u64>, ends: Vec<u64>) -> Self {
    let mut order: Vec<u32> = (0..paths.len() as u32).collect();
    order.sort_by(|&a, &b| paths[a as usize].cmp(&paths[b as usize]));
    Self {
      paths,
      starts,
      ends,
      order,
    }
  }

  /// A table from explicit rows — tests only (a real table comes from [`FileTable::build`]).
  #[cfg(test)]
  fn from_rows(rows: &[(&str, u64, u64)]) -> Self {
    Self::from_parts(
      rows.iter().map(|(p, _, _)| (*p).to_string()).collect(),
      rows.iter().map(|(_, s, _)| *s).collect(),
      rows.iter().map(|(_, _, e)| *e).collect(),
    )
  }

  pub fn file_count(&self) -> usize {
    self.paths.len()
  }

  /// The dense-id blocks of the files under one absolute prefix (segment-exact: the entry
  /// itself when it is a file, and everything below `prefix/`).
  fn runs_under(&self, prefix: &str, out: &mut Vec<Range<u64>>) {
    let below = format!("{prefix}/");
    let past = format!("{prefix}0");
    let exact_lo = self.order.partition_point(|&f| self.paths[f as usize].as_str() < prefix);
    let exact_hi = self.order.partition_point(|&f| self.paths[f as usize].as_str() <= prefix);
    let lo = self.order.partition_point(|&f| self.paths[f as usize].as_str() < below.as_str());
    let hi = self.order.partition_point(|&f| self.paths[f as usize].as_str() < past.as_str());
    for i in (exact_lo..exact_hi).chain(lo..hi) {
      let file = self.order[i] as usize;
      out.push(self.starts[file]..self.ends[file]);
    }
  }

  /// The ranges `scope` covers: the union of its include prefixes minus its excludes.
  pub fn ranges_for(&self, scope: &PathScope) -> ScopeRanges {
    let mut include = Vec::new();
    if scope.prefixes().is_empty() {
      // Excludes only: everything, minus them.
      for file in 0..self.paths.len() {
        include.push(self.starts[file]..self.ends[file]);
      }
    } else {
      for prefix in scope.prefixes() {
        self.runs_under(prefix, &mut include);
      }
    }
    let included = ScopeRanges::from_ranges(include);
    if scope.excludes().is_empty() {
      return included;
    }
    let mut exclude = Vec::new();
    for prefix in scope.excludes() {
      self.runs_under(prefix, &mut exclude);
    }
    let excluded = ScopeRanges::from_ranges(exclude);
    let mut out = Vec::new();
    for r in included.ranges() {
      let mut cursor = r.start;
      for x in excluded.ranges() {
        if x.end <= cursor {
          continue;
        }
        if x.start >= r.end {
          break;
        }
        if x.start > cursor {
          out.push(cursor..x.start);
        }
        cursor = cursor.max(x.end);
      }
      if cursor < r.end {
        out.push(cursor..r.end);
      }
    }
    ScopeRanges::from_ranges(out)
  }
}

/// Every regular file under `dir`, skipping the index's and git's own directories.
fn push_files_under(dir: &Path, out: &mut Vec<String>) {
  let skipped = |path: &Path| path.file_name().is_some_and(|name| name == ".vorpal" || name == ".git");
  if skipped(dir) {
    return;
  }
  let mut stack = vec![dir.to_path_buf()];
  while let Some(dir) = stack.pop() {
    let Ok(entries) = std::fs::read_dir(&dir) else { continue };
    for entry in entries.flatten() {
      let path = entry.path();
      if skipped(&path) {
        continue;
      }
      if path.is_dir() {
        stack.push(path);
      } else if path.is_file() {
        out.push(path.to_string_lossy().into_owned());
      }
    }
  }
}

/// The nearest directory at or above `dir` (never above `root`) holding a package manifest.
fn package_root(dir: &Path, root: Option<&Path>) -> Option<PathBuf> {
  let mut at = Some(dir);
  while let Some(candidate) = at {
    if PACKAGE_MANIFESTS.iter().any(|name| candidate.join(name).is_file()) {
      return Some(candidate.to_path_buf());
    }
    if root.is_some_and(|root| candidate == root) {
      break;
    }
    at = candidate.parent();
  }
  None
}

/// `path` is `prefix` itself or lies below `prefix/`.
fn under(path: &str, prefix: &str) -> bool {
  path.len() >= prefix.len()
    && path.starts_with(prefix)
    && (path.len() == prefix.len() || path.as_bytes()[prefix.len()] == b'/')
}

/// The number of leading directory segments `path` shares with `anchor_dir` — the path
/// proximity used to order a symbol's neighbours nearest-first (same file, then same
/// directory, then the longest shared ancestor).
pub fn shared_dir_segments(path: &str, anchor_dir: &str) -> usize {
  let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
  if dir.starts_with('/') != anchor_dir.starts_with('/') {
    return 0;
  }
  dir
    .trim_start_matches('/')
    .split('/')
    .zip(anchor_dir.trim_start_matches('/').split('/'))
    .take_while(|(a, b)| a == b)
    .count()
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A real tree, because entries must exist: `<base>/fs/read_write.c`, `<base>/fsnotify/`,
  /// `<base>/mm/slab.c`, `<base>/mm/slab.h`.
  fn tree(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("vorpal-scope-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for dir in ["fs", "fsnotify", "mm"] {
      std::fs::create_dir_all(base.join(dir)).unwrap();
    }
    for file in ["fs/read_write.c", "fsnotify/mark.c", "mm/slab.c", "mm/slab.h"] {
      std::fs::write(base.join(file), "").unwrap();
    }
    base.canonicalize().unwrap()
  }

  #[test]
  fn admits_is_segment_exact_and_files_admit_themselves() {
    let base = tree("admits");
    let scope = PathScope::resolve(&["fs".to_string(), "mm/slab.c".to_string()], Some(&base)).unwrap();
    let at = |rel: &str| base.join(rel).to_string_lossy().into_owned();
    assert!(scope.admits(&at("fs/read_write.c")));
    assert!(scope.admits(&at("fs")));
    assert!(!scope.admits(&at("fsnotify/mark.c")));
    assert!(scope.admits(&at("mm/slab.c")));
    assert!(!scope.admits(&at("mm/slab.h")));
    assert!(!scope.admits("/other/fs/x.c"));
    let _ = std::fs::remove_dir_all(&base);
  }

  #[test]
  fn entries_must_exist_relative_ones_need_a_root_and_trailing_slashes_do_not_matter() {
    let base = tree("resolve");
    assert!(PathScope::resolve(&["fs/".to_string()], None).is_err());
    assert!(PathScope::resolve(&["  ".to_string()], Some(&base)).is_err());
    let missing = PathScope::resolve(&["nope".to_string()], Some(&base)).unwrap_err();
    assert!(missing.contains("names nothing under"), "{missing}");
    assert!(PathScope::resolve(&["/definitely/not/here".to_string()], None).is_err());
    let a = PathScope::resolve(&["fs/".to_string()], Some(&base)).unwrap();
    let b = PathScope::resolve(&["fs".to_string()], Some(&base)).unwrap();
    assert_eq!(a.prefixes(), b.prefixes());
    assert_eq!(a.within, vec!["fs/".to_string()]);
    let abs = PathScope::resolve(&[base.join("mm").to_string_lossy().into_owned()], None).unwrap();
    assert!(abs.admits(&base.join("mm/slab.c").to_string_lossy()));
    let _ = std::fs::remove_dir_all(&base);
  }

  #[test]
  fn split_counts_the_excluded_rows() {
    let base = tree("split");
    let scope = PathScope::resolve(&["fs".to_string()], Some(&base)).unwrap();
    let at = |rel: &str| base.join(rel).to_string_lossy().into_owned();
    let rows = vec![at("fs/read_write.c"), at("mm/slab.c"), at("fs/sub/c.c")];
    let (kept, outside) = scope.split(rows, |p| p.as_str());
    assert_eq!(kept, vec![at("fs/read_write.c"), at("fs/sub/c.c")]);
    assert_eq!(outside, 1);
    let empty = PathScope::default();
    let (kept, outside) = empty.split(vec!["/x"], |p| p);
    assert_eq!((kept, outside), (vec!["/x"], 0));
    let _ = std::fs::remove_dir_all(&base);
  }

  #[test]
  fn scope_ranges_merge_contain_and_count() {
    let r = ScopeRanges::from_ranges(vec![10..20, 5..12, 30..31, 31..40, 50..50]);
    assert_eq!(r.ranges(), &[5..20, 30..40]);
    assert_eq!(r.rows(), 25);
    assert!(r.contains(5) && r.contains(19) && !r.contains(20) && r.contains(39) && !r.contains(40));
    assert!(!r.contains(0) && !r.contains(45));
    assert!(ScopeRanges::default().is_empty());
  }

  fn synthetic(within: &[&str], except: &[&str]) -> PathScope {
    PathScope {
      within: within.iter().map(|s| s.to_string()).collect(),
      except: except.iter().map(|s| s.to_string()).collect(),
      prefixes: within.iter().map(|s| format!("/r/{s}")).collect(),
      excludes: except.iter().map(|s| format!("/r/{s}")).collect(),
      ..PathScope::default()
    }
  }

  #[test]
  fn file_table_maps_a_scope_to_per_bucket_runs_minus_excludes() {
    // Two buckets, path-sorted within each; ids contiguous per file, file node first.
    // Two path-sorted buckets back to back — and the table must not care either way.
    let table = FileTable::from_rows(&[
      ("/r/fs/a.c", 0, 10),
      ("/r/fs/ext4/b.c", 10, 25),
      ("/r/fsnotify/m.c", 25, 30),
      ("/r/mm/slab.c", 30, 50),
      ("/r/fs/c.c", 50, 60),
      ("/r/fs/ext4/d.c", 60, 70),
      ("/r/mm/slab.h", 70, 80),
    ]);
    let ranges = table.ranges_for(&synthetic(&["fs"], &[]));
    assert_eq!(ranges.ranges(), &[0..25, 50..70], "{ranges:?}");
    assert_eq!(ranges.rows(), 45);
    let ranges = table.ranges_for(&synthetic(&["fs"], &["fs/ext4"]));
    assert_eq!(ranges.ranges(), &[0..10, 50..60], "{ranges:?}");
    let slab = 30..50;
    assert_eq!(table.ranges_for(&synthetic(&["mm/slab.c"], &[])).ranges(), std::slice::from_ref(&slab));
    assert!(table.ranges_for(&synthetic(&["zz"], &[])).is_empty());
    assert_eq!(table.ranges_for(&synthetic(&["fs", "mm"], &[])).rows(), 75);
    // Excludes only: everything but.
    assert_eq!(table.ranges_for(&synthetic(&[], &["mm"])).ranges(), &[0..30, 50..70]);
    // An unsorted layout yields more, still exact, runs.
    let shuffled = FileTable::from_rows(&[
      ("/r/mm/slab.c", 0, 5),
      ("/r/fs/a.c", 5, 9),
      ("/r/x/y.c", 9, 12),
      ("/r/fs/b.c", 12, 20),
    ]);
    assert_eq!(shuffled.ranges_for(&synthetic(&["fs"], &[])).ranges(), &[5..9, 12..20]);
  }

  #[test]
  fn proximity_counts_shared_leading_directories() {
    assert_eq!(shared_dir_segments("/r/fs/a.c", "/r/fs"), 2);
    assert_eq!(shared_dir_segments("/r/fs/sub/a.c", "/r/fs"), 2);
    assert_eq!(shared_dir_segments("/r/mm/a.c", "/r/fs"), 1);
    assert_eq!(shared_dir_segments("/x/a.c", "/r/fs"), 0);
    assert_eq!(shared_dir_segments("a.c", "/r/fs"), 0);
  }
}

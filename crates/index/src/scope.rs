//! Path scope: the caller's working radius as a set of directory (or file) prefixes.
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

use std::path::{Path, PathBuf};

use serde::Serialize;

/// A resolved scope: the entries as given (echoed back on every scoped answer) and the
/// absolute prefixes they resolve to, trailing slashes removed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PathScope {
  pub within: Vec<String>,
  #[serde(skip)]
  prefixes: Vec<String>,
}

impl PathScope {
  /// Resolve `entries` against `root` (the index's source root, canonical). A relative
  /// entry needs a root, and every entry must name a directory or file that exists —
  /// otherwise it is an error naming the entry, never a silent empty answer later. Paths
  /// are canonicalized because the build canonicalizes its root (`/tmp/...` must meet
  /// `/private/tmp/...`).
  pub fn resolve(entries: &[String], root: Option<&Path>) -> Result<Self, String> {
    let root = root.map(|r| r.canonicalize().unwrap_or_else(|_| r.to_path_buf()));
    let mut prefixes = Vec::with_capacity(entries.len());
    let mut within = Vec::with_capacity(entries.len());
    for raw in entries {
      let entry = raw.trim();
      if entry.is_empty() {
        return Err("scope entry is empty".to_string());
      }
      let lexical: PathBuf = if Path::new(entry).is_absolute() {
        PathBuf::from(entry)
      } else {
        let Some(root) = &root else {
          return Err(format!(
            "scope entry '{entry}' is relative and this index has no source root — pass an \
             absolute path"
          ));
        };
        root.join(entry)
      };
      let resolved = lexical.canonicalize().map_err(|_| match &root {
        Some(root) if !Path::new(entry).is_absolute() => {
          format!("scope entry '{entry}' names nothing under {}", root.display())
        }
        _ => format!("scope entry '{entry}' names nothing"),
      })?;
      let mut spelled = resolved.to_string_lossy().into_owned();
      while spelled.len() > 1 && spelled.ends_with('/') {
        spelled.pop();
      }
      if !prefixes.contains(&spelled) {
        prefixes.push(spelled);
        within.push(entry.to_string());
      }
    }
    Ok(Self { within, prefixes })
  }

  /// No entries: admits everything (the unscoped view).
  pub fn is_empty(&self) -> bool {
    self.prefixes.is_empty()
  }

  /// The absolute prefixes this scope resolved to.
  pub fn prefixes(&self) -> &[String] {
    &self.prefixes
  }

  /// Segment-exact prefix test: `path` is the prefix itself or lies below it.
  pub fn admits(&self, path: &str) -> bool {
    if self.prefixes.is_empty() {
      return true;
    }
    self.prefixes.iter().any(|prefix| {
      path.len() >= prefix.len()
        && path.starts_with(prefix.as_str())
        && (path.len() == prefix.len() || path.as_bytes()[prefix.len()] == b'/')
    })
  }

  /// Keep the rows the scope admits; return them with the count of rows it excluded.
  pub fn split<T>(&self, rows: Vec<T>, path_of: impl Fn(&T) -> &str) -> (Vec<T>, usize) {
    if self.prefixes.is_empty() {
      return (rows, 0);
    }
    let before = rows.len();
    let kept: Vec<T> = rows.into_iter().filter(|row| self.admits(path_of(row))).collect();
    let outside = before - kept.len();
    (kept, outside)
  }
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
  fn proximity_counts_shared_leading_directories() {
    assert_eq!(shared_dir_segments("/r/fs/a.c", "/r/fs"), 2);
    assert_eq!(shared_dir_segments("/r/fs/sub/a.c", "/r/fs"), 2);
    assert_eq!(shared_dir_segments("/r/mm/a.c", "/r/fs"), 1);
    assert_eq!(shared_dir_segments("/x/a.c", "/r/fs"), 0);
    assert_eq!(shared_dir_segments("a.c", "/r/fs"), 0);
  }
}

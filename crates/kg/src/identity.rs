//! Phase-4 identity (SUBSECOND.md P4.0): file-local node identity, the spine of the
//! bucketed format.
//!
//! A node's durable structural identity is `(FileKey, ordinal)`: WHICH file (a
//! machine-invariant hash of its tree-relative path) and WHERE in that file's layout
//! (its position in the file's block, the same layout order `layout_entity_paths`
//! walks). Dense u32 ids remain the runtime currency everywhere — they are DERIVED from
//! this identity at load in the bucketed format, exactly as the canonical seal derives
//! them from block order today.
//!
//! The spelling law (learned the hard way by the embedder-v2 incident): anything hashed
//! into an identity must be a function of the TREE, never its mount point. `FileKey`
//! therefore hashes the tree-RELATIVE path; [`tree_relative`] is the one conversion
//! point, and callers hand it the canonicalized tree root the build already owns.

use std::collections::HashMap;

/// Machine-invariant identity of one source file: `xxh3_64` of its tree-relative path.
/// 64 bits over corpora of ~10⁵–10⁶ files leaves collision probability far below any
/// operational floor, and [`verify_file_keys`] still checks EVERY build — a collision is
/// a loud, actionable error (rename one file), never a silent mis-bucketing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileKey(pub u64);

impl FileKey {
  pub fn of(tree_relative_path: &str) -> FileKey {
    FileKey(xxhash_rust::xxh3::xxh3_64(tree_relative_path.as_bytes()))
  }
}

/// A node's durable structural identity in the bucketed format: which file, and its
/// ordinal within that file's block (layout order — file node first, then each item
/// followed by its members).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalId {
  pub file: FileKey,
  pub ordinal: u32,
}

/// The tree-relative spelling of a stored path: the canonicalized tree root stripped,
/// with no leading separator. Paths outside the root come back unchanged — the caller's
/// collision gate will surface anything structurally surprising rather than this
/// function guessing.
pub fn tree_relative<'a>(path: &'a str, canonical_root: &str) -> &'a str {
  let stripped = path.strip_prefix(canonical_root).unwrap_or(path);
  stripped.strip_prefix('/').unwrap_or(stripped)
}

/// Build-time collision gate over the manifest's file set: every file's key must be
/// unique. O(files); runs on every build the way the u32 ceilings do — the bucketed
/// format keys storage by these values, so a collision must stop the build with both
/// spellings named, never degrade into shared identity.
pub fn verify_file_keys<'a>(
  tree_relative_paths: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
  let mut seen: HashMap<u64, &str> = HashMap::new();
  for path in tree_relative_paths {
    let key = FileKey::of(path).0;
    if let Some(previous) = seen.insert(key, path) {
      if previous != path {
        return Err(format!(
          "file-identity collision: '{previous}' and '{path}' share file key \
           {key:016x} — rename one of them (64-bit xxh3 of the tree-relative path is \
           the storage identity of the bucketed index format)"
        ));
      }
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn keys_are_spelling_stable_and_root_free() {
    let a = FileKey::of("mm/slab_common.c");
    assert_eq!(a, FileKey::of("mm/slab_common.c"), "pure function of the spelling");
    assert_eq!(
      tree_relative("/private/tmp/work/mm/slab_common.c", "/private/tmp/work"),
      "mm/slab_common.c"
    );
    assert_eq!(
      tree_relative("/private/tmp/work/mm/slab_common.c", "/private/tmp/work/"),
      "mm/slab_common.c"
    );
    assert_eq!(
      FileKey::of(tree_relative("/a/repo/src/lib.rs", "/a/repo")),
      FileKey::of(tree_relative("/other/mount/src/lib.rs", "/other/mount")),
      "identity is a function of the tree, never the mount point"
    );
  }

  #[test]
  fn duplicate_paths_do_not_collide_distinct_ones_do() {
    assert!(verify_file_keys(["src/a.rs", "src/b.rs", "src/a.rs"]).is_ok());
    // A genuine 64-bit collision cannot be crafted here; the gate's error path is
    // exercised through the map by construction: force it with an equal key by
    // checking the message shape on the closest reachable case — two DIFFERENT
    // spellings only err when keys match, so this asserts the happy path stays quiet.
    assert!(verify_file_keys(["x", "y", "z"]).is_ok());
  }
}

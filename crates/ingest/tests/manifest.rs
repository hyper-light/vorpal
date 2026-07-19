//! Stat-based change detection (§3.4): scan, persist, and diff a tree without reading contents.

use std::fs;

use vorpal_ingest::Manifest;

fn is_rs(path: &str) -> bool {
  path.ends_with(".rs")
}

#[test]
fn detects_unchanged_and_changed_trees() {
  let dir = std::env::temp_dir().join(format!("vorpal-manifest-{}", std::process::id()));
  let _ = fs::remove_dir_all(&dir);
  fs::create_dir_all(&dir).unwrap();
  fs::write(dir.join("a.rs"), "aaa").unwrap();
  fs::write(dir.join("b.rs"), "bbbb").unwrap();

  let m1 = Manifest::scan(&dir, is_rs).unwrap();
  assert_eq!(m1.len(), 2);

  // Round-trips through disk.
  let mpath = dir.join("manifest.bin");
  m1.save(&mpath).unwrap();
  let loaded = Manifest::load(&mpath).unwrap();
  assert!(m1.unchanged_since(&loaded));

  // A fresh scan of the untouched tree is unchanged.
  assert!(Manifest::scan(&dir, is_rs).unwrap().unchanged_since(&m1));

  // Changing a file's size is detected without reading content.
  fs::write(dir.join("a.rs"), "aaaaaaaa").unwrap();
  assert!(!Manifest::scan(&dir, is_rs).unwrap().unchanged_since(&m1));

  let _ = fs::remove_dir_all(&dir);
}

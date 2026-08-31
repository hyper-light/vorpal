//! Shareable index artifacts (D2): export → import round-trips a generation byte-for-byte
//! under trust-by-recomputation, and every corruption class is refused with nothing installed.

use std::fs;

use vorpal_index::artifact::{export_generation, import_generation};
use vorpal_index::build_index;
use vorpal_kg::Kg;

fn fixture(base: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
  let src = base.join("src");
  let idx = base.join("index");
  let _ = fs::remove_dir_all(base);
  fs::create_dir_all(&src).unwrap();
  fs::write(src.join("b.rs"), "pub fn target() -> i32 {\n    0\n}\n").unwrap();
  fs::write(
    src.join("a.rs"),
    "pub fn caller() -> i32 {\n    target()\n}\n",
  )
  .unwrap();
  build_index(&src, &idx).unwrap();
  (src, idx)
}

#[test]
fn export_import_round_trips_and_refuses_corruption() {
  let base = std::env::temp_dir().join(format!("vorpal-vidx-{}", std::process::id()));
  let (_src, idx) = fixture(&base);
  let exported_dir = vorpal_kg::resolve_index_dir(&idx);
  let exported_id = exported_dir.file_name().unwrap().to_string_lossy().into_owned();

  // Export.
  let vidx = base.join("gen.vidx");
  let report = export_generation(&idx, &vidx).expect("export succeeds");
  assert_eq!(report.content_id, exported_id);
  assert!(report.artifacts >= 6, "{}", report.artifacts);
  assert!(vidx.metadata().unwrap().len() > 0);

  // Deterministic export: same generation, same bytes.
  let vidx2 = base.join("gen2.vidx");
  export_generation(&idx, &vidx2).expect("second export");
  assert_eq!(
    fs::read(&vidx).unwrap(),
    fs::read(&vidx2).unwrap(),
    "identical generations must export byte-identically"
  );

  // Import into a fresh root: same binary, so the recomputed id matches the exporter's.
  let target_root = base.join("imported");
  let imported = import_generation(&vidx, &target_root).expect("import succeeds");
  assert_eq!(imported.installed_id, exported_id);
  assert_eq!(imported.exported_id, exported_id);
  assert!(imported.fold_note.is_none(), "{:?}", imported.fold_note);
  assert_eq!(
    fs::read_to_string(target_root.join("CURRENT")).unwrap().trim(),
    format!("gen/{exported_id}"),
    "CURRENT swapped atomically to the imported generation"
  );

  // The imported graph answers queries exactly like the original.
  let kg = Kg::load(&target_root).expect("imported index opens");
  let callers: Vec<String> = kg
    .callers_of("target")
    .into_iter()
    .filter_map(|id| kg.node(id).map(|v| v.name.to_string()))
    .collect();
  assert!(callers.contains(&"caller".to_string()), "{callers:?}");
  drop(kg);

  // Tamper: flip one byte in the middle — the import must refuse and install nothing.
  let mut bytes = fs::read(&vidx).unwrap();
  let mid = bytes.len() / 2;
  bytes[mid] ^= 0xff;
  let tampered = base.join("tampered.vidx");
  fs::write(&tampered, &bytes).unwrap();
  let fresh_root = base.join("tampered-root");
  let err = import_generation(&tampered, &fresh_root).expect_err("tamper refused");
  assert!(
    err.contains("verification") || err.contains("corrupt") || err.contains("zstd") || err.contains("archive"),
    "{err}"
  );
  assert!(
    !fresh_root.join("CURRENT").exists(),
    "a refused import must install nothing"
  );

  // Truncation: cut the file — refuse, nothing installed.
  let cut = base.join("truncated.vidx");
  fs::write(&cut, &fs::read(&vidx).unwrap()[..mid]).unwrap();
  let cut_root = base.join("cut-root");
  assert!(import_generation(&cut, &cut_root).is_err());
  assert!(!cut_root.join("CURRENT").exists());

  // Not-a-vidx: refuse with a clear message.
  let junk = base.join("junk.vidx");
  fs::write(&junk, b"not a vidx at all").unwrap();
  assert!(import_generation(&junk, &base.join("junk-root")).is_err());

  let _ = fs::remove_dir_all(&base);
}

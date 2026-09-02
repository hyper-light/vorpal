//! Regression: the vendored children-block cache must never over-bin a node-claimed
//! block. The fixture is the exact C++ header whose parse produced an exact-reserved
//! (non-class-shaped) children array big enough to skip the grow branch; before the
//! claim-time guard (`ts_children_node_block_ok`) the block re-entered a LARGER bin on
//! release and the next reuse overflowed it — heap corruption, then SIGSEGV inside
//! `ts_parser_parse` (found indexing llvm-project; rust-lang/rust crashed identically).
//! A crash here kills the test process — the assertion is the build completing at all,
//! plus the graph carrying the header's definitions.

use std::fs;

#[test]
fn cpp_claim_shape_header_indexes_without_corruption() {
  let base = std::env::temp_dir().join(format!("vorpal-claim-shape-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("src");
  fs::create_dir_all(&src).unwrap();
  fs::copy(
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claim-shape/selectiondag.h"),
    src.join("selectiondag.h"),
  )
  .unwrap();
  let out = base.join("idx");
  let report = vorpal_index::build_index(&src, &out).expect("index build survives the parse");
  assert_eq!(report.indexed, 1, "the header was parsed fresh");
  let _ = fs::remove_dir_all(&base);
}

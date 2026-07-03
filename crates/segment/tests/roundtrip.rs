//! End-to-end: build a `.vseg`, mmap it back through `vorpal-mem`, verify integrity, and do
//! O(1) HOT-column point lookups via the dense-id directory (§9.1, §9.2).

use vorpal_mem::{CorpusProbe, ResourcePolicy};
use vorpal_segment::{NodeId, Segment, SegmentBuilder, SegmentDirectory};

fn sample_segment(base: u64) -> Vec<u8> {
  let mut b = SegmentBuilder::new(base);
  b.add_u8("kind", &[1, 2, 3]).unwrap();
  b.add_u32("name_ref", &[10, 20, 30]).unwrap();
  b.add_u64("content_hash", &[0xAAAA_0000, 0xBBBB_1111, 0xCCCC_2222])
    .unwrap();
  // 3 rows of 4-byte opaque codes (e.g. quantized vector code).
  b.add_bytes("code", 4, &[0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2])
    .unwrap();
  assert_eq!(b.row_count(), Some(3));
  b.build().unwrap()
}

#[test]
fn build_verify_and_point_lookup_in_ram() {
  let seg = Segment::open_owned(sample_segment(1000)).unwrap();
  seg.verify().expect("fresh segment must verify");

  assert_eq!(seg.row_count(), 3);
  assert_eq!(seg.logical_id_base(), 1000);
  assert_eq!(seg.column_count(), 4);

  let kind = seg.column("kind").unwrap();
  assert_eq!(kind.get_u8(0), Some(1));
  assert_eq!(kind.get_u8(2), Some(3));
  assert_eq!(kind.get_u8(3), None);

  let name = seg.column("name_ref").unwrap();
  assert_eq!(name.get_u32(1), Some(20));
  assert_eq!(name.as_slice::<u32>().unwrap(), &[10, 20, 30]);

  let hash = seg.column("content_hash").unwrap();
  assert_eq!(hash.get_u64(2), Some(0xCCCC_2222));

  let code = seg.column("code").unwrap();
  assert_eq!(code.stride(), 4);
  assert_eq!(code.row_bytes(1), Some(&[1u8, 1, 1, 1][..]));

  assert!(seg.column("missing").is_none());

  // Dense id → row (§9.2).
  assert_eq!(seg.contains_id(NodeId(1000)), Some(0));
  assert_eq!(seg.contains_id(NodeId(1002)), Some(2));
  assert_eq!(seg.contains_id(NodeId(1003)), None);
  assert_eq!(seg.contains_id(NodeId(999)), None);
}

#[test]
fn round_trips_through_an_adaptive_mmap() {
  let policy = ResourcePolicy::probe(CorpusProbe::new(4_000, 3));
  let mut path = std::env::temp_dir();
  path.push(format!("vorpal-seg-{}.vseg", std::process::id()));

  SegmentBuilder::new(0)
    .add_u32("name_ref", &[7, 8, 9])
    .unwrap()
    .write_to(&path)
    .unwrap();

  let seg = Segment::open_file(&path, &policy).unwrap();
  seg.verify().expect("mmapped segment must verify");
  assert_eq!(seg.column("name_ref").unwrap().get_u32(2), Some(9));

  let _ = std::fs::remove_file(&path);
}

#[test]
fn detects_column_corruption() {
  let mut bytes = sample_segment(0);
  // Corrupt a byte inside a HOT stripe (well past the 4 KiB header, before the footer).
  let mid = bytes.len() / 2;
  bytes[mid] ^= 0xFF;
  let seg = Segment::open_owned(bytes).unwrap(); // still parses (framing intact)
  // The cheap per-column path pinpoints the corrupted column...
  match seg.verify_column_checksums() {
    Err(vorpal_segment::SegmentError::ColumnHash(_)) => {}
    other => panic!("expected ColumnHash, got {other:?}"),
  }
  // ...and the full segment blake3 also catches it (it covers all column bytes).
  assert!(matches!(
    seg.verify(),
    Err(vorpal_segment::SegmentError::SegmentHash)
  ));
}

#[test]
fn detects_header_corruption() {
  let mut bytes = sample_segment(0);
  bytes[12] ^= 0x01; // flags byte: covered by the header blake3, not a parsed bound
  let seg = Segment::open_owned(bytes).unwrap();
  assert!(matches!(
    seg.verify(),
    Err(vorpal_segment::SegmentError::HeaderHash)
  ));
}

#[test]
fn builder_rejects_inconsistent_input() {
  assert!(SegmentBuilder::new(0).build().is_err(), "empty segment");

  let mut b = SegmentBuilder::new(0);
  b.add_u32("a", &[1, 2, 3]).unwrap();
  assert!(b.add_u32("b", &[1, 2]).is_err(), "row count mismatch");

  assert!(
    SegmentBuilder::new(0)
      .add_bytes("c", 4, &[0, 0, 0])
      .is_err(),
    "ragged bytes column"
  );
}

#[test]
fn directory_and_segments_compose() {
  // Two segments; a directory resolves ids to the right (segment, row).
  let s0 = Segment::open_owned(sample_segment(0)).unwrap();
  let s1 = Segment::open_owned(sample_segment(3)).unwrap();

  let mut dir = SegmentDirectory::new();
  dir.insert(s0.logical_id_base(), s0.row_count(), 0);
  dir.insert(s1.logical_id_base(), s1.row_count(), 1);
  assert_eq!(dir.next_id_base(), 6);

  let segs = [&s0, &s1];
  for (id, want_seg, want_row) in [(0u64, 0u32, 0u64), (2, 0, 2), (3, 1, 0), (5, 1, 2)] {
    let (seg_id, row) = dir.locate(NodeId(id)).unwrap();
    assert_eq!((seg_id, row), (want_seg, want_row));
    // Cross-check: the resolved segment agrees.
    assert_eq!(segs[seg_id as usize].contains_id(NodeId(id)), Some(row));
  }
  assert_eq!(dir.locate(NodeId(6)), None);
}

//! Warm-time semantic-engine calibration (`ann.calib`): the mid-range beam→scan routing
//! crossover is LEARNED from the ingested index on the running machine — never a
//! shipped constant. Pinned here: the sidecar's lifecycle (written by warm, exactly 32
//! stamped+checksummed bytes; torn/stale reads as absent) and routing NEUTRALITY — on
//! an exact tier the calibration steers latency only, never results, so forging any
//! crossover value must leave rankings bit-identical.

use std::fs;

use vorpal_index::{SearchFilter, Searcher, build_index, warm_ann};

fn fingerprint(searcher: &Searcher, query: &str) -> Vec<(String, u32)> {
  searcher
    .records(query, 10, &SearchFilter::default())
    .unwrap()
    .into_iter()
    .map(|h| (h.node.name, h.score.to_bits()))
    .collect()
}

#[test]
fn calibration_lifecycle_and_routing_neutrality() {
  unsafe { std::env::set_var("VORPAL_NO_AUTOWARM", "1") };
  let base = std::env::temp_dir().join("vorpal-calibration");
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  for i in 0..12 {
    fs::write(
      src.join(format!("m{i}.rs")),
      format!("pub fn alpha_{i}() -> u32 {{ {i} }}\npub fn beta_{i}() -> u32 {{ {i} }}\n"),
    )
    .unwrap();
  }
  build_index(&src, &out).unwrap();
  warm_ann(&out).unwrap();

  let gen_dir = vorpal_kg::resolve_index_dir(&out);
  let calib_path = gen_dir.join("ann.calib");
  let original = fs::read(&calib_path).expect("warm writes ann.calib");
  assert_eq!(original.len(), 32, "VCAL v1 is exactly 32 bytes");
  assert_eq!(&original[0..4], b"VCAL");

  // Baseline ranking under the measured calibration.
  let baseline = fingerprint(&Searcher::open(&out).unwrap(), "alpha");
  assert!(!baseline.is_empty());

  // Forge crossover = 1 (every fetch routes to the exact scan): identical results —
  // calibration is a latency decision, never a semantics one.
  let mut forged = original[0..16].to_vec();
  forged.extend_from_slice(&1u64.to_le_bytes());
  let checksum = xxhash_rust::xxh3::xxh3_64(&forged);
  forged.extend_from_slice(&checksum.to_le_bytes());
  fs::write(&calib_path, &forged).unwrap();
  assert_eq!(fingerprint(&Searcher::open(&out).unwrap(), "alpha"), baseline);

  // Stale stamp (valid checksum, wrong generation): reads as absent → structural floor,
  // and results are still identical.
  let mut stale = original[0..8].to_vec();
  let mut stamp = [0u8; 8];
  stamp.copy_from_slice(&original[8..16]);
  let stamp = u64::from_le_bytes(stamp).wrapping_add(1);
  stale.extend_from_slice(&stamp.to_le_bytes());
  stale.extend_from_slice(&original[16..24]);
  let checksum = xxhash_rust::xxh3::xxh3_64(&stale);
  stale.extend_from_slice(&checksum.to_le_bytes());
  fs::write(&calib_path, &stale).unwrap();
  assert_eq!(fingerprint(&Searcher::open(&out).unwrap(), "alpha"), baseline);

  // Torn file: absent, never a value — and never an error.
  fs::write(&calib_path, &original[0..10]).unwrap();
  assert_eq!(fingerprint(&Searcher::open(&out).unwrap(), "alpha"), baseline);

  // A fresh-tier warm heals the missing/invalid calibration without a rebuild.
  fs::remove_file(&calib_path).unwrap();
  warm_ann(&out).unwrap();
  let healed = fs::read(&calib_path).expect("fresh-branch warm re-measures");
  assert_eq!(healed.len(), 32);
  assert_eq!(&healed[0..16], &original[0..16], "same magic/version/stamp");

  let _ = fs::remove_dir_all(&base);
}

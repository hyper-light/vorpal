//! RetainedIndex equivalence (SUBSECOND.md Phase 3): a retained state that absorbed edits by
//! tombstone-and-append must link+seal to the SAME graph bytes an independent from-scratch
//! pipeline (Ingestor → in-RAM resolve → plain seal) produces over the final live tree, with
//! the same resolution stats. Products come from the real extractor.

use std::fs;

use vorpal_ingest::{
  Ingestor, OutlineExtractor, Resolver, RetainedIndex, encode_product_into,
};

fn itn() -> &'static vorpal_ingest::Interner {
  static INTERNER: std::sync::OnceLock<vorpal_ingest::Interner> = std::sync::OnceLock::new();
  INTERNER.get_or_init(vorpal_ingest::Interner::default)
}

const X: &str = "class Widget:\n    def render(self):\n        return helper()\n\ndef helper():\n    return 1\n";
const Y_OLD: &str = "def y_old_fn():\n    return helper()\n";
const Y_NEW: &str =
  "from x import helper\n\ndef y_fn():\n    return helper()\n\ndef y_extra():\n    return y_fn()\n";
const Z: &str = "def z_fn():\n    return y_fn()\n";
const W: &str = "def w_gone():\n    return z_fn()\n";

fn product_bytes(extractor: &OutlineExtractor, path: &str, source: &str) -> Vec<u8> {
  let product = extractor
    .extract_product(path, source)
    .expect("extraction succeeds");
  let mut buf = Vec::new();
  encode_product_into(&product, &mut buf);
  buf
}

fn kg_bytes(kg: &vorpal_ingest::Kg, dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
  kg.save(dir).expect("save kg");
  let mut out: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
    .expect("read dir")
    .flatten()
    .map(|e| {
      (
        e.file_name().to_string_lossy().into_owned(),
        fs::read(e.path()).expect("read artifact"),
      )
    })
    .collect();
  out.sort_by(|a, b| a.0.cmp(&b.0));
  out
}

#[test]
fn retained_edits_link_identical_to_scratch_pipeline() {
  let extractor = OutlineExtractor::new().unwrap();
  let root = std::env::temp_dir().join(format!("vorpal-retained-{}", std::process::id()));
  let _ = fs::remove_dir_all(&root);
  fs::create_dir_all(&root).unwrap();

  // Retained: initial corpus x, y_old, z, w — then edit y, delete w.
  let initial = [
    ("x.py", product_bytes(&extractor, "x.py", X)),
    ("y.py", product_bytes(&extractor, "y.py", Y_OLD)),
    ("z.py", product_bytes(&extractor, "z.py", Z)),
    ("w.py", product_bytes(&extractor, "w.py", W)),
  ];
  let mut retained = RetainedIndex::build(
    itn(),
    &root.join("refs.store"),
    initial.iter().map(|(p, b)| (*p, b.as_slice())),
  )
  .expect("retained build");
  let y_new = product_bytes(&extractor, "y.py", Y_NEW);
  retained
    .apply_file(itn(), "y.py", Some(&y_new))
    .expect("apply y edit");
  retained.apply_file(itn(), "w.py", None).expect("delete w");
  assert_eq!(retained.file_count(), 3);
  assert!(retained.dead_row_fraction() > 0.0);
  let (kg_live, stats_live, evidence_live, _flows_live, _sigs_live) = retained
    .link(itn(), &Resolver::new(), &[])
    .expect("retained link");

  // Scratch: the independent pipeline over the final live tree, in canonical (path) order.
  let scratch_products: Vec<(&str, vorpal_ingest::FileProduct)> =
    [("x.py", X), ("y.py", Y_NEW), ("z.py", Z)]
      .into_iter()
      .map(|(path, source)| {
        (
          path,
          extractor
            .extract_product(path, source)
            .expect("extraction succeeds"),
        )
      })
      .collect();
  let mut scratch = Ingestor::new(itn(), extractor);
  for (path, product) in scratch_products {
    scratch.ingest_product(path, product);
  }
  let (kg_scratch, stats_scratch) = scratch.link_and_seal(&Resolver::new());

  assert_eq!(
    (stats_live.resolved, stats_live.ambiguous, stats_live.external, stats_live.masked),
    (
      stats_scratch.resolved,
      stats_scratch.ambiguous,
      stats_scratch.external,
      stats_scratch.masked
    ),
    "resolution stats must match scratch"
  );
  let live = kg_bytes(&kg_live, &root.join("live"));
  let reference = kg_bytes(&kg_scratch, &root.join("scratch"));
  assert_eq!(
    live.iter().map(|(n, _)| n).collect::<Vec<_>>(),
    reference.iter().map(|(n, _)| n).collect::<Vec<_>>()
  );
  for ((name, lb), (_, rb)) in live.iter().zip(&reference) {
    assert_eq!(lb, rb, "{name} diverged from the scratch pipeline");
  }

  // Edit-path evidence ≡ build-path evidence: a FRESH retained state over the final live
  // set must produce the identical evidence row set (compared canonically via the saver).
  let fresh_extractor = OutlineExtractor::new().unwrap();
  let fresh_inputs = [
    ("x.py", product_bytes(&fresh_extractor, "x.py", X)),
    ("y.py", y_new.clone()),
    ("z.py", product_bytes(&fresh_extractor, "z.py", Z)),
  ];
  let mut fresh = RetainedIndex::build(
    itn(),
    &root.join("refs2.store"),
    fresh_inputs.iter().map(|(p, b)| (*p, b.as_slice())),
  )
  .expect("fresh retained build");
  let (_kg_fresh, _stats_fresh, evidence_fresh, _flows_fresh, _sigs_fresh) = fresh
    .link(itn(), &Resolver::new(), &[])
    .expect("fresh link");
  let ev_dir_a = root.join("ev-a");
  let ev_dir_b = root.join("ev-b");
  fs::create_dir_all(&ev_dir_a).unwrap();
  fs::create_dir_all(&ev_dir_b).unwrap();
  vorpal_kg::save_evidence(&ev_dir_a, evidence_live).unwrap();
  vorpal_kg::save_evidence(&ev_dir_b, evidence_fresh).unwrap();
  assert_eq!(
    fs::read(ev_dir_a.join("evidence.bin")).unwrap(),
    fs::read(ev_dir_b.join("evidence.bin")).unwrap(),
    "edit-path evidence must equal build-path evidence"
  );
  let _ = fs::remove_dir_all(&root);
}

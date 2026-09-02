//! Keeps docs/INDEX_FORMAT.md's version table true (IMPROVEMENTS #12): the table is
//! generated from the version constants in source, so a format bump that forgets the
//! compatibility document is impossible.
//!
//! The normal run ASSERTS ONLY — a stale table fails with instructions, never a write
//! (a test that mutates the working tree collides with concurrent sessions and
//! read-only checkouts; this is the `grammar_provenance` convention). To refresh the
//! table after bumping a constant:
//!
//! `cargo test -p vorpal-index --test format_policy -- --ignored regenerate`
//!
//! and own the doc diff in the same commit as the bump.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../..")
    .canonicalize()
    .unwrap()
}

/// Read `const NAME: u32 = N;` from a source file.
fn version_of(rel_path: &str, name: &str) -> u32 {
  let source = fs::read_to_string(repo_root().join(rel_path)).unwrap();
  for line in source.lines() {
    let trimmed = line.trim();
    for prefix in [
      format!("const {name}: u32 = "),
      format!("pub const {name}: u32 = "),
      format!("pub(crate) const {name}: u32 = "),
    ] {
      if let Some(rest) = trimmed.strip_prefix(&prefix) {
        return rest
          .trim_end_matches(';')
          .parse()
          .unwrap_or_else(|_| panic!("unparseable {name} in {rel_path}"));
      }
    }
  }
  panic!("{name} not found in {rel_path}");
}

fn generated_table() -> String {
  let rows: Vec<(&str, &str, &str, u32, &str)> = vec![
    (
      "extraction products (`products/*.vpb`, pack bodies)",
      "PRODUCT_FORMAT_VERSION",
      "crates/ingest/src/product.rs",
      version_of("crates/ingest/src/product.rs", "PRODUCT_FORMAT_VERSION"),
      "cache miss → re-parse",
    ),
    (
      "product pack, bucketed layout (`products/<k>.pack` + `products/toc.bin`) — the default",
      "BUCKET_VERSION",
      "crates/ingest/src/pack.rs",
      version_of("crates/ingest/src/pack.rs", "BUCKET_VERSION"),
      "pack ignored → rebuilt by next build",
    ),
    (
      "product pack, legacy flat layout (`products.pack`/`products.idx`) — deprecated, written only under `VORPAL_FORMAT=flat`; reads retained",
      "PACK_VERSION",
      "crates/ingest/src/pack.rs",
      version_of("crates/ingest/src/pack.rs", "PACK_VERSION"),
      "pack ignored → rebuilt by next build",
    ),
    (
      "graph segments (`*.vseg`, `strings.heap`, `graph.bin`)",
      "FORMAT_VERSION",
      "crates/segment/src/format.rs",
      version_of("crates/segment/src/format.rs", "FORMAT_VERSION"),
      "`Kg::load` fails loudly → rebuild",
    ),
    (
      "evidence sidecar (`evidence.bin`)",
      "VERSION",
      "crates/kg/src/evidence.rs",
      version_of("crates/kg/src/evidence.rs", "VERSION"),
      "sidecar treated as absent → `why` reports no evidence",
    ),
    (
      "edge slabs (`edges/<k>.bin` + toc)",
      "VERSION",
      "crates/kg/src/edgestore.rs",
      version_of("crates/kg/src/edgestore.rs", "VERSION"),
      "family treated as absent → scoped composes decline; next full build rewrites it",
    ),
    (
      "usage postings (`usage/<k>.bin` + toc)",
      "VERSION",
      "crates/kg/src/usagestore.rs",
      version_of("crates/kg/src/usagestore.rs", "VERSION"),
      "family treated as absent → scoped composes decline; next full build rewrites it",
    ),
    (
      "sigs sketch ledger (`sigs/<k>.bin` + toc)",
      "VERSION",
      "crates/kg/src/sigstore.rs",
      version_of("crates/kg/src/sigstore.rs", "VERSION"),
      "prior generation neither reused nor composed from → full pipeline rebuilds the family",
    ),
    (
      "include-reach graph (`reach.bin`)",
      "REACH_GRAPH_VERSION",
      "crates/resolve/src/reach.rs",
      version_of("crates/resolve/src/reach.rs", "REACH_GRAPH_VERSION"),
      "scoped composes decline (reach oracle unreplayable) → full pipeline rebuilds it",
    ),
    (
      "data-flow sidecar (`dataflow.bin`)",
      "VERSION",
      "crates/kg/src/dataflow.rs",
      version_of("crates/kg/src/dataflow.rs", "VERSION"),
      "load fails loudly → rebuild (absent file ≠ mismatch: older generations answer no flows)",
    ),
    (
      "lexical posting tier (`postings.bin`)",
      "VERSION",
      "crates/index/src/postings.rs",
      version_of("crates/index/src/postings.rs", "VERSION"),
      "scan fallback → warm rebuilds",
    ),
    (
      "embedding semantics (`ann.model.json`)",
      "LEXICAL_EMBED_VERSION",
      "crates/ann/src/embed.rs",
      version_of("crates/ann/src/embed.rs", "LEXICAL_EMBED_VERSION"),
      "ANN tier distrusted → exact fallback → warm rebuilds",
    ),
    (
      "calls-graph communities (`communities.bin`)",
      "VERSION",
      "crates/kg/src/communities.rs",
      version_of("crates/kg/src/communities.rs", "VERSION"),
      "sidecar treated as absent → `community` answers `null`, `architecture` says not built → warm rebuilds",
    ),
    (
      "semantic engine calibration (`ann.calib`)",
      "ANN_CALIB_VERSION",
      "crates/index/src/lib.rs",
      version_of("crates/index/src/lib.rs", "ANN_CALIB_VERSION"),
      "calibration treated as absent → structural routing floor (full-population fetches scan; the beam keeps everything below) → next warm re-measures",
    ),
    (
      "learned embedding model (`ann.model.bin`)",
      "LEARNED_MODEL_VERSION",
      "crates/ann/src/learned/persist.rs",
      version_of("crates/ann/src/learned/persist.rs", "LEARNED_MODEL_VERSION"),
      "model unreadable/stale → lexical fallback stated in provenance → warm retrains",
    ),
  ];

  let mut table = String::from("| Artifact | Constant | Value | On mismatch |\n|---|---|---|---|\n");
  for (artifact, constant, file, value, mismatch) in &rows {
    table.push_str(&format!(
      "| {artifact} | `{constant}` ({file}) | {value} | {mismatch} |\n"
    ));
  }
  table
}

const DOC_REL: &str = "docs/INDEX_FORMAT.md";
const BEGIN: &str = "<!-- BEGIN GENERATED VERSION TABLE -->\n";
const END: &str = "<!-- END GENERATED VERSION TABLE -->";

/// Read the doc and splice the freshly generated table between the markers.
fn doc_with_current_table() -> (PathBuf, String, String) {
  let doc_path = repo_root().join(DOC_REL);
  let doc = fs::read_to_string(&doc_path)
    .unwrap_or_else(|e| panic!("{DOC_REL} exists: {e}"));
  let start = doc.find(BEGIN).expect("BEGIN marker") + BEGIN.len();
  let end = doc.find(END).expect("END marker");
  let rebuilt = format!("{}{}{}", &doc[..start], generated_table(), &doc[end..]);
  (doc_path, doc, rebuilt)
}

#[test]
fn version_table_matches_the_constants() {
  let (_, doc, rebuilt) = doc_with_current_table();
  assert_eq!(
    doc, rebuilt,
    "{DOC_REL}'s version table is stale relative to the version constants in source.\n\
     Refresh it (and own the diff in the bumping commit) with:\n\
     `cargo test -p vorpal-index --test format_policy -- --ignored regenerate`"
  );
}

/// Manual regeneration: rewrite the version table from code truth.
#[test]
#[ignore = "mutates docs/INDEX_FORMAT.md; run explicitly after a version bump"]
fn regenerate() {
  let (doc_path, doc, rebuilt) = doc_with_current_table();
  if rebuilt != doc {
    fs::write(&doc_path, &rebuilt).unwrap();
    println!("rewrote {DOC_REL} version table from code truth");
  } else {
    println!("{DOC_REL} version table already current");
  }
}

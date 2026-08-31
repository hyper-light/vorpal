//! Keeps docs/INDEX_FORMAT.md's version table true (IMPROVEMENTS #12): the table is
//! regenerated from the version constants in source, so a format bump that forgets the
//! compatibility document is impossible — the doc self-heals and the diff rides the commit
//! that bumped the constant.

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

#[test]
fn version_table_matches_the_constants() {
  let rows: Vec<(&str, &str, &str, u32, &str)> = vec![
    (
      "extraction products (`products/*.vpb`, pack bodies)",
      "PRODUCT_FORMAT_VERSION",
      "crates/ingest/src/product.rs",
      version_of("crates/ingest/src/product.rs", "PRODUCT_FORMAT_VERSION"),
      "cache miss → re-parse",
    ),
    (
      "product pack, flat layout (`products.pack`/`products.idx`)",
      "PACK_VERSION",
      "crates/ingest/src/pack.rs",
      version_of("crates/ingest/src/pack.rs", "PACK_VERSION"),
      "pack ignored → rebuilt by next build",
    ),
    (
      "product pack, bucketed layout (`products/<k>.pack` + `products/toc.bin`, written under `VORPAL_FORMAT=next`)",
      "BUCKET_VERSION",
      "crates/ingest/src/pack.rs",
      version_of("crates/ingest/src/pack.rs", "BUCKET_VERSION"),
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
  ];

  let mut table = String::from("| Artifact | Constant | Value | On mismatch |\n|---|---|---|---|\n");
  for (artifact, constant, file, value, mismatch) in &rows {
    table.push_str(&format!(
      "| {artifact} | `{constant}` ({file}) | {value} | {mismatch} |\n"
    ));
  }

  let doc_path = repo_root().join("docs/INDEX_FORMAT.md");
  let doc = fs::read_to_string(&doc_path).expect("docs/INDEX_FORMAT.md exists");
  const BEGIN: &str = "<!-- BEGIN GENERATED VERSION TABLE -->\n";
  const END: &str = "<!-- END GENERATED VERSION TABLE -->";
  let start = doc.find(BEGIN).expect("BEGIN marker") + BEGIN.len();
  let end = doc.find(END).expect("END marker");
  let rebuilt = format!("{}{}{}", &doc[..start], table, &doc[end..]);
  if rebuilt != doc {
    fs::write(&doc_path, &rebuilt).unwrap();
    println!("rewrote docs/INDEX_FORMAT.md version table from code truth");
  }
  assert_eq!(fs::read_to_string(&doc_path).unwrap(), rebuilt);
}

//! Import edge extraction + resolution (§3.1/§3.3): `use` statements become `imports` edges.

use vorpal_ingest::{Ingestor, OutlineExtractor, Resolver};

/// One shared session for the whole test binary — bounded vocabulary, no lifetime plumbing.
fn itn() -> &'static vorpal_ingest::Interner {
  static INTERNER: std::sync::OnceLock<vorpal_ingest::Interner> = std::sync::OnceLock::new();
  INTERNER.get_or_init(vorpal_ingest::Interner::default)
}


#[test]
fn extracts_and_resolves_rust_imports() {
  let mut ing = Ingestor::new(itn(), OutlineExtractor::new().unwrap());
  ing.ingest_source("b.rs", "pub fn target() -> i32 {\n    0\n}\n");
  ing.ingest_source(
    "a.rs",
    "use b::target;\n\npub fn caller() -> i32 {\n    0\n}\n",
  );
  let (kg, _stats) = ing.link_and_seal(&Resolver::new());

  // a.rs imports `target` (defined + exported in b.rs) → a File --imports--> target edge.
  let importers: Vec<String> = kg
    .importers_of("target")
    .into_iter()
    .filter_map(|id| kg.node(id).map(|v| v.name.to_string()))
    .collect();
  assert!(
    importers.contains(&"a.rs".to_string()),
    "importers of target: {importers:?}"
  );
}

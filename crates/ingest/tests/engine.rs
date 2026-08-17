//! End-to-end L0→L1→L3: a real tree-sitter parse + outline extraction into the KG.

use vorpal_ingest::{Ingestor, OutlineExtractor};
use vorpal_kg::{NodeId, SymbolKind};

/// One shared session for the whole test binary — bounded vocabulary, no lifetime plumbing.
fn itn() -> &'static vorpal_ingest::Interner {
  static INTERNER: std::sync::OnceLock<vorpal_ingest::Interner> = std::sync::OnceLock::new();
  INTERNER.get_or_init(vorpal_ingest::Interner::default)
}


const RUST_SRC: &str = "\
pub struct Point {
    pub x: i32,
    pub y: i32,
}

pub fn dist(p: Point) -> i32 {
    p.x + p.y
}
";

fn node_names(kg: &vorpal_kg::Kg) -> Vec<(String, SymbolKind)> {
  (0..kg.node_count() as u64)
    .filter_map(|i| kg.node(NodeId::new(i)))
    .map(|v| (v.name.to_string(), v.kind))
    .collect()
}

#[test]
fn compiled_ruleset_covers_all_supported_languages() {
  let extractor = OutlineExtractor::new().expect("bundled rules compile");
  assert!(
    extractor.languages() >= 28,
    "expected outline rules for all 28 supported languages, got {}",
    extractor.languages()
  );
  // One extension per SupportLang variant — every language is handled end-to-end.
  for ext in [
    "sh", "c", "cpp", "cs", "css", "dart", "go", "ex", "hs", "tf", "html", "java", "js", "json",
    "kt", "lua", "md", "nix", "php", "py", "rb", "rs", "scala", "sol", "swift", "ts", "tsx", "yml",
  ] {
    assert!(extractor.handles(&format!("file.{ext}")), "extension {ext}");
  }
  assert!(!extractor.handles("notes.unknownext"));
}

const GENERIC_SRC: &str = "\
pub struct Reader<'a> {
    pub data: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn read(&self) -> u8 {
        self.data[0]
    }
}

pub fn consume(r: Reader<'_>) -> u8 {
    r.read()
}
";

#[test]
fn generic_impl_methods_share_the_types_identity() {
  let mut ing = Ingestor::new(itn(), OutlineExtractor::new().unwrap());
  ing.ingest_source("lib.rs", GENERIC_SRC);
  let (kg, stats) = ing.link_and_seal(&vorpal_ingest::Resolver::new());
  assert!(stats.resolved >= 1, "r.read() should resolve: {stats:?}");

  let names = node_names(&kg);
  // No phantom `Reader<'a>` node: the impl's name strips generics and dedups onto the struct.
  assert!(
    names.iter().all(|(n, _)| !n.contains('<')),
    "no generic-carrying identities: {names:?}"
  );
  assert_eq!(
    names.iter().filter(|(n, _)| n == "Reader").count(),
    1,
    "one Reader identity: {names:?}"
  );

  // The method hangs off the struct node and its call site resolves to it.
  let read = (0..kg.node_count() as u64)
    .map(NodeId::new)
    .find(|&id| kg.node(id).is_some_and(|v| v.name == "read"))
    .expect("read method node");
  assert_eq!(kg.node(read).unwrap().kind, SymbolKind::Method);
  let callers: Vec<String> = kg
    .callers_of("read")
    .into_iter()
    .filter_map(|id| kg.node(id).map(|v| v.name.to_string()))
    .collect();
  assert!(callers.contains(&"consume".to_string()), "{callers:?}");
}

#[test]
fn extracts_a_real_rust_file_into_the_kg() {
  let mut ing = Ingestor::new(itn(), OutlineExtractor::new().unwrap());
  ing.ingest_source("lib.rs", RUST_SRC);
  let kg = ing.seal();

  let names = node_names(&kg);
  assert!(
    names.iter().any(|(n, _)| n == "lib.rs"),
    "expected a File node; got {names:?}"
  );
  assert!(
    names.iter().any(|(n, _)| n == "Point"),
    "expected the Point struct to be extracted; got {names:?}"
  );
  assert!(
    names.iter().any(|(n, _)| n == "dist"),
    "expected the dist function to be extracted; got {names:?}"
  );

  // The file should define the top-level items (containment forest).
  let file = (0..kg.node_count() as u64)
    .map(NodeId::new)
    .find(|&id| kg.node(id).is_some_and(|v| v.name == "lib.rs"))
    .unwrap();
  let defined: Vec<String> = kg
    .defines(file)
    .into_iter()
    .filter_map(|id| kg.node(id).map(|v| v.name.to_string()))
    .collect();
  assert!(
    defined.contains(&"Point".to_string()),
    "file defines Point; got {defined:?}"
  );
  assert!(
    defined.contains(&"dist".to_string()),
    "file defines dist; got {defined:?}"
  );
}

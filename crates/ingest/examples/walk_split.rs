//! Phase split of one extraction: parse vs walk (diagnosis only).
use std::time::Instant;
fn main() {
  let path = std::env::args().nth(1).expect("file");
  let source = std::fs::read_to_string(&path).expect("read");
  let extractor = vorpal_ingest::OutlineExtractor::new().expect("rules");
  let _ = extractor.extract_product("warm.c", "int x;\n");
  let t = Instant::now();
  let lang: vorpal_lang_registry::SgLang = "c".parse().expect("c");
  let grep = vorpal_core::tree_sitter::LanguageExt::grep(&lang, source.as_str());
  let parse_ms = t.elapsed().as_millis();
  drop(grep);
  let t = Instant::now();
  let product = extractor.extract_product("bench.c", &source).expect("extract");
  let total_ms = t.elapsed().as_millis();
  let mut buf = Vec::new();
  let t = Instant::now();
  vorpal_ingest::encode_product_into(&product, &mut buf);
  let encode_ms = t.elapsed().as_millis();
  println!(
    "parse {} ms | full-extract {} ms | walk-ish {} ms | re-encode {} ms | items {} refs {}",
    parse_ms, total_ms, total_ms.saturating_sub(parse_ms), encode_ms,
    product.items.len(), product.refs.len()
  );
}

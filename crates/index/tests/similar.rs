//! Near-clone edges (`similar_to`): extraction-time MinHash sketches paired at link, symmetric,
//! confidence = estimated similarity, bounded and stated on the report.

use std::fs;

use vorpal_index::build_index;
use vorpal_kg::{EdgeType, Kg, NodeId, SymbolKind};

const ORIGINAL: &str = "pub fn parse_header(input: &[u8]) -> Option<(u8, u16)> {\n\
    if input.len() < 4 { return None; }\n\
    let version = input[0];\n\
    let length = u16::from_le_bytes([input[2], input[3]]);\n\
    if version != 1 || length as usize > input.len() { return None; }\n\
    let mut sum = 0u32;\n\
    for byte in &input[4..] { sum = sum.wrapping_add(*byte as u32); }\n\
    if sum % 7 == 3 { return None; }\n\
    Some((version, length))\n\
}\n";

#[test]
fn near_clones_are_paired_and_stated() {
  let base = std::env::temp_dir().join(format!("vorpal-similar-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  // A copy-pasted function with a renamed variable and one changed constant; an unrelated
  // function of similar size; a tiny function that can never be signed.
  let clone = ORIGINAL
    .replace("parse_header", "parse_frame")
    .replace("sum", "total")
    .replace("% 7 == 3", "% 7 == 5");
  fs::write(src.join("a.rs"), ORIGINAL).unwrap();
  fs::write(src.join("b.rs"), format!("{clone}\npub fn tiny() -> u8 {{ 1 }}\n")).unwrap();
  fs::write(
    src.join("c.rs"),
    "pub fn render(rows: &[String], width: usize) -> String {\n\
       let mut out = String::new();\n\
       for (i, row) in rows.iter().enumerate() {\n\
         if i > 0 { out.push('\\n'); }\n\
         let pad = width.saturating_sub(row.len());\n\
         out.push_str(row);\n\
         for _ in 0..pad { out.push(' '); }\n\
       }\n\
       out\n\
     }\n",
  )
  .unwrap();
  let report = build_index(&src, &out).unwrap();
  assert_eq!(report.similar_edges, 1, "{:?}", report.similar_note);
  assert!(report.similar_note.is_none());

  let kg = Kg::load(&out).unwrap();
  let id = |name: &str| {
    (0..kg.node_count() as u64)
      .map(NodeId::new)
      .find(|&n| kg.node(n).is_some_and(|v| v.name == name && v.kind == SymbolKind::Function))
      .unwrap_or_else(|| panic!("{name}"))
  };
  let (header, frame, render, tiny) = (id("parse_header"), id("parse_frame"), id("render"), id("tiny"));
  let partners = |n: NodeId| kg.incoming_with_confidence(n, EdgeType::SIMILAR_TO);
  // Symmetric, with the estimate as confidence.
  let to_header = partners(header);
  assert_eq!(to_header.len(), 1, "{to_header:?}");
  assert_eq!(to_header[0].0, frame);
  assert!(to_header[0].1 >= 70, "{}", to_header[0].1);
  assert_eq!(partners(frame), vec![(header, to_header[0].1)]);
  assert!(partners(render).is_empty());
  assert!(partners(tiny).is_empty());

  // The relation is first-class: query language, verb surface, schema name.
  let rows = vorpal_query::run(
    &kg,
    r#"MATCH (f {name: "parse_header"})-[:similar_to]->(g) RETURN g.name"#,
  )
  .unwrap();
  assert_eq!(rows.rows, vec![vec![vorpal_query::Cell::Text("parse_frame".into())]]);
  let target = vorpal_index::GraphTarget {
    name: "parse_header".into(),
    id: None,
    external_id: None,
    path_suffix: None,
    kind: None,
    merge_all: false,
    show_ids: false,
  };
  let text = vorpal_index::graph_query_on(&kg, "similar", &target).unwrap();
  assert!(text.contains("parse_frame") && text.contains("% similar)"), "{text}");
  match vorpal_index::records::related_records(&kg, "similar", &target).unwrap() {
    vorpal_index::records::Selected::Hits(records) => {
      assert_eq!(records.len(), 1);
      assert_eq!(records[0].similarity, Some(to_header[0].1));
    }
    other => panic!("{other:?}"),
  }

  // Deterministic: a from-scratch rebuild names the same generation.
  let current = fs::read_to_string(out.join("CURRENT")).unwrap();
  let again = base.join("again");
  build_index(&src, &again).unwrap();
  assert_eq!(fs::read_to_string(again.join("CURRENT")).unwrap(), current);

  // Nothing signed: stated, never a silent zero.
  let lone = base.join("lone");
  fs::create_dir_all(&lone).unwrap();
  fs::write(lone.join("x.rs"), "pub fn tiny() -> u8 { 1 }\n").unwrap();
  let report = build_index(&lone, &base.join("lone-index")).unwrap();
  assert_eq!(report.similar_edges, 0);
  assert!(report.similar_note.as_deref().unwrap().contains("signing floor"), "{:?}", report.similar_note);

  let _ = fs::remove_dir_all(&base);
}

//! Golden queries over a hand-built fixture graph, error/ceiling coverage, IR round-trips,
//! and a lexer/parser fuzz sweep (the parser must be total: typed errors, never panics).

use vorpal_kg::{EdgeType, Kg, KgWriter, NodeDef, SymbolKind};
use vorpal_query::{Cell, QueryError, parse, parse_ir_json, run};

fn def<'a>(kind: SymbolKind, name: &'a str, path: &'a str, exported: bool) -> NodeDef<'a> {
  NodeDef {
    kind,
    name,
    entity_path: name,
    path,
    signature: "",
    exported,
    content_hash: 1,
    span: (0, 0),
  }
}

/// main → parse → validate → deserialize (calls, confidences 100/95/85);
/// helper (Method) → deserialize (calls, 100); main —data_flows(85)→ parse;
/// Config is an unconnected Class.
fn fixture() -> Kg {
  let mut writer = KgWriter::new();
  let main = writer.define(def(SymbolKind::Function, "main", "src/main.rs", true));
  let parse_fn = writer.define(def(SymbolKind::Function, "parse", "src/parse.rs", true));
  let validate = writer.define(def(SymbolKind::Function, "validate", "src/parse.rs", false));
  let deser = writer.define(def(SymbolKind::Function, "deserialize", "src/serde.rs", true));
  let helper = writer.define(def(SymbolKind::Method, "helper", "src/util.rs", false));
  let _config = writer.define(def(SymbolKind::Class, "Config", "src/config.rs", true));
  writer.add_edge(main, parse_fn, EdgeType::CALLS.with_confidence(100));
  writer.add_edge(parse_fn, validate, EdgeType::CALLS.with_confidence(95));
  writer.add_edge(validate, deser, EdgeType::CALLS.with_confidence(85));
  writer.add_edge(helper, deser, EdgeType::CALLS.with_confidence(100));
  writer.add_edge(main, parse_fn, EdgeType::DATA_FLOWS.with_confidence(85));
  writer.seal()
}

fn texts(rows: &[Vec<Cell>], col: usize) -> Vec<String> {
  rows.iter().map(|r| r[col].to_string()).collect()
}

#[test]
fn single_node_lookups_and_scans() {
  let kg = fixture();

  let r = run(&kg, r#"MATCH (f {name: "parse"}) RETURN f.name, f.path"#).unwrap();
  assert_eq!(r.columns, ["f.name", "f.path"]);
  assert_eq!(r.rows, vec![vec![
    Cell::Text("parse".into()),
    Cell::Text("src/parse.rs".into())
  ]]);

  let r = run(&kg, "MATCH (f:Function) RETURN COUNT(*)").unwrap();
  assert_eq!(r.rows, vec![vec![Cell::Int(4)]]);

  // Bare-variable projection expands to the four identity columns.
  let r = run(&kg, r#"MATCH (f {name: "main"}) RETURN f"#).unwrap();
  assert_eq!(r.columns, ["f.id", "f.name", "f.kind", "f.path"]);
  assert_eq!(r.rows[0][2], Cell::Text("function".into()));

  // id-anchored: node 0 is `main` (definition order).
  let r = run(&kg, "MATCH (f {id: 0}) RETURN f.name").unwrap();
  assert_eq!(texts(&r.rows, 0), ["main"]);

  // Inline path property is a suffix match, like --path everywhere else.
  let r = run(&kg, r#"MATCH (f:Function {path: "parse.rs"}) RETURN f.name ORDER BY f.name"#)
    .unwrap();
  assert_eq!(texts(&r.rows, 0), ["parse", "validate"]);
}

#[test]
fn one_hop_and_reverse_anchoring() {
  let kg = fixture();

  let r = run(&kg, r#"MATCH (f {name: "main"})-[:calls]->(g) RETURN g.name"#).unwrap();
  assert_eq!(texts(&r.rows, 0), ["parse"]);

  // The right side is cheaper (name beats kind), so the planner anchors there and walks
  // In-edges; the kind label still filters the reached side (helper is a Method).
  let r = run(
    &kg,
    r#"MATCH (f:Function)-[:calls]->(g {name: "deserialize"}) RETURN f.name"#,
  )
  .unwrap();
  assert_eq!(texts(&r.rows, 0), ["validate"]);

  // Undirected: parse is called by main and calls validate.
  let r = run(&kg, r#"MATCH (p {name: "parse"})-[:calls]-(x) RETURN x.name ORDER BY x.name"#)
    .unwrap();
  assert_eq!(texts(&r.rows, 0), ["main", "validate"]);

  // Relation union + In direction spelled from the other side.
  let r = run(&kg, r#"MATCH (g)<-[:calls|data_flows]-(f {name: "main"}) RETURN g.name"#).unwrap();
  assert_eq!(texts(&r.rows, 0), ["parse"]);
}

#[test]
fn var_length_paths_and_grades() {
  let kg = fixture();

  // The flagship shape: who reaches deserialize through calls within 5 hops?
  let r = run(
    &kg,
    r#"MATCH (f)-[:calls*1..5]->(g {name: "deserialize"}) RETURN f.name ORDER BY f.name"#,
  )
  .unwrap();
  assert_eq!(texts(&r.rows, 0), ["helper", "main", "parse", "validate"]);

  // Exact-grade floor (100) breaks the 85/95 links: only helper's direct call survives.
  let r = run(
    &kg,
    r#"MATCH (f)-[:calls*1..5 {grade: exact}]->(g {name: "deserialize"}) RETURN f.name"#,
  )
  .unwrap();
  assert_eq!(texts(&r.rows, 0), ["helper"]);

  // Minimum depth: exactly the two-hop-or-more reachers of deserialize.
  let r = run(
    &kg,
    r#"MATCH (f)-[:calls*2..5]->(g {name: "deserialize"}) RETURN f.name ORDER BY f.name"#,
  )
  .unwrap();
  assert_eq!(texts(&r.rows, 0), ["main", "parse"]);

  // `*2` = exactly two hops.
  let r = run(&kg, r#"MATCH (f {name: "main"})-[:calls*2]->(g) RETURN g.name"#).unwrap();
  assert_eq!(texts(&r.rows, 0), ["validate"]);
}

#[test]
fn where_predicates_and_aliases() {
  let kg = fixture();

  let r = run(
    &kg,
    r#"MATCH (f:Function) WHERE f.path STARTS WITH "src/p" AND f.exported = false RETURN f.name"#,
  )
  .unwrap();
  assert_eq!(texts(&r.rows, 0), ["validate"]);

  let r = run(
    &kg,
    r#"MATCH (f) WHERE f.name CONTAINS "eri" RETURN f.name AS who ORDER BY who"#,
  )
  .unwrap();
  assert_eq!(r.columns, ["who"]);
  assert_eq!(texts(&r.rows, 0), ["deserialize"]);

  let r = run(&kg, r#"MATCH (f:Function) WHERE f.name <> "main" RETURN COUNT(*)"#).unwrap();
  assert_eq!(r.rows, vec![vec![Cell::Int(3)]]);
}

#[test]
fn aggregation_grouping_and_paging() {
  let kg = fixture();

  // Implicit grouping on the single non-count key.
  let r = run(
    &kg,
    r#"MATCH (f)-[:calls]->(g) RETURN g.name, COUNT(*) ORDER BY count DESC, g.name"#,
  )
  .unwrap();
  assert_eq!(r.columns, ["g.name", "count"]);
  assert_eq!(
    r.rows,
    vec![
      vec![Cell::Text("deserialize".into()), Cell::Int(2)],
      vec![Cell::Text("parse".into()), Cell::Int(1)],
      vec![Cell::Text("validate".into()), Cell::Int(1)],
    ]
  );

  let r = run(&kg, "MATCH (f)-[:calls]->(g) RETURN COUNT(DISTINCT g.path)").unwrap();
  assert_eq!(r.rows, vec![vec![Cell::Int(2)]]); // parse.rs and serde.rs

  let r = run(&kg, "MATCH (f:Function) RETURN f.name ORDER BY f.name SKIP 1 LIMIT 2").unwrap();
  assert_eq!(texts(&r.rows, 0), ["main", "parse"]);
  assert_eq!(r.total_rows, 4); // pre-SKIP/LIMIT count survives for "showing k of n"
}

#[test]
fn typed_errors_name_the_boundary() {
  let kg = fixture();
  let plan_err = |text: &str| match run(&kg, text) {
    Err(QueryError::Plan(message)) => message,
    other => panic!("expected a plan error for {text}, got {other:?}"),
  };

  assert!(plan_err("MATCH (f:Wibble) RETURN f.name").contains("unknown kind"));
  assert!(plan_err("MATCH (f)-[:befriends]->(g) RETURN f.name").contains("unknown relation"));
  assert!(
    plan_err(r#"MATCH (f)-[:calls {grade: mythic}]->(g) RETURN f.name"#).contains("unknown grade")
  );
  assert!(plan_err("MATCH (f) RETURN g.name").contains("not bound"));
  assert!(plan_err("MATCH (f) WHERE f.vibes = 3 RETURN f.name").contains("unknown property"));
  assert!(
    plan_err("MATCH (f) RETURN f.name ORDER BY f.path").contains("does not name a returned column")
  );

  match run(&kg, "MATCH (f)-[:calls*1..11]->(g) RETURN f.name") {
    Err(QueryError::Ceiling { what: "depth", limit: 10 }) => {}
    other => panic!("expected the depth ceiling, got {other:?}"),
  }
  match parse(&format!("MATCH (f) RETURN f.name -- {}", "x".repeat(17_000))) {
    Err(QueryError::Ceiling { what: "query text bytes", .. }) => {}
    other => panic!("expected the text ceiling, got {other:?}"),
  }

  // v1 boundaries fail with teaching errors, not generic syntax noise.
  let parse_err = |text: &str| match parse(text) {
    Err(QueryError::Parse { message, .. }) => message,
    other => panic!("expected a parse error for {text}, got {other:?}"),
  };
  assert!(parse_err("MATCH (f) WHERE f.name IN ['a'] RETURN f.name").contains("IN lists"));
  assert!(
    parse_err(&format!("MATCH (a){} RETURN a.name", "-[:calls]->(x)".repeat(9)))
      .contains("at most 8"),
    "segment ceiling"
  );
}

#[test]
fn ir_json_round_trips() {
  let kg = fixture();
  for text in [
    r#"MATCH (f {name: "parse"}) RETURN f.name, f.path"#,
    r#"MATCH (f)-[:calls*1..5 {grade: constrained}]->(g {name: "deserialize"}) RETURN f.name, f.path LIMIT 50"#,
    r#"MATCH (f)-[:calls]->(g) RETURN g.name, COUNT(*) ORDER BY count DESC, g.name"#,
  ] {
    let query = parse(text).unwrap();
    let json = serde_json::to_string(&query).unwrap();
    let reparsed = parse_ir_json(&json).unwrap();
    assert_eq!(query, reparsed, "IR round-trip changed the query for: {text}");
    assert_eq!(
      vorpal_query::execute(&kg, &query).unwrap(),
      vorpal_query::execute(&kg, &reparsed).unwrap()
    );
  }
}

/// The parser is total: seeded pseudo-random byte soup and token soup never panic.
#[test]
fn parser_fuzz_never_panics() {
  let mut state = 0x9E3779B97F4A7C15u64;
  let mut next = move || {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
  };
  let vocab = [
    "MATCH", "WHERE", "RETURN", "ORDER", "BY", "SKIP", "LIMIT", "COUNT", "DISTINCT", "AND",
    "OR", "NOT",
    "AS", "STARTS", "ENDS", "WITH", "CONTAINS", "(", ")", "[", "]", "{", "}", ":", ",", ".",
    "..", "|", "*", "<", ">", "-", "->", "<-", "=", "<>", "!=", "'x'", "\"y\"", "42", "f",
    "calls", "name", "grade", "true", "false", "\\", "\u{1F980}", "\0",
  ];
  for _ in 0..5_000 {
    // Token soup: random vocabulary joins.
    let n = (next() % 24) as usize;
    let mut text = String::new();
    for _ in 0..n {
      text.push_str(vocab[(next() as usize) % vocab.len()]);
      if next() % 3 == 0 {
        text.push(' ');
      }
    }
    let _ = parse(&text); // must return, never panic
  }
  for _ in 0..5_000 {
    // Byte soup (valid UTF-8 by construction from random chars).
    let n = (next() % 40) as usize;
    let text: String = (0..n)
      .map(|_| char::from_u32((next() % 0xFFFF) as u32).unwrap_or('�'))
      .collect();
    let _ = parse(&text);
  }
}

#[test]
fn degree_properties_and_ordered_comparisons() {
  let kg = fixture();

  // Hub finder: in_degree counts ALL incoming edges (parse: call + data-flow from main;
  // deserialize: two calls).
  let r = run(
    &kg,
    "MATCH (f:Function) WHERE f.in_degree >= 2 RETURN f.name, f.in_degree ORDER BY f.name",
  )
  .unwrap();
  assert_eq!(
    r.rows,
    vec![
      vec![Cell::Text("deserialize".into()), Cell::Int(2)],
      vec![Cell::Text("parse".into()), Cell::Int(2)],
    ]
  );

  let r = run(&kg, "MATCH (f:Function) WHERE f.out_degree = 0 RETURN f.name").unwrap();
  assert_eq!(texts(&r.rows, 0), ["deserialize"]);

  let r = run(
    &kg,
    "MATCH (f) WHERE f.in_degree < 1 AND f.kind <> \"class\" RETURN f.name ORDER BY f.name",
  )
  .unwrap();
  assert_eq!(texts(&r.rows, 0), ["helper", "main"]);

  // Ordered comparisons are typed at plan time.
  let plan_err = |text: &str| match run(&kg, text) {
    Err(QueryError::Plan(message)) => message,
    other => panic!("expected a plan error for {text}, got {other:?}"),
  };
  assert!(plan_err("MATCH (f) WHERE f.name > 3 RETURN f.name").contains("ordered comparison"));
  assert!(
    plan_err(r#"MATCH (f) WHERE f.in_degree CONTAINS "x" RETURN f.name"#)
      .contains("substring comparison")
  );
  assert!(plan_err(r#"MATCH (f) WHERE f.exported = "yes" RETURN f.name"#).contains("type mismatch"));

  // Degrees project and order like any other column.
  let r = run(
    &kg,
    "MATCH (f:Function) RETURN f.name, f.out_degree ORDER BY f.out_degree DESC, f.name LIMIT 2",
  )
  .unwrap();
  assert_eq!(r.rows[0][0], Cell::Text("main".into())); // calls + data_flows out
}

#[test]
fn scc_size_property_finds_recursion_knots() {
  // alpha ⇄ beta (mutual recursion) + gamma → alpha (acyclic caller).
  let mut writer = KgWriter::new();
  let alpha = writer.define(def(SymbolKind::Function, "alpha", "src/knot.rs", true));
  let beta = writer.define(def(SymbolKind::Function, "beta", "src/knot.rs", false));
  let gamma = writer.define(def(SymbolKind::Function, "gamma", "src/main.rs", true));
  writer.add_edge(alpha, beta, EdgeType::CALLS.with_confidence(100));
  writer.add_edge(beta, alpha, EdgeType::CALLS.with_confidence(100));
  writer.add_edge(gamma, alpha, EdgeType::CALLS.with_confidence(100));
  let kg = writer.seal();

  let r = run(
    &kg,
    "MATCH (f:Function) WHERE f.scc_size > 1 RETURN f.name, f.scc_size ORDER BY f.name",
  )
  .unwrap();
  assert_eq!(
    r.rows,
    vec![
      vec![Cell::Text("alpha".into()), Cell::Int(2)],
      vec![Cell::Text("beta".into()), Cell::Int(2)],
    ]
  );
  let r = run(&kg, r#"MATCH (f {name: "gamma"}) RETURN f.scc_size"#).unwrap();
  assert_eq!(r.rows, vec![vec![Cell::Int(1)]]);
}

#[test]
fn boolean_predicate_trees() {
  let kg = fixture();

  // OR at the bottom of precedence: (exported AND path-p) OR name=deserialize.
  let r = run(
    &kg,
    r#"MATCH (f:Function) WHERE f.exported = true AND f.path STARTS WITH "src/p" OR f.name = "deserialize" RETURN f.name ORDER BY f.name"#,
  )
  .unwrap();
  assert_eq!(texts(&r.rows, 0), ["deserialize", "parse"]);

  // Parentheses regroup: exported AND (path-p OR deserialize).
  let r = run(
    &kg,
    r#"MATCH (f:Function) WHERE f.exported = true AND (f.path STARTS WITH "src/p" OR f.name = "deserialize") RETURN f.name ORDER BY f.name"#,
  )
  .unwrap();
  assert_eq!(texts(&r.rows, 0), ["deserialize", "parse"]);

  // NOT binds tighter than AND; De Morgan sanity against the equivalent positive form.
  let negative = run(
    &kg,
    r#"MATCH (f:Function) WHERE NOT f.name = "main" AND NOT f.name = "parse" RETURN f.name ORDER BY f.name"#,
  )
  .unwrap();
  let positive = run(
    &kg,
    r#"MATCH (f:Function) WHERE NOT (f.name = "main" OR f.name = "parse") RETURN f.name ORDER BY f.name"#,
  )
  .unwrap();
  assert_eq!(negative.rows, positive.rows);
  assert_eq!(texts(&negative.rows, 0), ["deserialize", "validate"]);

  // NOT over a substring operator.
  let r = run(
    &kg,
    r#"MATCH (f) WHERE NOT f.path CONTAINS "src/" RETURN COUNT(*)"#,
  )
  .unwrap();
  assert_eq!(r.rows, vec![vec![Cell::Int(0)]]);

  // Plan errors surface from inside the tree.
  match run(&kg, r#"MATCH (f) WHERE f.name = "x" OR g.name = "y" RETURN f.name"#) {
    Err(QueryError::Plan(message)) => assert!(message.contains("not bound"), "{message}"),
    other => panic!("expected plan error, got {other:?}"),
  }
}

#[test]
fn multi_segment_chains() {
  let kg = fixture();

  // Forward chain from a cheap left anchor.
  let r = run(
    &kg,
    r#"MATCH (a {name: "main"})-[:calls]->(b)-[:calls]->(c) RETURN b.name, c.name"#,
  )
  .unwrap();
  assert_eq!(
    r.rows,
    vec![vec![Cell::Text("parse".into()), Cell::Text("validate".into())]]
  );

  // Right-anchored chain: the planner starts at the named end and walks the chain
  // backwards; output order is anchor-independent (rows sort by slot ids).
  let r = run(
    &kg,
    r#"MATCH (a)-[:calls]->(b)-[:calls]->(c {name: "deserialize"}) RETURN a.name, b.name"#,
  )
  .unwrap();
  assert_eq!(
    r.rows,
    vec![vec![Cell::Text("parse".into()), Cell::Text("validate".into())]]
  );

  // A var-length segment inside a chain.
  let r = run(
    &kg,
    r#"MATCH (a {name: "main"})-[:calls*1..3]->(b)-[:calls]->(c) RETURN b.name, c.name ORDER BY b.name"#,
  )
  .unwrap();
  assert_eq!(
    r.rows,
    vec![
      vec![Cell::Text("parse".into()), Cell::Text("validate".into())],
      vec![Cell::Text("validate".into()), Cell::Text("deserialize".into())],
    ]
  );

  // WHERE spans slots across the whole chain; grouping keys any slot.
  let r = run(
    &kg,
    r#"MATCH (a)-[:calls]->(b)-[:calls]->(c) WHERE c.name <> "validate" OR a.exported = false RETURN c.name, COUNT(*)"#,
  )
  .unwrap();
  assert_eq!(r.rows, vec![vec![Cell::Text("deserialize".into()), Cell::Int(1)]]);

  // Cycle constraints (a repeated variable) refuse with a teaching error.
  match run(&kg, "MATCH (a)-[:calls]->(a) RETURN a.name") {
    Err(QueryError::Plan(message)) => assert!(message.contains("bound twice"), "{message}"),
    other => panic!("expected plan error, got {other:?}"),
  }
}

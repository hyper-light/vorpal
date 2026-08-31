//! Request → route edges (ADOPTION #25 slice 2), through the real build path: HTTP client
//! call sites with literal URLs link to the `Route` nodes their paths match — across files
//! and across languages (a TS frontend calling a Go backend is one graph). Unique matches
//! only; ambiguity and external calls are counted on the report, never guessed.

use std::fs;

use vorpal_index::build_index;
use vorpal_kg::{EdgeType, Kg, NodeId, SymbolKind};

fn build(files: &[(&str, &str)], tag: &str) -> (Kg, vorpal_index::IndexReport, std::path::PathBuf) {
  let base = std::env::temp_dir().join(format!("vorpal-requests-{}-{}", tag, std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  for (name, text) in files {
    fs::write(src.join(name), text).unwrap();
  }
  let report = build_index(&src, &out).unwrap();
  let kg = Kg::load(&out).unwrap();
  (kg, report, base)
}

fn node(kg: &Kg, name: &str, kind: SymbolKind) -> NodeId {
  (0..kg.node_count() as u64)
    .map(NodeId::new)
    .find(|&n| kg.node(n).is_some_and(|v| v.name == name && v.kind == kind))
    .unwrap_or_else(|| {
      let all: Vec<String> = (0..kg.node_count() as u64)
        .filter_map(|i| kg.node(NodeId::new(i)).map(|v| format!("{} [{:?}]", v.name, v.kind)))
        .collect();
      panic!("no {kind:?} named {name:?} — nodes: {all:?}")
    })
}

#[test]
fn fetch_links_to_the_express_route_across_files() {
  let (kg, report, base) = build(
    &[
      (
        "frontend.js",
        "export function loadUsers() {\n  return fetch(\"/api/users\");\n}\n\
         export function loadUser(id) {\n  return fetch(`/api/users/${id}`);\n}\n\
         export function poll() {\n  return axios.get(\"https://other.example.com/status\");\n}\n",
      ),
      ("server.js", "function list(req, res) {}\napp.get(\"/api/users\", list);\n"),
    ],
    "express",
  );
  let route = node(&kg, "GET /api/users", SymbolKind::Route);
  let caller = node(&kg, "loadUsers", SymbolKind::Function);
  // Literal-exact match at 95; the template-literal URL records nothing, and the external
  // host is counted unmatched — stated, never silent.
  assert_eq!(kg.incoming_with_confidence(route, EdgeType::REQUESTS), vec![(caller, 95)]);
  assert_eq!(report.request_sites, 2, "{report:?}");
  assert_eq!(report.request_edges, 1);
  assert!(report.request_note.is_none());
  // The query language speaks the relation.
  let rows = vorpal_query::run(
    &kg,
    r#"MATCH (f)-[:requests]->(r:Route) RETURN f.name, r.name"#,
  )
  .unwrap();
  assert_eq!(rows.rows.len(), 1);
  let _ = fs::remove_dir_all(&base);
}

#[test]
fn python_client_links_to_a_fastapi_route_with_params() {
  let (kg, report, base) = build(
    &[
      (
        "api.py",
        "@app.get(\"/items/{item_id}\")\ndef read_item(item_id):\n    return item_id\n",
      ),
      (
        "client.py",
        "import requests\n\ndef fetch_item(n):\n    return requests.get(\"http://svc.internal/items/42\")\n",
      ),
    ],
    "fastapi",
  );
  let route = node(&kg, "GET /items/{item_id}", SymbolKind::Route);
  let caller = node(&kg, "fetch_item", SymbolKind::Function);
  // A template parameter absorbed one segment: constrained confidence.
  assert_eq!(kg.incoming_with_confidence(route, EdgeType::REQUESTS), vec![(caller, 85)]);
  assert_eq!(report.request_edges, 1, "{report:?}");
  let _ = fs::remove_dir_all(&base);
}

#[test]
fn cross_language_frontend_to_backend() {
  let (kg, report, base) = build(
    &[
      (
        "ui.ts",
        "export function ping(): Promise<Response> {\n  return fetch(\"/health\");\n}\n",
      ),
      (
        "main.go",
        "package main\n\nfunc health() {}\n\nfunc main() {\n\thttp.HandleFunc(\"/health\", health)\n}\n",
      ),
    ],
    "cross",
  );
  let route = node(&kg, "ROUTE /health", SymbolKind::Route);
  let caller = node(&kg, "ping", SymbolKind::Function);
  // The verb-less route (`ROUTE`) accepts any method; literal-exact path → 95.
  assert_eq!(kg.incoming_with_confidence(route, EdgeType::REQUESTS), vec![(caller, 95)]);
  assert_eq!(report.request_edges, 1, "{report:?}");
  let _ = fs::remove_dir_all(&base);
}

#[test]
fn ambiguous_matches_refuse_and_the_report_says_so() {
  let (kg, report, base) = build(
    &[
      (
        "server.js",
        "function a(req, res) {}\nfunction b(req, res) {}\n\
         app.get(\"/users/:id\", a);\napp.get(\"/users/:name\", b);\n",
      ),
      ("client.js", "export function load() { return fetch(\"/users/42\"); }\n"),
    ],
    "ambig",
  );
  let a = node(&kg, "GET /users/:id", SymbolKind::Route);
  let b = node(&kg, "GET /users/:name", SymbolKind::Route);
  assert!(kg.incoming_with_confidence(a, EdgeType::REQUESTS).is_empty());
  assert!(kg.incoming_with_confidence(b, EdgeType::REQUESTS).is_empty());
  assert_eq!(report.request_sites, 1);
  assert_eq!(report.request_edges, 0);
  assert!(
    report.request_note.as_deref().unwrap().contains("1 ambiguous"),
    "{:?}",
    report.request_note
  );
  let _ = fs::remove_dir_all(&base);
}

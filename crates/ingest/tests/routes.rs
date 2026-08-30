//! HTTP route nodes (ADOPTION #25, slice 1): framework route registrations become `Route`
//! items named `VERB /path`, and each route `calls` its handler — so callers, reachability,
//! impact, and dead-code all see endpoints without special cases. One block per framework;
//! the outline rule (item) and the route spec (handler edge) are pinned together here.

use vorpal_ingest::{Ingestor, Kg, NodeId, OutlineExtractor, Resolver};
use vorpal_kg::{EdgeType, SymbolKind};

fn itn() -> &'static vorpal_ingest::Interner {
  static INTERNER: std::sync::OnceLock<vorpal_ingest::Interner> = std::sync::OnceLock::new();
  INTERNER.get_or_init(vorpal_ingest::Interner::default)
}

fn kg_for(files: &[(&str, &str)]) -> Kg {
  let mut ing = Ingestor::new(itn(), OutlineExtractor::new().unwrap());
  for (path, src) in files {
    ing.ingest_source(path, src);
  }
  ing.link_and_seal(&Resolver::new()).0
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

/// The route exists and `calls` its handler.
fn assert_route_calls(kg: &Kg, route: &str, handler: NodeId) {
  let route_id = node(kg, route, SymbolKind::Route);
  let callers = kg.incoming_of(handler, EdgeType::CALLS);
  assert!(
    callers.contains(&route_id),
    "{route:?} does not call its handler; callers: {callers:?}"
  );
}

#[test]
fn express_routes_call_their_handlers() {
  let kg = kg_for(&[
    (
      "app.js",
      "import { list } from \"./handlers\";\n\
       function show(req, res) {}\n\
       app.get(\"/users/:id\", show);\n\
       app.post(\"/users\", auth, list);\n\
       router.delete(\"/users/:id\", (req, res) => {});\n\
       cache.get(\"/looks/like/a/path\");\n",
    ),
    ("handlers.js", "export function list(req, res) {}\n"),
  ]);
  assert_route_calls(&kg, "GET /users/:id", node(&kg, "show", SymbolKind::Function));
  // Middleware between path and handler is skipped; the import-bound handler resolves.
  assert_route_calls(&kg, "POST /users", node(&kg, "list", SymbolKind::Function));
  // An inline closure names nothing — the route still exists.
  node(&kg, "DELETE /users/:id", SymbolKind::Route);
  // A one-argument `.get("/…")` is a lookup, not a registration: no Route node.
  assert!(
    (0..kg.node_count() as u64)
      .filter_map(|i| kg.node(NodeId::new(i)))
      .filter(|v| v.kind == SymbolKind::Route)
      .count()
      == 3,
    "exactly the three registrations are routes"
  );
}

#[test]
fn nestjs_decorators_route_to_methods() {
  let kg = kg_for(&[(
    "cats.controller.ts",
    "class CatsController {\n  @Get(\"cats/:id\")\n  findOne(): string { return \"\" }\n}\n",
  )]);
  assert_route_calls(&kg, "GET cats/:id", node(&kg, "findOne", SymbolKind::Method));
}

#[test]
fn fastapi_and_django_routes() {
  let kg = kg_for(&[
    (
      "api.py",
      "@app.get(\"/items/{item_id}\")\ndef read_item(item_id):\n    return item_id\n",
    ),
    (
      "urls.py",
      "import views\n\nurlpatterns = [\n    path(\"users/<int:id>/\", views.detail, name=\"detail\"),\n]\n",
    ),
    ("views.py", "def detail(request, id):\n    return id\n"),
  ]);
  assert_route_calls(&kg, "GET /items/{item_id}", node(&kg, "read_item", SymbolKind::Function));
  assert_route_calls(&kg, "ROUTE /users/<int:id>/", node(&kg, "detail", SymbolKind::Function));
}

#[test]
fn go_routes_all_three_shapes() {
  let kg = kg_for(&[(
    "main.go",
    "package main\n\n\
     func health() {}\n\
     func ping() {}\n\
     func getUser() {}\n\
     func main() {\n\
     \thttp.HandleFunc(\"/health\", health)\n\
     \tr.GET(\"/ping\", ping)\n\
     \tmux.HandleFunc(\"GET /users/{id}\", getUser)\n\
     }\n",
  )]);
  assert_route_calls(&kg, "ROUTE /health", node(&kg, "health", SymbolKind::Function));
  assert_route_calls(&kg, "GET /ping", node(&kg, "ping", SymbolKind::Function));
  assert_route_calls(&kg, "GET /users/{id}", node(&kg, "getUser", SymbolKind::Function));
}

#[test]
fn rust_axum_and_attribute_routes() {
  let kg = kg_for(&[(
    "main.rs",
    "fn handler() {}\n\n\
     #[get(\"/other\")]\n\
     fn other() {}\n\n\
     fn app() {\n    let app = Router::new().route(\"/x\", get(handler));\n}\n\n\
     #[derive(Debug)]\nstruct NotARoute;\n",
  )]);
  assert_route_calls(&kg, "GET /x", node(&kg, "handler", SymbolKind::Function));
  assert_route_calls(&kg, "GET /other", node(&kg, "other", SymbolKind::Function));
}

#[test]
fn spring_and_aspnet_routes() {
  let kg = kg_for(&[
    (
      "UserController.java",
      "public class UserController {\n  @GetMapping(\"/users\")\n  public String list() { return \"\"; }\n}\n",
    ),
    (
      "ItemsController.cs",
      "public class ItemsController {\n  [HttpGet(\"items/{id}\")]\n  public string Get(int id) { return \"\"; }\n}\n",
    ),
  ]);
  assert_route_calls(&kg, "GET /users", node(&kg, "list", SymbolKind::Method));
  assert_route_calls(&kg, "GET items/{id}", node(&kg, "Get", SymbolKind::Method));
}

#[test]
fn rails_routes_are_items() {
  let kg = kg_for(&[(
    "routes.rb",
    "get \"/users\", to: \"users#index\"\npost \"/users\", to: \"users#create\"\n",
  )]);
  // Ruby routes are items (handler strings like "users#index" gain edges in a later slice).
  node(&kg, "GET /users", SymbolKind::Route);
  node(&kg, "POST /users", SymbolKind::Route);
}

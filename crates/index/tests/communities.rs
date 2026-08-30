//! The community sidecar: built by the warm path beside the search tiers, stamped to the
//! generation, read lazily by the graph (`null` until built), and surfaced through the
//! query language and the architecture summary.

use std::fs;

use vorpal_index::build_index;
use vorpal_kg::{Kg, NodeId, SymbolKind};

#[test]
fn communities_warm_and_answer() {
  let base = std::env::temp_dir().join(format!("vorpal-communities-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  // Two tightly-coupled groups bridged by one call, plus an isolated function.
  fs::write(
    src.join("a.rs"),
    "pub fn a1() { a2(); a3(); }\npub fn a2() { a1(); a3(); }\npub fn a3() { a1(); a2(); b1(); }\n",
  )
  .unwrap();
  fs::write(
    src.join("b.rs"),
    "pub fn b1() { b2(); b3(); }\npub fn b2() { b1(); b3(); }\npub fn b3() { b1(); b2(); }\npub fn lonely() {}\n",
  )
  .unwrap();
  build_index(&src, &out).unwrap();

  // Before the warm: unknown, not "alone".
  let kg = Kg::load(&out).unwrap();
  let id = |name: &str| {
    (0..kg.node_count() as u64)
      .map(NodeId::new)
      .find(|&n| kg.node(n).is_some_and(|v| v.name == name && v.kind == SymbolKind::Function))
      .unwrap_or_else(|| panic!("{name}"))
  };
  assert_eq!(kg.community(id("a1")), None);
  let r = vorpal_query::run(&kg, r#"MATCH (f {name: "a1"}) RETURN f.community"#).unwrap();
  assert_eq!(r.rows, vec![vec![vorpal_query::Cell::Null]]);

  // Warm builds the sidecar; a fresh handle sees it.
  vorpal_index::warm_ann(&out).unwrap();
  let kg = Kg::load(&out).unwrap();
  let (a1, a2, a3, b1, b2, lonely) = (id("a1"), id("a2"), id("a3"), id("b1"), id("b2"), id("lonely"));
  let c = |n: NodeId| kg.community(n).expect("sidecar built");
  assert_eq!(c(a1), c(a2));
  assert_eq!(c(a2), c(a3));
  assert_eq!(c(b1), c(b2));
  assert_ne!(c(a1), c(b1), "the bridge does not merge the groups");
  assert!(c(lonely) != c(a1) && c(lonely) != c(b1), "isolated fn is its own community");

  // Query surface: grouping by community counts the groups.
  let r = vorpal_query::run(
    &kg,
    "MATCH (f:Function) RETURN f.community AS c, count(*) AS n ORDER BY n DESC, c",
  )
  .unwrap();
  assert_eq!(r.rows[0][1], vorpal_query::Cell::Int(3));
  assert_eq!(r.rows[1][1], vorpal_query::Cell::Int(3));

  // Architecture summary lists clusters with a representative and dominant module.
  let report = vorpal_index::records::architecture_report(&kg, None, 10);
  assert!(report.clusters_note.is_none(), "{:?}", report.clusters_note);
  // Singletons are communities but not clusters: two groups of three.
  assert_eq!(report.total_clusters, 2);
  assert_eq!(report.clusters.len(), 2);
  assert_eq!(report.clusters[0].members, 3);
  assert_eq!(report.clusters[1].members, 3);
  assert!(report.clusters[0].dominant_module.ends_with("src"));

  // Stale sidecar (different generation) reads as absent: rewrite a file, rebuild, and
  // the old stamp no longer matches.
  fs::write(src.join("b.rs"), "pub fn b1() { b2(); }\npub fn b2() { b1(); }\npub fn b3() {}\npub fn lonely() {}\n").unwrap();
  build_index(&src, &out).unwrap();
  let kg = Kg::load(&out).unwrap();
  assert_eq!(kg.community(id("a1")), None, "new generation, no sidecar yet");
  let report = vorpal_index::records::architecture_report(&kg, None, 10);
  assert!(report.clusters.is_empty());
  assert!(report.clusters_note.is_some());

  let _ = fs::remove_dir_all(&base);
}

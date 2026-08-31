//! Channel nodes and `notifies` edges (ADOPTION #25 slice 3), through the real build path:
//! listener registrations become `EVENT <topic>` Channel nodes that call their handlers;
//! emitters link to EVERY matching registration (pub/sub fan-out), cross-file.

use std::fs;

use vorpal_index::build_index;
use vorpal_kg::{EdgeType, Kg, NodeId, SymbolKind};

fn build(files: &[(&str, &str)], tag: &str) -> (Kg, vorpal_index::IndexReport, std::path::PathBuf) {
  let base = std::env::temp_dir().join(format!("vorpal-channels-{}-{}", tag, std::process::id()));
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

fn nodes_named(kg: &Kg, name: &str, kind: SymbolKind) -> Vec<NodeId> {
  (0..kg.node_count() as u64)
    .map(NodeId::new)
    .filter(|&n| kg.node(n).is_some_and(|v| v.name == name && v.kind == kind))
    .collect()
}

fn node(kg: &Kg, name: &str, kind: SymbolKind) -> NodeId {
  let found = nodes_named(kg, name, kind);
  assert_eq!(found.len(), 1, "exactly one {kind:?} named {name:?}: {found:?}");
  found[0]
}

#[test]
fn emitters_fan_out_to_every_listener_and_handlers_are_called() {
  let (kg, report, base) = build(
    &[
      (
        "audit.js",
        "export function onUserCreated(user) {}\nbus.on(\"user.created\", onUserCreated);\n",
      ),
      (
        "mailer.js",
        "export function sendWelcome(user) {}\nqueue.subscribe(\"user.created\", sendWelcome);\n",
      ),
      (
        "signup.js",
        "export function signup(user) {\n  bus.emit(\"user.created\", user);\n  bus.emit(\"user.vanished\", user);\n}\n",
      ),
    ],
    "fanout",
  );
  // Two registrations of the same topic, one per file — both are Channel nodes.
  let channels = nodes_named(&kg, "EVENT user.created", SymbolKind::Channel);
  assert_eq!(channels.len(), 2, "{report:?}");
  let emitter = node(&kg, "signup", SymbolKind::Function);
  // The emitter notifies BOTH registrations.
  for &channel in &channels {
    assert_eq!(
      kg.incoming_with_confidence(channel, EdgeType::NOTIFIES),
      vec![(emitter, 90)],
      "{report:?}"
    );
  }
  // Each registration calls its handler.
  let audit = node(&kg, "onUserCreated", SymbolKind::Function);
  let mail = node(&kg, "sendWelcome", SymbolKind::Function);
  assert!(kg.incoming_of(audit, EdgeType::CALLS).iter().any(|id| channels.contains(id)));
  assert!(kg.incoming_of(mail, EdgeType::CALLS).iter().any(|id| channels.contains(id)));
  // The topic nobody listens to is counted, not guessed.
  assert_eq!(report.request_sites, 2);
  assert_eq!(report.request_edges, 2);
  let _ = fs::remove_dir_all(&base);
}

#[test]
fn go_publish_links_to_subscribe_cross_file() {
  let (kg, report, base) = build(
    &[
      (
        "listen.go",
        "package main\n\nfunc onOrder(m *Msg) {}\n\nfunc wire(nc *Conn) {\n\tnc.Subscribe(\"orders.new\", onOrder)\n}\n",
      ),
      (
        "emit.go",
        "package main\n\nfunc place(nc *Conn) {\n\tnc.Publish(\"orders.new\", nil)\n}\n",
      ),
    ],
    "go",
  );
  let channel = node(&kg, "EVENT orders.new", SymbolKind::Channel);
  let emitter = node(&kg, "place", SymbolKind::Function);
  assert_eq!(kg.incoming_with_confidence(channel, EdgeType::NOTIFIES), vec![(emitter, 90)], "{report:?}");
  let handler = node(&kg, "onOrder", SymbolKind::Function);
  assert!(kg.incoming_of(handler, EdgeType::CALLS).contains(&channel));
  let _ = fs::remove_dir_all(&base);
}

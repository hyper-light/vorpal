//! G-M1 capture semantics: receiver typing from file-local bindings, per-entity params,
//! argument records — and every conservatism rule (disagreement poisons; complex receivers
//! stay untyped; annotation beats constructor only as a LABEL, never on conflicting types).

use vorpal_ingest::OutlineExtractor;

fn product(path: &str, src: &str) -> vorpal_ingest::FileProduct {
  OutlineExtractor::new()
    .unwrap()
    .extract_product(path, src)
    .expect("extractable")
}

#[test]
fn rust_receiver_typing_and_params() {
  let src = r#"
struct Widget { size: u32 }

fn run(w: Widget, n: u32) {
    let a: Widget = make();
    let b = Widget::new();
    a.render();
    b.render();
    w.render();
    (a + b).render();
}
"#;
  let p = product("t.rs", src);
  let call = |name: &str, recv: &str| {
    p.refs
      .iter()
      .find(|r| r.name == name && r.receiver.as_deref() == Some(recv))
      .unwrap_or_else(|| panic!("call {name} via {recv} extracted"))
  };
  // let a: Widget  → annotated
  let a = call("render", "a");
  assert_eq!(a.receiver_type.as_deref(), Some("Widget"), "{a:?}");
  assert_eq!(a.receiver_type_origin, 0, "annotated tag");
  // let b = Widget::new() → constructed
  let b = call("render", "b");
  assert_eq!(b.receiver_type.as_deref(), Some("Widget"));
  assert_eq!(b.receiver_type_origin, 1, "constructed tag");
  // typed param w: Widget → param
  let w = call("render", "w");
  assert_eq!(w.receiver_type.as_deref(), Some("Widget"));
  assert_eq!(w.receiver_type_origin, 2, "param tag");
  // complex receiver (a + b) → no receiver captured at all
  assert!(
    p.refs
      .iter()
      .filter(|r| r.name == "render")
      .any(|r| r.receiver.is_none()),
    "complex receivers stay untyped"
  );
  // entity_params: run's params recorded with their types, in order.
  let (_, params) = p
    .entity_params
    .iter()
    .find(|(_, ps)| ps.iter().any(|(n, _)| n == "w"))
    .expect("run's params recorded");
  assert!(params.contains(&("w".to_string(), Some("Widget".to_string()))), "{params:?}");
  assert!(params.contains(&("n".to_string(), Some("u32".to_string()))), "{params:?}");
}

#[test]
fn disagreement_poisons_and_python_kwargs_record() {
  let src = r#"
def run(w: Widget):
    x: A = mk()
    x = B()
    x.render()
    w.render(1, key=w, s="lit")
"#;
  let p = product("t.py", src);
  // x bound to A (annotated) and B (constructed) → poisoned, no type.
  let x = p
    .refs
    .iter()
    .find(|r| r.name == "render" && r.receiver.as_deref() == Some("x"))
    .expect("x.render extracted");
  assert_eq!(x.receiver_type, None, "disagreeing bindings poison: {x:?}");
  // w: Widget param types the other call, and its args carry class + kw records.
  let w = p
    .refs
    .iter()
    .find(|r| r.name == "render" && r.receiver.as_deref() == Some("w"))
    .expect("w.render extracted");
  assert_eq!(w.receiver_type.as_deref(), Some("Widget"));
  assert_eq!(w.receiver_type_origin, 2);
  assert_eq!(w.args.len(), 3, "{:?}", w.args);
  assert_eq!(w.args[1].kw_name.as_deref(), Some("key"));
  assert_eq!(w.args[1].class, 0, "bare-name kwarg is a Var");
  assert_eq!(w.args[1].expr.as_deref(), Some("w"));
  assert_eq!(w.args[2].class, 3, "string literal");
  assert_eq!(w.args[2].expr, None, "literals carry no expression text");
}

#[test]
fn typescript_annotations_and_new_expressions() {
  let src = r#"
class Widget { render(): void {} }
function run(w: Widget) {
  const a: Widget = mk();
  const b = new Widget();
  a.render();
  b.render();
  w.render();
}
"#;
  let p = product("t.ts", src);
  for (recv, origin) in [("a", 0u8), ("b", 1u8), ("w", 2u8)] {
    let r = p
      .refs
      .iter()
      .find(|r| r.name == "render" && r.receiver.as_deref() == Some(recv))
      .unwrap_or_else(|| panic!("{recv}.render extracted"));
    assert_eq!(r.receiver_type.as_deref(), Some("Widget"), "{recv}: {r:?}");
    assert_eq!(r.receiver_type_origin, origin, "{recv}");
  }
}

#[test]
fn v14_round_trips_the_new_fields_bit_exactly() {
  let src = "struct W;\nfn run(w: W) {\n    let a: W = mk();\n    a.go(w, 1);\n}\n";
  let mut p = product("t.rs", src);
  p.source_size = 77;
  p.source_mtime_ns = 88;
  let mut buf = Vec::new();
  vorpal_ingest::encode_product_into(&p, &mut buf);
  assert!(!buf.is_empty());
  let back = vorpal_ingest::decode_product(&buf).expect("decodes");
  assert_eq!(back.refs, p.refs, "owned round-trip");
  assert_eq!(back.entity_params, p.entity_params);
  // The zero-copy view agrees, including lazily decoded args.
  let view = vorpal_ingest::decode_product_view(&buf).expect("view decodes");
  for (owned, viewed) in p.refs.iter().zip(&view.refs) {
    assert_eq!(viewed.receiver, owned.receiver.as_deref());
    assert_eq!(viewed.receiver_type, owned.receiver_type.as_deref());
    assert_eq!(viewed.receiver_type_origin, owned.receiver_type_origin);
    let args: Vec<_> = viewed.args().collect();
    assert_eq!(args.len(), owned.args.len());
    for (ov, vv) in owned.args.iter().zip(&args) {
      assert_eq!(vv.index, ov.index);
      assert_eq!(vv.class, ov.class);
      assert_eq!(vv.kw_name, ov.kw_name.as_deref());
      assert_eq!(vv.expr, ov.expr.as_deref());
    }
  }
}

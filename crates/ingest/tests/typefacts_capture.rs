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
fn python_param_ledger_lists_every_parameter_in_order() {
  // G-M5: the kwarg binder needs the FULL signature — untyped, defaulted, and splat
  // parameters included, in declaration order, splats keeping their sigils.
  let src = "def blend(alpha, beta: int, gamma=0, *args, **kwargs):\n    return alpha\n\n\
             class P:\n    def draw(self, x):\n        return x\n";
  let p = product("t.py", src);
  let ledgers: Vec<Vec<&str>> = p
    .entity_params
    .iter()
    .map(|(_, params)| params.iter().map(|(name, _)| name.as_str()).collect())
    .collect();
  assert!(
    ledgers.contains(&vec!["alpha", "beta", "gamma", "*args", "**kwargs"]),
    "{ledgers:?}"
  );
  assert!(ledgers.contains(&vec!["self", "x"]), "{ledgers:?}");
  // Typed entries still carry their annotation (the receiver-typing map feeds off them).
  let blend = p
    .entity_params
    .iter()
    .find(|(_, params)| params.first().is_some_and(|(n, _)| n == "alpha"))
    .expect("blend ledger");
  assert_eq!(blend.1[1], ("beta".to_string(), Some("int".to_string())));
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
fn v15_round_trips_the_new_fields_bit_exactly() {
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

#[test]
fn go_receiver_typing_from_params_vars_and_composites() {
  let src = r#"package m

func run(w Widget, n int) {
	w.Render()
	var v Widget
	v.Draw()
	x := Widget{}
	x.Paint()
	p := &Widget{}
	p.Fill()
}
"#;
  let p = product("t.go", src);
  let by_receiver = |recv: &str| {
    p.refs
      .iter()
      .find(|r| r.receiver.as_deref() == Some(recv))
      .unwrap_or_else(|| panic!("{recv} call extracted"))
  };
  let w = by_receiver("w");
  assert_eq!(w.receiver_type.as_deref(), Some("Widget"), "{w:?}");
  assert_eq!(w.receiver_type_origin, 2, "param-typed");
  let v = by_receiver("v");
  assert_eq!(v.receiver_type.as_deref(), Some("Widget"));
  assert_eq!(v.receiver_type_origin, 0, "var annotation");
  let x = by_receiver("x");
  assert_eq!(x.receiver_type.as_deref(), Some("Widget"), "{x:?}");
  assert_eq!(x.receiver_type_origin, 1, "composite literal is constructor-shaped");
  let ptr = by_receiver("p");
  assert_eq!(ptr.receiver_type.as_deref(), Some("Widget"), "&Widget{{}} unwraps: {ptr:?}");
}

#[test]
fn java_receiver_typing_from_declarations_params_fields_and_var() {
  let src = r#"class Painter {
  Widget canvas;

  void run(Widget p) {
    Widget x = make();
    x.render();
    p.render();
    canvas.render();
    var y = new Widget();
    y.render();
    List<Widget> zs = mk();
    zs.iterate();
  }
}
"#;
  let p = product("T.java", src);
  let by_receiver = |recv: &str| {
    p.refs
      .iter()
      .find(|r| r.receiver.as_deref() == Some(recv))
      .unwrap_or_else(|| panic!("{recv} call extracted"))
  };
  let x = by_receiver("x");
  assert_eq!(x.receiver_type.as_deref(), Some("Widget"));
  assert_eq!(x.receiver_type_origin, 0, "declared type");
  let par = by_receiver("p");
  assert_eq!(par.receiver_type.as_deref(), Some("Widget"));
  assert_eq!(par.receiver_type_origin, 2, "formal parameter");
  let field = by_receiver("canvas");
  assert_eq!(field.receiver_type.as_deref(), Some("Widget"));
  assert_eq!(field.receiver_type_origin, 3, "field declaration");
  let var = by_receiver("y");
  assert_eq!(var.receiver_type.as_deref(), Some("Widget"), "{var:?}");
  assert_eq!(var.receiver_type_origin, 1, "var + new Widget() is constructor-shaped");
  let generic = by_receiver("zs");
  assert_eq!(generic.receiver_type.as_deref(), Some("List"), "generics strip: {generic:?}");
}

#[test]
fn rust_reference_params_strip_to_the_owner_name() {
  // `&Widget` / `&mut Widget` annotations must meet the owner comparison — the sigils are
  // stripped at capture (v3), which is what lets `fn f(w: &Widget)` type `w.draw()`.
  let src = "struct Widget;\nfn f(a: &Widget, b: &mut Widget, c: Box<Widget>) {\n    a.draw();\n    b.draw();\n    c.draw();\n}\n";
  let p = product("t.rs", src);
  let by_receiver = |recv: &str| {
    p.refs
      .iter()
      .find(|r| r.receiver.as_deref() == Some(recv))
      .unwrap_or_else(|| panic!("{recv} call extracted"))
  };
  assert_eq!(by_receiver("a").receiver_type.as_deref(), Some("Widget"));
  assert_eq!(by_receiver("b").receiver_type.as_deref(), Some("Widget"));
  assert_eq!(by_receiver("c").receiver_type.as_deref(), Some("Box"), "wrapper name, not the parameter");
}

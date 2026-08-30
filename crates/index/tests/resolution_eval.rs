//! The labelled resolution-evaluation harness (IMPROVEMENTS §5): per-language fixtures whose
//! expected resolution outcomes are **exhaustively** hand-labelled, measured against the
//! evidence sidecar — which is exactly the emitted-resolution-edge population, so precision
//! has an honest denominator.
//!
//! The published contract these tests enforce:
//! - **Precision = 1.0 and recall = 1.0 on the labelled fixtures**, per edge, including each
//!   edge's *grade* (exact/constrained/heuristic) and *resolver reason* — a resolver change
//!   that shifts any labelled outcome fails here and must justify the new labels explicitly.
//! - **Expected-absent references stay absent**: externals and masked references never gain a
//!   faked edge.
//!
//! Fixtures are the contract, not a field sample: real corpora carry unresolvable ambiguity
//! these labels deliberately exclude. The harness is the gate for resolver-semantics changes
//! (scope/import-aware tables land against these labels plus new ones proving the upgrade).

use std::fs;

use vorpal_index::build_index;
use vorpal_ingest::{Confidence, ResolveReason};
use vorpal_kg::{EdgeType, Kg};

/// One labelled edge: (from name, to name, edge type, grade label, reason label). File nodes
/// are labelled by path basename.
type Labelled = (
  &'static str,
  &'static str,
  &'static str,
  &'static str,
  &'static str,
);

struct Fixture {
  lang: &'static str,
  files: &'static [(&'static str, &'static str)],
  /// The complete expected edge set — every resolution edge the fixture must produce.
  expected: &'static [Labelled],
  /// References that must produce **no** edge: (enclosing definition name, referenced name).
  absent: &'static [(&'static str, &'static str)],
}

/// Basename-normalize a node name (file nodes are full paths; definitions are identifiers).
fn short(name: &str) -> String {
  name.rsplit('/').next().unwrap_or(name).to_string()
}

fn run(fixture: &Fixture) {
  let base = std::env::temp_dir().join(format!(
    "vorpal-reseval-{}-{}",
    fixture.lang,
    std::process::id()
  ));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  for (name, source) in fixture.files {
    fs::write(src.join(name), source).unwrap();
  }
  build_index(&src, &out).unwrap();
  let kg = Kg::load(&out).unwrap();

  // The emitted EDGE population, normalized to labelled tuples (no-edge outcome rows are
  // measured separately below, against the absent list).
  let all_rows = kg.all_evidence();
  let emitted: Vec<(String, String, String, String, String)> = all_rows
    .iter()
    .filter(|row| row.outcome == vorpal_kg::EvidenceOutcome::Edge)
    .map(|row| {
      let name_of = |id: u32| {
        kg.node(vorpal_kg::NodeId::new(id as u64))
          .map(|v| short(v.name))
          .unwrap_or_else(|| format!("<missing:{id}>"))
      };
      (
        name_of(row.from),
        name_of(row.to),
        EdgeType(row.etype).name().to_string(),
        Confidence(row.confidence).grade().label().to_string(),
        ResolveReason::from_tag(row.reason).label().to_string(),
      )
    })
    .collect();

  let expected: Vec<(String, String, String, String, String)> = fixture
    .expected
    .iter()
    .map(|&(f, t, e, g, r)| {
      (
        f.to_string(),
        t.to_string(),
        e.to_string(),
        g.to_string(),
        r.to_string(),
      )
    })
    .collect();

  // Precision/recall over the labelled truth (set semantics; fixtures use single occurrences).
  let hits = emitted.iter().filter(|e| expected.contains(e)).count();
  let precision = hits as f64 / emitted.len().max(1) as f64;
  let recall = hits as f64 / expected.len().max(1) as f64;
  println!(
    "[{}] precision {:.3} ({hits}/{}) recall {:.3} ({hits}/{})",
    fixture.lang,
    precision,
    emitted.len(),
    recall,
    expected.len()
  );
  let unexpected: Vec<_> = emitted.iter().filter(|e| !expected.contains(e)).collect();
  let missing: Vec<_> = expected.iter().filter(|e| !emitted.contains(e)).collect();
  assert!(
    unexpected.is_empty(),
    "[{}] unlabelled edges emitted (precision breach):\n{unexpected:#?}",
    fixture.lang
  );
  assert!(
    missing.is_empty(),
    "[{}] labelled edges not emitted (recall breach):\n{missing:#?}\nemitted:\n{emitted:#?}",
    fixture.lang
  );

  // Expected-absent pairs: no edge may connect them — and (v2) the absence itself must be
  // retained as a no-edge evidence row with the referenced name's hash.
  for &(from, name) in fixture.absent {
    let offender = emitted
      .iter()
      .find(|(f, t, ..)| f == from && t.contains(name));
    assert!(
      offender.is_none(),
      "[{}] '{from}' must not resolve '{name}', but: {offender:?}",
      fixture.lang
    );
    let name_hash = xxhash_rust::xxh3::xxh3_64(name.as_bytes()) as u32;
    let recorded = all_rows.iter().any(|row| {
      row.outcome != vorpal_kg::EvidenceOutcome::Edge
        && row.name_hash == name_hash
        && kg
          .node(vorpal_kg::NodeId::new(row.from as u64))
          .is_some_and(|v| short(v.name) == from)
    });
    assert!(
      recorded,
      "[{}] the absence of '{from}' → '{name}' must itself be retained as evidence",
      fixture.lang
    );
  }
  let _ = fs::remove_dir_all(&base);
}

#[test]
fn rust_resolution_meets_published_labels() {
  run(&Fixture {
    lang: "rust",
    files: &[
      (
        "r_def.rs",
        "pub fn r_target() -> u32 { 1 }\npub struct RThing;\nfn r_private() -> u32 { 2 }\n\
         pub fn r_plain() -> u32 { 8 }\n",
      ),
      (
        "r_use.rs",
        "pub fn r_caller() -> u32 { r_target() }\n\
         pub fn r_local_a() -> u32 { r_local_b() }\n\
         pub fn r_local_b() -> u32 { 3 }\n\
         pub fn r_uses_thing(_t: RThing) -> u32 { 4 }\n\
         pub fn r_ext() -> u32 { total_mystery_fn() }\n\
         pub fn r_masked() -> u32 { r_private() }\n\
         pub fn r_amb_caller() -> u32 { r_shared() }\n",
      ),
      // Two same-named exports in two other files: from a third file the call is a genuine
      // tie — labelled approximate, deterministic min-id target. (A caller *inside* one of
      // these files would bind locally instead — local-first precedence, which this fixture's
      // first draft mislabelled and the harness caught.)
      ("r_amb1.rs", "pub fn r_shared() -> u32 { 10 }\n"),
      ("r_amb2.rs", "pub fn r_shared() -> u32 { 11 }\n"),
      // `use` paths carry their module prefix as a qualifier. `r_shared` is defined in both
      // amb files, so `use crate::r_amb2::r_shared` binding at qualifier-match (not a tie)
      // proves the path disambiguated the conflict — and the bare `r_shared()` call in
      // `r_imp_user` inherits that binding (import-bound), while the textually identical
      // call in `r_amb_caller` (a file with no such use) stays a labelled blind tie: the
      // same call text resolves differently because file scope differs. Importing from
      // r_amb2 — not the min-node-id file a blind tie would pick — makes the target flip
      // real, not coincidental. `use vendored::r_caller` names a module outside the corpus:
      // it must NOT fall back to the same-named local definition.
      (
        "r_imp.rs",
        "use crate::r_def::r_target;\n\
         use crate::r_amb2::r_shared;\n\
         use crate::r_def::r_plain as rp;\n\
         use vendored::r_caller;\n\
         pub fn r_imp_user() -> u32 { r_shared() }\n\
         pub fn r_alias_user() -> u32 { rp() }\n",
      ),
    ],
    expected: &[
      ("r_caller", "r_target", "calls", "constrained", "visible-export"),
      ("r_local_a", "r_local_b", "calls", "exact", "same-file"),
      ("r_uses_thing", "RThing", "of_type", "constrained", "visible-export"),
      ("r_amb_caller", "r_shared", "calls", "heuristic", "visible-tie"),
      ("r_imp.rs", "r_target", "imports", "constrained", "qualifier-match"),
      ("r_imp.rs", "r_shared", "imports", "constrained", "qualifier-match"),
      ("r_imp_user", "r_shared", "calls", "constrained", "import-bound"),
      // Aliased use: the import edge targets the original; the call to the alias `rp` —
      // defined nowhere — binds through the file's import.
      ("r_imp.rs", "r_plain", "imports", "constrained", "qualifier-match"),
      ("r_alias_user", "r_plain", "calls", "constrained", "import-bound"),
    ],
    absent: &[
      ("r_ext", "total_mystery_fn"), // defined nowhere — external, no edge
      ("r_masked", "r_private"),     // private to a sibling file — masked, no edge
      // The external-module use stays masked despite a same-named local definition.
      ("r_imp.rs", "r_caller"),
    ],
  });
}

#[test]
fn python_resolution_meets_published_labels() {
  run(&Fixture {
    lang: "python",
    files: &[
      (
        "p_def.py",
        "def p_target():\n    return 1\ndef p_shade():\n    return 3\n\
         def p_via_alias():\n    return 6\n",
      ),
      (
        "p_use.py",
        "def p_caller():\n    return p_target()\n\
         def p_local_a():\n    return p_local_b()\n\
         def p_local_b():\n    return 2\n\
         def p_ext():\n    return never_defined_fn()\n",
      ),
      // From-imports carry their module as a qualifier. `p_shared` is defined in BOTH amb
      // files, so `from p_amb2 import p_shared` binding at qualifier-match (not visible-tie)
      // proves the module disambiguated the conflict: the p_amb1 symbol cannot match the
      // qualifier, so a mispick would surface as a different reason and fail these labels.
      // Importing from p_amb2 — the file that does NOT hold the lower node id — makes the
      // disambiguation non-vacuous: a blind tie would have picked p_amb1's symbol, so the
      // qualifier (and the import-bound call in `p_imp_user` that inherits the binding)
      // provably lands on a different target than the old guess.
      // `from vendored_dep import p_caller` names a module outside the corpus: it must NOT
      // fall back to the coincidentally-named local `p_caller`.
      // The aliased import rebinds under its LOCAL name: the import edge still targets the
      // original definition, and the bare call to the ALIAS — a name defined nowhere —
      // resolves import-bound through the file's own import.
      (
        "p_imp.py",
        "from p_def import p_target\n\
         from p_amb2 import p_shared\n\
         from p_def import p_via_alias as local_alias\n\
         from vendored_dep import p_caller\n\
         def p_imp_user():\n    return p_shared()\n\
         def p_alias_user():\n    return local_alias()\n",
      ),
      ("p_amb1.py", "def p_shared():\n    return 10\n"),
      ("p_amb2.py", "def p_shared():\n    return 11\n"),
      // Receiver hints: `PBox.p_make()` names class PBox, which owns p_make — corroborated
      // like a static qualifier. `mystery.p_make()` names nothing (an opaque variable), so
      // the hint falls back to plain Method semantics: p_make is unique among visible
      // definitions, and the unique member binds at cross-file grade as before the hint
      // machinery existed.
      (
        "p_box.py",
        "class PBox:\n    def p_make(self):\n        return 1\n",
      ),
      (
        "p_recv.py",
        "def p_recv_hinted(b):\n    return PBox.p_make()\n\
         def p_recv_opaque(mystery):\n    return mystery.p_make()\n",
      ),
      // A local definition shadows the file's own import of the same name: the import edge
      // still targets p_def's `p_shade` (the qualifier says so), but the bare call binds
      // same-file, never import-bound.
      (
        "p_shadow.py",
        "from p_def import p_shade\n\
         def p_shade():\n    return 5\n\
         def p_shadow_user():\n    return p_shade()\n",
      ),
    ],
    expected: &[
      ("p_caller", "p_target", "calls", "constrained", "visible-export"),
      ("p_local_a", "p_local_b", "calls", "exact", "same-file"),
      ("p_imp.py", "p_target", "imports", "constrained", "qualifier-match"),
      ("p_imp.py", "p_shared", "imports", "constrained", "qualifier-match"),
      ("p_imp_user", "p_shared", "calls", "constrained", "import-bound"),
      ("p_imp.py", "p_via_alias", "imports", "constrained", "qualifier-match"),
      ("p_alias_user", "p_via_alias", "calls", "constrained", "import-bound"),
      ("p_shadow.py", "p_shade", "imports", "constrained", "qualifier-match"),
      ("p_shadow_user", "p_shade", "calls", "exact", "same-file"),
      ("p_recv_hinted", "p_make", "calls", "constrained", "qualifier-match"),
      ("p_recv_opaque", "p_make", "calls", "constrained", "visible-export"),
    ],
    absent: &[
      ("p_ext", "never_defined_fn"),
      // The external-module import stays masked despite a same-named local definition.
      ("p_imp.py", "p_caller"),
    ],
  });
}

#[test]
fn typescript_resolution_meets_published_labels() {
  run(&Fixture {
    lang: "typescript",
    files: &[
      (
        "t_def.ts",
        "export function tTarget(): number { return 1 }\n",
      ),
      (
        "t_use.ts",
        "import { tTarget } from './t_def'\n\
         export function tCaller(): number { return tTarget() }\n\
         function tLocalA(): number { return tLocalB() }\n\
         function tLocalB(): number { return 2 }\n\
         export function tExt(): number { return neverDefinedFn() }\n",
      ),
    ],
    expected: &[
      // The path-form import binds the importing file to the imported file node.
      ("t_use.ts", "t_def.ts", "imports", "constrained", "import-path"),
      ("tCaller", "tTarget", "calls", "constrained", "visible-export"),
      ("tLocalA", "tLocalB", "calls", "exact", "same-file"),
    ],
    absent: &[("tExt", "neverDefinedFn")],
  });
}

#[test]
fn go_resolution_meets_published_labels() {
  run(&Fixture {
    lang: "go",
    files: &[
      ("g_def.go", "package main\n\nfunc GTarget() int { return 1 }\n"),
      (
        "g_use.go",
        "package main\n\nfunc GCaller() int { return GTarget() }\n\n\
         func gLocalA() int { return gLocalB() }\n\n\
         func gLocalB() int { return 2 }\n\n\
         func gExt() int { return neverDefinedFn() }\n",
      ),
    ],
    expected: &[
      ("GCaller", "GTarget", "calls", "constrained", "visible-export"),
      ("gLocalA", "gLocalB", "calls", "exact", "same-file"),
    ],
    absent: &[("gExt", "neverDefinedFn")],
  });
}

#[test]
fn java_resolution_meets_published_labels() {
  run(&Fixture {
    lang: "java",
    files: &[
      (
        "JDef.java",
        "public class JDef {\n  public static int jTarget() { return 1; }\n}\n",
      ),
      (
        "JUse.java",
        "public class JUse {\n  \
         public static int jCaller() { return JDef.jTarget(); }\n  \
         public static int jLocalA() { return jLocalB(); }\n  \
         public static int jLocalB() { return 2; }\n  \
         public static int jExt() { return neverDefinedFn(); }\n\
         }\n",
      ),
    ],
    expected: &[
      // The receiver-hint upgrade, exactly as this fixture predicted: `JDef.jTarget()`
      // carries its receiver as an owner hint, class JDef owns jTarget, and the resolution
      // is corroborated — qualifier-match, no longer a bare visible-export coincidence.
      ("jCaller", "jTarget", "calls", "constrained", "qualifier-match"),
      ("jLocalA", "jLocalB", "calls", "exact", "same-file"),
    ],
    absent: &[("jExt", "neverDefinedFn")],
  });
}

#[test]
fn c_resolution_meets_published_labels() {
  run(&Fixture {
    lang: "c",
    files: &[
      (
        "c_def.c",
        "int c_target(void) { return 1; }\nstatic int c_priv(void) { return 2; }\n\
         static int c_tu_local = 3;\nint c_shared_var = 4;\n",
      ),
      (
        "c_use.c",
        "int c_caller(void) { return c_target(); }\n\
         int c_local_a(void) { return c_local_b(); }\n\
         int c_local_b(void) { return 2; }\n\
         int c_ext(void) { return never_defined_fn(); }\n\
         int c_masked(void) { return c_priv(); }\n\
         int c_var_masked(void) { return c_tu_local(); }\n",
      ),
    ],
    expected: &[
      ("c_caller", "c_target", "calls", "constrained", "visible-export"),
      ("c_local_a", "c_local_b", "calls", "exact", "same-file"),
    ],
    absent: &[
      ("c_ext", "never_defined_fn"),
      ("c_masked", "c_priv"), // `static` is translation-unit-local — masked, no edge
      // A `static` file-scope VARIABLE is TU-local too: before the linkage fix this call's
      // only candidate was c_def.c's static variable and the resolver minted a nonsense
      // call→variable edge across files; now it is masked. (c_shared_var above stays
      // exported — non-static file-scope variables keep external linkage.)
      ("c_var_masked", "c_tu_local"),
    ],
  });
}

#[test]
fn cpp_resolution_meets_published_labels() {
  run(&Fixture {
    lang: "cpp",
    files: &[
      (
        "x_def.cpp",
        "int x_target() { return 1; }\nstatic int x_priv() { return 2; }\n",
      ),
      (
        "x_use.cpp",
        "int x_caller() { return x_target(); }\n\
         int x_masked() { return x_priv(); }\n",
      ),
    ],
    expected: &[
      ("x_caller", "x_target", "calls", "constrained", "visible-export"),
    ],
    absent: &[
      ("x_masked", "x_priv"), // `static` at namespace scope is TU-local — masked
    ],
  });
}

#[test]
fn typed_receivers_meet_published_labels() {
  // G-M2: the receiver's file-locally bound type narrows same-named methods that untyped
  // Method-form resolution must refuse. Every origin class is labelled, the Python cap is
  // pinned, unknown types fall through (upgrade, never veto), and same-type duplicates tie.
  run(&Fixture {
    lang: "rust-typed",
    files: &[
      (
        "tw.rs",
        "pub struct Widget;\nimpl Widget {\n  pub fn render(&self) -> u32 { 1 }\n}\n\
         pub struct Gadget;\nimpl Gadget {\n  pub fn render(&self) -> u32 { 2 }\n}\n",
      ),
      (
        "tuse.rs",
        "use crate::tw::Widget;\nuse crate::tw::Gadget;\n\
         pub fn t_annotated() -> u32 {\n    let wa: Widget = make();\n    wa.render()\n}\n\
         pub fn t_constructed() -> u32 {\n    let gc = Gadget::new();\n    gc.render()\n}\n\
         pub fn t_param(wp: Widget) -> u32 {\n    wp.render()\n}\n\
         pub fn t_unknown() -> u32 {\n    let x: Mystery = make();\n    x.render()\n}\n",
      ),
    ],
    expected: &[
      // Annotated in a checked language, cross-file target: compiler-grade evidence.
      ("t_annotated", "render", "calls", "constrained", "receiver-annotated"),
      // Constructor-inferred: capped at TYPE_BOUND — still constrained, differently earned.
      ("t_constructed", "render", "calls", "constrained", "receiver-constructed"),
      ("t_param", "render", "calls", "constrained", "receiver-param-typed"),
      // Exhaustive-labelling contract: the use-lines' imports AND the annotations' type
      // uses (an annotation is itself an of_type reference).
      ("tuse.rs", "Widget", "imports", "constrained", "qualifier-match"),
      ("tuse.rs", "Gadget", "imports", "constrained", "qualifier-match"),
      ("t_annotated", "Widget", "of_type", "constrained", "import-bound"),
      ("t_param", "Widget", "of_type", "constrained", "import-bound"),
    ],
    absent: &[
      // Mystery names no in-tree owner: the hint upgrades, never vetoes — and untyped
      // Method form on a two-way tie must refuse, exactly as before G-M2.
      ("t_unknown", "render"),
    ],
  });
}

#[test]
fn typed_receivers_python_cap_and_tie() {
  run(&Fixture {
    lang: "python-typed",
    files: &[
      (
        "pw.py",
        "class Widget:\n    def render(self):\n        return 1\n\n\
         class Gadget:\n    def render(self):\n        return 2\n",
      ),
      (
        "puse.py",
        "from pw import Widget\n\n\
         def p_annotated():\n    w: Widget = make()\n    return w.render()\n\n\
         def p_constructed():\n    g = Widget()\n    return g.render()\n",
      ),
    ],
    expected: &[
      // Python annotations are unenforced hints: BOTH origins cap at TYPE_BOUND (grade
      // constrained), and the labels say how each was earned.
      ("p_annotated", "render", "calls", "constrained", "receiver-annotated"),
      ("p_constructed", "render", "calls", "constrained", "receiver-constructed"),
      // The constructor call itself is a call edge to the class, bound via the import.
      ("p_constructed", "Widget", "calls", "constrained", "import-bound"),
      ("puse.py", "Widget", "imports", "constrained", "qualifier-match"),
    ],
    absent: &[],
  });
}

#[test]
fn typed_receivers_typescript_and_same_type_tie() {
  run(&Fixture {
    lang: "ts-typed",
    files: &[
      (
        "tsw.ts",
        "export class Widget { render(): number { return 1 } }\n\
         export class Gadget { render(): number { return 2 } }\n",
      ),
      (
        "tsuse.ts",
        "import { Widget } from \"./tsw\";\n\
         export function s_new(): number {\n  const b = new Widget();\n  return b.render();\n}\n",
      ),
    ],
    expected: &[
      ("s_new", "render", "calls", "constrained", "receiver-constructed"),
      ("tsuse.ts", "tsw.ts", "imports", "constrained", "import-path"),
    ],
    absent: &[],
  });
}

#[test]
fn typed_receivers_go_all_origins() {
  // G-M5: Go joins the capture set — annotations (`var v Widget`), composite literals
  // (`x := Gadget{}`) and typed parameters are captured into products (pinned by the
  // typefacts_capture suite). The RESOLVER upgrade is blocked on Go method OWNERSHIP:
  // `func (w Widget) Render()` is a top-level item in the outline (not a member of
  // Widget — Go methods aren't syntactically inside their type, and parentRuleIds
  // attaches by enclosure), so owner==receiver_type has no owner to meet and the
  // same-named tie refuses, exactly as before. Labels below pin TODAY's truth; the
  // calls land when Go method ownership does (follow-up work item).
  run(&Fixture {
    lang: "go-typed",
    files: &[
      (
        "gw.go",
        "package m\n\ntype Widget struct{}\n\nfunc (w Widget) Render() int { return 1 }\n\n\
         type Gadget struct{}\n\nfunc (g Gadget) Render() int { return 2 }\n",
      ),
      (
        "guse.go",
        "package m\n\nfunc gAnnotated() int {\n\tvar v Widget\n\treturn v.Render()\n}\n\n\
         func gComposite() int {\n\tx := Gadget{}\n\treturn x.Render()\n}\n\n\
         func gParam(p Widget) int {\n\treturn p.Render()\n}\n",
      ),
    ],
    expected: &[
      // Type mentions resolve (annotation, composite literal, parameter, receivers).
      ("gAnnotated", "Widget", "of_type", "constrained", "visible-export"),
      ("gComposite", "Gadget", "of_type", "constrained", "visible-export"),
      ("gParam", "Widget", "of_type", "constrained", "visible-export"),
      ("Render", "Widget", "of_type", "exact", "same-file"),
      ("Render", "Gadget", "of_type", "exact", "same-file"),
    ],
    absent: &[
      // Two same-named methods, no ownership to narrow by → the tie refuses (conservative;
      // see the header comment). These flip to expected edges with Go method ownership.
      ("gAnnotated", "Render"),
      ("gComposite", "Render"),
      ("gParam", "Render"),
    ],
  });
}

#[test]
fn typed_receivers_java_all_origins() {
  // G-M5: Java joins the capture set — declared locals, fields, formal parameters, and
  // `var` + constructor recovery all narrow; generic heads strip to the owner name.
  run(&Fixture {
    lang: "java-typed",
    files: &[
      (
        "Jw.java",
        "class Widget {\n  int render() { return 1; }\n}\n\
         class Gadget {\n  int render() { return 2; }\n}\n",
      ),
      (
        "Juse.java",
        "class Runner {\n  Widget canvas;\n\n\
         \x20 int jAnnotated() {\n    Widget x = make();\n    return x.render();\n  }\n\n\
         \x20 int jVar() {\n    var y = new Widget();\n    return y.render();\n  }\n\n\
         \x20 int jParam(Gadget p) {\n    return p.render();\n  }\n\n\
         \x20 int jField() {\n    return canvas.render();\n  }\n}\n",
      ),
    ],
    expected: &[
      ("jAnnotated", "render", "calls", "constrained", "receiver-annotated"),
      ("jVar", "render", "calls", "constrained", "receiver-constructed"),
      ("jParam", "render", "calls", "constrained", "receiver-param-typed"),
      ("jField", "render", "calls", "constrained", "receiver-field-typed"),
      // Exhaustive-labelling contract: the declarations' type mentions.
      ("canvas", "Widget", "of_type", "constrained", "visible-export"),
      ("jAnnotated", "Widget", "of_type", "constrained", "visible-export"),
      ("jVar", "Widget", "of_type", "constrained", "visible-export"),
      ("jParam", "Gadget", "of_type", "constrained", "visible-export"),
    ],
    absent: &[],
  });
}

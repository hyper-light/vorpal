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
        "pub fn r_target() -> u32 { 1 }\npub struct RThing;\nfn r_private() -> u32 { 2 }\n",
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
         use vendored::r_caller;\n\
         pub fn r_imp_user() -> u32 { r_shared() }\n",
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
        "def p_target():\n    return 1\ndef p_shade():\n    return 3\n",
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
      (
        "p_imp.py",
        "from p_def import p_target\n\
         from p_amb2 import p_shared\n\
         from vendored_dep import p_caller\n\
         def p_imp_user():\n    return p_shared()\n",
      ),
      ("p_amb1.py", "def p_shared():\n    return 10\n"),
      ("p_amb2.py", "def p_shared():\n    return 11\n"),
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
      ("p_shadow.py", "p_shade", "imports", "constrained", "qualifier-match"),
      ("p_shadow_user", "p_shade", "calls", "exact", "same-file"),
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
      // Today the extraction does not capture the `JDef.` qualifier for Java static calls, so
      // the resolver takes the bare visible-export path (unique target — same edge, same
      // grade). When Java qualifier capture lands, this label flips to qualifier-match and
      // this harness gates the change.
      ("jCaller", "jTarget", "calls", "constrained", "visible-export"),
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
        "int c_target(void) { return 1; }\nstatic int c_priv(void) { return 2; }\n",
      ),
      (
        "c_use.c",
        "int c_caller(void) { return c_target(); }\n\
         int c_local_a(void) { return c_local_b(); }\n\
         int c_local_b(void) { return 2; }\n\
         int c_ext(void) { return never_defined_fn(); }\n\
         int c_masked(void) { return c_priv(); }\n",
      ),
    ],
    expected: &[
      ("c_caller", "c_target", "calls", "constrained", "visible-export"),
      ("c_local_a", "c_local_b", "calls", "exact", "same-file"),
    ],
    absent: &[
      ("c_ext", "never_defined_fn"),
      ("c_masked", "c_priv"), // `static` is translation-unit-local — masked, no edge
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

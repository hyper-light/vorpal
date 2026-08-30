//! F-M3 acceptance: a registered dynamic language whose grammar arrives via dlopen indexes
//! end-to-end under an [`vorpal_index::ExtractionEnv`] carrying its outline rules — nodes
//! queryable next to builtin-language nodes, replay/fast-path behavior intact, and a rules
//! change re-keying products loudly (full re-extract, not a silent stale serve).
//!
//! The grammar fixture is compiled from the vendored tree-sitter-json source at test time
//! (same approach as `vorpal-dynamic`'s own tests) so no binary blob is checked in. Where no
//! C toolchain applies the test skips with a note. Registration is process-global and
//! one-shot, which is why this file holds a single #[test]: one process, one registration.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use vorpal_dynamic::{CustomLang, LibraryPath};
use vorpal_index::{CacheMode, ExtractionEnv, ParseHealthPolicy, RuleSource, build_index_env};
use vorpal_kg::Kg;

/// Compile the vendored JSON grammar into a shared library, once per test process.
fn json_fixture() -> Option<PathBuf> {
  #[cfg(not(unix))]
  {
    None
  }
  #[cfg(unix)]
  {
    let src =
      std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../grammars/tree-sitter-json/src");
    let out = std::env::temp_dir().join(format!("vorpal-jsonx-fixture-{}.so", std::process::id()));
    let status = std::process::Command::new("cc")
      .arg("-shared")
      .arg("-fPIC")
      .arg("-O1")
      .arg("-I")
      .arg(&src)
      .arg(src.join("parser.c"))
      .arg("-o")
      .arg(&out)
      .status()
      .ok()?;
    status.success().then_some(out)
  }
}

/// The bundled JSON outline rules, retargeted at the dynamic `jsonx` registration — same
/// grammar surface, so the same kinds/fields apply.
fn jsonx_rules() -> String {
  let builtin = include_str!("../../outline/src/default_rules/json.yml");
  builtin.replace("language: Json", "language: jsonx")
}

#[test]
fn dynamic_language_indexes_end_to_end() {
  let Some(so) = json_fixture() else {
    eprintln!("skipping dynamic_language_indexes_end_to_end: no C toolchain for the fixture");
    return;
  };

  let base = std::env::temp_dir().join(format!("vorpal-dynlang-{}", std::process::id()));
  let src = base.join("src");
  let out = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();

  // One-shot process-global registration — the caller-side dlopen the index path never does.
  let custom = CustomLang {
    library_path: LibraryPath::Single(so),
    language_symbol: Some("tree_sitter_json".into()),
    meta_var_char: None,
    expando_char: None,
    extensions: vec!["jsonx".into()],
    outline_rules: Some(PathBuf::from("rules/jsonx.yml")),
  };
  CustomLang::register(&base, HashMap::from([("jsonx".to_string(), custom)]))
    .expect("fixture grammar registers");

  fs::write(
    src.join("config.jsonx"),
    "{\n  \"alpha\": {\"nested\": 1},\n  \"omega\": true\n}\n",
  )
  .unwrap();
  fs::write(src.join("b.rs"), "pub fn beta() -> i32 {\n    0\n}\n").unwrap();

  let env = ExtractionEnv {
    outline_sources: vec![RuleSource {
      origin: "rules/jsonx.yml".into(),
      yaml: jsonx_rules(),
    }],
  };

  // Build 1: both files extract — the dynamic language next to the builtin one.
  let report = build_index_env(&src, &out, CacheMode::default(), ParseHealthPolicy::default(), &env)
    .expect("dynamic-language index builds");
  assert!(!report.reused, "first build is real: {report:?}");
  assert_eq!(report.indexed, 2, "both files extracted: {report:?}");

  let kg = Kg::load(&out).unwrap();
  let names_of = |name: &str| -> usize {
    kg
      .select(&vorpal_kg::SymbolSelector {
        name: Some(name),
        ..Default::default()
      })
      .len()
  };
  assert!(names_of("alpha") > 0, "jsonx key indexed as a node");
  assert!(names_of("omega") > 0, "second jsonx key indexed");
  assert!(names_of("beta") > 0, "builtin language unaffected");
  drop(kg);

  // Build 2: nothing changed — the whole-tree fast path holds with a registered dynamic
  // language folded into the manifest stamp.
  let report = build_index_env(&src, &out, CacheMode::default(), ParseHealthPolicy::default(), &env)
    .expect("unchanged rebuild");
  assert!(report.reused, "unchanged tree reuses: {report:?}");

  // Build 3: the rules source changes → the rules digest (global) re-keys every product;
  // everything re-extracts. Loud full re-key, never a stale product served.
  let mut changed = env.clone();
  changed.outline_sources[0].yaml.push_str("\n# rules edited\n");
  let report = build_index_env(
    &src,
    &out,
    CacheMode::default(),
    ParseHealthPolicy::default(),
    &changed,
  )
  .expect("rules-changed rebuild");
  assert!(!report.reused, "rules change busts the fast path: {report:?}");
  assert_eq!(report.indexed, 2, "all products re-keyed: {report:?}");
  assert_eq!(report.skipped, 0, "no stale replay under a new rules digest: {report:?}");

  let _ = fs::remove_dir_all(&base);
}

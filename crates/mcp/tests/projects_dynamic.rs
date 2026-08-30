//! D4 v2: per-project custom languages in multi-project serving. The launcher union-
//! registers dynamic grammars once (this test plays the launcher), and each project's
//! extraction ENVIRONMENT gates what its builds walk — the same .jsonx file yields nodes in
//! the project that declares the language and nothing in the one that doesn't, proving env
//! routing rather than process-global behavior.
//!
//! Registration is process-global and one-shot, so this file holds a single #[test].

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};
use vorpal_dynamic::{CustomLang, LibraryPath};
use vorpal_mcp::registry;

fn call(server: &mut vorpal_mcp::MultiServerForTest, line: &str) -> Value {
  let response = server.handle_line(line).expect("request gets a response");
  serde_json::from_str(&response).expect("valid json")
}

fn tool_call(
  server: &mut vorpal_mcp::MultiServerForTest,
  name: &str,
  args: Value,
) -> (String, bool) {
  let line = json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
                    "params": {"name": name, "arguments": args}})
  .to_string();
  let value = call(server, &line);
  let result = &value["result"];
  let text = result["content"][0]["text"].as_str().unwrap_or("").to_string();
  let is_err = result["isError"].as_bool().unwrap_or(false);
  (text, is_err)
}

/// Compile the vendored JSON grammar into a shared library (same fixture approach as
/// crates/index/tests/dynamic_lang.rs). None → no C toolchain, test skips with a note.
fn json_fixture(out: &std::path::Path) -> Option<PathBuf> {
  #[cfg(not(unix))]
  {
    let _ = out;
    None
  }
  #[cfg(unix)]
  {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("../../grammars/tree-sitter-json/src");
    let so = out.join("jsonx-fixture.so");
    let status = std::process::Command::new("cc")
      .arg("-shared")
      .arg("-fPIC")
      .arg("-O1")
      .arg("-I")
      .arg(&src)
      .arg(src.join("parser.c"))
      .arg("-o")
      .arg(&so)
      .status()
      .ok()?;
    status.success().then_some(so)
  }
}

fn jsonx_rules() -> String {
  let builtin = include_str!("../../outline/src/default_rules/json.yml");
  builtin.replace("language: Json", "language: jsonx")
}

#[test]
fn per_project_dynamic_language_envs_route() {
  let base = std::env::temp_dir().join(format!("vorpal-mcp-dynproj-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&base).unwrap();
  let Some(so) = json_fixture(&base) else {
    eprintln!("skipping per_project_dynamic_language_envs_route: no C toolchain");
    return;
  };

  // Two projects; the SAME .jsonx content lands in both.
  let plain = base.join("plain");
  let dynp = base.join("dynp");
  fs::create_dir_all(plain.join("src")).unwrap();
  fs::create_dir_all(dynp.join("src")).unwrap();
  for project in [&plain, &dynp] {
    fs::write(
      project.join("src/config.jsonx"),
      "{\n  \"gadget_knob\": 1\n}\n",
    )
    .unwrap();
    fs::write(project.join("src/lib.rs"), "pub fn shared_fn() {}\n").unwrap();
  }

  // The launcher's act: ONE union registration for the whole process.
  let custom = CustomLang {
    library_path: LibraryPath::Single(so),
    language_symbol: Some("tree_sitter_json".into()),
    meta_var_char: None,
    expando_char: None,
    extensions: vec!["jsonx".into()],
    outline_rules: None,
    ref_spec: None,
    canary: None,
  };
  CustomLang::register(&base, HashMap::from([("jsonx".to_string(), custom)]))
    .expect("fixture grammar registers");

  // Only dynp's ENV carries jsonx rules — the language exists process-wide, the walking
  // of it is per-project.
  let rules_path = base.join("jsonx.yml");
  fs::write(&rules_path, jsonx_rules()).unwrap();
  let mut env = vorpal_index::ExtractionEnv::default();
  env.outline_sources.push(vorpal_index::RuleSource {
    origin: "jsonx.yml".into(),
    yaml: jsonx_rules(),
  });
  let mut envs = BTreeMap::new();
  envs.insert("dynp".to_string(), env);

  // SAFETY: test-scoped registry file.
  unsafe { std::env::set_var("VORPAL_PROJECTS_FILE", base.join("projects.yml")) };
  registry::enroll(&plain, Some("plain"), None).unwrap();
  registry::enroll(&dynp, Some("dynp"), None).unwrap();
  let mut server = vorpal_mcp::multi_server_for_test_with_envs(envs);

  for project in ["plain", "dynp"] {
    let (text, is_err) = tool_call(
      &mut server,
      "index",
      json!({"project": project, "src": base.join(project).to_string_lossy()}),
    );
    assert!(!is_err, "{project}: {text}");
  }

  // The jsonx-declared project extracted the key; the plain one holds no jsonx nodes —
  // and both indexed the shared Rust file (builtins unaffected).
  let (text, is_err) = tool_call(
    &mut server,
    "node",
    json!({"project": "dynp", "name": "gadget_knob"}),
  );
  assert!(!is_err && text.contains("config.jsonx"), "dynp extracts jsonx: {text}");
  let (text, _) = tool_call(
    &mut server,
    "node",
    json!({"project": "plain", "name": "gadget_knob"}),
  );
  assert!(
    text.contains("no results"),
    "plain (default env) must not walk jsonx: {text}"
  );
  for project in ["plain", "dynp"] {
    let (text, is_err) =
      tool_call(&mut server, "node", json!({"project": project, "name": "shared_fn"}));
    assert!(!is_err && text.contains("lib.rs"), "{project}: {text}");
  }

  unsafe { std::env::remove_var("VORPAL_PROJECTS_FILE") };
  let _ = fs::remove_dir_all(&base);
}

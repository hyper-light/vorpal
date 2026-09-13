//! `vorpal graph --within/--except` and `vorpal search --within`: the CLI mirrors of the
//! MCP scope — rows outside the scope are counted, never listed.

use assert_cmd::{Command, cargo_bin};
use std::fs;

fn tree(tag: &str) -> std::path::PathBuf {
  let base = std::env::temp_dir().join(format!("vorpal-cli-scope-{tag}-{}", std::process::id()));
  let src = base.join("src");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(src.join("sub")).unwrap();
  fs::write(src.join("b.rs"), "pub fn target() -> i32 {\n    0\n}\n").unwrap();
  fs::write(src.join("a.rs"), "use b::target;\n\npub fn caller() -> i32 {\n    target()\n}\n").unwrap();
  fs::write(
    src.join("sub").join("c.rs"),
    "use b::target;\n\npub fn caller2() -> i32 {\n    target()\n}\n",
  )
  .unwrap();
  src
}

#[test]
fn graph_and_search_honour_scope_flags() {
  let src = tree("flags");
  Command::new(cargo_bin!())
    .args(["index", src.to_str().unwrap()])
    .assert()
    .success();
  let index = src.join(".vorpal").join("index");
  let index = index.to_str().unwrap();

  let out = Command::new(cargo_bin!())
    .args(["graph", "callers", "target", "--index", index, "--within", "sub", "--format", "json"])
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
  let value: serde_json::Value = serde_json::from_slice(&out).expect("json envelope");
  assert_eq!(value["total"], 1, "{value}");
  assert_eq!(value["outsideScope"], 1, "{value}");
  assert_eq!(value["scope"]["within"], serde_json::json!(["sub"]), "{value}");
  assert!(value["records"][0]["path"].as_str().unwrap().ends_with("sub/c.rs"), "{value}");

  let out = Command::new(cargo_bin!())
    .args(["graph", "callers", "target", "--index", index, "--except", "sub", "--format", "json"])
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
  let value: serde_json::Value = serde_json::from_slice(&out).expect("json envelope");
  assert_eq!(value["total"], 1, "{value}");
  assert!(value["records"][0]["path"].as_str().unwrap().ends_with("/a.rs"), "{value}");

  // Text output with scope flags renders the scoped rows and the count left out.
  let out = Command::new(cargo_bin!())
    .args(["graph", "callers", "target", "--index", index, "--within", "sub"])
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
  let text = String::from_utf8_lossy(&out);
  assert!(text.contains("caller2") && !text.contains("\tcaller\t") && text.contains("outside scope: 1 rows not listed"), "{text}");

  // A typo is an error, never an empty answer.
  Command::new(cargo_bin!())
    .args(["graph", "callers", "target", "--index", index, "--within", "nope"])
    .assert()
    .failure();

  let out = Command::new(cargo_bin!())
    .args(["search", "caller", "--index", index, "--within", "sub", "--format", "json"])
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
  let value: serde_json::Value = serde_json::from_slice(&out).expect("json envelope");
  let rows = value["records"].as_array().expect("records");
  assert!(!rows.is_empty(), "{value}");
  assert!(rows.iter().all(|r| r["path"].as_str().unwrap().contains("/sub/")), "{value}");
  let _ = fs::remove_dir_all(src.parent().unwrap());
}

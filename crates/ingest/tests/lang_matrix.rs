//! The all-languages matrix: every code language proves a resolved cross-symbol `calls` edge
//! end-to-end (parse → extract defs+refs → resolve → link → query); structural languages prove
//! their structure nodes. One table, one harness — a failing row names its language.

use vorpal_ingest::{Ingestor, Kg, NodeId, OutlineExtractor, Resolver};
use vorpal_kg::EdgeType;

fn kg_for(files: &[(&str, &str)]) -> (Kg, vorpal_ingest::ResolveStats) {
  let mut ing = Ingestor::new(OutlineExtractor::new().unwrap());
  for (path, src) in files {
    ing.ingest_source(path, src);
  }
  ing.link_and_seal(&Resolver::new())
}

fn names_of(kg: &Kg, ids: &[NodeId]) -> Vec<String> {
  ids
    .iter()
    .filter_map(|&id| kg.node(id).map(|v| v.name.to_string()))
    .collect()
}

fn all_names(kg: &Kg) -> Vec<String> {
  (0..kg.node_count() as u64)
    .filter_map(|i| kg.node(NodeId::new(i)).map(|v| v.name.to_string()))
    .collect()
}

/// `(language label, file name, source with a `helper` def called from `run`)`
const CALL_ROWS: &[(&str, &str, &str)] = &[
  (
    "tsx",
    "a.tsx",
    "export function helper(): number { return 1 }\nexport function run(): number {\n  return helper()\n}\n",
  ),
  (
    "javascript",
    "a.js",
    "export function helper() { return 1 }\nexport function run() { return helper() }\n",
  ),
  (
    "python",
    "a.py",
    "def helper():\n    return 1\n\ndef run():\n    return helper()\n",
  ),
  (
    "go",
    "a.go",
    "package main\n\nfunc helper() int { return 1 }\n\nfunc run() int { return helper() }\n",
  ),
  (
    "c",
    "a.c",
    "int helper() { return 1; }\nint run() { return helper(); }\n",
  ),
  (
    "java",
    "A.java",
    "class A {\n  int helper() { return 1; }\n  int run() { return helper(); }\n}\n",
  ),
  (
    "csharp",
    "A.cs",
    "class A {\n  int Helper() { return 1; }\n  int Run() { return Helper(); }\n}\n",
  ),
  (
    "kotlin",
    "a.kt",
    "fun helper(): Int = 1\n\nfun run(): Int {\n    return helper()\n}\n",
  ),
  (
    "swift",
    "a.swift",
    "func helper() -> Int { return 1 }\n\nfunc run() -> Int { return helper() }\n",
  ),
  (
    "ruby",
    "a.rb",
    "def helper\n  1\nend\n\ndef run\n  helper()\nend\n",
  ),
  (
    "php",
    "a.php",
    "<?php\nfunction helper() { return 1; }\nfunction run() { return helper(); }\n",
  ),
  (
    "scala",
    "a.scala",
    "object App {\n  def helper(): Int = 1\n  def run(): Int = helper()\n}\n",
  ),
  (
    "lua",
    "a.lua",
    "function helper()\n  return 1\nend\n\nfunction run()\n  return helper()\nend\n",
  ),
  (
    "bash",
    "a.sh",
    "helper() {\n  echo hi\n}\n\nrun() {\n  helper\n}\n",
  ),
  (
    "elixir",
    "a.ex",
    "defmodule App do\n  def helper do\n    1\n  end\n\n  def run do\n    helper()\n  end\nend\n",
  ),
  (
    "haskell",
    "a.hs",
    "helper :: Int -> Int\nhelper x = x\n\nrun :: Int -> Int\nrun y = helper y\n",
  ),
  (
    "dart",
    "a.dart",
    "int helper() { return 1; }\n\nint run() { return helper(); }\n",
  ),
  (
    "solidity",
    "a.sol",
    "contract App {\n  function helper() public pure returns (uint) { return 1; }\n  function run() public pure returns (uint) { return helper(); }\n}\n",
  ),
];

#[test]
fn every_code_language_resolves_a_call_edge() {
  let mut failures = Vec::new();
  for (label, path, src) in CALL_ROWS {
    let (kg, stats) = kg_for(&[(path, src)]);
    let (callee, caller) = if *label == "csharp" {
      ("Helper", "Run")
    } else {
      ("helper", "run")
    };
    let callers = names_of(&kg, &kg.callers_of(callee));
    if !callers.iter().any(|n| n == caller) {
      failures.push(format!(
        "{label}: callers_of({callee}) = {callers:?} (stats {stats:?}; nodes {:?})",
        all_names(&kg)
      ));
    }
  }
  assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn symbol_imports_resolve_to_importing_files() {
  // Java: `import util.Helper;` resolves by simple name to the exported class.
  let (kg, stats) = kg_for(&[
    (
      "Helper.java",
      "public class Helper {\n  int one() { return 1; }\n}\n",
    ),
    (
      "App.java",
      "import util.Helper;\n\nclass App {\n  int zero() { return 0; }\n}\n",
    ),
  ]);
  let importers = names_of(&kg, &kg.importers_of("Helper"));
  assert!(
    importers.iter().any(|n| n == "App.java"),
    "importers {importers:?} stats {stats:?}"
  );
}

#[test]
fn string_imports_stay_honestly_unresolved() {
  // `./util` names no symbol node, so the import is counted unresolved — never faked.
  let (kg, stats) = kg_for(&[(
    "a.ts",
    "import { x } from \"./util\";\n\nexport function run(): number { return x }\n",
  )]);
  assert!(stats.unresolved >= 1, "{stats:?}");
  assert!(kg.nodes_named("./util").is_empty());
}

#[test]
fn structural_languages_extract_structure_nodes() {
  let cases: &[(&str, &str, &str, &[&str])] = &[
    (
      "json",
      "cfg.json",
      "{\n  \"server\": {\n    \"port\": 8080\n  }\n}\n",
      &["server", "port"],
    ),
    (
      "yaml",
      "cfg.yml",
      "server:\n  port: 8080\n",
      &["server", "port"],
    ),
    (
      "markdown",
      "doc.md",
      "# Title\n\nintro\n\n## Section One\n\nbody\n",
      &["Title", "Section One"],
    ),
    (
      "css",
      "style.css",
      ".btn {\n  color: red;\n}\n",
      &[".btn", "color"],
    ),
    // The outline model is two-level (item + members), so the document element and its direct
    // structure are captured; deeper elements (div inside body) are beyond the member tier.
    (
      "html",
      "index.html",
      "<html>\n<body>\n<div id=\"app\">hi</div>\n</body>\n</html>\n",
      &["html", "body"],
    ),
    ("nix", "default.nix", "{\n  pkgs = 1;\n}\n", &["pkgs"]),
    (
      "hcl",
      "main.tf",
      "resource \"aws_instance\" \"web\" {\n  ami = \"abc\"\n}\n",
      &["ami"],
    ),
  ];
  let mut failures = Vec::new();
  for (label, path, src, expected) in cases {
    let (kg, _) = kg_for(&[(path, src)]);
    let names = all_names(&kg);
    for want in *expected {
      if !names.iter().any(|n| n == want) {
        failures.push(format!("{label}: missing node '{want}' in {names:?}"));
      }
    }
  }
  assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn structural_containment_is_queryable() {
  let (kg, _) = kg_for(&[(
    "cfg.json",
    "{\n  \"server\": {\n    \"port\": 8080\n  }\n}\n",
  )]);
  let server = (0..kg.node_count() as u64)
    .map(NodeId::new)
    .find(|&id| kg.node(id).is_some_and(|v| v.name == "server"))
    .expect("server node");
  let contained = names_of(&kg, &kg.defines(server));
  assert!(contained.iter().any(|n| n == "port"), "{contained:?}");
  // Containment edges only — no calls in data.
  assert!(
    kg.out_neighbors(server)
      .iter()
      .all(|&(_, e)| e != EdgeType::CALLS)
  );
}

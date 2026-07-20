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
fn path_imports_resolve_to_indexed_file_nodes() {
  // `import "./util"` from a.ts resolves to the indexed util.ts FILE node — a real IMPORTS edge.
  let (kg, stats) = kg_for(&[
    (
      "util.ts",
      "export function tsHelper(): number { return 1 }\n",
    ),
    (
      "a.ts",
      "import { tsHelper } from \"./util\";\n\nexport function run(): number { return tsHelper() }\n",
    ),
  ]);
  assert!(stats.resolved >= 1, "{stats:?}");
  let importers = names_of(&kg, &kg.importers_of("util.ts"));
  assert!(importers.iter().any(|n| n == "a.ts"), "{importers:?}");
}

#[test]
fn string_imports_stay_honestly_unresolved() {
  // `./missing` matches no indexed file and no symbol: counted unresolved — never faked.
  let (kg, stats) = kg_for(&[(
    "a.ts",
    "import { x } from \"./missing\";\n\nexport function run(): number { return x }\n",
  )]);
  assert!(stats.unresolved() >= 1, "{stats:?}");
  assert!(kg.nodes_named("./missing").is_empty());
}

#[test]
fn type_and_implements_edges() {
  // Rust: trait impl → IMPLEMENTS; param type use → OF_TYPE.
  let (kg, _) = kg_for(&[(
    "a.rs",
    "pub trait Render {\n    fn go(&self) -> i32;\n}\n\npub struct Widget {\n    pub size: i32,\n}\n\nimpl Render for Widget {\n    fn go(&self) -> i32 {\n        self.size\n    }\n}\n\npub fn draw(w: Widget) -> i32 {\n    w.go()\n}\n",
  )]);
  let implementors = names_of(&kg, &kg.implementors_of("Render"));
  assert!(
    implementors.iter().any(|n| n == "Widget"),
    "{implementors:?}"
  );
  let users = names_of(&kg, &kg.users_of_type("Widget"));
  assert!(users.iter().any(|n| n == "draw"), "{users:?}");

  // TypeScript: `implements` clause.
  let (kg, _) = kg_for(&[(
    "a.ts",
    "export interface Shape {\n  area(): number\n}\n\nexport class Circle implements Shape {\n  area(): number { return 1 }\n}\n",
  )]);
  let implementors = names_of(&kg, &kg.implementors_of("Shape"));
  assert!(
    implementors.iter().any(|n| n == "Circle"),
    "{implementors:?}"
  );

  // Java: `extends`.
  let (kg, _) = kg_for(&[(
    "A.java",
    "class Base {\n  int x() { return 1; }\n}\n\nclass Sub extends Base {\n  int y() { return 2; }\n}\n",
  )]);
  let implementors = names_of(&kg, &kg.implementors_of("Base"));
  assert!(implementors.iter().any(|n| n == "Sub"), "{implementors:?}");
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
    // Labeled blocks are named by their LAST label (the Terraform resource name).
    (
      "hcl",
      "main.tf",
      "resource \"aws_instance\" \"web\" {\n  ami = \"abc\"\n}\n\nmodule \"vpc\" {\n  source = \"./vpc\"\n}\n",
      &["web", "vpc", "ami", "source"],
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

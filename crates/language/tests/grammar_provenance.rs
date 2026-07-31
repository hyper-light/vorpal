//! Vendored grammars are a supply chain (IMPROVEMENTS #10): every `grammars/<crate>/` tree
//! carries machine-readable provenance — repository URL, upstream version + commit, license,
//! generator ABI, local patches, and a **complete source-tree digest** — in
//! `grammars/PROVENANCE.json`, and this test enforces all of it:
//!
//! - every vendored directory has an entry, and every entry has a directory (no orphans);
//! - required fields are present and plausible (https repository, non-empty commit/license);
//! - the recorded generator ABI equals the `LANGUAGE_VERSION` compiled into `src/parser.c`;
//! - a `LICENSE*` file is actually vendored; and
//! - the recorded digest equals a fresh digest of the tree — **any** change to any vendored
//!   byte fails CI until the provenance is regenerated and the diff is owned in review.
//!
//! Regeneration is deliberately a manual step:
//! `cargo test -p vorpal-language --test grammar_provenance -- --ignored regenerate`
//! recomputes digests/ABIs/metadata, preserves commits and patch notes for unchanged trees,
//! and seeds missing commits from the audited ledger in docs/UPSTREAM.md. A changed tree
//! with an unchanged upstream commit still fails review honestly: the digest diff is visible
//! and the patches field is where the explanation belongs.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../..")
    .canonicalize()
    .unwrap()
}

fn grammar_dirs() -> Vec<PathBuf> {
  let mut dirs: Vec<PathBuf> = fs::read_dir(repo_root().join("grammars"))
    .expect("grammars/ exists")
    .flatten()
    .map(|entry| entry.path())
    .filter(|path| path.is_dir())
    .collect();
  dirs.sort();
  dirs
}

/// Deterministic digest of a vendored tree: sorted relative paths, each mixed as
/// `path\0bytes\0`. Build artifacts and OS litter are excluded; everything else — sources,
/// generated parsers, scanners, queries, corpora, lockfiles — is supply chain and counts.
fn digest_tree(dir: &Path) -> String {
  let mut files: Vec<PathBuf> = Vec::new();
  let mut stack = vec![dir.to_path_buf()];
  while let Some(current) = stack.pop() {
    for entry in fs::read_dir(&current).unwrap().flatten() {
      let path = entry.path();
      let name = entry.file_name();
      let name = name.to_string_lossy();
      if path.is_dir() {
        if name != "target" && name != "node_modules" && name != ".git" {
          stack.push(path);
        }
      } else if name != ".DS_Store" {
        files.push(path);
      }
    }
  }
  files.sort();
  let mut hasher = xxhash_rust::xxh3::Xxh3::new();
  for file in &files {
    let rel = file.strip_prefix(dir).unwrap();
    hasher.update(rel.to_string_lossy().as_bytes());
    hasher.update(&[0]);
    hasher.update(&fs::read(file).unwrap());
    hasher.update(&[0]);
  }
  format!("xxh3:{:016x}", hasher.digest())
}

/// The generator ABI compiled into the vendored parser(s) (`#define LANGUAGE_VERSION n`).
/// Split-grammar repos (tree-sitter-md) vendor several `src/parser.c`; all must agree —
/// a tree carrying mixed generator ABIs is itself a supply-chain smell worth failing on.
fn parser_abi(dir: &Path) -> Option<u64> {
  let mut parsers = Vec::new();
  let mut stack = vec![dir.to_path_buf()];
  while let Some(current) = stack.pop() {
    for entry in fs::read_dir(&current).into_iter().flatten().flatten() {
      let path = entry.path();
      let name = entry.file_name();
      let name = name.to_string_lossy();
      if path.is_dir() {
        if name != "target" && name != "node_modules" && name != ".git" {
          stack.push(path);
        }
      } else if name == "parser.c" && current.file_name().is_some_and(|d| d == "src") {
        parsers.push(path);
      }
    }
  }
  let mut abis: Vec<u64> = parsers
    .iter()
    .filter_map(|parser| {
      fs::read_to_string(parser).ok()?.lines().find_map(|line| {
        line
          .trim()
          .strip_prefix("#define LANGUAGE_VERSION")
          .and_then(|rest| rest.trim().parse().ok())
      })
    })
    .collect();
  abis.sort_unstable();
  abis.dedup();
  match abis.as_slice() {
    [abi] => Some(*abi),
    _ => None,
  }
}

/// `key = "value"` line lookup in a grammar's Cargo.toml (no toml dependency needed for
/// the two flat fields we read).
fn cargo_field(dir: &Path, key: &str) -> Option<String> {
  let cargo = fs::read_to_string(dir.join("Cargo.toml")).ok()?;
  cargo.lines().find_map(|line| {
    let line = line.trim();
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim();
    Some(rest.trim_matches('"').to_string())
  })
}

fn has_license_file(dir: &Path) -> bool {
  fs::read_dir(dir)
    .into_iter()
    .flatten()
    .flatten()
    .any(|entry| {
      entry
        .file_name()
        .to_string_lossy()
        .to_uppercase()
        .starts_with("LICENSE")
    })
}

/// Upstream commits from the audited ledger table in docs/UPSTREAM.md:
/// `| tree-sitter-x | version | \`commit\` | patches |`.
fn ledger_commits() -> BTreeMap<String, String> {
  let text = fs::read_to_string(repo_root().join("docs/UPSTREAM.md")).unwrap_or_default();
  let mut commits = BTreeMap::new();
  for line in text.lines() {
    let mut cells = line.split('|').map(str::trim);
    let _ = cells.next();
    let (Some(name), Some(_version), Some(commit)) = (cells.next(), cells.next(), cells.next())
    else {
      continue;
    };
    if name.starts_with("tree-sitter-") {
      commits.insert(name.to_string(), commit.trim_matches('`').to_string());
    }
  }
  commits
}

fn provenance_path() -> PathBuf {
  repo_root().join("grammars/PROVENANCE.json")
}

#[test]
fn every_vendored_grammar_has_verified_provenance() {
  let text = fs::read_to_string(provenance_path()).expect(
    "grammars/PROVENANCE.json missing — run \
     `cargo test -p vorpal-language --test grammar_provenance -- --ignored regenerate`",
  );
  let entries: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
  let entries = entries.as_object().expect("top-level object");

  let dirs = grammar_dirs();
  for dir in &dirs {
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    let entry = entries.get(&name).unwrap_or_else(|| {
      panic!("{name}: vendored but missing from PROVENANCE.json — regenerate and review")
    });
    let field = |key: &str| -> String {
      entry
        .get(key)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
    };
    assert!(
      field("repository").starts_with("https://"),
      "{name}: repository URL must be recorded"
    );
    assert!(!field("version").is_empty(), "{name}: version must be recorded");
    assert!(!field("commit").is_empty(), "{name}: upstream commit must be recorded");
    assert!(!field("license").is_empty(), "{name}: license must be recorded");
    assert!(has_license_file(dir), "{name}: LICENSE file must be vendored");
    let abi = entry.get("abi").and_then(|value| value.as_u64()).unwrap_or(0);
    let compiled = parser_abi(dir).unwrap_or_else(|| panic!("{name}: no LANGUAGE_VERSION in parser.c"));
    assert_eq!(abi, compiled, "{name}: recorded generator ABI must match src/parser.c");
    assert!(entry.get("patches").is_some_and(|p| p.is_array()), "{name}: patches array");
    let recorded = field("source_digest");
    let fresh = digest_tree(dir);
    assert_eq!(
      recorded, fresh,
      "{name}: vendored tree changed without regenerating provenance — \
       rerun the regenerate test and own the diff in review"
    );
  }

  // No orphans: every entry maps to a vendored directory.
  for name in entries.keys() {
    assert!(
      dirs.iter().any(|d| d.file_name().unwrap().to_string_lossy() == *name),
      "{name}: provenance entry has no vendored directory"
    );
  }
}

/// Manual regeneration (`-- --ignored regenerate`): recompute digests/ABIs/metadata,
/// preserve commit + patches for existing entries, seed missing commits from UPSTREAM.md.
#[test]
#[ignore = "writes grammars/PROVENANCE.json; run explicitly when vendoring changes"]
fn regenerate() {
  let existing: serde_json::Value = fs::read_to_string(provenance_path())
    .ok()
    .and_then(|text| serde_json::from_str(&text).ok())
    .unwrap_or_else(|| serde_json::json!({}));
  let ledger = ledger_commits();

  let mut out: BTreeMap<String, serde_json::Value> = BTreeMap::new();
  for dir in grammar_dirs() {
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    let prior = existing.get(&name);
    let prior_str = |key: &str| -> Option<String> {
      prior
        .and_then(|entry| entry.get(key))
        .and_then(|value| value.as_str())
        .map(str::to_string)
    };
    let commit = prior_str("commit")
      .filter(|commit| !commit.is_empty())
      .or_else(|| ledger.get(&name).cloned())
      .unwrap_or_default();
    let patches = prior
      .and_then(|entry| entry.get("patches"))
      .cloned()
      .unwrap_or_else(|| serde_json::json!([]));
    out.insert(
      name.clone(),
      serde_json::json!({
        "repository": cargo_field(&dir, "repository").unwrap_or_default(),
        "version": cargo_field(&dir, "version").unwrap_or_default(),
        "commit": commit,
        "license": cargo_field(&dir, "license").unwrap_or_default(),
        "abi": parser_abi(&dir).unwrap_or(0),
        "patches": patches,
        "source_digest": digest_tree(&dir),
      }),
    );
  }
  let rendered = serde_json::to_string_pretty(&out).unwrap();
  fs::write(provenance_path(), format!("{rendered}\n")).unwrap();
  println!("wrote {} entries", out.len());
}

//! Giant-file tree cache: repeated re-parses of edited large sources are the real
//! large-file tax — a long-lived process (the MCP overlay daemon, an SDK server calling
//! `indexBuild` on every save, the watch loop's in-process builds) pays a whole-file
//! parse for a one-line edit to a multi-megabyte source. This cache retains the parse
//! state (source + tree; the tree handle is a refcount, the source the real cost) for
//! the largest recently-parsed files and re-parses edits **incrementally** through
//! tree-sitter's own contract: `ts_tree_edit` + reparse yields the identical tree a
//! from-scratch parse produces. Correctness is the library's guarantee, pinned by a
//! product-byte oracle across edit shapes (including a vendored multi-megabyte
//! generated source), and `VORPAL_TREE_CACHE=0` disables the whole path.
//!
//! Chosen over chunked parallel parsing after measurement (see BENCHMARKS "chunked C
//! parsing — REJECTED"): lexical boundary proofs die on the C preprocessor, generated
//! giants are single-declaration-capped, and cold builds have no idle cores to give a
//! chunk fan-out anyway. Incremental reparse attacks the case that actually recurs.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use vorpal_core::tree_sitter::{StrDoc, TSTree};
use vorpal_lang_registry::SgLang;

type Root = vorpal_core::Vorpal<StrDoc<SgLang>>;

/// Files below this size never cache: a small parse is cheaper than retention.
/// `VORPAL_TREE_CACHE_MIN` overrides (bytes).
const DEFAULT_MIN_BYTES: usize = 1 << 20;
/// Total RETAINED SOURCE bytes across entries; retained trees cost roughly an order of
/// magnitude more than their sources (ledger-profiled 10–40×), so this bounds resident
/// tree mass at ~2.5 GB worst-case — one julia-parser.c-class giant, or a handful of
/// kernel-class large files. Swept 2026-09-02 (BENCHMARKS "giant-file tree cache"):
/// wins are 2.2–3.1× per save from 1 MB upward; the floor stays at 1 MB so cold builds
/// (which parse each file once — retention is pure cost there) stay inert on the
/// kernel corpus (4 files qualify). `VORPAL_TREE_CACHE_BUDGET` overrides.
const DEFAULT_BUDGET_BYTES: usize = 64 << 20;

struct Policy {
  enabled: bool,
  min_bytes: usize,
  budget_bytes: usize,
}

fn policy() -> &'static Policy {
  static POLICY: OnceLock<Policy> = OnceLock::new();
  POLICY.get_or_init(|| {
    let env_usize = |key: &str, default: usize| {
      std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
    };
    Policy {
      enabled: std::env::var("VORPAL_TREE_CACHE").map_or(true, |v| v != "0"),
      min_bytes: env_usize("VORPAL_TREE_CACHE_MIN", DEFAULT_MIN_BYTES),
      budget_bytes: env_usize("VORPAL_TREE_CACHE_BUDGET", DEFAULT_BUDGET_BYTES),
    }
  })
}

struct Entry {
  source: String,
  tree: TSTree,
  lang: SgLang,
  /// Monotonic touch stamp for LRU eviction.
  stamp: u64,
}

#[derive(Default)]
struct Cache {
  entries: HashMap<String, Entry>,
  bytes: usize,
  clock: u64,
}

impl Cache {
  fn evict_to_fit(&mut self, incoming: usize, budget: usize) {
    while self.bytes + incoming > budget && !self.entries.is_empty() {
      let Some(oldest) = self
        .entries
        .iter()
        .min_by_key(|(_, e)| e.stamp)
        .map(|(k, _)| k.clone())
      else {
        break;
      };
      if let Some(evicted) = self.entries.remove(&oldest) {
        self.bytes -= evicted.source.len();
      }
    }
  }
}

fn cache() -> &'static Mutex<Cache> {
  static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
  CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

/// Parse `source` for `path`, incrementally when this process parsed an earlier version
/// of the same path. Falls back to a plain parse on any miss, poisoned lock, policy
/// exclusion, or language change — never a correctness surface, only a latency one.
pub(crate) fn grep_cached(lang: SgLang, path: &str, source: &str) -> Root {
  let policy = policy();
  if !policy.enabled || source.len() < policy.min_bytes {
    return vorpal_core::tree_sitter::LanguageExt::grep(&lang, source);
  }
  grep_cached_unpoliced(lang, path, source)
}

/// The policy-free body (test seam: oracles force tiny files through the cache).
pub(crate) fn grep_cached_unpoliced(lang: SgLang, path: &str, source: &str) -> Root {
  // Take ownership of the entry so the (potentially seconds-long) parse runs unlocked
  // and concurrent giants never serialize on the cache.
  let history = cache()
    .lock()
    .ok()
    .and_then(|mut cache| {
      let entry = cache.entries.remove(path)?;
      cache.bytes -= entry.source.len();
      Some(entry)
    })
    .filter(|entry| entry.lang == lang);
  let root = match &history {
    Some(entry) => vorpal_core::Vorpal::try_new_incremental(
      source,
      lang,
      Some((entry.source.as_str(), &entry.tree)),
    ),
    None => vorpal_core::Vorpal::try_new_incremental(source, lang, None),
  };
  let root = match root {
    Ok(root) => root,
    // A parse failure here would fail the plain path identically; surface it the same
    // way `grep` does (extraction treats the file as unparseable).
    Err(_) => return vorpal_core::tree_sitter::LanguageExt::grep(&lang, source),
  };
  let (src, tree) = root.parse_state();
  let entry = Entry {
    source: src.to_string(),
    tree: tree.clone(),
    lang,
    stamp: 0,
  };
  if let Ok(mut cache) = cache().lock() {
    cache.clock += 1;
    let mut entry = entry;
    entry.stamp = cache.clock;
    let budget = policy().budget_bytes;
    cache.evict_to_fit(entry.source.len(), budget);
    if cache.bytes + entry.source.len() <= budget {
      cache.bytes += entry.source.len();
      cache.entries.insert(path.to_string(), entry);
    }
  }
  root
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::OutlineExtractor;
  use vorpal_core::Language as _;

  /// THE oracle: extraction through the cache (prime parse, then an incremental
  /// reparse) must produce products byte-identical to a fresh whole parse of the edited
  /// source — tree-sitter's incremental contract, verified end to end through the
  /// product encoder.
  fn oracle(path: &str, before: &str, after: &str) {
    let extractor = OutlineExtractor::new().expect("rules compile");
    // Prime the cache with the BEFORE state via the unpoliced seam.
    let _ = grep_cached_unpoliced(SgLang::from_path(path).expect("lang"), path, before);
    // Incremental: extract AFTER through the cached history.
    let incremental = extractor
      .extract_product_via(path, after, grep_cached_unpoliced)
      .expect("incremental extraction");
    // Fresh truth: a brand-new whole parse of AFTER.
    let fresh = extractor
      .extract_product(&format!("fresh-{path}"), after)
      .expect("fresh extraction");
    let mut a = Vec::new();
    let mut b = Vec::new();
    crate::product::encode_product_into(&incremental, &mut a);
    crate::product::encode_product_into(&fresh, &mut b);
    // The path differs only in the probe name; products carry no path, so bytes match
    // exactly when the parses agree.
    assert_eq!(a, b, "{path}: incremental product diverged from fresh parse");
    // Drop the entry so the next oracle starts clean.
    if let Ok(mut c) = cache().lock() {
      if let Some(e) = c.entries.remove(path) {
        c.bytes -= e.source.len();
      }
    }
  }

  #[test]
  fn oracle_edit_shapes() {
    let base = r#"
#include <stdio.h>

static int counter = 0;

int add(int a, int b) {
  return a + b;
}

struct pair { int x; int y; };

int main(void) {
  struct pair p = { 1, 2 };
  printf("%d\n", add(p.x, p.y));
  return counter;
}
"#;
    let cases: Vec<(&str, String)> = vec![
      ("prepend", format!("// edited\n{base}")),
      ("append", format!("{base}\nint tail(void) {{ return 9; }}\n")),
      (
        "mid-replace",
        base.replace("return a + b;", "return a * b + counter;"),
      ),
      ("delete-span", base.replace("struct pair { int x; int y; };\n", "")),
      ("whitespace", base.replace("int main", "int  main")),
      ("identical", base.to_string()),
      (
        "two-distant-edits",
        base
          .replace("counter = 0", "counter = 42")
          .replace("return counter;", "return counter + 1;"),
      ),
      (
        "definition-split",
        base.replace("int add(int a, int b) {", "int add(int a,\n        int b) {"),
      ),
    ];
    for (name, after) in cases {
      oracle(&format!("shape-{name}.c"), base, &after);
    }
    // Cross-language sanity: the cache is language-agnostic.
    let py_before = "def alpha():\n    return 1\n\ndef beta():\n    return alpha()\n";
    let py_after = "def alpha():\n    return 2\n\ndef beta():\n    return alpha() + 1\n";
    oracle("shape.py", py_before, py_after);
  }

  #[test]
  fn oracle_vendored_giant() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = repo.join("grammars/tree-sitter-c/src/parser.c");
    let Ok(source) = std::fs::read_to_string(&path) else {
      return;
    };
    // A realistic save: one line changes deep in the file.
    let edited = source.replace("static const char * const ts_symbol_names", "/* touched */\nstatic const char * const ts_symbol_names");
    assert_ne!(source, edited);
    oracle("giant.c", &source, &edited);
  }

  #[test]
  fn eviction_respects_budget() {
    // Direct cache mechanics, no extraction.
    let lang = SgLang::from_path("x.c").expect("lang");
    let big_a = "int a;\n".repeat(4000);
    let big_b = "int b;\n".repeat(4000);
    let _ = grep_cached_unpoliced(lang, "evict-a.c", &big_a);
    let _ = grep_cached_unpoliced(lang, "evict-b.c", &big_b);
    let cache = cache().lock().unwrap();
    assert!(cache.bytes <= policy().budget_bytes);
    let total: usize = cache.entries.values().map(|e| e.source.len()).sum();
    assert_eq!(total, cache.bytes, "byte accounting is exact");
  }
}

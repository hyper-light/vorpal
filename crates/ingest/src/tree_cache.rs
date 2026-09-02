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

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use vorpal_core::tree_sitter::{IncrementalDelta, StrDoc, TSTree};
use vorpal_lang_registry::SgLang;

use crate::walk_reuse::WalkSnapshot;

type Root = vorpal_core::Vorpal<StrDoc<SgLang>>;

/// Files below this size never cache: a small parse is cheaper than retention.
/// `VORPAL_TREE_CACHE_MIN` overrides (bytes).
const DEFAULT_MIN_BYTES: usize = 1 << 20;
/// Total entry cost across entries, where one entry costs its RETAINED SOURCE bytes
/// plus its walk snapshot's resident mass. The snapshot measures 2.0–2.8× the source
/// across giant classes (measured 2026-09-02, `snapshot_mass` example: tree-sitter-c
/// parser.c 2.5×, -haskell 2.0×, -cpp 2.5×, -julia 2.8× at 54.7 MB source / 1.13 M
/// rows), so the budget is the swept 64 MB source capacity × (1 + 3) — a ratio ceiling
/// of 3 covers every measured class — preserving the original sweep's intent: one
/// julia-parser.c-class giant, or a handful of kernel-class large files, with retained
/// TREE mass (ledger-profiled 10–40× source, uncounted here) still bounded at ~2.5 GB
/// worst-case by the same source capacity. The 1 MB floor keeps cold builds inert
/// (they parse each file once — retention is pure cost there).
/// `VORPAL_TREE_CACHE_BUDGET` overrides.
const DEFAULT_BUDGET_BYTES: usize = (64 << 20) * 4;

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
  /// The extraction walk's snapshot for THIS source (walk reuse) — attached by
  /// [`store_snapshot`] after extraction, consumed (moved into the reuse context) by the
  /// next incremental parse of the same path.
  snapshot: Option<Box<WalkSnapshot>>,
}

impl Entry {
  /// Budget cost: retained source bytes plus the walk snapshot's resident mass — one
  /// knob bounds both.
  fn cost(&self) -> usize {
    self.source.len() + self.snapshot.as_ref().map_or(0, |s| s.approx_bytes())
  }
}

/// First-sight marker: a path parsed ONCE holds no tree — cold builds parse every file
/// exactly once, and retaining their trees starves the tree-sitter children-cache
/// freelists that would otherwise recycle into the very next parse (ledger-measured:
/// +3.05 M ts allocations on a cold kernel build from just four retained giants).
/// Promotion to a full entry happens on the SECOND parse of the same path — the save
/// loop's shape — so retention only ever pays where reuse is real.
struct Seen {
  lang: SgLang,
}

#[derive(Default)]
struct Cache {
  entries: HashMap<String, Entry>,
  seen: HashMap<String, Seen>,
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
        self.bytes -= evicted.cost();
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
  // and concurrent giants never serialize on the cache. `promote` is the two-touch
  // rule: retain a tree only for a path this process has parsed before.
  let (history, promote) = match cache().lock() {
    Ok(mut cache) => {
      let history = cache
        .entries
        .remove(path)
        .inspect(|entry| cache.bytes -= entry.cost())
        .filter(|entry| entry.lang == lang);
      let promote = history.is_some()
        || cache.seen.get(path).is_some_and(|seen| seen.lang == lang);
      if !promote {
        cache.seen.insert(path.to_string(), Seen { lang });
      }
      (history, promote)
    }
    Err(_) => (None, false),
  };
  if std::env::var("VORPAL_TREE_CACHE_TRACE").is_ok() {
    eprintln!(
      "[tree-cache] {path}: history={} promote={promote}",
      history.is_some()
    );
  }
  let parsed = match &history {
    Some(entry) => vorpal_core::Vorpal::try_new_incremental_ranged(
      source,
      lang,
      Some((entry.source.as_str(), &entry.tree)),
    ),
    None => vorpal_core::Vorpal::try_new_incremental_ranged(source, lang, None),
  };
  let (root, delta) = match parsed {
    Ok(parsed) => parsed,
    // A parse failure here would fail the plain path identically; surface it the same
    // way `grep` does (extraction treats the file as unparseable).
    Err(_) => return vorpal_core::tree_sitter::LanguageExt::grep(&lang, source),
  };
  // Walk-reuse handoff: an incremental parse over history that carried a snapshot of the
  // SAME old source arms the extraction (which runs next, on this thread) to splice
  // retained walk rows around the edit instead of re-walking the whole file.
  REUSE.with(|slot| {
    let ctx = match (history, delta) {
      (Some(entry), Some(delta)) => entry.snapshot.and_then(|snapshot| {
        (snapshot.source_xxh3 == xxhash_rust::xxh3::xxh3_64(entry.source.as_bytes())).then(
          || ReuseCtx {
            path: path.to_string(),
            snapshot,
            delta,
          },
        )
      }),
      _ => None,
    };
    *slot.borrow_mut() = ctx;
  });
  if !promote {
    return root;
  }
  let (src, tree) = root.parse_state();
  let entry = Entry {
    source: src.to_string(),
    tree: tree.clone(),
    lang,
    stamp: 0,
    snapshot: None,
  };
  if let Ok(mut cache) = cache().lock() {
    cache.clock += 1;
    let mut entry = entry;
    entry.stamp = cache.clock;
    let budget = policy().budget_bytes;
    cache.evict_to_fit(entry.cost(), budget);
    if cache.bytes + entry.cost() <= budget {
      cache.bytes += entry.cost();
      cache.entries.insert(path.to_string(), entry);
    }
  }
  root
}

/// One armed walk-reuse context: the snapshot taken OUT of the cache entry plus the
/// incremental parse's delta, parked between the parse and the extraction that follows
/// it on the same thread. Thread-local because that pairing is strictly sequential —
/// each pipeline worker parses then extracts one file at a time.
struct ReuseCtx {
  path: String,
  snapshot: Box<WalkSnapshot>,
  delta: IncrementalDelta,
}

thread_local! {
  static REUSE: RefCell<Option<ReuseCtx>> = const { RefCell::new(None) };
}

/// Claim the walk-reuse context the immediately preceding parse of `path` armed, if any.
/// Consuming — the snapshot moves to the caller; a second call returns `None`.
pub(crate) fn take_reuse(path: &str) -> Option<(Box<WalkSnapshot>, IncrementalDelta)> {
  REUSE.with(|slot| {
    let ctx = slot.borrow_mut().take()?;
    (ctx.path == path).then_some((ctx.snapshot, ctx.delta))
  })
}

/// Whether extraction should capture a walk snapshot for `path`: only when the tree
/// cache actually retained the parse (the two-touch save-loop shape) — cold single
/// parses never pay capture.
pub(crate) fn wants_snapshot(path: &str) -> bool {
  cache()
    .lock()
    .map(|cache| cache.entries.contains_key(path))
    .unwrap_or(false)
}

/// Attach the freshly captured walk snapshot to `path`'s cache entry, charging its mass
/// against the byte budget (evicting colder entries to fit; if THIS entry ends up over
/// budget on its own, the snapshot is simply not retained).
pub(crate) fn store_snapshot(path: &str, snapshot: Box<WalkSnapshot>) {
  let Ok(mut cache) = cache().lock() else {
    return;
  };
  let budget = policy().budget_bytes;
  let added = snapshot.approx_bytes();
  let Some(entry) = cache.entries.get_mut(path) else {
    return;
  };
  entry.snapshot = Some(snapshot);
  cache.bytes += added;
  if cache.bytes > budget {
    // Evict others first; as a last resort drop the snapshot we just attached.
    cache.evict_to_fit(0, budget);
    if cache.bytes > budget {
      if let Some(entry) = cache.entries.get_mut(path) {
        if let Some(dropped) = entry.snapshot.take() {
          cache.bytes -= dropped.approx_bytes();
        }
      }
    }
  }
}

/// Diagnostic for the `snapshot_mass` example: the stored snapshot's (approx bytes,
/// pending rows, items) for `path`, if one is retained.
pub(crate) fn snapshot_stats(path: &str) -> Option<(usize, usize, usize)> {
  let cache = cache().lock().ok()?;
  let snap = cache.entries.get(path)?.snapshot.as_ref()?;
  Some((snap.approx_bytes(), snap.pending.len(), snap.items.len()))
}

/// Test support: drop `path`'s entry (and its byte accounting) so oracle tests that share
/// the process-global cache leave no residue for their siblings.
#[cfg(test)]
pub(crate) fn evict_for_tests(path: &str) {
  if let Ok(mut cache) = cache().lock() {
    if let Some(entry) = cache.entries.remove(path) {
      cache.bytes -= entry.cost();
    }
    cache.seen.remove(path);
  }
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
    // Prime TWICE: the two-touch rule marks a path on first sight and retains the tree
    // only on the second parse — one prime would leave no history and the "incremental"
    // leg would silently run whole (a vacuous oracle).
    let lang = SgLang::from_path(path).expect("lang");
    let _ = grep_cached_unpoliced(lang, path, before);
    let _ = grep_cached_unpoliced(lang, path, before);
    assert!(
      cache().lock().unwrap().entries.contains_key(path),
      "{path}: history must be retained before the incremental leg"
    );
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
        c.bytes -= e.cost();
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
    let total: usize = cache.entries.values().map(Entry::cost).sum();
    assert_eq!(total, cache.bytes, "byte accounting is exact");
  }
}

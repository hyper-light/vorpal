//! Extraction self-check: prove this binary can extract before letting it build an index.
//!
//! Why this exists: an index build whose extraction machinery is internally broken — a stale
//! binary linked from a mid-refactor tree, a grammar bump that no longer matches the compiled
//! outline rules, a reference-spec dispatch table that resolved against the wrong kind ids —
//! does not crash. It parses every file, emits a plausible node count, resolves zero (or a
//! gutted subset of) references, prints healthy-looking stats, and seals the generation with
//! exit code 0. The damage is silent and, through the product bank, durable: products written
//! by a broken build carry the same grammar/rules digests as a correct build of the same
//! source, so they replay into later, healthy runs.
//!
//! The check: one tiny embedded canary source per supported language, extracted through the
//! exact same [`OutlineExtractor::extract_product`] path real files take. Every language must
//! yield at least its known minimum of outline items and references. Any shortfall fails the
//! build loudly, naming the broken languages, before a single artifact is staged. The floors
//! are deliberately minimums, not exact counts — a legitimate grammar or rule change may add
//! richer extraction without touching this table, but can never silently reduce a language to
//! nothing.
//!
//! Cost: the whole table is under two kilobytes of source and parses in single-digit
//! milliseconds, once per process (the verdict is cached). The warm unchanged-tree fast path
//! returns before the check runs, so a no-op re-index pays nothing.

use std::sync::OnceLock;

use vorpal_language::SupportLang;

use crate::OutlineExtractor;

/// One language's canary: a minimal source that must extract.
struct Canary {
  lang: SupportLang,
  /// Virtual path with the extension that routes to `lang` (never touches the filesystem).
  path: &'static str,
  source: &'static str,
  /// Minimum outline items (definitions) the canary must yield.
  min_items: usize,
  /// Minimum references (calls/imports/types) the canary must yield.
  min_refs: usize,
}

/// The full canary table: every language with compiled outline rules and/or a reference
/// spec. Pure-structural languages (CSS, HTML, JSON, Markdown, YAML) have no call/import
/// semantics, so their `min_refs` is 0 — the item floor still proves their rules bind.
const CANARIES: &[Canary] = &[
  Canary {
    lang: SupportLang::Rust,
    path: "vorpal-selfcheck/canary.rs",
    source: "use canary_dep::helper;\n\npub struct Canary {\n  pub field: u32,\n}\n\nfn canary() {\n  helper();\n}\n",
    min_items: 2,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::Python,
    path: "vorpal-selfcheck/canary.py",
    source: "from canary_dep import helper\n\nclass Canary:\n    def method(self):\n        helper()\n\ndef canary():\n    helper()\n",
    min_items: 2,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::Go,
    path: "vorpal-selfcheck/canary.go",
    source: "package canary\n\nimport \"canary/dep\"\n\ntype Canary struct{}\n\nfunc run() {\n\thelper()\n}\n",
    min_items: 2,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::JavaScript,
    path: "vorpal-selfcheck/canary.js",
    source: "import { helper } from \"./dep\";\n\nclass Canary {}\n\nfunction canary() {\n  helper();\n}\n",
    min_items: 2,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::TypeScript,
    path: "vorpal-selfcheck/canary.ts",
    source: "import { helper } from \"./dep\";\n\nclass Canary {}\n\nfunction canary(): void {\n  helper();\n}\n",
    min_items: 2,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::Tsx,
    path: "vorpal-selfcheck/canary.tsx",
    source: "import { helper } from \"./dep\";\n\nfunction canary(): void {\n  helper();\n}\n",
    min_items: 1,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::C,
    path: "vorpal-selfcheck/canary.c",
    source: "void helper(void);\n\nstruct canary {\n  int field;\n};\n\nvoid canary_run(void) {\n  helper();\n}\n",
    min_items: 2,
    min_refs: 1,
  },
  Canary {
    lang: SupportLang::Cpp,
    path: "vorpal-selfcheck/canary.cpp",
    source: "void helper();\n\nclass Canary {\npublic:\n  int field;\n};\n\nvoid canary_run() {\n  helper();\n}\n",
    min_items: 2,
    min_refs: 1,
  },
  Canary {
    lang: SupportLang::Java,
    path: "vorpal-selfcheck/canary.java",
    source: "import canary.Helper;\n\nclass Canary {\n  void run() {\n    helper();\n  }\n}\n",
    min_items: 1,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::CSharp,
    path: "vorpal-selfcheck/canary.cs",
    source: "using Canary.Dep;\n\nclass Canary {\n  void Run() {\n    Helper();\n  }\n}\n",
    min_items: 1,
    // The C# reference spec captures the invocation; `using` directives are not import refs.
    min_refs: 1,
  },
  Canary {
    lang: SupportLang::Kotlin,
    path: "vorpal-selfcheck/canary.kt",
    source: "import canary.helper\n\nclass Canary\n\nfun run() {\n  helper()\n}\n",
    min_items: 2,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::Swift,
    path: "vorpal-selfcheck/canary.swift",
    source: "import Foundation\n\nstruct Canary {}\n\nfunc run() {\n  helper()\n}\n",
    min_items: 2,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::Ruby,
    path: "vorpal-selfcheck/canary.rb",
    source: "require \"canary\"\n\nclass Canary\n  def run\n    helper()\n  end\nend\n",
    min_items: 1,
    min_refs: 1,
  },
  Canary {
    lang: SupportLang::Php,
    path: "vorpal-selfcheck/canary.php",
    source: "<?php\nuse Canary\\Helper;\n\nclass Canary {\n  function run() {\n    helper();\n  }\n}\n",
    min_items: 1,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::Dart,
    path: "vorpal-selfcheck/canary.dart",
    source: "import 'dep.dart';\n\nclass Canary {}\n\nvoid run() {\n  helper();\n}\n",
    min_items: 2,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::Scala,
    path: "vorpal-selfcheck/canary.scala",
    source: "import canary.helper\n\nclass Canary {\n  def run(): Unit = helper()\n}\n",
    min_items: 1,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::Lua,
    path: "vorpal-selfcheck/canary.lua",
    source: "local dep = require(\"dep\")\n\nfunction canary()\n  helper()\nend\n",
    min_items: 1,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::Bash,
    path: "vorpal-selfcheck/canary.sh",
    source: "canary() {\n  helper\n}\n",
    min_items: 1,
    min_refs: 1,
  },
  Canary {
    lang: SupportLang::Elixir,
    path: "vorpal-selfcheck/canary.ex",
    source: "defmodule Canary do\n  import Helper\n\n  def run do\n    helper()\n  end\nend\n",
    min_items: 1,
    min_refs: 2,
  },
  Canary {
    lang: SupportLang::Haskell,
    path: "vorpal-selfcheck/canary.hs",
    source: "module Canary where\n\nimport Helper\n\ncanary :: Int -> Int\ncanary x = helper x\n",
    min_items: 1,
    min_refs: 1,
  },
  Canary {
    lang: SupportLang::Solidity,
    path: "vorpal-selfcheck/canary.sol",
    source: "contract Canary {\n  function run() public {\n    helper();\n  }\n}\n",
    min_items: 1,
    min_refs: 1,
  },
  Canary {
    lang: SupportLang::Nix,
    path: "vorpal-selfcheck/canary.nix",
    source: "{ helper }:\n{\n  out = helper \"x\";\n}\n",
    min_items: 1,
    min_refs: 1,
  },
  Canary {
    lang: SupportLang::Hcl,
    path: "vorpal-selfcheck/canary.tf",
    source: "resource \"canary\" \"c\" {\n  value = helper(\"x\")\n}\n",
    min_items: 1,
    min_refs: 1,
  },
  Canary {
    lang: SupportLang::Css,
    path: "vorpal-selfcheck/canary.css",
    source: ".canary {\n  color: red;\n}\n",
    min_items: 1,
    min_refs: 0,
  },
  Canary {
    lang: SupportLang::Html,
    path: "vorpal-selfcheck/canary.html",
    source: "<html>\n<body>\n<div id=\"canary\"></div>\n</body>\n</html>\n",
    min_items: 1,
    min_refs: 0,
  },
  Canary {
    lang: SupportLang::Json,
    path: "vorpal-selfcheck/canary.json",
    source: "{\n  \"canary\": 1\n}\n",
    min_items: 1,
    min_refs: 0,
  },
  Canary {
    lang: SupportLang::Markdown,
    path: "vorpal-selfcheck/canary.md",
    source: "# Canary\n\nBody.\n",
    min_items: 1,
    min_refs: 0,
  },
  Canary {
    lang: SupportLang::Yaml,
    path: "vorpal-selfcheck/canary.yml",
    source: "canary: 1\n",
    min_items: 1,
    min_refs: 0,
  },
];

/// Run every canary through `extractor` and report all shortfalls at once.
///
/// An `Err` means this binary's compiled-in extraction machinery cannot produce what it is
/// contractually able to produce — indexing with it would seal silently gutted generations
/// and poison the product bank. Nothing about the corpus being indexed can cause this.
pub fn verify_extraction(extractor: &OutlineExtractor) -> Result<(), String> {
  let mut broken: Vec<String> = Vec::new();
  for canary in CANARIES {
    // A slim build legitimately lacks some grammars; its selfcheck covers what it ships.
    if !canary.lang.is_enabled() {
      continue;
    }
    let product = extractor.extract_product(canary.path, canary.source);
    let (items, refs) = product
      .as_ref()
      .map(|p| (p.items.len(), p.refs.len()))
      .unwrap_or((0, 0));
    if items < canary.min_items || refs < canary.min_refs {
      broken.push(format!(
        "{} (items {items}, expected ≥{}; refs {refs}, expected ≥{})",
        canary.lang, canary.min_items, canary.min_refs
      ));
    }
  }
  if broken.is_empty() {
    return Ok(());
  }
  Err(format!(
    "extraction self-check failed for {}: {}. This binary's compiled-in grammars, outline \
     rules, and reference specs are inconsistent (a stale or mid-refactor build?) — indexing \
     with it would silently produce an incomplete knowledge graph. Rebuild from a clean tree. \
     VORPAL_NO_SELFCHECK=1 bypasses this check (unsafe; the damage it prevents is silent).",
    if broken.len() == 1 {
      "1 language".to_string()
    } else {
      format!("{} languages", broken.len())
    },
    broken.join("; ")
  ))
}

/// [`verify_extraction`] for the default (built-in rules) extractor, cached process-wide.
///
/// The verdict is a pure function of the binary, so one check covers every build in this
/// process (the MCP daemon and embedded hosts pay it once). `VORPAL_NO_SELFCHECK=1` skips it
/// with a warning — an escape hatch for triage, never for production indexing.
pub fn verify_default_extraction(extractor: &OutlineExtractor) -> Result<(), String> {
  if std::env::var_os("VORPAL_NO_SELFCHECK").is_some_and(|v| v == "1") {
    eprintln!(
      "warning: VORPAL_NO_SELFCHECK=1 — skipping the extraction self-check; a broken binary \
       will index silently"
    );
    return Ok(());
  }
  static VERDICT: OnceLock<Result<(), String>> = OnceLock::new();
  VERDICT.get_or_init(|| verify_extraction(extractor)).clone()
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Every canary extracts at least its floor on this build — the check any healthy binary
  /// must pass, and the CI tripwire for a grammar bump or rule edit that guts a language.
  #[test]
  fn canaries_extract_on_this_build() {
    let extractor = OutlineExtractor::new().expect("default rules compile");
    if let Err(err) = verify_extraction(&extractor) {
      panic!("{err}");
    }
  }

  /// The check actually rejects a gutted binary: an extractor holding zero compiled rules is
  /// exactly the observed failure state (file nodes only, definitions silently gone), and it
  /// must fail the self-check rather than index.
  #[test]
  fn gutted_extraction_is_rejected() {
    // One lone Rust rule: every other language extracts nothing — the observed failure shape.
    let extractor = OutlineExtractor::from_rules(
      "id: canary-rust-struct\nlanguage: Rust\nrole: item\nsymbolType: struct\nrule:\n  pattern: 'struct $NAME { $$$BODY }'\nname: $NAME\n",
    )
    .expect("single-rule set compiles");
    let err = verify_extraction(&extractor).expect_err("gutted rules must fail the self-check");
    assert!(
      err.contains("extraction self-check failed"),
      "unexpected message: {err}"
    );
  }

  /// Diagnostic inventory: exact per-language counts, for updating the table deliberately.
  /// Run with `--ignored --nocapture`.
  #[test]
  #[ignore = "diagnostic: prints per-canary extraction counts"]
  fn probe_canary_counts() {
    let extractor = OutlineExtractor::new().expect("default rules compile");
    for canary in CANARIES {
      let product = extractor.extract_product(canary.path, canary.source);
      let (items, refs) = product
        .as_ref()
        .map(|p| (p.items.len(), p.refs.len()))
        .unwrap_or((0, 0));
      println!(
        "{:<12} items {items:>2} (min {}), refs {refs:>2} (min {})",
        canary.lang.to_string(),
        canary.min_items,
        canary.min_refs
      );
    }
  }

  /// The table stays in lockstep with the languages the extractor actually serves: every
  /// language with a reference spec must have a canary that demands at least one reference,
  /// and every canary language must be handled at all.
  #[test]
  fn canary_table_covers_every_extractable_language() {
    let extractor = OutlineExtractor::new().expect("default rules compile");
    for &lang in SupportLang::all_langs() {
      // all_langs() is the ENABLED set, so this coverage check matches what selfcheck runs.
      let canary = CANARIES.iter().find(|c| c.lang == lang);
      let has_ref_spec = crate::references::ref_spec(lang).is_some();
      match canary {
        Some(canary) => {
          assert!(
            !has_ref_spec || canary.min_refs > 0,
            "{lang} has a reference spec but its canary demands no references"
          );
          assert!(
            extractor.handles(canary.path),
            "{lang} canary path {} is not handled — extension routing broke",
            canary.path
          );
        }
        None => {
          assert!(
            !has_ref_spec,
            "{lang} has a reference spec but no self-check canary"
          );
        }
      }
    }
  }
}

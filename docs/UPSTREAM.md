# Upstream (ast-grep) synchronization ledger

> **Base:** ast-grep **v0.44.0** (release tarball; the fork predates commit-pinning — pin the
> exact commit at the next sync). **Last reconciliation:** 2026-07-24 against v0.44.1.

Vorpal's structural engine (`crates/core`, `crates/config`, `crates/language`, `crates/cli`'s
run/scan/test/LSP surfaces, and the napi/pyo3/wasm bindings) is a rebranded fork of ast-grep.
Compatibility with upstream patterns, rules, and CLI behavior is a maintained contract, not an
accident. This ledger records where and why the trees diverge.

## Process

1. Each upstream release: diff the engine crates, adopt or record every change.
2. Adopted changes land as ordinary commits referencing the upstream release.
3. Changes we cannot adopt get a row in the divergence table with a reason.
4. Differential fixtures (`vorpal run`/`scan`/`outline` golden outputs) guard the shared
   behavior; intentional differences get their own fixtures.

## Reconciled against v0.44.1

| Upstream change | Status |
|---|---|
| Outline extraction: bounded `sync_channel(256)` producer queue | **Adopted** (2026-07-24, `crates/cli/src/outline/extract.rs`) |

## Intentional divergences (vorpal-side)

| Area | Divergence | Reason |
|---|---|---|
| Branding / crate names | `ast-grep-*` → `vorpal-*`, CLI `sg`/`ast-grep` → `vorpal`/`vp` | Product identity; API shapes preserved |
| Scan/run prefilter | SIMD literal prefilter incl. regex required-literal analysis + `vorpal-ignore` suppression awareness | Perf (kernel-scale scan at/below ripgrep); behavior-preserving by necessary-condition design |
| Outline default rules | Bundled outline rules for all 28 languages; C/C++ declarator-identity fixes (pointer/array/fn-ptr names, body-required type definitions, method vs fn-ptr-field classification) | Feeds the knowledge graph; upstream coverage is narrower |
| Repository layer | `index`/`graph`/`search`/`mcp` subcommands, knowledge graph, ANN, bindings additions | Vorpal's differentiating layer; additive |
| Allocator | jemalloc + tree-sitter allocator unification in the binaries | Measured RSS/fault wins; no behavior change |

## Known-not-yet-reconciled

- Upstream `0.44.1` non-outline patches have not been case-by-case audited (this ledger
  starts 2026-07-24); next pass should diff `crates/core`/`crates/config` against the
  v0.44.1 tag and fill this table.

---

# Tree-sitter grammar ledger

A *separate* upstream from ast-grep: the per-language tree-sitter parsers. Most are consumed
straight from crates.io (pinned in `crates/language/Cargo.toml`). Some are **vendored** into
`grammars/<name>/` and injected via `[patch.crates-io]` in the workspace `Cargo.toml`, so we own
the exact parser bytes and can carry local fixes without waiting on an upstream release.

The compiled-in reality is inspectable at runtime with **`vorpal grammars`** (ABI, declared
semver, node/state counts, and the generation **digest**). That digest — a fingerprint of the
parser's structure — is what the **product cache keys on** (`vorpal_language::grammar_digest`,
product header v8): editing a vendored grammar changes its digest, which invalidates exactly that
language's cached products on the next index. Use `vorpal grammars` to confirm this table and the
binary agree.

## Re-syncing a vendored grammar

1. Fetch the target upstream tag/commit; copy the published-crate layout into `grammars/<name>/`
   (`grammar.js`, `src/`, `bindings/`, `queries/`, `tree-sitter.json`, `Cargo.toml`,
   `.cargo_vcs_info.json`, and — for regression coverage — `test/corpus/`).
2. Re-apply the local patches listed below (`tree-sitter generate` after editing `grammar.js`).
3. `tree-sitter test` (corpus must pass), then rebuild and re-run the corpus + a real-corpus
   index; confirm parse-error counts and determinism.
4. Update the row here (version, commit, patch summary) and bump the recorded commit.

## Vendored grammars

| Language | Path | Version | Upstream commit | Local patches |
|---|---|---|---|---|
| Python | `grammars/tree-sitter-python` | 0.25.0 | `293fdc02038ee2bf0e2e206711b69c90ac0d413f` | **PEP 810 lazy imports** — `lazy` soft keyword before `import`/`from` via an external-scanner context token (`src/scanner.c`), `optional($.lazy)` on both import rules, `+test/corpus/lazy_imports.txt`. See [[vendored-grammars-pep810]]. |

## From crates.io (pinned, not vendored)

Pinned in `crates/language/Cargo.toml`; vendor-on-demand when a fix is needed.

| Language | Crate version | | Language | Crate version |
|---|---|---|---|---|
| Bash | 0.25.0 | | Elixir | 0.3.0 |
| C | 0.24.0 | | Go | 0.25.0 |
| C++ | 0.23.0 | | Haskell | 0.23.0 |
| C# | 0.23.0 | | HCL | 1.1.0 |
| CSS | 0.25.0 | | HTML | 0.23.0 |
| Dart | 0.2.0 | | Java | 0.23.0 |
| JavaScript | 0.25.0 | | Ruby | 0.23.0 |
| JSON | 0.24.8 | | Rust | 0.24.0 |
| Kotlin | 0.4.1 (`-sg`) | | Scala | 0.26.0 |
| Lua | 0.5.0 | | Solidity | 1.2.11 |
| Markdown | 0.5.3 | | Swift | 0.7.0 |
| Nix | 0.3.0 | | TypeScript/TSX | 0.23.2 |
| PHP | 0.24.0 | | YAML | 0.7.0 |

## Planned

- **tree-sitter-c**: vendor + add a name-agnostic recovery production for function-like macros in
  specifier position (`IDENT(args)` before a type/declarator, e.g. `__alloc_size(1)`), which
  currently produce an unrecoverable ERROR and drop the function. Validate on non-kernel
  macro-heavy corpora before landing.
- Vendor the remaining grammars as concrete fixes arise (the cache-key + this ledger make it
  turnkey).

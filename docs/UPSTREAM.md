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

A *separate* upstream from ast-grep: the per-language tree-sitter parsers. **All** are now
**vendored** into `grammars/<name>/` and injected via `[patch.crates-io]` in the workspace
`Cargo.toml`, so we own the exact parser bytes and can carry local fixes without waiting on an
upstream release. Each was copied from its published crate at the resolved version (the
`.cargo_vcs_info.json` in each records the upstream commit); vendoring is **byte-identical** to
crates.io except where a local patch is noted, so `vorpal grammars`' `global grammar stamp` is
unchanged and no product cache was invalidated by the move.

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

All under `grammars/<crate>/`, patched into the workspace. `Local patches` is "—" unless noted.

| Crate | Version | Upstream commit | Local patches |
|---|---|---|---|
| tree-sitter-bash | 0.25.1 | `a06c2e4415e9` | — |
| tree-sitter-c | 0.24.2 | `b780e47fc780` | — (macro-recovery work reverted; see Planned) |
| tree-sitter-c-sharp | 0.23.5 | `cac6d5fb595f` | — |
| tree-sitter-cpp | 0.23.4 | `f41e1a044c8a` | — |
| tree-sitter-css | 0.25.0 | `dda5cfc5722c` | — |
| tree-sitter-dart | 0.2.0 | `b57d734c84f5` | — |
| tree-sitter-elixir | 0.3.5 | `e2d9e6e0e76b` | — |
| tree-sitter-go | 0.25.0 | `1547678a9da5` | — |
| tree-sitter-haskell | 0.23.1 | `c30d812bc908` | — |
| tree-sitter-hcl | 1.1.0 | `009def4ae38e` | — |
| tree-sitter-html | 0.23.2 | `5a5ca8551a17` | — |
| tree-sitter-java | 0.23.5 | `94703d5a6bed` | — |
| tree-sitter-javascript | 0.25.0 | `44c892e0be05` | — |
| tree-sitter-json | 0.24.8 | `ee35a6ebefce` | — |
| tree-sitter-kotlin-sg | 0.4.1 | `1a6f9b1ee112` | — (crate `tree-sitter-kotlin-sg`, imported as `tree-sitter-kotlin`) |
| tree-sitter-lua | 0.5.0 | `10fe0054734e` | — |
| tree-sitter-md | 0.5.3 | `f969cd3ae3f9` | — (block + inline parsers) |
| tree-sitter-nix | 0.3.0 | `ea1d87f7996b` | — |
| tree-sitter-php | 0.24.2 | `5b5627faaa29` | — (php + php_only parsers) |
| tree-sitter-python | 0.25.0 | `293fdc02038e` | **PEP 810 lazy imports** — `lazy` soft keyword before `import`/`from` via an external-scanner context token (`src/scanner.c`), `optional($.lazy)` on both import rules, `+test/corpus/lazy_imports.txt`. See [[vendored-grammars-pep810]]. |
| tree-sitter-ruby | 0.23.1 | `71bd32fb7607` | — |
| tree-sitter-rust | 0.24.2 | `e2bee853694a` | — |
| tree-sitter-scala | 0.26.0 | `38950b525c9d` | — |
| tree-sitter-solidity | 1.2.13 | `4e938a46c703` | — |
| tree-sitter-swift | 0.7.3 | `b8b22bffbb34` | — |
| tree-sitter-typescript | 0.23.2 | `f975a621f4e7` | — (typescript + tsx parsers) |
| tree-sitter-yaml | 0.7.2 | `7708026449be` | — |

## Planned

- **tree-sitter-c**: vendor + add a name-agnostic recovery production for function-like macros in
  specifier position (`IDENT(args)` before a type/declarator, e.g. `__alloc_size(1)`), which
  currently produce an unrecoverable ERROR and drop the function. Validate on non-kernel
  macro-heavy corpora before landing.
- Vendor the remaining grammars as concrete fixes arise (the cache-key + this ledger make it
  turnkey).

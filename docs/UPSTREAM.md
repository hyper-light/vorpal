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

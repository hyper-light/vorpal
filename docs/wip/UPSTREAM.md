# Upstream (ast-grep) synchronization ledger

> **Base:** ast-grep **v0.44.0** (release tarball; the fork predates commit-pinning).
> **Last reconciliation:** 2026-07-26 against **v0.45.0** (`5d439d9bb92d5ba9e7dba8343348c4597e7a1fbc`).

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

## Reconciled against v0.45.0

Behavioral commits between the base and v0.45.0, classified. Adopted changes match upstream
byte-for-byte where the surrounding code had not diverged.

| Upstream commit | Change | Status |
|---|---|---|
| `a18c29c8` | Smart-strictness skips trivia via `SgNode::is_extra` instead of a `kind().contains("comment")` heuristic | **Adopted** — `is_extra` added to `core/{node,source,tree_sitter}`, `strictness.rs` now identical to upstream |
| `ada747d0` | Smart strictness ignores comments by default (moved to the skip-trivia group) | **Adopted** — same `strictness.rs` |
| `94bc9582` | Stop consulting ignore files outside `ruleDirs`/`utilDirs` (`.parents(false)`) | **Adopted** — `cli/src/config.rs`, both walker sites |
| `63e94c48` + `4f75c214` | Outline recognizes TypeScript `namespace` and ambient `declare module` (incl. `ambient_declaration` wrapping), TS + TSX | **Adopted** — 8 rules appended to `outline/src/default_rules/typescript.yml`; verified extracting `export namespace` and `declare module "x"` |
| `07080186` | Bump `ignore` crate to 0.4.27 | **Not applicable** — dependency version, tracked by the workspace lockfile |
| `sg` deprecation, MSRV/dep bumps | CLI alias + toolchain | **Intentional divergence** — vorpal ships its own CLI identity (`vorpal`/`vp`); MSRV governed locally |

Differential fixtures comparing vorpal against the pinned ast-grep binary across
`run`/`scan`/`test`/outline/JSON/edits are still to be added (the `#1` "Done when" bar).

## Reconciled against v0.44.1

Complete audit of all 24 commits between `0.44.0` and `0.44.1` (2026-07-29) — the ledger has no
unaudited window between the fork base and the declared v0.45.0 baseline.

| Upstream commit | Change | Status |
|---|---|---|
| `fe3607ea` | Outline extraction: bounded `sync_channel(256)` producer queue | **Adopted** (2026-07-24, `crates/cli/src/outline/extract.rs`) |
| `24a573f1` | Load custom-language outline rules from `customLanguages` config | **Already present** — `crates/cli/src/config.rs` carries the same per-custom-language `outline_rules` collection (`custom_language_outline_rules`) |
| `07778fb3` | Outline rules for more builtin languages | **Superseded** — vorpal bundles outline defaults for all 28 supported languages (upstream covers a subset); the v0.45.0 TypeScript ambient additions were adopted separately |
| `06e5ba27` | Map `*.bazel` files to Python | **Already present** — vorpal's Python extension list includes `bzl` and `bazel` |
| `cd217149` / `05459688` | tree-sitter / web-tree-sitter 0.26.10 | **Adopted** — workspace is on tree-sitter 0.26.10 |
| `1713e86f` | pyo3 dependency update | **Not applicable** — dependency versions governed by vorpal's own workspace lockfile |
| 12 × `chore(deps)` (napi, napi-derive, oxlint ×2, terminal-light, clap_complete ×2, ignore 0.4.27, @ast-grep/napi, dprint, @napi-rs/cli, anyhow, wasm-bindgen) | Dependency bumps | **Not applicable** — same reason; vorpal pins its own versions |
| `8e88a9e5` | actions/cache v6 | **Not applicable** — CI configuration; vorpal maintains its own workflows |
| `26f78451` | 0.44.1 version bump | **Not applicable** |

## Intentional divergences (vorpal-side)

| Area | Divergence | Reason |
|---|---|---|
| Branding / crate names | `ast-grep-*` → `vorpal-*`, CLI `sg`/`ast-grep` → `vorpal`/`vp` | Product identity; API shapes preserved |
| Scan/run prefilter | SIMD literal prefilter incl. regex required-literal analysis + `vorpal-ignore` suppression awareness | Perf (kernel-scale scan at/below ripgrep); behavior-preserving by necessary-condition design |
| Outline default rules | Bundled outline rules for all 28 languages; C/C++ declarator-identity fixes (pointer/array/fn-ptr names, body-required type definitions, method vs fn-ptr-field classification) | Feeds the knowledge graph; upstream coverage is narrower |
| Repository layer | `index`/`graph`/`search`/`mcp` subcommands, knowledge graph, ANN, bindings additions | Vorpal's differentiating layer; additive |
| Allocator | jemalloc + tree-sitter allocator unification in the binaries | Measured RSS/fault wins; no behavior change |

## Differential compatibility testing

`crates/cli/tests/differential.rs` compares vorpal's inherited structural surfaces against a
**pinned upstream ast-grep binary** on shared fixtures (pattern `run` and rule `scan`, JSON
outputs normalized for binary name/paths). The job is env-gated: set `VORPAL_ASTGREP_BIN` to an
ast-grep binary built from the baseline commit (`5d439d9`, v0.45.0) to activate it; without the
variable the tests skip loudly rather than pass silently. Intentional divergences are asserted
as fixtures there, not prose exceptions. Every future baseline advance must (1) classify all new
upstream commits in this ledger and (2) pass the differential job against the new pin.

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

## Tree-sitter runtime (vendored)

The tree-sitter **runtime** — not just the grammars — is now vendored into `vendor/tree-sitter/`
and injected via `[patch.crates-io]`, at the resolved version **0.26.10** (byte-identical to
crates.io as copied). Cold indexing is ~two-thirds parser CPU, so the C runtime
(`lexer.c`/`parser.c`/`subtree.c`/`stack.c`) is ours to profile and optimize in place. Every
change below was verified **byte-identical**: the kernel and CPython index content-ids
(`gen/<id>`) are unchanged, and the full suite + `retrieval_eval` stay green.

| Patch | Where | Effect |
|---|---|---|
| **Lexer ASCII fast path** | `vendor/tree-sitter/src/lexer.c` (`ts_lexer__get_lookahead`) | A byte `< 0x80` under UTF-8 is a self-decoding codepoint; handle it inline instead of the encoding dispatch + indirect `decode(...)` call that otherwise ran per character. ~7% off kernel cold-index. |
| **`set_contains` ASCII fast path** | 23 grammars' `src/tree_sitter/parser.h` | Character sets are sorted by range start; for a lookahead `< 0x80` a short linear scan over the leading ranges beats ~log2(len) probes across the full table (the C identifier set alone is 687 ranges). Recorded per-grammar in `grammars/PROVENANCE.json` `patches`. |

Both are pure membership/decoding fast paths — same tokens, same trees. Build note: a grammar's
`build.rs` tracks `src/parser.c`, **not** the headers, so after editing a bundled `parser.h`
you must `touch` that grammar's `parser.c` (or `cargo clean -p <grammar>`) to force cc to
recompile. Indexing worker count is tunable via `VORPAL_INDEX_THREADS` (default: CPU count);
mild oversubscription hides per-file read stalls on a parse-bound corpus.

## Licenses (inherited engine + vendored parsers)

The inherited structural engine is MIT; the repository root `LICENSE` preserves ast-grep's
attribution (© Herrington Darkholme) unchanged. Every vendored grammar's license is recorded
in `grammars/PROVENANCE.json` and **enforced by test**: the declared license field must be
present and the license text itself must be vendored in the tree (all 27 currently MIT;
nine missing license texts were fetched from their pinned upstream commits when enforcement
first ran — see the provenance commit history).

## Known upstream behaviors

**Error recovery is optimization-sensitive** (tree-sitter C runtime): the same vendored
sources compiled under different optimization profiles (workspace `lto = true` vs a plain
release profile) can recover *different trees* inside ERROR regions. Measured on the Linux
kernel (2026-08-17): 2,748,638 vs 2,748,539 nodes, every divergent file an assembly-macro
C header the grammar cannot parse; with `--parse-health exclude` both compilations produce
byte-identical `nodes.vseg`/`graph.bin`/`strings.heap`/`evidence.bin`. Healthy-file parses
are bit-stable across profiles, and allocator choice (jemalloc vs system) provably does not
matter (same generation id). Vorpal's determinism contract — same binary, bit-identical
output — is unaffected; this only concerns byte-agreement across *differently compiled*
binaries, and docs/EMBEDDING.md carries the guidance.

## Provenance, corpora, and the reproducible update procedure (IMPROVEMENTS #10)

Machine-readable provenance lives in **`grammars/PROVENANCE.json`** (repository, version,
pinned commit, license, generator ABI, local patches, full-tree digest), enforced on every
`cargo test` by `crates/language/tests/grammar_provenance.rs` — any vendored byte change fails
until provenance is regenerated and the diff is owned in review. Each grammar's **upstream test
corpus** is vendored at the pinned commit and executed against the compiled parsers by
`crates/language/tests/grammar_corpus.rs` (4k+ tests; `:skip`/`:platform`/`:error`/`:language`
attributes honored; exclusions and allowlist entries carry written reasons). A **weekly audit**
(`.github/workflows/grammar-audit.yml` → `scripts/grammar_audit.py`) re-verifies pinned commits
exist upstream and our parser sources byte-match them, and reports newer upstream tags into a
tracking issue.

Updating a grammar is deliberate and reproducible:

1. `curl -L https://codeload.github.com/<org>/<repo>/tar.gz/<new-commit-or-tag>` and replace
   `grammars/<name>/` wholesale (keep local patches by re-applying them on top; record each in
   the provenance `patches` field).
2. Re-import its `test/corpus` from the same tarball.
3. `cargo test -p vorpal-language --test grammar_provenance -- --ignored regenerate` (preserves
   commit + patches fields; update `commit` to the new pin) and update this ledger's table row.
4. Let the gates arbitrate the PR: provenance enforcement, the full corpus run, and the
   workspace suite. A parser-semantics change invalidates exactly that language's cached
   products via the grammar digest — no manual cache handling.

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
| tree-sitter-rust | 0.24.2 | `77a3747266f4` | — |
| tree-sitter-scala | 0.26.0 | `38950b525c9d` | — |
| tree-sitter-solidity | 1.2.13 | `4e938a46c703` | — |
| tree-sitter-swift | 0.7.3 | `b8b22bffbb34` | — |
| tree-sitter-typescript | 0.23.2 | `f975a621f4e7` | — (typescript + tsx parsers) |
| tree-sitter-astro-next | 0.1.1 | `15a3b95bf444` (crate vcs commit) | upstream ships no test corpus; canary + lang_matrix gate extraction |
| tree-sitter-cmake | 0.7.4 | `v0.7.4` tag | — |
| tree-sitter-erlang | 0.20.0 | `0.20` tag | — |
| tree-sitter-jsdoc | 0.25.0 | `658d18dcdddb` (crate vcs commit) | injection-target grammar (JS comment blocks via languageInjections config) |
| tree-sitter-julia | 0.23.1 | `a8e1262997d5` (crate vcs commit) | corpus imported at the same commit |
| tree-sitter-objc | 3.0.2 | `v3.0.2` tag | — |
| tree-sitter-ocaml | 0.25.0 | `v0.25.0` tag | multi-grammar crate; vorpal binds LANGUAGE_OCAML (.ml); .mli deferred |
| tree-sitter-perl | 1.1.2 | `883ab51150f3` (crate vcs commit) | corpus imported at the same commit (no 1.1.2 tag upstream) |
| tree-sitter-powershell | 0.26.4 | `v0.26.4` tag | — |
| tree-sitter-r | 1.3.0 | `v1.3.0` tag | — |
| tree-sitter-sequel | 0.3.11 | `v0.3.11` tag | derekstride/tree-sitter-sql, published as tree-sitter-sequel |
| tree-sitter-zig | 1.1.2 | `v1.1.2` tag | upstream ships NO test corpus (any version); canary + lang_matrix are the extraction gates |
| tree-sitter-dockerfile | 0.2.0 | `v0.2.0` tag | bindings modernized to LanguageFn (crate 0.2.0 shipped `fn language()` on tree-sitter 0.20) |
| tree-sitter-graphql | 0.2.1 | `v0.2.1` tag | — |
| tree-sitter-ini | 1.4.0 | `v1.4.0` tag | — |
| tree-sitter-make | 1.1.1 | `v1.1.1` tag | — |
| tree-sitter-proto | 0.5.0 | `0.5.0` tag | — |
| tree-sitter-svelte-ng | 1.0.2 | `774a65aea563` (crate vcs commit) | — |
| tree-sitter-toml-ng | 0.7.0 | `64b56832c2cf` | — |
| tree-sitter-vue | 0.0.3 | `8bbcd4cbd59c` (crate vcs commit) | embedded html-scanner copy de-exported (duplicate C symbols silently replaced the real HTML scanner at link); bindings modernized to LanguageFn |
| tree-sitter-xml | 0.7.0 | `v0.7.0` tag | — |

Deliberately NOT vendored: **tree-sitter-jinja2** (crates.io 0.0.16, uros-5) — Cargo.toml
declares MIT but the repository ships no license text at any ref (GitHub license API: none);
blocked pending upstream. Also deliberately NOT vendored: **tree-sitter-groovy** (crates.io 0.1.2, amaanq) — its Cargo.toml
declares MIT but the repository ships **no license text at any ref** (GitHub license API:
none). Vendoring is blocked until upstream adds one; Gradle Kotlin-DSL files (.gradle.kts)
already route through Kotlin.
| tree-sitter-yaml | 0.7.2 | `7708026449be` | — |

## Planned

- **tree-sitter-c macro recovery** (the grammar itself is already vendored, as-is): add a
  name-agnostic recovery production for function-like macros in specifier position (`IDENT(args)`
  before a type/declarator, e.g. `__alloc_size(1)`), which currently produce an unrecoverable
  ERROR and drop the function. A leading-position-only prototype passed the upstream corpus but
  regressed real CPython C at scale (ref-resolution shifted), so it was reverted; a real fix needs
  position-specific handling (leading / between-type-and-declarator / trailing) each validated on
  a large non-kernel C corpus before landing. Prototype kept in scratch.

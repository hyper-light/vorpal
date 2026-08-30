# Benchmarks — commands, datasets, hardware, and raw results

Reproducible measurements only (release builds, stated machine state). Every number below
was produced by the exact command shown, on the stated hardware and dataset commits, on
**2026-08-30** (prior sweep 2026-07-31; deltas are called out inline). Numbers are honest
points, not marketing: re-run them on your hardware; the commands are the contract.
`VORPAL_NO_AUTOWARM=1` is set throughout so background warms never blur a measurement.
Cold rows are best-of-3; sub-0.5 s wall deltas are unresolvable on this hardware.

## Hardware and toolchain

- Apple M5 Max, 18 cores, 128 GiB RAM, macOS 26.4.1
- rustc 1.98.0 (pinned via rust-toolchain.toml); `cargo build --release`
- Datasets: `linux` @ `1590cf032971` (72,541 indexable files), `cpython` @ `b86a41cbf63`
  (3,592 indexable files), this repository (856 files including the vendored tree-sitter
  runtime and grammars — generated `parser.c` files up to 33 MB)

## Definitions

- **cold**: no index root exists (`rm -rf <out>`); everything parses.
- **warm-unchanged**: immediately re-run on an unchanged tree — the manifest fast path.
- **touch**: one file's mtime bumped, contents identical — the product-equality cutoff
  proves the graph unchanged and commits by hardlink (no re-link).
- **one-file edit**: one real content edit — stat sweep + one re-parse + 72,540 pack
  replays + full deterministic re-link/seal.
- **tiers warm**: `ann.bin` fresh under stamp + model-provenance gates. Without them,
  search takes the exact fallback (slower, identical results — pinned by test).

## Indexing

```
vorpal-index index ~/Projects/linux /tmp/bench-lk        # cold, then re-run for warm
vorpal-index index ~/Projects/cpython /tmp/bench-py
```

| Measurement | wall | user CPU | notes |
|---|---|---|---|
| linux cold | 6.31 s (best-of-3: 6.31/6.58/6.94) | 93–100 s | 72,541 files → 2,748,638 nodes; 2.17 M refs resolved |
| linux warm-unchanged | 0.10 s | — | manifest fast path, no file reads |
| linux touch (contents unchanged) | 0.20 s | — | product-equality cutoff; was reported 2.30 s pre-cutoff |
| linux one-file edit | 0.98 s | 9.8 s | replay + deterministic re-link (was 2.30 s on 2026-07-31) |
| cpython cold | 0.67 s | 8.4 s | 3,592 files → 143,450 nodes |
| this repo cold | 3.8 s | 30 s | 856 files → 44,359 nodes; the wall is one 33 MB generated `parser.c` |
| this repo one-file edit | 0.04 s | — | |
| this repo warm-unchanged | 0.02 s | — | |

2026-08-30 pass (7.01 → 6.31 s cold; one-file edit 2.30 → 0.98 s): the references walk now
carries an explicit ancestor stack (`PreWithDepth`) instead of per-node `Node::parent`
root-walks — bit-identical output, cursor-walk profile bucket 26.2% → 22.2%; the interner's
dedup map moved to FxHash (`4ad36b1`); and the incremental path's earlier absorber/stream
work landed. Leaf-weight attribution of the remaining cold build: tree-sitter runtime 83.9%
of on-CPU (parse+lex+stack ~30%, cursor walking ~22%, subtree machinery ~10%), vorpal
extraction 5.3%, allocator 4.2%, memmove 2.0%. jemalloc config probes (decay, narenas)
measured dead — the cost is call count, not purge policy.

2026-08-29 tree-sitter runtime pass (7.85 s → 7.01 s, output bit-identical): vendored
runtime (`vendor/tree-sitter`, docs/UPSTREAM.md) with a lexer ASCII fast path (~7%), an
ASCII fast path in the grammars' `set_contains`, and O(1) `has_error` instead of a
whole-tree error scan. `target-cpu=native`, parser reuse, and worker oversubscription
measured and rejected (see git history for the numbers).

Index size (linux): generation 2.0 GiB — includes the 811 MB vector tier and the 581 MB
product pack; the index root retains the current + prior generation (older ones are GC'd
at commit).

## Search (linux index, k=10, query "socket buffer alloc")

```
vorpal-index search /tmp/bench-lk "socket buffer alloc" 10
```

| State | wall | user CPU | path taken |
|---|---|---|---|
| no tiers | 0.28 s | 1.98 s | exact fallback (exhaustive semantic + name scan) |
| tiers warm | 0.02 s | 0.01 s | ANN beam + posting intersection |

Vector tier build (one-time per generation; in daemon use it runs in the background and
per-edit maintenance replaces it — see the daemon section): **19.13 s wall / 294 core-s**,
at measured **pool recall 0.9937** (`ann.calibration.json`: 32 seeded probes, exact
quantized oracle, l=200 — the production search shape). The 2026-07-31 sweep reported
12.53 s at what later measured as pool recall 0.9125 with 66k structurally unreachable
nodes; the current build spends the extra time on two frozen refinement rounds and
in-coverage repair (recall 0.9937, 0 unreachable). The full adopt/reject ledger with
mechanisms is docs/wip/ANN_FRONTIER.md. Results remain byte-identical with and without
tiers (test-pinned): warm state changes latency only, never answers.

## Daemon (MCP server, linux tree, warm)

The daemon serves from a retained in-memory graph and a LIVE vector tier keyed by durable
node identity: edits apply as per-row tier updates, and the full tier rebuild demotes to a
background compactor. Measured over stdio JSON-RPC (real round trips, 50-sample medians):

| Measurement | value |
|---|---|
| Boot → vector tier adopted and serving | 2–4 s (reconciles the persisted tier; zero rebuilds) |
| Graph tool call (`callers`), warm | **< 0.1 ms** median round-trip |
| Hybrid `search` via the live tier, warm | **27 ms** median (p90 28 ms) |
| One-file edit saved → fresh answer served | ~0.5 s |
| Per-edit vector-tier maintenance | ~140 ms, background (−145/+115 rows typical) |
| Daemon CPU per edit/restore cycle | ~3.3 core-s (was ~83 before the live tier) |
| 60-file edit burst → query | ~1.1 s; tier retires and re-adopts in the background |

Tier quality is measured, not assumed: the daemon re-probes pool recall on a PINNED probe
set per 1% of live-row churn (self-anchored baseline; measured equivalence with build
calibration to the fourth decimal) and retires the tier to the compactor on tombstone debt
> 5% **or** measured recall through the degradation bar.

## Structural scan vs text grep (linux, 63,775 C files)

```
vorpal scan --rule /tmp/rule.yml ~/Projects/linux     # kind: call_expression + regex: kmalloc
rg 'kmalloc\(' -t c ~/Projects/linux                  # comparison
```

| Tool | wall | what you get |
|---|---|---|
| `vorpal scan` (kind + regex rule) | 1.39–1.46 s | 42.6k kind-scoped structural matches from full ASTs |
| `ripgrep` | 0.68–0.72 s | raw text lines |

A full parse of every file plus structural matching costs ~2× a raw text grep of the same
tree. (An earlier README line claimed vorpal was faster than ripgrep here; on the current
build and this rule shape it is not, and the claim is retired.)

## Suites (debug builds, `cargo test`)

- Grammar corpus: 4,279 upstream tests across 27 grammars in **0.43 s**
  (`cargo test -p vorpal-language --test grammar_corpus`).
- Retrieval quality: 10 labelled queries, fused recall@5 = 1.0, MRR = 1.0, mean query
  ~0.9 ms on the fixture corpus (`cargo test -p vorpal-index --test retrieval_eval -- --nocapture`).
- Workspace: 110 test binaries green at the measured head.

## Determinism

Every build above is bit-reproducible: rebuilding an unchanged tree produces the same
generation content id (verified again 2026-08-30 — two independent cold kernel builds both
committed generation `2ff6da6af3e21b2dbc31da974b820221`), and the ANN tier rebuilds
byte-identical (`ann.bin` sha stable across rebuilds — the standing gate every format
change re-verifies).

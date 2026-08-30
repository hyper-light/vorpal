# Benchmarks — commands, datasets, hardware, and raw results

Reproducible measurements only (release builds, stated machine state). Every number below
was produced by the exact command shown, on the stated hardware and dataset commits, on
2026-08-30. Numbers are honest points, not marketing: re-run them on your hardware; the
commands are the contract. `VORPAL_NO_AUTOWARM=1` is set throughout so background warms
never blur a measurement, and every timed run was taken with the 1-minute load average
below 3 (a loaded machine doubled cold wall time with the same user CPU — those runs were
discarded, not averaged).

## Hardware and toolchain

- Apple M5 Max, 18 cores, 128 GiB RAM, macOS 26.4.1
- rustc 1.98.0 (pinned via rust-toolchain.toml); `cargo build --release -p vorpal-index -p vorpal`
- Datasets: `linux` @ `1590cf032971` (75,954 indexable files under 49 grammars), `cpython` @
  `b86a41cbf63` (3,841 indexable files)
- vorpal @ `3bf4b5e` (product format 15, typefacts 4, 49 grammars)

## Definitions

- **cold**: no index root exists (`rm -rf <out>`); everything parses.
- **warm-unchanged**: immediately re-run on an unchanged tree — the manifest fast path.
- **one-file update**: `touch` one file, re-index — stat sweep, one re-parse, every other
  product replayed from the prior generation's pack, then a full deterministic re-link/seal.
- **tiers warm**: `ann.bin` + `postings.bin` built and stamp/provenance-fresh. Without
  them, search takes the exact fallback.

## Indexing

```
vorpal-index index ~/Projects/linux /tmp/bench-lk        # cold; re-run for warm; touch a file for one-file
vorpal-index index ~/Projects/cpython /tmp/bench-py
```

| Measurement | wall (best-of-3) | user CPU | notes |
|---|---|---|---|
| linux cold | 7.04 s (7.04 / 7.33 / 7.99) | 101–107 s | 75,954 files → 2,763,928 nodes; 2,180,009 refs resolved |
| linux warm-unchanged | 0.13 s (0.13 / 0.13 / 0.13) | 0.23 s | manifest fast path, no file reads |
| linux one-file update | 2.11 s (2.11 / 2.17 / 2.22) | 16.3 s | 1 parsed, 75,953 replayed; deterministic re-link + seal |
| cpython cold | 0.88 s (0.88 / 0.89 / 0.89) | 10.4 s | 3,841 files → 150,470 nodes; 234,161 refs resolved |
| cpython warm-unchanged | 0.02 s | 0.03 s | |
| cpython one-file update | 0.23 s | 1.0 s | 1 parsed, 3,840 replayed |

2026-08-30 incremental replay fix (`8d83f2c`): since product v14 (2026-08-30 morning,
`12c9472`) every incremental build had been silently re-parsing ~80% of the tree — the
product validator was a second, hand-rolled byte walker that never learned the v14/v15
reference layout, failed on any product with references, and re-extracted the file. Kernel
one-file update measured 6.9 s (55,463 of 75,954 files re-parsed) before the fix, 2.11 s
after; cpython 0.96 s → 0.23 s. Cold builds never touch that path, which is why cold A/Bs
looked clean throughout. The validator now IS the decoder, and a test pins replay counts
(one re-extract, N−1 replays), which no test did before. A zero-allocation shared-walker
variant was built and A/B'd against that base — dead even (2.09–2.22 s vs 2.10–2.22 s,
identical user CPU), so it did not land: validation is not on the incremental critical
path; re-link + seal is.

Index size (linux, one generation): 1.27 GiB before tiers — `products.pack` 595 MB,
`evidence.bin` 268 MB, `nodes.vseg` 171 MB, `strings.heap` 150 MB, `graph.bin` 122 MB,
`names.idx` 44 MB, `manifest.bin` 6.8 MB, `products.idx` 6.5 MB, `dataflow.bin` 0.75 MB
(12,946 flow edges at kernel scale — the kernel is C, outside the typed-capture languages).
2.07 GiB after the search tiers warm (`ann.bin` 811 MB, `postings.bin` 40 MB, `ann.files`
7 MB). A CLI-only flow keeps no loose product bank, so the root is the generation.

## Search (linux index, k=10, query "socket buffer alloc")

```
vorpal-index search /tmp/bench-lk "socket buffer alloc" 10     # exact fallback until tiers exist
vorpal-index __warm-ann /tmp/bench-lk                            # builds both tiers (the daemon does this in the background)
vorpal-index search /tmp/bench-lk "socket buffer alloc" 10     # tiers warm
```

| State | wall | user CPU | path taken |
|---|---|---|---|
| no tiers | 0.31 s (0.31 / 0.31 / 0.32) | 2.05 s | exact fallback (exhaustive semantic + name scan) |
| tiers warm | 0.02 s | 0.01 s | ANN beam + posting intersection |

Tier build (one-time per generation, heals in the background in daemon use): 12.8 s wall /
178 s user for both tiers at kernel scale.

**Tier answers are approximate at this scale — measured, not assumed.** The previous edition
of this document claimed tier and exact results were byte-identical (a fixture test pins
that). At kernel scale they are not: over eight queries (`socket buffer alloc`, `page fault
handler`, `tcp congestion window`, `mutex lock acquire`, `inode lookup path`, `interrupt
request register`, `dma coherent alloc`, `scheduler pick next task`) the tier's top-10
overlapped the exact top-10 in **66/80** positions, ranging from 10/10 down to 4/10
(`mutex lock acquire`). The fused ranking is deterministic — the misses are candidates the
ANN beam never surfaced (e.g. `socket_alloc` at exact rank 3), consistent with the beam-width
reduction in `a048aa0`. Recorded here as an open finding for the ANN owner; the exact path
remains the reference answer.

## Graph and query latency (linux index, warm, best-of-3)

```
vorpal graph <verb> … --index /tmp/bench-lk
vorpal query '<text>' --index /tmp/bench-lk
```

| Query | wall |
|---|---|
| `graph node tcp_v4_do_rcv` | < 5 ms |
| `graph callers tcp_v4_do_rcv` | < 5 ms |
| `graph reachable tcp_v4_do_rcv --direction out --relations calls,data_flows --depth 3` | < 5 ms |
| `graph snippet tcp_v4_do_rcv` | < 5 ms |
| `graph flows tcp_v4_do_rcv` | < 5 ms |
| `graph schema` | 10 ms |
| `graph dead --kind function --limit 50 --format json` | 90 ms |
| `query 'MATCH (f)-[:calls]->(g)-[:calls]->(h {name: "tcp_v4_do_rcv"}) RETURN f.name, g.name'` | < 5 ms |
| `query 'MATCH (f:Function) WHERE f.in_degree >= 500 AND f.path CONTAINS "net/" RETURN … ORDER BY f.in_degree DESC LIMIT 5'` | 10 ms |
| `query 'MATCH (f:Function) WHERE f.name =~ "^tcp_v[46]_(rcv\|do_rcv)$" RETURN f.name'` | 20 ms |
| `query 'MATCH (f:Function) RETURN COUNT(*)'` (765,791 functions, streamed) | 10 ms |
| `query 'MATCH (f:Function) WHERE f.scc_size > 3 RETURN … LIMIT 5'` | 10 ms |
| `search "socket buffer alloc" -k 10` (tiers warm) | 30 ms |

`/usr/bin/time -p` resolves 10 ms on this machine; "< 5 ms" rows reported `0.00`.

## Suites (debug builds, `cargo test`)

- Grammar corpus: 5,787 upstream cases across 49 grammars — 5,738 pass, 60 skipped with
  written reasons, 0 fail — in **0.65 s**
  (`cargo test -p vorpal-language --test grammar_corpus`).
- Retrieval quality: 10 labelled queries, recall@5 = 1.0 and MRR = 1.0 on every channel
  (fusion, graph, name, vector), mean query 730 µs on the fixture corpus
  (`cargo test -p vorpal-index --test retrieval_eval -- --nocapture`).
- Incremental replay: one touched file → exactly one re-extract, N−1 replays
  (`cargo test -p vorpal-index --test incremental_replay`).

## Determinism

Every build above is bit-reproducible: two cold kernel builds from the same tree produce the
same generation content id and `diff -rq` over the two generation directories is clean —
the standing gate every format change re-verifies. (Product headers carry the source
mtime, so `touch`ing a file changes its product bytes and therefore the generation id —
by design: the id is the content, mtimes included.)

## Agent-task evaluation (`cargo xtask eval`)

Ten code-navigation questions over this repository, answered by one vorpal CLI invocation
against a fresh index vs a file-exploration baseline (one `grep -rn` + opening the first
five matched files — deliberately generous to the baseline). Correctness is judged against
hand-labelled expectations; the suite fails loudly if any vorpal answer misses its label.
Regenerate with `cargo build --release -p vorpal -p vorpal-index && cargo xtask eval --write`.

<!-- BEGIN GENERATED EVAL TABLE -->

| Question | vorpal calls | bytes | ms | ok | baseline calls | bytes | ms | ok |
|---|---:|---:|---:|---|---:|---:|---:|---|
| where-defined | 1 | 186 | 5 | ✓ | 2 | 5253 | 27 | ✓ |
| who-calls | 1 | 105 | 4 | ✓ | 2 | 73399 | 28 | ✓ |
| snippet | 1 | 759 | 4 | ✓ | 2 | 73233 | 27 | ✓ |
| type-users | 1 | 84 | 4 | ✓ | 4 | 91014 | 19 | ✓ |
| impact | 1 | 2993 | 4 | ✓ | 6 | 107945 | 18 | ✓ |
| data-flow | 1 | 2891 | 4 | ✓ | 3 | 119203 | 27 | ✓ |
| schema | 1 | 567 | 5 | ✓ | 6 | 52338 | 19 | ✓ |
| hubs | 1 | 259 | 6 | ✓ | 6 | 93917 | 25 | ✓ |
| reachable | 1 | 920 | 4 | ✓ | 3 | 119374 | 27 | ✓ |
| search | 1 | 431 | 13 | ✓ | 2 | 30014 | 25 | ✓ |
| **total** | **10** | **9195** | | **10/10** | **36** | **765690** | | **10/10** |

Bytes an agent must read: baseline/vorpal = **83.3×** (baseline model: one grep + opening the first 5 matched files — generous; real exploration loops grep repeatedly).
<!-- END GENERATED EVAL TABLE -->

## History (earlier passes, kept for the record)

- 2026-08-29 grammar Waves 1–2 (28 → 49 languages): the kernel corpus itself grew — vorpal
  indexes its Makefiles, Perl, SQL, TOML, INI and more (+3,413 files, +3.4k resolved refs);
  cpython gained pyproject/TOML, INI, and HTML docs whose embedded scripts extract through
  real injected parses. The 28-grammar corpus measured 7.01 s cold (72,541 files →
  2,748,638 nodes) and cpython 0.71 s (3,592 files → 143,450 nodes).
- 2026-08-29 tree-sitter runtime pass (7.85 s → 7.01 s, output bit-identical): the runtime
  is vendored (`vendor/tree-sitter`, see docs/UPSTREAM.md) and cold indexing is
  ~two-thirds parser CPU, so the parse path was profiled and optimized: a lexer ASCII fast
  path (`ts_lexer__get_lookahead`, ~7%), an ASCII fast path in the grammars' `set_contains`,
  and skipping the clean-parse error scan via tree-sitter's O(1) `has_error`.
  `target-cpu=native` measured no difference (latency/branch-bound); parser reuse across
  files (0.16 µs per `Parser::new`) and worker oversubscription (~3%) did not pay.
- 2026-08-17 saturation pass (8.79 s / 70% core utilization → 7.95 s / ~79%): parallel
  total-order evidence sort+encode, the generation artifacts written concurrently,
  per-worker stream budget 8 → 24 MiB, parallel names-index sort, rolling absorption on a
  dedicated thread, work queue 36 → 1,152 entries. File-level longest-first admission was
  evaluated and rejected with a proof sketch (it either deadlocks the byte budget or unbounds
  the absorber's holdback). The remaining gap to 100% is parse-length imbalance plus ~0.6 s
  of inherently ordered link tail; inspect with `VORPAL_PHASE_TRACE=1`.
- Wall-clock deltas below ~0.5 s are unresolvable on this hardware: user CPU for identical
  cold builds varies 101–117 s with thermal state across a session.

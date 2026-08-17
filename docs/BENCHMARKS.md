# Benchmarks — commands, datasets, hardware, and raw results

Reproducible measurements only (release builds, stated machine state). Every number below
was produced by the exact command shown, on the stated hardware and dataset commits, on
2026-07-31. Numbers are honest points, not marketing: re-run them on your hardware; the
commands are the contract. `VORPAL_NO_AUTOWARM=1` is set throughout so background warms
never blur a measurement.

## Hardware and toolchain

- Apple M5 Max, 18 cores, 128 GiB RAM, macOS 26.4.1
- rustc 1.94.1; `cargo build --release -p vorpal-index`
- Datasets: `linux` @ `1590cf032971` (72,541 indexable files), `cpython` @ `b86a41cbf63`
  (3,592 indexable files)

## Definitions

- **cold**: no index root exists (`rm -rf <out>`); everything parses.
- **warm-unchanged**: immediately re-run on an unchanged tree — the manifest fast path.
- **one-file update**: `touch` one file, re-index — stat sweep + one re-parse + full
  deterministic re-link/seal.
- **tiers warm**: `ann.bin` + `postings.bin` built and stamp/provenance-fresh. Without
  them, search takes the exact fallback (slower, identical results — pinned by test).

## Indexing

```
vorpal-index index ~/Projects/linux /tmp/bench-lk        # cold, then re-run for warm
vorpal-index index ~/Projects/cpython /tmp/bench-py
```

| Measurement | wall | user CPU | notes |
|---|---|---|---|
| linux cold | 7.95 s (min-of-3) | 113.1 s | 72,541 files → 2,748,638 nodes; 2.17 M refs resolved |
| linux warm-unchanged | 0.11 s | — | manifest fast path, no file reads |
| linux one-file update | 2.30 s | — | product replay + deterministic re-link |
| cpython cold | 0.83 s | — | 3,592 files → 143,450 nodes |

2026-08-17 saturation pass (was 8.79 s / 70% core utilization → 7.95 s / ~79%): parallel
total-order evidence sort+encode (0.47 s → 0.07 s, overlapped with the graph save), the four
generation artifacts written concurrently, per-worker stream budget 8 → 24 MiB (large
generated headers stalled admission), parallel names-index sort. The remaining gap to 100%
is ~12% distributed inside the extraction pipeline (parse-length imbalance and committer
micro-waits) plus ~0.6 s of inherently ordered link tail; the phase timeline is inspectable
with `VORPAL_PHASE_TRACE=1`.

Index size (linux): generation 2.03 GiB (includes the 805 MB vector tier and 39.7 MB
posting tier after a warm); index root 4.1 GiB total with the root-level product bank.

## Search (linux index, k=10, query "socket buffer alloc")

```
vorpal-index search /tmp/bench-lk "socket buffer alloc" 10
```

| State | wall | user CPU | path taken |
|---|---|---|---|
| no tiers | 0.29 s | 1.94 s | exact fallback (exhaustive semantic + name scan) |
| tiers warm | 0.03 s | 0.01 s | ANN beam + posting intersection |

Tier build (one-time per generation, heals in the background in daemon use): 12.53 s for
both tiers at kernel scale. Results are byte-identical with and without tiers (test-pinned),
so warm state changes latency only, never answers. The ~50× user-CPU drop on the name
channel is the posting tier (`53303e2`); the model-provenance gate (`626b182`) is why a
tier built by a different embedder version is distrusted and falls back rather than
answering from incompatible vectors.

## Suites (debug builds, `cargo test`)

- Grammar corpus: 4,279 upstream tests across 27 grammars in **0.43 s**
  (`cargo test -p vorpal-language --test grammar_corpus`).
- Retrieval quality: 10 labelled queries, fused recall@5 = 1.0, MRR = 1.0, mean query
  ~0.9 ms on the fixture corpus (`cargo test -p vorpal-index --test retrieval_eval -- --nocapture`).

## Determinism

Every build above is bit-reproducible: rebuilding an unchanged tree produces the same
generation content id (`diff -r` clean across double builds at kernel scale — the standing
gate every format change re-verifies).

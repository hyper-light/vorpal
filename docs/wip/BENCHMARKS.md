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

2026-08-30 co-change edges (`changes_with`, ADOPTION #27): the pass reads the last 2,000
non-merge commits with `git log --name-only`. Measured on the kernel: run after the stream it
cost 1,135 ms serial (over the +10% cold line), so it now runs as a child process *beside*
extraction and is joined before link — the serial cost is the join + pair count, **82 ms**
cold; the git work itself (~1 s of CPU) hides under parsing. Incremental builds never
re-run git: results cache by `HEAD` + window (`cochange.cache` at the index root), and a
cache hit costs **14 ms** (the kind-gated File-node scan), so the one-file update stays at
2.06–2.22 s. Kernel: 178 file pairs (its commits are small); cpython: 1,216 pairs, cold
0.95 s. Cold rows above were measured before the pass landed; the user-CPU column grows by
the overlapped git work.

2026-08-30 near-clone edges (`similar_to`, product format 16): every Function/Method/
Constructor with ≥ 32 tokens carries a 64-byte one-permutation MinHash sketch over its
3-token shingles (captured inside the existing reference walk — one hash per token, no
second pass; comments skipped), and link pairs them by LSH banding (16 × 4 bytes) with
sketch verification at ≥ 0.7 estimated Jaccard, 8 partners per definition, stars for
clone families above 64 members. Measured on the kernel against the pre-change binary,
interleaved: cold **+0.03 s / +0.34 s** wall on 8.5 s / 7.0 s base runs (+4% user CPU
for the token hashing); `products.pack` **595 → 641 MB (+7.7%)** for 629,887 signed
definitions; `graph.bin` +1.9% for **94,516 pairs**. The pairing itself is **0.18 s** on
its own thread (630k sketches → 6.76M candidates → 106k accepted → 94,516 kept after the
partner cap), started at the top of link so it finishes under the symbol-table build — the
one-file update measured 2.12–2.43 s against 2.17 s base (+1.5 s user CPU: sketch replay,
spill, pairing). The kernel's pairs are the v4/v6 twins one expects
(`__cookie_v4_check`/`__cookie_v6_check`, `tcp_v4_rcv`/`tcp_v6_rcv`,
`bictcp_cong_avoid`/`cubictcp_cong_avoid`); `IGMP_V1_SEEN`/`IGMP_V2_SEEN` show macro
bodies are signed too. Cold/update rows above predate the format bump.

2026-08-30 HTTP route nodes (ADOPTION #25 slice 1): framework route registrations become
`Route` nodes named `VERB /path`, each `calls` its handler — Express-family, NestJS,
Flask/FastAPI, Django `urlpatterns`, Go net/http/gin/echo/chi (Go 1.22 `"GET /x"` patterns),
axum, actix/Rocket attributes, Spring, ASP.NET, Rails/Sinatra, pinned by per-framework
fixtures. Routes live inside function/class bodies, so they ride a dedicated `nested: true`
outline pass that only runs for languages declaring such rules. Measured against the
pre-route binary, interleaved: kernel cold **7.35 → 7.33 s** (parity — C declares no nested
rules); cpython cold wall parity (0.90 / 0.93 s) at **+6% user CPU** (the Python nested
pass), one-file update parity (0.26 / 0.24 s). cpython and the kernel truthfully report
zero routes (no web code); the only ref-level delta is one extra *masked* reference — a
handler probe that resolved to nothing, producing no edge.

2026-08-30 request → route edges (`requests`, ADOPTION #25 slice 2, product format 17):
HTTP client call sites with literal URLs (`fetch("/api/users")`, `requests.get(url)`,
`axios.<verb>`, `http.Get`/`NewRequest`, reqwest) are recorded at extraction and matched
against `Route` templates at link — unique matches only, template parameters absorb
segments, cross-language (a TS `fetch("/health")` links into a Go `HandleFunc` route in
the fixtures), directional edges at confidence 95 (literal-exact) / 85 (parameterized).
Ambiguous and unmatched sites are counted on the report, never guessed. Measured against
the route-slice binary, interleaved: kernel cold 8.02 → 7.82 s, cpython cold 2.17/0.98 →
1.98/0.98 s, one-file update 0.27/0.28 s — parity throughout (+1.4% cpython user CPU);
pack +4 bytes per product (the empty section count: kernel +304 KB = +0.05%). cpython in
the wild: 1 client call site found, 0 linked, stated on the report ("this tree defines no
routes — all external"). Cross-repo linking waits on the fleet index merge (R2).

2026-08-30 channel nodes + `notifies` edges (ADOPTION #25 slice 3): event listener
registrations (`bus.on("user.created", handler)`, `queue.subscribe`, Go `Subscribe`)
become `Channel` nodes named `EVENT <topic>` that `call` their handlers (the same
nested-rule + handler machinery as routes), and emitters (`emit`, `publish`, `dispatch`)
are matched at link by exact topic — pub/sub is one-to-many by design, so an emitter
links to EVERY matching registration via `notifies` (confidence 90, fan-out capped at 16
per site and counted). Channels and routes never cross-match. Measured against the
request-slice binary, interleaved: cpython cold 2.38/0.94 → 2.24/0.96 s (+2.4% user CPU),
update parity (0.26 s); kernel cold 7.56/7.53 → 7.69/7.67 s (**+0.14 s ≈ +2% wall**, user
CPU parity; a first pair read +0.96 s under a load spike and was discarded). In the wild:
cpython gains 74 Channel nodes (d3 listeners in a vendored flamegraph HTML's embedded JS
— truthfully extracted) and reports 266 emit sites, 0 linked, all counted; the kernel has
53 emit sites and no channels, stated on the report.

2026-08-30 runtime-trace ingestion (`observed.bin`, ADOPTION #26):
`vorpal-index ingest-traces <index> <folded-stacks>` turns collapsed stacks (perf +
stackcollapse, py-spy, inferno) into observed caller→callee rows — evidence of calls the
static graph can never prove (function pointers, dynamic dispatch), kept BESIDE the graph
as an additive sidecar stamped to the node segment: a rebuild invalidates it (re-ingest)
rather than carrying renumbered ids. Frame names normalize (`+0x…`, ` [module]`,
`(file:line)` stripped) and resolve only when exactly one callable matches (qualified
frames fall back to their last segment); unknown/ambiguous frames break the chain — a
gap never fabricates a direct call — and are counted and sampled on the report. Surfaces:
`vorpal graph observed <name>` and the MCP `observed` tool, each row flagged
`in_static_graph` (false = the interesting ones). Zero indexing cost by construction —
ingestion is a separate offline command, O(stack lines); nothing rides extraction, link,
or the generation id.

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

The calls-graph community sidecar (`communities.bin`, built by the same warm right after the
search tiers; `VORPAL_PHASE_TRACE=1 vorpal-index __warm-ann /tmp/bench-lk` prints its
`communities:` stamps) costs **0.36 s wall** and a 40 MB transient at kernel scale, and is
11 MB on disk (one `u32` per node). Deterministic Louvain over the `calls` graph with exact
integer modularity gains; 69 omnipresent nodes (degree above √(2m) = 2,274 — `kfree`,
`memcpy`, `printk`, …) are held out as singletons, and the reported community is a
size-bounded cut of the dendrogram (`VORPAL_COMMUNITY_CAP`, default 512, 0 = the top Louvain
level). On the kernel that yields 80,485 clusters of ≥2 members over 686k participating
functions (sizes p50 3, p90 12, p99 113, max 5,879 — a first-level group, the floor); the
top Louvain level instead was a few 46k-member hub basins over a dust of pairs.
`tcp_v4_rcv`'s community is `tcp_v4_do_rcv`, `tcp_v4_fill_cb`, `tcp_v4_restore_cb`,
`tcp_v4_cookie_check`, `tcp_child_process`, `tcp_inbound_hash`, `tcp_inbound_md5_hash`.

**Tier answers are approximate at this scale — measured, not assumed.** The previous edition
of this document claimed tier and exact results were byte-identical (a fixture test pins
that). At kernel scale they are not: over eight queries (`socket buffer alloc`, `page fault
handler`, `tcp congestion window`, `mutex lock acquire`, `inode lookup path`, `interrupt
request register`, `dma coherent alloc`, `scheduler pick next task`) the tier's top-10
overlapped the exact top-10 in **66/80** positions, ranging from 10/10 down to 4/10
(`mutex lock acquire`). The fused ranking is deterministic — the misses are candidates the
ANN beam never surfaced (e.g. `socket_alloc` at exact rank 3), consistent with the beam-width
reduction in `a048aa0`. Recorded here as an open finding for the ANN owner; the exact path
remains the reference answer. This figure is now tracked continuously:
`cargo xtask searcheval <idx> xtask/labels/kernel.json --overlap` (see "Retrieval quality"
below).

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
| `query 'MATCH (f:Function {name: "tcp_v4_rcv"}) RETURN f.community'` | < 5 ms |
| `query 'MATCH (g:Function) WHERE g.community = 1919789 RETURN count(*)'` (2.76M-node scan) | 20 ms |
| `graph architecture` (modules, hubs, entries, 80k-community cluster pass) | 50 ms |
| `graph similar tcp_v4_rcv` (near-clones with similarity) | 10 ms |
| `search "socket buffer alloc" -k 10` (tiers warm) | 30 ms |

`/usr/bin/time -p` resolves 10 ms on this machine; "< 5 ms" rows reported `0.00`.

## Suites (debug builds, `cargo test`)

- Grammar corpus: 5,787 upstream cases across 49 grammars — 5,738 pass, 60 skipped with
  written reasons, 0 fail — in **0.65 s**
  (`cargo test -p vorpal-language --test grammar_corpus`).
- Retrieval quality: 17 labelled queries across 8 classes — exact / short-keyword /
  subset / descriptive / graph-disambiguation stay at recall@5 = 1.0 (the fusion
  invariant), paraphrase / sparse-name / conjunctive are PINNED at their honest lexical
  floors so any movement is loud — plus per-channel ablations and a double-run
  determinism gate (`cargo test -p vorpal-index --test retrieval_eval -- --nocapture`).
- Incremental replay: one touched file → exactly one re-extract, N−1 replays
  (`cargo test -p vorpal-index --test incremental_replay`).
- Multi-phrase AND: parser table (the conjunction syntax claims no ordinary query),
  hits ⊆ every phrase's pool with the exact min-of-RRF score, phrase-tagged provenance,
  lexical-support eliminators (nonsense phrase, disjoint families, empty filter), and a
  double-run determinism gate (`cargo test -p vorpal-index --test multi_phrase`).
- Engine calibration: `ann.calib` lifecycle (written by warm, stamp-gated, torn-tolerant)
  and routing neutrality — forged crossovers leave rankings bit-identical
  (`cargo test -p vorpal-index --test calibration`).

## Determinism

Every build above is bit-reproducible: two cold kernel builds from the same tree produce the
same generation content id and `diff -rq` over the two generation directories is clean —
the standing gate every format change re-verifies. (Product headers carry the source
mtime, so `touch`ing a file changes its product bytes and therefore the generation id —
by design: the id is the content, mtimes included.) With the co-change pass on (default),
the id also folds the last 2,000 commits of git history — see docs/wip/INDEX_FORMAT.md,
"Git history in the generation id"; `VORPAL_COCHANGE_COMMITS=0` restores tree-only identity.

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

## Retrieval quality (`cargo xtask searcheval`, semantic-tier Stage 0)

Graded labels (`xtask/labels/*.json`, grades 1–3; every labelled name existence-checked
against the index at the start of each run, so a renamed symbol fails loudly instead of
scoring as a miss) scored per class as NDCG@10 / MRR / recall@5, with a double-run
determinism gate and a tier-vs-exact top-10 overlap mode:

```
cargo build --release -p xtask
vorpal-index index <tree> <idx> && vorpal-index __warm-ann <idx>
target/release/xtask searcheval <idx> xtask/labels/<set>.json [--overlap]
```

2026-08-30 lexical-fusion baselines (pre-semantic-tier — the "before" every stage of the
semantic-tier plan measures against):

| set | queries | NDCG@10 | MRR | recall@5 | tier-vs-exact top-10 |
|---|---:|---:|---:|---:|---|
| vorpal self-index | 10 | 0.559 | 0.544 | 0.550 | — |
| cpython @ `b86a41cbf63` | 6 | 0.141 | 0.222 | 0.250 | 59/60 positions, 60/60 set |
| linux @ `1590cf032971` | 8 | 0.208 | 0.250 | 0.167 | 47/80 positions, 75/80 set |

Findings these baselines pin:

- **The fixture's short-keyword supremacy does not survive scale.** The 17-query fixture
  holds recall@5 = 1.0 on every fully-retrievable class, but the same class collapses at
  kernel scale (NDCG@10 **0.103** over 7 short-keyword queries): "mutex lock acquire"
  does not surface `mutex_lock` in the top 25 — thousands of definitions carry those name
  tokens, and the token-subset tier plus in-degree fusion drown the definitive answers.
  Stage 4 (BM25 postings channel: TF + length normalization) targets exactly this; these
  rows are its before.
- Self-index paraphrase and conjunctive floors are 0.0 by construction (zero vocabulary
  overlap) — the Stage 1 / Stage AND before.
- Tier-vs-exact at kernel scale: 47/80 positional agreement but **75/80 set agreement** —
  the beam surfaces nearly every exact-path candidate, and the positional disagreement is
  mostly pool-composition reordering (a candidate's per-channel rank shifts with pool
  membership before RRF), not lost answers; only 5/80 were true beam misses. cpython:
  59/60 positions, 60/60 set. Exact-path mean wall: linux 365 ms, cpython 75 ms — the
  fallback cost a search pays when tiers are cold.

## Multi-phrase AND + semantic engine routing (semantic-tier Stage AND)

`"phrase one" AND "phrase two"` (two+ double-quoted phrases joined by literal uppercase
AND — anything else flows byte-identically through the single-phrase path) runs every
phrase through the full three-channel pass and intersects **lexical supports**: a phrase
matches a row iff they share ≥1 real token over the exact embedded surface (name,
signature, file basename). Vector-space sign cannot define a match — measured twice:
hashed-bucket collisions hand unrelated rows positive dot products (universal tokens
like `fn`/`u32` collide a nonsense phrase into near-global "positive support").
Survivors score min-of-RRF (ties: sum, then id); two depth rungs, both computed —
shallow = the single-phrase rerank pool, deep = the node count (structurally exhausts
every channel, so an empty intersection means the FULL supports are disjoint); an empty
result names its eliminator with per-phrase support sizes and the rung depth.

### Semantic engine cost sweep (beam vs flat exact scan)

```
cargo run --release -p vorpal-index --features bench-internals --example sweep_semantic -- <idx>
```

2026-08-30, medians over 8 fixed queries × 3 reps, interleaved; load < 3:

| take | linux beam ms | linux scan ms | cpython beam ms | cpython scan ms |
|---:|---:|---:|---:|---:|
| 400 | 1.02 | 90.02 | 0.91 | 7.87 |
| 800 | 2.15 | 91.53 | 2.05 | 8.79 |
| 1600 | 4.80 | 91.73 | 5.07 | 10.63 |
| 3200 | 13.15 | 101.64 | 13.69 | 10.91 |
| 6400 | 41.57 | 130.12 | 44.53 | 10.86 |
| 12800 | 150.76 | 208.56 | 153.85 | 9.51 |
| 25600 | 563.99 | 361.07 | 543.71 | 9.43 |
| 51200 | 2156.06 | 420.01 | 1712.74 | 9.41 |

(linux n = 2,354,838 semantic rows; cpython n = 133,098. Beam cost is n-independent —
the two curves overlay; scan cost is ~linear in n. Observed crossovers: linux ~16–20k,
cpython ~2.8k.)

**No number from this table ships in the product.** Two earlier attempts were rejected
as magic: a guessed cutover (4096), then sweep-fitted coefficients (machine-specific
milliseconds frozen into source). The shipped design:

- **Structural floor (proven property, no constants):** `take ≥ node_count` → exhaustive
  scan — an ANN beam's reach is a graph traversal with no completeness guarantee over
  the full population; the scan is exact by construction (and faster there, per the
  table).
- **Mid-range routing is LEARNED at warm time from the ingested index on the running
  machine** and persisted as `ann.calib` (32 bytes: magic VCAL, version, node-segment
  stamp, crossover, xxh3 self-checksum; machine-local like every warm sidecar, excluded
  from byte-identity gates because it is a measurement; absent/stale/torn → the
  structural floor). Protocol: 3 seeded probe vectors, median of 3 reps; scan reference
  at take = 1 (the n-driven floor — real scans cost at least this, so the error
  direction always prefers the exact engine); beam probes on the ×2 ladder stop at the
  first width that loses. Learned values on this machine: **linux 16,384; cpython
  2,048** — independently matching the offline sweep's observed crossovers. Calibration
  cost, paid once per warm: linux 5.77 s (heal-only re-warm, includes the probes),
  cpython 0.15 s. Routing is a latency decision only — the exact scan is never worse —
  pinned by `crates/index/tests/calibration.rs` (forged crossovers leave rankings
  bit-identical).

#### 2026-08-30 second pass: explicit SIMD kernels + heap-bounded selection

Two exact-preserving engine changes after the table above was recorded (same command,
same tree, load < 3):

- The crate's hot float reductions became explicit SIMD with ONE fixed rounding tree on
  every architecture (`crates/ann/src/kernels.rs`: stable `core::arch` — NEON on
  aarch64, AVX2 behind runtime detection on x86_64, the scalar 8-lane loop as the
  executable specification and fallback; multiply-then-add only, never FMA, so every
  path is bit-identical — pinned by parity tests across sizes). `l2_sq` is now also the
  ONE public distance function the scan and the rerank both call, making their
  bit-agreement (which the rerank-skip relies on) structural. `std::simd` /
  `portable_simd` remains nightly-only; the workspace pins stable 1.98.0.
- The scan's bounded top-set (a sorted-insert Vec) degraded quadratically as `take`
  approached the per-chunk length — measured as a 3× hump at take = 25,600 (317 ms vs
  106 ms one row later). Replaced by a max-heap on the same `(dist, id)` total order:
  O(log take) per row, no regime anywhere, kept set provably identical (the unique
  top-take under a total order).

| take | linux beam ms | linux scan ms |
|---:|---:|---:|
| 400 | 1.06 | 52.83 |
| 800 | 2.15 | 53.17 |
| 1600 | 4.78 | 55.38 |
| 3200 | 12.90 | 58.13 |
| 6400 | 41.23 | 65.23 |
| 12800 | 150.21 | 79.59 |
| 25600 | 560.87 | 104.43 |
| 51200 | 2101.40 | 105.25 |

Scan floor 90 → 53 ms (−41%), the 25.6k hump 317 → 104 ms, curve now monotone. The
warm-time calibration RE-LEARNED the crossover on its own — 16,384 → 8,192, because the
scan got cheaper — which is precisely why the crossover is measured on the running
machine instead of shipped. Conjunctions: 2-phrase 0.66 s, 3-phrase 0.94 s, garbage
0.63 s (from 0.77 / 1.08 / 0.72); mid-range single k=2000 rides the re-learned scan
route at 0.40 s; single k=10 unchanged (0.03 s). Distance BITS changed (a different
fixed tree), yet every pinned fixture baseline held — no rank flipped at fixture scale.

### Conjunction latency and behavior (linux index, warm + calibrated, best-of-3)

```
vorpal search '"socket buffer" AND "alloc"' -k 10 --index <idx>
```

| Query | wall | note |
|---|---|---|
| `socket buffer alloc` (single, k=10) | 0.03 s | unchanged |
| `socket buffer alloc` (single, k=2000) | 0.30 s | mid-range → calibrated exact-scan route |
| `"socket buffer" AND "alloc"` | 0.77 s | deep rung (shallow supports disjoint at pool 50) |
| `"socket buffer" AND "alloc" AND "packet"` | 1.08 s | eliminator: pools 16500, 13483, 4860 — no kernel row carries all three vocabularies (`skb` ≠ "socket buffer" lexically; that equivalence is Stage 1's job) |
| `"socket buffer" AND "zzyzxqv nonexistent"` | 0.72 s | eliminator: pools 16500, 11 (11 rows genuinely contain "nonexistent") |
| `"mutex lock" AND "interruptible"` | — | `mutex_lock_interruptible` at rank 0 |

The deep rung's cost fell 4.0 s → 0.77 s (2-phrase) from two exact-preserving fixes,
both output-identical by construction: the scan's per-chunk top-set skips its sorted
insert when the cap cannot bind (`take ≥ chunk len` — O(len²) memmove → linear;
crates/ann/src/scan.rs), and exhaustive candidates skip the rerank (the scan already
returns the rerank's exact `(dist, id)` total order).

## Learned semantic tier (semantic-tier Stage 1): training cost + engine integration

The corpus-trained static-embedding tier (`--semantic-tier learned`, vorpalconfig
`semanticTier`, MCP `semantic_tier`) trains at warm time over the generation's node
surfaces and persists `ann.model.bin` (VMD v2, xxh3-checksummed; `ann.model.json`
carries tier + weights hash; flips are staleness; incoherent artifacts route to the
lexical default — never mixed embedders, never a silent zero).

```
vorpal-index index <tree> <idx> --semantic-tier learned && vorpal-index __warm-ann <idx>
VORPAL_PHASE_TRACE=1 vorpal-index __warm-ann <idx>   # per-phase attribution
```

2026-08-31, linux @ `1590cf032971` (2.76M nodes) and cpython @ `b86a41cbf63` (150K
nodes), release, quiet machine. Warm totals (lexical warm on the same day: linux
13.45 s, cpython 0.50 s):

| corpus | learned warm | train | ann build | calib | model |
|---|---:|---:|---:|---:|---|
| linux | 78.4 s | 65.6 s | 11.0 s | ~1.4 s | dim 249 (PIP), 462 MB, exact gram table |
| cpython | 6.0 s | ~5.4 s | 0.4 s | ~0.1 s | dim 238 (PIP), 69 MB |

Training went 52.6 s → 11.5 s (cpython) and 200.7 s → 65.6 s (kernel) across four
measured, output-preserving fixes — each found by phase stamps + `sample(1)`
attribution, each pinned by the existing bit-identity oracles:

| kernel train phase | before | after | fix |
|---|---:|---:|---|
| factorization (eigen) | dominated 52 s cpython trains | 12.2 s | right-looking MGS as contiguous row sweeps + one bounded `block_gram` for QᵀMQ (the strided per-pair form spent 308 s SYS in rayon yield-spin); rank-revealing via UNCONDITIONAL second sweep — "twice is enough" (Giraud–Langou–Rozložník 2005) after the single-pass basis measurably lost orthogonality (5.8e-3) on graded survivors |
| uSIF/sentence passes | 75.7 s | 5.5 s | word vectors stored COMPOSED (VMD v2: factor row + Σ gram rows precomputed — same sum, bit-identical vectors); sentence-PC Gram batched 65,536 docs and accumulated by the deterministic `block_gram` |
| cooc merge | 55.9 s | 0.02 s | spill buffer = the one-merge-pass balance M ≥ √(N·page/pair) (Knuth §5.4) — the plain √N missed the page factor and forced two rewrite levels over ~4,200 runs |
| σ + CSR streaming | 176 s (after the balance fix left ~700 live runs) | 29.4 s | k-way run merge via a min-heap of (pair, cursor) heads — O(log k)/record instead of two O(k) scans; integer sums, bytes unchanged |

A second optimization round (2026-08-31, same protocol) took the kernel warm 84.0 s →
62.4 s WITH retrofit (cpython ~6.2 s → 5.1 s): the serial event feed became a parallel
one and the σ/CSR consumers overlapped — output bits pinned unchanged by the
serial-vs-parallel spill oracle and the searcheval reproduction recorded with the
retrofit table below.

| kernel train phase | before | after | fix |
|---|---:|---:|---|
| cooc event feed | 16.4 s (serial doc walk) | 5.5 s (1.4 materialize + 4.1 generate/spill) | token-id docs materialized ONCE to sequential scratch, then `count_ranges`: fixed doc ranges generate pre-aggregated runs in parallel through a small writer pool — bit-equal by aggregation invariance (`parallel_ranges_match_the_serial_feed_bitwise`); per-SPLIT worker state (marginals pre-sized by `id_bound`, buffers reused) after `sample(1)` caught per-range exact-growth marginal resizes as the dominant cost |
| run compaction | 26 s when triggered | 0 s (never fires on the balanced path) | range count capped at `merge_fan_in` — the Knuth balance puts ceil(events/buffer) AT the fan-in, so one run per range per counter stays single-level mergeable; the compaction pass (parallel groups, retained for degenerate shapes) measured NET-NEGATIVE: ~26 s of read+rewrite to save ~17 s of consumer heap depth |
| σ + CSR streaming | 29.4 s sequential | 20.3 s joint | `rayon::join`: σ (half-counter streams) hides under the CSR build (full-counter streams) — disjoint data, deterministic reductions, unchanged bytes |

Remaining leads (recorded, not taken this round): the four PPMI stream passes recompute
ln() per record each pass — caching the CSR size pass's filtered output for the fill
pass would kill one ~730-cursor merge plus one PPMI recompute (~5 s); the ~5.3 s
train-done → build-start gap (model persist); eigen 12.5 s.

Peak training RSS 2.26 GB (CSR + eigen + composed tables at the kernel's 464K matrix
rows); double-warm byte-identity of `ann.bin` + `ann.model.bin` is a standing test
(`crates/index/tests/learned_tier.rs`), and the starved-vs-roomy spill oracle keeps the
external pipeline bit-equal to the in-RAM reference at every buffer shape.

**Query/calibration engine rule (measured, then locked):** under the learned tier an
exhaustive fetch NEVER re-embeds the population — the first kernel warm spent 191 s of
404.7 s inside calibration's scan reference re-embedding 2.35M rows through uSIF
(~50 µs/row). Exhaustive fetches and the calibration scan now walk the persisted i8
codes (`AnnIndex::scan_codes` — the beam's own distance, complete over the admitted
population, deterministic at any thread count), full precision returning for bounded
pools and winners; a learned handle without its ann tier serves lexical entirely. The
exact-only reference seam (bench-internals) still re-embeds: it is the truth oracle.

### Retrieval quality: lexical vs learned, same day, same commits

`target/release/xtask searcheval <idx> xtask/labels/<set>.json` on both tiers of the
SAME indexes (flip via `--semantic-tier`, re-warm, re-run); the lexical rows reproduce
the Stage-0 baselines exactly, pinning corpus-state identity. NDCG@10 / MRR / recall@5:

| set · class | queries | lexical | learned |
|---|---:|---|---|
| linux · short-keyword | 7 | 0.103 / 0.143 / 0.048 | **0.170 / 0.227 / 0.095** |
| linux · descriptive | 1 | 0.947 / 1.000 / 1.000 | 0.947 / 1.000 / 1.000 |
| linux · **all** | 8 | 0.208 / 0.250 / 0.167 | **0.267 / 0.324 / 0.208** |
| cpython · descriptive | 4 | 0.042 / 0.083 / 0.125 | **0.333 / 0.250 / 0.500** |
| cpython · short-keyword | 2 | 0.338 / 0.500 / 0.500 | 0.293 / 0.500 / 0.250 |
| cpython · **all** | 6 | 0.141 / 0.222 / 0.250 | **0.320 / 0.333 / 0.417** |

Stage-1 gates: the paraphrase-shaped split is STRICTLY better (cpython descriptive
NDCG 0.042 → 0.333 — "append item to list" and "dictionary insert entry" now rank 3/2;
these scored ~0 lexically by construction). Short-keyword supremacy holds: the kernel
class — 7 queries at the scale where Stage 0 measured the collapse — improves 0.103 →
0.170 ("dma coherent alloc" → rank 0). Honest exception: cpython's 2-query
short-keyword class dips 0.338 → 0.293 NDCG — "garbage collect run" scores 0 under
BOTH tiers, and "import find spec" slips one label from top-5 to rank 7; a marginal
rank shift, not a collapse, and the 9-query short-keyword aggregate across both sets
improves. Query latency (k=25, tier path): linux mean 2.05 ms lexical → 4.06 ms
learned; cpython 0.75 → 2.83 ms — the learned query embed + 249-dim rerank, well
inside budget.

### Conjunction support under the learned tier: measured, unchanged

Stage AND vetoed vector-space sign as a conjunction MATCH criterion twice in the
hashed lexical space (collisions made nonsense phrases near-globally "positive").
Third measurement, now in the TRAINED space over the kernel's 2,354,838 rows
(`cargo run --release -p vorpal-index --features bench-internals --example
sweep_semantic -- <idx> --positivity <phrase>…`, dist² < 2 ⇔ cos > 0 on the
persisted codes):

| phrase | positive rows | lexical support |
|---|---:|---:|
| "socket buffer" | 1,101,592 (46.8%) | 16,733 (0.71%) |
| "mutex lock" | 1,218,818 (51.8%) | 20,755 (0.88%) |
| "packet" | 1,160,701 (49.3%) | 4,820 (0.21%) |
| "zzyzxqv nonexistent" | 1,310,255 (55.6%) | 11 |
| "qqq zzz vvv" | 1,335,101 (56.7%) | 4 |

Nonsense phrases are MORE "positive" than real ones — OOV gram composition lands
near the corpus's central direction, and a 249-dim unit space keeps a broadly
positive hemisphere even after ABTT + sentence-PC removal. Verdict: positivity can
never gate a conjunction in ANY tier; lexical token-overlap support stays the match
criterion (embedder-independent by design, now by measurement three times). The
learned tier changes conjunction RANKING only — its deep rung now rides the code
walk: `"socket buffer" AND "alloc" AND "packet"` eliminates honestly (pools
15,767 / 13,523 / 4,942 at depth 2,763,928) in 0.54 s, from 1.08 s lexical;
`"mutex lock" AND "interruptible"` keeps `mutex_lock_interruptible` at rank 0.

### Zero-copy model open (VMD v3): the query side never materializes the model

`ann.model.bin` moved to the v3 layout (`LEARNED_MODEL_VERSION` 3): fixed 56-byte
header, 8-aligned numeric sections, u64 prefix-offset + u32 sorted-permutation tables,
raw term blobs — every boundary header-derivable. `Searcher::open` now maps the file
(`ModelView`: full-checksum verify, checked section arithmetic, typed refusal of any
malformed byte) and looks tokens up by in-place binary search; only ABTT/sentence/uSIF
materialize (~KB). The owned loader remains the training/test form, and BOTH backings
run the one generic pipeline (`TokenLexicon`), pinned bit-identical by test.

`sweep_semantic -- <model> --model-open` (median of 3, page-cache warm):

| model | owned load | mapped view |
|---|---:|---:|
| linux, 467 MB (dim 249) | 107.1 ms | **32.2 ms** (≈ the xxh3 pass) |
| cpython, 69 MB (dim 238) | 17.3 ms | **4.7 ms** |

One-shot CLI searches on the learned tier, open included (`VORPAL_NO_AUTOWARM=1`,
best-of-3): linux `socket buffer alloc` k=10 **0.06 s** (top hit `socket_alloc`, the
learned signature), k=2000 mid-range 0.08 s; cpython 0.01–0.02 s. Beyond wall time,
the mapped open removes the per-open allocation of the full table set (467 MB of rows
plus term/frequency hash maps) — the daemon-fleet and one-shot CLI cost the plan
flagged.

## Relation-aware retrofit (semantic-tier Stage 2, identity form)

The learned tier's rows refine over the knowledge graph before the tier builds:
Faruqui et al. 2015's convex objective verbatim (α = 1, cited), solved by JACOBI
sweeps (rows read only X_t — thread-count invariance is structural) with descent
PROVEN for our symmetric weights (2D−A = 2(I+Deg+W), a signless-Laplacian form, so a
measured Ψ increase is a typed defect, and the auto-disable seam states it in
provenance). Termination is ΔΨ ≤ Ψ₀·ε₃₂ — sweep count is an output. Every edge
weight is structural or learned from the corpus: grade gate (structural confidence-0
edges + grades ≥ Constrained; Heuristic/Unresolved are the measured-harmful noise),
√(2m) hub hold-out (the communities null-model scale), relation weight = mean anchor
cosine per base edge type clamped at 0, the resolver's own confidence/100, and
symmetric 1/√(dᵢ·dⱼ) normalization (normalized-Laplacian form — symmetry is what the
descent proof requires; the paper's 1/deg(i) is a Gauss–Seidel prescription). The
three n×dim working regions live on `ScratchMmap` files in `<gen>/retrofit.scratch/`
(statvfs precheck → typed insufficient-space error; swept on entry, deleted on
success), so anonymous RSS stays bounded.

2026-08-31, phase-stamped (`VORPAL_PHASE_TRACE=1 vorpal-index __warm-ann <idx>`):

| corpus | edges kept | sweeps | Ψ descent | retrofit wall | peak resident |
|---|---:|---:|---:|---:|---:|
| linux (2.35M rows) | 8,889,742 | 7 | 2.654e5 → 1.618e5 (−39%) | **5.2 s** (budget ≤10 s) | 600 MB (gate <1 GB) |
| cpython | 766,600 | 6 | 1.636e4 → 1.119e4 (−32%) | 0.7 s | 96 MB |

Retrieval A/B against the plain learned tier (same day, same corpora commits;
`xtask searcheval`, NDCG@10 / MRR / recall@5):

| set · class | learned | learned + retrofit |
|---|---|---|
| linux · short-keyword (protected) | 0.170 / 0.227 / 0.095 | **0.206 / 0.298 / 0.095** |
| linux · descriptive | 0.947 / 1.000 / 1.000 | 0.947 / 1.000 / 1.000 |
| linux · **all** | 0.267 / 0.324 / 0.208 | **0.298 / 0.386 / 0.208** |
| cpython · short-keyword (protected) | 0.293 / 0.500 / 0.250 | 0.293 / 0.500 / 0.250 |
| cpython · descriptive | 0.333 / 0.250 / 0.500 | 0.316 / 0.229 / 0.500 |
| cpython · **all** | 0.320 / 0.333 / 0.417 | 0.308 / 0.319 / 0.417 |

The plan's regression rule (short-keyword / exact-name may not degrade, else the
retrofit disables itself for that corpus): NO corpus trips it — the kernel's
protected class IMPROVES (+21% NDCG, +31% MRR; retrofitted-learned now +43% NDCG over
the lexical baseline overall) and cpython's is bit-for-bit unchanged. cpython pays a
small descriptive NDCG dip (0.333 → 0.316 over 4 queries, recall@5 held) — recorded,
unprotected, and the class remains ~7.5× the lexical floor (0.042). Query latency
unchanged (4.09 ms / 2.91 ms mean at k=25). Total kernel learned warm with retrofit:
84.0 s (78.4 s without); the second optimization round (parallel event feed + σ/CSR
overlap, table in the training section) brings it to 62.4 s with retrofit. Double-warm
byte-identity holds with retrofit in the loop (fixture-pinned in `learned_tier.rs`;
kernel run recorded alongside this table).

### Penalty-form A/B: identity vs per-relation diagonal maps

The plan's second form (functional retrofitting, Lengerich et al. 2018) ships
restricted to DIAGONAL per-relation maps — dense d×d maps cost d² per edge per sweep
(~5.5×10¹¹ flops at kernel scale, two orders past the stage budget) while diagonal
keeps the penalty component-decoupled: the same Jacobi skeleton, the same ε₃₂
termination, and the same signless descent proof for ANY sign of scale (per edge
w(a·xᵢ+xⱼ)² ≥ 0). The fit is closed-form 1-D least squares per relation per
dimension over the relation's directed pairs — NO ridge constant: dimensions below
the relation's max-denominator × ε₃₂ floor keep a = 1. Both forms are selected by
one code-level constant (`RETROFIT_FORM`), never a runtime knob, and the persisted
record's new `retrofit` field makes a form change retrain old tiers (with a stated
"retrofit disabled" outcome accepted as fresh so a persistent disable never
rebuild-loops).

Measured 2026-08-31 (10 relations fitted maps on both corpora; kernel functional
retrofit 3.3 s / 6 sweeps / Ψ 2.082e5 → 1.545e5):

| set · class | identity | functional (diagonal) |
|---|---|---|
| linux · short-keyword | **0.206 / 0.298 / 0.095** | 0.170 / 0.227 / 0.095 |
| linux · **all** | **0.298 / 0.386 / 0.208** | 0.267 / 0.324 / 0.208 |
| cpython · **all** | 0.308 / 0.319 / 0.417 | 0.308 / 0.319 / 0.417 |

**Identity wins and is pinned.** The functional kernel numbers land exactly on the
UNRETROFITTED tier's: fitting a_r per relation absorbs the systematic component of
each edge (its Ψ₀ is already 22% below identity's before any sweep), which removes
precisely the neighbor pull that produced identity's gains — a coherent mechanism,
now measured rather than argued. The functional implementation remains in the tree
as the plan requires: implemented, oracle-tested (exact-map recovery, hand-solved
directed fixture, a≡1 equivalence), and measured.

## Stage 3 (int8 + rescore): closed by delta analysis — the shipped tier already is one

Stage 3's spec predates the measured ANN decisions this campaign inherited; mapping
each element to the shipped design (2026-08-31):

| Stage-3 element | disposition |
|---|---|
| int8 storage, ~4× size | SHIPPED since ANN v5: per-row-scaled i8 codes, exact integer dots, distances pure functions of the codes |
| oversampled beam + float rescore | SHIPPED: the beam overfetches and every pool re-scores at full precision (§10 — approximation picks pools, never final order) |
| float originals retained | SUPERSEDED by a stronger form: originals re-derive from the mapped model (467 MB) instead of a 2.3 GB float matrix — the lexical tier's originals are the hash itself |
| default flips only past the recall gate | int8 IS the measured default: recall 0.9937 (ann-frontier, closed do-not-reopen), and on TODAY'S learned+retrofitted tiers tier-vs-exact top-10 set agreement is 77/80 (linux) and 58/60 (cpython) — ≥ the 75/80 the lexical-era pure-quantization run recorded, even though this measurement also folds in retrofit displacement (the exact side re-embeds anchor-space) |
| per-dim 0.99-quantile calibration | NOT ADOPTED: it targets ≤1% recall loss, and the shipped per-row max-abs scheme already measures 0.63% loss at kernel scale — switching would reopen a closed, measured design for no demonstrated headroom |
| quantize∘dequantize / torn-bytes / format checks | standing: ann.bin round-trip + header/version gates + bit-identical build tests |

No new machinery ships for Stage 3; the recall evidence above is its record. Exact
path mean per overlap query: linux 2.82 s, cpython 135 ms (the mapped-model re-embed
scan — the reference oracle's price, never the query path's).

## Stage 4 (BM25 postings channel): infrastructure shipped, channel measured OFF

Postings moved to **v2** (`VPST` version 2): saturating u8 term frequency per
posting, a dense per-node doc-length section, doc count + avgdl in the header —
exactly what BM25's length normalization needs, +30% postings size (linux 39.95 →
51.89 MB, cpython 2.10 → 2.67 MB). v1 files read stale through the version gate and
rebuilt from plain warms (linux 2.1 s). The scorer is exact Okapi BM25 (k1 = 1.2,
b = 0.75 — Robertson's values, Lucene's defaults; IDF in Lucene's nonnegative form),
with two bit-identical paths — the persisted walk (`bm25_ranked`, union semantics,
sorted-token sums, rank by score desc then id) and a one-pass exhaustive twin —
pinned by a hand-computed golden (a fixture at dl = avgdl makes the term component
exactly 1, so scores are pure IDF sums; ln 2 asserted against the named constant)
and a tier-parity + thread-invariance oracle. The search filter pre-applies inside
both paths (an admit-nothing filter must empty every channel — the multi-phrase
eliminator test caught the first wiring ignoring it), while df/IDF stay collection
statistics.

**The fourth-list eval gate failed at kernel scale, twice, and the channel ships
OFF** (`BM25_CHANNEL = false`, pinned like the retrofit form). Same day, same
corpora, `xtask searcheval` (NDCG@10 / MRR / recall@5):

| set · class | learned+retrofit (baseline) | + bm25, plain | + bm25, ≥2-token floor |
|---|---|---|---|
| linux · short-keyword | **0.206 / 0.298 / 0.095** | 0.109 / 0.156 / 0.048 | 0.137 / 0.157 / 0.048 |
| linux · descriptive | **0.947** / 1.000 / 1.000 | 0.790 / 1.000 / 1.000 | 0.790 / 1.000 / 1.000 |
| linux · **all** | **0.298 / 0.386 / 0.208** | 0.194 / 0.261 / 0.167 | 0.219 / 0.263 / 0.167 |
| cpython · short-keyword | 0.293 / 0.500 / 0.250 | 0.500 / 0.500 / 0.500 | 0.500 / 0.500 / 0.500 |
| cpython · **all** | 0.308 / 0.319 / 0.417 | 0.315 / 0.354 / 0.333 | **0.392 / 0.403 / 0.500** |

Mechanism, not mystery: RRF is scale-free — a rank list hands its top 1/(60+r)
fusion mass however flat its scores — and at kernel scale BM25's top is literal-token
pollution, because the true answers live in subwords exact-token matching cannot see
(`sock_alloc` tokenizes to `sock`, not `socket`). The ≥2-distinct-token match floor
(the smallest non-trivial partial — structural, replacing a tuned depth cap) removed
the 1-of-n class and improved both corpora, but the kernel gate (no split regresses;
short-keyword ≥ baseline) still fails: 2-of-3 literal matches at kernel df remain
wrong evidence no fusion weight short of zero fixes. cpython's clean gain
(all-classes 0.308 → 0.392, short-keyword 0.293 → 0.500 — literal tokens align
there) is the recorded motivation for a future per-corpus, warm-time-gated enable;
the machinery ships tested and deterministic for that day. Stage 0's original
hypothesis is also revisited by measurement: the kernel short-keyword collapse BM25
was drafted to fix (lexical 0.103) was already fixed by the learned+retrofit tier
(0.206) through subword generalization — the road BM25's exact tokens cannot take.

**Freshness law hardened by an incident**: the v3 layout initially landed WITHOUT its
version bump, so v2-layout files passed the cheap prefix gate (magic+version) yet
misparsed past the header — warms no-op'd "fresh" while queries fell back to lexical.
Degradation was safe by construction (typed errors → lexical answers; a detached
autowarm even self-healed the kernel index in the background), but the wedge class is
real, so the gate is now structural: **the build-side freshness check IS the
query-side open** — `ann_is_fresh` (learned) runs `ModelView::open` and compares the
sealed checksum, ~32 ms per kernel-scale freshness check. Prefix gates and readers can
drift; one shared criterion cannot. Regression-proved: v2-layout files under the fixed
binary read stale and retrain (cpython 7.0 s, linux 79.9 s, files re-stamp v3).

### Per-corpus BM25 warm-time gate (directive 4): machinery shipped, gate conservative

The compile-time pin (`BM25_CHANNEL = false`) is replaced by a PER-CORPUS persisted
verdict: the tier record gains `bm25` (consulted) + `bm25_gate` (evidence, deliberately
unread), written by a warm-time gate that runs once per generation after the stamp
commits and HEALS like calibration (pre-gate records fill in on the next warm; crash
between stamp and gate → verdict absent → healed). `Searcher::open`/`open_exact` read
the verdict; all three fusion sites (single-phrase, multi-phrase, gate probes) dispatch
on it. Double-warm byte-identity extends over the gated record; determinism + heal are
fixture-pinned (`learned_tier.rs::bm25_gate_verdict_is_deterministic_and_heals`).

The gate itself is label-free known-item self-probing: nodes sampled in
`xxh3(id, seed = node-segment stamp)` order (content-derived, deterministic), eligible
= non-Import with ≥ 3 distinct name tokens; query = 2–3 leading name tokens (strict
subset → the token-subset regime where channels disagree); metric = the probed node's
reciprocal rank in the fused top 10, PAIRED on/off from one channel computation;
enable iff mean strictly improves AND wins − losses > 1.96·√(wins+losses) (two-sided
95% sign test, cited bound). Verdict + evidence go into the record verbatim.

**Measured-and-rejected along the way** (both bench corpora, records stripped and
re-healed per run): (a) a SIGNATURE-token probe family (2–3 signature tokens absent
from the name, meant to reach the descriptive regime) — signature tokens are shared by
thousands of nodes, the probed node never reaches the fused top 10, paired means
0.0000/0.0000 (kernel) and 0.0000/0.0016 (cpython): no signal; (b) probe-count as a
power fix — the sweep (production path, `bench-internals` env seam only):

| probes | kernel wins:losses (mean on/off) | cpython wins:losses (mean on/off) | verdicts |
|---:|---|---|---|
| 64 | 0:6 (0.4795/0.4954) | 3:1 (0.4595/0.4453) | off / off |
| 128 | 5:8 (0.4374/0.4449) | 7:4 (0.4534/0.4387) | off / off |
| 256 | 15:16 (0.4345/0.4402) | 15:14 (0.4675/0.4612) | off / off |
| 512 | 32:27 (0.4453/0.4444) | 33:21 (0.4677/0.4615) | off / off |

The signal DILUTES with count (kernel drifts from 0:6 demotion to a wins-excess at
512; cpython never clears the bound, 33−21 = 12 vs ~14.4 needed), and no rule
separates kernel 1.19:1 from cpython 1.57:1 at n = 512 without inventing a tuned
threshold. Conclusion, recorded honestly: label-free known-item probes measure the
subset-reordering mechanism but cannot reproduce the graded cpython descriptive win —
that class lives in NL-intent phrasing self-probes cannot synthesize. **The gate ships
as the conservative enabler** (verdicts identical across the whole sweep; the largest
count, 512, is pinned — ~2 s once per generation at kernel scale): it fires only on
strong probe-visible evidence, correctly keeps the kernel off, and currently enables
nowhere. cpython's graded enable (all 0.308 → 0.392) therefore remains reachable only
by labeled evidence — a per-corpus manual override (selection-file-shaped, beside
`semantic.tier`) is the natural follow-up if wanted. The same comparator + verdict +
heal pattern is the designed substrate for a retrofit-quality auto-disable
(retrofitted vs unretrofitted rows); unimplemented — it needs a second ANN build per
warm, a cost question deferred with the design.

## Stage 5 (NUDGE-style constrained step): measured-and-rejected

Implemented in full and swept, per the plan's conditional license: one constrained
direct step after the Stage-2 solve — per row, out = ‖x₂‖ · normalize(x₂ + γ·ĝ)
with ĝ the grade-weighted neighbor direction over the retrofit's OWN edge CSR
(NUDGE-N's bounded step + non-degeneracy constraint, adapted to this crate's exact-L2
row space as norm preservation), γ = scale × the retrofit's median displacement
(content-derived — kernel median 0.0882, all 2.35M edged rows moved). Norm-ball,
norm-preservation, γ=0-identity, edgeless-copy, and bitwise-determinism oracles ship
in `vorpal_ann::retrofit`. Sweep (bench-internals `VORPAL_NUDGE_SCALE` seam,
full retrain per point, `xtask searcheval`, NDCG@10 / MRR / recall@5):

| scale (γ) | kernel short-kw | kernel all | cpython descriptive | cpython all |
|---|---|---|---|---|
| Stage-2 baseline | **0.206** / 0.298 / 0.095 | **0.298** / 0.386 / 0.208 | **0.316** / 0.229 / 0.500 | **0.308** / 0.319 / 0.417 |
| 0.5 (0.0441) | 0.206 / 0.299 / 0.095 | 0.298 / 0.386 / 0.208 | 0.316 / 0.229 / 0.500 | 0.308 / 0.319 / 0.417 |
| 1 (0.0882) | 0.170 / 0.227 / 0.095 | 0.267 / 0.324 / 0.208 | 0.246 / 0.188 / 0.500 | 0.261 / 0.292 / 0.417 |
| 2 (0.1764) | 0.170 / 0.227 / 0.095 | 0.267 / 0.324 / 0.208 | 0.246 / 0.188 / 0.500 | 0.261 / 0.292 / 0.417 |

The gate ("beats Stage 2 on the target splits, regresses nothing") fails at every
scale: 0.5 is a no-op at eval granularity (one MRR digit +0.001), 1 regresses BOTH
corpora on BOTH target splits, and 2 saturates to the same ranks as 1. The mechanism
is the honest headline: the Stage-2 retrofit CONVERGES (Ψ-descent to the ε₃₂ floor)
over exactly these grade-weighted edges, so the weighted-neighbor direction carries
no signal the optimum has not already priced in — a further step toward it is a step
off the optimum. NUDGE's published wins come from held-out QUERY-relevance labels,
an independent signal that graph positives are not. Rejected and pinned off
(`NUDGE_STAGE = None`); the algebra + oracles ship as a measured seam (the
functional-form precedent) for any future INDEPENDENT positive source (e.g. labeled
or interaction data).

## Stage 6 (vendored encoder): owned inference core, proven against references

Owner waiver given; candidate = CodeRankEmbed (MIT verified, 137M NomicBert,
CoRNStack-curated — D5's rule). Weights vendored-in-waiting at sha256
`827529bcd58aef0d9082e66eeff7e7d53a02f62bd005f841a26b3d3e2fb17ebe` (546,938,168 B
f32 safetensors); NEVER downloaded at runtime — the model directory is a local
artifact validated at open.

`vorpal_ann::encoder` is owned end to end: a strict zero-copy safetensors loader
(F32-only; header/bounds/shape-product/4-alignment all typed refusals), an owned
WordPiece pipeline (BertNormalizer order clean → CJK spacing → NFD accent-strip →
lowercase; BERT's punctuation predicate = the four ASCII ranges ∪ Unicode P*;
greedy longest-match with the `##` continuation), and an owned NomicBert forward
(embeddings + emb_ln; post-norm blocks; biasless QKV/out_proj; non-interleaved
rotate-half rotary, base 1000, full 64-dim head; softmax 1/√64 max-subtracted;
SwiGLU with fc12 carrying the gate; CLS pool). Numerics are correctness-first:
every reduction accumulates f64 in fixed order, parallelism only across
independent output rows — bitwise identical at any thread count. A config asking
for semantics not reproduced here (pre-norm, RMS norm, interleaved/partial
rotary, biases, causal) refuses at open.

Oracles (gated on `VORPAL_CODERANK_DIR` — the artifact cannot live in the repo;
goldens regenerate via the reference generator `ref_forward.py`, which uses the
real `tokenizers` library for ids and an independent numpy forward for
activations, BLAS cross-checked against einsum at 2.8e-14):

| oracle | result |
|---|---|
| tokenizer vs reference library (unicode/code/casing battery + 3 texts) | byte-exact |
| forward CLS vs numpy reference, 3 texts | max rel err ≤ 1e-4 ✓ |
| embed bitwise reproducibility | identical bits |
| behavioral smoke (query vs factorial-def vs unrelated) | correct order |
| 3 texts + 4 embeds, release, f64 correctness-first pass | 0.63 s (~90 ms/short seq) |

The scale law that shaped the integration: doc-side encoding at kernel scale is
~2.4 × 10¹⁶ FLOPs (8.9 M definitions × ~2.7 GFLOP each; the earlier "~10¹²" here was
an arithmetic slip caught by the 2026-09-02 encoder research — the conclusion stood,
the exponent did not; hours of CPU) — this encoder can never be the warm-time row
embedder. Its shape is the opt-in QUERY-TIME RERANKER, shipped as
`<root>/encoder.dir` (a local model directory; missing = off; an unopenable
selection states itself via `Searcher::encoder_status` and searches keep
serving): the fused top-k stable-reorders by encoder cosine between the prefixed
query and each hit's name/signature/basename surface, RRF scores and channel
ranks untouched, conjunctions un-reranked. Plumbing oracles: a bad selection
degrades stated and still serves (ungated); the live rerank is bitwise
deterministic and only ever REORDERS (gated).

**Three rerank variants measured, one survives** (searcheval, both corpora,
NDCG@10 / MRR / recall@5; baseline = learned+retrofit fusion):

| variant | kernel short-kw (prot.) | kernel all | cpython descriptive | cpython short-kw (prot.) | cpython all |
|---|---|---|---|---|---|
| baseline | 0.206 / 0.298 / 0.095 | 0.298 / 0.386 / 0.208 | 0.316 / 0.229 / 0.500 | 0.293 / 0.500 / 0.250 | 0.308 / 0.319 / 0.417 |
| rerank, unpinned | 0.091 ✗ | 0.198 ✗ | 0.441 | 0.169 ✗ | 0.350 |
| rerank, ≤3-token guard | 0.206 = | 0.298 = | 0.298 ✗ | 0.293 = | 0.296 ✗ |
| **rerank, fused-winner pin** | **0.223 / 0.330 / 0.143** | **0.313 / 0.414 / 0.250** | 0.343 / 0.244 / 0.375 | 0.169 ✗ (MRR/recall hold) | 0.285 |

Mechanisms, measured: unpinned reranking demotes the consensus winners the
channels already agreed on — the 2026 "neural embedders collapse on short
keywords" finding reproduced in-house. The token-length guard fails because
length does not separate NL-intent from keyword queries (cpython's best rerank
win, "dictionary insert entry", is a 3-token NL query; its ≥4-token reranks are
a slight net negative). The FUSED-WINNER PIN — the encoder arbitrates only the
uncertain tail, never the rank-0 consensus — is the variant with a real win:
**the kernel gates GREEN** (all-NDCG +5%, and the protected short-keyword class
itself improves +8% with recall@5 0.095 → 0.143: the encoder FIXES deep hits it
can no longer break), while cpython stays mixed (descriptive +8.5%, short-kw
NDCG down with MRR/recall held). Pin+guard combined is derivably dominated (it
trades the kernel's gain for cpython's protection). DISPOSITION: the pinned
rerank ships; enabling is PER-CORPUS by construction (`encoder.dir`) and should
follow a measured gate on the target corpus — green at kernel scale, red on
cpython, recorded here. Query cost with the encoder live: ~3.2–3.6 s mean at
k=25 on the correctness-first f64 pass; the f32 GEMM round (hidden state f32,
six GEMMs in eight fixed f32 lanes reduced in fixed order — LN moments, rotary,
attention dots/softmax/A·V sums stay f64) brings it to **1.29 s mean / 2.49 s
max** with the parity oracle still ≤ 1e-4 and the kernel quality table
BIT-IDENTICAL (0.223 / 0.313). The batching round finishes the ladder:
`forward_batch` concatenates the prefixed query and every cache-missed
candidate surface into ONE token matrix (block-diagonal attention, rotary over
LOCAL positions — batched embeddings are BITWISE equal to solo ones, pinned by
a gated oracle), and candidate rows persist in a FIFO session cache (4096 rows
≈ 12 MB, the `cached_searcher` LRU-8 shipped-cap precedent) —
**0.887 s mean / 1.24 s max**, quality bit-identical again. Ladder:
3.62 → 1.29 → 0.887 s mean (4.1×); the f16-native GEMM kernel remains the
recorded lead. The opt-in is also configurable per index: `encoderDir` in
vorpalconfig.yml (relative to the project dir, must exist locally) writes the
selection at `vorpal kg index` time, the `semanticTier` discipline.

### Stage 6 packaging (owner decision 2026-08-31): both precisions, optional everywhere

Weights never ship inside release artifacts and are never fetched implicitly —
the ONE explicit download path is `vorpal_index::models` (installer behind the
`model-install` feature; the enable/read half is in every build, so any
consumer honors an enable a feature-full tool wrote). `vorpal enable
semantic-f32 | semantic-f16` installs under `$VORPAL_HOME/models`
(defaults `~/.vorpal/models`; `$VORPAL_MODELS_DIR` overrides) with streaming
sha256 verification against the pinned checksums (model `827529bc…`, tokenizer
`91f1def9…`, config `5ff856a4…`; `.part` + atomic rename; idempotent), then
writes the GLOBAL `~/.vorpal/encoder.dir` — the `Searcher` fallback when an
index root has no selection; per-index selections always win, and internal
generation-dir opens (the BM25 gate's probes) keep measuring un-reranked
fusion. `vorpal disable semantic-f32 | semantic-f16` is the symmetric partner:
variant-checked (a mismatch states what IS enabled and touches nothing),
weights kept on disk so re-enabling is instant. The per-corpus decision loop
ships as commands: `vorpal search "q" --ranked` renders the fused and
encoder-reranked orderings side by side with movement markers — ONE search,
both views from the same fusion; `vorpal tune --queries file` scores both
optional features on the user's own queries (`query => expected-substring`
lines, reciprocal rank, paired one-search views — the BM25 pair reuses the
warm gate's single-channel-pass trick with the encoder reranking both sides)
and writes this index's switches from the verdicts: the reranker via
`encoder.dir` (a model path, or the `off` SENTINEL — a per-index opt-out that
shadows a global enable), BM25 via a manual record override
(`set_bm25_override`) that holds until the index content retrains. No signal →
no write, stated. Python (`semantic_install/semantic_enable/semantic_disable`) and Node
(`semanticInstall/semanticEnable/semanticDisable`) expose the same verbs with a
`root` placement parameter.

The f16 variant converts the VERIFIED f32 bytes locally: owned IEEE 754
binary16 round-to-nearest-even (`encoder::f16`, EXHAUSTIVE oracle — all 65 536
half patterns round-trip), deterministic safetensors rewrite (names sorted,
offsets repacked, 8-aligned header), and an all-F16 loader path that upconverts
once at open into an owned f32 arena. Measured: 273.5 MB on disk (vs 546.9),
end-to-end embedding drift **cosine 1.000000** on both probe texts (the
inter-layer LayerNorms absorb the weight rounding), f16 path bitwise
reproducible. The stated trade: full-size RSS while a handle is live, until the
f16-native kernel lands.

## Extraction coverage, Wave 1 — macros/unions/type aliases + include-visibility resolution (2026-08-31)

The kernel bench against codebase-memory-mcp exposed the gap: their 8.53M nodes
vs our 2.76M was almost entirely `#define`s (ground truth: 6,122,556 `#define`
lines in the tree). Wave 1 makes vorpal extract them — and resolve them
*correctly*. New kinds `Macro`/`Union`/`TypeAlias` flow outline → product
(format v18) → KG → CLI/query (the query parser now accepts keyword-spelled
labels, so `MATCH (n:Union)` parses). Rules landed for C (`preproc_def`,
`preproc_function_def`, `type_definition`, union fix), C++ (namespace, unions in
member parents, macros, `typedef`/`using` aliases), Rust (`macro_rules!`, `type`,
`union`).

Resolution follows the candidate law (`SymbolKind::is_resolution_candidate`, one
definition consulted by every table feed): macros ARE candidates but bind by
INCLUSION, not name-globality. `IncludeReach` (crates/resolve/src/reach.rs)
condenses the file→file include graph through iterative Tarjan SCC (include
guards make cycles legal) into per-SCC sorted closures; the resolver's gate
admits a macro candidate only when its defining file is the reference's own file
or include-reachable — otherwise the reference is *masked*, never faked. Include
edges come from the import stream: exact → importer-relative → root-relative
suffix matching, where root-relative (`#include <linux/export.h>`) disambiguates
by nearest-prefix, then corpus-learned root support (the `-I` set inferred from
how much of the import stream each root satisfies — `learn_include_roots`), then
a dead tie stays unresolved.

Measured (release, quiet machine, `VORPAL_NO_AUTOWARM=1 VORPAL_PHASE_TRACE=1
/usr/bin/time -l vorpal-index index ../linux <dir>`):

| | pre-campaign | Wave 1 |
|---|---|---|
| kernel cold index | 10.13 s | **13.32 s** (117 s user; RSS 3.40 GB) |
| nodes | 2,763,928 | **8,695,186** (Macro 6,032,462 = 98.5% of `#define` truth; Union 3,111; TypeAlias 19,493) |
| edges | 6.67 M | 14,964,123 (`calls` 4,439,434; `imports` 75,190 → 381,838) |
| refs | 33 K resolved / 8.10 M **external** (self-index) | kernel: 3.88 M resolved / 1.72 M ambiguous / 217 K external / 976 K masked |

Oracle cost inside link (phase stamps): roots-learn 32 ms, seed 264 ms,
include-reach build 1.57 s (~1.1 GB transient closures), gated resolve 342 ms.
Correctness oracles: self-index — all 76 `array_push` call edges bind each
grammar's `scanner.c` to *its own* `tree_sitter/array.h` (48 same-named copies,
zero cross-grammar bleed; ambiguous count unchanged vs pre-macro baseline);
kernel — `EXPORT_SYMBOL` 0 → 2,633 calls, every sample to
`include/linux/export.h` (not the `tools/include/` shadow — root support broke
the prefix tie the right way); `list_for_each_entry` 8,312 calls to
`include/linux/list.h`. Cross-arch `<asm/...>` ties resolve only where one arch
root strictly dominates; equal-evidence ties stay honestly ambiguous/masked.

### Wave 2 — Java/C#/Go/JS/TS gaps + container transparency (2026-08-31)

Audit-driven rule additions: Java records (+compact constructors), `@interface`
annotations (+elements), packages, modules, enum constants, interface
constants; C# namespaces (block + file-scoped), events (both forms),
operators (arithmetic operators carry a literal `operator` name — the token is
an anonymous node — and stay distinct by signature), conversion operators,
indexers, destructors, `#define` → Macro; Go `type_alias` + the correction
that `type Foo int` is `TypeAlias`, not `TypeParameter`; JS/TS/TSX generator
functions, `var`-form declarations, TS `function_signature`
(declare/overloads), interface index/call/construct signatures; TS
`type_alias_declaration` corrected `Struct` → `TypeAlias`. Python decorated
definitions verified already-extracting (wrapper-node audit noise).

Two mechanism fixes the fixtures forced:

* **Container transparency** (`transparent: true` item flag,
  crates/outline/src/{extractor,combined_extractor}.rs): the item traversal
  never re-enters a matched item's subtree — correct for functions, fatal for
  namespaces (C++/C# block namespaces and TS namespace/ambient-module bodies
  swallowed every class inside once namespace rules landed). A transparent
  container extracts itself, then the traversal DESCENDS and keeps
  item-matching — contents are items with their own members, never members of
  the container. Applied to C++/C# namespaces and all TS/TSX
  namespace/ambient-module rules; pinned by a combined-extractor unit test.
* **Typedef/record disambiguation**: `typedef struct Config {…} Config;` is
  the struct's DEFINITION — the c/cpp typedef rules now decline body-bearing
  record/enum types (the traversal then reaches the struct rule), keeping
  only bodyless forms (`typedef struct Foo Bar;`, `typedef int u32;`) as
  aliases. Caught by the c-family goldens.

Real-repo validation (shallow clones, indexed, kind counts vs `rg` ground
truth): spring-petclinic — Java `Class` **42/42 exact**, `Package` **50/50
exact**; Humanizer — `.cs` `Module` 491 vs 499 namespace declarations (98.4%;
the gap sits in its 14 parse-error files); gin — `TypeAlias` 35 + `Struct`
123 + `Interface` 19 = 177 vs 176 grep'd `type` declarations (grouped-decl
variance). All outline suites green after golden updates that ADD the new
symbols (namespace Module lines, enum members, corrected alias kinds).

### Wave 3 — full 49-grammar closure (2026-08-31)

Every remaining language with real gaps, closed against per-language fixtures
and re-audited grammars (the audit's grammar-path map itself was fixed for
astro-next, nested md/ocaml, sequel, svelte-ng, toml-ng — astro/svelte/toml
prove CLEAN with zero definition-like nodes):

ObjC (protocol split from class; the full C-family port — records, enums,
unions, members, both macro forms, guarded typedefs; property names no longer
swallow `(nonatomic)` attributes) · Zig (`const X = struct/enum/union/error/
opaque` kind splits with container fields and enum/error members; `var` vs
`const` globals; `test` and `extern fn` declarations) · Lua (local + bare
top-level assignment globals) · Erlang (`-define` macros with arg-strip,
`-type`) · CMake (macro kind correction) · Julia (macros split from
functions, consts, primitives) · Haskell (`type` synonyms split to TypeAlias,
class signatures as methods) · Perl (file-scope `my`/`our`) · PowerShell
(rules REWRITTEN kind-based — context-pattern snippets never parse in this
grammar; class/method/property/enum extraction newly functional) · Scala
(top-level val/var/given, enum cases, abstract members, `type` → TypeAlias) ·
Kotlin (typealias) · Swift (extensions — the bare `extension` node kind is an
anonymous token, so class_declaration + declaration_kind with a `user_type`
descent; typealias, macro declarations, operators, deinit, subscript,
associatedtype, protocol and class properties, top-level bindings) · PHP
(transparent block namespaces, file and class consts) · Solidity
(constructors, modifiers, fallback/receive, errors, user-defined types,
constants, nested Yul functions) · Dart (typedefs, extension types, top-level
vars, external functions, getters/setters, all three constructor forms,
operators, static finals) · GraphQL (schema, directives, enum values, input
fields) · Bash (`declare`-command and bare-assignment variables; env-prefix
and function-local assignments excluded) · Elixir (defprotocol, defimpl,
defstruct) · Rust (top-level `const`/`static` — absent in our own language
until this wave — and `extern crate`) · OCaml (record/variant/synonym kind
splits with fields and constructors, module types, exceptions, `external`,
classes with methods and instance variables) · SQL (columns as table members,
CREATE TYPE/TRIGGER).

Debug law recorded: a `CombinedExtractors` failure reading "Fail to parse
yaml as Rule" means an anonymous (named=false) node was used as a `kind:`
matcher — verify node names against the COMPILED grammar
(`--debug-query=cst`), not node-types.json alone.

Campaign-final numbers (release, quiet machine, `VORPAL_NO_AUTOWARM=1`):
kernel **8,807,122 nodes** / 15,060,435 edges, 18.15 s real (121 s user,
RSS 3.46 GB; 13.3–18.2 s across runs on this hardware), refs 3.86 M resolved
/ 1.73 M ambiguous / 245 K external / 975 K masked. The typedef guard's
reclassification is visible at scale: TypeAlias 19,493 → 4,391 honest
aliases while `typedef enum/struct/union { … } name;` definitions surface as
the records they are — Enum 23,508 → 33,311 (+86 K EnumMembers), Struct
88,063 → 90,807, Union 3,111 → 3,274, Field +21 K. Variable +6.5 K from the
new shell/Perl rules. Self-index mirrors it (TypeAlias 1,069 → 440, Struct
739 → 766 with +413 fields), with ambiguous refs DOWN 12,840 → 12,331.
Pre-campaign baseline for the whole arc: 2,763,928 kernel nodes at 10.13 s.
Commits: 17b2c60 (wave 1) · d64c2de (wave 2) · 7771d3e (wave 3).

## Hyper-optimization campaign, pass 1 — allocation/fault/contention ledger + six fixes (2026-08-31)

Measurement first: feature `alloc-ledger` (opt-in, never default) wraps the
binary's jemalloc in exact event counters — Rust and tree-sitter C churn
attributed separately via `set_allocator` counting shims — plus mach
`TASK_EVENTS_INFO` faults, per-phase `getrusage` (parallel efficiency,
voluntary context switches), and contention counters at the pipeline's known
serialization points (interner shard try-locks, byte-budget parks,
full-channel sends). Counters are sharded across 32 cache-line-aligned
pthread-affine slots: the first build's four global atomics DOUBLED kernel
user CPU purely on cache-line ping-pong — the measurement manufacturing the
contention it measured — and the sharded rework holds overhead to +13 % user
with exact counts. Per-phase deltas via `ledger_deltas.py` over
`VORPAL_PHASE_TRACE=1` stamps.

Headline profile (kernel, pre-fix): **444 M allocator events per build** —
273 M tree-sitter C (~3,600 per file) + 171 M Rust with **57.8 GB cumulative
churn against a 1.4 GB live peak** (40×); 2.26 M faults; the stream phase
carries 96 % of Rust churn at healthy 0.87 efficiency in EVERY language
(polyglot matrix: kernel, cpython, TypeScript, spring-petclinic, gin,
Humanizer); interner contention is negligible everywhere (≤2.2 K contended of
~10 M+ acquisitions — the 64-way sharding holds); the real parallelism losses
are serialized phases (~3.5 s of 13 s at ≤0.08 efficiency). Small repos paid
a universal tax the kernel never showed: compiling all 49 languages' outline
rules plus the full canary table — ~160 K allocations, half of gin's total.

Fixes (each A/B-proven, ledger-instrumented kernel unless noted):

1. **IncludeReach closures — level-parallel CSR arena** replacing serial
   per-SCC `Vec<Vec>` growth: phase 1.85 s → 0.48 s (3.3×), transient spike
   1,055 MB → 262 MB, linear memory at any scale.
2. **`scc_sizes` — per-component collection Vec removed** (sizes assigned
   from the Tarjan stack tail, then truncate): exactly 8.8 M allocations
   eliminated on the kernel's acyclic-majority graph — every language's shape.
3. **Lazy per-language rule compilation** (`ExtractorSet::Lazy`): bundled
   docs bucket per language by a raw `language:` scan (zero-copy `&'static`
   slices), each language serde-parses + compiles on first use behind a
   `OnceLock`; eager fallback if bucketing can't attribute a doc; user rule
   sources keep the eager validating path; rules digest byte-identical.
   Startup: 158,737 allocs / 44 MB → 78 allocs.
4. **`build_include_reach` edge collection parallelized** (threads×2 chunks;
   `from_edges` is order-invariant, pinned by test): 0.27 s serial removed.
5. **Manifest-scoped canary self-check** (`verify_extraction_for_manifest`,
   per-language verdicts memoized process-wide): only languages the tree
   contains are checked — the threat model is per-language, so equally
   protective for the build at hand. gin total allocations 332 K → 175 K
   (−47 %), faults −36 %.
6. **`seed_import_bindings` parallelized** (chunked resolve + in-order serial
   fold = identical last-write-wins semantics): 0.28 s at 0.06 efficiency →
   0.04 s at 0.82.

Production A/B (plain release binaries, interleaved base/new, quiet machine,
`VORPAL_NO_AUTOWARM=1 /usr/bin/time -l`):

| corpus | wall (base → new) | peak RSS | page reclaims |
|---|---|---|---|
| kernel | 11.97/12.81 → 11.81/11.03 s (mean **12.39 → 11.42 s, −7.8 %**) | 3.53/3.49 → **3.25 GB (−7 %)** | ~flat |
| vorpal self | 7.82 → 7.63 s | 8.0 → 8.6 GB (single-file parse-tree monsters dominate; run-order variance) | ~flat |
| gin (small repo) | 0.08 → 0.07 s | **79 → 44.5 MB (−44 %)** | 11.5 K → 8.3 K (−28 %) |

Ledger-instrumented kernel wall across the pass: 13.46 → 11.43 s (−15 %).

### Pass 2 — callsite attribution sampler + two churn fixes (2026-08-31)

`VORPAL_ALLOC_SAMPLE=<shift>` (ledger builds): every 2^shift-th allocation
captures a symbolized backtrace into a bounded site table dumped at exit —
reentrancy-guarded per pthread slot (capture allocates; TLS is unsafe in
allocator context), and it symbolizes under LTO. Kernel and cpython
histograms AGREE on the top sites — the extraction inner loop owns Rust
churn: `extract_entry` 12.9 %, template rendering ~17 % across three stacks,
`MetaVarEnv::add_label` 5.5 %, `layout_entity_paths` ~15 % across three call
paths. Fixes from that data:

* **One entity-path layout per file** (`KgWriter::ingest_file_with_layout`):
  the committer built the same per-entity `String` layout twice (writer
  identity + reference attribution). Stream-phase allocations 154.9 M →
  144.8 M (−10.1 M).
* **Content-id chunk-buffer reuse, correctly**: `map_init` alone did NOT work
  — rayon's adaptive splitting reaches single-chunk jobs under work-stealing
  and re-runs the init closure per job, so the 3.9 GB stood. With a
  shard-derived `with_min_len` floor: interval churn 3,859 MB → 1,220 MB
  (−68 %), faults 247 K → 78 K, digests identical. (Recorded as a general
  law: `map_init` without a split floor is not buffer reuse.)

Totals across passes 1+2 (kernel, ledger-instrumented): allocations 171 M →
151.9 M, faults 2.26 M → 2.09 M, reallocs 16.9 M → 13.85 M, wall 13.46 →
11.76 s.

### Pass 3 — trivial-name template fast path (2026-08-31)

The dominant sampled template shape is a name template that is exactly one
plain metavariable. `NameTemplate::Trivial` renders `$NAME` by **borrowing
the matched node's text** (`Cow<'tree>`), skipping the engine's
leading-indent scan, byte-vector assembly, re-indent, and `String` build —
with the compiled engine template retained as an exact-semantics fallback
for capture shapes `get_match` cannot serve. `default_signature` and
`render_signature` likewise return borrowed `Cow`s for rules without a
signature template. Byte-identity proven by the outline golden suites across
all 49 languages. A/B (ledger kernel): stream-phase allocations 144.8 M →
**127.0 M (−17.8 M)**; self-index reallocs −26 %. Campaign totals, passes
1–3: kernel allocations **171 M → 134.2 M (−21.5 %)**.

### Pass 4 — allocation-free path probes + parallel CSR directions (2026-08-31)

`join_normalize_into` writes joined paths into a reused scratch `String`
with exactly the Vec-collect-and-join semantics (`join_normalize` delegates
to it — the two cannot drift), threaded through `ResolveScratch` and the
reach-build chunk closures. The per-probe `Vec<&str>` + `join` + `format!`
chain ran ~a million times per kernel link. A/B: the reach-build interval
went 1,148,207 → **106** allocations; reach-done and resolve-done each
−1.15 M; reallocs 13.85 M → **10.47 M**. `Graph::from_parts` now builds its
two CSR directions on scoped threads — measured NULL at kernel scale after
correcting an attribution error (the "seal: compact" interval is segment
column streaming faulting in ~1 GB of fresh `PodColumn` pages, not the CSR
build, which occupies the next 0.10 s interval); kept as correct and
linear-scaling toward 2 B-LOC edge counts. Campaign totals, passes 1–4:
kernel allocations **171 M → 130.7 M (−23.6 %)**, ledger wall 13.46 →
11.68 s.

### Pass 5 — assoc-vec metavariable environments (2026-08-31)

The matcher clones the metavariable environment copy-on-write per match
candidate, and the post-pass-4 histogram put ~29 % of remaining sampled
allocations in that family (String keys, hashbrown tables, rehashes). The
env's three maps are now insertion-ordered association vectors — public API
unchanged, linear scans over a handful of entries beating SipHash on String
keys, clones reduced to three Vec memcpys, iteration order now
deterministic. Gated wide (core, config scan/rewrite, outline goldens ×49
languages, ingest — all green). A/B (ledger kernel): allocations 130.7 M →
**127.55 M**, cumulative bytes −2 GB, faults −23 K; reallocs +1.4 M (vector
growth replaces table allocations; net events down). Honest residual: the
String keys still clone per env copy — interned or `Arc<str>` keys recorded
as the deeper follow-up. Campaign totals, passes 1–5: kernel allocations
**171 M → 127.55 M (−25.4 %)**, ledger wall 13.46 → 11.60 s.

Consolidated production A/B, passes 1–5 (plain release binaries vs the
pre-campaign snapshot, interleaved, quiet machine):

| corpus | wall (base → new) | peak RSS | page reclaims |
|---|---|---|---|
| kernel | 12.37/13.05 → 11.52/10.97 s (mean **12.71 → 11.25 s, −11.5 %**) | 3.54/3.48 → **3.25 GB** | 2.28 M → **2.10 M (−7.7 %)** |
| cpython | 1.11 → 1.11 s (parse-bound; churn already low) | ~flat | ~flat |
| gin | 0.09 → 0.07 s | 79 → **45.6 MB (−42 %)** | 11.5 K → 8.3 K (−27 %) |

### Pass 21 — template rendering into a per-file buffer + owned-string transforms (2026-09-01)

The post-pass-20 Rust histogram's leaders taken: the **template-render
family (22.2 %)** — `render_template` → `generate_replacement` →
`indent_lines` paid up to four allocations per rendered outline entry
(fixer vec growth, an indent copy that cloned its `leading` pad per line,
`.to_vec()`, `.to_string()`) — and the transform value's final
String → bytes copy.

* `TemplateFix::render_into` renders into a caller buffer: the fixer writes
  straight into it and re-indentation happens IN PLACE via one backward
  shift (`indent_multiline_in_place`, drift-anchored byte-equal to
  `indent_lines` across multi-line/trailing/leading/empty shapes); a
  single-line render — the overwhelming outline case — touches nothing.
  `generate_replacement` delegates, so the CLI fix path is unchanged. The
  outline walk threads one `RenderScratch` per file through
  `extract_entry`/`resolve_member_of`/both extracts; each rendered value
  costs exactly its one live `String`.
* `Content::decode_string` gives owned strings a MOVE into stored transform
  bytes (`String::into_bytes`) instead of the `decode_str(&s).to_vec()`
  copy; the default keeps the copy for non-byte sources.

A/B (ledger kernel): Rust allocations 9.90 M → 8.02 M (render scratch)
→ **7.54 M (−23.9 % total)**; TypeScript −7.0 %, Humanizer −12.7 %,
cpython flat (its templates were already pass-3 Trivial borrows — its
residue is transform-compute internals). Campaign totals: kernel Rust
allocations **171 M → 7.54 M (−95.6 %)**. Humanizer artifacts
byte-identical; full gate 127 suites / 1,223 tests green. Remaining
recorded leads: transform-compute internals (~1.4 M: per-type
`compute`-into-scratch through `string_case`/`rewrite`), the resolve
interner (~0.7 M, mostly live data), `SgNode::ancestors`' per-call Vec
under `Inside` rules (~0.35 M), reference-extraction `select_all`.

### Pass 20 — the C-side residue, all five items (2026-09-01)

The pass-19 residual (89.8 M) attributed and taken down, item by item —
each landed only after its own measurement:

1. **Cap "coldness" was ambient noise** — the deciding fact. Interleaved
   quiet re-runs show depth 65536 matches 8192's user CPU while cutting
   allocator calls a further 3.5×; the slab mini-allocator designed for the
   miss path is therefore **not needed** (measured-and-avoided). Default cap
   raised to 65536 (`TS_CHILDREN_CACHE_CAP` overrides).
2. **Leaf headers recycle across files** (`ts_leaf_cache_*` in the same TLS
   cache; `SubtreePool`'s malloc miss, overflow, and — the real leak —
   `ts_tree_delete`'s throwaway-pool teardown all route through it;
   `TS_LEAF_CACHE_CAP`).
3. **Cursor stacks come from the block cache** (`ts_tree_cursor_init`
   pre-carves 512 B, delete returns exact-size; grown stacks floor-bin and
   feed smaller classes). This one serves vorpal's own reference-extraction
   and rule-matching walks, ~8 M creates per kernel index.
4. **Stack-node pool cap: measured-null** — `TS_STACK_NODE_POOL=1000` moved
   ~46 K allocations (~0.05 %); upstream's 50 stands, knob kept for
   evidence.
5. **Parser reuse re-measured and flipped ON** (one parser per thread,
   `parse_lang`; the pre-campaign not-worth-it verdict predates the block
   cache): −3 M allocations, −0.25 M reallocs, CPU-neutral,
   `VORPAL_PARSER_REUSE=0` opts out. Re-entrant parses fall back to a fresh
   parser; a failed parse rebuilds it.

Combined (ledger, kernel, quiet ×2): C-side allocations 89.8 M →
**16.3 M** — from the 273.2 M pre-campaign baseline, **−94.0 %**, now in
line with the Rust side's −94 %. User CPU 133.7–134.4 → **112.3–113.4 s
(−16 % vs pre-pass-19)**, wall 10.33–10.37 → **9.20–9.25 s (−11 %)**, RSS
+~0.3 GB (deeper freelists — inside the decay-off frontier). Artifacts
byte-identical (Humanizer at final defaults; cpython with reuse ON),
grammar corpus battery green, full 127-suite / 1,222-test gate green.

### Pass 19 — the children-block cache: the one lever, pulled (2026-09-01)

The per-grammar examination's conclusion implemented: a thread-local,
size-classed intrusive freelist for the `[children..., SubtreeHeapData]`
blocks in the vendored runtime (`vendor/tree-sitter/src/children_cache.h`;
call sites: `stack__iter`'s carve, `ts_subtree_array_copy`/`_delete`,
`ts_subtree_clone`, `ts_subtree_new_node`'s grow, `ts_subtree_release`'s
free). Blocks a dropped tree returns are reused by the next file parsed on
that worker thread. Two free modes carry the no-overflow proof: exact
physical size (array paths, FLOOR class, sub-class bypass) and
claimed-node lower bound (ROUND-UP class == birth class on every audited
flow). The first build omitted the sub-class bypass and corrupted the heap
within seconds (`ref_count` assertion) — the audit's value is that the
failure was loud and immediate, and the two-mode split is the durable fix.

Per-class depth swept per the no-magic-constants law (kernel + cpython,
caps 512/8192/65536): kernel user CPU 123.6 / **116.5** / 119.4 s — depth
8192 is the optimum (deeper lists keep cutting allocator calls but colder
blocks cost more than the calls saved); cpython mildly prefers deeper.
Default 8192, `TS_CHILDREN_CACHE_CAP` overrides.

A/B (ledger, kernel): C-side allocations 273.2 M → **89.8 M (−67 %)**,
C-side reallocs 6.42 M → 1.41 M (−78 %), C-side bytes 27.6 → 12.7 GB,
**user CPU 129.1 → 116.4 s (−10 %, reproduced ×3), wall 10.32 → 9.33 s
(−9.6 %)** — the largest single-pass wall win of the campaign, and the
first to move the parse-bound core. RSS ~flat (3.86–4.24 GB), faults +2 %.
Polyglot: TypeScript ts-allocs −60 %, Humanizer −57 % (user −10 %),
cpython −41 %. Artifacts byte-identical (gin + Humanizer full-generation
diffs empty, at the shipped default cap); the 4k+ vendored grammar corpus
battery and the full 127-suite workspace gate (1,222 tests) green.
Ledger entry in docs/wip/UPSTREAM.md (runtime patch table).

### The per-grammar C-side examination (2026-09-01) — every grammar, one lever

Owner directive: examine *each* tree-sitter grammar. Instruments: the
langcorpus generator (`scripts/gen_langcorpus.py`, 45 languages from the
vendored grammar test corpora) at ×1 and ×32 content (same file count, ~32×
bytes — the delta isolates per-byte churn from per-parse setup), the ts
shims' per-run counters, a static scanner audit, and a NEW `VORPAL_TS_SAMPLE`
mask in the alloc ledger that backtrace-samples the tree-sitter C-side shims
(C frames symbolize; separate mask so C and Rust sampling don't swamp each
other).

**Per-byte slopes** (allocs/KB on grammar-author corpus text; ranking signal,
not absolute — real code is sparser): 18× spread, median 597. Heavy:
Haskell 1931 (+243 reallocs/KB), Perl 1404, Kotlin 1246, Elixir 1151,
OCaml 1147, Svelte 1124 (**+597 reallocs/KB** vs ~5 median), Dart 1093.
Light: Yaml 106, Proto 136, GraphQL 220, Json 241. C = 403, Cpp = 505,
TypeScript = 535. Scanner audit correlates: Perl's scanner carries 10
alloc refs (slope #2), Haskell's is 3,471 lines (slope #1); most scanners
allocate once, not per token.

**Callsite attribution — the headline**: across the 45-grammar sweep,
**43 grammars' top C-side site is one function — `stack__iter` under
`ts_parser__reduce`** (24–93 % of each grammar's C allocations; C = 87.4 %,
PowerShell = 93.0 %, CSharp = 91.8 %). Mechanism (vendored
`vendor/tree-sitter/src/stack.c`): every reduction pops its children through
a freshly `array_new`'d `SubtreeArray` that becomes the new internal node's
child storage — ~one malloc per internal AST node, the heap-per-node floor
of the runtime. Two secondary classes: `ts_subtree_new_node` growth chains
(Svelte 33.5 % — the realloc storm), and the `ts_parser__recover` path
(Xml 37.6 %, JsDoc 22.4 % lead with it; Haskell's #1 secondary) — recovery
iteration churns hardest exactly on error-bearing files. At kernel scale
`stack__iter` ≈ **~240 M of the 273 M C-side allocations (~88 %)** —
estimated ~4-5 s of user CPU in allocator calls alone, before locality.

**The lever is ONE change, not 49**: a tree-lifetime slab arena for subtree
child arrays in the vendored runtime (allocated in chunks owned by the tree,
freed wholesale with it), with realloc-in-arena support for the growth
chains. Recorded as the next deep arc; grammar-side follow-ups stay
secondary (Perl/Haskell scanner allocation habits, and the recover-path
churn which rewards the existing parse-health gates).

### Pass 18 — inline evidence alternatives: `AltSet` (2026-09-01)

The post-pass-17 leader, `link_resolve` (15.5 %), allocated a `Vec<u32>` per
evidence row with a non-empty tie set — `alt_ids[..alt_count].to_vec()` —
even though the resolver already hands the alternatives as a fixed
`[u32; MAX_RETAINED_ALTERNATIVES]` and the sidecar encodes a length byte
plus a pool. `EvidenceRow.alternatives` is now `AltSet`, a flat
`[u32; 8] + count` whose equality/ordering are slice semantics (exactly the
old `Vec`) and whose reads go through `Deref<Target = [u32]>`; the
conversion site takes the resolver's array by value, so a cap drift is a
compile error. Decode is corrupt-tolerant (`from_iter_capped`).

A/B (ledger kernel): allocations 11.65 → **9.91 M (−14.9 %)** — ~1.74 M
kernel rows carried alternatives; bytes 44.06 → 44.21 GB (flat — the array
rides in the row Vec). Polyglot: TypeScript −3.9 %, cpython −4.0 %, small
corpora ~flat (few ties). Campaign totals: kernel allocations
**171 M → 9.91 M (−94.2 %)**. **Byte-proof**: indexing Humanizer with the
pre-pass-15 baseline binary and this one produces an identical
`evidence.bin` SHA-256 and an EMPTY recursive diff over the entire
generation directory — passes 15–18 are invisible in every artifact byte.
Gate: full workspace `--no-fail-fast` = **127 suites, 1,222 tests green**
(sole failure remains the docs/wip move's `format_policy` path; earlier
"37-suite" gate counts were fail-fast truncations at that failure —
recorded so future gates use `--no-fail-fast`). Sampler after: transform
value computation (15.0 %) + template rendering (~15.6 % across
`render_template`/`generate_replacement`/`indent_lines`) + the resolve
interner (7.4 %) lead — the remaining `link_resolve` residue
(`DataflowRow.expr` strings) left the top five.

### Pass 17 — the last env clones: constraints, transforms, predicates (2026-09-01)

Pass 16's post-histogram named three surviving `MetaVarEnv` cloners, each a
borrow-the-env-and-let-the-first-write-clone-it protocol made expensive by
warm (high-water-capacity) envs. All three now run clone-free:

1. **`match_constraints`** snapshots just the constrained bindings (a
   handful of node handles — the exact reads the old iterate-self,
   write-a-copy protocol performed, preserving per-constraint candidate
   identity), then runs the constraints on the live env under a trial:
   failure rolls back byte-exactly, success commits in place.
2. **`do_match`'s transform `enclosing` clone** is gated by
   `Transform::needs_enclosing_env()`: only `rewrite` transforms read the
   enclosing env (their sub-rule matching inherits its bindings) —
   `replace`/`substring`/`convert` sets pass a shared empty env instead of
   cloning the whole warm env per transformed match.
3. **`OutlinePredicate::evaluate`** (`isImport`/`isExported`/`isPublic`
   rules) probed with a borrowed env whose first write — a relational
   label, a bind — cloned it per predicate per item, then dropped the clone
   with the verdict. Core now exposes ONE safe speculative surface,
   `MetaVarEnv::probe` (take → mark → run → rollback → restore): the
   predicate sees the item's bindings, keeps nothing, allocates nothing.

A/B (ledger, interleaved): kernel 36.54 → **11.65 M (−68.1 % vs the
pre-pass-15 base)**, bytes 48.65 → 44.06 GB; polyglot: gin −22.3 %,
Humanizer −33.7 %, spring-petclinic −32.1 %, TypeScript −49.2 %, cpython
−26.1 %. Campaign totals: kernel allocations **171 M → 11.65 M (−93.2 %)**;
user CPU flat (parse-bound). Gate: 806 tests / 37 release suites green
(sole failure remains the docs/wip move's `format_policy` path). The raw
scan path is untouched by construction (bare patterns carry no constraints,
transforms, or predicates). Sampler after: the `MetaVarEnv::clone` family
is GONE from the histogram; the leaders are now `link_resolve` string
churn (15.5 %), transform value computation (13.5 % + 3.3 %), template
rendering (~12 %), and the resolve interner (6.9 %) — different subsystems,
each its own arc. Still on the old protocol (cold, recorded): the
`nth_child` sibling probe.

### Pass 16 — the rule engine stops cloning envs: mark/commit/rollback everywhere + env recycling (2026-09-01)

Attribution first (the pass-2 sampler, shift 13): the two top kernel sites
(~38 % of samples) both symbolized inside `MetaVarEnv` growth — and a
diagnostic backtrace on a Toml-only corpus proved the `RawVec<Undo>` frame a
**symbol-folding artifact** (LTO identical-code-folding merges same-layout
`grow_one` bodies; there is no `RawVec<(&str, Node)>` symbol left in the
binary at all). The real story was env BIRTH: every candidate × rule × bind
bought vectors that failure threw away. Three coordinated cuts, each
A/B-measured:

1. **Scratch envs in the outline walk** (`MatcherExt::match_node_reusing` +
   `MetaVarEnv::reset_for_reuse`): one env per file walk; failed attempts
   reset-and-recycle it. Alone: kernel FLAT — the growth wasn't on the outer
   env, which sits empty while rule internals clone.
2. **Combinators trade the borrow-clone-discard protocol for
   mark/commit/rollback** (`And`/`All`/`Any`/`Or`/`Not`, plus the ellipsis
   probe from pass 15, all via one `CowEnvExt`): a failed branch is undone
   byte-exactly on the live env, a winner commits in place (`commit` closes
   the trial keeping writes; journal entries survive while any outer trial
   is open, and drop at depth zero). Kernel 36.52 → 34.28 M (−6.1 %).
3. **`Pattern::match_node_with_env` — the single biggest engine** — same
   rewrite, plus success envs RECLAIMED into the scratch after `extract`
   reads them (an outline `NodeMatch` dies moments after extraction; its
   buffers are not live data). The interim reclaim-only build regressed
   +550 K by warming envs that `Pattern`'s clone-per-bind then copied at
   full high-water capacity — the sampler showed `MetaVarEnv::clone` under
   `match_node_impl` jumping to 41.7 % — which is exactly why the Pattern
   rewrite and the reclaim land together.

A/B (ledger, kernel): allocations 36.53 M → **14.80 M (−59.5 %)**, bytes
48.65 → 44.62 GB; polyglot: gin −19.7 %, Humanizer −24.0 %, spring-petclinic
−31.7 %, TypeScript **−47.0 %**, cpython −24.8 % (an interim gin +1.2 %
regression from the composite-only build inverted once Pattern stopped
cloning). Campaign totals: kernel allocations **171 M → 14.8 M (−91.3 %)**.
Counterbalanced quiet pairs: user 133.29 vs 133.78 s, wall 10.99 vs 11.25 s,
RSS flat — CPU-neutral within run-order drift (indexing stays parse-bound;
the wall gains of this campaign came from earlier passes and accrue with
scale). The isolating scan bench (pass 15) reproduces **exactly** —
272,475 allocations, matches byte-identical — the brackets add zero
allocator traffic on the raw-pattern path. Gate: 806 tests / 37 release
suites green (sole failure remains the docs/wip move's `format_policy`
path). Sampler after: the matcher family is down to ~21 % of a 5× smaller
total — `match_constraints`' Cow commit and `do_match`'s transform
`enclosing` clone (recorded next), then `link_resolve` (12 %) leads.

### Pass 15 — mark/rollback pattern matcher: ellipsis probes without env clones (2026-09-01)

The lever pass 14 named, implemented by owner directive: the ellipsis
lookahead probe in `may_match_ellipsis_impl` no longer clones the aggregator
per candidate — it runs on the live aggregator between `mark()` and
`rollback(mark)`. The `Aggregator` trait trades its `Clone` bound for
`type Mark` + the pair; `ComputeEnd` restores its `usize`, and a
`Cow<MetaVarEnv>` marks by stashing the borrow itself while untouched
(rollback reassigns `Cow::Borrowed`, dropping any `to_mut` clone) or by an
`EnvMark` once owned. `MetaVarEnv::rollback_to` replays an undo journal
newest-first, then truncates the four assoc vecs to their marked lengths.
The journal — armed only while a trial is open, entries only for non-append
writes (slot overwrites move the old value in; label inner-pushes pop) —
makes rollback **byte-exact**, not merely equivalent: `does_node_match_exactly`
is non-transitive in one corner (named-leaf vs non-leaf text equality), so a
leaked equality-checked overwrite could otherwise flip a later verdict.
Probes roll back on success too (real consumption is re-done downstream),
preserving the old discard-the-probe-clone semantics exactly; marks nest
LIFO; `visit_nodes` re-adopts journal-held nodes. Two new tests pin
original-node-identity restoration and LIFO nesting.

Measured honestly, two different verdicts by workload:

* **Index path: neutral, structurally.** Every ellipsis in the bundled
  outline rules is terminal or trivia-followed (`($$$ARGS)`, `{ $$$BODY }`,
  `{ $$$SPECIFIERS };`) — the probe loop needs an ellipsis followed by a
  significant anchor and never executes during extraction. Ledger A/B:
  kernel 36.516 → 36.513 M allocations (jitter), gin / Humanizer /
  spring-petclinic / TypeScript / cpython all Δ<0.1 %. Counterbalanced quiet
  kernel pairs (base,new,new,base): user CPU mean 132.47 vs 132.07 s, wall
  mean 10.74 vs 10.74 s, RSS overlapping — the run-order drift exceeds any
  side effect.
* **Scan path (user patterns): −31 % matcher allocations.** Isolating bench
  (counting `GlobalAlloc`, `find_all` of `$F($$$A, $L)` plus a
  function-with-return anchor over 4.2 MB of real TypeScript —
  checker/parser/utilities.ts ×3 reps; base built from a pristine HEAD
  worktree): allocations 397,188 → **272,475 (−31.4 %)**, allocated bytes
  62.0 → **36.7 MB (−40.8 %)**, matches byte-identical (41,358), counts
  exactly reproducible across interleaved rounds; wall 203 → 200 ms.

Gate: 806 tests across 37 release suites green; the sole failure
(`format_policy::version_table_matches_the_constants`) reads
`docs/INDEX_FORMAT.md`, which the in-flight docs → docs/wip move relocated —
unrelated to this pass.

### Pass 14 — root-scratch env seeding: measured-and-null, reverted (2026-09-01)

Hypothesis: envs born empty per candidate re-pay vector growth, so a per-file
scratch env (take-on-entry, restore-on-failure) should recycle capacity into
every relational clone via the capacity-preserving `Clone`. Implemented
through the outline matching loop, gated green (53 suites, goldens
byte-identical) — and measured **null**: 36,511,750 vs 36,511,667 allocations
(Δ83, run noise). The growth the histogram shows lives in per-branch pattern
clones (ellipsis backtracking inside `match_node_impl`) that are discarded on
branch failure — their capacity never reaches the root env, so the scratch
accumulates nothing; successful envs depart with theirs. Clone-out-on-success
variants price out negative (per-success row copies exceed per-attempt
growth). The code was reverted; the genuine lever — eliminating the branch clones
inside the pattern matcher itself (mark/rollback backtracking on the
insertion-ordered env) — proceeds as the next pass by owner directive.

### Pass 13 — the RSS giveback: real phase-boundary purges (2026-09-01)

Two hypotheses for reclaiming pass-12's +~1.8 GB retained-dirty, measured in
order. **narenas consolidation: rejected.** Sweeping `narenas:4/8/18/36`
(kernel, decay-off binary) left peak footprint flat (4.34–4.55 GB — the
retention is NOT per-arena duplication) while walls ballooned 15.5–19.7 s at
flat user CPU: arena-mutex blocking under 14–18 allocation-heavy threads.

The resident-per-stamp timeline then localized the stacking: stream retains
~2.4 GB of dirty slab pages (3,430 MB resident at 1,024 MB live), and
cochange/link allocate ~1.9 GB of FRESH pages on top (4,202 → 4,541 MB,
flat to exit) — they never reused stream's pool (size-class mismatch), so
releasing it at the boundary forfeits almost nothing. The repo already had
the boundary hook: `release_freed_pages()`, placed at seven once-per-run
phase-death points by the memory campaign — but it calls macOS
`malloc_zone_pressure_relief`, a SYSTEM-allocator API that has been a silent
no-op for as long as the binary has linked jemalloc; default decay did the
releasing and masked it, and decay-off unmasked it. The fn now purges
jemalloc's dirty pages via `arena.<MALLCTL_ARENAS_ALL>.purge` (an action
node that genuinely supports the ALL sentinel; decay-independent), behind a
target-gated `jemalloc` feature forwarded from the index binary — embedder
builds keep the zone call.

A/B vs pass-12 (ledger kernel): post-stream resident **4,541 → ≤2,429 MB
flat (−2.1 GB)**, final resident **4,541 → 1,832 MB**, peak footprint
4.57 → **4.15 GB** (the remaining peak sits inside the stream drain window,
bounded by the streaming byte budget — scale-safe); faults 477 K → 582 K
(+105 K of genuinely forfeited reuse; still **−74 %** vs the 2.246 M
decay-on baseline), sys +0.5 s of bulk madvise, user CPU and allocation
counters unchanged. The measured frontier now stands recorded:
{decay-on: 2.58 GB peak, 2.25 M faults, sys 10.6 s} ↔
{decay-off + purges: 4.15 GB peak / 1.83 GB final, 0.58 M faults, sys 7.3 s}.

### Pass 12 — fault economics: decay-off for batch runs (2026-09-01)

Per-phase attribution put 71 % of the kernel build's 2.25 M page faults in the
stream phase, with `cow=86, pageins=0`: almost all were soft faults re-touching
pages jemalloc's default decay had purged mid-run while ~65 GB of churn cycled
through a ~1 GB live set. Knob battery on the unmodified binary
(`MALLOC_CONF`/`_RJEM_MALLOC_CONF`, kernel corpus):

| conf | faults | sys | user | wall | peak footprint |
|---|---|---|---|---|---|
| defaults (baseline) | 2,245,820 | 10.6 s | 128.7 s | 10.91 s | 2.58 GB |
| `dirty_decay_ms:-1,muzzy_decay_ms:-1` | **474,390** | **6.60 s** | 127.6 s | **10.22 s** | 4.36 GB |
| `oversize_threshold:0` | 2,248,306 | 10.9 s | 129.8 s | 11.00 s | 2.58 GB |
| both | 659,414 | 6.95 s | 133.5 s | 10.74 s | 4.35 GB |

Verdict: decay-off wins outright (−79 % faults, −4 s sys); `oversize_threshold`
is measured-and-rejected (fault-flat alone, +5 s user combined). Shipped as a
runtime `mallctl` at the **`index` command arm only** — a batch process whose
retained pages die at exit — while the long-lived daemon/serve paths keep
default decay (that decay is what returns their idle memory). The +~1.8 GB
peak-footprint trade is recorded as the explicit next target (phase-boundary
purge + narenas consolidation), not accepted.

Shipping the sentinel exposed an upstream bug: jemalloc 5.3.1's
`arena_i_decay_ms_ctl_impl` admits `MALLCTL_ARENAS_ALL` through the mib
resolver but never checks for it, so `arena_get(tsdn, 4096, …)` indexes one
past the `MALLOCX_ARENA_LIMIT`-slot arenas array and dereferences garbage —
observed as `EXC_BAD_ACCESS` in `pac_decay_ms_set` (lldb). Fixed at the
source: `tikv-jemalloc-sys 0.7.1` is vendored (`vendor/tikv-jemalloc-sys`,
`[patch.crates-io]`, tree-sitter precedent) with the handler mirroring
`arena_i_decay`'s ALL branch — writes iterate every initialized arena, ALL
reads return `EINVAL`; the smoke test is the exact crashing path. In-binary
A/B (ledger kernel, four runs): faults 471–487 K (−79 %), sys 6.1–6.5 s,
user 126.0–127.3 s (campaign best), wall best 10.54 s quiet (wall readings
noisy under ambient load; the load-invariant counters agree across all runs);
allocation counters unchanged (this is a fault pass).

### Pass 11 — interned env keys + high-water clones (2026-09-01)

The match engine owned ~46 % of post-pass-10 allocation samples, split across
three env sites: `insert`'s key `to_string` per fresh capture (16.0 %), the
assoc-vec growth under it (13.5 %), and `add_label`'s `secondary` growth
(16.2 %) — the latter two amplified by copy-on-write env clones whose derived
`Clone` produced exact-capacity vectors, so the very next push after every
clone reallocated. Two changes, both inside `meta_var.rs`: keys are now
interned `&'static str` (`intern_var` — a thread-local cache over a leak-once
global set; the name universe is compile-bounded rule meta-vars, and the hot
path touches no shared memory), and a custom `Clone` preserves each vector's
capacity — the source's own high-water mark, data-derived slack with no
constants. Read surfaces (`get_matched_variables`, the `HashMap` export)
reconstruct owned Strings on their cold paths. A/B (ledger kernel, quiet):
allocations 50.50 M → **36.51 M (−14.0 M, −27.7 %)**, reallocs 4.56 M →
**3.36 M (−26.4 %)**, faults FLAT at 2.25 M (the passes-9/10 rise stopped),
wall 10.91 s (within noise of the 10.84 s best), user CPU flat; tree-sitter
counters byte-identical. Campaign totals, passes 1–11: kernel allocations
**171 M → 36.5 M (−78.7 %)**, reallocs 16.9 M → 3.36 M (−80.1 %), ledger
wall 13.46 s → ~10.9 s.

### Pass 10 — fresh parses never materialize the product (2026-09-01)

`own_entry` (two `String`s per definition) plus the reference/request/param
owning block were ~35 % of post-pass-9 allocation samples — all spent
detaching extraction from its parse tree into an owned `FileProduct` whose
only stream-path job was crossing the worker→committer channel. Extraction
now has one body (`extract_with`) handing **borrowed** parts to one of two
finishes: the owning finish keeps `extract_product` byte-identical for the
batch path and tests, and the encoding finish (`encode_parts_into`, the
byte-for-byte twin of `encode_product_into`, pinned equal by a full-section
battery) serializes straight into stamped `.vpb` bytes. Those bytes cross
the channel (`StreamWork::ParsedEncoded`); the committer decodes views off
them and applies through the existing view kernel, then **moves the same
buffer** on into the pack sink — a single-owner chain
worker → committer → pack thread with no `Arc` and no copy (the pack's
canonical path-sort makes arrival order irrelevant; policy-excluded files
still bank from the worker, which is the only place they exist). The
encoded and owned fresh-parse paths are pinned to byte-identical sealed
output. A/B (ledger kernel, quiet): allocations 75.30 M → **50.50 M
(−24.8 M, −32.9 % — the largest single-pass cut of the campaign)**, user
CPU −1.6 s, wall 10.89 s → **10.84 s** (campaign best); reallocs flat;
tree-sitter counters byte-identical. Honest counters: cumulative churn
+1.1 GB and faults 2.121 M → 2.253 M (+132 K, reproduced on both runs) —
the per-file buffer lifecycle shifted (fresh encode/view vectors instead of
recycled product strings); CPU and wall pay for it several times over, but
the fault trend across passes 9–10 is a standing watch item. Campaign
totals, passes 1–10: kernel allocations **171 M → 50.5 M (−70.5 %)**,
reallocs 16.9 M → 4.56 M (−73 %), ledger wall 13.46 s → 10.84 s (−19.5 %).

### Pass 9 — identity paths without the Strings (2026-08-31)

The two `layout_entity_paths` sites were ~19 % of post-pass-8 allocation samples:
the committer built a `Vec<String>` of entity paths per file solely to hand the
writer transient `&str`s, and the worker built the same `Vec<String>` solely so
`owner_of_entity` could read each path's first `.`-segment. Both now share one
renderer, `write_entity_path_into`: the writer renders each identity into a
single reused buffer inline in its ingest walk (the caller-supplied-layout
variant is folded back in — lockstep by shared code instead of a debug
assertion), and the worker keeps a borrowed `EntityIdentity { owner, name,
discriminator }` per layout slot (one `Vec` per file), reconstructing the owner
segment on demand with a unit battery pinning reconstruction ≡ rendered-path
`split('.')` per branch. A/B (ledger kernel, quiet re-run): allocations
95.38 M → **75.30 M (−20.1 M, −21.1 %)**, reallocs 10.18 M → **4.56 M
(−55.2 %** — the layout `format!` growth chains**)**, user CPU −4.6 s, wall
11.67 s → **10.89 s** (campaign best). Faults 2.048 M → 2.121 M (+73 K,
reproduced on both runs — an allocator size-class shift, outweighed by the
CPU/wall win); tree-sitter-side counters byte-identical (parser untouched).
Campaign totals, passes 1–9: kernel allocations **171 M → 75.3 M (−56.0 %)**,
reallocs 16.9 M → 4.56 M (−73 %), ledger wall 13.46 s → 10.89 s (−19.1 %).

### Pass 8 — dedicated secondary-label storage (2026-08-31)

`"secondary"` is the only label the workspace ever adds — every relational
sub-match (`inside`/`has`/`precedes`/`follows`, pervasive in outline rules)
pushes one, and the map/assoc form allocated a key `String` plus a
one-element `Vec` per env, re-cloned on every copy-on-write env clone. The
label now lives in a dedicated `Vec<Node>` field with the old behavior
reconstructed on every read surface: `get_labels`, the matched-variables
listing, the JSON env export, and `visit_nodes`' cross-thread re-adoption.
A/B (ledger kernel): allocations 118.2 M → **95.4 M (−22.8 M, −19.3 % —
the largest single-pass cut of the campaign)**, reallocs −1.4 M. Campaign
totals, passes 1–8: kernel allocations **171 M → 95.4 M (−44.2 %)**, faults
2.26 M → 2.05 M, reallocs 16.9 M → 10.2 M.

### Pass 7 — `'static` kind names end to end (2026-08-31)

tree-sitter kind names live in the compiled grammar with `'static` storage;
the source trait's `Cow<'_, str>` erased that, so `extract_entry` built one
`String` per extracted definition — 8.8 M per kernel index — which product
assembly immediately interned and dropped. A defaulted `kind_static()`
trait method (tree-sitter backend overrides it; every other Doc keeps the
default `None`) lets the entry borrow the name. A/B (ledger kernel):
allocations 127.46 M → **118.19 M (−9.27 M)**; wall confirmed **10.97 s**
on a quiet re-run (a first 14.2 s reading was external machine noise —
user CPU flat across both; wall spikes get re-run before they get
recorded). Campaign totals, passes 1–7: kernel allocations **171 M →
118.2 M (−30.9 %)**, faults 2.26 M → 2.03 M, ledger wall 13.46 → ~11.0 s.

### Pass 6 — span-indexed entity attribution (2026-08-31)

`apply_parts` now maps every entity index straight into the NodeId array
`ingest_file_with_layout` already returns — index-aligned with product
layout order by construction — replacing the per-row canonical lookup that
blake3-hashed path+entity strings and probed a hash map for every
reference, parameter-ledger, sketch, and request row (~5.8 M rows per
kernel link). Correctness pinned by the streamed≡batch identity and
serial-specification suites. A/B (ledger kernel): instructions 1.892 T →
**1.866 T (−26 G)**, user CPU −3.8 s, cumulative bytes −800 MB; allocation
count ~flat — this pass buys CPU and cache locality rather than churn.
Ranked next arcs: the stream-phase churn monster (155 M Rust allocs — needs
the 1-in-N backtrace attribution sampler), the remaining serial phases
(cochange 0.36 s, seal:compact 0.36 s, kg save 0.48 s, content-id hashing's
3.9 GB buffer reads → streaming/mmap, mandatory at 2 B-LOC scale), resolver
path-probe string churn, a channel-depth/committer sweep (chfull 2.7 K), and
the tree-sitter per-parser arena question (273 M C-side allocations,
already jemalloc-routed).

## Two-mains integration — hyperopt line × phase-4/live line (2026-09-01)

Two divergent mains (this session's hyperopt passes 1–21 + semantic tier; the
other session's phase-4 bucketed format + subsecond daemon + live ANN + 0.3.0
release) merged into one line under a hard owner constraint: **no wall-clock
regression on either side's numbers**. Merge law where both sides bumped the
same constant independently: `PRODUCT_FORMAT_VERSION` → 19.

New persisted family: **`reach.bin`** (`VRCH`, `REACH_GRAPH_VERSION=1`,
path-table + u32 CSR; `crates/resolve/src/reach.rs`) — the full pipeline
persists its include-edge graph so scoped composes REPLAY the include-reach
oracle instead of rebuilding it (session-fresh first hop + BFS over stored
rows; `reach_rows_match` declines on any divergence, absence is a hard decline
to the full pipeline). Every generation assembler carries it: full save,
deferred (`PendingPersist`), served (`ServedPersist`), defs-stable,
defs-changed, respan, cutoff.

Convergence root causes found by the battery and closed:
1. **Seed-predicate asymmetry** — the spill retains EVERY import
   (`spill.imports()`) but the scoped compose, legacy link driver, and retained
   store all seeded from `Import && form==Static` subsets; a bare use bound
   ImportBound (reason 8) in scratch vs VisibleExport (6) in composes — one
   byte, evidence row 761, caught by generation-id divergence. Now ONE
   predicate everywhere (`kind == Import`); the seed's own resolution-reason
   filter decides what binds.
2. **Missing reach carries** — respan and the deferred/served persists did not
   write `reach.bin`; their generations differed from the scratch twin by
   exactly that file. `learn_include_roots` now clears root support before
   learning (idempotent re-learn — a maintained retained table must equal a
   from-scratch table over the same alive set).

Verification (release build, kernel = Linux tree, VORPAL_FORMAT=next):
- convergence battery: gin / Humanizer / cpython, scratch-determinism + 6 edit
  shapes — **PASS** (all 21 rows);
- kernel-scale convergence: one-shot edit incremental generation id ≡
  from-scratch id — **PASS**;
- full workspace gate 140 suites / **1275 passed / 0 failed**; clippy clean.

Walls vs the owner's no-regression bars:
| lane | their line (pre-merge) | merged | verdict |
|---|---|---|---|
| kernel cold build | — (mine ~9.2 s ledger-era) | 8.61–9.51 s (median 8.7) | ≤ bar |
| one-shot edit, steady state | 0.89–0.91 s | **0.52–0.53 s** | −41 % |
| respan (span-shift, clean parse) | 0.86 s | **0.74–0.77 s** | −12 % |
| ledger cold (vorpal-index alloc-ledger bin) | — | 7.60 s | campaign best |

Files whose error accounting shifts under an edit (macro-heavy kernel C, e.g.
`kernel/sched/core.c` top-insert changing tree-sitter error recovery) decline
all scoped composes by the UNCHANGED eligibility ladder and take the full
pipeline (~4.5 s) — their line declines identically; the ladder is byte-equal.

Ledger A/B on the merged tree (kernel): Rust allocs **36.9 M** (pass-21 line
was 7.54 M — their worker-side apply supersedes the encode-only extraction and
re-materializes products; recorded follow-up lead: port encode-only into the
worker apply), ts-side **16.08 M** (bar 16.3 M — held), wall 7.60 s (campaign
best; the alloc delta is outweighed by their pipeline restructure).

## Encode-only extraction, re-landed inside the worker-apply topology (2026-09-02)

The merge's one recorded alloc regression closed: fresh parses again never
materialize an owned `FileProduct`. `StreamWork::ParsedEncoded(path, bytes)` —
extraction encodes borrowed parts straight to stamped `.vpb` bytes
(`extract_product_encoded`), the worker applies from a `ProductView` decoded
over those bytes (the same view-apply machinery `ReplayedPacked` uses), then
the SAME buffer moves to the pack through a fresh-product sink threaded into
the stream (`stream_apply_spilled` gained the parameter; `ParsedEncoded`
without a sink is a hard error, not a silent drop). Unhealthy-excluded files
still bank their bytes from the closure and skip the apply.

Oracle restored (`encoded_stream_matches_owned_stream`): encoded-path sealed
artifacts byte-equal the owned path's, AND the sink receives exactly the owned
encoding, per file.

Kernel A/B (ledger binary): Rust allocs **36.92 M → 9.88 M (−73.2 %)**;
ts-side flat (16.08 M); churn bytes flat (same product bytes, fewer buffers);
wall flat within the recorded thermal band (best 7.71 s vs 7.60 s pre-port,
user floor 110.9 s vs 110.6 s). Production: cold 8.96–8.97 s, steady one-shot
edit 0.52–0.53 s — both unchanged. Battery PASS ×3 corpora; kernel edit
convergence PASS; workspace gate 140 suites / 1276 / 0.

Remaining gap to the pass-21 line's 7.54 M is their pipeline's own share
(worker-side writers, map_chunk streaming) — no single dominant site recorded.

## v0.4.0 release benchmarks — the polyglot table + a corruption find (2026-09-02)

The README performance section was re-measured end to end on the v0.4.0 binary
(M5 Max, quiet machine, best-of runs; datasets pinned by commit). Flagship kernel:
cold 8.19/8.24/8.96 s → **8,890,840 nodes** (3.1× the pre-merge line's 2.85 M — macro
truth, records with members, 49 grammars), edit 0.55–0.64 s, touch 0.71 s, unchanged
0.16 s, generation 5.5 GB. CLI one-shot search 1.5–1.9 s at that scale (process start +
mmap); daemon medians over 30 stdio round-trips: graph <1 ms, hybrid search 53 ms,
first-search tier warm-up 3.6 s. Scan-vs-grep: 4.83 s vs rg 1.03 s.

Fourteen-repo polyglot cold table (shallow clones, commit-pinned — full rows in the
README): llvm-project 8.1 s/1.44 M nodes, zig 6.3 s/1.09 M, kotlin 2.7 s/796 K,
kubernetes 2.0 s/693 K, roslyn 0.6 s/490 K, rust 2.7 s/464 K, WordPress 1.8 s/287 K,
spark 1.6 s/254 K, kafka 0.7 s/209 K, next.js 1.0 s/205 K, ghc 0.7 s/178 K, cpython
2.3 s/163 K, rails 0.3 s/50 K, neovim 0.3 s/41 K, vuejs/core 0.1 s/11 K.

**The sweep caught a real bug**: llvm-project and rust-lang/rust segfaulted the
indexer — the children-cache claim-shape hole (exact-reserved arrays claimed by
`ts_subtree_new_node` un-class-shaped; round-up binning over-promised; heap
corruption). Fixed with the claim-time guard `ts_children_node_block_ok` (see
UPSTREAM.md and the vendored `claim-shape` regression fixture); ts_allocs
16.081 M → 16.066 M (flat), walls in band, battery PASS. The polyglot benchmark table
is now also a standing corruption canary — the exact reason the tool is benchmarked
wide instead of kernel-shaped.

## Giant-file tree cache — incremental reparse for long-lived processes (2026-09-02)

The landing of the large-file arc (the chunker below was the rejected road): a
process-global cache retains parse state (source + tree handle) for files ≥ 1 MiB and
re-parses edits through tree-sitter's own incremental contract (`ts_tree_edit` + reuse
seed ⇒ identical tree, by library guarantee). One seam — `extract_with`'s parse — so
every long-lived surface benefits transparently: the MCP overlay daemon, watch-loop
in-process builds, and every SDK server calling `indexBuild` per save.

Measured (M5 Max, cumulative-save bench, byte-verified against a fresh parse EVERY
round): 54 MB julia parser.c **4.13–4.27 s → 1.86–1.91 s per save (2.2×)** over an
8-round soak; 17 MB cpp parser.c 1.33 s → 0.58 s (2.3×); 1.4 MB cpython Parser/parser.c
104 ms → 34 ms (3.1×). The residual is the extraction walk — the parse share is fully
eliminated. Cold builds are inert by design (each file parses once; retention is pure
cost): interleaved kernel colds 7.95/8.86 s on vs 8.57/8.88 s off (noise-law
indistinguishable; only 4 kernel files reach the 1 MiB floor). Budget defaults 64 MiB
of retained SOURCE (tree mass ≈ 10–40× source, ledger-profiled → ~2.5 GB worst-case),
`VORPAL_TREE_CACHE{,_MIN,_BUDGET}` override; `=0` disables. Oracles: eight edit shapes
+ cross-language + the vendored 3.8 MB giant, products byte-identical incremental vs
fresh; full gate 141 suites / 1280 / 0; battery PASS.

## Walk reuse — incremental saves re-walk only the dirty region (2026-09-02)

The tree cache's recorded residual — the extraction walk itself — closed. Beside the
retained parse, extraction now snapshots its PRE-finalize outputs per giant file:
pre-adoption outline items, pre-dedup reference rows, binders, finished near-clone
sketches, and the definition layout, in a compact span-or-text form (`Snip`: source
slices as byte offsets, rendered strings as text — capture allocates for rendered
strings only, and resolving against the next save's source is `Cow::Borrowed` both
ways). On the next save, `try_new_incremental_ranged` reports the spanning edit plus
tree-sitter's changed-range verdict; the dirty region = that union expanded to whole
top-level item spans (fixpoint); ONLY the dirty subtrees re-walk (ancestor context
seeded so parent-sensitive dispatch matches the full walk; a regional signer signs only
dirty definitions); retained rows splice around the fresh ones — byte positions
shifted, attribution remapped by definition-span lookup in the NEW layout, sketches
carried verbatim — and the unchanged file-global laws (adoption, layout, binder
shadowing, type/impl dedup, receiver stamping, error scan, encode) run over the merged
whole. Eligibility, C first: no injections, no typefacts, no request specs, no nested
item rules; parse ERRORS deliberately do NOT gate (the incremental tree equals a fresh
parse by the library contract — the error-carrying vendored giant is oracle-pinned).
Every splice invariant is re-checked at runtime; any violation falls back to the full
walk. `VORPAL_WALK_REUSE=0` disables, `_TRACE` prints dirty regions + phase millis,
`SPLICES`/`FALLBACKS` counters keep the oracles non-vacuous.

Snapshot mass (`snapshot_mass` example): 2.0–2.8× source across giant classes —
tree-sitter-c parser.c 3.87 MB → 9.5 MB (2.5×, 70,133 rows); -haskell 19.8 → 40.5 MB
(2.0×); -cpp 17.3 → 43.9 MB (2.5×); -julia 54.7 → 152.4 MB (2.8×, 1,133,046 rows).
Cache accounting therefore charges entry cost = source + snapshot, and the default
budget rescales to the swept 64 MiB source capacity × (1 + ratio ceiling 3) = 256 MiB —
same retention intent (one julia-class giant, or a handful of kernel-class files).

Measured (M5 Max, cumulative-save bench, byte-verified vs a genuinely fresh whole-file
extraction every round — the control probe name is now unique per round, since the old
shared name entered the cache itself from round 2 and quietly weakened the check):

| 54.7 MB julia parser.c, per save | wall | vs fresh 4.2 s |
|---|---:|---:|
| tree cache only | ~1.93 s | 2.2× |
| + walk splice, edit between definitions | **0.66–0.71 s** | **6.1×** |
| + walk splice, edit INSIDE the single 43.3 MB parse-table item | 1.70–1.71 s | 2.5× |

Splice machinery inside a 684 ms save (`_TRACE` phases): row split 31 ms + regional
walk/merge 10 ms + capture/store 30 ms + finalize 13 ms ≈ 84 ms on 1.13 M retained
rows; the remainder is the incremental reparse plus the bench's owning finish
(production streams encoded parts). Item granularity is the honest floor: one item
spanning 80 % of the file means an in-item edit re-walks that item.

Oracles: 12 edit shapes (add/delete/rename definitions, struct-member change,
two-distant-edits, whitespace, identical, UTF-8 retained prefix …), multi-save chains
(splicing against snapshots the splice path itself captured), and the error-carrying
vendored giant — each byte-compared against fresh extraction, with a SPLICES-moved
assert so a silently-dead reuse path can never pass. Cold builds stay inert (two-touch:
capture needs a retained entry): kernel cold 8.25 s reuse-on vs 8.82 s off (noise law);
`wants_snapshot`'s per-file mutex tap is additionally pre-gated by the lock-free 1 MiB
policy check so cold builds touch the cache lock only for actual giants.

Closing gates for the campaign (kernel, this landing): workspace 141 suites /
**1286 passed / 0 failed**, both clippy lanes clean; convergence battery **21/21 PASS**
(ast-grep, cpython@HEAD, kafka@HEAD); polyglot canary 15 repos @ HEAD cold+unchanged
all exit 0 (llvm re-checked under the README protocol at 7.86 s best-of-3 — the sweep's
one-shot 9.0 s reading was thermal context, not a regression); production cold
**8.25 s** / one-shot edit **0.51 s** / unchanged 0.12 s (bars 8.6–9.5 / 0.52–0.53 /
0.13). Ledger binary (exact counters, deterministic): Rust allocs **7.60 M** (campaign
start 9.87 M, pre-walk-reuse 7.99 M), reallocs **3.11 M** (start 7.77 M), churn
**48.9 GB** (start 56.1 GB), ts-side 16.06 M (bar 16.3 M held), faults 653 K,
`time -l` peak RSS 5.58–5.64 GB (campaign start 6.7 GB), user 108–113 s at the
recorded 110.9 s floor; walls 7.9–9.1 s across a warm session — inside this file's
standing thermal-variance law (identical builds vary 101–117 s user).

## Ranking tiers re-baselined on the v0.6.1 graph + daemon memory per tier (2026-09-02)

The README gained a search-quality section, so every tier was re-measured on the
CURRENT extraction (8.89 M kernel definitions — 3.1× the graph the Stage 0–6 tables
above were graded on, macro/union/typedef kinds included). The recorded per-stage
deltas (learned +43 %, encoder +5 %/+8 %) do NOT transfer to this graph; the numbers
below supersede them for README purposes, and the per-stage tables stay as the
history of each mechanism on the graph it was measured on.

Substrate: kernel `1590cf032971` (README pin), cpython `b86a41cbf63` (README pin),
this repo at v0.6.1; per corpus one `--semantic-tier lexical` index and one
`--semantic-tier learned` index, both `__warm-ann`'d; encoder = CodeRankEmbed pinned
weights installed into a scratch `VORPAL_HOME` via `vorpal enable semantic-f32` then
`semantic-f16` (f16 converted from the verified f32); tier flips via `vorpal enable`.
Harness: `xtask searcheval` (NDCG@10 / MRR / recall@5, determinism + label-existence
gates, `VORPAL_NO_AUTOWARM=1`).

| corpus · tier | NDCG@10 | MRR | recall@5 | class detail |
|---|---:|---:|---:|---|
| kernel · lexical | 0.299 | 0.375 | 0.229 | short-kw 0.206/0.286/0.119; descriptive 0.947/1.0/1.0 |
| kernel · learned | 0.295 | 0.305 | 0.250 | short-kw 0.202/0.206/0.143 ("mutex lock acquire" rank 0 → 23) |
| kernel · learned + f16 | 0.244 | 0.288 | 0.167 | short-kw 0.143/0.186/0.048 — REGRESSION |
| kernel · learned + f32 | 0.244 | 0.288 | 0.167 | bit-identical to f16 |
| cpython · lexical | 0.137 | 0.208 | 0.250 | descriptive 0.036/0.062/0.125; short-kw 0.338/0.5/0.5 |
| cpython · learned (BM25 gate ON) | 0.412 | 0.528 | 0.333 | descriptive **0.475**/0.542/0.375; short-kw 0.287/0.5/0.25 |
| cpython · learned + f16 | 0.410 | 0.556 | 0.500 | descriptive 0.531/0.583/**0.625**; short-kw 0.169/0.5/0.25 |
| cpython · learned + f32 | 0.410 | 0.556 | 0.500 | bit-identical to f16 |
| vorpal · lexical | 0.571 | 0.560 | 0.550 | exact 1.0; short-kw 0.894; paraphrase 0; conjunctive 0 |
| vorpal · learned (BM25 gate ON) | 0.559 | 0.550 | 0.550 | conjunctive 0.301/0.111/0; subset 0.500 |
| vorpal · learned + f16 | 0.648 | 0.625 | 0.750 | descriptive 0.815/0.75/1.0; conjunctive 0.431/0.25/1.0 |
| vorpal · learned + f32 | 0.648 | 0.625 | 0.750 | bit-identical to f16 |

Reading: (1) f16 ≡ f32 in every cell, as the conversion oracle (cosine 1.000000)
predicted — the size choice is disk/RSS only. (2) Tiers are per-corpus decisions:
the learned tier triples cpython's descriptive class and is neutral on the kernel and
this repo; the encoder rerank lifts recall@5 (cpython 0.33 → 0.50, vorpal 0.55 →
0.75) but lowers the kernel's short-keyword class (0.202 → 0.143) — the subword-
identifier answers the fused-winner pin protects only at rank 0. `vorpal tune` is the
instrument that decides this per index. (3) The per-corpus BM25 warm gate fired ON for
the learned warms of cpython (36 W / 19 L, mean 0.4442 vs 0.4388) and this repo
(38/21, 0.3715 vs 0.3661) and OFF on the kernel (17/13) and on every lexical warm —
so the cpython/vorpal learned rows carry the BM25 channel; see the attribution note
below. (4) Tier-path latency in the graded runs (k = 25, in-process): kernel
lexical 50.9 ms mean, learned 53.5 ms, + encoder 0.93 s (f16) / 0.97 s (f32); cpython
1.6 / 35 / 657–693 ms; vorpal 0.9 / 14 / 639–658 ms.

Daemon latency + peak RSS per tier (`mcp_bench.py`: stdio `vorpal mcp --index`,
initialize handshake, 30 `tools/call` round-trips each of `search` (k = 10, label
queries cycling), `callers`, `node`; RSS via `ps -o rss` after every call):

| index · tier | search median | search p95 | first search | callers median | node median | peak RSS |
|---|---:|---:|---:|---:|---:|---:|
| kernel · lexical | 58.8 ms | 64.7 ms | 210 ms | 0.11 ms | 0.10 ms | 2,101 MB |
| kernel · learned | 60.5 ms | 63.1 ms | 243 ms | 0.12 ms | 0.11 ms | 2,638 MB |
| kernel · learned + f16 | 90.9 ms | 476 ms | 673 ms | 0.12 ms | 0.11 ms | 3,167 MB |
| kernel · learned + f32 | 86.6 ms | 474 ms | 583 ms | 0.12 ms | 0.11 ms | 3,081 MB |
| cpython · lexical | 1.0 ms | 1.3 ms | 5 ms | 0.05 ms | 0.05 ms | 106 MB |
| cpython · learned | 2.2 ms | 2.6 ms | 15 ms | 0.07 ms | 0.07 ms | 149 MB |
| cpython · learned + f16 | 30.5 ms | 285 ms | 441 ms | 0.08 ms | 0.07 ms | 677 MB |
| cpython · learned + f32 | 30.3 ms | 289 ms | 354 ms | 0.08 ms | 0.07 ms | 591 MB |
| vorpal · lexical | 0.7 ms | 0.8 ms | 3 ms | 0.05 ms | 0.05 ms | 63 MB |
| vorpal · learned | 1.6 ms | 1.9 ms | 7 ms | 0.07 ms | 0.07 ms | 81 MB |
| vorpal · learned + f16 | 30.5 ms | 283 ms | 484 ms | 0.08 ms | 0.07 ms | 609 MB |
| vorpal · learned + f32 | 31.4 ms | 288 ms | 385 ms | 0.07 ms | 0.07 ms | 525 MB |

Encoder medians are cache-served (8–10 distinct queries cycling through the 4,096-row
FIFO embedding cache); p95/first-search are the uncached cost. f16 RSS EXCEEDS f32 by
~90 MB on every index: the f16 file maps at 274 MB AND decodes into a 547 MB owned f32
arena at open (recorded caveat, models.rs) — the f16-native GEMM kernel remains the
lead that would make f16 the memory win it looks like on disk.

One-shot CLI `vorpal search` (process start + mmap, page cache warm, lexical index):
kernel 0.19–0.20 s, cpython 0.01 s, vorpal < 0.01 s — the README's earlier 1.5–1.9 s
kernel figure was a cold-cache-era reading and is replaced. Index on disk (one
generation): kernel 7.6 GB lexical / 8.3 GB learned (the README's 5.5 GB dated from the
2.85 M-node graph), cpython 200 / 267 MB, vorpal 836 / 867 MB. Encoder weights on disk
547 MB (f32) / 274 MB (f16). Indexer peak RSS (`time -l`): kernel 5.6 GB, cpython
0.82 GB, vorpal 12.2 GB (the 49 vendored parser.c giants).

**BM25-gate attribution** (the learned rows above carry the channel where the gate
fired; re-measured with the record forced off via a throwaway `set_bm25_override`
helper, then restored): cpython learned BM25-off **0.390 / 0.438 / 0.417** (vs ON
0.412 / 0.528 / 0.333) — the learned tier itself carries the cpython gain over lexical
(0.137 → 0.390), BM25 adds MRR and costs recall; vorpal learned BM25-off **0.609 /
0.617 / 0.550** (vs ON 0.559 / 0.550 / 0.550; subset "postings" 1.0 → 0.5 under BM25)
— here the label-free gate's verdict (38 W / 21 L on name-token probes) DISAGREES with
the labelled eval by −0.05 NDCG. Recorded as the gate's known limit: it measures
known-item name-subset retrieval, not descriptive/subset intent. Open lead: feed
`vorpal tune` labels into the gate's decision when they exist.

### Rerank mode A/B — encoder as an RRF list vs tail reorder (2026-09-02, owner: "measure, don't guess")

Hypothesis from the kernel regression: the encoder's cosine order REPLACES the fused
order of the tail, discarding the lexical/graph rank evidence for ranks 1..k — so add
it as one more reciprocal-rank list over the pool instead (same embeddings, same
cache, zero latency delta). `RerankMode` (lib.rs) implements three: `Reorder`
(shipped), `BlendPinned` (rank 0 pinned, tail = fused mass + 1/(K+encoder rank)),
`Blend` (whole pool). Encoder f32, learned indexes, `VORPAL_RERANK_MODE` sweep:

| corpus | no encoder | reorder (shipped) | blend-pinned | blend |
|---|---:|---:|---:|---:|
| kernel all | 0.295 / 0.305 / 0.250 | 0.244 / 0.288 / 0.167 | **0.279 / 0.292 / 0.167** | 0.279 / 0.291 / 0.167 |
| kernel short-kw | 0.202 / 0.206 / 0.143 | 0.143 / 0.186 / 0.048 | 0.184 / 0.191 / 0.048 | 0.184 / 0.190 / 0.048 |
| cpython all | 0.412 / 0.528 / 0.333 | **0.410 / 0.556 / 0.500** | 0.411 / 0.528 / 0.333 | 0.411 / 0.528 / 0.333 |
| vorpal all | 0.559 / 0.550 / 0.550 | **0.648 / 0.625 / 0.750** | 0.605 / 0.575 / 0.550 | 0.642 / 0.625 / 0.550 |

Verdict: the blends recover most of the kernel loss (0.244 → 0.279) but not to the
no-encoder line (0.295), and they damp exactly the deep pulls that produce the recall
gains elsewhere (cpython recall@5 0.50 → 0.33, vorpal 0.75 → 0.55; vorpal `subset`
alone goes 0.631 → 1.0 under unpinned blend — the pin costs that one query). No mode
dominates → `RERANK_MODE = Reorder` stays pinned; the per-index decision (`vorpal
tune`, `encoder.dir` / `off`) remains the correct instrument. The blend mechanism
ships behind the sweep env as the measured seam.

### Learned-tier kind policy A/B — what the distributional model trains on (2026-09-02)

Hypothesis from the 3.1× graph: 5.9 M of the kernel's 8.9 M definitions are `#define`s,
so macro surfaces dominate the co-occurrence statistics the learned tier factors.
`TrainKindPolicy` (lib.rs `train_learned_model`): `All` (shipped), `ExcludeMacros`,
`BalanceToCallables` (every kind capped at the population of the largest callable
kind — Function/Method/Constructor — by a deterministic id-order stride; the cap is
read off the corpus). Fresh `--semantic-tier learned` indexes + warms per policy
(`VORPAL_LEARNED_KIND_POLICY`), no encoder:

| corpus | all (shipped) | exclude-macros | balance |
|---|---:|---:|---:|
| kernel all | 0.295 / 0.305 / 0.250 | 0.267 / 0.312 / 0.208 | **0.313 / 0.346 / 0.250** |
| kernel short-kw (protected) | 0.202 / 0.206 / 0.143 | 0.170 / 0.214 / 0.095 | **0.222 / 0.253 / 0.143** |
| cpython all | 0.412 / 0.528 / 0.333 (BM25 on) | 0.256 / 0.278 / 0.333 (BM25 gate flipped OFF) | 0.412 / 0.528 / 0.333 (identical: no kind exceeds the callable cap) |
| vorpal all | 0.559 / 0.550 / 0.550 | 0.562 / 0.548 / 0.550 | 0.529 / 0.542 / 0.550 (conjunctive 0.301 → 0.000, one query) |

Reading: excluding macros LOSES on the kernel — kernel macros (`mutex_lock` is one)
are retrieval targets and real distributional signal. Balance is the first policy
that puts the learned tier ABOVE the lexical tier on today's kernel graph (0.313 vs
0.299 all; 0.222 vs 0.206 protected NDCG), at zero query-time cost (train-time only;
kernel warm 217 s vs 224 s exclude in this contended run), bit-identical on cpython,
and −0.03 on this repo from a single conjunctive query ("louvain community size cap"
→ `cut`, where the tier only re-ranks a lexically-supported conjunction). Not a
dominance; the kernel is the design-floor corpus. DISPOSITION (owner: "Item 2. Do
item 2."): **`LEARNED_TRAIN_KINDS = BalanceToCallables` PINNED.** The policy label
now rides in `ann.model.json` (`train_kinds`; pre-policy files read as `"all"`) and
the learned freshness gate demands the active label, so every existing learned tier
retrains under the new binary instead of serving a model the code would no longer
produce. Proved live on cpython: fresh warm trains and stamps `balance`; a control
re-warm no-ops (0 train starts); the same record with the field removed (a
pre-policy file) retrains and re-stamps. Re-verified under the pinned binary (fresh
indexes + warms): see the "pinned" rows appended below. The other two policies stay
reachable through the sweep env as measured seams.

Pinned-binary re-verification (fresh `--semantic-tier learned` index + warm per
corpus, `train_kinds: balance` in every record, no encoder):

| corpus · pinned learned | NDCG@10 | MRR | recall@5 | note |
|---|---:|---:|---:|---|
| kernel | **0.313** | **0.346** | **0.250** | reproduces the A/B exactly; short-kw 0.222 / 0.253 / 0.143; BM25 gate off |
| cpython | **0.412** | **0.528** | **0.333** | identical to `All` (no kind exceeds the callable cap); BM25 gate on |
| vorpal | **0.612** | **0.614** | **0.550** | BM25 gate OFF this warm (the A/B's 0.529 had it on); subset 1.0, conjunctive 0.333 |

The vorpal row moved because its corpus is this working tree (edited between the A/B
and the pin) and the BM25 gate's verdict flipped with it — the self-index is a
convenience corpus, not a pinned one; kernel and cpython are the pinned truth. Net:
on the design-floor corpus the learned tier now beats the lexical tier (0.313 vs
0.299), and on the small corpora it matches or beats it (0.412 vs 0.137; 0.612 vs
0.571).

Encoder rows re-measured on the pinned learned base (the README's third column):

| corpus · pinned learned + encoder | f32 | f16 |
|---|---:|---:|
| kernel | 0.222 / 0.294 / 0.208 | 0.227 / 0.294 / 0.208 |
| cpython | 0.410 / 0.556 / 0.500 | 0.410 / 0.556 / 0.500 |
| vorpal | 0.622 / 0.625 / 0.650 | 0.622 / 0.625 / 0.650 |

The kernel verdict stands (encoder off there: 0.313 without vs 0.222 with — `vorpal
tune` will say OFF); on this repo the encoder adds recall (0.55 → 0.65) over the
pinned learned base. f16 vs f32: identical on two corpora, one adjacent-rank swap on
one kernel query (NDCG 0.222 vs 0.227) — the first cell-level difference observed;
consistent with the ≤1 % drift bar and the README now says "rank the same" rather
than "bit-for-bit".

### vs codebase-memory-mcp (same machine, same checkouts, same labels, same metrics)

cbm = /Users/adalundhe/Projects/codebase-memory-mcp at `997d087` (v0.10.8, "dev"
build of the production `-O2` target, 286 MB binary, 162 grammars, SQLite+FTS5, static
per-token nomic vectors — no inference at query time), scratch `CBM_CACHE_DIR`, `cli`
one-shot path (its documented scriptable mode; MCP mode starts a daemon + watchers),
`--mode full`, memory = peak RSS summed over the process tree (`psmax.py`, 50 ms
sampling; `time -l` sees only the 3 MB launcher — the work runs in a spawned child).

| corpus | tool | cold index | nodes / edges | peak RSS | on disk | no-change re-index |
|---|---|---:|---:|---:|---:|---:|
| kernel | vorpal | 8.2 s | 8.89 M / — | 5.6 GB | 7.6 GB | 0.12 s |
| kernel | cbm | 265.4 s | 8.53 M / 15.98 M | 70.3 GB | 15.9 GB | 12.5 s |
| cpython | vorpal | 1.0 s (HEAD) / 2.3 s (pin) | 162,813 | 0.82 GB | 200 MB | 0.02 s |
| cpython | cbm | 36.2 s | 136,118 / 867,563 | 6.6 GB | 663 MB | 5.1 s |
| vorpal repo | vorpal | 7.4 s | 78,894 | 12.2 GB | 836 MB | 0.02 s |
| vorpal repo | cbm | 44.8 s | 66,141 / 206,085 | 32.3 GB | 291 MB | 5.3 s |

Graded search over the same label sets (`cbm_grade.py`: identical NDCG@10 / MRR /
recall@5 math, hit = name equality [+ path suffix], cbm's `search_graph` modes;
`--semantic-query` takes a keyword ARRAY so NL queries are split on whitespace;
`--name-pattern` given as `.*w1.*w2.*` over the query words):

| corpus | cbm BM25 | cbm semantic | cbm name-regex | vorpal lexical | vorpal best tier |
|---|---:|---:|---:|---:|---:|
| kernel (8) | 0.116 / 0.104 / 0.167 | 0.000 | 0.000 | 0.299 / 0.375 / 0.229 | lexical |
| cpython (6) | 0.274 / 0.246 / 0.167 | 0.000 | 0.000 | 0.137 / 0.208 / 0.250 | 0.410 / 0.556 / 0.500 (learned + encoder) |
| vorpal (10) | 0.479 / 0.500 / 0.450 | 0.000 | 0.429 / 0.433 / 0.450 | 0.571 / 0.560 / 0.550 | 0.648 / 0.625 / 0.750 (learned + encoder) |

cbm's keyword-vector semantic mode returned e.g. svelte grammar functions at cosine
0.05 for "manifest stat" on this repo — the static-token bag composes to noise; it
never surfaced a labelled definition on any corpus. Its BM25 beats vorpal's LEXICAL
tier on cpython's descriptive class (the same gap the Stage-4 BM25 work measured and
that the learned tier closes), loses on the kernel (subword identifiers) and this repo.

One-shot latency (process start included — cbm's only scriptable path): cbm search
3.3–3.5 s/query (kernel BM25 5.5 s, name-regex 4.2 s), `trace_path` callers 3.3–4.4 s;
vorpal `search` 0.19–0.20 s kernel / 0.01 s cpython, `graph callers` 0.06 s (2.3 s on
the first touch of a cold edge segment); daemon: 59 ms / 0.1 ms.

## Doc-side dense channel — CodeRankEmbed as a candidate generator (2026-09-02, ENCODER_RESEARCH §8.2 option 2)

Two gated stages. **Stage A** lifts the encoder's doc-side throughput E; **Stage B**
pre-embeds definition surfaces at warm time into a stamp-gated sidecar (`ann.dense` +
`ann.dense.json`, int8 codes + per-row scale + f16 rows, keyed by node id) and fuses the
encoder's cosine ranking as a FIFTH RRF list, the query embedding computed once and
shared with the rerank. Machine: M5 Max (18 cores), Accelerate present; every latency and
warm-time row below was CONTENDED (a concurrent agent's kernel warm at 15 cores + this
gate's own test runs; `uptime` 10–39) unless marked quiet. Quality numbers are
deterministic (searcheval's double-run gate).

### Stage A — GEMM path (`GemmPath::{FixedOrder, Throughput}`, `crates/ann/src/encoder/forward.rs`)

`examples/sweep_encoder.rs` (bench-internals), real vorpal-index surfaces in coverage
order (name + signature + basename; 14–21 tok/seq), FLOPs = 2 × 113.2 M × tokens, median
of 3, quiet machine:

| batch | tokens | pre-change fixed-order | fixed-order (row-parallel passes) | Accelerate `cblas_sgemm` | speedup | min cosine | raw sgemm ceiling |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 26 | 364 | 0.356 s / 232 GFLOPS | 0.317 s / 260 | **0.088 s / 935 GFLOPS** (295 seq/s) | 3.6–4.0× | 1.000000 | 1128 GFLOPS |
| 256 | 4690 | 4.608 s / 230 | 3.882 s / 274 | **0.706 s / 1505** (363 seq/s) | 5.5–6.5× | 1.000000 | 1854 GFLOPS |
| 1024 | 21860 | 22.47 s / 220 | 20.33 s / 244 | **3.386 s / 1462** (302 seq/s) | 6.0–6.6× | 1.000000 | — |
| 4096 | 101959 | — | 97.06 s / 238 | **14.70 s / 1571** (279 seq/s) | 6.6× | 1.000000 | — |

The forward runs at 81–83% of the raw `cblas_sgemm` ceiling for these shapes (a C
microbench of the six GEMM shapes × 12 layers), so the remaining ~1/5 is the f64
attention / LayerNorm / SwiGLU passes. Determinism statement: the Accelerate path is
run-to-run reproducible and measured **bit-identical across rayon thread counts**
(1 vs 18) on the parity battery — Accelerate's own threading is a process-level setting
(`VECLIB_MAXIMUM_THREADS`), not exercised here — so it is admissible for the stamp-gated
sidecar; the query-side rerank stays on the fixed-order lanes (unchanged: the forced-off
configuration below reproduces the pre-change rankings exactly). Parity oracle:
cosine ≥ 0.9999 vs fixed-order (measured min 1.0000000 over the battery; gated tests
`throughput_path_*` in `crates/ann/tests/encoder.rs`). The research doc's E ≈ 0.08
was a whole-query figure (tokenization + reorder + the 26-sequence batch at 0.887 s);
the kernel-only pre-change rate on this machine is 0.23 TFLOPS, post-change 0.9–1.6.

### Stage B — the sidecar and the fused channel (`crates/index/src/dense.rs`)

Coverage rule (no constants): the caller passes a wall-clock budget; the build measures
this machine's seconds-per-TOKEN on the faster of its first two 256-surface batches and
covers the longest prefix of the in-degree order whose tokens fit the budget. A
per-DEFINITION rate was tried first and overran a 300 s budget by 43% on this repo
(the highest-in-degree names are the shortest surfaces: 14 tok at the head vs ~24 mean).

| corpus | population (non-Import) | budget | measured rate (contended) | covered | tokens | build | gate (label-free probes) | bytes | peak RSS |
|---|---:|---:|---:|---:|---:|---:|---|---:|---:|
| vorpal | 74,368 | 600 s | 3,320 tok/s | **74,368 (100%)** | 1.80 M | 390.6 s | OFF (wins 28 / losses 37, MRR 0.3935 vs 0.3999) | 172 MB | 1.32 GB |
| cpython | 145,367 | 1000 s | 3,601 tok/s | 131,203 (90.3%) | 3.60 M | 1,231.9 s (+23% over budget: contention rose after the probe) | OFF (wins 32 / losses 46, MRR 0.4414 vs 0.4442) | 304 MB | 1.54 GB |
| kernel | 8,481,757 | 300 s | 2,559 tok/s | 24,369 (0.3%) | 0.77 M | 188.5 s (probe ran under heavier load than the build) | OFF (wins 1 / losses 1 — a 0.3% hot subset rarely holds a probed node) | 56 MB | 6.17 GB (the graph itself) |

Gate cost: 512 fixed-order query forwards per corpus — 19–62 s on vorpal, 29 s on
cpython, 148 s on the kernel (all contended).

(Quiet-machine vorpal rate was 305.8 defs/s ≈ 5,500 tok/s at the head; contention halved it.)

Quality — vorpal (labels `xtask/labels/vorpal.json`, NDCG@10 / MRR / recall@5):

| configuration | conjunctive | descriptive | exact | paraphrase | short-kw | subset | **all** | mean µs (contended) |
|---|---|---|---|---|---|---|---|---:|
| learned tier, no encoder | 0.289/0.100/0.000 | 0.500/0.500/0.500 | 1/1/1 | 0/0/0 | 0.894/1.000/0.750 | 0.631/0.500/1.000 | 0.571/0.560/0.550 | 4,386 |
| + rerank, channel forced OFF (= pre-change binary, bit-identical) | 0.431/0.250/1.000 | 0.500/0.500/0.500 | 1/1/1 | 0/0/0 | 0.894/1.000/0.750 | 0.631/0.500/1.000 | 0.585/0.575/0.650 | 937,663 |
| + rerank, gate verdict (OFF) | same as above | | | | | | 0.585/0.575/0.650 | 939,074 |
| + rerank + channel ON | 0.431/0.250/1.000 | **0.815/0.750/1.000** | 1/1/1 | 0/0/0 | 0.894/1.000/0.750 | **1.000/1.000/1.000** | **0.685/0.675/0.750** | 1,089,899 |
| channel ON, no rerank | 0.356/0.167/0.000 | 0.500/0.528/0.500 | 1/1/1 | 0/0/0 | 0.894/1.000/0.750 | 1.000/1.000/1.000 | 0.614/0.622/0.550 | 138,565 |

Paraphrase, answered directly (`sweep_encoder --dense-rank`, full-corpus dense ranking):
"near duplicate code detection" → `similar_pairs` at dense rank **289** of 74,368,
`Sketch` 36,820; "who called what at runtime" → `ObservedStore` 6,669, `ingest_traces`
72,428. The dense top-10 for the first is junk surfaces (`&nearrow;`, six `description`
nodes). So on the rerank's surface recipe the channel does NOT surface the paraphrase
targets at the k=25 pool (100): the mechanism exists (289 of 74 K is the top 0.4%) but
the surface carries too little of the concept.

Quality — cpython (`xtask/labels/cpython.json`; 4 descriptive + 2 short-keyword):

| configuration | descriptive | short-keyword | **all** | mean µs (contended) |
|---|---|---|---|---:|
| learned tier, no encoder | 0.475/0.542/0.375 | 0.287/0.500/0.250 | 0.412/0.528/0.333 | 5,263 |
| + rerank, channel forced OFF (= pre-change, bit-identical) | 0.531/0.583/0.625 | 0.169/0.500/0.250 | 0.410/0.556/0.500 | 957,810 |
| + rerank, gate verdict (OFF) | same | same | 0.410/0.556/0.500 | 1,024,961 |
| + rerank + channel ON | 0.489/0.562/0.500 | **0.300/0.571/0.250** | 0.426/0.565/0.417 | 1,169,715 |
| channel ON, no rerank | 0.495/0.550/0.625 | 0.282/0.533/0.250 | 0.424/0.544/0.500 | 155,891 |

Candidate generation observed: "garbage collect run" → `gc_collect_main` enters the
fused top-25 ONLY through the dense list (dense#7; vector#56, no name/BM25 placement)
and lands fused#7 — the mechanism the reranker cannot supply. But descriptive loses
0.531 → 0.489 NDCG / 0.625 → 0.500 recall@5 with the channel + rerank (`PyList_Append`
fused#3 → #6, `PyArg_ParseTuple` #3 → #4: the fifth list's RRF mass reorders the tail
the reranker then arbitrates from), so cpython is MIXED under force-on and the gate's
OFF keeps it exactly at baseline.

Quality — Linux kernel (`xtask/labels/kernel.json`; 1 descriptive + 7 short-keyword;
the gate is all ≥ 0.313, the learned tier's no-encoder line):

| configuration | descriptive | short-keyword | **all** | mean µs (contended) |
|---|---|---|---|---:|
| learned tier, no encoder | 0.947/1.000/1.000 | 0.222/0.253/0.143 | 0.313/0.346/0.250 | 218,583 |
| + rerank, channel forced OFF (= pre-change, bit-identical) | 0.905/1.000/1.000 | 0.125/0.193/0.095 | 0.222/0.294/0.208 | 1,346,043 |
| + rerank, gate verdict (OFF) | same | same | 0.222/0.294/0.208 | 1,079,896 |
| + rerank + channel ON | 0.608/0.500/1.000 | **0.307/0.393/0.190** | **0.345/0.406/0.292** | 1,372,622 |
| channel ON, no rerank | 0.496/0.333/1.000 | **0.357/0.421/0.238** | **0.374/0.410/0.333** | 374,702 |

With only 24,369 of 8.48 M definitions covered (the in-degree head), the channel is the
ONLY placement for `alloc_skb` (dense#1 → fused#5), `request_irq` (dense#2 → fused#3)
and `mutex_lock` (dense#4 → fused#8) — three of the seven short-keyword answers the
research doc named as the subword-identifier failures — and it lifts `handle_mm_fault`
from fused#14 to #1. It costs the single descriptive query (`pick_next_task` #1 → #2/#3).
Both channel-on rows clear the 0.313 gate; the no-rerank row is the best all-NDCG
measured on this graph.

**Gate verdict vs labels — the finding that decides the shipping shape.** The label-free
self-probe gate (the BM25 protocol: name-token-subset known-item queries, MRR@10) voted
OFF on all three corpora, while the labelled harness measures the channel + rerank at
+0.100 (vorpal), +0.016 (cpython, mixed by class), +0.123 (kernel) all-NDCG. The probe
queries are lexical by construction and the sidecar's value is on descriptive and
subword-identifier queries the probes never ask — the same blind spot the BM25 gate
was recorded with ("label-free probes can't see the descriptive class"). So as landed the
channel SELF-GATES OFF everywhere (the shipping configuration is bit-identical to the
pre-change ranking; nothing regresses) and the labelled gains are reachable only through
the bench override. The decision — ship ON where a sidecar exists (labels: 3/3 corpora up
on all-NDCG, cpython descriptive down), a per-index `vorpal tune`-style verdict, or a
better label-free gate — is an owner decision recorded here, not taken unilaterally.

Latency (all contended, `uptime` 20–70; k=25, searcheval mean): the channel's own cost is
the no-rerank row minus the no-encoder row — ~135 ms on vorpal, ~150 ms on cpython,
~155 ms on the kernel — of which one fixed-order query forward (a single ~12-token
sequence at batch 1: 26 sequences take 88 ms on the throughput path but ~300 ms on the
fixed path under contention) is the bulk and the int8 scan + f16 rescore the rest
(74 K–131 K rows). With the rerank on, the rerank's own 26-sequence fixed-order batch
dominates (0.9–1.4 s contended vs 0.63–1.0 s in the pre-change baseline runs, which ran
under lighter load) — the "zero added latency" claim holds for the query EMBEDDING
(computed once, shared), not for the scan, which is the ~ms–100 ms figure above.

End-of-run re-measure (`uptime` 20–24 — the concurrent agent was still loading the
machine, so these are contended too; quality identical to the tables above):

| corpus | rerank, channel OFF (shipping = pre-change) | rerank + channel ON | channel ON, no rerank |
|---|---:|---:|---:|
| vorpal (74 K rows) | 607 ms | 720 ms | 98 ms |
| cpython (131 K rows) | 836 ms | 930 ms | 120 ms |
| kernel (24 K rows) | 1,135 ms | 919 ms | 118 ms |

Pre-change baseline runs (lighter load, `uptime` ~10): 625 / 687 / 1,015 ms. The
no-rerank column is the channel's whole per-query cost (one fixed-order query forward +
scan + rescore + the four-list fusion): ~100–120 ms contended.

Reproduction: `VORPAL_CODERANK_DIR=<model> cargo run --release -p vorpal-index
--features bench-internals --example sweep_encoder -- <index> 26 256 1024` (Stage A);
`vorpal-index __warm-ann <idx> --dense-budget-secs <N>` then `xtask searcheval` under
`VORPAL_DENSE_CHANNEL=off|on` and `VORPAL_RERANK_MODE=off` (Stage B);
`sweep_encoder <idx> --dense-rank <query> <name…>` for the paraphrase ranks;
`VORPAL_SEARCHEVAL_CHANNELS=1` prints per-channel provenance of labelled hits.
Gate: 142 suites / 1,292 tests green (`cargo test --workspace --release`), both clippy
lanes clean.

Not done / open: (1) the optional richer surface (leading doc comment / body head) was
not measured — the paraphrase ranks say the surface is the lever, and it would also
change the rerank's surface (recipe law: one recipe for sidecar and rerank), so it is a
separate A/B; (2) the sidecar is not carried across generations (no `ann.files`-style
reconciliation, unlike the ANN tier) — every content change rebuilds it in full, minutes
at these budgets; (3) the default budget policy when no budget is given is "no sidecar"
(the tier's own warm is 4–8 s on vorpal/cpython, far too small to derive a budget from);
(4) all Stage B wall-clock rows are contended — the token-rule's N is a machine-and-load
measurement, recorded in `ann.dense.json`; the uncontended vorpal head rate was 1.7×
the contended one.

## Cross-platform encoder throughput — x86 f32 kernels, int8 measured-and-rejected (2026-09-02)

The doc-side throughput lift above was macOS-only: off macOS `GemmPath::Throughput`
silently WAS the fixed lanes. This pass adds the missing rungs
(`crates/ann/src/encoder/{gemm_x86,gemm_i8,cache}.rs`; branch `xplat-cpu` on the
dense-channel seam `4d588c1`). What is local and what is not is stated per row:
the development machine is an M5 Max, where AVX2 is reachable only under Docker's
linux/amd64 emulation (correctness, never rate) and AVX-512 / VNNI are not reachable
at all — those rates come from the `encoder-x86` CI job (`.github/workflows/ci.yml`,
`ubuntu-latest`), and are PENDING until the coordinator's PR runs it.

### Platform matrix — what each `GemmPath` resolves to (runtime-detected, `label()`)

| OS / ISA | `Throughput` (doc-side sidecar) | `Int8` (opt-in API only) | `FixedOrder` (query-side rerank, every platform) |
|---|---|---|---|
| macOS, Apple silicon | Accelerate `cblas_sgemm`, row-sharded (`accelerate-sgemm`) | NEON `sdot` via inline asm (`int8-neon-sdot`) | eight fixed lanes |
| macOS, Intel | Accelerate `cblas_sgemm` | AVX-512-VNNI / AVX-VNNI / AVX2 `pmaddwd` by CPUID | eight fixed lanes |
| Linux / Windows, x86-64 with AVX-512F | owned AVX-512F 4×4 tiles, 16 lanes (`avx512f-sgemm`) | `int8-avx512-vnni` when VNNI is present, else `int8-avx-vnni` / `int8-avx2-madd` | eight fixed lanes |
| Linux / Windows, x86-64 with AVX2+FMA (no AVX-512) | owned AVX2+FMA 2×4 tiles, 8 lanes (`avx2-fma-sgemm`) — **bit-identical to the fixed lanes** | `int8-avx-vnni` (Alder Lake+, Zen 4+) else `int8-avx2-madd` | eight fixed lanes |
| x86-64 without FMA (pre-Haswell) | the fixed lanes (`fixed-order`) | `int8-portable` | eight fixed lanes |
| Linux aarch64 (no Accelerate) | the fixed lanes (`fixed-order`; auto-vectorized NEON fma) | `int8-neon-sdot` where `dotprod` is present (ARMv8.2+), else `int8-portable` | eight fixed lanes |
| anything else | the fixed lanes | `int8-portable` | eight fixed lanes |

Row-sharding is the Accelerate path's split on every rung (`throughput_shards()` =
`available_parallelism()`); the L2 panel size is DERIVED from the cache the platform
enumerates (x86 CPUID leaf 4 / 0x8000001D, macOS `hw.l2cachesize`, Linux sysfs; half the
L2 in whole tiles; no L2 figure → one tile per panel, i.e. no reuse assumed rather than a
guessed size). Tile shapes are sized to the register file (AVX2 2×4 = 11 of 16 ymm;
AVX-512 4×4 = 21 of 32 zmm; NEON int8 4×4 = 24 of 32 v-regs), not tuned.

### Determinism statements

* **AVX2+FMA f32**: the kernel is the fixed lanes' exact reduction structure (eight
  `fma` lanes over ascending blocks, `((l0+l4)+(l1+l5))+((l2+l6)+(l3+l7))`, ascending
  scalar tail), so on an AVX2 machine the `Throughput` GEMM reproduces the fixed-order
  GEMM **bit for bit** — asserted by the unit oracle on six tail shapes × four shard
  counts and shown by `gemm_bench` (Δ = 0.00e0 on all five shapes), both executed under
  linux/amd64 emulation below. The whole forward under `Throughput` is NOT bit-equal to
  `FixedOrder` on any platform, because the seam's throughput path also swaps the SwiGLU
  gate for the f32 `exp_fast` — a first draft of the gated test asserted the bits and
  failed on exactly that; the parity bound on every rung is the cosine oracle.
* **Cross-platform bits**: no path's whole forward is bit-identical ACROSS operating
  systems — the f64 attention softmax calls the platform libm's `exp`, which differs by an
  ulp between Apple's libm and glibc — so every determinism statement here is within one
  platform (the sidecar is stamp-gated per machine; the query-side law is per platform).
* **AVX-512F f32**: sixteen lanes, a fixed fold + the same eight-lane tree, per-element
  structure independent of tile / panel / shard → bit-identical across thread and shard
  counts BY CONSTRUCTION; differs from the fixed lanes by summation order only (parity
  oracle cosine ≥ 0.9999; the unit oracle bounds |Δ| ≤ 1e-5·dim_in per element). Not
  executed locally — CI.
* **int8 (every ISA)**: every kernel computes the same exact i32 sum, so the GEMM is
  bit-identical across ISAs, tiles, shards and thread counts (unit oracle: every present
  kernel vs an i64 reference, bit-equal on seven shapes × four shard counts; gated test:
  run-to-run and rayon-1-vs-18 bit-equal on the real encoder, on NEON and on emulated AVX2). The VNNI forms multiply
  u8×s8, so activations are sign-flipped (`+128`) and the driver subtracts `128·Σw` — the
  per-row sum recorded at quantization; exact.
* **Query side**: untouched — `FixedOrder` on every platform, rankings byte-identical
  (the fixed-path thread-stability assertion still passes).

### Local (M5 Max, Accelerate present) — what was measured here

`gemm_bench` (weights-free, the five per-layer GEMM shapes at the recorded 256-surface
batch, 4690 tokens, median of 3; **contended**: load average 18–22 from concurrent
sessions):

| GEMM | shape | fixed-order | Accelerate (`Throughput`) | int8 NEON `sdot` | Accelerate Δ | int8 Δ |
|---|---|---:|---:|---:|---:|---:|
| qkv | 4690 × 768 → 2304 | 0.0676 s / 245 GFLOPS | 0.0069 s / 2397 | 0.0073 s / 2275 | 1.4e-6 | 6.0e-3 |
| out_proj | 4690 × 768 → 768 | 0.0215 s / 258 | 0.0026 s / 2096 | 0.0023 s / 2431 | 1.3e-6 | 5.4e-3 |
| fc11 | 4690 × 768 → 3072 | 0.0914 s / 242 | 0.0098 s / 2250 | 0.0088 s / 2515 | 1.5e-6 | 5.8e-3 |
| fc12 | 4690 × 768 → 3072 | 0.0989 s / 224 | 0.0092 s / 2405 | 0.0086 s / 2580 | 1.5e-6 | 5.6e-3 |
| fc2 | 4690 × 3072 → 768 | 0.0945 s / 234 | 0.0105 s / 2109 | 0.0086 s / 2579 | 2.5e-6 | 5.6e-3 |
| per-layer sum (×12 = GEMM per forward) | | 0.374 s (237) → 4.49 s | 0.039 s (2264) → 0.47 s | 0.036 s (2491) → 0.43 s | | |

(Δ = max |out − fixed| / max |fixed| on random operands; the int8 Δ is the quantization
itself, exact per the unit oracle.) The NEON int8 GEMM is only 1.10× Accelerate's f32 on
this machine — the AMX units already run the f32 GEMM near the int8 dot rate, so int8 has
no throughput case on Apple silicon whatever its retention.

`sweep_encoder` on a fresh index of this repo (real surfaces in coverage order, 14–21
tok/seq; FLOPs = 2 × 113.2 M × tokens; median of 3; **contended**: load average 20–42
during the run — the seam's quiet-machine Accelerate rows were 935 / 1505 / 1462 GFLOPS):

| batch | tokens | fixed-order | Accelerate `Throughput` | speedup | min cosine | int8 NEON forward | int8 vs fixed | int8 vs Accelerate |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 26 | 364 | 0.389 s / 212 GFLOPS | **0.136 s / 607** (191 seq/s) | 2.9× | 1.000000 | 0.074 s / 1111 | 5.2× | 1.83× |
| 256 | 4690 | 4.679 s / 227 | **0.582 s / 1824** (440 seq/s) | 8.0× | 1.000000 | 0.485 s / 2190 | 9.6× | 1.20× |
| 1024 | 21814 | 26.00 s / 190 | **3.389 s / 1458** (302 seq/s) | 7.7× | 1.000000 | 3.455 s / 1430 | 7.5× | 0.98× |

The Accelerate path stays run-to-run reproducible and bit-identical across rayon thread
counts (1 vs 18: "IDENTICAL bytes"), parity min cosine 1.0000000 — unchanged by this pass
(the macOS dispatch is untouched). The whole-forward int8 advantage shrinks to nothing at
1024 because the f64 attention / LayerNorm passes, not the GEMMs, bound the large batch.

### x86 (correctness under emulation; rate PENDING CI)

Method: `docker run --platform linux/amd64 rust:1.98` on the M5 Max (Rosetta-backed
emulation, which exposes `avx2` + `fma` and nothing wider — 18 vCPUs), the worktree and
the weights directory mounted, `cargo test -p vorpal-ann`. Emulated timings are NOT
throughput and are not reported as such; only correctness lines are.

| what ran under emulated AVX2 | result |
|---|---|
| `gemm_x86` unit oracle (six shapes incl. tails × shard counts 0/1/3/64) | `avx2-fma-sgemm` == fixed lanes **bit-equal** |
| `gemm_i8` unit oracle (`Avx2Madd`, `Portable` present) | both **bit-equal** to the i64 reference on seven shapes × four shard counts |
| `cache` L2 probe | CPUID leaf enumerated (plausibility test passed) |
| `gemm_bench 512 1` | executed on the AVX2 rung; throughput Δ = **0.00e0** on all five shapes (int8 Δ 5.2–5.9e-3, the quantization itself) |
| gated real-encoder `throughput_path_reproducibility_is_stated` | `avx2-fma-sgemm` run-to-run reproducible; rayon 1 vs 18: **IDENTICAL bytes** |
| gated real-encoder `throughput_path_matches_fixed_order_within_cosine` | `avx2-fma-sgemm` vs fixed-order: **min cosine 1.0000000** over the 12-surface battery (bits differ only by the `exp_fast` gate) |
| gated real-encoder `int8_path_…` (`int8-avx2-madd`) | deterministic (run-to-run and rayon 1 vs 18 bit-equal); functional floor held; min cosine 0.962879 ("ObservedStore …"), mean 1−cos 2.70e-2 vs bar 2.89e-5 → FAILS (same verdict as NEON) |

The AVX-512F f32 kernel and the AVX-512-VNNI / AVX-VNNI int8 kernels compiled (native and
`--target x86_64-apple-darwin` clippy lanes, `-D warnings`) but were NOT executed anywhere
reachable from this machine — CI.

### int8 retention — the derived bar, the measurement, the verdict

Bar derivation: the research's published int8 datums (ENCODER_RESEARCH §6) are for int8
OUTPUT quantization of the embedding — 97 % MTEB-retrieval retention for the best 1024-d
model, 99 % with ×4 rescore (the sidecar's own recipe: int8 codes + f16 rescore). The
mapping used: retention is monotone in embedding perturbation, so an int8 FORWARD whose
mean angular deviation from the f32 embedding is no larger than the deviation int8 output
quantization imposes on the same embeddings (`dense::quantize_row` → dequantize, measured
on this model's own vectors) retains ≥ 97 % by the same evidence. That bar is computed per
run beside the measurement (gated test `int8_path_is_deterministic_and_reports_retention…`,
and the `sweep_encoder` int8 table on real surfaces).

Measured on the 12-surface parity battery (gated test) and on 26 / 256 / 1024 real
surfaces of this repo (sweep table's int8 columns), NEON `sdot` kernel, 108 MB of int8
weights built lazily at the first `Int8` embed:

| surfaces | int8 forward: min cosine vs f32 | mean 1−cos | output-int8 bar: min cosine | mean 1−cos | verdict |
|---:|---:|---:|---:|---:|---|
| 12 (battery), NEON `sdot` | 0.962879 ("ObservedStore pub struct …") | 2.70e-2 | 0.999960 | 2.89e-5 | FAILS (830× the bar) |
| 12 (battery), emulated AVX2 `pmaddwd` | 0.962879 ("ObservedStore pub struct …") | 2.70e-2 | 0.999960 | 2.89e-5 | FAILS |
| 26 | 0.970155 | 2.64e-2 | 0.999954 | 3.37e-5 | FAILS (780× the bar) |
| 256 | 0.963265 | 2.68e-2 | 0.999924 | 3.40e-5 | FAILS |
| 1024 | 0.957490 | 2.67e-2 | 0.999845 | 3.30e-5 | FAILS |

Determinism held (int8 bits equal run-to-run and at rayon 1 vs 18), and the functional
floor held (the int8 query embedding still ranks the factorial snippet above the unrelated
one) — the numerics are exact; the retention is not.

**Verdict: int8 FAILS the bar — and the f16 precedent floor (cosine 0.99) — so it stays
OFF by default: `GemmPath::Int8` is an opt-in API (nothing in `dense.rs` selects it), the
sidecar keeps `Throughput`.** The kernels are exact (unit oracles); the loss is the
numerics of naive W8A8 on a post-norm BERT — per-token max-abs activation scales are
dominated by outlier channels, so the bulk of each row quantizes coarsely across 12 layers.
Recorded lead, NOT taken (no throughput case on the only machine that can measure, and
the x86 VNNI case is pending CI): block-wise activation scales (one scale per 64-wide
block along `dim_in`, folded as one f32 fma per block per output) or SmoothQuant-style
per-channel migration into the weights; both need re-measuring against this bar.

### What is pending CI

* AVX-512F f32 rate and parity on real hardware; AVX-512-VNNI / AVX-VNNI int8 kernel
  execution (the unit oracles run on whatever ISA `ubuntu-latest` provides — Ice Lake
  Xeon runners have AVX-512 + VNNI, EPYC 7763 runners have AVX2 only; the job prints the
  runner's ISA first).
* Any x86 THROUGHPUT number: the `gemm_bench` step's log is the datum to copy here.
* The weights-gated real-encoder parity on x86 (`workflow_dispatch` only: it downloads the
  547 MB weights via `vorpal enable semantic-f32`).

## Chunked C parsing — measured, understood, and REJECTED (2026-09-02)

The premise: split giant C sources at proven top-level boundaries, parse slices in
parallel, merge products byte-identically (min 4 MiB, target 1 MiB, oracle-enforced).
The study, so nobody re-litigates this blind:

- **The mechanism works where the proof holds.** Forced-tiny-chunk oracles produced
  byte-identical products across a trap corpus and the vendored multi-MB generated
  `parser.c` files — after the oracle caught two real merge laws: the reference walk's
  per-file `(entity, name)` dedup for Type/Implements must replay across chunks, and
  interior parse errors (macro-specifier ERRORs) are tolerable exactly when they cannot
  reach a synthetic chunk edge (clean prefix ⇒ identical parser state; the final chunk's
  end is the real EOF).
- **Generated giants are ceiling-capped.** tree-sitter-julia's 54 MB `parser.c` has 74
  cut points and a 44 MB single declaration (80% max-share): chunked wall 3.63 s vs
  whole 3.65 s — flat, by Amdahl. The kernel's only >1 MB C files are `rtw89` tables —
  same shape. Solo wins exist only on cut-dense sources (synthetic 12×;
  tree-sitter-c's 3.8 MB parser.c 1.5×).
- **Hand-written giants defeat the proof.** sqlite3.c (9.4 MB, ~9,600 potential cuts):
  a lexical scanner cannot decide brace balance under the preprocessor. Alternatives
  double-open (`#ifdef X f(){ #else f(int){ #endif`), linkage braces close megabytes
  away in a different `#ifdef __cplusplus`, and function bodies close inside one
  branch (`setGetterMethod`: `#if SQLITE_MAX_MMAP_SIZE>0 } else …`). Branch-scoped
  depth snapshots, max-across-branches at `#endif`, and an extern-"C" idiom carve-out
  each recovered recall (0 → 9,630 cuts) and each was another epicycle: 22 opens still
  leaked, and every added rule weakened "proven boundary" toward "heuristic the oracle
  usually catches". Pipeline-wide, the pre-fix double-parse fallback made self-index
  10.7–12.6 s vs 7.6–7.9 s off — chunking every parser.c in an already-saturated
  18-worker build has no idle cores to exploit.

Verdict: rejected. The habitat is empty — files that are giant AND cut-dense AND
preprocessor-trivial barely exist. The correct mechanism for the real pain (repeated
re-parses of edited giants in long-lived processes) is tree-sitter's own incremental
reparse, which is sound by library contract — landed separately as the giant-file
tree cache.

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

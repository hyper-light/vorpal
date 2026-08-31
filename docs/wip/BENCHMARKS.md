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
~10¹² FLOPs (hours of CPU) — this encoder can never be the warm-time row
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
weights kept on disk so re-enabling is instant. Python (`semantic_install/semantic_enable/semantic_disable`) and Node
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

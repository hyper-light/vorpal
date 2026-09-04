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
target/release/xtask searcheval <idx> xtask/labels/<set>.json [--overlap] [--root <tree>]
```

(`--root` is required for the kernel set since 2026-09-03: labels whose `path` starts with
`/` are anchored at the tree root, the only way to tell `lib/rbtree.c` from its
`tools/lib/rbtree.c` mirror. Every set ships a `<set>.evidence.md` sidecar citing the
source line behind each grade-3 answer.)

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

### Surface-recipe A/B — is the candidate SURFACE the lever? (2026-09-02, owner directive)

`SurfaceRecipe` (`dense.rs`): (a) `head` = name + signature + basename; (b) `doc` = (a) +
the contiguous comment block above the span (comment family by extension; attribute /
decorator lines skipped; digest-verified indexed source, per-definition fallback to (a)
when absent — counted); (c) `body` = (b) + the span's first paragraph. One recipe for
sidecar AND rerank (label in `ann.dense.json`, demanded by freshness). Cap per surface:
the largest token matrix the Stage-A sweep validated (101,959 tokens at batch 4096) ÷
the 256-surface batch = **398 tokens** (truncations counted); the encoder's 2,048
`max_trained_positions` is the hard clamp above it. Quiet machine (`uptime` 5–10),
`VORPAL_DENSE_GATE=skip`, channel forced through the bench override.

vorpal (74,474 defs; budget 1,800 s; all three recipes reach 100% coverage):

| recipe | tok/def | fallbacks | truncations | warm | RSS | + rerank + channel ON | channel only | rerank only (recipe surfaces) | searcheval mean (ON) | uncached CLI k=10 / k=25 |
|---|---:|---:|---:|---:|---:|---|---|---|---:|---:|
| head | 24.3 | 0 | 0 | 283 s | 1.41 GB | **0.685 / 0.675 / 0.750** | 0.648 / 0.631 / 0.650 | 0.622 / 0.625 / 0.650 | 629 ms | 0.377 / 0.678 s |
| doc | 27.9 | **66,754 (89.6%)** | 13 | 330 s | 1.97 GB | 0.573 / 0.570 / 0.550 | 0.588 / 0.554 / 0.550 | 0.608 / 0.610 / 0.550 | 906 ms | 0.540 / 1.000 s |
| body | 86.6 | 2,291 | 2,970 | 1,356 s | 4.41 GB | 0.606 / 0.577 / 0.550 | 0.589 / 0.556 / 0.550 | 0.608 / 0.610 / 0.550 | 2,061 ms | 0.993 / 2.180 s |

(Sidecar bytes are recipe-independent: 172 MB at 74 K rows.) Paraphrase dense ranks under
`head`: `similar_pairs` 292, `Sketch` 36,910, `ObservedStore` 6,673, `ingest_traces`
72,530; the dense top-10 for "near duplicate code detection" DOES contain
`near_clones_are_paired_and_stated` (a test whose name carries the concept) — the
model matches concepts, the target's surface simply has none of the words. Under `body`
the targets move: `similar_pairs` 292 → **99**, `ObservedStore` 6,673 → **164**,
`ingest_traces` 72,530 → 69,590 (its body head is a signature + error plumbing, no
"runtime"), and the dense top-10 becomes `ObservedRecord`, `Observed`, `callers_of` —
concept-right neighbours of the labelled answer. So the surface IS the lever for the
paraphrase mechanism, but at 3.6× the tokens, 4.8× the warm and 3.3× the query latency
it buys rank 99 (still outside the k=25 pool of 100 after fusion), while the recipe's
noise costs every other class. Under `doc`: `similar_pairs` 304, `Sketch` 43,790,
`ObservedStore` **212** (its doc comment carries "observed"), `ingest_traces` 72,136 —
the comment helps exactly the definitions that have one (10% here) and changes nothing
for the rest.

cpython (145,367 defs; budget 2,400 s; quiet machine):

| recipe | tok/def | coverage | fallbacks | truncations | warm | RSS | bytes | + rerank + channel ON | channel only | rerank only | searcheval mean ON / OFF | uncached CLI k=10 / k=25 |
|---|---:|---:|---:|---:|---:|---:|---:|---|---|---|---:|---:|
| head | 27.6 | 100% | 0 | 0 | 628 s | 1.55 GB | 337 MB | 0.426 / 0.565 / 0.417 | 0.419 / 0.544 / 0.500 | 0.410 / 0.556 / 0.500 | 580 / 584 ms | 0.347 / 0.689 s |
| doc | 28.8 | 100% | **138,145 (95.0%)** | 8 | 657 s | 1.65 GB | 337 MB | 0.423 / 0.561 / 0.417 | 0.419 / 0.545 / 0.500 | 0.401 / 0.542 / 0.500 | 619 / 607 ms | 0.371 / 0.736 s |
| body | — | — | — | — | (killed at ~17 min by owner decision: recipe (c) already fails the no-added-latency constraint on vorpal — 3.3× query, 4.8× warm — and loses every class except paraphrase rank movement; its vorpal row is the record) | | | | | | | |

Linux kernel (8,481,757 defs; budget 300 s — hot subset; quiet machine):

| recipe | tok/def | covered | fallbacks | truncations | warm | + rerank + channel ON | channel only | rerank only |
|---|---:|---:|---:|---:|---:|---|---|---|
| doc | 58.5 | 27,310 (0.3%) | 19,565 (72%) | 449 | 306 s | 0.337 / 0.396 / 0.250 | **0.387 / 0.440 / 0.333** | 0.218 / 0.285 / 0.167 |
| head | 32.8 | 58,293 (0.7%) | 0 | 0 | 294 s | **0.335 / 0.382 / 0.250** | 0.312 / 0.348 / 0.229 | 0.222 / 0.294 / 0.208 |
| head (earlier, contended rate) | 27.8 | 24,369 (0.3%) | 0 | 0 | 189 s | 0.345 / 0.406 / 0.292 | 0.374 / 0.410 / 0.333 | 0.222 / 0.294 / 0.208 |

Kernel head: 135 MB, RSS 3.0 GB; searcheval mean ON 834 ms vs OFF 1,299 ms (the channel's
candidates shorten the rerank's cache-miss batch). The kernel is CONFOUNDED by coverage
size: the same head recipe scored 0.374 channel-only at 24 K rows and 0.312 at 58 K rows
— a larger hot subset adds more high-in-degree distractors to the dense top-100 than it
adds answers — so `doc`'s 0.387 at 27 K rows is a coverage-size effect, not recipe
evidence (its 72% fallback rate says the recipe barely differs there). Every kernel
channel-ON row clears the 0.313 gate; channel-only at 58 K sits 0.001 under it.

**Recipe (b) per class — where the trade actually happens.** Definitions WITH a leading
comment move toward concept queries (`ObservedStore` 6,673 → 212 on vorpal); every
definition WITHOUT one is unchanged in the dense list but the rerank now scores the
commented candidates on longer text, which dilutes name matches: vorpal `Postings`
(subset class) drops fused#1 → #8 under doc / #6 under body, and cpython's
`PyList_Append` / `PyArg_ParseTuple` slide the same way. Under the one-recipe law the
rerank pays the dilution on every query while the dense channel gains only on the
10 % (vorpal) / 5 % (cpython) / 28 % (kernel hot subset) of definitions that carry a
comment — a losing exchange at these comment densities.

**Two-field variant (rich surface for the dense channel only, rerank on head) — expected,
not run.** From the rows above: the rerank-only columns show the dilution cost is ~0.015–
0.05 all-NDCG (0.622 → 0.608 vorpal; 0.410 → 0.401 cpython) and the channel-only columns
show the doc/body dense lists are NOT better than head's as candidate sources (vorpal
0.648 head vs 0.588 doc / 0.589 body; cpython 0.419 vs 0.419) — the gain from the
richer dense list is confined to the paraphrase ranks (292 → 99 / 6,673 → 164), which
still sit outside the k=25 pool. So splitting the recipes would recover the rerank's
0.015–0.05 but not surface a paraphrase answer at k ≤ 25; it would need a deeper dense
pool (≥ 300 for `similar_pairs` at rank 99 under body's 4.8× warm, or a query-side
expansion) to pay off. Worth a follow-up only together with a pool-depth study; not on
its own. Kernel: the doc surface's 72 % fallback rate makes it moot there.

The quiet-machine latency answers the "zero added query latency" claim exactly: with the
rerank on, channel ON costs 580 ms vs OFF 584 ms (the query embedding is shared; the
131 K-row int8 scan + f16 rescore is inside the noise), and channel-only is 48 ms
(one fixed-order query forward + scan). PENDING: doc paraphrase ranks, kernel.

**Shipping shape (owner decision 2026-09-02, final): ALWAYS ON, filled in the
background.** No gate, no budget, no per-index verdict is needed: whenever an encoder is
selected, the warm (daemon warm thread / detached autowarm / `__warm-ann`) fills the
sidecar AFTER the core tiers commit, in coverage order over every definition something
references (referential in-degree ≥ 1, highest first), committing checkpoints by
geometric doubling (first after the two rate-probe batches, then whenever the rows added
match the rows committed — rewrite volume ≤ 2× the final file, no tunable interval).
A search never waits: it serves whatever checkpoint exists (a missing sidecar contributes
nothing) and picks up each new one on its next open. A later warm on the same
stamp / model / recipe RESUMES at the recorded coverage; a new generation rebuilds
(cross-generation carry stays a recorded lead). `--dense-budget-timeout <1h|5m30s|90s|secs>`
(and `<root>/dense.budget`) is an explicit cap on a round; `<root>/dense.channel = off` is
the opt-out; `vorpal tune` still reports the channel's paired verdict and writes the
override. The self-probe gate is gone. Recipe stays `head`.

`vorpal tune` verdicts (labels converted to `query => best-grade name` lines, k=10, head
sidecars at full coverage on vorpal/cpython and 58 K rows on the kernel):

| corpus | reranker (mean RR, W/L) | bm25 | dense channel |
|---|---|---|---|
| vorpal | 0.631 → 0.675, 2W/0L → ON | no signal | 0.633 → 0.675, 1W/1L → **ON** |
| cpython | 0.240 → 0.281, 2W/0L → ON | 0.221 → 0.281, 2W/0L → ON | 0.267 → 0.281, 1W/0L → **ON** |
| kernel | 0.365 → 0.364, 1W/3L → OFF | 0.364 → 0.289, 1W/3L → OFF | 0.351 → 0.364, 2W/3L → OFF (mean up, wins < losses) |

(Reciprocal rank of the single best-grade label at k=10 — a coarser instrument than the
graded NDCG tables; the kernel's dense row improves the mean but loses the wins count,
so tune's rule says OFF while NDCG@10 says +0.11. Evidence only under the always-on
decision below; `dense.channel = off` remains the user's opt-out.)

### The always-on background fill — referenced-definition stop rule (2026-09-02)

| corpus | definitions | referenced (in-degree ≥ 1) | fill (uncontended, pre-utilization-pass) | checkpoints | bytes | RSS | searcheval ON + rerank | vs full-coverage row |
|---|---:|---:|---:|---:|---:|---:|---|---|
| vorpal | 74,474 | **11,622 (15.6%)** | 47.3 s wall / 46.0 s embed, 6,465 tok/s, 25.6 tok/def | 6 | 26.9 MB | 0.98 GB | **0.705 / 0.700 / 0.750** | 0.685 / 0.675 / 0.750 (74 K rows) |
| cpython | 145,367 | **35,292 (24.3%)** | 137.0 s / 136.6 s, 6,555 tok/s, 25.4 tok/def | 8 | 81.7 MB | 1.10 GB | 0.396 / 0.423 / 0.333 | 0.426 / 0.565 / 0.417 (145 K rows) |
| kernel | 8,481,757 | **716,721 (8.5%)** | 10 m round: 111,872 rows, 6,407 tok/s, 34.2 tok/def → full referenced set ≈ 716,721 × 34.5 / 6,400 ≈ **64 min** (extrapolated, not run) | 9 (+1 in the 3 m resume round → 142,592 rows) | 259 MB (330 MB at 143 K) | 3.01 GB | 0.328 / 0.372 / 0.208 at 112 K rows; **0.308 / 0.365 / 0.208 at 143 K** | 0.345 (24 K) / 0.335 (58 K) |

The kernel does NOT hold the 0.313 gate as coverage grows: 24 K rows 0.345, 58 K 0.335,
112 K 0.328, 143 K 0.308 — each doubling of the hot subset adds more high-in-degree
distractors to the dense top-100 than answers, and the full referenced set (717 K) is
5× further along that curve. Holding the gate on the kernel needs a size-aware fusion
weight or a per-corpus coverage cap; the always-on rule as landed will pass 0.313 only
while the fill is young there. Recorded, not solved here.

Foreground searches issued ~25 s into each fill were served from the tiers + the latest
checkpoint: vorpal 0.411 s, cpython 0.373 s, kernel 0.670 s (process start + open +
query, k=10). The stop rule's cost shows on cpython: `list_append` (a static helper) and
`PyList_Append` resolve no referential in-edge in this graph, so the referenced-only
sidecar never embeds them and "append item to list" falls fused#1 → #7 / #3 → #10
(0.426 → 0.396 all-NDCG); on vorpal the subset REMOVES distractors (0.685 → 0.705). The
explicit cap and the full-coverage rows above remain the record of the alternative.

### Utilization pass — where the fill's wall goes, and the AMX ceiling (2026-09-02)

Owner observation: the fill ran at ~350% CPU on 18 cores. Wall-clock stage attribution
(`VORPAL_ENCODER_TRACE`, one 256-surface batch = 4,690 tokens, quiet machine):

| stage | shards = 1 (Accelerate's own threading) | shards = 18 (row-sharded `cblas_sgemm` on rayon) |
|---|---:|---:|
| six GEMMs × 12 layers | **0.584 s (88%)** | 0.478 s (87%) |
| attention (f64, row-parallel) | 0.030 s | 0.030 s |
| SwiGLU gate (throughput path: f32 `exp_fast`) | 0.013 s | 0.010 s |
| LayerNorm (row-parallel) | 0.012 s | 0.012 s |
| qkv unpack + rotary + residuals | 0.026 s | 0.022 s |
| forward | 0.664 s → 6,958 tok/s | **0.552 s → 8,509 tok/s** |

A `sample` profile showed `exp` as the top frame — a many-threads-in-short-phases
artifact: the wall clock says the non-GEMM stages are 12%. GEMM shard sweep (tokens/s
and cores busy for the whole process, `/usr/bin/time -l`):

| shards | tok/s | GFLOPS | cores busy |
|---:|---:|---:|---:|
| 1 | 6,917 | 1,567 | 2.6 |
| 2 | 7,074 | 1,602 | 3.5 |
| 4 | 6,910 | 1,565 | 5.2 |
| 8 | 8,274 | 1,874 | 8.6 |
| 18 | **8,512** | **1,928** | 13.8 |

Verdict: **AMX-bound.** 18 independent sgemm calls keep 13.8 cores busy for +23%
throughput, and 1.93 TFLOPS matches the raw single-call `cblas_sgemm` ceiling measured
in Stage A (1.85 TFLOPS at these shapes) — the matrix units, not the thread count, are
the limit; the extra cores mostly wait on them. The shard count is derived from
`available_parallelism()` (no constant) and pinned by this sweep. The fill's remaining
non-GEMM work is now pipelined (next batch's surfaces + tokenization on a scoped producer
thread during the current forward; int8/f16 quantization row-parallel) — worth ~2% at
these surface lengths, kept because it is what makes richer recipes free of I/O stalls.
Determinism statement after the pass: row shards cannot change a result (each output
row's reduction stays inside one `cblas_sgemm`); the throughput path's SwiGLU gate moved
from f64 libm to the deterministic f32 Cephes polynomial (`exp_fast`, ≈2e-7 relative
error), so its rows differ from the pre-pass sidecar bits while staying inside the parity
bound (gated oracle after the pass: min cosine 1.0000000; rayon 1 vs 18 threads
IDENTICAL bytes) and bit-identical across thread counts. The fixed-order query path is
untouched.

Fill rates before → after the pass (same referenced sets, same recipe; quiet machine):

| corpus | referenced rows | tok/s before → after | time to cover before → after | foreground search during fill | searcheval ON + rerank (after) |
|---|---:|---:|---:|---:|---|
| vorpal | 11,622 | 6,465 → **8,180** (+27%) | 47.3 s → **37.5 s** | 0.41 → 0.78 s (the fill now holds ~14 cores) | 0.705 / 0.700 / 0.750 (unchanged) |
| cpython | 35,292 | 6,555 → **8,402** (+28%) | 137.0 s → **106.9 s** | 0.37 → 0.73 s | 0.396 / 0.423 / 0.333 (unchanged) |
| kernel | 716,721 | 6,407 → **7,467** (10 m round; 8,075 in the 3 m resume round) (+17–26%) | 10 m round 111,872 → **129,280** rows; full set ≈ 64 → **≈ 51 min** (extrapolated) | 0.67 → 0.98 s | 0.308 / 0.365 / 0.208 at 129 K; 0.306 / 0.361 / 0.208 at 168 K |

Peak RSS during the fills: 1.02 / 1.15 / 3.15 GB. The kernel's checkpoint after the
pass is past the coverage where it last held 0.313 (112 K rows, 0.328) — the coverage
curve above, not the pass, decides that; with the always-on rule the kernel channel
needs the size-aware fusion weight or coverage cap recorded there. Cores busy during a
fill: ~14 of 18 (from 3.5), at the measured AMX ceiling.

### Stop rule extended: referenced OR exported (2026-09-02, final)

Owner decision after the cpython finding: eligible = referential in-degree ≥ 1 OR
`exported` (a structural flag the graph carries on every definition; no constant).
Order stays in-degree descending, id ascending — exported-but-unreferenced definitions
(degree 0) follow every referenced one, in id order.

Operational datum from the re-setup: with the channel always on, a plain `__warm-ann`
of the kernel (no cap) proceeds from the tier commit straight into the fill; the harness's
1 h task limit killed it ~55 min in at **checkpoint 9 = 262,144 rows**, and that checkpoint
stayed servable (the resume design under a kill). The daemon's warm thread and the
detached autowarm are where that hour belongs; `--dense-budget-timeout` / `dense.budget`
bound it where it does not.

| corpus | definitions | referenced | exported-only | eligible (rule) | fill (complete unless noted) | tok/s | bytes | RSS | searcheval ON + rerank | referenced-only row | full-coverage row |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|
| vorpal | 74,559 | 11,660 | 55,687 | **67,347 (90.3%)** | 206.6 s | 7,910 | 156 MB | 1.49 GB | 0.648 / 0.625 / 0.750 | 0.705 / 0.700 / 0.750 | 0.685 / 0.675 / 0.750 |
| cpython | 145,367 | 35,292 | 96,686 | **131,978 (90.8%)** | 481.9 s | 7,654 | 306 MB | 1.65 GB | 0.408 / 0.436 / 0.417 | 0.396 / 0.423 / 0.333 | 0.426 / 0.565 / 0.417 |
| kernel | 8,481,757 | 716,721 | 7,245,875 | **7,962,596 (93.9%)** | 10 m round → 143,104 rows; full ≈ 7.96 M × 35 / 8,400 ≈ **9.2 h** (extrapolated) | 8,327 (8,552 in the 3 m resume round → 183,296) | 331 MB at 143 K | 3.29 GB | 0.308 at 143 K; **0.223 / 0.243 / 0.167 at 183 K** | 0.308 (129 K) | 0.345 (24 K) |

Foreground searches during these fills: 1.07 / 0.79 / 1.11 s (served). What the rule did:
in these graphs `exported` is nearly everything — every non-`static` C symbol on the
kernel (94% eligible), every `pub` item plus the vendored grammar tables here (90%),
90.8% of cpython — so "referenced OR exported" is, on two of three corpora, the
full-coverage regime with its distractors back: vorpal 0.705 → 0.648, the kernel's
183 K-row checkpoint 0.223 (below the no-encoder 0.313 by a wide margin, on the same
coverage curve recorded above). cpython does recover the case that motivated the change
(`PyList_Append` is exported: dense#8, fused#10 → #7; `list_append` is a `static` helper
and stays out), 0.396 → 0.408 — still under its 0.426 full-coverage row. Net: the
exported flag is too coarse to be the stop rule's second clause as these graphs set it;
a stricter notion (exported AND named from outside the file / at API depth) or the
referenced-only rule with a per-corpus cap is the follow-up — recorded, not taken.

DISPOSITION (coordinator, on this evidence): **`CoverageRule::Referenced` is PINNED**
as the always-on stop rule — the measured-best regime (vorpal 0.705, cpython 0.396,
kernel 0.308 @ 129 K); `ReferencedOrExported` stays reachable through
`VORPAL_DENSE_COVERAGE` under `bench-internals` as the measured seam. The rule label
now rides in `ann.dense.json` (`coverage_rule`; pre-label records infer it from their
`exported_only` count), and a resume under a different rule starts over. Open leads
unchanged: a size-aware fusion weight or per-corpus coverage cap for the kernel's
coverage/quality slide, and a stricter "exported" notion for API entry points.

Reproduction: `VORPAL_CODERANK_DIR=<model> cargo run --release -p vorpal-index
--features bench-internals --example sweep_encoder -- <index> 26 256 1024` (Stage A);
`vorpal-index __warm-ann <idx>` (always-on fill; `--dense-budget-timeout 10m` caps a
round) then `xtask searcheval` under `VORPAL_DENSE_CHANNEL=off|on` and
`VORPAL_RERANK_MODE=off` (Stage B); `sweep_encoder <idx> --shards <k>` with
`VORPAL_ENCODER_TRACE=1` for the utilization pass;
`sweep_encoder <idx> --dense-rank <query> <name…>` for the paraphrase ranks;
`VORPAL_SEARCHEVAL_CHANNELS=1` prints per-channel provenance of labelled hits.
Gate (final, after the utilization pass): 142 suites / 1,299 tests green
(`cargo test --workspace --release`), both clippy lanes clean, the gated encoder oracles
re-verified on the final literals (min cosine 1.0000000; 1 vs 18 threads IDENTICAL).

Not done / open: (1) the optional richer surface (leading doc comment / body head) was
not measured — the paraphrase ranks say the surface is the lever, and it would also
change the rerank's surface (recipe law: one recipe for sidecar and rerank), so it is a
separate A/B; (2) the sidecar is not carried across generations (no `ann.files`-style
reconciliation, unlike the ANN tier) — every content change rebuilds it in full, minutes
at these budgets — superseded for same-generation warms by the resumable checkpoint fill
below, still open across generations; (3) the budget requirement is gone (always-on
fill with the referenced-definition stop rule, below); (4) the early Stage B wall-clock
rows are contended — the uncontended rates are in the fill tables below; (5) the kernel's
coverage/quality curve (0.345 at 24 K rows → 0.306 at 168 K) is unsolved: the always-on
channel holds the 0.313 gate there only while the fill is young.

## Cross-platform GPU rung for the doc-side fill — `wgpu` compute GEMM (2026-09-02)

ONE GPU path for the doc-side encoder fill — NVIDIA / AMD / Intel / Apple through
`wgpu` compute shaders over Metal / Vulkan / DX12, no vendor SDK, no runtime dependency
beyond the OS driver, inside the single binary. `GemmPath::Gpu` (`crates/ann/src/encoder/
gemm_wgpu.rs` + `gemm_nt.wgsl` + `swiglu.wgsl`), chosen by the doc-side ladder
`CodeEncoder::doc_side_rung()` ONCE per model open: **GPU → Accelerate (macOS) / the CPU
throughput path → fixed-order lanes**. Doc-side ONLY: the query-side rerank stays on the
fixed-order lanes everywhere (rankings byte-identical to before). Dependency: `wgpu
=30.0.0` (MIT OR Apache-2.0, rust-version 1.87; 30.0.1 raised its wasm-bindgen floor past
`crates/wasm`'s `=0.2.126` pin); `gpu-allocator` re-resolved onto `windows 0.62` so the
DX12 backend compiles against one `windows` crate (the workspace also carries 0.59 for
`pageant`). Machine: M5 Max (18 CPU cores, 40-core GPU, Metal 4). **Every row below was
CONTENDED** (`uptime` load 30–95 throughout: a concurrent agent's builds and benches plus
this branch's own builds); the GPU-only figures are the steadier ones, the CPU-side
figures vary 2× run to run — the tables say which run each comes from.

### Platform / device matrix

| platform | backend | adapter (`adapter.get_info()`) | status |
|---|---|---|---|
| macOS arm64 (this machine) | Metal | `wgpu-metal:Apple M5 Max` (IntegratedGpu) | **measured**: GEMM oracle, parity, determinism, throughput, fill, %CPU/%GPU, RSS |
| Linux x86_64 (Debian bookworm container, amd64 under emulation) | Vulkan | `wgpu-vulkan:llvmpipe (LLVM 15.0.6, 256 bits)` (Cpu, admitted by `VORPAL_ENCODER_GPU=software`) | **software-Vulkan correctness only**: ragged-shape GEMM oracle + gated parity (min cosine 1.0000000 vs fixed-order, 12 surfaces) + determinism (run-to-run, second device open, rayon 1 vs default: IDENTICAL bytes) — run twice, on the un-fused kernel (337 s for the two oracles) and on the shipped fused-MLP kernel (168 s); never a throughput figure |
| Linux x86_64, hardware Vulkan | Vulkan | — | compile-verified (`cargo check --target x86_64-unknown-linux-gnu -p vorpal-ann`); pending CI / hardware |
| Windows x86_64 | DX12 | — | compile-verified (`cargo check --target x86_64-pc-windows-msvc -p vorpal-ann`); pending CI / hardware |

Device selection: discrete > integrated > virtual > other by device type; software
adapters (`DeviceType::Cpu`) only under `VORPAL_ENCODER_GPU=software`; `=off` skips the
rung (stated in the record). No adapter, a limit too small for the shapes, `dim % 4 ≠ 0`,
a non-resident weight: typed refusals, never panics — `wgpu`'s default uncaptured-error
handler (which aborts) is replaced by a recording handler + device-lost callback, and every
submission runs under validation / OOM / internal error scopes; any fault RETIRES the rung
and that GEMM plus every later one runs on the next rung, the sidecar record naming the
chain (`wgpu-metal:Apple M5 Max→accelerate-sgemm (<fault>)`). Weights resident once per
open (453.0 MB, 60 buffers); activations round-trip per GEMM through scratch buffers grown
to the largest batch; the MLP block (fc11, fc12, SwiGLU gate, fc2) is FUSED on the device
so its `rows × 3072` intermediates never cross the host boundary.

### Parity and determinism (gated `gpu_path_*` oracles, `crates/ann/tests/encoder.rs`)

| device | vs fixed-order (min cosine, 12 surfaces) | vs Accelerate | run-to-run | 2nd device open | rayon 1 vs default |
|---|---:|---:|---|---|---|
| Metal, Apple M5 Max (fused MLP, device `exp` gate) | **1.0000000** | 1.0000000 | IDENTICAL (asserted) | IDENTICAL | IDENTICAL |
| Vulkan, llvmpipe (container) | **1.0000000** | — (fixed lanes) | IDENTICAL (asserted) | IDENTICAL | IDENTICAL |

Determinism story, stated honestly: the shader fixes each output's summation order
(k ascending, x/y/z/w lanes through `fma`), so two dispatches of the same compiled
pipeline agree bitwise — measured so on both devices, including across a fresh pipeline
compile in the same process. A different driver, device or `wgpu` release may compile a
different order (Metal compiles with its fast-math default), so the GPU rows are
reproducible per (device, driver) and the sidecar admits that variance (stamp-gated, never
part of the generation id); `ann.dense.json` records `gemm_path` = the rung — re-read at
every checkpoint, so a mid-fill retirement is recorded too.

### GEMM kernel — three versions, dispatch-only (no host copies)

`examples/gpu_gemm_probe.rs` (`-p vorpal-ann`, synthetic operands at the six layer
shapes) and `sweep_encoder`'s dispatch-only line (real weights):

| kernel | 21,853 rows, layer GEMMs | note |
|---|---:|---|
| v1: runtime-indexed `array<array<f32,4>,4>` accumulators, k-major tiles | 634 GFLOPS | naga bounds-checks every accumulator access; strided tile reads |
| v2: four named `vec4` accumulators, A k-major, B transposed to `[k lane][column quad]` (broadcast reads) | **9,123 GFLOPS** (real weights, 64×64 / bk4 8) | 14× v1; the shipped kernel |
| v2 under the contended final run | 4,764 GFLOPS | same binary, GPU sharing the package power budget with a loaded CPU |

GPU tile sweep (`gpu_gemm_probe`, layer GFLOPS = the five shapes of one layer at once,
mean of 20 submissions; the caps in `Tile::derive` come from this table):

| tile (block, K stage in vec4) | 364 rows | 4,690 rows | 21,853 rows |
|---|---:|---:|---:|
| 32×32, 2 | 3,253 | 6,283 | 4,597 |
| 32×32, 4 | 3,260 | **6,533** | 4,303 |
| 32×32, 8 | **4,929** | 6,271 | 4,992 |
| 32×32, 16 | 3,257 | 4,891 | 4,203 |
| 64×64, 2 | 3,679 | 5,418 | 4,858 |
| 64×64, 4 | 3,176 | 5,773 | 5,675 |
| **64×64, 8 (derived)** | 4,541 | 5,183 | **6,007** |
| 64×64, 16 | 952 | 1,263 | 1,439 |
| 128×128, 2 | 2,791 | 5,580 | 4,298 |
| 128×128, 4 | 2,747 | 6,434 | 5,139 |
| 128×128, 8 | 1,443 | 2,796 | 3,465 |
| 128×128, 16 | refused (64 KiB > the 32 KiB workgroup-memory limit) | | |

Derivation (no magic numbers): the largest square workgroup the invocation and per-axis
limits admit, capped at 16 × 16 (= 64 × 64 block: best at the fill's 21,853-row batches,
within noise of the best at the others); the deepest K stage the workgroup-memory limit
holds, capped at 8 vec4 (16 KiB staged) — 16 vec4 at 64 × 64 (32 KiB, one workgroup per
core) collapses to 1.4 TFLOPS at every scale, the occupancy cliff the cap avoids.

### End-to-end forward — GPU rung vs Accelerate vs fixed-order (`sweep_encoder`)

Real vorpal-index surfaces in coverage order, FLOPs = 2 × 113.2 M × tokens, median of 3,
contended (`uptime` 52–57 during this run; the quiet-machine Accelerate baseline recorded
above was 935 / 1,505 / 1,462 GFLOPS — this run's Accelerate column is 2–12× below it):

| batch | tokens | fixed-order | Accelerate `cblas_sgemm` | **GPU rung (fused MLP)** | GPU vs Accel | host copies (share of GPU wall) | device s (compute + blit) |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 26 | 364 | 2.168 s / 38 GFLOPS | 1.101 s / 75 | **0.437 s / 189** (59 seq/s) | 2.5× | 2.9% (40 + 67 MB) | 0.116 |
| 256 | 4,690 | 13.36 s / 80 | 3.099 s / 343 | **2.285 s / 465** (112 seq/s) | 1.4× | 4.5% (519 + 864 MB) | 0.299 |
| 1,024 | 21,853 | 32.99 s / 150 | 4.137 s / 1,197 | **2.011 s / 2,461** (509 seq/s) | 2.1× | 22.0% (2,417 + 4,028 MB) | 0.756 |

Same sweep one run earlier, v2 kernel WITHOUT the MLP fusion (contended, load 35–95):
GPU 0.391 / 1.323 / 4.084 s with host copies at 7.8 / 28.5 / 32.1% (107 + 174 /
1,383 + 2,248 / 6,445 + 10,473 MB per batch) — fusion cut the bytes 2.6× and the
1,024-batch wall 2×. And v1 (the 634-GFLOPS kernel): 0.257 / 2.253 / 11.34 s.

**Transfer share and where the time went.** With the fused MLP only `x` goes up and the
block's output comes down: per 4,690-token batch 519 MB up + 864 MB down (qkv's 3·dim
output is the largest remaining readback), 4.5% of the wall as host-side copies, 13% as
device time (GEMMs + blits: pure GEMM is 12 × 0.087 s ≈ 1.0 s at 21,853 rows under
contention, 0.54 s quiet), and the remaining ~80% is the CPU-side passes the GEMM path
never touched — attention (f64, per row/head), LayerNorm, rotary, residual adds, the qkv
unpack — which are now the dominant term of the GPU forward and the obvious next lever
(attention + LayerNorm on the device would leave one upload and one readback per layer).

### The fill (`vorpal __warm-ann`, this repo; `/usr/bin/time -l` + 1 s samples of `ps %cpu` and IOAccelerator "Device Utilization %")

Referenced population 11,701 of 74,652 definitions, 296,683 tokens, six checkpoints,
complete; same index, sidecar deleted between runs; contended (`uptime` 30–50):

| rung (`gemm_path` in `ann.dense.json`) | fill wall | tok/s (round) | fastest batch tok/s | mean %CPU (max) | mean GPU util (max) | peak RSS |
|---|---:|---:|---:|---:|---:|---:|
| `wgpu-metal:Apple M5 Max` | **41.0 s** | **7,240** | 8,233 | 190% (289%) | 38% (50%) | 1.02 GB |
| `accelerate-sgemm (gpu: disabled by VORPAL_ENCODER_GPU=off)` | 74.9 s | 3,962 | 4,257 | 638% (731%) | 5% (19%) | 1.06 GB |

1.83× the fill rate at 3.4× less CPU (the machine stays usable for the concurrent index
work the daemon is doing); GPU utilisation 38% says the device idles while the CPU passes
run — the pipelining lead (overlap batch i+1's GEMMs with batch i's attention) is the
other half of the lever above.

Kernel checkpoint round (`--dense-budget-timeout 120s`; referenced population 716,721 of
8,481,757 definitions; seven checkpoints each; contended, `uptime` 30–46):

| rung | rows covered in the round | tokens | fill s | tok/s (round) | fastest batch tok/s | process wall / %CPU / GPU util / peak RSS |
|---|---:|---:|---:|---:|---:|---|
| `wgpu-metal:Apple M5 Max` | **30,464** | **967,556** | 118.8 | **8,142** | 9,768 | 325.8 s / 501% mean (1473% max) / 22% mean (65% max) / 8.47 GB — this run ALSO built the kernel's ANN tier first (the graph + ANN warm own the RSS and most of the CPU), so its process-level columns are not fill-only |
| `accelerate-sgemm (…VORPAL_ENCODER_GPU=off)` | 16,640 | 517,005 | 120.5 | 4,290 | 4,574 | 121.8 s / 727% (833%) / 7% (52%) / 2.78 GB — ANN tier already warm, so this IS the fill's own profile |

1.9× the tokens per capped round on the kernel head (its surfaces are the shortest —
31.8 tok/def here vs 25.4 on this repo — so the GPU's per-batch fixed costs weigh more).
The record's `gemm_path` names the rung in both cases.

Reproduction: `VORPAL_CODERANK_DIR=<model> cargo run --release -p vorpal-index --features
bench-internals --example sweep_encoder -- <index> 26 256 1024` (throughput + transfer
share + dispatch-only ceiling; `--gpu-tiles [batch]` for the end-to-end tile sweep);
`cargo run --release -p vorpal-ann --example gpu_gemm_probe -- [rows]` (model-free kernel
sweep); `VORPAL_CODERANK_DIR=<model> cargo test -p vorpal-ann --release --test encoder
gpu_path -- --nocapture` (parity + determinism; `VORPAL_ENCODER_GPU=software` admits
lavapipe); `VORPAL_HOME=<home> vorpal __warm-ann <index> [--dense-budget-timeout 120s]`
under `/usr/bin/time -l` for the fill. Gate: 142 suites / 1,305 tests green (`cargo test
--workspace --release`), both clippy lanes clean under rust 1.98 (which also flagged five
pre-existing findings — the verbatim Cephes `exp` coefficients and the bench-only
`SurfaceRecipe` / `RerankMode` / `TrainKindPolicy` variants — now carrying stated allows).

Not done / open: (1) Vulkan and DX12 on hardware are compile-verified only — the parity
oracle needs a CI runner or a machine with such an adapter (the software-Vulkan run is the
correctness evidence for the Vulkan backend's shader path); (2) the CPU-side passes now
dominate the GPU forward (above) — attention/LayerNorm on the device and batch pipelining
are the next levers, not the GEMM; (3) `MAPPABLE_PRIMARY_BUFFERS` (direct mapping on
unified-memory devices, skipping the staging blit) was not tried — host copies are 4.5%
of the fill batch after fusion, below the noise of this contended machine; (4) every
CPU-side number here is contended — re-measure the throughput table on a quiet machine
before quoting it as the platform's rate.

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

### First CI execution (2026-09-03, run 33706453902 on `004a750`, `ubuntu-latest`)

Runner: AMD EPYC 9V74, 4 vCPUs, ISA **avx2 fma** only — so the AVX2+FMA lane ran and
the AVX-512F / VNNI kernels stayed compile-verified (no AVX-512 on this runner class;
still pending hardware). Kernel oracles: 13 tests pass. `gemm_bench`, 4,690 tokens
(the 256-surface batch), median of 3, 4 shards / 4 rayon threads:

| GEMM | shape | fixed-order lanes | AVX2+FMA throughput | int8 (avx2-madd) | throughput Δ |
|---|---|---:|---:|---:|---:|
| qkv | 4690 × 768 → 2304 | 6.83 s / 2 GFLOPS | 0.155 s / **107 GFLOPS** | 0.097 s / 172 | 0.00e0 |
| out_proj | 4690 × 768 → 768 | 2.27 s / 2 | 0.051 s / **108** | 0.036 s / 154 | 0.00e0 |
| fc11 | 4690 × 768 → 3072 | 9.11 s / 2 | 0.203 s / **109** | 0.127 s / 174 | 0.00e0 |
| fc12 | 4690 × 768 → 3072 | 9.23 s / 2 | 0.200 s / **111** | 0.127 s / 175 | 0.00e0 |
| fc2 | 4690 × 3072 → 768 | 9.09 s / 2 | 0.196 s / **113** | 0.134 s / 166 | 0.00e0 |
| per-layer sum (×12 layers per forward) | | 36.5 s (438 s/forward) | **0.81 s (9.7 s/forward)** | 0.52 s (6.2 s/forward) | |

Reading: on x86 the pre-lift fixed-order lanes ran at **2 GFLOPS** — the doc-side fill
was effectively unusable there (438 s of GEMM per 256-surface batch); the AVX2+FMA
kernels are **≈45×** that, bit-equal to the fixed lanes (Δ 0.00e0), at ~485 tokens/s
of GEMM-bound forward on four vCPUs. int8 (avx2-madd) is a further 1.55× but stays
OFF by the retention verdict above. AVX-512 remains the open measurement.

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

## Graded label sets expanded to ~50 queries per corpus (2026-09-03)

Every pinning decision above moved on one query per class, so the three sets were
expanded from 8 / 6 / 10 to **54 / 54 / 55** (kernel / cpython / vorpal), class-balanced,
with every original query kept verbatim. Each set now ships a sidecar
`xtask/labels/<set>.evidence.md` citing the source line (kerneldoc, `Doc/c-api`,
docstring, `///` doc, or body) that proves each grade-3 answer — grading is from the
source, never from what search returns; paraphrase queries share no token with the
identifier or its file, descriptive queries at most one. Two harness additions:
`--root <tree>` and anchored label paths (`"path": "/lib/rbtree.c"` = tree-relative
equality; a plain suffix stays `ends_with`), because the kernel mirrors `lib/rbtree.c`,
`include/linux/slab.h`, `include/linux/spinlock.h` under `tools/` and no suffix can
single the tree copy out. Unit tests cover the path rule, the greedy matcher, NDCG and
`validate` (`cargo test -p xtask`).

| corpus | exact | short-kw | subset | descriptive | paraphrase | conjunctive | total |
|---|---:|---:|---:|---:|---:|---:|---:|
| kernel `1590cf032971` | 8 | 13 | 7 | 13 | 9 | 4 | **54** |
| cpython `b86a41cbf63` | 8 | 13 | 6 | 13 | 9 | 5 | **54** |
| vorpal v0.7.0 | 8 | 12 | 7 | 13 | 10 | 5 | **55** |

Candidates dropped because the symbol is in source but NOT in the index (the existence
gate would fail the run): kernel `hrtimer_interrupt` (kernel/time/hrtimer.c:2185),
`hrtimer_start_range_ns` (:1493), `netif_receive_skb` / `__netif_receive_skb`
(net/core/dev.c:6464 / :6305), `kzalloc_noprof` (include/linux/slab.h:1292); cpython
`PyObject_GenericGetAttr` (Objects/object.c:2010) and `PyCallable_Check` (:2178).
The cpython pair is an extraction gap, not a label problem: `Objects/object.c` is
indexed only through `PyObject_Hash` (:1158) — `PyObject_GetAttr` (:1310),
`PyObject_SetAttr` (:1508), `PyObject_IsTrue` (:2138), `PyObject_Not` (:2166) and
everything after are absent, while `vorpal-index health` reports the file clean.
Open lead for the extraction owner. Also recorded, not changed: the original
cpython `list_append` label (no `path`) is satisfied by the clinic wrapper and a
`Modules/_testlimitedcapi/list.c` helper, and `gc_collect_main` / `PyGC_Collect`
exist in both `gc.c` and `gc_free_threading.c`.

Substrate: fresh `--semantic-tier lexical` and `learned` indexes per corpus (kernel
8,890,840 nodes, cpython 162,813, vorpal 79,431), `__warm-ann`'d, `VORPAL_NO_AUTOWARM=1`;
encoder = `vorpal enable semantic-f32` into a scratch `VORPAL_HOME` (no dense sidecar —
searcheval prints "dense sidecar: none", so "+ encoder" is the rerank only, the README's
configuration). Old-set rows reproduce the README pins bit-for-bit on cpython and vorpal
(cpython 0.137 / 0.412 / 0.410; vorpal 0.572 / 0.572 / 0.585 all-NDCG) and on kernel
lexical (0.299); the kernel learned rows are recorded below as measured on this build.

### Before/after, NDCG@10 / MRR / recall@5

vorpal (self-index):

| configuration | set | conjunctive | descriptive | exact | paraphrase | short-kw | subset | **all** |
|---|---|---|---|---|---|---|---|---|
| lexical | old (10) | 0/0/0 | 0.651/0.556/0.500 | 1/1/1 | 0/0/0 | 0.894/1.000/0.750 | 0.631/0.500/1.000 | 0.572/0.561/0.550 |
| lexical | new (55) | 0.067/0.208/0.100 | 0.129/0.099/0.077 | 0.990/1.000/1.000 | 0/0/0 | 0.590/0.568/0.542 | 0.710/0.648/0.857 | **0.400/0.394/0.400** |
| learned | old (10) | 0.301/0.111/0 | 0.500/0.500/0.500 | 0.815/0.750/1.000 | 0/0/0 | 0.894/1.000/0.750 | 1/1/1 | 0.572/0.561/0.550 |
| learned | new (55) | 0.381/0.529/0.600 | 0.173/0.148/0.192 | 0.944/0.938/1.000 | 0/0/0 | 0.643/0.618/0.792 | 0.763/0.695/0.929 | **0.450/0.443/0.536** |
| learned + f32 | old (10) | 0.431/0.250/1.000 | 0.500/0.500/0.500 | 0.815/0.750/1.000 | 0/0/0 | 0.894/1.000/0.750 | 1/1/1 | 0.585/0.575/0.650 |
| learned + f32 | new (55) | 0.442/0.525/0.700 | 0.191/0.170/0.192 | 0.940/0.938/1.000 | 0/0/0 | 0.664/0.653/0.750 | 0.743/0.676/0.857 | **0.461/0.453/0.527** |

cpython:

| configuration | set | conjunctive | descriptive | exact | paraphrase | short-kw | subset | **all** |
|---|---|---|---|---|---|---|---|---|
| lexical | old (6) | — | 0.036/0.062/0.125 | — | — | 0.338/0.500/0.500 | — | 0.137/0.208/0.250 |
| lexical | new (54) | 0.119/0.100/0.200 | 0.088/0.096/0.115 | 0.938/0.917/1.000 | 0/0/0 | 0.365/0.332/0.423 | 0.435/0.396/0.333 | **0.307/0.292/0.333** |
| learned | old (6) | — | 0.475/0.542/0.375 | — | — | 0.287/0.500/0.250 | — | 0.412/0.528/0.333 |
| learned | new (54) | 0.157/0.133/0.200 | 0.195/0.205/0.192 | 0.954/0.938/1.000 | 0/0/0 | 0.386/0.310/0.538 | 0.401/0.400/0.417 | **0.340/0.320/0.389** |
| learned + f32 | old (6) | — | 0.531/0.583/0.625 | — | — | 0.169/0.500/0.250 | — | 0.410/0.556/0.500 |
| learned + f32 | new (54) | 0.135/0.100/0.200 | 0.212/0.218/0.269 | 0.938/0.917/1.000 | 0/0/0 | 0.388/0.339/0.577 | 0.488/0.458/0.500 | **0.350/0.330/0.426** |

Linux kernel (`--root /path/to/linux`):

| configuration | set | conjunctive | descriptive | exact | paraphrase | short-kw | subset | **all** |
|---|---|---|---|---|---|---|---|---|
| lexical | old (8) | — | 0.947/1.000/1.000 | — | — | 0.206/0.286/0.119 | — | 0.299/0.375/0.229 |
| lexical | new (54) | 0.250/0.282/0.250 | 0.073/0.077/0.077 | 0.891/0.854/1.000 | 0/0/0 | 0.250/0.308/0.141 | 0.544/0.520/0.643 | **0.299/0.307/0.302** |
| learned | old (8) | — | 0.947/1.000/1.000 | — | — | 0.222/0.253/0.143 | — | 0.313/0.346/0.250 |
| learned | new (54) | 0.339/0.300/0.500 | 0.073/0.077/0.077 | 0.908/0.875/1.000 | 0/0/0 | 0.276/0.271/0.308 | 0.536/0.519/0.643 | **0.313/0.303/0.361** |
| learned + f32 | old (8) | — | 0.905/1.000/1.000 | — | — | 0.125/0.193/0.095 | — | 0.222/0.294/0.208 |
| learned + f32 | new (54) | 0.395/0.375/0.500 | 0.070/0.077/0.077 | 0.908/0.875/1.000 | 0/0/0 | 0.216/0.245/0.205 | 0.438/0.416/0.429 | **0.289/0.289/0.309** |

(The kernel old-set learned rows come out REVERSED from the README pin on this build —
learned 0.222 / + f32 0.313 where the README records 0.313 / 0.222; latency proves which
run carried the encoder (60 ms vs 1.3 s mean). Eight queries flip on one rank; the
54-query set gives learned 0.313 > lexical 0.299 > + f32 0.289, the README's ordering.)

> Correction (coordinator, same day): the kernel `old (8)` learned and learned + f32 rows above were swapped in the first draft; the coordinator's independent repro on the same binary (learned 0.313 at 0.11 s/query, + f32 rerank 0.222 at 1.85 s/query — the latency proves which run carried the encoder) matches the README pin, and the rows now read that way.

### Reading

- **Paraphrase is 0.000 everywhere** — 28 queries across three corpora, no configuration
  surfaces a single answer in the top 25 (lexical, learned, learned + encoder rerank). The
  original sets had two such queries, both on this repo, both already zero; the expanded
  sets show it is the class, not the corpus. Without the doc-side dense channel (which
  self-gates OFF on every corpus, §"Doc-side dense channel") nothing in the stack reads
  the doc comment, and every paraphrase answer lives only there. This is the strongest
  case yet for revisiting the channel's gate on labelled evidence.
- **Descriptive collapses at scale.** The single kernel descriptive query (`pick_next_task`,
  0.947) was the exception: over 13 queries the class is 0.073 on every kernel tier, and
  on cpython the learned tier's celebrated 0.036 → 0.475 shrinks to 0.088 → 0.195 (still
  a 2.2× lift, and still the reason to prefer learned there). Twelve of the thirteen
  kernel descriptive answers are not in the top 25 on any tier.
- **The per-corpus verdicts survive with smaller margins.** learned > lexical on all-NDCG
  for every corpus (kernel 0.313 vs 0.299, cpython 0.340 vs 0.307, vorpal 0.450 vs
  0.400) — and on recall@5 by more (0.361 vs 0.302, 0.389 vs 0.333, 0.536 vs 0.400). The
  encoder rerank edges learned on cpython (0.350 / recall 0.426) and vorpal (0.461) but
  loses on the kernel (0.289; short-kw 0.276 → 0.216, subset 0.536 → 0.438) — the same
  direction the README pins, now on 54 queries instead of 8.
- **Exact is not 1.0 once names repeat.** `getmembers` (labelled `Lib/inspect.py`) ranks
  behind `tarfile.py`'s method (rank 2); `update_load_avg` rank 1; `amdgpu_device_init`
  rank 2 — the exact class now measures duplicate-name tie-breaking, which is what the
  `path` disambiguator exists for.
- **Short-keyword on the kernel is 0.25–0.28**, not the 0.13–0.21 the 7-query set showed,
  but the added six split the same way the original seven do (learned tier, best rank of
  the grade-3 answer): `queue_delayed_work` 0, `bio_alloc_bioset` 1, `alloc_workqueue` 4;
  `skb_clone`, `d_alloc`, `find_vma` not in the top 25 — alongside the original misses
  `alloc_skb`, `tcp_cong_avoid_ai`, `mutex_lock`, `request_threaded_irq`. Seven of thirteen
  short-keyword answers are absent from the top 25 on the learned tier; the class's
  verdicts now move on several queries, not one.


## Size-aware dense fusion — the coverage→quality curve and the laws that bend it (2026-09-03)

**The measured problem** ("Doc-side dense channel", "always-on fill" above): the dense
sidecar list fuses as one more reciprocal-rank list at the fusion's single K = 60, and on
the kernel its quality SLIDES as the fill grows — all-NDCG@10 with the channel ON + rerank
0.345 at 24 K rows, 0.335 at 58 K, 0.328 at 112 K, 0.308 at 143 K — against the
no-encoder learned tier's 0.313 (the gate). RRF is scale-free: a list drawn from 700 K
candidates hands its arbitrary top the same 1/(60 + rank) mass as a list drawn from 25 K,
so as coverage grows the dense head fills with high-cosine lookalikes that outvote the
exact-name and graph evidence for the subword-identifier answers (the Stage-4 BM25 lesson
in SEMANTIC_TIER.md — "wrong evidence that scale-free RRF rewards anyway").

**Candidates** (`DenseFusion` in `crates/index/src/lib.rs`; `VORPAL_DENSE_FUSION=
flat|quantile|subset|cutoff|gap|degree` under `bench-internals`; every law leaves the other
four lists at K = 60 and is byte-identical to the plain fusion when no dense list is
present — `mass_seam_reproduces_the_standard_fusion_bitwise`). Each is DERIVED from the
sidecar's own counts (`n_c` = rows covered, `n_e` = the fill's eligible population from
`ann.dense.json`) or the query's own scores; none carries a tuned constant:

| law | what changes | derivation |
|---|---|---|
| `flat` | nothing (shipped) | one more list at K, full pool depth |
| `quantile` (a) | dense mass = 1/(K + r·n_e/n_c) | independent-prefilter law: rank r among n_c covered rows is the quantile r/n_c; its rank-equivalent in the list's own population is r·n_e/n_c. Head keeps 1/K; the tail damps by the coverage fraction; `flat` at complete coverage |
| `subset` (a') | dense mass = 1/(K·n_c/n_e + r) | prefix-conjunction law: a sidecar filled in in-degree order is the conjunction of two rankings, so a list drawn from the fraction φ = n_c/n_e of its population has a rank offset K·φ; `flat` at complete coverage |
| `cutoff` (b) | dense list truncated to m = rerank_pool(k) / (non-empty lists) | equal-share law: the pool depth divided among the lists that actually nominate (k = 25 → pool 100; kernel 4 nominating lists → m = 25) |
| `gap` (c) | dense rows admitted only while cosine ≥ (c_max + c_median)/2 over the rescored oversample (4 × pool) | self-referenced score-gap: the upper half of the query's own score range; the analogue of the conjunction support law |
| `degree` (e) | the graph list ranks named ∪ dense candidates by referential in-degree | the fusion's own graph evidence applied to the dense nominees: a referenced hub earns graph mass, a lookalike nobody calls does not |
| `hub` (f) | of the dense cosine top-pool, keep the equal share m = pool / lists with the highest referential in-degree (cosine order preserved) | the coverage order made explicit: the fill covers in in-degree order, so every row a later checkpoint adds has in-degree ≤ every earlier row — the lookalikes that pile into the dense head are, by construction, the least-referenced covered rows; at any coverage this is the dense list a young fill would have produced |
| `support` (g) | a dense row is admitted if a SIBLING list also surfaces it (name / semantic / graph / BM25 at pool depth) or it is in the `hub` share | the conjunction support law on the dense list: a single-source nomination needs structural evidence, a corroborated one does not |

**Protocol.** One resumable kernel fill (`vorpal-index __warm-ann --dense-budget-timeout`),
searcheval at successive checkpoints (the fill commits at every cap), the same binary and
labels at every point; `VORPAL_NO_AUTOWARM=1`; k = 25; searcheval's double-run
determinism gate on every row. Arms per checkpoint: no encoder (`VORPAL_HOME` without
weights = the learned tier's gate line), channel forced OFF + rerank (the pre-change
shipping), and for each law channel ON + rerank and channel ON without rerank
(`VORPAL_RERANK_MODE=off`). Machine: M5 Max, CONTENDED throughout (two concurrent agents;
`uptime` 15–35) — latencies are relative within a checkpoint only; quality is
deterministic.

### Kernel coverage curve — v0.7.0 8-query set, all-NDCG@10 (exploration: every law, every checkpoint)

One fill (`__warm-ann --dense-budget-timeout 150s / 75s / 270s / 460s`, resumed round to round; 34.2 GB peak RSS on the first round which also built the learned tier), checkpoints copied aside and restored for the later confirmation runs. The no-encoder line (0.313) is the gate.

| arm | 40 K rows | 51 K rows | 99 K rows | 190 K rows |
|---|---:|---:|---:|---:|
| noenc | 0.313 | 0.313 | 0.313 | 0.313 |
| off | 0.222 | 0.222 | 0.222 | 0.222 |
| flat-on | 0.345 | 0.340 | 0.332 | 0.245 |
| quantile-on | 0.277 | 0.211 | 0.208 | 0.200 |
| subset-on | 0.239 | 0.202 | 0.173 | 0.153 |
| cutoff-on | 0.345 | 0.340 | 0.332 | 0.243 |
| gap-on | 0.345 | 0.337 | 0.246 | 0.242 |
| degree-on | 0.291 | 0.291 | 0.280 | 0.209 |
| hub-on | 0.415 | — | 0.340 | 0.332 |
| support-on | 0.378 | — | 0.342 | 0.275 |
| flat-only | 0.358 | 0.322 | 0.280 | 0.251 |
| cutoff-only | 0.365 | 0.330 | 0.280 | 0.251 |
| gap-only | 0.388 | 0.352 | 0.202 | 0.232 |
| degree-only | 0.298 | 0.295 | 0.259 | 0.279 |
| hub-only | 0.441 | — | 0.330 | 0.280 |
| support-only | 0.345 | — | 0.318 | 0.278 |

`noenc` and `off` carry no dense list and are constant by construction (the byte-identity the laws promise). Per class, the three laws that matter:

Kernel, v0.7.0 8-query set (1 descriptive + 7 short-keyword; NDCG@10 / MRR / recall@5), channel ON + rerank unless `-only`:

| arm | rows | descriptive | short-keyword | **all** |
|---|---:|---|---|---|
| noenc | — | 0.947/1.000/1.000 | 0.222/0.253/0.143 | **0.313**/0.346/0.250 |
| off | 40704 | 0.905/1.000/1.000 | 0.125/0.193/0.095 | **0.222**/0.294/0.208 |
| flat-on | 40704 | 0.608/0.500/1.000 | 0.307/0.389/0.190 | **0.345**/0.403/0.292 |
| hub-on | 40704 | 0.905/1.000/1.000 | 0.345/0.418/0.190 | **0.415**/0.491/0.292 |
| support-on | 40704 | 0.608/0.500/1.000 | 0.345/0.418/0.190 | **0.378**/0.428/0.292 |
| flat-only | 40704 | 0.608/0.500/1.000 | 0.322/0.416/0.190 | **0.358**/0.426/0.292 |
| hub-only | 40704 | 0.905/1.000/1.000 | 0.375/0.456/0.238 | **0.441**/0.524/0.333 |
| support-only | 40704 | 0.496/0.333/1.000 | 0.323/0.424/0.190 | **0.345**/0.413/0.292 |
| noenc | — | 0.947/1.000/1.000 | 0.222/0.253/0.143 | **0.313**/0.346/0.250 |
| off | 99328 | 0.905/1.000/1.000 | 0.125/0.193/0.095 | **0.222**/0.294/0.208 |
| flat-on | 99328 | 0.608/0.500/1.000 | 0.292/0.361/0.095 | **0.332**/0.378/0.208 |
| hub-on | 99328 | 0.608/0.500/1.000 | 0.302/0.378/0.190 | **0.340**/0.393/0.292 |
| support-on | 99328 | 0.608/0.500/1.000 | 0.304/0.379/0.190 | **0.342**/0.394/0.292 |
| flat-only | 99328 | 0.402/0.200/0.500 | 0.263/0.355/0.095 | **0.280**/0.335/0.146 |
| hub-only | 99328 | 0.435/0.250/0.500 | 0.315/0.397/0.238 | **0.330**/0.378/0.271 |
| support-only | 99328 | 0.435/0.250/0.500 | 0.301/0.379/0.238 | **0.318**/0.363/0.271 |
| noenc | — | 0.947/1.000/1.000 | 0.222/0.253/0.143 | **0.313**/0.346/0.250 |
| off | 190720 | 0.905/1.000/1.000 | 0.125/0.193/0.095 | **0.222**/0.294/0.208 |
| flat-on | 190720 | 0.790/1.000/1.000 | 0.168/0.206/0.048 | **0.245**/0.306/0.167 |
| hub-on | 190720 | 0.608/0.500/1.000 | 0.292/0.361/0.095 | **0.332**/0.378/0.208 |
| support-on | 190720 | 0.790/1.000/1.000 | 0.201/0.234/0.095 | **0.275**/0.330/0.208 |
| flat-only | 190720 | 0.585/1.000/0.500 | 0.203/0.270/0.143 | **0.251**/0.361/0.188 |
| hub-only | 190720 | 0.402/0.200/0.500 | 0.263/0.356/0.095 | **0.280**/0.337/0.146 |
| support-only | 190720 | 0.568/1.000/0.500 | 0.236/0.299/0.143 | **0.278**/0.386/0.188 |

**What the per-query provenance shows (`VORPAL_SEARCHEVAL_CHANNELS=1`, `flat`).** Every labelled answer is in the fused top-25 at every coverage; what moves is each answer's rank INSIDE the dense list as newly covered rows with higher cosine land above it — `alloc_skb` dense#1 → #2 → #8 (fused#5 → #6 → #10), `request_irq` dense#2 → #7 → #13 (fused#3 → #8 → #14), `handle_mm_fault` dense#7 → #19 → #42 (fused#1 → #1 → #17: it loses the pinned rank-0 it held while its dense mass made it the fused winner). With the rerank on, the fusion decides only pool MEMBERSHIP; the encoder's cosine order over the pool sets the final positions, and every lookalike the dense list admits above an answer has, by construction, the higher cosine and wins the reorder. That is why `flat`, `cutoff` and `gap` give identical final positions for pool members (their dense lists differ only below the answers or as prefixes that cut answers and lookalikes at the same rate), why the population-scaled laws can only lose (a monotone rank→mass map cannot reorder a list), and why `degree`/`subset` sink the descriptive query (giving every dense candidate a second placement floods the pool and pushes a vector-only answer out). The in-degree of the newly covered rows is the one signal that separates the lookalikes from the answers — under `hub` at 190 K rows: `alloc_skb` dense#2 (fused#6), `request_irq` dense#7 (fused#8), `handle_mm_fault` fused#1 again — and `hub` is the only law ≥ 0.313 at every point (0.415 / 0.340 / 0.332). `tcp_cong_avoid_ai` enters the pool only from 99 K rows on (dense#12, fused#23): coverage also ADDS answers, which is the argument for the always-on fill.

### Kernel coverage curve — expanded 54-query set (`6e048ee`; confirmation, trimmed to `off` / `flat` / the two candidates)

| arm | 40 K rows | 99 K rows | 190 K rows |
|---|---:|---:|---:|
| noenc | 0.313 | — | — |
| off | 0.289 | 0.289 | 0.289 |
| flat-on | 0.404 | 0.416 | 0.414 |
| quantile-on | 0.415 | — | — |
| cutoff-on | — | — | — |
| gap-on | — | — | — |
| degree-on | — | — | — |
| hub-on | 0.397 | 0.410 | 0.405 |
| support-on | 0.406 | 0.421 | 0.403 |
| flat-only | 0.427 | — | — |
| hub-only | 0.422 | 0.421 | 0.411 |
| support-only | 0.424 | 0.415 | 0.402 |

54-query set (`6e048ee`; `--root`), same arms:

| arm | rows | conjunctive | descriptive | exact | paraphrase | short-keyword | subset | **all** |
|---|---:|---|---|---|---|---|---|---|
| noenc | — | 0.339/0.300/0.500 | 0.073/0.077/0.077 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.276/0.271/0.308 | 0.536/0.519/0.643 | **0.313**/0.303/0.361 |
| off | 40704 | 0.395/0.375/0.500 | 0.070/0.077/0.077 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.216/0.245/0.205 | 0.438/0.416/0.429 | **0.289**/0.289/0.309 |
| flat-on | 40704 | 0.594/0.583/0.750 | 0.216/0.231/0.269 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.446/0.490/0.333 | 0.507/0.530/0.429 | **0.404**/0.415/0.404 |
| hub-on | 40704 | 0.395/0.375/0.500 | 0.245/0.273/0.231 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.489/0.544/0.333 | 0.439/0.416/0.429 | **0.397**/0.408/0.377 |
| support-on | 40704 | 0.594/0.583/0.750 | 0.205/0.212/0.269 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.466/0.505/0.333 | 0.508/0.530/0.429 | **0.406**/0.414/0.404 |
| flat-only | 40704 | 0.624/0.625/0.750 | 0.228/0.214/0.308 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.467/0.506/0.372 | 0.613/0.603/0.643 | **0.427**/0.427/0.451 |
| hub-only | 40704 | 0.327/0.275/0.250 | 0.271/0.288/0.269 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.536/0.592/0.436 | 0.536/0.519/0.643 | **0.422**/0.429/0.420 |
| support-only | 40704 | 0.624/0.625/0.750 | 0.210/0.186/0.269 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.468/0.510/0.372 | 0.615/0.606/0.643 | **0.424**/0.422/0.441 |
| off | 99328 | 0.395/0.375/0.500 | 0.070/0.077/0.077 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.216/0.245/0.205 | 0.438/0.416/0.429 | **0.289**/0.289/0.309 |
| flat-on | 99328 | 0.502/0.458/0.750 | 0.260/0.240/0.269 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.482/0.556/0.397 | 0.508/0.530/0.429 | **0.416**/0.424/0.420 |
| hub-on | 99328 | 0.502/0.458/0.750 | 0.274/0.259/0.308 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.443/0.484/0.333 | 0.508/0.530/0.429 | **0.410**/0.411/0.414 |
| support-on | 99328 | 0.502/0.458/0.750 | 0.274/0.259/0.308 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.489/0.566/0.449 | 0.508/0.530/0.429 | **0.421**/0.431/0.441 |
| hub-only | 99328 | 0.499/0.458/0.750 | 0.253/0.231/0.308 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.472/0.508/0.436 | 0.577/0.590/0.643 | **0.421**/0.418/0.466 |
| support-only | 99328 | 0.482/0.438/0.750 | 0.240/0.208/0.269 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.467/0.543/0.436 | 0.575/0.572/0.643 | **0.415**/0.417/0.457 |
| off | 190720 | 0.395/0.375/0.500 | 0.070/0.077/0.077 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.216/0.245/0.205 | 0.438/0.416/0.429 | **0.289**/0.289/0.309 |
| flat-on | 190720 | 0.697/0.667/0.875 | 0.260/0.260/0.308 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.410/0.464/0.372 | 0.510/0.527/0.429 | **0.414**/0.421/0.432 |
| hub-on | 190720 | 0.502/0.458/0.750 | 0.260/0.240/0.269 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.438/0.474/0.282 | 0.508/0.530/0.429 | **0.405**/0.404/0.392 |
| support-on | 190720 | 0.594/0.583/0.750 | 0.274/0.279/0.269 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.385/0.426/0.321 | 0.510/0.527/0.429 | **0.403**/0.411/0.401 |
| hub-only | 190720 | 0.463/0.417/0.500 | 0.230/0.197/0.269 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.444/0.487/0.359 | 0.619/0.603/0.643 | **0.411**/0.403/0.420 |
| support-only | 190720 | 0.624/0.625/0.750 | 0.218/0.234/0.192 | 0.908/0.875/1.000 | 0.000/0.000/0.000 | 0.395/0.460/0.385 | 0.567/0.547/0.643 | **0.402**/0.414/0.426 |

**The confirmation contradicts the exploration in two places, stated rather than reconciled.** (1) On the 54-query set the shipped fusion does NOT slide — 0.404 / 0.416 / 0.414 — and `hub` sits 0.007–0.009 BELOW it at every point: the short-keyword gain the 8-query set measured is real on this set too (0.410 → 0.438 at 190 K) but the same admission costs the conjunctive class (0.697 → 0.502, one query's worth) and the two cancel. `support` is +0.002 / +0.005 / −0.011 — inside one query. The 8-query slide was carried by three dense-only hub answers of one class; six classes and 54 queries dilute it to nothing. (2) `quantile` — the worst law on the 8-query set (0.277 vs 0.345 at 40 K) — measures 0.415 vs 0.404 at 40 K on the 54-query set (its conjunctive 0.709 vs 0.594). Not pursued further under the trim; recorded as a contradiction, not as evidence for the law.

### cpython and vorpal — complete referenced coverage

cpython, v0.7.0 6-query set, complete referenced coverage (35,292 rows):

| arm | descriptive | short-keyword | **all** |
|---|---|---|---|
| noenc | 0.475/0.542/0.375 | 0.287/0.500/0.250 | **0.412**/0.528/0.333 |
| off | 0.531/0.583/0.625 | 0.169/0.500/0.250 | **0.410**/0.556/0.500 |
| flat-on | 0.420/0.348/0.375 | 0.349/0.571/0.250 | **0.396**/0.423/0.333 |
| quantile-on | 0.420/0.348/0.375 | 0.349/0.571/0.250 | **0.396**/0.423/0.333 |
| subset-on | 0.420/0.348/0.375 | 0.349/0.571/0.250 | **0.396**/0.423/0.333 |
| cutoff-on | 0.361/0.344/0.375 | 0.349/0.571/0.250 | **0.357**/0.420/0.333 |
| gap-on | 0.558/0.562/0.500 | 0.300/0.571/0.250 | **0.472**/0.565/0.417 |
| degree-on | 0.670/0.750/0.500 | 0.467/0.571/0.250 | **0.602**/0.690/0.417 |
| hub-on | 0.676/0.750/0.625 | 0.338/0.625/0.500 | **0.564**/0.708/0.583 |
| support-on | 0.420/0.348/0.375 | 0.360/0.583/0.250 | **0.400**/0.427/0.333 |
| flat-only | 0.540/0.500/0.625 | 0.407/0.550/0.250 | **0.496**/0.517/0.500 |
| cutoff-only | 0.540/0.500/0.625 | 0.417/0.562/0.250 | **0.499**/0.521/0.500 |
| gap-only | 0.607/0.625/0.625 | 0.417/0.562/0.250 | **0.544**/0.604/0.500 |
| degree-only | 0.686/0.750/0.625 | 0.529/0.583/0.500 | **0.634**/0.694/0.583 |
| hub-only | 0.680/0.750/0.625 | 0.557/0.583/0.500 | **0.639**/0.694/0.583 |
| support-only | 0.540/0.500/0.625 | 0.407/0.550/0.250 | **0.496**/0.517/0.500 |

vorpal (this worktree), v0.7.0 10-query set, complete referenced coverage (11,751 rows):

| arm | conjunctive | descriptive | exact | paraphrase | short-keyword | subset | **all** |
|---|---|---|---|---|---|---|---|
| noenc | 0.000/0.043/0.000 | 0.500/0.500/0.500 | 0.815/0.750/1.000 | 0.000/0.000/0.000 | 0.894/1.000/0.750 | 0.500/0.333/1.000 | **0.492**/0.488/0.550 |
| off | 0.431/0.250/1.000 | 0.500/0.500/0.500 | 0.815/0.750/1.000 | 0.000/0.000/0.000 | 0.894/1.000/0.750 | 0.631/0.500/1.000 | **0.548**/0.525/0.650 |
| flat-on | 0.431/0.250/1.000 | 0.815/0.750/1.000 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.894/1.000/0.750 | 1.000/1.000/1.000 | **0.685**/0.675/0.750 |
| quantile-on | 0.431/0.250/1.000 | 0.815/0.750/1.000 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.894/1.000/0.750 | 1.000/1.000/1.000 | **0.685**/0.675/0.750 |
| subset-on | 0.431/0.250/1.000 | 0.815/0.750/1.000 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.894/1.000/0.750 | 1.000/1.000/1.000 | **0.685**/0.675/0.750 |
| cutoff-on | 0.431/0.250/1.000 | 0.815/0.750/1.000 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.894/1.000/0.750 | 1.000/1.000/1.000 | **0.685**/0.675/0.750 |
| gap-on | 0.431/0.250/1.000 | 0.815/0.750/1.000 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.894/1.000/0.750 | 1.000/1.000/1.000 | **0.685**/0.675/0.750 |
| degree-on | 0.631/0.500/1.000 | 0.815/0.750/1.000 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.894/1.000/0.750 | 0.631/0.500/1.000 | **0.668**/0.650/0.750 |
| hub-on | 0.000/0.000/0.000 | 0.500/0.500/0.500 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.894/1.000/0.750 | 0.631/0.500/1.000 | **0.542**/0.550/0.550 |
| support-on | 0.431/0.250/1.000 | 0.815/0.750/1.000 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.894/1.000/0.750 | 1.000/1.000/1.000 | **0.685**/0.675/0.750 |
| flat-only | 0.500/0.333/1.000 | 0.500/0.545/0.500 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.944/1.000/0.750 | 1.000/1.000/1.000 | **0.639**/0.642/0.650 |
| cutoff-only | 0.500/0.333/1.000 | 0.500/0.545/0.500 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.944/1.000/0.750 | 1.000/1.000/1.000 | **0.639**/0.642/0.650 |
| gap-only | 0.631/0.500/1.000 | 0.693/0.600/1.000 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.894/1.000/0.750 | 1.000/1.000/1.000 | **0.680**/0.670/0.750 |
| degree-only | 0.631/0.500/1.000 | 0.500/0.542/0.500 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.894/1.000/0.750 | 0.631/0.500/1.000 | **0.605**/0.608/0.650 |
| hub-only | 0.000/0.000/0.000 | 0.500/0.500/0.500 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.894/1.000/0.750 | 0.500/0.333/1.000 | **0.529**/0.533/0.550 |
| support-only | 0.500/0.333/1.000 | 0.500/0.545/0.500 | 1.000/1.000/1.000 | 0.000/0.000/0.000 | 0.942/1.000/0.750 | 1.000/1.000/1.000 | **0.638**/0.642/0.650 |

cpython, expanded 54-query set (35,292 rows):

| arm | conjunctive | descriptive | exact | paraphrase | short-keyword | subset | **all** |
|---|---|---|---|---|---|---|---|
| noenc | 0.157/0.133/0.200 | 0.195/0.205/0.192 | 0.954/0.938/1.000 | 0.000/0.000/0.000 | 0.386/0.310/0.538 | 0.401/0.400/0.417 | **0.340**/0.320/0.389 |
| off | 0.135/0.100/0.200 | 0.212/0.218/0.269 | 0.938/0.917/1.000 | 0.000/0.000/0.000 | 0.388/0.339/0.577 | 0.488/0.458/0.500 | **0.350**/0.330/0.426 |
| flat-on | 0.347/0.415/0.300 | 0.254/0.197/0.269 | 0.938/0.917/1.000 | 0.000/0.000/0.000 | 0.555/0.578/0.654 | 0.498/0.480/0.500 | **0.421**/0.414/0.454 |
| quantile-on | 0.347/0.415/0.300 | 0.254/0.197/0.269 | 0.938/0.917/1.000 | 0.000/0.000/0.000 | 0.555/0.578/0.654 | 0.498/0.480/0.500 | **0.421**/0.414/0.454 |
| cutoff-on | — | — | — | — | — | — | — |
| gap-on | — | — | — | — | — | — | — |
| hub-on | 0.296/0.270/0.200 | 0.295/0.295/0.346 | 0.857/0.812/0.875 | 0.032/0.011/0.000 | 0.659/0.695/0.769 | 0.484/0.458/0.500 | **0.443**/0.436/0.472 |
| support-on | 0.406/0.420/0.300 | 0.226/0.184/0.269 | 0.938/0.917/1.000 | 0.032/0.011/0.000 | 0.557/0.580/0.654 | 0.498/0.480/0.500 | **0.426**/0.414/0.454 |
| flat-only | 0.410/0.422/0.200 | 0.238/0.204/0.269 | 0.929/0.906/1.000 | 0.000/0.000/0.000 | 0.607/0.589/0.692 | 0.460/0.417/0.500 | **0.430**/0.411/0.454 |
| hub-only | 0.312/0.273/0.200 | 0.258/0.274/0.269 | 0.908/0.875/1.000 | 0.000/0.007/0.000 | 0.704/0.705/0.692 | 0.441/0.487/0.583 | **0.444**/0.446/0.463 |
| support-only | 0.410/0.422/0.200 | 0.237/0.200/0.269 | 0.929/0.906/1.000 | 0.000/0.005/0.000 | 0.611/0.593/0.692 | 0.460/0.418/0.500 | **0.431**/0.411/0.454 |

vorpal, expanded 55-query set (11,751 rows):

| arm | conjunctive | descriptive | exact | paraphrase | short-keyword | subset | **all** |
|---|---|---|---|---|---|---|---|
| off | 0.520/0.590/0.800 | 0.192/0.171/0.192 | 0.940/0.938/1.000 | 0.000/0.000/0.000 | 0.701/0.694/0.750 | 0.690/0.605/0.857 | **0.470**/0.459/0.536 |
| flat-on | 0.494/0.557/0.800 | 0.320/0.260/0.346 | 0.986/1.000/1.000 | 0.000/0.000/0.000 | 0.848/0.854/0.917 | 0.832/0.790/0.857 | **0.555**/0.544/0.609 |
| hub-on | 0.456/0.507/0.600 | 0.239/0.200/0.346 | 0.986/1.000/1.000 | 0.000/0.000/0.000 | 0.695/0.688/0.750 | 0.704/0.605/0.857 | **0.483**/0.466/0.555 |
| support-on | 0.494/0.557/0.800 | 0.329/0.263/0.385 | 0.986/1.000/1.000 | 0.000/0.000/0.000 | 0.848/0.854/0.917 | 0.832/0.790/0.857 | **0.557**/0.545/0.618 |
| hub-only | 0.367/0.509/0.500 | 0.243/0.185/0.308 | 0.990/1.000/1.000 | 0.000/0.000/0.000 | 0.703/0.708/0.750 | 0.673/0.600/0.857 | **0.474**/0.466/0.536 |
| support-only | 0.483/0.562/0.700 | 0.259/0.195/0.231 | 0.990/1.000/1.000 | 0.000/0.000/0.000 | 0.862/0.854/0.958 | 0.849/0.810/0.929 | **0.545**/0.532/0.591 |

`hub` is the lesson in corpus dependence: +0.168 (6-query) / +0.022 (54-query) on cpython, −0.143 / −0.072 on vorpal, where it drops cosine-best answers with in-degree 1 (`ingest_traces` dense#1, `cut` dense#2 — both corroborated by the vector list, which is what `support` reads). `support` never regresses an all-NDCG on the v0.7.0 sets (kernel 0.378 / 0.342 / 0.275 vs 0.345 / 0.332 / 0.245; cpython 0.400 vs 0.396; vorpal 0.685 bit-identical per class) and on the expanded sets is +0.005 (cpython, but descriptive 0.254 → 0.226) and +0.002 (vorpal, no class down); its kernel curve still slides and still fails the 0.313 gate at 190 K on the 8-query set.

### Latency (paired, interleaved; contended, `uptime` 48–52)

Kernel, 190,720 rows, expanded 54-query set, k = 25, searcheval mean; two interleaved reps of `flat` / `hub` / `support`, channel ON, rerank on (`reorder`) and off:

| law | rerank on, rep 1 | rep 2 | channel only, rep 1 | rep 2 |
|---|---:|---:|---:|---:|
| `flat` | 2,562 ms | 2,163 ms | 490 ms | 332 ms |
| `hub` | 2,351 ms | 1,981 ms | 420 ms | 374 ms |
| `support` | 2,471 ms | 2,826 ms | 435 ms | 541 ms |

Rep-to-rep spread of the SAME law is ±30 % (the machine carried two other agents' builds and fills, load 42–53), and the laws' own work — a sort of the 100-row dense pool by in-degree, a set of ≤ 400 sibling ids — is microseconds against the 4-list channel pass, so query latency is unchanged within what this machine could resolve; the sweep tables' `mean µs` columns above are contended the same way and comparable only within a checkpoint.

### Verdict

**`DENSE_FUSION = Flat` stays pinned.** The acceptance criterion was a coverage→quality curve made flat by a derived law with no class regression elsewhere: no law meets it. `hub` flattens the 8-query curve and fails vorpal; `support` regresses nothing on the v0.7.0 sets but does not hold the 8-query gate at 190 K and is inside one query of `flat` everywhere else; on the 54-query kernel set the shipped fusion is already flat from 40 K to 190 K rows. Under the no-dominance rule (the rerank-mode precedent) the pin does not move. The seam and all eight laws stay under `bench-internals` for the next sweep; the constructive leads are (a) `support` as a per-corpus verdict the way `encoder.dir` is (it is the only non-regressing candidate), and (b) the mechanism itself — the rerank's cosine order over the pool, not the fusion, sets the positions the slide is measured on, so a size-aware answer to the 8-query slide lives in the rerank's pool/pin law, which was out of scope here. Housekeeping: the first `cargo test --workspace --release` run exited 101 with every captured `test result` ok while sharing cargo's target lock with a concurrent build; the clean re-run is green (60 suites), and both CI clippy lanes are green after fixing a redundant closure in the cherry-picked `--root` check (`xtask/src/searcheval.rs:90`).
## Two-field surfaces — rich sidecar, head rerank (2026-09-03)

The "Surface-recipe A/B" above ran under the one-recipe law (sidecar and rerank share one
`SurfaceRecipe`), so the richer recipes paid twice: the sidecar gained paraphrase rank
while the rerank re-encoded longer candidate surfaces on every query (3.3× latency) and
diluted its exact-name evidence (`Postings` fused #1 → #6–8). This pass breaks the law
deliberately: `dense::SurfacePair { sidecar, rerank }` — the SIDECAR embeds
`HeadDocBody` (head + leading doc comment + body head; comment family by extension,
attribute lines skipped, the derived 398-token cap, per-definition fallback to head with
counts), the RERANK keeps `Head` (`name signature basename`). `ann.dense.json` carries
both labels (`surface`, `rerank_surface`); freshness demands both, a pre-two-field record
reads `rerank_surface = surface`; a resume under another pair starts over.
`VORPAL_SURFACE_RECIPE=<sidecar>/<rerank>` sweeps the pair under `bench-internals`
(`body/head` is the pin, `head/head` the shipped v0.7.0 pair, `body/body` the one-recipe
rich reference; a bare `<recipe>` sets both). The dense list's LENGTH is the second seam:
`DenseSidecar::depth(pool)` = `DENSE_DEPTH_FACTOR × pool` (pinned 1) with
`VORPAL_DENSE_DEPTH=<factor>|share` (`share` = pool × ⌈population / covered⌉).

Substrate: fresh `--semantic-tier learned` indexes of the worktree at v0.7.0 + this
branch (79,431 nodes), cpython (162,813) and the kernel (8.89 M), tier-warmed with the
shipped v0.7.0 binary, sidecars filled by this branch's binary under the pinned
referenced-only rule; encoder f32 via `vorpal enable semantic-f32` into a scratch
`VORPAL_HOME`; `VORPAL_NO_AUTOWARM=1`. "Shipped" = a pristine release build of `eaa0f7e`
(same target dir, sources stashed) reading the `head/head` sidecar — its freshness ignores
`rerank_surface`, so the same files serve both binaries. **The whole pass was CONTENDED**
(`uptime` load 25–90 throughout: two concurrent agents' builds and fills on this machine);
every wall-clock / tok/s / latency figure below says so, the ranking figures are
deterministic (double-run gate) and unaffected. Label sets: the OLD v0.7.0 sets (10 / 6 /
8 queries) and the NEW 55 / 54 / 54 sets (`labels-50`, `5444b71`, cherry-picked onto this
branch; kernel with `--root`).

### vorpal (self-index) — per class, NDCG@10 / MRR / recall@5

OLD set (10 queries):

| pair (sidecar / rerank) | binary | conjunctive | descriptive | exact | paraphrase | short-kw | subset | **all** | searcheval mean |
|---|---|---|---|---|---|---|---|---|---:|
| head / head | shipped | 0.500/0.333/1.000 | 0.815/0.750/1.000 | 1/1/1 | 0/0/0 | 0.894/1.000/0.750 | 1/1/1 | **0.692/0.683/0.750** | 1.05–1.51 s |
| head / head | this branch | 0.500/0.333/1.000 | 0.815/0.750/1.000 | 1/1/1 | 0/0/0 | 0.894/1.000/0.750 | 1/1/1 | **0.692/0.683/0.750** (= shipped) | 1.13–1.57 s |
| **body / head (pin)** | this branch | 0.500/0.333/1.000 | 0.815/0.750/1.000 | 1/1/1 | 0/**0.025**/0 | 0.894/1.000/0.750 | 1/1/1 | **0.692/0.688/0.750** | 0.88–0.92 s |
| body / head, channel only | this branch | 0.631/0.500/1.000 | 0.693/0.600/1.000 | 1/1/1 | 0/0.026/0 | 0.947/1.000/0.750 | 1/1/1 | 0.691/0.675/0.750 | 0.07–0.11 s |
| body / head, depth ×2 / ×4 / share (×7) | this branch | 0.500/0.333/1.000 | 0.815/0.750/1.000 | 1/1/1 | 0/0.024/0 | 0.894/1.000/0.750 | 1/1/1 | 0.692/0.688/0.750 | 0.93–1.17 s |
| body / body (one-recipe rich) | this branch | 0.333/0.143/0 | 0.815/0.750/1.000 | 1/1/1 | 0/0.036/0 | 0.894/1.000/0.750 | 1/1/1 | 0.675/0.671/0.650 | 3.3–3.8 s |

NEW set (55 queries; 10 paraphrase, 13 descriptive):

| pair (sidecar / rerank) | binary | conjunctive | descriptive | exact | paraphrase | short-kw | subset | **all** | searcheval mean |
|---|---|---|---|---|---|---|---|---|---:|
| head / head | shipped | 0.506/0.573/0.800 | 0.393/0.323/0.423 | 0.986/1/1 | 0/0/0 | 0.876/0.896/0.917 | 0.763/0.676/0.857 | **0.571/0.555/0.627** | 1.91 s |
| head / head | this branch | 0.506/0.573/0.800 | 0.393/0.323/0.423 | 0.986/1/1 | 0/0/0 | 0.876/0.896/0.917 | 0.763/0.676/0.857 | **0.571/0.555/0.627** (= shipped) | 0.87 s |
| head / head, channel only | shipped | 0.520/0.600/0.700 | 0.296/0.242/0.308 | 0.990/1/1 | 0/0/0 | 0.888/0.892/0.958 | 0.797/0.726/0.929 | 0.556/0.544/0.609 | 0.07 s |
| **body / head (pin)** | this branch | 0.505/0.573/0.800 | **0.429/0.357/0.423** | 0.986/1/1 | 0/**0.009**/0 | 0.877/0.896/0.917 | 0.750/0.676/0.857 | **0.577/0.565/0.627** | 1.06 s |
| body / head, channel only | this branch | 0.529/0.600/0.900 | 0.322/0.259/0.308 | 0.990/1/1 | **0.032/0.018/0** | 0.888/0.892/0.958 | 0.797/0.750/0.929 | 0.569/0.554/0.627 | 0.10 s |
| body / head, depth ×2 = ×4 = share (×7) | this branch | 0.506/0.573/0.800 | 0.428/0.355/0.423 | 0.986/1/1 | 0/0.009/0 | 0.877/0.896/0.917 | 0.763/0.676/0.857 | 0.579/0.565/0.627 | 0.68–0.84 s |
| body / body (one-recipe rich) | this branch | 0.619/0.595/0.700 | 0.485/0.412/0.654 | 0.990/1/1 | 0.067/0.057/0.100 | 0.896/0.917/0.917 | 0.736/0.665/0.786 | 0.616/0.592/0.682 | **5.89 s** |

### vorpal — the paraphrase targets: dense rank (1-based, over the 11,751 covered rows) and fused rank (k = 25, 0-based)

| query | answer (grade) | dense rank, head sidecar | dense rank, body sidecar | fused, head/head | fused, body/head | fused, body/body |
|---|---|---:|---:|---|---|---|
| near duplicate code detection | `similar_pairs` (3) | 490 | **8** | — | **19** | 14 |
| who called what at runtime | `ObservedStore` (2) | 1,758 | **41** | — | — | — |
| definition text presented to encoder | `SurfaceRecipe` (3) | 3,120 | 2,861 | — | — | — |
| which definitions the fill embeds | `CoverageRule` (3) | 5,640 | 5,438 | — | — | — |
| run indexer as child process | `Supervisor` (3) | 721 | **25** | — | — | — |
| why no edge to a name | `explain_absence_on` (3) | not covered (unreferenced) | not covered | — | — | — |
| skip entity whose content hash matches | `is_unchanged` (3) | not covered | not covered | — | — | — |
| files edited together in git history | `Cochange` (3) / `CochangeEdge` (2) | 171 / 119 | 261 / **90** | — | — | — |
| serialize a reference to fixed width bytes | `encode_record` (3) | 28 | **18** | — | — | 1 |
| greedy longest match segmentation | `word_pieces` (3) | 3,298 | **1** | — | **23** | — |

Grade-3 answers inside the dense top-100: **1 → 4** of 10 (any grade ≥ 2 label: 1 → 6);
inside the fused top-25: 0 → 2 (body/head) / 2 (body/body). Two answers are unreferenced
in this graph and outside the stop rule's population under every recipe. The old two
probe targets (BENCHMARKS "Surface-recipe A/B": `similar_pairs` 292 → 99, `ObservedStore`
6,673 → 164 at full coverage) move much further under the referenced-only rule — the
11.7 K-row population has fewer distractors — to **8** and **41**.

**Why depth cannot finish the job, and what can.** `word_pieces` is dense #1 under the
body sidecar and lands at fused #24 with score 0.0167 = 1/60: `channels [dense#1]`, a
single-list candidate, and RRF(K = 60) ranks every two-list pair with both ranks < 60
(2/(60+r) > 1/60) above it. The fusion truncates to k BEFORE the rerank, so a candidate
only the dense list carries at rank r needs r ≤ k − 1 to be seen at all, and even at r = 0
it sits behind every consensus pair. Deeper dense lists (×2, ×4, ×7 = the coverage-share
projection 74,859 / 11,751) change the pinned all-NDCG by +0.002 on the new set (through
candidates another list also carries) and move no paraphrase answer — as the bound says
they cannot. `DENSE_DEPTH_FACTOR` is therefore pinned at 1 (the sweep and the bound, not a
guess). The remaining lever is the FUSION's treatment of single-list dense evidence (K,
per-channel weight, or a dense-only reserve slot) — the fusion owner's seam, outside this
pass; with the body sidecar it now has 4 of 10 vorpal answers inside the dense top-25 to
work with, where the head sidecar gave it one.

### cpython — per class, NDCG@10 / MRR / recall@5

OLD set (6 queries: 4 descriptive, 2 short-keyword):

| pair (sidecar / rerank) | binary | descriptive | short-kw | **all** | searcheval mean |
|---|---|---|---|---|---:|
| head / head | shipped | 0.420/0.348/0.375 | 0.349/0.571/0.250 | **0.396/0.423/0.333** | 1.30 s |
| head / head | this branch | 0.420/0.348/0.375 | 0.349/0.571/0.250 | **0.396/0.423/0.333** (= shipped) | 1.59 s |
| head / head, channel only | shipped | 0.540/0.500/0.625 | 0.407/0.550/0.250 | 0.496/0.517/0.500 | 0.25 s |
| **body / head (pin)** | this branch | 0.361/0.344/0.375 | 0.349/0.571/0.250 | 0.357/0.420/0.333 | 1.17 s |
| body / head, channel only | this branch | 0.540/0.500/0.625 | 0.424/0.571/0.250 | **0.502/0.524/0.500** | 0.18 s |
| body / head, depth ×4 | this branch | 0.420/0.348/0.375 | 0.349/0.571/0.250 | 0.396/0.423/0.333 | 1.18 s |
| body / body | this branch | 0.388/0.331/0.375 | 0.391/0.625/0.500 | 0.389/0.429/0.417 | 3.06 s |

NEW set (54 queries; 9 paraphrase, 13 descriptive):

| pair (sidecar / rerank) | binary | conjunctive | descriptive | exact | paraphrase | short-kw | subset | **all** | searcheval mean |
|---|---|---|---|---|---|---|---|---|---:|
| head / head | shipped | 0.347/0.415/0.300 | 0.254/0.197/0.269 | 0.938/0.917/1 | 0/0/0 | 0.555/0.578/0.654 | 0.498/0.480/0.500 | **0.421/0.414/0.454** | 1.93 s |
| head / head | this branch | 0.347/0.415/0.300 | 0.254/0.197/0.269 | 0.938/0.917/1 | 0/0/0 | 0.555/0.578/0.654 | 0.498/0.480/0.500 | **0.421/0.414/0.454** (= shipped) | 1.13 s |
| head / head, channel only | shipped | 0.410/0.422/0.200 | 0.238/0.204/0.269 | 0.929/0.906/1 | 0/0/0 | 0.607/0.589/0.692 | 0.460/0.417/0.500 | 0.430/0.411/0.454 | 0.45 s |
| **body / head (pin)** | this branch | 0.390/0.420/0.300 | 0.208/0.187/0.269 | 0.938/0.917/1 | 0/0/0 | 0.555/0.578/0.654 | 0.498/0.481/0.500 | 0.414/0.412/0.454 | 1.15 s |
| body / head, channel only | this branch | 0.420/0.433/0.200 | 0.253/0.223/0.346 | 0.929/0.906/1 | 0/0/0 | 0.601/0.577/0.692 | 0.481/0.445/0.417 | **0.436/0.417/0.463** | 0.15 s |
| body / head, depth ×2 | this branch | 0.390/0.420/0.300 | 0.208/0.188/0.269 | 0.938/0.917/1 | 0/0/0 | 0.563/0.578/0.654 | 0.498/0.481/0.500 | 0.416/0.412/0.454 | 1.16 s |
| body / head, depth ×4 = share (×5) | this branch | 0.390/0.420/0.300 | 0.226/0.189/0.269 | 0.938/0.917/1 | 0/0/0 | 0.563/0.578/0.654 | 0.498/0.481/0.500 | 0.420/0.413/0.454 | 0.71–1.09 s |
| body / body | this branch | 0.451/0.467/0.400 | 0.255/0.204/0.346 | 0.929/0.906/1 | 0/0/0 | 0.552/0.551/0.615 | 0.503/0.483/0.583 | 0.430/0.413/0.481 | 3.53 s |

cpython paraphrase targets (dense rank, 1-based, over 35,292 covered rows; NO pair puts any
of the 9 into the fused top-25):

| query | answer (grade) | dense rank, head | dense rank, body |
|---|---|---:|---:|
| release the interpreter lock | `drop_gil` (3) / `PyEval_ReleaseThread` (2) | 2,352 / 326 | **96** / **76** |
| C3 linearization of base classes | `mro_implementation` (3) | 290 | 2,333 |
| run handlers for delivered interrupts | `PyErr_CheckSignals` (3) | 1,135 | 622 |
| binary search for where a key belongs in a sorted run | `gallop_left` (3) / `gallop_right` (2) | 6,194 / 1,741 | 276 / 121 |
| memoize call results | `cache` (3) / `lru_cache` (2) | 224 / 1,211 | 450 / 3,579 |
| clone object recursively | `deepcopy` (3) | 13 | **2** |
| ignore given errors inside with statement | `suppress` (3) | 2,089 | 1,667 |
| turn an error into printable lines | `format_exception` (3) / `format_exc` (2) | 277 / 96 | 866 / 90 |
| recursively list every subfolder | `walk` (3) | 81 | 69 |

Grade-3 answers inside the dense top-100: 2 → 3 of 9 (any grade ≥ 2: 3 → 4). The body
recipe is NOT monotone here: it lifts the definitions whose first paragraph is a
docstring or a doc comment (`deepcopy` 13 → 2, `drop_gil` 2,352 → 96, `gallop_left`
6,194 → 276) and sinks the C functions whose body head is code (`mro_implementation`
290 → 2,333, `cache` 224 → 450, `format_exception` 277 → 866) — the body clause is a
docstring proxy that pays off per language, which is why the recipe's per-corpus effect
on the rerank-on all-NDCG is a wash (vorpal +0.006, cpython −0.007 on the new sets; the
old cpython set's −0.039 is one conjunctive query, 0.420 → 0.361) while channel-only
improves on both (vorpal 0.556 → 0.569, cpython 0.430 → 0.436).

### Linux kernel — per class, NDCG@10 / MRR / recall@5 (10-minute rounds: head 112,896 rows, body 26,368 rows)

OLD set (8 queries: 1 descriptive, 7 short-keyword; gate ≥ 0.313 all-NDCG):

| pair (sidecar / rerank) | binary | descriptive | short-kw | **all** | gate | searcheval mean |
|---|---|---|---|---|---|---:|
| head / head | shipped | 0.608/0.500/1.000 | 0.288/0.354/0.095 | **0.328/0.372/0.208** | pass | 2.44 s |
| head / head | this branch | 0.608/0.500/1.000 | 0.288/0.354/0.095 | **0.328/0.372/0.208** (= shipped) | pass | 2.24 s |
| head / head, channel only | shipped | 0.402/0.200/0.500 | 0.257/0.343/0.095 | 0.276/0.325/0.146 | — | 0.40 s |
| **body / head (pin)** | this branch | 0.608/0.500/1.000 | **0.306/0.390/0.190** | **0.344/0.403/0.292** | **pass** | 2.27 s |
| body / head, channel only | this branch | 0.496/0.333/1.000 | 0.347/0.402/0.238 | 0.366/0.393/0.333 | — | 0.41 s |
| body / head, depth ×2 = ×4 = share (×322) | this branch | 0.608/0.500/1.000 | 0.306/0.390/0.190 | 0.344/0.403/0.292 | pass | 2.23–2.42 s |
| body / body | this branch | 0.445/0.250/1.000 | 0.296/0.371/0.143 | 0.314/0.356/0.250 | pass (0.001) | 3.90 s |

NEW set (54 queries; 9 paraphrase, 13 descriptive; `--root`; the no-encoder learned tier
measures 0.313/0.303/0.361 and the rerank without the channel 0.289/0.289/0.309 on this set):

| pair (sidecar / rerank) | rows | conjunctive | descriptive | exact | paraphrase | short-kw | subset | **all** | gate | searcheval mean |
|---|---:|---|---|---|---|---|---|---|---|---:|
| head / head (= shipped) | 112,896 | 0.502/0.458/0.750 | **0.250/0.237/0.269** | 0.908/0.875/1 | 0/0/0 | **0.483/0.556/0.397** | 0.508/0.530/0.429 | **0.414/0.423/0.420** | pass | 2.06 s |
| head / head, channel only | 112,896 | — | — | — | 0/0/0 | — | — | 0.402/0.406/0.429 | — | 0.33 s |
| **body / head (pin candidate)** | 26,368 | 0.479/0.500/0.500 | 0.234/0.257/0.269 | 0.862/0.812/1 | 0/0.005/0 | 0.467/0.529/0.333 | 0.508/0.530/0.429 | 0.398/0.416/0.386 | pass | 2.14 s |
| body / head, channel only | 26,368 | 0.500/0.500/0.500 | 0.283/0.268/0.308 | 0.862/0.812/1 | **0.043/0.022/0.111** | 0.519/0.562/0.436 | 0.623/0.618/0.714 | **0.446/0.441/0.475** | — | 0.44 s |
| body / head, depth ×2 = ×4 = share (×322) | 26,368 | 0.479/0.500/0.500 | 0.235/0.264/0.269 | 0.862/0.812/1 | 0/0.005/0 | 0.467/0.529/0.333 | 0.508/0.530/0.429 | 0.398/0.418/0.386 | pass | 1.7–2.0 s |
| body / body | 26,368 | 0.490/0.500/0.500 | 0.249/0.249/0.308 | 0.820/0.760/1 | 0.070/0.056/0.111 | 0.453/0.517/0.308 | 0.490/0.527/0.429 | 0.402/0.412/0.407 | pass | 3.42 s |
| head / head, **coverage-matched** (`head26k`: 140 s cap on a quieter GPU → 39,680 rows) | 39,680 | — | — | — | 0/0/0 | — | — | 0.401/0.412/0.404 | pass | 5.13 s (contended by the latency runs) |
| head / head, coverage-matched, channel only | 39,680 | — | — | — | 0/0/0 | — | — | 0.425/0.424/0.441 | — | 0.32 s |
| head / head, coverage-matched, OLD set | 39,680 | | | | | | | 0.345/0.403/0.292 | pass | 1.86 s |

Kernel paraphrase answers in the fused top-25: head/head 0 of 9; body/head 1 (fused #23,
0-based 22); body/head channel-only 1 (fused **#5**); body/body 1 (fused **#2**). The
head sidecar alone is the kernel's biggest lever on this set — the channel takes the
learned tier from 0.313 to 0.414 (descriptive 0.073 → 0.250, short-kw 0.276 → 0.483) —
and the rerank is the kernel's known loss (channel-only 0.446 > +rerank 0.398 under the
body sidecar), the same direction as the "+f32 rerank loses on the kernel" reading above.
The body / head row is CONFOUNDED by coverage (26 K vs 113 K rows); the isolating
experiment is the `head26k` row: a head fill capped at 140 s so it commits ≈ the body
round's row count, evaluated under the same pair.

Coverage-matched, the recipe's own effect on the kernel is: rerank ON 0.401 (head, 39.7 K)
vs 0.398 (body, 26.4 K) — a wash; channel only 0.425 vs **0.446** (+0.021, body); OLD set
0.345 vs 0.344. (Coverage itself: head 39.7 K → 113 K rows moves the new set 0.401 → 0.414
with the rerank and 0.425 → 0.402 channel-only, the old set 0.345 → 0.328 — the
"always-on fill" coverage curve, now visible on 54 queries as a rerank-dependent sign.)

Kernel paraphrase targets (dense rank, 1-based; the body and head26k sidecars are prefixes
of the same coverage order, so their covered sets nest):

| query | answer (grade) | head, 112,896 rows | head, 39,680 rows | body, 26,368 rows |
|---|---|---:|---:|---:|
| place page array in contiguous kernel virtual range | `vmap` (3) | 28,499 | 10,316 | 10,748 |
| defer a job to the system per-cpu pool | `schedule_work` (3) / `queue_work` (2) | 8,396 / 20,842 | 3,257 / 7,887 | 1,835 / 5,437 |
| register attribute directory under kobject | `sysfs_create_group` (3) | 605 | 237 | **2** |
| block until all pre-existing readers finish | `synchronize_rcu` (3) | 2,531 | 707 | **56** |
| make a task runnable | `wake_up_process` (3) | 588 | 192 | **41** |
| write out all cached changes of a mounted volume | `sync_filesystem` (3) | 8,422 | 2,985 | **70** |
| background page reclaim daemon thread | `kswapd` (3) | not covered (unreferenced / beyond the round) | not covered | not covered |
| pick victim task when memory exhausted | `select_bad_process` (3) | not covered | not covered | not covered |
| run function across all processors | `on_each_cpu` (3) | not covered | not covered | not covered |

Grade-3 answers inside the dense top-100: head 0 of 9 at either coverage → **body 4 of 9**
(the kerneldoc block above each of these four is exactly what the body recipe embeds);
three targets sit outside every 10-minute round's coverage. Across the three corpora the
28 paraphrase answers inside the dense top-100 go 3 → **11** (vorpal 1 → 4, cpython 2 → 3,
kernel 0 → 4), inside the fused top-25 (rerank ON, k = 25) 0 → 3 (vorpal 2, kernel 1),
and the first non-zero paraphrase recall@5 on any corpus is the kernel channel-only row
(0.111: `sysfs_create_group` dense #2 → fused #5).

### Fill cost (doc-side only) — head vs body sidecar under the pinned referenced-only rule

| corpus | rows (share of defs) | recipe | tok/def | tokens | fallbacks | truncations | fill wall (CONTENDED, load) | tok/s (contended) | bytes | peak RSS |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| vorpal | 11,751 (15.7%) | head | 25.3 | 297,478 | 0 | 0 | 35.0 s (load 37–48) | 8,642 | 27.2 MB | 0.91 GB |
| vorpal | 11,751 | **body** | **121.0 (4.8×)** | 1,421,674 | 534 (4.5%) | 591 | 261.7 s (7.5×; load 32–44) | 5,444 | 27.2 MB (recipe-independent) | 2.80 GB |
| cpython | 35,292 (24.3%) | head | 25.4 | 895,515 | 0 | 0 | 439 s (load 75–86) | 2,052 | 81.7 MB | 1.01 GB |
| cpython | 35,292 | **body** | **107.2 (4.2×)** | 3,784,562 | 579 (1.6%) | 1,094 | 1,013.6 s (2.3×; load 75–122) | 3,741 | 81.7 MB | 5.07 GB |
| kernel | 10-minute round: **112,896** of 716,721 referenced (15.8%; 1.3% of 8.48 M defs) | head | 34.3 | 3,867,967 | 0 | 0 | 604.5 s (cap; load 32–47) | 6,515 | 261 MB | 2.92 GB |
| kernel | 10-minute round: **26,368** of 716,721 (3.7%) — **4.3× fewer rows per round** | **body** | **125.5 (3.7×)** | 3,307,072 | 1,514 (5.7%) | 1,336 | 605.9 s (cap; load 50–52) | 5,479 | 61 MB | 4.42 GB |

Time-to-cover the referenced population, extrapolated from each round's rate (contended
GPU rung, so upper bounds): kernel head ≈ 716,721 × 34.3 / 6,515 ≈ **63 min**, kernel
body ≈ 716,721 × 125.5 / 5,479 ≈ **4.6 h**; cpython head 107 s uncontended (recorded
above) → body ≈ 4.2× ≈ 7.5 min; vorpal head 37.5 s → body ≈ 3 min. On the kernel the
body recipe therefore also changes WHAT a 10-minute round serves: a quarter of the
rows, drawn from the same degree-descending order — the kernel rows below are measured
at those coverages (112,896 head vs 26,368 body), which the earlier coverage curve
(0.345 at 24 K → 0.306 at 168 K, "always-on fill") says is itself a quality variable.

### Query latency vs the shipped binary (uncached one-shot CLI: process start + open + query; median of 3 per query, mean over the NEW set's queries; CONTENDED, load stated)

| corpus | binary / pair | k=10 mean (max) | k=25 mean (max) | load |
|---|---|---:|---:|---:|
| vorpal | shipped, head/head | 0.909 s (1.256) | 1.336 s (1.935) | 46 |
| vorpal | this branch, head/head | 0.895 s (1.252) | 2.051 s (2.788) | 57 |
| vorpal | **this branch, body/head (pin candidate)** | 1.237 s (1.791) | 1.896 s (2.793) | 48 |
| vorpal | shipped, head/head (again, after the above) | 0.767 s (1.324) | 1.358 s (2.104) | 44 |
| cpython | shipped, head/head (during the kernel evals) | 1.152 s (4.404) | 2.160 s (5.690) | 39 |
| cpython | this branch, head/head | 0.386 s (0.667) | 0.831 s (1.293) | 25 |
| cpython | **this branch, body/head (pin)** | 0.358 s (0.537) | 0.722 s (1.032) | 20 |
| cpython | shipped, head/head (again, back-to-back with the pin row) | 0.347 s (0.514) | 0.717 s (1.026) | 19 |
| cpython | shipped with the BODY sidecar in place (stale for it → channel OFF) | 0.326 s (0.493) | 0.713 s (1.016) | 19 |

The same binary and pair measures 0.77–0.91 s (k=10) and 1.34–2.05 s (k=25) run to run
under this load, which is the resolution of the comparison; within it the two-field pair
and the shipped pair are indistinguishable, as the construction says they must be: the
query path encodes ONE prefixed query on the fixed lanes plus the head surfaces of the
cache-missed fused candidates in both cases, and the dense scan is the same int8 scan over
the same row count (the sidecar's bytes are recipe-independent). searcheval's in-process
means (above; the encoder open amortized) agree: vorpal 0.87–1.57 s head/head vs
0.88–1.06 s body/head; cpython 1.13–1.59 s vs 1.15–1.17 s; kernel 2.06–2.24 s vs
2.14–2.27 s. The one-recipe rich pair (body/body) is the latency regression the two-field
split removes: 3.3–5.9 s per query (re-encoding 100–400-token candidate surfaces).

The body recipe's tokens/def is 4.8× head's on the referenced population (the previous
full-coverage A/B measured 3.6×: referenced definitions carry more documentation than the
average definition). Every fill ran on the GPU rung (`wgpu-metal:Apple M5 Max`) — the
extra tokens cost the background fill only; the query path encodes the same head
surfaces as v0.7.0. Sidecar bytes are recipe-independent (rows × (8 + 4 + dim × 3)).

QUIET re-fills of the vorpal sidecar once the other agents' load had dropped (load 11–13,
same rows, same GPU rung; the head row matches the 2026-09-02 "always-on fill" 37.5 s
uncontended figure in tokens/s terms):

| corpus | recipe | fill wall | tok/s | tok/def | peak RSS | wall ratio body/head |
|---|---|---:|---:|---:|---:|---:|
| vorpal (11,751 rows) | head | **19.1 s** | **15,842** | 25.3 | 0.91 GB | — |
| vorpal (11,751 rows) | body | **171.5 s** | 8,275 | 120.6 | 2.80 GB | **9.0×** (4.8× tokens × ~1.9× lower tok/s: attention is quadratic in the sequence, and the 398-token cap lets body surfaces run 4–16× longer than head's) |


### Verdict — the pair pinned, and why

**Pinned: `SurfacePair { sidecar: HeadDocBody, rerank: Head }`** (`body/head`);
`DENSE_DEPTH_FACTOR = 1`. What the measurement says, in the order the brief asked:

1. **The split recovers the rerank's dilution and the latency, exactly.** body/head keeps
   the rerank's surfaces, cache and batch identical to v0.7.0: same exact/subset/short-kw
   rows as shipped on every corpus (vorpal `Postings` stays fused #1), searcheval means
   and one-shot CLI walls indistinguishable from the shipped binary within the run-to-run
   spread of this machine, while the one-recipe rich pair costs 3.3–5.9 s per query.
2. **Paraphrase into k ≤ 25 — partly, and the ceiling is now the fusion's.** Under
   body/head 3 of the 28 answers enter the fused top-25 (vorpal #19, #23; kernel #22),
   against 0 under any head pair; 11 of 28 are inside the dense top-100 (3 before), five
   of them inside the dense top-10 (`word_pieces` #1, `deepcopy` #2, `sysfs_create_group`
   #2, `similar_pairs` #8, `encode_record` #18 → the rerank window). None reaches the
   fused top-10 because a single-list candidate holds at most 1/60 of RRF mass and every
   two-list pair with both ranks < 60 outranks it — proven on `word_pieces` (dense #1 →
   fused #24, `channels [dense#1]`). Dense-list depth cannot change that (bound + sweep:
   ×2 / ×4 / share = +0.002, +0.004, 0.000 all-NDCG on vorpal / cpython / kernel, no
   paraphrase movement); the fusion's handling of single-list dense evidence can.
3. **Kernel gate:** 0.344 (old set) / 0.398 (new set) with the channel ON + rerank at the
   10-minute round's coverage — ≥ 0.313 in both; the coverage-matched head pair is 0.345 /
   0.401 (a wash), channel-only body is +0.021 over channel-only head.
4. **All-NDCG on the shipping configuration is a wash** (vorpal +0.006, cpython −0.007,
   kernel −0.003 coverage-matched; old sets 0.000 / −0.039 on one conjunctive query /
   +0.001) and **channel-only improves on all three** (+0.013 / +0.006 / +0.021). The
   pair is pinned on (1)–(3) plus this: it is the only configuration that gives the fusion
   paraphrase evidence to work with at zero query-side cost.
5. **The cost is doc-side and stated:** 4.2–4.8× tokens per definition, ~4× fewer rows
   per capped kernel round (26 K vs 113 K in 10 minutes; the full referenced population
   extrapolates to ≈ 1.5 h on a quiet GPU rung vs ≈ 25 min under head), peak fill RSS
   2.8 / 5.1 / 4.4 GB vs 0.9 / 1.0 / 2.9 GB. The body clause is a docstring proxy that
   pays per language (Python docstrings and kerneldoc blocks move 10–100×; C functions
   whose first paragraph is code move the other way) — a `HeadDoc`-only sidecar or a
   language-aware body clause is the recorded follow-up if the fill cost matters more
   than the paraphrase evidence on a given corpus. `head/head` remains one env var away
   under `bench-internals`, and a record written under it is served by both binaries.

Reproduction: fill under a pair with `VORPAL_SURFACE_RECIPE=<sidecar>/<rerank> vorpal
__warm-ann <idx> [--dense-budget-timeout 10m]` (the record's `surface` /
`rerank_surface` name the pair; a stash of `ann.dense` + `ann.dense.json` per pair swaps
in without refilling — the rows depend on the sidecar recipe only, so a body/body record
is a body/head sidecar with `rerank_surface` rewritten); `xtask searcheval <idx>
xtask/labels/<corpus>.json [--root <tree>]` under `VORPAL_SURFACE_RECIPE`,
`VORPAL_RERANK_MODE=off` (channel only) and `VORPAL_DENSE_DEPTH=2|4|share`; dense ranks
via `sweep_encoder <idx> --dense-rank <query> <name…>` (bench-internals) under the same
pair env; one-shot walls with `vorpal search <q> -k 10|25 --index <idx>` per binary.
Gate: `cargo test --workspace --release` green (0 failures), both clippy lanes clean.

Not done / open: (1) the fusion-side lever this pass exposes (single-list dense evidence
capped at 1/K) is the fusion owner's — K, a per-channel weight, or a dense-only reserve
slot are the candidates; (2) the kernel's per-round coverage under the rich sidecar is
4× smaller — a per-corpus recipe choice or a `HeadDoc`-only sidecar is the lead if the
fill budget binds there; (3) a language-aware body clause (docstring / kerneldoc only,
never a code paragraph) would keep the C-function regressions on cpython (`mro_implementation`
290 → 2,333) out; (4) every wall-clock figure here is contended — the quiet-machine fill
rates are the two QUIET rows above only; (5) three kernel paraphrase answers are outside
every 10-minute round (unreferenced or too deep in the degree order) and no surface can
reach them — the stop rule / coverage lead recorded earlier.

## Parser swallow recovery — definitions tree-sitter parsed inside an unclosed body (2026-09-03)

**The bug.** tree-sitter-c admits `function_definition` as a block item. When a bare
statement-position macro wrecks a body (`_Py_COMP_DIAG_PUSH`, `scoped_guard(x) { }`,
`#define N(v)` + `N(UP) N(DOWN)`), the parser loses the closing brace, and every later
definition in the file parses INSIDE that body — no ERROR node ever appears at top
level, the byte-ratio health policy calls the file clean (`Objects/object.c`: 0.3 %),
and the pruned item traversal never looks inside a matched item. cpython
`Objects/object.c` was indexed only up to `_PyObject_GetAttrId` (L1267 → EOF; 65 of 142
function heads); kernel `net/core/dev.c` lost everything after `netdev_cmd_to_name`
(L1860 → EOF, `netif_receive_skb` among the losses), `kernel/time/hrtimer.c` everything
after `clock_was_set` (L975: `hrtimer_interrupt`, `hrtimer_start_range_ns`).

**The fix is extractor-side** (memory law: the vendored C grammar stays untouched —
the attribute_macro fix regressed real C at scale). `crates/outline/src/combined_extractor.rs`,
`OutlineItemIter`:

* *Trigger — a parse shape, armed by a grammar fact.* A rule declares
  `swallowRecovery: true` (C and C++ `function_definition`: a node of this kind can never
  legitimately contain another of its kind, so a nested one is parser recovery). A match
  of such a rule is diagnosed when it **carries errors** (`has_error`), **reaches
  end-of-file** — its end is not before the tree root's last non-extra child's end
  (comments are extras; no percentages), and a pruned walk of its body finds a nested
  match of a swallow-root rule: the **floor**. No floor, no swallow — the file's last
  definition with a damaged body extracts exactly as before.
* *Body boundary = the floor.* The floor is the first nested swallow-root match whose
  name is NOT a keyword of its own grammar (`id_for_node_kind(name, anonymous)`) — a
  keyword-named match (`Py_END_ALLOW_THREADS if (x) { … }` parses as a function named
  `if`) is the parser's fusion of a bare macro with the statement after it, a statement
  inside the real body, never the resync point. Chosen over "first ERROR beginning with
  `}`" because the lost brace is usually fused INTO a node (object.c: inside the `if`
  blob's ERROR child; hrtimer: inside the `scoped_guard` blob), not a sibling ERROR. The
  swallower's span is cut back to the latest node ending before the floor.
* *Lifting.* From the floor on, every node is item-matched as if top-level, subject to
  three structural tests: (1) **top-level shape** — walking up to the nearest diagnosed
  swallower (or its body node) meets no swallow-root-kind node and no node of a
  swallower's body kind (locals inside `scoped_guard { }` / `for_each_x(c) { }` blobs and
  bare `{ }` blocks after unparseable heads are never lifted); (2) **not stitched** — no
  direct ERROR child (the object.c blob: `_Py_COMP_DIAG_POP if (!oname) … ERROR(`} int
  next(…)`) { next's body }`); (3) **not keyword-named**. `recoveryOnly: true` rules
  match only here — `c-recovered-variable` is `c-global-variable` minus the
  `not inside compound_statement` guard the mis-nesting defeats. Nested swallows compose
  (the floor only rises).
* *Products* gain `swallows: [(start, lifted)]` (`PRODUCT_FORMAT_VERSION` 19 → 20 — the
  decoders reject trailing bytes so no compatible extension existed; the C rule edit
  re-keys every product through the rules digest anyway, so the bump costs nothing
  extra). `vorpal-index health` prints "swallowed tail recovered: N definitions lifted
  from `f` (line L)" per file and a total in its header. Files where the diagnosis fires
  never enter the walk-reuse fast path (no snapshot captured; a dirty-subtree walk that
  reports one falls back to the full walk) — lifted items live inside a top-level
  subtree, which the region model does not describe. Respan / defs-stable /
  defs-changed gates compare the recovery vector (count + lifted, starts excepted).

**Measurements** (this M-series, `cargo build --release`, corpora read-only, indexes in
scratch, deleted after; base = `afffe07` built from a `git archive` into scratch).

| corpus | nodes before → after | files fired | lifted | product byte-identity oracle (`crates/index/examples/product_hashes.rs`) |
|---|---:|---:|---:|---|
| linux kernel (75,954 files) | 8,890,840 → **8,891,771** (+931) | 25 | 906 | 75,954 bodies compared: changed = fired = 25, 0 unexplained |
| cpython (3,841) | 162,813 → **162,945** (+132) | 3 | 132 | changed = fired = 3, 0 unexplained |
| vorpal (self) | 79,510 → 79,576 | 1 (jemalloc `conf.c`) | 2 | changed = fired + the 12 sources this change edits |

Per file: `Objects/object.c` 100 definitions lifted from `_PyObject_GetAttrId`
(`PyObject_GetAttr` / `SetAttr` / `IsTrue` / `Not`, `PyCallable_Check`,
`PyObject_GenericGetAttr`, `_Py_NoneStruct`, … all present; `vorpal graph callers
PyObject_GetAttr` resolves 16 callers); `net/core/dev.c` 431 from `netdev_cmd_to_name`
(`netif_receive_skb`, `__netif_receive_skb`; `callers netif_receive_skb` resolves);
`kernel/time/hrtimer.c` 59 from `clock_was_set` (`hrtimer_interrupt`,
`hrtimer_start_range_ns`). `kzalloc_noprof` (`include/linux/slab.h:1292`) is NOT a
swallow — a variadic `#define` right after a `static inline __alloc_size(1) void *`
definition, lost to the attribute-macro declarator shape (the closed grammar lead);
recorded, not changed.

**Coverage probe re-read** (`crates/ingest/examples/structural_coverage.rs`, now with an
extraction-aware residual column). The parse-shape count that motivated this work (8,428
kernel files / 5.9 % of bytes; 256 cpython / 18.4 %) over-counts: by swallowing-node
kind the kernel is 5,069 `preproc_ifdef` (header guards with an error inside — the
traversal descends through them, definitions inside were always reached), 2,457
`function_definition`, 349 ERROR, 227 `declaration`, …; of the 2,457 function shapes
2,396 hold no nested function at all (the file's LAST function, damaged, nothing after
it), 29 hold only parenthesized-declarator macro loops (`for_each_x(c) { }`, legit body
constructs), 7 are function-shaped nodes `c-function` never matched (an ERROR `enum`
before the name — the traversal descended into them all along), and 25 fired. The
`declaration`-kind swallows (227 / 12) hold no nested function definition. Extraction-aware
residual after the fix: kernel **0.48 %** of bytes past a swallow start with no item
(from 5.88 % under the parse-shape count), cpython **8.39 %** (from 18.39 %) — the
cpython residual is `Python/generated_cases.c.h` (587 KB of `TARGET()` case blocks, a
file included inside a function) and the `_ssl_data_*.h` tables: no definitions to lift.

**False-positive oracle** (kernel and cpython put every top-level definition at column
0, so a lifted item off column 0 is a candidate false positive): kernel 6 of 906 — all six
real definitions whose `static __always_inline struct sk_buff *` head the parser split
(`sch_handle_ingress`, `tcx_run`, `clock_base_next_timer_safe`), cpython **0 of 132**.
Before the keyword-floor and body-kind rules landed the count was 20 / 966 (kernel) and
69 / 276 (cpython): `Py_END_ALLOW_THREADS if (…)` blobs lifted as functions named `if`,
their followers' locals lifted as globals, `SYSCALL_DEFINE2(…) { struct timespec64 tu; }`
leaking `tu` — the two rules removed every one, and five cpython files that had "fired"
on nothing but such blobs (`_winapi.c`, `timemodule.c`, …) now correctly do not fire.
Parity note: `static DECLARE_WORK(a, b);` mints an empty-named variable in the lift
exactly as the ordinary traversal does at top level (a MISSING identifier) — pre-existing,
recorded.

**Walls** (kernel, quiet machine, `uptime` load < 4 at the cold runs; edit walls on a
scratch copy of the tree): cold **8.24 / 8.83 s** (base 8.21 / 9.05; band 7.9–9.5);
unchanged **0.13 s**; one-shot body edit `kernel/sched/fair.c` **0.51 s**, touch 0.56 s;
body-comment edit inside the LIFTED `netif_receive_skb` (`net/core/dev.c`, a fired file
on the full-walk path) **0.75 s** (base 0.74 s). A head edit that changes a lifted
definition's signature takes 4.25 s — the defs-changed lane declining scoped resolution
("session imports diverge from the carried reach graph") and running the full pipeline;
the control (the same edit on `update_curr_fair` in fair.c) costs 4.18 s on the fix and
4.23 s on base: the lane's own cost for these files, not the recovery's. Under base the
dev.c head edit was 0.76 s only because the definition did not exist.

**Battery** (`scripts/convergence_battery.sh`, ast-grep + cpython, next lane): **PASS**
(scratch determinism + S1–S6 on both). **Ledger** (`--features alloc-ledger`,
`VORPAL_PHASE_TRACE=1`, kernel cold, allocs at `commit: content-id hash start`): fix
7.61 / 7.86 / 8.00 M over three runs, base 7.78 / 7.78 M — within the run-to-run spread
(rayon scheduling), within the ≤ 8.5 M band; `ts_allocs` 16.12 M both.

**Retrieval** (`xtask searcheval`, fresh `--semantic-tier learned` + `__warm-ann`, no
encoder, `VORPAL_NO_AUTOWARM=1`; all-NDCG@10 / MRR / recall@5):

| corpus | base | fix |
|---|---|---|
| cpython (54) | 0.340 / 0.320 / 0.389 | 0.341 / 0.322 / 0.389 (conjunctive 0.157 → 0.176) |
| kernel (54) | 0.313 / 0.303 / 0.361 | 0.315 / 0.304 / 0.361 (short-kw 0.276 → 0.282, subset 0.536 → 0.542) |

The label sets cannot show the recovered symbols: `hrtimer_interrupt`,
`hrtimer_start_range_ns`, `netif_receive_skb`, `PyObject_GenericGetAttr`,
`PyCallable_Check` were DROPPED from the sets because the existence gate failed
(`xtask/labels/*.evidence.md`); every one of them now exists in the index and the label
owner can re-admit them. `kzalloc_noprof` stays absent (above).

**Gates:** `cargo test --workspace --release` green; `cargo clippy --workspace
--all-targets -- -D warnings`, its `--release` twin, and `-p vorpal-py --features python`
clean; outline unit tests pin the object.c shape, the hrtimer macro-block shape, and the
no-swallow last-definition case (`crates/outline/tests/c_family_outline_rules.rs`); the
index-level fixture `crates/index/tests/fixtures/swallow-shape/object_tail.c` +
`tests/swallow_recovery.rs` pins graph presence, call resolution INTO and FROM lifted
definitions, the swallower's cut span, and the health lines. Also fixed on the way:
`vorpal-index health` reported cpython "clean" (591 damaged files) because the bucketed
pack's inferred root declined and absolute graph paths missed relative pack keys — the
report now resolves the root from the graph's paths.

**Not achieved / open:** definitions fused INTO the parser's resync blob (object.c's
`_PyObject_SetAttributeErrorContext`, whose head sits inside an ERROR node) have no node
to lift; swallows rooted in a `declaration` or `struct_specifier` carry no nested
function definitions in either corpus, so nothing arms there; C++ is armed by the same
grammar fact but measured only on the fixture set — the polyglot canary is the
coordinator's at merge; the walk-reuse fast path simply excludes fired files (25 kernel
files) rather than modelling lifted items.

### Landing on main (2026-09-03, `85c14b0` merge → `603f2b5`)

Coordinator's merge gates on the merged tree: workspace 143 suites / 1,321 / 0,
all three CI clippy lanes clean; convergence battery PASS; polyglot canary 15/15
@ HEAD (cold + unchanged exit 0 everywhere; the recovery is visible where C/C++
lives — llvm +929 nodes, cpython 164,045 → 164,182, neovim +151, rust +136,
roslyn +193 — and non-C corpora move only with HEAD drift); CI green on `603f2b5`
(both jobs). Kernel walls: unchanged 0.12 s, one-shot edit 0.50 s; cold measured as
an INTERLEAVED A/B against the pre-recovery binary (`6a1a7c0`), three rounds on a
quiet machine — base 9.58 / 8.91 / 8.88 s vs merged 9.48 / 8.84 / 8.70 s: the
recovery walk costs nothing cold (the day's upper-band readings were thermal). The
first canary pass in the landing chain was VACUOUS (`export -f` + `xargs bash -c`
does not survive this zsh shell; every clone was skipped and "0 failures" measured
nothing) and was rerun with an inline loop — recorded so the shape is not reused.

## v0.7.1 README restamp — the polyglot table, and a "regression" that was a crash (2026-09-03)

The README performance section was re-measured on the v0.7.1 binary against the SAME
pinned commits as the v0.4.0 table (fresh shallow clones fetched by full SHA — GitHub
refuses `--depth 1` fetches of abbreviated ones, which is why an earlier pass silently
drifted every row to HEAD). Quiet machine, cold best of three, load printed beside every
timing. Every row held or moved with the swallow recovery: llvm 8.4 s / 1,444,028 nodes
(+420 at the same pin — C++ sources), zig 6.3 s (+34), kotlin 2.7, kubernetes 2.1, rust
2.7, WordPress 1.9, spark 1.6, kafka 0.7, next.js 1.0, ghc 0.7, rails 0.4, neovim 0.3,
vue 0.1 (all node counts identical to the v0.4.0 table); cpython 1.0 s / 162,945 (+132,
the recorded recovery delta); this repo 7.8 s cold / 79,567 nodes. Self-index parsed
files 1,884 of 2,868 tracked.

**Column semantics fixed.** The old table mixed two counts: cpython's "3,841" and the
kernel's "75,954" were files a grammar PARSED, every other row was `git ls-files`
(so the apparent cpython jump to 6,212 was the tracked count, not a corpus change:
2,343 py + 1,228 rst + 635 h + 479 c + …). The table now reports "Files parsed" for all
rows, read from the indexer's own `parsed N files` line (llvm 86,124 of 183,249 tracked;
kotlin 75,448 of 110,106; roslyn 19,522 of 35,125; kernel 75,954 of 94,843).

**dotnet/roslyn "0.6 s → 2.2 s" — falsified as a regression, and the 0.6 s was never a
build.** Interleaved cold at the exact pin (`4cac4334`): v0.7.1 2.03–2.11 s, v0.7.0
2.10–2.17 s, HEAD-of-roslyn 2.14–2.27 s — flat across releases. A bisect against the
v0.4.0 table's binary named the FIRST commit after v0.4.0, the children-cache
claim-shape guard (`60fdd1f`, 28 lines of C), as "first bad" at 2.15 s vs v0.4.0's
0.55 s. An instrumented build then showed the guard's migration path fires **2 times in
81,009,378 nodes** on roslyn (0 on cpython and the kernel) — it cannot cost anything.
The resolution is the exit code: `/usr/bin/time -l` on v0.4.0 and on HEAD-with-the-
guard-deleted gives **rc=139/138 (SIGSEGV/SIGBUS) at 0.54 s with 6.7 s user CPU** on
5 of 6 runs; the one run that completes takes **2.07 s / 25.1 s user** — identical to
v0.7.1. The v0.4.0 table's 0.6 s was a heap-corruption crash a quarter of the way
through the corpus, recorded by a bench script whose `time -p` wrapper never checked
the exit status (the same bug the guard fixed on llvm/rust, where it was noticed only
because those crashed deterministically). Bisect is only as good as its "good" end:
v0.4.0 was never good on roslyn, it was fast because it died.

Lessons, standing: (1) every timed invocation in a bench harness asserts `rc == 0` and
prints it beside the wall — a crash is faster than any build; (2) confirm the "good" end
of a bisect by running it, not by assuming the recorded number; (3) recorded polyglot
rows carry the count rule they use. Cleanup: five bench worktrees + target dirs and the
15 corpus clones removed; memory notes updated.

**Daemon rows re-measured WITH the always-on dense sidecar** (the README latency table's
encoder rows were the pre-sidecar floor). Protocol: fresh index per (corpus, tier) with
`semantic.tier = learned` and a per-index `encoder.dir`, `__warm-ann --dense-budget-timeout
10m` (wgpu-metal GEMM; cpython 35,364/35,364 and this repo 11,785/11,785 referenced filled
to completion in 6.6 / 2.9 min, kernel 40,704–44,800 of 717,369 at the cap), then 30
stdio round-trips per tool on a quiet machine. Kernel f16 96 ms median / 485 p95 /
first 0.69 s / 2.8 GB; f32 94 / 356 / 0.62 / 2.7 GB; cpython f16 36 / 263 / 0.38 s /
719 MB, f32 35 / 245 / 0.40 / 677 MB; this repo f16 35 / 276 / 0.42 / 623 MB, f32 34 /
244 / 0.42 / 603 MB. Versus the no-sidecar rows: +3–5 ms median, p95 flat-to-lower,
+40–80 MB RSS on the small corpora. Two harness lessons: (1) the learned tier is selected
by `<root>/semantic.tier` (written by `semanticTier:` in vorpalconfig.yml) and a warm
without it silently builds the lexical tier under a "learned" label — the first pass of
this bench did exactly that and was discarded; (2) the very first daemon process pays a
one-time page-in of weights + index (kernel f16 "first search" 4.8–5.3 s on two separate
first-of-batch runs, 0.69 s on every rerun) — order the batch or state the cache state.

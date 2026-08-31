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

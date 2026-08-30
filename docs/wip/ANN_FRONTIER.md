# ANN frontier synthesis — 2026-08-30

Seven parallel research sweeps (DiskANN-family build engineering; kNN-first constructions;
build fidelity + entry points; quantization frontier 2023-2026; distance-computation
pruning; frontier graph structures; incremental maintenance) distilled into one program.
Context: n≈2.34M × 256d L2-normalized lexical-hash vectors, per-row-scaled i8 + SDOT,
Vamana R=32 α=1.2 l_build=48, build ≈135 CPU-s / ~8.5s wall, pool recall 0.9125
(top-10 ⊂ l=200 visited pool, 32 probes; exact rerank + rank fusion downstream).
Quality is the bar: pool recall must go UP, never traded away. Byte-deterministic builds.

## Calibration facts the sweeps established

- We already hold ParlayANN's structural wins (prefix-doubling batch build, counting-scatter
  back-edge transpose, append-if-fits reverse merge) and are ~5× more CPU-efficient than
  their published throughput at matched work. Remaining gains are algorithmic/memory-shape,
  not parallelization.
- The build is memory-latency-bound: ~10⁹ distance evals whose cost is dominated by random
  256B row fetches (600MB pool ≫ LLC), not SDOT arithmetic (~2.5-3ns compute vs ~10-25ns
  effective). Byte reduction and layout beat kernel swaps.
- Incremental insertion structurally caps early nodes' candidate quality (FastKCNA k-CNA
  ≤0.5 expected) — the theory behind refinement passes and our batch-cap gap.
- Classic triangle-inequality pruning is dead at 256d (measured 0.08% eliminations);
  FINGER-style per-edge sketches conflict with a mutating build graph.

## The program (each increment gated: pool recall ≥ prior, retrieval_eval, determinism
## A/B — two builds byte-identical, full suites, clippy; ANN is a lazy sidecar so
## generation ids never move)

### Tier 0 — build engineering, quality-invariant semantics
1. Deterministic instrumentation: dist-eval + expansion counters (pure functions of
   input+binary — the contention-immune A/B metric), VORPAL_PHASE_TRACE-gated.
2. Lazy memoized occlusion in robust_prune (DiskANN-Rust `prune.rs` structure: per-candidate
   occlude_factor cache + last_checked resume, first-occluder early exit) — same selected
   set for a fixed α sequence, 30-60% fewer prune-side evals.
3. Batched expansion: collect unvisited survivors, prefetch all rows, then one interleaved
   4-8-chain SDOT batch (memory-level parallelism; today's pipeline is 1-deep).

### Tier 1 — graph quality at fixed R (the priority)
4. Round-size cap = n/50 (ParlayANN batch truncation): today's final round inserts HALF the
   corpus against a frozen graph, forfeiting every same-batch true-NN edge. Cap → recall up,
   modest time cost; ParlayANN reports parity-with-sequential under the cap.
5. Progressive α inside the prune (cur_α = 1.0 → ×1.2 → α): admit strict-RNG edges first,
   relax only to fill — diverse-but-close slots. Production-DiskANN standard.
6. Saturate-to-R: after α-prune, fill remaining slots with nearest unselected candidates —
   denser navigable graph at zero extra distance evals.
7. In-degree floor post-pass: count in-degrees, force-attach starved vertices (< derived
   floor) into their nearest neighbors' lists via α-eviction — protects the weakly-referenced
   tail that pool recall measures.
8. Final refinement round (two-pass Vamana, k-CNA-grounded): re-search + re-prune every node
   against the FINISHED graph, deterministic reverse merge. +0.5-2pt pool recall at +40-70%
   build — paid for by Tier 0/2 savings.

### Tier 2 — rotation + 1-bit traversal tier (quality up AND build/search down)
9. Seeded blocked fast-Walsh-Hadamard rotation (±1 diagonal → FHT-256 → fixed permutation,
   2-3 rounds; exact power-of-two scales; seed in the index header) and re-encode the
   existing i8 tier in the rotated domain: strictly tighter distance fidelity at identical
   bytes (Weaviate-RQ measured +1-5 recall pts vs plain SQ8; ExRaBitQ error 1.3-3.1× lower
   than LVQ). Zero storage cost, format version bump.
10. 1-bit RaBitQ side-tier (32B codes + 2 f32 factors ≈ 100MB — largely LLC-resident at our
    scale): unbiased estimator with error bound; traversal reads 32B not 256B. Search: beam
    steering on the 1-bit tier, exact i8 on expansion + pool rerank (the architecture we
    already have — SymphonyQG-shaped). Spend the 2-3.5× as l=200→300+ at iso-latency →
    pool recall up. Build: candidate beams on the tier, ALL prune comparisons exact i8
    (approximate traversal is safe; approximate pruning is where quality dies — Weaviate's
    −3.2%, QuIVer's cliff). Expected build 135 → 60-90 CPU-s at equal-or-better graphs.
    Escape hatch: B=2 (+75MB) halves the estimator error.
11. Conditional (profile-gated): FastScan packed-adjacency blocks (codes-alongside-edges,
    SymphonyQG proper, +~2.4GB) only if the flat tier still shows miss-bound traversal;
    ExRaBitQ-4 middle tier (nibble-split) as the rerank-fidelity middle rung.

### Tier 3 — daemon incremental tier (kills the ~8.5s per-generation rebuild)
12. FreshDiskANN-consensus design, adapted: tombstone bitmap + slot versions; per-edit
    micro-inserts (one beam+prune each, ~60-120µs); same-edit opportunistic repair (the
    edit's own insert beams walk the damaged region); Alg-4 consolidation at ≥1% dead /
    probe-drop / 15min; α=1.2 in EVERY repair path (the no-decay condition, flat over 50
    churn cycles in FreshDiskANN's data); the existing canonical rebuild stays as compactor
    + reconciliation anchor (oplog replay = bit-exact state identity). ~15-25ms CPU per
    edit; staleness bounded by one edit-apply instead of 8.5s.

### Measured on-corpus since synthesis
- Round-size cap n/50 (Tier-1 item 4): **REJECTED by gate** — dist evals 2.71B → 3.97B
  (+46%), vamana build ~9.7s → 13.5-18.3s, pool recall 0.9125 → 0.9094. ParlayANN's cap
  guards against quality loss vs sequential; our uncapped build already matches sequential
  quality on this corpus, so the cap only added work. Counters (deterministic dist-eval /
  expansion totals) landed and are the standing A/B instrument.

- Progressive α (1.0→1.2, DiskANN occlude_list semantics) + saturate-to-R (Tier-1 items
  5-6): **BOTH REJECTED by gate.** Progressive alone: pool recall 0.9125 → 0.9062; with
  saturate: → 0.8562 (and +20% evals). Geometry explanation: lexical-hash vectors form
  near-duplicate clusters (similar identifiers), and the pool metric rewards retrieving
  those twins — strict-RNG-first selection evicts exactly them, and saturate back-fills
  dominated edges that displace nothing useful. Production-DiskANN defaults do not
  transfer to this embedding geometry; single-α=1.2 stands. (Lazy memoized single-α prune
  kept — sha-identical, structural win.)

- Refinement round (Tier-1 item 8): **ADOPTED** — pool recall 0.9125 → **0.9812** (+6.9pt,
  the campaign's largest quality move); unreachables 66k → 31k pre-repair; fused output
  visibly stronger. Cost 8.8 → 19.4s vamana (Tier-2 is the claw-back).
- In-coverage repair (Tier-1 item 7, in-degree-0 form): **ADOPTED** — 66k structurally
  unreachable nodes rejoined; probe recall unchanged; correctness-shaped.
- Two-pass expansion (Tier-0 item 3): **ADOPTED**, sha-pinned inert; wall within noise
  (256B rows = 2 lines mute the MLP win); kept as the 1-bit tier's required shape.
- Rotation on the i8 tier (Tier-2 item 9): **REJECTED by gate** — pool recall 0.9812 →
  0.9469. Mechanism: the pool oracle lives in the quantized domain, so i8-fidelity gains
  don't score; meanwhile Gaussianizing the spiky lexical coordinates removed natural
  navigation signposts and made the ANN problem harder. `rotate_row` retained (tested,
  deterministic) as the 1-bit tier's required foundation — sign-bit estimators are only
  unbiased AFTER rotation.

### Scoreboard after the first implementation day (2026-08-30)
| metric | session start | now |
|---|---|---|
| pool recall (l=200, 32 probes) | 0.9125 | **0.9937** (refine×2, saturated: ×3 identical) |
| structurally unreachable nodes | 66,257 silently invisible | **0** (repair pass) |
| vamana build (kernel, 16 cores) | ~8.6s | ~21-23s; phase telemetry: insertion ~8.1-8.4s, refine ~6.5-7.0s each (x4 kernel −8-10% on search phases) |
| determinism | sha-stable | sha-stable + dist-eval/expansion counters as the noise-immune A/B instrument |

CORRECTION (phase telemetry, supersedes the fa9ef31 commit message): refine pass 2 costs
the SAME as pass 1 (~7.5s), not ~2.4s — the earlier figure was wall-clock variance read as
mechanism. The +1.25pt from pass 2 is real and saturating (×3 identical), but paid at full
price. Refinement passes do not get cheaper on refined graphs.

Refinement-cost negatives (all gated, all reverted): warm-start seeding (own out-edges +
medoid) → 0.9344 AND slower — the medoid APPROACH PATH is the refinement's candidate-
diversity source, and local seeds evict the medoid from the beam before it expands;
substrate 2/3·l_build with refine×1 → 0.9281 @13.7s; with refine×2 → 0.9438 @19.1s —
the insertion pass's fidelity is load-bearing, refinement polishes but does not replace it;
chunked refine passes (8 sequential frozen chunks) → pass-1 cheaper (7.6s) but recall
0.9844 and pass-2 stayed expensive — smaller merges see fewer competitors and keep
different edges: the frozen-round symmetry (every node against the SAME complete graph)
is load-bearing, the third independent confirmation of that law.

- 1-bit steering, FLAT-SCALAR variant (Tier-2 item 10, first cut): **REJECTED by gate on
  both axes** — build 19.5 → 31-37s AND pool recall 0.9812 → 0.9469 (deterministic, sha
  ×2). Post-mortem, quantified: (a) a scalar 4-plane popcount estimator is ~40 scalar ops
  vs SDOT's 16 vector instructions at dim 256 — the 32B-vs-256B memory win is swamped by
  compute, per-insert query construction (dequant+rotate ≈ µs × 4.7M), and exact pool
  re-scoring; (b) steering noise costs ~3.4pt pool recall — the same figure as the
  i8-rotation experiment, i.e., estimator-ordered beams visit measurably worse pools at
  fixed l on this geometry. CONSEQUENCE for the plan: the SymphonyQG numbers are only
  reachable as the FULL package — NEON TBL FastScan over 32-candidate blocks, codes packed
  INTO the adjacency rows (one stream per expansion), multiple-estimates admission, and l
  reinvestment — a Branch-scale project, not an incremental tier swap. Implementation of
  the flat variant preserved in git history (reverted commit range noted in the log).

- Refine-HALF (insertion-order prefix, mechanism-derived cost cut): **REJECTED** — pool
  recall 0.9031 (below even the unrefined 0.9125) and unreachables ballooned to 122k.
  Mechanism: partial refinement redistributes edges toward the refined half; merge prunes
  on their targets evict back-edges the UNREFINED half depended on, orphaning it. Full
  refinement re-balances symmetrically — the 19.5s build is the honest price of 0.9812.

- Interleaved x4 SDOT kernel: **ADOPTED, sha-pinned** — four candidates' misses overlap;
  refine phases 7.2-7.9 → 6.5-7.0s. Remaining kernel headroom (query-in-registers full
  unroll at dim 256) judged diminishing; the structural time lever remains the FastScan
  package below.

- Tier-3 incremental overlay, T3a — **LANDED** (`vorpal_ann::AnnOverlay`): immutable base
  tier + tombstones + appended rows + per-node adjacency patches; every repair path prunes
  with the build's α=1.2 (the FreshDiskANN no-decay condition); deletes tombstone in O(1)
  and keep ROUTING (removal-from-routing is the documented collapse); searches never
  return tombstones; deterministic replay pinned by test. Kernel-scale probe (2.34M rows):
  insert 184µs, delete 88ns, search(l=80) 140µs, adopt 52ms per generation. A 100-del +
  100-ins edit ≈ **18.5ms CPU vs ~330 CPU-s full rebuild (~18,000×)**. Churn test: 10
  cycles × 5% delete+insert holds pool recall within ε of start.

- Tier-3 daemon wiring, T3b — **LANDED** (2f08282, `crates/index/src/live_ann.rs` +
  mcp/server lifecycle). `LiveAnnTier` re-keys the committed tier by durable eids
  (node-identity lo-64) so it survives per-generation dense renumbering; per commit the
  daemon tombstones removed eids and re-embeds added ones from the current graph (the
  eid-churn ledger in `ingest/retained.rs`; the overlay BUILD's replay churn is drained —
  it once handed the first edit a 2.3M-row "update"). Serving proposes candidates only
  (eid → current id, unknown drops); rerank/filters/fusion identical to every tier.
  Hard-won lifecycle laws, each from a measured failure:
  * **Stale-tolerant adoption**: on an edited tree a classic warm can never land fresh
    (bootstrap race, 4 rebuilds/51s observed) — adoption reconciles ANY persisted tier
    through `ann.files` (remap / sentinel-tombstone / insert), 2.34M rows in ~200ms.
  * **Provenance travels with the tier**: generation carry-forward must include
    `ann.model.json` — daemon-committed generations lost it and every provenance-gated
    consumer rejected reconcilable tiers forever (the "adoption failed every generation"
    failure).
  * **A live-ANN task in flight counts as tier-present** for warm suppression: updates
    OWN the tier while running; treating the window as tier-less fired a full rebuild
    per edit (527 CPU-s/6 edits observed).
  * **Adopt-first at every kg-servable site**, warm as fallback: reap-failure requests
    the warm, a per-generation latch (cleared by reap_warm — warms rewrite in place)
    bounds attempts; boot warm yields when the tier looks reconcilable.
  Kernel-scale daemon validation: tier ready 3.2s after first query, ZERO full builds
  across boot + 6 edit cycles (was 4-6), daemon 19.9 CPU-s/6 cycles (was 496.6 — 25×),
  per-edit update −145/+115 rows in 130-150ms off the serve path, live-tier semantic
  search 52ms finding a just-added symbol. Compaction: dead_fraction > 5% retires the
  tier, the classic warm rebuilds densely, adoption re-keys the fresh tier.

Next up: FastScan-packed SymphonyQG layout as a dedicated branch-scale effort (design
above). T3 follow-ups: quality-probe cadence on the live tier (probe machinery exists),
persistence policy for long-lived overlays (warm-as-compactor already wired).

### Rejected with cause (recorded so we do not re-litigate)
- l_build 48→32: pool recall 0.9125→0.7781 measured — quality bar violation.
- kNN-first wholesale (NN-descent/RNN-descent/HCNNG): 10-16× more distance evals; honest
  ceiling 1.3-2.5× via GEMM shaping; worse risk profile than Tier 2 which composes.
- FINGER / per-edge sketches (mutating graph + memory 3-4× + GloVe-200 recall collapse),
  classic triangle inequality (0.08%), PDX vertical layout for graph random access (authors'
  own limitation), TRIM-during-build (42-60% build tax), patience/saturation-stop in build
  (measured ~1pt recall loss), HNSW++-style approximate beam ordering (trades our currency),
  RoarGraph (near-IID queries: no win, big build tax), DEG continuous refinement (hours-scale
  stochastic optimizer, determinism-hostile), LeanVec/PCA-nav (flat lexical-hash spectrum is
  its failure mode — revisit only if the measured spectrum is skewed), anisotropic PQ/SOAR
  (benefits accrue to quantized-final-score systems; α-RNG pruning already IS the graph
  analog of SOAR's orthogonal redundancy), NF4 (rotation manufactures the Gaussian premise,
  then a uniform grid is optimal), plain unrotated i4 (dominated by rotated 4-bit).
- Medoid micro-optimization: no literature support for build-cost sensitivity among
  near-central starts; our 8.1↔10.4s swing was machine noise (counters now decide).

Full agent reports live in the session task outputs; citations inline there (ParlayANN
PPoPP'24, DiskANN cpp+Rust sources, FastKCNA PVLDB'25, SymphonyQG SIGMOD'25, RaBitQ
SIGMOD'24 + Extended SIGMOD'25, LVQ VLDB'23, ADSampling/DADE/DDC + ICDE'26 benchmark,
FreshDiskANN/IP-DiskANN/CleANN, GASS SIGMOD'25, CAGRA ICDE'24, Flash SIGMOD'25,
EnhanceGraph, τ-MNG SIGMOD'23, Weaviate/Elastic/Milvus/Qdrant/Vespa engineering).

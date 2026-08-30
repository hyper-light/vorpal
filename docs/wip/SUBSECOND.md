# The Sub-Second / Sub-Millisecond Plan

> Synthesis of seven research passes (2026-08-29): two code deep-dives (replay/apply, link/seal/save),
> a Vamana build anatomy, and four literature surveys (incremental-index prior art, fast ANN builds,
> memory-first architectures, cache-hierarchy engineering). This document is the single reference
> for the incremental-latency campaign. Measured baseline, Linux kernel corpus (72,541 files,
> 2.75M nodes, 6.8M refs, 18-core M-series): cold index ~7.0s · one-file incremental ~1.6s ·
> unchanged ~0.10s · vector-tier build ~14s.

## The three latencies (they are different products)

| Latency | Today | Target | Owner |
|---|---|---|---|
| Query-time freshness overhead (unchanged tree) | 2.8µs | keep | daemon watcher (done) |
| **Edit → queries reflect it** | ~1.6s | **sub-ms typical** | memory-primary daemon (Phase 3) |
| Edit → canonical bit-identical generation | ~1.6s | ~0.1s no-op edits · ~0.9s semantic (Phase 0/1); ~0.1–0.25s with format v-next (Phase 4) | the pipeline = the compactor |

Nobody at scale rebuilds durable artifacts in the foreground (Glean stacks, stack-graphs
per-file rows, Zoekt delta shards, rust-analyzer pure-memory). The disk pipeline's job changes:
it stops being the edit path and becomes the **background canonicalizer**. The determinism
contract survives intact — every *committed* generation remains content-addressed and
byte-identical to a from-scratch build; live overlays are ephemeral, query-equivalent, and
compact to canonical form (the ANN tier already ships this exact contract).

## Design rules (standing)

1. **Determinism is the asset.** Early cutoff / carry-forward is *sound* because builds are
   deterministic ("inputs byte-equal ⇒ outputs byte-equal" is a theorem here — Build Systems à
   la Carte's precondition for constructive traces). Every optimization is gated on: streamed≡batch
   byte-identity, content-id A/B vs a frozen baseline binary **at fixed tree state** (mtime
   discipline — a `touch` between runs invalidates the comparison), retrieval_eval,
   resolution_eval, full suite.
2. **Platform-agnostic, correctly.** Portable baseline always present; platform fast paths behind
   cfg/runtime dispatch with bit-exact-vs-baseline tests (the `dot_i8` pattern). Integer kernels
   with fixed summation shape so an index built on ARM equals one built on x86. Cache-line pads
   via `crossbeam_utils::CachePadded` (already per-arch aware: 128B on aarch64, 64B on x86).
   Hardlink-with-copy-fallback. No fork-based snapshots (epoch read-views instead). No
   macOS-hugepage assumptions (unsupported on Apple Silicon; keep the Linux hugepage policy in
   `mem::store`).
3. **Hardware/data-derived parameters, never constants tuned to a benchmark.** Constants become
   policies: ANN build fidelity self-calibrates against an exact oracle; committer/worker split
   derives from the replay-vs-parse ratio; batch caps scale with cores; prefetch distance from
   detected line size; dirty-set fallback thresholds as measured fractions. Derivations must be
   deterministic (seeded, pure functions of input) and stamped into provenance.
4. **No fake edges — in the live view too.** Overlay resolution uses retract-then-rederive of
   whole dirty name-buckets (the join is non-monotone under scope precedence; never patch
   edge-by-edge). Pathological fan-out edits mark buckets *pending* (IntelliJ dumb-mode
   honesty), never stale.

---

## Phase 0 — Bit-identical pipeline & ANN surgery (no format change, no contract change)

Target: incremental 1.6s → ~0.85–1.0s; cold improves too; ANN build 14s → ~5–7s. Everything in
this phase must produce byte-identical artifacts (content-id A/B) — these are removals of
redundant work and parallelizations of provably-order-free steps.

### 0.A Stream phase (replay ~700ms → ~120–200ms)

The measured shape: 18 workers do a byte-scan; 9 committers do all real work (decode, intern,
~9.55M blake3, 36M column pushes); 1 absorber does O(output-bytes) serially. `KgWriter::absorb`
reads exactly two scalars (id_base, heap_base) — **absorb is associative**, and the bases are
prefix sums (the `SymbolTable::from_shards` counting-scatter pattern, already in-tree and tested).

- **A1. Compute `layout_entity_paths` once per file** (it runs twice: `ingest_file_with_spans`
  and `local_layout` — ~5.5M redundant `format!` allocations per replay).
- **A2. Kill the per-reference blake3**: `ingest_file_with_spans` already returns
  `Vec<(Range, NodeId)>` in layout order; index it by `from_entity_index` instead of
  re-hashing `entity_id(path, entity)` 6.8M times.
- **A3. Committer count derived** from the replay share of the run (replay-heavy → threads;
  parse-heavy → threads/2), env-overridable.
- **A4. Apply-on-workers**: per-file `KgWriter` built on the 18 workers (pooled in
  `ExtractScratch`), committers reduce to sequence-ordered `absorb`. File-scoped canonical
  index is already the semantics (`forget_identity_scope` per file). Drop the worker-side
  `validate_product` (decode *is* validation) — decode moves to the worker.
- **A5. Prefix-sum scatter absorb — TRIED 2026-08-29, NO BENEFIT, REVERTED.** Implemented in
  full (absorb_batch with parallel disjoint-region scatter, positioned heap + 34-byte-record
  spill writes); bit-identical and all oracles green, but wall time did not move: after A4 the
  copies already overlap the admission window, the live absorber sees size-1 batches (the
  trickle pattern), and the tail is small on incremental runs. Interleaved A4-vs-A5 duel split
  within thermal noise. Do not re-attempt without first changing the completion pattern
  (e.g. committers handing off partial shards).

### 0.B Tail (link/seal/save ~840ms → ~600ms)

- **B1. blake3 `update_rayon`** in `SegmentBuilder::build` (tree hash ⇒ same digest), parallel
  per-column xxh3 + minmax, borrow instead of `to_vec` (kills a full column copy).
- **B2. names.idx**: parallel build via `node_name` (not full `NodeView`), bulk two-column
  write via `cast_slice` (replaces 5.5M eight-byte `write_all`s).
- **B3. Path-intern memo** in the symbol-table shard loop (2.75M interns → ~72k; rows are
  contiguous per file); same memo for the owner `peek`.
- **B4. Evidence vec reserved** from `spill.count()` (~330MB grown from `Vec::new()` today);
  dedicated spill-reader thread so the sink thread only drains in order.
- **B5. `rayon::join` the CSR/CSC builds**; parallelize the `group` count pass (scatter stays
  ordered).
- **B6. Manifest scan micro**: `entry.metadata()` (not a second stat), per-thread vecs merged
  at end, drop the per-entry lossy String.

### 0.C Interner & symbol-table cache surgery (from the cache report)

- **C1. Hash-once interner**: one deterministic fixed-seed hash (foldhash::fast::FixedState or
  equivalent) supplies both the shard bits and the probe hash via `hashbrown::HashTable` —
  removes the double hash (~18–24ns → ~3ns/call; shard_of still runs SipHash today even after
  the FxHash by_text swap). NameId values are process-private; artifacts must not depend on
  them — **gated by content-id A/B** (empirically, not just by argument).
- **C2. `CachePadded` interner shards** (112B shards currently share 128B lines — cross-shard
  false sharing on every lock word RMW).
- **C3. Symbol-table `ranges` → dense direct-index**: `NameId` *is* {shard, dense index} — a
  perfect hash we mint ourselves. `HashMap<NameId,(u32,u32)>` becomes per-shard flat tables:
  no hash, no probe, L2-resident. Same for `files`.
- **C4. CSR `row_offsets` u32 in memory** (u64 stays on disk — artifact bytes unchanged).

### 0.D ANN build, bit-identical subset (14s → ~7–9s; also speeds queries)

- **D1. Frontier cursor** in `greedy_search` (the unexpanded-scan restarts from 0 every
  expansion — ~1.7×10¹⁰ wasted flag scans per build).
- **D2. Pool `beam`/`visited`** per task (the `stamp_pool` pattern) with real capacities.
- **D3. ParlayANN batch merge**: per expansion, collect candidates, sort, one `set_union`
  splice into the beam — replaces up to R sorted-array inserts (the 17% memmove).
- **D4. Parallel CSR-transpose back-edge merge** (counting scatter keyed by target, batch
  order preserved per target) + `ArrayVec<u32, R>` proposals — removes the ~1.5–2.5s
  single-threaded HashMap merge (the measured WAIT) and ~300MB of peak.
- **D5. Prefetch correctness**: prefetch the *whole* row portably (stride-64 loop — harmless
  over-prefetch on 128B-line machines), batch-prefetch the first ~8 neighbor rows on node pop
  (lookahead 4–8, not 1), prefetch the visit-mark slot alongside the codes row. The Apple DMP
  does not chase computed `base + id*stride` addresses — software prefetch is load-bearing.

Gates for Phase 0: content-id A/B (linux + cpython) vs the frozen baseline binary; `ann.bin`
SHA A/B for 0.D; streamed≡batch; full workspace suite; retrieval_eval; resolution_eval.

## Phase 1 — Early cutoff & O(changed) commit (additive, contract-preserving)

- **1a. Product-equality cutoff — LANDED 2026-08-29.** Kernel touch class: 1.05s → **0.21s**
  ("content-unchanged — restamped"); comment/whitespace edits included (the stamp window
  [8..32) — size/mtime/xxh3 — is patched into a pack clone; everything else must be
  byte-equal). Gate: the committed generation's content id equals a from-scratch build's
  (pinned by crates/index/tests/stamp_cutoff.rs, plus live kernel A/B). Racy-mtime hazard
  scoped to stat-UNCHANGED files (changed files are re-extracted — strictly stronger).
  Original design: (Bazel change-pruning / salsa backdating): after re-extracting
  the changed file, if the new product bytes equal the cached ones (comment/whitespace/touch
  edits — a large real-world class), the from-scratch build differs only in `manifest.bin`:
  hardlink the other seven artifacts from the prior generation, rewrite the manifest, fold the
  content-id from cached digests. **~50–100ms for that entire edit class**, identical by theorem.
- **1b. `digests.bin` sidecar**: persist the per-artifact chunk-digest folds the commit already
  computes → commit hashing cost becomes O(changed artifacts). Backfillable, self-validating,
  `VORPAL_VERIFY_CACHE`-style full-rehash mode retained.
- **1c. Journal handshake**: the daemon watcher keeps (clock → changed-path set); a CLI/daemon
  build asks "since clock C" and stats only those (fresh-instance fallback = today's full
  sweep). Additive manifest patching; deletes owned by the reconciliation scan cadence. The
  hint can only *narrow* the stat sweep — digests remain the identity.

## Phase 2 — ANN adaptive fidelity & incremental consolidation

- **2a. LANDED as measurement-only (2026-08-29) — the ladder is dead, the oracle lives.**
  The escalation ladder was implemented and the kernel measurement killed it: pool-recall@10
  at the production search contract is **0.9156 at the historical l_build=48** (lower rungs
  lower still), so no absolute floor below ~0.92 is reachable and the ladder just built the
  graph 2-3× for the same final rung (20.9s vs 9.8s). What ships: every Vamana build runs the
  seeded exact oracle (32 probes × n integer dots, ~30ms) + one production-shaped
  pool-recall measurement, stamped into `ann.calibration.json` — a per-corpus honesty stamp
  and the foundation for any future adaptive policy (a prefix-build predictor is the viable
  successor, not an absolute floor). ann.bin remains bit-identical to the historical graph;
  build time unchanged (9.2-9.6s cooled). Original design:
  `QuantMatrix::from_rows` (which touches every row anyway), score Q seeded probe queries
  exactly (~50ms parallel at kernel scale). Build at the derived floor (scaled from n, degree
  budget, cores), measure pool-recall@K through the production search path against the oracle,
  escalate one rung and rebuild if below floor (rare, bounded, deterministic). Chosen
  parameters stamped into `ann.model.json`. Shipped defaults elsewhere (cuVS L=64/R=32, Faiss
  efC=40) justify starting the ladder low; the floor is *measured*, so a hub-heavy embedding
  space that needs more, gets more. Decouple `pool_cap` from `l_build` first.
- **2b. Batch-cap policy**: prefix-doubling cap derived from core count (ParlayANN θ=0.02n;
  cuVS ships 0.06n) — recall-gated by 2a's probes.
- **2c. Approximate visited filter** (ParlayANN: 28–44% on beam-heavy phases) — deterministic
  (fixed hash), changes graph bytes ⇒ lands only with 2a's gate green.
- **2d. Deterministic FreshDiskANN-style consolidation**: inserts run through the existing
  batch-propose/sequential-merge machinery over the overlay set; deletes via the local
  neighbor-patch (a pure parallel map over affected nodes). At 5% churn this is ~10–13% of a
  rebuild (~0.5–2s instead of 14s). α=1.2 (already ours) keeps recall flat across churn
  cycles. New contract: lineage stamped in the v5 header; incremental-mode reproducibility
  test (same edit sequence ⇒ same bytes); generation-boundary full rebuilds remain the
  canonical reset. `ann.bin` is already outside the content-id — this is legal today.

## Phase 3 — Memory-primary daemon (the sub-millisecond product)

The daemon's RAM becomes the source of truth; disk becomes a cache of memory.

- **Frozen base**: the loaded generation's SoA/CSR, immutable between compactions — zero
  synchronization, torn reads structurally impossible.
- **Delta overlay** (MB-scale), left-right double-buffered: tombstone bitmaps, append-only
  node/edge arenas (LLAMA-style delta adjacency), patched name-bucket entries. Single writer;
  wait-free readers; the 2× memory cost applies to megabytes, not the GB base. No Arc-per-object.
- **Edit transaction**: watcher paths → re-extract the file (tree-sitter *incremental* reparse;
  clangd-style LRU of retained trees — trees are a convenience cache, products are durable) →
  product diff → retract old contributions (tombstones) + insert new → recompute dirty name
  buckets *in full, in canonical order* (refs from the file; refs to names whose candidate set
  changed — including the `insert_if_referenced` admission flips; import-binding dependents) →
  single epoch publish. Typical budget: re-extract 10–100µs (warm tree) + bucket rederive
  µs–ms + splice µs ⇒ **sub-ms typical, bounded by dirty-set size worst-case**.
- **Fan-out escape hatch**: dirty scope beyond a derived threshold ⇒ buckets marked *pending*
  (resolve-on-demand or report unresolved-pending) — never stale edges.
- **Durability = product cache as journal** (VoltDB command-logging, degenerate case): recovery
  loads the last generation (snapshot), diffs its manifest against the tree, replays newer
  products / re-extracts missing ones. No new journal. Product writes stay tmp+rename.
- **Compactor**: pins an epoch (no fork; Tarantool read-view style), runs the Phase-0/1
  pipeline in background, swaps CURRENT. Compaction input is the *set* of live products —
  never edit order — so the emitted generation is bit-identical to from-scratch.
- **Correctness harness — LANDED 2026-08-30** (crates/index/tests/differential.rs): 24
  seeded random edits over a synthetic corpus spanning all seven edit classes (body edit,
  add/remove function, add/delete file, comment restamp, pure touch); after EVERY step the
  incremental index must equal a scratch build on two oracles — generation content-id
  (total, byte-level) and a rendered-answer battery over six graph verbs + hybrid search
  (the oracle that survives into the overlay era, where live bytes may differ but answers
  may not). Runs in <1s; extend with overlay-vs-scratch comparisons when the live view
  lands. Debug-mode left-right copy-compare still pending with the overlay itself.

- **Live rebuild v1 — LANDED 2026-08-30** (`build_index_live` + `PendingPersist`): the
  stepping stone to the overlay. A watched semantic rebuild now runs the real pipeline to
  the sealed in-memory `Kg` and **serves it immediately**; the persistence tail (evidence +
  segment saves, manifest, content-id hash, generation commit) moves to a daemon background
  thread executing the *exact* synchronous code — so the committed generation is
  byte-identical (pinned by crates/index/tests/live_build.rs, plus the differential
  harness). Ordering discipline: one background committer at a time — the sync path drains
  it, the serve-immediately probe defers to it (serving stays provably correct meanwhile),
  the explicit `index` tool drains both committers, and generation-bound tools
  (`fetch_span`, `why`, `search`, `health`, …) drain before pinning `kg_dir`; navigation
  tools serve from the sealed graph at full speed during the window. Kernel, live daemon:
  semantic edit→answer 1074→965ms median; steady 0.02–0.06ms. Honest negatives: adopting
  the sealed graph instead of re-`Kg::load`ing saved only ~30ms (mmap load was never the
  cost), and the deferred tail bought ~110ms — the remaining ~950ms is compute
  (replay+link), which is exactly the Phase-3 overlay's target, not a persistence problem.
- **ANN warm hygiene — LANDED 2026-08-30**: eager warms are now single-flight and
  coalescing (an edit burst costs at most one running + one trailing warm, not one
  9-second, core-saturating build per commit), `VORPAL_NO_AUTOWARM=1` actually disables
  eager warming (it previously did nothing), and the boot-time warm resolves the
  generation directory (it silently never fired under the `gen/<id>` layout).

- **Live overlay v1 — LANDED 2026-08-30** (`vorpal_index::live::LiveOverlay` over
  `vorpal_ingest::RetainedIndex` + `vorpal_resolve::RefStore` + `KgWriter::seal_canonical`):
  the daemon retains the post-absorb pipeline state; a small semantic edit tombstones the
  file's row/heap/edge/ref footprints, re-applies its product at the tail, and re-links only
  derived state (masked canonical-order table, alive-range resolution feed, canonical-order
  seal) — no corpus replay. Sealed bytes are byte-identical to scratch (three independent
  pins: canonical_seal, retained equivalence vs the Ingestor pipeline, live differential
  through the daemon), so the background canonicalizer commits the very generation the
  served answers came from. Kernel, live daemon: semantic edit→answer 1074→**497ms median**
  (476–502ms typical; occasional ~1s guarded fallback when a committer overlaps); overlay
  construction 0.7s background (batched parallel decode+ingest, serial absorb); steady
  0.02–0.06ms. Env hatch: `VORPAL_NO_LIVE_OVERLAY=1`.
  Bugs the oracles caught, recorded for the overlay era: (1) resolution EMISSION order
  follows feed order — the retained ref feed must walk canonical file order, not append
  order (graph.bin diverged); (2) `absorb` never advances the writer's canonical index, so
  mixing absorb-based and define-based applies hands out node ids from 0 against tail rows
  — every retained apply is now absorb-based; (3) an overlay builder spawned while a
  commit was in flight reads stale CURRENT and resurrects retired rows (deleted symbols
  reappeared) — builders spawn only from post-commit sites, enforced in
  `spawn_overlay_build` itself. (4) A RAM-served `Kg` had no names.idx and paid a ~20ms
  full scan on every named query — `Kg::build_names_index` now stamps served graphs.
  Remaining serve-path spend (~500ms kernel): masked table + full resolution (181ms) +
  canonical seal gather; next levers are scoped (dirty-bucket) resolution and a
  parallel/zero-copy seal gather.
- **Scoped rederive — LANDED 2026-08-30** (same day, three follow-on commits): per-file
  resolution buckets (edges in emission order + evidence + stats, writer-id space); applies
  diff definition rows by durable eid (name/kind/exported/owner-eid) into a pending-scope
  lattice (Clean ⊑ Scoped ⊑ Full; import wiring → Full); link expands dirty names through
  reference postings, re-resolves only those buckets, and heals every untouched bucket in
  place by chasing dead edge/evidence targets through the eid repair map — an unrepairable
  target outside the dirty set recomputes in full, loudly. Serve-path link also stopped
  materializing the evidence sidecar (~7M discarded clones, ~100ms). Kernel scoreboard,
  semantic edit→answer: 1074 (session start) → 965 (deferred persist) → 497 (overlay v1)
  → 401 (coalesced feed + parallel gathers) → 342 (scoped rederive: "1 dirty file" for a
  local edit) → **251ms median**. Steady 0.02–0.06ms; touch/comment ~8–15ms.
  Remaining 251ms floor: symbol table full rebuild ~69ms, qualified seed + repair scan
  ~40ms, edge LUT remap ~31ms, Graph::compact ~33ms, names index ~20ms, extract/apply
  ~11ms, probe/glue ~30ms.
- **Persistent symbol table + three-track seal — LANDED 2026-08-30 (same session):**
  `RetainedSymbolTable` erases the interner brand for storage (the table holds interned IDS
  only; rebind confined to the originating interner) and maintenance is SPLICE, not
  rebuild-from-definers — a dirty name's run keeps unedited files' id-stable symbols and
  swaps only the edited blocks' contributions (telemetry killed the definer-scan design:
  145 dirty names touched 46k definer files via hub statics like s_show). Admission flips
  are non-events (the polluted-marks table is total). The canonical seal then fans out
  three tracks under rayon::join — segment build, in-memory name index straight off the
  gathered name column, edge remap + compaction — critical path = longest track. Kernel
  scoreboard, semantic edit→answer: … → 251 (evidence skip) → 185-207 (table splice) →
  **157-170ms warm** (parallel tail), then shared extraction probe (one extraction serves
  the serve-immediately check AND the overlay absorb) + parallel repair scan →
  **127-137ms warm, 135ms median on a CLEAN machine** (every earlier number ran under 2-3
  cores of fseventsd/syspolicyd churn from the disk cleanup; the churn also explained a
  15s-convergence flake — FSEvents delivery starvation, deadline now 30s). Session arc:
  1074 → 135ms (8×). Remaining floor ≈ apply/diff ~15 + seed ~10 + longest seal track ~50
  + probe ~10 + glue. Next levers: Phase-4 file-local ids (kills the remap + renumber),
  retained persist (kills the canonicalizer's ~1s background CPU per edit).
  Superseded design note (kept for the record): store name/path/
  owner as u32 bits (no interner lifetime), maintain per-name candidate lists through the
  def-postings (rebuild only names defined by edited files, in canonical file order),
  reset import bindings per link, patch surviving candidate ids via the repair map. The
  catch to gate carefully: DenseRanges' flat candidate layout is deliberate (cache-dense
  full-link resolution); per-name indirection must NOT regress the cold/full path —
  resolve-eval + full-link A/B before adopting. Backlog: parallel edge remap (~15ms),
  probe↔overlay double extraction (~10ms), retained persist to retire the canonicalizer's
  ~1s background CPU per edit, def-postings for repair-scan narrowing.

- **ANN build findings — 2026-08-30 (negative results, recorded):** the build decomposes
  as quantize 81ms / oracle 51ms / **vamana graph ~8.1-8.7s** / recall 6ms — the graph IS
  the build, and its distance kernel is already SDOT-vectorized, so remaining levers are
  algorithmic. (1) `l_build` 48→32: -35% build time but pool recall 0.9125→0.7781 and >half
  the fused top-10 lines changed (visibly worse hits) — REJECTED, quality is the bar.
  (2) Parallelizing the medoid centroid pass (f32 or f64, deterministic fixed-chunk folds):
  picks a slightly different medoid; recall −0.3pp on the probe set, and build-time effects
  were unmeasurable under machine contention (identical-byte builds varied 8.2→10.6s) —
  REVERTED, no proven benefit. Kept: build-phase telemetry (VORPAL_PHASE_TRACE `[ann]`
  stamps). Open quality-safe research items: multi-entry-point starts, partitioned builds,
  PQ-fidelity build passes — all recall-gated, none free.

## CLI cold build — profile findings 2026-08-30 (clean-ish machine, `sample` 4s mid-parse)

The "85% parse-bound" belief is STALE. Current cold build (6.5-7.0s wall, ~99 CPU-s,
kernel) buckets: **allocator/memmove/mmap 26.7%**, **vorpal extraction code 20.9%**
(extract_references 447 samples, combined_extractor OutlineItemIter 371, extract_product
279, TsPre traversal 219, KgWriter::define 208), **tree-sitter CURSOR WALKING 15.6%**
(goto_first_child/sibling/child_iterator + ts_node_child_with_descendant 462 — repeated
descendant seeks), rule matching (Matcher::match_node_with_env 895 + relational Has 302),
and only **8.2% actual parse+lex**. RawVecInner::finish_grow hot (Vec growth churn), heavy
rallocx traffic. Attack list (quality-free, determinism-pinned): (1) allocation churn —
capacity reservations, per-thread scratch reuse across files, bump-arena per file for
extraction temporaries; (2) rule dispatch — per-language kind→rule bitmap prefilter if
matching is tried per node×rule; (3) single-pass traversal — eliminate re-seeks behind
ts_node_child_with_descendant; (4) background tree drops (ts_subtree_release 4.2%).

## Phase 4 — Format v-next (canonical semantic edits at 100–250ms)

The consensus lesson from Glean/SCIP/stack-graphs/Kythe: identity must be file-local or
content-derived, never globally sequential. One coherent format revision:

- Node identity `(file_key = xxh3(path), local_ordinal)`; artifacts become
  header + TOC + N fixed buckets (bucket = f(file_key)), files canonically ordered within
  buckets — one edited file rewrites one bucket segment per artifact + TOC.
- Bucketed `products.pack` (today a one-file change rewrites ~the whole pack).
- Persisted symbol table (generalized names.idx) with per-product Sorbet-style def-hashes and
  a `usage.idx` (name → posting list of file_keys) for dirty-name scoping; scoped re-resolve
  with a derived fallback threshold to the full re-link (both paths land on the same bytes).
- Merkle commit over per-segment digests.

From-scratch and incremental builds emit identical bytes *by construction*. This phase also
makes the Phase-3 compactor itself O(changed buckets). Nightly CI keeps the golden check:
scratch id == incremental id on the kernel tree.

## Execution order & gates

Phase 0 chunks land independently, each gated (streamed≡batch, content-id A/B, ann SHA A/B,
retrieval/resolution evals, full suite): 0.B tail → 0.A stream → 0.C cache surgery → 0.D ANN.
Then 1a/1b/1c (new convergence tests), then 2 (recall gates + new ANN lineage tests), then 3
(differential harness gates every merge), then 4 (format-version bump, migration test matrix,
nightly golden convergence). Perf numbers recorded per chunk in docs/wip/BENCHMARKS.md
methodology: release builds, best-of-3, fixed tree state, thermal notes.

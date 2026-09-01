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

**Fresh attribution 2026-08-30 (leaf-weight `sample`, current binary — SUPERSEDES the
26.7% allocator figure above):** tree-sitter runtime 83.9% of on-CPU (parse+lex+stack
~30%, cursor walking 26.2%, subtree machinery ~10%), vorpal extraction code 5.3%,
allocator only 4.2%, memmove 2.0%. jemalloc floor probes measured DEAD: decay 10s trades
+0.4 GB RSS for −2.3 s sys but wall holds ~6.6-6.8 s; narenas 8→16 within noise — the
config has no juice, the cost is call count. **Landed from the cursor bucket:** the
references fused walk now maintains an explicit ancestor stack driven by the traversal's
own depth (`PreWithDepth` in core) — `Node::parent` has no parent pointer and re-walks
from the tree root per call (`child_with_descendant` per level), and stage_type_use paid
up to three of those per type leaf. Bit-identical output (same generation content-id),
fused-vs-reference battery green, cursor bucket 26.2% → 22.2% (~2.5-4 core-s). The
remaining bucket is legitimate semantic walking (field lookups, children scans) — no
single lever; the parser proper (~30%) is the floor at this design point.

**Cold-build allocation ground truth (jemalloc stats_print, one cold kernel build):**
211.5M slab mallocs / 361M total requests in ~6.5s (~32M allocs/sec). Dominant bins:
size-112 → 40.2M nmalloc; size-96 → 15.2M nmalloc but **187M requests** (tcache-served);
size-80 → 6.7M, size-32 → 3.9M, size-16 → 3.6M. These counts fingerprint tree-sitter's
per-subtree heap objects (SubtreeHeapData-class sizes), NOT our extraction Vecs — and the
29.7k parse-error files' error recovery likely amplifies subtree churn. Levers (we vendor
the runtime): (a) thread-local size-bucketed freelist behind ts_malloc/ts_free (ts_free is
size-less → 8B header or usable-size query); (b) per-parse bump arena reset at tree drop
(lifetime audit needed: parser-persistent allocations must not land in it); (c) jemalloc
tcache tuning as a zero-code floor probe. Allocation COUNTS are the contention-immune
gate metric; wall-clock confirmation deferred to a quiet machine.

## CLI one-file incremental — phase attribution 2026-08-30 (kernel, 1.07-1.13s wall)

Honest breakdown of the streamed replay path (VORPAL_PHASE_TRACE, one C file edited):
manifest stat sweep **153ms** (already 16-way parallel ignore-walker with per-thread
flush — near the macOS stat floor at 72k files); stream admission+replay **363ms**
(72,540 pack products through zero-copy view decode+apply at ~5µs/file, 18-way);
absorb tail **174ms** (sequential single-writer splice — the determinism anchor);
resolve **174ms** (2.17M references, parallel); seal 43ms; evidence+kg save 82ms
(concurrent); content-id hash 33ms; commit/pack tail ~100ms. Every bucket is parallel
and tight; the stateless CLI's floor is ~1s because it must re-APPLY every product and
re-link the whole graph — the daemon's retained state is the designed escape (135ms
edit→answer), and patchable sealed columns are the Phase-4 format question below.
Also fixed the same day: daemon↔CLI alternation used to re-parse the world (path
spelling split, see "one tree, one spelling" commit) — interop now replays 72,540/72,541.

## CLI edit-one floor 2026-08-31 — the replay wall (Phase-4 motivation, measured)

Post-merge grind took kernel edit-one 3.1 s → 1.67 s: the space-invariant run-order pick
removed ~1.35 s of hub-name tie-break comparisons, the near-clone pairing now spawns at the
stream tail (overlapping the pack tail, cochange, table, and resolution — it was the link's
critical path by ~110 ms), and the pack consolidation tail joins after the link instead of
before it (riding into `PendingPersist` on the live path). Remaining layout: 713 ms product
replay (75,953 unchanged files re-applied because one edit renumbers every later id),
243 ms resolve, ~145 ms seal, ~250 ms artifact writes + content hash, 140 ms stat scan.
The replay is the design law's price at CLI grain — sub-second edit-one is Phase 4's
file-local ids, not a tuning pass. (The daemon already serves the same edit at 0.57 s
steady; restamp-class saves ~10 ms.)

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

### Phase-4 execution decomposition (2026-08-31) — gated slices, each landing alone

- **P4.0 — identity spike (no format change):** `FileKey = xxh3_64(tree-relative path)` +
  `LocalOrdinal(u32)` types; a shadow map (file_key, ordinal) ⇄ dense id built at seal and
  pinned bijective at kernel scale. Collisions are DETECTED at build and fail loudly with
  the colliding paths (same posture as the u32 ceilings) — never probabilistically ignored.
  Gate: shadow-map bijectivity + zero byte movement anywhere.
- **P4.1 — bucketed `products.pack` (pack v2):** header + TOC + B buckets,
  bucket = file_key mod B, files canonically ordered inside buckets. B derives from the
  corpus at build (target bucket size from a recorded two-scale sweep — vorpal repo and
  kernel — per the no-magic-constants law). One edited file rewrites one bucket + TOC.
  Reader keeps v1 for one release; writer emits v2 behind `VORPAL_FORMAT=next` until the
  flip. Gate: roundtrip, single-bucket rewrite proof, determinism ×2, kernel identity A/B.
- **P4.2 — bucketed node segment + heap:** per-bucket `nodes.vseg`/`strings.heap` slabs
  with a TOC; DENSE ids stay the runtime currency (prefix sums over bucket counts at load),
  so every in-RAM consumer — CSR, ANN rows, evidence lookups, tools — is untouched. The
  format stores locals; the runtime derives dense. Gate: v2 load ≡ v1 load in RAM,
  byte-for-byte, plus artifact determinism.
- **P4.3 — reference-bearing artifacts on (file_key, local):** THE open design tension,
  decided by measurement not taste: naive 8-byte endpoints double edge storage
  (~144 MB kernel); candidate layouts are (a) per-bucket edge slabs with src implicit and
  dst as key+local varint, (b) a per-generation sorted file_key→slot table with slot-local
  endpoints and TOC-driven slot repair on file add/remove. Prototype both on the kernel,
  record size/load/query deltas, then commit. Evidence/dataflow follow the winner.
- **P4.4 — Merkle commit:** per-bucket digests in each TOC; the generation id hashes the
  TOCs. Commit cost becomes O(changed buckets); the stamp-only cutoff and retained persist
  reuse unchanged digests. Gate: id equality against the full-rehash oracle on every path.
- **P4.5 — scoped re-resolve on the CLI path:** `usage.idx` (name → file_key postings)
  generalizes the daemon's dirty-name machinery to disk; a CLI edit re-resolves only dirty
  buckets, with the derived escalation threshold falling back to the full re-link — both
  paths land on identical bytes (the differential harness gates every scale). This is the
  slice where the 100–250 ms edit lands; nothing before it changes user-visible latency,
  everything before it removes the risk.

Standing rules for every slice: the nightly golden (scratch id == incremental id, kernel)
never regresses; v-1 readers stay supported for one release per format-bearing slice; no
slice ships with a constant that wasn't swept at two scales and recorded here.

#### P4.1 resolved design (recon 2026-08-31, from code facts)

- **File-per-bucket, not regions in one file.** A single bucket-major file still rewrites
  everything after the first changed bucket (≈half the pack on average) and can never
  hard-link unchanged bytes across generations. `products/<k>.pack` (k = `file_key & (B-1)`,
  zero-padded name) + `products/toc.bin` gives O(changed buckets) writes, and unchanged
  bucket files HARD-LINK into the next generation (immutable once sealed; rename-over never
  writes through a link; GC of old generations is refcount-safe). The one-file v1 pack is
  exactly the degenerate B=1 case, which is how the shared code paths treat it.
- **toc.bin is the v2 `products.idx`.** `VPPT` header, B, per-bucket {entry count, byte
  length, xxh3 digest} (the digest column is P4.4's Merkle spine, landed now because the
  writer is already streaming the bytes), then the slot table (path, bucket, body span) —
  the existing `PathSrc::Sidecar` machinery IS this design; slots grow a bucket index.
  Buckets land tmp+rename, toc last; on toc/bucket mismatch (killed legacy-mode run) the
  reader rebuilds slots by scanning the self-describing records — v1's recovery posture.
- **Pack keys become tree-relative (the P4.0 spelling law applied to storage).** v1 embeds
  absolute canonical paths, so today's pack bytes — and therefore generation content-ids —
  are mount-dependent, and a moved tree cannot reuse its own product cache. v2 stores
  tree-relative spellings; `PackReader::open_rooted(dir, root)` strips incoming absolute
  paths at the API boundary. Build/daemon sites pass the canonical src they already hold;
  query surfaces handed only an index dir derive the root EXACTLY from the generation's
  own manifest (`open_generation_pack`: every entry must strip to a pack hit and the
  counts must match — acceptance is provably unique, and failure degrades to rootless
  misses, never wrong bytes; suffix-guessing was rejected because a same-suffix twin can
  byte-verify wrongly). Senders are untouched.
- **B is a pure function of the tree** (`clamp(next_pow2(files / TARGET), B_MIN, B_MAX)`,
  constants from the two-scale sweep below): stamping B at creation would make an
  incremental that grows past a threshold diverge byte-wise from scratch, violating the
  convergence law. Crossings are log-spaced and cost one v1-style full rewrite — the price
  v1 pays on EVERY edit.
- **Stamp-only cutoff stops copying the pack.** Today it `fs::copy`s the whole pack and
  patches 24-byte stamp windows. v2: buckets with no patched files hard-link; buckets with
  patches copy-then-patch (never patch through a link — prior-generation bytes are
  immutable, and the inode oracle asserts it).
- **Enumeration sites** (`GENERATION_ARTIFACTS` is a flat 9-name list): content-id, export,
  import validation, commit keep-list, cutoff staging all learn one predicate — flat name
  OR `products/` member, walked in sorted name order so ids stay deterministic.
- **Migration cliff is one pack write, not a re-extract:** the first `VORPAL_FORMAT=next`
  build reuses v1 bodies through the format-sniffing reader and emits v2.

#### Recorded sweep — content-id fold chunk (`HASH_CHUNK`), 2026-08-31

`HASH_CHUNK` is a fold-protocol constant (chunk boundaries shape the digests the id folds,
so it must be machine-invariant; changing it re-keys every id, absorbed as one re-commit
per tree). The inherited 8 MiB was never swept; this run froze **1 MiB**.

Command: `cargo run --release -p vorpal-index --example content_id_sweep -- <gen-dir> 3`
(M-series laptop, quiet, artifacts page-cache-hot — the production shape: the id is
computed immediately after the artifacts are written). Best-of-3 wall:

| chunk   | kernel v1 flat (1.56 GB) | vorpal v1 flat (517 MB) | vorpal v2 bucketed (517 MB) |
|---------|--------------------------|--------------------------|------------------------------|
| 256 KiB | 16.38 ms                 | 5.66 ms                  | 5.65 ms                      |
| 512 KiB | 16.58 ms                 | 5.79 ms                  | 5.80 ms                      |
| **1 MiB**   | **16.32 ms**         | **5.61 ms**              | 5.85 ms                      |
| 2 MiB   | 19.76 ms                 | 6.63 ms                  | 6.65 ms                      |
| 4 MiB   | 21.19 ms                 | 7.21 ms                  | 7.19 ms                      |
| 8 MiB (old) | 24.01 ms             | 8.35 ms                  | 8.05 ms                      |
| 16 MiB  | 24.78 ms                 | 8.61 ms                  | 8.61 ms                      |
| 32 MiB  | 25.82 ms                 | 8.71 ms                  | 9.14 ms                      |

256 KiB–1 MiB tie within noise on every shape; 1 MiB takes the plateau with 4× fewer
per-chunk digests than 256 KiB. The 8 MiB rows reproduced the committed generation ids
exactly (sweep tool ≡ production fold). The parallel fold itself is schedule-only: the
serial fold consumes per-artifact digests in fixed name order, so ids are unchanged by the
restructure — proven by the old-binary/new-binary A/B committing identical `f3d02ef3…` on
the same tree.

#### Recorded sweep — bucket-count law (`BUCKET_TARGET_FILES`), 2026-08-31

Kernel tree (76 868 manifest files), release binary, `bucket-sweep.sh` (scratchpad; per B:
scratch v2 build, one-file edit to `mm/slab_common.c`, incremental build, inode-diff of
bucket files across the two generations). v1 baseline on the same tree and probe:
edit-one **1.40 s** (whole-pack rewrite: 726 MB + sidecar).

| B (forced) | edit-one wall | buckets rewritten | bucket bytes rewritten | toc bytes |
|------------|---------------|-------------------|------------------------|-----------|
| 256        | **0.43 s**    | 1                 | 2.60 MB                | 4.16 MB   |
| 1024       | 0.54 s        | 1                 | 0.56 MB                | 4.17 MB   |
| 4096       | 1.24 s        | 1                 | 0.11 MB                | 4.24 MB   |

Two corrections discovered post-sweep, recorded because the numbers above must be read
with them:
- **The sweep's "edit-one" rows measured the STAMP-CUTOFF path**, not the full pipeline:
  the probe was a comment append at EOF, which is extraction-identical outside the stamp
  window. The per-B ORDERING stands (the pack cost — links + rewritten buckets + TOC — is
  the same shape on both paths), and the cutoff is a real, common edit class; but the
  labels here mean "cutoff wall".
- The sweep ran with the data volume near-full (≤12 GiB free of 7.3 TiB — a later v2 cold
  hit ENOSPC), which inflated v1's row (`fs::copy` is an APFS clonefile — near-free with
  headroom, degraded at 100% full). Healthy-disk walls below supersede the absolute
  values.

The single-bucket law held at every B. Wall is not byte-dominated in this range: past
~100 files/bucket the per-bucket costs (hard-links, opens, mmaps at reader open) beat the
byte savings — B=4096 (19 files/bucket) loses to B=256 (300 files/bucket) by 0.8 s while
writing 23× fewer bucket bytes. Frozen: `BUCKET_TARGET_FILES = 512`
(kernel → next_pow2(76 868/512) = **256**, its measured optimum; vorpal repo → MIN-clamped
16, healthy in the vorpal-scale runs), MIN = 16, MAX = 4096 (the `{:04}` naming bound and
comfortably past the measured over-bucketing cliff).

Cold A/B (interleaved v1/v2, same tree, after freeing the volume to ~27 GiB — the sweep's
own cold columns were disk-pressure artifacts, 31.7–48.8 s with one ENOSPC):

| rep | v1 cold | v2 cold (law B=256) |
|-----|---------|----------------------|
| 1   | 7.46 s  | 7.43 s               |
| 2   | 7.38 s  | 7.58 s               |

The bucketed publish is free at cold scale (≤2%, within rep noise): same spool, same
bytes, 256 sequential bucket streams + a 4 MB TOC in place of one pack stream + sidecar.

Healthy-disk kernel walls, both edit classes, interleaved (the honest P4.1 scoreboard):

| class                        | v1          | v2 (law B=256) |
|------------------------------|-------------|-----------------|
| cold                         | 6.74 s      | 7.02 s          |
| stamp-cutoff edit (comment)  | 0.24 s      | 0.32 s          |
| full-pipeline edit (new fn)  | 1.66–2.05 s | 1.73–1.76 s     |

Reading it straight: **on APFS, P4.1 is wall-neutral today** (±0.1–0.3 s, inside the rep
noise) because (a) `fs::copy` clones — v1's cutoff "726 MB copy" was already a metadata
op here, and (b) the full-edit pipeline's writes land in page cache without fsync, so the
726 MB pack rewrite was only ~0.1 s of the 1.7 s wall, which is dominated by the ~900 MB
of still-monolithic artifacts (evidence 362 MB, nodes 179 MB, heap 154 MB, graph 154 MB,
names 46 MB) — P4.2/P4.3's lane — plus link/resolve.

What P4.1 buys, measured and structural: real write bytes per edit drop 726 MB → ~2.6 MB
+ 4.2 MB TOC (SSD wear, battery, write contention); on NON-reflink filesystems (ext4 — CI
and most Linux hosts) v1's cutoff and full edit pay the 726 MB in real bytes and v2's
hard-links make both O(changed); pack bytes are mount-invariant (a moved tree reuses its
own products); and the per-bucket digest columns are the spine P4.4's O(changed) Merkle
commit and P4.5's scoped re-resolve stand on.

Kernel identity A/B (the convergence law at scale, across the format flip): scratch v2 of
the final tree == v2 reached incrementally through a v1 prior → migration build with an
edit → revert build (hard-link carries throughout) — both committed
`gen/7a2dd70ca166b3dcbfe103bd738f24cf`. And the flat lane never moved: the pre-P4.1 and
post-P4.1 binaries commit the same id on the same tree (`f3d02ef3…`), so the default path
is byte-preserved through the whole slice, content-id refactor included.

P4.1 status: CERTIFIED (workspace clippy, python-feature clippy, 131-suite release run
all green; pack unit ladder, pack_v2 end-to-end, live_differential_v2 daemon pin, kernel
identity A/B). The v1 read path retires after one release per the standing rule; the flip
itself (default VORPAL_FORMAT) waits for P4.2+ so the whole revolution moves together.

#### P4.2 resolved design (recon 2026-08-31, from code facts)

Targets the next two monoliths: nodes.vseg (179 MB kernel) + strings.heap (154 MB),
rewritten in full on every edit today. Under `VORPAL_FORMAT=next` they become
`nodes/<k>.vseg` + `nodes/<k>.heap` + `nodes/toc.bin` (one TOC row per bucket: rows,
vseg len/digest, heap len/digest — same bucket law and file_key as the pack).

- **Node slab bytes are id-free.** The 14 node columns are per-file data plus one derived
  size column (`scc_size`); dense ids live only in graph.bin/names.idx/evidence. So under
  bucket-major canonical order, adding a file moves OTHER buckets' id bases (TOC prefix
  sums) without touching their slab bytes — the property that makes per-bucket carry
  possible at all. `scc_size` stays in-slab; carry eligibility hash-compares the built
  bucket against the prior TOC digest, so a cross-file cycle change honestly rewrites the
  buckets it touched (exactness over dirty-set assumptions).
- **Bucket-major canonical order ships here** (locked P4 decision 3). The batch pipeline
  gets it for free: ingest order IS canonical order, so the stream walks manifest entries
  through a (bucket, tree-relative path) permutation under v2 — the manifest FILE stays
  path-sorted (two-pointer diffs, racy windows untouched). The retained tier's canonical
  order centralizes in one comparator (`canonical_blocks()`), which the ledgers, chains,
  similar inverse, and edge-log law all follow — scratch ≡ incremental holds WITHIN each
  format; ambiguous-pick winners may move once at the migration (the pick law is
  run-order-relative by design; answer oracles adjudicate).
- **Split-at-save, not split-at-seal.** The seal keeps building one global segment + heap
  (zero change to the serve path); `Kg::save` under v2 derives bucket boundaries from the
  sealed columns, slices per bucket, rebases the three heap-offset columns to LOCAL,
  builds per-bucket segments in parallel (`SegmentBuilder::new(0)` — no id base in slab
  bytes), hash-compares against the prior TOC, hard-links unchanged / writes changed.
  Load maps every slab zero-copy with local offsets — NO rebasing at load — through the
  already-multi-segment `SegmentDirectory` (§9.2 was built for this).
- **Kg goes multi-segment uniformly**: `segments/cols/heaps` become parallel vectors
  (length 1 for v1 and for sealed in-RAM graphs — the directory-routed accessors are
  shared). The whole-graph stripe APIs (`kind_tags`, `content_hashes`) become per-segment
  stripe iterators — scans stay contiguous within each slab; the six call sites convert.
  The ANN/postings freshness stamp becomes the ordered xxh3 fold over slab bytes
  (bit-identical to today's value for single-segment graphs).
- Gate: NodeView/name-index equality oracle between a v1 and v2 generation of the same
  tree (every id, every field), nodes/ single-bucket rewrite inode proof, determinism ×2,
  cutoff links all node slabs, daemon pin (live_differential_v2), kernel identity A/B.

#### P4.2 results (2026-08-31)

All gates green. Kernel (76 868 files, B=256 → 513 node-store members):

- **One-file edit: the node store rewrites 1.20 MB and hard-links 510 of 512 slabs**
  (the edited bucket's vseg+heap pair) — 333 MB of per-edit writes become ~1.2 MB.
  Cumulative with P4.1: real write bytes per full edit drop from ~1.6 GB (v1) to
  ~530 MB (evidence 362 + graph 154 + names 46 + manifest — P4.3's lane) + ~8 MB of
  bucketed slabs and TOCs.
- Identity A/B PASS through edit + revert (`gen/32798d84…` from both lanes); the
  cross-format truth oracle holds (flat and bucketed generations of the same tree
  describe the identical node universe, every field + scc, as canonical sets).
- Walls: full edit 1.76/1.85 s, cutoff 0.51/0.37 s — same classes as P4.1 (APFS;
  the remaining monoliths and the content-id rehash dominate). Cold: tight alternating
  pairs after settle read flat 7.44/7.43/7.41 s vs v2 7.52/7.60/7.53 s — the
  split-at-save (256 parallel slab builds + digests) costs **+0.12 s (+1.6%)** at
  kernel scale. (Two earlier lone samples at 8.7–15.7 s were ambient — the concurrent
  editor session at 160% CPU; alternation bounds it.)
- Both daemon differentials green: the retained tier seals in the SAME bucket-major
  order through one comparator, and its background committer lands the scratch
  generation id under either format.

#### P4.3 resolved design (recon 2026-08-31 — the decomposition's tension, decided)

Targets the remaining monoliths: evidence.bin (362 MB kernel), graph.bin (154 MB),
dataflow.bin, names.idx (46 MB).

- **Endpoint coding: explicit `(bucket u16, local u32)` — 6 bytes, no packing.** A packed
  u32 (12-bit bucket | 20-bit ordinal) would put a silent 1M-ordinal ceiling under any
  generated megafile; explicit fields never error and keep rows FIXED-WIDTH, which the
  evidence binary-search lookup requires. Evidence rows grow 36 → 38 bytes; `from`
  drops to a bucket-local ordinal (slabs are per-SOURCE-bucket, so the id base never
  appears in slab bytes). The alternatives pool entries grow 4 → 6.
- **Edge truth moves to `edges/<k>.bin`** (per-source-bucket, emission order): the CSR is
  exactly reconstructible from per-src slab order (Graph::compact is an insertion-stable
  per-src scatter — the interleaved global log order is not needed at runtime, only each
  source's restriction of it, which is what a slab stores). 12 B/edge fixed:
  `[src_local u32][dst_bucket u16][dst_local u32][etype u16]`.
- **graph.bin becomes a DERIVED CACHE** (the ANN precedent): same bytes, same mmap load
  when its stamp matches; excluded from the content id; rebuilt from the slabs when
  stale/missing. Cold opens keep today's zero-cost path; the 154 MB leaves the edit path.
- **Dirty-slab law = digest comparison, never dirty-set reasoning**: build every slab's
  bytes (parallel, in-RAM), hash, link-or-write against the prior TOC — the P4.2 carry
  law verbatim. Position-independence bounds the honest rewrite set: an edit in bucket k
  rewrites bucket k's slabs plus exactly the buckets whose edges point at ordinals that
  MOVED inside k (reverse-dependency locality), never everything.
- names.idx: measured decision at implementation — demote to a stamped derived cache
  (regeneration law shared with postings) or leave in-identity; 46 MB/edit rides on it.
- dataflow.bin follows the evidence coding (same row surgery, tiny file).

#### P4.3 results (2026-08-31) — including a prototype rejected by measurement

The decomposition's endpoint-coding tension was settled the way it demanded: prototype,
measure, commit. **`(bucket, bucket-local)` (6-byte destinations, 36-byte rows) FAILED at
kernel scale** — a one-file function append shifts bucket-mates' ordinals, and incoming
references cascade globally: evidence carried 6/257 slabs (370 MB rewritten), edges
128/257 (53 MB). Recoded to the P4.0 identity **`(file_key u64, ordinal u32)`** (12-byte
destinations, 42-byte evidence rows, 18-byte edge rows), anchored by the node-store TOC's
new FILE TABLE (v2: per file `{key, dense start, rows}` — `NodeIdMap`, the one dense⇄key
map every coded family shares):

| family    | size (kernel) | one-file append edit          |
|-----------|---------------|-------------------------------|
| evidence  | 466 MB (+28%) | **255/257 linked, 2.0 MB**    |
| edges     | 154 MB (+50%) | **255/257 linked, 0.6 MB**    |
| nodes     | 335 MB        | 510/513 linked, 2.7 MB        |
| products  | 730 MB        | 255/257 linked, 6.8 MB        |

Truth-writes per kernel edit: ~900 MB of monoliths → **~12 MB of slabs + TOCs**. The two
derived caches (graph.bin 146 MB, names.idx 46 MB) are now the dominant residual writes —
written eagerly for query UX, excluded from identity, P4.5 revisits skipping them on the
scoped path. Walls: full edit 1.95–2.03 s, cold 8.78 s (≈ +6–8% vs pre-P4.3 under a
noisy machine; the digest-compare law builds all slab bytes to prove them unchanged —
~200 ms honest cost, with the P4.5 dirty-set as the recorded hook). Identity A/B PASS
through edit+revert. Bulk endpoint conversion goes through a lazily built dense table
(binary-search-per-endpoint measured ~0.6 s/edit and was fixed before landing); the
cache-miss graph rebuild densifies through a transient key map.

CSC law shipped: under the bucketed format the seal builds the CSC over the src-major
enumeration (`Graph::compact_src_major`) — exactly the slab concatenation — so a
slab-rebuilt graph is bit-identical to the sealed one and the daemon's in-RAM graph
equals the loaded one. E2E: cache delete → identical answers → lazy re-cache, proven.
dataflow.bin stays global and in-identity (0.8 MB; bucketing buys nothing measurable).

#### P4.4 results (2026-08-31) — the Merkle commit

For bucketed generations the content id folds the small fixed set {manifest.bin,
dataflow.bin, the four family TOCs} — the TOCs pin every member's digest, so the id
covers the same bytes transitively while the commit reads **~13 MB instead of ~1.5 GB**.
Dedup-guard soundness holds by construction (writers compute TOCs from member bytes);
adversarial member tampering is caught where trust is needed (import verifies raw
digests per artifact; loaders verify lengths and family self-checks). The agreement
oracle rides the e2e: generations equal under the Merkle id are equal under the
full-rehash fold, and distinct ones are distinct under both.

Kernel walls after P4.4: **stamp-cutoff edit 0.42/0.40 s** (the id rehash WAS its
dominant cost), full edit 1.90/1.91 s, cold 8.43 s, identity A/B PASS through
edit+revert under Merkle ids. The remaining full-edit costs, in order: link/resolve,
the two derived-cache writes (graph.bin 146 MB + names.idx 46 MB), slab digest-compare
builds — all P4.5's lane.

#### P4.5 execution decomposition (planned 2026-08-31) — the scoped CLI edit

The enabler is P4.3's identity coding: carried edge/evidence slabs stay VALID across
dense-id shifts (`(file_key, ordinal)` never moves for unchanged files), so a scoped
build can compose a generation from prior slabs + freshly derived dirty slabs without
replaying the corpus. The retained daemon already proves the semantics in RAM; P4.5 is
its disk twin, landed in three certified sub-slices:

- **P4.5a — the usage family + dirty law on disk (behavior unchanged).**
  `usage/<k>.idx` (name_hash-bucketed, TOC + digests like every family): the
  `(referenced-name-hash, from-file-key)` pairs, derived from the evidence rows at save
  (no new pipeline plumbing) and carried per bucket by digest — only buckets whose pairs
  changed rewrite. Gate: usage answers equal the retained tier's postings over the same
  corpus; the scoped path still ESCALATES ALWAYS (infrastructure lands, behavior holds).
- **P4.5b — scoped compose for the local case.** When the dirty closure == the changed
  files (usage says no external referrers of any changed definition — measured to be the
  common edit), compose: splice node/evidence/edge slabs at FILE granularity (prior bytes
  for unchanged files, fresh for changed), rebuild TOCs + Merkle id, commit. Everything
  else falls back to the full pipeline. Gate: composed generation == full-pipeline
  generation BYTE-FOR-BYTE at every scale (the differential harness's strongest form).
- **P4.5b SHIPPED (2026-08-31) as the RESPAN compose** — scoped to the span-only class
  (comment insertions, blank lines, formatting: every non-span product field byte-equal),
  where outcomes are a theorem, not a re-derivation. Eligibility is an explicit exactness
  ladder (grammar identity, error accounting, params, returns, sketches, requests, items
  sans ranges, refs sans spans — each rejection phase-stamped); fresh rows come from a
  scratch single-file seal through the pipeline's own ingest and are VERIFIED field-by-
  field against the prior generation; the surgery re-encodes only the edited buckets'
  node/evidence slabs through the same builders the full save uses, hard-links edges,
  usage, names.idx and graph.bin (stamp refreshed for the moved node fold), re-spans
  dataflow, republishes the pack bucket, and Merkle-commits. Kernel: **comment-inside-
  function edit 0.82 s** (vs 1.9–2.4 s full pipeline), edges+usage 257/257 linked, both
  caches linked, **composed generation == scratch generation** (Merkle AND full-rehash
  folds) — the convergence gate also self-proves the compose ran (a cutoff misfire would
  carry stale spans and diverge). Falls back loudly on any proof failure; error-span
  SHIFTS are eligible (they live only in the republished product), error COUNTS are not.
- **P4.5c — the full dirty closure**, landed as three certified sub-slices. The original
  "no new family needed" premise fell to one analysis: near-clone pairing is GLOBAL (LSH
  banding + star centers over the ENTIRE sketch ledger), so a scoped build re-pairing
  after an edit needs every prior sketch — decodable only by a corpus-wide product
  decode, which is exactly what scoped builds exist to avoid. Hence:
  - **c-1 — the sigs family: SHIPPED (2026-08-31).** `sigs/<k>.bin` + `sigs/toc.bin`,
    80-byte rows `[file_key u64][ordinal u32][shingles u32][sketch;64]`, sorted
    (bucket, key, ordinal), digest-carried per bucket like every family; in the Merkle
    fold ("sigs/toc.bin" — an older prior without it simply fails the cutoff/compose
    pre-checks and migrates through the full pipeline). Rows ride the existing pairing
    thread (both linkers hand them back beside the pairs; bulk writer ids ARE sealed ids
    — the P4.2 canonical stream makes the seal remap-free — and the retained pre-LUT is
    asserted equal to the seal's), saved concurrently with evidence/dataflow/kg in all
    three persistence tails (sync, deferred, served), flat lane untouched. Respan
    hard-links the whole family (sketch equality is an eligibility premise); the cutoff
    links it like every stamp-free family. Width is pinned (`BINS ==
    vorpal_kg::SIG_SKETCH_LEN`, compile-time). Gates: pack_v2 content oracle — family
    multiset-equals the packed products' sketch ledger per file (fixture grew signable
    near-clone pairs in rs+py above the MIN_TOKENS floor; 6 languages, 32 files);
    single-bucket rewrite law on edit; full-link law on cutoff+respan; migration
    publishes the family; both daemon differentials green. Kernel: 49 MB family
    (257 members, ~2.6% of generation), cold 7.56–8.28 s (band unchanged — the save
    overlaps evidence), determinism ×2 PASS, edit/revert/respan/cutoff convergence PASS,
    respan 0.86 s + 257/257 sig slabs linked, cutoff 0.49 s + 257/257 linked.
  - **c-2 slice i — the resolution-equality core: SHIPPED (2026-08-31).**
    `vorpal_ingest::scoped_resolve_file` re-resolves ONE defs-stable-edited file against
    the prior sealed generation: partial symbol table over the file's name closure
    (candidates via `nodes_named`, File registrations for every file, peek-or-sentinel
    owners — rule-for-rule with `build_symbol_table_over`), file-scoped import bindings
    (bindings key on `from_path`), bounded product-decode closure for the rets chain and
    param ledgers (files defining any called name; capped, escalate past the cap), and
    the pipeline's OWN kernels end to end: `reference_from_view` (extracted, shared),
    `resolve_batch` (the chunk kernel, published), `join_call_edge`, `match_requests`.
    The defs-stable ladder (`views_defs_stable_reject`): grammar, error accounting,
    definition set sans ranges, entity params, returns — refs/sketches/requests free.
    GATE (crates/index/tests/scoped_oracle.rs): scoped outcomes == a scratch build of
    the edited tree for the edited file — evidence multiset (edge AND no-edge rows,
    external pinned), per-source ORDERED edge sequences (confidence-labeled, DATA_FLOWS
    spliced at first-pair positions, request tail), dataflow rows (kw + positional
    binding through the closure's ledgers), sketch rows — field for field; the rets
    chain lane pinned non-vacuously (maker().render() resolves through the decoded
    ledger). Two drift bugs the oracle caught at birth: edges must carry
    `with_confidence` labels (base-only diverged), and literal args are untraceable
    (fixture, not code). Costs measured at kernel scale for the coming slices: table
    build 70 ms, resolve 258 ms full-corpus (F-only: µs), scc 74 ms, pairing 370 ms
    solo at 638 k rows (the c2-ii driver; ceiling truncation makes pairing
    order-dependent — SigStore's (bucket, key, ordinal) sort IS the canonical feed
    order).
  - **c-2 slices ii+iii — the pairing repair and the family surgery: SHIPPED
    (2026-08-31).** `scoped_similar_repair` re-pairs the FULL ledger with the edited
    file's run swapped at its canonical (bucket, file, ordinal) position (SigStore's
    order IS the pipeline's feed order — load-bearing under the measured candidate
    ceiling), diffs against the prior pair set read from the sealed adjacency
    (`Kg::similar_pairs`, a zero-allocation CSR walk — `out_neighbors` was ~9M transient
    allocs at kernel scale), and names every endpoint whose similar segment rewrites.
    `vorpal_kg::defs_stable::compose_defs_stable` splices the families: node vsegs
    (spans; heap links — strings are defs-stable), the scc_size column wherever the
    CALLS condensation ripple lands (recomputed from the prior GRAPH + the delta, only
    when the file's call set moved), the file's evidence bucket (row/pool/total TOC
    re-splice), edge slabs by per-source CLASS partition (containment | co-change |
    resolution+DF | similar | requests — disjoint bases, monotonicity asserted), usage
    delta, sigs swap, dataflow filter+extend, and — critically — the SUCCESSOR GRAPH
    CACHE built from the prior CSR + the delta (without it every post-compose build
    paid a measured ~1.4 s lazy slab-decode; compose chains compounded to 5.1 s).
    The co-change rung (`cochange::inputs_unchanged`, HEAD-keyed cache header) now
    gates BOTH composes — a gap this slice found in shipped P4.5b: respan carried
    CHANGES_WITH bytes with no git-stability check. Two more real-world catches: the
    writer COLLAPSES duplicate entities (C decl+def pairs — 150 layout entries, 145
    rows on mm/slab_common.c), so reference attribution goes through the pipeline's own
    `layout_ids` bridge (`ingest_product_mapped`), and the bounded closure decodes ONLY
    what its consumers can read (rets ← files defining a Method/MethodHinted ref's
    receiver_type, the resolver's single chain consult; params ← Python files) — the
    call-name closure decoded 542 dead products/1.4 s on a C edit before the split,
    and rets values now intern BEFORE the table build (the linkers' order), making
    owner-peek hit-ness deterministic.
    KERNEL (76,868 files, call-inserting edit in mm/slab_common.c — the WORST lane:
    call set moved ⇒ full scc recompute + pairing recompute): **1.12–1.19 s** steady
    across a compose chain vs 1.9–2.1 s full pipeline, byte-converged to scratch every
    cycle (Merkle + full fold). Gates: scoped_compose e2e (scc-ripple cycle across
    files with the OTHER file's vseg physically rewritten; pair-appearance; def-adding
    decline), pack_v2 (its semantic-edit step now composes: vseg-only rewrite, heap
    links), both daemon differentials, suite 133/133, battery 48/48 across
    rs/go/py/ts × both lanes — nats.go's span-only shapes now compose too (the
    defs-stable ladder does not require request-span equality; requests re-derive).
    PERF LEADS (landed 2026-09-01): the node-bucket loop reads+parses each vseg ONCE
    (was up to three times on the scc lane); usage updates are bucket-scoped deltas with
    digest-carry (untouched postings buckets hard-link; the full-swap re-encoded the
    world to rediscover nothing moved); and the pairing gained an EXACT short-circuit —
    identical input rows are a pure-function guarantee of identical pairs, so every edit
    that changes no signed row's (id, shingles, sketch) skips banding+verify entirely.
    Incremental LSH banding beyond that was examined and REJECTED: partner caps and the
    global candidate ceiling make pair selection a function of the ENTIRE candidate
    stream (evicted candidates are not persisted; truncation is order-dependent, and at
    kernel scale the ceiling BINDS — measured), so any partial recompute is unsound the
    moment either bound engages. Kernel post-leads: defs-stable call-insert 1.16–1.20 s.
    ORDER-LAW FINDING (queued behind the format flip): the pipeline's pairing feed is
    LAYOUT order within a file while the sigs family is ORDINAL order — they differ
    exactly on duplicate-collapsed files (C decl+def). Every convergence gate including
    the dup-heavy kernel has held, but the equivalence deserves construction: after the
    flip retires the flat byte-identity law, the feed canonicalizes to
    (bucket, key, ordinal) everywhere — one order, exact by build, and the short-circuit
    then fires on dup-reordered files too.
  - **c-2 — defs-stable scoped resolve (single-file semantic edits).** Ground truths
    verified in-code (2026-08-31): a file's node rows are exactly [File][item][member…]
    in outline order (`ingest_file_with_spans`), so a defs-stable edit (same items,
    members, imports, signatures — bodies changed) keeps the file's node COUNT and ORDER
    ⇒ ordinals stable ⇒ every incoming `(dst_key, dst_ord)` reference row in OTHER
    buckets is byte-stable; item `content_hash = hash(entity_path, signature)` is
    body-invariant (only File-node hashes and spans move). The resolver's candidate
    table builds straight from the prior graph (`SymbolTable::from_kg`), import bindings
    for the edited files decode from their own products, and re-resolution runs for the
    EDITED files' references only. Sketch changes re-pair over prior sigs rows
    (unchanged files) + fresh rows (edited) — the family's purpose. Costs to engineer,
    not assume: scc_size is a node column over the CALLS graph, so a body edit that
    forms/breaks a cycle legitimately rewrites other buckets' node slabs (digest-carry
    already handles it — the fixture's "no scc ripple" note is fixture-specific, not a
    law); eligibility must reject returns-ledger changes (rets are global name-keyed
    chain inputs) and request-span drift exactly like the respan ladder; and the edge
    stream's global order (pre-link, per-bucket resolution, similar, requests) must be
    recomposed, not approximated. Gate: byte convergence per edit class on the fixture,
    the kernel, AND the battery repos.
  - **c-3 slice i — the overlay session and its oracle: SHIPPED (2026-09-01).** The
    scoped core generalized to a SESSION over a `UniverseView` (candidates by name, file
    registry, routes — the only three questions resolution asks of the world):
    `PriorUniverse` re-derives c-2 unchanged (all its gates re-proven through the
    refactor), `OverlayUniverse` swaps the edited file's definitions for its scratch
    seal's and translates every dense id by the shift law. One shared table for the whole
    session, all files' import bindings seeded together (the bulk's own one-table shape).
    New entries: `views_defs_changed_reject` (grammar + error rungs; defs free),
    `affected_def_names` (per-name row-sequence diff — ordinal-shifted survivors
    included, which is what keeps every non-dirty bucket byte-stable),
    `resolve_defs_changed` (edited + dirty files, successor space). GATE
    (tests/defs_changed_oracle.rs): a def added ABOVE two survivors — the shift-law file
    table equals the scratch build's per file; the usage-derived dirty set is exactly the
    referrer (found through gamma_new's prior NO-EDGE rows — the added-def case);
    evidence and per-source edge sequences equal scratch for the edited AND dirty files;
    the dangling reference resolves non-vacuously. Surgery + compose wiring = c3-ii.
  - **c-3 slice ii — the defs-changed surgery: SHIPPED (2026-09-01).**
    `vorpal_kg::defs_changed::compose_defs_changed` + `try_defs_changed_compose` (guard
    order: cutoff → respan → defs-stable → defs-changed → full). The edited bucket's
    node columns and heap BYTE-SPLICE around the scratch seal's rows (file heap runs are
    back-to-back by the writer's gather order — asserted); the successor identity
    (bases, file table, map) builds in RAM from the shift law; session buckets swap
    evidence/edges with dense translation; scc recomputes over the successor call graph;
    usage/sigs/dataflow/names.idx/graph all emit successor artifacts (names by
    translation + the fresh block — never a full rescan). THE LAW THE KERNEL FORCED: an
    UNMOVED ordinal (same identity, same position — every append's survivors) keeps its
    dense id verbatim, which is exactly why its referrers are not dirty; `translate` is
    identity there for TARGETS while source-side enumerations drop the whole old block
    (the fresh block re-contributes survivors) — the first fixture (insert-at-top,
    everything moves) could not see either half; the kernel append found both within
    two runs, each as a LOUD decline/divergence, never a wrong commit. Escalations live:
    Route/Channel-def changes, dirty past the quarter-corpus shape, error-accounting
    deltas, co-change movement, file adds/deletes.
    KERNEL (fn add/remove toggles in mm/slab_common.c): **1.28–1.32 s** per compose,
    byte-converged to scratch EVERY cycle, vs 1.9–2.1 s full. Gates:
    tests/defs_changed_compose.rs (add-above-survivors with shift + dirty referrer +
    fresh names.idx + carried unaffected buckets; remove with edge decay; file-add
    decline), the oracle, all prior compose gates, suite 135/135, clippy 0/0, battery
    48/48. REMAINING for the campaign: the default-format flip + v1 read retirement
    (owner decision — it changes every user's default), multi-file compose sessions,
    and the recorded perf leads (incremental LSH banding, scc pre-read).
  - **c-3 — defs-changed closure via the usage family (design of record, 2026-09-01).**
    Single modified file F whose DEFINITION SET changed (adds/removes/renames/signature/
    export changes; file adds/deletes stay full-pipeline). The dense-shift law collapses:
    bucket-major order means F's row-count delta `d` shifts every dense id ≥ F's old end
    by exactly `d` (`translate(x) = x + (x ≥ F_end_old) · d`), `src_local`/`from_local`
    are bucket-base-relative (whole-bucket shifts cancel), and every durable coordinate —
    edge dsts, evidence targets, ALT POOL ENTRIES (verified 12-byte `(key, ord)`), sigs,
    usage — is identity-coded, so a non-dirty bucket's bytes are stable UNLESS a row
    targets an F ordinal that moved. The dirty law makes that impossible outside the
    closure: `affected_names` = every F def name whose (ordinal, kind, signature,
    exported, eid) row sequence differs old→new (ordinal-moves included — that is what
    keeps unchanged buckets byte-stable, not just semantics-stable); `dirty =
    ∪ usage[hash(name)] − {F}` — and usage is evidence-derived over ALL outcomes, so
    referrers of a name that did not exist yet (an ADDED def) are found through their
    no-edge rows. All dirty files re-resolve through the c-2 kernels against an OVERLAY
    universe (prior candidates translated to new dense space, F's defs swapped from the
    scratch seal; one shared table, all files' import bindings seeded — the bulk's own
    shape). Escalations, each loud: |dirty| past the quarter-of-corpus recorded shape;
    a Route/Channel def among the affected (request matching is URL-keyed — usage cannot
    bound its dirty set); error-accounting deltas (slice-1 posture); co-change inputs
    moved. dataflow.bin and the successor graph translate dense ids wholesale (the step
    function). Slices: c3-i universe overlay + multi-file resolution + outcome oracle;
    c3-ii surgery (node file-table splice, multi-file bucket swaps, TOC totals) + compose
    wiring + convergence gates + kernel + battery. Then the default-format flip and v1
    read retirement.
  Standing gate for c-2/c-3 (the generic-tool law): **scripts/convergence_battery.sh**
  (shipped with c-1) — real repos, real edit shapes, scoped-vs-full BYTE compare per
  class, beyond the kernel and the fixture. Per repo copy × format lane: scratch
  determinism ×2, then touch / top-comment / body-comment / literal-flip / fn-append,
  each demanding incremental id == scratch id of the same tree. First run (2026-08-31,
  ast-grep=Rust, nats.go=Go, ProxyBroker=Python, pierre=TypeScript; lanes next+flat):
  **48/48 PASS**.
  Honest path notes from the run: ast-grep + ProxyBroker span-only edits took the respan
  compose (names.idx linked); nats.go's did NOT — its probe file is dense with request
  sites (nats:// URLs) and request-span exactness is a stated conservative eligibility
  premise, so the compose declined and the full pipeline converged instead. That is the
  designed fallback, and the battery's gate is convergence, not path choice. Escalation
  stays LOUD: any under-approximated closure falls back to the full pipeline, never to a
  wrong generation.

#### The default-format flip (2026-09-01) — bucketed ships as THE format

`PackFormat::from_env` now defaults to **Bucketed**: unset/empty/`next` → bucketed
(`next` stays as the historical opt-in name, a no-op synonym); `flat` → the deprecated
legacy writer (an explicit escape hatch until v1 retirement); any other value → stamped
to the phase log and treated as the default. v1 READS are retained everywhere — legacy
indexes keep serving and migrate on their first rebuild (rebuild is the migration).

The flip surfaced one REAL v2 gap and one daemon hole, both fixed here rather than
papered over in tests:

- **Whole-tree reuse gate was flat-only** (`crates/index/src/lib.rs`): the
  unchanged-tree fast path looked for flat artifact names, so under bucketed a no-edit
  rebuild replayed the world. Now format-aware (nodes TOC ∨ flat pair). Kernel: no-edit
  rebuild 0.13 s, generation id unchanged.
- **Boot-window backstop hole** (`crates/mcp/src/server.rs` + `crates/index/src/live.rs`):
  `lane_ready` required the overlay, so a daemon serving a fresh index with no overlay
  yet never swept for offline edits. The backstop now sweeps against the COMMITTED
  generation's manifest (`stat_changes_against_generation`) when the overlay is absent,
  and `lane_ready` is overlay-independent. The live_differential suite runs green under
  the flipped default.
- Test-truth updates where flat was baked in as "the" layout: artifact-export sentinel
  (now `!nodes.vseg && !NODES_TOC`), cochange graph-truth count, bookmarks same-file
  insert, verified-mode stale-view assert, incremental-convergence and live_build
  expectations, pack_v2's flat selections now set `VORPAL_FORMAT=flat` explicitly.

Docs: `docs/INDEX_FORMAT.md` gained a **Format selection** section (default, escape
hatch, unknown-value posture, reader compatibility) and the generated version table now
lists bucketed first as "the default" with flat marked deprecated
(`crates/index/tests/format_policy.rs` row strings — the doc self-heals from these).
`scripts/convergence_battery.sh` defaults to the `next` lane; `--formats "next flat"`
exercises the deprecated writer.

Gates, all on the flipped default: suite 135/135 twice consecutively after the backstop
fix (plus the final pre-land run); clippy 0/0 workspace AND `-p vorpal-py --features
python`; battery **24/24** (ast-grep/nats.go/ProxyBroker/pierre — and nats.go's
span-only shapes, which DECLINED respan in the first battery, now take a scoped compose
via the c-2/c-3 ladders: convergence identical, path faster); kernel default-env probe
with NO `VORPAL_FORMAT` in the environment: cold 8.94 s producing the v2 family layout,
no-edit rebuild 0.13 s same id, scratch twin bit-identical
(`8e146313a6ab3f19ff1c6351dd80fd90`). Since `from_env` is the single read of the
variable, `next` and unset are the same code path past that point — the battery's
explicit-`next` lane certifies the bare default. QUEUED NEXT: the pairing-feed order
law (canonicalize to (bucket, key, ordinal) now that the flat byte-identity law is no
longer load-bearing), then multi-file compose sessions (S2).

#### The pairing-feed order law (2026-09-01) — pairs become a pure function of the multiset

The recorded finding held under measurement, and it was sharper than "order": the feed
can carry TWO rows for ONE node id. The writer collapses same-(entity_path, signature)
entities onto one node — C decl+def, and cfg-ARM DOUBLE DEFINITIONS (two `#ifdef`
branches of the same function; the extractor does not preprocess) — and both occurrences
can sign with different sketches. Census (temporary probe at `similar_pairs` entry):
**kernel 639,554 rows → 548 duplicate node ids, 543 content-differing; ast-grep 2,128 →
4, all 4 differing** (so the case is cross-language, not a C quirk). The survivor was
whatever the node-keyed UNSTABLE sort left first — deterministic per feed arrangement,
NOT per multiset: bulk stream, retained RAM, and scoped splice arrangements agreed only
by coincidence. Everything downstream of the dedup (band keys, ceiling truncation, star
hubs) already walks the node-sorted sequence, so the dedup was the single point of
order-dependence in the whole pass.

THE LAW: sort by full row content `(node, shingles, sketch)`, dedup by node — survivor =
smallest content. Pairs and family rows are now a pure function of the row MULTISET by
construction (unit-pinned: both feed arrangements of a content-differing duplicate yield
identical pairs and rows). `scoped_similar_repair` canonicalizes the fresh run the same
way before splicing (the raw run arrives in product-layout order — non-monotone on
collapsed files, duplicates possible — which is exactly why the EXACT short-circuit
missed on such files even when no sketch changed).

The survivor change is OUTPUT-changing where duplicates exist, so it rides a version:
**sigs family VERSION 1 → 2**, and `build_index` gains ONE family-law gate where the
prior generation is admitted — a v1-family prior is neither reused (whole-tree lane
included) nor composed from (cutoff/respan/c-2/c-3), stamped loudly; the full pipeline
rebuilds the family once and every fast lane reopens. Without that gate the byte-carry
lanes would perpetuate v1 survivor bytes while scratch now writes v2 — silent
scratch≢incremental. The version table gains the three family rows (edges 1, usage 1,
sigs 2) alongside the existing evidence row.

Gates: unit invariance pin; dup-collapsed C fixture through the defs-changed compose
with a NON-VACUITY assertion (two signature records must bridge onto one node id with
differing sketches — the test refuses to pin nothing); kernel scratch determinism ×2
(generation id moved ONCE: 8e146313… → 04d3845f…, the 543 survivors); both toggle
directions byte-converge; v1→v2 migration lane proven live (old-law binary builds, new
binary declines loudly and full-rebuilds to exactly the new-law scratch id, no-edit
reuse correctly refused); battery 24/24; suite green; clippy 0/0 both lanes.
STRUCTURAL A/B (pinned binaries, same edit): old law RAN banding on the kernel tiny-fn
append (`similar: … candidates` stamped), new law short-circuited (no stamp);
interleaved walls **1.28 s → 1.18 s** (two rounds each, exact repeats). The earlier
~0.9–1.0 s guess overestimated banding's share of this shape; the honest win is ~0.10 s
plus the constructive guarantee. The generation id is stat-sensitive by design (the
manifest rides the fold): re-appending the same bytes after a revert yields a NEW id —
convergence comparisons are always within one tree state.

#### S2-a (2026-09-01) — multi-file DEFS-STABLE sessions

k body-edited files now ride ONE defs-stable compose (the respan lane was already
k-capable from P4.5b — MAX_RESPANNED — so span-only multi-edits were composing all
along; this closes the semantic class). What generalized, and why it was mechanical:
defs-stability means NO dense id moves anywhere, so per-file slices are independent by
construction — `DefsStablePlan` became `files: Vec<DefsStableFilePlan>` + the global
fields (pair set, changed endpoints, sigs ledger, which are session-wide by nature).
The session core was ALREADY multi-file (`resolve_session` — c-3 built it that way);
`scoped_resolve_files` is the new plural entry (one shared table, all import bindings
seeded together; `scoped_resolve_file` delegates), and `scoped_similar_repair` now takes
`swaps: &[(file_key, fresh run)]` — k runs spliced at their canonical positions, each
canonicalized under the order law. Surgery per family: evidence/edges retain-and-extend
per OWNING file (an `owner_of` range lookup replaces the single file_range test; two
edited files may share a bucket), scc recomputes when ANY file's call set moved (all
ranges excluded, all plans' calls pushed), usage removes/adds per (name_hash, owning
key), dataflow filters any range, names still hard-link (defs stable session-wide).
Previously-silent per-file declines (universe miss, row-count move, stable-field move)
now stamp loudly — found when a STALE BINARY (libs rebuilt, CLI not) silently declined a
kernel k=2 probe: the diff-shape bails stay quiet by design (every >1-file edit hits
them), but premise violations speak.

Gates: `a_multi_file_defs_stable_session_composes_and_converges` (THREE files — two
Python forming the cross-file scc cycle + the Rust neighbor — one session, indexed==3,
names.idx hard-linked, byte-converged) first-run green; kernel k=2 literal flips
(mm/slab_common.c + kernel/fork.c): **1.20 s forward / 1.07 s reverse**, byte-converged
both directions — barely above the single-file 1.03 s because the session amortizes the
table build and the single global pairing pass; battery gained **S6 two-file** (top
comments in the two largest files, one build) — **28/28 PASS** across
ast-grep/nats.go/ProxyBroker/pierre, S6 composing on every repo; suite green; clippy 0/0
both lanes. Mixed sessions (defs-stable + defs-changed members) still decline to the
full pipeline — S2-b routes them through the changed lane (stable members = delta-0
blocks) with the multi-block shift law.

#### S2-b (2026-09-01) — multi-file DEFS-CHANGED sessions + mixed routing

k definition-set edits ride ONE defs-changed compose, and MIXED sessions (some members
defs-stable, some defs-changed) route through the changed lane with stable members as
delta-0 blocks — every ordinal unmoved, nothing declines just because edits of
different classes landed together. The single-block shift law generalized to the
MULTI-BLOCK law everywhere it lived:
`translate(x) = x + Σ_{i: x ≥ old_end_i} delta_i` outside the blocks (ascending
disjoint blocks + prefix sums; binary search per lookup), and inside a block the
unmoved-ordinal identity becomes `new_start + ordinal` — identical to the prior id
exactly when no earlier block's delta reaches it, which is what keeps identity-coded
carried rows and the dirty law untouched. Three implementations of the law stay in
lockstep by construction (the overlay session's `OverlayUniverse::build`, the surgery's
`Shift`, the compose's successor map) — same block sort, same prefix, same bases rule
(`new_bases[b] += prefix[partition(old_end ≤ bases[b])]`; files never straddle
buckets). The edited-bucket node splice generalized from prefix|block|suffix to a
REGION WALK — alternating gap/splice regions, each gap's heap extent tiling
monotonically (asserted; two edited files in ONE bucket splice in one pass), offsets
rebased per region. Fresh edge sources come from each block's OWN seal;
evidence/usage/dataflow/names already looped per session file (c-3 built them that
way). `resolve_defs_changed` now takes `edited: &[(DirtyFileInput, &Kg)]`;
`DefsChangedPlan.unmoved_ordinals` became `edited: Vec<(file_key, unmoved)>`.

Gates, all first-run green: `a_multi_file_defs_changed_session_composes_and_converges`
(defs added to TWO files in one build, then both removed — two blocks, cumulative
deltas) and `a_mixed_session_routes_through_the_changed_lane_and_converges` (one
def-adding member + one body-only member: the stable lane's ladder rejects, the changed
lane composes both); all 11 prior scoped/changed/oracle gates unchanged-green (the k=1
path); kernel k=2 fn appends (mm/slab_common.c + kernel/fork.c, one build):
**1.31 s forward / 1.30 s reverse**, byte-converged both directions — the same
1.28–1.32 s band as SINGLE-file defs-changed (one table, one pairing pass, one scc);
battery 28/28; suite green; clippy 0/0 both lanes. S2 COMPLETE: respan (P4.5b),
defs-stable (S2-a), and defs-changed (S2-b) all compose k-file sessions; mixed classes
route to the strongest lane that admits them.

#### Session k-scaling sweep + parallelism (2026-09-01) — the slope was the SESSION, not the seals

Recorded sweep (kernel, tiny-fn append per file, defs-changed lane VERIFIED per run by
its stamp, byte-converged at every point; lesson en route: an early sweep's k=16 wall
was the FULL PIPELINE's — kernel/exit.c carries parse-error regions, appends shift its
error accounting, both ladders correctly rejected — lane-verify every sweep row, and
convergence alone cannot distinguish lanes):

| k (files) | serial session | + parallel seals | + parallel session |
|---|---|---|---|
| 2  | 1.27 s | 1.25 s | 1.24 s |
| 4  | 1.57 s | 1.54 s | 1.39 s |
| 8  | 1.84 s | 1.77 s | 1.42 s |
| 16 | 2.23 s | 2.32 s | **1.57 s** |

First attribution FALSIFIED by measurement: parallelizing the per-file seal loops
(compose.rs, rayon over ladder+seal+verify — kept, structurally right, ~5 ms/file)
barely moved the slope. The traced k=16 breakdown put it in the SESSION: table build
385 ms (3,027 call names, serial candidate enumeration), per-file collect 298 ms,
closure decode 181 ms (59 products); the family surgeries were 59–135 ms each
(permanent trace-gated stamps now cover every surgery section). Fix: `UniverseView`
gained `Sync` as a supertrait and the three blocks parallelized — per-file collect
(sets union after; interner id VALUES never reach artifacts, the bulk pipeline already
interns in shard-arrival order), closure derivation + product decode per unit, and
candidate ENUMERATION per name with insertion kept serial in the same per-name order
(`finalize` canonicalizes across names exactly as before). Slope 0.076 → **0.024 s/file**
(3.2× flatter); the compose now beats the ~2.0–2.1 s pipeline across the whole measured
range (the serial crossover was k≈10). k=1 parity: 1.17–1.25 s toggles (was 1.18).
Gates: all oracles + compose fixtures green (outcome equality is the parallelism's
correctness pin), battery 28/28, suite green, clippy 0/0 both lanes.

#### The scc CARRY LAW (2026-09-01) — defs-changed stops paying for cycles it provably didn't touch

The k=1 traced surgery (finer stamps, incl. a names split) put the fixed costs at:
scc recompute 92 ms, node store 102 ms (the always-recompute forced EVERY bucket's
column compare — 179 MB of vseg reads per edit), graph cache 118 ms, usage 90 ms,
names sort 45 ms + write 8 ms, staging tail 100 ms. The law: carried (non-session)
sources' call edges are translation-invariant by construction, so if every SESSION
file's fresh call set equals its prior call set under translation, the successor calls
graph IS translate(prior calls graph) — the condensation is isomorphic under translate
and every carried node's `scc_size` carries verbatim. Ordinals outside translate's
image then have no call edges (one would break the equality) and take the algorithm's
own isolated-node value, 1 (scc.rs: `sizes = vec![1; n]`, "1 for acyclic nodes" — a
definition, not a constant). Any untranslatable endpoint on a prior call bails to the
recompute, so a moved def that participates in calls always recomputes. Under the
carry, non-edited buckets hard-link BLIND (no read, no compare) and the edited bucket's
scc slice assembles from the prior column + unmoved flags.

Kernel (fn-append toggles, all byte-converged, lane-verified): k=1 **1.10–1.12 s**
(was 1.17–1.25); sweep k=2/4/8/16 → **1.11 / 1.26 / 1.30 / 1.47 s** (was
1.24/1.39/1.42/1.57 post-parallelism; 2.32 s at k=16 this morning). scc phase 92→0 ms,
node store 102→85 ms on the append shape. The day's defs-changed trajectory:
1.28–1.32 s → 1.10–1.12 s single-file. Gates: all compose/oracle fixtures green (both
carry and recompute paths exercised — call-set-changing fixtures recompute, uncalled
adds carry), battery 28/28, suite green, clippy 0/0 both lanes. Remaining k=1 spend,
should sub-second demand more: graph cache 118 ms, staging tail 100 ms, usage 90 ms,
identity 78 ms, names 53 ms, sigs 53 ms.

#### Usage delta rework + THE LINK-FAN-OUT FINDING (2026-09-01)

`usagestore::apply_delta` reworked: per-bucket sorted three-way merge (slab rows are
saved sorted by (hash, key)) replaces the per-pair `HashSet` probe + per-bucket re-sort
— no SipHash, no sort; PLUS the EXACT NO-OP law — identical sorted delta sides cancel
((S − R) ∪ R = S with R ⊆ S by the family's evidence-derivation invariant, which the
exact-postings oracle pins), so every bucket the changed names miss links with no slab
walk at all; and the bucket loop parallelized. On the kernel append the census reads
**0 wrote, 256 linked**. Byte-converged; fixtures/battery/suite green; k=1 settles at
**1.04–1.10 s**.

But the phase only fell 90→72 ms, and the census exposed why — THE REAL FLOOR:
**256 pure hard-links cost ~68 ms.** Isolating experiment (this box, APFS): 256
remove-miss+link into one directory = **69.2 ms serial-equivalent (0.27 ms/entry — the
directory inode lock serializes even a parallel loop)**; `clonefile()` of the same
256-entry directory = **1.5 ms** (46×). A compose carries ~1,800 family entries
(nodes 512 incl. heaps, evidence/edges/usage/sigs/products × 256) ⇒ the link fan-out
is ~400 ms of the ~1.05 s wall — the dominant remaining fixed cost, spread across
every phase's tail (it is why "sigs done" costs ~54 ms while writing nothing).

DESIGNED NEXT SLICE (owner tradeoff stated): carry each family directory by APFS
`clonefile` (one syscall), then rename the rewritten members over their cloned
entries; per-file `hard_link` stays the portable fallback (ext4 links are ~10–20 µs —
this is an APFS-specific pain). The trade: COW clones get NEW inodes, so the
inode-equality oracles (scoped_compose's names.idx pin, defs_changed_compose's
unaffected-bucket pin, the battery's `[scoped: names.idx linked]` detector) stop
observing physical carry — they would move to the report-note lane signal plus
convergence, which is the true gate anyway. Not landed tonight: it touches every
carry site plus those oracles, and deserves its own gated slice.

## Execution order & gates

Phase 0 chunks land independently, each gated (streamed≡batch, content-id A/B, ann SHA A/B,
retrieval/resolution evals, full suite): 0.B tail → 0.A stream → 0.C cache surgery → 0.D ANN.
Then 1a/1b/1c (new convergence tests), then 2 (recall gates + new ANN lineage tests), then 3
(differential harness gates every merge), then 4 (format-version bump, migration test matrix,
nightly golden convergence). Perf numbers recorded per chunk in README.md#performance
methodology: release builds, best-of-3, fixed tree state, thermal notes.

## Status 2026-08-30 — flow-era unification: retained ledger parity LANDED, tier default-on

The feature-line merge widened what a committed generation contains (chain-resolved calls,
`data_flows` + `dataflow.bin`, `similar_to`, `requests`/`notifies`, `changes_with`). The
retained tier now reproduces all of it:

- **Per-file flow ledgers**: `RetainedIndex` keeps each file's `FlowSidecar` (args, params,
  rets, sigs, requests) in retained id space — rebased at absorb exactly like the bulk
  committer (rets are name-keyed, never rebased), replaced on edit, dropped on delete.
- **Chain-aware scoped rederive**: a rets delta dirties the affected function NAMES (the
  definition row is unchanged, so candidate-set diffing cannot see it); the postings
  expansion re-resolves every file referencing them. Pinned by live_differential's
  return-annotation-retarget class (scoped link, 1 dirty file, answers move).
- **Link-time joins**: the retained link folds `ChainReturns` into resolution, interleaves
  `DATA_FLOWS` through the shared `join_call_edge` (one function, both linkers), pairs
  near-clones over CANONICAL ids (the pairing tie-breaks on id values — star centers,
  partner caps — so retained ids would fork the pair set), matches requests, and accepts
  caller-derived co-change pre-edges — all in the bulk pipeline's exact edge-log order,
  because the CSR build is an insertion-stable counting scatter.
- **Persist parity**: `ServedPersist` stages `dataflow.bin` (rows LUT-remapped to sealed
  ids); the served commit's generation id equals a scratch build's.

**No-bypass routing (same day):** the overlay is the guaranteed path for absorbable edits —
the probe runs for ANY captured change set (the old ≤8-file cap was the only absorb bound
and is gone), watcher capture loss recovers via `LiveOverlay::stat_changes` (a stat sweep
against the retained manifest, same trust model as the pipeline's own sweep) instead of a
full rebuild, the overlay builder spawns at daemon construction, and the only remaining
pipeline fallbacks are stated ones: past the shared absorb budget
(`RetainedIndex::within_absorb_budget`, the link's own measured escalation shape), a
missing/retired overlay, a custom extraction environment, or boot.

**Space-invariant ambiguous pick (kernel A/B find):** the deterministic tie-break for
ambiguous resolutions was lowest-RAW-ID — path-ordered in bulk space but tail-inverted in
the retained writer whenever an edited file defines one of the tied candidates
(kernel repro: `slab_is_available`, mm/slab_common.c vs tools/testing/memblock/lib/slab.c,
41 flipped rows). The pick now orders by (path text, id) — `pick_key` in
crates/resolve/src/resolver.rs — the same definition in every id space. This legitimately
re-picks ties whose import-seeded binding symbols ordered differently, so content ids moved
once, with the tie set still retained in alternatives.

Gates: canonical_seal, retained, differential, stamp_cutoff, hinted_scan, live_differential
(flow corpus + generation-convergence pin; traced to 10 overlay serves / 10 retained-persist
commits in one run), and the KERNEL-SCALE A/B: daemon boot on linux → first edit (capture
lost → stat sweep recovered 1 path) → overlay serve → retained persist —
`gen/015477582e27fdcd51e4cd6f46f1f639` == the from-scratch build of the same tree, byte for
byte. Cold-first-serve edit→queryable 3.23 s (stat sweep + the first link after an overlay
build is a FULL retained link by design); steady-state scoped-serve latency re-measured on
the merge benches.




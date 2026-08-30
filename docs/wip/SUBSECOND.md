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
- **A5. Prefix-sum scatter absorb**: shards report (node_count, heap_len, edge_count,
  ref_count); exclusive prefix sums computed in shard order; shards scatter in parallel at
  known offsets — positioned pwrites for the streamed heap and the fixed 34-byte spill
  records. Preserves the rolling-absorb memory property (scatter-and-drop as bases arrive).

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

- **1a. Product-equality cutoff** (Bazel change-pruning / salsa backdating): after re-extracting
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

- **2a. Oracle-fused self-calibration** (replaces the l_build constant): during
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
- **Correctness harness**: differential testing — N seeded random edits live, compact,
  cold-load, byte/semantics-diff every query surface. Debug-mode left-right copy-compare.

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

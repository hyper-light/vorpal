# Vorpal — Architecture & Phased Build Plan

> **Status:** design / proposal. Prepared 2026-07-01. Supersedes nothing; complements
> `IMPROVEMENT_PLAN.md` (the ast-grep engine speed/semantics tiers).
> **Baseline:** ast-grep v0.44.0 has been copied into this repo and rebranded to build,
> run, and test as `vorpal` (crates `vorpal-*`, binary `vorpal`/`vp`, library type
> `Vorpal`, method `.grep()`, bindings `@vorpal/node` · `@vorpal/wasm` · `vorpal_py`).
> 708 engine/CLI tests green. This document plans what we build *on top of* that engine.

---

## 1. Vision & non-negotiables

Vorpal is a **Rust-native** code ingest → index → search engine that fuses ast-grep's
structural matching with a scale-first knowledge graph + document store, exposed via CLI,
an MCP server, and TypeScript bindings.

Hard requirements (each drives the design below):

- **Scale to 10⁹ LOC** — Meta-sized monorepos. Every subsystem is streaming, incremental,
  bounded-memory, and mmap/segment-backed. No whole-repo-in-RAM anywhere.
- **Maximally correct, no shortcuts** — complexity is acceptable when it's the better
  solution. Precise AST-based analysis, never heuristic substring matching.
- **"Sees everything"** — the parsing/extraction layer is bleeding-edge thorough: it
  captures the *full* structure of the code, uniformly across all 28 languages, using the
  native tree-sitter engine vorpal already owns (no regex, unlike sylk).
- **Deterministic core** — no embeddings/LLM in the matcher or graph derivation. Semantic
  vectors are an *additive* layer; graph edges carry confidence + evidence, but are
  reproducible.
- **Extend in place** — grow the existing engine crates; don't fork a parallel tool.

### Locked decisions

| Decision | Choice |
|---|---|
| Language | **Rust**, extend ast-grep in place |
| Embeddings | **Pluggable, local-first** (Rust embedder trait; local default, optional remote; off the hot commit path) |
| KG vocabulary | **Code-focused core** (files/packages/symbols/types + code edges; no agent/claims/academic/history domains) |
| Parsing depth | **Comprehensive** — tree-sitter for all langs, extraction-as-rules, "sees everything" |
| KG on-disk format | **Custom binary** (segmented append + mmap indexes), *not* SQL |
| Distribution | npm (per-platform prebuilts via napi-rs), plus PyPI wheel + wasm |
| Concurrency & reclamation | **Arc-free hot paths** (see §7): per-batch/per-worker arenas + generational-index handles instead of shared pointers; two-tier reclamation — coarse version/epoch **pin** over mmap segments + `seize` (Hyaline) for in-memory maps; wait-free reads via RCU pointer-swap, `left-right`, and seqlocks; **single-writer-per-shard** streaming ingest over bounded MPMC queues |
| Memory & IDs (§8) | Dense `u32`/`u48` ordinal IDs + SoA hot/cold zero-copy mmap + **adaptive** huge-page/arena/prefetch policy (corpus+hardware probe → per-store; near-zero baseline); `blake3` = external identity only |
| Storage format (§9) | `.vseg` columnar segments (hot/cold split) + FastLanes/Elias–Fano/Roaring64/FSST codecs; edges = hash-partitioned delta-log ⋈ compacted CSR/CSC; `fjall` canonical index + `redb` manifest; `io_uring`/`O_DIRECT` (Linux) |
| ANN (§10) | RaBitQ/Vamana-over-IVF: RaBitQ quantization (provable bound), **PipeANN** `io_uring` search, **ParlayANN** deterministic build, FreshDiskANN/SPFresh updates, ACORN filtering |
| Graph & closure (§11) | index-CSR (both directions) + **masked-SpMV** Datalog closure + succinct containment forest (Euler/RMQ O(1) scope); streaming LSM-for-graphs assembly |
| Matcher fast path (§12) | SIMD literal pre-filter (per-rule DNF) + build-once dispatch + zero-copy mmap + thread-local parser reuse + interned metavar env |

---

## 2. Layered architecture

```
 L6  Surfaces        CLI · MCP server · TS (napi) · LSP · daemon (warm index)
 L5  Query           hybrid: structural(engine) + lexical(FTS) + semantic(ANN) + graph, RRF-fused
 L4  Ingest          streaming pipeline: walk → read(mmap) → hash-skip → parse → extract →
                     chunk → embed(async) → commit; incremental, bounded-memory, partitioned
 L3  Knowledge graph code-focused nodes/edges · precise cross-file resolution · Datalog closure
 L2  Storage         segmented append stores + paged mmap offset index · hash-sharded edges ·
                     Vamana/IVF ANN · full-text index · on-disk canonical (content-hash) index
 L1  Extraction      "sees everything": comprehensive AST → entities/relations, as ast-grep rules
 L0  Engine          tree-sitter + 28 grammars + Matcher/Pattern + rules + CombinedScan   [EXISTS]
```

Everything below L0 is new; L0 is the rebranded ast-grep engine we already have and will
also improve per `IMPROVEMENT_PLAN.md` (literal pre-filter, cache/daemon, scoped metavars).

---

## 3. Subsystem designs

### 3.1 L1 — Comprehensive extraction ("sees everything")

The differentiator. sylk extracted only functions/types/imports (go/ast for Go, **regex**
for TS/Python) with heuristic, partly-stubbed cross-file linking. Vorpal instead:

- **One substrate, all languages.** Reuse the engine's tree-sitter parse + `CombinedScan`
  (one DFS per file, `kind_id → [rule]` dispatch). Extraction is *additive* to matching.
- **Extraction-as-rules.** Model each captured construct as an ast-grep rule/pattern
  (building on the existing `outline` crate, which already does rule-driven symbol
  extraction with per-language `default_rules/*.yml`). "Sees everything" = comprehensive
  rule sets per language capturing: declarations & definitions, every reference/use site,
  scopes & bindings, calls, imports/exports, type & generic usage, implements/embeds,
  fields/methods, decorators/attributes/macros, docstrings/comments, and signatures.
- **Precise scopes.** Per-file scope/symbol table (tree-sitter `locals`-style queries) →
  binding-accurate references (foundation for cross-file resolution and `IMPROVEMENT_PLAN`
  Tier 2 scoped metavars).
- **Confidence + evidence.** Every derived relation carries a confidence and an evidence
  span (byte/line range) — adopt sylk's honesty model; keep it reproducible.
- **Streaming + incremental.** Extraction consumes the tree-sitter cursor as a **SAX
  enter/leave event stream** (no derived tree, §11.1) with SIMD kind-dispatch, on the shared
  **fast path** (§12: build-once dispatch, thread-local parser reuse, zero-copy mmap). Nothing
  buffers whole-repo; content-hash skip avoids re-extracting unchanged files.

*Acceptance:* for a fixture repo, vorpal extracts a strict superset of sylk's entities with
AST-precise spans, uniformly across ≥3 languages, with zero regex in the path.

### 3.2 L2 — Storage substrate (the long pole)

Reimplement sylk's proven primitives in Rust, applying the scale fixes the research surfaced
(sylk's own storage held up; its *orchestration* did not — see §4). Primitives:

- **Segmented append-only data files** per entity kind (nodes / vectors / chunks / docs).
  sylk used a single monolithic `data.bin`; at 10⁹ LOC that's multi-TB single files. Vorpal
  segments (e.g. 64k-entity segments, like the ANN store already does), each mmap'd.
- **Offset index → a tiny segment directory.** Internal IDs are **dense monotone ordinals**
  (`segment.base + row`, not hashes — §9.2), so `id → offset` is O(1) arithmetic and the "index"
  collapses to a ~180 KB sorted `id_base → segment` directory — not sylk's dense in-heap
  `[]int64` (~80 GB at 10¹⁰ ids), nor even a paged sparse map. `blake3` is external identity only.
- **Hash-partitioned edge shards** (not sylk's fixed `SourceID/65536` range partition,
  which hotspots the newest shard and fans incoming-edge queries across all shards). Add
  per-shard bloom filters for negative lookups; maintain both directions or a target index.
- **On-disk canonical (`blake3`→`NodeId`) index** — the dedup / identity / incremental-skip
  spine, *not* sylk's fully-resident `map[string]uint32` (~100 GB at 10⁹). **Resolved (§9.6):**
  `fjall` (LSM, write-optimized for random keys) fronted by a `papaya` cache; `redb` for the
  manifest; learned indexes rejected. Frozen to a minimal-perfect-hash on segment seal (§11.6).
- **RaBitQ/Vamana-over-IVF ANN (§10).** Port sylk's Vamana/IVF *skeleton*, modernized:
  **RaBitQ** quantization (provable error bound; replaces BBQ) + rerank; **PipeANN** `io_uring`
  pipelined beam search; **ParlayANN** lock-free *deterministic* batch-parallel build (replaces
  lock striping); **FreshDiskANN/SPFresh** non-stalling updates; **ACORN** filtered search.
  Hybrid port: reuse the `rabitq-rs` kernel, native index/storage; data-derived params.
- **Full-text index** — a Rust FTS (Tantivy — Bleve's lineage/Lucene-model, mmap segments,
  incremental) replacing sylk's Bleve; shard by subtree; epoch-pinned versions.

*Acceptance:* insert/lookup are O(1)/O(delta) and bounded-memory under a synthetic 10⁸-node
load; a cold open mmaps in ms without loading indexes into heap.

### 3.3 L3 — Knowledge graph (code-focused, deep)

- **Nodes:** file, package/module, function, method, type (struct/interface/class/enum),
  field, variable, constant, import/export, (chunk for doc/vector linkage). String,
  carrying kind/name/path/pkg/signature/content-hash/span. **IDs are two-level (§9.2):** a dense
  monotone `NodeId` for all hot cross-refs (CSR endpoints, offsets), with `blake3(path:entityPath)`
  as the external content-addressed identity in the canonical index.
- **Edges:** `defines/defined_in`, `calls/called_by`, `references`, `imports/imported_by`,
  `implements`, `embeds`, `has_field/has_method`, `returns`, `of_type`, `overrides`,
  `similar_to` (semantic), `supersedes` (version) — materialized both directions.
- **Cross-file resolution — done right.** This is the hard part sylk left unsolved. Vorpal:
  intra-language, import-graph-driven resolution using the precise per-file scope tables;
  qualified-name + signature matching; confidence-scored with evidence; start with 2–3
  languages, expand behind the `Language` trait. Approximate edges are labeled, never faked.
- **Transitive closure = masked SpMV (§11.5).** Semi-naive Datalog is iterated masked
  sparse-matrix × Roaring-frontier over the CSR (GraphBLAS model) with direction-optimizing
  push/pull — **one** vectorized kernel serves `callersOf`/`refsTo`/importers and their
  transitive versions (iterate to fixpoint); incremental on edge add/remove.

*Acceptance:* callers-of / refs-to / importers-of / implementors-of return correct results
on a multi-file fixture; incremental edits update only affected edges.

### 3.4 L4 — Streaming ingest (disk → index)

Rebuilt from first principles (sylk's is whole-repo, RAM-materialized, single-writer —
OOMs well before 10⁹ LOC). Vorpal:

1. **Bounded fan-out pipeline:** `discover → read(mmap)+blake3 → hash-skip → parse →
   extract → chunk → (embed) → flush` as fixed-capacity channel stages. **Peak RAM =
   O(batch × workers), independent of repo size.** No `PendingMode` whole-repo buffer.
2. **Content-hash skip is the spine:** persist a content-hash→node index; skip unchanged
   files before read/parse/embed. Continuous incremental ingest at 10⁹ LOC is only tractable
   when ~99.9% of files are skipped each pass. Git-diff change detection selects the delta.
3. **Embeddings off the hot path:** commit text + structure first (cheap, deterministic);
   an async, throughput-limited worker drains a durable on-disk queue into the vector store.
   (Embedding is both the RAM and wall-clock wall in sylk.)
4. **Partitioned / parallel commit + LSM-for-graphs assembly (§11.3):** shard by path prefix;
   per-shard append + offset index; no single global write lock; a top-level manifest maps
   shard → head. Edges append to a per-shard **delta log**, compacted to CSR/CSC in the
   background (GVEL prefix-sum + scatter) — the stream never buffers whole-repo.
5. **Log-structured manifests + coarse epochs:** O(delta) per-commit metadata (embedded KV),
   not whole-JSON rewrites; batch many files per commit; compactable epochs, not a directory
   per micro-commit. Bounded WAL: one entry per batch, checkpoint/GC on flush.

*Acceptance:* full index of a large repo with flat memory; re-index of an unchanged tree is
near-instant (hash-skip); crash mid-commit recovers to the last published epoch.

### 3.5 L5 — Query (hybrid)

One coordinator fusing four searchers with weighted **Reciprocal Rank Fusion** (k=60):
**structural** (the engine's pattern/rule matcher), **lexical** (FTS), **semantic** (ANN),
**graph** (traversal + Datalog). Results carry provenance; optional temporal decay / trust
boost. Powers `callersOf`, `refsTo`, `hybrid(text, semantic, graph)`, `fetchDoc`.

### 3.6 L6 — Surfaces

- **CLI** — extend the existing `vorpal` commands with `index`, `query`, `graph`.
- **MCP server** — a long-running daemon holding the **warm index** (mmap'd stores + hot
  caches), exposing tools agents pull from: `structural_search`, `graph.{callers,refs,
  importers,implementors,neighbors}`, `hybrid_search`, `fetch_doc`, `index/reindex`, and a
  **shared-parse** endpoint (parse once → matches + graph, killing sylk's double-parse).
  The warm index is a **process-lifetime singleton** exposed as `&'static Index` (via
  `OnceLock`/one intentional `Box::leak`), so every async task borrows it with **zero
  `Arc` refcount traffic** — unlike ast-grep's LSP `DashMap<…>` + `Arc<RwLock<…>>` model,
  which we replace with `papaya`/`left-right` state and epoch-pinned reads (§7). Keep
  `tokio` at the I/O edge only; dispatch CPU-bound search onto a `rayon` pool that borrows
  `&'static Index`; hold `seize`/`papaya` **owned guards** across `.await`. MCP SDK: `rmcp`.
- **TypeScript (napi)** — superset of ast-grep's napi surface (`parse/find/findAll/replace`)
  + `graph`/`search`/`fetchDoc` namespaces; opaque node handles, JSON only for configs.
- **Engine improvements** — interleave `IMPROVEMENT_PLAN` Tier 1–3 (literal pre-filter ⭐,
  content-hash cache/daemon, scoped metavars) as they compound with the ingest cache + KG.

---

## 4. Scale playbook (sylk's breaking points → vorpal's fixes)

| # | sylk breaking point (from research) | Vorpal fix |
|---|---|---|
| 1 | Whole-repo read + duplicate content copy | streaming stages, mmap, no `contentMap` copy |
| 2 | Entire `CodeGraph` + all entities in RAM | per-batch flush; O(batch×workers) memory |
| 3 | All embeddings buffered before write (~400 GB @1e9) | async durable embed queue; stream to vector store |
| 4 | Monolithic `data.bin` (multi-TB single file) | segmented mmap data files |
| 5 | Dense in-heap offset index (~80 GB) | paged/mmap two-level sparse index |
| 6 | Fully-resident canonical map (~100 GB) | on-disk LSM/B-tree canonical index |
| 7 | Fixed 25 range-shards → write hotspot + all-shard fan-out | hash partitioning + bloom filters + reverse index |
| 8 | Whole-JSON manifest rewritten per commit (O(N²)) | log-structured embedded-KV manifest |
| 9 | Dir-per-micro-commit (9.6k dirs) | batched commits, compactable epochs |
| 10 | Global single-writer lock | path-partitioned parallel commit |
| 11 | Full FTS/ANN rebuild on recovery | incremental, epoch-pinned, shard-local recovery |
| 12 | 4 KB pages → TLB-walk-bound random access | adaptive huge pages (2 M/1 G) + SoA + beam/CSR prefetch + graph reorder (§8) |
| 13 | `Arc` refcount contention on shared hot data | Arc-free: generational handles + arenas + epoch/`seize` reclamation (§7) |
| 14 | String/hash IDs chased on the hot path | dense `u32`/`u48` ordinals; `blake3` canonical-only (§9.2) |
| 15 | No codec / random-access format | `.vseg` columnar (hot/cold) + FastLanes/EF/Roaring64/FSST (§9.4) |
| 16 | `Parser::new()` + full copy + double-DFS per file | build-once dispatch + thread-local parser + zero-copy mmap + fused traversal (§12) |
| 17 | No literal pre-filter (parse every file) | SIMD per-rule-DNF pre-filter → skip non-matching files (§12.3) |
| 18 | Arrival-order output assumed deterministic | bounded channel + opt-in `--sort=path` (§12.5) |

---

## 5. Phased build plan

Each phase is independently testable, benchmarked, and leaves the tree green. Ordering is
by dependency (storage is the long pole) with quick wins interleaved.

- **Phase 0 — Baseline rebrand.** ✅ Done (engine + bindings build/run/test as vorpal).
- **Phase 1 — Storage substrate (§9).** `.vseg` segmented columnar store + segment-directory
  offset index + hash-partitioned edge delta-log→CSR + `fjall` canonical index; the RaBitQ/Vamana
  ANN skeleton (§10). *Gate:* 10⁸-node synthetic load, bounded memory, O(1)/O(delta) ops, ms
  cold-open, `dtlb_load_misses.walk_active` under budget.
- **Phase 2 — Comprehensive extractor + KG model.** Rule-driven "sees everything" extraction
  on `CombinedScan` (extend `outline`); code-focused node/edge model; per-file scope tables;
  confidence+evidence. *Gate:* strict superset of sylk entities, AST-precise, ≥3 languages.
- **Phase 3 — Streaming ingest.** Bounded pipeline, content-hash skip, git-diff delta,
  partitioned commit, log-structured manifest, async embed queue (pluggable local-first
  embedder). *Gate:* flat-memory full index; near-instant re-index; crash-safe.
- **Phase 4 — Cross-file resolution + graph closure + query.** Precise resolution (2–3 langs),
  Datalog closure, hybrid RRF query, CLI `index/query/graph`. *Gate:* callers/refs/importers
  correct; incremental edge updates.
- **Phase 5 — MCP server (daemon).** Warm-index daemon + MCP tool surface incl. shared-parse.
  *Gate:* sub-second agent queries on a warm large index.
- **Phase 6 — TS surface + engine fast path (§12).** napi `graph`/`search` namespaces; land the
  matcher plumbing (**Tier 1.1a**: build-once dispatch, thread-local parser, zero-copy mmap, fused
  DFS — pure refactor, can interleave earlier) then the **SIMD per-rule-DNF pre-filter (1.1b)**.
  *Gate:* TS parity; **byte-identical differential vs. ast-grep**; measured speedup + % files skipped.

Cross-cutting from day one: a **benchmark + memory-perf harness** (none exists in-repo today) —
`divan`/`criterion` micro + `hyperfine` macro + **TMA top-down** / `perf c2c`/`mem` + deterministic
**`iai-callgrind` D1/LL-miss budgets** in CI; the **adaptive resource model** (§8.1); bounded,
**Arc-free** concurrency (§7, no untracked threads / unbounded growth); and data-derived constants
(no magic numbers). Per-phase perf gates: Phase 1 reports STLB-reach + `dtlb_load_misses.walk_active`
share at 10⁸ nodes; Phase 4/5 add `iai-callgrind` miss budgets + recall@k on the query/ANN path.

---

## 6. Open decisions & risks

- **FTS engine:** Tantivy (recommended) vs. a custom segment index. Tantivy is mature,
  mmap'd, incremental; validate its multi-version/epoch story at scale.
- **Local embedder:** which runtime (candle vs. ort/ONNX vs. fastembed) + default model +
  dimension. Must batch, run off-hot-path, and be swappable for a remote API.
- **Canonical store engine — RESOLVED (§9.6):** `fjall` (LSM) for the `blake3`→`NodeId` index +
  `redb` for the manifest; learned indexes rejected; bespoke hash-log LSM kept as the "control"
  if profiling demands.
- **ANN: port vs. crate — RESOLVED (§10): hybrid** — reuse the `rabitq-rs` quantization kernel;
  port + modernize the index/storage/build/update/filter natively (no crate covers on-SSD DiskANN
  + Arc-free CSR + tombstone compaction + filtered + deterministic together).
- **Cross-file resolution scope** — genuinely hard; start intra-language / import-path-based
  for 2–3 languages, expand behind the `Language` trait; never fake edges.
- **Distribution matrix** — per-platform napi/CLI prebuilts require CI cross-compilation
  (host-only builds work locally today).
- **Global allocator (§8.3):** `mimalloc` (THP-by-default) vs. `tikv-jemallocator`
  (`thp:always,metadata_thp`) for the server tier — feature-flagged, not the small-run default.
- **Page backing (§8.2):** `hugetlbfs` mount vs. THP large-folios + `MADV_COLLAPSE` for hot
  file-backed segments.
- **Succinct / graph crates (§11):** `sucds` vs `vers-vecs` vs `sux` (BP/DFUDS/Elias–Fano);
  Aspen-C-tree port vs `scc` for the dynamic edge overlay; `webgraph` cold-segment compression vs
  Roaring-only.

---

## 7. Concurrency & memory reclamation (Arc-free hot paths)

Non-negotiable: **ingest and search run fully in parallel, and no `Arc` refcount is touched
on any hot path** (per-element, per-node, per-edge, per-query-data). `Arc<T>` clone/drop are
atomic RMWs on a *shared* counter; when N cores clone/drop the same `Arc` the counter's cache
line ping-pongs (each RMW needs the line in MESI Modified/Exclusive), serializing work that
should scale linearly and creating a false-sharing hotspot. At 10⁹ LOC with a long-lived
daemon, that contention is the scaling wall. We remove `Arc` from hot paths by design, not by
tuning.

> **Honest scope.** `Arc` is unavoidable *inside* `tokio` task allocation and *inside*
> channel handles (`crossbeam-channel`/`flume` `Sender` clones bump an internal count). Those
> are constant, off the per-item/per-node path. "Arc-free hot paths" = zero `Arc` on the data
> a query traverses or an ingest worker produces — which we achieve completely.

### 7.1 Where `Arc` sneaks in — and the replacement

| `Arc` source | Why it hurts | Vorpal replacement |
|---|---|---|
| `Arc<DashMap>` + `Arc` values (LSP model) | shard `RwLock`s block; `Ref` holds a lock (daemon deadlock risk); per-value refcount | `papaya` (lock-free reads, `seize`-backed) / `scc::HashIndex`; values by **index**, not `Arc` |
| `Arc<Node>`, pointer-linked graph | refcount per edge chase; cache-hostile | **generational-index handles** (`thunderdome`/`slotmap`, 8-byte `Copy`) + CSR adjacency |
| `Arc<AppState>` cloned per `tokio` task | refcount traffic per request | `&'static Index` singleton (`OnceLock`/`Box::leak`); tasks *borrow* |
| `Arc` into `tokio::spawn` (`'static`) | forces sharing | `rayon::scope` / `std::thread::scope` → workers **borrow** `&` config |
| `Arc<Mutex<T>>` shared mutable | lock + refcount | single-writer-per-shard (no sharing) + RCU/`left-right`/seqlock for read-mostly |
| `Arc` around parsed trees/entities | alloc/free churn, refcount | per-worker **`bumpalo`** arena; trees/entities are locally owned `&'bump` |

### 7.2 Ownership without refcounts

- **Handles, not pointers.** `NodeId/EdgeId/SegmentId/ChunkId` are `Copy` 8-byte values —
  either content-addressed (`blake3` truncated) or **generational indices** (`thunderdome::Index`
  = 32-bit slot + 32-bit generation). Cross-references (graph adjacency, edge endpoints, index
  entries) store *handles*, never `Arc`/`&`. A stale handle to a reused slot fails the
  generation check → `None`, which turns logical use-after-free / ABA into a safe miss.
- **Graph as index-based CSR**, not a pointer graph: committed nodes/edges live in mmap
  segments; adjacency is offset/index arrays. Traversal = index chasing (cache-friendly, no
  refcount). This is the single biggest `Arc` eliminator.
- **Per-batch region allocation.** Each ingest worker owns a `bumpalo::Bump` (heterogeneous:
  scope tables, candidate entities, `bumpalo::collections::Vec`) and/or a `typed-arena` (for
  `&mut` homogeneous nodes). Parse → extract writes `&'bump T`; on commit the arena is
  **reset** (bulk O(1) free). No per-entity malloc/free, no `Arc`. Lifetime `'bump` (and, for
  readers, `'epoch`) encodes ownership the borrow checker enforces.
- **Single-owner + index cross-refs** everywhere a classic design would reach for `Arc`.

### 7.3 Reclamation: two-tier (choice + rationale)

Framed by the **ERA theorem** (Singh et al., 2022 — arXiv:2211.04351): no safe-reclamation
scheme gives all three of *Robustness*, *Ease-of-integration*, *Applicability*; pick per
context. Vorpal uses two tiers:

- **Tier A — coarse version/epoch *pinning* for readers over mmap segments + the published
  index version.** A query pins the current global version once (QSBR-flavored: readers are
  quiescent between queries). While pinned, no segment it can observe is `munmap`'d and no
  index version it reads is freed; compaction/unmap of a segment waits until every reader
  pinned at a version that referenced it has left. This is *the* fit for an append-only mmap
  store: immutable-after-publish data needs **no per-object reclamation at all**, only reader
  liveness at segment granularity — one pin amortized over the whole query.
- **Tier B — `seize` (Hyaline) for the fine-grained lock-free in-memory maps** (canonical-hash
  cache, node/edge delta overlay, offset-page cache, resize) where individual objects retire
  often. Hyaline gives EBR-class speed with **hazard-pointer-class memory bounds** and filters
  stalled threads, so a slow reader can't grow limbo lists unboundedly (the classic
  `crossbeam-epoch` failure mode — fatal for a 10⁹-scale long-running daemon).

Rejected alternatives and why: **plain `crossbeam-epoch`** — unbounded limbo under a stalled
pinner (kept only as a fallback / where already vendored). **Global hazard pointers
(`haphazard`)** — per-pointer advertise + fence on *every* edge chase wrecks traversal
throughput (fine-grained protection is the wrong cost model for pointer/index chasing; use
coarse pinning there). **`sdd`** (EBR variant, auto-drop `Shared`/`Owned`, `no_unsafe`) — a
strong, ergonomic option and the reclaimer behind `scc`; acceptable substitute for Tier B if
we standardize on `scc`. Wait-free reclaimers **Crystalline / WFE** (2021–2024) are noted for
the future if a component needs wait-free *reclamation* (not just wait-free reads).

### 7.4 Wait-free reads (never block a writer, never touch a refcount)

- **Manifest / visible-segment set / "head" version — RCU pointer-swap.** Writer builds a new
  immutable manifest (arena/`Box`), publishes with an `AtomicPtr` swap (`Release`); readers
  `guard.protect`-load it (`Acquire`) under a `seize` guard; the old manifest is `defer_retire`d.
  Readers never lock, never refcount.
- **Read-mostly rebuilt-in-bulk maps** (warm-index rule sets, resolved-symbol cache,
  name→[symbol ids] multimap) — **`left-right` / `evmap`**: two copies, wait-free `ReadHandle`,
  single writer applies an oplog to the standby then flips. Reads scale linearly with cores;
  cost is 2× memory + eventual consistency (fine for caches/warm state).
- **Small POD hot metadata** (per-shard head `{offset,count}`, current-epoch struct, stats) —
  **seqlock**: reader reads seq → data → re-reads seq, retries on change. Writer never blocks;
  restricted to `Copy`/POD (no embedded pointers that could dangle).

### 7.5 Streaming parallel ingest (bounded memory + backpressure, no `Arc`)

> **Status:** the work-stealing parse/extract core of this section is implemented — `vorpal
> index` fans per-file read → parse → extract → product-cache work (and the ANN embed pass)
> across a rayon pool, workers borrowing the shared extractor by `&`, with an order-preserving
> collect keeping output bit-identical to a serial build. The parallel walker also drives the
> stat manifest scan. Reference extraction runs the §12 fused traversal: one walk per file
> with dense kind-id dispatch (no per-node string comparison, no separate binder pass),
> equivalence-tested against the specification implementation. The extraction-product cache is
> the hand-rolled `.vpb` binary (bounds-checked, versioned; references index the file-local
> entity layout instead of repeating entity-path strings). Products are **self-validating** —
> each carries its source's stat stamp, so completed runs, interrupted runs, and searches
> banking their matches (`run`/`scan` feed the cache for every file they match) all produce
> equally replayable entries. The MCP daemon watches default-layout source roots via the OS
> (FSEvents/inotify, `notify`): queries revalidate lazily off a dirty flag that fails open to
> revalidation on any doubt, making steady-state freshness a single atomic check (measured
> 2.8 µs per full tool call). The commit path runs the **sharded single-writer** design:
> contiguous shards of the path-sorted product list each get a private lock-free writer in
> parallel, absorbed in order with id/heap rebasing — proven byte-identical to the serial
> writer and ~3× faster at repo scale (apply ~1 GB/s). The resolution pass is sharded the
> same way: reference chunks resolve in parallel against the immutable table (edge lists
> concatenate in chunk order — proven edge-for-edge identical to the serial loop), and the
> table itself builds from contiguous row-range shards absorbed in order (proven equal to the
> serial insertion). Ingest itself now **streams** through this section's full shape:
> byte-budget admission (CAS reserve on a cache-padded atomic, condvar-parked when exhausted,
> symmetric clamps so accounting always balances) gates in-order discovery; bounded
> crossbeam channels join admission → scoped extraction workers (borrowing config by `&` —
> no `Arc` on the hot path) → per-shard single-writer committers, whose sequence-ordered
> reorder buffers are bounded by the byte budget and are what break the backpressure cycle.
> A product exists in RAM only between extraction and application: peak transit is
> O(budget + queue capacities), independent of corpus size, and the output is proven
> byte-identical to the batch path — including under a deliberately starved budget. The
> per-worker arenas are realized as scratch reuse (source-read and product-encode buffers
> amortize to zero per file); contents that escape are copied exactly once, and the parse
> tree itself remains in tree-sitter's allocator. What still scales with the corpus is the
> essential output — the graph under construction — whose bounding is §9/§11 segment-spill
> territory, not §7.5's. The vector tier's Vamana construction is the §5 ParlayANN-style
> deterministic batch-parallel build (doubling-prefix rounds; frozen-graph parallel proposals;
> per-target parallel back-edge merges preserving serial per-target semantics), its beam
> search runs on a sorted-array beam with no hashing and no per-hop sort, and — per §3.4 —
> embeddings are off the commit hot path entirely: `ann.bin` is built lazily and validated
> by an xxh3 stamp of the node segment, so incremental re-indexes never rebuild the vector
> graph (measured on the Linux kernel: one-file re-index 168 s → 1.2 s). **No search ever
> waits on that build** — the query path is a three-tier freshness decision, every artifact
> checked against the stamp of the *loaded* KG bytes:
>
> 1. **Base-fresh** (`ann.stamp` == bin header `base_stamp` (ANN5) == live stamp): beam
>    search over the mmapped tier — the fast path.
> 2. **Overlay** (base is ≥1 edits behind, but `ann.files` — the per-file
>    `(path, id-range, xxh3 of (kind, content_hash) rows)` map written beside the base —
>    reconciles against the live KG): unchanged files' beam candidates remap by per-file id
>    offset, changed/new files' rows are embedded and scored *exactly* (FreshDiskANN-style
>    tombstones route around deleted rows; the dead-row count bumps the beam overfetch), and
>    the union feeds the ordinary exact rerank. Refused above ~15% changed rows.
> 3. **Fallback** (anything else — no tier, torn artifact combination, oversized overlay): a
>    fused exhaustive scan (embed → score → discard per row; exact recall; ~0.33 s at kernel
>    scale), which then spawns a **detached, file-locked background warm** (argv-sentinel
>    re-entry, registered binaries only, `VORPAL_NO_AUTOWARM=1` veto) so the next search
>    takes tier 1.
>
> Determinism contract: base artifacts (`ann.bin`, `ann.files`) are bit-identical across
> rebuilds and land via tmp+rename (never truncating a mapped file); tiers 1 and 3 coincide
> byte-for-byte at flat-exact scale, tier 3 ⊇ tier 1 in recall at Vamana scale, and tier 2's
> rankings converge to tier 1's at compaction — hash-gates must compare like state with like
> state.
>
> **Cache-validation contract (products, IMPROVEMENTS §6):** stat (size + mtime) is the
> cheap replay hint; the v6 product header's `source_xxh3` is the content identity. Digests
> are verified automatically for files in the *racy window* (mtime within 2s of the previous
> manifest write — the git racily-clean hazard, where an edit can restore size+mtime within
> timestamp granularity), and for everything under `VORPAL_VERIFY_CACHE=1`. The whole-tree
> reuse fast path verifies racy files' digests against the pack before short-circuiting.
> Format generations (`PRODUCT_FORMAT_VERSION`) are part of the key: extraction-affecting
> changes bump it, and foreign-generation products are cache misses, never errors.

Pipeline `discover → read(mmap)+blake3 → hash-skip → parse → extract → chunk → (embed) → flush`
as **fixed-capacity stages joined by bounded MPMC queues** (`crossbeam-queue::ArrayQueue`, or
`flume`/`crossbeam-channel` bounded). Fixed capacity *is* the backpressure: a full downstream
queue blocks/parks upstream, so **peak RAM = O(capacity × item_size × stages), independent of
repo size** — the property sylk's whole-repo buffer lacked. Queues move *owned* items
(`FileJob`, `ParsedBatch`) by value; no per-item `Arc`.

- **Work-stealing parse/extract** via `rayon::scope` so worker closures **borrow** read-only
  config by `&` (no `'static` ⇒ no `Arc`). Each rayon worker holds a **thread-local `bumpalo`
  arena**; parsed tree-sitter trees + extracted entities are locally owned and never shared —
  the whole CPU-bound stage is `Arc`-free.
- **Single-writer-per-shard commit.** Partition by path prefix into K shards; each shard has
  exactly one writer draining a shard-local MPSC (Vyukov / `crossbeam`) of finished batches,
  appending to its segment + updating its offset-index page. **No global write lock; shards
  commit fully in parallel.** Cross-shard coordination happens only at manifest publish (one
  RCU pointer swap, §7.4). Single-writer means the mutable shard state is *never shared*, so
  the borrow checker — not a lock — guarantees data-race freedom.
- **Byte-budget admission** for precise memory bounding: a `CachePadded<AtomicU64>` in-flight
  budget; discovery CAS-reserves bytes before reading and releases on flush. No `Arc`, exact
  RAM ceiling.
- **Embeddings off the hot path** (already in §3.4): commit text+structure first, enqueue
  `ChunkId`s (Copy) to a durable on-disk queue; an async `tokio` worker pool drains it under a
  `Semaphore` and reads chunk text from mmap by id — vectors never buffer, chunk data is never
  `Arc`'d.

### 7.6 Concurrent indexes without `Arc`-per-entry

- **Canonical content-hash index** (dedup/identity spine): cold path = on-disk sorted-segment
  LSM/B-tree (`redb` or bespoke); in front, a lock-free in-memory cache = **`papaya`**
  (lock-free reads via `seize`, deadlock-free, owned guards for async) — the `DashMap`
  replacement. Values inline/by-index, reclamation by `seize`; zero per-entry `Arc`.
- **Node/edge maps:** committed data is **index CSR in mmap** (no hashmap on the read path);
  the mutable *delta overlay* (recent, not-yet-compacted edges) uses **`scc::HashIndex`**
  (lock-free reads, `sdd`-managed) for search-time reads, or **`scc::HashMap`** (bucket
  `RwLock`s, no container lock) when the ingest side is write-heavy. `papaya` is the read-mostly
  default.
- **Why not `dashmap`/`flurry`/`leapfrog`:** `dashmap` — `RwLock` shards block, `Ref` holds a
  lock, values usually `Arc`; `flurry` — closed-addressing ⇒ allocator pressure; `leapfrog` —
  fast open-addressing but values effectively `Copy`/atomically-storable and less maintained.
  `papaya`/`scc` are current SOTA: sharded open-addressing (SwissTable-style, metadata-probed),
  atomic slots, epoch/`seize` reclamation, incremental (non-blocking) resize.

### 7.7 Correctness / no-UAF without `Arc`

- **Segments:** a reader pinned at version V sees only segments live at V; `munmap`/compaction
  of a segment is deferred until all readers pinned at referencing versions leave → the mapping
  outlives every guard that can observe it. No UAF on unmap.
- **In-memory maps:** `guard.protect` keeps a loaded pointer valid until the guard drops;
  `defer_retire` frees only after all active guards leave; Hyaline bounds limbo and tolerates
  stalls. No UAF on node/cache free.
- **`left-right`:** readers only ever see a fully-built side; the flip is one atomic; the writer
  waits for readers to vacate the old side before mutating. No torn reads.
- **seqlock:** odd/changed seq ⇒ retry; `Copy`-only payload ⇒ no torn value and no dangling
  pointer.
- **Generational handles:** generation mismatch on a reused slot ⇒ safe miss, defeating ABA.
- **Send/Sync boundaries:** single-writer-per-shard + immutable/atomically-published shared
  state means Rust's aliasing rules statically forbid the data races locks would otherwise
  guard.

### 7.8 Crates & code-level patterns

`seize` 0.5 (Hyaline) · `papaya` 0.2 (read-heavy lock-free map) · `scc` 2.x / `sdd` 4.x
(HashIndex/HashMap alt) · `boxcar` (lock-free append-only vec, stable addresses, for in-RAM
append tables) · `crossbeam-queue` 0.3 (`ArrayQueue`/`SegQueue`) + `crossbeam-channel` ·
`crossbeam-utils` (`CachePadded`, `Backoff`) · `bumpalo` 3.20 + `typed-arena` · `thunderdome`
0.6 / `slotmap` · `rayon` · `flume` (async+sync bridge tokio↔rayon) · `left-right`/`evmap` ·
`seqlock` · `memmap2` · `redb` · `tokio` (edge only) + `rmcp`. Testing: `loom`/`shuttle`
(model-check the reclamation glue — `seize`/`sdd`/`papaya` support `loom`), `miri` (UB on
unsafe), `criterion` + a scalability harness, `perf c2c`/`perf stat` (false sharing / cache).

```rust
// seize (0.5) — RCU pointer-swap for the manifest; no Arc, no lock, no torn read.
let guard = collector.enter();                                   // pin (Tier B / per-op)
let m = guard.protect(&manifest_ptr, Ordering::Acquire);         // *mut Manifest, protected
let hits = unsafe { (*m).segments_for(query) };                  // read immutable snapshot
// writer side, after building `next: *mut Manifest`:
let old = manifest_ptr.swap(next, Ordering::AcqRel);
unsafe { guard.defer_retire(old, seize::reclaim::boxed); }       // freed when readers leave
```

```rust
// Streaming ingest — bounded stages + rayon scope; parsed trees are arena-local, Arc-free.
let jobs = ArrayQueue::<FileJob>::new(CAP);      // backpressure = fixed capacity
rayon::scope(|s| {
  for _ in 0..workers {
    s.spawn(|_| {
      let mut bump = Bump::new();                 // per-worker region
      while let Some(job) = jobs.pop() {
        let tree = parse(&job, &cfg);             // borrows &cfg — no 'static, no Arc
        let ents = extract_in(&bump, &tree);      // &'bump [Entity]
        shard_writer(job.shard).push(commit_batch(ents)); // single-writer-per-shard MPSC
        bump.reset();                             // O(1) bulk free per batch
      }
    });
  }
});
```

### 7.9 Pitfalls & benchmarking

- **False sharing** — `CachePadded` every hot atomic (per-shard heads, epoch/seq counters,
  queue indices, byte budget); verify with `perf c2c` (watch HITM).
- **Stalled pinner** — a reader that pins and never quiesces stalls reclamation (unbounded
  RAM). Mitigate: Hyaline's bound + query timeouts + a watchdog; QSBR needs periodic quiesce.
- **Guards across long `.await`** delay reclamation — use `OwnedGuard`/owned pins deliberately
  and drop early.
- **seqlock under write-heavy load** starves readers — restrict to rarely-written POD.
- **`left-right`** = 2× memory + writer double-applies — read-mostly only.
- **rayon×tokio** — never block a tokio worker on rayon; bridge via `flume`/oneshot.
- **Benchmarking** — the real proof of `Arc`-avoidance is a **read-throughput-vs-cores
  scalability curve** (target near-linear) measured against an `Arc<DashMap>` baseline; plus
  p99 latency under mixed ingest+search, `perf stat` cache-miss deltas, `loom`/`shuttle` for
  correctness, `miri` for UB, and `heaptrack` to confirm the O(capacity×workers) memory ceiling.

---

## 8. Memory hierarchy, TLB & the adaptive resource model

**The reframing.** At 10⁹ LOC the dominant query cost is **not** I/O or compute — it is **TLB
page-walks and LLC misses on random graph/ANN access.** A big core's L2 STLB reaches only
~6–8 MB with 4 KB pages; a shard's hot ANN set is ~25–40 GB, so *nearly every random hop
page-walks* (4–5 dependent loads; ~400 µs/query on walks alone). Capacity was solved in §3.2;
this section solves *access cost*. It is greenfield — layout is the one thing you cannot
retrofit into an mmap'd on-disk format later.

### 8.1 Adaptive resource model (the cross-cutting layer)

vorpal must be as ruthlessly efficient on 5 files as at Meta scale — **nothing pre-sized for
"huge."** Two probes feed one per-store **policy object**:

- **Corpus probe** (free — the `discover` walk already enumerates files+sizes):
  `total_bytes`, `file_count` → projected *hot working-set bytes per store*
  (`est_nodes≈bytes/40`, `est_edges≈nodes·deg`, …) *before* allocating anything.
- **Hardware probe** (once, cached): page sizes + hugetlb pools (`/proc/meminfo`,
  `/sys/kernel/mm/hugepages`), NUMA nodes+distances (`hwlocality`), LLC size, dTLB/STLB
  entries → the machine's **TLB reach** at each page size.

The policy picks, **per store**, page size / arena size / prefetch distance / NUMA placement
at runtime. **One code path; the knobs scale.** Everything heavy is **lazy**: `MAP_NORESERVE`,
`madvise` marks *intent* without reserving, no arena pre-touch, prefetch compiles to a **no-op
at D=0**. A few-file run touches native pages + one 64 KiB arena + nothing else — instant and
tiny. The same `map_store(len, access, hotness)` / `arena.alloc()` / `maybe_prefetch(ptr)`
call sites serve 1 file and 10⁹ LOC; only the policy differs. **This is `ConfigForN` applied
to *structures*, not just ANN — no magic constants; every knob is a function of N / cores / a
measured statistic.**

| Knob | Baseline (few files) | Mid (10⁶–10⁷) | Meta (10⁸–10⁹) | Decision input |
|---|---|---|---|---|
| Page size / store | native 4 K/16 K, no madvise | `MADV_HUGEPAGE` when `hot_bytes > stlb_reach_4k` (~6 MB) | `MAP_HUGETLB 2M`; 1 G if `hot_bytes > stlb_reach_2M` (~3–4 GB) & pool exists | `hot_bytes` vs measured STLB reach |
| Arena chunk | one 64 KiB, reset | `clamp(pow2(batch),64K,2M)` | 2 MiB, huge-page-backed | `batch_bytes` |
| Prefetch distance | 0 (no-op) | ~4 (micro-sweep) | tuned 8–32 | latency÷body-work sweep |
| NUMA | single node | single node | shard→node + first-touch + interleave | `nodes≥2 && resident > node_dram×0.8` |
| Structures | Vec adjacency, no succinct/vEB/MPHF | CSR + Roaring + blooms | + succinct BP, vEB index, MPHF-on-seal | per-shard node/edge/degree stats (§11.6) |

### 8.2 Huge pages & TLB reach

2 MB pages widen STLB reach ~512× (~3–4 GB); the hot subgraph becomes TLB-resident. **Trap:
file-backed `MAP_HUGETLB` is anonymous-only** — `memmap2` 0.9.11 `MmapOptions::huge(Some(21|30))`
(`MAP_HUGE_2MB`/`1GB`) applies only to `map_anon()`, and silently no-ops on files.
Real paths: hot indexes on a **`hugetlbfs`** mount, or normal-file mmap + `MADV_HUGEPAGE` +
**`MADV_COLLAPSE`** (Linux 6.1+, synchronously promotes page-cache to 2 MB folios at warmup),
or an anonymous `MAP_HUGETLB` arena `pread` into. Use **mTHP** (6.10+, sub-PMD 16 K–512 K) as
the adaptive substrate; `defrag=defer+madvise` to avoid 50–100 ms compaction stalls; explicit
`MAP_HUGETLB` for the warm daemon so promotion never stalls a query; `MADV_POPULATE_READ` to
prefault. **Cold data (source text, f32 rerank vectors) stays 4 KB.** **macOS Apple Silicon
has no superpages (16 KB base pages)** → the whole huge-page path is `#[cfg(target_os="linux")]`
and macOS is the small/dev tier (leaning on 16 KB native reach); authoritative TLB profiling
runs on a Linux bench node.

### 8.3 Layout, IDs, prefetch, NUMA, allocation

- **SoA + hot/cold split + 64 B (and 2 MB segment) alignment**, zero-copy typed mmap
  (`zerocopy`/`bytemuck`, `rkyv` for variable-shape) — not the `Cow`/`Vec`/`usize` DTO shapes.
  **Indices over pointers** everywhere (§9.2 dense `NodeId`); `blake3` is external identity only.
- **Locality-preserving id order (delivers playbook #12's "graph reorder").** Commit-time
  `NodeId` assignment follows **path/subtree order** (§3.4 path-partitioned shards, §10.5
  path-sorted id space), so intra-file/package neighbors are already numerically clustered →
  their CSR rows + hot columns share cache lines and (huge) pages for free. For hub-heavy call
  graphs, **compaction may apply a within-segment locality relabel** (reverse Cuthill–McKee /
  community order — Starling/Gorgeous "reorder-for-page-locality") behind a compaction-time id
  remap (§9.8) — a bounded streaming pass on sealed/read-mostly segments only (adaptive; never on
  the ingest hot path) — cutting cache-line *and* TLB misses on multi-hop `callersOf`/beam walks.
- **Software prefetch** in ANN beam search and CSR frontier expansion — cfg-gated
  (`_mm_prefetch` x86 *stable*; inline `prfm pldl1keep` on aarch64, whose intrinsic is still
  unstable behind `feature(stdarch_aarch64_prefetch)`). Distance auto-tuned by a warm-up sweep (D=0 no-op for tiny inputs). `MADV_WILLNEED`
  ahead / `MADV_DONTNEED` behind for bounded-RSS streaming scans (Gorgeous/VeloANN pipelining).
- **NUMA** (daemon, ≥2 sockets only): pin each shard's memory + its query workers to one node
  (`mbind`/`MPOL_BIND`), first-touch from the owning thread, **interleave** read-mostly global
  indexes; disable `numa_balancing`. Crate: `hwlocality` 1.0.0-alpha (libhwloc ≥2.0, MSRV 1.85 = ours).
- **Allocation:** per-worker **`bumpalo` 3.20.3** arenas, reset-per-batch (O(batch×workers), no
  global allocator on the extract loop; huge-page-back once chunk ≥2 MiB); global = **`mimalloc`**
  (THP by default) or **`tikv-jemallocator` 0.7.0** `thp:always,metadata_thp` for the server tier
  (not the small-run default — inflates RSS). `CachePadded` every hot atomic (dovetails §7.6).

### 8.4 Profiling loop ("lookaside profiling")

Data-driven, per store & query kind: **TMA top-down** (`toplev`/`perf stat --topdown`) →
Backend→Memory→{DTLB_Load, L3, DRAM}. The DTLB node decides *huge-pages vs prefetch/layout*.
Exact counters: **`dtlb_load_misses.walk_active`** (the "lookaside tax"), `.walk_completed`,
`.stlb_hit`, `cycle_activity.stalls_l3_miss`, `mem_load_retired.l3_miss`; `perf c2c` (HITM /
false sharing), `perf mem` (local vs remote DRAM). **CI-friendly + deterministic:**
`iai-callgrind` D1/LL-miss budgets (no PMU needed) as regression gates; `perf-event` crate for
in-process counter assertions. macOS = Instruments "CPU Counters"/`xctrace` (directional only).

Sources: [kernel THP](https://docs.kernel.org/admin-guide/mm/transhuge.html) ·
[mTHP (LWN 954094)](https://lwn.net/Articles/954094/) ·
[madvise(2)](https://man7.org/linux/man-pages/man2/madvise.2.html) ·
[Gorgeous disk-ANN layout (arXiv 2508.15290)](https://arxiv.org/html/2508.15290) ·
[VeloANN cache-aware beam + proactive prefetch (arXiv 2602.22805)](https://arxiv.org/html/2602.22805v1) ·
[rustc THP +5% (Kobzol)](https://kobzol.github.io/rust/rustc/2023/10/21/make-rust-compiler-5percent-faster.html) ·
[Apple Silicon no superpages](https://developer.apple.com/forums/thread/713579) ·
[perf top-down](https://perfwiki.github.io/main/top-down-analysis/) · `memmap2`, `hwlocality`,
`iai-callgrind`, `mimalloc`/`tikv-jemallocator`.

---

## 9. On-disk storage format & IO

Realizes the byte layout, codecs, and IO under §3.2. Borrows the 2024–26 CWI/Spiral columnar
stack (**FastLanes** codec layout + **FSST/ALP** string/float codecs + **Vortex** zero-copy
Rust framework) but wrapped in a **bespoke, mmap-first, point-access + CSR framing** (vorpal's
hot path is graph pointer-chasing + `id→row` point lookup, narrower than Vortex's scan target).

### 9.1 The `.vseg` immutable segment container

Every store (nodes / edges-delta / edges-CSR / vectors / chunks / docs) is a sequence of
**immutable, page-aligned, mmap'd segments**; sealed segments never mutate (deletion =
tombstone). Layout: `[4 KiB header]` (magic, ver, `row_count`, `logical_id_base`, column dir
offset, header `blake3`) → `[column directory]` (per-column type/encoding/HOT|COLD/offsets/
blockmeta/dict/null-roaring/stats) → **`[HOT stripe: raw fixed-stride, 64 B-aligned]`** →
`[WARM/COLD blocks: FastLanes/FSST/zstd, 1024-val-aligned; cold 2 MB-aligned]` →
`[per-column BlockMeta]` (first_row, encoding, raw/stored len, **xxh3**, min/max = zone-map) →
`[4 KiB footer]` (segment `blake3` — torn-write detection). **Hot/cold column split is the
central lever:** a point lookup on a HOT column is `base + id_local·stride` → one cache line,
zero decode, zero deserialize (mmap `&[T]` via `zerocopy`). Node HOT stripe ≈28–32 B/node;
`content_hash`/`signature`/`docstring`/`pkg` are WARM/COLD. Vectors: HOT = 1-bit RaBitQ code
(fixed stride, SIMD), COLD = fp16/int8 rerank residuals.

### 9.2 Two id spaces (resolves the ID model across §7/§8)

- **Dense monotone `NodeId` u64** = `segment.logical_id_base + row`, assigned at commit. All
  internal cross-refs / CSR endpoints / offset math key on it → **O(1) direct**, no associative
  structure. It is a **physical locator, not a permanent identity** — valid from its assigning
  epoch and **forwarded (not preserved) across a locality-relabel compaction (§9.8)**; `blake3`
  carries permanent identity. Because ids are dense+contiguous per segment, the "paged offset index" collapses
  to a **~180 KB sorted segment directory** (`id_base → segment_id`, binary-searched, resident).
  *(Per-shard `u32` local ordinal + global `u48` handle; `u32` overflows at 10¹⁰ edges.)*
- **`blake3`(path:entityPath) → `NodeId`** is the identity/dedup/skip spine and the **only
  associative index** (§9.6). It's the *external* identity.
- Reconciliation with §7: durable *in-graph* cross-refs = offset-computable dense `NodeId`
  (forwarded at compaction, §9.8); references that must survive an arbitrary relabel store
  `blake3` and resolve via the canonical index; the in-memory *delta overlay* uses
  generational-index handles (`thunderdome`, ABA-safe); `blake3` = external permanent identity.

### 9.3 Edges = one LSM (write form ⋈ read form)

Hash-partition and CSR are the write- and read-optimized halves of **one graph LSM**:
**write** = per-shard append-only **delta log** (`shard = hash(key) mod K`, single-writer-per-
shard §7.5, no global lock); **read** = background-compacted **CSR (out) + CSC (in)**
(`row_ptr` delta+FastLanes, sorted `col_idx` Elias–Fano, parallel `edge_type`/`confidence`/
`evidence` columns). Each direction is partitioned by its *own* traversal key (out by `src`, in
by `dst`) → `callersOf`/`refsTo` hit **exactly one shard** (fixes playbook #7). Queries
read-merge compacted CSR ∪ small delta; compaction is the write→read transform (GVEL
prefix-sum+scatter, §11.3). Per-shard Roaring bloom for negative lookups.

### 9.4 Codecs (the Vortex/FastLanes stack)

| Data | Codec | Crate |
|---|---|---|
| HOT SoA cols, BBQ/RaBitQ codes, adjacency | **RAW** (mmap zero-copy, SIMD in place) | `zerocopy` 0.8 / `bytemuck` |
| WARM ints (ids, spans, degrees, `row_ptr`) | **FastLanes FFOR + delta** (1024-val, >100 B ints/s scalar decode) | `fastlanes` |
| Monotone seqs (CSR `col_idx`, postings, offsets) | **Elias–Fano** | `sucds` |
| Tombstones / sets / postings | **Roaring64** | `roaring` / `croaring` (SIMD) |
| Cold short strings | **FSST** (random-access per-string decode) | `fsst` |
| Doc/text blobs | **zstd + per-column trained dict**; `lz4` latency paths | `zstd` 0.13 (`ZDICT`), `lz4_flex` |
| Manifest / variable-shape metadata | **rkyv** archives (zero-copy) | `rkyv` 0.8 |

Blocks are FastLanes-1024 multiples sized to IO granularity (small 4–64 KiB for random access);
**per-column zstd-trained dictionaries** recover ratio on small blocks (decisive for
repetitive code). Decode into per-worker **huge-page arenas** (§8.3). Streaming compression on
ingest; dictionaries trained lazily once enough sample bytes accrue. (Vortex reports ~100×
random-access / 10–20× scan / 5× write vs Parquet — bleeding-edge but production at Spice.ai /
under eval at Polar Signals.)

### 9.5 IO strategy (per platform)

Reads = **mmap + `madvise` everywhere** (`RANDOM` for point/traversal, `SEQUENTIAL`/`WILLNEED`
for scans/compaction, `HUGEPAGE` hot, `DONTNEED` evict). Writes diverge: **Linux** = the
low-level **`io-uring`** crate for batched **group-commit** (linked write + `fdatasync` SQEs,
one submit) + **`O_DIRECT`** for big sequential cold-segment writes (avoid page-cache
pollution); **macOS** = **`F_NOCACHE`** + **`F_FULLFSYNC`** (mandatory on APFS — plain `fsync`
doesn't flush the drive cache) on a small blocking IO pool. `glommio` (thread-per-core, per-core
rings) is the option *if* we adopt thread-per-core (maps onto single-writer-per-shard) — start
with `io-uring` under our own per-shard threads.

### 9.6 Canonical index engine — resolved

Key = uniform-random `blake3`, value = `NodeId`, **write-heavy** ingest + point lookups, **no
range scans**. **`fjall` (LSM 2.8)** for the canonical index (write-optimized for random keys,
best write-amp; KV-separation off — values are 8 B), fronted by the §7.6 `papaya` cache;
**`redb`** (single-writer MVCC B-tree) for the manifest (atomic epoch flips). **Learned
indexes (RMI/PGM/ALEX) rejected for the core** — they need smooth/monotone keys + read-heavy
workloads; uniform hashes + write-heavy ingest defeat them (arXiv 2305.01237; EDBT'26). `sled`
rejected (highest disk use). Bespoke hash-log LSM is the "control" if profiling demands.
Build = **streaming external-merge** (fixed-RAM buffer → sort → spill sorted run → k-way merge
→ leveled segments); peak RAM = O(buffer).

### 9.7 Crash-safety, MVCC & compaction

Sealed segments write-temp → fsync → **atomic rename**. A single **manifest** (rkyv/redb txn:
`{epoch, live segment-ids, canonical checkpoint, index versions, tombstone versions}`) is the
truth; publish = write `manifest.<epoch>` (fsync) then flip `CURRENT` atomically
(RocksDB-style), and in memory RCU-swap the pointer with `seize` `defer_retire` (§7.4) — readers
never lock. Integrity: per-block **xxh3-64** (decode fast path) + per-segment **blake3-256**
(content addressing / torn-write). **MVCC** = immutable segments + epoch'd manifest + **Roaring
tombstone bitmaps** (delete = set bit; readers AND their epoch's bitmap). Recovery = load last
good `CURRENT` → drop orphan temp segments → replay bounded WAL tail; a segment is atomically in
the manifest or not, so no partial-segment corruption is observable (fixes playbook #8/#9/#11).
**Compaction** is background, **per-shard** (preserves single-writer): read live + apply
tombstones → write new segments → publish new epoch → `munmap`+unlink old **only after readers
pinned at referencing epochs quiesce** (§7.3 Tier-A) — non-blocking for readers; also transforms
edge delta-log → CSR (§9.3) and **optionally relabels for locality (§9.8)**.

### 9.8 Compaction-time `NodeId` remap (locality relabel without breaking identity)

Delivers §8.3's "graph reorder" as a *correct, bounded* mechanism, resolving the tension with the
stable-`NodeId` contract (§9.2): **`NodeId` is a physical locator, not an identity — `blake3` is
identity.** Compaction is therefore free to reassign `NodeId`s in locality order, via the
copying-GC **"forwarding pointer + remembered set"** pattern mapped onto LSM segment compaction +
epoch-MVCC, reusing the fact that compaction already rewrites both CSR directions.

- **Fused into compaction, not a separate pass.** Compaction already reads a shard's live
  nodes/edges, applies tombstones, and rebuilds **CSR(out)+CSC(in)** (§9.3/§9.7); it additionally
  assigns new dense `NodeId`s in **locality order** — cheap **RCM / BFS-order** (deterministic
  seed = lowest `blake3`), *not* full **Gorder** (whose reorder cost needs ~800+ queries to
  amortize; RCM/BFS amortize far faster). The compaction unit is a path-partitioned shard/subtree
  (already clustered, §3.4/§10.5), so this is a light refinement, and endpoints **inside the unit**
  are emitted with new ids for free → zero extra passes.
- **Forwarding table = remembered set.** Compaction emits `fwd: old_id → new_id` for relabeled
  nodes — a **dense array over the old id range** (Elias–Fano if tombstones sparsified it),
  published *immutable* into the new manifest epoch (RCU-swap, §7.4), resident, small (spans only
  the relabeled range). **Successive relabels compose** (each new table rewrites its targets
  through prior tables → maps straight to *current* ids), so translation is always a **single
  lookup, never a chain**.
- **Cross-unit edges — the one hard case.** An out-edge from `c` (un-compacted shard) to relabeled
  `u` still holds `dst = u_old`. Two mechanisms: (1) **read-time translate** — a traversal
  following endpoint `x` bounds-checks it against the epoch's relabeled ranges and, on hit, does
  one `fwd[x]` load → **one check + one indexed load per cross-unit boundary crossing** (not per
  hop; within-unit CSR already uses new ids), decaying as fixup proceeds. This is the
  external→logical translation the dynamic-graph-store literature measures (Teseo/RapidStore vertex
  index), but a *dense array* since our old ids are dense — not a hash table. (2) **Lazy scavenge**
  — when shard `S` is itself later compacted, its endpoints are rewritten through the current `fwd`
  and resolved entries retired; a `fwd` entry is GC'd once no segment at any pinned epoch
  references its old id (epoch refcount). Forwarding lives **at most until every referencing shard
  compacts once** — bounded by compaction cadence, like generational-GC remembered-set drain.
- **MVCC — no torn relabel.** Old segments + ids stay mapped until readers pinned at referencing
  epochs quiesce (§7.3 Tier-A, already the rule). A reader at epoch `E` translates only via `fwd`
  tables published ≤`E`; a reader at the old epoch uses old ids directly and never consults `fwd`.
  Tables are immutable once published → no reader observes a partial relabel.
- **Remapped vs. untouched (the invariant that bounds cost).** *Only* `NodeId`-keyed structures
  are touched: CSR/CSC (rebuilt — free); canonical `blake3→NodeId` (LSM *put*, stable key — one
  put per relabeled node, batched into the compaction's canonical checkpoint, §9.6/§9.7);
  containment forest (§11.4), tombstone bitmaps, offset directory (§9.2) — per-segment,
  rebuilt/natural. **Untouched** (stable-keyed): ANN (`ChunkId`/vector id; chunk→node is a
  forwarded graph edge), FTS (Tantivy join on `path`/`blake3`), and the in-memory delta overlay
  (`thunderdome` generational handles, `NodeId`-independent, folded in with fresh ids at compaction).
- **Adaptive — the common case pays nothing.** Relabel is **opt-in per compaction unit**, gated by
  §8.1/§11.6: engage only when a segment's measured locality (avg edge id-span / cache-line
  utilization, §8.4 loop) is poor enough that the reorder win (amortized over many queries) beats
  the forwarding cost. Path-clustered segments (the norm) → **no relabel, `fwd` dormant, zero
  overhead** — preserving "as efficient on 5 files as at Meta." RCM/BFS-order is deterministic →
  **bit-identical rebuild** (§10 bar).
- **Tuning lever (not a redesign) if cross-unit translation gets hot.** The relabel unit is a
  path-partitioned shard/subtree, so most edges stay intra-unit and `fwd` stays small. If the §8.4
  loop shows boundary translation is still hot on **hub-heavy call graphs** (a few very-high-in-
  degree nodes whose callers span many shards), **widen the compaction unit** — co-compact a hot
  node's in-neighbor shards so its inbound edges are relabeled in the *same* pass, trading a larger
  compaction working set for fewer forwarding lookups. Data-derived per hub (measured in-degree ×
  cross-shard span, §8.1/§11.6); a knob, not a new mechanism. (The symmetric read-side option —
  cap `fwd` translation by pinning hot hubs' ids across relabels — stays available if profiling
  prefers it.)

Sources: [FastLanes (VLDB'23)](https://www.vldb.org/pvldb/vol16/p2132-afroozeh.pdf) ·
[Vortex](https://github.com/vortex-data/vortex) · [Fjall 2.8](https://fjall-rs.github.io/post/fjall-2-8/) ·
[rust-storage-bench](https://github.com/marvin-j97/rust-storage-bench) ·
[io-uring](https://github.com/tokio-rs/io-uring) · [glommio](https://github.com/DataDog/glommio) ·
[Learned indexes on disk (arXiv 2305.01237)](https://arxiv.org/pdf/2305.01237) ·
[EDBT'26 learned-index LSM eval](https://openproceedings.org/2026/conf/edbt/paper-111.pdf) ·
[Teseo dynamic graph store (VLDB'21)](http://vldb.org/pvldb/vol14/p1053-leo.pdf) ·
[RapidStore (arXiv 2507.00839)](https://arxiv.org/html/2507.00839v1) ·
[in-memory dynamic graph storage (arXiv 2502.10959)](https://arxiv.org/pdf/2502.10959) ·
[graph-reordering survey (arXiv 2309.07581)](https://arxiv.org/pdf/2309.07581) ·
[amortized cost of graph reordering (SSDBM'25)](https://dl.acm.org/doi/10.1145/3733723.3733730) ·
[reordering for cache-efficient NNS](https://openreview.net/pdf?id=8LeCgKb6UX).

---

## 10. Semantic search — ANN (billion-scale, adaptive)

Backbone = **RaBitQ/Vamana-over-IVF** (DiskANN-over-IVF): port sylk's Vamana/IVF *skeleton* and
modernize every internal to 2024–26 SOTA. (sylk's "BBQ" *is* RaBitQ-1-bit repackaged, so this
is modernization, not redesign.)

### 10.1 Quantization — RaBitQ family

**Two-tier RaBitQ.** 1-bit RaBitQ for traversal/first-pass; **Extended RaBitQ** (2–6 bit) or
full precision for rerank. RaBitQ has a **sharp, unbiased estimator with a provable error
bound** (PQ/OPQ/SQ are heuristic, no bound) — so the bit-budget is *derivable*, not tuned: at
build, sample queries, measure the neighbor-distance margin, pick the smallest B whose closed-
form error-std < margin at the target recall quantile; rerank depth C follows from the same
bound. Asymmetric distance is native (full-precision query rotated by a stored-seed Hadamard/JL
transform; DB = codes) → VNNI integer accumulation. Space @10⁹, D=768: full 3.07 TB (SSD only),
1-bit **96 GB**, 4-bit 384 GB, adjacency (R=64) 256 GB. **Reuse the `rabitq-rs` kernel** (the
estimator math is subtle) — do not hand-roll.

### 10.2 Index + IO at scale

DiskANN-canonical **co-located `[1-bit code + adjacency]` pages** (one read → neighbors *and*
the codes to rank them), mmap + **huge pages** (§8.2); full-precision/4-bit rerank vectors in a
separate SSD-resident segment. **PipeANN `io_uring` pipelined beam search** (overlap I/O +
compute — <1 ms, 20K QPS, ~40 GB DRAM at 10⁹, top-10@90%) + software prefetch of the next code
block. **AiSAQ** pushes codes fully to SSD (~10 MB DRAM) at the top tier. IVF partitioning
(K data-derived, ~few-thousand vectors/partition) enables the streaming build, predicate
pushdown (§10.5), and SPFresh-style local update.

### 10.3 SIMD distance kernels

**`simsimd`** (runtime AVX-512/VNNI/AVX2/NEON/SVE dispatch) for cosine/L2/dot (f32, i8 rerank)
and **Hamming popcount** on 1-bit codes. **Hand-write the RaBitQ asymmetric estimator**
(query-bit-expansion + VNNI integer dot + per-vector scale/offset), `#[target_feature]`
x86 VNNI / NEON `sdot`, runtime-dispatched, scalar fallback + oracle test. FMA on the f32 rerank
path. (`std::simd` is still nightly in 2026 — prefer `simsimd` for stable + peak.)

### 10.4 Arc-free parallel + streaming build; incremental update

- **Build = ParlayANN lock-free batch-parallel Vamana** — decouples traversal from mutation,
  removes medoid/high-centrality contention that serializes naive concurrent Vamana, and is
  **deterministic** (same graph regardless of thread count → reproducible builds). Fits §7:
  rayon workers do read-only greedy search (`&` graph), candidate edges into per-batch `bumpalo`
  arenas, one parallel RobustPrune pass over generational-index CSR. Strictly better than the
  "lock striping" §3.2 first drafted.
- **Streaming/external build (bounded RAM):** IVF-first — train centroids on a sample,
  stream-assign vectors to partitions, build each partition's graph in RAM, persist, add
  cross-partition edges (SPANN-style). Peak RAM = O(largest partition + batch).
- **Incremental update = FreshDiskANN + SPFresh:** a small in-memory *delta* index absorbs
  inserts; deletes = tombstones; a background **StreamingMerge** folds delta+tombstones into the
  SSD index at cost ∝ change set while search runs (`search = delta ∪ SSD − tombstones`);
  **SPFresh LIRE** does in-place cluster-local reassignment (2.41× lower tail latency, 10 GB/2
  cores) instead of global rebuild. Compaction publishes via the §7.4 RCU manifest swap — search
  never stalls.

### 10.5 Filtered ANN (path-subtree / language)

Adaptive pre/in-filter. Predicates are structured + low-cardinality → **partition/zone-map
pushdown**: path-subtree = a contiguous range in the path-sorted id space (Roaring bitmap),
language = a precomputed bitmap; **skip IVF partitions** with no matches; within selected
partitions, **ACORN-style in-filter beam search** (traverse through non-matching nodes for
connectivity, collect only matching). **Selectivity decides correctness:** low selectivity →
**pre-filter + exact flat** search (guaranteed correct, cheaper); high → in-filter graph
(measured recall). Crossover is data-derived from `|matches|`.

### 10.6 Adaptive tiers + eval

**Tier 0** flat/brute-force (`simsimd`, exact, zero build) → **1** in-RAM Vamana + 1-bit codes →
**2** mmap split (graph+codes hugepage, f32 on SSD) → **3** SSD-resident graph + PipeANN +
AiSAQ. Promotion fires at `size > available_RAM / per_vector_cost(D,B,R)` — machine-derived, no
magic numbers. All params (IVF K, degree R, search list L, bit-budget B, rerank C, beam W)
data-derived at build; the RaBitQ rotation seed + params persisted → **bit-reproducible
rebuild**. Eval gate per tier: **recall@10/100 vs QPS** against exact ground truth, plus
measured quantization error vs. the RaBitQ bound.

**Port verdict (resolves §6): hybrid** — reuse the `rabitq-rs` kernel; port+modernize the
index/storage/build/update/filter natively (no Rust crate covers on-SSD DiskANN + Arc-free
CSR-in-mmap + tombstone compaction + filtered + adaptive + deterministic together;
DiskANN/PipeANN/AiSAQ/SPFresh are all C++). Reject `instant-distance`/`hnsw_rs` as core (HNSW,
in-RAM, no on-SSD/filter/Arc-free story) — possibly a Tier-1 fallback only.

Sources: [RaBitQ (SIGMOD'24)](https://dl.acm.org/doi/pdf/10.1145/3654970) ·
[Extended RaBitQ (arXiv 2409.09913)](https://arxiv.org/abs/2409.09913) ·
[rabitq-rs](https://github.com/lqhl/rabitq-rs) · [PipeANN (OSDI'25)](https://www.usenix.org/system/files/osdi25-guo.pdf) ·
[AiSAQ (arXiv 2404.06004)](https://arxiv.org/abs/2404.06004) · [SPFresh (SOSP'23, arXiv 2410.14452)](https://arxiv.org/pdf/2410.14452) ·
[ParlayANN (PPoPP'24, arXiv 2305.04359)](https://arxiv.org/abs/2305.04359) ·
[ACORN (arXiv 2403.04871)](https://arxiv.org/html/2403.04871v1) · [simsimd](https://github.com/ashvardanian/SimSIMD) ·
FreshDiskANN (arXiv 2105.09613).

---

## 11. Tree traversal, KG assembly & graph closure

**Load-bearing distinction — two "trees":** the **transient AST** (tree-sitter CST, µs-lived,
10²–10⁵ nodes, parse→extract→drop) gets only *cheap* techniques; the **persisted KG** (forever,
10⁷–10¹⁰ nodes, random+frontier traversal) is where succinct/CSR/vEB build cost amortizes.
Conflating them is the main risk: **succinct trees / cache-oblivious layouts / Euler-RMQ pay off
almost exclusively on the persisted KG.**

### 11.1 AST traversal (transient — cheap only)

- **SAX (enter/leave) event streaming** off the tree-sitter cursor (`goto_first_child`/
  `next_sibling`/`goto_parent`) feeding `CombinedScan`'s `kind_id→[rule]` dispatch — **no second
  derived tree**. Composes with the one-DFS-per-file design (§3.1).
- **SIMD kind-dispatch** (batch nodes by `kind_id`, vectorize predicate checks — SIMTREE-style).
- **DFS/Euler flatten into the per-worker `bumpalo` arena** for multi-pass extraction
  (pointer-free, cache-local, O(1) reset). **No succinct structure is built for the CST.**

### 11.2–11.3 Persisted KG = index-CSR + streaming (LSM-for-graphs) assembly

Committed graph = **index-based CSR in mmap** (§9.3). Build via **GVEL parallel counting-scatter**
(degree histogram → exclusive prefix-sum → parallel scatter into offset slots; ~1.9 B edges/s) —
cheap enough to *rebuild CSR from an edge-log on compaction*. **Streaming assembly** (the concrete
"how the stream becomes CSR" §3.4 hand-waved): single-writer-per-shard **edge-log append** (O(1),
no whole-repo buffer) → background GVEL compaction log→CSR when a shard crosses a threshold; the
uncompacted delta lives in a **batch-parallel compressed dynamic set** (Aspen C-tree-style, or
`scc`) so live queries see recent edges; external-memory build is pipelined; cold/hub adjacency
gap-compressed (WebGraph model) on seal. This is an LSM for graphs.

### 11.4 Containment forest — succinct + Euler/RMQ

The `defines/defined_in` relation is a genuine **forest**, read-mostly after commit, huge → store
per sealed segment as **succinct BP/DFUDS** (2n+1 bits vs 8–24 B/node pointers; `sucds`) with an
**Euler-tour + RMQ** index → **O(1) LCA / innermost-enclosing-scope / subtree-enumeration**. So
cross-file **scope resolution (§3.3) becomes a constant-time lookup, not a walk.** The general
code graph (calls/refs — *not* a tree) stays in CSR.

### 11.5 Closure = masked SpMV (unifies traversal + Datalog)

Semi-naive Datalog (§3.3) *is* iterated **masked sparse-matrix × frontier** over the CSR
(GraphBLAS semiring model): `frontier_{k+1} = Aᵀ ⊗ frontier_k`, the "new-this-round" set is the
mask (never re-expand settled nodes). Linear TC converges in O(diameter); doubling in O(log d).
Frontiers = **Roaring bitmaps**; traversal is **direction-optimizing push/pull** (Beamer — top-
down while the frontier is small, bottom-up set-intersection when it explodes; power-law call
graphs need this). **One vectorized kernel** serves `callersOf`/`refsTo`/`importersOf` (one
push-step SpMV) and their transitive versions (iterate to fixpoint) — faster *and* less code than
separate traversal + Datalog engines. (Implement masked-SpMV directly over CSR+Roaring; the
GraphBLAS *model* guides it, but a C dependency breaks the Rust-native/deterministic bar.)

### 11.6 Cache-oblivious index + MPHF; adaptive engagement

The offset/canonical search structures use a **van-Emde-Boas / cache-oblivious layout** — O(log_B
N) transfers, near-optimal L1→SSD with **no B tuning** (the theoretical embodiment of "nothing
pre-sized"). A hot shard's hash→id map is `papaya`+`fjall`; when **sealed** it freezes to a
**minimal perfect hash** (`boomphf`/BBHash, ~2–4 bits/key). **Adaptive rule:** cheap structures
(arena, DFS-flatten, SIMD dispatch, Roaring, CSR) are always-on; **build-heavy ones (succinct BP,
vEB, MPHF, WebGraph) engage only past data-derived thresholds on *sealed, read-mostly* segments**
where build cost amortizes — the structural analogue of `ConfigForN` (§8.1 table).

Honest caveat: **tree-sitter has no true streaming parser** (whole-file heap tree) — bounded AST
memory comes from processing one file's tree at a time in an arena, not a streaming parser; for
pathological single files, pre-scan top-level boundaries and parse chunks. Benchmark target: beat
*Codebase-Memory* (2026 tree-sitter→KG-over-SQLite) on traversal/closure latency.

Sources: [GVEL (arXiv 2311.14650)](https://arxiv.org/abs/2311.14650) ·
[Aspen C-trees](https://www.khoury.northeastern.edu/home/pandey/courses/cs7280/spring25/papers/aspen.pdf) ·
[SuiteSparse:GraphBLAS (TOMS)](https://dl.acm.org/doi/10.1145/3577195) ·
[Beamer direction-optimizing BFS (arXiv 1503.04359)](https://arxiv.org/pdf/1503.04359) ·
[Datalog° convergence (doi 3695839)](https://unpaywall.org/10.1145/3695839) ·
[cache-oblivious B-trees (Demaine)](https://erikdemaine.org/papers/CacheObliviousBTrees_SICOMP/paper.pdf) ·
[SIMTREE (PACT'13)](https://engineering.purdue.edu/~milind/docs/pact13.pdf) · `sucds`, `boomphf`.

---

## 12. Matcher fast path (structural search at scale)

The engine's cold-scan/daemon hot path. Ground truth (verified in code): today it does a full
`src.to_string()` copy + `Parser::new()` **per file**, **two whole-tree DFS passes** (suppression
then match), rebuilds the dispatch table + `canonicalize()`s **per file**, has **no literal
prefilter**, and `MetaVarEnv` = 3× `HashMap<String>` whose ellipsis-probe clones all three.

### 12.1–12.2 Plumbing (Tier 1.1a — pure refactor, do first) + zero-copy

- **Build the dispatch plan once per (language, rule-set)**, `Arc`-shared (`LangScanPlan`):
  **CSR dispatch** (`kind_id→[rule]` as flat offsets+indices, not `Vec<Vec<usize>>`), a
  `has_rule` bitmap for O(1) node-skip. Stop the per-file rebuild + `canonicalize()` syscall.
- **Reuse a thread-local `tree_sitter::Parser`** (`set_language` only on change) — no
  `Parser::new()` per file.
- **Zero-copy `MmapDoc`**: mmap the file (ripgrep-style size heuristic: `pread` into a reused
  thread-local buffer for <64 KiB, else mmap + `madvise`), parse from `&[u8]` — no `to_string()`.
  A **parse size cap** bounds per-worker RAM (tree-sitter builds a whole CST ~180–250× source
  size; 4 GiB u32-offset hard limit). Keep `StrDoc` for napi/wasm; `MmapDoc` for the CLI.

### 12.3 SIMD literal pre-filter (Tier 1.1b — the ⭐ win)

Reject files that cannot match before parsing. Compile each rule to a **Boolean DNF formula of
required byte-strings**, checked against the file's literal-hit set. **Correctness invariant:
per-token AND, never the concatenated pattern** — `console.log($A)` gates on `"console"` AND
`"log"`, *not* `"console.log"` (because `console . log`, comments, newlines match structurally;
the naive-concatenation phrasing was a latent false-negative bug). Composition: `All`→AND,
`Any`→OR (unfiltered if any branch lacks a literal), `Not`/bare-metavar→none, `Regex`→
`regex_syntax` literal extraction, relational→AND the target's literals too. **Strictness gate:**
`Signature`/`Template` erase terminal text → no mandatory literal. Quality filter (drop <3-byte /
punctuation tokens). Engine auto-selected by literal count — **`memchr::memmem` → `aho-corasick`
Teddy (SIMD) → full AC** — **zero new deps** (both already transitive via `regex`). Rules with no
provable literal → `always_parse`. The file's `live` set also narrows dispatch. **Fuse the two
DFS passes**, gating all suppression work behind a `"vorpal-ignore"` `memmem` check.

### 12.4–12.5 Interned env + bounded parallel

- **id-indexed `SmallVec` `MetaVarEnv`**: metavar names interned to dense `u16` slots at compile
  time → `SmallVec<[Option<Node>; N]>`, no hashing/`to_string()` per capture, and `Clone` becomes
  a cheap memcpy → the `$$$` ellipsis probe is near-free (on top of the existing COW).
- **Bounded `crossbeam-channel`** (backpressure → O(workers×batch) RAM) replacing the unbounded
  `std::sync::mpsc`; keep `ignore::WalkParallel` (no second rayon pool). **Determinism is opt-in**
  (`--sort=path` via reorder buffer) — arrival order isn't deterministic (correcting §3.4/§L4).

### 12.6 Scope (prevents over-claiming)

The prefilter accelerates **selective lint/query** scans (Semgrep's analogous change: 7.5 h → 90
s). The **"sees-everything" extraction** path (§3.1) is `Kind`-based (all `always_parse`) — you
parse everything to extract everything — so *it* scales via **parser-reuse + mmap + fused
traversal + content-hash skip** (§3.4), not the prefilter.

Correctness gate: **differential test vs. ast-grep** (byte-identical `(path,range,rule,env)`) +
**no-false-negative** property tests (`matches(prefilter) == matches(parse_all)`) + `--no-prefilter`
escape hatch. Harness: `divan` (micro) + `hyperfine` (macro, incl. % files skipped) + TMA.

Sources: [aho-corasick Teddy](https://github.com/BurntSushi/aho-corasick/blob/master/src/packed/teddy/README.md) ·
[Teddy (ICPP'21)](https://dl.acm.org/doi/10.1145/3472456.3473512) · [memchr memmem](https://docs.rs/memchr/2.8.2/memchr/memmem/index.html) ·
[Semgrep prefiltering](https://semgrep.dev/blog/2026/making-semgrep-rip-how-ripgrep-inspired-us-to-shave-hours-off-some-scans/) ·
[ast-grep optimize](https://ast-grep.github.io/blog/optimize-ast-grep.html) · [tree-sitter large files #222](https://github.com/tree-sitter/tree-sitter/issues/222).

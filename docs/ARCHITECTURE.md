# Vorpal — Architecture & Phased Build Plan

> **Status:** design / proposal. Prepared 2026-07-01. Supersedes nothing; complements
> `IMPROVEMENT_PLAN.md` (the ast-grep engine speed/semantics tiers).
> **Baseline:** ast-grep v0.44.0 has been copied into this repo and rebranded to build,
> run, and test as `vorpal` (crates `vorpal-*`, binary `vorpal`/`vp`, library type
> `Vorpal`, method `.grep()`, bindings `@vorpal/napi` · `@vorpal/wasm` · `vorpal_py`).
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
- **Streaming + incremental.** Extraction runs per file in the ingest pipeline; nothing
  buffers whole-repo. Content-hash skip avoids re-extracting unchanged files.

*Acceptance:* for a fixture repo, vorpal extracts a strict superset of sylk's entities with
AST-precise spans, uniformly across ≥3 languages, with zero regex in the path.

### 3.2 L2 — Storage substrate (the long pole)

Reimplement sylk's proven primitives in Rust, applying the scale fixes the research surfaced
(sylk's own storage held up; its *orchestration* did not — see §4). Primitives:

- **Segmented append-only data files** per entity kind (nodes / vectors / chunks / docs).
  sylk used a single monolithic `data.bin`; at 10⁹ LOC that's multi-TB single files. Vorpal
  segments (e.g. 64k-entity segments, like the ANN store already does), each mmap'd.
- **Paged / two-level offset index** (`id → offset`). sylk's dense in-heap `[]int64` is
  `8 bytes × maxID` (~80 GB at 10¹⁰ ids). Vorpal uses a paged, mmap-backed sparse index —
  never fully resident.
- **Hash-partitioned edge shards** (not sylk's fixed `SourceID/65536` range partition,
  which hotspots the newest shard and fans incoming-edge queries across all shards). Add
  per-shard bloom filters for negative lookups; maintain both directions or a target index.
- **On-disk canonical (content-hash) index** — an embedded LSM/B-tree (e.g. `redb`/`sled`
  or a custom sorted-segment index), *not* sylk's fully-resident `map[string]uint32`
  (~100 GB at 10⁹ symbols). This is the dedup + identity + incremental-skip spine.
- **Vamana/IVF ANN** — port sylk's self-contained engine (data-derived `ConfigForN`, BBQ
  1-bit quantization + full-precision rerank, tombstones). Persist mmap'd + sharded; build
  incrementally with lock striping; background tombstone compaction. (Rust ANN crates exist,
  but sylk's tuned design + our storage integration argue for a port.)
- **Full-text index** — a Rust FTS (Tantivy — Bleve's lineage/Lucene-model, mmap segments,
  incremental) replacing sylk's Bleve; shard by subtree; epoch-pinned versions.

*Acceptance:* insert/lookup are O(1)/O(delta) and bounded-memory under a synthetic 10⁸-node
load; a cold open mmaps in ms without loading indexes into heap.

### 3.3 L3 — Knowledge graph (code-focused, deep)

- **Nodes:** file, package/module, function, method, type (struct/interface/class/enum),
  field, variable, constant, import/export, (chunk for doc/vector linkage). String,
  content-addressed IDs (`blake3(path:entityPath)`), carrying domain/kind/name/path/pkg/
  signature/content-hash/span.
- **Edges:** `defines/defined_in`, `calls/called_by`, `references`, `imports/imported_by`,
  `implements`, `embeds`, `has_field/has_method`, `returns`, `of_type`, `overrides`,
  `similar_to` (semantic), `supersedes` (version) — materialized both directions.
- **Cross-file resolution — done right.** This is the hard part sylk left unsolved. Vorpal:
  intra-language, import-graph-driven resolution using the precise per-file scope tables;
  qualified-name + signature matching; confidence-scored with evidence; start with 2–3
  languages, expand behind the `Language` trait. Approximate edges are labeled, never faked.
- **Transitive closure:** port sylk's clean stdlib Datalog semi-naive evaluator for
  reachability (transitively-calls/references), incremental on edge add/remove.

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
4. **Partitioned / parallel commit:** shard by path prefix; per-shard append + offset index;
   no single global write lock. A top-level manifest maps shard → head.
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
  Model the warm-state server on ast-grep's LSP crate (DashMap of parsed state + hot-swap
  rules); pick the Rust MCP SDK (`rmcp`).
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

---

## 5. Phased build plan

Each phase is independently testable, benchmarked, and leaves the tree green. Ordering is
by dependency (storage is the long pole) with quick wins interleaved.

- **Phase 0 — Baseline rebrand.** ✅ Done (engine + bindings build/run/test as vorpal).
- **Phase 1 — Storage substrate.** Segmented append store + paged mmap offset index +
  hash-sharded edges + on-disk canonical index. Port the Vamana/IVF ANN. *Gate:* 10⁸-node
  synthetic load, bounded memory, O(1)/O(delta) ops, ms cold-open.
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
- **Phase 6 — TS surface + engine tiers.** napi `graph`/`search` namespaces; land
  `IMPROVEMENT_PLAN` Tier 1.1 pre-filter + cache. *Gate:* TS parity + measured speedups.

Cross-cutting from day one: a benchmark harness (none exists in-repo today), bounded
concurrency (no untracked goroutines/threads, no unbounded growth), and data-derived
constants (no magic numbers).

---

## 6. Open decisions & risks

- **FTS engine:** Tantivy (recommended) vs. a custom segment index. Tantivy is mature,
  mmap'd, incremental; validate its multi-version/epoch story at scale.
- **Local embedder:** which runtime (candle vs. ort/ONNX vs. fastembed) + default model +
  dimension. Must batch, run off-hot-path, and be swappable for a remote API.
- **Canonical store engine:** `redb`/`sled` vs. a bespoke sorted-segment LSM (control vs.
  time-to-build).
- **ANN: port vs. crate** — porting sylk's tuned Vamana/IVF gives control + storage
  integration; a crate is faster to stand up. Leaning port for the scale + determinism bar.
- **Cross-file resolution scope** — genuinely hard; start intra-language / import-path-based
  for 2–3 languages, expand behind the `Language` trait; never fake edges.
- **Distribution matrix** — per-platform napi/CLI prebuilts require CI cross-compilation
  (host-only builds work locally today).

# Index-format compatibility and migration policy

The version table below is maintained by `crates/index/tests/format_policy.rs` — it is
regenerated from the constants in source, so this document cannot drift from the code.

## Store identity

An index root holds `CURRENT` (a pointer file naming `gen/<content-id>`), immutable
content-addressed generation directories, and a root-level `products/` loose bank. A build
stages a complete new generation and commits it with one atomic pointer swap; readers see
the whole old index or the whole new one, never a mixture. GC keeps the new and prior
generations. A legacy flat root (no `CURRENT`) is read as-is and migrated into a generation
by its first rebuild (pinned by `legacy_flat_index_migrates_on_rebuild`).

## Format selection

The **bucketed** family layout is the default: products in `products/<k>.pack` +
`products/toc.bin`, nodes/evidence/edges/usage/sigs in per-bucket slabs with per-slab TOC
digests, generation identity as a Merkle fold of the family TOCs. It is what makes
incremental builds O(changed) — unchanged buckets hard-link across generations and the
scoped composes (stamp-cutoff / respan / defs-stable / defs-changed) splice buckets instead
of rewriting the corpus.

`VORPAL_FORMAT` selects the write format for the generation being built:

- unset, empty, or `next` — bucketed (the default; `next` is the historical opt-in name,
  kept as an explicit synonym),
- `flat` — the deprecated legacy monolithic layout (`products.pack`/`products.idx`,
  single-segment `nodes.vseg`, monolithic sidecars). An escape hatch only; it will be
  removed with v1 retirement,
- any other non-empty value — stamped to the phase log and treated as the default.

Readers are format-agnostic: both layouts load through the same surfaces, so existing flat
indexes keep serving and migrate to bucketed on their first rebuild (rebuild is the
migration, policy #1). The format is a property of a generation, never mixed within one.

## Policy

1. **Rebuild is the migration.** Graph segments are never migrated in place: a format bump
   means the next build re-derives everything from source, deterministically, into a new
   content-addressed generation. Builds are bit-reproducible, so migration is exact by
   construction.
2. **Caches retire by version; they are never reinterpreted.** Extraction products carry
   `PRODUCT_FORMAT_VERSION`; any mismatch is a cache miss that re-parses. Version bumps are
   mandatory whenever extraction output changes shape **or semantics** (the constant's doc
   comment records the history).
3. **Readers fail loudly or degrade honestly — never misread.** Foreign or newer graph
   segments fail `Kg::load` with an explicit error. Optional sidecars (evidence, ANN,
   postings, names) that are missing, stale, or foreign make their features answer
   "unavailable" (or fall back to exact-but-slower paths) while queries stay correct.
4. **Additive sidecars are the only writes an existing generation admits**, and each must be
   self-validating: the ANN tier and posting tier are stamped with the node-segment hash
   (plus model provenance for ANN); `names.idx` is backfilled once and validated on read.
5. **The durable identities are external ids (`eid:<32 hex>`) and the source tree**, not the
   on-disk format. Pre-1.0, the index format is explicitly not a cross-version interchange
   format; nothing needs migrating because nothing is lost by rebuilding.

## Version and freshness table

<!-- BEGIN GENERATED VERSION TABLE -->
| Artifact | Constant | Value | On mismatch |
|---|---|---|---|
| extraction products (`products/*.vpb`, pack bodies) | `PRODUCT_FORMAT_VERSION` (crates/ingest/src/product.rs) | 20 | cache miss → re-parse |
| product pack, bucketed layout (`products/<k>.pack` + `products/toc.bin`) — the default | `BUCKET_VERSION` (crates/ingest/src/pack.rs) | 1 | pack ignored → rebuilt by next build |
| product pack, legacy flat layout (`products.pack`/`products.idx`) — deprecated, written only under `VORPAL_FORMAT=flat`; reads retained | `PACK_VERSION` (crates/ingest/src/pack.rs) | 2 | pack ignored → rebuilt by next build |
| graph segments (`*.vseg`, `strings.heap`, `graph.bin`) | `FORMAT_VERSION` (crates/segment/src/format.rs) | 1 | `Kg::load` fails loudly → rebuild |
| evidence sidecar (`evidence.bin`) | `VERSION` (crates/kg/src/evidence.rs) | 2 | sidecar treated as absent → `why` reports no evidence |
| edge slabs (`edges/<k>.bin` + toc) | `VERSION` (crates/kg/src/edgestore.rs) | 1 | family treated as absent → scoped composes decline; next full build rewrites it |
| usage postings (`usage/<k>.bin` + toc) | `VERSION` (crates/kg/src/usagestore.rs) | 1 | family treated as absent → scoped composes decline; next full build rewrites it |
| sigs sketch ledger (`sigs/<k>.bin` + toc) | `VERSION` (crates/kg/src/sigstore.rs) | 2 | prior generation neither reused nor composed from → full pipeline rebuilds the family |
| include-reach graph (`reach.bin`) | `REACH_GRAPH_VERSION` (crates/resolve/src/reach.rs) | 2 | scoped composes decline (reach oracle unreplayable) → full pipeline rebuilds it |
| data-flow sidecar (`dataflow.bin`) | `VERSION` (crates/kg/src/dataflow.rs) | 1 | load fails loudly → rebuild (absent file ≠ mismatch: older generations answer no flows) |
| lexical posting tier (`postings.bin`) | `VERSION` (crates/index/src/postings.rs) | 2 | scan fallback → warm rebuilds |
| embedding semantics (`ann.model.json`) | `LEXICAL_EMBED_VERSION` (crates/ann/src/embed.rs) | 2 | ANN tier distrusted → exact fallback → warm rebuilds |
| calls-graph communities (`communities.bin`) | `VERSION` (crates/kg/src/communities.rs) | 1 | sidecar treated as absent → `community` answers `null`, `architecture` says not built → warm rebuilds |
| semantic engine calibration (`ann.calib`) | `ANN_CALIB_VERSION` (crates/index/src/lib.rs) | 1 | calibration treated as absent → structural routing floor (full-population fetches scan; the beam keeps everything below) → next warm re-measures |
| learned embedding model (`ann.model.bin`) | `LEARNED_MODEL_VERSION` (crates/ann/src/learned/persist.rs) | 3 | model unreadable/stale → lexical fallback stated in provenance → warm retrains |
<!-- END GENERATED VERSION TABLE -->

The ANN tier itself (`ann.bin` + `ann.files` + `ann.stamp`) is freshness-gated by the
node-segment stamp and the persisted model provenance rather than a standalone format
version: any mismatch routes queries to the exact fallback until a re-warm rebuilds it.
The reference spill (`.refs.spill`) is process-private scratch — created, read once, and
deleted within a single build; it is deliberately unversioned and never persisted.

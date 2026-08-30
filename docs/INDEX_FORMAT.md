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
| extraction products (`products/*.vpb`, pack bodies) | `PRODUCT_FORMAT_VERSION` (crates/ingest/src/product.rs) | 14 | cache miss → re-parse |
| product pack (`products.pack`/`products.idx`) | `PACK_VERSION` (crates/ingest/src/pack.rs) | 2 | pack ignored → rebuilt by next build |
| graph segments (`*.vseg`, `strings.heap`, `graph.bin`) | `FORMAT_VERSION` (crates/segment/src/format.rs) | 1 | `Kg::load` fails loudly → rebuild |
| evidence sidecar (`evidence.bin`) | `VERSION` (crates/kg/src/evidence.rs) | 2 | sidecar treated as absent → `why` reports no evidence |
| lexical posting tier (`postings.bin`) | `VERSION` (crates/index/src/postings.rs) | 1 | scan fallback → warm rebuilds |
| embedding semantics (`ann.model.json`) | `LEXICAL_EMBED_VERSION` (crates/ann/src/embed.rs) | 1 | ANN tier distrusted → exact fallback → warm rebuilds |
<!-- END GENERATED VERSION TABLE -->

The ANN tier itself (`ann.bin` + `ann.files` + `ann.stamp`) is freshness-gated by the
node-segment stamp and the persisted model provenance rather than a standalone format
version: any mismatch routes queries to the exact fallback until a re-warm rebuilds it.
The reference spill (`.refs.spill`) is process-private scratch — created, read once, and
deleted within a single build; it is deliberately unversioned and never persisted.

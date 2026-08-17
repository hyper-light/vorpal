# Embedding vorpal in a long-lived host

The CLI's process model (one process per build, exit is the cleanup) hides several
process-lifetime behaviors that matter the moment vorpal runs as a **library inside a
long-lived host**. This document is the contract for that mode, and the honest input to the
instance-model decision: embedded library vs session-scoped worker process vs a dedicated
KG-service pod.

## The one real hazard: the session interner

Resolution interns every distinct name/path/qualifier into a process-wide table
(`vorpal_resolve::intern`) so millions of references collapse to `u32` ids. Within one build
it is bounded by the corpus vocabulary (**~718k strings / ~17.6 MB for the Linux kernel**);
across many sessions in one process it grows monotonically — indexing different repos for
weeks accumulates every distinct name ever seen.

The valve (this is the "scoped-interner patch"):

- Strings live in reclaimable **arenas** (not leaks). `vorpal_ingest::intern_retained_bytes()`
  / `intern_retained_strings()` are the safe telemetry a host watches.
- `unsafe fn vorpal_index::reclaim_session_memory() -> ReclaimStats` frees everything at a
  session boundary. It panics if any `build_index*` call is in flight; the remaining safety
  contract is the caller's: **quiesce all vorpal calls and drop every vorpal value that
  predates the reclaim** (references, tables, spills — all internal to builds and dropped
  before `build_index` returns; loaded `Kg`s and query results are safe to keep, since
  persisted artifacts never contain interner ids). Kernel-scale cycle verified: build →
  reclaim to zero → rebuild → **bit-identical generation id**.
- One-shot processes never call it. The grammar-kind interner is exempt by boundedness (a
  few thousand strings, the union of node-kind names across compiled grammars).

If a host cannot uphold the quiescence contract, take the **session-scoped worker process**
model instead: same binary, one process per session, exit is the reclaim. The KG-service pod
is the same decision at fleet granularity. All three are legitimate; the interner is no
longer the thing forcing the choice.

## Hygiene facts (verified in code)

- **Allocator**: the library never sets a global allocator. jemalloc (`#[global_allocator]`,
  decay tuning, tree-sitter allocator unification) is the *binary's* memory profile, behind
  the default `jemalloc` feature of `vorpal-index` — hosts take `default-features = false`
  and keep their own allocator. Measured: allocator choice does not change index output
  (same generation id with and without).
- **Process spawning**: `autowarm` (the detached background tier-warm) only ever spawns
  when the binary's `main` called `autowarm::register()`. Library hosts never spawn
  processes; call `vorpal_index::warm_ann(dir)` yourself when you want tiers built.
- **Warm concurrency**: the in-process ANN/postings build lock is **per index directory**
  (plus a per-dir advisory file lock across processes) — a host serving many indexes warms
  them concurrently.
- **Environment channels** (process-global; a library host should treat them as CLI-only
  and use the programmatic equivalents): `VORPAL_VERIFY_CACHE` (= pass `CacheMode` to
  `build_index_with`), `VORPAL_NO_AUTOWARM` (moot without `register()`),
  `VORPAL_STREAM_BUDGET_MB` / `VORPAL_SHARD_CAP` (perf escape hatches),
  `VORPAL_PHASE_TRACE` (diagnostics).

## Cross-binary determinism: what holds and what doesn't

Vorpal's determinism gate is: **same binary → bit-identical generations** (double-build
verified at kernel scale continuously). Embedding adds a subtlety we measured rather than
assumed:

- Two binaries compiled with different optimization profiles (the workspace ships
  `lto = true`; a host's default profile may not) produce **byte-identical graph artifacts
  for every cleanly-parsed file** — and can differ inside tree-sitter **error-recovery**
  regions of damaged files (~99 nodes across ~72.5k kernel files, all in assembly-macro
  headers the C grammar cannot parse). This is optimization-sensitive behavior in the
  vendored tree-sitter C runtime's recovery path, upstream of vorpal
  (docs/UPSTREAM.md, "Known upstream behaviors").
- Confinement is exact, and parse health is the tool that proves it: with
  `--parse-health exclude`, the two differently-compiled binaries produce **byte-identical
  `nodes.vseg`, `graph.bin`, `strings.heap`, and `evidence.bin`** on the kernel; only the
  banked products of the damaged files differ (they are cached even when excluded).
- Practical guidance: build all cooperating processes from one profile; where mixed
  profiles are unavoidable and byte-agreement matters, run `exclude` and treat damaged
  files as the measured, inspectable exception the health surfaces already report.

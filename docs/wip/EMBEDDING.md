# Embedding vorpal in a long-lived host

The CLI's process model (one process per build, exit is the cleanup) hides several
process-lifetime behaviors that matter the moment vorpal runs as a **library inside a
long-lived host**. This document is the contract for that mode, and the honest input to the
instance-model decision: embedded library vs session-scoped worker process vs a dedicated
KG-service pod.

## The former hazard, retired: the interner is session-owned

Resolution interns every distinct name/path/qualifier so millions of references collapse to
`u32` ids (**~718k strings / ~17.6 MB for one Linux-kernel build**). The interner is now the
**scoped-interner design proper**: `vorpal_resolve::Interner` is an owned object each
`build_index*` call creates and drops internally — **reclaim is `Drop`**, and there is no
process-wide table, no reclaim API, and no quiescence contract at all.

The contract is enforced by the type system, not documentation: `NameId<'i>` (and therefore
`Reference<'i>`, `SymbolTable<'i>`, the spill) is branded with the session lifetime, so
holding any of them past their session **fails to compile** (pinned by a `compile_fail`
doctest on `Interner`). Loaded `Kg`s and every query result are session-free — persisted
artifacts never contain interner ids — so hosts keep those as long as they like. Kernel-scale
verification: consecutive sessions in one process produce **bit-identical generation ids**,
and each session's vocabulary frees when its build returns.

Embedded hosts therefore need to do *nothing*: call `build_index` repeatedly; memory is
bounded per build. Hosts driving the lower layers directly create their own
`Interner::default()` per session and let scope end it; `Interner::retained_bytes()` /
`retained_strings()` give per-session telemetry. The worker-process and KG-service-pod models
remain available for *other* reasons (isolation, fleet topology), but no memory behavior
forces them. The grammar-kind interner remains process-wide and is exempt by boundedness (a
few thousand node-kind names).

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

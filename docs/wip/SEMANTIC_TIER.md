# The high-tier embedding option — design

Status: DESIGN ONLY (nothing here is implemented). Owner decision points are at the end.

The ask: a semantic-quality tier above the deterministic lexical hasher, **owned end to
end** — our tokenizer, our math, our optimization — and **never an independent download**.
No runtime model fetching, no inference-framework dependency (no ort/candle/ggml), no
tokenizer crate.

## What the seam already gives us

These are verified facts of the current code, and they shape everything below:

- **One embedder seam.** `active_embedder()` (crates/index/src/lib.rs) is the single
  construction point; build, warm, cold fallback, and rerank all flow through it. The
  `Embedder` trait is two methods (`dim`, `embed`); the per-node recipe
  (`embed_node_into`: name ×2, signature, file basename) is shared by every path.
- **The tier is a warm sidecar, not an artifact.** `ann.bin`/`ann.files`/`ann.model.json`/
  `ann.stamp` are stamp- and provenance-gated, machine-local, and **excluded from the
  generation content id**. A model change flips provenance → queries route to the exact
  fallback → the next warm rebuilds. A *learned* high tier therefore cannot fork
  generation determinism — the safety contract for it already exists and is tested.
- **Honest labeling is already in the contract.** `ModelProvenance.learned` exists and is
  documented: a learned adapter says `learned: true` and never masquerades as the
  deterministic default.
- **Scale envelope** (kernel, current lexical tier): 2.76M nodes − imports embedded at
  dim 256 f32 → `ann.bin` 811 MB; tier build 12.8 s wall / 178 s user; tier-vs-exact
  top-10 overlap 66/80 over 8 queries (the beam finding in BENCHMARKS).

## Why the lexical default caps out

Hashing gives *unrelated tokens orthogonal directions by construction*. `kmalloc` and
`alloc_pages` share a token and correlate; `sk_buff` and `socket buffer` mostly don't;
`parse` and `decode` never will. Everything the user wants from a "semantic" tier —
synonyms, paraphrase queries, and a meaningful multi-phrase AND — needs directions that
encode *usage*, not spelling.

## The ladder: three rungs, strictly ordered by ownership

### Rung 1 — corpus-derived token semantics (PPMI + randomized SVD) — fully owned

Learn a dense vector per vocabulary token **from the index being built**, with closed-form
math:

1. **Vocabulary**: the existing tokenizer (camel-hump + boundary splitting) over every
   embedded part; top-N tokens by frequency (N ≈ 128k) get learned rows, the tail keeps
   hash buckets (graceful OOV, never a panic).
2. **Co-occurrence**: tokens co-occurring within one definition's parts (window = the
   definition) → sparse counts → **PPMI** weighting.
3. **Factorization**: rank-d (256) truncated SVD via deterministic randomized SVD
   (Halko–Martinsson–Tropp; fixed seed, fixed reduction order). PPMI+SVD is the
   Levy–Goldberg equivalence: word2vec-class quality without SGD.
4. **Definition vectors**: SIF-weighted token mean (a/(a+freq)) minus the first principal
   component (Arora et al.) — closed form, deterministic.

Cost model (kernel): the co-occurrence pass rides the same per-node parts the lexical
build already touches (O(total tokens)); sparse PPMI nnz est. 20–80M; ~10 power-iteration
sparse matvecs at rank 256 → seconds, parallel. Definition embedding cost ≈ today's
hashing. Target: warm tier build ≤ 2× current wall.

Small-corpus floor: below a measured token/definition count, PPMI is noise — the tier
falls back to lexical and the provenance says so (stated, never silent).

### Rung 2 — graph-contrastive refinement — fully owned, and *only vorpal can do this*

A small learned head on top of rung 1: a two-layer MLP (256→256→256, GELU; ~130k params)
trained at warm time with an InfoNCE-style contrastive objective whose **positives come
from the knowledge graph we already built**: caller↔callee pairs, same `community`
members, `similar_to` pairs, co-changed files' definitions. Negatives sampled with a fixed
seed. Deterministic minibatch order, fixed epoch count, our own SIMD GEMM (NEON/AVX2
microkernels) — CPU seconds-to-minutes at kernel scale.

This is the piece no downloaded generic model has: embeddings that know *this codebase's*
structure. It is also where "own the optimization" earns its keep: int8-quantized stored
vectors (811 MB → ~200 MB `ann.bin` at d=256) with a recall re-check gate.

Determinism contract for both rungs: **bit-reproducible per machine** (double-warm →
byte-identical `ann.bin` + `ann.model.json`, pinned by test). Cross-ISA bit-identity is
explicitly NOT required — the tier is machine-local and stamp-gated, exactly like the
Vamana build today.

### Rung 3 — vendored micro-transformer — a decision point, not a plan

Only if rungs 1+2 plateau on the eval set: vendor a permissively-licensed small encoder's
weights (MiniLM-L6-class, 22M params, Apache-2.0/MIT) **into the repo** as int8
(~20–25 MB), with OUR WordPiece tokenizer and OUR forward pass (attention, LayerNorm,
GELU, int8 GEMM). No download, no inference dependency — but two honest costs:

- **Binary/package weight**: +20–25 MB on a ~71 MB binary, felt by npm/PyPI/musl
  packaging (coordinate with the release-workflow owner before any such change).
- **Throughput**: 2.7M definitions × ~64 tokens × 22M params ≈ 10^15 FLOPs — tens of CPU
  minutes even with good int8 kernels. Kernel-scale full-corpus embedding is *not* a
  per-warm cost anyone should pay; it would have to be incremental/idle-time work, and
  measured before being believed.
- Weights would be third-party-*trained* (provenance recorded): we own the math, not the
  training. That tension is why this rung is last.

## Option surface

`semanticTier: lexical | learned` in vorpalconfig.yml (+ CLI flag/env), flowing through
`active_embedder()` (becomes selection-aware; returns the trait object). Default stays
`lexical` until the eval gates below say otherwise. Provenance `model_id` distinguishes
(`lexical-hash` / `corpus-svd+graph-mlp`), `learned: true` set honestly; tier invalidation
on any switch is automatic via the existing gate. Multi-phrase semantic AND (separate,
also unlicensed today) is a query-time feature that becomes genuinely useful at rung 1+.

## Evaluation gates (before any default change)

1. retrieval_eval gains paraphrase/synonym cases that lexical hashing fails **by
   construction** — labelled, fixture-scale, precision=recall targets stated per tier.
2. Kernel-scale: the existing 8-query exact-vs-tier methodology plus a paraphrase set;
   report recall@5 / MRR per rung against lexical.
3. Perf gates: warm build ≤ 2× current wall; query embed ≤ 1 ms; double-warm
   bit-identity; `ann.bin` size (f32 and int8) reported.
4. Small-repo behavior: the fallback floor demonstrated on a tiny fixture.

## Risks, named

- PPMI/SIF on tiny corpora → the stated fallback floor.
- Training nondeterminism → pinned by double-warm byte-identity tests, single place for
  every seed.
- Quality ceiling of rungs 1+2 vs a real pretrained encoder → that is exactly what the
  eval set decides; rung 3 stays a measured decision, not a default.
- A learned per-index model means vectors are not comparable across indexes — they never
  were (the tier is per-generation already); stated in provenance.

## Decision points (owner)

1. License rungs 1+2 (fully owned, zero download, corpus-adaptive) as the `learned` tier?
2. d = 256 (keep) or evaluate 384 alongside?
3. int8 stored vectors from day one (with the recall gate) or after f32 lands?
4. Rung 3 stance: reject outright (keeps the no-third-party-weights line absolute) or
   keep as a post-eval decision point?

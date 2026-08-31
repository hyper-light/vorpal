# The high-tier embedding option — evidence-based design

Status: DESIGNED AND DETERMINED (2026-08-30). Nothing here is implemented yet. Trade-offs
are ranked on correctness/robustness/performance/efficiency/speed only — implementation
complexity is explicitly unweighted (owner directive); it is absorbed by the staged plan's
checks (§4). Determinations in §3; the only remaining owner calls are the Stage-6 trigger
waiver and the go-ahead to start Stage 0.

The ask: a semantic-quality tier above the deterministic lexical hasher, **owned end to
end** — our tokenizer, our math, our optimization — and **never an independent download**.

## 1. What the seam already gives us (verified in code)

- **One embedder seam.** `active_embedder()` (crates/index/src/lib.rs) is the single
  construction point; build, warm, cold fallback, and rerank share the per-node recipe
  (`embed_node_into`: name ×2, signature, file basename). `Embedder` is `dim` + `embed`.
- **The tier is a warm sidecar, not an artifact.** `ann.bin`/`ann.files`/`ann.model.json`/
  `ann.stamp` are stamp- and provenance-gated, machine-local, excluded from the generation
  content id. A learned model cannot fork generation determinism; a model change flips
  provenance → exact fallback → re-warm. `ModelProvenance.learned` already exists.
- **Scale envelope** (kernel): 2.76M embedded nodes, dim 256 f32 → 811 MB `ann.bin`;
  tier warm 12.8 s wall / 178 s user. Embedded surface ≈ **9.9 tokens/definition**
  (p50 9, p99 23; cpython 20k-callable sample). Corpus range: ~10⁵ (small repo) to
  ~10⁸ (kernel-class) tokens.

## 2. What the research says (three fan-out reports, 2026-08-30; primary sources cited)

### 2a. The indexing surface is right, and lexical must stay authoritative for short queries

- On the human-judged CodeSearchNet benchmark the **Neural Bag-of-Words baseline beat
  self-attention** (NDCG 0.574 vs 0.493; ElasticSearch beat the CNN/RNN too) — keyword
  matching over identifiers is the first-order signal in code search (Husain et al. 2019,
  arXiv:1909.09436). NCS/UNIF replicate this on real Stack Overflow queries
  (arXiv:1905.03813).
- **Identifiers ≫ structure**: GraphCodeBERT's entire data-flow machinery adds +2.0 MRR
  while naming-aware pretraining adds +5.0 (arXiv:2009.08366); DietCode finds method
  signatures the highest-attention statement class and body pruning cheap (−3 MRR for
  −40% tokens, arXiv:2206.14390); identifier obfuscation collapses model quality
  (arXiv:2510.03178). Agentless localizes SWE-bench edits from **signatures alone** at
  78.7% vs 67.7% for full-body embedding retrieval (arXiv:2407.01489).
  → Our name×2+signature+basename surface is the evidenced surface.
- **Short keyword queries collapse every neural model to ~0 NDCG@10** — 0.6B–8B
  embedders and rerankers included ("Beyond Retrieval", Xue et al. 2026,
  arXiv:2605.04615; abstract verified directly). → The deterministic lexical/exact
  channels stay authoritative for short symbol-shaped queries; the semantic tier is a
  **long-query / NL-intent specialist**, fused, never a replacement.
- Ceiling calibration: 256-dim static models reach ~92% of MiniLM on MTEB average but
  only ~72–82% on retrieval, roughly **tying BM25** (potion-8M 0.442 vs BM25 0.452
  NanoBEIR NDCG@10); GPU-contrastive statics reach 0.503–0.512 vs MiniLM 0.562
  (huggingface.co/blog/static-embeddings, 2025). Dense wins big only where query and
  code share no vocabulary (BM25 0.95 vs dense ~26 on APPS; CoIR 2024).

### 2b. Count-based factorization is the *empirically correct* choice at our corpus sizes

- SGNS implicitly factorizes shifted PMI (Levy & Goldberg, NeurIPS 2014). With the right
  knobs, **PPMI+SVD ties SGNS on similarity and beats it below ~10⁸ tokens** — at
  1M–100M words count-based wins outright; SGNS needs ~10⁹ (Sahlgren & Lenci, EMNLP
  2016, D16-1099; TACL 2015 Q15-1016). Vorpal's corpora live entirely in the
  count-based-wins regime. Analogy structure is what SVD sacrifices — irrelevant to
  retrieval.
- The knobs that matter (TACL 2015): context-distribution smoothing **α=0.75** (the one
  always-safe knob, ≈+3 pts); **no PMI shift under SVD** (k=1; shifting costs SVD
  −14 pts); small windows favor count models; **symmetric eigenvalue weighting Σ^p,
  p=0–0.5** ("using SVD 'correctly' (p=1) is bad": −15 pts).
- Pooling: **uSIF** (closed-form weight a from the frequency table alone; piecewise
  removal of top-5 sentence PCs) beats SIF by ~7.6% avg, and SIF beats plain averaging
  by 10–30% (Arora ICLR 2017; Ethayarajh W18-3012). **ABTT** on the word space (drop the
  mean + top d/100 PCs — they encode frequency, not meaning) adds ~+4% STS
  (arXiv:1702.01417). The 2024–25 distilled statics (Model2Vec/potion) get their quality
  from *exactly these tricks* — PCA (ABTT) + Zipf/SIF re-weighting — applied to teacher
  token tables; a 2025 EMNLP paper finds learned re-weighting converges on "a more
  nuanced SIF" (arXiv:2506.04624). The closed-form path and the trained path meet at the
  same math; we can own it outright.
- **Subwords fix the small-corpus floor**: fastText char 3–6-grams hashed into bounded
  buckets — trained on 1% of Wikipedia it beats CBOW on 100% for rare words
  (arXiv:1607.04606). OOV identifiers get compositional vectors deterministically.
- **Dimension by closed form**: the PIP-loss criterion picks d from the spectrum
  (optima ≈100–300 for 10⁶–10⁸ tokens; symmetric p=0.5 is robust to over-parametrizing;
  NeurIPS 2018). 256 remains the cap; 1024→256 truncation costs ~1.5% NDCG in modern
  static models.
- No teacher-free training-free frontier beyond count+factorize exists as of 2026 —
  Model2Vec/potion/WordLlama all require a teacher transformer or LLM codebook.

### 2c. Graph refinement works — if and only if it is relation-aware and edge-clean

- **Retrofitting** (Faruqui NAACL 2015): convex, ~10 Jacobi sweeps, ~5 s per 100k×300;
  deterministic by construction with fixed sweep order. But similarity-style
  retrofitting on a *directional* relation bought +0.9 accuracy where **relation-aware
  (per-edge-type linear) retrofitting bought +12.2** (Lengerich COLING 2018), and noisy
  graph edges turn retrofitting into a **−3.5–5.2% retrieval loss** (2026, withdrawn
  paper — weak source, but the direction matches the Roam result). Counter-fitting's
  lesson: preserve the original topology (strong anchor term) or downstream retrieval
  degrades (Mrkšić NAACL 2016).
- Vorpal is unusually well-positioned here: our edges are **typed** (calls/imports/
  of_type/implements/similar_to/changes_with), **confidence-graded** (evidence grades),
  and we already compute **omnipresent hubs** — the exact pruning/weighting inputs the
  literature says decide gain vs regression.
- Magnitude honesty: expect GraphCodeBERT-scale lift (+~2 MRR overall), concentrated on
  sparsely-named symbols (TADW pattern: graph helps most where text is weakest) — which
  is precisely the lexical tier's weak spot.
- **NUDGE** (arXiv:2409.02343, 2024): directly optimizing *stored* vectors under a norm
  constraint beats trained adapters (+10% NDCG, minutes on CPU) — structurally the same
  move as retrofitting; keeps the "edit corpus vectors, not query adapters" direction.
- **int8 storage is settled practice at 256 dims**: scalar quantization with per-dim
  0.99-quantile calibration, quantized *after* refinement, oversample ~1.3–2× and
  float-rescore top-k → ≤1% recall loss (Qdrant ≤0.3% at 384-dim; Elastic −1.05% erased
  by 5 extra candidates; HAKARI −1.95 → −0.09 with rescore). **Binary quantization is
  unsafe below ~768 dims** — ruled out at d=256.

## 3. Determinations (quality axes only; complexity absorbed by checks, per owner directive)

**D1 — Tier composition: BUILD Tiers 1+2, with a staged Tier 2.5 escalation.**
Tier 1 (subword PPMI+SVD+ABTT+uSIF) is the maximally correct foundation at 10⁵–10⁸
tokens — count-based factorization *beats* SGNS in this regime (D16-1099), and the
closed-form post-processing is the same math the trained 2024–25 models converge on.
Tier 2 (relation-aware retrofit) is the highest-evidence refinement for our inputs
(typed, graded, hub-pruned edges). Tier 2.5 — NUDGE-style deterministic constrained
optimization of the stored vectors with graph positives — is NOT rejected for
complexity: it is staged behind Tier 2's eval, kept only if it measurably wins
(NUDGE's +10% NDCG over adapters says the direction is live).

**D2 — Dimension: ADAPTIVE via the PIP criterion, clamped to [64, 256], recorded in
provenance.** Closed-form, corpus-appropriate (small corpora get smaller d — the PIP
theory says optimal d shrinks with noise), robust by clamping, auditable by recording.
Fixed-256 survives only as the clamp ceiling. Check: the corpus-size sweep must show
adaptive ≥ fixed at every scale, else the gate reverts to fixed.

**D3 — Storage: BOTH paths in one slice; f32 is the oracle, int8+rescore becomes the
default only by measurement.** int8 scalar quantization (per-dim 0.99-quantile,
calibrated after retrofit) with oversample+float-rescore is settled practice at ≤1%
loss; the recall gate (fused NDCG@10 ≥ 99% of f32 on every query class, kernel scale)
decides the default automatically. Binary quantization stays ruled out at d=256.

**D4 — BM25 postings channel: IN SCOPE.** Evidence-backed (ties distilled statics on
retrieval), exact, deterministic; lands as its own stage with fusion weights re-tuned
under the eval harness. The fused system must be ≥ current on every split.

**D5 — Vendored encoder: a TRIGGERED stage, not a plan and not an outright rejection.**
Trigger: the NL-intent split still misses its target after Tier 2.5. Firing it requires
an owner waiver (third-party-trained weights — a values call, not a quality call) and
release-size coordination. Candidates pre-named (all-MiniLM-L6 Apache-2.0 22M;
gte-modernbert-base Apache-2.0 149M; CodeRankEmbed 137M, license to verify then);
curation-beats-parameters (CoRNStack 2024) rules the choice if fired.

**Fusion invariant (from the short-query collapse result):** the exact/lexical channels
remain authoritative for short symbol-shaped queries in every configuration; the
semantic tier only ever adds. This is enforced by the eval gates, not by hope.

## 4. Implementation plan — every stage carries its checks

Ordering rule: measurement precedes optimization; nothing advances past a stage whose
gates aren't green; every stage lands behind the provenance gate (model id/version bump
→ tier auto-invalidation → exact fallback), so a bad stage can never poison an index.

**Stage 0 — the eval harness (first, before any model code).**
Labelled sets with query-class splits: short-keyword (≤3 tokens — the collapse regime),
NL-intent paraphrases (no vocabulary overlap with targets, by construction), and
sparsely-named-symbol queries; fixture-scale (exact labels) + cpython/kernel (graded).
Fusion-level NDCG@10/MRR/recall@5 per split; per-channel ablations; corpus-size sweep.
CHECKS: baseline (current lexical fusion) scores recorded and pinned; harness
determinism (two runs byte-equal); label review pass; the existing 8-query tier-vs-exact
overlap runner wired in.

**Stage 1 — Tier 1: tokenizer → counts → PPMI → randomized SVD → ABTT → uSIF.**
Sub-steps and their oracles:
- Subword tokenizer (identifier split + char-3–6-gram hash buckets, bucket count scaled
  to corpus): golden tests incl. unicode/edge identifiers; OOV compositionality test
  (unseen identifier gets a nonzero, deterministic vector).
- Co-occurrence + PPMI (cds α=0.75, no shift): hand-computed values on a toy corpus,
  exact equality; sparsity/ceiling guards return errors (no-panics rule).
- Randomized SVD (fixed seed, symmetric Σ^0.5): against exact dense SVD on small
  matrices (subspace angle ≤ 1e-5); orthonormality residual ‖QᵀQ−I‖ bound asserted;
  power-iteration count fixed; NaN/Inf → typed error, never a panic.
- ABTT + uSIF (closed-form a from the frequency table; top-5 sentence-PC piecewise
  removal): property tests (removing components reduces the removed directions' energy
  to ~0; weights monotone in frequency); pinned golden vectors on a fixture corpus.
- PIP-based d selection: deterministic curve; clamps; d + every hyperparameter into
  `ann.model.json` provenance.
DETERMINISM GATES: double-warm byte-identity of `ann.bin` + `ann.model.json`; thread-count
invariance (1 vs N threads byte-equal — fixed-order reductions proven, the MKL-class
pitfall made structurally impossible); cross-run stamp stability.
PERF GATES: warm tier ≤ 2× current wall at kernel scale (phase-stamped per sub-step);
query embed ≤ 1 ms.
EVAL GATE: fused NL-paraphrase split strictly better than baseline; short-keyword split
not regressed; small-corpus floor demonstrated (below floor → lexical fallback, stated
in provenance).

**Stage 2 — Tier 2: relation-aware convex retrofit.**
Per-relation weights β(type, grade); degree normalization; omnipresent hubs and
below-grade edges excluded (reusing the existing hub machinery and evidence grades);
strong anchor α; both penalty forms — identity and per-relation linear A_r — implemented
and A/B'd on the eval (the +0.9 vs +12.2 result decides empirically for OUR graph).
CHECKS: convexity exploited as an oracle — Ψ(Q) strictly non-increasing every sweep,
asserted; fixed sweep order → byte-identity; ≤10 s at kernel scale (measured);
REGRESSION GUARD (the noisy-graph lesson): if retrofit degrades short-keyword or
exact-name results on a given corpus, the stage auto-disables for that corpus and says
so in provenance — never a silent regression.

**Stage 3 — int8 SQ + rescore (beside f32, gate decides the default).**
Per-dim 0.99-quantile calibration on the real corpus, quantize after retrofit, float
originals retained for top-k rescoring, oversampling factor tuned by measurement.
CHECKS: quantize(dequantize) idempotence; calibration determinism; RECALL GATE ≥99% of
f32 fused NDCG on all splits at kernel scale (else f32 stays default); size verified
(~4×); format-policy row + torn/foreign-bytes tests for the new region.

> **DISPOSITION (2026-08-31): closed by delta analysis — no new machinery.** This
> stage was drafted before the ann-frontier campaign's measured decisions; the shipped
> tier already is the int8+rescore design (per-row-scaled i8 codes, exact integer
> dots, overfetching beam, full-precision pool re-scoring), measured at recall 0.9937
> and re-evidenced on the learned+retrofitted tiers (tier-vs-exact top-10 set
> agreement 77/80 linux, 58/60 cpython). "Float originals retained" is superseded by
> the mapped model as the f32 truth; the per-dim 0.99-quantile scheme targets a ≤1%
> loss bar the shipped per-row max-abs scheme already meets (0.63%), so switching
> would reopen a closed, measured design for no demonstrated headroom. Full table in
> docs/wip/BENCHMARKS.md "Stage 3: closed by delta analysis".

**Stage 4 — BM25 postings channel.**
Exact Okapi BM25 (k1, b fixed and recorded) over the existing postings tier; fusion
weights re-tuned on the eval harness.
CHECKS: hand-computed BM25 scores on a toy corpus; determinism trivial but pinned;
EVAL GATE: fused metrics ≥ current on every split, short-keyword split expected to
improve.

> **DISPOSITION (2026-08-31): infrastructure shipped; channel measured OFF.** The
> postings v2 format (TF + doc lengths + avgdl), the exact scorer with its parity
> twin, goldens, and the ≥2-distinct-token match floor all ship tested — but the
> fourth fused list failed the kernel eval gate twice (short-keyword 0.206 → 0.109
> plain / 0.137 floored; descriptive 0.947 → 0.790) and is pinned off
> (`BM25_CHANNEL = false`). Root mechanism: the kernel's answers live in subwords
> exact-token BM25 cannot see (sock ≠ socket), so its rank list is wrong evidence
> that scale-free RRF rewards anyway; the stage's original target (the lexical
> short-keyword collapse, 0.103) had already been fixed by Stages 1–2 via subword
> generalization (0.206). cpython IMPROVED (all 0.308 → 0.392) — recorded as the
> motivation for a future per-corpus, warm-time-gated enable. Tables in
> docs/wip/BENCHMARKS.md "Stage 4".

**Stage 5 (conditional) — Tier 2.5: constrained direct optimization (NUDGE-style).**
Trigger: sparse-name or NL-intent splits show headroom after Stage 2. Deterministic
seeded optimization of stored vectors in a norm ball around their Stage-2 positions;
positives from typed, grade-weighted edges; fixed batch order; deterministic reductions.
CHECKS: byte-identity (seeded); eval must beat Stage-2 output on the target splits
without regressing others — else recorded as measured-and-rejected in this document.

**Stage 6 (owner-gated) — vendored code-specialized encoder.** Per D5.

**Ready-when-licensed:** multi-phrase semantic AND becomes real per-phrase semantics at
Stage 1+; it remains separately unlicensed and is not scheduled here.

## 5. Standing risks, named
- Small-corpus PPMI noise → the demonstrated floor + fallback (Stage 1 gate).
- Graph-noise regression in retrofit → the auto-disable guard (Stage 2).
- Quantization drift on future model changes → the recall gate re-runs per model bump
  (provenance ties them).
- Eval overfitting to our own labels → the kernel overlap methodology and corpus sweep
  are the counterweights; labels reviewed, splits reported separately, never averaged
  into one number.

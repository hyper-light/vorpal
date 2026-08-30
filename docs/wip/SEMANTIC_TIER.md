# The high-tier embedding option — evidence-based design

Status: DESIGN, researched (2026-08-30). Nothing here is implemented. Trade-offs are ranked
on correctness/robustness/performance/efficiency/speed only — implementation complexity is
explicitly unweighted (owner directive). Owner decision points at the end.

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

## 3. The design, re-derived from the evidence

**Tier 1 — corpus-derived subword semantics (the foundation).**
Identifier-split + char-3–6-gram tokens hashed into bounded buckets (compositional OOV);
definition-window co-occurrence with cds α=0.75; PPMI, no shift; truncated randomized SVD
(fixed seed) with symmetric Σ^0.5; ABTT on the token space (mean + top d/100 PCs); uSIF
pooling (closed-form a; piecewise top-5 PC removal) over name×2+signature+basename;
d picked by the PIP criterion, capped at 256; L2 rows. Every step closed-form,
deterministic, owned. Positioning per the ceiling data: the **vector channel of the
existing fusion** — the exact/lexical channels stay authoritative for short queries.

**Tier 2 — relation-aware graph retrofitting of definition vectors.**
Convex retrofit with per-relation weights (β per edge type × resolution grade),
degree-normalized, omnipresent hubs and low-grade edges excluded, strong anchor α;
evaluate identity-style vs per-relation linear (A_r) penalties — the +0.9-vs-+12.2 result
says the linear form may matter for directional `calls`. 10 fixed-order Jacobi sweeps →
bit-reproducible; est. ≤10 s at kernel scale (10M edges × 10 sweeps × 256 dims).

**Storage/query.** int8 SQ per the settled recipe (calibrate 0.99-quantile per dim after
retrofit; float originals kept for rescoring; oversampled beam + float rescore). 811 MB →
~200 MB at kernel scale inside a ≤1% recall budget. Query embeds through the same model
(sub-ms). Multi-phrase semantic AND becomes meaningful at Tier 1+ (per-phrase vectors are
real semantics, not hash blends).

**Determinism contract.** Double-warm → byte-identical `ann.bin` + `ann.model.json`
(pinned by test): fixed seeds, fixed iteration orders, fixed thread counts in every
reduction (the MKL/OpenBLAS thread-order pitfall is the known hazard). Machine-local, as
today; cross-ISA bit-identity not required (stamp-gated sidecar).

**Deliberately rejected/parked, with reasons attached:**
- *Binary quantization*: unsafe at d=256 (evidence above).
- *Query-side linear adapters*: need ~1.5k labeled query→symbol pairs we don't have, and
  NUDGE-style corpus-vector editing dominates them anyway; revisit if usage data ever
  exists.
- *SGD-trained contrastive head as Tier 2*: retrofitting attacks the same objective with
  a convex, evidence-backed method whose failure modes (noisy edges, directional
  relations) we can control with signals we already compute. If eval shows Tier 2
  plateauing, an InfoNCE head over graph positives is the named escalation — complexity
  is not a counter-argument (owner directive), only its unproven marginal quality is.
- *Vendored micro-transformer* (was rung 3): the calibrated numbers shrink its promise —
  MiniLM-class sits at 0.562 NanoBEIR vs 0.503–0.512 for the best statics, and code-
  specialized small models (CodeRankEmbed-137M > voyage-code-002; curation beats
  parameters, CoRNStack 2024) would mean vendoring ~140 MB, ~10¹⁵ FLOPs per kernel-scale
  re-embed, and third-party-trained weights — for a lift concentrated on long NL queries
  that the fusion's lexical channels don't already serve. Stays a **post-eval decision
  point**, with candidates named (all-MiniLM-L6 Apache-2.0 22M; gte-modernbert-base
  Apache-2.0 149M; CodeRankEmbed 137M — license to verify), reopened only if Tiers 1+2
  miss the eval bar on NL-intent queries.

**A separate lever the research surfaced**: our lexical channel is hashed-cosine, not
BM25 — and BM25 ties distilled statics on retrieval benchmarks. A BM25-scored postings
channel is an independent, cheap, deterministic upgrade to evaluate alongside Tier 1.

## 4. Evaluation gates (before any default change)

1. Labelled eval extended with **query-class splits**: short keyword (≤3 tokens — the
   collapse regime; lexical must win or tie), long NL-intent paraphrases (the semantic
   tier's raison d'être), and sparsely-named-symbol queries (Tier 2's target).
2. Fusion-level metrics (NDCG@10 / MRR / recall@5), not channel-only; corpus-size sweep:
   tiny fixture / cpython / kernel; per-tier ablation (T1 vs T1+T2 vs +int8).
3. Perf gates: warm tier build ≤ 2× current wall; query embed ≤ 1 ms; double-warm
   byte-identity; small-corpus floor demonstrated (below the floor: lexical fallback,
   stated in provenance).
4. Tier-vs-exact overlap methodology (the existing 8-query kernel harness) rerun per tier.

## 5. Decision points (owner)

1. License Tiers 1+2 (fully owned, zero download, corpus-adaptive; closed-form + convex)?
2. Adaptive d via PIP (cap 256) vs fixed 256?
3. int8-with-rescore from day one, or after f32 correctness lands?
4. The BM25 postings-channel evaluation alongside — in scope?
5. Vendored-model stance: reject outright, or keep as the named post-eval decision?

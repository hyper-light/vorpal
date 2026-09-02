# Encoder research — faster, better code-retrieval encoders for the search tiers — 2026-09-02

Status: RESEARCH ONLY (read-only sweep of primary sources; nothing installed, downloaded,
or benchmarked). Question: what should replace or augment the query-time CodeRankEmbed
reranker on 79K–8.9M-definition corpora WITHOUT adding query latency, under hard
constraints — CPU only (Apple M-class, no GPU), fully local at query time, MIT-compatible
licenses, redistributable weights, scales to millions of definitions. Every number below
is cited to the source that owns it; "unknown / not published" is stated where a number
does not exist. Companion docs: SEMANTIC_TIER.md (the shipped tiers), BENCHMARKS.md
(Stage 6 tables), ANN_FRONTIER.md (the ANN sidecar).

## 0. The measured context this ranks against (ours, 2026-09-02; NDCG@10 / MRR / recall@5)

| corpus | defs | lexical | learned static | + encoder rerank |
|---|---|---|---|---|
| Linux kernel | 8.9M (5.9M `#define`) | 0.299 / 0.375 / 0.229 | 0.313 / 0.346 / 0.250 | **0.222** (hurts) |
| CPython | 163K | 0.137 | 0.412 | 0.410 / 0.556 / 0.500 (recall up) |
| vorpal (self) | 79K | 0.571 | 0.612 | 0.622 / 0.625 / 0.650 |

Paraphrase-class queries ("near duplicate code detection" → `similar_pairs`) score 0.0 in
every tier. Query cost today: 0.3–0.5 s uncached at k=10, 0.64–0.97 s at k=25, ~90 ms with
the candidate cache warm (BENCHMARKS "Stage 6": ladder 3.62 → 1.29 → 0.887 s mean at k=25;
~90 ms per short sequence on the f64 correctness-first pass). Candidate surface: name ×2 +
signature + file basename (`embed_node_into`, crates/index/src/lib.rs; ≈9.9 tokens p50,
23 p99 — SEMANTIC_TIER §1).

**Two framing facts that decide the ranking (coordinator datum, 2026-09-02):**

1. **Paraphrase is a candidate-generation gap, not a ranking gap.** On a fresh self-index
   with the learned tier warm, `vorpal search "near duplicate code detection"` at k=25,
   k=200 and k=1000 never surfaces `similar_pairs` or `Sketch` in the fused list; "who
   called what at runtime" never surfaces `ObservedStore` / `ingest_traces` at k=1000. A
   query-time reranker over the fused top-k — today's CodeRankEmbed shape, any
   cross-encoder, any ColBERT-rerank — **cannot fix paraphrase by construction**. Only a
   channel that scores documents the lexical/learned channels never surface can:
   a doc-side dense index (full or hot subset), a learned-sparse/expansion channel, or a
   distilled static model used doc-side. Every option below is tagged
   **[RERANK-ONLY]** or **[CANDIDATE-GENERATING]**.
2. **The kernel regression (0.313 → 0.222) is a rank-1..k reorder failure on subword
   identifiers** (`alloc_skb`, `tcp_cong_avoid_ai`, `request_threaded_irq`). A rerank-only
   option inherits it unless it can show it preserves exact-identifier evidence. Note the
   unreconciled datum: BENCHMARKS' 2026-08-31 "fused-winner pin" table reports the kernel
   all-NDCG going 0.298 → **0.313 GREEN** with the pin; the 2026-09-02 figure above is
   0.222. Whether 0.222 is the unpinned variant, a different k, or a label-set change must
   be established before any rerank A/B is scored (findings-before-fixes).

**A recorded scale-law needs correcting.** SEMANTIC_TIER §4 Stage 6 and BENCHMARKS line
~841 say doc-side encoding at kernel scale is "~10¹² FLOPs (hours of CPU)". Those two
halves are inconsistent. NomicBERT-137M has ≈23.4M embedding-table params (30,528 × 768)
and ≈113M non-embedding params; a forward pass costs ≈ 2 × 113M × tokens FLOPs, i.e.
≈2.7 GFLOP for a 12-token surface. 8.9M definitions × 2.7 GFLOP ≈ **2.4 × 10¹⁶ FLOPs** —
at 1 TFLOPS sustained that is ≈6.7 h, which is the "hours" the note meant. 10¹² FLOPs
would be one second. The conclusion (never the warm-time row embedder for the FULL kernel)
survives; the number does not. Section 6 redoes the arithmetic with stated assumptions.

## 1. Tiered ledger — every candidate, with the numbers that exist

Legend: **CG** = candidate-generating (can touch paraphrase); **RO** = rerank-only. License
verdict: OK = MIT/Apache-2.0/BSD; **NC** = non-commercial or use-restricted → excluded.

### Tier A — fits all hard constraints AND has code-domain evidence at ≤ ~150M params

| model | params / dim / max seq | license | code-retrieval evidence (primary) | CPU latency | CG/RO |
|---|---|---|---|---|---|
| **nomic-ai/CodeRankEmbed** (today's tier 2) | 137M / 768 (dim inherited from base Snowflake/arctic-embed-m-long, which states 768) / 8192 | MIT (card) | CoIR NDCG@10 **60.1**, CSN MRR **77.9** (card + CoRNStack paper Table 4: Apps 21.1, CosQA 36.3, SQL 58.8, CSN 83.7, CSN-CCR 86.9, CodeTrans 78.8, StackOverflow 32.8, CodeFeedback 82.3, Contest 75.7, DL 45.2). Independent re-evals: 59.14 (MinishLab harness), 60.47 MTEB-Code (LightOn table). SWE-bench-Lite function localization R@5 50.0 / R@10 59.1 (CoRNStack); Acc@5 51.82 vs BM25 31.75 (SweRank). | none published; ours ≈90 ms/short seq f64, 0.887 s per k=25 rerank | RO today; CG if precomputed doc-side (§6) |
| **lightonai/LateOn-Code-edge** | **17M** / **48-d per token** (multi-vector) / doc 2048, query 256; base Ettin-17M (mixedbread edge-colbert) | **Apache-2.0** (card) | MTEB-Code avg **66.64** (card; pre-train 57.50); vs granite-small-r2 55.84, EmbeddingGemma-300M 68.76, **CodeRankEmbed 60.47** in the same table. CSN NDCG@10 Python 0.9244, Go 0.9607, JS 0.7937. Trained on 2,117,771 samples over CoIR+CSN (i.e. **CoIR-trained — its CoIR-family numbers are in-domain**). | none published (see next-plaid row) | **CG** (doc-side multi-vector index) and token-level RO |
| **lightonai/LateOn-Code** | 149M / 128-d per token / 2048; ModernBERT-base | Apache-2.0 | MTEB-Code avg **74.12**; vs GTE-ModernBERT 71.66, Qwen3-Embedding-0.6B 75.42, C2LLM-0.5B 75.46, CodeRankEmbed 60.47 (card table). Same in-domain caveat. | none published | CG / RO |
| **Alibaba-NLP/gte-modernbert-base** | 149M / 768 / 8192 | Apache-2.0 | Self-reported CoIR **79.31** (card, "20 benchmarks"). **Disputed**: IBM Granite R2 paper Table 11 reproduces it at **71.5**; LightOn's table 71.66; mteb issue #1861 ("Discrepancies in CoIR results") is **open** — three harnesses disagree. ModernBERT paper: CSN NDCG@10 56.4 (base) with a code-aware OLMo-derived 50,368-token tokenizer. | GPU only: 148.1k tok/s @512 on RTX 4090 (ModernBERT paper) | CG / RO |
| **Alibaba-NLP/gte-reranker-modernbert-base** (cross-encoder) | 149M / — / 8192 | Apache-2.0 | CoIR **79.99** (card, same self-reported harness → same caveat), BEIR 56.73, LoCo 90.68 | none published | **RO** |
| **ibm-granite/granite-embedding-small-english-r2** | 47M / 384 / 8192 (ModernBERT-style, 12 layers) | Apache-2.0 | CoIR **53.8** (card) / 53.4 (paper Table 11); english-r2 149M: 54.8. Below CodeRankEmbed. | H100 only: 199 docs/s at 512-token chunks | CG |
| **codesage/codesage-small-v2** | 130M / 1024 (MRL) / 1024 (paper: "maximum sequence length to 1024") | Apache-2.0 | v1 CoIR 54.4, CSN MRR 64.9 (CodeRankEmbed card table); v2: NL2Code avg 64.41, Code2Code MRR avg 38.13 (card). **v2 CoIR: not published.** | none | CG |
| **jinaai/jina-embeddings-v2-base-code** | 161M / 768 / 8192 | Apache-2.0 | CoIR **58.4** (CoRNStack Table 4) — but jina.ai's own page states **52.24**; discrepancy unresolved. CSN MRR 67.2 (CodeRankEmbed card table). | none | CG |
| **Salesforce/codet5p-110m-embedding** | 110M / **256** / n.s. | BSD-3-Clause | CSN MRR 74.2 avg (card: Ruby 74.51, JS 69.07, Go 90.69, Py 71.55, Java 71.82, PHP 67.72); CoIR **45.9**. CLARC C/C++ MRR 58.84 vs Nomic-7B 86.23 (Group 1). | none | CG |
| **microsoft/unixcoder-base** | ~125M (n.s. on card) / 768 / 512 | Apache-2.0 | CoIR avg **37.33** (CoIR paper Table); CSN-CCR 58.36 (code-to-code) | none | CG |

### Tier B — fits license and CPU, but general-text only (no code training) — cheap rerank/sparse probes

| model | params | license | evidence | CPU latency | CG/RO |
|---|---|---|---|---|---|
| **mixedbread-ai/mxbai-rerank-xsmall-v1** | 70.8M | Apache-2.0 | BEIR NDCG@10 **43.9** (base-v1 46.9, large-v1 48.8; bge-reranker-base 41.6) — no code numbers | none published | RO |
| **cross-encoder/ms-marco-MiniLM-L6-v2** | 22.7M | Apache-2.0 | TREC-DL19 74.30, MS MARCO MRR@10 39.01; **1800 docs/s on a V100** (sbert table; no CPU figure) | GPU only | RO |
| **jinaai/jina-reranker-v1-turbo-en** | 37.8M | Apache-2.0 | BEIR 49.60 (base-en 52.45; mxbai-base-v1 49.19) | none | RO |
| **answerdotai/answerai-colbert-small-v1** | 33M, 96-d tokens | Apache-2.0 | BEIR **53.79** vs ColBERTv2.0 50.02, bge-base 53.25 — general text | none | CG / RO |
| **colbert-ir/colbertv2.0** | ~110M (0.1B), 128-d | MIT | MS MARCO MRR@10 39.7; index 16/25 GiB (1-/2-bit) vs 154 GiB uncompressed; 20 or 36 bytes/vector; search "50–250 ms" per query (ColBERTv2 paper) | see PLAID row | CG / RO |
| **PLAID engine** (ColBERTv2 index) | — | MIT (code) | MS MARCO v1 (8.8M passages): **352.3 ms 1-CPU / 101.3 ms 8-CPU / 38.4 ms GPU** at k=1000, MRR@10 39.8; v2 (138.4M): 251.3 ms 8-CPU; index 21.6 GiB (v1), 202.2 GiB (v2); Xeon Gold 6132 | published (above) | CG |
| **lightonai/next-plaid** (Rust CPU multi-vector DB) | — | Apache-2.0 | BEIR with GTE-ModernColBERT-v1 (~300 tokens/doc): CPU QPS 6.6–20.9, p95 219–455 ms; indexing 105.5 docs/s on Quora (522,931 docs); 2- or 4-bit PQ | published (above) | CG |
| **opensearch-project/opensearch-neural-sparse-encoding-doc-v3-distill** | 67M (DistilBERT) | Apache-2.0 | BEIR **0.517** (v2-distill 0.528, v3-gte 133M **0.546**; BM25 not in that table); **inference-free**: "For queries, it just use a tokenizer and a weight look-up table"; doc FLOPS 1.8 (v3-gte 1.7). OpenSearch paper (arXiv 2411.04403): +3.3 NDCG@10 over prior inference-free SOTA, client latency **1.1× BM25**. No code evaluation. | query side ≈ BM25 | **CG** |
| **BAAI/bge-m3** (sparse head) | 568M / 1024 / 8192 | MIT | MIRACL: BM25 31.9, M3 sparse **53.9**, dense 69.2, all 71.5; MLDR: sparse 62.2 > dense 52.5 (M3 paper). CoIR (dense) **39.31** (CoIR paper). 568M is 4× CodeRankEmbed per token. | none | CG |
| **sentence-transformers/static-retrieval-mrl-en-v1** | 0 active params, 1024-d (MRL 32–1024), BERT-uncased vocab | Apache-2.0 | NanoBEIR NDCG@10 **0.5032** (256-d 0.4819, −1.47%); BM25 0.4518; potion-base-8M 0.4421; all-MiniLM-L6-v2 0.5623; bge-base 0.6376. **107,419 sentences/s on an i7-13700K** vs 270.40 for all-mpnet-base-v2 (397×) — HF static-embeddings blog | published (above) | CG |

### Tier C — excluded by a hard constraint (recorded so the door stays closed)

| model | why excluded | numbers for calibration |
|---|---|---|
| **minishlab/potion-code-16M(-v2)** — distilled FROM CodeRankEmbed | Not excluded by license (MIT) — excluded on **quality**: CoIR avg **39.08** (v2; v1 37.05) is **below BM25's 42.31** in the same harness (CodeRankEmbed 59.14); +BM25 hybrid 43.36. Per-subset v2: CSN 46.37 vs BM25 40.86 vs teacher 94.70; CosQA 24.36; StackOverflow 59.57 vs BM25 70.26. Semble's own repo benchmark: potion-code raw 0.650 < BM25 raw 0.675 NDCG@10 (SembleSharp ablation). | 16M params, 256-d, ~63.5k vocab (43k code tokens mined from CoRNStack added to the CodeRankEmbed tokenizer); recipe = Model2Vec distill (PCA 256) → Tokenlearn on 1.2M pairs → MNRL contrastive on 1.2M pairs → SIF re-regularization. This is the strongest published static-from-code-encoder result and it lands at BM25 level. |
| Qodo-Embed-1 (1.5B/7B) | **QodoAI-Open-RAIL-M**: royalty-free incl. commercial, but use-based restrictions must propagate to every derivative and downstream license, plus naming obligations — **not MIT-compatible**; also 1.5B/7B is 11–51× CodeRankEmbed per token | 1536-d / 3584-d, 32k ctx |
| Salesforce SFR-Embedding-Code-400M/2B | **CC-BY-NC-4.0** | CoIR 61.9 (400M), 67.4 (2B) — card table |
| jinaai/jina-code-embeddings-0.5b/1.5b | **CC-BY-NC-4.0**; Qwen2.5-Coder-based decoders | 1.5B: 79.04 overall / 78.94 MTEB-Code (paper) |
| jinaai/jina-reranker-v2-base-multilingual, jina-colbert-v2 | **CC-BY-NC-4.0** | reranker: CSN MRR@10 71.36, BEIR 53.17; colbert-v2 BEIR 0.531 |
| naver SPLADE (v2/++/v3), **SPLADE-Code** (arXiv 2603.22008) | naver SPLADE weights are **CC BY-NC-SA 4.0** (splade-v3, cocondenser-ensembledistil cards); SPLADE-Code is **600M–8B params** (MTEB-Code 75.4 under 1B, 79.0 at 8B; "sub-millisecond retrieval on a 1M-passage collection"); weights/license **not verified** | the one code-domain LSR result; it says "learned expansion tokens are critical to bridge lexical and semantic matching" and names "subword fragmentation" as the core difficulty |
| Elastic ELSER | subscription-gated ("must have the appropriate subscription level"); redistribution not addressed → excluded | ELSER v2 vs BM25: 10 wins / 1 draw / 1 loss, avg +18% NDCG@10 (Elastic docs) |
| Qwen3-Embedding-0.6B / Qwen3-Reranker-0.6B | Apache-2.0 and code-strong (MTEB-Code **75.41** / reranker **73.42** vs bge-reranker-v2-m3 41.38, jina-v2 58.98 — Qwen3 report Tables 9/4) — but 0.6B decoder = **4.4× CodeRankEmbed FLOPs per token** plus an instruction template; violates the no-added-latency constraint on CPU | 1024-d, 32k ctx |
| nomic-ai/nomic-embed-code (7B), voyage-code-2/3, OpenAI, Gemini | 7B (51× per token) / API-only | nomic-embed-code CSN: Py 81.7, Java 80.5, Go 93.8 vs CodeRankEmbed 78.4/76.9/92.7 |
| BAAI/bge-code-v1 (2B), CodeSage-large (1.3B), mxbai-rerank-base-v2 (0.5B, latency 0.67 s **on an A100**) | size | bge-code-v1 CoIR 81.77; mxbai-v2 "Code Search" column 31.73 (column definition unknown) |
| bigcode/starencoder | BigCode OpenRAIL-M (use restrictions) | 125M, 1024 ctx, no retrieval numbers |
| Snowflake arctic-embed-m-v2.0 | Apache-2.0 but general-text | CoIR 52.2 (Granite Table 11) |

## 2. (a) Distilled static models — what distillation is, and what it would buy us

**Mechanism (model2vec docs + author blog).** Distillation "forward pass[es] a vocabulary
through a sentence transformer model, creating static embeddings for the individual
tokens", then PCA (`pca_dims=256` default) and Zipf/SIF re-weighting (`sif_coefficient`
default 1e-4; frequency rank from the tokenizer's vocabulary order). Inference is "the mean
of the token embeddings". It needs the **teacher model AND its tokenizer** (a forward pass
per vocabulary entry) — not the tokenizer alone. Custom vocabularies are supported
(`vocabulary: list[str]`; modes "Output" (teacher wordpieces), "Vocab (word)", "Vocab
(subword)"). Cost: "a few minutes" on CPU (docs); ablations attribute ≈2.8–3.1% each to
PCA and to Zipf weighting. `model2vec-rs` (MIT) runs f32/f16/i8 safetensors at
**8,000 samples/s single-threaded** (1.7× the Python path).

**Quality ceiling (primary numbers).** Raw distillation: M2V_base_output MTEB 48.77 vs
potion-base-8M 51.32 (Tokenlearn-trained) vs all-MiniLM 55.80. Retrieval is the weak task:
potion-retrieval-32M MTEB-retrieval 35.06 = 81.69% of all-MiniLM's 42.92; potion-base-8M
NanoBEIR 0.4421 < BM25 0.4518; the best static (GPU-contrastive static-retrieval-mrl)
reaches 0.5032. On **code**, the definitive datum is potion-code-16M-v2: distilled from
CodeRankEmbed with 43k mined code tokens, then 2.4M training pairs — **CoIR 39.08 vs
BM25 42.31 vs teacher 59.14**.

**Versus our per-corpus learned tier.** Our tier 1 is already a static model (PPMI →
eigen-factorization → uSIF → retrofit, ~250-d, 60–80 s warm on the kernel) and it
already beats lexical on all three corpora (kernel 0.313 vs 0.299; cpython 0.412 vs 0.137;
vorpal 0.612 vs 0.571). A per-corpus model2vec distillation from CodeRankEmbed would
replace corpus co-occurrence with the teacher's token geometry. The one mechanism it adds
that PPMI cannot learn is **cross-vocabulary synonymy inherited from the teacher**
("duplicate" ≈ "similar", "runtime" ≈ "trace") even when the words never co-occur in the
corpus — exactly the paraphrase gap. Expected magnitude: at or below BM25 on CoIR-style
tasks (above), so this is a **cheap probe, not a replacement**. Warm cost: ~60k vocabulary
entries × ~0.7 GFLOP (2–3 tokens each) ≈ 4 × 10¹³ FLOPs → ~40 s at 1 TFLOPS, ~8 min at
today's ~0.08 TFLOPS effective (§6). **[CG]** because it embeds every definition.
WordLlama (MIT; LLM token-codebook, 64–1024-d, 16 MB): MTEB STS 67.91 / clustering 33.25
vs all-MiniLM 78.90 / 42.35 — no retrieval or code numbers; strictly dominated here.

## 3. (b) Small code encoders that beat CodeRankEmbed

On CoIR/MTEB-Code, with MIT/Apache licenses and ≤ ~150M params, the only models with
published numbers ABOVE CodeRankEmbed's 60.1 are the **ModernBERT/Ettin family**:
LateOn-Code-edge 17M (66.64 MTEB-Code), LateOn-Code 149M (74.12), gte-modernbert-base
149M (71.5–79.31 depending on harness — disputed). Every dense single-vector model at
≤150M other than those is **below** CodeRankEmbed: granite-r2 54.8/53.8, jina-v2-code
58.4 (or 52.24), CodeSage-small 54.4, CodeT5+ 45.9, UniXcoder 37.33, arctic-m-v2 52.2.
Caveats that matter for us: (i) LateOn-Code's 2.1M training samples are drawn from
CoIR+CSN, so its CoIR-family scores are in-domain — CodeRankEmbed's CoRNStack training is
also code but the eval overlap differs; (ii) none of these publishes a CPU latency;
(iii) all were trained on function bodies/docstrings, not 10-token name+signature surfaces
— transfer to our surface is **unknown** and is the first thing the A/B must measure;
(iv) none is trained on C-kernel-style `#define` corpora. CodeSage-v2 is Apache-2.0 and
MRL-capable (1024-d) but its CoIR is unpublished. Voyage-code is API-only → excluded.

## 4. (c) Cross-encoder / listwise rerankers sized for CPU — all [RERANK-ONLY]

Published CPU per-pair latency: **none** for any listed model (sbert's 1800 docs/s for
MiniLM-L6 is a V100; mxbai-v2's 0.67 s is an A100; jina-turbo, mxbai-v1, bge-reranker-v2-m3
publish no latency). Quality-per-FLOP by arithmetic (2 × non-embedding params × (query +
doc tokens), ~25 tokens per pair on our surface): ms-marco-MiniLM-L6 (22.7M) ≈ 1.1 GFLOP
/pair; mxbai-rerank-xsmall-v1 (70.8M) ≈ 3.5; gte-reranker-modernbert-base (149M) ≈ 7.5;
bge-reranker-v2-m3 (568M) ≈ 28; Qwen3-Reranker-0.6B ≈ 30 + a template. For k=25 that is
28 / 88 / 188 / 700 / 750+ GFLOP per query — the 149M ModernBERT reranker is **≈7× the
FLOPs of today's bi-encoder rerank** (25 candidate embeds ≈ 68 GFLOP, cacheable; a
cross-encoder is never cacheable because the pair changes). Only gte-reranker-modernbert-
base and Qwen3-Reranker have code-domain numbers (79.99 self-reported CoIR; 73.42
MTEB-Code); the MS-MARCO/BEIR rerankers have none. Verdict: a cross-encoder cannot touch
paraphrase (§0 fact 1), costs more per query than the bi-encoder it would replace, and has
no published evidence on exact-identifier preservation — **defer**; if probed at all, probe
gte-reranker-modernbert-base on cpython's descriptive class only, behind the fused-winner
pin.

## 5. (d) Late interaction — the only rerank shape with a mechanism for subword evidence

**Cost model (ColBERTv2/PLAID papers).** Per-token vectors at 20 or 36 bytes (centroid id +
1-/2-bit residuals); MS MARCO 8.8M passages → 21.6 GiB (PLAID), search 101 ms on 8 CPU
threads at k=1000 (Xeon Gold 6132), 352 ms single-thread. Our definitions are ~12 tokens,
not ~70–300, so per-document vectors are 6–25× fewer: **kernel 8.9M × 12 tokens × 20–36 B
≈ 2.1–3.8 GB** multi-vector index (at 48-d LateOn-Code-edge tokens with 2-bit PQ,
proportionally smaller); cpython 163K → ~40–70 MB; vorpal 79K → ~20–35 MB.
**Doc-side encoding cost is the same forward pass as a dense model** — the FLOP budget of
§6 applies unchanged; the multi-vector index does not add compute, only bytes.

**Why it is the right shape for identifier-heavy code.** MaxSim keeps one vector per
subword token, so a query token `skb` can match the `skb` inside `alloc_skb` directly
instead of being averaged into a pooled vector with `alloc`; CLARC (ICLR 2026) shows code
retrievers "are heavily dependent on the lexical information from identifiers" (MRR drops
under identifier anonymization for every model; BM25 8.20 vs dense 86–92 on C/C++ NL
queries) — i.e. the identifier tokens ARE the signal dense models use, and late interaction
is the architecture that does not pool them away. LightOn's stated rationale: code is "a
domain where lexical search is still dominant" and late interaction adds "soft matching"
when "query and the relevant document don't share the exact same terms". The **code-domain
evidence** is LateOn-Code(-edge) alone (§1); "ColBERT-style models have not been publicly
studied for code retrieval before" (LightOn blog). Direct evidence that MaxSim preserves
*exact-identifier ranking* on a kernel-like corpus: **none published** — it is the
hypothesis the A/B tests. Semble's benchmark places ColGREP (LateOn-Code-edge, no lexical
fusion) at 0.693 NDCG@10 vs BM25+ranking 0.834 and their hybrid 0.854 on a 63-repo,
19-language set (LLM-labelled) — a late-interaction channel is **additive to lexical, not
a replacement**, consistent with SEMANTIC_TIER's fusion invariant.

## 6. (e)+(f) Learned sparse, and the doc-side feasibility arithmetic

**Learned sparse.** Doc-side cost is a full encoder forward pass per document (SPLADE
uses the MLM head over the encoder output); query-side cost is likewise a forward pass
EXCEPT for the inference-free family, where "queries just use a tokenizer and a weight
look-up table" and client latency is 1.1× BM25 (OpenSearch paper) — **zero added query
latency, and a candidate-generating channel** via learned document expansion (the
`similar_pairs` document would carry expansion mass on "duplicate"/"detect" if the model
knew code). Quality vs BM25 on *text*: opensearch v3-gte BEIR 0.546 vs BM25 ≈0.45 (HF
NanoBEIR BM25 0.4518 is the nearest primary); M3-sparse MIRACL 53.9 vs BM25 31.9. Quality
vs BM25 on *code*: **no MIT/Apache code-trained LSR exists**; SPLADE-Code (600M–8B,
license unverified) is the only result and it is 4–58× CodeRankEmbed per token.
Corpus-specific vocabularies help LSR "up to 12%" quality and "up to 50%" latency
(arXiv 2401.06703) — which points at our subword-hashed postings (Stage 4 infrastructure,
BM25 channel measured OFF on the kernel because `sock ≠ socket` — SEMANTIC_TIER Stage 4).
A learned expansion channel over that postings tier is the sparse option; its doc-side
encoder must be small and code-aware — none is available today. **Verdict: rank 3, probe
the inference-free 67M English model on cpython/vorpal descriptive only; expect nothing on
the kernel.**

**Doc-side dense feasibility — the arithmetic (assumptions stated).**

- Per-definition FLOPs at a 12-token surface: CodeRankEmbed ≈ **2.7 GFLOP**;
  LateOn-Code-edge (17M) ≈ **0.4 GFLOP**; opensearch v3-distill (67M) ≈ 1.6 GFLOP;
  a static model ≈ 0 (table lookups; 8,000 defs/s single-thread per model2vec-rs).
- Effective throughput E (TFLOPS): **today's owned kernel E ≈ 0.08** — 26 sequences
  (query + 25 candidates) ≈ 70 GFLOP in 0.887 s ≈ 79 GFLOPS (BENCHMARKS Stage 6; f32
  GEMM lanes, f64 reductions, correctness-first; the 0.887 s also includes tokenization
  and the reorder, so 0.08 is a floor on the kernel's own rate). **Published Apple ceiling: fp32 sgemm on an M1
  (4P+4E) reaches 614–809 GFLOPS (cblas_sgemm / BNNSMatMul) and 1,125–1,250 GFLOPS with a
  direct-AMX kernel at 8 threads** (arXiv 2606.25426, Table 3). **M5 Max: no published GEMM
  figure located** — assume E = 1.0 (M1 8-thread band; conservative) and E = 3.0
  (unverified M5-Max scaling) as bracket. fp16/bf16 rates on Apple AMX: not published in
  that paper.
- Definitions per second = E × 10¹² / FLOPs-per-def:

| encoder | E=0.08 (today) | E=1.0 (M1-published) | E=3.0 (assumed) |
|---|---|---|---|
| CodeRankEmbed (2.7 GFLOP) | 30 defs/s | 370 defs/s | 1,100 defs/s |
| LateOn-Code-edge (0.4 GFLOP) | 200 defs/s | 2,500 defs/s | 7,500 defs/s |

- Warm budgets (60 s / 300 s) → definitions pre-embeddable:
  CodeRankEmbed: today 1.8K / 9K; E=1: **22K / 111K**; E=3: 66K / 333K.
  LateOn-Code-edge: today 12K / 60K; E=1: **150K / 750K**; E=3: 450K / 2.2M.
  → **vorpal (79K) and CPython (163K) are FULLY pre-embeddable within 300 s** by either
  encoder once E ≥ 1; the kernel (8.9M) is not (CodeRankEmbed 6.7 h at E=1, 2.2 h at E=3;
  LateOn-edge ~1 h / 20 min). The kernel gets a **hot subset**: top-N by graph in-degree
  (the "known lever"). At E=1, a 300 s budget buys **111K kernel definitions with
  CodeRankEmbed or 750K with LateOn-Code-edge** (8.4% of the kernel; note 5.9M of 8.9M
  are `#define`s, so 750K can cover most of the ~3M non-macro definitions).
- The first-order lever is therefore **E, not the model**: today's kernel is 8–10× below
  the *published M1* Accelerate band (614–809 GFLOPS) and 14–16× below the direct-AMX
  band (1,125–1,250 GFLOPS). Doc-side rows are a stamp-gated warm sidecar (SEMANTIC_TIER
  §1), so bitwise thread-stability there is a determinism-of-the-stamp question, not a
  generation-id question — an Accelerate/AMX GEMM path (or the recorded f16-native kernel)
  is admissible for the doc side even if the query-side rerank keeps the fixed-order lanes.
- Storage (kernel full / hot 750K / cpython 163K): 768-d f16 13.7 GB / 1.15 GB / 250 MB;
  int8 6.8 GB / 576 MB / 125 MB; binary 855 MB / 72 MB / 16 MB. Quantization retention
  (HF embedding-quantization blog, MTEB retrieval): int8 97% (mxbai-large), 94.68%
  (e5-base), 90.79% (all-MiniLM); **int8 + rescore ×4 → 99%**; binary 92.53%, **96.45%
  with rescoring** at 1024-d — SEMANTIC_TIER D3 already rules binary unsafe below ~768-d.
  MRL truncation: 1024→256 costs 1.47% NanoBEIR on a Matryoshka-trained static model;
  nomic-embed-text-v1.5 MTEB 62.28 → 61.04 at 256-d (−2%). **CodeRankEmbed is not stated
  to be MRL-trained — 768→256 retention is unknown and must be measured**; CodeSage-v2 is
  MRL-trained (1024-d) if a truncated store is wanted.
- CPU int8 transformer throughput in primary sources: Microsoft/Intel report **"up to 2.9×"**
  for a 12-layer BERT with VNNI int8 under ONNX Runtime (11th-gen Core) and 3.38× for
  DistilBERT; Sapphire Rapids bf16/AMX "60–65% faster" than the prior Xeon generation;
  CoIR's Table 4 lists ≈7.4–7.8 ms per sample for 110M-class encoders (hardware
  unspecified). **No absolute sequences/s at seq-128 on CPU was found in a primary source
  reached by this sweep**; do not plan on a number that was not published.

## 7. (g) Behaviour at millions of documents and on identifier-heavy C

- **Scale**: CORE-Bench (2026) is the one million-scale datum: Level-2 issue-to-edit
  localization over **9,377,120 repository chunks** — Qwen3-Embedding-8B falls from
  71.7/96.9 (Level-1, 2.4M corpus) to **20.3/48.0**; in-domain SFT recovers only to
  32.8/66.4. CoIR's largest corpora are 1M (CSN, CSN-CCR). PLAID/ColBERTv2 hold MRR@10 at
  138M passages (18.0 MRR@100) but latency is 250 ms on 8 CPU threads.
- **Identifier vs NL queries**: CodeSearchNet's human-judged set (99 queries, "clearly
  technical keywords" filtered OUT) still had NBoW (0.574 NDCG-within) beat self-attention
  (0.493) — "keyword matching … a crucial facility"; ElasticSearch 0.337. "Beyond Retrieval"
  (2026): on short keyword queries (~19 tokens) **every** model collapses — max nDCG@10
  **0.015** (Qwen3-4B), 0.000–0.008 for the rest, code-specialized models included;
  code-specialized ≈2× general on code-to-code. CLARC (C/C++, ICLR 2026): BM25 MRR 8.20 vs
  dense 86.23–86.93 on *LLM-written NL* queries; identifier anonymization drops every model;
  "persistent reliance on lexical features". CoIR CSN-CCR (code→code, 1M corpus):
  CodeRankEmbed 86.9, UniXcoder 58.36, BM25 34.69.
- **Repo-level**: SWE-bench chose BM25 because dense retrieval is "ill-suited … due to very
  long key and query lengths, and … retrieving code documents with natural language
  queries"; BM25 recall vs oracle 29.58/44.41/51.06% at 13k/27k/50k tokens. SweRank
  (issue → function): BM25 Acc@5 31.75 vs CodeRankEmbed 51.82 vs a CodeRankEmbed
  fine-tuned on issue data (SweRankEmbed-Small, same 137M) **63.14** — the largest lever at
  fixed size was **training data for the query distribution, not architecture**.
  CodeRAG-Bench RepoEval: BM25 93.2 vs BGE-base 77.5 vs Voyage-code 94.3; SWE-bench-Lite
  BM25 43.0 vs GIST-large 47.8; "retrievers still struggle … with limited lexical overlap".
- Reading for vorpal: the kernel failure (subword identifiers, 5.9M macros) is the
  short-keyword regime where the literature says every neural model is ~0 and lexical is
  authoritative; CPython's descriptive gain is the NL-intent regime where dense wins; the
  paraphrase 0.0 is "limited lexical overlap" — the regime only doc-side channels reach.

## 8. Ranked recommendation — what to A/B first, inside the existing architecture

All three keep the fusion invariant (exact/lexical channels authoritative for short
symbol queries; the fused-winner pin stays) and are measured with
`cargo xtask searcheval <index-dir> xtask/labels/{kernel,cpython,vorpal}.json`
(NDCG@10 / MRR / recall@5, per query class, never averaged into one number).

### 1. Doc-side multi-vector channel with LateOn-Code-edge (17M, Apache-2.0) — [CANDIDATE-GENERATING] + token-level rerank
- **What**: at warm time, encode definition surfaces (name + signature + basename, plus
  doc-comment/body-head where present — the "richer surface" lever) to 48-d per-token
  vectors; store 2-bit-PQ'd (next-plaid/PLAID scheme) as a stamp-gated sidecar; at query
  time encode the query once (17M params ≈ 0.4 GFLOP → ≈5 ms at E=0.08 today) and (i) run
  it as a FOURTH fused list via centroid-pruned MaxSim over the sidecar, (ii) use MaxSim
  instead of pooled cosine for the rerank of the fused tail.
- **Expected effect**: kernel — neutral-to-positive on subword identifiers (MaxSim keeps
  the `skb`/`irq` tokens; hypothesis, unmeasured); CPython descriptive — up (66.64 vs
  60.47 MTEB-Code, in-domain caveat); paraphrase — the only option here with a mechanism
  AND full-corpus coverage on cpython/vorpal.
- **Latency math**: query encode ~5 ms; MaxSim rerank over 25 × 12 tokens × ~20 query
  tokens × 48-d ≈ 0.3 MFLOP (negligible); channel search: PLAID 8-thread at 8.8M docs of
  ~70 tokens = 101 ms — our docs are ~12 tokens, so **budget ≤ 100 ms on the kernel hot
  subset, ≤ 20 ms on cpython** (assumption: latency scales with token count). Warm:
  cpython 163K × 0.4 GFLOP = 65 TFLOP → **65 s at E=1, ~14 min at today's E=0.08**;
  kernel hot 750K → 300 s at E=1. Index bytes: cpython ~40–70 MB, kernel-hot ~180–320 MB.
- **License**: Apache-2.0 model + Apache-2.0 next-plaid reference (owned inference must be
  written: Ettin/ModernBERT architecture — rotary, GeGLU, alternating local/global
  attention — is NOT NomicBERT; a second owned kernel).
- **Biggest risk**: transfer from function-body training to 10-token surfaces is
  unmeasured (the semble ablation shows raw LateOn-Code at 0.693 < BM25+ranking 0.834 on
  chunk-level code); CoIR-in-domain scores overstate; a new architecture to own. Gate: on
  vorpal first (79K, minutes at any E), kill if descriptive/paraphrase do not move.

### 2. Doc-side dense precompute with the existing CodeRankEmbed — full corpus ≤ ~200K defs, in-degree hot subset on the kernel — [CANDIDATE-GENERATING]
- **What**: no new model, no new license. Precondition: lift E on the doc-side path
  (Accelerate/AMX sgemm or the recorded f16-native kernel) — the arithmetic in §6 says the
  doc side is **kernel-bound by 8–16× before it is FLOP-bound**. Then embed every
  definition (cpython/vorpal) or the top-N in-degree definitions (kernel), store int8 with
  ×4 oversample + f32 rescore (99% retention datum), and add the dense list as a fused
  channel; the query embedding is computed once and shared with the rerank → **zero added
  query latency** beyond one ANN probe (≤ the existing lexical-ANN cost, ~ms).
- **Expected effect**: CPython — descriptive up, recall up (the encoder already lifts
  recall@5 to 0.500 as a reranker; as a channel it can surface unseen candidates);
  vorpal — paraphrase gains where the target's surface carries the concept ("similar_pairs"
  vs "near duplicate" is exactly a CodeRankEmbed-space match to test); kernel — must be
  fused behind the pin, else the 0.222 regression reappears (pooled vectors cannot preserve
  subword evidence; the pin, not the model, is what made the kernel GREEN on 08-31).
- **Latency/warm math**: cpython 163K × 2.7 GFLOP = 4.4 × 10¹⁴ → **7.4 min at E=1,
  2.5 min at E=3, ~92 min today**; vorpal 79K → 3.6 min at E=1; kernel top-111K → 5 min at
  E=1. Storage int8: cpython 125 MB; kernel-hot 111K → 85 MB. MRL 768→256 retention:
  **unknown for CodeRankEmbed — measure before truncating**.
- **License**: MIT, already vendored and checksummed.
- **Biggest risk**: if E cannot be lifted (owned-kernel determinism constraints extend to
  the sidecar), a 300 s warm budget only buys ~9K definitions — enough for vorpal's hot
  core, not for cpython. Second risk: hot-subset selection by in-degree misses low-degree
  paraphrase targets (`similar_pairs` may be low-degree).

### 3. Inference-free learned-sparse expansion channel (opensearch doc-v3-distill, 67M, Apache-2.0) over the existing postings tier — [CANDIDATE-GENERATING]
- **What**: warm-time doc-side expansion vectors (30,522-d BERT vocab; FLOPS-regularized
  so postings stay short) merged into a second postings tier; query side = tokenizer + IDF
  lookup (published 1.1× BM25 client latency). Exact-scoring, deterministic, **no encoder
  at query time**.
- **Expected effect**: cpython/vorpal descriptive and paraphrase — the expansion terms are
  the mechanism ("learned expansion tokens are critical to bridge lexical and semantic
  matching", SPLADE-Code); kernel — expect nothing or harm (English WordPiece cannot see
  `tcp_cong_avoid_ai`; the BM25 channel is already measured OFF there for the same reason),
  so ship behind the same per-corpus gate as BM25.
- **Warm math**: cpython 163K × 1.6 GFLOP = 2.6 × 10¹⁴ → 4.4 min at E=1; postings growth
  bounded by the model's FLOPS term (1.8 avg). Query: +≈0 ms.
- **License**: Apache-2.0. **Biggest risk**: no code-domain evidence at all for this model
  family; it may simply add English noise. It is ranked because it is the cheapest
  zero-query-latency candidate-generating channel and reuses Stage-4 infrastructure.

### Deferred (recorded, not recommended for the first A/B)
- **Per-corpus model2vec distillation from CodeRankEmbed** [CG]: cheap (~40 s at E=1),
  but the published ceiling is BM25-level (potion-code-16M-v2 39.08 vs BM25 42.31) and our
  learned tier already beats lexical; probe only as a paraphrase-synonymy experiment
  (does "duplicate"≈"similar" in teacher-token space surface `similar_pairs`?).
- **Cross-encoder rerank** (gte-reranker-modernbert-base) [RO]: ≈7× the FLOPs of the
  current rerank per query, cannot touch paraphrase, no identifier-preservation evidence,
  self-reported CoIR disputed. Probe on cpython descriptive only if options 1–2 fail there.
- **Swapping the bi-encoder reranker for a bigger dense model** (Qwen3-0.6B, jina-code)
  [RO]: violates the latency constraint (4.4×+ per token) or the license.

## 9. What could NOT be verified in this sweep
- Any absolute CPU throughput (sequences/s) for a BERT-base-class encoder at seq ~128 in a
  primary source — only speedup ratios (2.9× int8, 60–65% bf16) were published.
- Apple M5 Max GEMM throughput (the only published Apple-AMX numbers are M1: 614–1,250
  GFLOPS fp32); fp16/bf16 AMX rates.
- CodeRankEmbed's embedding dim (768) — inferred from its stated base
  (Snowflake/arctic-embed-m-long, 768); MRL behaviour of CodeRankEmbed — not stated.
- gte-modernbert-base CoIR (79.31 self-reported vs 71.5 IBM vs 71.66 LightOn; mteb #1861
  open); jina-v2-code CoIR (58.4 CoRNStack vs 52.24 jina.ai); CodeSage-v2 CoIR
  (unpublished); mxbai-rerank-v2's "Code Search" column definition.
- SPLADE-Code weights/license; ELSER redistribution terms (subscription-gated); CPU latency
  for every reranker and for LateOn-Code (none published; next-plaid's 219–455 ms p95 is
  for ~300-token docs on unspecified CPU hardware).
- Whether MaxSim preserves exact-identifier ranking on a kernel-like corpus — no code-domain
  study exists; it is the hypothesis option 1 tests.
- ~~The 0.222 vs 0.313 kernel "+encoder" discrepancy between the 2026-09-02 numbers and
  BENCHMARKS' 2026-08-31 pinned table.~~ RECONCILED (coordinator, same day): same
  harness (`xtask searcheval`, k = 25), same label file (`xtask/labels/kernel.json`),
  same winner-pinned rerank — the GRAPH changed. The 08-31 table was graded on the
  2.85 M-node kernel extraction; the extraction-coverage campaign (macro / union /
  typedef kinds) took it to 8.89 M nodes, 5.9 M of them `#define`s, and every tier was
  re-baselined on that graph on 09-02 (BENCHMARKS "Ranking tiers re-baselined"). On the
  new graph the rerank's rank-1..k reorder loses subword-identifier answers to macro and
  lookalike candidates that did not exist in the old pool. The per-stage deltas are
  history of each mechanism on the graph it was measured on; the 09-02 rows are current.

## 10. Sources (primary; all reached 2026-09-02)
CodeRankEmbed cards: huggingface.co/nomic-ai/CodeRankEmbed, huggingface.co/cornstack/CodeRankEmbed ·
CoRNStack (ICLR 2025): arxiv.org/abs/2412.01007 (html v2) · CoIR (ACL 2025): arxiv.org/abs/2407.02883 (html v3) ·
CodeSage (ICLR 2024): arxiv.org/abs/2402.01935; huggingface.co/codesage/codesage-small-v2 ·
SFR-Embedding-Code: huggingface.co/Salesforce/SFR-Embedding-Code-400M_R · nomic-embed-code: huggingface.co/nomic-ai/nomic-embed-code; nomic.ai/news/introducing-state-of-the-art-nomic-embed-code ·
Qodo: huggingface.co/Qodo/Qodo-Embed-1-1.5B, huggingface.co/Qodo/Qodo-Embed-1-7B, qodo.ai/open-rail-m-license ·
jina: huggingface.co/jinaai/jina-embeddings-v2-base-code, jina.ai/models/jina-embeddings-v2-base-code, huggingface.co/jinaai/jina-code-embeddings-0.5b, arxiv.org/abs/2508.21290, huggingface.co/jinaai/jina-reranker-v1-turbo-en, huggingface.co/jinaai/jina-reranker-v2-base-multilingual, huggingface.co/jinaai/jina-colbert-v2 ·
Snowflake: huggingface.co/Snowflake/snowflake-arctic-embed-m-long · BAAI: huggingface.co/BAAI/bge-code-v1, huggingface.co/BAAI/bge-m3, arxiv.org/abs/2402.03216, huggingface.co/BAAI/bge-reranker-v2-m3 ·
Qwen3: huggingface.co/Qwen/Qwen3-Embedding-0.6B, huggingface.co/Qwen/Qwen3-Reranker-0.6B, arxiv.org/abs/2506.05176 ·
CodeT5+: huggingface.co/Salesforce/codet5p-110m-embedding · UniXcoder: huggingface.co/microsoft/unixcoder-base · StarEncoder: huggingface.co/bigcode/starencoder ·
Granite R2: arxiv.org/abs/2508.21085, huggingface.co/ibm-granite/granite-embedding-small-english-r2 · gte-modernbert: huggingface.co/Alibaba-NLP/gte-modernbert-base, huggingface.co/Alibaba-NLP/gte-reranker-modernbert-base, github.com/embeddings-benchmark/mteb/issues/1861 · ModernBERT: arxiv.org/abs/2412.13663 ·
LateOn-Code: huggingface.co/lightonai/LateOn-Code, huggingface.co/lightonai/LateOn-Code-edge, huggingface.co/blog/lightonai/colgrep-lateon-code, github.com/lightonai/next-plaid ·
model2vec / potion: github.com/MinishLab/model2vec, minish.ai/packages/model2vec/distillation, huggingface.co/blog/Pringled/model2vec, github.com/MinishLab/model2vec/blob/main/results/README.md, huggingface.co/minishlab/potion-base-8M, huggingface.co/minishlab/potion-retrieval-32M, huggingface.co/minishlab/potion-code-16M, huggingface.co/minishlab/potion-code-16M-v2, github.com/MinishLab/model2vec-rs, github.com/MinishLab/semble (README + benchmarks/README.md), github.com/MechRosey/SembleSharp/tree/main/benchmarks · WordLlama: github.com/dleemiller/WordLlama ·
Static embeddings: huggingface.co/blog/static-embeddings, huggingface.co/sentence-transformers/static-retrieval-mrl-en-v1 · quantization/MRL: huggingface.co/blog/embedding-quantization, huggingface.co/blog/matryoshka, huggingface.co/nomic-ai/nomic-embed-text-v1.5 ·
Rerankers: huggingface.co/mixedbread-ai/mxbai-rerank-xsmall-v1, huggingface.co/mixedbread-ai/mxbai-rerank-base-v2, sbert.net/docs/cross_encoder/pretrained_models.html, huggingface.co/cross-encoder/ms-marco-MiniLM-L6-v2 ·
Late interaction: huggingface.co/answerdotai/answerai-colbert-small-v1, huggingface.co/colbert-ir/colbertv2.0, arxiv.org/abs/2112.01488 (ColBERTv2), arxiv.org/abs/2205.09707 (PLAID) ·
Sparse: huggingface.co/naver/splade-v3, huggingface.co/naver/splade-cocondenser-ensembledistil, huggingface.co/opensearch-project/opensearch-neural-sparse-encoding-doc-v3-distill, huggingface.co/opensearch-project/opensearch-neural-sparse-encoding-doc-v3-gte, arxiv.org/abs/2411.04403, arxiv.org/abs/2603.22008 (SPLADE-Code), arxiv.org/abs/2401.06703, elastic.co/docs/explore-analyze/machine-learning/nlp/ml-nlp-elser ·
CPU: opensource.microsoft.com/blog/2021/03/01/optimizing-bert-model-for-intel-cpu-cores-using-onnx-runtime-default-execution-provider, huggingface.co/blog/intel-sapphire-rapids-inference, huggingface.co/blog/bert-cpu-scaling-part-1, arxiv.org/abs/2606.25426 (Apple AMX GEMM) ·
Scale/identifier evidence: arxiv.org/abs/1909.09436 (CodeSearchNet), arxiv.org/abs/2605.04615 (Beyond Retrieval), arxiv.org/abs/2603.04484 (CLARC), arxiv.org/abs/2606.11864 (CORE-Bench), arxiv.org/abs/2406.14497 (CodeRAG-Bench), arxiv.org/abs/2310.06770 (SWE-bench), arxiv.org/abs/2505.07849 (SweRank).

//! The Tier-1 trainer and query embedder: one deterministic pipeline from streamed
//! documents to a queryable static model (design doc §4 Stage 1). Every stage is the
//! cited method, wired together:
//!
//! tokenized documents → frequency table → matrix vocabulary (min-count, fastText's
//! cited floor) + subword grams (fastText joint style: gram rows share their word's
//! contexts) → symmetric windowed co-occurrence (TACL small window) → PPMI (cds 0.75,
//! no shift) → randomized SVD (seeded; symmetric Σ^0.5 weighting) with the dimension
//! chosen by PIP over the corpus's own spectrum and noise (half-split σ) → ABTT fit on
//! the composed word vectors → uSIF weighting + piecewise sentence-component removal.
//!
//! Failure IS an answer here: corpora below their own floors (empty matrix vocabulary,
//! spectrum under the noise threshold, degenerate uSIF α) return typed errors and the
//! caller states the lexical fallback in provenance — never a silently bad model.

use std::collections::HashMap;

use crate::learned::cooc::{PpmiStream, Vocab, ppmi_stream};
use crate::learned::pip::{estimate_noise_sigma_streams, select_dimension, soft_threshold};
use crate::learned::pool::{Abtt, SentenceComponents, UsifWeighting};
use crate::learned::rsvd::{FactorWorkspace, SymmetricCsr, top_symmetric_eigen};
use crate::learned::spill::{PairIter, SpillCounter, SpilledCounts, buffer_events_for};
use crate::learned::subword::SubwordTokenizer;

/// fastText's vocabulary floor (Bojanowski et al. 2017, `-minCount 5` default): tokens
/// rarer than this get no matrix row — their vectors compose from subword grams.
pub const MIN_COUNT: u64 = 5;

/// fastText's subword bucket bound (`-bucket 2000000` default). Below it the gram
/// table is EXACT (collision-free, interned strings); only past it do grams hash into
/// the bounded bucket space.
pub const GRAM_BUCKET_BOUND: usize = 2_000_000;

/// Co-occurrence window: TACL 2015's small-window regime (win = 2), where count-based
/// factorization wins — the paper's knob, not a tunable of ours.
pub const COOC_WINDOW: usize = 2;

/// The dimension clamp of design determination D2: PIP chooses d from the spectrum,
/// clamped to `[64, 256]` and recorded in provenance.
pub const DIMENSION_CLAMP: (usize, usize) = (64, 256);

/// How subword grams are addressed in the trained tables.
pub(super) enum GramTable {
  /// Every observed gram interned exactly — collision-free.
  Exact(HashMap<String, u32>),
  /// Grams hashed into `GRAM_BUCKET_BOUND` buckets (fnv1a64 % buckets).
  Bucketed(usize),
}

impl GramTable {
  pub(super) fn slot(&self, gram: &str) -> Option<u32> {
    match self {
      GramTable::Exact(map) => map.get(gram).copied(),
      GramTable::Bucketed(buckets) => Some((fnv1a64(gram) % *buckets as u64) as u32),
    }
  }
}

pub(super) fn fnv1a64(text: &str) -> u64 {
  let mut hash = 0xcbf2_9ce4_8422_2325u64;
  for byte in text.as_bytes() {
    hash ^= *byte as u64;
    hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
  }
  hash
}

/// The trained static model: matrix rows for words and grams (RAW factor space — ABTT
/// and normalization apply at composition time, because composition is a sum and ABTT
/// is affine), the frequency table for uSIF weights, and the fitted post-processing.
pub struct LearnedModel {
  pub dim: usize,
  /// Matrix-vocabulary words in row order.
  pub(super) word_terms: Vec<String>,
  pub(super) word_ids: HashMap<String, u32>,
  /// Row-major (word_terms.len() × dim). COMPOSED word vectors — factor row + Σ gram
  /// rows, precomputed once at train time: composition per vocab word is invariant,
  /// so neither training's pooling passes nor queries re-derive grams for known words.
  pub(super) word_rows: Vec<f32>,
  pub(super) gram_table: GramTable,
  /// Row-major (gram slots × dim).
  pub(super) gram_rows: Vec<f32>,
  /// Unigram probabilities over EVERY corpus token (not just matrix vocabulary) — uSIF
  /// weights are defined for rare tokens precisely because they matter most.
  pub(super) frequencies: HashMap<String, f64>,
  pub(super) usif: UsifWeighting,
  pub(super) abtt: Abtt,
  pub(super) sentence: SentenceComponents,
}

/// What the training run measured and decided — persisted into provenance so the model
/// can always answer "why this dimension, why these weights".
#[derive(Debug)]
pub struct TrainReport {
  pub documents: usize,
  pub token_events: u64,
  pub matrix_words: usize,
  pub gram_slots: usize,
  pub cooc_pairs: usize,
  pub noise_sigma: f64,
  pub selected_dim: usize,
  pub usif_a: f64,
  pub average_doc_len: f64,
}

/// Streamed corpus access: the trainer replays the corpus multiple times (frequency
/// pass, co-occurrence pass, sentence-component pass) through this closure — the caller
/// re-walks its source each call, nothing is materialized here.
pub type CorpusFn<'a> = &'a dyn Fn(&mut dyn FnMut(&[String]));

/// Bounded-memory training resources: the scratch directory for spill runs and factor
/// workspaces, plus the two policy-derived sizes every buffer derivation uses (from
/// vorpal-mem — `HardwareProbe::base_page_bytes` and
/// `ResourcePolicy::arena_chunk_bytes`). One code path from a three-file toy to Meta
/// scale: small corpora never overflow their buffers and never touch the directory.
pub struct TrainResources {
  pub scratch_dir: std::path::PathBuf,
  pub page_bytes: usize,
  pub arena_chunk_bytes: usize,
  /// Phase-boundary progress hook (e.g. the host's warm phase stamps): called at each
  /// training sub-step so kernel-scale runs attribute their time without a profiler.
  /// A plain fn pointer — no captures, no new dependency; pass `|_| {}` to silence.
  pub progress: fn(&str),
}

/// The lookup surface the embedding pipeline needs from a model — implemented by the
/// OWNED [`LearnedModel`] (training, tests) and by the zero-copy mapped view
/// (`persist::ModelView`, the query side). The pipeline itself
/// ([`compose_raw_via`] → [`token_vector_via`] → [`pooled_document_via`] →
/// [`embed_text_via`]) exists ONCE, generic over this trait, so the two backings can
/// never drift — the one-formula-source law.
pub(super) trait TokenLexicon {
  fn dim(&self) -> usize;
  /// The stored (COMPOSED) row for an in-vocabulary word.
  fn word_row(&self, token: &str) -> Option<&[f32]>;
  fn gram_slot(&self, gram: &str) -> Option<u32>;
  fn gram_row(&self, slot: u32) -> Option<&[f32]>;
  /// Unigram probability over the FULL corpus vocabulary (0.0 for unseen).
  fn frequency(&self, token: &str) -> f64;
  fn usif(&self) -> &UsifWeighting;
  fn abtt(&self) -> &Abtt;
  fn sentence(&self) -> &SentenceComponents;
}

impl TokenLexicon for LearnedModel {
  fn dim(&self) -> usize {
    self.dim
  }
  fn word_row(&self, token: &str) -> Option<&[f32]> {
    let &id = self.word_ids.get(token)?;
    self
      .word_rows
      .get(id as usize * self.dim..(id as usize + 1) * self.dim)
  }
  fn gram_slot(&self, gram: &str) -> Option<u32> {
    self.gram_table.slot(gram)
  }
  fn gram_row(&self, slot: u32) -> Option<&[f32]> {
    let start = slot as usize * self.dim;
    self.gram_rows.get(start..start + self.dim)
  }
  fn frequency(&self, token: &str) -> f64 {
    self.frequencies.get(token).copied().unwrap_or(0.0)
  }
  fn usif(&self) -> &UsifWeighting {
    &self.usif
  }
  fn abtt(&self) -> &Abtt {
    &self.abtt
  }
  fn sentence(&self) -> &SentenceComponents {
    &self.sentence
  }
}

/// Raw factor-space composition (fastText: word row + its gram rows; OOV = gram rows
/// alone) over any [`TokenLexicon`]. A known word's stored row already carries its
/// gram sum (composed at train time) — the vocabulary path is a straight copy and the
/// gram walk runs only for OOV tokens. No ABTT, no normalization.
pub(super) fn compose_raw_via<L: TokenLexicon + ?Sized>(lex: &L, token: &str, out: &mut [f32]) {
  if let Some(row) = lex.word_row(token) {
    out.copy_from_slice(row);
    return;
  }
  out.fill(0.0);
  let mut any = false;
  for gram in SubwordTokenizer::grams(token) {
    if let Some(slot) = lex.gram_slot(&gram) {
      if let Some(row) = lex.gram_row(slot) {
        any = true;
        for (slot, value) in out.iter_mut().zip(row) {
          *slot += value;
        }
      }
    }
  }
  if !any {
    out.fill(0.0);
  }
}

/// One token's finished vector: compose → ABTT → unit-normalize (uSIF normalizes word
/// vectors before weighting — the paper's own §1 estimator description).
pub(super) fn token_vector_via<L: TokenLexicon + ?Sized>(lex: &L, token: &str, out: &mut [f32]) -> bool {
  compose_raw_via(lex, token, out);
  if out.iter().all(|v| *v == 0.0) {
    return false;
  }
  lex.abtt().apply(out);
  let norm = out.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>().sqrt();
  if norm <= 0.0 || !norm.is_finite() {
    return false;
  }
  let inverse = (1.0 / norm) as f32;
  for value in out.iter_mut() {
    *value *= inverse;
  }
  true
}

/// uSIF pooling WITHOUT sentence-component removal (training fits the components from
/// exactly this): c̃ = (1/|s|) Σ w(p) · v(token).
pub(super) fn pooled_document_via<L: TokenLexicon + ?Sized>(
  lex: &L,
  doc: &[String],
  out: &mut [f32],
) -> Result<(), ()> {
  out.fill(0.0);
  let mut token_vec = vec![0.0f32; lex.dim()];
  let mut used = 0usize;
  for token in doc {
    if token_vector_via(lex, token, &mut token_vec) {
      let weight = lex.usif().weight(lex.frequency(token)) as f32;
      for (slot, value) in out.iter_mut().zip(&token_vec) {
        *slot += weight * value;
      }
      used += 1;
    }
  }
  if used == 0 {
    return Err(());
  }
  let inverse = 1.0 / doc.len().max(1) as f32;
  for value in out.iter_mut() {
    *value *= inverse;
  }
  Ok(())
}

/// Embed arbitrary text: tokenize (the system-wide rules) → uSIF pooling → piecewise
/// sentence-component removal → final L2 normalization. No representable token = the
/// zero vector ("no semantic signal", exactly like the lexical embedder's empty case).
pub(super) fn embed_text_via<L: TokenLexicon + ?Sized>(lex: &L, text: &str, out: &mut [f32]) {
  debug_assert_eq!(out.len(), lex.dim());
  let tokens = SubwordTokenizer::words(text);
  if tokens.is_empty() || pooled_document_via(lex, &tokens, out).is_err() {
    out.fill(0.0);
    return;
  }
  lex.sentence().remove(out);
  let norm = out.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>().sqrt();
  if norm > 0.0 && norm.is_finite() {
    let inverse = (1.0 / norm) as f32;
    for value in out.iter_mut() {
      *value *= inverse;
    }
  } else {
    out.fill(0.0);
  }
}

/// The PPMI stream of one spilled count set (pull-based; re-streamable).
fn counts_ppmi<'a>(counts: &'a SpilledCounts) -> Result<PpmiStream<'a, PairIter<'a>>, String> {
  ppmi_stream(counts.iter()?, counts.marginals(), counts.total_events())
}

impl LearnedModel {
  /// Train from a re-streamable corpus of tokenized documents. Deterministic for a
  /// fixed corpus order and seed — and bit-identical whether the count pipeline stays
  /// in its buffer or spills through scratch (the equality oracle pins it).
  pub fn train(
    corpus: CorpusFn,
    seed: u64,
    resources: &TrainResources,
  ) -> Result<(LearnedModel, TrainReport), String> {
    // Pass 1 — frequency table, document stats, matrix vocabulary (first-seen order,
    // min-count floor).
    let mut counts: HashMap<String, u64> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut documents = 0usize;
    let mut token_events = 0u64;
    corpus(&mut |doc: &[String]| {
      documents += 1;
      for token in doc {
        let entry = counts.entry(token.clone()).or_insert(0);
        if *entry == 0 {
          order.push(token.clone());
        }
        *entry += 1;
        token_events += 1;
      }
    });
    if documents == 0 || token_events == 0 {
      return Err("empty corpus — nothing to train (lexical fallback)".to_string());
    }
    let average_doc_len = token_events as f64 / documents as f64;
    let total_tokens = token_events as f64;
    let frequencies: HashMap<String, f64> = counts
      .iter()
      .map(|(term, &count)| (term.clone(), count as f64 / total_tokens))
      .collect();
    (resources.progress)("learned: pass1 token counts");

    let mut vocab = Vocab::default();
    for term in &order {
      if counts.get(term).copied().unwrap_or(0) >= MIN_COUNT {
        vocab.intern(term);
      }
    }
    let matrix_words = vocab.len();
    if matrix_words == 0 {
      return Err(format!(
        "no token reaches the min-count floor ({MIN_COUNT}) — corpus below the learned \
         tier's floor (lexical fallback)"
      ));
    }

    // Gram vocabulary over matrix words: exact while it fits, bucketed past the cited
    // bound. Gram row ids live above the word rows in one shared matrix.
    let mut exact_grams: HashMap<String, u32> = HashMap::new();
    let mut word_grams: Vec<Vec<u32>> = Vec::with_capacity(matrix_words);
    let mut distinct_grams = 0u32;
    for word in vocab.terms() {
      let mut slots = Vec::new();
      for gram in SubwordTokenizer::grams(word) {
        let next_id = distinct_grams;
        let slot = *exact_grams.entry(gram).or_insert_with(|| {
          distinct_grams = next_id + 1;
          next_id
        });
        slots.push(slot);
      }
      word_grams.push(slots);
    }
    let (gram_table, gram_slots, word_grams) = if (distinct_grams as usize) <= GRAM_BUCKET_BOUND {
      (
        GramTable::Exact(exact_grams),
        distinct_grams as usize,
        word_grams,
      )
    } else {
      // Rebuild the per-word slots through the bucket map.
      let table = GramTable::Bucketed(GRAM_BUCKET_BOUND);
      let rebuilt: Vec<Vec<u32>> = vocab
        .terms()
        .iter()
        .map(|word| {
          SubwordTokenizer::grams(word)
            .iter()
            .filter_map(|gram| table.slot(gram))
            .collect()
        })
        .collect();
      (table, GRAM_BUCKET_BOUND, rebuilt)
    };
    let matrix_rows = matrix_words + gram_slots;
    (resources.progress)("learned: gram tables");

    // Pass 2 — co-occurrence events (symmetric window over matrix-vocabulary tokens;
    // out-of-vocabulary tokens are transparent, per the count formulation). fastText
    // joint subwords: every pair event (w, c) also credits w's grams with c — the
    // count analog of v_w = z_w + Σ z_g. Documents alternate into two halves for the
    // PIP noise estimate (σ̂ from ‖M̃₁ − M̃₂‖_F), each half the SAME construction.
    //
    // Events NEVER materialize: they stream through bounded spill counters (RAM ∝ one
    // buffer; disk holds pre-aggregated sorted runs; small corpora never overflow and
    // never touch the directory) — the one-code-path law at kernel/Meta scale, where
    // materialized joint-gram events would be tens to hundreds of GB. The buffer is
    // the classical √N external-sort balance with the policy clamp as floor; N is a
    // sizing ESTIMATE (window pairs × average gram fan-out) shaping only the buffer,
    // never correctness.
    let average_grams =
      word_grams.iter().map(Vec::len).sum::<usize>() as f64 / matrix_words.max(1) as f64;
    let expected_events =
      (token_events as f64 * COOC_WINDOW as f64 * (1.0 + 2.0 * average_grams)) as u64;
    let buffer =
      buffer_events_for(expected_events, resources.page_bytes, resources.arena_chunk_bytes);
    let half_buffer = buffer_events_for(
      expected_events / 2,
      resources.page_bytes,
      resources.arena_chunk_bytes,
    );
    let spill_path = |name: &str| resources.scratch_dir.join(format!("train-cooc-{name}.spill"));
    let mut full = SpillCounter::new(spill_path("full"), buffer, resources.page_bytes);
    let mut half_counters = [
      SpillCounter::new(spill_path("half0"), half_buffer, resources.page_bytes),
      SpillCounter::new(spill_path("half1"), half_buffer, resources.page_bytes),
    ];
    let mut doc_index = 0usize;
    let gram_base = matrix_words as u32;
    let mut spill_error: Option<String> = None;
    corpus(&mut |doc: &[String]| {
      if spill_error.is_some() {
        return;
      }
      let ids: Vec<u32> = doc.iter().filter_map(|t| vocab.get(t)).collect();
      let half_index = doc_index % 2;
      doc_index += 1;
      let mut push = |a: u32, b: u32| -> Result<(), String> {
        full.push(a, b)?;
        half_counters[half_index].push(a, b)
      };
      let mut feed = || -> Result<(), String> {
        for (position, &center) in ids.iter().enumerate() {
          let end = (position + COOC_WINDOW + 1).min(ids.len());
          for &context in ids.get(position + 1..end).unwrap_or(&[]) {
            push(center, context)?;
            for &gram in &word_grams[center as usize] {
              push(gram_base + gram, context)?;
            }
            for &gram in &word_grams[context as usize] {
              push(gram_base + gram, center)?;
            }
          }
        }
        Ok(())
      };
      if let Err(error) = feed() {
        spill_error = Some(error);
      }
    });
    if let Some(error) = spill_error {
      return Err(format!("co-occurrence spill failed: {error}"));
    }
    (resources.progress)("learned: cooc events fed");
    let counts = full.finish()?;
    if counts.total_events() == 0 {
      return Err("no co-occurrence events — documents too short (lexical fallback)".to_string());
    }
    let [half_a, half_b] = half_counters;
    let half_a = half_a.finish()?;
    let half_b = half_b.finish()?;
    (resources.progress)("learned: cooc spilled+merged");

    // σ from the streamed half-split difference — the halves never materialize either.
    let sigma = {
      let stream_a = counts_ppmi(&half_a);
      let stream_b = counts_ppmi(&half_b);
      match (stream_a, stream_b) {
        (Ok(a), Ok(b)) => estimate_noise_sigma_streams(a, b, matrix_rows)?,
        // A half too sparse for PPMI means the corpus is at its floor; σ then comes
        // from the fuller side against nothing (conservatively large).
        (Ok(a), Err(_)) | (Err(_), Ok(a)) => {
          estimate_noise_sigma_streams(a, std::iter::empty(), matrix_rows)?
        }
        (Err(a), Err(b)) => return Err(format!("both σ halves untrainable: {a}; {b}")),
      }
    };
    half_a.delete()?;
    half_b.delete()?;
    (resources.progress)("learned: noise sigma");

    // Factorize and pick d by PIP on the corpus's own (thresholded) spectrum. The CSR
    // builds by streaming the PPMI twice (size, then fill) — triples never exist as a
    // vector.
    let matrix = SymmetricCsr::from_pair_stream(matrix_rows, || counts_ppmi(&counts))?;
    let cooc_pairs = matrix.nnz();
    counts.delete()?;
    (resources.progress)("learned: ppmi csr");

    let probe_dim = DIMENSION_CLAMP.1.min(matrix_rows);
    let eigen = top_symmetric_eigen(
      &matrix,
      probe_dim,
      seed,
      FactorWorkspace::Scratch {
        dir: &resources.scratch_dir,
      },
    )?;
    // Rank-deficient corpora return fewer factors than requested — their ENTIRE
    // numerical range, exactly (the small-corpus regime). D2's floor applies only when
    // the rank supports it; the achieved d is recorded in provenance either way, and
    // the vectors' stride is the returned factor count, never the request.
    (resources.progress)("learned: eigen");
    let factors = eigen.eigenvalues.len();
    let magnitudes: Vec<f64> = eigen.eigenvalues.iter().map(|v| v.abs()).collect();
    let signal = soft_threshold(&magnitudes, sigma, matrix_rows);
    let selection = select_dimension(&signal, sigma, matrix_rows, DIMENSION_CLAMP)?;
    let dim = selection.d.min(factors);

    // Symmetric Σ^0.5 weighting (TACL: p = 0.5), rows split back into words and grams.
    let mut weighted = vec![0.0f32; matrix_rows * dim];
    for row in 0..matrix_rows {
      for column in 0..dim {
        let scale = magnitudes[column].sqrt() as f32;
        weighted[row * dim + column] = eigen.vectors[row * factors + column] * scale;
      }
    }
    let word_rows = weighted[..matrix_words * dim].to_vec();
    let gram_rows = weighted[matrix_words * dim..].to_vec();

    // ABTT on the COMPOSED word vectors (the vectors queries actually use).
    let word_ids: HashMap<String, u32> = vocab
      .terms()
      .iter()
      .enumerate()
      .map(|(id, term)| (term.clone(), id as u32))
      .collect();
    let mut prototype = LearnedModel {
      dim,
      word_terms: vocab.terms().to_vec(),
      word_ids,
      word_rows,
      gram_table,
      gram_rows,
      frequencies,
      usif: UsifWeighting { a: 1.0 }, // placeholder until fitted below
      abtt: Abtt {
        mean: vec![0.0; dim],
        components: Vec::new(),
      },
      sentence: SentenceComponents {
        lambdas: Vec::new(),
        components: Vec::new(),
      },
    };
    let mut composed = vec![0.0f32; matrix_words * dim];
    for word_id in 0..matrix_words {
      let row = &mut composed[word_id * dim..(word_id + 1) * dim];
      prototype.compose_word_raw(word_id as u32, row);
    }
    prototype.abtt = Abtt::fit(&composed, matrix_words, dim)?;
    // The composed table IS the stored word table from here on: composition per vocab
    // word is invariant, and recomposing it per token OCCURRENCE dominated kernel-scale
    // training (sampled mid-train: the uSIF/sentence pass lived in compose_raw's gram
    // lookups and per-call allocations across 27M+ occurrences). Same sum, same order —
    // token vectors are bit-identical; gram rows remain for OOV composition.
    prototype.word_rows = composed;
    (resources.progress)("learned: abtt+composed");

    // uSIF: closed-form a from the frequency table + average length; sentence
    // components from the training documents' embeddings (d×d Gram, fixed order).
    // uSIF's V is the FULL unigram vocabulary: the paper computes α over the corpus
    // unigram distribution's support, and the threshold 1 − (1 − 1/|V|)ⁿ is calibrated
    // against that support — a pruned matrix vocabulary collapses it to nonsense.
    // (α is a count over the set, so map-iteration order cannot affect the result.)
    let probabilities: Vec<f64> = prototype.frequencies.values().copied().collect();
    prototype.usif = UsifWeighting::from_frequencies(&probabilities, average_doc_len)?;
    (resources.progress)("learned: usif fit");
    // Sentence-PC Gram over every training document, batched: docs buffer in corpus
    // order, each batch pools in parallel (documents are independent; a failed pool
    // leaves a zero row, contributing exactly the nothing it contributed before), and
    // the batch Gram is ONE deterministic `block_gram` (per-doc rank-1 accumulation as
    // contiguous row sweeps, fixed chunks folded in chunk order). The serial per-doc
    // d×d loop this replaces was 42% of kernel-scale training (75.7 s: 2.76M docs ×
    // 249² f64 on one thread). FIXED batch size — boundaries shape the f64 fold order,
    // so a machine-derived value would break cross-machine bit-determinism; 65,536
    // rows also cap the batch matrix at 64 MB under the D2 dimension clamp (≤256·4 B
    // per row).
    const SENTENCE_BATCH_DOCS: usize = 65_536;
    let mut gram_acc = vec![0.0f64; dim * dim];
    let mut batch: Vec<Vec<String>> = Vec::with_capacity(SENTENCE_BATCH_DOCS);
    fn flush_batch(
      model: &LearnedModel,
      dim: usize,
      batch: &mut Vec<Vec<String>>,
      gram_acc: &mut [f64],
    ) {
      use rayon::prelude::*;
      if batch.is_empty() {
        return;
      }
      let mut matrix = vec![0.0f32; batch.len() * dim];
      matrix
        .par_chunks_mut(dim)
        .zip(batch.par_iter())
        .for_each(|(row, doc)| {
          // Err leaves the row's entry-fill zeros — a zero row adds zero to the Gram.
          let _ = model.pooled_document(doc, row);
        });
      for (total, part) in gram_acc
        .iter_mut()
        .zip(super::rsvd::block_gram(&matrix, &matrix, dim))
      {
        *total += part;
      }
      batch.clear();
    }
    corpus(&mut |doc: &[String]| {
      batch.push(doc.to_vec());
      if batch.len() == SENTENCE_BATCH_DOCS {
        flush_batch(&prototype, dim, &mut batch, &mut gram_acc);
      }
    });
    flush_batch(&prototype, dim, &mut batch, &mut gram_acc);
    prototype.sentence = SentenceComponents::from_gram(&gram_acc, dim)?;
    (resources.progress)("learned: sentence pass");

    let report = TrainReport {
      documents,
      token_events,
      matrix_words,
      gram_slots,
      cooc_pairs,
      noise_sigma: sigma,
      selected_dim: dim,
      usif_a: prototype.usif.a,
      average_doc_len,
    };
    Ok((prototype, report))
  }

  /// [`compose_raw_via`] over this owned model — the pipeline exists once, generic
  /// over [`TokenLexicon`]. Production goes through the generic pipeline directly;
  /// this named surface serves the distributional diagnostics.
  #[cfg(test)]
  fn compose_raw(&self, token: &str, out: &mut [f32]) {
    compose_raw_via(self, token, out);
  }

  /// Training-time composition of one vocabulary word from the RAW factor tables (word
  /// row + gram rows) — the pass that BUILDS the composed table [`compose_raw`] then
  /// serves from. Only meaningful before `word_rows` is swapped to composed values.
  fn compose_word_raw(&self, word_id: u32, out: &mut [f32]) {
    out.fill(0.0);
    let word = word_id as usize;
    let row = &self.word_rows[word * self.dim..(word + 1) * self.dim];
    for (slot, value) in out.iter_mut().zip(row) {
      *slot += value;
    }
    for gram in SubwordTokenizer::grams(&self.word_terms[word]) {
      if let Some(slot_id) = self.gram_table.slot(&gram) {
        let start = slot_id as usize * self.dim;
        if let Some(row) = self.gram_rows.get(start..start + self.dim) {
          for (slot, value) in out.iter_mut().zip(row) {
            *slot += value;
          }
        }
      }
    }
  }

  /// [`token_vector_via`] over this owned model (test/diagnostic surface, as above).
  #[cfg(test)]
  fn token_vector(&self, token: &str, out: &mut [f32]) -> bool {
    token_vector_via(self, token, out)
  }

  /// [`pooled_document_via`] over this owned model.
  fn pooled_document(&self, doc: &[String], out: &mut [f32]) -> Result<(), ()> {
    pooled_document_via(self, doc, out)
  }

  /// Embed arbitrary text: tokenize (the system-wide rules) → uSIF pooling → piecewise
  /// sentence-component removal → final L2 normalization (the tier's contract: unit
  /// rows, cosine-ordering distances, the algebraic orthogonality boundary).
  /// A text with no representable token embeds as the zero vector — callers treat that
  /// as "no semantic signal", exactly like the lexical embedder's empty case.
  pub fn embed_text(&self, text: &str, out: &mut [f32]) {
    embed_text_via(self, text, out);
  }
}

/// Shared test corpus, visible to sibling modules' tests (persist round-trips train
/// through it).
#[cfg(test)]
pub(super) mod tests_support {
  /// Six word families that never co-occur, repeated enough to clear the min-count
  /// floor and de-generate α. SIX, deliberately: ABTT removes ⌈d/100⌉ = 1 principal
  /// component, and with F families the centered word cloud spans F−1 directions — the
  /// method's premise (top PCs are artifacts, not the only signal axis) requires
  /// F − 1 > components removed. F = 2 is the exact degenerate point, pinned in the
  /// model tests.
  pub const FAMILIES: [[&str; 4]; 6] = [
    ["socket", "buffer", "alloc", "packet"],
    ["parser", "token", "grammar", "syntax"],
    ["thread", "mutex", "futex", "sched"],
    ["inode", "dentry", "vfs", "mount"],
    ["page", "folio", "pte", "tlb"],
    ["crypto", "cipher", "digest", "nonce"],
  ];

  pub fn corpus(callback: &mut dyn FnMut(&[String])) {
    let doc = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    for i in 0..30 {
      for family in FAMILIES {
        callback(&doc(&family));
      }
      // Low-frequency noise below the min-count floor, varying per iteration.
      callback(&doc(&[&format!("noise{i}"), "socket"]));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::tests_support::corpus as corpus_fn;
  use super::*;

  fn test_resources(tag: &str) -> TrainResources {
    let dir = std::env::temp_dir().join(format!("vorpal-train-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    TrainResources {
      scratch_dir: dir,
      page_bytes: 4096,
      arena_chunk_bytes: 64 * 1024,
      progress: |_| {},
    }
  }

  #[test]
  fn trains_and_separates_families() {
    let resources = test_resources("families");
    let (model, report) = LearnedModel::train(&corpus_fn, 42, &resources).unwrap();
    assert!(report.matrix_words >= 24, "{report:?}");
    assert!(report.selected_dim >= 1);
    assert!(report.noise_sigma.is_finite());

    let embed = |text: &str| {
      let mut v = vec![0.0f32; model.dim];
      model.embed_text(text, &mut v);
      v
    };
    let cos = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
    let socket = embed("socket");
    let packet = embed("packet");
    let buffer = embed("buffer");
    let alloc = embed("alloc");
    let grammar = embed("grammar");
    assert!(socket.iter().any(|v| *v != 0.0));
    // Distributional invariants — what PPMI-SVD actually promises. In window-2 chain
    // documents [s, b, a, p], socket and packet have IDENTICAL context profiles (both
    // flanked by {buffer, alloc}; the window never pairs s with p), so they must embed
    // nearly identically and survive ABTT + uSIF removal; buffer/alloc share the two
    // contexts {socket, packet}. Adjacent-in-document pairs (socket/buffer) share only
    // {alloc} and are NOT predicted close — first-order co-occurrence is not what a
    // distributional model measures (`stage_cosines_probe` prints the full picture;
    // measured here: +0.997 identical-context vs +0.165 cross-family — the margins
    // below sit far inside the measured gap).
    assert!(
      cos(&socket, &packet) > 0.9,
      "identical-context pair diverged: {}",
      cos(&socket, &packet)
    );
    assert!(
      cos(&socket, &packet) > cos(&socket, &grammar) + 0.5,
      "identical-context pair must dominate cross-family: {} vs {}",
      cos(&socket, &packet),
      cos(&socket, &grammar)
    );
    assert!(
      cos(&buffer, &alloc) > cos(&socket, &grammar),
      "shared-context pair {} must beat cross-family {}",
      cos(&buffer, &alloc),
      cos(&socket, &grammar)
    );
    let _ = std::fs::remove_dir_all(&resources.scratch_dir);
  }

  #[test]
  fn oov_tokens_compose_deterministic_nonzero_vectors() {
    let resources = test_resources("oov");
    let (model, _) = LearnedModel::train(&corpus_fn, 42, &resources).unwrap();
    // "sockets" shares grams with the in-vocabulary "socket".
    let mut first = vec![0.0f32; model.dim];
    let mut second = vec![0.0f32; model.dim];
    model.embed_text("sockets", &mut first);
    model.embed_text("sockets", &mut second);
    assert!(first.iter().any(|v| *v != 0.0), "OOV must compose from grams");
    assert_eq!(
      first.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      second.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    // An OOV word sharing grams with "socket" lands nearer the socket family.
    let mut socket = vec![0.0f32; model.dim];
    model.embed_text("socket", &mut socket);
    let mut grammar = vec![0.0f32; model.dim];
    model.embed_text("grammar", &mut grammar);
    let cos = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
    assert!(cos(&first, &socket) > cos(&first, &grammar));
    let _ = std::fs::remove_dir_all(&resources.scratch_dir);
  }

  #[test]
  fn training_is_bit_deterministic() {
    let resources = test_resources("determinism");
    let (model_a, _) = LearnedModel::train(&corpus_fn, 7, &resources).unwrap();
    let (model_b, _) = LearnedModel::train(&corpus_fn, 7, &resources).unwrap();
    assert_eq!(model_a.dim, model_b.dim);
    assert_eq!(
      model_a.word_rows.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      model_b.word_rows.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    let mut a = vec![0.0f32; model_a.dim];
    let mut b = vec![0.0f32; model_b.dim];
    model_a.embed_text("socket buffer alloc", &mut a);
    model_b.embed_text("socket buffer alloc", &mut b);
    assert_eq!(
      a.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      b.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&resources.scratch_dir);
  }

  #[test]
  fn spilled_and_in_buffer_training_produce_identical_models() {
    // The same corpus through the never-spills path (roomy policy floor) and through a
    // deliberately starved pipeline (16-byte arena floor → √N buffer → many runs and
    // merge levels; 64-byte pages → small fan-in): the models must match BIT FOR BIT.
    // The external count pipeline is an equality-preserving implementation detail of
    // scale, never an approximation.
    let roomy = test_resources("roomy");
    let starved = TrainResources {
      scratch_dir: test_resources("starved").scratch_dir,
      page_bytes: 64,
      arena_chunk_bytes: 16,
      progress: |_| {},
    };
    let (model_a, _) = LearnedModel::train(&corpus_fn, 7, &roomy).unwrap();
    let (model_b, _) = LearnedModel::train(&corpus_fn, 7, &starved).unwrap();
    assert_eq!(model_a.dim, model_b.dim);
    assert_eq!(
      model_a.word_rows.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      model_b.word_rows.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    assert_eq!(
      model_a.gram_rows.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      model_b.gram_rows.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    let mut a = vec![0.0f32; model_a.dim];
    let mut b = vec![0.0f32; model_b.dim];
    model_a.embed_text("socket buffer alloc", &mut a);
    model_b.embed_text("socket buffer alloc", &mut b);
    assert_eq!(
      a.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      b.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&roomy.scratch_dir);
    let _ = std::fs::remove_dir_all(&starved.scratch_dir);
  }

  #[test]
  fn degenerate_corpora_are_typed_errors() {
    let resources = test_resources("degenerate");
    let empty = |_cb: &mut dyn FnMut(&[String])| {};
    assert!(LearnedModel::train(&empty, 1, &resources).is_err());
    // Everything below the min-count floor.
    let sparse = |cb: &mut dyn FnMut(&[String])| {
      let doc: Vec<String> = vec!["one".into(), "two".into()];
      cb(&doc);
    };
    assert!(LearnedModel::train(&sparse, 1, &resources).is_err());
    let _ = std::fs::remove_dir_all(&resources.scratch_dir);
  }

  /// Diagnostic probe (run with --nocapture): cosine of key pairs at every pipeline
  /// stage, so a geometry inversion identifies its own stage.
  #[test]
  fn stage_cosines_probe() {
    let resources = test_resources("probe");
    let (model, report) = LearnedModel::train(&corpus_fn, 42, &resources).unwrap();
    println!("report: {report:?}");
    let cos = |a: &[f32], b: &[f32]| -> f64 {
      let dot: f64 = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum();
      let na: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
      let nb: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
      if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
    };
    let pairs = [
      ("socket", "buffer"),
      ("socket", "packet"),
      ("buffer", "alloc"),
      ("socket", "grammar"),
      ("socket", "cipher"),
    ];
    for (x, y) in pairs {
      let mut raw_x = vec![0.0f32; model.dim];
      let mut raw_y = vec![0.0f32; model.dim];
      model.compose_raw(x, &mut raw_x);
      model.compose_raw(y, &mut raw_y);
      let mut tok_x = vec![0.0f32; model.dim];
      let mut tok_y = vec![0.0f32; model.dim];
      let _ = model.token_vector(x, &mut tok_x);
      let _ = model.token_vector(y, &mut tok_y);
      let mut fin_x = vec![0.0f32; model.dim];
      let mut fin_y = vec![0.0f32; model.dim];
      model.embed_text(x, &mut fin_x);
      model.embed_text(y, &mut fin_y);
      println!(
        "{x:>7}/{y:<7}  raw {:+.3}  post-abtt {:+.3}  final {:+.3}",
        cos(&raw_x, &raw_y),
        cos(&tok_x, &tok_y),
        cos(&fin_x, &fin_y),
      );
    }
    // Stored-row cosines (the persisted COMPOSED word vectors), directly from the table.
    let row = |term: &str| -> Vec<f32> {
      let id = model.word_ids[term] as usize;
      model.word_rows[id * model.dim..(id + 1) * model.dim].to_vec()
    };
    for (x, y) in pairs {
      println!(
        "{x:>7}/{y:<7}  stored-composed-row {:+.3}",
        cos(&row(x), &row(y))
      );
    }
    let _ = std::fs::remove_dir_all(&resources.scratch_dir);
  }

  #[test]
  fn two_family_degenerate_corpus_is_deterministic_documented_behavior() {
    // With exactly TWO families, the centered word cloud spans ONE direction; ABTT's
    // single removed component IS that axis, and what survives is within-family
    // residual noise — anti-correlated by construction (residuals sum to ~0 inside a
    // family, so pairwise cosines sit near −1/(family_size−1)). This is the cited
    // method faithfully applied below its regime, NOT a defect to "fix" by special-
    // casing: whether such corpora ship a learned tier at all is the eval gate's
    // decision (the Stage-1 small-corpus floor), pinned at integration level. Here we
    // pin only what train() guarantees everywhere: success and bit-determinism.
    let two_families = |cb: &mut dyn FnMut(&[String])| {
      let doc = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
      for i in 0..30 {
        cb(&doc(&["socket", "buffer", "alloc", "packet"]));
        cb(&doc(&["parser", "token", "grammar", "syntax"]));
        cb(&doc(&[&format!("noise{i}"), "socket"]));
      }
    };
    let resources = test_resources("two-family");
    let (model_a, _) = LearnedModel::train(&two_families, 11, &resources).unwrap();
    let (model_b, _) = LearnedModel::train(&two_families, 11, &resources).unwrap();
    let mut a = vec![0.0f32; model_a.dim];
    let mut b = vec![0.0f32; model_b.dim];
    model_a.embed_text("socket buffer", &mut a);
    model_b.embed_text("socket buffer", &mut b);
    assert_eq!(
      a.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      b.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&resources.scratch_dir);
  }

  #[test]
  fn unrepresentable_text_embeds_to_zero() {
    let resources = test_resources("unrepresentable");
    let (model, _) = LearnedModel::train(&corpus_fn, 42, &resources).unwrap();
    let mut v = vec![1.0f32; model.dim];
    model.embed_text("", &mut v);
    assert!(v.iter().all(|x| *x == 0.0));
    let _ = std::fs::remove_dir_all(&resources.scratch_dir);
  }
}

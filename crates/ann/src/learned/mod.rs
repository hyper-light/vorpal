//! Tier 1 of the semantic plan (docs/wip/SEMANTIC_TIER.md §3 D1, §4 Stage 1): a fully
//! owned, deterministic, corpus-derived static embedding — subword tokenization →
//! definition-window co-occurrence → PPMI → randomized SVD → ABTT → uSIF pooling, with
//! the dimension chosen by the PIP criterion. Every knob is either derived from the
//! ingested corpus, a proven closed form, or a cited literature constant — never a
//! machine-tuned number (the no-magic rule; see each site's citation).
//!
//! Evidence base (§2b of the design doc, primary sources): count-based factorization
//! ties/beats SGNS below ~10⁸ tokens (Sahlgren & Lenci D16-1099; TACL Q15-1016) — the
//! entire corpus range vorpal serves; the TACL 2015 knobs are context-distribution
//! smoothing α = 0.75, NO PMI shift under SVD, small windows, symmetric Σ^p weighting;
//! subword n-grams fix the small-corpus floor and give OOV identifiers compositional
//! vectors (Bojanowski et al., arXiv:1607.04606).
//!
//! This module is pure math over caller-supplied documents: vorpal-index feeds each
//! node's embedded surface (name / signature / basename tokens) as one document, and
//! receives a serializable model. Nothing here touches production search until the
//! tier-selection plumbing wires it in — the lexical default stays byte-identical.

mod cooc;
mod embedder;
mod model;
mod persist;
mod pip;
mod pool;
mod rsvd;
mod spill;
mod subword;

pub use cooc::{CoocCounts, PpmiStream, Vocab, ppmi, ppmi_stream};
pub use embedder::{LEARNED_EMBED_VERSION, LearnedStaticEmbedder};
pub use model::{
  COOC_WINDOW, DIMENSION_CLAMP, GRAM_BUCKET_BOUND, LearnedModel, MIN_COUNT, TrainReport,
  TrainResources,
};
pub use persist::{LEARNED_MODEL_VERSION, ModelView, load_model, model_to_bytes, save_model};
pub use pip::{
  PIP_ALPHA_EXPONENT, PipSelection, estimate_noise_sigma, select_dimension, soft_threshold,
};
pub use pool::{Abtt, SentenceComponents, USIF_COMPONENTS, UsifWeighting, abtt_component_count};
pub use rsvd::{FactorWorkspace, SymmetricCsr, TopEigen, top_symmetric_eigen};
pub use spill::{PairIter, SpillCounter, SpilledCounts, buffer_events_for};
pub use subword::SubwordTokenizer;

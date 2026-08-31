//! The [`Embedder`] adapter over a trained model — what plugs the learned tier into
//! the existing pluggable-embedding seam (tier builds' fill closures, query embedding,
//! the provenance gate). Provenance carries `learned: true`, honestly labeled; the
//! provenance gate invalidates any persisted tier on model change, so a swap can only
//! ever route queries to the exact fallback until a re-warm rebuilds — never mix
//! embedders in one pool.

use crate::embed::{Embedder, ModelProvenance};
use crate::learned::model::LearnedModel;

/// Semantics version of the learned-static pipeline (tokenization, gram scheme, PPMI
/// knobs, factorization, ABTT/uSIF post-processing). Bumped on ANY semantic change so
/// vectors persisted under older semantics never silently compare against fresh query
/// vectors — the same contract `LEXICAL_EMBED_VERSION` carries for the default.
pub const LEARNED_EMBED_VERSION: u32 = 1;

/// A trained corpus-derived static embedder (docs/wip/SEMANTIC_TIER.md Tier 1).
pub struct LearnedStaticEmbedder {
  model: LearnedModel,
}

impl LearnedStaticEmbedder {
  pub fn new(model: LearnedModel) -> Self {
    Self { model }
  }

  pub fn model(&self) -> &LearnedModel {
    &self.model
  }

  /// This model's complete provenance. `dim` is the PIP-chosen dimension — adaptive
  /// per corpus, carried by the model itself and validated by the freshness gate.
  pub fn provenance(&self) -> ModelProvenance {
    ModelProvenance {
      model_id: "learned-static".to_string(),
      dim: self.model.dim,
      normalization: "l2".to_string(),
      version: LEARNED_EMBED_VERSION,
      learned: true,
    }
  }
}

impl Embedder for LearnedStaticEmbedder {
  fn dim(&self) -> usize {
    self.model.dim
  }

  fn embed(&self, text: &str) -> Vec<f32> {
    let mut out = vec![0.0f32; self.model.dim];
    self.model.embed_text(text, &mut out);
    out
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::learned::model::{TrainResources, tests_support};

  #[test]
  fn adapter_matches_the_model_bitwise_and_labels_itself_learned() {
    let dir = std::env::temp_dir().join(format!("vorpal-embedder-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let resources = TrainResources {
      scratch_dir: dir.clone(),
      page_bytes: 4096,
      arena_chunk_bytes: 64 * 1024,
      progress: |_| {},
    };
    let (model, _) = LearnedModel::train(&tests_support::corpus, 42, &resources).unwrap();
    let dim = model.dim;
    let embedder = LearnedStaticEmbedder::new(model);

    let via_trait = embedder.embed("socket buffer alloc");
    let mut direct = vec![0.0f32; dim];
    embedder.model().embed_text("socket buffer alloc", &mut direct);
    assert_eq!(
      via_trait.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
      direct.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );

    let provenance = embedder.provenance();
    assert!(provenance.learned, "the learned tier must label itself");
    assert_eq!(provenance.model_id, "learned-static");
    assert_eq!(provenance.dim, dim);
    assert_eq!(provenance.normalization, "l2");
    let _ = std::fs::remove_dir_all(&dir);
  }
}

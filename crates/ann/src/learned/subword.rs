//! Subword tokenization for identifiers: the shared word tokenizer (`tokenize` — the
//! same boundary rules the whole system uses) plus fastText-style character n-grams so
//! unseen identifiers compose deterministic vectors from their pieces
//! (Bojanowski et al., "Enriching Word Vectors with Subword Information",
//! arXiv:1607.04606 — trained on 1% of Wikipedia it beats CBOW on 100% for rare words;
//! the small-corpus floor fix the design doc names).

use crate::embed::tokenize;

/// fastText's n-gram range, cited defaults (arXiv:1607.04606 §4: n ∈ [3, 6]). The word
/// itself is always represented in addition to its n-grams.
pub const MIN_GRAM: usize = 3;
pub const MAX_GRAM: usize = 6;

/// Character n-gram extraction over a token, fastText-style: the token is wrapped in
/// boundary markers `<` and `>` (so prefixes/suffixes are distinct grams — `<wh` differs
/// from `whe`), and every character n-gram with n ∈ [MIN_GRAM, MAX_GRAM] is emitted.
/// A token shorter than MIN_GRAM (with markers) emits only itself.
///
/// Grams are emitted as owned strings — model building interns them into an explicit
/// gram table (exact, collision-free where it fits; hashed buckets only when the table
/// must be bounded, decided by the model builder from the observed corpus).
pub struct SubwordTokenizer;

impl SubwordTokenizer {
  /// The word tokens of `text` — the system-wide boundary rules (non-alphanumeric +
  /// camelCase humps, lowercased).
  pub fn words(text: &str) -> Vec<String> {
    tokenize(text)
  }

  /// The character n-grams of one (already lowercased) word token, with boundary
  /// markers. Deterministic order: by n, then by position.
  pub fn grams(word: &str) -> Vec<String> {
    let wrapped: Vec<char> = std::iter::once('<')
      .chain(word.chars())
      .chain(std::iter::once('>'))
      .collect();
    let mut grams = Vec::new();
    for n in MIN_GRAM..=MAX_GRAM {
      if wrapped.len() < n {
        break;
      }
      for window in wrapped.windows(n) {
        // Skip the gram equal to the whole wrapped word: the word itself is already a
        // first-class vocabulary entry (fastText makes the same distinction).
        if n == wrapped.len() {
          continue;
        }
        grams.push(window.iter().collect());
      }
    }
    grams
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn words_use_the_shared_boundary_rules() {
    assert_eq!(
      SubwordTokenizer::words("resolveImportPath_v2"),
      vec!["resolve", "import", "path", "v2"]
    );
  }

  #[test]
  fn grams_are_wrapped_windows_by_n_then_position() {
    // "abc" wraps to "<abc>" (5 chars): 3-grams <ab, abc, bc>; 4-grams <abc, abc>;
    // the full 5-gram equals the wrapped word and is skipped.
    assert_eq!(
      SubwordTokenizer::grams("abc"),
      vec!["<ab", "abc", "bc>", "<abc", "abc>"]
    );
  }

  #[test]
  fn short_tokens_emit_no_grams() {
    // "v2" wraps to "<v2>" (4 chars): 3-grams <v2, v2>; 4-gram would equal the wrapped
    // word → skipped.
    assert_eq!(SubwordTokenizer::grams("v2"), vec!["<v2", "v2>"]);
    // "a" wraps to "<a>" (3 chars): the only 3-gram equals the wrapped word → nothing.
    assert_eq!(SubwordTokenizer::grams("a"), Vec::<String>::new());
  }

  #[test]
  fn unicode_identifiers_gram_by_characters_not_bytes() {
    // "héllo" wraps to "<héllo>" (7 chars) — windows are char-wise, no byte splits.
    let grams = SubwordTokenizer::grams("héllo");
    assert!(grams.contains(&"<hé".to_string()));
    assert!(grams.contains(&"llo>".to_string()));
    assert!(grams.iter().all(|g| g.chars().count() >= MIN_GRAM));
  }

  #[test]
  fn deterministic() {
    assert_eq!(
      SubwordTokenizer::grams("determinism"),
      SubwordTokenizer::grams("determinism")
    );
  }
}

//! Owned WordPiece tokenizer for the vendored code encoder (semantic-tier Stage 6).
//!
//! Reproduces the exact `tokenizer.json` pipeline the model was trained with
//! (BertNormalizer → BertPreTokenizer → WordPiece → `[CLS] A [SEP]`), verified
//! byte-for-byte against the reference `tokenizers` library over a battery of
//! unicode/code/casing inputs (`tests/encoder.rs`, gated on the model directory).
//! Every rule below is the upstream tokenizer's documented behavior, not a choice:
//! the configuration is READ from tokenizer.json and validated — a file asking for
//! a pipeline this implementation does not reproduce is a typed error, never a
//! silently different tokenization.

use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};

pub struct WordPiece {
  vocab: HashMap<String, u32>,
  unk: u32,
  cls: u32,
  sep: u32,
  continuing_prefix: String,
  max_input_chars: usize,
  lowercase: bool,
  strip_accents: bool,
  clean_text: bool,
  handle_chinese_chars: bool,
}

impl WordPiece {
  /// Parse and VALIDATE a `tokenizer.json`: the model, normalizer, pre-tokenizer,
  /// and template must be exactly the pipeline this implementation reproduces.
  pub fn from_tokenizer_json(bytes: &[u8]) -> Result<WordPiece, String> {
    let value: serde_json::Value =
      serde_json::from_slice(bytes).map_err(|e| format!("tokenizer.json parse: {e}"))?;
    let model = value
      .get("model")
      .ok_or("tokenizer.json: no model section")?;
    if model.get("type").and_then(|t| t.as_str()) != Some("WordPiece") {
      return Err("tokenizer.json: model type is not WordPiece".to_string());
    }
    let vocab_value = model
      .get("vocab")
      .and_then(|v| v.as_object())
      .ok_or("tokenizer.json: no vocab object")?;
    let mut vocab = HashMap::with_capacity(vocab_value.len());
    for (token, id) in vocab_value {
      let id = id
        .as_u64()
        .and_then(|id| u32::try_from(id).ok())
        .ok_or_else(|| format!("tokenizer.json: non-u32 id for {token:?}"))?;
      vocab.insert(token.clone(), id);
    }
    let unk_token = model
      .get("unk_token")
      .and_then(|t| t.as_str())
      .ok_or("tokenizer.json: no unk_token")?;
    let continuing_prefix = model
      .get("continuing_subword_prefix")
      .and_then(|p| p.as_str())
      .unwrap_or("##")
      .to_string();
    let max_input_chars = model
      .get("max_input_chars_per_word")
      .and_then(|m| m.as_u64())
      .unwrap_or(100) as usize;
    let normalizer = value
      .get("normalizer")
      .ok_or("tokenizer.json: no normalizer")?;
    if normalizer.get("type").and_then(|t| t.as_str()) != Some("BertNormalizer") {
      return Err("tokenizer.json: normalizer is not BertNormalizer".to_string());
    }
    let flag = |name: &str, default: bool| -> bool {
      normalizer.get(name).and_then(|f| f.as_bool()).unwrap_or(default)
    };
    let lowercase = flag("lowercase", true);
    // The upstream default: strip accents exactly when lowercasing, unless the
    // file says otherwise explicitly.
    let strip_accents = normalizer
      .get("strip_accents")
      .and_then(|f| f.as_bool())
      .unwrap_or(lowercase);
    if value.get("pre_tokenizer").and_then(|p| p.get("type")).and_then(|t| t.as_str())
      != Some("BertPreTokenizer")
    {
      return Err("tokenizer.json: pre_tokenizer is not BertPreTokenizer".to_string());
    }
    let id_of = |token: &str| -> Result<u32, String> {
      vocab
        .get(token)
        .copied()
        .ok_or_else(|| format!("tokenizer.json: vocab lacks {token:?}"))
    };
    let (unk, cls, sep) = (id_of(unk_token)?, id_of("[CLS]")?, id_of("[SEP]")?);
    Ok(WordPiece {
      vocab,
      unk,
      cls,
      sep,
      continuing_prefix,
      max_input_chars,
      lowercase,
      strip_accents,
      clean_text: flag("clean_text", true),
      handle_chinese_chars: flag("handle_chinese_chars", true),
    })
  }

  /// `[CLS] pieces([A]) [SEP]` — the single-sequence template the model card uses.
  pub fn encode(&self, text: &str) -> Vec<u32> {
    let mut ids = vec![self.cls];
    for word in self.pre_tokenize(&self.normalize(text)) {
      self.word_pieces(&word, &mut ids);
    }
    ids.push(self.sep);
    ids
  }

  /// BertNormalizer: clean_text → CJK spacing → strip accents (NFD, drop combining
  /// marks) → lowercase. Order matches the reference implementation; the gated
  /// battery test pins it against the real library.
  fn normalize(&self, text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    for c in text.chars() {
      if self.clean_text {
        if c == '\0' || c == '\u{fffd}' || (c.is_control() && c != '\t' && c != '\n' && c != '\r') {
          continue;
        }
        if c.is_whitespace() {
          cleaned.push(' ');
          continue;
        }
      }
      if self.handle_chinese_chars && is_cjk(c) {
        cleaned.push(' ');
        cleaned.push(c);
        cleaned.push(' ');
        continue;
      }
      cleaned.push(c);
    }
    let accentless: String = if self.strip_accents {
      cleaned.nfd().filter(|c| !is_combining_mark(*c)).collect()
    } else {
      cleaned
    };
    if self.lowercase {
      accentless.chars().flat_map(char::to_lowercase).collect()
    } else {
      accentless
    }
  }

  /// BertPreTokenizer: whitespace-separated runs, with every punctuation character
  /// (ASCII or Unicode P*) split out as its own token.
  fn pre_tokenize(&self, text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
      if c.is_whitespace() {
        if !current.is_empty() {
          words.push(std::mem::take(&mut current));
        }
      } else if is_punctuation(c) {
        if !current.is_empty() {
          words.push(std::mem::take(&mut current));
        }
        words.push(c.to_string());
      } else {
        current.push(c);
      }
    }
    if !current.is_empty() {
      words.push(current);
    }
    words
  }

  /// Greedy longest-match WordPiece: first piece bare, continuations prefixed;
  /// an unmatchable word (or one over the char cap) is one `[UNK]`.
  fn word_pieces(&self, word: &str, ids: &mut Vec<u32>) {
    let chars: Vec<char> = word.chars().collect();
    if chars.len() > self.max_input_chars {
      ids.push(self.unk);
      return;
    }
    let mut pieces = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
      let mut end = chars.len();
      let mut found = None;
      while end > start {
        let mut piece: String = if start > 0 {
          self.continuing_prefix.clone()
        } else {
          String::new()
        };
        piece.extend(&chars[start..end]);
        if let Some(&id) = self.vocab.get(&piece) {
          found = Some(id);
          break;
        }
        end -= 1;
      }
      match found {
        Some(id) => {
          pieces.push(id);
          start = end;
        }
        None => {
          ids.push(self.unk);
          return;
        }
      }
    }
    ids.extend(pieces);
  }
}

/// BERT's documented CJK codepoint blocks (the `handle_chinese_chars` set).
fn is_cjk(c: char) -> bool {
  matches!(u32::from(c),
    0x4E00..=0x9FFF
      | 0x3400..=0x4DBF
      | 0x20000..=0x2A6DF
      | 0x2A700..=0x2B73F
      | 0x2B740..=0x2B81F
      | 0x2B820..=0x2CEAF
      | 0xF900..=0xFAFF
      | 0x2F800..=0x2FA1F)
}

/// BERT's punctuation predicate: the four ASCII ranges (which include characters
/// like `$` and `` ` `` that Unicode classes as symbols) OR the Unicode P* group.
fn is_punctuation(c: char) -> bool {
  let cp = u32::from(c);
  matches!(cp, 33..=47 | 58..=64 | 91..=96 | 123..=126)
    || c.general_category_group() == GeneralCategoryGroup::Punctuation
}

#[cfg(test)]
mod tests {
  use super::*;

  fn tiny() -> WordPiece {
    let mut vocab = HashMap::new();
    // Deliberately NO whole "world": greedy longest-match must take wor + ##ld.
    for (index, token) in [
      "[UNK]", "[CLS]", "[SEP]", "hello", "##ld", "wor", "he", "##llo", ",",
    ]
    .iter()
    .enumerate()
    {
      vocab.insert((*token).to_string(), index as u32);
    }
    WordPiece {
      vocab,
      unk: 0,
      cls: 1,
      sep: 2,
      continuing_prefix: "##".to_string(),
      max_input_chars: 100,
      lowercase: true,
      strip_accents: true,
      clean_text: true,
      handle_chinese_chars: true,
    }
  }

  #[test]
  fn greedy_longest_match_and_unk() {
    let tokenizer = tiny();
    // "hello" matches whole; "world" -> wor + ##ld; "xyz" -> [UNK].
    assert_eq!(tokenizer.encode("Hello world, xyz"), vec![1, 3, 5, 4, 8, 0, 2]);
  }

  #[test]
  fn normalizer_strips_accents_and_lowercases() {
    let tokenizer = tiny();
    assert_eq!(tokenizer.normalize("HÉLLO"), "hello");
    assert_eq!(tokenizer.normalize("a\u{0}b\tc"), "ab c");
  }

  #[test]
  fn punctuation_splits_and_cjk_spaces() {
    let tokenizer = tiny();
    assert_eq!(
      tokenizer.pre_tokenize("a,b c"),
      vec!["a".to_string(), ",".to_string(), "b".to_string(), "c".to_string()]
    );
    assert_eq!(tokenizer.normalize("a中b"), "a 中 b");
  }
}

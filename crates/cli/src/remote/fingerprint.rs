//! Grammar fingerprint (invariant I2 — extraction parity).
//!
//! Agent-mode matches are trustworthy only if the node's tree-sitter grammars behave exactly like
//! the coordinator's. The fingerprint is a `blake3` digest over each language's observable grammar
//! surface — name, ABI version, the full node-kind table (with namedness), and the full field
//! table — plus the crate version and wire protocol. Two builds that disagree on any of those can
//! produce different trees, so the coordinator refuses (or demotes to streaming) on mismatch;
//! matching fingerprints plus the exact-version gate make silent divergence structurally hard.
//!
//! Computed over `SgLang::all_langs()` **at call time**: custom languages register per job, so the
//! handshake carries the built-in fingerprint and the job carries the post-`LangEnv` one.

use crate::lang::SgLang;
use vorpal_language::LanguageExt;

/// Fingerprint of every currently registered language (builtins + any registered customs).
pub fn grammar_fingerprint() -> [u8; 32] {
  fingerprint_langs(SgLang::all_langs())
}

/// Fingerprint of the built-in grammars only. This is what an agent advertises in `Welcome`
/// (custom languages register later, per job), so it is what the coordinator can check at
/// handshake time; the post-`LangEnv` fingerprint is re-verified agent-side against the job.
pub fn builtin_fingerprint() -> [u8; 32] {
  fingerprint_langs(
    SgLang::all_langs()
      .into_iter()
      .filter(|lang| matches!(lang, SgLang::Builtin(_)))
      .collect(),
  )
}

fn fingerprint_langs(mut langs: Vec<SgLang>) -> [u8; 32] {
  // Sort by display name so registration order cannot perturb the digest.
  langs.sort_by_key(|l| l.to_string());
  let mut hasher = blake3::Hasher::new();
  hasher.update(b"vorpal-grammar-fingerprint/v1\n");
  hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
  hasher.update(&[vorpal_wire::PROTOCOL_VERSION]);
  for lang in langs {
    let ts = lang.get_ts_language();
    hasher.update(b"\nlang\n");
    hasher.update(lang.to_string().as_bytes());
    hasher.update(&(ts.abi_version() as u64).to_le_bytes());
    let kinds = ts.node_kind_count();
    hasher.update(&(kinds as u64).to_le_bytes());
    for id in 0..kinds {
      let id = id as u16;
      if let Some(kind) = ts.node_kind_for_id(id) {
        hasher.update(kind.as_bytes());
      }
      hasher.update(&[0, u8::from(ts.node_kind_is_named(id))]);
    }
    let fields = ts.field_count();
    hasher.update(&(fields as u64).to_le_bytes());
    for id in 1..=fields {
      if let Some(field) = ts.field_name_for_id(id as u16) {
        hasher.update(field.as_bytes());
      }
      hasher.update(&[0]);
    }
  }
  *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn fingerprint_is_deterministic_and_registration_order_independent() {
    let a = grammar_fingerprint();
    let b = grammar_fingerprint();
    assert_eq!(a, b);
    // Reversed input order must not change the digest (sorted internally).
    let mut langs = SgLang::all_langs();
    langs.reverse();
    assert_eq!(fingerprint_langs(langs), a);
  }

  #[test]
  fn fingerprint_is_not_trivial() {
    assert_ne!(grammar_fingerprint(), [0u8; 32]);
  }
}

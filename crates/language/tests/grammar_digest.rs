//! F-M0 acceptance: the shared-surface refactor of the grammar digest is VALUE-IDENTICAL
//! to the historical inline implementation for every compiled-in grammar. Digests live in
//! product headers on disk — a changed value would silently invalidate every cache.

use vorpal_language::{LanguageExt, SupportLang, grammar_digest, grammar_digest_of};

/// The pre-refactor implementation, verbatim.
fn legacy_digest(lang: SupportLang) -> u64 {
  let ts = lang.get_ts_language();
  let mut h = xxhash_rust::xxh3::Xxh3::new();
  if let Some(name) = ts.name() {
    h.update(name.as_bytes());
  }
  h.update(&(ts.abi_version() as u64).to_le_bytes());
  if let Some(md) = ts.metadata() {
    h.update(&[md.major_version, md.minor_version, md.patch_version]);
  }
  let node_kinds = ts.node_kind_count();
  let fields = ts.field_count();
  h.update(&(node_kinds as u64).to_le_bytes());
  h.update(&(ts.parse_state_count() as u64).to_le_bytes());
  h.update(&(fields as u64).to_le_bytes());
  for id in 0..node_kinds as u16 {
    if let Some(name) = ts.node_kind_for_id(id) {
      h.update(name.as_bytes());
      h.update(&[u8::from(ts.node_kind_is_named(id))]);
    }
    h.update(&[0xff]);
  }
  for id in 1..=fields as u16 {
    if let Some(name) = ts.field_name_for_id(id) {
      h.update(name.as_bytes());
    }
    h.update(&[0xff]);
  }
  h.digest()
}

#[test]
fn surface_walk_digest_is_value_identical_to_legacy_for_every_grammar() {
  for &lang in SupportLang::all_langs() {
    let legacy = legacy_digest(lang);
    let through_cache = grammar_digest(lang);
    let through_surface = grammar_digest_of(&lang.get_ts_language());
    assert_eq!(legacy, through_surface, "{lang:?}: surface walk drifted from legacy bytes");
    assert_eq!(legacy, through_cache, "{lang:?}: cached digest drifted");
    assert_ne!(legacy, 0, "{lang:?}: degenerate digest");
  }
}

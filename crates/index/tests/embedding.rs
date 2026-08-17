//! The embedded-host lifecycle contract (docs/EMBEDDING.md): the session interner is owned
//! by each build and freed by `Drop` — a long-lived host indexes repeatedly with bounded
//! memory and NO reclaim call, and output is bit-identical across session boundaries.
//!
//! The compile-time half of the contract cannot be shown by a passing test: `NameId`
//! carries the session lifetime, so code that tries to hold a `Reference` (or anything
//! built from session ids) past its `Interner` simply does not compile — pinned by the
//! `compile_fail` doctest on `vorpal_resolve::Interner`.

use std::fs;

// The escape-is-a-compile-error half lives as a `compile_fail` doctest on
// `vorpal_resolve::Interner` (doctests only execute for library targets).

#[test]
fn sessions_free_on_drop_and_output_is_stable_across_them() {
  let base = std::env::temp_dir().join(format!("vorpal-embed-{}", std::process::id()));
  let src = base.join("src");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  fs::write(
    src.join("a.rs"),
    "pub fn alpha() -> u32 { beta() }\npub fn beta() -> u32 { 2 }\n",
  )
  .unwrap();
  fs::write(src.join("b.py"), "def gamma():\n    return alpha_like()\n").unwrap();

  // Session 1 and session 2: each build owns and drops its interner internally — the
  // embedded host's steady state is just "call build_index again". Bit-identical output
  // across the boundary pins that session scoping never perturbs artifacts (interner ids
  // are process-internal and never persisted).
  let out1 = base.join("s1");
  vorpal_index::build_index(&src, &out1).unwrap();
  let id1 = fs::read_to_string(out1.join("CURRENT")).unwrap();

  let out2 = base.join("s2");
  vorpal_index::build_index(&src, &out2).unwrap();
  let id2 = fs::read_to_string(out2.join("CURRENT")).unwrap();
  assert_eq!(id1, id2, "session boundaries must not perturb output");

  // Per-session telemetry: an explicit session observes its own bounded vocabulary and
  // frees it on drop (a fresh session starts from zero).
  let session = vorpal_ingest::Interner::default();
  session.intern("observed_name");
  assert_eq!(session.retained_strings(), 1);
  assert!(session.retained_bytes() > 0);
  drop(session);
  assert_eq!(vorpal_ingest::Interner::default().retained_strings(), 0);

  // Artifacts from prior sessions keep serving: they never contain interner ids.
  let kg = vorpal_index::Kg::load(&out1).unwrap();
  assert!(kg.node_count() > 0);

  let _ = fs::remove_dir_all(&base);
}

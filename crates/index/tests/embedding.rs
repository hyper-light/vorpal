//! The embedded-host lifecycle contract (docs/EMBEDDING.md): a long-lived process indexes,
//! reclaims the session interner, and indexes again — memory returns to zero between
//! sessions and output is bit-identical across the reclaim boundary.
//!
//! Own integration file deliberately: each integration test binary is its own process, so
//! this test owns the process-wide interner outright — `reclaim_all`'s safety contract is
//! upheld by construction (no other test's `NameId`s exist here).

use std::fs;

#[test]
fn reclaim_between_sessions_frees_the_interner_and_preserves_output() {
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

  // Session 1: build, observe retained vocabulary.
  let out1 = base.join("s1");
  vorpal_index::build_index(&src, &out1).unwrap();
  let id1 = fs::read_to_string(out1.join("CURRENT")).unwrap();
  let retained = vorpal_ingest::intern_retained_strings();
  assert!(retained > 0, "a build interns names/paths");
  assert!(vorpal_ingest::intern_retained_bytes() > 0);

  // Session boundary: nothing from the build is alive (build_index returned; Kg holds no
  // interner ids) — the reclaim contract holds by construction.
  let stats = unsafe { vorpal_index::reclaim_session_memory() };
  assert_eq!(stats.strings, retained);
  assert_eq!(vorpal_ingest::intern_retained_strings(), 0, "arena returned to zero");
  assert_eq!(vorpal_ingest::intern_retained_bytes(), 0);

  // Session 2: same corpus, fresh interner — bit-identical output (the interner is
  // process-internal and never reaches artifacts; this pins that across a reclaim).
  let out2 = base.join("s2");
  vorpal_index::build_index(&src, &out2).unwrap();
  let id2 = fs::read_to_string(out2.join("CURRENT")).unwrap();
  assert_eq!(id1, id2, "reclaim must not perturb output");
  assert_eq!(vorpal_ingest::intern_retained_strings(), retained, "same corpus, same vocabulary");

  // Queries on a pre-reclaim index still work afterwards: persisted artifacts carry no
  // interner ids.
  let kg = vorpal_index::Kg::load(&out1).unwrap();
  assert!(kg.node_count() > 0);

  let _ = fs::remove_dir_all(&base);
}

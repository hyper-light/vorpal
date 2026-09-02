//! Live rebuild v1 (SUBSECOND.md Phase 3): `build_index_live`'s deferred persistence must be
//! observationally invisible — the sealed graph handed back answers exactly like the graph
//! loaded from the generation it later persists, and that generation is identical to what a
//! synchronous build of the same tree commits.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

fn gen_id(out: &Path) -> String {
  fs::read_to_string(out.join("CURRENT")).expect("CURRENT exists")
}

fn corpus(src: &Path) {
  fs::create_dir_all(src).unwrap();
  fs::write(
    src.join("alpha.py"),
    "def alpha():\n    return beta()\n\ndef beta():\n    return 1\n",
  )
  .unwrap();
  fs::write(
    src.join("gamma.py"),
    "from alpha import beta\n\ndef gamma():\n    return beta()\n",
  )
  .unwrap();
  fs::write(
    src.join("delta.c"),
    "int epsilon(void) { return 5; }\nint delta(void) { return epsilon(); }\n",
  )
  .unwrap();
}

/// Rendered answers straight off a graph object — the exact surface the daemon serves from
/// during the deferred-persist window.
fn battery_on(kg: &vorpal_kg::Kg) -> String {
  let mut out = String::new();
  for name in ["alpha", "beta", "gamma", "delta", "epsilon", "ghost"] {
    for verb in ["node", "callers", "refs", "importers"] {
      out.push_str(&format!("== {verb} {name}\n"));
      let target = vorpal_index::GraphTarget {
        name: (*name).to_string(),
        ..vorpal_index::GraphTarget::default()
      };
      match vorpal_index::graph_query_on(kg, verb, &target) {
        Ok(rendered) => out.push_str(&rendered),
        Err(err) => out.push_str(&format!("error: {err}\n")),
      }
    }
  }
  out
}

#[test]
fn live_deferred_persist_matches_synchronous_build() {
  let root = std::env::temp_dir().join(format!("vorpal-live-{}", std::process::id()));
  let _ = fs::remove_dir_all(&root);
  let src = root.join("src");
  corpus(&src);
  let out = root.join("idx");

  // Live: full pipeline with deferred persistence; the graph serves before persist runs.
  let build = vorpal_index::build_index_live(&src, &out, None, &Default::default()).expect("live build");
  let served = build.kg.expect("full pipeline hands the sealed graph back");
  let served_battery = battery_on(&served);
  let pending = build.pending.expect("full pipeline defers persistence");
  assert!(
    !out.join("CURRENT").exists(),
    "nothing may be committed before persist() runs"
  );
  let committed = pending.persist().expect("deferred persist");

  // The generation the deferred tail committed is the one CURRENT names…
  assert_eq!(
    committed,
    vorpal_kg::resolve_index_dir(&out),
    "persist() must return the committed generation"
  );
  // …its reloaded graph answers identically to the graph that served meanwhile…
  let reloaded = vorpal_kg::Kg::load(&committed).expect("load committed generation");
  assert_eq!(
    served_battery,
    battery_on(&reloaded),
    "served graph and persisted graph must answer identically"
  );
  assert_eq!(served.node_count(), reloaded.node_count());

  // …and a from-scratch synchronous build of the same tree commits the same generation.
  let out2 = root.join("idx2");
  vorpal_index::build_index(&src, &out2).expect("sync build of same tree");
  assert_eq!(
    gen_id(&out),
    gen_id(&out2),
    "deferred and synchronous builds of the same tree must commit identical generations"
  );
  let _ = fs::remove_dir_all(&root);
}

#[test]
fn live_fast_paths_commit_synchronously_and_hand_back_no_graph() {
  let root = std::env::temp_dir().join(format!("vorpal-live-fast-{}", std::process::id()));
  let _ = fs::remove_dir_all(&root);
  let src = root.join("src");
  corpus(&src);
  let out = root.join("idx");
  vorpal_index::build_index(&src, &out).expect("seed build");
  let before = gen_id(&out);

  // Unchanged tree: whole-tree fast path — reused, no graph, no pending, gen unchanged.
  let build = vorpal_index::build_index_live(&src, &out, None, &Default::default()).expect("noop live");
  assert!(build.report.reused);
  assert!(build.kg.is_none(), "fast path must not rebuild the graph");
  assert!(build.pending.is_none(), "fast path commits synchronously");
  assert_eq!(before, gen_id(&out));

  // Hinted single-file semantic change: under the bucketed default this COMPOSES —
  // committed synchronously, no in-RAM graph handed back (the cutoff/respan contract,
  // now covering the semantic classes too).
  fs::write(
    src.join("alpha.py"),
    "def alpha():\n    return beta() + 1\n\ndef beta():\n    return 2\n",
  )
  .unwrap();
  let hints: HashSet<PathBuf> = [src.join("alpha.py")].into_iter().collect();
  let build = vorpal_index::build_index_live(&src, &out, Some(&hints), &Default::default()).expect("hinted live");
  assert!(build.pending.is_none(), "a composed edit commits synchronously");
  assert!(build.kg.is_none(), "a composed edit hands back no graph");
  // A FILE ADDITION is outside every compose class — the full pipeline lane, with the
  // sealed graph and the deferred persist, stays pinned by it permanently.
  fs::write(src.join("newcomer.py"), "def newcomer():\n    return alpha()\n").unwrap();
  let hints: HashSet<PathBuf> = [src.join("newcomer.py")].into_iter().collect();
  let build = vorpal_index::build_index_live(&src, &out, Some(&hints), &Default::default()).expect("added live");
  let pending = build.pending.expect("a file addition runs the full pipeline");
  assert!(build.kg.is_some(), "the full pipeline hands the daemon its sealed graph");
  pending.persist().expect("persist hinted build");
  let out2 = root.join("idx2");
  vorpal_index::build_index(&src, &out2).expect("scratch build of changed tree");
  assert_eq!(
    gen_id(&out),
    gen_id(&out2),
    "hinted live build must converge to the scratch generation"
  );
  let _ = fs::remove_dir_all(&root);
}

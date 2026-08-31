//! Live differential (SUBSECOND.md Phase 3 gate): a watched daemon — serve-immediately
//! probe, deferred persistence, coalesced warms, all engaged — must answer every tool
//! exactly like a from-scratch build of the current tree, after every edit class.
//!
//! This is the oracle the overlay era will rely on: live internals may diverge from the
//! canonical artifacts (deferred commits today, delta overlays tomorrow), but rendered
//! answers never may. Convergence is detected with per-step marker symbols (the watcher is
//! asynchronous); stamp-preserving classes (touch, comment) assert equality immediately —
//! for them there is nothing to wait for, no answer is allowed to change at any point.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vorpal_mcp::Server;

fn call_tool(server: &mut Server, id: u64, tool: &str, args: Value) -> (String, bool) {
  let line = json!({
    "jsonrpc": "2.0", "id": id, "method": "tools/call",
    "params": {"name": tool, "arguments": args}
  })
  .to_string();
  let response = server
    .handle_line(&line)
    .expect("tool call gets a response");
  let response: Value = serde_json::from_str(&response).expect("valid JSON");
  let result = &response["result"];
  let text = result["content"][0]["text"].as_str().expect("text content");
  (text.to_owned(), result["isError"].as_bool().unwrap_or(true))
}

/// Rendered answers across the graph verbs + hybrid search, over live and dead probes.
fn battery(server: &mut Server, id: &mut u64, probes: &[&str]) -> String {
  let mut out = String::new();
  for name in probes {
    for tool in ["node", "callers", "references", "importers", "similar"] {
      *id += 1;
      let (text, _) = call_tool(server, *id, tool, json!({"name": name}));
      out.push_str(&format!("== {tool} {name}\n{text}\n"));
    }
  }
  for query in ["alpha chain", "marker step"] {
    *id += 1;
    let (text, _) = call_tool(server, *id, "search", json!({"query": query, "k": 5}));
    out.push_str(&format!("== search {query}\n{text}\n"));
  }
  out
}

/// Scratch oracle: index the tree into a fresh custom-location dir (no watch) and answer
/// the same battery through the very same server surface.
fn scratch_battery(src: &Path, scratch_root: &Path, step: usize, probes: &[&str]) -> String {
  let out = scratch_root.join(format!("scratch-{step}"));
  vorpal_index::build_index(src, &out).expect("scratch build");
  let mut server = Server::new(out);
  let mut id = 1_000_000;
  battery(&mut server, &mut id, probes)
}

fn wait_for_marker(server: &mut Server, id: &mut u64, marker: &str) {
  let deadline = Instant::now() + Duration::from_secs(30);
  loop {
    *id += 1;
    let (text, is_error) = call_tool(server, *id, "node", json!({"name": marker}));
    // Match the definition line, not the name alone — the "(no results for '<name>')"
    // message contains the name too.
    if !is_error && text.contains(&format!("{marker} [Function]")) {
      return;
    }
    assert!(
      Instant::now() < deadline,
      "daemon never converged on {marker}; last: {text}"
    );
    std::thread::sleep(Duration::from_millis(50));
  }
}

const PROBES: &[&str] = &[
  "alpha", "beta", "beta2", "gamma", "probe_1", "probe_2", "probe_6", "probe_7", "probe_8a",
  "probe_8b", "probe_10", "broken_fn", "chain_probe", "maker", "render", "sim_probe_a",
  "sim_probe_b", "probe_11", "probe_12",
];

#[test]
fn watched_daemon_answers_equal_scratch_after_every_edit_class() {
  let base = std::env::temp_dir().join(format!("vorpal-live-diff-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  let scratch_root = base.join("oracle");
  fs::create_dir_all(&src).unwrap();
  fs::create_dir_all(&scratch_root).unwrap();
  // The daemon rebuilds under the CANONICAL source root (its watcher resolves symlinks —
  // /var vs /private/var on macOS); the oracle must spell paths identically or every
  // rendered path, evidence id, and File-node embedding diverges for spelling alone.
  let src = src.canonicalize().unwrap();

  fs::write(
    src.join("a.py"),
    "def alpha():\n    return beta()\n",
  )
  .unwrap();
  fs::write(
    src.join("b.py"),
    "def beta():\n    return 1\n",
  )
  .unwrap();
  fs::write(
    src.join("c.py"),
    "from b import beta\n\ndef gamma():\n    return beta()\n",
  )
  .unwrap();
  // Flow-era corpus (G-M3/G-M5/v16): traceable call arguments with a keyword binding, a
  // declared-return-type chain (`maker().render()` resolves through the rets ledger), and
  // a near-clone pair past the 32-token signing floor — so the daemon's retained tier must
  // reproduce the bulk pipeline's data-flow, chain, and similar_to derivations to pass the
  // answer battery AND the generation-convergence pin below.
  fs::write(
    src.join("flow_chain.py"),
    "class Widget:\n    def render(self):\n        return 1\n\nclass Gadget:\n    def render(self):\n        return 2\n\ndef maker() -> Widget:\n    return Widget()\n\ndef chain_probe():\n    return maker().render()\n",
  )
  .unwrap();
  fs::write(
    src.join("flow_args.py"),
    "def sink(value, other=None):\n    return value\n\ndef feeder(k):\n    return sink(k, other=k)\n",
  )
  .unwrap();
  fs::write(
    src.join("flow_clones.py"),
    "def sim_probe_a(items, floor, ceiling):\n    total = 0\n    for item in items:\n        if item < floor:\n            total += floor\n        elif item > ceiling:\n            total += ceiling\n        else:\n            total += item\n    return total\n\ndef sim_probe_b(items, floor, ceiling):\n    total = 0\n    for item in items:\n        if item < floor:\n            total += floor\n        elif item > ceiling:\n            total += ceiling\n        else:\n            total += item + 1\n    return total\n",
  )
  .unwrap();
  let index: PathBuf = src.join(".vorpal").join("index");
  vorpal_index::build_index(&src, &index).expect("initial index");

  let mut daemon = Server::new(index.clone());
  let mut id = 0u64;
  let mut step = 0usize;

  // Semantic steps (a marker) poll to quiescence: the daemon may briefly serve the carried
  // ANN tier + digest-map overlay while its background warm rebuilds the fresh tier — a
  // documented, grounded approximation window for the `search` channel only. Stamp-
  // preserving steps (no marker) assert strictly and immediately: for them NO answer may
  // change at ANY point, window or no window.
  let converge_and_compare = |daemon: &mut Server, id: &mut u64, step: &mut usize, marker: Option<&str>| {
    let strict = marker.is_none();
    if let Some(marker) = marker {
      wait_for_marker(daemon, id, marker);
    }
    *step += 1;
    let scratch = scratch_battery(&src, &scratch_root, *step, PROBES);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
      let live = battery(daemon, id, PROBES);
      if live == scratch {
        return;
      }
      if strict || Instant::now() > deadline {
        assert_eq!(
          live, scratch,
          "live answers diverged from scratch at step {step}"
        );
      }
      std::thread::sleep(Duration::from_millis(200));
    }
  };

  // Baseline.
  converge_and_compare(&mut daemon, &mut id, &mut step, None);

  // 1: body edit + new function (semantic; live-adopt + deferred persist path).
  fs::write(
    src.join("a.py"),
    "def alpha():\n    return beta() + 1\n\ndef probe_1():\n    return alpha()\n",
  )
  .unwrap();
  converge_and_compare(&mut daemon, &mut id, &mut step, Some("probe_1"));

  // 2: new file.
  fs::write(src.join("f.py"), "def probe_2():\n    return beta()\n").unwrap();
  converge_and_compare(&mut daemon, &mut id, &mut step, Some("probe_2"));

  // 3: comment-only edit (restamp class; serve-immediately probe) — answers may never
  // change, at any point, so compare immediately and again after the canonicalization
  // window.
  let a_with_comment = fs::read_to_string(src.join("a.py")).unwrap() + "# stamp probe\n";
  fs::write(src.join("a.py"), a_with_comment).unwrap();
  std::thread::sleep(Duration::from_millis(300));
  converge_and_compare(&mut daemon, &mut id, &mut step, None);
  std::thread::sleep(Duration::from_millis(1200));
  converge_and_compare(&mut daemon, &mut id, &mut step, None);

  // 4: pure touch.
  let now = std::time::SystemTime::now();
  let file = fs::OpenOptions::new()
    .append(true)
    .open(src.join("b.py"))
    .unwrap();
  file.set_modified(now).unwrap();
  drop(file);
  std::thread::sleep(Duration::from_millis(300));
  converge_and_compare(&mut daemon, &mut id, &mut step, None);

  // 5: delete a file (probe_2 must vanish everywhere).
  fs::remove_file(src.join("f.py")).unwrap();
  fs::write(src.join("m5.py"), "def marker_5():\n    return 0\n").unwrap();
  converge_and_compare(&mut daemon, &mut id, &mut step, Some("marker_5"));

  // 6: rename a hub symbol (dirty-bucket class: every beta caller re-resolves).
  fs::write(src.join("b.py"), "def beta2():\n    return 2\n").unwrap();
  fs::write(
    src.join("a.py"),
    "def alpha():\n    return beta2() + 1\n\ndef probe_1():\n    return alpha()\n\ndef probe_6():\n    return beta2()\n",
  )
  .unwrap();
  converge_and_compare(&mut daemon, &mut id, &mut step, Some("probe_6"));

  // 7: import edit (import-binding dependents).
  fs::write(
    src.join("c.py"),
    "from b import beta2\n\ndef gamma():\n    return beta2()\n\ndef probe_7():\n    return gamma()\n",
  )
  .unwrap();
  converge_and_compare(&mut daemon, &mut id, &mut step, Some("probe_7"));

  // 8: rapid two-file burst (hint batching + pending-persist drain ordering). The daemon
  // must stay live mid-burst — answer between the writes, no waiting.
  fs::write(src.join("g.py"), "def probe_8a():\n    return alpha()\n").unwrap();
  let (_, is_error) = {
    id += 1;
    call_tool(&mut daemon, id, "callers", json!({"name": "beta2"}))
  };
  assert!(!is_error, "daemon must stay live mid-burst");
  fs::write(src.join("h.py"), "def probe_8b():\n    return probe_8a()\n").unwrap();
  converge_and_compare(&mut daemon, &mut id, &mut step, Some("probe_8b"));

  // 9: a file with parse errors enters…
  fs::write(
    src.join("broken.py"),
    "def broken_fn(:\n    return ???\n",
  )
  .unwrap();
  fs::write(src.join("m9.py"), "def marker_9():\n    return 0\n").unwrap();
  converge_and_compare(&mut daemon, &mut id, &mut step, Some("marker_9"));

  // 10: …and is fixed.
  fs::write(
    src.join("broken.py"),
    "def broken_fn():\n    return 3\n\ndef probe_10():\n    return broken_fn()\n",
  )
  .unwrap();
  converge_and_compare(&mut daemon, &mut id, &mut step, Some("probe_10"));

  // 11: return-annotation retarget — the definition row is UNCHANGED (same name, kind,
  // export, owner), so candidate-set diffing sees nothing; only the retained rets-ledger
  // diff can dirty the chain, and `callers(render)`'s answer moves between classes.
  fs::write(
    src.join("flow_chain.py"),
    "class Widget:\n    def render(self):\n        return 1\n\nclass Gadget:\n    def render(self):\n        return 2\n\ndef maker() -> Gadget:\n    return Gadget()\n\ndef chain_probe():\n    return maker().render()\n\ndef probe_11():\n    return chain_probe()\n",
  )
  .unwrap();
  converge_and_compare(&mut daemon, &mut id, &mut step, Some("probe_11"));

  // 12: near-clone edit — the pair must survive re-sketching, and the retained tier's
  // pairing must keep matching the bulk pipeline's (id-value tie-breaks and all).
  fs::write(
    src.join("flow_clones.py"),
    "def sim_probe_a(items, floor, ceiling):\n    total = 0\n    for item in items:\n        if item < floor:\n            total += floor\n        elif item > ceiling:\n            total += ceiling\n        else:\n            total += item\n    return total\n\ndef sim_probe_b(items, floor, ceiling):\n    total = 0\n    for item in items:\n        if item < floor:\n            total += floor\n        elif item > ceiling:\n            total += ceiling\n        else:\n            total += item + 2\n    return total\n\ndef probe_12():\n    return sim_probe_a([], 0, 1)\n",
  )
  .unwrap();
  converge_and_compare(&mut daemon, &mut id, &mut step, Some("probe_12"));

  // Generation convergence (the retained-persist pin): once the daemon's background
  // committers land, its CURRENT must name the SAME content-addressed generation a
  // from-scratch build of the final tree commits — stamps, pack, evidence, manifest, all
  // of it, bit for bit. Steady queries nudge the reaps along while we poll.
  let scratch_out = base.join("final-scratch");
  vorpal_index::build_index(&src, &scratch_out).expect("final scratch build");
  let want = fs::read_to_string(scratch_out.join("CURRENT")).expect("scratch CURRENT");
  let deadline = Instant::now() + Duration::from_secs(30);
  loop {
    id += 1;
    let _ = call_tool(&mut daemon, id, "node", json!({"name": "alpha"}));
    let got = fs::read_to_string(index.join("CURRENT")).unwrap_or_default();
    if got == want {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "daemon generation never converged to scratch: got {got:?}, want {want:?}"
    );
    std::thread::sleep(Duration::from_millis(100));
  }

  let _ = fs::remove_dir_all(&base);
}

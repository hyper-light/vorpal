//! Live differential under the bucketed pack (P4.1, `VORPAL_FORMAT=next`): the watched
//! daemon's serve-immediately probe, retained tier, and deferred ServedPersist all run
//! against — and republish — the bucketed layout, and none of it may be visible in
//! answers: after a semantic edit the daemon must answer exactly like a from-scratch v2
//! build, and its background committer must land the SAME content-addressed generation
//! (the retained-persist pin), TOC and bucket files included.
//!
//! Own test binary: the format env var is process-global, and the flat-lane sibling
//! (`live_differential.rs`) must keep running with it unset.

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

fn battery(server: &mut Server, id: &mut u64, probes: &[&str]) -> String {
  let mut out = String::new();
  for name in probes {
    for tool in ["node", "callers", "references", "importers", "similar"] {
      *id += 1;
      let (text, _) = call_tool(server, *id, tool, json!({"name": name}));
      out.push_str(&format!("== {tool} {name}\n{text}\n"));
    }
  }
  for query in ["alpha chain", "bucketed probe"] {
    *id += 1;
    let (text, _) = call_tool(server, *id, "search", json!({"query": query, "k": 5}));
    out.push_str(&format!("== search {query}\n{text}\n"));
  }
  out
}

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

const PROBES: &[&str] = &["alpha", "beta", "gamma", "chain_probe", "sim_probe_a", "probe_v2"];

#[test]
fn watched_daemon_converges_under_bucketed_format() {
  // Process-global by design; this file is its own binary with exactly one test.
  unsafe { std::env::set_var("VORPAL_FORMAT", "next") };
  let base = std::env::temp_dir().join(format!("vorpal-live-diff-v2-{}", std::process::id()));
  let _ = fs::remove_dir_all(&base);
  let src = base.join("repo");
  let scratch_root = base.join("oracle");
  fs::create_dir_all(&src).unwrap();
  fs::create_dir_all(&scratch_root).unwrap();
  let src = src.canonicalize().unwrap();

  fs::write(src.join("a.py"), "def alpha():\n    return beta()\n").unwrap();
  fs::write(src.join("b.py"), "def beta():\n    return 1\n").unwrap();
  fs::write(
    src.join("c.py"),
    "from b import beta\n\ndef gamma():\n    return beta()\n",
  )
  .unwrap();
  // Flow-era corpus: the retained tier must reproduce chains and similar pairs under v2
  // exactly as the flat-lane differential demands under v1.
  fs::write(
    src.join("flow_chain.py"),
    "class Widget:\n    def render(self):\n        return 1\n\ndef maker() -> Widget:\n    return Widget()\n\ndef chain_probe():\n    return maker().render()\n",
  )
  .unwrap();
  fs::write(
    src.join("flow_clones.py"),
    "def sim_probe_a(items, floor, ceiling):\n    total = 0\n    for item in items:\n        if item < floor:\n            total += floor\n        elif item > ceiling:\n            total += ceiling\n        else:\n            total += item\n    return total\n\ndef sim_probe_b(items, floor, ceiling):\n    total = 0\n    for item in items:\n        if item < floor:\n            total += floor\n        elif item > ceiling:\n            total += ceiling\n        else:\n            total += item + 1\n    return total\n",
  )
  .unwrap();
  let index: PathBuf = src.join(".vorpal").join("index");
  vorpal_index::build_index(&src, &index).expect("initial index");
  let boot_gen = vorpal_kg::resolve_index_dir(&index);
  assert!(
    boot_gen.join("products/toc.bin").is_file(),
    "the daemon boots on a bucketed generation"
  );

  let mut daemon = Server::new(index.clone());
  let mut id = 0u64;
  let mut step = 0usize;

  let converge_and_compare =
    |daemon: &mut Server, id: &mut u64, step: &mut usize, marker: Option<&str>| {
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
        if marker.is_none() || Instant::now() > deadline {
          assert_eq!(live, scratch, "live answers diverged from scratch at step {step}");
        }
        std::thread::sleep(Duration::from_millis(200));
      }
    };

  // Baseline: a v2-booted daemon answers exactly like a v2 scratch build.
  converge_and_compare(&mut daemon, &mut id, &mut step, None);

  // Semantic edit (probe + body change): the serve-immediately probe extracts against the
  // bucketed pack, the retained tier links, ServedPersist republishes bucketed artifacts.
  fs::write(
    src.join("a.py"),
    "def alpha():\n    return beta() + beta()\n\ndef probe_v2():\n    return alpha()\n",
  )
  .unwrap();
  converge_and_compare(&mut daemon, &mut id, &mut step, Some("probe_v2"));

  // The retained-persist pin, v2 edition: the daemon's background committer must land the
  // SAME content-addressed generation a from-scratch v2 build of the final tree commits —
  // bucket files, TOC, stamps, all of it — and that generation must BE bucketed.
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
      "daemon generation never converged to scratch under v2: got {got:?}, want {want:?}"
    );
    std::thread::sleep(Duration::from_millis(100));
  }
  assert!(
    vorpal_kg::resolve_index_dir(&index)
      .join("products/toc.bin")
      .is_file(),
    "the converged generation carries the bucketed pack"
  );

  let _ = fs::remove_dir_all(&base);
}

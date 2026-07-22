//! R0 differential gate (docs/REMOTE.md §7, §8): `vorpal <cmd> --remote loopback://` must equal a
//! local `vorpal <cmd>` over the same corpus, for every non-interactive printer, in **both**
//! execution modes (agent and stream), on a repo *with* a `.gitignore`.
//!
//! Equivalence is defined "after canonicalization", never raw byte compare: local order is already
//! nondeterministic (parallel walk → mpsc) and remoting adds interleaving. JSON/SARIF results are
//! parsed and sorted; text output is compared as the sorted set of lines.

use std::path::Path;
use std::process::Output;

use assert_cmd::Command;
use tempfile::TempDir;

/// Build a fixture repo whose scan/search results depend on `.gitignore` semantics being honored:
/// gitignored files and a nested-ignored dir contain matches that must NOT appear, so any
/// discovery drift (agent or stream) changes the result and fails the gate.
fn fixture() -> TempDir {
  let dir = TempDir::new().unwrap();
  let root = dir.path();
  // A .git dir so `require_git` (default) makes .gitignore active.
  std::fs::create_dir_all(root.join(".git")).unwrap();
  write(root, ".gitignore", "ignored/\n*.skip.rs\ntarget/\n");
  write(root, "src/main.rs", "fn main() {\n    let x = foo().unwrap();\n}\n");
  write(root, "src/lib.rs", "fn a() { one().unwrap(); }\nfn b() { two().unwrap(); }\n");
  write(root, "src/util.rs", "pub fn ok() -> i32 { 1 }\n"); // no match
  // These are gitignored — their matches must never surface.
  write(root, "ignored/hidden.rs", "fn z() { nope().unwrap(); }\n");
  write(root, "gen.skip.rs", "fn s() { skip().unwrap(); }\n");
  write(root, "target/build.rs", "fn t() { built().unwrap(); }\n");
  // A hidden file (default-skipped) with a match.
  write(root, ".secret.rs", "fn h() { hidden().unwrap(); }\n");
  dir
}

fn write(root: &Path, rel: &str, content: &str) {
  let p = root.join(rel);
  std::fs::create_dir_all(p.parent().unwrap()).unwrap();
  std::fs::write(p, content).unwrap();
}

const RULE: &str = "{id: no-unwrap, language: Rust, severity: error, message: no unwrap, rule: {pattern: $X.unwrap()}}";

fn run(dir: &Path, args: &[&str]) -> Output {
  Command::cargo_bin("vorpal")
    .unwrap()
    .current_dir(dir)
    .args(args)
    .output()
    .expect("vorpal runs")
}

/// Sort the lines of a text blob — the canonical form for colored/file-name output whose only
/// nondeterminism is per-file block ordering.
fn sorted_lines(bytes: &[u8]) -> Vec<String> {
  let s = String::from_utf8_lossy(bytes);
  let mut lines: Vec<String> = s.lines().map(str::to_owned).filter(|l| !l.is_empty()).collect();
  lines.sort();
  lines
}

/// Parse newline-delimited JSON (`--json=stream`) into a sorted multiset of canonical strings.
fn sorted_json_stream(bytes: &[u8]) -> Vec<String> {
  let s = String::from_utf8_lossy(bytes);
  let mut items: Vec<String> = s
    .lines()
    .filter(|l| !l.trim().is_empty())
    .map(|l| {
      let v: serde_json::Value = serde_json::from_str(l).expect("stream line is JSON");
      serde_json::to_string(&v).unwrap()
    })
    .collect();
  items.sort();
  items
}

/// Parse a JSON array (`--json` pretty/compact) into a sorted multiset of canonical strings.
fn sorted_json_array(bytes: &[u8]) -> Vec<String> {
  let v: serde_json::Value = serde_json::from_slice(bytes).expect("output is a JSON array");
  let mut items: Vec<String> = v
    .as_array()
    .expect("array")
    .iter()
    .map(|e| serde_json::to_string(e).unwrap())
    .collect();
  items.sort();
  items
}

/// Extract and sort SARIF results, dropping the tool/version envelope (identical either way).
fn sorted_sarif_results(bytes: &[u8]) -> Vec<String> {
  let v: serde_json::Value = serde_json::from_slice(bytes).expect("SARIF is JSON");
  let results = v["runs"][0]["results"].as_array().cloned().unwrap_or_default();
  let mut items: Vec<String> = results.iter().map(|r| serde_json::to_string(r).unwrap()).collect();
  items.sort();
  items
}

/// The core assertion: local and remote produce equal canonicalized stdout and equal exit codes.
fn assert_equiv(
  dir: &Path,
  base_args: &[&str],
  remote_extra: &[&str],
  normalize: fn(&[u8]) -> Vec<String>,
) {
  let local = run(dir, base_args);
  let mut remote_args = base_args.to_vec();
  remote_args.extend_from_slice(remote_extra);
  let remote = run(dir, &remote_args);

  let ln = normalize(&local.stdout);
  let rn = normalize(&remote.stdout);
  assert_eq!(
    ln, rn,
    "\n--- local {base_args:?} ---\n{}\n--- remote {remote_extra:?} ---\n{}\n",
    String::from_utf8_lossy(&local.stdout),
    String::from_utf8_lossy(&remote.stdout),
  );
  assert_eq!(
    local.status.code(),
    remote.status.code(),
    "exit codes diverge for {base_args:?} vs remote {remote_extra:?}"
  );
  // The gitignored matches must never appear — a discovery drift would leak them. (`.secret.rs`
  // is deliberately absent from this list: it is a hidden *.rs* file, and the language
  // type-filter whitelists it, which beats the hidden check in the `ignore` crate — so scan and
  // `run -l rs` legitimately include it, identically local and remote.)
  let out = String::from_utf8_lossy(&remote.stdout);
  for forbidden in ["hidden.rs", "gen.skip.rs", "build.rs"] {
    assert!(!out.contains(forbidden), "remote output leaked a gitignored file: {forbidden}");
  }
}

// --------------------------------------------------------------------------
// scan
// --------------------------------------------------------------------------

#[test]
fn scan_json_stream_agent_and_stream_equiv_local() {
  let dir = fixture();
  let base = ["scan", "--inline-rules", RULE, "--json=stream"];
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "agent"], sorted_json_stream);
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "stream"], sorted_json_stream);
}

#[test]
fn scan_json_pretty_agent_equiv_local() {
  let dir = fixture();
  let base = ["scan", "--inline-rules", RULE, "--json=pretty"];
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "agent"], sorted_json_array);
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "stream"], sorted_json_array);
}

#[test]
fn scan_sarif_agent_and_stream_equiv_local() {
  let dir = fixture();
  let base = ["scan", "--inline-rules", RULE, "--format", "sarif"];
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "agent"], sorted_sarif_results);
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "stream"], sorted_sarif_results);
}

#[test]
fn scan_github_agent_equiv_local() {
  let dir = fixture();
  let base = ["scan", "--inline-rules", RULE, "--format", "github"];
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "agent"], sorted_lines);
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "stream"], sorted_lines);
}

#[test]
fn scan_colored_short_agent_equiv_local() {
  let dir = fixture();
  let base = ["scan", "--inline-rules", RULE, "--color", "never", "--report-style", "short"];
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "agent"], sorted_lines);
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "stream"], sorted_lines);
}

#[test]
fn scan_files_with_matches_agent_equiv_local() {
  let dir = fixture();
  let base = ["scan", "--inline-rules", RULE, "--files-with-matches", "--color", "never"];
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "agent"], sorted_lines);
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "stream"], sorted_lines);
}

// --------------------------------------------------------------------------
// run (search)
// --------------------------------------------------------------------------

#[test]
fn run_json_stream_agent_and_stream_equiv_local() {
  let dir = fixture();
  let base = ["run", "-p", "$X.unwrap()", "-l", "rs", "--json=stream"];
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "agent"], sorted_json_stream);
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "stream"], sorted_json_stream);
}

#[test]
fn run_plain_agent_equiv_local() {
  let dir = fixture();
  let base = ["run", "-p", "$X.unwrap()", "-l", "rs", "--color", "never", "--heading", "never"];
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "agent"], sorted_lines);
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "stream"], sorted_lines);
}

#[test]
fn run_inferred_lang_agent_equiv_local() {
  // No `-l`: the inferred-lang worker path (walks all files, infers per extension).
  let dir = fixture();
  let base = ["run", "-p", "$X.unwrap()", "--json=stream"];
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "agent"], sorted_json_stream);
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "stream"], sorted_json_stream);
}

// --------------------------------------------------------------------------
// knobs & guards
// --------------------------------------------------------------------------

#[test]
fn scan_max_results_caps_remotely() {
  // 4 total matches across main.rs (1), lib.rs (2 — a multi-match fragment), and the
  // type-whitelisted hidden .secret.rs (1). The cap holds exactly in both agent and stream modes:
  // the agent truncates locally and reports accurate per-fragment match_count, and the coordinator
  // claims that count into a global MaxItemCounter. An uncapped run establishes the true total.
  let dir = fixture();
  let uncapped = sorted_json_stream(
    &run(dir.path(), &["scan", "--inline-rules", RULE, "--json=stream", "--remote", "loopback://"]).stdout,
  )
  .len();
  assert_eq!(uncapped, 4, "sanity: fixture has 4 visible matches");
  for mode in ["agent", "stream"] {
    for (cap, want) in [("1", 1usize), ("2", 2), ("3", 3), ("4", 4), ("9", 4)] {
      let out = run(
        dir.path(),
        &[
          "scan", "--inline-rules", RULE, "--json=stream", "--max-results", cap, "--remote",
          "loopback://", "--remote-mode", mode,
        ],
      );
      let n = sorted_json_stream(&out.stdout).len();
      assert_eq!(n, want, "remote --max-results={cap} in {mode} mode must yield {want}, got {n}");
    }
  }
}

#[test]
fn globs_are_honored_remotely() {
  let dir = fixture();
  let base = ["scan", "--inline-rules", RULE, "--json=stream", "--globs", "src/lib.rs"];
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "agent"], sorted_json_stream);
  assert_equiv(dir.path(), &base, &["--remote", "loopback://", "--remote-mode", "stream"], sorted_json_stream);
}

#[test]
fn no_ignore_surfaces_gitignored_files_remotely() {
  // With --no-ignore vcs, the gitignored matches SHOULD appear — both local and remote.
  let dir = fixture();
  let base = ["scan", "--inline-rules", RULE, "--json=stream", "--no-ignore", "vcs", "--no-ignore", "hidden"];
  let local = run(dir.path(), &base);
  let mut agent = base.to_vec();
  agent.extend_from_slice(&["--remote", "loopback://", "--remote-mode", "agent"]);
  let remote = run(dir.path(), &agent);
  assert_eq!(sorted_json_stream(&local.stdout), sorted_json_stream(&remote.stdout));
  // Sanity: the gitignored/hidden matches are now present.
  let out = String::from_utf8_lossy(&remote.stdout);
  assert!(out.contains("hidden.rs") && out.contains("gen.skip.rs"));
}

#[test]
fn duplicate_loopback_targets_collapse_to_one_node() {
  let dir = fixture();
  let single = run(dir.path(), &["scan", "--inline-rules", RULE, "--json=stream", "--remote", "loopback://"]);
  let double = run(
    dir.path(),
    &["scan", "--inline-rules", RULE, "--json=stream", "--remote", "loopback://", "--remote", "loopback://"],
  );
  assert_eq!(
    sorted_json_stream(&single.stdout),
    sorted_json_stream(&double.stdout),
    "duplicate targets must not duplicate results"
  );
  assert_eq!(single.status.code(), double.status.code());
}

#[test]
fn user_input_errors_are_not_node_failures() {
  // A malformed --globs pattern must fail exactly like a local run (BuildGlobs), never as a
  // node death (RemoteIncomplete, exit 4).
  let dir = fixture();
  let local = run(dir.path(), &["scan", "--inline-rules", RULE, "--globs", "*.{rs"]);
  assert!(!local.status.success());
  for mode in ["agent", "stream"] {
    let remote = run(
      dir.path(),
      &["scan", "--inline-rules", RULE, "--globs", "*.{rs", "--remote", "loopback://", "--remote-mode", mode],
    );
    assert_eq!(
      local.status.code(),
      remote.status.code(),
      "bad-glob exit code must match local in {mode} mode"
    );
    assert_ne!(remote.status.code(), Some(4), "input error misclassified as node failure");
  }
}

#[test]
fn inline_rules_do_not_ship_project_utils() {
  // `--inline-rules` compiles against empty global utils locally; a project with a malformed
  // util YAML must not break the equivalent remote run.
  let dir = fixture();
  write(dir.path(), "vorpalconfig.yml", "ruleDirs: [rules]\nutilDirs: [utils]\n");
  write(dir.path(), "rules/.keep", "");
  write(dir.path(), "utils/broken.yml", "this is: [not, a, util\n");
  let base = ["scan", "--inline-rules", RULE, "--json=stream"];
  let local = run(dir.path(), &base);
  let mut remote_args = base.to_vec();
  remote_args.extend_from_slice(&["--remote", "loopback://"]);
  let remote = run(dir.path(), &remote_args);
  assert_eq!(local.status.code(), remote.status.code());
  assert_eq!(sorted_json_stream(&local.stdout), sorted_json_stream(&remote.stdout));
}

#[cfg(unix)]
#[test]
fn wedged_agent_binary_fails_the_node_instead_of_hanging() {
  use std::os::unix::fs::PermissionsExt;
  let dir = fixture();
  let stub = dir.path().join("stub.sh");
  std::fs::write(&stub, "#!/bin/sh\nsleep 30\n").unwrap();
  std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
  let out = Command::cargo_bin("vorpal")
    .unwrap()
    .current_dir(dir.path())
    .env("VORPAL_REMOTE_HANDSHAKE_TIMEOUT_MS", "400")
    .args([
      "scan", "--inline-rules", RULE, "--remote", "loopback://", "--agent-binary",
      stub.to_str().unwrap(),
    ])
    .timeout(std::time::Duration::from_secs(30))
    .output()
    .expect("must not hang");
  assert_eq!(out.status.code(), Some(4), "a wedged agent is an incomplete node (exit 4)");
}

#[test]
fn agent_wedged_after_handshake_is_reaped_by_the_read_deadline() {
  // A real agent that completes the handshake, then goes silent *with heartbeats off* must trip the
  // steady-state read deadline and be reaped (exit 4) — not hang the CLI. The stall (3 s) is far
  // longer than the read deadline (400 ms), so the coordinator kills the node well before it wakes.
  let dir = fixture();
  let out = Command::cargo_bin("vorpal")
    .unwrap()
    .current_dir(dir.path())
    .env("VORPAL_REMOTE_READ_TIMEOUT_MS", "400")
    .env("VORPAL_REMOTE_HEARTBEAT_MS", "0") // heartbeats disabled ⇒ true silence
    .env("VORPAL_AGENT_TEST_STALL_MS", "3000")
    .args(["scan", "--inline-rules", RULE, "--remote", "loopback://", "--remote-mode", "agent"])
    .timeout(std::time::Duration::from_secs(30))
    .output()
    .expect("must not hang");
  assert_eq!(out.status.code(), Some(4), "a steady-state wedge is an incomplete node (exit 4)");
}

#[test]
fn heartbeats_keep_a_slow_but_alive_agent_from_being_reaped() {
  // The same stall, but with heartbeats *on* and faster than the read deadline: the coordinator
  // must tolerate the quiet period (each heartbeat resets the deadline) and the job then completes
  // with output identical to local. Stall 1.2 s > read deadline 500 ms > heartbeat 80 ms.
  let dir = fixture();
  let base = ["scan", "--inline-rules", RULE, "--json=stream"];
  let local = run(dir.path(), &base);
  let remote = Command::cargo_bin("vorpal")
    .unwrap()
    .current_dir(dir.path())
    .env("VORPAL_REMOTE_READ_TIMEOUT_MS", "500")
    .env("VORPAL_REMOTE_HEARTBEAT_MS", "80")
    .env("VORPAL_AGENT_TEST_STALL_MS", "1200")
    .args(["scan", "--inline-rules", RULE, "--json=stream", "--remote", "loopback://", "--remote-mode", "agent"])
    .timeout(std::time::Duration::from_secs(30))
    .output()
    .expect("must not hang");
  assert_eq!(
    local.status.code(),
    remote.status.code(),
    "a heartbeating agent must not be reaped; exit codes should match local"
  );
  assert_eq!(
    sorted_json_stream(&local.stdout),
    sorted_json_stream(&remote.stdout),
    "output of a slow-but-alive agent must equal local"
  );
}

#[test]
fn interactive_and_stdin_are_rejected() {
  let dir = fixture();
  let out = run(dir.path(), &["scan", "--inline-rules", RULE, "--interactive", "--remote", "loopback://"]);
  assert!(!out.status.success(), "--interactive + --remote must be rejected");
  let out = run(dir.path(), &["scan", "--inline-rules", RULE, "--stdin", "--remote", "loopback://"]);
  assert!(!out.status.success(), "--stdin + --remote must be rejected");
}

#[test]
fn unknown_target_is_rejected() {
  let dir = fixture();
  let out = run(dir.path(), &["scan", "--inline-rules", RULE, "--remote", "ssh://nope"]);
  assert!(!out.status.success(), "R0 rejects non-loopback targets");
}

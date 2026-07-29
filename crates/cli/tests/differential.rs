//! Differential compatibility testing against a pinned upstream ast-grep binary
//! (IMPROVEMENTS 07-29 §1): vorpal's inherited structural surfaces must produce the same
//! answers as the upstream baseline recorded in `docs/UPSTREAM.md`, on shared fixtures.
//!
//! Env-gated: set `VORPAL_ASTGREP_BIN` to an ast-grep binary built from the pinned baseline
//! commit to activate the comparison. Without it the tests **skip loudly** (they print the
//! skip and pass) rather than fail on machines without the pin — CI provides the binary; a
//! laptop without one still runs the rest of the suite.
//!
//! Comparisons use `--json=pretty` output with the binary-specific noise normalized away
//! (nothing else): both tools run from the same working directory over the same fixture
//! tree, so paths, byte ranges, matched text, and metavariable captures must agree exactly.
//! Intentional divergences, when they exist, must be asserted here as fixtures — not waved
//! through as prose exceptions in the ledger.

use std::fs;
use std::path::Path;
use std::process::Command;

/// The pinned upstream binary, if the environment provides one.
fn upstream_bin() -> Option<String> {
  match std::env::var("VORPAL_ASTGREP_BIN") {
    Ok(bin) if !bin.is_empty() => Some(bin),
    _ => {
      eprintln!(
        "differential: VORPAL_ASTGREP_BIN not set — skipping upstream comparison \
         (set it to an ast-grep binary built from the ledger's pinned baseline)"
      );
      None
    }
  }
}

/// Run one binary with `args` in `dir`; returns `(exit_code, stdout)`. Exit 0 and 1 are both
/// legitimate (`1` is the shared ripgrep-style "no matches" convention in both tools);
/// anything else is a hard failure.
fn run_in(dir: &Path, bin: &str, args: &[&str]) -> (i32, String) {
  let output = Command::new(bin)
    .args(args)
    .current_dir(dir)
    .output()
    .unwrap_or_else(|err| panic!("spawn {bin}: {err}"));
  let code = output.status.code().unwrap_or(-1);
  assert!(
    code == 0 || code == 1,
    "{bin} {args:?} exited {code}:\n{}",
    String::from_utf8_lossy(&output.stderr)
  );
  (code, String::from_utf8(output.stdout).expect("utf8 stdout"))
}

/// Both engines on the same invocation: exit codes AND parsed JSON must agree — parsing means
/// formatting-only differences can neither mask nor fake a delta.
fn assert_json_parity(dir: &Path, upstream: &str, args: &[&str]) {
  let vorpal = env!("CARGO_BIN_EXE_vorpal");
  let (our_code, ours) = run_in(dir, vorpal, args);
  let (their_code, theirs) = run_in(dir, upstream, args);
  assert_eq!(
    our_code, their_code,
    "exit code diverged from the pinned upstream for {args:?}"
  );
  let ours: serde_json::Value = serde_json::from_str(&ours).expect("vorpal emitted JSON");
  let theirs: serde_json::Value = serde_json::from_str(&theirs).expect("upstream emitted JSON");
  assert_eq!(
    ours, theirs,
    "structural output diverged from the pinned upstream for {args:?}"
  );
}

fn fixture_tree(tag: &str) -> std::path::PathBuf {
  let dir = std::env::temp_dir().join(format!("vorpal-diff-{tag}-{}", std::process::id()));
  let _ = fs::remove_dir_all(&dir);
  fs::create_dir_all(dir.join("src")).unwrap();
  fs::write(
    dir.join("src/app.ts"),
    "export function greet(name: string): string {\n\
     \x20\x20console.log('greeting');\n\
     \x20\x20return `hi ${name}`;\n\
     }\n\
     export function twice(x: number): number {\n\
     \x20\x20console.log(x);\n\
     \x20\x20return x + x;\n\
     }\n",
  )
  .unwrap();
  fs::write(
    dir.join("src/lib.rs"),
    "pub fn alpha() -> u32 {\n    beta(1)\n}\n\
     pub fn beta(x: u32) -> u32 {\n    x + 1\n}\n\
     /* comment */ pub fn gamma() -> u32 {\n    beta(beta(2))\n}\n",
  )
  .unwrap();
  dir
}

/// Pattern `run` (the most contractual inherited surface): matches, ranges, and metavariable
/// captures must agree exactly with the pinned upstream.
#[test]
fn run_json_matches_pinned_upstream() {
  let Some(upstream) = upstream_bin() else { return };
  let dir = fixture_tree("run");
  for (pattern, lang) in [
    ("console.log($ARG)", "ts"),
    ("beta($X)", "rust"),
    ("function $NAME($$$PARAMS) { $$$BODY }", "ts"),
  ] {
    assert_json_parity(
      &dir,
      &upstream,
      &["run", "--pattern", pattern, "--lang", lang, "--json=pretty", "src"],
    );
  }
  let _ = fs::remove_dir_all(&dir);
}

/// Rule `scan`: the YAML rule model (kind + relational constraints) over the same tree.
#[test]
fn scan_json_matches_pinned_upstream() {
  let Some(upstream) = upstream_bin() else { return };
  let dir = fixture_tree("scan");
  // Intentional branding divergence, asserted as a fixture: vorpal discovers
  // `vorpalconfig.yml`, upstream discovers `sgconfig.yml` — identical contents.
  fs::write(dir.join("sgconfig.yml"), "ruleDirs:\n- rules\n").unwrap();
  fs::write(dir.join("vorpalconfig.yml"), "ruleDirs:\n- rules\n").unwrap();
  fs::create_dir_all(dir.join("rules")).unwrap();
  fs::write(
    dir.join("rules/no-console.yml"),
    "id: no-console\n\
     language: TypeScript\n\
     severity: warning\n\
     message: no console\n\
     rule:\n\
     \x20\x20pattern: console.log($A)\n\
     \x20\x20inside:\n\
     \x20\x20\x20\x20kind: function_declaration\n\
     \x20\x20\x20\x20stopBy: end\n",
  )
  .unwrap();
  assert_json_parity(&dir, &upstream, &["scan", "--json=pretty", "src"]);
  let _ = fs::remove_dir_all(&dir);
}

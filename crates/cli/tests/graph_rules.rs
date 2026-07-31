//! The IMPROVEMENTS #5 done-gate: a rename/migration rule with graph predicates rewrites
//! **only** references proven to target the selected symbol, and every unproven site comes
//! back as an auditable candidate on stderr instead of being edited.
//!
//! Two files call the same-named `p_shared()`. One file imported it from `p_amb1` (the
//! resolver binds the call `import-bound`, constrained grade, provably to p_amb1's
//! definition); the other has no import, so its call is a labelled blind tie (heuristic).
//! With `minimumGrade: constrained` + `resolvesTo: {name, path}`, the proven call is
//! rewritten and the tie is skipped with the resolution-level reason in the audit line.

use std::fs;
use std::path::Path;
use std::process::Command;

fn write(dir: &Path, name: &str, content: &str) {
  fs::write(dir.join(name), content).unwrap();
}

#[test]
fn rename_rewrites_only_proven_references_and_reports_the_rest() {
  let base = std::env::temp_dir().join(format!("vorpal-graphrule-{}", std::process::id()));
  let src = base.join("src");
  let idx = base.join("index");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();

  write(&src, "p_amb1.py", "def p_shared():\n    return 10\n");
  write(&src, "p_amb2.py", "def p_shared():\n    return 11\n");
  write(
    &src,
    "caller_proven.py",
    "from p_amb1 import p_shared\ndef use_it():\n    return p_shared()\n",
  );
  write(
    &src,
    "caller_blind.py",
    "def use_blind():\n    return p_shared()\n",
  );

  vorpal_index::build_index(&src, &idx).unwrap();

  let rule = base.join("rename.yml");
  fs::write(
    &rule,
    r#"id: rename-p-shared
language: python
rule:
  pattern: $F($$$A)
constraints:
  F:
    regex: ^p_shared$
fix: p_renamed($$$A)
graph:
  minimumGrade: constrained
  predicates:
    - capture: F
      resolvesTo:
        name: p_shared
        path: p_amb1.py
"#,
  )
  .unwrap();

  let vorpal = env!("CARGO_BIN_EXE_vorpal");
  let output = Command::new(vorpal)
    .arg("scan")
    .arg("--rule")
    .arg(&rule)
    .arg("--update-all")
    .arg("--index")
    .arg(&idx)
    .arg(&src)
    .output()
    .unwrap();
  let stderr = String::from_utf8_lossy(&output.stderr);

  let proven = fs::read_to_string(src.join("caller_proven.py")).unwrap();
  let blind = fs::read_to_string(src.join("caller_blind.py")).unwrap();
  assert!(
    proven.contains("p_renamed()"),
    "the import-proven call must be rewritten\nstderr: {stderr}\nfile: {proven}"
  );
  assert!(
    proven.contains("from p_amb1 import p_shared"),
    "the import statement is not a call and must not be touched: {proven}"
  );
  assert!(
    blind.contains("p_shared()") && !blind.contains("p_renamed"),
    "the unproven (tie) call must NOT be rewritten\nstderr: {stderr}\nfile: {blind}"
  );

  // The skipped site is an explicit, auditable candidate: file, span, and the
  // resolution-grade reason it was not rewritten.
  assert!(
    stderr.contains("caller_blind.py") && stderr.contains("not rewritten"),
    "the unproven site must be reported for audit\nstderr: {stderr}"
  );
  assert!(
    stderr.contains("heuristic"),
    "the audit line must carry the resolution grade\nstderr: {stderr}"
  );

  // The definitions themselves were never candidates (the pattern matches calls).
  assert!(
    fs::read_to_string(src.join("p_amb1.py")).unwrap().contains("def p_shared()"),
    "definitions stay untouched"
  );
  let _ = fs::remove_dir_all(&base);
}

#[test]
fn missing_index_errors_by_default_and_ignore_waves_through() {
  let base = std::env::temp_dir().join(format!("vorpal-graphrule-miss-{}", std::process::id()));
  let src = base.join("src");
  let _ = fs::remove_dir_all(&base);
  fs::create_dir_all(&src).unwrap();
  write(&src, "caller.py", "def use_blind():\n    return p_shared()\n");

  let rule = |require: &str| {
    format!(
      r#"id: rename-p-shared
language: python
rule:
  pattern: $F($$$A)
constraints:
  F:
    regex: ^p_shared$
graph:
  require: {require}
  predicates:
    - capture: F
      resolvesTo:
        name: p_shared
"#
    )
  };
  let vorpal = env!("CARGO_BIN_EXE_vorpal");

  // Default `require: error`: a rule that demands proofs fails loudly without an index.
  let rule_error = base.join("rule_error.yml");
  fs::write(&rule_error, rule("error")).unwrap();
  let missing = base.join("no-such-index");
  let output = Command::new(vorpal)
    .arg("scan")
    .arg("--rule")
    .arg(&rule_error)
    .arg("--index")
    .arg(&missing)
    .arg(&src)
    .output()
    .unwrap();
  assert!(
    !output.status.success(),
    "missing index with require:error must fail the scan"
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("graph predicates need an index"),
    "stderr must say why: {stderr}"
  );

  // `require: ignore`: the structural match stands on its own.
  let rule_ignore = base.join("rule_ignore.yml");
  fs::write(&rule_ignore, rule("ignore")).unwrap();
  let output = Command::new(vorpal)
    .arg("scan")
    .arg("--rule")
    .arg(&rule_ignore)
    .arg("--index")
    .arg(&missing)
    .arg(&src)
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "require:ignore scans structurally: {}",
    String::from_utf8_lossy(&output.stderr)
  );
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("caller.py"),
    "the structural match must be reported: {stdout}"
  );
  let _ = fs::remove_dir_all(&base);
}

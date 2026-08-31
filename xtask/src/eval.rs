//! `cargo xtask eval` — the agent-task evaluation harness (ADOPTION #29 / plan Phase E).
//!
//! A fixed suite of code-navigation questions over THIS repository, answered two ways:
//!
//! * **vorpal**: one CLI invocation against a freshly built index (the same library surface
//!   the MCP tools serve) — measuring invocations, bytes returned, wall time, and
//!   correctness against hand-labelled expectations.
//! * **baseline**: the file-exploration model — `grep -rn <term>` over the source tree,
//!   then "open" the first five distinct matched files (an agent must read them to answer).
//!   Bytes = grep output + those files' sizes; correct = the labelled answer file appears
//!   among the matches. The model is deliberately generous to the baseline: a real
//!   exploration loop greps more than once and reads more than it needs.
//!
//! Labels are substrings of stable facts of this codebase; a refactor that moves one is a
//! one-line fixture edit, loudly flagged by the failing expectation. `--write` regenerates
//! the marked section of docs/wip/BENCHMARKS.md so the published table always carries the
//! exact command that produced it.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};

struct Question {
  id: &'static str,
  ask: &'static str,
  /// `vorpal` CLI arguments (the index flag is appended).
  vorpal: &'static [&'static str],
  /// Every substring must appear in vorpal's stdout for the answer to count as correct.
  expect: &'static [&'static str],
  /// What a file-exploring agent would grep for.
  grep: &'static str,
  /// The labelled answer location the baseline must surface to count as correct.
  answer_file: &'static str,
}

const QUESTIONS: &[Question] = &[
  Question {
    id: "where-defined",
    ask: "Where is scc_sizes defined?",
    vorpal: &["graph", "node", "scc_sizes"],
    expect: &["crates/kg/src/scc.rs"],
    grep: "fn scc_sizes",
    answer_file: "crates/kg/src/scc.rs",
  },
  Question {
    id: "who-calls",
    ask: "Who calls load_arg_spill?",
    vorpal: &["graph", "callers", "load_arg_spill"],
    expect: &["pipeline.rs"],
    grep: "load_arg_spill",
    answer_file: "crates/ingest/src/pipeline.rs",
  },
  Question {
    id: "snippet",
    ask: "Show me the body of bind_param_index.",
    vorpal: &["graph", "snippet", "bind_param_index"],
    expect: &["fn bind_param_index", "NO_PARAM"],
    grep: "fn bind_param_index",
    answer_file: "crates/ingest/src/pipeline.rs",
  },
  Question {
    id: "type-users",
    ask: "What uses the DataflowRow type?",
    vorpal: &["graph", "typeusers", "DataflowRow"],
    expect: &["save_dataflow"],
    grep: "DataflowRow",
    answer_file: "crates/kg/src/dataflow.rs",
  },
  Question {
    id: "impact",
    ask: "What breaks if EdgeType changes?",
    vorpal: &["graph", "typeusers", "EdgeType"],
    expect: &["crates/graph", "crates/kg"],
    grep: "EdgeType",
    answer_file: "crates/graph/src/edge.rs",
  },
  Question {
    id: "data-flow",
    ask: "Which arguments flow into save_dataflow?",
    vorpal: &["graph", "flows", "link_writer_spilled_with_flows"],
    expect: &["--arg#"],
    grep: "save_dataflow(",
    answer_file: "crates/kg/src/dataflow.rs",
  },
  Question {
    id: "schema",
    ask: "What relations does this index contain?",
    vorpal: &["graph", "schema"],
    expect: &["calls", "data_flows", "imports"],
    grep: "EdgeType::",
    answer_file: "crates/graph/src/edge.rs",
  },
  Question {
    id: "hubs",
    ask: "What are the most-used symbols in the resolver crate?",
    vorpal: &[
      "query",
      "MATCH (f) WHERE f.path CONTAINS \"crates/resolve\" AND f.in_degree >= 20 \
       RETURN f.name, f.kind, f.in_degree ORDER BY f.in_degree DESC LIMIT 10",
    ],
    expect: &["resolve"],
    grep: "fn resolve",
    answer_file: "crates/resolve/src/resolver.rs",
  },
  Question {
    id: "reachable",
    ask: "What does commit_generation reach through calls, two hops out?",
    vorpal: &[
      "graph", "reachable", "commit_generation", "--direction", "out", "--relations", "calls",
      "--depth", "2",
    ],
    expect: &["generation_content_id"],
    grep: "commit_generation",
    answer_file: "crates/index/src/lib.rs",
  },
  Question {
    id: "search",
    ask: "Where is the bounded BFS with the edge budget?",
    vorpal: &["search", "bounded bfs edge budget", "-k", "5"],
    expect: &["crates/query"],
    grep: "bounded_bfs",
    answer_file: "crates/query/src/exec.rs",
  },
];

/// The baseline "agent" opens this many matched files after one grep.
const BASELINE_OPENS: usize = 5;

struct Outcome {
  calls: u32,
  bytes: u64,
  millis: u128,
  correct: bool,
}

pub fn run_eval(write_doc: bool) -> Result<()> {
  let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
  let repo = repo.canonicalize().context("repo root")?;
  let vorpal_bin = repo.join("target/release/vorpal");
  let index_bin = repo.join("target/release/vorpal-index");
  if !vorpal_bin.exists() || !index_bin.exists() {
    bail!("release binaries missing — run `cargo build --release -p vorpal -p vorpal-index` first");
  }

  // A fresh index in target/ (ignored by the walker of the repo itself via target/ ignore).
  let index_dir = repo.join("target/eval-index");
  let _ = std::fs::remove_dir_all(&index_dir);
  println!("indexing {} …", repo.display());
  let started = Instant::now();
  let status = Command::new(&index_bin)
    .arg("index")
    .arg(&repo)
    .arg(&index_dir)
    .stdout(std::process::Stdio::null())
    .status()
    .context("running vorpal-index")?;
  if !status.success() {
    bail!("index build failed");
  }
  println!("indexed in {:.2}s\n", started.elapsed().as_secs_f64());

  let mut rows: Vec<(&str, Outcome, Outcome)> = Vec::new();
  for q in QUESTIONS {
    println!("[{}] {}", q.id, q.ask);
    let vorpal = run_vorpal(&vorpal_bin, &index_dir, q)?;
    let baseline = run_baseline(&repo, q)?;
    rows.push((q.id, vorpal, baseline));
  }
  println!();

  let mut table = String::new();
  let _ = writeln!(
    table,
    "| Question | vorpal calls | bytes | ms | ok | baseline calls | bytes | ms | ok |"
  );
  let _ = writeln!(table, "|---|---:|---:|---:|---|---:|---:|---:|---|");
  let (mut vb, mut bb, mut vc, mut bc, mut vok, mut bok) = (0u64, 0u64, 0u32, 0u32, 0u32, 0u32);
  for (id, v, b) in &rows {
    let _ = writeln!(
      table,
      "| {id} | {} | {} | {} | {} | {} | {} | {} | {} |",
      v.calls,
      v.bytes,
      v.millis,
      if v.correct { "✓" } else { "✗" },
      b.calls,
      b.bytes,
      b.millis,
      if b.correct { "✓" } else { "✗" },
    );
    vb += v.bytes;
    bb += b.bytes;
    vc += v.calls;
    bc += b.calls;
    vok += v.correct as u32;
    bok += b.correct as u32;
  }
  let n = rows.len() as u32;
  let _ = writeln!(
    table,
    "| **total** | **{vc}** | **{vb}** | | **{vok}/{n}** | **{bc}** | **{bb}** | | **{bok}/{n}** |"
  );
  let _ = writeln!(
    table,
    "\nBytes an agent must read: baseline/vorpal = **{:.1}×** (baseline model: one grep + \
     opening the first {BASELINE_OPENS} matched files — generous; real exploration loops \
     grep repeatedly).",
    bb as f64 / vb.max(1) as f64
  );
  print!("{table}");

  // Loud correctness gate: the labels are part of the suite.
  if vok != n {
    for (id, v, _) in &rows {
      if !v.correct {
        eprintln!("FAILED expectation: {id}");
      }
    }
    bail!("{}/{n} vorpal answers met their labels", vok);
  }

  if write_doc {
    write_benchmarks_section(&repo, &table)?;
    println!("\nwrote docs/wip/BENCHMARKS.md eval section");
  }
  Ok(())
}

fn run_vorpal(bin: &Path, index_dir: &Path, q: &Question) -> Result<Outcome> {
  let started = Instant::now();
  let output = Command::new(bin)
    .args(q.vorpal)
    .arg("--index")
    .arg(index_dir)
    .output()
    .with_context(|| format!("running vorpal for {}", q.id))?;
  let millis = started.elapsed().as_millis();
  let stdout = String::from_utf8_lossy(&output.stdout);
  let correct = q.expect.iter().all(|needle| stdout.contains(needle));
  Ok(Outcome {
    calls: 1,
    bytes: output.stdout.len() as u64,
    millis,
    correct,
  })
}

fn run_baseline(repo: &Path, q: &Question) -> Result<Outcome> {
  let started = Instant::now();
  // One recursive grep over the crates tree (the shape an exploring agent starts with).
  let output = Command::new("grep")
    .args(["-rn", "--include=*.rs", q.grep])
    .arg(repo.join("crates"))
    .output()
    .with_context(|| format!("running grep for {}", q.id))?;
  let stdout = String::from_utf8_lossy(&output.stdout);
  let mut bytes = output.stdout.len() as u64;
  // The agent opens the first distinct matched files to actually read the code.
  let mut opened: Vec<&str> = Vec::new();
  for line in stdout.lines() {
    let Some((path, _)) = line.split_once(':') else {
      continue;
    };
    if !opened.contains(&path) {
      opened.push(path);
      if opened.len() >= BASELINE_OPENS {
        break;
      }
    }
  }
  for path in &opened {
    if let Ok(meta) = std::fs::metadata(path) {
      bytes += meta.len();
    }
  }
  let millis = started.elapsed().as_millis();
  let correct = stdout.contains(q.answer_file);
  Ok(Outcome {
    calls: 1 + opened.len() as u32,
    bytes,
    millis,
    correct,
  })
}

const BEGIN: &str = "<!-- BEGIN GENERATED EVAL TABLE -->";
const END: &str = "<!-- END GENERATED EVAL TABLE -->";

fn write_benchmarks_section(repo: &Path, table: &str) -> Result<()> {
  let path = repo.join("docs/wip/BENCHMARKS.md");
  let doc = std::fs::read_to_string(&path).context("docs/wip/BENCHMARKS.md")?;
  let (Some(start), Some(end)) = (doc.find(BEGIN), doc.find(END)) else {
    bail!("markers missing in BENCHMARKS.md — add {BEGIN} / {END}");
  };
  let rebuilt = format!(
    "{}{BEGIN}\n\n{}\n{}",
    &doc[..start],
    table.trim_end(),
    &doc[end..]
  );
  std::fs::write(&path, rebuilt)?;
  Ok(())
}

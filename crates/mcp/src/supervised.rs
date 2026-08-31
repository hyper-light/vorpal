//! Supervised index builds (D3): the daemon runs the indexer as a **child process**, so one
//! pathological file — a scanner segfault, a runaway allocation, an OOM kill — costs one build
//! attempt and an error string, never the server. The daemon's mmap'd graph keeps serving the
//! committed generation throughout (generation GC retains the prior generation precisely so a
//! live reader's files survive a concurrent commit); only the atomic `CURRENT` swap publishes
//! the child's work.
//!
//! Which binary: `VORPAL_INDEX_BIN` overrides; otherwise the daemon's own executable when it
//! IS an indexer-capable binary (`vorpal`, `vorpal-index`), else a `vorpal-index` sitting next
//! to it. The `vorpal` CLI flavor is spawned with the source tree as its working directory so
//! the child re-discovers `vorpalconfig.yml` exactly like an interactive `vorpal index` —
//! re-registering custom languages and rebuilding the same extraction environment (its own
//! one-shot dlopen, in its own process). When no candidate exists (library embedders, test
//! harnesses), the caller falls back to an in-process build and says so — supervision is
//! reported, never silently absent.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How the discovered binary spells "index this tree into that root".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndexerFlavor {
  /// `vorpal index <src> --out <out>` — project-aware (config discovery, custom languages).
  VorpalCli,
  /// `vorpal-index index <src> <out>` — the standalone tool, builtin grammars only.
  VorpalIndex,
}

/// The outcome of asking for a supervised build.
pub(crate) enum BuildOutcome {
  /// The child ran and committed; the string is its report (stdout tail).
  Supervised(String),
  /// No indexer binary could be discovered — the caller should build in-process and note it.
  Unavailable,
}

#[derive(Clone)]
pub(crate) struct Supervisor {
  candidate: Option<(PathBuf, IndexerFlavor)>,
  timeout: Duration,
}

impl Supervisor {
  pub(crate) fn discover() -> Self {
    let timeout = std::env::var("VORPAL_MCP_BUILD_TIMEOUT_S")
      .ok()
      .and_then(|v| v.parse::<u64>().ok())
      .filter(|&s| s > 0)
      .map(Duration::from_secs)
      .unwrap_or(Duration::from_secs(1800));
    Self {
      candidate: discover_candidate(),
      timeout,
    }
  }

  /// Run one supervised build of `src` into the index root `out`. `Ok(Unavailable)` means no
  /// child binary exists; a spawned child that fails or times out is an `Err` — the caller
  /// must NOT retry in-process (a crashing input would then take the daemon down, which is
  /// exactly what supervision exists to prevent).
  pub(crate) fn build(&self, src: &Path, out: &Path) -> Result<BuildOutcome, String> {
    let Some((program, flavor)) = &self.candidate else {
      return Ok(BuildOutcome::Unavailable);
    };
    // The child's cwd moves to `src` (config discovery); every path it receives is absolute.
    let src_abs = src
      .canonicalize()
      .map_err(|err| format!("supervised build: source {} unreadable: {err}", src.display()))?;
    let out_abs = absolutize(out);

    let mut cmd = Command::new(program);
    match flavor {
      IndexerFlavor::VorpalCli => {
        cmd.arg("index").arg(&src_abs).arg("--out").arg(&out_abs);
      }
      IndexerFlavor::VorpalIndex => {
        cmd.arg("index").arg(&src_abs).arg(&out_abs);
      }
    }
    cmd
      .current_dir(&src_abs)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped());

    let start = Instant::now();
    let mut child = cmd
      .spawn()
      .map_err(|err| format!("supervised build: spawning {} failed: {err}", program.display()))?;
    loop {
      match child.try_wait() {
        Ok(Some(status)) => {
          let mut stdout = String::new();
          let mut stderr = String::new();
          if let Some(mut pipe) = child.stdout.take() {
            let _ = pipe.read_to_string(&mut stdout);
          }
          if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
          }
          if status.success() {
            return Ok(BuildOutcome::Supervised(tail(&stdout, 12)));
          }
          return Err(format!(
            "supervised build failed ({status}) — the daemon and its current index are \
             unaffected. Indexer said:\n{}",
            tail(&stderr, 12)
          ));
        }
        Ok(None) => {
          if start.elapsed() > self.timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
              "supervised build exceeded {}s and was killed — the daemon and its current \
               index are unaffected",
              self.timeout.as_secs()
            ));
          }
          std::thread::sleep(Duration::from_millis(100));
        }
        Err(err) => {
          let _ = child.kill();
          let _ = child.wait();
          return Err(format!("supervised build: waiting on the child failed: {err}"));
        }
      }
    }
  }
}

fn discover_candidate() -> Option<(PathBuf, IndexerFlavor)> {
  if let Some(explicit) = std::env::var_os("VORPAL_INDEX_BIN") {
    let path = PathBuf::from(explicit);
    let flavor = match path.file_stem().and_then(|s| s.to_str()) {
      Some("vorpal") => IndexerFlavor::VorpalCli,
      _ => IndexerFlavor::VorpalIndex,
    };
    return Some((path, flavor));
  }
  let exe = std::env::current_exe().ok()?;
  match exe.file_stem().and_then(|s| s.to_str()) {
    Some("vorpal") => return Some((exe, IndexerFlavor::VorpalCli)),
    Some("vorpal-index") => return Some((exe, IndexerFlavor::VorpalIndex)),
    _ => {}
  }
  // A `vorpal-index` in the SAME directory only (never parent dirs: a test binary under
  // target/debug/deps must not discover target/debug/vorpal-index and start spawning it).
  let sibling = exe.parent()?.join("vorpal-index");
  sibling
    .is_file()
    .then_some((sibling, IndexerFlavor::VorpalIndex))
}

fn absolutize(path: &Path) -> PathBuf {
  if path.is_absolute() {
    path.to_path_buf()
  } else {
    std::env::current_dir()
      .map(|cwd| cwd.join(path))
      .unwrap_or_else(|_| path.to_path_buf())
  }
}

/// The last `n` non-empty lines — build reports end with the lines that matter.
fn tail(text: &str, n: usize) -> String {
  let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
  let start = lines.len().saturating_sub(n);
  lines[start..].join("\n")
}

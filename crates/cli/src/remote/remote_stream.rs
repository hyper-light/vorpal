//! Streaming mode over a non-loopback transport (docs/REMOTE.md §3.3).
//!
//! The node's filesystem is remote, so the coordinator (1) enumerates the node's tree + ignore
//! metadata over the transport, (2) reconstructs which files survive using the **exact matcher
//! core** the local walk uses (`discovery::reconstruct_snapshot` → `is_skipped`), so I1 *filtering*
//! parity is inherited, then (3) fetches the surviving files' content over the transport and runs
//! the real match pipeline on it. Only the *source* of the tree/content differs from loopback.
//!
//! Known bounds (surfaced, never silently wrong): paths containing newlines are not handled (the
//! enumeration is line-oriented); symlinks are not followed (matching the no-`--follow` default);
//! the node's global gitignore is approximated by the coordinator's. `--follow` over a remote
//! stream is refused rather than silently diverging.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use base64::Engine;
use tokio::io::AsyncReadExt;

use vorpal_transport::{ExecSpec, Transport};

use super::Target;
use super::discovery::{
  DirFacts, GlobalIgnore, RemoteSnapshot, RootKind, SnapEntry, WalkConfig, reconstruct_snapshot,
};
use super::producer::{
  self, SshDialOpts, StreamStep, StreamWorker, match_stream_content, worker_should_stop,
};
use crate::print::{Printer, WireFragment};
use crate::utils::ItemSink;

/// Hard cap on a single fetched file (defends the coordinator against a pathological huge file;
/// the real size/line gate is applied by the match pipeline on the fetched content).
const MAX_FETCH_BYTES: u64 = 512 * 1024 * 1024;

/// Reconstruct + stream one remote node: dial the transport, drive it, and close it. The
/// negotiating producer, which already holds a dialed+probed transport, calls
/// [`stream_over_transport`] directly to avoid re-dialing (and re-authenticating) the node.
pub async fn stream_remote<P>(
  target: &Target,
  _idx: usize,
  ssh: &SshDialOpts,
  worker: &StreamWorker,
  tx: &ItemSink<P::Processed>,
  processor: &P::Processor,
) -> Result<()>
where
  P: Printer,
  P::Processed: WireFragment,
{
  if matches!(target, Target::Loopback) {
    return Err(anyhow!("loopback stream is served locally, not here"));
  }
  // Dial through the shared dialer so SSH, k8s, and docker all stream identically (the agent
  // binary is irrelevant to streaming — the coordinator matches the node's bytes itself).
  let (transport, _label) = producer::dial_transport(target, 0, &None, ssh).await?;
  let res = stream_over_transport::<P>(&*transport, worker, tx, processor).await;
  let _ = transport.shutdown().await;
  res
}

/// Enumerate + reconstruct + fetch-and-match one node over an already-connected transport. The
/// transport's lifecycle (dial/shutdown) belongs to the caller — negotiation reuses the same
/// connection it probed the node on.
pub async fn stream_over_transport<P>(
  transport: &dyn Transport,
  worker: &StreamWorker,
  tx: &ItemSink<P::Processed>,
  processor: &P::Processor,
) -> Result<()>
where
  P: Printer,
  P::Processed: WireFragment,
{
  let (input, types, langs_types_all) = producer::stream_walk_inputs(worker)?;
  if input.input.follow {
    return Err(anyhow!(
      "--follow is not supported for stream mode over a remote transport (symlink-loop detection needs the node's inode identity); use --remote-mode agent"
    ));
  }
  let overrides = input.input.build_globs()?;
  let walk_ignore = crate::utils::NoIgnore::disregard(&input.input.no_ignore).effective();
  let cfg = WalkConfig::from_walk_ignore(
    overrides,
    if langs_types_all { None } else { Some(types) },
    walk_ignore,
  );

  for root in &input.input.paths {
    let root_str = root
      .to_str()
      .ok_or_else(|| anyhow!("non-UTF-8 root path cannot be sent to a remote node"))?;
    let snapshot = enumerate_remote(transport, root, root_str).await?;
    // The node's global gitignore is approximated by the coordinator's (`LocalMachine`); over
    // loopback/localhost they are identical.
    let files = reconstruct_snapshot(&snapshot, &cfg, &GlobalIgnore::LocalMachine);
    if fetch_and_match::<P>(transport, root_str, &files, worker, tx, processor).await? {
      break; // stop signal (consumer hung up / --max-results reached)
    }
  }
  Ok(())
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

/// The POSIX-sh enumeration script. Emits, for one root: kind, canonical dir, every dir/file/
/// symlink (relative to the root), `.git`/`.jj` markers, base64 ignore-file contents, git excludes,
/// and the ancestor chain with its ignore facts. Portable across BSD/GNU (no `-printf`).
const ENUM_SCRIPT: &str = r#"
R=$1
if [ -d "$R" ]; then
  printf 'K dir\n'
  C=$(cd "$R" 2>/dev/null && pwd -P)
  printf 'C %s\n' "$C"
  cd "$R" 2>/dev/null || { printf 'END\n'; exit 0; }
  find . -type d 2>/dev/null | while IFS= read -r p; do printf 'D %s\n' "$p"; done
  find . -type f 2>/dev/null | while IFS= read -r p; do printf 'F %s\n' "$p"; done
  find . -type l 2>/dev/null | while IFS= read -r p; do printf 'L %s\n' "$p"; done
  find . -name .git 2>/dev/null | while IFS= read -r p; do printf 'G %s\n' "$p"; done
  find . -name .jj 2>/dev/null | while IFS= read -r p; do printf 'J %s\n' "$p"; done
  find . -type f \( -name .gitignore -o -name .ignore \) 2>/dev/null | while IFS= read -r p; do
    printf 'I %s %s\n' "$p" "$(base64 < "$p" 2>/dev/null | tr -d '\n')"
  done
  find . -type d -name .git 2>/dev/null | while IFS= read -r p; do
    if [ -f "$p/info/exclude" ]; then printf 'X %s %s\n' "$p" "$(base64 < "$p/info/exclude" | tr -d '\n')"; fi
  done
  A="$C"
  while [ -n "$A" ] && [ "$A" != "/" ]; do
    A=$(dirname "$A")
    HG=0; { [ -e "$A/.git" ] || [ -e "$A/.jj" ]; } && HG=1
    printf 'A %s %s\n' "$A" "$HG"
    [ -f "$A/.gitignore" ] && printf 'AI %s %s\n' "$A" "$(base64 < "$A/.gitignore" | tr -d '\n')"
    [ -f "$A/.ignore" ] && printf 'AD %s %s\n' "$A" "$(base64 < "$A/.ignore" | tr -d '\n')"
    [ "$A" = "/" ] && break
  done
elif [ -e "$R" ]; then
  printf 'K file\n'
else
  printf 'K err\n'
fi
printf 'END\n'
"#;

async fn enumerate_remote(
  transport: &dyn Transport,
  root: &Path,
  root_str: &str,
) -> Result<RemoteSnapshot> {
  // `sh -c SCRIPT sh ROOT` — `$1` is the root (not interpolated into the script, so a weird root
  // path can't inject shell).
  let spec = ExecSpec::shell(format!(
    "sh -c {} sh {}",
    vorpal_transport_shell_quote(ENUM_SCRIPT),
    vorpal_transport_shell_quote(root_str)
  ));
  let out = transport
    .exec_capture(&spec)
    .await
    .map_err(|e| anyhow!("remote enumeration failed: {e}"))?;
  if !out.status.success() {
    return Err(anyhow!("remote enumeration exited {:?}", out.status.code));
  }
  parse_snapshot(root, &out.stdout)
}

fn parse_snapshot(root: &Path, stdout: &[u8]) -> Result<RemoteSnapshot> {
  let text = String::from_utf8_lossy(stdout);
  let mut snap = RemoteSnapshot { root: root.to_path_buf(), ..Default::default() };
  // Collect entries and facts, then assemble.
  let mut dirs_seen: Vec<PathBuf> = Vec::new();
  let mut files: Vec<PathBuf> = Vec::new();
  let mut has_git: std::collections::BTreeSet<PathBuf> = Default::default();
  let mut gitignore: BTreeMap<PathBuf, String> = BTreeMap::new();
  let mut dot_ignore: BTreeMap<PathBuf, String> = BTreeMap::new();
  let mut git_exclude: BTreeMap<PathBuf, String> = BTreeMap::new();
  // ancestors, in emission order (parent-first)
  let mut ancestors: Vec<(PathBuf, DirFacts)> = Vec::new();
  let mut ancestor_ix: BTreeMap<PathBuf, usize> = BTreeMap::new();

  let b64 = base64::engine::general_purpose::STANDARD;
  let decode = |s: &str| -> Option<String> {
    b64.decode(s.as_bytes()).ok().and_then(|b| String::from_utf8(b).ok())
  };
  // rel = a find path like "./a/b" → "a/b"; "." → "" (the root).
  let rel_of = |p: &str| -> PathBuf {
    let p = p.strip_prefix("./").unwrap_or(p);
    if p == "." { PathBuf::new() } else { PathBuf::from(p) }
  };

  for line in text.lines() {
    let Some((tag, rest)) = line.split_once(' ').or(Some((line, ""))) else { continue };
    match tag {
      "K" => {
        snap.kind = match rest {
          "dir" => RootKind::Dir,
          "file" => RootKind::File,
          _ => RootKind::Error("remote root missing or unreadable".into()),
        };
      }
      "C" => snap.canonical = Some(PathBuf::from(rest)),
      "D" => dirs_seen.push(rel_of(rest)),
      "F" => files.push(rel_of(rest)),
      "L" => { /* unfollowed symlink: neither dir nor file → never emitted (no-follow default) */ }
      "G" | "J" => {
        // marker path like "./sub/.git" → the dir containing it.
        if let Some(dir) = rel_of(rest).parent() {
          has_git.insert(dir.to_path_buf());
        }
      }
      "I" => {
        if let Some((p, data)) = rest.split_once(' ') {
          let rp = rel_of(p);
          let dir = rp.parent().unwrap_or(Path::new("")).to_path_buf();
          if let Some(content) = decode(data) {
            match rp.file_name().and_then(|n| n.to_str()) {
              Some(".gitignore") => {
                gitignore.insert(dir, content);
              }
              Some(".ignore") => {
                dot_ignore.insert(dir, content);
              }
              _ => {}
            }
          }
        }
      }
      "X" => {
        if let Some((p, data)) = rest.split_once(' ') {
          // p is the ".git" dir; the exclude belongs to its parent dir.
          if let (Some(dir), Some(content)) = (rel_of(p).parent().map(Path::to_path_buf), decode(data)) {
            git_exclude.insert(dir, content);
          }
        }
      }
      "A" => {
        if let Some((dir, hg)) = rest.rsplit_once(' ') {
          let dir = PathBuf::from(dir);
          let facts = DirFacts { has_git: hg == "1", ..Default::default() };
          ancestor_ix.insert(dir.clone(), ancestors.len());
          ancestors.push((dir, facts));
        }
      }
      "AI" | "AD" | "AX" => {
        if let Some((dir, data)) = rest.split_once(' ') {
          let dir = PathBuf::from(dir);
          if let (Some(&ix), Some(content)) = (ancestor_ix.get(&dir), decode(data)) {
            match tag {
              "AI" => ancestors[ix].1.gitignore = Some(content),
              "AD" => ancestors[ix].1.dot_ignore = Some(content),
              _ => ancestors[ix].1.git_exclude = Some(content),
            }
          }
        }
      }
      _ => {}
    }
  }

  // Assemble dirs facts (root "" always present).
  let mut all_dirs: std::collections::BTreeSet<PathBuf> = dirs_seen.iter().cloned().collect();
  all_dirs.insert(PathBuf::new());
  for d in &all_dirs {
    snap.dirs.insert(
      d.clone(),
      DirFacts {
        has_git: has_git.contains(d),
        gitignore: gitignore.get(d).cloned(),
        dot_ignore: dot_ignore.get(d).cloned(),
        git_exclude: git_exclude.get(d).cloned(),
      },
    );
  }
  snap.ancestors = ancestors;

  // Assemble children map (parent dir → entries). Every dir and file is a child of its parent.
  let mut children: BTreeMap<PathBuf, Vec<SnapEntry>> = BTreeMap::new();
  for d in &all_dirs {
    if d.as_os_str().is_empty() {
      continue; // the root is not a child of anything
    }
    let parent = d.parent().unwrap_or(Path::new("")).to_path_buf();
    children.entry(parent).or_default().push(SnapEntry {
      rel: d.clone(),
      is_dir: true,
      is_file: false,
      hidden: is_hidden_rel(d),
    });
  }
  for f in &files {
    let parent = f.parent().unwrap_or(Path::new("")).to_path_buf();
    children.entry(parent).or_default().push(SnapEntry {
      rel: f.clone(),
      is_dir: false,
      is_file: true,
      hidden: is_hidden_rel(f),
    });
  }
  snap.children = children;
  Ok(snap)
}

/// Hidden = the basename starts with `.` (remote nodes are POSIX; the Windows attribute bit does
/// not apply). Mirrors `discovery::entry_is_hidden` on non-Windows.
fn is_hidden_rel(rel: &Path) -> bool {
  rel.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with('.')).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Content fetch + match
// ---------------------------------------------------------------------------

/// Fetch surviving files' content in one transport command (length-prefixed framing, so arbitrary
/// bytes are safe) and run the match pipeline on each. Returns `true` on a stop signal.
async fn fetch_and_match<P>(
  transport: &dyn Transport,
  root_str: &str,
  files: &[PathBuf],
  worker: &StreamWorker,
  tx: &ItemSink<P::Processed>,
  processor: &P::Processor,
) -> Result<bool>
where
  P: Printer,
  P::Processed: WireFragment,
{
  if files.is_empty() {
    return Ok(false);
  }
  // The survivor paths are display paths (root-joined). Send them **relative to the root** so the
  // remote `cd $ROOT` resolves them; map back to display paths locally.
  let root_path = PathBuf::from(root_str);
  let mut rel_list = String::new();
  let mut display_by_rel: BTreeMap<String, PathBuf> = BTreeMap::new();
  for f in files {
    let rel = f.strip_prefix(&root_path).unwrap_or(f);
    let rel_str = rel.to_string_lossy().into_owned();
    rel_list.push_str(&rel_str);
    rel_list.push('\n');
    display_by_rel.insert(rel_str, f.clone());
  }

  // Reader: `cd $ROOT; while read p; do if -f: emit "F <size> <path>\n" then raw bytes. `wc -c`
  // pads with spaces on BSD/macOS, so keep only the digits.
  let script = format!(
    "cd {} 2>/dev/null || exit 0; while IFS= read -r p; do if [ -f \"$p\" ]; then sz=$(wc -c < \"$p\" | tr -dc '0-9'); printf 'F %s %s\\n' \"$sz\" \"$p\"; cat \"$p\"; fi; done",
    vorpal_transport_shell_quote(root_str)
  );
  let spec = ExecSpec::shell(script);
  let mut proc = transport
    .exec(&spec)
    .await
    .map_err(|e| anyhow!("remote content fetch failed: {e}"))?;
  let mut stdin = proc.stdin.take().ok_or_else(|| anyhow!("no stdin for content fetch"))?;
  {
    use tokio::io::AsyncWriteExt;
    stdin.write_all(rel_list.as_bytes()).await.map_err(|e| anyhow!("{e}"))?;
    stdin.flush().await.ok();
    stdin.shutdown().await.ok();
  }
  let mut stdout = proc.stdout.take().ok_or_else(|| anyhow!("no stdout for content fetch"))?;

  // Parse the length-prefixed stream.
  let mut reader = FramedReader::new(&mut stdout);
  while let Some((rel, size)) = reader.next_header().await? {
    if size > MAX_FETCH_BYTES {
      reader.skip(size).await?;
      continue;
    }
    let bytes = reader.take(size).await?;
    let Some(display) = display_by_rel.get(&rel) else {
      continue;
    };
    // Non-UTF-8 files are skipped exactly as the local reader would (it reads to String).
    let Ok(content) = String::from_utf8(bytes) else {
      continue;
    };
    // `warm_cache=false`: the file is on the node, not local, so there is no local product to bank.
    if let StreamStep::Stop =
      match_stream_content::<P>(worker, display, content, false, processor, tx)
    {
      let _ = proc.wait().await;
      return Ok(true);
    }
    if worker_should_stop(worker) {
      let _ = proc.wait().await;
      return Ok(true);
    }
  }
  let _ = proc.wait().await;
  Ok(false)
}

/// Reads the length-prefixed content stream: a header line `F <size> <relpath>`, then exactly
/// `<size>` raw bytes, repeated. Buffered over the async transport stream.
struct FramedReader<'a, R: tokio::io::AsyncRead + Unpin> {
  reader: &'a mut R,
  buf: Vec<u8>,
}

impl<'a, R: tokio::io::AsyncRead + Unpin> FramedReader<'a, R> {
  fn new(reader: &'a mut R) -> Self {
    Self { reader, buf: Vec::new() }
  }

  /// Ensure at least one full line is buffered; return the next line (without the newline), or None
  /// at clean EOF.
  async fn next_header(&mut self) -> Result<Option<(String, u64)>> {
    loop {
      if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = self.buf.drain(..=pos).collect();
        let line = &line[..line.len() - 1];
        let line = String::from_utf8_lossy(line);
        // "F <size> <relpath>"
        let mut parts = line.splitn(3, ' ');
        if parts.next() != Some("F") {
          continue;
        }
        let size: u64 = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let rel = parts.next().unwrap_or("").to_string();
        return Ok(Some((rel, size)));
      }
      let mut chunk = [0u8; 64 * 1024];
      let n = self.reader.read(&mut chunk).await.map_err(|e| anyhow!("{e}"))?;
      if n == 0 {
        return Ok(None);
      }
      self.buf.extend_from_slice(&chunk[..n]);
    }
  }

  async fn take(&mut self, size: u64) -> Result<Vec<u8>> {
    let size = size as usize;
    while self.buf.len() < size {
      let mut chunk = [0u8; 64 * 1024];
      let n = self.reader.read(&mut chunk).await.map_err(|e| anyhow!("{e}"))?;
      if n == 0 {
        break;
      }
      self.buf.extend_from_slice(&chunk[..n]);
    }
    let n = size.min(self.buf.len());
    Ok(self.buf.drain(..n).collect())
  }

  async fn skip(&mut self, size: u64) -> Result<()> {
    let mut remaining = size as usize;
    let drop_now = remaining.min(self.buf.len());
    self.buf.drain(..drop_now);
    remaining -= drop_now;
    while remaining > 0 {
      let mut chunk = [0u8; 64 * 1024];
      let n = self.reader.read(&mut chunk).await.map_err(|e| anyhow!("{e}"))?;
      if n == 0 {
        break;
      }
      remaining = remaining.saturating_sub(n);
    }
    Ok(())
  }
}

/// Single-quote for POSIX sh (transport crate's helper is private; this mirrors it).
fn vorpal_transport_shell_quote(s: &str) -> String {
  let mut out = String::with_capacity(s.len() + 2);
  out.push('\'');
  for ch in s.chars() {
    if ch == '\'' {
      out.push_str("'\\''");
    } else {
      out.push(ch);
    }
  }
  out.push('\'');
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::BTreeSet;

  use vorpal_transport::Redacted;
  use vorpal_transport::ssh::{HostKeyVerifier, SshAuth, SshConfig, SshTransport};

  use super::super::discovery::reconstruct_local;
  use super::super::testserver;

  fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
  }

  /// A repo whose file set depends on gitignore semantics being honored end-to-end.
  fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    write(root, ".gitignore", "target/\n*.log\n!keep.log\nsub/deep.rs\n");
    write(root, "main.rs", "fn main() {}\n");
    write(root, "a.log", "log\n");
    write(root, "keep.log", "kept\n");
    write(root, "target/build.rs", "ignored dir\n");
    write(root, "sub/deep.rs", "parent-ignored\n");
    write(root, "sub/ok.rs", "fn ok() {}\n");
    write(root, "sub/.gitignore", "local.rs\n");
    write(root, "sub/local.rs", "locally ignored\n");
    // nested repo boundary
    std::fs::create_dir_all(root.join("nested/.git")).unwrap();
    write(root, "nested/.gitignore", "inner.rs\n");
    write(root, "nested/inner.rs", "inner ignored\n");
    write(root, "nested/outer.log", "outer *.log stops at the boundary\n");
    dir
  }

  fn cfg() -> WalkConfig {
    let walk_ignore = crate::utils::NoIgnore::disregard(&[]).effective();
    WalkConfig::from_walk_ignore(ignore::overrides::Override::empty(), None, walk_ignore)
  }

  #[tokio::test]
  async fn remote_enumeration_reconstructs_the_same_file_set_as_the_local_walk() {
    let dir = fixture();
    // Canonicalize so the local walk and the remote `pwd -P` agree on the root (macOS /var
    // symlinks would otherwise differ).
    let root = dir.path().canonicalize().unwrap();
    let root_str = root.to_str().unwrap().to_string();

    let (addr, pubkey) = testserver::start().await;
    let transport = SshTransport::connect(SshConfig {
      host: addr.ip().to_string(),
      port: addr.port(),
      auth: SshAuth::Password { user: "t".into(), password: Redacted("x".into()) },
      verifier: HostKeyVerifier::Pinned(vec![pubkey]),
    })
    .await
    .expect("connect");

    let snapshot = enumerate_remote(&transport, &root, &root_str).await.expect("enumerate");
    let cfg = cfg();
    let remote: BTreeSet<PathBuf> =
      reconstruct_snapshot(&snapshot, &cfg, &GlobalIgnore::LocalMachine).into_iter().collect();
    let local: BTreeSet<PathBuf> =
      reconstruct_local(&root, false, &cfg, &GlobalIgnore::LocalMachine).unwrap().into_iter().collect();

    assert_eq!(
      remote, local,
      "\nremote-enumerated file set diverges from the local walk\nremote={remote:#?}\nlocal={local:#?}"
    );
    // Sanity: gitignored files are absent, kept files present.
    assert!(local.iter().any(|p| p.ends_with("main.rs")));
    assert!(local.iter().any(|p| p.ends_with("keep.log")));
    assert!(!local.iter().any(|p| p.ends_with("build.rs")), "target/ must be ignored");
  }

  fn scan_worker(root: &Path, rule_yaml: &str) -> std::sync::Arc<crate::scan::ScanWithConfig> {
    use vorpal_config::{CombinedScan, RuleCollection, Severity, from_yaml_string};
    use vorpal_language::SupportLang;
    let configs =
      RuleCollection::try_new(from_yaml_string(rule_yaml, &Default::default()).unwrap()).unwrap();
    let unused = CombinedScan::unused_config(Severity::Off, SupportLang::Rust.into());
    let nosup = CombinedScan::no_suppress_all_config(Severity::Off, SupportLang::Rust.into());
    let input = crate::utils::InputArgs {
      paths: vec![root.to_path_buf()],
      follow: false,
      no_ignore: vec![],
      stdin: false,
      globs: vec![],
      threads: 0,
    };
    let output = crate::utils::OutputArgs {
      interactive: false,
      update_all: false,
      files_with_matches: false,
      json: None,
      color: crate::print::ColorArg::Never,
      inspect: Default::default(),
    };
    let context = crate::utils::ContextArgs { before: 0, after: 0, context: 0 };
    let arg = crate::scan::ScanArg::for_remote_agent(input, output, context, None);
    std::sync::Arc::new(crate::scan::ScanWithConfig::from_remote_parts(
      arg,
      configs,
      unused,
      nosup,
      root.canonicalize().unwrap(),
    ))
  }

  /// Collect newline-delimited JSON fragments as a sorted multiset.
  fn sorted_json(frags: &[Vec<u8>]) -> Vec<String> {
    let mut v: Vec<String> = frags
      .iter()
      .flat_map(|b| String::from_utf8_lossy(b).lines().map(str::to_owned).collect::<Vec<_>>())
      .filter(|l| !l.trim().is_empty())
      .collect();
    v.sort();
    v
  }

  #[tokio::test]
  async fn stream_over_ssh_matches_loopback_stream_output() {
    use crate::print::{JSONPrinter, JsonStyle, Printer};
    use crate::utils::ItemSink;

    let dir = fixture();
    let root = dir.path().canonicalize().unwrap();

    // Add matchable content to a non-ignored and an ignored file.
    write(&root, "main.rs", "fn main() { a().unwrap(); }\n");
    write(&root, "sub/ok.rs", "fn ok() { b().unwrap(); c().unwrap(); }\n");
    write(&root, "target/build.rs", "fn t() { gone().unwrap(); }\n"); // gitignored
    let rule = "id: u\nlanguage: Rust\nrule: {pattern: $X.unwrap()}\n";

    // Loopback stream (≡ local, per R0) → reference fragments.
    let lb_worker = StreamWorker::Scan(scan_worker(&root, rule));
    let printer = JSONPrinter::new(std::io::sink(), JsonStyle::Stream);
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    producer::stream_local::<JSONPrinter<std::io::Sink>>(
      &lb_worker,
      &ItemSink::Unbounded(tx),
      &printer.get_processor(),
    )
    .unwrap();
    let loopback: Vec<Vec<u8>> = rx.try_iter().collect();

    // Stream over SSH → the same repo, enumerated + fetched over the transport.
    let (addr, pubkey) = testserver::start().await;
    let ssh_worker = StreamWorker::Scan(scan_worker(&root, rule));
    let printer2 = JSONPrinter::new(std::io::sink(), JsonStyle::Stream);
    let (tx2, rx2) = std::sync::mpsc::channel::<Vec<u8>>();
    let uri = super::super::SshUri { user: Some("t".into()), host: addr.ip().to_string(), port: addr.port() };
    let ssh = SshDialOpts { password_env: Some("VORPAL_TEST_SSH_PW".into()), ..Default::default() };
    // SAFETY: single-threaded test setup; the password is consumed immediately by the connect.
    unsafe { std::env::set_var("VORPAL_TEST_SSH_PW", "x") };
    let _ = &pubkey; // TOFU (AcceptAny) is used by connect_ssh; the pin is exercised elsewhere.
    stream_remote::<JSONPrinter<std::io::Sink>>(
      &Target::Ssh(uri),
      0,
      &ssh,
      &ssh_worker,
      &ItemSink::Unbounded(tx2),
      &printer2.get_processor(),
    )
    .await
    .expect("stream over ssh");
    let over_ssh: Vec<Vec<u8>> = rx2.try_iter().collect();

    assert_eq!(
      sorted_json(&over_ssh),
      sorted_json(&loopback),
      "stream-over-ssh output must equal loopback-stream output"
    );
    // Sanity: three unwrap matches (main.rs 1 + sub/ok.rs 2), gitignored target/ absent.
    let joined = String::from_utf8_lossy(&over_ssh.concat()).to_string();
    assert!(!joined.contains("build.rs"), "gitignored file must not be scanned");
    assert_eq!(sorted_json(&over_ssh).len(), 3);
  }

  /// `--remote-mode auto` against a node that cannot exec a pushed binary must **demote to stream**
  /// and still equal a local run (docs/REMOTE.md D1). We force the demotion deterministically with a
  /// policy that forbids push+exec, so `decide` returns `Stream` for the exec-capable bridge; the
  /// `NegotiatingProducer` then reuses the very connection it probed on to stream the node's files.
  #[test]
  fn auto_negotiation_demotes_to_stream_and_matches_local() {
    use crate::print::{JSONPrinter, JsonStyle, Printer};
    use crate::utils::{ItemSink, Produce};
    use producer::{NegotiatingProducer, NodeOutcomes};
    use vorpal_transport::RemotePolicy;

    use super::super::rules_wire::LangEnv;
    use super::super::spec::build_scan_job;

    // `produce()` builds its own runtime and `block_on`s, so it cannot run inside `#[tokio::test]`.
    // A kept-alive multi-thread runtime hosts the in-process SSH bridge; the producer's own runtime
    // dials it over a real localhost socket (cross-runtime is fine — it's a TCP connection).
    let setup = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let (addr, _pubkey) = setup.block_on(testserver::start());

    let dir = fixture();
    let root = dir.path().canonicalize().unwrap();
    write(&root, "main.rs", "fn main() { a().unwrap(); }\n");
    write(&root, "sub/ok.rs", "fn ok() { b().unwrap(); c().unwrap(); }\n");
    write(&root, "target/build.rs", "fn t() { gone().unwrap(); }\n"); // gitignored
    let rule = "id: u\nlanguage: Rust\nrule: {pattern: $X.unwrap()}\n";

    // Reference: loopback stream (≡ local, per R0).
    let lb_worker = StreamWorker::Scan(scan_worker(&root, rule));
    let printer = JSONPrinter::new(std::io::sink(), JsonStyle::Stream);
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    producer::stream_local::<JSONPrinter<std::io::Sink>>(
      &lb_worker,
      &ItemSink::Unbounded(tx),
      &printer.get_processor(),
    )
    .unwrap();
    let loopback: Vec<Vec<u8>> = rx.try_iter().collect();

    // Auto negotiation over the SSH bridge, with push+exec forbidden → forced stream demotion.
    let worker = scan_worker(&root, rule);
    let job = build_scan_job(&worker, &LangEnv::from_project(None), vec![]).unwrap();
    let stream_worker = StreamWorker::Scan(worker.clone());
    let uri = super::super::SshUri {
      user: Some("t".into()),
      host: addr.ip().to_string(),
      port: addr.port(),
    };
    let ssh = SshDialOpts { password_env: Some("VORPAL_TEST_SSH_PW_AUTO".into()), ..Default::default() };
    // SAFETY: single-threaded test setup; the password is consumed immediately by the connect.
    unsafe { std::env::set_var("VORPAL_TEST_SSH_PW_AUTO", "x") };
    let no_exec = RemotePolicy { allow_push_exec: false, ..Default::default() };
    let producer = NegotiatingProducer::<JSONPrinter<std::io::Sink>>::new(
      vec![Target::Ssh(uri)],
      job,
      stream_worker,
      None,
      ssh,
      NodeOutcomes::default(),
      None,
      None,
    )
    .with_policy(no_exec);

    let printer2 = JSONPrinter::new(std::io::sink(), JsonStyle::Stream);
    let (tx2, rx2) = std::sync::mpsc::channel::<Vec<u8>>();
    Box::new(producer)
      .produce(ItemSink::Unbounded(tx2), printer2.get_processor())
      .expect("negotiating produce");
    let auto: Vec<Vec<u8>> = rx2.try_iter().collect();

    assert_eq!(
      sorted_json(&auto),
      sorted_json(&loopback),
      "auto-demoted-to-stream output must equal loopback-stream output"
    );
    assert_eq!(sorted_json(&auto).len(), 3, "three unwrap matches, gitignored target/ excluded");
    let joined = String::from_utf8_lossy(&auto.concat()).to_string();
    assert!(!joined.contains("build.rs"), "gitignored file must not be scanned");
  }

  #[tokio::test]
  async fn remote_content_fetch_returns_file_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let root_str = root.to_str().unwrap().to_string();
    std::fs::write(root.join("a.txt"), b"alpha").unwrap();
    std::fs::write(root.join("b.txt"), b"bravo-bytes").unwrap();

    let (addr, pubkey) = testserver::start().await;
    let transport = SshTransport::connect(SshConfig {
      host: addr.ip().to_string(),
      port: addr.port(),
      auth: SshAuth::Password { user: "t".into(), password: Redacted("x".into()) },
      verifier: HostKeyVerifier::Pinned(vec![pubkey]),
    })
    .await
    .unwrap();

    // Fetch both files and collect (path, content) via a tiny harness mirroring fetch_and_match's
    // framing parse.
    let files = [root.join("a.txt"), root.join("b.txt")];
    let mut got = std::collections::BTreeMap::new();
    // A trailing newline on every entry: POSIX `read` drops a final line with no newline.
    let rel_list = files
      .iter()
      .map(|f| format!("{}\n", f.strip_prefix(&root).unwrap().to_string_lossy()))
      .collect::<String>();
    let script = format!(
      "cd {} || exit 0; while IFS= read -r p; do if [ -f \"$p\" ]; then sz=$(wc -c < \"$p\" | tr -dc '0-9'); printf 'F %s %s\\n' \"$sz\" \"$p\"; cat \"$p\"; fi; done",
      vorpal_transport_shell_quote(&root_str)
    );
    let mut proc = transport.exec(&ExecSpec::shell(script)).await.unwrap();
    let mut stdin = proc.stdin.take().unwrap();
    {
      use tokio::io::AsyncWriteExt;
      stdin.write_all(rel_list.as_bytes()).await.unwrap();
      stdin.shutdown().await.unwrap();
    }
    let mut stdout = proc.stdout.take().unwrap();
    let mut reader = FramedReader::new(&mut stdout);
    while let Some((rel, size)) = reader.next_header().await.unwrap() {
      let bytes = reader.take(size).await.unwrap();
      got.insert(rel, String::from_utf8(bytes).unwrap());
    }
    assert_eq!(got.get("a.txt").map(String::as_str), Some("alpha"));
    assert_eq!(got.get("b.txt").map(String::as_str), Some("bravo-bytes"));
  }
}

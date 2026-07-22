//! `vorpal-loader` — the stage-0 launcher pushed to a node (docs/REMOTE.md §2, §6).
//!
//! Flow: the coordinator pushes this tiny binary, execs it as
//! `vorpal-loader --pubkey <hex> -- <agent argv…>`, and streams `[LoaderHeader][payload]` on its
//! stdin. The loader verifies the Ed25519 signature + blake3 hash, then execs the agent **from a
//! memfd** (Linux) so the multi-MB agent binary never lands on disk — the agent inherits the same
//! stdin (now positioned exactly at the first wire frame), stdout, and environment, and speaks the
//! protocol as if it had been launched directly.
//!
//! Reads are unbuffered (a `BufReader` would greedily pull bytes past the payload — the agent's own
//! wire frames — into a buffer that `exec` then discards). Any verification failure exits non-zero
//! and execs nothing.

use std::io::{self, Read};
use std::process::ExitCode;

use vorpal_loader::{LoaderError, verify_and_extract, verifying_key_from_hex};

/// Ceiling on the streamed agent payload (defends against a malformed/hostile `payload_len`).
const MAX_AGENT_BYTES: u64 = 512 * 1024 * 1024;

fn main() -> ExitCode {
  match run() {
    Ok(()) => ExitCode::SUCCESS, // unreachable on success: exec replaces this process
    Err(msg) => {
      eprintln!("vorpal-loader: {msg}");
      ExitCode::FAILURE
    }
  }
}

fn run() -> Result<(), String> {
  let cli = parse_args().map_err(|e| e.to_string())?;
  let key = resolve_pubkey(cli.pubkey_hex.as_deref())?;

  // Read + verify the framed agent from the raw stdin fd (no buffering — see module docs).
  let agent = verify_and_extract(&mut RawStdin, &key, MAX_AGENT_BYTES).map_err(fmt_verify_err)?;

  // Hand off: exec the agent with the passed-through argv. Returns only on failure.
  let err = exec_agent(&agent, &cli.agent_argv);
  Err(format!("failed to exec verified agent: {err}"))
}

struct Cli {
  pubkey_hex: Option<String>,
  /// The agent's argv (everything after `--`); `agent_argv[0]` becomes the agent's `argv[0]`.
  agent_argv: Vec<String>,
}

fn parse_args() -> Result<Cli, LoaderError> {
  let mut pubkey_hex = None;
  let mut agent_argv = Vec::new();
  let mut args = std::env::args().skip(1);
  while let Some(a) = args.next() {
    match a.as_str() {
      "--pubkey" => pubkey_hex = args.next(),
      "--" => {
        agent_argv.extend(args.by_ref());
        break;
      }
      other => {
        return Err(LoaderError::Decompress(format!("unexpected argument `{other}` before `--`")));
      }
    }
  }
  if agent_argv.is_empty() {
    return Err(LoaderError::Decompress("no agent argv after `--`".into()));
  }
  Ok(Cli { pubkey_hex, agent_argv })
}

/// The trusted public key, from (in order) `--pubkey`, `$VORPAL_LOADER_PUBKEY`, or the key embedded
/// at build time via `VORPAL_LOADER_PUBKEY_HEX` (release.yml). Verification is mandatory — no key,
/// no exec.
fn resolve_pubkey(arg: Option<&str>) -> Result<ed25519_dalek::VerifyingKey, String> {
  let hex = arg
    .map(str::to_owned)
    .or_else(|| std::env::var("VORPAL_LOADER_PUBKEY").ok())
    .or_else(|| option_env!("VORPAL_LOADER_PUBKEY_HEX").map(str::to_owned))
    .ok_or_else(|| {
      "no signing public key (pass --pubkey, set VORPAL_LOADER_PUBKEY, or embed one at build time)"
        .to_string()
    })?;
  verifying_key_from_hex(&hex).ok_or_else(|| "invalid Ed25519 public key hex".to_string())
}

fn fmt_verify_err(e: LoaderError) -> String {
  e.to_string()
}

/// Unbuffered reader over stdin (fd 0), so `read_exact` consumes exactly the framed bytes and the
/// agent inherits the untouched remainder of the pipe.
struct RawStdin;

impl Read for RawStdin {
  fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    #[cfg(unix)]
    {
      let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
      if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
    }
    #[cfg(not(unix))]
    {
      io::stdin().read(buf)
    }
  }
}

// ---------------------------------------------------------------------------
// Zero-residue exec
// ---------------------------------------------------------------------------

/// Exec the verified agent, passing through `argv` and inheriting stdio+env. Returns only on
/// failure (on success the process image is replaced).
#[cfg(target_os = "linux")]
fn exec_agent(agent: &[u8], argv: &[String]) -> io::Error {
  // Preferred: memfd — the agent bytes live only in an anonymous in-memory file, so nothing ever
  // reaches disk (true zero *persistent* residue, §2).
  let e = memfd_exec(agent, argv);
  eprintln!("vorpal-loader: memfd exec failed ({e}); falling back to a tmpfs file");
  tempfile_exec(agent, argv)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn exec_agent(agent: &[u8], argv: &[String]) -> io::Error {
  // No memfd (e.g. macOS, for local testing): fall back to a temp file. Residue is non-zero here,
  // which is acceptable for the fallback — the memfd path is what runs on real (Linux) nodes.
  tempfile_exec(agent, argv)
}

#[cfg(not(unix))]
fn exec_agent(_agent: &[u8], _argv: &[String]) -> io::Error {
  io::Error::new(io::ErrorKind::Unsupported, "the loader only execs on unix nodes")
}

#[cfg(target_os = "linux")]
fn memfd_exec(agent: &[u8], argv: &[String]) -> io::Error {
  use std::ffi::CString;
  let name = c"vorpal-agent";
  let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
  if fd < 0 {
    return io::Error::last_os_error();
  }
  // Write the whole binary into the memfd.
  let mut off = 0usize;
  while off < agent.len() {
    let n = unsafe { libc::write(fd, agent[off..].as_ptr().cast(), agent.len() - off) };
    if n <= 0 {
      return io::Error::last_os_error();
    }
    off += n as usize;
  }
  let c_argv: Vec<CString> = argv.iter().filter_map(|a| CString::new(a.as_str()).ok()).collect();
  let mut argv_ptrs: Vec<*const libc::c_char> = c_argv.iter().map(|s| s.as_ptr()).collect();
  argv_ptrs.push(std::ptr::null());
  let c_env = env_cstrings();
  let mut env_ptrs: Vec<*const libc::c_char> = c_env.iter().map(|s| s.as_ptr()).collect();
  env_ptrs.push(std::ptr::null());
  // fexecve runs the in-memory image directly; returns only on error.
  unsafe {
    libc::fexecve(fd, argv_ptrs.as_ptr(), env_ptrs.as_ptr());
  }
  io::Error::last_os_error()
}

#[cfg(unix)]
fn tempfile_exec(agent: &[u8], argv: &[String]) -> io::Error {
  use std::os::unix::fs::PermissionsExt;
  use std::os::unix::process::CommandExt;
  // Prefer an exec-capable tmpfs; `TMPDIR`/`/tmp` otherwise.
  let dir = std::env::var_os("XDG_RUNTIME_DIR")
    .map(std::path::PathBuf::from)
    .filter(|d| d.exists())
    .unwrap_or_else(std::env::temp_dir);
  let path = dir.join(format!(".vorpal-agent-{}", std::process::id()));
  if let Err(e) = std::fs::write(&path, agent) {
    return e;
  }
  if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)) {
    return e;
  }
  let mut cmd = std::process::Command::new(&path);
  if let Some((a0, rest)) = argv.split_first() {
    cmd.arg0(a0);
    cmd.args(rest);
  }
  // Replaces the process image; returns only on failure.
  cmd.exec()
}

#[cfg(target_os = "linux")]
fn env_cstrings() -> Vec<std::ffi::CString> {
  std::env::vars()
    .filter_map(|(k, v)| std::ffi::CString::new(format!("{k}={v}")).ok())
    .collect()
}

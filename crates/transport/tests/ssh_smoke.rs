//! End-to-end SSH transport test against an **in-process russh server** — the hermetic stand-in
//! for "a VM" (docs/REMOTE.md R1 gate) on a machine without sshd. A real russh client
//! (`SshTransport`) connects to a real russh server over a real localhost TCP socket; the server
//! bridges each `exec` to a `sh -c` subprocess, so this exercises the whole client↔server path
//! plus the transport's channel↔pipe adapter and exit-status capture.

#![cfg(feature = "ssh")]

use std::collections::HashMap;
use std::sync::Arc;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId, CryptoVec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;

use vorpal_transport::ssh::{HostKeyVerifier, SshAuth, SshConfig, SshTransport};
use vorpal_transport::{ExecSpec, Redacted, Transport};

// ---------------------------------------------------------------------------
// A minimal exec-bridging SSH server for tests.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TestServer;

impl server::Server for TestServer {
  type Handler = TestHandler;
  fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> TestHandler {
    TestHandler { stdins: Arc::new(Mutex::new(HashMap::new())) }
  }
}

struct TestHandler {
  /// Per-channel subprocess stdin, so the `data` handler can feed the client's stdin to it.
  stdins: Arc<Mutex<HashMap<ChannelId, ChildStdin>>>,
}

impl server::Handler for TestHandler {
  type Error = russh::Error;

  async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
    Ok(Auth::Accept)
  }

  async fn channel_open_session(
    &mut self,
    _channel: Channel<Msg>,
    _session: &mut Session,
  ) -> Result<bool, Self::Error> {
    Ok(true)
  }

  async fn exec_request(
    &mut self,
    channel: ChannelId,
    data: &[u8],
    session: &mut Session,
  ) -> Result<(), Self::Error> {
    let cmd = String::from_utf8_lossy(data).to_string();
    let mut child = tokio::process::Command::new("sh")
      .arg("-c")
      .arg(&cmd)
      .stdin(std::process::Stdio::piped())
      .stdout(std::process::Stdio::piped())
      .stderr(std::process::Stdio::null())
      .spawn()
      .map_err(|_| russh::Error::Disconnect)?;
    let stdin = child.stdin.take().unwrap();
    self.stdins.lock().await.insert(channel, stdin);
    let mut stdout = child.stdout.take().unwrap();
    session.channel_success(channel)?;
    let handle = session.handle();
    tokio::spawn(async move {
      let mut buf = vec![0u8; 32 * 1024];
      loop {
        match stdout.read(&mut buf).await {
          Ok(0) | Err(_) => break,
          Ok(n) => {
            if handle.data(channel, CryptoVec::from(&buf[..n])).await.is_err() {
              break;
            }
          }
        }
      }
      let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(0) as u32;
      let _ = handle.exit_status_request(channel, code).await;
      let _ = handle.eof(channel).await;
      let _ = handle.close(channel).await;
    });
    Ok(())
  }

  async fn data(
    &mut self,
    channel: ChannelId,
    data: &[u8],
    _session: &mut Session,
  ) -> Result<(), Self::Error> {
    if let Some(stdin) = self.stdins.lock().await.get_mut(&channel) {
      let _ = stdin.write_all(data).await;
      let _ = stdin.flush().await;
    }
    Ok(())
  }

  async fn channel_eof(
    &mut self,
    channel: ChannelId,
    _session: &mut Session,
  ) -> Result<(), Self::Error> {
    // Client closed its stdin → drop the subprocess stdin so the command sees EOF.
    self.stdins.lock().await.remove(&channel);
    Ok(())
  }
}

/// Start the in-process server on a random localhost port; returns (addr, host public key).
async fn start_server() -> (std::net::SocketAddr, russh::keys::PublicKey) {
  let key = PrivateKey::random(&mut rand_core::OsRng, Algorithm::Ed25519).unwrap();
  let pubkey = key.public_key().clone();
  let config = Arc::new(server::Config {
    keys: vec![key],
    ..Default::default()
  });
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  tokio::spawn(async move {
    let mut server = TestServer;
    let _ = server.run_on_socket(config, &listener).await;
  });
  (addr, pubkey)
}

fn client_config(addr: std::net::SocketAddr, pubkey: russh::keys::PublicKey) -> SshConfig {
  SshConfig {
    host: addr.ip().to_string(),
    port: addr.port(),
    auth: SshAuth::Password { user: "tester".into(), password: Redacted("hunter2".into()) },
    verifier: HostKeyVerifier::Pinned(vec![pubkey]),
  }
}

#[tokio::test]
async fn ssh_exec_pipes_stdout_and_exit() {
  let (addr, pubkey) = start_server().await;
  let t = SshTransport::connect(client_config(addr, pubkey)).await.expect("connect");
  let out = t.exec_capture(&ExecSpec::shell("printf 'hello over ssh'".into())).await.unwrap();
  assert_eq!(String::from_utf8_lossy(&out.stdout), "hello over ssh");
  assert!(out.status.success());
}

#[tokio::test]
async fn ssh_exec_round_trips_stdin() {
  let (addr, pubkey) = start_server().await;
  let t = SshTransport::connect(client_config(addr, pubkey)).await.expect("connect");
  let mut proc = t.exec(&ExecSpec::shell("cat".into())).await.unwrap();
  let mut stdin = proc.stdin.take().unwrap();
  stdin.write_all(b"echo me back").await.unwrap();
  stdin.flush().await.unwrap();
  stdin.shutdown().await.unwrap();
  drop(stdin);
  let mut out = Vec::new();
  proc.stdout.take().unwrap().read_to_end(&mut out).await.unwrap();
  assert_eq!(out, b"echo me back");
  assert!(proc.wait().await.unwrap().success());
}

#[tokio::test]
async fn ssh_push_file_and_nonzero_exit() {
  let (addr, pubkey) = start_server().await;
  let t = SshTransport::connect(client_config(addr, pubkey)).await.expect("connect");
  let dir = tempfile::tempdir().unwrap();
  let dest = dir.path().join("pushed.bin");
  let mut body = &b"pushed-over-ssh"[..];
  t.push_file(dest.to_str().unwrap(), 0o600, 15, &mut body).await.unwrap();
  assert_eq!(std::fs::read(&dest).unwrap(), b"pushed-over-ssh");

  // A nonzero exit is reported as such.
  let out = t.exec_capture(&ExecSpec::shell("exit 7".into())).await.unwrap();
  assert_eq!(out.status.code, Some(7));
}

#[tokio::test]
async fn ssh_rejects_unpinned_host_key() {
  let (addr, _pubkey) = start_server().await;
  // Pin a DIFFERENT key than the server's → host-key check must reject the connection.
  let other = PrivateKey::random(&mut rand_core::OsRng, Algorithm::Ed25519).unwrap();
  let cfg = SshConfig {
    host: addr.ip().to_string(),
    port: addr.port(),
    auth: SshAuth::Password { user: "tester".into(), password: Redacted("x".into()) },
    verifier: HostKeyVerifier::Pinned(vec![other.public_key().clone()]),
  };
  assert!(SshTransport::connect(cfg).await.is_err(), "unpinned host key must be refused");
}

//! SSH transport via `russh` (docs/REMOTE.md §2): pure-Rust, rustls/ring, static-musl-clean.
//!
//! One TCP connection → one `client::Handle`; each `exec` opens a session channel with
//! `exec(false, cmd)` (**no PTY** — a TTY line discipline would corrupt the binary frame stream).
//! The channel's byte pipe is adapted to `AsyncRead`/`AsyncWrite` by a small pump task that also
//! captures the remote exit status. Host-key verification is enforced in the handler
//! (reject-unknown by default; TOFU/pinned are explicit opt-ins).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use russh::client::{self, Handle};
use russh::keys::{PrivateKeyWithHashAlg, PublicKey, load_secret_key};
use russh::{ChannelMsg, Disconnect};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

use crate::process::{ExecSpec, RemoteKiller, RemoteProcess};
use crate::{ExitStatus, NodeDescriptor, Transport, TransportError};

/// How the client decides whether to trust a server's host key.
#[derive(Clone)]
pub enum HostKeyVerifier {
  /// Accept any key (TOFU / test only). Logs a warning — never silent.
  AcceptAny,
  /// Accept only these keys (operator-pinned, or a test's known server key).
  Pinned(Vec<PublicKey>),
}

/// How the client authenticates to the server.
#[derive(Clone)]
pub enum SshAuth {
  Password { user: String, password: crate::Redacted<String> },
  /// A private key file (OpenSSH format), optionally passphrase-protected.
  KeyFile { user: String, path: PathBuf, passphrase: Option<crate::Redacted<String>> },
}

impl SshAuth {
  fn user(&self) -> &str {
    match self {
      SshAuth::Password { user, .. } | SshAuth::KeyFile { user, .. } => user,
    }
  }
}

/// Connection parameters for one SSH node.
#[derive(Clone)]
pub struct SshConfig {
  pub host: String,
  pub port: u16,
  pub auth: SshAuth,
  pub verifier: HostKeyVerifier,
}

struct ClientHandler {
  verifier: HostKeyVerifier,
}

impl client::Handler for ClientHandler {
  type Error = russh::Error;

  async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
    match &self.verifier {
      HostKeyVerifier::AcceptAny => {
        tracing::warn!("accepting UNVERIFIED ssh host key (TOFU) — pin the key to harden");
        Ok(true)
      }
      HostKeyVerifier::Pinned(keys) => Ok(keys.iter().any(|k| k == key)),
    }
  }
}

/// A live SSH connection to one node.
pub struct SshTransport {
  descriptor: NodeDescriptor,
  handle: Arc<Handle<ClientHandler>>,
}

impl SshTransport {
  /// Connect, verify the host key, and authenticate.
  pub async fn connect(config: SshConfig) -> Result<Self, TransportError> {
    let russh_config = Arc::new(client::Config::default());
    let handler = ClientHandler { verifier: config.verifier.clone() };
    let mut handle = client::connect(russh_config, (config.host.as_str(), config.port), handler)
      .await
      .map_err(|e| TransportError::connect(format!("{}:{} — {e}", config.host, config.port)))?;

    let authed = match &config.auth {
      SshAuth::Password { user, password } => handle
        .authenticate_password(user.clone(), password.expose().clone())
        .await
        .map_err(|e| TransportError::auth(e.to_string()))?,
      SshAuth::KeyFile { user, path, passphrase } => {
        let pass = passphrase.as_ref().map(|p| p.expose().as_str());
        let key = load_secret_key(path, pass)
          .map_err(|e| TransportError::auth(format!("cannot load key {}: {e}", path.display())))?;
        let hash = handle
          .best_supported_rsa_hash()
          .await
          .map_err(|e| TransportError::auth(e.to_string()))?
          .flatten();
        handle
          .authenticate_publickey(user.clone(), PrivateKeyWithHashAlg::new(Arc::new(key), hash))
          .await
          .map_err(|e| TransportError::auth(e.to_string()))?
      }
    };
    if !authed.success() {
      return Err(TransportError::auth(format!(
        "authentication rejected for user {}",
        config.auth.user()
      )));
    }

    Ok(Self {
      descriptor: NodeDescriptor { scheme: "ssh", address: format!("{}:{}", config.host, config.port) },
      handle: Arc::new(handle),
    })
  }
}

/// The SSH channel has no separate kill; closing the session takes the remote process with it.
struct SshKiller {
  handle: Arc<Handle<ClientHandler>>,
}

#[async_trait]
impl RemoteKiller for SshKiller {
  async fn kill(&self) {
    let _ = self.handle.disconnect(Disconnect::ByApplication, "coordinator killed session", "").await;
  }
}

#[async_trait]
impl Transport for SshTransport {
  fn descriptor(&self) -> &NodeDescriptor {
    &self.descriptor
  }

  async fn exec(&self, spec: &ExecSpec) -> Result<RemoteProcess, TransportError> {
    // Prefix env exports (the SSH server may restrict `set_env`, so exporting in the shell line is
    // the portable path); the secret token, if any, rides here — never as a world-readable argv.
    let mut line = String::new();
    for (k, v) in &spec.env {
      line.push_str(&format!("export {}={} ; ", k, crate::shell_quote(v)));
    }
    line.push_str(&spec.to_shell_line());

    let channel = self
      .handle
      .channel_open_session()
      .await
      .map_err(|e| TransportError::exec(format!("open channel: {e}")))?;
    channel
      .exec(true, line)
      .await
      .map_err(|e| TransportError::exec(format!("exec: {e}")))?;

    // Adapt the channel to duplex byte pipes so the coordinator sees plain AsyncRead/AsyncWrite,
    // and capture the exit status out of band.
    let (stdin_w, stdin_r) = tokio::io::duplex(256 * 1024);
    let (stdout_w, stdout_r) = tokio::io::duplex(256 * 1024);
    let (exit_tx, exit_rx) = oneshot::channel::<u32>();
    tokio::spawn(pump_channel(channel, stdin_r, stdout_w, exit_tx));

    let wait = Box::pin(async move {
      match exit_rx.await {
        Ok(code) => Ok(ExitStatus { code: Some(code as i32) }),
        // Channel closed without an explicit exit-status message — treat a clean close as success
        // (the wire `Done` frame is the authoritative signal for the agent session).
        Err(_) => Ok(ExitStatus { code: Some(0) }),
      }
    });
    let killer = Arc::new(SshKiller { handle: self.handle.clone() });
    Ok(RemoteProcess::new(
      Some(Box::new(stdin_w)),
      Some(Box::new(stdout_r)),
      None,
      wait,
      killer,
    ))
  }

  async fn shutdown(&self) -> Result<(), TransportError> {
    let _ = self.handle.disconnect(Disconnect::ByApplication, "", "").await;
    Ok(())
  }
}

/// Bidirectionally bridge one SSH session channel to duplex pipes: coordinator stdin → channel
/// data; channel data → coordinator stdout; channel exit status → `exit_tx`.
async fn pump_channel(
  channel: russh::Channel<client::Msg>,
  mut stdin_r: tokio::io::DuplexStream,
  mut stdout_w: tokio::io::DuplexStream,
  exit_tx: oneshot::Sender<u32>,
) {
  let (mut read_half, write_half) = channel.split();
  let mut code: Option<u32> = None;
  let mut stdin_open = true;
  let mut buf = vec![0u8; 64 * 1024];

  loop {
    tokio::select! {
      // Coordinator → remote stdin.
      r = stdin_r.read(&mut buf), if stdin_open => match r {
        Ok(0) => {
          let _ = write_half.eof().await;
          stdin_open = false;
        }
        Ok(n) => {
          if write_half.data(&buf[..n]).await.is_err() {
            break;
          }
        }
        Err(_) => {
          let _ = write_half.eof().await;
          stdin_open = false;
        }
      },
      // Remote → coordinator stdout, plus exit status.
      msg = read_half.wait() => match msg {
        Some(ChannelMsg::Data { data }) => {
          if stdout_w.write_all(&data).await.is_err() {
            break;
          }
        }
        Some(ChannelMsg::ExtendedData { data, .. }) => {
          // Remote stderr: relay to our stderr so the agent's diagnostics surface, matching the
          // inherited-stderr behavior of the loopback subprocess transport.
          let _ = tokio::io::stderr().write_all(&data).await;
        }
        Some(ChannelMsg::ExitStatus { exit_status }) => {
          code = Some(exit_status);
        }
        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
        Some(_) => {}
      },
    }
  }

  // Signal EOF to the coordinator's reader, then hand back the exit status.
  let _ = stdout_w.shutdown().await;
  let _ = exit_tx.send(code.unwrap_or(0));
}

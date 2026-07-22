//! An in-process SSH server that bridges each `exec` to a `sh -c` subprocess — the hermetic
//! stand-in for "a VM" (docs/REMOTE.md R1 gate) used by the remote-stream / SSH differential
//! tests. Test-only.

#![cfg(test)]

use std::collections::HashMap;
use std::sync::Arc;

use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId, CryptoVec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;

#[derive(Clone)]
struct BridgeServer;

impl server::Server for BridgeServer {
  type Handler = BridgeHandler;
  fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> BridgeHandler {
    BridgeHandler { stdins: Arc::new(Mutex::new(HashMap::new())) }
  }
}

struct BridgeHandler {
  stdins: Arc<Mutex<HashMap<ChannelId, ChildStdin>>>,
}

impl server::Handler for BridgeHandler {
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
    self.stdins.lock().await.remove(&channel);
    Ok(())
  }
}

/// Start the bridge server on a random localhost port; returns (addr, host public key).
pub async fn start() -> (std::net::SocketAddr, PublicKey) {
  let key = PrivateKey::random(&mut rand_core::OsRng, Algorithm::Ed25519).unwrap();
  let pubkey = key.public_key().clone();
  let config = Arc::new(server::Config { keys: vec![key], ..Default::default() });
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  tokio::spawn(async move {
    let mut server = BridgeServer;
    let _ = server.run_on_socket(config, &listener).await;
  });
  (addr, pubkey)
}

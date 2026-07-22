//! End-to-end tests of the `vorpal-loader` binary: it verifies a signed stream and execs the agent
//! from memory, passing through argv and leaving the trailing stdin (the agent's own wire frames)
//! intact — and refuses to exec anything whose signature does not verify.
//!
//! On Linux the exec is a memfd; on macOS (local dev) it falls back to a temp file. Either way the
//! observable contract — "the verified agent runs with the right argv and untouched stdin, or
//! nothing runs" — is identical, which is what these assert.

#![cfg(unix)]

use std::io::Write;
use std::process::{Command, Stdio};

use vorpal_loader::{Ed25519SigningKey, sign_payload, verifying_key_to_hex};

/// A shell "agent" that echoes its argv and then its stdin — standing in for the real agent so we
/// can observe both the passed-through argv and the untouched trailing pipe.
const AGENT_SCRIPT: &[u8] = b"#!/bin/sh\nprintf 'ARGV:%s\\n' \"$*\"\ncat\n";

#[test]
fn loader_verifies_then_execs_the_agent_with_argv_and_trailing_stdin() {
  let sk = Ed25519SigningKey::from_bytes(&[42u8; 32]);
  let pubkey_hex = verifying_key_to_hex(&sk.verifying_key());

  let mut stream = sign_payload(AGENT_SCRIPT, &sk, false);
  let trailer = b"TRAILING-WIRE-BYTES-FOR-THE-AGENT";
  stream.extend_from_slice(trailer);

  let mut child = Command::new(env!("CARGO_BIN_EXE_vorpal-loader"))
    .args(["--pubkey", &pubkey_hex, "--", "vorpal-agent", "alpha", "beta"])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn loader");
  child.stdin.take().unwrap().write_all(&stream).expect("write stream");
  let out = child.wait_with_output().expect("wait");

  let stdout = String::from_utf8_lossy(&out.stdout);
  assert!(out.status.success(), "loader/agent exited non-zero: {:?}\n{stdout}", out.status);
  assert!(stdout.contains("ARGV:alpha beta"), "agent must see its passed-through argv; got: {stdout:?}");
  assert!(
    stdout.contains("TRAILING-WIRE-BYTES-FOR-THE-AGENT"),
    "agent must read the untouched trailing stdin; got: {stdout:?}"
  );
}

#[test]
fn loader_refuses_a_stream_signed_by_the_wrong_key() {
  let trusted = Ed25519SigningKey::from_bytes(&[1u8; 32]);
  let attacker = Ed25519SigningKey::from_bytes(&[2u8; 32]);
  let pubkey_hex = verifying_key_to_hex(&trusted.verifying_key());

  // A hostile agent, signed by the attacker's key — must never run.
  let hostile = b"#!/bin/sh\necho SHOULD-NOT-RUN\n";
  let stream = sign_payload(hostile, &attacker, false);

  let mut child = Command::new(env!("CARGO_BIN_EXE_vorpal-loader"))
    .args(["--pubkey", &pubkey_hex, "--", "vorpal-agent"])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn loader");
  let _ = child.stdin.take().unwrap().write_all(&stream); // may break-pipe once it bails; fine
  let out = child.wait_with_output().expect("wait");

  assert!(!out.status.success(), "loader must exit non-zero on a bad signature");
  assert!(
    !String::from_utf8_lossy(&out.stdout).contains("SHOULD-NOT-RUN"),
    "the unverified agent must not execute"
  );
  assert!(
    String::from_utf8_lossy(&out.stderr).contains("signature"),
    "the failure must name the signature check"
  );
}

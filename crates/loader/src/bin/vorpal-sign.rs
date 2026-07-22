//! `vorpal-sign` — the release/coordinator-side signing helper for the stage-0 loader
//! (docs/REMOTE.md §6, `release.yml`). Ed25519-signs an agent binary over its blake3 hash so the
//! pushed `vorpal-loader` (which embeds the matching public key) will verify + exec it.
//!
//! Subcommands:
//!   keygen                                   → print `<private_hex>` then `<public_hex>`
//!   pubkey  --key-hex <hex>                  → derive the public key hex from a private key
//!   hash    --agent <path>                   → print the agent's blake3 hex (for pinning)
//!   sign    --key-hex <hex> --agent <path> --out <vld> [--manifest <json>]
//!                                            → write the signed `[header][payload]` stream, and
//!                                              optionally a JSON manifest {blake3, signature, pubkey}
//!
//! The private key is passed as hex (from a CI secret / env), never a file on a shared node.

use std::collections::HashMap;
use std::process::ExitCode;

use vorpal_loader::{
  agent_blake3_hex, sign_payload, signing_key_from_hex, signing_key_to_hex, verifying_key_to_hex,
};

fn main() -> ExitCode {
  match run() {
    Ok(()) => ExitCode::SUCCESS,
    Err(msg) => {
      eprintln!("vorpal-sign: {msg}");
      ExitCode::FAILURE
    }
  }
}

fn run() -> Result<(), String> {
  let mut args = std::env::args().skip(1);
  let cmd = args.next().ok_or("expected a subcommand: keygen | pubkey | hash | sign")?;
  let opts = parse_opts(args);

  match cmd.as_str() {
    "keygen" => {
      #[cfg(feature = "keygen")]
      {
        let (sk, vk) = vorpal_loader::generate_keypair();
        println!("{}", signing_key_to_hex(&sk));
        println!("{}", verifying_key_to_hex(&vk));
        Ok(())
      }
      #[cfg(not(feature = "keygen"))]
      {
        Err("this build lacks the `keygen` feature".into())
      }
    }
    "pubkey" => {
      let sk = signing_key_from_hex(req(&opts, "key-hex")?).ok_or("invalid --key-hex")?;
      println!("{}", verifying_key_to_hex(&sk.verifying_key()));
      Ok(())
    }
    "hash" => {
      let agent = read_agent(req(&opts, "agent")?)?;
      println!("{}", agent_blake3_hex(&agent));
      Ok(())
    }
    "sign" => {
      let sk = signing_key_from_hex(req(&opts, "key-hex")?).ok_or("invalid --key-hex")?;
      let agent = read_agent(req(&opts, "agent")?)?;
      let out = req(&opts, "out")?;
      let stream = sign_payload(&agent, &sk, false);
      std::fs::write(out, &stream).map_err(|e| format!("writing {out}: {e}"))?;
      if let Some(manifest) = opts.get("manifest") {
        let blake3 = agent_blake3_hex(&agent);
        let pubkey = verifying_key_to_hex(&sk.verifying_key());
        // The signature is recoverable from the stream header; surface it in the manifest too.
        let sig_hex = &stream[48..112].iter().map(|b| format!("{b:02x}")).collect::<String>();
        let json = format!(
          "{{\n  \"blake3\": \"{blake3}\",\n  \"signature\": \"{sig_hex}\",\n  \"pubkey\": \"{pubkey}\",\n  \"agent_bytes\": {}\n}}\n",
          agent.len()
        );
        std::fs::write(manifest, json).map_err(|e| format!("writing {manifest}: {e}"))?;
      }
      eprintln!("vorpal-sign: signed {} bytes → {out}", agent.len());
      Ok(())
    }
    other => Err(format!("unknown subcommand `{other}`")),
  }
}

/// Parse `--flag value` pairs into a map (the tiny surface this tool needs).
fn parse_opts(mut args: impl Iterator<Item = String>) -> HashMap<String, String> {
  let mut map = HashMap::new();
  while let Some(a) = args.next() {
    if let Some(flag) = a.strip_prefix("--") {
      if let Some(val) = args.next() {
        map.insert(flag.to_string(), val);
      }
    }
  }
  map
}

fn req<'a>(opts: &'a HashMap<String, String>, key: &str) -> Result<&'a str, String> {
  opts.get(key).map(String::as_str).ok_or_else(|| format!("missing required --{key}"))
}

fn read_agent(path: &str) -> Result<Vec<u8>, String> {
  std::fs::read(path).map_err(|e| format!("reading agent {path}: {e}"))
}

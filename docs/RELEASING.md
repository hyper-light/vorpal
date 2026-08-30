# Release artifacts: checksums, signatures, provenance

`cargo xtask release-artifacts <dist-dir>` finalizes a directory of release files:

- **`SHA256SUMS`** — one `sha256  name` line per artifact, sorted.
- **`provenance.json`** — `vorpal-provenance/1`: the exact git commit, the `rustc` that built,
  a build timestamp, and per-artifact `size`/`sha256`/`blake3`.
- **`SHA256SUMS.sig`** — written only when `VORPAL_SIGN_KEY_HEX` is set (a CI secret, never a
  key file): one line, `ed25519 <pubkey-hex> <signature-hex>`, a detached signature over the
  exact bytes of `SHA256SUMS`, made with the same ed25519 machinery the fleet's stage-0
  loader (`vorpal-loader`) verifies agent binaries with.

## Verifying a release

```sh
shasum -a 256 --check SHA256SUMS            # every artifact's content
```

Signature check (any ed25519 verifier works; with vorpal's own helper):

```rust
let line = std::fs::read_to_string("SHA256SUMS.sig")?;
let [_, pubkey, sig] = line.split_whitespace().collect::<Vec<_>>()[..] else { panic!() };
assert!(vorpal_loader::verify_bytes_hex(pubkey, &std::fs::read("SHA256SUMS")?, sig));
```

The expected public key is pinned wherever you first learned to trust vorpal (the repository,
your fleet's coordinator config) — a signature only authenticates against a key you already
trust. Key generation: `cargo run -p vorpal-loader --features keygen --bin vorpal-sign -- keygen`.

CI wiring (attaching these files to a GitHub release, SLSA provenance generation) lives in the
release workflows and is intentionally not duplicated here.

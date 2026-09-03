mod eval;
mod schema;
mod searcheval;
use anyhow::{Context, Result, bail};
use serde_json::{Value as JSON, from_str as parse_json, to_string_pretty};
use std::env::args;
use std::fs::{self, read_dir, read_to_string};
use std::path::Path;
use std::process::{Command, Stdio};
use toml_edit::{DocumentMut, value as to_toml};

enum Task {
  Schema,
  Release(String),
  /// Checksums + optional detached signatures + a provenance record over a dist directory.
  ReleaseArtifacts(String),
  /// The agent-task evaluation suite (Phase E): vorpal vs file-exploration baseline.
  Eval { write_doc: bool },
  /// Graded retrieval-quality eval (semantic-tier Stage 0): a labels file against an
  /// arbitrary index — per-class NDCG@10 / MRR / recall@5, optional tier-vs-exact overlap.
  SearchEval {
    index: String,
    labels: String,
    overlap: bool,
    /// The indexed tree's directory — required only when a label's `path` is anchored
    /// (starts with `/`, i.e. tree-relative equality rather than `ends_with`).
    root: Option<String>,
  },
}

fn get_task() -> Result<Task> {
  let message = "argument is missing. Example usage: \ncargo xtask 0.1.3\ncargo xtask schema";
  let arg = args().nth(1).context(message)?;
  if arg == "eval" {
    return Ok(Task::Eval {
      write_doc: args().nth(2).as_deref() == Some("--write"),
    });
  }
  if arg == "searcheval" {
    let usage = "usage: cargo xtask searcheval <index-dir> <labels.json> [--overlap] [--root <tree>]";
    let rest: Vec<String> = args().skip(4).collect();
    let root = rest
      .iter()
      .position(|a| a == "--root")
      .map(|i| rest.get(i + 1).cloned().context("--root wants the indexed tree's directory"))
      .transpose()?;
    return Ok(Task::SearchEval {
      index: args().nth(2).context(usage)?,
      labels: args().nth(3).context(usage)?,
      overlap: rest.iter().any(|a| a == "--overlap"),
      root,
    });
  }
  if arg == "schema" {
    Ok(Task::Schema)
  } else if arg == "release-artifacts" {
    let dir = args()
      .nth(2)
      .context("usage: cargo xtask release-artifacts <dist-dir>")?;
    Ok(Task::ReleaseArtifacts(dir))
  } else {
    Ok(Task::Release(arg))
  }
}

fn main() -> Result<()> {
  match get_task()? {
    Task::Schema => schema::generate_schema(),
    Task::Release(version) => release_new_version(&version),
    Task::ReleaseArtifacts(dir) => release_artifacts(Path::new(&dir)),
    Task::Eval { write_doc } => eval::run_eval(write_doc),
    Task::SearchEval {
      index,
      labels,
      overlap,
      root,
    } => searcheval::run(Path::new(&index), Path::new(&labels), overlap, root.as_deref().map(Path::new)),
  }
}

/// Supply-chain surface for a dist directory (D6, the release-flow half that does not touch
/// CI workflow files): writes `SHA256SUMS`, a `provenance.json` (git commit, rustc/cargo
/// versions, per-file sha256 + blake3 + size), and — when `VORPAL_SIGN_KEY_HEX` is set (a CI
/// secret, never a file) — an ed25519 `SHA256SUMS.sig` over the checksums file via the same
/// vorpal-loader signing machinery the fleet's stage-0 loader verifies with. Verification
/// instructions live in docs/RELEASING.md.
fn release_artifacts(dist: &Path) -> Result<()> {
  use std::io::Read;
  if !dist.is_dir() {
    bail!("{} is not a directory", dist.display());
  }
  let mut names: Vec<String> = read_dir(dist)?
    .flatten()
    .filter(|e| e.path().is_file())
    .map(|e| e.file_name().to_string_lossy().into_owned())
    .filter(|n| n != "SHA256SUMS" && n != "SHA256SUMS.sig" && n != "provenance.json")
    .collect();
  names.sort();
  if names.is_empty() {
    bail!("{} holds no artifacts", dist.display());
  }

  let mut sums = String::new();
  let mut files = Vec::new();
  for name in &names {
    let path = dist.join(name);
    let mut file = fs::File::open(&path)?;
    use sha2::Digest;
    let mut sha = sha2::Sha256::new();
    let mut blake = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut len: u64 = 0;
    loop {
      let n = file.read(&mut buf)?;
      if n == 0 {
        break;
      }
      sha.update(&buf[..n]);
      blake.update(&buf[..n]);
      len += n as u64;
    }
    let sha_hex = format!("{:x}", sha.finalize());
    sums.push_str(&format!("{sha_hex}  {name}
"));
    files.push(serde_json::json!({
      "name": name,
      "size": len,
      "sha256": sha_hex,
      "blake3": blake.finalize().to_hex().to_string(),
    }));
  }
  fs::write(dist.join("SHA256SUMS"), &sums)?;
  println!("wrote SHA256SUMS ({} artifacts)", names.len());

  let commit = Command::new("git")
    .args(["rev-parse", "HEAD"])
    .output()
    .ok()
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    .unwrap_or_default();
  let rustc = Command::new("rustc")
    .arg("--version")
    .output()
    .ok()
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    .unwrap_or_default();
  let provenance = serde_json::json!({
    "format": "vorpal-provenance/1",
    "git_commit": commit,
    "rustc": rustc,
    "built_at_unix": std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map(|d| d.as_secs())
      .unwrap_or(0),
    "artifacts": files,
  });
  fs::write(
    dist.join("provenance.json"),
    format!("{}
", to_string_pretty(&provenance)?),
  )?;
  println!("wrote provenance.json (commit {commit})");

  match std::env::var("VORPAL_SIGN_KEY_HEX") {
    Ok(key_hex) if !key_hex.trim().is_empty() => {
      let key = vorpal_loader::signing_key_from_hex(key_hex.trim())
        .context("VORPAL_SIGN_KEY_HEX is not a valid ed25519 private key hex")?;
      let signature = vorpal_loader::sign_bytes_hex(&key, sums.as_bytes());
      let pubkey = vorpal_loader::verifying_key_to_hex(&key.verifying_key());
      fs::write(
        dist.join("SHA256SUMS.sig"),
        format!("ed25519 {pubkey} {signature}
"),
      )?;
      println!("wrote SHA256SUMS.sig (pubkey {pubkey})");
    }
    _ => println!("VORPAL_SIGN_KEY_HEX not set — checksums unsigned (fine for local builds)"),
  }
  Ok(())
}

fn release_new_version(version: &str) -> Result<()> {
  check_git_status()?;
  schema::generate_schema()?;
  bump_version(version)?;
  update_and_commit_changelog()?;
  commit_and_tag(version)?;
  Ok(())
}

fn check_git_status() -> Result<()> {
  let git = Command::new("git")
    .arg("status")
    .arg("--porcelain")
    .stdout(Stdio::piped())
    .spawn()?
    .wait_with_output()?;
  if !git.stdout.is_empty() {
    bail!(
      "The git working directory has uncommitted changes. Please commit or abandon them before release!"
    )
  } else {
    Ok(())
  }
}

fn bump_version(version: &str) -> Result<()> {
  update_npm(version)?;
  update_napi(version)?;
  update_python(version)?;
  update_crates(version)?;
  update_cargo_lock()?;
  Ok(())
}

fn update_npm(version: &str) -> Result<()> {
  let npm_path = "npm/package.json";
  let root_json = read_to_string(npm_path)?;
  let mut root_json: JSON = parse_json(&root_json)?;
  root_json["version"] = version.into();
  let deps = root_json["optionalDependencies"]
    .as_object_mut()
    .context("parse json error")?;
  for val in deps.values_mut() {
    *val = version.into();
  }
  fs::write(npm_path, to_string_pretty(&root_json)?)?;
  for entry in read_dir("npm/platforms")? {
    let path = entry?.path();
    if !path.is_dir() {
      continue;
    }
    let path = path.join("package.json");
    edit_json(path, version)?;
  }
  Ok(())
}

fn edit_json<P: AsRef<Path>>(path: P, version: &str) -> Result<()> {
  let json_str = read_to_string(&path)?;
  let mut json: JSON = parse_json(&json_str)?;
  json["version"] = version.into();
  fs::write(path, to_string_pretty(&json)?)?;
  Ok(())
}

fn update_napi(version: &str) -> Result<()> {
  let napi_path = "crates/napi/package.json";
  edit_json(napi_path, version)?;
  for entry in read_dir("crates/napi/npm")? {
    let path = entry?.path();
    if !path.is_dir() {
      continue;
    }
    let path = path.join("package.json");
    edit_json(path, version)?;
  }
  Ok(())
}

fn edit_root_toml<P: AsRef<Path>>(path: P, version: &str) -> Result<()> {
  let mut toml: DocumentMut = read_to_string(&path)?.parse()?;
  toml["workspace"]["package"]["version"] = to_toml(version);
  let deps = toml["workspace"]["dependencies"]
    .as_table_mut()
    .context("dep should be table")?;
  for (key, value) in deps.iter_mut() {
    if !key.starts_with("vorpal-") {
      continue;
    }
    if value.is_str() {
      *value = to_toml(version);
      continue;
    }
    if let Some(inline) = value.as_inline_table_mut() {
      inline["version"] = version.into();
    }
  }
  fs::write(path, toml.to_string())?;
  Ok(())
}

fn update_crates(version: &str) -> Result<()> {
  // update root toml
  let root_toml = Path::new("Cargo.toml");
  edit_root_toml(root_toml, version)?;
  // no need to update crates or benches
  Ok(())
}

fn update_python(version: &str) -> Result<()> {
  // update pypi pyproject.toml and pyo3 bindings
  for path in ["pyproject.toml", "crates/pyo3/pyproject.toml"] {
    let pyproject = Path::new(path);
    let mut toml: DocumentMut = read_to_string(pyproject)?.parse()?;
    toml["project"]["version"] = to_toml(version);
    fs::write(pyproject, toml.to_string())?;
  }
  Ok(())
}

fn update_cargo_lock() -> Result<()> {
  if Command::new("cargo").args(["build"]).status()?.success() {
    Ok(())
  } else {
    bail!("cargo build fail! cannot update Cargo.lock")
  }
}

fn commit_and_tag(version: &str) -> Result<()> {
  // NB: napi needs line break to decide npm tag
  // https://github.com/ast-grep/ast-grep/blob/998691d36b477766be92f1ede3c0bc153d0cca42/.github/workflows/napi.yml#L164
  let message = format!("{version}\nbump version");
  let commit = Command::new("git")
    .arg("commit")
    .arg("-am")
    .arg(message)
    .spawn()?
    .wait()?;
  if !commit.success() {
    bail!("commit failed");
  }
  let tag = Command::new("git")
    .arg("tag")
    .arg(version)
    .spawn()?
    .wait()?;
  if !tag.success() {
    bail!("create tag failed");
  }
  Ok(())
}

fn update_and_commit_changelog() -> Result<()> {
  Command::new("auto-changelog")
    .arg("-p")
    .arg("npm/package.json")
    .arg("--breaking-pattern")
    .arg("BREAKING CHANGE")
    .spawn()
    .context("cannot run command `auto-changelog`. Please install it.")?
    .wait()?;
  Ok(())
}

//! Shareable index artifacts (D2): export a committed generation as one `.vidx` file; import
//! it elsewhere with **trust by recomputation**, never by faith in the file.
//!
//! Format (`vidx 1`): a zstd-compressed tar. The first entry, `VIDX-MANIFEST`, records the
//! format version, the exporter's generation content id, and — per artifact — the byte length
//! and an xxh3-128 digest of the raw bytes. The remaining entries are exactly the generation
//! artifacts, in the fixed artifact order, with zeroed metadata (mtime/uid/gid) so identical
//! generations export byte-identically under one zstd version.
//!
//! Import verifies every artifact against its manifest digest (a byte-level tamper check that
//! is stable across vorpal versions), stages the files, and then commits through the exact
//! generation machinery a local build uses — recomputing the content id with THIS binary's
//! fold and installing under the recomputed name, with an atomic `CURRENT` swap. When the
//! recomputed id differs from the exporter's (the fold is a per-version internal, not an
//! interchange format), the import still succeeds — bytes were verified — and the report says
//! so explicitly rather than pretending the ids should have matched.
//!
//! Extraction is allowlist-only: entries are matched against the manifest's artifact set;
//! anything else (unexpected names, path tricks) is refused loudly.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};


const MANIFEST_NAME: &str = "VIDX-MANIFEST";
const FORMAT_LINE: &str = "vidx 1";

#[derive(Debug)]
pub struct ExportReport {
  /// The exporter-side generation content id.
  pub content_id: String,
  /// Artifacts written (an older/smaller generation may legitimately lack some).
  pub artifacts: usize,
  /// Compressed size of the written `.vidx`.
  pub bytes: u64,
}

#[derive(Debug)]
pub struct ImportReport {
  /// The id the generation was installed under (recomputed by THIS binary).
  pub installed_id: String,
  /// The id the exporter recorded.
  pub exported_id: String,
  /// Present when the two ids differ: the artifact bytes verified, but the exporting binary's
  /// content-id fold differs from this one's (a per-version internal, not corruption).
  pub fold_note: Option<String>,
}

/// Export the live generation of `index_root` to `out_file`.
pub fn export_generation(index_root: &Path, out_file: &Path) -> Result<ExportReport, String> {
  let dir = vorpal_kg::resolve_index_dir(index_root);
  if !dir.join("nodes.vseg").exists() {
    return Err(format!(
      "no index generation at {} (build one first: `vorpal-index index <src> {}`)",
      dir.display(),
      index_root.display()
    ));
  }
  // gen/<id> dirs are named by content; a legacy flat root recomputes.
  let content_id = match (dir.parent(), dir.file_name()) {
    (Some(parent), Some(name)) if parent.file_name().is_some_and(|p| p == "gen") => {
      name.to_string_lossy().into_owned()
    }
    _ => crate::generation_content_id(&dir)
      .map_err(|err| format!("hashing legacy index at {}: {err}", dir.display()))?,
  };

  // Manifest first: names, lengths, and raw-byte digests (version-stable tamper checks).
  let mut manifest = format!("{FORMAT_LINE}\ncontent-id {content_id}\n");
  let mut present: Vec<String> = Vec::new();
  for artifact in crate::generation_artifact_names(&dir) {
    let path = dir.join(&artifact);
    let Ok(bytes) = fs::read(&path) else {
      continue;
    };
    manifest.push_str(&format!(
      "artifact {artifact} {} {:032x}\n",
      bytes.len(),
      xxhash_rust::xxh3::xxh3_128(&bytes)
    ));
    present.push(artifact);
  }

  let file = fs::File::create(out_file)
    .map_err(|err| format!("creating {}: {err}", out_file.display()))?;
  let encoder = zstd::stream::write::Encoder::new(file, 3)
    .map_err(|err| format!("zstd encoder: {err}"))?
    .auto_finish();
  let mut tar = tar::Builder::new(encoder);

  append_entry(&mut tar, MANIFEST_NAME, manifest.as_bytes())?;
  for artifact in &present {
    let bytes = fs::read(dir.join(artifact))
      .map_err(|err| format!("reading artifact {artifact}: {err}"))?;
    append_entry(&mut tar, artifact, &bytes)?;
  }
  tar
    .into_inner()
    .map_err(|err| format!("finalizing archive: {err}"))?;
  let bytes = fs::metadata(out_file).map(|m| m.len()).unwrap_or(0);
  Ok(ExportReport {
    content_id,
    artifacts: present.len(),
    bytes,
  })
}

fn append_entry<W: Write>(
  tar: &mut tar::Builder<W>,
  name: &str,
  bytes: &[u8],
) -> Result<(), String> {
  let mut header = tar::Header::new_gnu();
  header.set_size(bytes.len() as u64);
  header.set_mode(0o644);
  header.set_mtime(0);
  header.set_uid(0);
  header.set_gid(0);
  header.set_cksum();
  tar
    .append_data(&mut header, name, bytes)
    .map_err(|err| format!("writing archive entry {name}: {err}"))
}

/// Import a `.vidx` into `index_root`: verify, stage, commit, atomically swap `CURRENT`.
pub fn import_generation(vidx: &Path, index_root: &Path) -> Result<ImportReport, String> {
  let file =
    fs::File::open(vidx).map_err(|err| format!("opening {}: {err}", vidx.display()))?;
  let decoder =
    zstd::stream::read::Decoder::new(file).map_err(|err| format!("zstd decoder: {err}"))?;
  let mut archive = tar::Archive::new(decoder);
  let mut entries = archive
    .entries()
    .map_err(|err| format!("reading archive: {err}"))?;

  // First entry MUST be the manifest.
  let mut manifest_text = String::new();
  {
    let mut first = entries
      .next()
      .ok_or_else(|| "empty archive".to_string())?
      .map_err(|err| format!("reading archive: {err}"))?;
    let name = first
      .path()
      .map_err(|err| format!("archive entry path: {err}"))?
      .to_string_lossy()
      .into_owned();
    if name != MANIFEST_NAME {
      return Err(format!(
        "not a vidx archive: first entry is '{name}', expected {MANIFEST_NAME}"
      ));
    }
    first
      .read_to_string(&mut manifest_text)
      .map_err(|err| format!("reading manifest: {err}"))?;
  }
  let (exported_id, expected) = parse_manifest(&manifest_text)?;

  // Stage under gen/ (same filesystem as the final dir — the commit's rename stays atomic).
  let staging = index_root
    .join("gen")
    .join(format!(".staging-import-{}", std::process::id()));
  let _ = fs::remove_dir_all(&staging);
  fs::create_dir_all(&staging).map_err(|err| format!("creating staging: {err}"))?;

  let cleanup = |staging: &PathBuf| {
    let _ = fs::remove_dir_all(staging);
  };

  let mut seen: usize = 0;
  for entry in entries {
    let mut entry = entry.map_err(|err| format!("reading archive: {err}"))?;
    let name = entry
      .path()
      .map_err(|err| format!("archive entry path: {err}"))?
      .to_string_lossy()
      .into_owned();
    // Allowlist extraction: only names the manifest declares, written under staging by OUR
    // joined path — an archive cannot place files anywhere else, whatever its headers say.
    let Some((len, digest)) = expected.get(name.as_str()) else {
      cleanup(&staging);
      return Err(format!(
        "archive entry '{name}' is not in the manifest — refusing the import"
      ));
    };
    let mut bytes = Vec::with_capacity(*len as usize);
    entry
      .read_to_end(&mut bytes)
      .map_err(|err| format!("reading artifact {name}: {err}"))?;
    if bytes.len() as u64 != *len || format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&bytes)) != *digest {
      cleanup(&staging);
      return Err(format!(
        "artifact {name} failed verification (length or digest mismatch) — the archive is \
         corrupt or tampered; nothing was installed"
      ));
    }
    let dst = staging.join(&name);
    if let Some(parent) = dst.parent().filter(|p| *p != staging.as_path()) {
      fs::create_dir_all(parent)
        .map_err(|err| format!("staging artifact {name}: {err}"))?;
    }
    fs::write(&dst, &bytes).map_err(|err| format!("staging artifact {name}: {err}"))?;
    seen += 1;
  }
  if seen != expected.len() {
    cleanup(&staging);
    return Err(format!(
      "archive holds {seen} of {} manifest artifacts — truncated; nothing was installed",
      expected.len()
    ));
  }

  // Commit through the exact machinery a local build uses: recomputes the content id with
  // this binary's fold, dedups, swaps CURRENT atomically, GCs superseded generations.
  let prior = vorpal_kg::resolve_index_dir(index_root);
  let installed_id = crate::commit_generation(index_root, &prior, staging)
    .map_err(|err| format!("committing imported generation: {err}"))?;

  let fold_note = (installed_id != exported_id).then(|| {
    format!(
      "artifact bytes verified against the manifest, but this binary's content-id fold \
       differs from the exporter's ({exported_id}) — installed under the locally computed \
       id (the fold is a per-version internal, not an interchange format)"
    )
  });
  Ok(ImportReport {
    installed_id,
    exported_id,
    fold_note,
  })
}

/// `artifact name → (length, digest-hex)`. Owned keys: the bucketed pack's member names
/// (`products/<k>.pack`) are corpus-dependent, not a fixed static set.
type ManifestArtifacts = BTreeMap<String, (u64, String)>;

/// Parse the manifest: `(content id, artifacts)`.
fn parse_manifest(text: &str) -> Result<(String, ManifestArtifacts), String> {
  let mut lines = text.lines();
  if lines.next() != Some(FORMAT_LINE) {
    return Err("unsupported vidx format (expected 'vidx 1')".to_string());
  }
  let id_line = lines.next().unwrap_or("");
  let exported_id = id_line
    .strip_prefix("content-id ")
    .ok_or_else(|| "manifest missing content-id".to_string())?
    .to_string();
  let mut expected = BTreeMap::new();
  for line in lines {
    if line.trim().is_empty() {
      continue;
    }
    let mut parts = line.split(' ');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
      (Some("artifact"), Some(name), Some(len), Some(digest)) => {
        // Only legal artifact names are ever accepted, however the manifest was produced:
        // the fixed flat set, or a bucketed pack member — the shared predicate admits no
        // separators beyond the single `products/` prefix, so a name can never traverse.
        if !crate::is_generation_artifact_name(name) {
          return Err(format!("manifest names unknown artifact '{name}'"));
        }
        let len: u64 = len
          .parse()
          .map_err(|_| format!("manifest length for {name} is not a number"))?;
        expected.insert(name.to_string(), (len, digest.to_string()));
      }
      _ => return Err(format!("malformed manifest line: '{line}'")),
    }
  }
  if expected.is_empty() {
    return Err("manifest lists no artifacts".to_string());
  }
  Ok((exported_id, expected))
}

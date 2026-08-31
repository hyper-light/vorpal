//! Optional-model packaging (semantic-tier Stage 6).
//!
//! The advanced embedder's weights NEVER ship inside release artifacts (547 MB f32
//! against npm/PyPI budgets) and are NEVER fetched implicitly: the checksum-pinned
//! installer below — `vorpal enable semantic-f32|semantic-f16` and the SDK install
//! APIs — is the one EXPLICIT download path, and it lives behind the
//! `model-install` feature so network machinery stays out of every other build.
//! The ENABLE half (paths, the global enable file, its reader) is always
//! available: any consumer honors an enable that a feature-full tool wrote.
//!
//! Layout: `$VORPAL_HOME` (default `~/.vorpal`) holds `models/<model>-<variant>/`
//! (models overridable wholesale via `$VORPAL_MODELS_DIR`) and the GLOBAL enable
//! file `encoder.dir` — the fallback every `Searcher` consults when an index root
//! has no selection of its own (per-index `encoder.dir`/`encoderDir` always
//! wins). The f16 variant halves download-adjacent disk by converting the
//! VERIFIED f32 weights locally (IEEE 754 round-to-nearest-even, in
//! `vorpal_ann::encoder`); the loader upconverts at open, so f16 currently trades
//! ~547 MB of anonymous RSS while a handle is live for the halved footprint —
//! recorded, with the f16-native kernel as the follow-up lead.

use std::path::{Path, PathBuf};

/// `$VORPAL_HOME`, default `~/.vorpal` — the per-user home for models and the
/// global enable file.
pub fn vorpal_home() -> Result<PathBuf, String> {
  if let Some(home) = std::env::var_os("VORPAL_HOME") {
    return Ok(PathBuf::from(home));
  }
  #[cfg(windows)]
  let user_home = std::env::var_os("USERPROFILE");
  #[cfg(not(windows))]
  let user_home = std::env::var_os("HOME");
  user_home
    .map(|home| PathBuf::from(home).join(".vorpal"))
    .ok_or_else(|| "no home directory (set VORPAL_HOME)".to_string())
}

/// Where models install: `$VORPAL_MODELS_DIR`, default `<vorpal home>/models`.
pub fn models_root() -> Result<PathBuf, String> {
  if let Some(dir) = std::env::var_os("VORPAL_MODELS_DIR") {
    return Ok(PathBuf::from(dir));
  }
  Ok(vorpal_home()?.join("models"))
}

/// The GLOBAL encoder enable file — the fallback selection every `Searcher`
/// consults when the index root carries none of its own.
pub fn global_selection_path() -> Result<PathBuf, String> {
  Ok(vorpal_home()?.join("encoder.dir"))
}

/// Read the global enable, if any (same trim/empty rules as the per-index file).
pub(crate) fn global_encoder_selection() -> Option<PathBuf> {
  let path = global_selection_path().ok()?;
  let text = std::fs::read_to_string(path).ok()?;
  let trimmed = text.trim();
  if trimmed.is_empty() {
    None
  } else {
    Some(PathBuf::from(trimmed))
  }
}

/// Write the GLOBAL enable file (tmp + rename) pointing at `model_dir`. Per-index
/// selections keep precedence.
pub fn enable_global(model_dir: &Path) -> Result<PathBuf, String> {
  let path = global_selection_path()?;
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
  }
  let tmp = path.with_extension("dir.tmp");
  std::fs::write(&tmp, format!("{}\n", model_dir.display()))
    .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
  std::fs::rename(&tmp, &path).map_err(|e| format!("writing {}: {e}", path.display()))?;
  Ok(path)
}

/// Remove the global enable (per-index selections are untouched). `Ok(false)`
/// when nothing was enabled.
pub fn disable_global() -> Result<bool, String> {
  let path = global_selection_path()?;
  match std::fs::remove_file(&path) {
    Ok(()) => Ok(true),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
    Err(error) => Err(format!("removing {}: {error}", path.display())),
  }
}

#[cfg(feature = "model-install")]
mod install_impl {
  use std::io::{Read, Write};
  use std::path::{Path, PathBuf};

  use sha2::Digest;

  use super::models_root;

  /// One pinned artifact of a model distribution.
  struct PinnedFile {
    name: &'static str,
    url: &'static str,
    sha256: &'static str,
    bytes: u64,
  }

  /// The vendored encoder's distribution: CodeRankEmbed (MIT, cornstack/Nomic),
  /// pinned at the exact revision the campaign proved against its references
  /// (checksums recorded in docs/wip/BENCHMARKS.md "Stage 6").
  const CODERANKEMBED_FILES: [PinnedFile; 3] = [
    PinnedFile {
      name: "model.safetensors",
      url: "https://huggingface.co/nomic-ai/CodeRankEmbed/resolve/main/model.safetensors",
      sha256: "827529bcd58aef0d9082e66eeff7e7d53a02f62bd005f841a26b3d3e2fb17ebe",
      bytes: 546_938_168,
    },
    PinnedFile {
      name: "tokenizer.json",
      url: "https://huggingface.co/nomic-ai/CodeRankEmbed/resolve/main/tokenizer.json",
      sha256: "91f1def9b9391fdabe028cd3f3fcc4efd34e5d1f08c3bf2de513ebb5911a1854",
      bytes: 711_649,
    },
    PinnedFile {
      name: "config.json",
      url: "https://huggingface.co/nomic-ai/CodeRankEmbed/resolve/main/config.json",
      sha256: "5ff856a41d0f53ef2d74520627d464bd75c2efd8f26f381bd528654895c29b6c",
      bytes: 1_525,
    },
  ];

  /// Which weight precision to install.
  #[derive(Clone, Copy, PartialEq, Eq)]
  pub enum ModelVariant {
    /// The published f32 weights, byte-identical to the pinned upstream artifact.
    F32,
    /// Locally converted f16 weights: halved download-adjacent disk, converted
    /// deterministically from the verified f32 bytes.
    F16,
  }

  impl ModelVariant {
    pub fn parse(option: &str) -> Option<ModelVariant> {
      match option {
        "semantic-f32" => Some(ModelVariant::F32),
        "semantic-f16" => Some(ModelVariant::F16),
        _ => None,
      }
    }

    fn directory(self) -> &'static str {
      match self {
        ModelVariant::F32 => "coderankembed-f32",
        ModelVariant::F16 => "coderankembed-f16",
      }
    }
  }

  fn sha256_of(path: &Path) -> Result<String, String> {
    let mut file =
      std::fs::File::open(path).map_err(|e| format!("opening {}: {e}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
      let read = file
        .read(&mut buffer)
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
      if read == 0 {
        break;
      }
      hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
  }

  /// Download `file` into `dir` with streaming sha256 verification: `.part` +
  /// atomic rename; a checksum mismatch deletes the download and errors. An
  /// already-present, checksum-verified file is kept (idempotent installs).
  fn fetch_pinned(
    dir: &Path,
    file: &PinnedFile,
    progress: &mut dyn FnMut(&str),
  ) -> Result<(), String> {
    let target = dir.join(file.name);
    if target.is_file() && sha256_of(&target)? == file.sha256 {
      progress(&format!("{} already installed (checksum verified)", file.name));
      return Ok(());
    }
    progress(&format!(
      "downloading {} ({:.1} MB)…",
      file.name,
      file.bytes as f64 / 1e6
    ));
    let response = ureq::get(file.url)
      .call()
      .map_err(|e| format!("downloading {}: {e}", file.url))?;
    let part = dir.join(format!("{}.part", file.name));
    let mut reader = response.into_reader();
    let mut writer = std::io::BufWriter::new(
      std::fs::File::create(&part).map_err(|e| format!("creating {}: {e}", part.display()))?,
    );
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut written = 0u64;
    loop {
      let read = reader
        .read(&mut buffer)
        .map_err(|e| format!("stream from {}: {e}", file.url))?;
      if read == 0 {
        break;
      }
      hasher.update(&buffer[..read]);
      writer
        .write_all(&buffer[..read])
        .map_err(|e| format!("writing {}: {e}", part.display()))?;
      written += read as u64;
    }
    writer
      .flush()
      .map_err(|e| format!("flushing {}: {e}", part.display()))?;
    drop(writer);
    let digest = format!("{:x}", hasher.finalize());
    if digest != file.sha256 || written != file.bytes {
      let _ = std::fs::remove_file(&part);
      return Err(format!(
        "{}: downloaded artifact does not match the pinned checksum/size \
         (got {digest}, {written} bytes; pinned {}, {} bytes) — refusing to install",
        file.name, file.sha256, file.bytes
      ));
    }
    std::fs::rename(&part, &target)
      .map_err(|e| format!("installing {}: {e}", target.display()))?;
    progress(&format!("{} verified and installed", file.name));
    Ok(())
  }

  /// Install the requested variant under the models root (or `root` when given)
  /// and return the installed model directory. f32 = the pinned upstream bytes;
  /// f16 = pinned f32 downloaded (or reused from a verified sibling f32 install),
  /// converted locally, the f32 original removed from the f16 directory.
  pub fn install(
    variant: ModelVariant,
    root: Option<&Path>,
    progress: &mut dyn FnMut(&str),
  ) -> Result<PathBuf, String> {
    let root = match root {
      Some(root) => root.to_path_buf(),
      None => models_root()?,
    };
    let dir = root.join(variant.directory());
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    match variant {
      ModelVariant::F32 => {
        for file in &CODERANKEMBED_FILES {
          fetch_pinned(&dir, file, progress)?;
        }
      }
      ModelVariant::F16 => {
        let f16_weights = dir.join("model.safetensors");
        // Small pinned files land as-is; the weights convert.
        for file in &CODERANKEMBED_FILES[1..] {
          fetch_pinned(&dir, file, progress)?;
        }
        if f16_weights.is_file()
          && vorpal_ann::encoder::safetensors_is_f16(&f16_weights).unwrap_or(false)
        {
          progress("f16 weights already installed");
        } else {
          // Reuse a verified sibling f32 install when present; else download into
          // this directory, convert, and remove the f32 original.
          let sibling = root
            .join(ModelVariant::F32.directory())
            .join("model.safetensors");
          let weights = &CODERANKEMBED_FILES[0];
          let (source, downloaded_here) =
            if sibling.is_file() && sha256_of(&sibling)? == weights.sha256 {
              progress("converting from the existing f32 install");
              (sibling, false)
            } else {
              fetch_pinned(&dir, weights, progress)?;
              (dir.join(weights.name), true)
            };
          progress("converting weights to f16 (round-to-nearest-even)…");
          let part = dir.join("model.safetensors.f16part");
          vorpal_ann::encoder::convert_safetensors_f32_to_f16(&source, &part)?;
          if downloaded_here {
            let _ = std::fs::remove_file(&source);
          }
          std::fs::rename(&part, &f16_weights)
            .map_err(|e| format!("installing {}: {e}", f16_weights.display()))?;
          progress("f16 weights installed");
        }
      }
    }
    Ok(dir)
  }
}

#[cfg(feature = "model-install")]
pub use install_impl::{ModelVariant, install};

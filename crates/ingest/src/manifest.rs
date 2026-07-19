//! Stat-based change detection (§3.4): a persisted `path → (size, mtime)` manifest so an
//! unchanged tree is detected without reading or parsing a single file (near-instant re-index).

use std::fs;
use std::io;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// One file's identity by cheap `stat` metadata (no content read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
  pub path: String,
  pub size: u64,
  pub mtime_ns: u64,
}

/// A sorted set of file stats for a tree — the change-detection spine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
  entries: Vec<FileStat>,
}

impl Manifest {
  /// Walk `root` (respecting `.gitignore`), `stat` each file the predicate accepts, and record
  /// its size + mtime. No file contents are read.
  pub fn scan(root: &Path, handled: impl Fn(&str) -> bool) -> io::Result<Self> {
    let mut entries = Vec::new();
    for entry in ignore::Walk::new(root) {
      let entry = entry.map_err(io::Error::other)?;
      if !entry.file_type().is_some_and(|t| t.is_file()) {
        continue;
      }
      let path = entry.path();
      let path_str = path.to_string_lossy();
      if !handled(&path_str) {
        continue;
      }
      let meta = fs::metadata(path)?;
      let mtime_ns = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
      entries.push(FileStat {
        path: path_str.into_owned(),
        size: meta.len(),
        mtime_ns,
      });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Self { entries })
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// True when the tree is byte-for-byte unchanged (same files, sizes, and mtimes).
  pub fn unchanged_since(&self, prior: &Manifest) -> bool {
    self.entries == prior.entries
  }

  pub fn save(&self, path: &Path) -> io::Result<()> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
    for entry in &self.entries {
      buf.extend_from_slice(&(entry.path.len() as u32).to_le_bytes());
      buf.extend_from_slice(entry.path.as_bytes());
      buf.extend_from_slice(&entry.size.to_le_bytes());
      buf.extend_from_slice(&entry.mtime_ns.to_le_bytes());
    }
    fs::write(path, buf)
  }

  pub fn load(path: &Path) -> io::Result<Self> {
    let bytes = fs::read(path)?;
    let mut entries = Vec::new();
    if bytes.len() < 8 {
      return Ok(Self::default());
    }
    let count = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let mut off = 8usize;
    for _ in 0..count {
      if off + 4 > bytes.len() {
        break;
      }
      let plen = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
      off += 4;
      if off + plen + 16 > bytes.len() {
        break;
      }
      let path = String::from_utf8_lossy(&bytes[off..off + plen]).into_owned();
      off += plen;
      let size = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
      off += 8;
      let mtime_ns = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
      off += 8;
      entries.push(FileStat {
        path,
        size,
        mtime_ns,
      });
    }
    Ok(Self { entries })
  }
}

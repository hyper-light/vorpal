//! Stat-based change detection (§3.4): a persisted `path → (size, mtime)` manifest so an
//! unchanged tree is detected without reading or parsing a single file (near-instant re-index).

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use ignore::WalkState;

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
  /// A digest over the grammar set this manifest was written under (see
  /// [`crate::global_grammar_stamp`]). The whole-tree fast path reuses the persisted index only
  /// when this still matches, so editing/bumping any grammar forces a re-index even when no file
  /// changed. `0` on manifests written before the field existed — which never equals a real
  /// stamp, so they correctly force a rebuild.
  grammar_stamp: u64,
}

const MANIFEST_MAGIC: &[u8; 4] = b"VMAN";
const MANIFEST_VERSION: u32 = 1;

impl Manifest {
  /// Walk `root` (respecting `.gitignore`), `stat` each file the predicate accepts, and record
  /// its size + mtime. No file contents are read. The walk + stats run on the parallel walker
  /// (§7.5 — the stat sweep is the entire cost of a no-change re-index); the final sort keeps
  /// the manifest deterministic regardless of arrival order. Error semantics match the serial
  /// walk: the first walk/stat error aborts the scan.
  pub fn scan(root: &Path, handled: impl Fn(&str) -> bool + Sync) -> io::Result<Self> {
    let entries = Mutex::new(Vec::new());
    let first_error: Mutex<Option<io::Error>> = Mutex::new(None);
    // Per-walker-thread accumulation: each visitor pushes into its own vector and flushes
    // once into the shared sink when the walker retires it (the Drop below) — the previous
    // form took the global mutex once per accepted file (~72k lock round-trips at kernel
    // scale, on the sweep that IS the whole cost of a no-change re-index). The final sort
    // makes arrival order unobservable, exactly as before.
    struct Flush<'a> {
      local: Vec<FileStat>,
      sink: &'a Mutex<Vec<FileStat>>,
    }
    impl Drop for Flush<'_> {
      fn drop(&mut self) {
        if !self.local.is_empty() {
          self.sink.lock().unwrap().append(&mut self.local);
        }
      }
    }
    ignore::WalkBuilder::new(root)
      .threads(
        std::thread::available_parallelism()
          .map(|n| n.get())
          .unwrap_or(1)
          .min(16),
      )
      .build_parallel()
      .run(|| {
        // Capture shared state by reference (references to Sync values are Send); `move`
        // then transfers only the per-thread `flush` buffer and these borrows.
        let handled = &handled;
        let first_error = &first_error;
        let mut flush = Flush {
          local: Vec::new(),
          sink: &entries,
        };
        Box::new(move |result| {
          let entry = match result {
            Ok(entry) => entry,
            Err(err) => {
              first_error
                .lock()
                .unwrap()
                .get_or_insert(io::Error::other(err));
              return WalkState::Quit;
            }
          };
          if !entry.file_type().is_some_and(|t| t.is_file()) {
            return WalkState::Continue;
          }
          let path_str = entry.path().to_string_lossy();
          if !handled(&path_str) {
            return WalkState::Continue;
          }
          // `entry.metadata()` reuses the walk's own entry instead of a second path
          // resolution + stat(2) per file. Symlinks never reach here (the dirent file-type
          // gate above rejects them), so the semantics match the follow-stat exactly.
          let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(err) => {
              first_error
                .lock()
                .unwrap()
                .get_or_insert(io::Error::other(err));
              return WalkState::Quit;
            }
          };
          let modified = match meta.modified() {
            Ok(modified) => modified,
            Err(err) => {
              first_error.lock().unwrap().get_or_insert(err);
              return WalkState::Quit;
            }
          };
          let mtime_ns = modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
          flush.local.push(FileStat {
            path: path_str.into_owned(),
            size: meta.len(),
            mtime_ns,
          });
          WalkState::Continue
        })
      });
    if let Some(err) = first_error.into_inner().unwrap() {
      return Err(err);
    }
    let mut entries = entries.into_inner().unwrap();
    // Paths are unique, so the unstable parallel sort is deterministic.
    {
      use rayon::prelude::*;
      entries.par_sort_unstable_by(|a, b| a.path.cmp(&b.path));
    }
    Ok(Self {
      entries,
      grammar_stamp: 0,
    })
  }

  /// Record the grammar-set digest this manifest was built under. Callers set this to
  /// [`crate::global_grammar_stamp`] before saving / before the fast-path comparison.
  pub fn set_grammar_stamp(&mut self, stamp: u64) {
    self.grammar_stamp = stamp;
  }

  /// The grammar-set digest recorded for this manifest (`0` if none).
  pub fn grammar_stamp(&self) -> u64 {
    self.grammar_stamp
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

  /// All scanned files, sorted by path.
  pub fn entries(&self) -> &[FileStat] {
    &self.entries
  }

  /// Whether this manifest holds an identical stat for `stat` (same path, size, and mtime) —
  /// the per-file unchanged test driving incremental re-index.
  pub fn contains(&self, stat: &FileStat) -> bool {
    self
      .entries
      .binary_search_by(|e| e.path.cmp(&stat.path))
      .is_ok_and(|i| self.entries[i] == *stat)
  }

  pub fn save(&self, path: &Path) -> io::Result<()> {
    let mut buf = Vec::new();
    buf.extend_from_slice(MANIFEST_MAGIC);
    buf.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
    buf.extend_from_slice(&self.grammar_stamp.to_le_bytes());
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
    // A pre-versioning manifest (no magic), a torn write, or a foreign file loads as the default
    // (empty, grammar_stamp 0) — which never matches the current tree/grammar, so the fast path
    // correctly falls through to a rebuild instead of trusting stale bytes.
    if bytes.len() < 24 || &bytes[0..4] != MANIFEST_MAGIC {
      return Ok(Self::default());
    }
    if u32::from_le_bytes(bytes[4..8].try_into().unwrap()) != MANIFEST_VERSION {
      return Ok(Self::default());
    }
    let grammar_stamp = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let count = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let mut off = 24usize;
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
    Ok(Self {
      entries,
      grammar_stamp,
    })
  }
}

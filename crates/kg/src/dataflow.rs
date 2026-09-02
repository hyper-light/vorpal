//! The data-flow sidecar (`dataflow.bin`, VDFL v1 — G-M3): one fixed-width row per traceable
//! argument at a **resolved** call site, with expression text in a deduplicated string pool.
//! Evidence-discipline throughout: rows sort under a TOTAL order before encoding, so the file
//! is a pure function of the row set; the reader tolerates an absent file (older generations)
//! by answering "no flows", never erroring; the pool is gated at the u32 ceiling.
//!
//! Row (28 bytes):
//!   from u32 · to u32 · span_start u32 · span_end u32 · arg_index u16 · param_index u16 ·
//!   class u8 · pad u8 · expr_off u32 (u32::MAX = none) · expr_len u16
//! Header: magic "VDFL", version u32 = 1, row count u32, pool length u32.

use std::io::{self, Write};
use std::path::Path;

const MAGIC: &[u8; 4] = b"VDFL";
const VERSION: u32 = 1;
const ROW: usize = 28;
const NO_EXPR: u32 = u32::MAX;

/// One traceable argument at a resolved call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataflowRow {
  /// Calling definition (dense node id).
  pub from: u32,
  /// Callee definition the call resolved to.
  pub to: u32,
  /// Call-site span in `from`'s file.
  pub span: (u32, u32),
  /// Argument position at the call site.
  pub arg_index: u16,
  /// Parameter position bound on the callee (positional v1: equals `arg_index`).
  pub param_index: u16,
  /// `ArgClass` discriminant (Var/FieldAccess/CallResult).
  pub class: u8,
  /// Expression text (≤64 bytes), when the class carries one.
  pub expr: Option<String>,
}

impl DataflowRow {
  fn key(&self) -> (u32, u32, u32, u32, u16, u16, u8, &str) {
    (
      self.from,
      self.to,
      self.span.0,
      self.span.1,
      self.arg_index,
      self.param_index,
      self.class,
      self.expr.as_deref().unwrap_or(""),
    )
  }
}

/// Load every row of `dir/dataflow.bin` (the respan compose's read side). `None` for an
/// absent or foreign file — degraded, never an error.
pub fn load_dataflow(dir: &Path) -> Option<Vec<DataflowRow>> {
  let bytes = std::fs::read(dir.join("dataflow.bin")).ok()?;
  if bytes.len() < 16 || &bytes[0..4] != MAGIC {
    return None;
  }
  if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION {
    return None;
  }
  let count = u32::from_le_bytes(bytes[8..12].try_into().ok()?) as usize;
  let pool_len = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
  let rows_at = 16usize;
  let pool_at = rows_at + count * ROW;
  if bytes.len() != pool_at + pool_len {
    return None;
  }
  let pool = &bytes[pool_at..];
  let mut rows = Vec::with_capacity(count);
  for i in 0..count {
    let at = rows_at + i * ROW;
    let b = &bytes[at..at + ROW];
    let expr_off = u32::from_le_bytes(b[22..26].try_into().ok()?);
    let expr_len = u16::from_le_bytes(b[26..28].try_into().ok()?) as usize;
    let expr = if expr_off == NO_EXPR {
      None
    } else {
      let start = expr_off as usize;
      Some(std::str::from_utf8(pool.get(start..start + expr_len)?).ok()?.to_string())
    };
    rows.push(DataflowRow {
      from: u32::from_le_bytes(b[0..4].try_into().ok()?),
      to: u32::from_le_bytes(b[4..8].try_into().ok()?),
      span: (
        u32::from_le_bytes(b[8..12].try_into().ok()?),
        u32::from_le_bytes(b[12..16].try_into().ok()?),
      ),
      arg_index: u16::from_le_bytes(b[16..18].try_into().ok()?),
      param_index: u16::from_le_bytes(b[18..20].try_into().ok()?),
      class: b[20],
      expr,
    });
  }
  Some(rows)
}

/// Persist rows to `dir/dataflow.bin`. Deterministic: total-order sort, first-occurrence
/// pool interning over the sorted sequence.
pub fn save_dataflow(dir: &Path, mut rows: Vec<DataflowRow>) -> io::Result<()> {
  use rayon::prelude::*;
  crate::phase_stamp("dataflow: sort start");
  rows.par_sort_unstable_by(|a, b| a.key().cmp(&b.key()));
  crate::phase_stamp("dataflow: encode start");

  // Pool: dedup expression texts, offsets assigned in sorted-row first-use order (pure
  // function of the sorted row set).
  let mut pool = Vec::new();
  let mut offsets: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
  for row in &rows {
    if let Some(expr) = row.expr.as_deref() {
      if !offsets.contains_key(expr) {
        if pool.len() + expr.len() > u32::MAX as usize {
          return Err(io::Error::other(
            "dataflow expression pool exceeds the u32 ceiling — file an issue; the corpus \
             has more distinct argument expressions than the format's address space",
          ));
        }
        offsets.insert(expr, pool.len() as u32);
        pool.extend_from_slice(expr.as_bytes());
      }
    }
  }

  let mut buf = Vec::with_capacity(16 + rows.len() * ROW + pool.len());
  buf.extend_from_slice(MAGIC);
  buf.extend_from_slice(&VERSION.to_le_bytes());
  buf.extend_from_slice(&(rows.len() as u32).to_le_bytes());
  buf.extend_from_slice(&(pool.len() as u32).to_le_bytes());
  for row in &rows {
    buf.extend_from_slice(&row.from.to_le_bytes());
    buf.extend_from_slice(&row.to.to_le_bytes());
    buf.extend_from_slice(&row.span.0.to_le_bytes());
    buf.extend_from_slice(&row.span.1.to_le_bytes());
    buf.extend_from_slice(&row.arg_index.to_le_bytes());
    buf.extend_from_slice(&row.param_index.to_le_bytes());
    buf.push(row.class);
    buf.push(0);
    match row.expr.as_deref() {
      Some(expr) => {
        buf.extend_from_slice(&offsets[expr].to_le_bytes());
        buf.extend_from_slice(&(expr.len() as u16).to_le_bytes());
      }
      None => {
        buf.extend_from_slice(&NO_EXPR.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
      }
    }
  }
  buf.extend_from_slice(&pool);

  let tmp = dir.join("dataflow.bin.tmp");
  {
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&buf)?;
    file.sync_all()?;
  }
  std::fs::rename(&tmp, dir.join("dataflow.bin"))?;
  crate::phase_stamp("dataflow: saved");
  Ok(())
}

/// Read-side handle. Absent-tolerant: a generation without the sidecar answers empty.
pub struct DataflowStore {
  rows: Vec<StoredRow>,
  pool: Vec<u8>,
}

#[derive(Clone, Copy)]
struct StoredRow {
  from: u32,
  to: u32,
  span: (u32, u32),
  arg_index: u16,
  param_index: u16,
  class: u8,
  expr_off: u32,
  expr_len: u16,
}

/// One flow row as answered to queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowView<'a> {
  pub from: u32,
  pub to: u32,
  pub span: (u32, u32),
  pub arg_index: u16,
  pub param_index: u16,
  pub class: u8,
  pub expr: Option<&'a str>,
}

impl DataflowStore {
  /// Load `dir/dataflow.bin`; an absent file is an EMPTY store (older generation — degrade,
  /// never error); torn or foreign bytes are an error (corruption is never silent).
  pub fn load(dir: &Path) -> io::Result<DataflowStore> {
    let path = dir.join("dataflow.bin");
    let bytes = match std::fs::read(&path) {
      Ok(bytes) => bytes,
      Err(err) if err.kind() == io::ErrorKind::NotFound => {
        return Ok(DataflowStore {
          rows: Vec::new(),
          pool: Vec::new(),
        });
      }
      Err(err) => return Err(err),
    };
    if bytes.len() < 16 || &bytes[0..4] != MAGIC {
      return Err(io::Error::other("dataflow.bin: bad magic"));
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().expect("4B"));
    if version != VERSION {
      return Err(io::Error::other(format!(
        "dataflow.bin: version {version} (this binary reads {VERSION})"
      )));
    }
    let count = u32::from_le_bytes(bytes[8..12].try_into().expect("4B")) as usize;
    let pool_len = u32::from_le_bytes(bytes[12..16].try_into().expect("4B")) as usize;
    let need = 16 + count * ROW + pool_len;
    if bytes.len() != need {
      return Err(io::Error::other("dataflow.bin: truncated"));
    }
    let mut rows = Vec::with_capacity(count);
    for i in 0..count {
      let at = 16 + i * ROW;
      let b = &bytes[at..at + ROW];
      let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().expect("4B"));
      let u16_at = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().expect("2B"));
      rows.push(StoredRow {
        from: u32_at(0),
        to: u32_at(4),
        span: (u32_at(8), u32_at(12)),
        arg_index: u16_at(16),
        param_index: u16_at(18),
        class: b[20],
        expr_off: u32_at(22),
        expr_len: u16_at(26),
      });
    }
    let pool = bytes[16 + count * ROW..].to_vec();
    Ok(DataflowStore { rows, pool })
  }

  pub fn len(&self) -> usize {
    self.rows.len()
  }

  pub fn is_empty(&self) -> bool {
    self.rows.is_empty()
  }

  fn view(&self, row: &StoredRow) -> FlowView<'_> {
    let expr = if row.expr_off == NO_EXPR {
      None
    } else {
      let start = row.expr_off as usize;
      self
        .pool
        .get(start..start + row.expr_len as usize)
        .and_then(|slice| std::str::from_utf8(slice).ok())
    };
    FlowView {
      from: row.from,
      to: row.to,
      span: row.span,
      arg_index: row.arg_index,
      param_index: row.param_index,
      class: row.class,
      expr,
    }
  }

  /// The flow rows between one (from, to) pair — the per-hop detail a trace attaches.
  /// Rows are from-major sorted, so this is a binary-searched slice.
  pub fn flows_between(&self, from: u32, to: u32) -> Vec<FlowView<'_>> {
    let start = self.rows.partition_point(|r| (r.from, r.to) < (from, to));
    self.rows[start..]
      .iter()
      .take_while(|r| (r.from, r.to) == (from, to))
      .map(|r| self.view(r))
      .collect()
  }

  /// Every flow row leaving `from` (from-major order).
  pub fn flows_from(&self, from: u32) -> Vec<FlowView<'_>> {
    let start = self.rows.partition_point(|r| r.from < from);
    self.rows[start..]
      .iter()
      .take_while(|r| r.from == from)
      .map(|r| self.view(r))
      .collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn round_trips_deterministically_and_degrades_when_absent() {
    let dir = std::env::temp_dir().join(format!("vorpal-vdfl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let rows = vec![
      DataflowRow {
        from: 7,
        to: 9,
        span: (10, 20),
        arg_index: 0,
        param_index: 0,
        class: 0,
        expr: Some("count".into()),
      },
      DataflowRow {
        from: 3,
        to: 9,
        span: (1, 5),
        arg_index: 1,
        param_index: 1,
        class: 1,
        expr: Some("cfg.size".into()),
      },
      DataflowRow {
        from: 7,
        to: 9,
        span: (30, 40),
        arg_index: 0,
        param_index: 0,
        class: 2,
        expr: None,
      },
      // Duplicate expression: pooled once.
      DataflowRow {
        from: 8,
        to: 9,
        span: (50, 60),
        arg_index: 0,
        param_index: 0,
        class: 0,
        expr: Some("count".into()),
      },
    ];
    save_dataflow(&dir, rows.clone()).unwrap();
    let first = std::fs::read(dir.join("dataflow.bin")).unwrap();
    // Reversed input order → identical bytes (total-order sort).
    let mut reversed = rows.clone();
    reversed.reverse();
    save_dataflow(&dir, reversed).unwrap();
    assert_eq!(first, std::fs::read(dir.join("dataflow.bin")).unwrap());

    let store = DataflowStore::load(&dir).unwrap();
    assert_eq!(store.len(), 4);
    let between = store.flows_between(7, 9);
    assert_eq!(between.len(), 2);
    assert_eq!(between[0].expr, Some("count"));
    assert_eq!(between[1].expr, None);
    assert_eq!(store.flows_from(3).len(), 1);
    assert_eq!(store.flows_from(999).len(), 0);

    // Absent file: empty store, no error.
    let empty_dir = dir.join("nothing-here");
    std::fs::create_dir_all(&empty_dir).unwrap();
    let empty = DataflowStore::load(&empty_dir).unwrap();
    assert!(empty.is_empty());

    // Torn file: loud error.
    std::fs::write(empty_dir.join("dataflow.bin"), b"VDFL junk").unwrap();
    assert!(DataflowStore::load(&empty_dir).is_err());

    let _ = std::fs::remove_dir_all(&dir);
  }
}

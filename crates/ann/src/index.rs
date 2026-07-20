//! The adaptive index: tier chosen per corpus size (ConfigForN), exact rerank everywhere.

use std::fs;
use std::io;
use std::path::Path;

use crate::quant::SignQuantizer;
use crate::vamana::{BuildParams, Vamana, greedy_search};
use crate::{l2_sq, normalize};

/// Which search tier a corpus of `n` vectors gets (§8.1's ConfigForN philosophy: data-derived,
/// near-zero baseline, one code path at every scale).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnConfig {
  /// Small corpus: a full exact scan is the fastest correct algorithm — zero build cost.
  FlatExact,
  /// Medium: 1-bit Hamming pre-filter over all codes, exact rerank of the candidate pool.
  FlatQuantized,
  /// Large: Vamana beam search, exact rerank of the visited pool.
  Vamana,
}

impl AnnConfig {
  /// Exact scan up to 64k vectors (a few-ms full scan IS the fastest correct algorithm there),
  /// Vamana beyond — its traversal uses full-precision distances, so it is safe for both dense
  /// and sparse embeddings. `FlatQuantized` is never auto-selected: 1-bit Hamming codes are only
  /// discriminative for *dense* embeddings (sparse hashing-trick vectors agree on every zero
  /// dimension), so the quantized tier is opt-in for dense neural embedders.
  pub fn for_n(n: usize) -> Self {
    if n <= 65_536 {
      AnnConfig::FlatExact
    } else {
      AnnConfig::Vamana
    }
  }
}

const VAMANA_R: usize = 32;
const VAMANA_L_BUILD: usize = 64;
const VAMANA_ALPHA: f32 = 1.2;
const BUILD_SEED: u64 = 0x9E37_79B9_7F4A_7C15;
const MAGIC: u32 = 0x414E_4E31; // "ANN1"

/// One length-checked section slice, advancing the read offset.
fn take_bulk<'a>(bytes: &'a [u8], off: &mut usize, len: usize) -> io::Result<&'a [u8]> {
  let end = off
    .checked_add(len)
    .ok_or_else(|| io::Error::other("truncated ann index"))?;
  let section = bytes
    .get(*off..end)
    .ok_or_else(|| io::Error::other("truncated ann index"))?;
  *off = end;
  Ok(section)
}

/// Decode a packed little-endian section into a typed vec: a straight `pod_collect` on LE
/// targets, the per-element decoder elsewhere.
fn read_le_slice<T: bytemuck::Pod, const W: usize>(
  section: &[u8],
  decode: fn([u8; W]) -> T,
) -> Vec<T> {
  if cfg!(target_endian = "little") {
    bytemuck::pod_collect_to_vec(section)
  } else {
    section
      .chunks_exact(W)
      .map(|chunk| decode(chunk.try_into().expect("exact chunk")))
      .collect()
  }
}

/// The sealed vector index: full-precision vectors (rerank ground truth) plus the tier's
/// acceleration structure, and the caller's stable id per row.
pub struct AnnIndex {
  dim: usize,
  vectors: Vec<f32>,
  ids: Vec<u64>,
  config: AnnConfig,
  codes: Vec<u64>,
  code_words: usize,
  graph: Vec<Vec<u32>>,
  medoid: u32,
}

impl AnnIndex {
  /// Build from `(stable id, vector)` rows; vectors are unit-normalized on the way in. The tier
  /// follows `AnnConfig::for_n` unless overridden (tests exercise every tier at any size).
  pub fn build(dim: usize, rows: Vec<(u64, Vec<f32>)>, config: Option<AnnConfig>) -> AnnIndex {
    let n = rows.len();
    let mut ids = Vec::with_capacity(n);
    let mut vectors = Vec::with_capacity(n * dim);
    for (id, mut v) in rows {
      v.resize(dim, 0.0);
      normalize(&mut v);
      ids.push(id);
      vectors.extend_from_slice(&v);
    }
    Self::build_flat(dim, ids, vectors, config)
  }

  /// Build from an already-flat row-major matrix of **pre-normalized** vectors — the
  /// bulk-indexing path, which fills the matrix in place in parallel instead of allocating a
  /// heap vector per row first.
  pub fn build_flat(
    dim: usize,
    ids: Vec<u64>,
    vectors: Vec<f32>,
    config: Option<AnnConfig>,
  ) -> AnnIndex {
    let n = ids.len();
    debug_assert_eq!(vectors.len(), n * dim);
    let config = config.unwrap_or_else(|| AnnConfig::for_n(n));

    let quantizer = SignQuantizer::new(dim);
    let (codes, code_words) = if config == AnnConfig::FlatQuantized {
      let mut codes = Vec::with_capacity(n * quantizer.words());
      for i in 0..n {
        codes.extend(quantizer.encode(&vectors[i * dim..(i + 1) * dim]));
      }
      (codes, quantizer.words())
    } else {
      (Vec::new(), 0)
    };

    let (graph, medoid) = if config == AnnConfig::Vamana {
      let vamana = Vamana::build(
        &vectors,
        dim,
        &BuildParams {
          r: VAMANA_R,
          l_build: VAMANA_L_BUILD,
          alpha: VAMANA_ALPHA,
          seed: BUILD_SEED,
        },
      );
      (vamana.graph, vamana.medoid)
    } else {
      (Vec::new(), 0)
    };

    AnnIndex {
      dim,
      vectors,
      ids,
      config,
      codes,
      code_words,
      graph,
      medoid,
    }
  }

  pub fn len(&self) -> usize {
    self.ids.len()
  }

  pub fn is_empty(&self) -> bool {
    self.ids.is_empty()
  }

  pub fn config(&self) -> AnnConfig {
    self.config
  }

  /// Top-`k` nearest rows as `(stable id, squared L2 distance)`, closest first. Every tier ends
  /// in an exact rerank over full-precision vectors.
  pub fn search(&self, query: &[f32], k: usize) -> Vec<(u64, f32)> {
    let n = self.len();
    if n == 0 || k == 0 {
      return Vec::new();
    }
    let mut q = query.to_vec();
    q.resize(self.dim, 0.0);
    normalize(&mut q);

    let candidates: Vec<u32> = match self.config {
      AnnConfig::FlatExact => (0..n as u32).collect(),
      AnnConfig::FlatQuantized => {
        let quantizer = SignQuantizer::new(self.dim);
        let qcode = quantizer.encode(&q);
        let pool = (k * 16).clamp(256, n);
        let mut by_hamming: Vec<(u32, u32)> = (0..n as u32)
          .map(|i| {
            let code =
              &self.codes[i as usize * self.code_words..(i as usize + 1) * self.code_words];
            (i, SignQuantizer::hamming(&qcode, code))
          })
          .collect();
        by_hamming.sort_by_key(|&(i, h)| (h, i));
        by_hamming.truncate(pool);
        by_hamming.into_iter().map(|(i, _)| i).collect()
      }
      AnnConfig::Vamana => {
        let l = (k * 8).clamp(64, n);
        greedy_search(&self.graph, self.medoid, &self.vectors, self.dim, &q, l)
          .into_iter()
          .map(|(i, _)| i)
          .collect()
      }
    };

    // Exact rerank: full-precision distances decide the final order, always.
    let mut ranked: Vec<(u64, f32)> = candidates
      .into_iter()
      .map(|i| {
        let v = &self.vectors[i as usize * self.dim..(i as usize + 1) * self.dim];
        (self.ids[i as usize], l2_sq(v, &q))
      })
      .collect();
    ranked.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(k);
    ranked
  }

  /// Persist: little-endian sections (header, ids, vectors, codes, CSR graph), **streamed**
  /// through a buffered writer — at millions of nodes the bulk sections span gigabytes, and
  /// building the whole file in memory first doubled peak RSS for nothing. Bulk sections are
  /// single slice writes on little-endian targets (the format is LE either way; only the
  /// fallback loops per element).
  pub fn save(&self, path: &Path) -> io::Result<()> {
    use std::io::Write;
    let file = fs::File::create(path)?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    out.write_all(&MAGIC.to_le_bytes())?;
    out.write_all(&(self.dim as u32).to_le_bytes())?;
    out.write_all(&(self.len() as u64).to_le_bytes())?;
    let config_tag: u32 = match self.config {
      AnnConfig::FlatExact => 0,
      AnnConfig::FlatQuantized => 1,
      AnnConfig::Vamana => 2,
    };
    out.write_all(&config_tag.to_le_bytes())?;
    out.write_all(&(self.code_words as u32).to_le_bytes())?;
    out.write_all(&self.medoid.to_le_bytes())?;
    if cfg!(target_endian = "little") {
      out.write_all(bytemuck::cast_slice(&self.ids))?;
      out.write_all(bytemuck::cast_slice(&self.vectors))?;
      out.write_all(bytemuck::cast_slice(&self.codes))?;
    } else {
      for &id in &self.ids {
        out.write_all(&id.to_le_bytes())?;
      }
      for &x in &self.vectors {
        out.write_all(&x.to_le_bytes())?;
      }
      for &w in &self.codes {
        out.write_all(&w.to_le_bytes())?;
      }
    }
    out.write_all(&(self.graph.len() as u64).to_le_bytes())?;
    for neighbors in &self.graph {
      out.write_all(&(neighbors.len() as u32).to_le_bytes())?;
      if cfg!(target_endian = "little") {
        out.write_all(bytemuck::cast_slice(neighbors))?;
      } else {
        for &nb in neighbors {
          out.write_all(&nb.to_le_bytes())?;
        }
      }
    }
    out.flush()
  }

  pub fn load(path: &Path) -> io::Result<AnnIndex> {
    let bytes = fs::read(path)?;
    let mut off = 0usize;
    let take32 = |bytes: &[u8], off: &mut usize| -> io::Result<u32> {
      let end = *off + 4;
      let v = bytes
        .get(*off..end)
        .ok_or_else(|| io::Error::other("truncated ann index"))?;
      *off = end;
      Ok(u32::from_le_bytes(v.try_into().expect("4 bytes")))
    };
    let take64 = |bytes: &[u8], off: &mut usize| -> io::Result<u64> {
      let end = *off + 8;
      let v = bytes
        .get(*off..end)
        .ok_or_else(|| io::Error::other("truncated ann index"))?;
      *off = end;
      Ok(u64::from_le_bytes(v.try_into().expect("8 bytes")))
    };

    if take32(&bytes, &mut off)? != MAGIC {
      return Err(io::Error::other("bad ann index magic"));
    }
    let dim = take32(&bytes, &mut off)? as usize;
    let n = take64(&bytes, &mut off)? as usize;
    let config = match take32(&bytes, &mut off)? {
      0 => AnnConfig::FlatExact,
      1 => AnnConfig::FlatQuantized,
      2 => AnnConfig::Vamana,
      other => return Err(io::Error::other(format!("bad ann config tag {other}"))),
    };
    let code_words = take32(&bytes, &mut off)? as usize;
    let medoid = take32(&bytes, &mut off)?;
    // Bulk sections: one length-checked slice each, then a single unaligned LE copy — the
    // element-at-a-time loops made cold `search` pay a bounds check + push per float.
    let ids: Vec<u64> = read_le_slice(take_bulk(&bytes, &mut off, n * 8)?, u64::from_le_bytes);
    let vectors: Vec<f32> = read_le_slice(
      take_bulk(&bytes, &mut off, n * dim * 4)?,
      f32::from_le_bytes,
    );
    let codes: Vec<u64> = read_le_slice(
      take_bulk(&bytes, &mut off, n * code_words * 8)?,
      u64::from_le_bytes,
    );
    let graph_len = take64(&bytes, &mut off)? as usize;
    let mut graph = Vec::with_capacity(graph_len);
    for _ in 0..graph_len {
      let degree = take32(&bytes, &mut off)? as usize;
      let mut neighbors = Vec::with_capacity(degree);
      for _ in 0..degree {
        neighbors.push(take32(&bytes, &mut off)?);
      }
      graph.push(neighbors);
    }
    Ok(AnnIndex {
      dim,
      vectors,
      ids,
      config,
      codes,
      code_words,
      graph,
      medoid,
    })
  }
}

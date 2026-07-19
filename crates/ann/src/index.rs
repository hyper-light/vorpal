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
    let config = config.unwrap_or_else(|| AnnConfig::for_n(n));
    let mut ids = Vec::with_capacity(n);
    let mut vectors = Vec::with_capacity(n * dim);
    for (id, mut v) in rows {
      v.resize(dim, 0.0);
      normalize(&mut v);
      ids.push(id);
      vectors.extend_from_slice(&v);
    }

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

  /// Persist: little-endian sections (header, ids, vectors, codes, CSR graph).
  pub fn save(&self, path: &Path) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let push32 = |buf: &mut Vec<u8>, v: u32| buf.extend_from_slice(&v.to_le_bytes());
    let push64 = |buf: &mut Vec<u8>, v: u64| buf.extend_from_slice(&v.to_le_bytes());
    push32(&mut buf, MAGIC);
    push32(&mut buf, self.dim as u32);
    push64(&mut buf, self.len() as u64);
    push32(
      &mut buf,
      match self.config {
        AnnConfig::FlatExact => 0,
        AnnConfig::FlatQuantized => 1,
        AnnConfig::Vamana => 2,
      },
    );
    push32(&mut buf, self.code_words as u32);
    push32(&mut buf, self.medoid);
    for &id in &self.ids {
      push64(&mut buf, id);
    }
    for &x in &self.vectors {
      buf.extend_from_slice(&x.to_le_bytes());
    }
    for &w in &self.codes {
      push64(&mut buf, w);
    }
    push64(&mut buf, self.graph.len() as u64);
    for neighbors in &self.graph {
      push32(&mut buf, neighbors.len() as u32);
      for &nb in neighbors {
        push32(&mut buf, nb);
      }
    }
    fs::write(path, buf)
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
    let mut ids = Vec::with_capacity(n);
    for _ in 0..n {
      ids.push(take64(&bytes, &mut off)?);
    }
    let mut vectors = Vec::with_capacity(n * dim);
    for _ in 0..n * dim {
      let end = off + 4;
      let v = bytes
        .get(off..end)
        .ok_or_else(|| io::Error::other("truncated ann vectors"))?;
      off = end;
      vectors.push(f32::from_le_bytes(v.try_into().expect("4 bytes")));
    }
    let mut codes = Vec::with_capacity(n * code_words);
    for _ in 0..n * code_words {
      codes.push(take64(&bytes, &mut off)?);
    }
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

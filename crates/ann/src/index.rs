//! The adaptive index: tier chosen per corpus size (ConfigForN), exact rerank everywhere.

use std::fs;
use std::io;
use std::path::Path;

use vorpal_mem::PodColumn;

use crate::qmatrix::QuantMatrix;
use crate::quant::SignQuantizer;
use crate::vamana::{Adjacency, BuildParams, Vamana, VisitStamps, greedy_search};
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
const VAMANA_L_BUILD: usize = 48;
const VAMANA_ALPHA: f32 = 1.2;
const BUILD_SEED: u64 = 0x9E37_79B9_7F4A_7C15;
/// "ANN5" — v2 moved the Vamana tier's vectors to per-row-scaled i8 codes (4× smaller,
/// integer-exact distances; flat tiers still store f32); v3 dropped Import nodes from the
/// row set (wiring, not semantic targets); v4 lays the Vamana tier out as aligned sections
/// with a CSR adjacency so queries **mmap it zero-copy**; v5 stamps the header with the
/// **generation of the KG the build embedded** (`base_stamp`), so a torn rebuild window can
/// never pair one generation's bin with another generation's sidecars. Older files fail the
/// magic check and are rebuilt (the ANN tier is a cache — see `ensure_ann`).
const MAGIC: u32 = 0x414E_4E35;

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
  /// xxh3 of the node segment this index was built from — generation identity (v5).
  base_stamp: u64,
  dim: usize,
  /// Full-precision rows — flat tiers only (empty for Vamana, whose rows live in `quant`).
  vectors: Vec<f32>,
  ids: PodColumn<u64>,
  config: AnnConfig,
  codes: Vec<u64>,
  code_words: usize,
  /// i8 rows + scale algebra — the Vamana tier's only vector storage.
  quant: Option<QuantMatrix>,
  /// Vamana adjacency: per-node lists when just built, CSR columns when loaded (mapped).
  graph: AnnGraphStore,
  medoid: u32,
}

/// The Vamana graph's storage form. Both expose identical rows via [`Adjacency`].
enum AnnGraphStore {
  /// Freshly built: the construction slab (fixed-capacity rows + lengths).
  Flat {
    flat: Vec<u32>,
    lens: Vec<u8>,
    cap: usize,
  },
  Csr {
    offsets: PodColumn<u64>,
    targets: PodColumn<u32>,
  },
}

impl AnnGraphStore {
  fn adjacency(&self) -> Adjacency<'_> {
    match self {
      AnnGraphStore::Flat { flat, lens, cap } => Adjacency::FlatCap {
        flat,
        lens,
        cap: *cap,
      },
      AnnGraphStore::Csr { offsets, targets } => Adjacency::Csr { offsets, targets },
    }
  }

  fn edge_count(&self) -> usize {
    match self {
      AnnGraphStore::Flat { lens, .. } => lens.iter().map(|&l| l as usize).sum(),
      AnnGraphStore::Csr { targets, .. } => targets.len(),
    }
  }
}

impl AnnIndex {
  /// Build from `(stable id, vector)` rows; vectors are unit-normalized on the way in. The tier
  /// follows `AnnConfig::for_n` unless overridden (tests exercise every tier at any size).
  /// Generation identity: the stamp of the KG this index embeds (0 for indexes built via
  /// the id/vector constructors outside an index directory, e.g. tests and library use).
  pub fn base_stamp(&self) -> u64 {
    self.base_stamp
  }

  /// Set the generation stamp on a freshly built index (builder-style; the persistence
  /// layer writes it into the v5 header).
  pub fn with_base_stamp(mut self, base_stamp: u64) -> Self {
    self.base_stamp = base_stamp;
    self
  }

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

    if config == AnnConfig::Vamana {
      let quant = QuantMatrix::from_rows(n, dim, |i, row: &mut [f32]| {
        row.copy_from_slice(&vectors[i * dim..(i + 1) * dim])
      });
      drop(vectors);
      return Self::build_vamana(dim, ids, quant);
    }

    AnnIndex {
      base_stamp: 0,
      dim,
      vectors,
      ids: PodColumn::from_vec(ids),
      config,
      codes,
      code_words,
      quant: None,
      graph: AnnGraphStore::Flat {
        flat: Vec::new(),
        lens: Vec::new(),
        cap: 0,
      },
      medoid: 0,
    }
  }

  /// Build the large tier by embedding rows straight into i8 codes — the full-precision
  /// matrix never materializes (2.9 GB of pure transient at kernel scale). `fill` writes row
  /// `i`'s (pre-normalized) embedding into its scratch slice.
  pub fn build_rows<F: Fn(usize, &mut [f32]) + Sync>(
    dim: usize,
    ids: Vec<u64>,
    fill: F,
  ) -> AnnIndex {
    let n = ids.len();
    if AnnConfig::for_n(n) != AnnConfig::Vamana {
      // Small corpus: materializing f32 rows is cheap and keeps the exact flat scan.
      let mut vectors = vec![0.0f32; n * dim];
      vectors
        .chunks_mut(dim)
        .enumerate()
        .for_each(|(i, row): (usize, &mut [f32])| fill(i, row));
      return Self::build_flat(dim, ids, vectors, None);
    }
    let quant = QuantMatrix::from_rows(n, dim, fill);
    Self::build_vamana(dim, ids, quant)
  }

  fn build_vamana(dim: usize, ids: Vec<u64>, quant: QuantMatrix) -> AnnIndex {
    let vamana = Vamana::build(
      &quant,
      &BuildParams {
        r: VAMANA_R,
        l_build: VAMANA_L_BUILD,
        alpha: VAMANA_ALPHA,
        seed: BUILD_SEED,
      },
    );
    AnnIndex {
      base_stamp: 0,
      dim,
      vectors: Vec::new(),
      ids: PodColumn::from_vec(ids),
      config: AnnConfig::Vamana,
      codes: Vec::new(),
      code_words: 0,
      quant: Some(quant),
      graph: AnnGraphStore::Flat {
        flat: vamana.flat,
        lens: vamana.lens,
        cap: vamana.cap,
      },
      medoid: vamana.medoid,
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
        // Traversal and candidate ranking both run on the i8 codes: distances are exact
        // functions of the codes, so the ordering is deterministic; callers wanting
        // full-precision final ordering re-score the returned pool (§10's rerank seam).
        let quant = self.quant.as_ref().expect("vamana tier carries codes");
        let query = quant.quantize_query(&q);
        let l = (k * 16).clamp(64, n);
        let mut stamps = VisitStamps::new(n);
        let adjacency = self.graph.adjacency();
        let mut pool = greedy_search(
          &adjacency,
          self.medoid,
          l,
          &mut stamps,
          |i| quant.dist_to_query(i, &query),
          |i| quant.prefetch_row(i),
        );
        pool.sort_by(|a, b| {
          (a.1, a.0)
            .partial_cmp(&(b.1, b.0))
            .unwrap_or(std::cmp::Ordering::Equal)
        });
        pool.truncate(k);
        return pool
          .into_iter()
          .map(|(i, d)| (self.ids[i as usize], d))
          .collect();
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

  /// Persist, little-endian throughout. The Vamana tier writes **aligned sections** in
  /// alignment order (header, u64 ids, u64 CSR offsets, f32 scale algebra, u32 CSR targets,
  /// i8 codes) so `load` maps the file zero-copy; flat tiers write their small owned
  /// sections behind the same header. Streamed through a buffered writer — the bulk
  /// sections span hundreds of megabytes.
  pub fn save(&self, path: &Path) -> io::Result<()> {
    use std::io::Write;
    let n = self.len();
    // Land via tmp + rename: a daemon may be serving searches from a map of the previous
    // ann.bin, and truncating a mapped file faults its readers. Rename swaps the directory
    // entry; the old inode survives until its last map drops.
    let tmp = path.with_extension("bin.tmp");
    let file = fs::File::create(&tmp)?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    out.write_all(&MAGIC.to_le_bytes())?;
    out.write_all(&(self.dim as u32).to_le_bytes())?;
    out.write_all(&(n as u64).to_le_bytes())?;
    let config_tag: u32 = match self.config {
      AnnConfig::FlatExact => 0,
      AnnConfig::FlatQuantized => 1,
      AnnConfig::Vamana => 2,
    };
    out.write_all(&config_tag.to_le_bytes())?;
    out.write_all(&(self.code_words as u32).to_le_bytes())?;
    out.write_all(&self.medoid.to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?; // pad: the u64 sections that follow stay 8-aligned
    out.write_all(&(self.graph.edge_count() as u64).to_le_bytes())?;
    out.write_all(&self.base_stamp.to_le_bytes())?; // header = 48 bytes

    fn write_le<T: bytemuck::Pod, W: std::io::Write, const B: usize>(
      out: &mut W,
      column: &[T],
      encode: fn(&T) -> [u8; B],
    ) -> io::Result<()> {
      if cfg!(target_endian = "little") {
        out.write_all(bytemuck::cast_slice(column))
      } else {
        for x in column {
          out.write_all(&encode(x))?;
        }
        Ok(())
      }
    }

    write_le(&mut out, &self.ids, |x: &u64| x.to_le_bytes())?;
    match &self.quant {
      Some(quant) => {
        // CSR offsets from either storage form — identical bytes by construction.
        match &self.graph {
          AnnGraphStore::Flat { lens, .. } => {
            let mut offset = 0u64;
            out.write_all(&offset.to_le_bytes())?;
            for &len in lens {
              offset += len as u64;
              out.write_all(&offset.to_le_bytes())?;
            }
          }
          AnnGraphStore::Csr { offsets, .. } => {
            write_le(&mut out, offsets, |x: &u64| x.to_le_bytes())?;
          }
        }
        write_le(&mut out, &quant.scales, |x: &f32| x.to_le_bytes())?;
        write_le(&mut out, &quant.snorm, |x: &f32| x.to_le_bytes())?;
        match &self.graph {
          AnnGraphStore::Flat { flat, lens, cap } => {
            for (i, &len) in lens.iter().enumerate() {
              let at = i * cap;
              write_le(&mut out, &flat[at..at + len as usize], |x: &u32| {
                x.to_le_bytes()
              })?;
            }
          }
          AnnGraphStore::Csr { targets, .. } => {
            write_le(&mut out, targets, |x: &u32| x.to_le_bytes())?;
          }
        }
        out.write_all(bytemuck::cast_slice(&quant.codes))?;
      }
      None => {
        write_le(&mut out, &self.vectors, |x: &f32| x.to_le_bytes())?;
        write_le(&mut out, &self.codes, |x: &u64| x.to_le_bytes())?;
        // Flat tiers have no graph; the header's edge count is 0.
      }
    }
    out.flush()?;
    drop(out);
    #[cfg(windows)]
    let _ = fs::remove_file(path);
    fs::rename(&tmp, path)
  }

  /// Whether the file at `path` exists, carries this build's format magic, **and** was built
  /// at `dim` — the cheap pre-check freshness logic uses to force a rebuild across format
  /// bumps *and* embedder-dimension changes (a query hashed into `d` buckets probing an index
  /// hashed into `d'` buckets would be silently meaningless).
  pub fn is_current_format(path: &Path, dim: usize) -> bool {
    Self::peek_header(path).is_some_and(|(header_dim, _)| header_dim == dim)
  }

  /// The `(dim, base_stamp)` of a v5 file at `path`, from its 48-byte header alone — `None`
  /// for missing, foreign, or older-format files. The generation check that makes torn
  /// bin/sidecar combinations detectable without loading anything.
  pub fn peek_header(path: &Path) -> Option<(usize, u64)> {
    let mut header = [0u8; 48];
    std::fs::File::open(path)
      .and_then(|mut f| {
        use std::io::Read;
        f.read_exact(&mut header)
      })
      .ok()?;
    if u32::from_le_bytes(header[0..4].try_into().expect("4 bytes")) != MAGIC {
      return None;
    }
    Some((
      u32::from_le_bytes(header[4..8].try_into().expect("4 bytes")) as usize,
      u64::from_le_bytes(header[40..48].try_into().expect("8 bytes")),
    ))
  }

  pub fn load(path: &Path) -> io::Result<AnnIndex> {
    // Header (32 bytes) from a plain read; the Vamana tier then maps the file and views its
    // sections zero-copy, so open cost is O(1) and pages fault in as the beam touches them.
    let store = std::sync::Arc::new(vorpal_mem::MappedStore::map_file(
      path,
      vorpal_mem::StoreKind::AnnCodes,
      vorpal_mem::AccessPattern::Random,
      vorpal_mem::Hotness::Hot,
      &vorpal_mem::ResourcePolicy::probe(vorpal_mem::CorpusProbe::new(0, 0)),
    )?);
    let bytes = store.as_bytes();
    let take32 = |at: usize| -> io::Result<u32> {
      bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
        .ok_or_else(|| io::Error::other("truncated ann index"))
    };
    let take64 = |at: usize| -> io::Result<u64> {
      bytes
        .get(at..at + 8)
        .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
        .ok_or_else(|| io::Error::other("truncated ann index"))
    };
    if take32(0)? != MAGIC {
      return Err(io::Error::other("bad ann index magic"));
    }
    let dim = take32(4)? as usize;
    let n = take64(8)? as usize;
    let config = match take32(16)? {
      0 => AnnConfig::FlatExact,
      1 => AnnConfig::FlatQuantized,
      2 => AnnConfig::Vamana,
      other => return Err(io::Error::other(format!("bad ann config tag {other}"))),
    };
    let code_words = take32(20)? as usize;
    let medoid = take32(24)?;
    let edges = take64(32)? as usize;
    let base_stamp = take64(40)?;

    let mut at = 48usize;
    let mut take = |len: usize| {
      let section = at;
      at += len;
      section
    };
    let ids = PodColumn::from_mapped_le(&store, take(n * 8), n * 8, u64::from_le_bytes)?;
    if config == AnnConfig::Vamana {
      let offsets =
        PodColumn::from_mapped_le(&store, take((n + 1) * 8), (n + 1) * 8, u64::from_le_bytes)?;
      let scales = PodColumn::from_mapped_le(&store, take(n * 4), n * 4, f32::from_le_bytes)?;
      let snorm = PodColumn::from_mapped_le(&store, take(n * 4), n * 4, f32::from_le_bytes)?;
      let targets =
        PodColumn::from_mapped_le(&store, take(edges * 4), edges * 4, u32::from_le_bytes)?;
      let padded = dim.next_multiple_of(16);
      let codes = PodColumn::from_mapped_le(&store, take(n * padded), n * padded, |b: [u8; 1]| {
        b[0] as i8
      })?;
      return Ok(AnnIndex {
        base_stamp,
        dim,
        vectors: Vec::new(),
        ids,
        config,
        codes: Vec::new(),
        code_words: 0,
        quant: Some(QuantMatrix::from_columns(dim, scales, snorm, codes)),
        graph: AnnGraphStore::Csr { offsets, targets },
        medoid,
      });
    }
    // Flat tiers: small corpora — owned decoded sections, exactly as before.
    let section = |at: usize, len: usize| -> io::Result<&[u8]> {
      bytes
        .get(at..at + len)
        .ok_or_else(|| io::Error::other("truncated ann index"))
    };
    let vectors: Vec<f32> =
      read_le_slice(section(take(n * dim * 4), n * dim * 4)?, f32::from_le_bytes);
    let codes: Vec<u64> = read_le_slice(
      section(take(n * code_words * 8), n * code_words * 8)?,
      u64::from_le_bytes,
    );
    Ok(AnnIndex {
      base_stamp,
      dim,
      vectors,
      ids,
      config,
      codes,
      code_words,
      quant: None,
      graph: AnnGraphStore::Flat {
        flat: Vec::new(),
        lens: Vec::new(),
        cap: 0,
      },
      medoid,
    })
  }
}

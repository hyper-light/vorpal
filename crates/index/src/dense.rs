//! The doc-side dense channel sidecar (ENCODER_RESEARCH §8.2, option 2): at warm
//! time the vendored CodeRankEmbed encoder embeds definition SURFACES (the
//! rerank's exact recipe — [`surface_of`]) through the throughput GEMM path and
//! persists them as int8 codes + per-row scale + f16 rows keyed by node id
//! (`ann.dense`), with provenance beside them (`ann.dense.json`). At query time
//! the fixed-order query embedding — computed ONCE and shared with the rerank —
//! is scanned against the codes and the bounded top rescored on the f16 rows
//! (`vorpal_ann::dense`), producing the FIFTH fused RRF list: the only channel
//! that can surface a paraphrase target the lexical/learned channels never
//! score (the coordinator's framing fact 1).
//!
//! COVERAGE RULE (no magic constants): the caller passes a wall-clock budget
//! (seconds; the CLI's `--dense-budget-secs`, or `<root>/dense.budget`). The
//! build embeds the first two batches in coverage order, takes the faster one as
//! this machine's measured seconds-per-TOKEN (the forward's cost law is linear
//! in tokens — a per-definition rate measured on the head of the order
//! overestimates: the highest-in-degree names are the shortest, 14 tokens per
//! surface at the head vs ~25 deeper in, and a per-definition rule overran a
//! 300 s budget by 43% on this repo), then covers the longest PREFIX of the
//! coverage order whose cumulative token count fits `budget / s_per_token` —
//! EVERY definition when the corpus fits the budget, else the top-N by
//! referential in-degree (the fusion's own degree channel), ties by id; only
//! the covered prefix is ever tokenized. Batches are [`BATCH_SEQUENCES`]
//! surfaces: the recorded sweep (examples/sweep_encoder.rs) saturates from 256
//! sequences on (1369 vs 1444 GFLOPS at 1024 — within 5%), and 26-sequence
//! batches run at 60% of that rate.
//!
//! DETERMINISM LAW: stamp-gated sidecar, never part of the generation id. The
//! encoder's throughput path measured bit-identical across rayon thread counts
//! (Stage A), and the int8 scan + f16 rescore are exact per row with a fixed
//! merge order, so a given sidecar answers identically at any thread count; the
//! sidecar's CONTENT depends on the measured throughput (N) — like `ann.calib`,
//! a machine measurement, recorded in the provenance. Freshness retrains on any
//! stamp, model-identity, surface-recipe, or format change, and on a larger
//! requested budget than the recorded one (unless coverage is already full).
//!
//! Every problem here is a stated degradation: an unreadable or stale sidecar
//! simply leaves the channel out (the fusion serves its four lists) — never a
//! failed search, never a panic.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use vorpal_ann::dense::{RESCORE_OVERSAMPLE, quantize_row, rescore_f16, row_to_f16, scan_i8};
use vorpal_ann::encoder::{CodeEncoder, GemmPath};
use vorpal_kg::{Kg, NodeId};

pub(crate) const SIDECAR_FILE: &str = "ann.dense";
pub(crate) const RECORD_FILE: &str = "ann.dense.json";
const MAGIC: &[u8; 4] = b"VDNS";
const VERSION: u32 = 1;
/// Fixed header: magic 4 + version 4 + stamp 8 + n 8 + dim 4 + pad 4.
const HEADER_BYTES: usize = 32;
/// The surface recipe label — the rerank's `format!("{name} {signature} {basename}")`.
pub(crate) const SURFACE_RECIPE: &str = "name signature basename";
/// Sequences per forward batch (module doc: the recorded sweep's saturation point).
pub(crate) const BATCH_SEQUENCES: usize = 256;

/// The provenance record beside the sidecar. Field order is canonical (BTreeMap
/// serialization) so rewrites are byte-reproducible.
#[derive(Clone, Debug)]
pub(crate) struct DenseRecord {
  pub stamp: u64,
  pub model_identity: u128,
  pub weights_digest: u128,
  pub surface: String,
  pub gemm_path: String,
  pub coverage: usize,
  pub population: usize,
  pub budget_secs: f64,
  /// This machine's measured encoder cost per token (the coverage rule's rate).
  pub measured_s_per_token: f64,
  /// Tokens the covered rows carry in total (coverage × mean surface length).
  pub covered_tokens: u64,
  pub build_secs: f64,
  /// Per-corpus channel verdict from the warm-time self-probe gate (`None` =
  /// not yet gated — healed on the next warm, like the BM25 verdict).
  pub gate: Option<bool>,
  pub gate_evidence: Option<String>,
}

pub(crate) fn read_record(dir: &Path) -> Option<DenseRecord> {
  let text = fs::read_to_string(dir.join(RECORD_FILE)).ok()?;
  let value: serde_json::Value = serde_json::from_str(&text).ok()?;
  let u64_of = |key: &str| value.get(key)?.as_u64();
  let f64_of = |key: &str| value.get(key)?.as_f64();
  let str_of = |key: &str| value.get(key)?.as_str().map(str::to_string);
  let hex_of = |key: &str| u128::from_str_radix(value.get(key)?.as_str()?, 16).ok();
  let gate = match value.get("gate") {
    None | Some(serde_json::Value::Null) => None,
    Some(serde_json::Value::Bool(enabled)) => Some(*enabled),
    Some(_) => return None,
  };
  Some(DenseRecord {
    stamp: u64_of("stamp")?,
    model_identity: hex_of("model_identity")?,
    weights_digest: hex_of("weights_digest")?,
    surface: str_of("surface")?,
    gemm_path: str_of("gemm_path")?,
    coverage: u64_of("coverage")? as usize,
    population: u64_of("population")? as usize,
    budget_secs: f64_of("budget_secs")?,
    measured_s_per_token: f64_of("measured_s_per_token")?,
    covered_tokens: u64_of("covered_tokens")?,
    build_secs: f64_of("build_secs")?,
    gate,
    gate_evidence: str_of("gate_evidence"),
  })
}

pub(crate) fn write_record(dir: &Path, record: &DenseRecord) -> std::io::Result<()> {
  let mut fields: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
  fields.insert("stamp", record.stamp.into());
  fields.insert("model_identity", format!("{:032x}", record.model_identity).into());
  fields.insert("weights_digest", format!("{:032x}", record.weights_digest).into());
  fields.insert("surface", record.surface.clone().into());
  fields.insert("gemm_path", record.gemm_path.clone().into());
  fields.insert("coverage", (record.coverage as u64).into());
  fields.insert("population", (record.population as u64).into());
  fields.insert("budget_secs", record.budget_secs.into());
  fields.insert("measured_s_per_token", record.measured_s_per_token.into());
  fields.insert("covered_tokens", record.covered_tokens.into());
  fields.insert("build_secs", record.build_secs.into());
  fields.insert("version", VERSION.into());
  if let Some(gate) = record.gate {
    fields.insert("gate", gate.into());
  }
  if let Some(evidence) = &record.gate_evidence {
    fields.insert("gate_evidence", evidence.clone().into());
  }
  let json = serde_json::to_string(&fields).map_err(std::io::Error::other)?;
  let tmp = dir.join(format!("{RECORD_FILE}.tmp"));
  fs::write(&tmp, format!("{json}\n"))?;
  fs::rename(tmp, dir.join(RECORD_FILE))
}

/// The ONE surface recipe shared by the sidecar build and the query-time rerank.
pub(crate) fn surface_of(view: &vorpal_kg::NodeView<'_>) -> String {
  let basename = view.path.rsplit('/').next().unwrap_or(view.path);
  format!("{} {} {basename}", view.name, view.signature)
}

/// Coverage order: the semantic row population (every non-Import node) by
/// referential in-degree descending, id ascending.
pub(crate) fn coverage_order(kg: &Kg) -> Vec<u64> {
  let mut rows: Vec<(usize, u64)> = crate::semantic_row_ids(kg)
    .into_iter()
    .map(|id| (kg.in_degree_referential(NodeId::new(id)), id))
    .collect();
  rows.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
  rows.into_iter().map(|(_, id)| id).collect()
}

/// Section offsets inside `ann.dense`, all 8-byte aligned.
struct Layout {
  ids: usize,
  scales: usize,
  codes: usize,
  halves: usize,
  end: usize,
}

fn layout(n: usize, dim: usize) -> Option<Layout> {
  let align = |at: usize| at.checked_add(7).map(|a| a & !7);
  let ids = HEADER_BYTES;
  let scales = align(ids.checked_add(n.checked_mul(8)?)?)?;
  let codes = align(scales.checked_add(n.checked_mul(4)?)?)?;
  let halves = align(codes.checked_add(n.checked_mul(dim)?)?)?;
  let end = align(halves.checked_add(n.checked_mul(dim)?.checked_mul(2)?)?)?;
  Some(Layout { ids, scales, codes, halves, end })
}

/// Build the sidecar under `budget_secs` (module doc: the coverage rule) and
/// commit `ann.dense` then `ann.dense.json` (tmp + rename each; the record is
/// the commit point — a crash between leaves a stale-by-construction pair).
pub(crate) fn build(
  kg: &Kg,
  dir: &Path,
  stamp: u64,
  encoder: &CodeEncoder,
  budget_secs: f64,
) -> Result<DenseRecord, String> {
  // A NaN budget fails this test too — it is not a positive number of seconds.
  if budget_secs.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
    return Err("dense sidecar: budget must be a positive number of seconds".to_string());
  }
  let started = std::time::Instant::now();
  let dim = encoder.dim();
  let order = coverage_order(kg);
  let population = order.len();
  if population == 0 {
    return Err("dense sidecar: no definitions to embed".to_string());
  }
  vorpal_kg::phase_stamp(&format!(
    "dense: population {population}, budget {budget_secs:.0}s, batch {BATCH_SEQUENCES}"
  ));
  let mut ids: Vec<u64> = Vec::new();
  let mut scales: Vec<f32> = Vec::new();
  let mut codes: Vec<i8> = Vec::new();
  let mut halves: Vec<u16> = Vec::new();
  // Per batch: (wall seconds, tokens) — the rate law's two measurements.
  let mut batch_cost: Vec<(f64, u64)> = Vec::new();
  let mut covered_tokens = 0u64;
  // Coverage is decided after the second batch (the first pays the page-in of
  // the 547 MB weights); until then the target is "at least these two".
  let mut target = population.min(2 * BATCH_SEQUENCES);
  let mut measured_s_per_token = 0.0f64;
  let mut cursor = 0usize;
  let surface_text = |id: u64| -> String {
    kg.node(NodeId::new(id))
      .map(|view| surface_of(&view))
      .unwrap_or_default()
  };
  while cursor < target {
    let end = (cursor + BATCH_SEQUENCES).min(target);
    let batch_ids = &order[cursor..end];
    let surfaces: Vec<String> = batch_ids.iter().map(|&id| surface_text(id)).collect();
    let texts: Vec<&str> = surfaces.iter().map(String::as_str).collect();
    let batch_tokens: u64 = texts.iter().map(|text| encoder.sequence_len(text) as u64).sum();
    let batch_started = std::time::Instant::now();
    let rows = encoder.embed_batch_with(&texts, GemmPath::Throughput)?;
    let elapsed = batch_started.elapsed().as_secs_f64();
    if rows.len() != batch_ids.len() {
      return Err("dense sidecar: encoder returned a short batch (invariant)".to_string());
    }
    for (&id, row) in batch_ids.iter().zip(&rows) {
      if row.len() != dim {
        return Err("dense sidecar: encoder row width disagrees with dim (invariant)".to_string());
      }
      let at = codes.len();
      codes.resize(at + dim, 0);
      halves.resize(at + dim, 0);
      scales.push(quantize_row(row, &mut codes[at..at + dim]));
      row_to_f16(row, &mut halves[at..at + dim]);
      ids.push(id);
    }
    batch_cost.push((elapsed, batch_tokens));
    covered_tokens += batch_tokens;
    cursor = end;
    if batch_cost.len() == 2 && population > target {
      // The coverage rule (module doc): the faster of the two first batches is
      // this machine's per-token rate; the covered prefix is the longest one
      // whose tokens fit the budget — never a shipped constant.
      let per_token = |(secs, tokens): (f64, u64)| secs / tokens.max(1) as f64;
      measured_s_per_token = per_token(batch_cost[0]).min(per_token(batch_cost[1]));
      let affordable_tokens = (budget_secs / measured_s_per_token).floor() as u64;
      let mut prefix_tokens = covered_tokens;
      let mut prefix = cursor;
      while prefix < population {
        let tokens = encoder.sequence_len(&surface_text(order[prefix])) as u64;
        if prefix_tokens + tokens > affordable_tokens {
          break;
        }
        prefix_tokens += tokens;
        prefix += 1;
      }
      target = prefix.max(target);
      vorpal_kg::phase_stamp(&format!(
        "dense: measured {:.0} tok/s ({:.1} tok/def at the head) → covering {target} of {population} ({:.1}%, {prefix_tokens} tokens)",
        1.0 / measured_s_per_token,
        covered_tokens as f64 / cursor as f64,
        target as f64 * 100.0 / population as f64
      ));
    }
    if batch_cost.len() % 64 == 0 {
      vorpal_kg::phase_stamp(&format!("dense: {cursor}/{target} embedded"));
    }
  }
  if measured_s_per_token == 0.0 {
    // Fewer than two batches (tiny corpus): the rate is the whole run's.
    let total: f64 = batch_cost.iter().map(|(secs, _)| secs).sum();
    measured_s_per_token = total / covered_tokens.max(1) as f64;
  }
  let n = ids.len();
  let layout = layout(n, dim).ok_or("dense sidecar: size overflows the layout")?;
  let mut bytes = vec![0u8; layout.end];
  bytes[0..4].copy_from_slice(MAGIC);
  bytes[4..8].copy_from_slice(&VERSION.to_le_bytes());
  bytes[8..16].copy_from_slice(&stamp.to_le_bytes());
  bytes[16..24].copy_from_slice(&(n as u64).to_le_bytes());
  bytes[24..28].copy_from_slice(&(dim as u32).to_le_bytes());
  bytes[layout.ids..layout.ids + n * 8].copy_from_slice(bytemuck::cast_slice(&ids));
  bytes[layout.scales..layout.scales + n * 4].copy_from_slice(bytemuck::cast_slice(&scales));
  bytes[layout.codes..layout.codes + n * dim].copy_from_slice(bytemuck::cast_slice(&codes));
  bytes[layout.halves..layout.halves + n * dim * 2].copy_from_slice(bytemuck::cast_slice(&halves));
  let tmp = dir.join(format!("{SIDECAR_FILE}.tmp"));
  {
    let mut file = fs::File::create(&tmp).map_err(|e| format!("dense sidecar: {e}"))?;
    file.write_all(&bytes).map_err(|e| format!("dense sidecar: {e}"))?;
    file.sync_all().map_err(|e| format!("dense sidecar: {e}"))?;
  }
  fs::rename(&tmp, dir.join(SIDECAR_FILE)).map_err(|e| format!("dense sidecar: {e}"))?;
  let record = DenseRecord {
    stamp,
    model_identity: encoder.model_identity(),
    weights_digest: encoder.weights_content_digest()?,
    surface: SURFACE_RECIPE.to_string(),
    gemm_path: GemmPath::Throughput.label().to_string(),
    coverage: n,
    population,
    budget_secs,
    measured_s_per_token,
    covered_tokens,
    build_secs: started.elapsed().as_secs_f64(),
    gate: None,
    gate_evidence: None,
  };
  write_record(dir, &record).map_err(|e| format!("dense sidecar record: {e}"))?;
  vorpal_kg::phase_stamp(&format!(
    "dense: committed {n} rows ({} bytes) in {:.1}s",
    layout.end,
    record.build_secs
  ));
  Ok(record)
}

/// Header check without mapping: `(n, dim)` when the file carries this format
/// and `stamp`.
fn peek_header(path: &Path, stamp: u64) -> Option<(usize, usize)> {
  let mut header = [0u8; HEADER_BYTES];
  let mut file = fs::File::open(path).ok()?;
  std::io::Read::read_exact(&mut file, &mut header).ok()?;
  if &header[0..4] != MAGIC || u32::from_le_bytes(header[4..8].try_into().ok()?) != VERSION {
    return None;
  }
  if u64::from_le_bytes(header[8..16].try_into().ok()?) != stamp {
    return None;
  }
  let n = u64::from_le_bytes(header[16..24].try_into().ok()?) as usize;
  let dim = u32::from_le_bytes(header[24..28].try_into().ok()?) as usize;
  Some((n, dim))
}

/// Freshness against the generation stamp, the opened encoder, the surface
/// recipe, and (when the caller states one) the requested budget — module doc.
pub(crate) fn is_fresh(
  dir: &Path,
  stamp: u64,
  encoder: &CodeEncoder,
  requested_budget: Option<f64>,
) -> bool {
  let Some(record) = read_record(dir) else {
    return false;
  };
  let Some((n, dim)) = peek_header(&dir.join(SIDECAR_FILE), stamp) else {
    return false;
  };
  record.stamp == stamp
    && record.model_identity == encoder.model_identity()
    && record.surface == SURFACE_RECIPE
    && record.coverage == n
    && dim == encoder.dim()
    && (record.coverage == record.population
      || requested_budget.is_none_or(|budget| budget <= record.budget_secs))
}

/// The mapped sidecar: sections viewed zero-copy from the mapping.
pub(crate) struct DenseSidecar {
  store: vorpal_mem::MappedStore,
  n: usize,
  dim: usize,
  layout: Layout,
}

impl DenseSidecar {
  /// Map `dir`'s sidecar when its header carries `stamp` and its sections fit
  /// the file; `None` is always safe (the channel stays out).
  pub(crate) fn load(dir: &Path, stamp: u64) -> Option<DenseSidecar> {
    let path = dir.join(SIDECAR_FILE);
    let (n, dim) = peek_header(&path, stamp)?;
    let layout = layout(n, dim)?;
    let store = vorpal_mem::MappedStore::map_file(
      &path,
      vorpal_mem::StoreKind::AnnCodes,
      vorpal_mem::AccessPattern::Sequential,
      vorpal_mem::Hotness::Hot,
      &vorpal_mem::ResourcePolicy::probe(vorpal_mem::CorpusProbe::new(0, 0)),
    )
    .ok()?;
    if store.as_bytes().len() < layout.end {
      return None;
    }
    Some(DenseSidecar { store, n, dim, layout })
  }

  pub(crate) fn len(&self) -> usize {
    self.n
  }

  /// The channel's ranked node ids for a fixed-order query embedding: int8 scan
  /// over `admit`ted rows to `RESCORE_OVERSAMPLE × pool`, f16 rescore, top `pool`.
  pub(crate) fn search(&self, query: &[f32], pool: usize, admit: &(dyn Fn(u64) -> bool + Sync)) -> Vec<u64> {
    if query.len() != self.dim || pool == 0 {
      return Vec::new();
    }
    let bytes = self.store.as_bytes();
    let (n, dim) = (self.n, self.dim);
    let Ok(ids) = bytemuck::try_cast_slice::<u8, u64>(&bytes[self.layout.ids..self.layout.ids + n * 8]) else {
      return Vec::new();
    };
    let Ok(scales) = bytemuck::try_cast_slice::<u8, f32>(&bytes[self.layout.scales..self.layout.scales + n * 4])
    else {
      return Vec::new();
    };
    let Ok(codes) = bytemuck::try_cast_slice::<u8, i8>(&bytes[self.layout.codes..self.layout.codes + n * dim])
    else {
      return Vec::new();
    };
    let Ok(halves) =
      bytemuck::try_cast_slice::<u8, u16>(&bytes[self.layout.halves..self.layout.halves + n * dim * 2])
    else {
      return Vec::new();
    };
    let mut q_codes = vec![0i8; dim];
    let q_scale = quantize_row(query, &mut q_codes);
    let take = pool.saturating_mul(RESCORE_OVERSAMPLE);
    let candidates = scan_i8(codes, scales, dim, &q_codes, q_scale, take, |row| admit(ids[row]));
    rescore_f16(halves, dim, query, &candidates)
      .into_iter()
      .take(pool)
      .map(|(row, _)| ids[row])
      .collect()
  }
}

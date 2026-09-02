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
//! ALWAYS ON, FILLED IN THE BACKGROUND (owner decision 2026-09-02): the channel
//! needs no gate, budget or per-index verdict — a fresh sidecar is served at
//! whatever coverage it holds, a missing one contributes nothing, and a search
//! never waits on the fill (the warm runs in the daemon's warm thread or the
//! detached autowarm child; the fill commits AFTER the core tiers' stamp).
//!
//! STOP RULE (data-derived): embed every non-Import definition something
//! references (referential in-degree ≥ 1), highest degree first, id ascending —
//! unreferenced definitions are not embedded. The fill is RESUMABLE: it commits
//! checkpoints (`ann.dense` + `ann.dense.json`, tmp + rename each) whenever the
//! rows added since the last checkpoint reach the rows already committed
//! (geometric doubling — total rewrite volume ≤ 2× the final file, no tunable
//! interval; the first lands after the two rate-probe batches), and a later warm
//! on the same stamp / model / recipe continues at the recorded coverage. A new
//! generation rebuilds (cross-generation carry is a recorded lead). An explicit
//! cap (`--dense-budget-timeout`, `<root>/dense.budget`; human durations) ends a
//! round early; `<root>/dense.channel = off` turns the channel off.
//!
//! Batches are [`BATCH_SEQUENCES`] surfaces: the recorded sweep
//! (examples/sweep_encoder.rs) saturates from 256 sequences on (1369 vs 1444
//! GFLOPS at 1024 — within 5%), and 26-sequence batches run at 60% of that rate.
//!
//! DETERMINISM LAW: stamp-gated sidecar, never part of the generation id. The
//! encoder's throughput path measured bit-identical across rayon thread counts
//! (Stage A), and the int8 scan + f16 rescore are exact per row with a fixed
//! merge order, so a given sidecar answers identically at any thread count; its
//! COVERAGE at a given moment depends on how long the fill has run — recorded in
//! the provenance. Freshness retrains on any stamp, model-identity,
//! surface-recipe, or format change.
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
/// Sequences per forward batch (module doc: the recorded sweep's saturation point).
pub(crate) const BATCH_SEQUENCES: usize = 256;

/// What text a definition presents to the encoder — ONE recipe for the sidecar
/// build and the query-time rerank (the one-recipe law: the rerank scores
/// candidates against the same surface the sidecar ranked them by). The label
/// rides in `ann.dense.json` and the freshness gate demands it, so a recipe
/// flip in code retrains every sidecar instead of serving mixed surfaces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SurfaceRecipe {
  /// `name signature basename` — the original rerank surface.
  Head,
  /// Head + the contiguous comment block immediately above the definition span
  /// (language family by extension; attribute/decorator lines between the
  /// comment and the definition are skipped).
  HeadDoc,
  /// HeadDoc + the head of the body: the definition span's first paragraph
  /// (lines up to the first blank line, or the span end) — for Python this is
  /// where the docstring lives.
  HeadDocBody,
}

impl SurfaceRecipe {
  pub(crate) fn label(self) -> &'static str {
    match self {
      SurfaceRecipe::Head => "name signature basename",
      SurfaceRecipe::HeadDoc => "name signature basename doc",
      SurfaceRecipe::HeadDocBody => "name signature basename doc body",
    }
  }

  fn reads_source(self) -> bool {
    !matches!(self, SurfaceRecipe::Head)
  }
}

/// The PINNED recipe (`Head`, the measured 2026-09-02 baseline) — the richer
/// recipes are the A/B this module records. `VORPAL_SURFACE_RECIPE=head|doc|body`
/// sweeps it under `bench-internals` only.
const ACTIVE_SURFACE: SurfaceRecipe = SurfaceRecipe::Head;

pub(crate) fn active_surface_recipe() -> SurfaceRecipe {
  #[cfg(feature = "bench-internals")]
  if let Ok(value) = std::env::var("VORPAL_SURFACE_RECIPE") {
    return match value.as_str() {
      "head" => SurfaceRecipe::Head,
      "doc" => SurfaceRecipe::HeadDoc,
      "body" => SurfaceRecipe::HeadDocBody,
      _ => ACTIVE_SURFACE,
    };
  }
  ACTIVE_SURFACE
}

/// Per-surface token cap for the richer recipes — DERIVED, not tuned: the largest
/// token matrix the recorded sweep validated on this forward (batch 4096 ×
/// 24.9 tok = 101,959 tokens, `examples/sweep_encoder.rs`) divided by the build's
/// [`BATCH_SEQUENCES`], so a batch of capped surfaces can never exceed a matrix
/// the path was measured on (memory law: the SwiGLU buffers are
/// `tokens × inner × 4 B × 2`). The encoder's own `max_trained_positions`
/// (2048) is the hard clamp above this. Truncations are counted per build.
pub(crate) const SURFACE_TOKEN_CAP: usize = 101_959 / BATCH_SEQUENCES;

/// The comment syntax a file's extension implies — the family whose line
/// markers `leading_comment` recognizes. Unknown families yield no comment (the
/// surface falls back to the head recipe for that definition, counted).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CommentFamily {
  Slash,
  Hash,
  Dash,
  Semicolon,
}

fn comment_family(path: &str) -> Option<CommentFamily> {
  let basename = path.rsplit('/').next().unwrap_or(path);
  if basename == "Makefile" || basename == "CMakeLists.txt" || basename == "Dockerfile" {
    return Some(CommentFamily::Hash);
  }
  let extension = basename.rsplit('.').next().filter(|ext| *ext != basename)?;
  Some(match extension {
    "c" | "h" | "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" | "rs" | "go" | "java" | "js" | "jsx"
    | "mjs" | "cjs" | "ts" | "tsx" | "kt" | "kts" | "scala" | "swift" | "cs" | "zig" | "m" | "mm"
    | "php" | "dart" | "groovy" | "proto" | "v" | "sv" | "vue" | "svelte" | "css" | "scss" => {
      CommentFamily::Slash
    }
    "py" | "pyi" | "rb" | "sh" | "bash" | "zsh" | "pl" | "pm" | "yaml" | "yml" | "toml" | "cmake"
    | "mk" | "r" | "jl" | "nix" | "tcl" | "ps1" | "ex" | "exs" | "cr" => CommentFamily::Hash,
    "lua" | "hs" | "sql" | "elm" | "ada" | "adb" | "ads" => CommentFamily::Dash,
    "lisp" | "el" | "clj" | "cljs" | "cljc" | "scm" | "rkt" => CommentFamily::Semicolon,
    _ => return None,
  })
}

/// Whether a trimmed line is a comment line in `family`; returns the text with
/// the markers stripped.
fn comment_text(line: &str, family: CommentFamily) -> Option<&str> {
  let stripped = match family {
    CommentFamily::Slash => line
      .strip_prefix("///")
      .or_else(|| line.strip_prefix("//!"))
      .or_else(|| line.strip_prefix("//"))
      .or_else(|| line.strip_prefix("/**"))
      .or_else(|| line.strip_prefix("/*"))
      .or_else(|| line.strip_prefix("*/").map(|_| ""))
      .or_else(|| line.strip_prefix('*'))?,
    CommentFamily::Hash => line.strip_prefix('#')?,
    CommentFamily::Dash => line.strip_prefix("--")?,
    CommentFamily::Semicolon => line.strip_prefix(';')?,
  };
  Some(
    stripped
      .trim_start_matches(['#', '!', '*', '-', ';', '/'])
      .trim_end_matches("*/")
      .trim(),
  )
}

/// Whether a trimmed line sits legitimately between a doc comment and its
/// definition (Rust attributes, Python/Java decorators, C# attributes) — skipped,
/// never breaking the block's contiguity.
fn is_attribute_line(line: &str) -> bool {
  line.starts_with("#[") || line.starts_with('@') || (line.starts_with('[') && line.ends_with(']'))
}

/// The contiguous comment block immediately above the line containing
/// `span_start`, markers stripped, whitespace collapsed. Empty when there is none
/// (or the family is unknown).
pub(crate) fn leading_comment(bytes: &[u8], span_start: usize, path: &str) -> String {
  let Some(family) = comment_family(path) else {
    return String::new();
  };
  let span_start = span_start.min(bytes.len());
  let definition_line = bytes[..span_start]
    .iter()
    .rposition(|&b| b == b'\n')
    .map_or(0, |nl| nl + 1);
  let mut lines: Vec<String> = Vec::new();
  let mut end = definition_line;
  while end > 0 {
    let start = bytes[..end - 1]
      .iter()
      .rposition(|&b| b == b'\n')
      .map_or(0, |nl| nl + 1);
    let line = String::from_utf8_lossy(&bytes[start..end - 1]);
    let trimmed = line.trim();
    if trimmed.is_empty() {
      // A blank line above the definition (before any comment) breaks contiguity.
      break;
    }
    if is_attribute_line(trimmed) {
      end = start;
      continue;
    }
    match comment_text(trimmed, family) {
      Some(text) => {
        if !text.is_empty() {
          lines.push(text.to_string());
        }
      }
      None => break,
    }
    end = start;
  }
  lines.reverse();
  lines.join(" ")
}

/// The definition span's first paragraph: its lines up to (not including) the
/// first blank line, or the span end.
pub(crate) fn body_head(bytes: &[u8], span: (usize, usize)) -> String {
  let end = span.1.min(bytes.len());
  let start = span.0.min(end);
  let text = String::from_utf8_lossy(&bytes[start..end]);
  let mut out = String::new();
  for line in text.lines() {
    let trimmed = line.trim();
    if trimmed.is_empty() {
      break;
    }
    if !out.is_empty() {
      out.push(' ');
    }
    out.push_str(trimmed);
  }
  out
}

/// Builds candidate surfaces under the active recipe — shared by the sidecar
/// build and the rerank. Source bytes come through the indexed-source read
/// (digest-verified against the generation's product pack; a changed or
/// unreadable file falls back to the head recipe for that definition, counted),
/// cached per path for the builder's life (a build touches each covered file
/// once per batch order; the corpus's covered files bound the cache).
pub(crate) struct SurfaceBuilder {
  recipe: SurfaceRecipe,
  pack: Option<std::sync::Arc<crate::PackReader>>,
  files: std::collections::HashMap<String, Option<Vec<u8>>>,
  /// Definitions that fell back to the head recipe (no span, no readable/verified
  /// source, unknown comment family AND empty body head).
  pub fallbacks: usize,
  /// Definitions whose extra text was cut to fit [`SURFACE_TOKEN_CAP`].
  pub truncations: usize,
}

impl SurfaceBuilder {
  pub(crate) fn new(generation_dir: &Path, recipe: SurfaceRecipe) -> SurfaceBuilder {
    SurfaceBuilder {
      recipe,
      pack: recipe.reads_source().then(|| crate::cached_pack(generation_dir)).flatten(),
      files: std::collections::HashMap::new(),
      fallbacks: 0,
      truncations: 0,
    }
  }

  pub(crate) fn recipe(&self) -> SurfaceRecipe {
    self.recipe
  }

  fn file_bytes(&mut self, path: &str) -> Option<&[u8]> {
    if !self.files.contains_key(path) {
      let read = match crate::read_indexed_source_with(self.pack.as_deref(), path) {
        Ok(crate::IndexedRead::Verified(bytes)) | Ok(crate::IndexedRead::Unverified(bytes)) => Some(bytes),
        Ok(crate::IndexedRead::Changed) | Err(_) => None,
      };
      self.files.insert(path.to_string(), read);
    }
    self.files.get(path).and_then(|bytes| bytes.as_deref())
  }

  /// The surface for node `id`, fitted to the token cap through `encoder`.
  pub(crate) fn surface(&mut self, kg: &Kg, id: u64, encoder: &CodeEncoder) -> String {
    let Some(view) = kg.node(NodeId::new(id)) else {
      return String::new();
    };
    let head = surface_of(&view);
    if !self.recipe.reads_source() {
      return head;
    }
    let (start, end) = (view.span.0 as usize, view.span.1 as usize);
    let (path, recipe) = (view.path.to_string(), self.recipe);
    let extra = if end <= start {
      None
    } else {
      self.file_bytes(&path).map(|bytes| {
        let mut extra = leading_comment(bytes, start, &path);
        if recipe == SurfaceRecipe::HeadDocBody {
          let body = body_head(bytes, (start, end));
          if !body.is_empty() {
            if !extra.is_empty() {
              extra.push(' ');
            }
            extra.push_str(&body);
          }
        }
        extra
      })
    };
    match extra {
      Some(extra) if !extra.is_empty() => self.fit(encoder, head, extra),
      _ => {
        self.fallbacks += 1;
        head
      }
    }
  }

  /// `head + extra` cut so the whole surface fits [`SURFACE_TOKEN_CAP`]: each
  /// pass scales the extra text by the measured tokens-over-cap ratio (at a
  /// char boundary) until it fits — converges in a few passes since the ratio
  /// strictly shrinks the text.
  fn fit(&mut self, encoder: &CodeEncoder, head: String, mut extra: String) -> String {
    let mut surface = format!("{head} {extra}");
    let mut tokens = encoder.sequence_len(&surface);
    if tokens <= SURFACE_TOKEN_CAP {
      return surface;
    }
    self.truncations += 1;
    while tokens > SURFACE_TOKEN_CAP && !extra.is_empty() {
      let keep = (extra.len() * SURFACE_TOKEN_CAP / tokens).min(extra.len().saturating_sub(1));
      let boundary = extra
        .char_indices()
        .map(|(at, _)| at)
        .take_while(|&at| at <= keep)
        .last()
        .unwrap_or(0);
      extra.truncate(boundary);
      surface = if extra.is_empty() { head.clone() } else { format!("{head} {extra}") };
      tokens = encoder.sequence_len(&surface);
    }
    surface
  }
}

/// The provenance record beside the sidecar — also the fill's CHECKPOINT: `coverage`
/// rows of the deterministic coverage order are committed, `complete` says whether
/// the referenced population is exhausted, and a later warm on the same stamp /
/// model / recipe resumes at `coverage`. Field order is canonical (BTreeMap
/// serialization) so rewrites are byte-reproducible.
#[derive(Clone, Debug)]
pub(crate) struct DenseRecord {
  pub stamp: u64,
  pub model_identity: u128,
  pub weights_digest: u128,
  pub surface: String,
  pub gemm_path: String,
  /// Rows committed — a prefix of the coverage order.
  pub coverage: usize,
  /// The stop rule's population: non-Import definitions with referential in-degree ≥ 1.
  pub referenced: usize,
  /// Every non-Import definition (the referenced fraction's denominator).
  pub population: usize,
  /// `coverage == referenced` (or the explicit cap ended the fill — see `capped`).
  pub complete: bool,
  /// The explicit cap that ended the last fill round, if any (seconds).
  pub cap_secs: Option<f64>,
  /// This machine's measured encoder cost per token (the extrapolation rate).
  pub measured_s_per_token: f64,
  /// Tokens the covered rows carry in total (coverage × mean surface length).
  pub covered_tokens: u64,
  /// Definitions that fell back to the head recipe (richer recipes only).
  pub surface_fallbacks: u64,
  /// Definitions whose extra text was cut to [`SURFACE_TOKEN_CAP`].
  pub surface_truncations: u64,
  /// Embedding wall time accumulated over every fill round (resumes add to it).
  pub fill_secs: f64,
  /// Checkpoints committed over the sidecar's life.
  pub checkpoints: u64,
}

pub(crate) fn read_record(dir: &Path) -> Option<DenseRecord> {
  let text = fs::read_to_string(dir.join(RECORD_FILE)).ok()?;
  let value: serde_json::Value = serde_json::from_str(&text).ok()?;
  let u64_of = |key: &str| value.get(key)?.as_u64();
  let f64_of = |key: &str| value.get(key)?.as_f64();
  let str_of = |key: &str| value.get(key)?.as_str().map(str::to_string);
  let hex_of = |key: &str| u128::from_str_radix(value.get(key)?.as_str()?, 16).ok();
  let coverage = u64_of("coverage")? as usize;
  let population = u64_of("population")? as usize;
  // Records written by the budgeted (pre-fill) builder carry no stop-rule fields:
  // they stay SERVABLE (same rows, same freshness keys) and the next fill starts
  // over because their `referenced` cannot match the stop rule's count.
  Some(DenseRecord {
    stamp: u64_of("stamp")?,
    model_identity: hex_of("model_identity")?,
    weights_digest: hex_of("weights_digest")?,
    surface: str_of("surface")?,
    gemm_path: str_of("gemm_path")?,
    coverage,
    referenced: u64_of("referenced").map_or(population, |n| n as usize),
    population,
    complete: value
      .get("complete")
      .and_then(|v| v.as_bool())
      .unwrap_or(coverage == population),
    cap_secs: f64_of("cap_secs").or_else(|| f64_of("budget_secs")),
    measured_s_per_token: f64_of("measured_s_per_token")?,
    covered_tokens: u64_of("covered_tokens")?,
    surface_fallbacks: u64_of("surface_fallbacks").unwrap_or(0),
    surface_truncations: u64_of("surface_truncations").unwrap_or(0),
    fill_secs: f64_of("fill_secs").or_else(|| f64_of("build_secs"))?,
    checkpoints: u64_of("checkpoints").unwrap_or(0),
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
  fields.insert("referenced", (record.referenced as u64).into());
  fields.insert("population", (record.population as u64).into());
  fields.insert("complete", record.complete.into());
  if let Some(cap) = record.cap_secs {
    fields.insert("cap_secs", cap.into());
    fields.insert("cap", crate::duration::render_duration(cap).into());
  }
  fields.insert("measured_s_per_token", record.measured_s_per_token.into());
  fields.insert("covered_tokens", record.covered_tokens.into());
  fields.insert("surface_fallbacks", record.surface_fallbacks.into());
  fields.insert("surface_truncations", record.surface_truncations.into());
  fields.insert("fill_secs", record.fill_secs.into());
  fields.insert("checkpoints", record.checkpoints.into());
  fields.insert("version", VERSION.into());
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

/// Coverage order (the stop rule): every non-Import definition that something
/// references (referential in-degree ≥ 1), highest degree first, id ascending —
/// deterministic for a generation, so a resumed fill continues the same prefix.
/// Returns `(order, population)` with `population` = every non-Import definition.
pub(crate) fn coverage_order(kg: &Kg) -> (Vec<u64>, usize) {
  let rows = crate::semantic_row_ids(kg);
  let population = rows.len();
  let mut referenced: Vec<(usize, u64)> = rows
    .into_iter()
    .map(|id| (kg.in_degree_referential(NodeId::new(id)), id))
    .filter(|(degree, _)| *degree >= 1)
    .collect();
  referenced.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
  (referenced.into_iter().map(|(_, id)| id).collect(), population)
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

/// The rows a fill accumulates (and resumes from): parallel to the coverage order.
struct Rows {
  ids: Vec<u64>,
  scales: Vec<f32>,
  codes: Vec<i8>,
  halves: Vec<u16>,
}

impl Rows {
  fn len(&self) -> usize {
    self.ids.len()
  }
}

/// Serialize `rows` as `ann.dense` (tmp + fsync + rename) — the checkpoint write.
/// Returns the file size.
fn write_sidecar(dir: &Path, stamp: u64, dim: usize, rows: &Rows) -> Result<usize, String> {
  let n = rows.len();
  let layout = layout(n, dim).ok_or("dense sidecar: size overflows the layout")?;
  let mut bytes = vec![0u8; layout.end];
  bytes[0..4].copy_from_slice(MAGIC);
  bytes[4..8].copy_from_slice(&VERSION.to_le_bytes());
  bytes[8..16].copy_from_slice(&stamp.to_le_bytes());
  bytes[16..24].copy_from_slice(&(n as u64).to_le_bytes());
  bytes[24..28].copy_from_slice(&(dim as u32).to_le_bytes());
  bytes[layout.ids..layout.ids + n * 8].copy_from_slice(bytemuck::cast_slice(&rows.ids));
  bytes[layout.scales..layout.scales + n * 4].copy_from_slice(bytemuck::cast_slice(&rows.scales));
  bytes[layout.codes..layout.codes + n * dim].copy_from_slice(bytemuck::cast_slice(&rows.codes));
  bytes[layout.halves..layout.halves + n * dim * 2]
    .copy_from_slice(bytemuck::cast_slice(&rows.halves));
  let tmp = dir.join(format!("{SIDECAR_FILE}.tmp"));
  {
    let mut file = fs::File::create(&tmp).map_err(|e| format!("dense sidecar: {e}"))?;
    file.write_all(&bytes).map_err(|e| format!("dense sidecar: {e}"))?;
    file.sync_all().map_err(|e| format!("dense sidecar: {e}"))?;
  }
  fs::rename(&tmp, dir.join(SIDECAR_FILE)).map_err(|e| format!("dense sidecar: {e}"))?;
  Ok(layout.end)
}

/// Read a coherent sidecar's rows back for a resume. `None` (unreadable, or the
/// rows are not the coverage order's prefix — a foreign or corrupt file) means
/// start over.
fn read_rows(dir: &Path, stamp: u64, dim: usize, order: &[u64]) -> Option<Rows> {
  let sidecar = DenseSidecar::load(dir, stamp)?;
  if sidecar.dim != dim || sidecar.n > order.len() {
    return None;
  }
  let (ids, scales, codes, halves) = sidecar.sections()?;
  if ids != &order[..ids.len()] {
    return None;
  }
  Some(Rows {
    ids: ids.to_vec(),
    scales: scales.to_vec(),
    codes: codes.to_vec(),
    halves: halves.to_vec(),
  })
}

/// Commit a checkpoint: rows first, then the record (the record is the commit
/// point — a crash between leaves a header/record mismatch that `fresh_record`
/// rejects, and the next fill starts over).
fn checkpoint(
  dir: &Path,
  stamp: u64,
  dim: usize,
  rows: &Rows,
  record: &mut DenseRecord,
) -> Result<usize, String> {
  let bytes = write_sidecar(dir, stamp, dim, rows)?;
  record.coverage = rows.len();
  record.checkpoints += 1;
  write_record(dir, record).map_err(|e| format!("dense sidecar record: {e}"))?;
  Ok(bytes)
}

/// The background fill (module doc): embed the referenced population in coverage
/// order, RESUMING from a coherent checkpoint on the same stamp/model/recipe,
/// committing a checkpoint whenever the rows added since the last one match the
/// rows already committed (geometric doubling — the total rewrite volume stays
/// within 2× the final sidecar with no tunable interval; the first checkpoint
/// lands after the two rate-probe batches so a search sees coverage within
/// seconds), and stopping at the population's end or at `cap_secs` of this
/// round's wall clock (the explicit override). Returns the record as committed.
pub(crate) fn fill(
  kg: &Kg,
  dir: &Path,
  stamp: u64,
  encoder: &CodeEncoder,
  cap_secs: Option<f64>,
) -> Result<DenseRecord, String> {
  let round_started = std::time::Instant::now();
  let dim = encoder.dim();
  let (order, population) = coverage_order(kg);
  let referenced = order.len();
  let mut builder = SurfaceBuilder::new(dir, active_surface_recipe());
  let recipe = builder.recipe().label().to_string();
  // Resume: a coherent checkpoint on this stamp/model/recipe continues; anything
  // else (older format, other recipe, torn pair) starts over.
  let resumed = fresh_record(dir, stamp, encoder)
    .filter(|record| record.surface == recipe && record.referenced == referenced)
    .and_then(|record| read_rows(dir, stamp, dim, &order).map(|rows| (record, rows)));
  let (mut record, mut rows) = match resumed {
    Some((record, rows)) => (record, rows),
    None => (
      DenseRecord {
        stamp,
        model_identity: encoder.model_identity(),
        weights_digest: encoder.weights_content_digest()?,
        surface: recipe.clone(),
        gemm_path: GemmPath::Throughput.label().to_string(),
        coverage: 0,
        referenced,
        population,
        complete: referenced == 0,
        cap_secs: None,
        measured_s_per_token: 0.0,
        covered_tokens: 0,
        surface_fallbacks: 0,
        surface_truncations: 0,
        fill_secs: 0.0,
        checkpoints: 0,
      },
      Rows { ids: Vec::new(), scales: Vec::new(), codes: Vec::new(), halves: Vec::new() },
    ),
  };
  vorpal_kg::phase_stamp(&format!(
    "dense: referenced {referenced} of {population} definitions ({:.1}%), resuming at {} (recipe {recipe:?}{})",
    referenced as f64 * 100.0 / population.max(1) as f64,
    rows.len(),
    cap_secs.map_or(String::new(), |cap| format!(", cap {}", crate::duration::render_duration(cap))),
  ));
  if rows.len() >= referenced {
    record.complete = true;
    record.cap_secs = cap_secs;
    checkpoint(dir, stamp, dim, &rows, &mut record)?;
    return Ok(record);
  }
  let mut committed = rows.len();
  let mut round_batches = 0usize;
  let mut round_tokens = 0u64;
  let mut round_embed_secs = 0.0f64;
  let mut fastest_per_token = f64::INFINITY;
  let mut cursor = rows.len();
  // Producer/consumer: while batch i runs its forward, batch i+1's surfaces are
  // built and tokenized on a scoped thread (the builder is the producer's; its
  // counters are folded in per batch), so the encoder never idles on surface
  // construction, file reads, or tokenization. Finished rows quantize in
  // parallel across rows (each row's quantization is independent — exact).
  let prepare = |builder: &mut SurfaceBuilder, from: usize| -> (Vec<String>, u64, usize, usize) {
    let end = (from + BATCH_SEQUENCES).min(referenced);
    let surfaces: Vec<String> = order[from..end]
      .iter()
      .map(|&id| builder.surface(kg, id, encoder))
      .collect();
    let tokens: u64 = surfaces.iter().map(|text| encoder.sequence_len(text) as u64).sum();
    let (fallbacks, truncations) = (builder.fallbacks, builder.truncations);
    builder.fallbacks = 0;
    builder.truncations = 0;
    (surfaces, tokens, fallbacks, truncations)
  };
  let mut next = prepare(&mut builder, cursor);
  loop {
    let end = (cursor + BATCH_SEQUENCES).min(referenced);
    let batch_ids = &order[cursor..end];
    let (surfaces, batch_tokens, fallbacks, truncations) = next;
    let texts: Vec<&str> = surfaces.iter().map(String::as_str).collect();
    let batch_started = std::time::Instant::now();
    let (embedded, prepared) = std::thread::scope(|scope| {
      let producer = (end < referenced).then(|| {
        let builder = &mut builder;
        scope.spawn(move || prepare(builder, end))
      });
      let embedded = encoder.embed_batch_with(&texts, GemmPath::Throughput);
      let prepared = producer.map(|handle| handle.join());
      (embedded, prepared)
    });
    let embedded = embedded?;
    let elapsed = batch_started.elapsed().as_secs_f64();
    next = match prepared {
      Some(Ok(prepared)) => prepared,
      Some(Err(_)) => return Err("dense sidecar: surface producer panicked".to_string()),
      None => (Vec::new(), 0, 0, 0),
    };
    if embedded.len() != batch_ids.len() {
      return Err("dense sidecar: encoder returned a short batch (invariant)".to_string());
    }
    if embedded.iter().any(|row| row.len() != dim) {
      return Err("dense sidecar: encoder row width disagrees with dim (invariant)".to_string());
    }
    let at = rows.codes.len();
    let added = embedded.len();
    rows.codes.resize(at + added * dim, 0);
    rows.halves.resize(at + added * dim, 0);
    let mut scales = vec![0.0f32; added];
    {
      use rayon::prelude::*;
      rows.codes[at..]
        .par_chunks_exact_mut(dim)
        .zip(rows.halves[at..].par_chunks_exact_mut(dim))
        .zip(scales.par_iter_mut())
        .zip(embedded.par_iter())
        .for_each(|(((codes, halves), scale), row)| {
          *scale = quantize_row(row, codes);
          row_to_f16(row, halves);
        });
    }
    rows.scales.extend(scales);
    rows.ids.extend_from_slice(batch_ids);
    record.surface_fallbacks += fallbacks as u64;
    record.surface_truncations += truncations as u64;
    cursor = end;
    round_batches += 1;
    round_tokens += batch_tokens;
    round_embed_secs += elapsed;
    record.covered_tokens += batch_tokens;
    record.fill_secs += elapsed;
    // The second batch onward measures the rate (the first pays the weights' page-in).
    if round_batches >= 2 {
      fastest_per_token = fastest_per_token.min(elapsed / batch_tokens.max(1) as f64);
      record.measured_s_per_token = fastest_per_token;
    }
    let exhausted = cursor >= referenced;
    let capped = cap_secs.is_some_and(|cap| round_started.elapsed().as_secs_f64() >= cap);
    let due = rows.len() - committed >= committed.max(2 * BATCH_SEQUENCES);
    if exhausted || capped || due {
      record.complete = exhausted;
      record.cap_secs = if capped { cap_secs } else { None };
      let bytes = checkpoint(dir, stamp, dim, &rows, &mut record)?;
      committed = rows.len();
      vorpal_kg::phase_stamp(&format!(
        "dense: checkpoint {} — {committed} of {referenced} rows ({:.1}%), {bytes} bytes, {:.0} tok/s this round, {:.1} tok/def{}",
        record.checkpoints,
        committed as f64 * 100.0 / referenced.max(1) as f64,
        round_tokens as f64 / round_embed_secs.max(1e-9),
        record.covered_tokens as f64 / committed.max(1) as f64,
        if exhausted { " — complete" } else if capped { " — cap reached" } else { "" },
      ));
      if exhausted || capped {
        break;
      }
    }
  }
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

/// The record when the sidecar on disk is coherent for this generation, encoder
/// and recipe (stamp, model identity, recipe label, header row count = record
/// coverage, dim) — servable as-is at whatever coverage it holds, and resumable.
pub(crate) fn fresh_record(dir: &Path, stamp: u64, encoder: &CodeEncoder) -> Option<DenseRecord> {
  let record = read_record(dir)?;
  let (n, dim) = peek_header(&dir.join(SIDECAR_FILE), stamp)?;
  (record.stamp == stamp
    && record.model_identity == encoder.model_identity()
    && record.surface == active_surface_recipe().label()
    && record.coverage == n
    && dim == encoder.dim())
  .then_some(record)
}

/// Freshness — [`fresh_record`] as a predicate.
pub(crate) fn is_fresh(dir: &Path, stamp: u64, encoder: &CodeEncoder) -> bool {
  fresh_record(dir, stamp, encoder).is_some()
}

/// The sidecar's four sections as typed views: `(ids, scales, codes, halves)`.
type Sections<'a> = (&'a [u64], &'a [f32], &'a [i8], &'a [u16]);

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

  /// The four sections as typed slices — `None` if the mapping is misaligned.
  fn sections(&self) -> Option<Sections<'_>> {
    let bytes = self.store.as_bytes();
    let (n, dim) = (self.n, self.dim);
    let ids = bytemuck::try_cast_slice::<u8, u64>(&bytes[self.layout.ids..self.layout.ids + n * 8]).ok()?;
    let scales =
      bytemuck::try_cast_slice::<u8, f32>(&bytes[self.layout.scales..self.layout.scales + n * 4]).ok()?;
    let codes =
      bytemuck::try_cast_slice::<u8, i8>(&bytes[self.layout.codes..self.layout.codes + n * dim]).ok()?;
    let halves =
      bytemuck::try_cast_slice::<u8, u16>(&bytes[self.layout.halves..self.layout.halves + n * dim * 2])
        .ok()?;
    Some((ids, scales, codes, halves))
  }

  /// The channel's ranked node ids for a fixed-order query embedding: int8 scan
  /// over `admit`ted rows to `RESCORE_OVERSAMPLE × pool`, f16 rescore, top `pool`.
  pub(crate) fn search(&self, query: &[f32], pool: usize, admit: &(dyn Fn(u64) -> bool + Sync)) -> Vec<u64> {
    if query.len() != self.dim || pool == 0 {
      return Vec::new();
    }
    let dim = self.dim;
    let Some((ids, scales, codes, halves)) = self.sections() else {
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

#[cfg(test)]
mod surface_tests {
  use super::{body_head, leading_comment};

  #[test]
  fn rust_doc_comment_block_is_collected_over_attributes() {
    let src = b"use x;\n\n/// Detect near-duplicate code.\n/// Second line.\n#[inline]\npub fn similar_pairs() {}\n";
    let at = String::from_utf8_lossy(src).find("pub fn").unwrap();
    assert_eq!(leading_comment(src, at, "a/b.rs"), "Detect near-duplicate code. Second line.");
  }

  #[test]
  fn blank_line_breaks_contiguity_and_unknown_family_yields_nothing() {
    let src = b"// far away\n\nfn f() {}\n";
    let at = String::from_utf8_lossy(src).find("fn f").unwrap();
    assert_eq!(leading_comment(src, at, "a.rs"), "");
    let py = b"# a hash comment\ndef g():\n    pass\n";
    let at = String::from_utf8_lossy(py).find("def g").unwrap();
    assert_eq!(leading_comment(py, at, "m.py"), "a hash comment");
    assert_eq!(leading_comment(py, at, "m.unknownext"), "");
    // A C preprocessor line is NOT a comment in the slash family.
    let c = b"#define X 1\nint h(void) {}\n";
    let at = String::from_utf8_lossy(c).find("int h").unwrap();
    assert_eq!(leading_comment(c, at, "h.c"), "");
  }

  #[test]
  fn c_block_comment_markers_are_stripped() {
    let src = b"/**\n * Allocate a socket buffer.\n * @size: bytes\n */\nstruct sk_buff *alloc_skb(void);\n";
    let at = String::from_utf8_lossy(src).find("struct sk_buff").unwrap();
    assert_eq!(leading_comment(src, at, "skbuff.h"), "Allocate a socket buffer. @size: bytes");
  }

  #[test]
  fn body_head_is_the_first_paragraph_of_the_span() {
    let src = b"def ingest(path):\n    \"\"\"Load folded stacks.\"\"\"\n    x = 1\n\n    return x\n";
    let end = src.len();
    assert_eq!(
      body_head(src, (0, end)),
      "def ingest(path): \"\"\"Load folded stacks.\"\"\" x = 1"
    );
    assert_eq!(body_head(src, (end, end)), "");
  }
}

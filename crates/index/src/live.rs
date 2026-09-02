//! The daemon's live overlay (SUBSECOND.md Phase 3): a retained in-memory pipeline state
//! that turns a small semantic edit into a sealed, scratch-identical graph WITHOUT replaying
//! the corpus — apply the changed files, re-link derived state, canonical-order seal.
//!
//! Built once from the committed generation (manifest-ordered pack + loose overlay) on a
//! background thread; each edit then costs extract(changed) + masked table + resolve + seal
//! instead of decode+absorb of every product. The sealed bytes are pinned byte-identical to
//! a from-scratch build of the same tree (crates/ingest/tests/retained.rs and
//! crates/kg/tests/canonical_seal.rs), so the daemon's background canonicalizer commits the
//! very generation these answers came from.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use std::sync::Arc;

use vorpal_ingest::{
  FileStat, Manifest, OutlineExtractor, PackMsg, PackReader, PackWriter, RetainedIndex,
  Resolver, cache_file_name, encode_product_into, peek_product_grammar_digest,
};
use vorpal_kg::Kg;

/// One hinted path's probed state: the daemon extracts each changed file ONCE, decides the
/// serve-immediately question with it, and hands the same bytes to the overlay — the
/// probe→overlay double extraction was ~10-15ms of every semantic serve.
pub enum ProbedPath {
  /// Gone from disk — the overlay retracts it.
  Vanished,
  /// Not an extractable source file: inert for the unchanged check, skipped by the overlay
  /// (unless it was indexed before, which the overlay treats as its own failure signal).
  Unhandled,
  /// Fresh extraction, encoded (stamps zeroed). `matches_cache` = byte-identical to the
  /// cached product outside the stamp window `[8..32)` — the serve-immediately criterion.
  /// `size`/`mtime_ns` are the probe-time stat, derived exactly as the manifest scan does —
  /// they stamp the product and the manifest entry when a served build persists.
  Extracted {
    bytes: Vec<u8>,
    matches_cache: bool,
    size: u64,
    mtime_ns: u64,
  },
  /// Unreadable or unextractable — callers take the full pipeline.
  Failed,
}

pub struct ExtractionProbe {
  pub per_path: Vec<(PathBuf, ProbedPath)>,
}

impl ExtractionProbe {
  /// The serve-immediately criterion, byte-equivalent to `extraction_unchanged`: every
  /// probed path is either inert (unhandled) or extracted byte-identical to its cached
  /// product. Vanished and failed paths can change answers, so they refuse.
  pub fn all_unchanged(&self) -> bool {
    !self.per_path.is_empty()
      && self.per_path.iter().all(|(_, probed)| match probed {
        ProbedPath::Unhandled => true,
        ProbedPath::Extracted { matches_cache, .. } => *matches_cache,
        ProbedPath::Vanished | ProbedPath::Failed => false,
      })
  }
}

/// Extract every hinted path once, comparing against the committed generation's cached
/// products. The decision logic mirrors `vorpal_index::extraction_unchanged`; the extracted
/// bytes ride along for [`LiveOverlay::apply_and_link_probed`]. `src` is the watched tree
/// root — the bucketed pack's stripping root for the absolute paths probed here.
pub fn probe_extraction(
  index_dir: &Path,
  src: &Path,
  paths: &HashSet<PathBuf>,
) -> Result<ExtractionProbe, String> {
  let generation = vorpal_kg::resolve_index_dir(index_dir);
  let tree_root = src
    .canonicalize()
    .unwrap_or_else(|_| src.to_path_buf())
    .to_string_lossy()
    .into_owned();
  let pack = PackReader::open_rooted(&generation, Some(&tree_root));
  let extractor =
    OutlineExtractor::new().map_err(|err| format!("probe: extractor init failed: {err}"))?;
  let mut ordered: Vec<&PathBuf> = paths.iter().collect();
  ordered.sort();
  let mut per_path = Vec::with_capacity(ordered.len());
  for path in ordered {
    let key = path.to_string_lossy();
    let probed = if !path.exists() {
      ProbedPath::Vanished
    } else if !extractor.handles(&key) {
      ProbedPath::Unhandled
    } else {
      // Stat before read — the pipeline's scan-then-parse ordering, same TOCTOU class.
      let stat = fs::metadata(path).ok().map(|meta| {
        let mtime_ns = meta
          .modified()
          .ok()
          .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
          .map(|d| d.as_nanos() as u64)
          .unwrap_or(0);
        (meta.len(), mtime_ns)
      });
      match (stat, fs::read_to_string(path)) {
        (Some((size, mtime_ns)), Ok(source)) => {
          match extractor.extract_product(&key, &source) {
            None => ProbedPath::Failed,
            Some(product) => {
              let mut bytes = Vec::new();
              encode_product_into(&product, &mut bytes);
              let matches_cache = pack
                .as_ref()
                .and_then(|p| p.get(&key))
                .is_some_and(|cached| {
                  bytes.len() == cached.len()
                    && bytes.len() >= 32
                    && bytes[0..8] == cached[0..8]
                    && bytes[32..] == cached[32..]
                });
              ProbedPath::Extracted {
                bytes,
                matches_cache,
                size,
                mtime_ns,
              }
            }
          }
        }
        _ => ProbedPath::Failed,
      }
    };
    per_path.push((path.clone(), probed));
  }
  Ok(ExtractionProbe { per_path })
}

pub struct LiveOverlay {
  interner: vorpal_ingest::Interner,
  index: RetainedIndex,
  extractor: OutlineExtractor,
  /// The current tree's manifest, maintained per absorb (probe-time stats) — a served
  /// build's persistence writes it verbatim, byte-equal to what the pipeline's
  /// `patch_manifest` would produce from the same stats.
  manifest: Manifest,
  /// The watched source root — the co-change pass consults its git history per link (the
  /// HEAD-keyed cache makes that a file read between commits).
  src: PathBuf,
  /// The index ROOT (not the generation): where `cochange.cache` lives.
  index_dir: PathBuf,
}

impl LiveOverlay {
  /// Build from the generation `CURRENT` names under `index_dir`: every manifest path's
  /// product, from the pack or the loose bank, in manifest (path-sorted) order. Heavy —
  /// one full product replay — so callers run it on a background thread. Refuses to build
  /// when the extraction identity changed since the generation was committed (a grammar or
  /// outline-rule edit): mixing old products with new extraction would fork answers.
  pub fn build(index_dir: &Path, src: &Path) -> Result<Self, String> {
    vorpal_kg::phase_stamp("overlay: build start");
    let generation = vorpal_kg::resolve_index_dir(index_dir);
    let manifest = Manifest::load(&generation.join("manifest.bin"))
      .map_err(|err| format!("overlay: manifest load failed: {err}"))?;
    let overlay_root = src
      .canonicalize()
      .unwrap_or_else(|_| src.to_path_buf())
      .to_string_lossy()
      .into_owned();
    let pack = PackReader::open_rooted(&generation, Some(&overlay_root));
    let products_dir = index_dir.join("products");
    let extractor =
      OutlineExtractor::new().map_err(|err| format!("overlay: extractor init failed: {err}"))?;
    let rules_digest = extractor.rules_digest();
    let interner = vorpal_ingest::Interner::default();
    // Hygiene: sweep ref stores abandoned by dead daemons (they are process-private scratch;
    // an hour of age is proof of abandonment several times over).
    if let Ok(entries) = fs::read_dir(index_dir) {
      for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let stale = name.starts_with(".overlay-")
          && name.ends_with(".refs")
          && entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > 3600);
        if stale {
          let _ = fs::remove_file(entry.path());
        }
      }
    }
    let store_path = index_dir.join(format!(".overlay-{}.refs", std::process::id()));
    let mut index = RetainedIndex::empty(&store_path)
      .map_err(|err| format!("overlay: ref store create failed: {err}"))?;
    // The tier seals in the generation's canonical order: bucket-major under the bucketed
    // format, path order otherwise — set BEFORE the first link so the retained-persist pin
    // (daemon generation == scratch generation) holds under either format.
    if matches!(
      vorpal_ingest::PackFormat::from_env(),
      vorpal_ingest::PackFormat::Bucketed
    ) {
      index.set_canonical_order(vorpal_ingest::CanonicalOrder::BucketMajor {
        tree_root: overlay_root.clone(),
      });
    }
    // Batched parallel replay: decode+ingest fan out per batch, absorbs run serially in
    // manifest order — the same bytes a serial pass produces, minutes faster at scale.
    const BATCH: usize = 1024;
    let entries = manifest.entries();
    let mut loose: Vec<(usize, Vec<u8>)> = Vec::new();
    for chunk in entries.chunks(BATCH) {
      loose.clear();
      for (i, stat) in chunk.iter().enumerate() {
        if pack.as_ref().and_then(|p| p.get(stat.path.as_str())).is_none() {
          let bytes = fs::read(products_dir.join(cache_file_name(&stat.path))).map_err(|err| {
            let msg = format!("overlay: product missing for {}: {err}", stat.path);
            vorpal_kg::phase_stamp(&msg);
            msg
          })?;
          loose.push((i, bytes));
        }
      }
      let mut loose_iter = loose.iter().peekable();
      let batch: Vec<(&str, &[u8])> = chunk
        .iter()
        .enumerate()
        .map(|(i, stat)| {
          let path = stat.path.as_str();
          let bytes: &[u8] = match pack.as_ref().and_then(|p| p.get(path)) {
            Some(bytes) => bytes,
            None => {
              let (_, bytes) = loose_iter
                .find(|(j, _)| *j == i)
                .expect("loose bytes were read above");
              bytes
            }
          };
          (path, bytes)
        })
        .collect();
      for (path, bytes) in &batch {
        // Per-path identity: the file's language grammar digest folded with the outline-rule
        // digest — the same gate the replay pipeline applies per product.
        if peek_product_grammar_digest(bytes)
          != vorpal_ingest::extraction_identity_for_path(path, rules_digest)
        {
          let msg = format!(
            "overlay: extraction identity changed since the generation was committed ({path})"
          );
          vorpal_kg::phase_stamp(&msg);
          return Err(msg);
        }
      }
      index.apply_files_parallel(&interner, &batch).map_err(|err| {
        let msg = format!("overlay: batch apply failed: {err}");
        vorpal_kg::phase_stamp(&msg);
        msg
      })?;
    }
    vorpal_kg::phase_stamp("overlay: build done");
    // The replay above ran every file through the same apply path live edits use, so the
    // eid-churn ledger now lists the ENTIRE graph as "added". Churn is defined as change
    // SINCE the built state — drain the replay's noise so the first live edit hands the
    // vector tier its own delta, not a 2.3M-row rebuild disguised as an update.
    let _ = index.take_eid_churn();
    Ok(Self {
      interner,
      index,
      extractor,
      manifest,
      src: src.to_path_buf(),
      index_dir: index_dir.to_path_buf(),
    })
  }

  /// Recover the exact change set by stat-diffing the live tree against the retained
  /// manifest — the overlay's answer to watcher capture loss. Same walker, same
  /// handled-filter, same (size, mtime) trust model as the pipeline's own sweep, so the
  /// recovered set is precisely what a full sweep would re-extract; vanished files ride
  /// along for retraction. ~a stat sweep at kernel scale, in place of a full rebuild.
  pub fn stat_changes(&self, src: &Path) -> Result<std::collections::HashSet<PathBuf>, String> {
    let scan = Manifest::scan(src, |path| self.extractor.handles(path))
      .map_err(|err| format!("overlay: change scan failed: {err}"))?;
    Ok(stat_diff(src, scan.entries(), self.manifest.entries()))
  }

  /// Change-set routing: whether `changed` files fit the retained absorb envelope (the
  /// store's own measured escalation shape) — past it the caller takes the streaming
  /// pipeline deliberately.
  pub fn within_absorb_budget(&self, changed: usize) -> bool {
    self.index.within_absorb_budget(changed)
  }

  /// The pre-link co-change edges the bulk pipeline derives before resolution (symmetric
  /// `changes_with` pairs from git history), in retained id space and the bulk emission
  /// order. Reads the same HEAD-keyed `cochange.cache` the pipeline maintains, so a serve
  /// pays a file read — a git walk only when a commit re-keyed the cache.
  fn cochange_pre_edges(&self) -> Vec<(u32, u32, vorpal_kg::EdgeType)> {
    let pending = crate::cochange::start(&self.src, &self.index_dir.join("cochange.cache"));
    let cochange = crate::cochange::finish(
      pending,
      &self.src,
      self.manifest.entries().iter().map(|e| e.path.as_str()),
    );
    let mut edges = Vec::with_capacity(cochange.edges.len() * 2);
    for edge in &cochange.edges {
      let (Some(a), Some(b)) = (
        self.index.file_node(&edge.a),
        self.index.file_node(&edge.b),
      ) else {
        continue;
      };
      let label = vorpal_kg::EdgeType::CHANGES_WITH.with_confidence(edge.confidence);
      edges.push((a, b, label));
      edges.push((b, a, label));
    }
    edges
  }

  /// Absorb a set of changed paths — re-extract present files, retract vanished ones —
  /// WITHOUT linking: the bookkeeping half, for callers that already have the sealed answer
  /// from another pipeline and only need the overlay kept truthful.
  ///
  /// Errors are the caller's signal to drop the overlay: a file that stopped being
  /// extractable, unreadable bytes, an unknown spelling.
  pub fn absorb(&mut self, changed: &HashSet<PathBuf>) -> Result<(), String> {
    let mut ordered: Vec<&PathBuf> = changed.iter().collect();
    ordered.sort();
    for path in ordered {
      let key = path.to_string_lossy();
      if !path.exists() {
        self
          .index
          .apply_file(&self.interner, &key, None)
          .map_err(|err| format!("overlay: retract {key} failed: {err}"))?;
        self.manifest.remove(&key);
        continue;
      }
      let stat = fs::metadata(path).ok().map(|meta| {
        let mtime_ns = meta
          .modified()
          .ok()
          .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
          .map(|d| d.as_nanos() as u64)
          .unwrap_or(0);
        (meta.len(), mtime_ns)
      });
      if !self.extractor.handles(&key) {
        if self.index.contains(&key) {
          return Err(format!("overlay: {key} indexed before but unhandled now"));
        }
        continue; // a file the index never sees (docs, assets) — nothing to absorb
      }
      let source = fs::read_to_string(path)
        .map_err(|err| format!("overlay: read {key} failed: {err}"))?;
      let Some(product) = self.extractor.extract_product(&key, &source) else {
        return Err(format!("overlay: {key} failed extraction"));
      };
      let mut bytes = Vec::new();
      encode_product_into(&product, &mut bytes);
      self
        .index
        .apply_file(&self.interner, &key, Some(&bytes))
        .map_err(|err| format!("overlay: apply {key} failed: {err}"))?;
      let Some((size, mtime_ns)) = stat else {
        return Err(format!("overlay: stat {key} failed"));
      };
      self.manifest.upsert(FileStat {
        path: key.into_owned(),
        size,
        mtime_ns,
      });
    }
    Ok(())
  }

  /// [`LiveOverlay::absorb`] from a probe's already-extracted products — the serve path
  /// never extracts twice. Same outcomes as `absorb` on the same tree state.
  pub fn absorb_probed(&mut self, probe: &ExtractionProbe) -> Result<(), String> {
    for (path, probed) in &probe.per_path {
      let key = path.to_string_lossy();
      match probed {
        ProbedPath::Vanished => {
          self
            .index
            .apply_file(&self.interner, &key, None)
            .map_err(|err| format!("overlay: retract {key} failed: {err}"))?;
          self.manifest.remove(&key);
        }
        ProbedPath::Unhandled => {
          if self.index.contains(&key) {
            return Err(format!("overlay: {key} indexed before but unhandled now"));
          }
        }
        ProbedPath::Extracted {
          bytes,
          size,
          mtime_ns,
          ..
        } => {
          self
            .index
            .apply_file(&self.interner, &key, Some(bytes))
            .map_err(|err| format!("overlay: apply {key} failed: {err}"))?;
          self.manifest.upsert(FileStat {
            path: key.into_owned(),
            size: *size,
            mtime_ns: *mtime_ns,
          });
        }
        ProbedPath::Failed => {
          return Err(format!("overlay: {key} failed extraction"));
        }
      }
    }
    Ok(())
  }

  /// Record probe-time stats for a stamp-preserving serve (content unchanged, stamps moved):
  /// no graph work, but the retained manifest must track the tree or a LATER served
  /// persistence would commit stale stamps and fork the generation id.
  pub fn note_stamps(&mut self, probe: &ExtractionProbe) {
    for (path, probed) in &probe.per_path {
      if let ProbedPath::Extracted { size, mtime_ns, .. } = probed {
        self.manifest.upsert(FileStat {
          path: path.to_string_lossy().into_owned(),
          size: *size,
          mtime_ns: *mtime_ns,
        });
      }
    }
  }

  /// [`LiveOverlay::apply_and_link`], reusing the probe's extractions.
  pub fn apply_and_link_probed(&mut self, probe: &ExtractionProbe) -> Result<Kg, String> {
    vorpal_kg::phase_stamp("overlay: apply start");
    self.absorb_probed(probe)?;
    self.link_served()
  }

  /// [`LiveOverlay::absorb`] followed by the re-link: the serve path. Returns the sealed
  /// graph for the updated tree — byte-identical to a from-scratch build of it.
  pub fn apply_and_link(&mut self, changed: &HashSet<PathBuf>) -> Result<Kg, String> {
    vorpal_kg::phase_stamp("overlay: apply start");
    self.absorb(changed)?;
    self.link_served()
  }

  fn link_served(&mut self) -> Result<Kg, String> {
    let pre_edges = self.cochange_pre_edges();
    let (kg, _stats) = self
      .index
      .link_for_serving(&self.interner, &Resolver::new(), &pre_edges)
      .map_err(|err| format!("overlay: link failed: {err}"))?;
    // The canonical seal embeds the in-memory name index (built in parallel with the
    // segment), so the served graph is lookup-ready as returned.
    vorpal_kg::phase_stamp("overlay: link done");
    Ok(kg)
  }

  /// The retained-persist serve path: absorb the probe, link WITH evidence, and hand back
  /// the served graph plus a [`ServedPersist`] job that commits the generation these
  /// answers came from — no replay pipeline anywhere. Every artifact is byte-equal to what
  /// a from-scratch build of this tree commits: graph bytes by the canonical-seal theorem,
  /// evidence by the edit≡build pin, manifest from the same stats the scan derives, pack
  /// via the same canonical consolidating writer.
  pub fn apply_and_link_probed_persisting(
    &mut self,
    probe: ExtractionProbe,
    prior: PathBuf,
    out: PathBuf,
  ) -> Result<(Arc<Kg>, ServedPersist), String> {
    vorpal_kg::phase_stamp("overlay: apply start");
    self.absorb_probed(&probe)?;
    let pre_edges = self.cochange_pre_edges();
    let (kg, _stats, evidence, flows, sigs, reach_graph) = self
      .index
      .link(&self.interner, &Resolver::new(), &pre_edges)
      .map_err(|err| format!("overlay: link failed: {err}"))?;
    vorpal_kg::phase_stamp("overlay: link done");
    let mut new_products = Vec::new();
    for (path, probed) in probe.per_path {
      if let ProbedPath::Extracted {
        mut bytes,
        size,
        mtime_ns,
        ..
      } = probed
      {
        // Stamp window [8..24): source size and mtime, exactly as the parse branch stamps
        // from the scan's stats. The xxh3 at [24..32) was stamped by extraction itself.
        if bytes.len() >= 24 {
          bytes[8..16].copy_from_slice(&size.to_le_bytes());
          bytes[16..24].copy_from_slice(&mtime_ns.to_le_bytes());
        }
        new_products.push((path.to_string_lossy().into_owned(), bytes));
      }
    }
    let kg = Arc::new(kg);
    let persist = ServedPersist {
      kg: kg.clone(),
      evidence,
      flows,
      sigs,
      reach_graph,
      manifest: self.manifest.clone(),
      prior,
      out,
      new_products,
      // Canonicalized so it matches the manifest's stored spellings whatever spelling the
      // daemon was booted with.
      tree_root: self
        .src
        .canonicalize()
        .unwrap_or_else(|_| self.src.clone())
        .to_string_lossy()
        .into_owned(),
    };
    Ok((kg, persist))
  }

  /// Drain the vector-tier eid churn (removed, added) accumulated by absorbs since the
  /// last drain — the live ANN tier's per-edit feed.
  pub fn take_eid_churn(&mut self) -> (Vec<u64>, Vec<u64>) {
    self.index.take_eid_churn()
  }

  /// Tombstoned share of the retained rows — the caller's rebuild-the-overlay trigger
  /// (garbage collects the writer tail and resets interner growth).
  pub fn dead_row_fraction(&self) -> f64 {
    self.index.dead_row_fraction()
  }
}

/// The stat diff both freshness sweeps share: live scan entries vs a retained manifest,
/// two-pointer over the sorted paths; vanished files ride along for retraction.
fn stat_diff(
  src: &Path,
  current: &[vorpal_ingest::FileStat],
  retained: &[vorpal_ingest::FileStat],
) -> std::collections::HashSet<PathBuf> {
  let mut changed = std::collections::HashSet::new();
  let (mut i, mut j) = (0usize, 0usize);
  while i < current.len() && j < retained.len() {
    match current[i].path.cmp(&retained[j].path) {
      std::cmp::Ordering::Less => {
        changed.insert(src.join(&current[i].path)); // new file
        i += 1;
      }
      std::cmp::Ordering::Greater => {
        changed.insert(src.join(&retained[j].path)); // vanished file
        j += 1;
      }
      std::cmp::Ordering::Equal => {
        if current[i].size != retained[j].size || current[i].mtime_ns != retained[j].mtime_ns {
          changed.insert(src.join(&current[i].path));
        }
        i += 1;
        j += 1;
      }
    }
  }
  for entry in &current[i..] {
    changed.insert(src.join(&entry.path));
  }
  for entry in &retained[j..] {
    changed.insert(src.join(&entry.path));
  }
  changed
}

/// The no-overlay freshness sweep: the live tree stat-diffed against the COMMITTED
/// generation's manifest — the daemon's liveness backstop during the boot window, before
/// (or without) an adopted overlay. Same walker, same filter, same trust model; only the
/// retained side's source differs (disk instead of RAM).
pub fn stat_changes_against_generation(
  index_dir: &Path,
  src: &Path,
) -> Result<std::collections::HashSet<PathBuf>, String> {
  let generation = vorpal_kg::resolve_index_dir(index_dir);
  let manifest = Manifest::load(&generation.join("manifest.bin"))
    .map_err(|err| format!("backstop: prior manifest unreadable: {err}"))?;
  let extractor =
    OutlineExtractor::new().map_err(|err| format!("backstop: extractor init failed: {err}"))?;
  let scan = Manifest::scan(src, |path| extractor.handles(path))
    .map_err(|err| format!("backstop: change scan failed: {err}"))?;
  Ok(stat_diff(src, scan.entries(), manifest.entries()))
}

/// A served build's persistence tail: everything already computed, only writes remain. Runs
/// on a daemon background thread; the committed generation is bit-identical to a
/// from-scratch build of the served tree, so this replaces the full replay pipeline the
/// canonicalizer used to run (~10+ core-seconds per edit at kernel scale).
pub struct ServedPersist {
  kg: Arc<Kg>,
  evidence: Vec<vorpal_kg::EvidenceRow>,
  /// Data-flow rows in sealed-id space — `dataflow.bin` is part of the generation's
  /// content identity, so the served commit must stage it like every other artifact.
  flows: Vec<vorpal_kg::DataflowRow>,
  /// Sigs-family rows (P4.5c) in sealed-id space — staged beside evidence under the
  /// bucketed format, nothing under flat.
  sigs: Vec<vorpal_ingest::SigRow>,
  /// Encoded `reach.bin` bytes from the link — persisted beside dataflow so composes
  /// over this generation replay the same include-reach oracle a scratch build writes.
  reach_graph: Vec<u8>,
  manifest: Manifest,
  prior: PathBuf,
  out: PathBuf,
  new_products: Vec<(String, Vec<u8>)>,
  /// Canonical tree root — the pack's absolute→tree-relative conversion point (P4.1).
  tree_root: String,
}

impl ServedPersist {
  pub fn persist(self) -> Result<PathBuf, String> {
    let ServedPersist {
      kg,
      evidence,
      flows,
      sigs,
      reach_graph,
      manifest,
      prior,
      out,
      new_products,
      tree_root,
    } = self;
    vorpal_kg::phase_stamp("served persist: start");
    let staging = out.join("gen").join(format!(
      ".staging-{}-{}",
      std::process::id(),
      crate::staging_nonce()
    ));
    fs::create_dir_all(&staging).map_err(|err| format!("served persist: staging: {err}"))?;
    // One format decision for the whole persisted generation: pack layout, node-store
    // layout, both from the same read.
    let format = vorpal_ingest::PackFormat::from_env();
    let layout = match format {
      vorpal_ingest::PackFormat::Flat => vorpal_kg::SegmentLayout::Flat,
      vorpal_ingest::PackFormat::Bucketed => vorpal_kg::SegmentLayout::Bucketed {
        tree_root: tree_root.clone(),
        prior: Some(prior.clone()),
        live_files: manifest.entries().len(),
      },
    };
    let evidence_bases = kg
      .node_id_map(&layout)
      .map_err(|err| format!("served persist: evidence bases: {err}"))?;
    // Three independent artifact groups write concurrently; the manifest stays last (the
    // commit point), exactly like the pipeline's tail.
    let (pack_result, evidence_result, dataflow_result, sigs_result, kg_result) =
      std::thread::scope(|scope| {
        let pack_task = scope.spawn(|| -> std::io::Result<()> {
          let reader = PackReader::open_rooted(&prior, Some(&tree_root)).map(Arc::new);
          let writer = PackWriter::new(
            &staging,
            reader,
            Some(tree_root.clone()),
            format,
          );
          let sink = writer.sink();
          for (path, body) in new_products {
            sink
              .send(PackMsg { path, body })
              .map_err(|_| std::io::Error::other("pack sink closed"))?;
          }
          drop(sink);
          writer.finish(manifest.entries().iter().map(|entry| entry.path.clone()))
        });
        let evidence_task = scope.spawn(|| {
          let evidence_layout = match &evidence_bases {
            None => vorpal_kg::EvidenceLayout::Flat,
            Some(map) => vorpal_kg::EvidenceLayout::Bucketed {
              nodes: map,
              prior: Some(&prior),
            },
          };
          vorpal_kg::save_evidence_with(&staging, evidence, &evidence_layout)
        });
        let dataflow_task = scope.spawn(|| vorpal_kg::save_dataflow(&staging, flows));
        let sigs_task =
          scope.spawn(|| crate::save_sig_family(&staging, &sigs, &evidence_bases, &prior));
        let kg_result = kg.save_with(&staging, &layout);
        (
          pack_task.join().expect("pack writer panicked"),
          evidence_task.join().expect("evidence saver panicked"),
          dataflow_task.join().expect("dataflow saver panicked"),
          sigs_task.join().expect("sigs saver panicked"),
          kg_result,
        )
      });
    pack_result.map_err(|err| format!("served persist: pack: {err}"))?;
    evidence_result.map_err(|err| format!("served persist: evidence: {err}"))?;
    dataflow_result.map_err(|err| format!("served persist: dataflow: {err}"))?;
    sigs_result.map_err(|err| format!("served persist: sigs: {err}"))?;
    kg_result.map_err(|err| format!("served persist: graph: {err}"))?;
    fs::write(staging.join(vorpal_ingest::REACH_GRAPH_FILE), &reach_graph)
      .map_err(|err| format!("served persist: reach: {err}"))?;
    manifest
      .save(&staging.join("manifest.bin"))
      .map_err(|err| format!("served persist: manifest: {err}"))?;
    let id = crate::commit_generation(&out, &prior, staging)
      .map_err(|err| format!("served persist: commit: {err}"))?;
    vorpal_kg::phase_stamp("served persist: committed");
    Ok(out.join("gen").join(id))
  }
}

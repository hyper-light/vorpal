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

use vorpal_ingest::{
  Manifest, OutlineExtractor, PackReader, RetainedIndex, Resolver, cache_file_name,
  encode_product_into, peek_product_grammar_digest,
};
use vorpal_kg::Kg;

pub struct LiveOverlay {
  interner: vorpal_ingest::Interner,
  index: RetainedIndex,
  extractor: OutlineExtractor,
}

impl LiveOverlay {
  /// Build from the generation `CURRENT` names under `index_dir`: every manifest path's
  /// product, from the pack or the loose bank, in manifest (path-sorted) order. Heavy —
  /// one full product replay — so callers run it on a background thread. Refuses to build
  /// when the extraction identity changed since the generation was committed (a grammar or
  /// outline-rule edit): mixing old products with new extraction would fork answers.
  pub fn build(index_dir: &Path) -> Result<Self, String> {
    vorpal_kg::phase_stamp("overlay: build start");
    let generation = vorpal_kg::resolve_index_dir(index_dir);
    let manifest = Manifest::load(&generation.join("manifest.bin"))
      .map_err(|err| format!("overlay: manifest load failed: {err}"))?;
    let pack = PackReader::open(&generation);
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
    Ok(Self {
      interner,
      index,
      extractor,
    })
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
        continue;
      }
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
    }
    Ok(())
  }

  /// [`LiveOverlay::absorb`] followed by the re-link: the serve path. Returns the sealed
  /// graph for the updated tree — byte-identical to a from-scratch build of it.
  pub fn apply_and_link(&mut self, changed: &HashSet<PathBuf>) -> Result<Kg, String> {
    vorpal_kg::phase_stamp("overlay: apply start");
    self.absorb(changed)?;
    let (mut kg, _stats) = self
      .index
      .link_for_serving(&self.interner, &Resolver::new())
      .map_err(|err| format!("overlay: link failed: {err}"))?;
    // Served from RAM: name lookups need the in-memory index (see Kg::build_names_index).
    kg.build_names_index();
    vorpal_kg::phase_stamp("overlay: link done");
    Ok(kg)
  }

  /// Tombstoned share of the retained rows — the caller's rebuild-the-overlay trigger
  /// (garbage collects the writer tail and resets interner growth).
  pub fn dead_row_fraction(&self) -> f64 {
    self.index.dead_row_fraction()
  }
}

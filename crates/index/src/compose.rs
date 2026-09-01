//! The RESPAN compose (P4.5b) — the scoped CLI edit for the span-only class. See
//! `vorpal_kg::respan` for the family surgery and SUBSECOND.md §P4.5 for the plan of
//! record. This module owns ELIGIBILITY (the exactness ladder over prior vs fresh
//! products) and the generation assembly (pack, manifest, report, commit); it either
//! proves the edit is span-only and composes, or returns `None` and the full pipeline
//! runs. It never guesses: every "unchanged" claim is a byte or field comparison, and
//! the composed generation is pinned byte-identical to the full pipeline's by the
//! convergence gate in `tests/pack_v2.rs`.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use vorpal_ingest::{
  ExtractScratch, FileStat, OutlineExtractor, PackMsg, PackReader, PackWriter, decode_product,
  decode_product_view, encode_product_into,
};

use crate::{CutoffContext, IndexReport, commit_generation, staging_nonce};

/// Above this many changed files the full pipeline is competitive — the same policy bound
/// the stamp-only cutoff uses.
const MAX_RESPANNED: usize = 64;

pub(crate) fn try_respan_compose(
  out: &Path,
  prior: &Path,
  ctx: &CutoffContext<'_>,
  extractor: &OutlineExtractor,
  cache_mode_label: &'static str,
) -> io::Result<Option<IndexReport>> {
  let CutoffContext {
    manifest,
    prior_manifest,
    prior_manifest_ns,
    tree_root,
  } = *ctx;
  // The compose writes bucketed generations only, over a fully bucketed prior.
  if !matches!(
    vorpal_ingest::PackFormat::from_env(),
    vorpal_ingest::PackFormat::Bucketed
  ) {
    return Ok(None);
  }
  for toc in [
    vorpal_kg::NODES_TOC,
    vorpal_kg::EVIDENCE_TOC,
    vorpal_kg::EDGES_TOC,
    vorpal_kg::USAGE_TOC,
    vorpal_kg::SIGS_TOC,
    "products/toc.bin",
  ] {
    if !prior.join(toc).is_file() {
      return Ok(None);
    }
  }
  // Modified-only two-pointer diff, same as the cutoff.
  let current = manifest.entries();
  let previous = prior_manifest.entries();
  let (mut i, mut j) = (0usize, 0usize);
  let mut changed: Vec<&FileStat> = Vec::new();
  while i < current.len() && j < previous.len() {
    match current[i].path.cmp(&previous[j].path) {
      std::cmp::Ordering::Less | std::cmp::Ordering::Greater => return Ok(None),
      std::cmp::Ordering::Equal => {
        if current[i].size != previous[j].size || current[i].mtime_ns != previous[j].mtime_ns {
          changed.push(&current[i]);
          if changed.len() > MAX_RESPANNED {
            return Ok(None);
          }
        }
        i += 1;
        j += 1;
      }
    }
  }
  if i < current.len() || j < previous.len() || changed.is_empty() {
    return Ok(None);
  }
  // Racy-mtime hazard for the files this compose trusts WITHOUT reading — identical to
  // the cutoff's law.
  {
    let changed_paths: std::collections::HashSet<&str> =
      changed.iter().map(|e| e.path.as_str()).collect();
    let racy: Vec<&FileStat> = manifest
      .entries()
      .iter()
      .filter(|e| {
        e.mtime_ns.abs_diff(prior_manifest_ns) < 2_000_000_000
          && !changed_paths.contains(e.path.as_str())
      })
      .collect();
    if !racy.is_empty() {
      let Some(pack) = PackReader::open_rooted(prior, Some(tree_root)) else {
        return Ok(None);
      };
      let all_match = racy.iter().all(|entry| {
        let stored = pack
          .get(&entry.path)
          .and_then(vorpal_ingest::peek_product_digest);
        match (stored, fs::read(&entry.path)) {
          (Some(digest), Ok(bytes)) => xxhash_rust::xxh3::xxh3_64(&bytes) == digest,
          _ => false,
        }
      });
      if !all_match {
        return Ok(None);
      }
    }
  }

  // CHANGES_WITH edges carry byte-identically — sound only while the co-change
  // derivation's inputs (git HEAD, commit window) are provably unchanged. A commit
  // between builds re-derives through the full pipeline instead.
  if !crate::cochange::inputs_unchanged(Path::new(tree_root), &out.join("cochange.cache")) {
    vorpal_kg::phase_stamp("respan: ineligible (co-change inputs moved)");
    return Ok(None);
  }
  let Some(pack) = PackReader::open_rooted(prior, Some(tree_root)) else {
    return Ok(None);
  };
  let Ok(prior_kg) = vorpal_kg::Kg::load(prior) else {
    return Ok(None);
  };
  let Some(prior_map) = vorpal_kg::NodeIdMap::from_dir(prior) else {
    return Ok(None);
  };

  let interner = vorpal_ingest::Interner::default();
  let mut plans: Vec<vorpal_kg::respan::FileRespan> = Vec::with_capacity(changed.len());
  let mut fresh_products: Vec<(String, Vec<u8>)> = Vec::with_capacity(changed.len());
  let mut scratch = ExtractScratch::default();
  for entry in &changed {
    let Ok(source) = fs::read_to_string(&entry.path) else {
      return Ok(None);
    };
    let Some(mut product) = extractor.extract_product(&entry.path, &source) else {
      return Ok(None);
    };
    product.source_size = entry.size;
    product.source_mtime_ns = entry.mtime_ns;
    scratch.encode.clear();
    encode_product_into(&product, &mut scratch.encode);
    let fresh_bytes = scratch.encode.clone();
    let Some(prior_bytes) = pack.get(&entry.path) else {
      return Ok(None);
    };
    let (Ok(old_view), Ok(new_view)) = (
      decode_product_view(prior_bytes),
      decode_product_view(&fresh_bytes),
    ) else {
      return Ok(None);
    };
    if let Some(reason) = views_respan_reject(&old_view, &new_view) {
      vorpal_kg::phase_stamp(&format!("respan: ineligible ({}): {reason}", entry.path));
      return Ok(None);
    }
    // Span maps, positionally over the validated-equal ref sequences. Collisions abort.
    let mut ref_spans = std::collections::HashMap::new();
    let mut call_spans = std::collections::HashMap::new();
    for (old, new) in old_view.refs.iter().zip(&new_view.refs) {
      let name_hash = xxhash_rust::xxh3::xxh3_64(old.name.as_bytes()) as u32;
      let key = (old.start, old.end, name_hash);
      let value = (new.start, new.end);
      if let Some(existing) = ref_spans.insert(key, value) {
        if existing != value {
          return Ok(None); // ambiguous mapping — not provably span-only
        }
      }
      let ckey = (old.start, old.end);
      if let Some(existing) = call_spans.insert(ckey, value) {
        if existing != value {
          return Ok(None);
        }
      }
    }
    // Fresh rows through the pipeline's own ingest: a scratch single-file seal whose
    // public views hand back pipeline-exact spans + content hashes — and every stable
    // field, which we VERIFY against the prior generation instead of assuming.
    let Ok(fresh_product) = decode_product(&fresh_bytes) else {
      return Ok(None);
    };
    let Ok(scratch_extractor) = OutlineExtractor::new() else {
      return Ok(None);
    };
    let mut ingestor = vorpal_ingest::Ingestor::new(&interner, scratch_extractor);
    ingestor.ingest_product(&entry.path, fresh_product);
    let scratch_kg = ingestor.seal();
    let rel = vorpal_kg::identity::tree_relative(&entry.path, tree_root);
    let file_key = vorpal_kg::identity::FileKey::of(rel).0;
    let Some(&(_, prior_start, prior_rows)) = prior_map
      .files()
      .iter()
      .find(|&&(key, _, _)| key == file_key)
    else {
      return Ok(None);
    };
    if scratch_kg.node_count() != prior_rows as usize {
      return Ok(None);
    }
    let mut rows = Vec::with_capacity(prior_rows as usize);
    for ordinal in 0..prior_rows as u64 {
      let Some(fresh) = scratch_kg.node(vorpal_kg::NodeId::new(ordinal)) else {
        return Ok(None);
      };
      let Some(prior_view) = prior_kg.node(vorpal_kg::NodeId::new(prior_start + ordinal))
      else {
        return Ok(None);
      };
      // The respan theorem's premises, verified per row: identity and semantics stable.
      if fresh.name != prior_view.name
        || fresh.path != prior_view.path
        || fresh.signature != prior_view.signature
        || fresh.kind != prior_view.kind
        || fresh.exported != prior_view.exported
        || fresh.external_id != prior_view.external_id
      {
        return Ok(None);
      }
      rows.push((fresh.span.0, fresh.span.1, fresh.content_hash));
    }
    plans.push(vorpal_kg::respan::FileRespan {
      file_key,
      rows,
      ref_spans,
      call_spans,
    });
    fresh_products.push((entry.path.clone(), fresh_bytes));
  }
  drop(prior_kg);
  drop(pack);

  // Stage the composed generation. Any error inside the surgery falls back to the full
  // pipeline (staging removed) — the compose never commits a guess.
  let staging = out.join("gen").join(format!(
    ".staging-{}-{}",
    std::process::id(),
    staging_nonce()
  ));
  let _ = fs::remove_dir_all(&staging);
  fs::create_dir_all(&staging)?;
  if let Err(err) = vorpal_kg::respan::respan_generation(&staging, prior, &plans) {
    vorpal_kg::phase_stamp(&format!("respan: fell back to the full pipeline: {err}"));
    let _ = fs::remove_dir_all(&staging);
    return Ok(None);
  }
  // The pack: the ordinary canonical publish — fresh products appended, everything else
  // carried (and hard-linked) from the prior generation's bucketed pack.
  let pack_reader = PackReader::open_rooted(prior, Some(tree_root)).map(Arc::new);
  let writer = PackWriter::new(
    &staging,
    pack_reader,
    Some(tree_root.to_string()),
    vorpal_ingest::PackFormat::Bucketed,
  );
  let sink = writer.sink();
  for (path, bytes) in fresh_products {
    sink
      .send(PackMsg { path, body: bytes })
      .map_err(|_| io::Error::other("respan: pack sink closed"))?;
  }
  drop(sink);
  writer.finish(manifest.entries().iter().map(|entry| entry.path.clone()))?;
  manifest.save(&staging.join("manifest.bin"))?;
  commit_generation(out, prior, staging)?;
  let nodes = vorpal_kg::Kg::peek_node_count(&vorpal_kg::resolve_index_dir(out)).unwrap_or(0);
  Ok(Some(IndexReport {
    // The compose re-extracted the changed files and committed a NEW generation; the
    // GRAPH (edges) is a proven byte-identical carry — only spans and stamps moved.
    reused: false,
    graph_reused: true,
    cache_mode: cache_mode_label,
    error_files: 0,
    error_nodes: 0,
    error_bytes: 0,
    excluded_files: 0,
    indexed: changed.len() as u64,
    skipped: manifest.entries().len() as u64 - changed.len() as u64,
    nodes,
    resolved: 0,
    ambiguous: 0,
    external: 0,
    masked: 0,
    unverified_langs: Vec::new(),
    cochange_edges: 0,
    cochange_note: Some(RESPAN_CARRY_NOTE.to_string()),
    similar_edges: 0,
    similar_note: Some(RESPAN_CARRY_NOTE.to_string()),
    request_sites: 0,
    request_edges: 0,
    request_note: Some(RESPAN_CARRY_NOTE.to_string()),
  }))
}

const RESPAN_CARRY_NOTE: &str = "respan compose: relations carried byte-identically from \
   the prior generation (a span-only edit cannot change them); the passes did not run \
   this build";

/// The respan eligibility ladder: EVERY field of both products must be equal except item
/// ranges, reference spans, and the stamp window — each comparison explicit, nothing
/// inferred. Grammar identity, error accounting, parameters, returns, sketches, and
/// request sites (spans included — conservative) must all be EXACT.
fn views_respan_reject(
  old: &vorpal_ingest::ProductView<'_>,
  new: &vorpal_ingest::ProductView<'_>,
) -> Option<&'static str> {
  if old.grammar_digest != new.grammar_digest {
    return Some("grammar digest");
  }
  if old.error_nodes != new.error_nodes || old.error_bytes != new.error_bytes {
    return Some("error accounting");
  }
  // error_spans shift with the edit like every other span; they live only in the product
  // (republished fresh in the pack) and never enter a composed artifact — the COUNT
  // equality above is the semantic gate. Their count must still agree.
  if old.error_spans.len() != new.error_spans.len() {
    return Some("error span count");
  }
  if old.entity_params != new.entity_params {
    return Some("entity params");
  }
  if old.returns != new.returns {
    return Some("returns");
  }
  if old.signatures.len() != new.signatures.len()
    || !old.signatures.iter().zip(&new.signatures).all(|(a, b)| {
      a.entity_index == b.entity_index && a.shingles == b.shingles && a.sketch == b.sketch
    })
  {
    return Some("near-clone sketches");
  }
  if old.requests.len() != new.requests.len()
    || !old.requests.iter().zip(&new.requests).all(|(a, b)| {
      a.from_entity_index == b.from_entity_index
        && a.method == b.method
        && a.path == b.path
        && a.start == b.start
        && a.end == b.end
    })
  {
    return Some("request sites");
  }
  if old.items.len() != new.items.len()
    || !old.items.iter().zip(&new.items).all(|(a, b)| items_equal_sans_spans(a, b))
  {
    return Some("outline items");
  }
  if old.refs.len() != new.refs.len() {
    return Some("reference count");
  }
  if !old.refs.iter().zip(&new.refs).all(|(a, b)| {
    a.from_entity_index == b.from_entity_index
      && a.name == b.name
      && a.kind == b.kind
      && a.qualifier == b.qualifier
      && a.form == b.form
      && a.alias == b.alias
      && a.receiver == b.receiver
      && a.receiver_type == b.receiver_type
      && a.receiver_type_origin == b.receiver_type_origin
      && args_equal(a, b)
  }) {
    return Some("reference fields");
  }
  None
}

fn args_equal(a: &vorpal_ingest::RefView<'_>, b: &vorpal_ingest::RefView<'_>) -> bool {
  let mut left = a.args();
  let mut right = b.args();
  loop {
    match (left.next(), right.next()) {
      (None, None) => return true,
      (Some(x), Some(y)) => {
        if x.index != y.index || x.class != y.class || x.kw_name != y.kw_name || x.expr != y.expr
        {
          return false;
        }
      }
      _ => return false,
    }
  }
}

fn items_equal_sans_spans(
  a: &vorpal_ingest::OutlineItem<'_>,
  b: &vorpal_ingest::OutlineItem<'_>,
) -> bool {
  a.entry.role == b.entry.role
    && a.entry.symbol_type == b.entry.symbol_type
    && a.entry.name == b.entry.name
    && a.entry.signature == b.entry.signature
    && a.entry.ast_kind == b.entry.ast_kind
    && a.is_import == b.is_import
    && a.is_exported == b.is_exported
    && a.members.len() == b.members.len()
    && a.members.iter().zip(&b.members).all(|(ma, mb)| {
      ma.entry.role == mb.entry.role
        && ma.entry.symbol_type == mb.entry.symbol_type
        && ma.entry.name == mb.entry.name
        && ma.entry.signature == mb.entry.signature
        && ma.entry.ast_kind == mb.entry.ast_kind
        && ma.is_public == mb.is_public
    })
}

/// The DEFS-STABLE compose (P4.5c-2; multi-file sessions since S2): modified files whose
/// definition sets are unchanged — bodies, references, sketches, and request sites may
/// move. Eligibility climbs `views_defs_stable_reject` PER FILE (any miss declines the
/// whole session — the chain falls through to defs-changed, then the pipeline); the
/// session re-resolves against the prior generation through the pipeline's own kernels
/// (`vorpal_ingest::scoped_resolve_files`, one shared table — proven outcome-equal to
/// scratch by tests/scoped_oracle.rs); the global pair set repairs over the sigs family
/// with every file's run swapped; and `vorpal_kg::defs_stable::compose_defs_stable`
/// performs the family surgery. Byte convergence with the full pipeline is pinned by
/// tests/scoped_compose.rs (and the multi-file gates).
pub(crate) fn try_defs_stable_compose(
  out: &Path,
  prior: &Path,
  ctx: &CutoffContext<'_>,
  extractor: &OutlineExtractor,
  cache_mode_label: &'static str,
) -> io::Result<Option<IndexReport>> {
  let CutoffContext {
    manifest,
    prior_manifest,
    prior_manifest_ns,
    tree_root,
  } = *ctx;
  if !matches!(
    vorpal_ingest::PackFormat::from_env(),
    vorpal_ingest::PackFormat::Bucketed
  ) {
    return Ok(None);
  }
  for toc in [
    vorpal_kg::NODES_TOC,
    vorpal_kg::EVIDENCE_TOC,
    vorpal_kg::EDGES_TOC,
    vorpal_kg::USAGE_TOC,
    vorpal_kg::SIGS_TOC,
    "products/toc.bin",
  ] {
    if !prior.join(toc).is_file() {
      return Ok(None);
    }
  }
  // Modified-only diff; the session composes ANY number of body-edited files (S2) —
  // adds, deletes, and renames still decline (the file table would move).
  let current = manifest.entries();
  let previous = prior_manifest.entries();
  let (mut i, mut j) = (0usize, 0usize);
  let mut changed: Vec<&FileStat> = Vec::new();
  while i < current.len() && j < previous.len() {
    match current[i].path.cmp(&previous[j].path) {
      std::cmp::Ordering::Less | std::cmp::Ordering::Greater => return Ok(None),
      std::cmp::Ordering::Equal => {
        if current[i].size != previous[j].size || current[i].mtime_ns != previous[j].mtime_ns {
          changed.push(&current[i]);
        }
        i += 1;
        j += 1;
      }
    }
  }
  if i < current.len() || j < previous.len() || changed.is_empty() {
    return Ok(None);
  }
  let changed_paths: std::collections::HashSet<&str> =
    changed.iter().map(|e| e.path.as_str()).collect();
  // Racy-mtime hazard — the cutoff's law, verbatim (every changed path exempted).
  {
    let racy: Vec<&FileStat> = manifest
      .entries()
      .iter()
      .filter(|e| {
        e.mtime_ns.abs_diff(prior_manifest_ns) < 2_000_000_000
          && !changed_paths.contains(e.path.as_str())
      })
      .collect();
    if !racy.is_empty() {
      let Some(pack) = PackReader::open_rooted(prior, Some(tree_root)) else {
        return Ok(None);
      };
      let all_match = racy.iter().all(|racy_entry| {
        let stored = pack
          .get(&racy_entry.path)
          .and_then(vorpal_ingest::peek_product_digest);
        match (stored, fs::read(&racy_entry.path)) {
          (Some(digest), Ok(bytes)) => xxhash_rust::xxh3::xxh3_64(&bytes) == digest,
          _ => false,
        }
      });
      if !all_match {
        return Ok(None);
      }
    }
  }
  if !crate::cochange::inputs_unchanged(Path::new(tree_root), &out.join("cochange.cache")) {
    vorpal_kg::phase_stamp("defs-stable: ineligible (co-change inputs moved)");
    return Ok(None);
  }

  let Some(pack) = PackReader::open_rooted(prior, Some(tree_root)) else {
    return Ok(None);
  };
  let Ok(prior_kg) = vorpal_kg::Kg::load(prior) else {
    return Ok(None);
  };
  let Some(prior_map) = vorpal_kg::NodeIdMap::from_dir(prior) else {
    return Ok(None);
  };

  // Per file: fresh product, the defs-stable ladder, the pipeline's own single-file
  // seal, and stable fields VERIFIED against the prior generation row by row (the
  // respan compose's proven pattern). Any miss anywhere declines the whole session.
  struct SealedFile {
    path: String,
    file_key: u64,
    fresh_bytes: Vec<u8>,
    layout_ords: Vec<u64>,
    node_rows: Vec<(u32, u32, u64)>,
  }
  let interner = vorpal_ingest::Interner::default();
  let mut sealed: Vec<SealedFile> = Vec::with_capacity(changed.len());
  for entry in &changed {
    let Ok(source) = fs::read_to_string(&entry.path) else {
      return Ok(None);
    };
    let Some(mut product) = extractor.extract_product(&entry.path, &source) else {
      return Ok(None);
    };
    product.source_size = entry.size;
    product.source_mtime_ns = entry.mtime_ns;
    let mut fresh_bytes = Vec::new();
    encode_product_into(&product, &mut fresh_bytes);
    let Some(prior_bytes) = pack.get(&entry.path) else {
      return Ok(None);
    };
    let (Ok(old_view), Ok(new_view)) = (
      decode_product_view(prior_bytes),
      decode_product_view(&fresh_bytes),
    ) else {
      return Ok(None);
    };
    if let Some(reason) = vorpal_ingest::views_defs_stable_reject(&old_view, &new_view) {
      vorpal_kg::phase_stamp(&format!("defs-stable: ineligible ({}): {reason}", entry.path));
      return Ok(None);
    }
    let Ok(fresh_product) = decode_product(&fresh_bytes) else {
      return Ok(None);
    };
    let Ok(scratch_extractor) = OutlineExtractor::new() else {
      return Ok(None);
    };
    let mut ingestor = vorpal_ingest::Ingestor::new(&interner, scratch_extractor);
    let layout_ords = ingestor.ingest_product_mapped(&entry.path, fresh_product);
    let scratch_kg = ingestor.seal();
    let rel = vorpal_kg::identity::tree_relative(&entry.path, tree_root);
    let file_key = vorpal_kg::identity::FileKey::of(rel).0;
    let Some(&(_, prior_start, prior_rows)) = prior_map
      .files()
      .iter()
      .find(|&&(key, _, _)| key == file_key)
    else {
      vorpal_kg::phase_stamp(&format!(
        "defs-stable: ineligible ({}): outside the prior universe",
        entry.path
      ));
      return Ok(None);
    };
    if scratch_kg.node_count() != prior_rows as usize {
      vorpal_kg::phase_stamp(&format!(
        "defs-stable: ineligible ({}): node row count moved",
        entry.path
      ));
      return Ok(None);
    }
    let mut node_rows = Vec::with_capacity(prior_rows as usize);
    for ordinal in 0..prior_rows as u64 {
      let Some(fresh) = scratch_kg.node(vorpal_kg::NodeId::new(ordinal)) else {
        return Ok(None);
      };
      let Some(prior_view) = prior_kg.node(vorpal_kg::NodeId::new(prior_start + ordinal))
      else {
        return Ok(None);
      };
      if fresh.name != prior_view.name
        || fresh.path != prior_view.path
        || fresh.signature != prior_view.signature
        || fresh.kind != prior_view.kind
        || fresh.exported != prior_view.exported
        || fresh.external_id != prior_view.external_id
      {
        vorpal_kg::phase_stamp(&format!(
          "defs-stable: ineligible ({}): stable node field moved at ordinal {ordinal}",
          entry.path
        ));
        return Ok(None);
      }
      node_rows.push((fresh.span.0, fresh.span.1, fresh.content_hash));
    }
    sealed.push(SealedFile {
      path: entry.path.clone(),
      file_key,
      fresh_bytes,
      layout_ords,
      node_rows,
    });
  }

  // Scoped re-resolution against the prior universe — the c2-i oracle's proven entry,
  // ONE session over all edited files (one shared table, all import bindings seeded).
  // The closure cap reuses the retained tier's MEASURED escalation shape: past ~a quarter
  // of the corpus, the full pipeline's streaming feed wins (SUBSECOND.md, retained scope
  // decision) — the same bound, the same reasoning, one recorded source.
  let decode_cap = (manifest.entries().len() / 4).max(1);
  let fetch = |path: &str| pack.get(path).map(<[u8]>::to_vec);
  let views: Vec<vorpal_ingest::ProductView<'_>> = {
    let mut views = Vec::with_capacity(sealed.len());
    for file in &sealed {
      let Ok(view) = decode_product_view(&file.fresh_bytes) else {
        return Ok(None);
      };
      views.push(view);
    }
    views
  };
  let inputs: Vec<vorpal_ingest::ScopedFileInput<'_>> = sealed
    .iter()
    .zip(&views)
    .map(|(file, view)| vorpal_ingest::ScopedFileInput {
      path: file.path.clone(),
      file_key: file.file_key,
      view,
      layout_ords: &file.layout_ords,
    })
    .collect();
  let outcomes = match vorpal_ingest::scoped_resolve_files(
    &interner,
    &prior_kg,
    &prior_map,
    &vorpal_ingest::Resolver::new(),
    &fetch,
    &inputs,
    decode_cap,
  ) {
    Ok(outcomes) => outcomes,
    Err(err) => {
      vorpal_kg::phase_stamp(&format!("defs-stable: scoped resolution declined: {err}"));
      return Ok(None);
    }
  };
  drop(inputs);
  drop(views);

  // The global pairing repair over the sigs family, every edited run swapped.
  vorpal_kg::phase_stamp("defs-stable: scoped resolution done");
  let Some(sig_store) = vorpal_kg::SigStore::open(prior) else {
    return Ok(None);
  };
  let Some(family_rows) = sig_store.rows(&prior_map) else {
    return Ok(None);
  };
  let prior_rows_sigs: Vec<vorpal_ingest::SigRow> = family_rows
    .into_iter()
    .map(|row| vorpal_ingest::SigRow {
      node: u64::from(row.node),
      shingles: row.shingles,
      sketch: row.sketch,
    })
    .collect();
  vorpal_kg::phase_stamp("defs-stable: sigs family loaded");
  let prior_pairs = vorpal_ingest::similar_pairs_of_kg(&prior_kg);
  vorpal_kg::phase_stamp("defs-stable: prior pairs extracted");
  let swaps: Vec<(u64, &[vorpal_ingest::SigRow])> = sealed
    .iter()
    .zip(&outcomes)
    .map(|(file, outcome)| (file.file_key, outcome.sigs.as_slice()))
    .collect();
  let repair = match vorpal_ingest::scoped_similar_repair(
    &prior_map,
    manifest.entries().len(),
    &prior_rows_sigs,
    &prior_pairs,
    &swaps,
  ) {
    Ok(repair) => repair,
    Err(err) => {
      vorpal_kg::phase_stamp(&format!("defs-stable: pairing repair declined: {err}"));
      return Ok(None);
    }
  };
  drop(swaps);
  let mut sig_rows = Vec::with_capacity(repair.swapped_rows.len());
  for row in &repair.swapped_rows {
    let Ok(node) = u32::try_from(row.node) else {
      return Ok(None);
    };
    sig_rows.push(vorpal_kg::SigFamilyRow {
      node,
      shingles: row.shingles,
      sketch: row.sketch,
    });
  }
  let (mut resolved, mut ambiguous, mut external, mut masked) = (0u64, 0u64, 0u64, 0u64);
  let mut file_plans = Vec::with_capacity(sealed.len());
  for (file, outcome) in sealed.iter_mut().zip(outcomes) {
    resolved += outcome.stats.resolved;
    ambiguous += outcome.stats.ambiguous;
    external += outcome.stats.external;
    masked += outcome.stats.masked;
    file_plans.push(vorpal_kg::defs_stable::DefsStableFilePlan {
      file_key: file.file_key,
      node_rows: std::mem::take(&mut file.node_rows),
      evidence: outcome.evidence,
      edges: outcome.edges,
      request_edges: outcome.request_edges,
      flows: outcome.flows,
    });
  }
  let plan = vorpal_kg::defs_stable::DefsStablePlan {
    files: file_plans,
    fresh_pairs: repair.fresh_pairs,
    changed_srcs: repair.changed_srcs,
    sig_rows,
  };
  drop(pack);

  // Stage, operate, publish, commit — the respan tail, verbatim in shape.
  let staging = out.join("gen").join(format!(
    ".staging-{}-{}",
    std::process::id(),
    staging_nonce()
  ));
  let _ = fs::remove_dir_all(&staging);
  fs::create_dir_all(&staging)?;
  if let Err(err) =
    vorpal_kg::defs_stable::compose_defs_stable(&staging, prior, &prior_kg, &plan)
  {
    vorpal_kg::phase_stamp(&format!("defs-stable: fell back to the full pipeline: {err}"));
    let _ = fs::remove_dir_all(&staging);
    return Ok(None);
  }
  let pack_reader = PackReader::open_rooted(prior, Some(tree_root)).map(Arc::new);
  let writer = PackWriter::new(
    &staging,
    pack_reader,
    Some(tree_root.to_string()),
    vorpal_ingest::PackFormat::Bucketed,
  );
  let sink = writer.sink();
  for file in &mut sealed {
    sink
      .send(PackMsg {
        path: file.path.clone(),
        body: std::mem::take(&mut file.fresh_bytes),
      })
      .map_err(|_| io::Error::other("defs-stable: pack sink closed"))?;
  }
  drop(sink);
  writer.finish(manifest.entries().iter().map(|e| e.path.clone()))?;
  manifest.save(&staging.join("manifest.bin"))?;
  commit_generation(out, prior, staging)?;
  let nodes = vorpal_kg::Kg::peek_node_count(&vorpal_kg::resolve_index_dir(out)).unwrap_or(0);
  Ok(Some(IndexReport {
    reused: false,
    // The graph CHANGED (the edited files' edges re-derived; pair diffs may reach
    // further) — this is a real semantic build, scoped to the closure.
    graph_reused: false,
    cache_mode: cache_mode_label,
    error_files: 0,
    error_nodes: 0,
    error_bytes: 0,
    excluded_files: 0,
    indexed: sealed.len() as u64,
    skipped: manifest.entries().len() as u64 - sealed.len() as u64,
    nodes,
    resolved,
    ambiguous,
    external,
    masked,
    unverified_langs: Vec::new(),
    cochange_edges: 0,
    cochange_note: Some(DEFS_STABLE_NOTE.to_string()),
    similar_edges: 0,
    similar_note: Some(DEFS_STABLE_NOTE.to_string()),
    request_sites: 0,
    request_edges: 0,
    request_note: Some(DEFS_STABLE_NOTE.to_string()),
  }))
}

const DEFS_STABLE_NOTE: &str = "defs-stable compose: unchanged files' relations carried \
   byte-identically; the edited files re-resolved against the prior universe and the \
   near-clone pair set repaired over the sigs family";

/// The DEFS-CHANGED compose (P4.5c-3; multi-file sessions since S2-b): modified files
/// whose definition sets moved — and MIXED sessions, where defs-stable members ride as
/// delta-0 blocks. The dirty closure comes from the usage family over the union of
/// `affected_def_names` (ordinal-shifted survivors included — the byte-impact law); the
/// whole closure re-resolves through the overlay session (oracle:
/// tests/defs_changed_oracle.rs); and `vorpal_kg::defs_changed::compose_defs_changed`
/// splices the successor generation under the multi-block shift law. Convergence is
/// pinned by tests/defs_changed_compose.rs.
pub(crate) fn try_defs_changed_compose(
  out: &Path,
  prior: &Path,
  ctx: &CutoffContext<'_>,
  extractor: &OutlineExtractor,
  cache_mode_label: &'static str,
) -> io::Result<Option<IndexReport>> {
  let CutoffContext {
    manifest,
    prior_manifest,
    prior_manifest_ns,
    tree_root,
  } = *ctx;
  if !matches!(
    vorpal_ingest::PackFormat::from_env(),
    vorpal_ingest::PackFormat::Bucketed
  ) {
    return Ok(None);
  }
  for toc in [
    vorpal_kg::NODES_TOC,
    vorpal_kg::EVIDENCE_TOC,
    vorpal_kg::EDGES_TOC,
    vorpal_kg::USAGE_TOC,
    vorpal_kg::SIGS_TOC,
    "products/toc.bin",
  ] {
    if !prior.join(toc).is_file() {
      return Ok(None);
    }
  }
  // Modified-only diff; the session composes ANY number of changed files (adds,
  // deletes, and renames still decline — the file SET must be stable).
  let current = manifest.entries();
  let previous = prior_manifest.entries();
  let (mut i, mut j) = (0usize, 0usize);
  let mut changed: Vec<&FileStat> = Vec::new();
  while i < current.len() && j < previous.len() {
    match current[i].path.cmp(&previous[j].path) {
      std::cmp::Ordering::Less | std::cmp::Ordering::Greater => return Ok(None),
      std::cmp::Ordering::Equal => {
        if current[i].size != previous[j].size || current[i].mtime_ns != previous[j].mtime_ns {
          changed.push(&current[i]);
        }
        i += 1;
        j += 1;
      }
    }
  }
  if i < current.len() || j < previous.len() || changed.is_empty() {
    return Ok(None);
  }
  let changed_paths: std::collections::HashSet<&str> =
    changed.iter().map(|e| e.path.as_str()).collect();
  {
    let racy: Vec<&FileStat> = manifest
      .entries()
      .iter()
      .filter(|e| {
        e.mtime_ns.abs_diff(prior_manifest_ns) < 2_000_000_000
          && !changed_paths.contains(e.path.as_str())
      })
      .collect();
    if !racy.is_empty() {
      let Some(pack) = PackReader::open_rooted(prior, Some(tree_root)) else {
        return Ok(None);
      };
      let all_match = racy.iter().all(|racy_entry| {
        let stored = pack
          .get(&racy_entry.path)
          .and_then(vorpal_ingest::peek_product_digest);
        match (stored, fs::read(&racy_entry.path)) {
          (Some(digest), Ok(bytes)) => xxhash_rust::xxh3::xxh3_64(&bytes) == digest,
          _ => false,
        }
      });
      if !all_match {
        return Ok(None);
      }
    }
  }
  if !crate::cochange::inputs_unchanged(Path::new(tree_root), &out.join("cochange.cache")) {
    vorpal_kg::phase_stamp("defs-changed: ineligible (co-change inputs moved)");
    return Ok(None);
  }

  let Some(pack) = PackReader::open_rooted(prior, Some(tree_root)) else {
    return Ok(None);
  };
  let Ok(prior_kg) = vorpal_kg::Kg::load(prior) else {
    return Ok(None);
  };
  let Some(prior_map) = vorpal_kg::NodeIdMap::from_dir(prior) else {
    return Ok(None);
  };

  // Per changed file: fresh product, the ladder (defs-changed accepts what defs-stable
  // would too — a stable member is a delta-0 block), the scratch seal, prior
  // coordinates, the affected-name set, and the per-old-ordinal unmoved flags.
  struct EditedSeal {
    path: String,
    file_key: u64,
    fresh_bytes: Vec<u8>,
    fresh_ords: Vec<u64>,
    fresh_kg: vorpal_kg::Kg,
    old_start: u64,
    old_rows: u32,
    affected: Vec<String>,
    unmoved: Vec<bool>,
  }
  let interner = vorpal_ingest::Interner::default();
  let row_facts = |kg: &vorpal_kg::Kg, id: u64| {
    kg.node(vorpal_kg::NodeId::new(id)).map(|node| {
      (
        node.name.to_string(),
        node.kind,
        node.signature.to_string(),
        node.exported,
        node.external_id.map(|eid| eid.to_string()),
      )
    })
  };
  let mut sealed: Vec<EditedSeal> = Vec::with_capacity(changed.len());
  let mut any_affected = false;
  for entry in &changed {
    let Ok(source) = fs::read_to_string(&entry.path) else {
      return Ok(None);
    };
    let Some(mut product) = extractor.extract_product(&entry.path, &source) else {
      return Ok(None);
    };
    product.source_size = entry.size;
    product.source_mtime_ns = entry.mtime_ns;
    let mut fresh_bytes = Vec::new();
    encode_product_into(&product, &mut fresh_bytes);
    let Some(prior_bytes) = pack.get(&entry.path) else {
      return Ok(None);
    };
    let (Ok(old_view), Ok(new_view)) = (
      decode_product_view(prior_bytes),
      decode_product_view(&fresh_bytes),
    ) else {
      return Ok(None);
    };
    if let Some(reason) = vorpal_ingest::views_defs_changed_reject(&old_view, &new_view) {
      vorpal_kg::phase_stamp(&format!("defs-changed: ineligible ({}): {reason}", entry.path));
      return Ok(None);
    }
    let Ok(fresh_product) = decode_product(&fresh_bytes) else {
      return Ok(None);
    };
    let Ok(scratch_extractor) = OutlineExtractor::new() else {
      return Ok(None);
    };
    let mut ingestor = vorpal_ingest::Ingestor::new(&interner, scratch_extractor);
    let fresh_ords = ingestor.ingest_product_mapped(&entry.path, fresh_product);
    let fresh_kg = ingestor.seal();
    let rel = vorpal_kg::identity::tree_relative(&entry.path, tree_root);
    let file_key = vorpal_kg::identity::FileKey::of(rel).0;
    let Some(&(_, old_start, old_rows)) = prior_map
      .files()
      .iter()
      .find(|&&(key, _, _)| key == file_key)
    else {
      vorpal_kg::phase_stamp(&format!(
        "defs-changed: ineligible ({}): outside the prior universe",
        entry.path
      ));
      return Ok(None);
    };
    let affected =
      vorpal_ingest::affected_def_names(&prior_kg, old_start, old_rows, &fresh_kg);
    any_affected |= !affected.is_empty();
    // Route/Channel escalation: request matching is URL-keyed — usage cannot bound it.
    let old_kinds = vorpal_ingest::def_kinds_of(&prior_kg, old_start, old_rows);
    let new_kinds = vorpal_ingest::def_kinds_of(&fresh_kg, 0, fresh_kg.node_count() as u32);
    let routeish = |kinds: Option<&Vec<vorpal_kg::SymbolKind>>| {
      kinds.is_some_and(|list| {
        list.iter().any(|kind| {
          matches!(kind, vorpal_kg::SymbolKind::Route | vorpal_kg::SymbolKind::Channel)
        })
      })
    };
    if affected
      .iter()
      .any(|name| routeish(old_kinds.get(name)) || routeish(new_kinds.get(name)))
    {
      vorpal_kg::phase_stamp("defs-changed: ineligible (a Route/Channel definition moved)");
      return Ok(None);
    }
    let new_rows_count = fresh_kg.node_count() as u32;
    // Per OLD ordinal: identity-and-position survival — the unmoved-ordinal law.
    let unmoved: Vec<bool> = (0..u64::from(old_rows))
      .map(|ord| {
        ord < u64::from(new_rows_count)
          && row_facts(&prior_kg, old_start + ord) == row_facts(&fresh_kg, ord)
      })
      .collect();
    sealed.push(EditedSeal {
      path: entry.path.clone(),
      file_key,
      fresh_bytes,
      fresh_ords,
      fresh_kg,
      old_start,
      old_rows,
      affected,
      unmoved,
    });
  }
  // Every file defs-stable means the stable lane already declined for a session-level
  // reason; nothing changed-scoped is provable here.
  if !any_affected {
    return Ok(None);
  }

  // The usage-dirty closure over the UNION of affected names, capped by the retained
  // tier's measured escalation shape (past ~a quarter of the corpus the full pipeline's
  // streaming feed wins).
  let Some(usage) = vorpal_kg::UsageStore::open(prior) else {
    return Ok(None);
  };
  let edited_keys: std::collections::HashSet<u64> =
    sealed.iter().map(|s| s.file_key).collect();
  let mut dirty_keys: Vec<u64> = sealed
    .iter()
    .flat_map(|s| s.affected.iter())
    .flat_map(|name| {
      usage.files_referencing((xxhash_rust::xxh3::xxh3_64(name.as_bytes()) & 0xFFFF_FFFF) as u32)
    })
    .filter(|key| !edited_keys.contains(key))
    .collect();
  dirty_keys.sort_unstable();
  dirty_keys.dedup();
  let dirty_cap = (manifest.entries().len() / 4).max(1);
  if dirty_keys.len() > dirty_cap {
    vorpal_kg::phase_stamp(&format!(
      "defs-changed: escalating ({} dirty files past the cap {dirty_cap})",
      dirty_keys.len(),
    ));
    return Ok(None);
  }
  vorpal_kg::phase_stamp(&format!(
    "defs-changed: {} edited files, {} affected names -> {} dirty files",
    sealed.len(),
    sealed.iter().map(|s| s.affected.len()).sum::<usize>(),
    dirty_keys.len(),
  ));

  // Dirty inputs: unchanged pack products + their layout bridges.
  let path_of_key = |key: u64| -> Option<String> {
    prior_map
      .files()
      .iter()
      .find(|&&(k, _, _)| k == key)
      .and_then(|&(_, start, _)| prior_kg.node(vorpal_kg::NodeId::new(start)))
      .map(|file| file.path.to_string())
  };
  let mut dirty_bytes: Vec<(u64, String, Vec<u8>)> = Vec::with_capacity(dirty_keys.len());
  for &key in &dirty_keys {
    let Some(path) = path_of_key(key) else {
      return Ok(None);
    };
    let Some(bytes) = pack.get(&path) else {
      return Ok(None);
    };
    dirty_bytes.push((key, path, bytes.to_vec()));
  }
  let mut dirty_views: Vec<(u64, String, vorpal_ingest::ProductView<'_>, Vec<u64>)> =
    Vec::with_capacity(dirty_bytes.len());
  for (key, path, bytes) in &dirty_bytes {
    let Ok(view) = decode_product_view(bytes) else {
      return Ok(None);
    };
    let Ok(product) = decode_product(bytes) else {
      return Ok(None);
    };
    let Ok(map_extractor) = OutlineExtractor::new() else {
      return Ok(None);
    };
    let mut mapper = vorpal_ingest::Ingestor::new(&interner, map_extractor);
    let ords = mapper.ingest_product_mapped(path, product);
    dirty_views.push((*key, path.clone(), view, ords));
  }

  // The session: every edited file (against its seal) + the dirty closure, one table.
  let edited_views: Vec<vorpal_ingest::ProductView<'_>> = {
    let mut views = Vec::with_capacity(sealed.len());
    for file in &sealed {
      let Ok(view) = decode_product_view(&file.fresh_bytes) else {
        return Ok(None);
      };
      views.push(view);
    }
    views
  };
  let edited_inputs: Vec<(vorpal_ingest::DirtyFileInput<'_>, &vorpal_kg::Kg)> = sealed
    .iter()
    .zip(&edited_views)
    .map(|(file, view)| {
      (
        vorpal_ingest::DirtyFileInput {
          path: file.path.clone(),
          file_key: file.file_key,
          view,
          layout_ords: &file.fresh_ords,
        },
        &file.fresh_kg,
      )
    })
    .collect();
  let dirty_inputs: Vec<vorpal_ingest::DirtyFileInput<'_>> = dirty_views
    .iter()
    .map(|(key, path, view, ords)| vorpal_ingest::DirtyFileInput {
      path: path.clone(),
      file_key: *key,
      view,
      layout_ords: ords,
    })
    .collect();
  let decode_cap = (manifest.entries().len() / 4).max(1);
  let outcomes = match vorpal_ingest::resolve_defs_changed(
    &interner,
    &prior_kg,
    &prior_map,
    &vorpal_ingest::Resolver::new(),
    &|path: &str| pack.get(path).map(<[u8]>::to_vec),
    &edited_inputs,
    &dirty_inputs,
    decode_cap,
  ) {
    Ok(outcomes) => outcomes,
    Err(err) => {
      vorpal_kg::phase_stamp(&format!("defs-changed: scoped resolution declined: {err}"));
      return Ok(None);
    }
  };
  drop(edited_inputs);
  drop(edited_views);

  // The pairing repair over the TRANSLATED ledger: each edited file's old rows drop
  // (its fresh run rides its outcome), every other row's node id shifts by the
  // multi-block law, and any prior pair that LOSES an edited-block endpoint forces the
  // surviving endpoint's segment to rewrite.
  // The blocks, ascending, with cumulative deltas — the same law the surgery applies.
  struct LocalBlock<'a> {
    old_start: u64,
    old_end: u64,
    new_start: u64,
    new_rows: u32,
    file_key: u64,
    unmoved: &'a [bool],
  }
  let mut law_blocks: Vec<LocalBlock<'_>> = sealed
    .iter()
    .map(|s| LocalBlock {
      old_start: s.old_start,
      old_end: s.old_start + u64::from(s.old_rows),
      new_start: 0,
      new_rows: s.fresh_kg.node_count() as u32,
      file_key: s.file_key,
      unmoved: &s.unmoved,
    })
    .collect();
  law_blocks.sort_by_key(|b| b.old_start);
  let mut law_prefix: Vec<i64> = Vec::with_capacity(law_blocks.len() + 1);
  law_prefix.push(0);
  for block in &mut law_blocks {
    let cum = *law_prefix.last().expect("non-empty prefix");
    block.new_start = (block.old_start as i64 + cum) as u64;
    law_prefix.push(cum + i64::from(block.new_rows) - (block.old_end - block.old_start) as i64);
  }
  let translate = |dense: u64| -> Option<u64> {
    let i = law_blocks.partition_point(|b| b.old_end <= dense);
    if i < law_blocks.len() && law_blocks[i].old_start <= dense {
      let block = &law_blocks[i];
      let ordinal = (dense - block.old_start) as usize;
      return block
        .unmoved
        .get(ordinal)
        .copied()
        .unwrap_or(false)
        .then_some(block.new_start + (dense - block.old_start));
    }
    Some((dense as i64 + law_prefix[i]) as u64)
  };
  let Some(sig_store) = vorpal_kg::SigStore::open(prior) else {
    return Ok(None);
  };
  let Some(family_rows) = sig_store.rows(&prior_map) else {
    return Ok(None);
  };
  let mut prior_rows_sigs: Vec<vorpal_ingest::SigRow> = Vec::with_capacity(family_rows.len());
  for row in family_rows {
    // The repair's splice replaces every edited file's run WHOLESALE, so keeping their
    // unmoved rows here (identity-translated) is safe — and it is what lets the exact
    // pairing short-circuit see an append for what it is: identical signed rows.
    // Moved/removed rows have no successor coordinate and drop; if any signed row
    // moved, the equality fails and the full re-pair runs, as it must.
    if let Some(node) = translate(u64::from(row.node)) {
      prior_rows_sigs.push(vorpal_ingest::SigRow {
        node,
        shingles: row.shingles,
        sketch: row.sketch,
      });
    }
  }
  let mut forced_changed: Vec<u32> = Vec::new();
  let mut prior_pairs: Vec<(u64, u64, u8)> = Vec::new();
  for (a, b, confidence) in vorpal_ingest::similar_pairs_of_kg(&prior_kg) {
    match (translate(a), translate(b)) {
      (Some(ta), Some(tb)) => prior_pairs.push((ta.min(tb), ta.max(tb), confidence)),
      (Some(t), None) | (None, Some(t)) => forced_changed.push(t as u32),
      (None, None) => {}
    }
  }
  prior_pairs.sort_unstable();
  // The successor identity map, for the repair's canonical splice — the surgery's law.
  let successor_map = {
    let bases = prior_map.bases();
    let mut new_bases = bases.to_vec();
    for base in &mut new_bases {
      let i = law_blocks.partition_point(|b| b.old_end <= *base);
      *base = (*base as i64 + law_prefix[i]) as u64;
    }
    let mut new_files: Vec<(u64, u64, u32)> = Vec::with_capacity(prior_map.files().len());
    for &(key, start, rows) in prior_map.files() {
      if let Some(block) = law_blocks.iter().find(|b| b.file_key == key) {
        new_files.push((key, block.new_start, block.new_rows));
      } else {
        let Some(translated) = translate(start) else {
          return Ok(None);
        };
        new_files.push((key, translated, rows));
      }
    }
    vorpal_kg::NodeIdMap::from_parts(new_bases, new_files)
  };
  let swaps: Vec<(u64, &[vorpal_ingest::SigRow])> = sealed
    .iter()
    .zip(&outcomes)
    .map(|(file, outcome)| (file.file_key, outcome.sigs.as_slice()))
    .collect();
  let repair = match vorpal_ingest::scoped_similar_repair(
    &successor_map,
    manifest.entries().len(),
    &prior_rows_sigs,
    &prior_pairs,
    &swaps,
  ) {
    Ok(repair) => repair,
    Err(err) => {
      vorpal_kg::phase_stamp(&format!("defs-changed: pairing repair declined: {err}"));
      return Ok(None);
    }
  };
  drop(swaps);
  let mut changed_srcs = repair.changed_srcs.clone();
  changed_srcs.extend(forced_changed);
  changed_srcs.sort_unstable();
  changed_srcs.dedup();
  let mut sig_rows = Vec::with_capacity(repair.swapped_rows.len());
  for row in &repair.swapped_rows {
    let Ok(node) = u32::try_from(row.node) else {
      return Ok(None);
    };
    sig_rows.push(vorpal_kg::SigFamilyRow {
      node,
      shingles: row.shingles,
      sketch: row.sketch,
    });
  }

  let mut stats_total = vorpal_ingest::ResolveStats::default();
  let mut plan_files: Vec<vorpal_kg::defs_changed::ChangedFilePlan> =
    Vec::with_capacity(outcomes.len());
  let keys_in_order: Vec<u64> = sealed
    .iter()
    .map(|s| s.file_key)
    .chain(dirty_keys.iter().copied())
    .collect();
  for (outcome, key) in outcomes.into_iter().zip(keys_in_order) {
    stats_total += outcome.stats;
    plan_files.push(vorpal_kg::defs_changed::ChangedFilePlan {
      file_key: key,
      evidence: outcome.evidence,
      edges: outcome.edges,
      request_edges: outcome.request_edges,
      flows: outcome.flows,
    });
  }
  let plan = vorpal_kg::defs_changed::DefsChangedPlan {
    files: plan_files,
    edited: sealed.iter().map(|s| (s.file_key, s.unmoved.clone())).collect(),
    fresh_pairs: repair.fresh_pairs,
    changed_srcs,
    sig_rows,
  };
  let dirty_count = dirty_keys.len();
  drop(pack);

  let staging = out.join("gen").join(format!(
    ".staging-{}-{}",
    std::process::id(),
    staging_nonce()
  ));
  let _ = fs::remove_dir_all(&staging);
  fs::create_dir_all(&staging)?;
  let fresh_seals: Vec<(u64, &vorpal_kg::Kg)> =
    sealed.iter().map(|s| (s.file_key, &s.fresh_kg)).collect();
  if let Err(err) = vorpal_kg::defs_changed::compose_defs_changed(
    &staging,
    prior,
    &prior_kg,
    &fresh_seals,
    &plan,
  ) {
    vorpal_kg::phase_stamp(&format!("defs-changed: fell back to the full pipeline: {err}"));
    let _ = fs::remove_dir_all(&staging);
    return Ok(None);
  }
  drop(fresh_seals);
  drop(prior_kg);
  let pack_reader = PackReader::open_rooted(prior, Some(tree_root)).map(Arc::new);
  let writer = PackWriter::new(
    &staging,
    pack_reader,
    Some(tree_root.to_string()),
    vorpal_ingest::PackFormat::Bucketed,
  );
  let sink = writer.sink();
  for file in &mut sealed {
    sink
      .send(PackMsg {
        path: file.path.clone(),
        body: std::mem::take(&mut file.fresh_bytes),
      })
      .map_err(|_| io::Error::other("defs-changed: pack sink closed"))?;
  }
  drop(sink);
  writer.finish(manifest.entries().iter().map(|e| e.path.clone()))?;
  manifest.save(&staging.join("manifest.bin"))?;
  commit_generation(out, prior, staging)?;
  let nodes = vorpal_kg::Kg::peek_node_count(&vorpal_kg::resolve_index_dir(out)).unwrap_or(0);
  let edited_count = sealed.len() as u64;
  Ok(Some(IndexReport {
    reused: false,
    graph_reused: false,
    cache_mode: cache_mode_label,
    error_files: 0,
    error_nodes: 0,
    error_bytes: 0,
    excluded_files: 0,
    indexed: edited_count + dirty_count as u64,
    skipped: manifest.entries().len() as u64 - edited_count - dirty_count as u64,
    nodes,
    resolved: stats_total.resolved,
    ambiguous: stats_total.ambiguous,
    external: stats_total.external,
    masked: stats_total.masked,
    unverified_langs: Vec::new(),
    cochange_edges: 0,
    cochange_note: Some(DEFS_CHANGED_NOTE.to_string()),
    similar_edges: 0,
    similar_note: Some(DEFS_CHANGED_NOTE.to_string()),
    request_sites: 0,
    request_edges: 0,
    request_note: Some(DEFS_CHANGED_NOTE.to_string()),
  }))
}

const DEFS_CHANGED_NOTE: &str = "defs-changed compose: the edited files and their usage-dirty \
   referrers re-resolved against the successor universe; every other file's relations \
   carried byte-identically under the shift law";


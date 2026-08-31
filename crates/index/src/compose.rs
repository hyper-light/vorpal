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

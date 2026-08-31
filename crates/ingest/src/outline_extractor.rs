//! The concrete engine-backed extractor: L0 tree-sitter parse → L1 outline rules (§3.1).

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use rayon::prelude::*;
use vorpal_core::Language;
use vorpal_kg::KgWriter;
use vorpal_lang_registry::SgLang;
use vorpal_language::LanguageExt;
use vorpal_outline::DEFAULT_OUTLINE_RULES;
use vorpal_outline::combined_extractor::CombinedExtractors;
use vorpal_outline::extractor::{SerializableOutlineRule, parse_outline_rules};
use vorpal_outline::model::OutlineItem;

use vorpal_kg::NodeId;
use vorpal_resolve::Reference;

use crate::pipeline::FileExtractor;
use crate::product::{self, FileProduct, ProductRef};
use crate::references::{extract_references_with_facts, ref_spec, resolved_ref_spec};

type LangExtractors = HashMap<SgLang, CombinedExtractors<SgLang>>;

/// The rule set behind an extractor.
///
/// `Eager` compiles everything at construction — user-supplied sources keep it
/// so validation fails loudly where the rules were provided. `Lazy` holds the
/// BUNDLED docs bucketed per language (zero-copy `&'static` slices out of
/// `DEFAULT_OUTLINE_RULES`) and serde-parses + matcher-compiles a language on
/// FIRST USE: the ledger measured the eager all-49 compile at 158,737
/// allocations / 44 MB per run — about half of a small repo's entire
/// allocation bill — while a typical tree touches a handful of languages.
/// Bundled-rule compilability is pinned by the outline crate's CI test, so
/// deferring it loses no validation in practice; a (theoretically unreachable)
/// lazy compile failure reports loudly and disables that language's outline
/// rules for the run rather than faking anything.
pub(crate) enum ExtractorSet {
  Eager(LangExtractors),
  Lazy {
    docs: HashMap<SgLang, Vec<&'static str>>,
    compiled: HashMap<SgLang, OnceLock<Option<CombinedExtractors<SgLang>>>>,
  },
}

impl ExtractorSet {
  fn get(&self, lang: SgLang) -> Option<&CombinedExtractors<SgLang>> {
    match self {
      ExtractorSet::Eager(map) => map.get(&lang),
      ExtractorSet::Lazy { docs, compiled } => compiled
        .get(&lang)?
        .get_or_init(|| {
          let mut rules: Vec<SerializableOutlineRule<SgLang>> = Vec::new();
          for doc in docs.get(&lang)? {
            match parse_outline_rules::<SgLang>(doc) {
              Ok(parsed) => rules.extend(parsed),
              Err(err) => {
                eprintln!(
                  "vorpal: bundled outline rules for {lang} failed to parse: {err} \
                   (outline extraction for this language is disabled this run)"
                );
                return None;
              }
            }
          }
          match CombinedExtractors::try_from(rules, &Default::default()) {
            Ok(combined) => Some(combined),
            Err(err) => {
              eprintln!(
                "vorpal: bundled outline rules for {lang} failed to compile: {err} \
                 (outline extraction for this language is disabled this run)"
              );
              None
            }
          }
        })
        .as_ref(),
    }
  }

  fn contains(&self, lang: SgLang) -> bool {
    match self {
      ExtractorSet::Eager(map) => map.contains_key(&lang),
      ExtractorSet::Lazy { docs, .. } => docs.contains_key(&lang),
    }
  }

  fn count(&self) -> usize {
    match self {
      ExtractorSet::Eager(map) => map.len(),
      ExtractorSet::Lazy { docs, .. } => docs.len(),
    }
  }

  fn langs(&self) -> Vec<SgLang> {
    match self {
      ExtractorSet::Eager(map) => map.keys().copied().collect(),
      ExtractorSet::Lazy { docs, .. } => docs.keys().copied().collect(),
    }
  }
}

/// Bucket the bundled rule docs per language by their top-level `language:`
/// line — a raw string scan, no serde, no copies. `None` (a doc without a
/// parseable language line — impossible for the shipped set, pinned by tests)
/// sends the caller to the eager path, so laziness can never drop a rule.
fn bucket_default_docs() -> Option<HashMap<SgLang, Vec<&'static str>>> {
  let mut buckets: HashMap<SgLang, Vec<&'static str>> = HashMap::new();
  for doc in DEFAULT_OUTLINE_RULES.split("\n---\n") {
    if doc.trim().is_empty() {
      continue;
    }
    let lang_text = doc
      .lines()
      .find_map(|line| line.strip_prefix("language:"))?
      .trim();
    let lang: SgLang = lang_text.parse().ok()?;
    // Slim builds: rules for disabled grammars never compile (mirrors
    // `compile_groups`' gate exactly).
    if !lang.is_enabled() {
      continue;
    }
    buckets.entry(lang).or_default().push(doc);
  }
  Some(buckets)
}

/// The default rule set, shared process-wide — lazily compiled per language
/// (see [`ExtractorSet`]); falls back to one eager compile if bucketing cannot
/// attribute every doc.
static DEFAULT_EXTRACTORS: OnceLock<Result<Arc<ExtractorSet>, String>> = OnceLock::new();

/// Compiles the bundled outline rules into one [`CombinedExtractors`] per language and runs them
/// against parsed files. Language is chosen from the file extension (§3.1 "all languages").
pub struct OutlineExtractor {
  by_lang: Arc<ExtractorSet>,
  /// Reference-extraction specs supplied as data (F-M4): dynamic languages, or a user override
  /// for a builtin. Consulted before the builtin const table, so a data spec wins for its
  /// language — identity-correct because the spec source is folded into the rules digest.
  dynamic_specs: HashMap<SgLang, crate::references::ResolvedRefSpec>,
  /// Digest of the exact outline-rule source this extractor was compiled from. Folded into each
  /// product's identity so editing an extraction rule invalidates products it produced — the
  /// grammar digest alone cannot see a rule change.
  rules_digest: u64,
}

impl OutlineExtractor {
  /// The built-in outline rule set (`DEFAULT_OUTLINE_RULES`), compiled once per process.
  pub fn new() -> Result<Self, String> {
    let by_lang = DEFAULT_EXTRACTORS
      .get_or_init(|| match bucket_default_docs() {
        Some(docs) => {
          let compiled = docs.keys().map(|&lang| (lang, OnceLock::new())).collect();
          Ok(Arc::new(ExtractorSet::Lazy { docs, compiled }))
        }
        None => compile_rules(DEFAULT_OUTLINE_RULES)
          .map(|map| Arc::new(ExtractorSet::Eager(map))),
      })
      .clone()?;
    Ok(Self {
      by_lang,
      dynamic_specs: HashMap::new(),
      rules_digest: xxhash_rust::xxh3::xxh3_64(DEFAULT_OUTLINE_RULES.as_bytes()),
    })
  }

  /// Compile an arbitrary outline-rule YAML document (bundled or user-supplied).
  pub fn from_rules(rules_yaml: &str) -> Result<Self, String> {
    Ok(Self {
      by_lang: Arc::new(ExtractorSet::Eager(compile_rules(rules_yaml)?)),
      dynamic_specs: HashMap::new(),
      rules_digest: xxhash_rust::xxh3::xxh3_64(rules_yaml.as_bytes()),
    })
  }

  /// The bundled rules plus extra sources (custom/dynamic languages, F-M3). Languages the
  /// sources name must already be registered — dlopen is the caller's job, never this crate's.
  ///
  /// The rules digest becomes a canonical stream: the builtin bytes first, then each source
  /// framed as `(origin, len, yaml)` in origin-sorted order — machine- and registration-order
  /// independent, and **byte-identical to [`OutlineExtractor::new`]'s digest when `sources` is
  /// empty**, so pure-builtin indexes never invalidate across this feature's introduction.
  /// `origin` must therefore be a stable label (the config-declared relative path), never an
  /// absolute filesystem path. Two sources sharing an origin with different contents are
  /// rejected loudly — an ambiguous identity must never fold into product digests.
  ///
  /// Note the digest is global: adding or editing any source re-keys every product (one full
  /// replay-or-parse pass), exactly like editing the bundled rules. Per-language isolation
  /// comes from the grammar half of the identity, not the rules half.
  pub fn with_sources(sources: &[RuleSource]) -> Result<Self, String> {
    Self::with_env(sources, &[], None)
  }

  /// Extra outline rules AND serialized ref specs (F-M4). The canonical digest stream extends
  /// F-M3's exactly: builtin bytes, origin-sorted outline frames, then origin-sorted ref-spec
  /// frames under a distinct tag — an outline-only environment digests identically to F-M3,
  /// and empty-everything identically to [`OutlineExtractor::new`]. Ref-spec kinds resolve
  /// under the strict policy; a spec for an already-spec'd language must be deliberate (a data
  /// spec overrides the builtin const for its language).
  pub fn with_env(
    outline_sources: &[RuleSource],
    ref_spec_sources: &[RuleSource],
    injection_config: Option<&RuleSource>,
  ) -> Result<Self, String> {
    if outline_sources.is_empty() && ref_spec_sources.is_empty() && injection_config.is_none() {
      return Self::new();
    }
    let canonical = |sources: &[RuleSource]| -> Result<Vec<RuleSource>, String> {
      let mut sorted: Vec<&RuleSource> = sources.iter().collect();
      sorted.sort_by(|a, b| a.origin.cmp(&b.origin).then(a.yaml.cmp(&b.yaml)));
      sorted.dedup_by(|a, b| a.origin == b.origin && a.yaml == b.yaml);
      for pair in sorted.windows(2) {
        if pair[0].origin == pair[1].origin {
          return Err(format!(
            "rule source '{}' provided twice with different contents",
            pair[0].origin
          ));
        }
      }
      Ok(sorted.into_iter().cloned().collect())
    };
    let outline = canonical(outline_sources)?;
    let ref_specs = canonical(ref_spec_sources)?;

    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(DEFAULT_OUTLINE_RULES.as_bytes());
    for source in &outline {
      h.update(b"\x00source\x00");
      h.update(source.origin.as_bytes());
      h.update(&[0]);
      h.update(&(source.yaml.len() as u64).to_le_bytes());
      h.update(source.yaml.as_bytes());
    }
    for source in &ref_specs {
      h.update(b"\x00refspec\x00");
      h.update(source.origin.as_bytes());
      h.update(&[0]);
      h.update(&(source.yaml.len() as u64).to_le_bytes());
      h.update(source.yaml.as_bytes());
    }
    if let Some(source) = injection_config {
      h.update(b"\x00injections\x00");
      h.update(source.origin.as_bytes());
      h.update(&[0]);
      h.update(&(source.yaml.len() as u64).to_le_bytes());
      h.update(source.yaml.as_bytes());
    }

    let mut rules = parse_outline_rules::<SgLang>(DEFAULT_OUTLINE_RULES)
      .map_err(|e| format!("parse rules: {e}"))?;
    for source in &outline {
      rules.extend(
        parse_outline_rules::<SgLang>(&source.yaml)
          .map_err(|e| format!("parse outline rules from {}: {e}", source.origin))?,
      );
    }

    let mut dynamic_specs: HashMap<SgLang, crate::references::ResolvedRefSpec> = HashMap::new();
    for source in &ref_specs {
      for spec in crate::refspec_config::parse_ref_specs(&source.yaml)
        .map_err(|e| format!("{}: {e}", source.origin))?
      {
        let lang: SgLang = spec.language.parse().map_err(|_| {
          format!(
            "unknown language '{}' in ref spec {} — register the custom language first",
            spec.language, source.origin
          )
        })?;
        let data = spec.to_data(lang, &source.origin)?;
        if dynamic_specs
          .insert(
            lang,
            crate::references::ResolvedRefSpec::build(lang, Arc::new(data)),
          )
          .is_some()
        {
          return Err(format!(
            "two ref specs target language '{}' (second in {}) — exactly one spec per language",
            spec.language, source.origin
          ));
        }
      }
    }

    Ok(Self {
      by_lang: Arc::new(ExtractorSet::Eager(compile_groups(rules)?)),
      dynamic_specs,
      rules_digest: h.digest(),
    })
  }

  /// The dynamic languages this extractor can extract (outline rules and/or a data ref spec) —
  /// sorted by name, so callers can report the unverified set deterministically.
  pub fn dynamic_langs(&self) -> Vec<String> {
    let mut names: Vec<String> = self
      .by_lang
      .langs()
      .iter()
      .chain(self.dynamic_specs.keys())
      .filter(|lang| matches!(lang, SgLang::Custom(_)))
      .map(|lang| lang.to_string())
      .collect();
    names.sort_unstable();
    names.dedup();
    names
  }

  /// How many languages have outline rules (compiled or lazily compilable).
  pub fn languages(&self) -> usize {
    self.by_lang.count()
  }

  /// The digest of the outline rules this extractor uses — a component of product identity.
  pub fn rules_digest(&self) -> u64 {
    self.rules_digest
  }
}

/// One extra outline-rule document for [`OutlineExtractor::with_sources`]: `origin` is the
/// stable, machine-independent label folded into the rules digest (the config-declared
/// relative path), `yaml` the document's exact bytes.
#[derive(Debug, Clone)]
pub struct RuleSource {
  pub origin: String,
  pub yaml: String,
}

/// Parse the YAML rule set and compile each language's rules — languages compile in parallel
/// (they are independent), with any failure surfaced eagerly, exactly as the serial path did.
fn compile_rules(rules_yaml: &str) -> Result<LangExtractors, String> {
  let rules =
    parse_outline_rules::<SgLang>(rules_yaml).map_err(|e| format!("parse rules: {e}"))?;
  compile_groups(rules)
}

/// Group parsed rules by language and compile each group (shared by the bundled-rules path and
/// [`OutlineExtractor::with_sources`]).
fn compile_groups(rules: Vec<SerializableOutlineRule<SgLang>>) -> Result<LangExtractors, String> {
  let mut grouped: HashMap<SgLang, Vec<SerializableOutlineRule<SgLang>>> = HashMap::new();
  for rule in rules {
    // Slim builds: rules for disabled grammars parse (vocabulary) but never compile
    // (compiling a pattern parses it with the grammar — an unimplemented!() stub there).
    if !rule.common().language.is_enabled() {
      continue;
    }
    grouped
      .entry(rule.common().language)
      .or_default()
      .push(rule);
  }

  grouped
    .into_par_iter()
    .map(|(lang, lang_rules)| {
      let combined = CombinedExtractors::try_from(lang_rules, &Default::default())
        .map_err(|e| format!("compile {lang} rules: {e}"))?;
      Ok((lang, combined))
    })
    .collect()
}

impl OutlineExtractor {
  /// Whether a file at `path` would be extracted: a known extension with compiled outline rules
  /// and/or a reference-extraction spec (a language may have either independently).
  pub fn handles(&self, path: &str) -> bool {
    SgLang::from_path(path).is_some_and(|lang| {
      self.by_lang.contains(lang)
        || self.dynamic_specs.contains_key(&lang)
        || ref_spec(lang).is_some()
    })
  }
}

impl OutlineExtractor {
  /// Extract one file into a cacheable [`FileProduct`]: outline items plus references keyed by
  /// their enclosing definition's *entity path* (stable across runs, unlike `NodeId`s). This is
  /// the single extraction path — live ingest applies the product immediately; incremental
  /// re-index replays persisted products for unchanged files.
  pub fn extract_product(&self, path: &str, source: &str) -> Option<FileProduct> {
    let lang = SgLang::from_path(path)?;
    let combined = self.by_lang.get(lang);
    // A data spec (dynamic language, or a user override) wins over the builtin const table.
    let spec = self.dynamic_specs.get(&lang).or_else(|| resolved_ref_spec(lang));
    if combined.is_none() && spec.is_none() {
      return None;
    }
    // A language with rules or a spec always has a linked grammar (rule compilation and spec
    // resolution are enablement-gated), so this is total here; `None` would mean a language we
    // cannot stamp an identity for, whose products could never validate — not extractable.
    // Host identity folds injectable grammars (C3a) — the same value the replay gate computes.
    let grammar_generation = crate::grammar_generation_for(lang)?;
    // The parse tree (`grep`) is owned locally; everything extracted is copied into the owned
    // product before it drops. Reference extraction runs even without outline rules (the file
    // node is the only definition span).
    let grep = lang.grep(source);
    // Injections (C3a): embedded languages parse with tree-sitter included ranges, so every
    // span below is a host-file byte offset. Hosts without injections pay nothing (the
    // injectable set is a static `None` for almost every language).
    let injected = if vorpal_language::LanguageExt::injectable_languages(&lang).is_some() {
      grep.get_injections(|name| name.parse::<SgLang>().ok())
    } else {
      Vec::new()
    };
    let root = grep.root();
    // Graceful-degradation telemetry (all languages): count the tree-sitter ERROR nodes this
    // parse produced (0 = clean) AND measure the damage — merged error ranges give an honest
    // affected-byte count (nested ERRORs never double-count) plus up to eight representative
    // spans, so health policies can threshold on a ratio and humans can look at the wreckage
    // without re-parsing (IMPROVEMENTS #11).
    //
    // The scan is gated on the root's O(1) `has_error` subtree flag: a clean parse (the
    // overwhelming majority of files) has provably zero ERROR nodes, so we skip the full-tree
    // DFS entirely. When the flag is set we walk exactly as before — a MISSING-only tree (flag
    // set, no ERROR node) still yields `(0, 0, [])`, byte-identical to the ungated result.
    let (error_nodes, error_bytes, error_spans) = if root.has_error()
      || injected.iter().any(|sub| sub.root().has_error())
    {
      let mut error_ranges: Vec<(u32, u32)> = root
        .dfs()
        .filter(|node| node.is_error())
        .map(|node| {
          let range = node.range();
          (range.start as u32, range.end as u32)
        })
        .collect();
      for sub in &injected {
        let sub_root = sub.root();
        error_ranges.extend(sub_root.dfs().filter(|node| node.is_error()).map(|node| {
          let range = node.range();
          (range.start as u32, range.end as u32)
        }));
      }
      let error_nodes = error_ranges.len() as u32;
      error_ranges.sort_unstable();
      let mut merged: Vec<(u32, u32)> = Vec::new();
      for (start, end) in error_ranges {
        match merged.last_mut() {
          Some((_, last_end)) if start <= *last_end => *last_end = (*last_end).max(end),
          _ => merged.push((start, end)),
        }
      }
      let error_bytes: u64 = merged.iter().map(|&(s, e)| u64::from(e - s)).sum();
      let error_spans: Vec<(u32, u32)> = merged.into_iter().take(8).collect();
      (error_nodes, error_bytes, error_spans)
    } else {
      (0u32, 0u64, Vec::new())
    };
    let mut items: Vec<OutlineItem<'_>> = combined
      .map(|c| c.extract(grep.root()).collect())
      .unwrap_or_default();
    for sub in &injected {
      let sub_lang = *sub.root().lang();
      if let Some(sub_combined) = self.by_lang.get(sub_lang) {
        items.extend(sub_combined.extract(sub.root()));
      }
    }
    if !injected.is_empty() {
      // Canonical layout across trees: document order by span. Stable, so equal-start items
      // keep host-then-injection order. Injection-free files never take this branch — their
      // product bytes are exactly the single-tree extraction's.
      items.sort_by_key(|item| {
        (
          item.entry.range.byte_offset.start,
          item.entry.range.byte_offset.end,
        )
      });
    }

    let (entities, spans) = local_layout(&items);

    // Near-clone signatures (v16): every callable definition's leaf tokens stream into a
    // sketch during the reference walk. The seed is the grammar generation, so tokens of
    // different grammars never collide by kind id.
    let mut signer = {
      let mut kinds: Vec<vorpal_kg::SymbolKind> = Vec::with_capacity(spans.len());
      kinds.push(vorpal_kg::SymbolKind::File);
      for item in &items {
        kinds.push(vorpal_kg::SymbolKind::from_symbol_type(item.entry.symbol_type, item.is_import));
        for member in &item.members {
          kinds.push(vorpal_kg::SymbolKind::from_symbol_type(member.entry.symbol_type, false));
        }
      }
      let signable: Vec<(std::ops::Range<usize>, u32)> = spans
        .iter()
        .filter(|(_, id)| {
          matches!(
            kinds.get(id.raw() as usize),
            Some(
              vorpal_kg::SymbolKind::Function
                | vorpal_kg::SymbolKind::Method
                | vorpal_kg::SymbolKind::Constructor
            )
          )
        })
        .map(|(range, id)| (range.clone(), id.raw() as u32))
        .collect();
      (!signable.is_empty()).then(|| crate::signature::Signer::new(grammar_generation, signable))
    };

    let mut raw = Vec::new();
    let mut bindings = Vec::new();
    let mut raw_requests = Vec::new();
    match spec {
      Some(spec) => extract_references_with_facts(
        grep.root(),
        spec,
        crate::references::resolved_typefacts(lang),
        &spans,
        &entities,
        &mut raw,
        &mut bindings,
        &mut raw_requests,
        signer.as_mut(),
      ),
      None => {
        // No reference spec: the signatures still need the token stream.
        if let Some(signer) = signer.as_mut() {
          for node in grep.root().dfs() {
            signer.visit(&node);
          }
        }
      }
    }
    for sub in &injected {
      let sub_lang = *sub.root().lang();
      if let Some(signer) = signer.as_mut() {
        signer.restart(crate::grammar_generation_for(sub_lang).unwrap_or(grammar_generation));
      }
      if let Some(sub_spec) = self
        .dynamic_specs
        .get(&sub_lang)
        .or_else(|| resolved_ref_spec(sub_lang))
      {
        extract_references_with_facts(
          sub.root(),
          sub_spec,
          crate::references::resolved_typefacts(sub_lang),
          &spans,
          &entities,
          &mut raw,
          &mut bindings,
          &mut raw_requests,
          signer.as_mut(),
        );
      } else if let Some(signer) = signer.as_mut() {
        for node in sub.root().dfs() {
          signer.visit(&node);
        }
      }
    }
    let signatures: Vec<product::ProductSignature> = signer
      .map(|signer| {
        signer
          .finish()
          .into_iter()
          .map(|(entity_index, sketch)| product::ProductSignature {
            entity_index,
            shingles: sketch.shingles,
            sketch: sketch.bins,
          })
          .collect()
      })
      .unwrap_or_default();
    if !injected.is_empty() {
      // Same canonicalization for references (attribution reads spans, so order is free to
      // normalize); untouched for injection-free files.
      raw.sort_by_key(|r| (r.start, r.end));
    }
    // File-local type knowledge (G-M1): fold captured bindings into name → (type, origin),
    // poisoning any name bound to disagreeing types — conservative by design. Binding order
    // never matters (a use-before-assign types identically), so the map is order-free.
    let mut typed: HashMap<&str, Option<(&str, crate::typefacts::BindOrigin)>> = HashMap::new();
    for binding in &bindings {
      // Return bindings key a FUNCTION's name to its return type — they feed the chained-
      // call ledger below and must never type a same-named receiver variable.
      if binding.origin == crate::typefacts::BindOrigin::Return {
        continue;
      }
      let Some(ty) = binding.ty.as_deref() else {
        continue;
      };
      typed
        .entry(binding.name.as_ref())
        .and_modify(|slot| {
          if let Some((existing, origin)) = slot {
            if *existing != ty {
              *slot = None; // disagreement → no type, ever
            } else if binding.origin < *origin {
              *origin = binding.origin; // stronger origin wins the label (Annotated < …)
            }
          }
        })
        .or_insert(Some((ty, binding.origin)));
    }

    // Per-entity parameter lists: every Param binding attributed to its innermost enclosing
    // definition span, in file order (dfs order is file order).
    let mut entity_params: product::EntityParams = Vec::new();
    {
      let mut cursor = crate::references::SpanCursor::new(&spans);
      let mut by_entity: std::collections::BTreeMap<u32, Vec<(String, Option<String>)>> =
        std::collections::BTreeMap::new();
      for binding in &bindings {
        if binding.origin != crate::typefacts::BindOrigin::Param {
          continue;
        }
        if let Some(from) = cursor.enclosing(binding.start as usize) {
          by_entity
            .entry(from.raw() as u32)
            .or_default()
            .push((
              binding.name.to_string(),
              binding.ty.as_deref().map(str::to_string),
            ));
        }
      }
      entity_params.extend(by_entity);
    }

    // The chained-call return ledger (v15): function name → declared return type, file-
    // local rows; the link-time map poisons cross-file disagreements.
    let returns: Vec<(String, String)> = bindings
      .iter()
      .filter(|b| b.origin == crate::typefacts::BindOrigin::Return)
      .filter_map(|b| Some((b.name.to_string(), b.ty.as_deref()?.to_string())))
      .collect();

    // The single ownership point: names/qualifiers rode through extraction as borrows of
    // `source`; they are copied exactly once, here, into the detachable product.
    let refs = raw
      .into_iter()
      .map(|r| {
        let receiver_typing = r
          .receiver
          .as_deref()
          .and_then(|name| typed.get(name).copied().flatten());
        ProductRef {
          from_entity_index: r.from.raw() as u32,
          name: r.name.into_owned(),
          kind: product::refkind_tag(r.kind),
          start: r.start,
          end: r.end,
          qualifier: r.qualifier.map(Cow::into_owned),
          form: product::refform_tag(r.form),
          alias: r.alias.map(Cow::into_owned),
          receiver_type: receiver_typing.map(|(ty, _)| ty.to_string()),
          receiver_type_origin: receiver_typing.map(|(_, o)| o.tag()).unwrap_or(0xFF),
          receiver: r.receiver.map(Cow::into_owned),
          args: r
            .args
            .into_iter()
            .map(|arg| product::ProductArg {
              index: arg.index,
              class: arg.class as u8,
              kw_name: arg.kw_name.map(Cow::into_owned),
              expr: arg.expr.map(Cow::into_owned),
            })
            .collect(),
        }
      })
      .collect();

    Some(FileProduct {
      version: product::PRODUCT_FORMAT_VERSION,
      // The never-matching default stamp: persisting callers stat the source and stamp the
      // product; an unstamped product can never replay. The content digest is stamped from
      // the exact bytes extraction saw — the identity staged validation trusts.
      source_size: 0,
      source_mtime_ns: 0,
      source_xxh3: xxhash_rust::xxh3::xxh3_64(source.as_bytes()),
      // Extraction identity: the language's grammar generation folded with the outline-rule
      // digest, so the cache invalidates a product once either the parser or the rules change.
      grammar_digest: crate::extraction_identity(grammar_generation, self.rules_digest),
      error_nodes,
      error_bytes,
      error_spans,
      items: items.into_iter().map(product::own_item).collect(),
      refs,
      entity_params,
      returns,
      signatures,
      requests: raw_requests
        .into_iter()
        .map(|r| product::ProductRequest {
          from_entity_index: r.from.raw() as u32,
          method: r.method,
          path: r.path.into_owned(),
          start: r.start,
          end: r.end,
        })
        .collect(),
    })
  }
}

/// Local definition layout for reference attribution: index 0 = the file, then items and their
/// members in the same order as the graph writer. Entity identities come from
/// [`vorpal_kg::layout_entity_identities`] — borrowed views of the single identity authority
/// (the [`vorpal_kg::layout_entity_paths`] convention) — so a reference resolves to exactly
/// the node the writer created, overloads included, without a rendered `String` per entity.
/// Spans are built in lockstep so each entity index maps to its byte range.
pub(crate) fn local_layout<'a>(
  items: &'a [OutlineItem<'_>],
) -> (
  Vec<vorpal_kg::EntityIdentity<'a>>,
  Vec<(std::ops::Range<usize>, NodeId)>,
) {
  let entities = vorpal_kg::layout_entity_identities(items);
  let mut spans: Vec<(std::ops::Range<usize>, NodeId)> = vec![(0..usize::MAX, NodeId::new(0))];
  let mut idx = 1u64;
  for item in items {
    spans.push((item.entry.range.byte_offset.clone(), NodeId::new(idx)));
    idx += 1;
    for member in &item.members {
      spans.push((member.entry.range.byte_offset.clone(), NodeId::new(idx)));
      idx += 1;
    }
  }
  debug_assert_eq!(entities.len(), spans.len(), "layout entity/span count mismatch");
  // SpanCursor's contract is DOCUMENT order. Layout order (item, then its members) breaks
  // it for semantically-adopted members whose bytes live elsewhere in the file (Go methods
  // under their receiver type) — attribution silently lost every ref inside them until the
  // table was sorted. Each pair carries its layout id, so sorting cannot shift identity.
  spans.sort_by_key(|(range, _)| range.start);
  (entities, spans)
}

impl FileExtractor for OutlineExtractor {
  fn handles(&self, path: &str) -> bool {
    OutlineExtractor::handles(self, path)
  }

  fn extract_into<'i>(
    &self,
    interner: &'i vorpal_resolve::Interner,
    path: &str,
    source: &str,
    writer: &mut KgWriter,
    references: &mut Vec<Reference<'i>>,
  ) {
    // One extraction path for live and incremental ingest: build the product, apply it.
    if let Some(prod) = self.extract_product(path, source) {
      crate::pipeline::apply_product(interner, path, prod, writer, references);
    }
  }
}

#[cfg(test)]
mod identity_tests {
  use super::OutlineExtractor;
  use crate::extraction_identity as id;

  #[test]
  fn extraction_identity_distinguishes_grammar_and_rules() {
    assert_ne!(id(1, 2), id(1, 3), "a rules change must change identity");
    assert_ne!(id(1, 2), id(4, 2), "a grammar change must change identity");
    assert_ne!(id(1, 2), id(2, 1), "combine is order-sensitive, not a plain XOR");
    assert_eq!(id(7, 9), id(7, 9), "deterministic");
  }

  #[test]
  fn rules_digest_is_source_derived_and_stable() {
    // The default extractor and one compiled from the same default source agree; the digest is
    // a pure function of the rule bytes, so it is stable and non-zero.
    let a = OutlineExtractor::new().unwrap();
    let b = OutlineExtractor::from_rules(vorpal_outline::DEFAULT_OUTLINE_RULES).unwrap();
    assert_eq!(a.rules_digest(), b.rules_digest());
    assert_ne!(a.rules_digest(), 0);
  }

  #[test]
  fn with_sources_digest_is_canonical() {
    use super::RuleSource;
    let src = |origin: &str, yaml: &str| RuleSource {
      origin: origin.into(),
      yaml: yaml.into(),
    };
    // A rule document for an always-compiled builtin keeps the fixture cheap; the digest
    // logic under test is language-agnostic.
    let rule_a = "id: xa\nlanguage: Rust\nrole: item\nsymbolType: function\nrule: {kind: function_item, has: {field: name, pattern: $N}}\nname: '$N'\n";
    let rule_b = "id: xb\nlanguage: Rust\nrole: item\nsymbolType: function\nrule: {kind: function_item, has: {field: name, pattern: $M}}\nname: '$M'\n";

    // Empty sources are byte-for-byte the bundled digest — introducing this feature must not
    // re-key any existing pure-builtin index.
    assert_eq!(
      OutlineExtractor::with_sources(&[]).unwrap().rules_digest(),
      OutlineExtractor::new().unwrap().rules_digest()
    );
    // Source order never matters (canonical origin sort)…
    let ab = OutlineExtractor::with_sources(&[src("a.yml", rule_a), src("b.yml", rule_b)])
      .unwrap()
      .rules_digest();
    let ba = OutlineExtractor::with_sources(&[src("b.yml", rule_b), src("a.yml", rule_a)])
      .unwrap()
      .rules_digest();
    assert_eq!(ab, ba);
    // …but origin and content both do.
    let renamed = OutlineExtractor::with_sources(&[src("c.yml", rule_a), src("b.yml", rule_b)])
      .unwrap()
      .rules_digest();
    assert_ne!(ab, renamed, "origin participates in identity");
    let edited = OutlineExtractor::with_sources(&[src("a.yml", rule_b), src("b.yml", rule_b)])
      .unwrap()
      .rules_digest();
    assert_ne!(ab, edited, "content participates in identity");
    // Exact duplicates collapse; same origin with different contents is refused loudly.
    let deduped = OutlineExtractor::with_sources(&[src("a.yml", rule_a), src("a.yml", rule_a), src("b.yml", rule_b)])
      .unwrap()
      .rules_digest();
    assert_eq!(ab, deduped);
    let conflict = OutlineExtractor::with_sources(&[src("a.yml", rule_a), src("a.yml", rule_b)]);
    assert!(conflict.is_err(), "conflicting same-origin sources must be rejected");
  }
}

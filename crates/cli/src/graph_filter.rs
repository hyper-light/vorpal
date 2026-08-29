//! Scan-side wiring of rule `graph:` sections (IMPROVEMENTS #5).
//!
//! Rules are compiled once per scan: the serde schema translates into index-layer predicate
//! types, selector external ids parse, and the grade floor becomes a packed confidence. Each
//! structural match is then post-filtered: every predicate must hold against the loaded
//! index generation, matches that fail are dropped **and reported to stderr with the
//! evidence-level reason** (an auditable candidate list, exactly what a migration needs to
//! review), and unavailable semantics follow the rule's `require` policy — error the scan,
//! drop the match, or wave it through.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use vorpal_config::{
  GraphRequire, MinimumGrade, RuleCollection, SerializableGraphFilter, SerializableGraphPredicate,
};
use vorpal_core::{Doc, NodeMatch};
use vorpal_index::graph_predicates::{GraphFacts, PredicateKind, PredicateOutcome, TargetSpec};

use crate::lang::SgLang;

/// One rule's `graph:` section, compiled to index types (capture spans instantiate per match).
struct CompiledFilter {
  require: GraphRequire,
  floor: u8,
  /// Generation content id the rule pins, if any (checked against the loaded facts).
  pin: Option<String>,
  predicates: Vec<(String, CompiledKind)>,
}

enum CompiledKind {
  ResolvesTo(TargetSpec),
  SameBindingAs(String),
  Calls(TargetSpec),
  Imports(TargetSpec),
  Implements(TargetSpec),
}

/// The scan's graph-filter state: at most one opened index shared by every graph-bearing
/// rule, plus the per-rule compiled filters. Rules without a `graph:` section cost nothing.
pub struct ScanGraphFilters {
  facts: Option<GraphFacts>,
  /// Why `facts` is `None` (index missing/unloadable) — replayed per match under `require`.
  unavailable: Option<String>,
  by_rule: HashMap<String, CompiledFilter>,
}

impl ScanGraphFilters {
  /// No graph-bearing rules: every filter call is a passthrough.
  pub fn empty() -> Self {
    Self {
      facts: None,
      unavailable: None,
      by_rule: HashMap::new(),
    }
  }

  /// Compile every rule's `graph:` section and open the index they name. `cli_index`
  /// (--index) overrides rule-declared locations; relative paths resolve against
  /// `proj_dir`. Rules that declare *different* indexes are an error — one scan evaluates
  /// against one fact base. When the index cannot be opened, compilation still succeeds if
  /// no rule set `require: error`; the unavailability is then applied per match.
  pub fn compile(
    rules: &RuleCollection<SgLang>,
    cli_index: Option<&Path>,
    proj_dir: &Path,
  ) -> Result<Self> {
    let mut sections: Vec<(String, SerializableGraphFilter)> = Vec::new();
    rules.for_each_rule(|rule| {
      if let Some(graph) = &rule.graph {
        sections.push((rule.id.clone(), graph.clone()));
      }
    });
    if sections.is_empty() {
      return Ok(Self::empty());
    }

    let mut by_rule = HashMap::new();
    let mut declared: Option<String> = None;
    let mut strictest_require = GraphRequire::Ignore;
    for (id, section) in &sections {
      if let Some(dir) = &section.index {
        match &declared {
          None => declared = Some(dir.clone()),
          Some(existing) if existing != dir => bail!(
            "rules declare different graph indexes ({existing} vs {dir}); \
             pass --index to choose one for this scan"
          ),
          Some(_) => {}
        }
      }
      if section.require == GraphRequire::Error {
        strictest_require = GraphRequire::Error;
      }
      by_rule.insert(id.clone(), compile_filter(id, section)?);
    }

    let root = cli_index
      .map(Path::to_path_buf)
      .or_else(|| declared.map(|d| proj_dir.join(d)))
      .unwrap_or_else(|| proj_dir.join(".vorpal/index"));
    let (facts, unavailable) = match GraphFacts::open(&root, None) {
      Ok(facts) => (Some(facts), None),
      Err(reason) => {
        if strictest_require == GraphRequire::Error {
          bail!("graph predicates need an index: {reason}");
        }
        (None, Some(reason))
      }
    };
    Ok(Self {
      facts,
      unavailable,
      by_rule,
    })
  }

  /// Post-filter one rule's matches in `path`. Surviving matches return; dropped ones are
  /// reported to stderr with the per-predicate reason. `require: error` turns unavailable
  /// semantics into a scan error here.
  pub fn filter<'t, D: Doc>(
    &self,
    rule_id: &str,
    path: &Path,
    matches: Vec<NodeMatch<'t, D>>,
  ) -> Result<Vec<NodeMatch<'t, D>>> {
    let Some(filter) = self.by_rule.get(rule_id) else {
      return Ok(matches);
    };
    let facts = match (&self.facts, &self.unavailable) {
      (Some(facts), _) => {
        if let Some(pin) = &filter.pin {
          let live = facts.generation();
          if pin.trim_start_matches("gen/") != live {
            return self.apply_unavailable(
              filter,
              rule_id,
              path,
              matches,
              &format!("rule pins generation {pin:?} but the index is at {live:?}"),
            );
          }
        }
        facts
      }
      (None, Some(reason)) => {
        let reason = reason.clone();
        return self.apply_unavailable(filter, rule_id, path, matches, &reason);
      }
      (None, None) => return Ok(matches),
    };

    let mut kept = Vec::with_capacity(matches.len());
    'matches: for m in matches {
      let env = m.get_env();
      for (capture, kind) in &filter.predicates {
        let Some(anchor) = env.get_match(capture) else {
          audit(rule_id, path, &m, capture, "capture did not participate in this match");
          continue 'matches;
        };
        let span = to_span(anchor.range());
        let kind = match kind {
          CompiledKind::ResolvesTo(spec) => PredicateKind::ResolvesTo(spec.clone()),
          CompiledKind::Calls(spec) => PredicateKind::Calls(spec.clone()),
          CompiledKind::Imports(spec) => PredicateKind::Imports(spec.clone()),
          CompiledKind::Implements(spec) => PredicateKind::Implements(spec.clone()),
          CompiledKind::SameBindingAs(other) => {
            let Some(other_node) = env.get_match(other) else {
              audit(rule_id, path, &m, other, "capture did not participate in this match");
              continue 'matches;
            };
            PredicateKind::SameBindingAs {
              other: to_span(other_node.range()),
            }
          }
        };
        match facts.evaluate(path, span, &kind, filter.floor) {
          PredicateOutcome::Holds(_) => {}
          PredicateOutcome::Fails(reason) => {
            audit(rule_id, path, &m, capture, &reason);
            continue 'matches;
          }
          PredicateOutcome::Unavailable(reason) => match filter.require {
            GraphRequire::Error => bail!("[{rule_id}] {}: {reason}", path.display()),
            GraphRequire::Skip => {
              audit(rule_id, path, &m, capture, &format!("unavailable: {reason}"));
              continue 'matches;
            }
            GraphRequire::Ignore => {}
          },
        }
      }
      kept.push(m);
    }
    Ok(kept)
  }

  fn apply_unavailable<'t, D: Doc>(
    &self,
    filter: &CompiledFilter,
    rule_id: &str,
    path: &Path,
    matches: Vec<NodeMatch<'t, D>>,
    reason: &str,
  ) -> Result<Vec<NodeMatch<'t, D>>> {
    match filter.require {
      GraphRequire::Error => bail!("[{rule_id}] {}: {reason}", path.display()),
      GraphRequire::Skip => {
        for m in &matches {
          audit(rule_id, path, m, "-", &format!("unavailable: {reason}"));
        }
        Ok(Vec::new())
      }
      GraphRequire::Ignore => Ok(matches),
    }
  }
}

/// One dropped-candidate line on stderr: enough to review the site by hand — the whole point
/// of the predicate layer is that unproven rewrites become an explicit worklist, not edits.
fn audit<D: Doc>(rule_id: &str, path: &Path, m: &NodeMatch<'_, D>, capture: &str, reason: &str) {
  let range = m.range();
  eprintln!(
    "vorpal: [{rule_id}] {}:{}..{} not rewritten (${capture}): {reason}",
    path.display(),
    range.start,
    range.end
  );
}

fn to_span(range: std::ops::Range<usize>) -> (u32, u32) {
  (range.start as u32, range.end as u32)
}

fn compile_filter(rule_id: &str, section: &SerializableGraphFilter) -> Result<CompiledFilter> {
  let floor = match section.minimum_grade {
    MinimumGrade::Exact => 100,
    MinimumGrade::Constrained => 90,
    MinimumGrade::Heuristic => 1,
  };
  let mut predicates = Vec::with_capacity(section.predicates.len());
  for predicate in &section.predicates {
    predicates.push((predicate.capture.clone(), compile_kind(rule_id, predicate)?));
  }
  Ok(CompiledFilter {
    require: section.require,
    floor,
    pin: section.generation.clone(),
    predicates,
  })
}

fn compile_kind(rule_id: &str, predicate: &SerializableGraphPredicate) -> Result<CompiledKind> {
  let spec = |selector: &vorpal_config::GraphTargetSelector| -> Result<TargetSpec> {
    let external_id = match &selector.external_id {
      Some(text) => Some(parse_eid(text).ok_or_else(|| {
        anyhow::anyhow!("[{rule_id}] invalid externalId {text:?} (want eid:<32 hex>)")
      })?),
      None => None,
    };
    Ok(TargetSpec {
      name: selector.name.clone(),
      path_suffix: selector.path.clone(),
      external_id,
    })
  };
  if let Some(selector) = &predicate.resolves_to {
    return Ok(CompiledKind::ResolvesTo(spec(selector)?));
  }
  if let Some(other) = &predicate.same_binding_as {
    return Ok(CompiledKind::SameBindingAs(other.clone()));
  }
  if let Some(selector) = &predicate.calls {
    return Ok(CompiledKind::Calls(spec(selector)?));
  }
  if let Some(selector) = &predicate.imports {
    return Ok(CompiledKind::Imports(spec(selector)?));
  }
  if let Some(selector) = &predicate.implements {
    return Ok(CompiledKind::Implements(spec(selector)?));
  }
  // Schema validation guarantees arity 1; this is unreachable through rule loading.
  bail!("[{rule_id}] graph predicate for ${} selects nothing", predicate.capture)
}

/// `eid:<32 hex>` or bare hex → the 128-bit durable id.
fn parse_eid(text: &str) -> Option<u128> {
  let hex = text.strip_prefix("eid:").unwrap_or(text);
  (hex.len() == 32).then(|| u128::from_str_radix(hex, 16).ok()).flatten()
}

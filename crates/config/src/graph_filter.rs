//! Graph predicates (IMPROVEMENTS #5): structural matches filtered by repository facts.
//!
//! A rule's `graph:` section names predicates that a structural match must additionally
//! satisfy against a vorpal index — "does this capture resolve to *that* symbol", "do these
//! two captures bind the same definition", "does the enclosing definition call X". The
//! section is a **post-filter over matches**, deliberately outside the `rule:` composition
//! tree: the inherited matcher stays byte-compatible with upstream, and graph predicates
//! compose with structure the same way `constraints` does — every listed predicate must hold
//! (conjunction) for the match to survive.
//!
//! This module is schema only (serde + validation). Evaluation lives with the index (the
//! only crate that can open a generation and its evidence sidecar); the scan surface wires
//! the two together and reports near-miss candidates for auditability.
//!
//! Contract points the assessment requires a rule to be able to state:
//! - **which index/generation** the facts must come from (`index`, `generation`);
//! - **what happens when semantics are unavailable** (`require`: no index on disk, a
//!   generation mismatch, or a language the resolver has no semantics for);
//! - **which resolution grades are accepted** (`minimumGrade`);
//! - a **heuristic edge below the floor is a non-match, never an error** — but scan surfaces
//!   it as an explicit unrewritten candidate, so migrations stay auditable.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `graph:` section of a rule: repository-fact predicates a match must satisfy.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SerializableGraphFilter {
  /// Index directory the predicates evaluate against. Relative paths resolve against the
  /// scanned project root. A CLI `--index` argument overrides this. When absent entirely,
  /// scan falls back to `.vorpal/index` under the project root.
  pub index: Option<String>,
  /// Generation content id (the `gen/<id>` name) this rule's facts must come from. When the
  /// live index's CURRENT generation differs, semantics count as **unavailable** and
  /// `require` decides what happens — a rule pinned to reviewed facts never silently
  /// evaluates against newer ones.
  pub generation: Option<String>,
  /// What to do when graph semantics are unavailable (no index, generation mismatch, or a
  /// file outside the index).
  #[serde(default)]
  pub require: GraphRequire,
  /// Minimum resolution grade an edge must carry to satisfy any predicate here.
  /// Below-floor edges are non-matches (reported as candidates, never rewritten).
  #[serde(default)]
  pub minimum_grade: MinimumGrade,
  /// The predicates; **all** must hold for the match to survive.
  pub predicates: Vec<SerializableGraphPredicate>,
}

/// Behavior when the facts a predicate needs cannot be obtained at all.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum GraphRequire {
  /// Fail the scan loudly. The default: a rule that asked for proofs must not degrade into
  /// a structural-only rule because an index was missing.
  #[default]
  Error,
  /// Drop the match (treat unavailable as unprovable, err toward not matching).
  Skip,
  /// Keep the match (treat the predicate as vacuously true) — for rules where graph facts
  /// only *sharpen* an already-safe structural match.
  Ignore,
}

/// The grade floor: which resolution grades satisfy a predicate.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum MinimumGrade {
  /// Only same-file lexical bindings.
  Exact,
  /// Exact or single-candidate cross-file resolutions (the default): proven targets,
  /// never tie guesses.
  #[default]
  Constrained,
  /// Any emitted edge, including labelled tie picks.
  Heuristic,
}

/// One predicate anchored on a rule capture (meta-variable name **without** the `$` sigil,
/// exactly like `constraints` keys). Exactly one predicate field must be set.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SerializableGraphPredicate {
  /// The meta-variable whose matched span anchors the predicate.
  pub capture: String,
  /// The reference at the capture's span must resolve (at or above the grade floor) to a
  /// definition matching this selector.
  pub resolves_to: Option<GraphTargetSelector>,
  /// This capture and the named other capture must resolve to the **same** definition.
  pub same_binding_as: Option<String>,
  /// The definition enclosing the capture must have a `calls` edge to a definition matching
  /// this selector.
  pub calls: Option<GraphTargetSelector>,
  /// The capture's file must have an `imports` edge to a target matching this selector.
  pub imports: Option<GraphTargetSelector>,
  /// The definition enclosing the capture must have an `implements` edge to a definition
  /// matching this selector.
  pub implements: Option<GraphTargetSelector>,
}

impl SerializableGraphPredicate {
  /// The number of predicate fields set — must be exactly one.
  pub fn arity(&self) -> usize {
    [
      self.resolves_to.is_some(),
      self.same_binding_as.is_some(),
      self.calls.is_some(),
      self.imports.is_some(),
      self.implements.is_some(),
    ]
    .iter()
    .filter(|set| **set)
    .count()
  }
}

/// Selects the definition a predicate must reach. Every given field must hold (conjunction);
/// at least one must be given. `externalId` is the durable content-derived identity
/// (`eid:<32 hex>` accepted with or without the prefix) and is the recommended pin for
/// migration rules — it survives rebuilds and file moves.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphTargetSelector {
  /// Definition name (exact).
  pub name: Option<String>,
  /// Defining file path suffix (`src/util.rs` matches `.../src/util.rs`).
  pub path: Option<String>,
  /// Durable external id, `eid:<32 hex>` or bare hex.
  pub external_id: Option<String>,
}

impl GraphTargetSelector {
  pub fn is_empty(&self) -> bool {
    self.name.is_none() && self.path.is_none() && self.external_id.is_none()
  }
}

/// Validation errors for the `graph:` section, raised at rule-load time so a malformed rule
/// never silently scans structurally.
#[derive(Debug, thiserror::Error)]
pub enum GraphFilterError {
  #[error("graph predicate for capture `{0}` must set exactly one of resolvesTo/sameBindingAs/calls/imports/implements")]
  PredicateArity(String),
  #[error("graph predicate target selector for capture `{0}` selects nothing (empty)")]
  EmptySelector(String),
  #[error("graph section has no predicates")]
  NoPredicates,
}

impl SerializableGraphFilter {
  /// Structural validation (predicate arity, non-empty selectors). Capture-name existence is
  /// checked by the rule config against its defined meta-variables.
  pub fn validate(&self) -> Result<(), GraphFilterError> {
    if self.predicates.is_empty() {
      return Err(GraphFilterError::NoPredicates);
    }
    for predicate in &self.predicates {
      if predicate.arity() != 1 {
        return Err(GraphFilterError::PredicateArity(predicate.capture.clone()));
      }
      for selector in [
        &predicate.resolves_to,
        &predicate.calls,
        &predicate.imports,
        &predicate.implements,
      ]
      .into_iter()
      .flatten()
      {
        if selector.is_empty() {
          return Err(GraphFilterError::EmptySelector(predicate.capture.clone()));
        }
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_the_assessment_shape_and_validates() {
    let yaml = r#"
index: .vorpal/index
minimumGrade: exact
predicates:
  - capture: METHOD
    resolvesTo:
      externalId: "eid:00112233445566778899aabbccddeeff"
"#;
    let filter: SerializableGraphFilter = serde_yaml::from_str(yaml).unwrap();
    filter.validate().unwrap();
    assert_eq!(filter.minimum_grade, MinimumGrade::Exact);
    assert_eq!(filter.require, GraphRequire::Error, "unavailability errs by default");
    assert_eq!(filter.predicates[0].capture, "METHOD");
  }

  #[test]
  fn rejects_zero_and_two_predicate_kinds() {
    let none: SerializableGraphFilter = serde_yaml::from_str(
      "predicates:\n  - capture: A\n",
    )
    .unwrap();
    assert!(matches!(
      none.validate(),
      Err(GraphFilterError::PredicateArity(_))
    ));
    let two: SerializableGraphFilter = serde_yaml::from_str(
      "predicates:\n  - capture: A\n    sameBindingAs: B\n    resolvesTo:\n      name: x\n",
    )
    .unwrap();
    assert!(matches!(
      two.validate(),
      Err(GraphFilterError::PredicateArity(_))
    ));
    let empty: SerializableGraphFilter = serde_yaml::from_str(
      "predicates:\n  - capture: A\n    resolvesTo: {}\n",
    )
    .unwrap();
    assert!(matches!(
      empty.validate(),
      Err(GraphFilterError::EmptySelector(_))
    ));
  }
}

//! Rule-set and language-environment wire encoding (docs/REMOTE.md §1).
//!
//! The coordinator ships its **post-overwrite** rule set — the exact `SerializableRuleConfig`s
//! (severities included) its own `RuleCollection` was compiled from — plus the raw util-rule YAML
//! files that fed `GlobalRules`. The agent re-compiles through the *same* code path a local scan
//! uses (`parse_global_utils` → `RuleConfig::try_from` → `RuleCollection::try_new`), so rule
//! semantics cannot fork between local and remote.
//!
//! Determinism (I3): rule structs contain `HashMap`s, so the shipped document is **canonical
//! JSON** (recursively sorted keys) and the `blake3` digest is computed once over exactly those
//! bytes; the agent verifies the digest against the received bytes and never re-serializes.
//! Enum-shaped rule fields round-trip through `serde_yaml`'s `singleton_map_recursive`, the same
//! representation rule files use on disk.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_yaml::with::singleton_map_recursive;

use vorpal_config::{
  DeserializeEnv, GlobalRules, RuleCollection, RuleConfig, SerializableGlobalRule,
  SerializableRuleConfig,
};
use vorpal_wire::{RulePayload, to_canonical_json};

use crate::config::ProjectConfig;
use crate::lang::{CustomLang, LanguageGlobs, SerializableInjection, SgLang};
use crate::utils::ErrorContext as EC;

/// Serialize a value the way rule files represent it (`singleton_map_recursive` enum encoding),
/// into a JSON value ready for canonicalization.
fn rule_repr_to_json<T: Serialize>(value: &T) -> Result<serde_json::Value> {
  struct AsRuleRepr<'a, T>(&'a T);
  impl<T: Serialize> Serialize for AsRuleRepr<'_, T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
      singleton_map_recursive::serialize(self.0, serializer)
    }
  }
  Ok(serde_json::to_value(AsRuleRepr(value))?)
}

/// Deserialize a value from the rule-file representation (inverse of [`rule_repr_to_json`]).
fn rule_repr_from_json<T: for<'de> Deserialize<'de>>(value: serde_json::Value) -> Result<T> {
  singleton_map_recursive::deserialize(value).map_err(|e: serde_json::Error| anyhow!(e))
}

/// Encode the coordinator's compiled rule set + raw util YAMLs as a canonical, digest-carrying
/// payload. `globals_yaml` must be sorted deterministically by the caller (it is: by file path).
pub fn encode_scan_rules(
  collection: &RuleCollection<SgLang>,
  globals_yaml: Vec<String>,
) -> Result<RulePayload> {
  let mut rule_values = Vec::new();
  let mut first_err: Option<anyhow::Error> = None;
  collection.for_each_rule(|rule| {
    if first_err.is_some() {
      return;
    }
    // `RuleConfig` derefs to the `SerializableRuleConfig` it was compiled from — the
    // post-overwrite source of truth.
    let inner: &SerializableRuleConfig<SgLang> = rule;
    match rule_repr_to_json(inner) {
      Ok(v) => rule_values.push(v),
      Err(e) => first_err = Some(e.context(format!("cannot serialize rule `{}`", rule.id))),
    }
  });
  if let Some(e) = first_err {
    return Err(e);
  }
  let doc = serde_json::json!({ "globals": globals_yaml, "rules": rule_values });
  Ok(RulePayload::new(to_canonical_json(&doc)))
}

/// Agent side: verify the digest over the received bytes, then compile globals and rules through
/// the exact local code path.
pub fn decode_scan_rules(payload: &RulePayload) -> Result<RuleCollection<SgLang>> {
  if !payload.verify() {
    return Err(anyhow!(
      "rule payload digest mismatch — refusing to scan with unverified rules"
    ));
  }
  let doc: serde_json::Value =
    serde_json::from_slice(&payload.bytes).context("malformed rule payload")?;
  let globals_yaml = doc
    .get("globals")
    .and_then(|g| g.as_array())
    .ok_or_else(|| anyhow!("rule payload missing `globals`"))?;
  let mut utils: Vec<SerializableGlobalRule<SgLang>> = Vec::with_capacity(globals_yaml.len());
  for y in globals_yaml {
    let yaml = y
      .as_str()
      .ok_or_else(|| anyhow!("global util must be a YAML string"))?;
    utils.push(vorpal_config::from_str(yaml).context(EC::InvalidGlobalUtils)?);
  }
  let globals: GlobalRules =
    DeserializeEnv::<SgLang>::parse_global_utils(utils).context(EC::InvalidGlobalUtils)?;
  let rule_values = doc
    .get("rules")
    .and_then(|r| r.as_array())
    .ok_or_else(|| anyhow!("rule payload missing `rules`"))?;
  let mut configs = Vec::with_capacity(rule_values.len());
  for value in rule_values {
    let ser: SerializableRuleConfig<SgLang> = rule_repr_from_json(value.clone())
      .with_context(|| format!("cannot decode shipped rule {value}"))?;
    let id = ser.id.clone();
    let config = RuleConfig::try_from(ser, &globals)
      .map_err(|e| anyhow!("cannot compile shipped rule `{id}`: {e}"))?;
    configs.push(config);
  }
  RuleCollection::try_new(configs).context(EC::GlobPattern)
}

/// The project language environment a job must reproduce on the node **before** rule
/// deserialization (`SgLang::Custom` resolves by registered name). Shipped as canonical JSON.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct LangEnv {
  pub project_dir: Option<String>,
  pub custom_languages: Option<HashMap<String, CustomLang>>,
  pub language_globs: Option<LanguageGlobs>,
  pub language_injections: Vec<SerializableInjection>,
}

impl LangEnv {
  pub fn from_project(project: Option<&ProjectConfig>) -> Self {
    let Some(p) = project else {
      return Self::default();
    };
    Self {
      project_dir: Some(p.project_dir.to_string_lossy().into_owned()),
      custom_languages: p.custom_languages.clone(),
      language_globs: p.language_globs.clone(),
      language_injections: p.language_injections.clone(),
    }
  }

  pub fn is_empty(&self) -> bool {
    self.custom_languages.is_none()
      && self.language_globs.is_none()
      && self.language_injections.is_empty()
  }

  pub fn encode(&self) -> Result<Option<Vec<u8>>> {
    if self.is_empty() {
      return Ok(None);
    }
    Ok(Some(to_canonical_json(&serde_json::to_value(self)?)))
  }

  pub fn decode(bytes: &[u8]) -> Result<Self> {
    serde_json::from_slice(bytes).context("malformed lang env payload")
  }

  /// Mirror `config::register_custom_language` exactly (same call order: custom languages →
  /// globs → injections). Custom-language dylib paths resolve against `project_dir`; on loopback
  /// that path is shared with the coordinator, on later transports the dylib must be present on
  /// the node (surfaced as an error, never ignored).
  pub fn apply(&self) -> Result<()> {
    if let Some(custom) = &self.custom_languages {
      let base = self
        .project_dir
        .as_ref()
        .ok_or_else(|| anyhow!("custom languages shipped without a project dir"))?;
      SgLang::register_custom_language(std::path::Path::new(base), custom.clone())?;
    }
    if let Some(globs) = &self.language_globs {
      SgLang::register_globs(globs.clone())?;
    }
    SgLang::register_injections(self.language_injections.clone())?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use vorpal_config::from_yaml_string;

  fn compile(yaml: &str) -> RuleCollection<SgLang> {
    let configs = from_yaml_string(yaml, &Default::default()).expect("rule parses");
    RuleCollection::try_new(configs).expect("collection builds")
  }

  const RULES: &str = r#"
id: no-unwrap
language: Rust
severity: error
message: don't unwrap $A
note: "prefer `?`"
rule:
  pattern: $A.unwrap()
constraints:
  A: { regex: "^[a-z]" }
labels:
  A: { style: primary, message: 'the unwrapped value' }
files: ['src/**']
metadata:
  zeta: 1
  alpha: [1, 2]
---
id: relational
language: Rust
rule:
  all:
    - pattern: Some($X)
    - inside: { kind: function_item, stopBy: end }
utils:
  in-test: { inside: { kind: function_item } }
"#;

  #[test]
  fn rules_round_trip_bit_exactly_through_the_wire() {
    let collection = compile(RULES);
    let payload = encode_scan_rules(&collection, vec![]).expect("encodes");
    assert!(payload.verify());
    let decoded = decode_scan_rules(&payload).expect("decodes and compiles");
    assert_eq!(decoded.total_rule_count(), collection.total_rule_count());
    // Re-encoding the decoded collection must produce the identical canonical bytes — this is
    // the round-trip fidelity the digest scheme rests on.
    let payload2 = encode_scan_rules(&decoded, vec![]).expect("re-encodes");
    assert_eq!(
      payload.bytes, payload2.bytes,
      "canonical rule bytes must be stable"
    );
    assert_eq!(payload.digest, payload2.digest);
  }

  #[test]
  fn tampered_payload_is_refused() {
    let collection = compile(RULES);
    let mut payload = encode_scan_rules(&collection, vec![]).expect("encodes");
    let last = payload.bytes.len() - 1;
    payload.bytes[last] ^= 0x01;
    assert!(
      decode_scan_rules(&payload).is_err(),
      "digest mismatch must refuse"
    );
  }

  #[test]
  fn global_utils_ship_and_resolve() {
    // A rule referencing a global util compiles on the agent only if globals travel with it.
    let globals_yaml =
      vec!["id: is-some\nlanguage: Rust\nrule: { pattern: Some($A) }\n".to_string()];
    let utils: Vec<SerializableGlobalRule<SgLang>> = globals_yaml
      .iter()
      .map(|y| vorpal_config::from_str(y).expect("global parses"))
      .collect();
    let globals = DeserializeEnv::<SgLang>::parse_global_utils(utils).expect("globals compile");
    let configs = from_yaml_string(
      "id: uses-global\nlanguage: Rust\nrule: { matches: is-some }\n",
      &globals,
    )
    .expect("rule using global parses");
    let collection = RuleCollection::try_new(configs).expect("collection");
    let payload = encode_scan_rules(&collection, globals_yaml).expect("encodes");
    let decoded = decode_scan_rules(&payload).expect("agent compiles rule against shipped globals");
    assert_eq!(decoded.total_rule_count(), 1);
  }
}

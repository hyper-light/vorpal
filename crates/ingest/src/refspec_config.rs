//! Serialized reference-extraction specs (F-M4): the YAML mirror of `references::RefSpec`, so
//! dynamic languages can ship call/import/type extraction as *data*. Builtin specs stay as the
//! `&'static` authoring consts; the round-trip test below proves this form can express every
//! one of them, so nothing about a language's extraction is const-only.
//!
//! Kind-name policy: builtins resolve leniently (families share specs — `JS_LIKE` names kinds
//! TSX has and JavaScript lacks; absent kinds simply never dispatch). User/dynamic specs are
//! **strict**: an entry whose `kind` is unknown to the grammar is a registration-time error
//! naming the language, origin, and kind — a typo must never become a silently-dead spec. Each
//! call/import/implements entry can opt out with `optional: true` (for specs deliberately
//! shared across grammar variants); for plain kind lists (`types`, `typeParams`) the escape is
//! omitting the entry.

use serde::{Deserialize, Serialize};
use vorpal_core::Language;
use vorpal_lang_registry::SgLang;

use crate::references::{
  CallSpecData, HandlerAtData, ImplSpecData, ImportSpecData, QualSourceData, RefSpecData,
  RequestSpecData, RouteSpecData, SelData, TextAction,
};

/// How to locate the referenced sub-node inside a matched node — the tagged mirror of
/// `references::Sel`. YAML tag forms: `firstNamedChild`, `!field name`, `!fieldLast name`,
/// `!childOfKind [kind, …]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SerializableSel {
  #[default]
  FirstNamedChild,
  Field(String),
  FieldLast(String),
  ChildOfKind(Vec<String>),
}

impl From<&SerializableSel> for SelData {
  fn from(sel: &SerializableSel) -> Self {
    match sel {
      SerializableSel::FirstNamedChild => SelData::FirstNamedChild,
      SerializableSel::Field(name) => SelData::Field(name.clone()),
      SerializableSel::FieldLast(name) => SelData::FieldLast(name.clone()),
      SerializableSel::ChildOfKind(kinds) => SelData::ChildOfKind(kinds.clone()),
    }
  }
}

impl From<&SelData> for SerializableSel {
  fn from(sel: &SelData) -> Self {
    match sel {
      SelData::FirstNamedChild => SerializableSel::FirstNamedChild,
      SelData::Field(name) => SerializableSel::Field(name.clone()),
      SelData::FieldLast(name) => SerializableSel::FieldLast(name.clone()),
      SelData::ChildOfKind(kinds) => SerializableSel::ChildOfKind(kinds.clone()),
    }
  }
}

/// Where an import's source-module qualifier lives — mirror of `references::QualSource`.
/// YAML tag forms: `none`, `targetPath`, `!nodeField module_name`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SerializableQualSource {
  #[default]
  None,
  NodeField(String),
  TargetPath,
}

impl From<&SerializableQualSource> for QualSourceData {
  fn from(source: &SerializableQualSource) -> Self {
    match source {
      SerializableQualSource::None => QualSourceData::None,
      SerializableQualSource::NodeField(field) => QualSourceData::NodeField(field.clone()),
      SerializableQualSource::TargetPath => QualSourceData::TargetPath,
    }
  }
}

impl From<&QualSourceData> for SerializableQualSource {
  fn from(source: &QualSourceData) -> Self {
    match source {
      QualSourceData::None => SerializableQualSource::None,
      QualSourceData::NodeField(field) => SerializableQualSource::NodeField(field.clone()),
      QualSourceData::TargetPath => SerializableQualSource::TargetPath,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SerializableCallSpec {
  pub kind: String,
  #[serde(default, skip_serializing_if = "is_default_sel")]
  pub callee: SerializableSel,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub receiver_field: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub scope_field: Option<String>,
  /// Strict-kind escape hatch: this entry may name a kind the grammar lacks (shared specs).
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SerializableImportSpec {
  pub kind: String,
  #[serde(default, skip_serializing_if = "is_default_sel")]
  pub target: SerializableSel,
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub string_target: bool,
  #[serde(default, skip_serializing_if = "is_default_qual")]
  pub qualifier: SerializableQualSource,
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SerializableImplSpec {
  pub kind: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub target: Option<SerializableSel>,
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub optional: bool,
}

/// `{ callee: require, action: importFirstArg }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SerializableTextRule {
  pub callee: String,
  pub action: SerializableTextAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SerializableTextAction {
  SkipDefinition,
  ImportFirstArg,
}

impl From<SerializableTextAction> for TextAction {
  fn from(action: SerializableTextAction) -> Self {
    match action {
      SerializableTextAction::SkipDefinition => TextAction::SkipDefinition,
      SerializableTextAction::ImportFirstArg => TextAction::ImportFirstArg,
    }
  }
}

impl From<TextAction> for SerializableTextAction {
  fn from(action: TextAction) -> Self {
    match action {
      TextAction::SkipDefinition => SerializableTextAction::SkipDefinition,
      TextAction::ImportFirstArg => SerializableTextAction::ImportFirstArg,
    }
  }
}

/// Where a route construct's handler name lives — mirror of `references::HandlerAt`.
/// YAML tag forms: `lastArgument`, `!unwrappedArgument 1`,
/// `!decoratedDefinition {ancestors: […], via: …}`, `!nextSibling [kind, …]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SerializableHandlerAt {
  LastArgument,
  UnwrappedArgument(u8),
  #[serde(rename_all = "camelCase")]
  DecoratedDefinition {
    ancestors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    via: Option<String>,
  },
  NextSibling(Vec<String>),
}

impl From<&SerializableHandlerAt> for HandlerAtData {
  fn from(at: &SerializableHandlerAt) -> Self {
    match at {
      SerializableHandlerAt::LastArgument => HandlerAtData::LastArgument,
      SerializableHandlerAt::UnwrappedArgument(index) => HandlerAtData::UnwrappedArgument(*index),
      SerializableHandlerAt::DecoratedDefinition { ancestors, via } => {
        HandlerAtData::DecoratedDefinition {
          ancestors: ancestors.clone(),
          via: via.clone(),
        }
      }
      SerializableHandlerAt::NextSibling(kinds) => HandlerAtData::NextSibling(kinds.clone()),
    }
  }
}

impl From<&HandlerAtData> for SerializableHandlerAt {
  fn from(at: &HandlerAtData) -> Self {
    match at {
      HandlerAtData::LastArgument => SerializableHandlerAt::LastArgument,
      HandlerAtData::UnwrappedArgument(index) => SerializableHandlerAt::UnwrappedArgument(*index),
      HandlerAtData::DecoratedDefinition { ancestors, via } => {
        SerializableHandlerAt::DecoratedDefinition {
          ancestors: ancestors.clone(),
          via: via.clone(),
        }
      }
      HandlerAtData::NextSibling(kinds) => SerializableHandlerAt::NextSibling(kinds.clone()),
    }
  }
}

/// An HTTP route registration construct — mirror of `references::RouteSpec`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SerializableRouteSpec {
  pub kind: String,
  pub name: Vec<SerializableSel>,
  pub names: Vec<String>,
  pub args: Vec<SerializableSel>,
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub path_any: bool,
  pub handler: SerializableHandlerAt,
  /// Strict-kind escape hatch: this entry may name a kind the grammar lacks (shared specs).
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub optional: bool,
}

impl SerializableRouteSpec {
  fn to_route_data(&self) -> RouteSpecData {
    RouteSpecData {
      kind: self.kind.clone(),
      name: self.name.iter().map(SelData::from).collect(),
      names: self.names.clone(),
      args: self.args.iter().map(SelData::from).collect(),
      path_any: self.path_any,
      handler: HandlerAtData::from(&self.handler),
    }
  }

  #[cfg(test)] // the round-trip expressiveness test's lifting direction
  fn from_route_data(data: &RouteSpecData) -> Self {
    Self {
      kind: data.kind.clone(),
      name: data.name.iter().map(SerializableSel::from).collect(),
      names: data.names.clone(),
      args: data.args.iter().map(SerializableSel::from).collect(),
      path_any: data.path_any,
      handler: SerializableHandlerAt::from(&data.handler),
      optional: false,
    }
  }
}

/// An HTTP client call construct — mirror of `references::RequestSpec`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SerializableRequestSpec {
  pub kind: String,
  pub name: Vec<SerializableSel>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub verb_names: Vec<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub get_names: Vec<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub event_names: Vec<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub receivers: Vec<String>,
  pub args: Vec<SerializableSel>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub method_from_arg: Option<u8>,
  /// Strict-kind escape hatch: this entry may name a kind the grammar lacks (shared specs).
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub optional: bool,
}

impl SerializableRequestSpec {
  fn to_request_data(&self) -> RequestSpecData {
    RequestSpecData {
      kind: self.kind.clone(),
      name: self.name.iter().map(SelData::from).collect(),
      verb_names: self.verb_names.clone(),
      get_names: self.get_names.clone(),
      event_names: self.event_names.clone(),
      receivers: self.receivers.clone(),
      args: self.args.iter().map(SelData::from).collect(),
      method_from_arg: self.method_from_arg,
    }
  }

  #[cfg(test)] // the round-trip expressiveness test's lifting direction
  fn from_request_data(data: &RequestSpecData) -> Self {
    Self {
      kind: data.kind.clone(),
      name: data.name.iter().map(SerializableSel::from).collect(),
      verb_names: data.verb_names.clone(),
      get_names: data.get_names.clone(),
      event_names: data.event_names.clone(),
      receivers: data.receivers.clone(),
      args: data.args.iter().map(SerializableSel::from).collect(),
      method_from_arg: data.method_from_arg,
      optional: false,
    }
  }
}

fn is_default_sel(sel: &SerializableSel) -> bool {
  *sel == SerializableSel::FirstNamedChild
}
fn is_default_qual(q: &SerializableQualSource) -> bool {
  *q == SerializableQualSource::None
}

/// One language's serialized reference-extraction spec — a 1:1 mirror of the authoring
/// `RefSpec`, self-describing via `language`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SerializableRefSpec {
  /// The language this spec extracts for (builtin name or registered custom language).
  pub language: String,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub calls: Vec<SerializableCallSpec>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub imports: Vec<SerializableImportSpec>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub text_rules: Vec<SerializableTextRule>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub types: Vec<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub implements: Vec<SerializableImplSpec>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub type_params: Vec<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub type_placeholders: Vec<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub static_callee_kinds: Vec<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub method_callee_kinds: Vec<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub self_receivers: Vec<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub routes: Vec<SerializableRouteSpec>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub requests: Vec<SerializableRequestSpec>,
}

impl SerializableRefSpec {
  /// Lower to walk data under the **strict** kind policy (user/dynamic specs): every
  /// non-`optional` dispatch kind must exist in `lang`'s grammar. `origin` names the source in
  /// errors. Optional entries naming unknown kinds are dropped (they could never match), which
  /// is exactly the builtin lenient behavior — but here it must be asked for, never implied.
  pub(crate) fn to_data(&self, lang: SgLang, origin: &str) -> Result<RefSpecData, String> {
    let known = |kind: &str| lang.kind_to_id(kind) != 0;
    let check = |kind: &str, what: &str, optional: bool| -> Result<bool, String> {
      if known(kind) {
        Ok(true)
      } else if optional {
        Ok(false)
      } else {
        Err(format!(
          "unknown node kind '{kind}' in {what} of ref spec for language '{}' ({origin}); \
           fix the kind or mark the entry `optional: true`",
          self.language
        ))
      }
    };

    let mut calls = Vec::with_capacity(self.calls.len());
    for entry in &self.calls {
      if check(&entry.kind, "calls", entry.optional)? {
        calls.push(CallSpecData {
          kind: entry.kind.clone(),
          callee: SelData::from(&entry.callee),
          receiver_field: entry.receiver_field.clone(),
          scope_field: entry.scope_field.clone(),
        });
      }
    }
    let mut imports = Vec::with_capacity(self.imports.len());
    for entry in &self.imports {
      if check(&entry.kind, "imports", entry.optional)? {
        imports.push(ImportSpecData {
          kind: entry.kind.clone(),
          target: SelData::from(&entry.target),
          string_target: entry.string_target,
          qualifier: QualSourceData::from(&entry.qualifier),
        });
      }
    }
    let mut implements = Vec::with_capacity(self.implements.len());
    for entry in &self.implements {
      if check(&entry.kind, "implements", entry.optional)? {
        implements.push(ImplSpecData {
          kind: entry.kind.clone(),
          target: entry.target.as_ref().map(SelData::from),
        });
      }
    }
    for kind in &self.types {
      check(kind, "types", false)?;
    }
    for kind in &self.type_params {
      check(kind, "typeParams", false)?;
    }

    let mut routes = Vec::with_capacity(self.routes.len());
    for entry in &self.routes {
      if check(&entry.kind, "routes", entry.optional)? {
        routes.push(entry.to_route_data());
      }
    }
    let mut requests = Vec::with_capacity(self.requests.len());
    for entry in &self.requests {
      if check(&entry.kind, "requests", entry.optional)? {
        requests.push(entry.to_request_data());
      }
    }

    Ok(RefSpecData {
      calls,
      imports,
      text_rules: self
        .text_rules
        .iter()
        .map(|rule| (rule.callee.clone(), rule.action.into()))
        .collect(),
      types: self.types.clone(),
      implements,
      type_params: self.type_params.clone(),
      type_placeholders: self.type_placeholders.clone(),
      static_callee_kinds: self.static_callee_kinds.clone(),
      method_callee_kinds: self.method_callee_kinds.clone(),
      self_receivers: self.self_receivers.clone(),
      routes,
      requests,
    })
  }

  /// Lower WITHOUT kind validation — the lenient path used by the round-trip expressiveness
  /// test to compare pure data (builtins are lenient by design; see the module docs).
  #[cfg(test)]
  pub(crate) fn to_data_lenient(&self) -> RefSpecData {
    RefSpecData {
      calls: self
        .calls
        .iter()
        .map(|entry| CallSpecData {
          kind: entry.kind.clone(),
          callee: SelData::from(&entry.callee),
          receiver_field: entry.receiver_field.clone(),
          scope_field: entry.scope_field.clone(),
        })
        .collect(),
      imports: self
        .imports
        .iter()
        .map(|entry| ImportSpecData {
          kind: entry.kind.clone(),
          target: SelData::from(&entry.target),
          string_target: entry.string_target,
          qualifier: QualSourceData::from(&entry.qualifier),
        })
        .collect(),
      text_rules: self
        .text_rules
        .iter()
        .map(|rule| (rule.callee.clone(), rule.action.into()))
        .collect(),
      types: self.types.clone(),
      implements: self
        .implements
        .iter()
        .map(|entry| ImplSpecData {
          kind: entry.kind.clone(),
          target: entry.target.as_ref().map(SelData::from),
        })
        .collect(),
      type_params: self.type_params.clone(),
      type_placeholders: self.type_placeholders.clone(),
      static_callee_kinds: self.static_callee_kinds.clone(),
      method_callee_kinds: self.method_callee_kinds.clone(),
      self_receivers: self.self_receivers.clone(),
      routes: self.routes.iter().map(SerializableRouteSpec::to_route_data).collect(),
      requests: self
        .requests
        .iter()
        .map(SerializableRequestSpec::to_request_data)
        .collect(),
    }
  }

  /// The serialized mirror of existing walk data — how the round-trip test lifts a builtin
  /// const into this format (and how tooling can dump one for a user to start from).
  #[cfg(test)]
  pub(crate) fn from_data(language: &str, data: &RefSpecData) -> Self {
    Self {
      language: language.to_string(),
      calls: data
        .calls
        .iter()
        .map(|c| SerializableCallSpec {
          kind: c.kind.clone(),
          callee: SerializableSel::from(&c.callee),
          receiver_field: c.receiver_field.clone(),
          scope_field: c.scope_field.clone(),
          optional: false,
        })
        .collect(),
      imports: data
        .imports
        .iter()
        .map(|i| SerializableImportSpec {
          kind: i.kind.clone(),
          target: SerializableSel::from(&i.target),
          string_target: i.string_target,
          qualifier: SerializableQualSource::from(&i.qualifier),
          optional: false,
        })
        .collect(),
      text_rules: data
        .text_rules
        .iter()
        .map(|(callee, action)| SerializableTextRule {
          callee: callee.clone(),
          action: (*action).into(),
        })
        .collect(),
      types: data.types.clone(),
      implements: data
        .implements
        .iter()
        .map(|i| SerializableImplSpec {
          kind: i.kind.clone(),
          target: i.target.as_ref().map(SerializableSel::from),
          optional: false,
        })
        .collect(),
      type_params: data.type_params.clone(),
      type_placeholders: data.type_placeholders.clone(),
      static_callee_kinds: data.static_callee_kinds.clone(),
      method_callee_kinds: data.method_callee_kinds.clone(),
      self_receivers: data.self_receivers.clone(),
      routes: data.routes.iter().map(SerializableRouteSpec::from_route_data).collect(),
      requests: data
        .requests
        .iter()
        .map(SerializableRequestSpec::from_request_data)
        .collect(),
    }
  }
}

/// Parse a YAML document stream of serialized ref specs (one or more `---`-separated specs).
pub fn parse_ref_specs(yaml: &str) -> Result<Vec<SerializableRefSpec>, String> {
  let mut specs = Vec::new();
  for doc in serde_yaml::Deserializer::from_str(yaml) {
    let spec = SerializableRefSpec::deserialize(doc).map_err(|e| format!("parse ref spec: {e}"))?;
    specs.push(spec);
  }
  Ok(specs)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::references::{RefSpecData, builtin_specs_for_test};

  /// The expressiveness proof the plan requires: every builtin authoring const survives
  /// data → YAML → parse → data unchanged, so the serialized form can express the entire
  /// builtin extraction surface — dynamic languages are not a second-class tier.
  #[test]
  fn every_builtin_spec_round_trips_through_yaml() {
    for (name, data) in builtin_specs_for_test() {
      let serial = SerializableRefSpec::from_data(&name, &data);
      let yaml = serde_yaml::to_string(&serial).expect("serialize");
      let parsed = parse_ref_specs(&yaml).expect("parse back");
      assert_eq!(parsed.len(), 1, "{name}: one document in, one out");
      assert_eq!(parsed[0].language, name);
      let back: RefSpecData = parsed[0].to_data_lenient();
      assert_eq!(back, data, "{name}: YAML round-trip must be lossless\n{yaml}");
    }
  }

  #[test]
  fn strict_kind_policy_names_the_typo_and_optional_escapes() {
    use std::str::FromStr;
    let lang = SgLang::from_str("rust").expect("builtin");
    let yaml = "language: rust\ncalls:\n  - kind: call_expresion\n    callee: !field function\n";
    let spec = &parse_ref_specs(yaml).expect("parses")[0];
    let err = spec.to_data(lang, "specs/rust.yml").expect_err("typo'd kind must fail");
    assert!(err.contains("call_expresion") && err.contains("specs/rust.yml"), "{err}");

    let yaml = "language: rust\ncalls:\n  - kind: call_expresion\n    callee: !field function\n    optional: true\n";
    let spec = &parse_ref_specs(yaml).expect("parses")[0];
    let data = spec.to_data(lang, "specs/rust.yml").expect("optional escapes");
    assert!(data.calls.is_empty(), "optional unknown entry drops, never dispatches");
  }
}

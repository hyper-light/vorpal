//! Rule loading and compilation for outline extraction.
//!
//! Outline rule files describe extractors in a YAML-friendly shape. This module
//! converts that schema into runtime rules with compiled vorpal matchers,
//! templates, and predicates. Traversal code should depend on the runtime
//! `Outline*Rule` types, not on serde defaults or config parsing details.

use serde::{Deserialize, Serialize};
use serde_yaml::{Deserializer, Error as YamlError, with::singleton_map_recursive::deserialize};
use std::borrow::Cow;
use thiserror::Error;
use vorpal_config::{
  GlobalRules, Rule, RuleConfig, RuleConfigError, RuleSerializeError, SerializableRewriter,
  SerializableRule, SerializableRuleConfig, SerializableRuleCore, Severity,
};
use vorpal_core::{
  Doc, Language, Node, NodeMatch,
  matcher::{Matcher, MatcherExt},
  meta_var::MetaVarEnv,
  replacer::{TemplateFix, TemplateFixError},
  source::Content,
};

use crate::model::{
  EntryRole, OutlineEntry, OutlineItem, OutlineMember, SourcePosition, SourceRange, SymbolType,
};
use crate::options::OutlineEntryDetail;

/// Serializable outline extractor definition loaded from an outline rule YAML document.
///
/// The `role` field selects the concrete rule shape. Item rules create top-level
/// entries. Member rules create direct child entries that can attach to eligible
/// item rules through `parentRuleIds`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum SerializableOutlineRule<L> {
  /// Top-level structure, like functions, classes, and imports.
  Item(SerializableItemRule<L>),
  /// Direct child structure under an item, such as fields, methods, or variants.
  Member(SerializableMemberRule<L>),
}

impl<L> SerializableOutlineRule<L> {
  pub fn common(&self) -> &SerializableOutlineCommon<L> {
    match self {
      Self::Item(rule) => &rule.common,
      Self::Member(rule) => &rule.common,
    }
  }
}

/// Shared serializable fields for every outline extractor.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializableOutlineCommon<L> {
  /// Stable extractor id used in diagnostics and member parent references.
  pub id: String,
  /// Language accepted by vorpal, including built-in and registered custom languages.
  pub language: L,
  /// LSP-compatible outline category produced by this extractor.
  pub symbol_type: SymbolType,
  /// vorpal rule-core fields used to select candidate syntax.
  #[serde(flatten)]
  pub matcher: SerializableRuleCore,
  /// Rewrite rules for `rewrite` transformation.
  pub rewriters: Option<Vec<SerializableRewriter>>,
  /// Name template evaluated from metavariables or transformed metavariables.
  pub name: String,
  /// Optional source-like signature template. The extractor falls back to the
  /// first non-empty matched source line when omitted.
  pub signature: Option<String>,
}

impl<L: Language> SerializableOutlineCommon<L> {
  fn into_rule_config(self) -> SerializableRuleConfig<L> {
    SerializableRuleConfig {
      core: self.matcher,
      fix: None,
      rewriters: self.rewriters,
      id: self.id,
      language: self.language,
      message: String::new(),
      note: None,
      severity: Severity::default(),
      labels: None,
      files: None,
      graph: None,
      ignores: None,
      url: None,
      metadata: None,
    }
  }
}

/// Item extractor for top-level file/module structure.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializableItemRule<L> {
  /// Common outline extractor fields.
  #[serde(flatten)]
  pub common: SerializableOutlineCommon<L>,
  /// Whether this item is an import/dependency edge.
  pub is_import: Option<SerializablePredicate>,
  /// Whether this item belongs to the file/module public surface.
  pub is_exported: Option<SerializablePredicate>,
  /// Name template (metavariables allowed) of the SAME-FILE item this one belongs to,
  /// for declarations that are members semantically but not syntactically — Go's
  /// `func (w Widget) Render()` declares `memberOf: $RECV` and is adopted as a member of
  /// the file's `Widget` item. An owner defined in another file leaves the item top-level
  /// (adoption is file-local by design; stated where it matters).
  pub member_of: Option<String>,
  /// Collect matches ANYWHERE in the file, including inside other items' subtrees, in a
  /// dedicated full-tree pass — for constructs that live in bodies by nature (HTTP route
  /// registrations inside `main`, decorators inside classes). Nested items never carry
  /// members and never suppress the traversal around them.
  #[serde(default)]
  pub nested: Option<bool>,
  /// Container transparency (namespaces, ambient modules): after this item is
  /// extracted, the traversal DESCENDS into its body and keeps matching items —
  /// the container's contents are items in their own right (classes with their
  /// own members), not members of the container. Without this, a matched item's
  /// subtree is never re-entered (functions inside functions stay internal).
  #[serde(default)]
  pub transparent: Option<bool>,
}

/// Member extractor for direct child structure under an item.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializableMemberRule<L> {
  /// Common outline extractor fields.
  #[serde(flatten)]
  pub common: SerializableOutlineCommon<L>,
  /// Eligible parent item extractor ids.
  pub parent_rule_ids: Vec<String>,
  /// Whether this member is syntactically public.
  pub is_public: Option<SerializablePredicate>,
}

/// Boolean derivation for outline flags.
///
/// A literal boolean sets the output flag directly. A rule object is evaluated
/// against the matched candidate node and sets the output flag from the match result.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SerializablePredicate {
  /// Literal boolean value.
  Literal(bool),
  /// vorpal predicate evaluated against the extracted candidate node.
  Rule(Box<SerializableRule>),
}

/// Shared parsed fields for every runnable outline extractor.
pub struct ExtractorCommon<L: Language> {
  /// Parsed vorpal rule config used to select candidate syntax.
  pub rule: RuleConfig<L>,
  /// LSP-compatible outline category produced by this extractor.
  pub symbol_type: SymbolType,
  /// Name template evaluated from metavariables or transformed metavariables.
  pub name: NameTemplate,
  /// Optional source-like signature template.
  pub signature: Option<TemplateFix>,
  /// Requested text detail for this entry.
  detail: OutlineEntryDetail,
}

#[derive(Debug, Error)]
pub enum OutlineRuleError {
  #[error(transparent)]
  RuleConfig(#[from] RuleConfigError),
  // TODO: this error message is vague
  #[error("Predicate rule is not correctly configured")]
  Predicate(#[from] RuleSerializeError),
  #[error(transparent)]
  Template(#[from] TemplateFixError),
  #[error("Member rule `{rule_id}` references unknown parent rule `{parent_id}`")]
  UnknownParentRuleId { rule_id: String, parent_id: String },
  #[error("Member rule `{rule_id}` cannot use member rule `{parent_id}` as a parent")]
  InvalidParentRuleRole { rule_id: String, parent_id: String },
}

impl<L: Language> ExtractorCommon<L> {
  pub fn try_from(
    common: SerializableOutlineCommon<L>,
    globals: &GlobalRules,
    detail: OutlineEntryDetail,
  ) -> Result<Self, OutlineRuleError> {
    let symbol_type = common.symbol_type;
    let transform_vars = transform_vars(&common.matcher);
    let compile = |tmpl| compile_template(tmpl, &common.language, &transform_vars);
    let name = NameTemplate::compile(&common.name, &common.language, &transform_vars)?;
    let signature = match detail {
      OutlineEntryDetail::Name => None,
      OutlineEntryDetail::Signature => common.signature.as_deref().map(compile).transpose()?,
    };
    let rule = RuleConfig::try_from(common.into_rule_config(), globals)?;
    Ok(Self {
      rule,
      symbol_type,
      name,
      signature,
      detail,
    })
  }

  // this function is not inherently bound to ExtractorCommon
  // just for convenience to avoid env
  fn compile_predicate(
    &self,
    predicate: Option<SerializablePredicate>,
    default: bool,
  ) -> Result<OutlinePredicate, OutlineRuleError> {
    let Some(predicate) = predicate else {
      return Ok(OutlinePredicate::Literal(default));
    };
    let ret = match predicate {
      SerializablePredicate::Literal(value) => OutlinePredicate::Literal(value),
      SerializablePredicate::Rule(rule) => {
        let env = self.rule.matcher.get_env(self.rule.language.clone());
        OutlinePredicate::Rule(env.deserialize_rule(*rule)?)
      }
    };
    Ok(ret)
  }
}

fn transform_vars(matcher: &SerializableRuleCore) -> Option<Vec<String>> {
  matcher
    .transform
    .as_ref()
    .map(|transform| transform.keys().cloned().collect())
}

fn compile_template<L: Language>(
  template: &str,
  language: &L,
  transform_vars: &Option<Vec<String>>,
) -> Result<TemplateFix, TemplateFixError> {
  if let Some(vars) = transform_vars {
    Ok(TemplateFix::with_transform(template, language, vars))
  } else {
    TemplateFix::try_new(template, language)
  }
}

/// A compiled name template, with a zero-allocation fast path for the dominant
/// shape: a template that is exactly one plain metavariable (`$NAME`) renders
/// by BORROWING the matched node's text. The template engine's per-render work
/// (leading-indent scan, byte-vector assembly, re-indent, `String` build)
/// measured ~17 % of stream-phase allocation samples across every language.
pub enum NameTemplate {
  /// The whole template is one metavariable naming a plain (non-transformed)
  /// capture. `fallback` is the compiled engine template for the rare capture
  /// shapes `get_match` cannot serve (multi captures) — exact old semantics.
  Trivial { var: String, fallback: TemplateFix },
  /// Literals, multiple variables, or transformed variables — the engine path.
  Template(TemplateFix),
}

impl NameTemplate {
  fn compile<L: Language>(
    template: &str,
    language: &L,
    transform_vars: &Option<Vec<String>>,
  ) -> Result<Self, TemplateFixError> {
    let fix = compile_template(template, language, transform_vars)?;
    let trivial_var = template.strip_prefix(language.meta_var_char()).filter(|rest| {
      !rest.is_empty()
        && rest
          .chars()
          .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && transform_vars
          .as_ref()
          .is_none_or(|vars| !vars.iter().any(|v| v == rest))
    });
    Ok(match trivial_var {
      Some(var) => NameTemplate::Trivial {
        var: var.to_string(),
        fallback: fix,
      },
      None => NameTemplate::Template(fix),
    })
  }

  fn render<'tree, D: Doc>(
    &self,
    node_match: &NodeMatch<'tree, D>,
    scratch: &mut RenderScratch<D>,
  ) -> Cow<'tree, str> {
    match self {
      NameTemplate::Trivial { var, fallback } => match node_match.get_env().get_match(var) {
        Some(node) => node.text(),
        None => Cow::Owned(render_template(fallback, node_match, scratch)),
      },
      NameTemplate::Template(fix) => Cow::Owned(render_template(fix, node_match, scratch)),
    }
  }

  pub fn used_vars(&self) -> std::collections::HashSet<&str> {
    match self {
      NameTemplate::Trivial { var, .. } => std::iter::once(var.as_str()).collect(),
      NameTemplate::Template(fix) => fix.used_vars(),
    }
  }
}

// imported/exported will be default accordingly to role
enum OutlinePredicate {
  Literal(bool),
  Rule(Rule),
}

impl OutlinePredicate {
  fn evaluate<D: Doc>(&self, node_match: &mut NodeMatch<D>) -> bool {
    match self {
      Self::Literal(value) => *value,
      Self::Rule(rule) => {
        // The predicate must see the main rule's bindings (metavar
        // consistency) but never keeps its own writes — `probe` runs it on
        // the live env and discards them byte-exactly. The old protocol
        // borrowed the env and let the predicate's first write clone it,
        // whole and at high-water capacity, once per predicate per item,
        // only to drop the clone with the verdict (ledger-sampled ~13 % of
        // kernel-scale stream allocations after pass 16).
        let node = node_match.get_node().clone();
        node_match
          .get_env_mut()
          .probe(|env| rule.match_node_with_env(node, env).is_some())
      }
    }
  }
}

/// Runnable item extractor for top-level file/module structure.
pub struct ItemExtractor<L: Language> {
  pub common: ExtractorCommon<L>,
  is_import: OutlinePredicate,
  is_exported: OutlinePredicate,
  member_of: Option<TemplateFix>,
  /// See [`SerializableItemRule::nested`].
  pub nested: bool,
  /// See [`SerializableItemRule::transparent`].
  pub transparent: bool,
}

impl<L: Language> ItemExtractor<L> {
  pub fn try_from(
    item: SerializableItemRule<L>,
    globals: &GlobalRules,
    detail: OutlineEntryDetail,
  ) -> Result<Self, OutlineRuleError> {
    let SerializableItemRule {
      common,
      is_import,
      is_exported,
      member_of,
      nested,
      transparent,
    } = item;
    let member_of = member_of
      .as_deref()
      .map(|tmpl| compile_template(tmpl, &common.language, &transform_vars(&common.matcher)))
      .transpose()?;
    let common = ExtractorCommon::try_from(common, globals, detail)?;
    let is_import = common.compile_predicate(is_import, false)?;
    let is_exported = common.compile_predicate(is_exported, true)?;
    Ok(Self {
      common,
      is_import,
      is_exported,
      member_of,
      nested: nested.unwrap_or(false),
      transparent: transparent.unwrap_or(false),
    })
  }

  pub fn match_node<'tree, D: Doc>(&self, node: &Node<'tree, D>) -> Option<NodeMatch<'tree, D>> {
    self.common.rule.matcher.match_node(node.clone())
  }

  /// The hot-loop form: failed attempts recycle the caller's scratch env
  /// instead of buying fresh vectors per candidate (see
  /// `MatcherExt::match_node_reusing`).
  pub fn match_node_reusing<'tree, D: Doc>(
    &self,
    node: &Node<'tree, D>,
    scratch: &mut MetaVarEnv<'tree, D>,
  ) -> Option<NodeMatch<'tree, D>> {
    self
      .common
      .rule
      .matcher
      .match_node_reusing(node.clone(), scratch)
  }

  pub fn extract<'tree, D: Doc>(
    &self,
    node_match: &mut NodeMatch<'tree, D>,
    members: Vec<OutlineMember<'tree>>,
    scratch: &mut RenderScratch<D>,
  ) -> OutlineItem<'tree> {
    OutlineItem {
      entry: self.common.extract_entry(EntryRole::Item, node_match, scratch),
      is_import: self.is_import.evaluate(node_match),
      is_exported: self.is_exported.evaluate(node_match),
      members,
    }
  }

  /// The resolved owner name for `memberOf` items — `None` when the rule declares no
  /// owner or the template renders empty (no receiver captured: nothing to adopt into).
  pub fn resolve_member_of<'tree, D: Doc>(
    &self,
    node_match: &NodeMatch<'tree, D>,
    scratch: &mut RenderScratch<D>,
  ) -> Option<String> {
    let template = self.member_of.as_ref()?;
    let owner = render_template(template, node_match, scratch);
    (!owner.is_empty()).then_some(owner)
  }
}

/// Runnable member extractor for direct child structure under an item.
pub struct MemberExtractor<L: Language> {
  pub common: ExtractorCommon<L>,
  pub parent_rule_ids: Vec<String>,
  is_public: OutlinePredicate,
}

impl<L: Language> MemberExtractor<L> {
  pub fn try_from(
    member: SerializableMemberRule<L>,
    globals: &GlobalRules,
    detail: OutlineEntryDetail,
  ) -> Result<Self, OutlineRuleError> {
    let SerializableMemberRule {
      common,
      parent_rule_ids,
      is_public,
    } = member;
    let common = ExtractorCommon::try_from(common, globals, detail)?;
    let is_public = common.compile_predicate(is_public, true)?;
    Ok(Self {
      common,
      parent_rule_ids,
      is_public,
    })
  }

  pub fn match_node<'tree, D: Doc>(&self, node: &Node<'tree, D>) -> Option<NodeMatch<'tree, D>> {
    self.common.rule.matcher.match_node(node.clone())
  }

  /// The hot-loop form — see `ItemExtractor::match_node_reusing`.
  pub fn match_node_reusing<'tree, D: Doc>(
    &self,
    node: &Node<'tree, D>,
    scratch: &mut MetaVarEnv<'tree, D>,
  ) -> Option<NodeMatch<'tree, D>> {
    self
      .common
      .rule
      .matcher
      .match_node_reusing(node.clone(), scratch)
  }

  pub fn extract<'tree, D: Doc>(
    &self,
    node_match: &mut NodeMatch<'tree, D>,
    scratch: &mut RenderScratch<D>,
  ) -> OutlineMember<'tree> {
    OutlineMember {
      entry: self.common.extract_entry(EntryRole::Member, node_match, scratch),
      is_public: self.is_public.evaluate(node_match),
    }
  }
}

impl<L: Language> ExtractorCommon<L> {
  fn extract_entry<'tree, D: Doc>(
    &self,
    role: EntryRole,
    node_match: &NodeMatch<'tree, D>,
    scratch: &mut RenderScratch<D>,
  ) -> OutlineEntry<'tree> {
    let node = node_match.get_node();
    OutlineEntry {
      role,
      symbol_type: self.symbol_type,
      name: self.name.render(node_match, scratch),
      range: source_range(node),
      signature: self.render_signature(node_match, scratch),
      ast_kind: match node.kind_static() {
        // tree-sitter kinds are 'static — borrow instead of building a String
        // per extracted definition (8.8M at kernel scale).
        Some(kind) => Cow::Borrowed(kind),
        None => Cow::Owned(node.kind().into_owned()),
      },
    }
  }

  fn render_signature<'tree, D: Doc>(
    &self,
    node_match: &NodeMatch<'tree, D>,
    scratch: &mut RenderScratch<D>,
  ) -> Cow<'tree, str> {
    match self.detail {
      OutlineEntryDetail::Name => Cow::Borrowed(""),
      OutlineEntryDetail::Signature => self
        .signature
        .as_ref()
        .map(|template| Cow::Owned(render_template(template, node_match, scratch)))
        .unwrap_or_else(|| default_signature(node_match.get_node())),
    }
  }
}

fn render_template<D: Doc>(
  template: &TemplateFix,
  node_match: &NodeMatch<D>,
  scratch: &mut RenderScratch<D>,
) -> String {
  // One owned String per rendered value (the live datum); every intermediate
  // rides the caller's per-file scratch (see `TemplateFix::render_into`).
  template.render_into(node_match, scratch);
  <D::Source as Content>::encode_bytes(scratch).to_string()
}

/// Per-file render buffer threaded through the outline walk — the type the
/// template engine renders into for doc source `D`.
pub type RenderScratch<D> = vorpal_core::meta_var::Underlying<D>;

/// First non-empty trimmed line of the node's text — BORROWED when the source
/// is (the overwhelming case; owned only for owned-cow docs), replacing a
/// per-entry `String` build for every rule without a `signature:` template.
fn default_signature<'tree, D: Doc>(node: &Node<'tree, D>) -> Cow<'tree, str> {
  match node.text() {
    Cow::Borrowed(text) => text
      .lines()
      .find_map(|line| {
        let trimmed = line.trim();
        (!trimmed.is_empty()).then_some(trimmed)
      })
      .map(Cow::Borrowed)
      .unwrap_or(Cow::Borrowed("")),
    Cow::Owned(text) => text
      .lines()
      .find_map(|line| {
        let trimmed = line.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
      })
      .map(Cow::Owned)
      .unwrap_or(Cow::Borrowed("")),
  }
}

fn source_range<D: Doc>(node: &Node<D>) -> SourceRange {
  let start = node.start_pos();
  let end = node.end_pos();
  SourceRange {
    byte_offset: node.range(),
    start: SourcePosition {
      line: start.line(),
      column: start.column(node),
    },
    end: SourcePosition {
      line: end.line(),
      column: end.column(node),
    },
  }
}

/// Parse a stream of YAML outline extractor documents.
pub fn parse_outline_rules<'a, L>(
  src: &'a str,
) -> Result<Vec<SerializableOutlineRule<L>>, YamlError>
where
  L: Deserialize<'a>,
{
  Deserializer::from_str(src).map(deserialize).collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use vorpal_core::tree_sitter::LanguageExt;
  use vorpal_language::SupportLang;

  fn parse_rule(src: &str) -> SerializableOutlineRule<SupportLang> {
    vorpal_config::from_str(src).expect("outline rule should deserialize")
  }

  #[test]
  fn deserializes_item_rule() {
    let rule = parse_rule(
      r#"
id: rust-struct
language: Rust
role: item
symbolType: struct
rule:
  pattern: $VIS struct $NAME { $$$BODY }
name: $NAME
isExported:
  has:
    regex: '^pub\b'
"#,
    );

    let SerializableOutlineRule::Item(item) = rule else {
      panic!("expected item rule");
    };
    assert_eq!(item.common.id, "rust-struct");
    assert_eq!(item.common.language, SupportLang::Rust);
    assert_eq!(item.common.symbol_type, SymbolType::Struct);
    assert_eq!(item.common.name, "$NAME");
    assert!(matches!(
      item.is_exported,
      Some(SerializablePredicate::Rule(_))
    ));
    assert!(item.is_import.is_none());
  }

  #[test]
  fn deserializes_member_rule() {
    let rule = parse_rule(
      r#"
id: rust-field
language: Rust
role: member
parentRuleIds: [rust-struct]
symbolType: field
rule:
  pattern: '$VIS $NAME: $TYPE'
name: $NAME
signature: '$VIS $NAME: $TYPE'
isPublic:
  has:
    regex: '^pub\b'
"#,
    );

    let SerializableOutlineRule::Member(member) = rule else {
      panic!("expected member rule");
    };
    assert_eq!(member.common.id, "rust-field");
    assert_eq!(member.parent_rule_ids, vec!["rust-struct"]);
    assert_eq!(member.common.symbol_type, SymbolType::Field);
    assert_eq!(
      member.common.signature.as_deref(),
      Some("$VIS $NAME: $TYPE")
    );
    assert!(matches!(
      member.is_public,
      Some(SerializablePredicate::Rule(_))
    ));
  }

  #[test]
  fn deserializes_literal_booleans() {
    let rule = parse_rule(
      r#"
id: rust-use
language: Rust
role: item
symbolType: module
rule:
  pattern: use $TARGET;
name: $TARGET
isImport: true
isExported: false
"#,
    );

    let SerializableOutlineRule::Item(item) = rule else {
      panic!("expected item rule");
    };
    assert!(matches!(
      item.is_import,
      Some(SerializablePredicate::Literal(true))
    ));
    assert!(matches!(
      item.is_exported,
      Some(SerializablePredicate::Literal(false))
    ));
  }

  #[test]
  fn deserializes_transform_and_rewriters() {
    let rule = parse_rule(
      r#"
id: rust-use
language: Rust
role: item
symbolType: module
rule:
  pattern: use $TARGET;
transform:
  NAME:
    replace:
      source: $TARGET
      replace: '^.*::'
      by: ''
rewriters:
  - id: trim
    rule:
      pattern: $A
    fix: $A
name: $NAME
isImport: true
"#,
    );

    let SerializableOutlineRule::Item(item) = rule else {
      panic!("expected item rule");
    };
    assert_eq!(item.common.name, "$NAME");
    assert!(item.common.matcher.transform.is_some());
    assert_eq!(item.common.rewriters.as_ref().unwrap()[0].id, "trim");
  }

  #[test]
  fn parses_yaml_document_stream() {
    let rules = parse_outline_rules::<SupportLang>(
      r#"
id: rust-struct
language: Rust
role: item
symbolType: struct
rule:
  pattern: struct $NAME { $$$BODY }
name: $NAME
---
id: rust-field
language: Rust
role: member
parentRuleIds: [rust-struct]
symbolType: field
rule:
  pattern: '$NAME: $TYPE'
name: $NAME
"#,
    )
    .expect("document stream should deserialize");

    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].common().id, "rust-struct");
    assert_eq!(rules[1].common().id, "rust-field");
  }

  #[test]
  fn parses_outline_common_to_runtime_rule() {
    let rule = parse_rule(
      r#"
id: ts-function
language: TypeScript
role: item
symbolType: function
rule:
  pattern: function $NAME() { $$$BODY }
name: $NAME
signature: function $NAME()
"#,
    );

    let SerializableOutlineRule::Item(item) = rule else {
      panic!("expected item rule");
    };
    let common = ExtractorCommon::try_from(
      item.common,
      &Default::default(),
      OutlineEntryDetail::Signature,
    )
    .expect("common rule should parse");

    assert_eq!(common.rule.id, "ts-function");
    assert_eq!(common.symbol_type, SymbolType::Function);
    assert!(common.name.used_vars().contains("NAME"));
    assert!(
      common
        .signature
        .as_ref()
        .is_some_and(|signature| signature.used_vars().contains("NAME"))
    );
  }

  #[test]
  fn parses_outline_item_rule() {
    let rule = parse_rule(
      r#"
id: ts-function
language: TypeScript
role: item
symbolType: function
rule:
  pattern: function $NAME() { $$$BODY }
name: $NAME
isImport: true
"#,
    );

    let SerializableOutlineRule::Item(item) = rule else {
      panic!("expected item rule");
    };
    let item = ItemExtractor::try_from(item, &Default::default(), OutlineEntryDetail::Signature)
      .expect("item rule should parse");

    assert_eq!(item.common.rule.id, "ts-function");
    assert!(matches!(item.is_import, OutlinePredicate::Literal(true)));
    assert!(matches!(item.is_exported, OutlinePredicate::Literal(true)));
  }

  #[test]
  fn predicate_rule_reuses_match_metavariables() {
    let rule = parse_rule(
      r#"
id: ts-class
language: TypeScript
role: item
symbolType: class
rule:
  pattern: class $NAME { $$$BODY }
name: $NAME
isExported:
  has:
    pattern:
      context: class A { $NAME() { $$$BODY } }
      selector: method_definition
"#,
    );

    let SerializableOutlineRule::Item(item) = rule else {
      panic!("expected item rule");
    };
    let item = ItemExtractor::try_from(item, &Default::default(), OutlineEntryDetail::Signature)
      .expect("item rule should parse");
    let root = SupportLang::TypeScript.grep("class Foo { bar() {} }");
    let class_node = root
      .root()
      .children()
      .find(|node| node.kind() == "class_declaration")
      .expect("class should exist");
    let mut node_match = item
      .match_node(&class_node)
      .expect("class should match item rule");
    let mut render_scratch = Vec::new();
    let outline = item.extract(&mut node_match, vec![], &mut render_scratch);

    assert_eq!(outline.entry.name.as_ref(), "Foo");
    assert!(!outline.is_exported);
  }

  #[test]
  fn parses_outline_member_rule() {
    let rule = parse_rule(
      r#"
id: ts-member
language: TypeScript
role: member
parentRuleIds: [ts-interface]
symbolType: field
rule:
  kind: property_signature
name: member
"#,
    );

    let SerializableOutlineRule::Member(member) = rule else {
      panic!("expected member rule");
    };
    let member =
      MemberExtractor::try_from(member, &Default::default(), OutlineEntryDetail::Signature)
        .expect("member rule should parse");

    assert_eq!(member.common.rule.id, "ts-member");
    assert_eq!(member.parent_rule_ids, vec!["ts-interface"]);
    assert!(matches!(member.is_public, OutlinePredicate::Literal(true)));
  }

  #[test]
  fn serializes_with_internal_role_tag() {
    let rule = SerializableOutlineRule::Item(SerializableItemRule {
      member_of: None,
      nested: None,
      transparent: None,
      common: SerializableOutlineCommon {
        id: "ts-function".into(),
        language: SupportLang::TypeScript,
        symbol_type: SymbolType::Function,
        matcher: SerializableRuleCore {
          rule: vorpal_config::from_str(
            r#"
pattern: function $NAME() { $$$BODY }
"#,
          )
          .expect("rule should deserialize"),
          constraints: None,
          utils: None,
          transform: None,
        },
        rewriters: None,
        name: "$NAME".into(),
        signature: Some("function $NAME()".into()),
      },
      is_import: None,
      is_exported: Some(SerializablePredicate::Literal(true)),
    });

    let value = serde_json::to_value(rule).expect("outline rule should serialize");

    assert_eq!(value["role"], "item");
    assert_eq!(value["id"], "ts-function");
    assert_eq!(value["symbolType"], "function");
    assert_eq!(value["isExported"], true);
  }
}

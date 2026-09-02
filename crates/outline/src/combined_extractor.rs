//! Combined outline extraction.
//!
//! Outline extraction has two matching phases. Top-level item extractors are
//! matched during a file-wide AST traversal, so they are indexed by node kind in
//! one dense table. Member extractors are only valid after a specific item
//! extractor has matched; they are grouped by parent item extractor id and then
//! indexed sparsely by child node kind inside that parent-scoped group.
//!
//! Extraction uses a single tree-sitter cursor-backed traversal instead of
//! `find_all` or a second member pass per matched item. The traversal has two
//! states: at file scope it matches item extractors; inside a matched item it
//! switches to the item's scoped member extractors until the cursor leaves that
//! item range.

use std::collections::HashMap;
use vorpal_config::GlobalRules;
use vorpal_core::{
  Language, Matcher, Node, NodeMatch,
  meta_var::MetaVarEnv,
  tree_sitter::{
    LanguageExt, StrDoc,
    traversal::{Prune, PruneSubtree},
  },
};

use crate::extractor::{
  ItemExtractor, MemberExtractor, OutlineRuleError, RenderScratch, SerializableOutlineRule,
};
use crate::model::{OutlineItem, OutlineMember};
use crate::options::OutlineExtractorOptions;

const POTENTIAL_KINDS_INVARIANT: &str =
  "compiled outline rules must have potential kinds because RuleConfig rejects unconstrained rules";

/// Runtime outline extractors organized for a shared item traversal.
pub struct CombinedExtractors<L: Language> {
  /// Top-level item extractors matched during the file-wide AST traversal.
  item_extractors: Vec<ItemExtractor<L>>,
  /// Dense node-kind index into `item_extractors`; shared across the whole file. Nested
  /// extractors are excluded — they run in their own full-tree pass.
  item_kind_index: Vec<Vec<usize>>,
  /// Node-kind index into `item_extractors` for `nested: true` rules (see
  /// [`crate::extractor::SerializableItemRule::nested`]); empty for almost every language.
  nested_kind_index: Vec<Vec<usize>>,
  /// Member extractors parsed once and referenced by parent-scoped groups below.
  member_extractors: Vec<MemberExtractor<L>>,
  /// Parent item extractor id to member extractors that may run inside it.
  member_index_by_parent: HashMap<String, MemberExtractorIndex>,
  /// Runtime filters and detail level requested by the caller.
  options: OutlineExtractorOptions,
}

struct ScopedMemberExtractors<'a, L: Language> {
  /// Shared member extractor storage owned by `CombinedExtractors`.
  extractors: &'a [MemberExtractor<L>],
  /// Parent-scoped index that selects members relevant to one matched item rule.
  index: &'a MemberExtractorIndex,
}

#[derive(Default)]
struct MemberExtractorIndex {
  /// Sparse node-kind index into `member_extractors` for scoped member traversal.
  kind_mapping: HashMap<u16, Vec<usize>>,
}

impl<L: Language> CombinedExtractors<L> {
  pub fn try_from(
    extractors: Vec<SerializableOutlineRule<L>>,
    globals: &GlobalRules,
  ) -> Result<Self, OutlineRuleError> {
    Self::try_from_rules(extractors, OutlineExtractorOptions::default(), globals)
  }

  pub fn try_from_rules(
    extractors: Vec<SerializableOutlineRule<L>>,
    options: OutlineExtractorOptions,
    globals: &GlobalRules,
  ) -> Result<Self, OutlineRuleError> {
    validate_parent_rule_ids(&extractors)?;
    let mut item_extractors = Vec::with_capacity(extractors.len());
    let mut member_extractors = Vec::with_capacity(extractors.len());
    // NB: if member option is None, we won't pass any member extractors
    // so this is safe to fallback to default as we won't use it
    let member_options = options.members.clone().unwrap_or_default();
    for extractor in extractors {
      if !options.retain_rule(&extractor) {
        continue;
      }
      match extractor {
        SerializableOutlineRule::Item(item) => {
          item_extractors.push(ItemExtractor::try_from(item, globals, options.detail)?);
        }
        SerializableOutlineRule::Member(member) => {
          member_extractors.push(MemberExtractor::try_from(
            member,
            globals,
            member_options.detail,
          )?);
        }
      }
    }
    Ok(Self::new_with_options(
      item_extractors,
      member_extractors,
      options,
    ))
  }

  fn new_with_options(
    item_extractors: Vec<ItemExtractor<L>>,
    member_extractors: Vec<MemberExtractor<L>>,
    options: OutlineExtractorOptions,
  ) -> Self {
    let item_kind_index = kind_index(&item_extractors, |e| !e.nested);
    let nested_kind_index = kind_index(&item_extractors, |e| e.nested);
    let member_index_by_parent = member_index_by_parent(&member_extractors);
    Self {
      item_extractors,
      item_kind_index,
      nested_kind_index,
      member_extractors,
      member_index_by_parent,
      options,
    }
  }

  fn member_scope_for(&self, parent_id: &str) -> Option<ScopedMemberExtractors<'_, L>> {
    self
      .member_index_by_parent
      .get(parent_id)
      .map(|index| ScopedMemberExtractors {
        extractors: &self.member_extractors,
        index,
      })
  }

  fn item_extractors_for_kind(&self, kind: u16) -> impl Iterator<Item = &ItemExtractor<L>> {
    self
      .item_kind_index
      .get(kind as usize)
      .map(Vec::as_slice)
      .unwrap_or(&[])
      .iter()
      .map(|&idx| &self.item_extractors[idx])
  }

  /// The raw item traversal over one subtree: PRE-adoption `(item, memberOf owner)`
  /// pairs in document order. [`CombinedExtractors::extract`] composes this with
  /// [`CombinedExtractors::adopt`]; walk reuse calls it per dirty top-level subtree and
  /// splices retained pairs around the result before adopting.
  pub fn extract_raw<'tree>(
    &self,
    root: Node<'tree, StrDoc<L>>,
  ) -> Vec<(OutlineItem<'tree>, Option<String>)>
  where
    L: LanguageExt,
  {
    OutlineItemIter {
      combined: self,
      traversal: Prune::new(&root),
      scratch: MetaVarEnv::new(),
      render_scratch: RenderScratch::<StrDoc<L>>::new(),
    }
    .collect()
  }

  /// The `memberOf` adoption pass over a complete file's collected pairs (see
  /// [`adopt_members`]) — file-global by design, so a spliced collection adopts
  /// identically to a fresh one. Takes `&self` only for inference-friendly call sites.
  pub fn adopt<'tree>(
    &self,
    collected: Vec<(OutlineItem<'tree>, Option<String>)>,
  ) -> Vec<OutlineItem<'tree>> {
    adopt_members(collected)
  }

  /// Whether this language declares `nested: true` item rules (their dedicated full-tree
  /// pass makes item extraction non-regional — walk reuse must fall back).
  pub fn has_nested(&self) -> bool {
    self.nested_kind_index.iter().any(|list| !list.is_empty())
  }

  pub fn extract<'a, 'tree>(
    &'a self,
    root: Node<'tree, StrDoc<L>>,
  ) -> impl Iterator<Item = OutlineItem<'tree>> + use<'a, 'tree, L>
  where
    L: LanguageExt,
  {
    // Materialize, then run the semantic-adoption pass (`memberOf` rules): items whose
    // declared owner is a same-file item become its members — Go's detached methods being
    // the motivating shape. File item counts are small; the traversal cost is identical.
    let collected: Vec<(OutlineItem<'tree>, Option<String>)> = self.extract_raw(root.clone());
    let mut items = adopt_members(collected);
    // Nested rules (`nested: true`): a dedicated full-tree pass, because the pruned item
    // traversal above never enters a matched item's subtree — exactly where route
    // registrations live. Skipped entirely (no walk) when the language declares none.
    if self.nested_kind_index.iter().any(|list| !list.is_empty()) {
      let mut scratch = MetaVarEnv::new();
      let mut render_scratch = RenderScratch::<StrDoc<L>>::new();
      for node in root.dfs() {
        for &idx in self
          .nested_kind_index
          .get(node.kind_id() as usize)
          .map(Vec::as_slice)
          .unwrap_or(&[])
        {
          if let Some(mut matched) = self.item_extractors[idx].match_node_reusing(&node, &mut scratch)
          {
            let item =
              self.item_extractors[idx].extract(&mut matched, vec![], &mut render_scratch);
            reclaim_env(&mut scratch, &mut matched);
            if self.options.keep_item(&item) {
              items.push(item);
            }
            break;
          }
        }
      }
    }
    items.into_iter()
  }

  fn match_item<'tree>(
    &self,
    node: &Node<'tree, StrDoc<L>>,
    scratch: &mut MetaVarEnv<'tree, StrDoc<L>>,
  ) -> Option<(&ItemExtractor<L>, NodeMatch<'tree, StrDoc<L>>)>
  where
    L: LanguageExt,
  {
    for extractor in self.item_extractors_for_kind(node.kind_id()) {
      if let Some(matched) = extractor.match_node_reusing(node, scratch) {
        return Some((extractor, matched));
      }
    }
    None
  }
}

impl<'a, L: Language> ScopedMemberExtractors<'a, L> {
  fn extractors_for_kind(&self, kind: u16) -> impl Iterator<Item = &MemberExtractor<L>> {
    self
      .index
      .kind_mapping
      .get(&kind)
      .map(Vec::as_slice)
      .unwrap_or(&[])
      .iter()
      .map(|&idx| &self.extractors[idx])
  }

  fn extract_member<'tree>(
    &self,
    node: &Node<'tree, StrDoc<L>>,
    scratch: &mut MetaVarEnv<'tree, StrDoc<L>>,
    render_scratch: &mut RenderScratch<StrDoc<L>>,
  ) -> Option<OutlineMember<'tree>>
  where
    L: LanguageExt,
  {
    for extractor in self.extractors_for_kind(node.kind_id()) {
      if let Some(mut matched) = extractor.match_node_reusing(node, scratch) {
        let member = extractor.extract(&mut matched, render_scratch);
        reclaim_env(scratch, &mut matched);
        return Some(member);
      }
    }
    None
  }
}

/// Recover a spent match's env into the scratch. An outline `NodeMatch` is
/// read by `extract` and dropped moments later — its env is not live data, so
/// its buffers (grown by every binding and relational label of the match) go
/// back into rotation. With failures already recycling via
/// `match_node_reusing`, this makes the per-file walk allocation-free at
/// steady state on the env side.
fn reclaim_env<'tree, D: vorpal_core::Doc>(
  scratch: &mut MetaVarEnv<'tree, D>,
  spent: &mut NodeMatch<'tree, D>,
) {
  let mut env = std::mem::take(spent.get_env_mut());
  env.reset_for_reuse();
  *scratch = env;
}

struct OutlineItemIter<'a, 'tree, L: LanguageExt> {
  combined: &'a CombinedExtractors<L>,
  traversal: Prune<'tree, L>,
  /// One env recycled across every failed match attempt in this file's walk
  /// — see `MatcherExt::match_node_reusing`.
  scratch: MetaVarEnv<'tree, StrDoc<L>>,
  /// One template-render buffer for the file's walk — every rendered name,
  /// signature, and owner rides it (see `TemplateFix::render_into`).
  render_scratch: RenderScratch<StrDoc<L>>,
}

impl<'a, 'tree, L: LanguageExt> Iterator for OutlineItemIter<'a, 'tree, L> {
  type Item = (OutlineItem<'tree>, Option<String>);

  fn next(&mut self) -> Option<Self::Item> {
    loop {
      let node = self.traversal.current_node()?;
      if let Some(item) = self.visit_current_node(node) {
        return Some(item);
      }
    }
  }
}

/// The `memberOf` adoption pass. Deterministic and total:
/// * the adoption target for a name is the FIRST non-import, non-adopting item bearing it
///   (file order);
/// * adopted items append to the target's members after its syntactic ones, in file order,
///   with `role: Member` and `is_public` taken from the item's exported flag;
/// * an item whose owner is absent from this file (defined elsewhere, or a template that
///   matched nothing) stays a top-level item in its original position — adoption is
///   file-local, never a guess;
/// * an item that already has members of its own is never adopted (nesting members under
///   members has no model), and an item can never adopt into itself.
fn adopt_members(collected: Vec<(OutlineItem<'_>, Option<String>)>) -> Vec<OutlineItem<'_>> {
  use std::collections::HashMap;
  if collected.iter().all(|(_, owner)| owner.is_none()) {
    return collected.into_iter().map(|(item, _)| item).collect();
  }
  // Target per name: first non-import item that is not itself adopting.
  let mut targets: HashMap<&str, usize> = HashMap::new();
  for (index, (item, owner)) in collected.iter().enumerate() {
    if owner.is_none() && !item.is_import {
      targets.entry(item.entry.name.as_ref()).or_insert(index);
    }
  }
  let adopts: Vec<Option<usize>> = collected
    .iter()
    .enumerate()
    .map(|(index, (item, owner))| {
      let owner = owner.as_deref()?;
      if !item.members.is_empty() {
        return None;
      }
      let target = *targets.get(owner)?;
      (target != index).then_some(target)
    })
    .collect();
  // Build the surviving top-level list, remembering where each original index landed.
  let mut placed: HashMap<usize, usize> = HashMap::new();
  let mut result: Vec<OutlineItem<'_>> = Vec::new();
  let mut adopted: Vec<(usize, OutlineItem<'_>)> = Vec::new();
  for (index, ((item, _), adopt)) in collected.into_iter().zip(&adopts).enumerate() {
    match adopt {
      Some(target) => adopted.push((*target, item)),
      None => {
        placed.insert(index, result.len());
        result.push(item);
      }
    }
  }
  for (target, item) in adopted {
    let Some(&at) = placed.get(&target) else {
      // The target itself was adopted elsewhere (cannot happen: targets never adopt) —
      // keep the item top-level rather than lose it.
      result.push(item);
      continue;
    };
    let OutlineItem {
      mut entry,
      is_exported,
      ..
    } = item;
    entry.role = crate::model::EntryRole::Member;
    result[at].members.push(crate::model::OutlineMember {
      entry,
      is_public: is_exported,
    });
  }
  result
}

impl<'a, 'tree, L: LanguageExt> OutlineItemIter<'a, 'tree, L> {
  fn visit_current_node(
    &mut self,
    node: Node<'tree, StrDoc<L>>,
  ) -> Option<(OutlineItem<'tree>, Option<String>)> {
    let combined = self.combined;
    let item_subtree = self.traversal.current_subtree();
    let Some((extractor, mut node_match)) = combined.match_item(&node, &mut self.scratch) else {
      self.traversal.descend();
      return None;
    };
    let member_of = extractor.resolve_member_of(&node_match, &mut self.render_scratch);
    if extractor.transparent {
      // Transparent containers (namespaces, ambient modules): extract the container
      // itself, then keep ITEM-matching inside its body — its contents are items in
      // their own right (classes with their own members), never members of the
      // container. See `SerializableItemRule::transparent`.
      let item = extractor.extract(&mut node_match, vec![], &mut self.render_scratch);
      reclaim_env(&mut self.scratch, &mut node_match);
      self.traversal.descend();
      return combined
        .options
        .keep_item(&item)
        .then_some((item, member_of));
    }
    let members = self.collect_members_for_item(&extractor.common.rule.id, item_subtree);
    let item = extractor.extract(&mut node_match, members, &mut self.render_scratch);
    reclaim_env(&mut self.scratch, &mut node_match);
    combined
      .options
      .keep_item(&item)
      .then_some((item, member_of))
  }

  fn collect_members_for_item(
    &mut self,
    item_rule_id: &str,
    item_subtree: PruneSubtree<'tree>,
  ) -> Vec<OutlineMember<'tree>> {
    let Some(member_extractors) = self.combined.member_scope_for(item_rule_id) else {
      self.traversal.skip_subtree();
      return vec![];
    };
    self.traversal.descend();
    collect_scoped_members(
      &mut self.traversal,
      member_extractors,
      &self.combined.options,
      item_subtree,
      &mut self.scratch,
      &mut self.render_scratch,
    )
  }
}

fn validate_parent_rule_ids<L>(
  extractors: &[SerializableOutlineRule<L>],
) -> Result<(), OutlineRuleError> {
  let mut rule_roles = HashMap::new();
  for extractor in extractors {
    rule_roles.insert(
      extractor.common().id.as_str(),
      matches!(extractor, SerializableOutlineRule::Item(_)),
    );
  }
  for extractor in extractors {
    let SerializableOutlineRule::Member(member) = extractor else {
      continue;
    };
    for parent_id in &member.parent_rule_ids {
      match rule_roles.get(parent_id.as_str()) {
        Some(true) => {}
        Some(false) => {
          return Err(OutlineRuleError::InvalidParentRuleRole {
            rule_id: member.common.id.clone(),
            parent_id: parent_id.clone(),
          });
        }
        None => {
          return Err(OutlineRuleError::UnknownParentRuleId {
            rule_id: member.common.id.clone(),
            parent_id: parent_id.clone(),
          });
        }
      }
    }
  }
  Ok(())
}

fn collect_scoped_members<'a, 'tree, L: LanguageExt>(
  traversal: &mut Prune<'tree, L>,
  member_extractors: ScopedMemberExtractors<'a, L>,
  options: &OutlineExtractorOptions,
  item_subtree: PruneSubtree<'tree>,
  scratch: &mut MetaVarEnv<'tree, StrDoc<L>>,
  render_scratch: &mut RenderScratch<StrDoc<L>>,
) -> Vec<OutlineMember<'tree>> {
  let mut members = vec![];
  while let Some(node) = traversal.current_node() {
    if traversal.has_left_subtree(item_subtree) {
      break;
    }
    if let Some(member) = member_extractors.extract_member(&node, scratch, render_scratch) {
      if options.keep_member(&member) {
        members.push(member);
      }
      traversal.skip_subtree();
    } else {
      traversal.descend();
    }
  }
  members
}

fn push_kind_mapping(mapping: &mut Vec<Vec<usize>>, kind: usize, idx: usize) {
  while mapping.len() <= kind {
    mapping.push(vec![]);
  }
  mapping[kind].push(idx);
}

fn kind_index<L: Language>(
  item_extractors: &[ItemExtractor<L>],
  retain: impl Fn(&ItemExtractor<L>) -> bool,
) -> Vec<Vec<usize>> {
  let mut mapping = Vec::new();
  for (idx, extractor) in item_extractors.iter().enumerate() {
    if !retain(extractor) {
      continue;
    }
    let kinds = extractor
      .common
      .rule
      .matcher
      .potential_kinds()
      .expect(POTENTIAL_KINDS_INVARIANT);
    for kind in &kinds {
      push_kind_mapping(&mut mapping, kind, idx);
    }
  }
  mapping
}

fn member_index_by_parent<L: Language>(
  member_extractors: &[MemberExtractor<L>],
) -> HashMap<String, MemberExtractorIndex> {
  let mut mapping: HashMap<String, MemberExtractorIndex> = HashMap::new();
  for (idx, extractor) in member_extractors.iter().enumerate() {
    for parent_id in &extractor.parent_rule_ids {
      let index = mapping.entry(parent_id.clone()).or_default();
      let kinds = extractor
        .common
        .rule
        .matcher
        .potential_kinds()
        .expect(POTENTIAL_KINDS_INVARIANT);
      for kind in &kinds {
        index.kind_mapping.entry(kind as u16).or_default().push(idx);
      }
    }
  }
  mapping
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::extractor::parse_outline_rules;
  use crate::options::{OutlineEntryDetail, OutlineExtractorOptions, OutlineFlagFilter};
  use vorpal_core::tree_sitter::LanguageExt;
  use vorpal_language::SupportLang;

  #[test]
  fn combines_extractors_by_item_kind_and_parent_id() {
    let extractors = parse_outline_rules::<SupportLang>(
      r#"
id: ts-function
language: TypeScript
role: item
symbolType: function
rule:
  pattern: function $NAME() { $$$BODY }
name: $NAME
---
id: ts-member
language: TypeScript
role: member
parentRuleIds: [ts-function]
symbolType: field
rule:
  kind: identifier
name: member
---
id: ts-other-member
language: TypeScript
role: member
parentRuleIds: [ts-function]
symbolType: field
rule:
  kind: property_signature
name: other
"#,
    )
    .expect("extractors should deserialize");

    let combined = CombinedExtractors::try_from(extractors, &Default::default())
      .expect("extractors should parse");
    let function_kind = SupportLang::TypeScript.kind_to_id("function_declaration");
    let item_extractors = combined
      .item_extractors_for_kind(function_kind)
      .collect::<Vec<_>>();
    let member_extractors = combined
      .member_scope_for("ts-function")
      .expect("member extractors should exist");
    let identifier_kind = SupportLang::TypeScript.kind_to_id("identifier");
    let identifier_members = member_extractors
      .extractors_for_kind(identifier_kind)
      .collect::<Vec<_>>();

    assert!(combined.member_scope_for("missing").is_none());
    assert_eq!(item_extractors.len(), 1);
    assert_eq!(item_extractors[0].common.rule.id, "ts-function");
    assert_eq!(identifier_members.len(), 1);
    assert_eq!(identifier_members[0].common.rule.id, "ts-member");
  }

  #[test]
  fn rejects_unknown_member_parent_rule_id() {
    let extractors = parse_outline_rules::<SupportLang>(
      r#"
id: ts-member
language: TypeScript
role: member
parentRuleIds: [missing-parent]
symbolType: method
rule:
  kind: method_definition
name: member
"#,
    )
    .expect("extractors should deserialize");

    let Err(err) = CombinedExtractors::try_from(extractors, &Default::default()) else {
      panic!("unknown parent id should be rejected");
    };

    assert!(matches!(err, OutlineRuleError::UnknownParentRuleId { .. }));
    assert_eq!(
      err.to_string(),
      "Member rule `ts-member` references unknown parent rule `missing-parent`"
    );
  }

  #[test]
  fn rejects_member_parent_rule_id_that_points_to_member_rule() {
    let extractors = parse_outline_rules::<SupportLang>(
      r#"
id: ts-parent-member
language: TypeScript
role: member
parentRuleIds: [ts-class]
symbolType: method
rule:
  kind: method_definition
name: parent
---
id: ts-member
language: TypeScript
role: member
parentRuleIds: [ts-parent-member]
symbolType: method
rule:
  kind: method_definition
name: child
---
id: ts-class
language: TypeScript
role: item
symbolType: class
rule:
  pattern: class $NAME { $$$BODY }
name: $NAME
"#,
    )
    .expect("extractors should deserialize");

    let Err(err) = CombinedExtractors::try_from(extractors, &Default::default()) else {
      panic!("member parent ids should only reference item rules");
    };

    assert!(matches!(
      err,
      OutlineRuleError::InvalidParentRuleRole { .. }
    ));
    assert_eq!(
      err.to_string(),
      "Member rule `ts-member` cannot use member rule `ts-parent-member` as a parent"
    );
  }

  #[test]
  fn extracts_items_without_visiting_matched_item_descendants() {
    let extractors = parse_outline_rules::<SupportLang>(
      r#"
id: ts-function
language: TypeScript
role: item
symbolType: function
rule:
  pattern: function $NAME() { $$$BODY }
name: $NAME
"#,
    )
    .expect("extractors should deserialize");
    let combined = CombinedExtractors::try_from(extractors, &Default::default())
      .expect("extractors should parse");
    let grep = SupportLang::TypeScript.grep(
      r#"
function outer() {
  function inner() {}
}
function after() {}
"#,
    );

    let items = combined.extract(grep.root()).collect::<Vec<_>>();
    let names = items
      .iter()
      .map(|item| item.entry.name.as_ref())
      .collect::<Vec<_>>();

    assert_eq!(names, vec!["outer", "after"]);
  }

  #[test]
  fn transparent_containers_expose_inner_items() {
    let extractors = parse_outline_rules::<SupportLang>(
      r#"
id: ts-namespace
language: TypeScript
role: item
symbolType: module
rule:
  kind: internal_module
  has:
    field: name
    pattern: $NAME
name: $NAME
transparent: true
---
id: ts-class
language: TypeScript
role: item
symbolType: class
rule:
  pattern: class $NAME { $$$BODY }
name: $NAME
---
id: ts-method
language: TypeScript
role: member
parentRuleIds: [ts-class]
symbolType: method
rule:
  pattern:
    context: 'class A { $NAME() { $$$BODY } }'
    selector: method_definition
name: $NAME
"#,
    )
    .expect("extractors should deserialize");
    let combined = CombinedExtractors::try_from(extractors, &Default::default())
      .expect("extractors should parse");
    let grep = SupportLang::TypeScript.grep(
      r#"
namespace Outer {
  export class Widget {
    render() {}
  }
}
class Top {}
"#,
    );

    let items = combined.extract(grep.root()).collect::<Vec<_>>();
    let names = items
      .iter()
      .map(|item| item.entry.name.as_ref())
      .collect::<Vec<_>>();

    assert_eq!(names, vec!["Outer", "Widget", "Top"]);
    assert!(items[0].members.is_empty(), "container carries no members");
    assert_eq!(items[1].members.len(), 1, "inner class keeps its own members");
    assert_eq!(items[1].members[0].entry.name, "render");
  }

  #[test]
  fn extracts_members_only_from_matched_parent_items() {
    let extractors = parse_outline_rules::<SupportLang>(
      r#"
id: ts-class
language: TypeScript
role: item
symbolType: class
rule:
  pattern: class $NAME { $$$BODY }
name: $NAME
signature: class $NAME
---
id: ts-method
language: TypeScript
role: member
parentRuleIds: [ts-class]
symbolType: method
rule:
  pattern:
    context: class A { $NAME() { $$$BODY } }
    selector: method_definition
name: $NAME
signature: $NAME()
"#,
    )
    .expect("extractors should deserialize");
    let combined = CombinedExtractors::try_from(extractors, &Default::default())
      .expect("extractors should parse");
    let grep = SupportLang::TypeScript.grep(
      r#"
class Box {
  parse() {
    function local() {}
  }
}
function standalone() {}
"#,
    );

    let items = combined.extract(grep.root()).collect::<Vec<_>>();

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].entry.name, "Box");
    assert_eq!(items[0].members.len(), 1);
    assert_eq!(items[0].members[0].entry.name, "parse");
    assert_eq!(items[0].members[0].entry.signature, "parse()");
  }

  #[test]
  fn resumes_item_matching_after_member_scope() {
    let extractors = parse_outline_rules::<SupportLang>(
      r#"
id: ts-class
language: TypeScript
role: item
symbolType: class
rule:
  pattern: class $NAME { $$$BODY }
name: $NAME
---
id: ts-function
language: TypeScript
role: item
symbolType: function
rule:
  pattern: function $NAME() { $$$BODY }
name: $NAME
---
id: ts-method
language: TypeScript
role: member
parentRuleIds: [ts-class]
symbolType: method
rule:
  pattern:
    context: class A { $NAME() { $$$BODY } }
    selector: method_definition
name: $NAME
"#,
    )
    .expect("extractors should deserialize");
    let combined = CombinedExtractors::try_from(extractors, &Default::default())
      .expect("extractors should parse");
    let grep = SupportLang::TypeScript.grep(
      r#"
class Box {
  parse() {}
}
function after() {}
"#,
    );

    let items = combined.extract(grep.root()).collect::<Vec<_>>();

    let names = items
      .iter()
      .map(|item| item.entry.name.as_ref())
      .collect::<Vec<_>>();
    assert_eq!(names, vec!["Box", "after"]);
    assert_eq!(items[0].members.len(), 1);
    assert_eq!(items[0].members[0].entry.name, "parse");
    assert!(items[1].members.is_empty());
  }

  #[test]
  fn compile_options_disable_members_and_name_only_signatures() {
    let extractors = parse_outline_rules::<SupportLang>(
      r#"
id: ts-class
language: TypeScript
role: item
symbolType: class
rule:
  pattern: class $NAME { $$$BODY }
name: $NAME
signature: class $NAME
---
id: ts-method
language: TypeScript
role: member
parentRuleIds: [ts-class]
symbolType: method
rule:
  pattern:
    context: class A { $NAME() { $$$BODY } }
    selector: method_definition
name: $NAME
signature: $NAME()
"#,
    )
    .expect("extractors should deserialize");
    let options = OutlineExtractorOptions {
      members: None,
      detail: OutlineEntryDetail::Name,
      ..Default::default()
    };
    let combined = CombinedExtractors::try_from_rules(extractors, options, &Default::default())
      .expect("extractors should parse");
    let grep = SupportLang::TypeScript.grep("class Box { parse() {} }");

    let items = combined.extract(grep.root()).collect::<Vec<_>>();

    assert!(combined.member_extractors.is_empty());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].entry.name, "Box");
    assert!(items[0].entry.signature.is_empty());
    assert!(items[0].members.is_empty());
  }

  #[test]
  fn compile_options_filter_rules_and_runtime_flags() {
    let extractors = parse_outline_rules::<SupportLang>(
      r#"
id: ts-import
language: TypeScript
role: item
symbolType: module
rule:
  kind: import_statement
name: import
isImport: true
isExported: false
---
id: ts-function
language: TypeScript
role: item
symbolType: function
rule:
  pattern: function $NAME() { $$$BODY }
name: $NAME
isImport: false
"#,
    )
    .expect("extractors should deserialize");
    let options = OutlineExtractorOptions {
      imports: OutlineFlagFilter::Yes,
      ..Default::default()
    };
    let combined = CombinedExtractors::try_from_rules(extractors, options, &Default::default())
      .expect("extractors should parse");
    let grep = SupportLang::TypeScript.grep(
      r#"
import { readFile } from 'node:fs';
function local() {}
"#,
    );

    let items = combined.extract(grep.root()).collect::<Vec<_>>();

    assert_eq!(combined.item_extractors.len(), 1);
    assert_eq!(combined.item_extractors[0].common.rule.id, "ts-import");
    assert_eq!(items.len(), 1);
    assert!(items[0].is_import);
  }
}

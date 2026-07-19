//! AST reference extraction (§3.1): call sites and imports → resolvable references.
//!
//! Walks the tree-sitter parse for call and import nodes, extracts the referenced identifier
//! (AST-based, never substring matching), and attributes it to a definition scope: **calls** to
//! their innermost enclosing item/member, **imports** to the outermost scope (the file node), so
//! `importers_of` reports files. The per-language `RefSpec` extends to new languages.

use std::ops::Range;

use vorpal_core::Node;
use vorpal_core::tree_sitter::StrDoc;
use vorpal_kg::NodeId;
use vorpal_language::SupportLang;
use vorpal_resolve::{RefKind, Reference};

type SgNode<'t> = Node<'t, StrDoc<SupportLang>>;

/// Per-language node kinds + callee/import fields.
pub(crate) struct RefSpec {
  call_kinds: &'static [&'static str],
  callee_field: &'static str,
  import_kinds: &'static [&'static str],
  import_field: &'static str,
}

/// Reference-extraction spec for a language, if its call grammar is known.
pub(crate) fn ref_spec(lang: SupportLang) -> Option<RefSpec> {
  let spec = |call_kinds, callee_field, import_kinds, import_field| {
    Some(RefSpec {
      call_kinds,
      callee_field,
      import_kinds,
      import_field,
    })
  };
  match lang {
    SupportLang::Rust => spec(
      &["call_expression"],
      "function",
      &["use_declaration"],
      "argument",
    ),
    SupportLang::Python => spec(&["call"], "function", &[], ""),
    SupportLang::Go => spec(&["call_expression"], "function", &[], ""),
    SupportLang::JavaScript => spec(&["call_expression"], "function", &[], ""),
    SupportLang::TypeScript => spec(&["call_expression"], "function", &[], ""),
    SupportLang::C => spec(&["call_expression"], "function", &[], ""),
    SupportLang::Cpp => spec(&["call_expression"], "function", &[], ""),
    _ => None,
  }
}

/// Emit `calls` and `imports` references from the parse tree.
pub(crate) fn extract_references(
  root: SgNode<'_>,
  spec: &RefSpec,
  def_spans: &[(Range<usize>, NodeId)],
  path: &str,
  out: &mut Vec<Reference>,
) {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    let kind = node.kind();
    let is_call = spec.call_kinds.iter().any(|k| *k == kind.as_ref());
    let is_import = spec.import_kinds.iter().any(|k| *k == kind.as_ref());
    if is_call || is_import {
      let range = node.range();
      let (field, refkind, from) = if is_call {
        (
          spec.callee_field,
          RefKind::Call,
          enclosing(def_spans, range.start),
        )
      } else {
        (
          spec.import_field,
          RefKind::Import,
          outermost(def_spans, range.start),
        )
      };
      if let (Some(from), Some(target)) = (from, node.field(field)) {
        if let Some(name) = callee_name(&target) {
          out.push(
            Reference::new(from, path, name, refkind)
              .with_evidence(range.start as u32, range.end as u32),
          );
        }
      }
    }
    for child in node.children() {
      stack.push(child);
    }
  }
}

/// The rightmost identifier of a callee/import expression: handles `foo`, `x.foo`, `a::b::foo`.
fn callee_name(node: &SgNode<'_>) -> Option<String> {
  match node.kind().as_ref() {
    "identifier" | "field_identifier" | "type_identifier" | "property_identifier" => {
      Some(node.text().into_owned())
    }
    _ => {
      for field in ["field", "name", "attribute", "property"] {
        if let Some(child) = node.field(field) {
          return callee_name(&child);
        }
      }
      None
    }
  }
}

/// The innermost (smallest) definition span containing `offset` — for call attribution.
fn enclosing(def_spans: &[(Range<usize>, NodeId)], offset: usize) -> Option<NodeId> {
  def_spans
    .iter()
    .filter(|(range, _)| range.contains(&offset))
    .min_by_key(|(range, _)| range.end - range.start)
    .map(|(_, id)| *id)
}

/// The outermost (largest) definition span containing `offset` — the file, for import attribution.
fn outermost(def_spans: &[(Range<usize>, NodeId)], offset: usize) -> Option<NodeId> {
  def_spans
    .iter()
    .filter(|(range, _)| range.contains(&offset))
    .max_by_key(|(range, _)| range.end - range.start)
    .map(|(_, id)| *id)
}

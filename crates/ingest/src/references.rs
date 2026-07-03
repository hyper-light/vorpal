//! AST reference extraction (§3.1): call sites → resolvable references.
//!
//! Walks the tree-sitter parse for call nodes, extracts the callee identifier (AST-based, never
//! substring matching), and attributes each to its innermost enclosing definition span. The
//! per-language `RefSpec` (call node kinds + callee field) is a small table that extends to new
//! languages; only languages whose call grammar is known are enabled.

use std::ops::Range;

use vorpal_core::Node;
use vorpal_core::tree_sitter::StrDoc;
use vorpal_kg::NodeId;
use vorpal_language::SupportLang;
use vorpal_resolve::{RefKind, Reference};

type SgNode<'t> = Node<'t, StrDoc<SupportLang>>;

/// Per-language call node kinds and the field naming the callee.
pub(crate) struct RefSpec {
  call_kinds: &'static [&'static str],
  callee_field: &'static str,
}

/// Reference-extraction spec for a language, if its call grammar is known.
pub(crate) fn ref_spec(lang: SupportLang) -> Option<RefSpec> {
  let spec = |call_kinds, callee_field| {
    Some(RefSpec {
      call_kinds,
      callee_field,
    })
  };
  match lang {
    SupportLang::Rust => spec(&["call_expression"], "function"),
    SupportLang::Python => spec(&["call"], "function"),
    SupportLang::Go => spec(&["call_expression"], "function"),
    SupportLang::JavaScript => spec(&["call_expression"], "function"),
    SupportLang::TypeScript => spec(&["call_expression"], "function"),
    SupportLang::C => spec(&["call_expression"], "function"),
    SupportLang::Cpp => spec(&["call_expression"], "function"),
    _ => None,
  }
}

/// Emit a `calls` reference per call site, attributed to the innermost definition span that
/// contains it.
pub(crate) fn extract_references(
  root: SgNode<'_>,
  spec: &RefSpec,
  def_spans: &[(Range<usize>, NodeId)],
  path: &str,
  out: &mut Vec<Reference>,
) {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if spec.call_kinds.iter().any(|k| *k == node.kind().as_ref()) {
      if let Some(func) = node.field(spec.callee_field) {
        if let Some(name) = callee_name(&func) {
          let range = node.range();
          if let Some(from) = enclosing(def_spans, range.start) {
            out.push(
              Reference::new(from, path, name, RefKind::Call)
                .with_evidence(range.start as u32, range.end as u32),
            );
          }
        }
      }
    }
    for child in node.children() {
      stack.push(child);
    }
  }
}

/// The rightmost identifier of a callee expression: handles `foo`, `x.foo`, `a::b::foo`, `x.prop`.
fn callee_name(func: &SgNode<'_>) -> Option<String> {
  match func.kind().as_ref() {
    "identifier" | "field_identifier" | "type_identifier" | "property_identifier" => {
      Some(func.text().into_owned())
    }
    _ => {
      for field in ["field", "name", "attribute", "property"] {
        if let Some(child) = func.field(field) {
          return callee_name(&child);
        }
      }
      None
    }
  }
}

/// The innermost (smallest) definition span containing `offset`.
fn enclosing(def_spans: &[(Range<usize>, NodeId)], offset: usize) -> Option<NodeId> {
  def_spans
    .iter()
    .filter(|(range, _)| range.contains(&offset))
    .min_by_key(|(range, _)| range.end - range.start)
    .map(|(_, id)| *id)
}

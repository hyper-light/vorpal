//! Shared helpers for HTML-family injection hosts (Html, Vue, Svelte, Astro): the `lang`
//! attribute reader and the node→TSRange conversion, moved verbatim from `html.rs` when the
//! Wave-2 hosts landed.

use vorpal_core::tree_sitter::TSRange;
use vorpal_core::{Doc, Node, matcher::KindMatcher};

pub(crate) fn find_lang<D: Doc>(node: &Node<D>) -> Option<String> {
  let html = node.lang();
  let attr_matcher = KindMatcher::new("attribute", html.clone());
  let name_matcher = KindMatcher::new("attribute_name", html.clone());
  let val_matcher = KindMatcher::new("attribute_value", html.clone());
  node.find_all(attr_matcher).find_map(|attr| {
    let name = attr.find(&name_matcher)?;
    if name.text() != "lang" {
      return None;
    }
    let val = attr.find(&val_matcher)?;
    Some(val.text().to_string())
  })
}

pub(crate) fn node_to_range<D: Doc>(node: &Node<D>) -> TSRange {
  let r = node.range();
  let start = node.start_pos();
  let sp = start.byte_point();
  let sp = tree_sitter::Point::new(sp.0, sp.1);
  let end = node.end_pos();
  let ep = end.byte_point();
  let ep = tree_sitter::Point::new(ep.0, ep.1);
  TSRange {
    start_byte: r.start,
    end_byte: r.end,
    start_point: sp,
    end_point: ep,
  }
}

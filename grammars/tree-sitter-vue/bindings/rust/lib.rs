//! This crate provides Vue language support for the [tree-sitter][] parsing library.
//
// vorpal local patch: modernized from the 0.0.3 `fn language()` binding to the `LanguageFn`
// shape (tree-sitter-language 0.1) so one tree-sitter runtime serves every grammar. The
// compiled parser (src/) is untouched. Recorded in grammars/PROVENANCE.json.

use tree_sitter_language::LanguageFn;

extern "C" {
    fn tree_sitter_vue() -> *const ();
}

/// The tree-sitter [`LanguageFn`][LanguageFn] for this grammar.
///
/// [LanguageFn]: https://docs.rs/tree-sitter-language/*/tree_sitter_language/struct.LanguageFn.html
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_vue) };

/// The content of the [`node-types.json`][] file for this grammar.
///
/// [`node-types.json`]: https://tree-sitter.github.io/tree-sitter/using-parsers#static-node-types
pub const NODE_TYPES: &str = include_str!("../../src/node-types.json");

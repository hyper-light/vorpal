//! This crate provides dockerfile language support for the [tree-sitter][] parsing library.
//!
//! Typically, you will use the [LANGUAGE][] constant to add this language to a
//! tree-sitter [Parser][], and then use the parser to parse some code.
//!
//! [Parser]: https://docs.rs/tree-sitter/*/tree_sitter/struct.Parser.html
//! [tree-sitter]: https://tree-sitter.github.io/
//
// vorpal local patch: modernized from the 0.2.0 `fn language()` + tree-sitter 0.20 binding to
// the `LanguageFn` shape (tree-sitter-language 0.1) so one tree-sitter runtime serves every
// grammar. The compiled parser (src/) is untouched. Recorded in grammars/PROVENANCE.json.

use tree_sitter_language::LanguageFn;

extern "C" {
    fn tree_sitter_dockerfile() -> *const ();
}

/// The tree-sitter [`LanguageFn`][LanguageFn] for this grammar.
///
/// [LanguageFn]: https://docs.rs/tree-sitter-language/*/tree_sitter_language/struct.LanguageFn.html
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_dockerfile) };

/// The content of the [`node-types.json`][] file for this grammar.
///
/// [`node-types.json`]: https://tree-sitter.github.io/tree-sitter/using-parsers#static-node-types
pub const NODE_TYPES: &str = include_str!("../../src/node-types.json");

# tree-sitter-proto

[![CI][ci]](https://github.com/coder3101/tree-sitter-proto/actions/workflows/ci.yml)
[![crates][crates]](https://crates.io/crates/tree-sitter-proto)

Protocol buffer grammar for [tree-sitter](https://github.com/tree-sitter/tree-sitter).

## ✨ Features

- ✅ Proto2 support
- ✅ Proto3 support
- ✅ Support for editions
- ✅ Neovim queries (highlights, folds, indents, injections)

## 📦 Queries

The following queries are provided for editor integration:

| Query | Description |
|-------|-------------|
| `highlights.scm` | Syntax highlighting |
| `folds.scm` | Code folding |
| `indents.scm` | Automatic indentation |
| `injections.scm` | Language injections (comments) |

## Usage in Neovim

This parser can be used with [nvim-treesitter](https://github.com/nvim-treesitter/nvim-treesitter).

### Special Thanks

Special thanks to the following people for their amazing work on this grammar:

- [mitchellh](https://github.com/mitchellh/tree-sitter-proto) for their original work.
- [treywood](https://github.com/treywood/tree-sitter-proto) for basic proto2 support.

[ci]: https://img.shields.io/github/actions/workflow/status/coder3101/tree-sitter-proto/ci.yml?logo=github&label=CI
[crates]: https://img.shields.io/crates/v/tree-sitter-proto?logo=rust

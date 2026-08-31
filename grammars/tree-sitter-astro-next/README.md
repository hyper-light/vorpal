# tree-sitter-astro-next

An Astro grammar for [tree-sitter](https://tree-sitter.github.io/), compatible with tree-sitter 0.25 and later versions.

## Overview

This project provides a tree-sitter parser for the Astro framework, enabling syntax highlighting, code navigation, and other advanced editor features for Astro files. It includes support for Astro's unique syntax including frontmatter blocks, HTML templates, JSX-style expressions, and embedded JavaScript/TypeScript and CSS.

## Features

- Full Astro syntax support (frontmatter, HTML, expressions)
- Compatible with tree-sitter 0.25+
- Comprehensive query files for:
  - Syntax highlighting (`highlights.scm`)
  - Language injections for frontmatter, `<script>`, `<style>`, and expression blocks (`injections.scm`)
  - Local variable tracking (`locals.scm`)
- Rust bindings with C-based parser implementation
- Extensive test coverage

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
tree-sitter-astro-next = "0.1.0"
```

## Contributing

Contributions are welcome. Please ensure that:

1. All tests pass (`cargo test`)
2. Code follows Rust formatting standards (`cargo fmt`)
3. New features include appropriate tests

## License

Licensed under [Apache License, Version 2.0](https://github.com/PRRPCHT/tree-sitter-astro-next/blob/master/LICENSE-APACHE).

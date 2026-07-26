# tree-sitter-dart

A [tree-sitter](https://tree-sitter.github.io/tree-sitter/) grammar for
[Dart](https://dart.dev/), built from the official [Dart language
specification](https://github.com/dart-lang/language/blob/main/specification/dartLangSpec.tex).

Supports Dart through 3.11: records, patterns, class modifiers, extension
types, null-aware elements, dot shorthands, and digit separators.

## Correctness

99.98% on a 219,918-file corpus drawn from pub.dev (205,728 files) and
curated open-source Dart projects (14,190 files):

```
pub                    205728 files    205705 passed     23 failed  99.99%
curated                 14190 files     14169 passed     21 failed  99.85%
TOTAL                  219918 files    219874 passed     44 failed  99.98%
```

The 44 remaining failures are invalid Dart input (NUL bytes in strings,
hyphens in identifiers, imports after declarations, mustache-template
files in mason brick directories, fixtures with intentional parse errors).

## AST shape

Each common construct emits a single named subtree, designed for
structural-search and structural-diff tools. Notable wrappers:

- Postfix chains: `call_expression`, `member_expression`, `index_expression`,
  `null_aware_member_expression`, `null_aware_index_expression`,
  `null_assertion_expression`, `instantiation_expression`, plus 7 cascade
  parallels.
- Top-level forms: `function_declaration`, `getter_declaration`,
  `setter_declaration`, `external_*_declaration` variants,
  `top_level_variable_declaration`, `external_variable_declaration`.
- Class members: `method_declaration` for body-bearing methods.
- Types: every type-labeled position emits a single `type` wrapper child;
  composite types (generics, function types, record types, nullable forms)
  nest inside it.
- Hidden supertypes: `_instantiation` groups `new_expression`,
  `const_object_expression`, `constructor_invocation`.

## Setup

Prerequisites: Node.js 18+, a C compiler.

```sh
npm install
npm run build-test
```

`build-test` runs `tree-sitter generate`, `node-gyp build`, and
`tree-sitter test`.

To try the grammar in a browser:

```sh
npm start
```

## Queries

Files in `queries/`:

- `highlights.scm` - keywords, types, functions, literals, operators,
  property access, method/function calls, references.
- `tags.scm` - symbol tagging (classes, methods, constructors, top-level
  declarations, call references).
- `locals.scm` - scope and variable resolution.

## Architecture

### External scanner

`src/scanner.c` handles three things tree-sitter's generated lexer cannot:

1. **String interpolation characters.** Dart strings allow `$expr` and
   `${expr}`; the scanner emits `template_chars_*` tokens for literal text
   between interpolation boundaries across all string quote styles (single,
   double, triple-single, triple-double, raw).

2. **Nested block comments.** `/* ... */` can nest; the scanner tracks
   depth and separates `block_comment` from `documentation_block_comment`
   (`/** ... */`).

3. **Annotation argument disambiguation.** `@Foo(args)` (no space) is an
   annotation with arguments; `@override (String, int)` (with space) is an
   annotation followed by a record type. The scanner emits
   `annotation_open_paren` only when `(` immediately follows the annotation
   name, matching the spec's NO_SPACE rule.

The scanner is stateless (serialize/deserialize are no-ops).

## Development

After grammar changes:

```sh
npm run build-test
```

To auto-update expected parse trees:

```sh
npx tree-sitter test --update
```

To parse a file:

```sh
npx tree-sitter parse path/to/file.dart
```

### Reference material

The [dart-lang/language](https://github.com/dart-lang/language) repo is a submodule (not fetched by default):

```sh
git submodule update --init
```

Provides `language/specification/dartLangSpec.tex` (formal grammar),
`language/accepted/` (Dart 3.x feature specs), and `language/working/`
(in-progress specs).

### Corpus testing

The submodule includes a corpus runner that downloads pub.dev packages
plus curated open-source Dart projects, then parses each file and reports
pass/fail counts.

```sh
cd language/tools/corpus/scripts
dart pub get
dart run bin/download_packages.dart   # populate download/pub/
dart run bin/clone_curated.dart        # populate download/curated/
dart run bin/report.dart               # parse everything, print summary
dart run bin/report.dart --details     # also list failing file paths
```

`report.dart` clears the tree-sitter cache and points the parser-directories
config at this repo, so it always uses the local grammar build.

## License

[MIT](LICENSE)

<h1 align="center">vorpal</h1>
<p align="center"><em>Code analysis and search, swift and sharp.</em></p>

Vorpal is a code **ingest → index → search** engine in a single binary. It fuses a precise,
tree-sitter-powered structural search & rewrite engine (built on [ast-grep]) with a **code
knowledge graph**, **hybrid semantic search**, and a built-in **MCP server** so coding agents can
query your codebase the way you do.

Point it at a repository and ask real questions:

```console
$ vorpal index .
parsed 355 files (0 replayed from cache) → 9795 nodes; refs: 10619 resolved, 1508 ambiguous, 7748 external, 8826 masked
index: ./.vorpal/index

$ vorpal graph callers resolve_import_path
resolve [Method] ./crates/resolve/src/resolver.rs

$ vorpal graph implementors FileExtractor
OutlineExtractor [Struct] ./crates/ingest/src/outline_extractor.rs

$ vorpal search "stat manifest change detection"
0.0167  FileStat [Struct] ./crates/ingest/src/manifest.rs

$ vorpal mcp          # serve all of the above to agents over MCP (stdio)
```

*Every output above is a verbatim capture from running vorpal on its own repository.*

## Install

**From source** (Rust 1.85+, works on any platform):

```console
$ git clone https://github.com/hyper-light/vorpal && cd vorpal
$ cargo build --release -p vorpal
$ ./target/release/vorpal --help      # `vp` is the short alias
```

**Prebuilt binaries** are attached to each [release](https://github.com/hyper-light/vorpal/releases)
as `app-<platform>.zip`. Full install options (macOS/Linux/Windows, PATH setup) are in
**[docs/getting-started.md](docs/getting-started.md)**.

## Quickstart

Run everything from your project root and the defaults just work:

```console
$ cd my-project
$ vorpal index .                        # build ./.vorpal/index (incremental on re-runs)
$ vorpal search "parse http request"    # hybrid semantic search
$ vorpal graph callers handle_request   # who calls this?
```

→ **[Getting started](docs/getting-started.md)** walks through every command with examples.

## What it does

- **Structural search & rewrite** — match code by AST pattern, not regex:
  `vorpal run -p 'console.log($ARG)'`. Full YAML rule system, project scanning, rule testing, LSP,
  and interactive rewrite — the ast-grep engine.
- **A real code knowledge graph** — every definition is a node; `calls`, `imports`, `implements`,
  `of_type`, `references`, and containment are edges. All AST-based, never substring matching.
- **Honest resolution** — references resolve with scope precedence and confidence labels; what
  can't be resolved is *counted, never faked*. No phantom edges, ever.
- **Hybrid search** — one query fuses exact/token name matching, lexical-embedding similarity, and
  graph in-degree (reciprocal rank fusion), with per-channel provenance on every hit.
- **Incremental by construction** — per-file extraction is cached; re-indexing re-parses only what
  changed and always re-links the whole graph, so renames and deletions never leave stale nodes.
- **Agent-native** — `vorpal mcp` serves graph queries, hybrid search, structural search, and
  verbatim source fetch as MCP tools over stdio.
- **28 languages** — one pipeline, tree-sitter grammars compiled in. No plugins to install.

## Use it with Claude

vorpal is an MCP server: it gives an AI agent tools to navigate your codebase — find callers,
trace what's reachable, search semantically, run structural patterns, and pull exact source.

```console
$ claude mcp add vorpal -- vorpal mcp --index /abs/path/to/project/.vorpal/index
```

→ **[MCP setup guide](docs/mcp.md)** — config for Claude Desktop/Code and the full tool list.

## Language packages

The pattern engine and index API are available beyond the CLI:

```console
$ pip install vorpal-py                    # Python
$ npm install @hyper-light/vorpal-node     # Node.js (native)
$ npm install @hyper-light/vorpal-wasm     # browser / portable
```

→ **[Python quickstart](docs/python.md)** · **[TypeScript/JS quickstart](docs/typescript.md)**

## Documentation

| Doc | What's in it |
|---|---|
| [Getting started](docs/getting-started.md) | Install, first index, every CLI command with examples |
| [MCP setup](docs/mcp.md) | Wire vorpal into Claude / any MCP client; the tool reference |
| [Python](docs/python.md) · [TypeScript/JS](docs/typescript.md) | Library quickstarts (patterns + index API) |
| [Supported languages](docs/LANGUAGES.md) | The full matrix of what each of the 28 grammars extracts |
| [Architecture](docs/ARCHITECTURE.md) | Storage format, memory model, concurrency, scaling roadmap |
| [Benchmarks](docs/BENCHMARKS.md) | Reproducible perf: commands, datasets, hardware, results |
| [Index format](docs/INDEX_FORMAT.md) | On-disk compatibility & migration policy |

## Supported languages

All **28** grammars are compiled into the binary: Bash, C, C++, C#, CSS, Dart, Elixir, Go,
Haskell, HCL/Terraform, HTML, Java, JavaScript, JSON, Kotlin, Lua, Markdown, Nix, PHP, Python,
Ruby, Rust, Scala, Solidity, Swift, TSX, TypeScript, YAML. Every language gets definition
extraction; the relation edges each supports are in the
**[language matrix](docs/LANGUAGES.md)**. Anything not extracted is simply absent — never guessed.

## How it works

```
parse (tree-sitter, 28 grammars)
  → extract   definitions (YAML outline rules) + references (AST walk: calls/imports/types/impl)
  → intern    blake3 path-qualified identity → dense node ids (dedup, incremental skip)
  → store     columnar node segment (mmap, checksummed) + string heap + edge lists
  → resolve   scope-precedence, confidence-labeled; approximate edges labeled, never faked
  → link      resolved references become graph edges (CSR/CSC, both directions)
  → query     name / graph / transitive closure / hybrid RRF search
```

Guiding principles: prefilters may only skip work that provably can't match (correctness never
depends on them); incrementality caches *extraction*, not conclusions, so the graph is re-linked
from complete inputs every run; edges are created on grammar-proven evidence only; and builds are
deterministic — same input, bit-identical index. The full design, storage layout, and
billion-LOC scaling roadmap live in **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)**.

## Performance

On this repo (~351 files): cold index **0.05 s**, unchanged re-index **0.01 s**, graph/search
queries in milliseconds. At Linux-kernel scale (72,541 files, ~30M LOC → 2.74M nodes): cold index
**~7 s** at a sub-gigabyte footprint; a warm MCP tool call measures **2.8 µs**. Methodology and
raw numbers: **[docs/BENCHMARKS.md](docs/BENCHMARKS.md)**.

## Contributing / development

```console
$ cargo build -p vorpal            # the main binary
$ cargo test --workspace           # full suite
$ cargo clippy --workspace --all-targets -- -D warnings
```

The workspace layout, the extraction pipeline, and how to add or deepen a language are documented
in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Acknowledgements

Vorpal's structural search engine began as [ast-grep] by [Herrington Darkholme] and contributors —
an exceptional foundation, gratefully built upon. The knowledge-graph, semantic search, and MCP
layers are original to vorpal.

## License

MIT.

[ast-grep]: https://github.com/ast-grep/ast-grep
[Herrington Darkholme]: https://github.com/HerringtonDarkholme
[Model Context Protocol]: https://modelcontextprotocol.io

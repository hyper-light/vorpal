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

### Prebuilt binary (recommended)

Every [release](https://github.com/hyper-light/vorpal/releases) attaches **one binary per
platform** — download it, make it executable, done. No archive to unpack.

```sh
# macOS (Apple Silicon) — pick your asset from the table below
curl -L -o vorpal https://github.com/hyper-light/vorpal/releases/latest/download/vorpal-macos-arm64
chmod +x vorpal && sudo mv vorpal /usr/local/bin/
vorpal --help
```

| Platform | Asset |
|---|---|
| macOS Apple Silicon | `vorpal-macos-arm64` |
| macOS Intel | `vorpal-macos-x64` |
| Linux x64 (glibc) | `vorpal-linux-x64` |
| Linux ARM64 (glibc) | `vorpal-linux-arm64` |
| Linux x64 (static/musl) | `vorpal-linux-x64-musl` |
| Linux ARM64 (static/musl) | `vorpal-linux-arm64-musl` |
| Windows x64 | `vorpal-windows-x64.exe` |
| Windows ARM64 | `vorpal-windows-arm64.exe` |
| Windows x86 (32-bit) | `vorpal-windows-x86.exe` |

### npm (cross-platform, global CLI)

```sh
npm install -g @hyper-light/vorpal-cli   # installs the vorpal binary for your platform
```

### From source (any platform, Rust 1.85+)

```sh
git clone https://github.com/hyper-light/vorpal && cd vorpal
cargo build --release -p vorpal
sudo mv target/release/vorpal /usr/local/bin/   # or add to PATH
```

More detail (PATH setup, verifying, troubleshooting): **[docs/getting-started.md](docs/getting-started.md)**.

## Quickstart

Run everything from your project root and the defaults just work:

```console
$ cd my-project
$ vorpal index .                        # build ./.vorpal/index (incremental on re-runs)
$ vorpal search "parse http request"    # hybrid semantic search
$ vorpal graph callers handle_request   # who calls this?
```

> **One gotcha:** `vorpal index <dir>` writes to `<dir>/.vorpal/index`, but `search`/`graph` read
> `./.vorpal/index` (relative to you). They line up when you index `.` from your project root; if
> you index elsewhere, point queries at it with `--index <dir>/.vorpal/index`.

## Use it with an AI agent (MCP)

`vorpal mcp` is a [Model Context Protocol] server over stdio — it gives Claude, Codex, Cursor,
and other agents tools to navigate your codebase (callers, references, reachability, semantic +
structural search, verbatim source). It auto-builds and keeps the index fresh while it runs.

**Claude Code**
```sh
claude mcp add vorpal -- vorpal mcp --index /abs/path/to/project/.vorpal/index
```

**Claude Desktop** — add to your MCP config:
```json
{
  "mcpServers": {
    "vorpal": { "command": "vorpal", "args": ["mcp", "--index", "/abs/path/to/project/.vorpal/index"] }
  }
}
```

**Codex CLI** — add to `~/.codex/config.toml`:
```toml
[mcp_servers.vorpal]
command = "vorpal"
args = ["mcp", "--index", "/abs/path/to/project/.vorpal/index"]
```

**Cursor / other JSON clients** — use the same `mcpServers` block as Claude Desktop (Cursor reads
it from `.cursor/mcp.json`).

> Use an **absolute** index path — MCP clients launch the server without a working directory. If
> `vorpal` isn't on the client's `PATH`, use the binary's absolute path as `command`.

Tools exposed: `index`, `health`, `schema`, `coverage`, `code_search`, `architecture`,
`compare_generations`, `impact`, `dead_code`, `node`, `callers`, `references`, `importers`,
`implementors`, `type_users`, `reachable`, `structural_search`, `rule_search`, `ast_dump`,
`fetch_span`, `snippet`, `why`, `search`. Record-bearing tools page with cursors and accept
`format: "toon" | "lean" | "ids"` for token-lean output; `--profile scout|analysis|full`
serves a smaller surface to read-only agents. Full descriptions and setup notes:
**[docs/mcp.md](docs/mcp.md)**.

## Language packages

The pattern engine and index API are available beyond the CLI:

```sh
pip install vorpal-py                    # Python  → import vorpal_py
npm install @hyper-light/vorpal-node     # Node.js (native)
npm install @hyper-light/vorpal-wasm     # browser / portable
```

→ **[Python quickstart](docs/python.md)** · **[TypeScript/JS quickstart](docs/typescript.md)**

## CLI reference

| Command | What it does |
|---|---|
| `vorpal index [src] [--out DIR] [--verify]` | Build/refresh the knowledge-graph index |
| `vorpal search <query> [-k N] [--index DIR]` | Hybrid (name + semantic + graph) search |
| `vorpal graph <verb> [name] [--index DIR]` | `callers` `refs` `importers` `implementors` `typeusers` `node` `reachable` `snippet` `schema` `dead` `coverage` `impact` `diff` `architecture` |
| `vorpal run -p <pattern> [-l lang] [-r fix]` | One-off structural search/rewrite (default command) |
| `vorpal scan [-r rule.yml] [--format github]` | Run configured YAML rules across a project |
| `vorpal outline [paths] [--view signatures]` | File structure: symbols, members, imports/exports |
| `vorpal mcp [--index DIR]` | Serve the MCP server over stdio |
| `vorpal test` · `new` · `lsp` · `grammars` · `completions` | Rule testing, scaffolding, LSP, grammar list, shell completions |

Full walkthrough with examples: **[docs/getting-started.md](docs/getting-started.md)**.

## Performance

All numbers below are release builds on an Apple M5 Max (18 cores, 128 GB, macOS 26.4.1,
rustc 1.98.0), measured 2026-08-30, wall-clock for the whole CLI invocation including
process start. Datasets are pinned so you can re-run them: Linux kernel @ `1590cf032971`
(72,541 indexable files), CPython @ `b86a41cbf63` (3,592 files). Cold times are best of 3.

### Indexing

```
vorpal index <source-tree> --out <index-dir>
```

| Tree | Cold index | Edit one file, re-index | `touch` one file | Nothing changed |
|---|---|---|---|---|
| **Linux kernel** (72,541 files, ~30 M LOC → 2.75 M nodes, 6.8 M references) | **6.3 s** | **0.98 s** | 0.20 s | 0.10 s |
| **CPython** (3,592 files → 143k nodes) | 0.67 s | — | — | — |
| **This repository** (856 files → 44k nodes, incl. vendored tree-sitter runtime + grammars) | 3.8 s¹ | 0.04 s | — | 0.02 s |

¹ Dominated by a single 33 MB generated `parser.c`; the other 855 files parse in parallel
underneath it.

Peak memory for the kernel cold index stays under 1 GB. The index on disk for the kernel
is a 2.0 GB generation (that includes the 811 MB semantic-search index and a 581 MB cache
of parsed files that makes the 0.98 s re-index possible); the previous generation is kept
until the next commit, then swept.

### Search (Linux kernel index, k = 10)

```
vorpal search "socket buffer alloc" -k 10 --index <index-dir>
```

| State | Time | What runs |
|---|---|---|
| First searches, before the semantic index exists | 0.28 s | exact scan of every candidate |
| Semantic index built | **0.03 s** | accelerated lookup |

The semantic index builds once in the background (19 s for the kernel) and is validated
before every use; results are **identical** with or without it — it changes latency, never
answers. Building it is never on your critical path: searches work immediately after
indexing.

### Running as an MCP server

`vorpal mcp` watches your tree, keeps the index fresh as you edit, and answers over
stdio — you never re-index by hand. Round-trip times measured from the client side
(medians of 50 calls, kernel tree):

| Operation | Time |
|---|---|
| Graph query (callers, references, …) | **< 1 ms** |
| Hybrid search | **27 ms** |
| Save a file → queries reflect the change | ~0.5 s |
| Keeping the semantic index current after an edit | ~140 ms, in the background |
| Server start → fully warm on an existing index | 2–4 s |

Editing never triggers index rebuilds — changes are applied incrementally, including to
the semantic-search index.

### Structural scan vs. text grep (Linux kernel, 63,775 C files)

```
vorpal scan --rule rule.yml ~/linux     # kind: call_expression + regex: kmalloc
rg 'kmalloc\(' -t c ~/linux             # comparison
```

| Tool | Time | What you get |
|---|---|---|
| `vorpal scan` | 1.4 s | 42.6k structural matches — real `call_expression` nodes, not lines that happen to contain the text |
| `ripgrep` | 0.7 s | raw text lines |

Full parsing plus AST matching of 63,775 files costs about 2× a plain text grep of the
same tree.

### Determinism

Indexing the same tree always produces byte-identical output — two independent cold
builds of the kernel commit the exact same content-addressed generation. Every release
re-verifies this.

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
- **28 languages** — one pipeline, tree-sitter grammars compiled in. No plugins to install.

## Supported languages

All **28** grammars are compiled into the binary: Bash, C, C++, C#, CSS, Dart, Elixir, Go,
Haskell, HCL/Terraform, HTML, Java, JavaScript, JSON, Kotlin, Lua, Markdown, Nix, PHP, Python,
Ruby, Rust, Scala, Solidity, Swift, TSX, TypeScript, YAML. The relation edges each supports are in
the **[language matrix](docs/wip/LANGUAGES.md)**. Anything not extracted is simply absent — never guessed.

## Documentation

| Doc | What's in it |
|---|---|
| [Getting started](docs/getting-started.md) | Install, first index, every CLI command with examples |
| [MCP setup](docs/mcp.md) | Wire vorpal into Claude / Codex / any MCP client; the tool reference |
| [Python](docs/python.md) · [TypeScript/JS](docs/typescript.md) | Library quickstarts (patterns + index API) |
| [Supported languages](docs/wip/LANGUAGES.md) | The full matrix of what each of the 28 grammars extracts |
| [Architecture](docs/wip/ARCHITECTURE.md) | Storage format, memory model, concurrency, scaling roadmap |
| [Index format](docs/wip/INDEX_FORMAT.md) | On-disk compatibility & migration policy |

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

Prefilters may only skip work that provably can't match (correctness never depends on them);
incrementality caches *extraction*, not conclusions, so the graph re-links from complete inputs
every run; edges are created on grammar-proven evidence only; builds are deterministic. Full
design and the billion-LOC scaling roadmap: **[docs/wip/ARCHITECTURE.md](docs/wip/ARCHITECTURE.md)**.

## Contributing / development

```sh
cargo build -p vorpal            # the main binary
cargo test --workspace           # full suite
cargo clippy --workspace --all-targets -- -D warnings
```

Workspace layout, the extraction pipeline, and how to add or deepen a language are in
[docs/wip/ARCHITECTURE.md](docs/wip/ARCHITECTURE.md).

## Acknowledgements

Vorpal's structural search engine began as [ast-grep] by [Herrington Darkholme] and contributors —
an exceptional foundation, gratefully built upon. The knowledge-graph, semantic search, and MCP
layers are original to vorpal.

## License

MIT — © 2026 Ada Lundhe; portions © 2022 Herrington Darkholme (ast-grep). See [LICENSE](LICENSE).

[ast-grep]: https://github.com/ast-grep/ast-grep
[Herrington Darkholme]: https://github.com/HerringtonDarkholme
[Model Context Protocol]: https://modelcontextprotocol.io

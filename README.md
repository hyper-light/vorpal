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

Vorpal ships two interchangeable binaries: **`vorpal`** and the short alias **`vp`**.

### Prebuilt binary (recommended)

Every [release](https://github.com/hyper-light/vorpal/releases) attaches a zip per platform,
`app-<target>.zip`, containing both `vorpal` and `vp`.

```sh
# macOS (Apple Silicon) — swap the asset name for your platform (table below)
curl -L -o vorpal.zip https://github.com/hyper-light/vorpal/releases/latest/download/app-aarch64-apple-darwin.zip
unzip vorpal.zip && chmod +x vorpal vp && sudo mv vorpal vp /usr/local/bin/
vorpal --help
```

| Platform | Asset |
|---|---|
| macOS Apple Silicon | `app-aarch64-apple-darwin.zip` |
| macOS Intel | `app-x86_64-apple-darwin.zip` |
| Linux x86-64 (glibc) | `app-x86_64-unknown-linux-gnu.zip` |
| Linux ARM64 (glibc) | `app-aarch64-unknown-linux-gnu.zip` |
| Linux x86-64 (musl/static) | `app-x86_64-unknown-linux-musl.zip` |
| Linux ARM64 (musl/static) | `app-aarch64-unknown-linux-musl.zip` |
| Windows x64 / ARM64 | `app-x86_64-pc-windows-msvc.zip` · `app-aarch64-pc-windows-msvc.zip` |

### npm (cross-platform, global CLI)

```sh
npm install -g @hyper-light/vorpal-cli   # installs the vorpal + vp binaries for your platform
```

### From source (any platform, Rust 1.85+)

```sh
git clone https://github.com/hyper-light/vorpal && cd vorpal
cargo build --release -p vorpal
sudo mv target/release/vorpal target/release/vp /usr/local/bin/   # or add to PATH
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

Release builds, Apple Silicon; wall-clock for the whole CLI invocation including process start.

**This repository** (~351 files, ~9.6k nodes):

| Operation | Time |
|---|---|
| Full cold index | **0.05 s** |
| Re-index after touching one file | 0.03 s |
| Re-index, nothing changed | 0.01 s |
| Graph / search query (mmap cold-open) | milliseconds |
| `scan` regex rule over the **Linux kernel** (63,775 C files) | **1.0 s** — faster than ripgrep, with structural results |

**Linux kernel scale** (72,541 files, ~30M LOC → 2.74M nodes, 6.8M references; M-series, 18 cores):

| Operation | Time |
|---|---|
| Cold index | **~7 s** at a sub-gigabyte peak footprint |
| One-file incremental re-index | ~1.25 s |
| Unchanged re-index | ~0.10 s |
| Vector tier build (lazy, first search) | ~14 s, stamp-validated thereafter |
| Warm MCP tool call (parse + freshness + query + render) | **2.8 µs** |

Indexing is deterministic and bit-identical run to run. Full methodology — commands, datasets,
hardware, cold/warm states, raw numbers: **[docs/wip/BENCHMARKS.md](docs/wip/BENCHMARKS.md)**.

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
- **45 languages** — one pipeline, tree-sitter grammars compiled in. No plugins to install.

## Supported languages

All **45** grammars are compiled into the binary: Bash, C, C++, C#, CMake, CSS, Dart,
Dockerfile, Elixir, Erlang, Go, GraphQL, Haskell, HCL/Terraform, HTML, INI, Java, JavaScript,
JSON, Julia, Kotlin, Lua, Make, Markdown, Nix, Objective-C, OCaml, Perl, PHP, PowerShell,
Protobuf, Python, R, Ruby, Rust, Scala, Solidity, SQL, Swift, TOML, TSX, TypeScript, XML,
YAML, Zig. The relation edges each supports are in
the **[language matrix](docs/wip/LANGUAGES.md)**. Anything not extracted is simply absent — never guessed.

## Documentation

| Doc | What's in it |
|---|---|
| [Getting started](docs/getting-started.md) | Install, first index, every CLI command with examples |
| [MCP setup](docs/mcp.md) | Wire vorpal into Claude / Codex / any MCP client; the tool reference |
| [Python](docs/python.md) · [TypeScript/JS](docs/typescript.md) | Library quickstarts (patterns + index API) |
| [Supported languages](docs/wip/LANGUAGES.md) | The full matrix of what each of the 45 grammars extracts |
| [Architecture](docs/wip/ARCHITECTURE.md) | Storage format, memory model, concurrency, scaling roadmap |
| [Benchmarks](docs/wip/BENCHMARKS.md) | Reproducible perf: commands, datasets, hardware, results |
| [Index format](docs/wip/INDEX_FORMAT.md) | On-disk compatibility & migration policy |

## How it works

```
parse (tree-sitter, 45 grammars)
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

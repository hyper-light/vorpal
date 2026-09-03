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

### From source (any platform, Rust 1.98+)

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
`implementors`, `type_users`, `similar`, `reachable`, `data_flow`, `observed`, `query`, `structural_search`,
`rule_search`, `ast_dump`, `fetch_span`, `snippet`, `why`, `search`. Record-bearing tools page with cursors and accept
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
| `vorpal graph <verb> [name] [--index DIR]` | `callers` `refs` `importers` `implementors` `typeusers` `similar` `observed` `node` `reachable` `flows` `snippet` `schema` `dead` `coverage` `impact` `diff` `architecture` |
| `vorpal query '<cypher>' [--index DIR]` | Cypher-shaped read-only graph queries (`MATCH … WHERE … RETURN … LIMIT`) |
| `vorpal run -p <pattern> [-l lang] [-r fix]` | One-off structural search/rewrite (default command) |
| `vorpal scan [-r rule.yml] [--format github]` | Run configured YAML rules across a project |
| `vorpal outline [paths] [--view signatures]` | File structure: symbols, members, imports/exports |
| `vorpal enable semantic-f16\|semantic-f32` · `disable` · `tune --queries FILE` | Install/enable the encoder tier (274 MB / 547 MB), or measure every ranking tier on your own labelled queries and write the verdict |
| `vorpal mcp [--index DIR]` | Serve the MCP server over stdio |
| `vorpal test` · `new` · `lsp` · `grammars` · `completions` | Rule testing, scaffolding, LSP, grammar list, shell completions |

Full walkthrough with examples: **[docs/getting-started.md](docs/getting-started.md)**.

## Performance

All numbers are release builds of **v0.7.0** on an Apple M5 Max (18 cores, 128 GB,
macOS 26.4.1, rustc 1.98.0), measured 2026-09-02, wall-clock for the whole CLI
invocation including process start; cold times are best of runs on a quiet machine.
Every dataset is pinned by commit so you can re-run it. Indexing derives the full
relation set — calls, imports, types, data flow, near-clone pairs, request→route
links, co-change history — across **49 languages**, so every number buys the whole
graph, not a bare symbol table. Deeper methodology and history: `docs/wip/BENCHMARKS.md`.

### Indexing, at scale

```
vorpal index <source-tree> --out <index-dir>
```

The flagship tree, end to end:

| Linux kernel @ `1590cf032971` (75,954 tracked files, ~30 M LOC) | |
|---|---|
| Cold index → **8,890,840 nodes** | **8.2 s** |
| Edit one file, re-index | **0.5 s** |
| `touch` one file (content unchanged) | 0.5 s |
| Nothing changed | **0.13 s** |

### Indexing, across languages

Same command, fourteen well-known repos, shallow-cloned at the pinned commit —
cold build and the no-change re-run:

| Repo | Language | Files | Nodes | Cold | Unchanged |
|---|---|---:|---:|---:|---:|
| llvm/llvm-project `d37814473` | C++ | 183,249 | 1,443,608 | 8.1 s | 0.26 s |
| ziglang/zig `738d2be9` | Zig | 20,545 | 1,085,533 | 6.3 s | 0.03 s |
| JetBrains/kotlin `9f27f51dd` | Kotlin | 110,106 | 795,719 | 2.7 s | 0.40 s |
| kubernetes/kubernetes `bce953e8` | Go | 31,296 | 692,828 | 2.0 s | 0.08 s |
| dotnet/roslyn `4cac4334` | C# | 35,125 | 490,284 | 0.6 s | 0.09 s |
| rust-lang/rust `5db7f4be8` | Rust | 62,568 | 464,064 | 2.7 s | 0.07 s |
| WordPress/WordPress `c195362` | PHP | 5,010 | 286,824 | 1.8 s | 0.02 s |
| apache/spark `06539777` | Scala | 27,322 | 253,753 | 1.6 s | 0.05 s |
| apache/kafka `6e4c555` | Java | 7,537 | 209,131 | 0.7 s | 0.03 s |
| vercel/next.js `483f8420` | TS/JS | 31,852 | 204,754 | 1.0 s | 0.23 s |
| ghc/ghc `44d7788f` | Haskell | 26,918 | 178,259 | 0.7 s | 0.04 s |
| python/cpython `b86a41cbf63` | Python/C | 3,841 | 162,813 | 2.3 s | 0.02 s |
| rails/rails `4130768` | Ruby | 4,996 | 49,635 | 0.3 s | 0.02 s |
| neovim/neovim `d423675` | C/Lua | 3,918 | 40,507 | 0.3 s | 0.01 s |
| vuejs/core `d63616c` | Vue/TS | 705 | 11,191 | 0.1 s | 0.01 s |

This repository (2,815 files → 78,527 nodes, incl. the vendored tree-sitter runtime +
49 grammars): 7.4 s cold¹, 0.02 s unchanged. Kernel peak disk is a 7.6 GB generation (the
parsed-product cache inside it is what makes sub-second edits possible); the previous
generation is kept until the next commit, then swept.

¹ Dominated by a single 33 MB generated `parser.c`; everything else parses in parallel
underneath it.

### Editing large files (long-lived processes)

A process that indexes repeatedly — the MCP daemon, a watch loop, an SDK server calling
`indexBuild` on every save — treats an edited multi-megabyte source **incrementally**,
twice over: vorpal retains parse state for files over 1 MiB and applies tree-sitter's
own incremental reparse, and it also snapshots the extraction walk's rows so the next
save re-walks only the edited top-level region and splices the retained rows around it
(byte positions shifted, attribution remapped, near-clone sketches carried). Output is
verified byte-identical to a fresh whole-file extraction on every measurement round
below.

| Edited file (per save) | Fresh | Incremental parse | + walk splice |
|---|---:|---:|---:|
| 54 MB generated C (`tree-sitter-julia` parser), edit between definitions | 4.2 s | 1.9 s | **0.7 s** |
| 54 MB generated C, edit *inside* its single 43 MB parse-table definition | 4.2 s | 1.9 s | **1.7 s** |
| 17 MB generated C (`tree-sitter-cpp` parser) | 1.33 s | **0.58 s** | — |
| 1.4 MB hand-written C (CPython `Parser/parser.c`) | 104 ms | **34 ms** | — |

The parse share is eliminated entirely, and for edits between definitions the walk
share too — the splice machinery itself costs ~84 ms on 1.13 M retained rows (the
granularity floor is the enclosing definition: an edit inside one giant definition
re-walks that definition). Walk splicing ships for C first, gated hard: any splice
invariant violation falls back to the full walk, and one-shot CLI builds are unaffected
by design — retention only pays when a file is parsed again. `VORPAL_TREE_CACHE=0`
disables retention, `VORPAL_WALK_REUSE=0` just the splice; `_MIN`/`_BUDGET` tune the
1 MiB floor and the 256 MiB retained source+snapshot budget.

### Search quality: the three ranking tiers

```
vorpal search "socket buffer alloc" -k 10 --index <index-dir>
vorpal index <src> --semantic-tier learned      # tier 1: trained per corpus at warm time
vorpal enable semantic-f16                       # tier 2: 274 MB encoder (or semantic-f32, 547 MB)
vorpal tune --queries my-queries.txt             # measure the tiers on YOUR queries, write the verdict
```

Every index ships with the **lexical tier**: hashed name/signature/path embeddings fused
with exact name matching and graph in-degree — no weights, nothing to download. Two
opt-in tiers sit above it: a **learned static tier** trained from the corpus itself
during the warm (subword co-occurrence factorization, refined over the graph's own
edges; no download either), and a **neural encoder** (CodeRankEmbed, MIT) that reranks
the fused top-k at query time — `semantic-f32` uses the upstream 547 MB weights,
`semantic-f16` a locally converted 274 MB copy. **The two encoder sizes rank the
same** (embedding drift after conversion: cosine 1.000000; identical tables below on
CPython and this repo, one adjacent-rank swap on one kernel query) — they differ in
disk and resident memory, not in quality. With the encoder enabled, the warm also fills a
doc-side embedding sidecar in the background; that fill runs on the GPU when one is
present (Metal / Vulkan / DX12 through `wgpu` — Apple, NVIDIA, AMD, Intel, no driver
beyond the OS's), else on the platform's BLAS or the portable lanes, and the search path
never depends on which built it (`VORPAL_ENCODER_GPU=off` forces the CPU rungs).

Graded retrieval on three corpora with the bundled labelled query sets
(`xtask/labels/`, graded relevance 1–3; metrics NDCG@10 / MRR / recall@5, exact
determinism gate on every run; `cargo xtask searcheval`):

| Corpus (queries) | Lexical (default) | + learned tier | + encoder (f16 ≈ f32) |
|---|---:|---:|---:|
| Linux kernel, 8.9 M defs (8) | 0.299 / 0.375 / 0.229 | **0.313 / 0.346 / 0.250** | 0.222 / 0.294 / 0.208 |
| CPython, 163 K defs (6) | 0.137 / 0.208 / 0.250 | **0.412 / 0.528 / 0.333**¹ | **0.410 / 0.556 / 0.500** |
| This repo, 79 K defs (10) | 0.571 / 0.560 / 0.550 | 0.612 / 0.614 / 0.550 | **0.622 / 0.625 / 0.650** |

The tiers are **per-corpus decisions, not upgrades**: the learned tier triples
CPython's descriptive-query quality (NDCG 0.036 → 0.475 on "bytecode evaluation loop"-
style questions) and edges past lexical on the kernel and this repo — it trains on
every kind of definition balanced to the callable population, so the kernel's 5.9 M
macros no longer drown its 1 M functions; the
encoder lifts recall@5 to 0.50 on CPython and 0.65 on this repo but *lowers* the
kernel's short-keyword class, whose answers live in subword identifiers the encoder
re-orders. That is exactly what `vorpal tune` measures on your own queries before it
writes anything — it enables a tier only when the mean strictly improves and
wins ≥ losses, never on a tie.

¹ The learned warm also runs a per-corpus BM25 gate (paired probes, enabled only on
strong evidence); it enabled itself on CPython, not on the kernel or this repo.

### Search latency and memory, per tier

One-shot CLI (`vorpal search`, process start + index mmap, page cache warm): kernel
**0.19–0.20 s**, CPython 0.01 s, this repo < 0.01 s. The daemon holds the index warm;
measured over 30 stdio round-trips per tool from a client process, with the server's
resident memory sampled after every call:

| Index · tier | Search median | Search p95 | First search | Graph query | Peak RSS |
|---|---:|---:|---:|---:|---:|
| Kernel · lexical | 59 ms | 65 ms | 0.21 s | 0.11 ms | 2.1 GB |
| Kernel · learned | 61 ms | 63 ms | 0.24 s | 0.12 ms | 2.6 GB |
| Kernel · learned + f16 | 91 ms² | 476 ms | 0.67 s | 0.12 ms | 3.2 GB |
| Kernel · learned + f32 | 87 ms² | 474 ms | 0.58 s | 0.12 ms | 3.1 GB |
| CPython · lexical | 1.0 ms | 1.3 ms | 5 ms | 0.05 ms | 106 MB |
| CPython · learned | 2.2 ms | 2.6 ms | 15 ms | 0.07 ms | 149 MB |
| CPython · learned + f16 | 31 ms² | 285 ms | 0.44 s | 0.08 ms | 677 MB |
| CPython · learned + f32 | 30 ms² | 289 ms | 0.35 s | 0.08 ms | 591 MB |
| This repo · lexical | 0.7 ms | 0.8 ms | 3 ms | 0.05 ms | 63 MB |
| This repo · learned + f32 | 31 ms² | 289 ms | 0.39 s | 0.07 ms | 525 MB |

² Encoder medians are over a cycling query set, so most calls hit the 4,096-row
embedding cache; the p95 and first-search columns are the uncached cost (0.3–0.5 s per
new query at k = 10; 0.64–0.97 s mean at k = 25 in the graded runs). Graph queries never
touch the encoder. f16 halves the weights on disk but decodes into an f32 arena at open,
so its resident footprint is *not* smaller than f32's.

Index on disk (one committed generation, parsed-product cache included): kernel
**7.6 GB** lexical / 8.3 GB with the learned model; CPython 200 / 267 MB; this repo
836 / 867 MB. Indexer peak RSS on the kernel: 5.6 GB. Encoder weights: 547 MB (f32) or
274 MB (f16), stored once under `~/.vorpal/models`.

### Running as an MCP server

`vorpal mcp` watches your tree, keeps the index fresh as you edit, and answers over
stdio — you never re-index by hand. Round-trip times measured from the client side
(medians of 30 calls, kernel index):

| Operation | Time |
|---|---|
| Graph query (`callers`, `node`, …) | **< 1 ms** |
| Hybrid search (lexical tier; per-tier table above) | **59 ms** |
| Server start → answering queries on an existing index | immediate |
| First search after start (semantic tier warm-up, once) | 3.6 s |
| Save a file → index current again (incremental re-index) | ~0.5 s |

Editing never triggers full rebuilds — changes apply incrementally, including to the
semantic-search tier.

### Structural scan vs. text grep (Linux kernel, 63,775 C files)

```
vorpal scan --rule rule.yml ~/linux     # kind: call_expression + regex: kmalloc
rg 'kmalloc\(' -t c ~/linux             # comparison
```

| Tool | Time | What you get |
|---|---|---|
| `vorpal scan` | 4.8 s | 42.6k structural matches — real `call_expression` nodes, not lines that happen to contain the text |
| `ripgrep` | 1.0 s | raw text lines |

Full parsing plus AST matching of 63,775 files costs about 5× a plain text grep of the
same tree.

### Side by side: codebase-memory-mcp

The closest neighbour in this space is [codebase-memory-mcp] (cbm, v0.10.8-dev built
from source at `997d087`): also a single local binary with tree-sitter parsing, a typed
code graph, BM25, a Cypher subset, and an MCP server — so the same corpora, the same
labelled queries, and the same metrics run against both. Both tools indexed the same
checkouts on the same machine; cbm in its `full` mode (the only mode with semantic
edges), timed through its one-shot `cli` (its documented scriptable path), memory as
peak RSS over the whole process tree.

| | vorpal | codebase-memory-mcp |
|---|---|---|
| **Linux kernel** cold index (75,954 files) | **8.2 s** · 8.89 M nodes · peak RSS 5.6 GB · 7.6 GB on disk | 265 s · 8.53 M nodes / 16.0 M edges · peak RSS **70.3 GB** · 15.9 GB SQLite |
| Kernel, nothing changed | **0.12 s** | 12.5 s |
| **CPython** cold index (3,841 files) | **1.0–2.3 s** · 162,813 nodes · 0.8 GB RSS · 200 MB | 36.2 s · 136,118 nodes · 6.6 GB RSS · 663 MB |
| **This repo** cold index (49 vendored grammar giants) | **7.4 s** · 78,894 nodes · 12.2 GB RSS · 836 MB | 44.8 s · 66,141 nodes · 32.3 GB RSS · 291 MB |
| Search, kernel labels (NDCG@10 / MRR / recall@5) | **0.299 / 0.375 / 0.229** (lexical, no weights) | 0.116 / 0.104 / 0.167 (BM25) |
| Search, CPython labels | 0.137 / 0.208 / 0.250 lexical · **0.410 / 0.556 / 0.500** with the learned tier + encoder | 0.274 / 0.246 / 0.167 (BM25) |
| Search, this repo's labels | 0.571 / 0.560 / 0.550 lexical · **0.648 / 0.625 / 0.750** with the encoder | 0.479 / 0.500 / 0.450 (BM25) |
| cbm `semantic_query` (keyword-vector mode), all three corpora | — | 0.000 on every class |
| One search, one-shot CLI (kernel) | **0.2 s** (daemon: 59 ms) | 3.3–5.5 s |
| Callers of a symbol, one-shot CLI (kernel) | **0.06 s** (daemon: 0.1 ms) | 3.3–4.4 s |
| Ranking tiers | lexical · learned (trained per corpus) · neural encoder rerank (f16/f32), per-index `tune` | BM25 · regex · static per-token vectors (no inference) |
| Languages | 49 grammars | 162 grammars |
| Determinism | byte-identical generations, incremental ≡ scratch (release-gated) | not claimed |

Where cbm is ahead: grammar count (162 vs 49), and its BM25 beats our *lexical* tier
on CPython's descriptive queries (0.274 vs 0.137) — the gap our learned tier and
encoder close and then invert (0.410). Its "semantic" mode is a bag of static
per-token vectors, not a model: on the labelled sets it never surfaced a relevant
definition. Everything else — build time, memory, incremental cost, query latency,
ranking quality on the two large corpora — favours vorpal by one to two orders of
magnitude; cbm's memory in particular is a design choice (RAM-first indexing budgeted
at half the machine), not a bug.

[codebase-memory-mcp]: https://github.com/DeusData/codebase-memory-mcp

### Determinism

Indexing the same tree always produces byte-identical output — two independent cold
builds commit the exact same content-addressed generation, on every corpus above.
Incremental builds converge to the same bytes as from-scratch builds (a release-gated
battery proves scratch-determinism plus six edit shapes across three repos, and the
kernel's one-shot edit is verified to the same generation id). Every release
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
- **49 languages** — one pipeline, tree-sitter grammars compiled in. No plugins to install.

## Supported languages

All **49** grammars are compiled into the binary: Astro, Bash, C, C++, C#, CMake, CSS, Dart,
Dockerfile, Elixir, Erlang, Go, GraphQL, Haskell, HCL/Terraform, HTML, INI, Java, JavaScript,
JSDoc, JSON, Julia, Kotlin, Lua, Make, Markdown, Nix, Objective-C, OCaml, Perl, PHP,
PowerShell, Protobuf, Python, R, Ruby, Rust, Scala, Solidity, SQL, Svelte, Swift, TOML, TSX,
TypeScript, Vue, XML, YAML, Zig. Vue/Svelte/Astro single-file components extract their
script/style/frontmatter content through real embedded parses (C3a injections). The relation edges each supports are in
the **[language matrix](docs/LANGUAGES.md)**. Anything not extracted is simply absent — never guessed.

## Documentation

| Doc | What's in it |
|---|---|
| [Getting started](docs/getting-started.md) | Install, first index, every CLI command with examples |
| [MCP setup](docs/mcp.md) | Wire vorpal into Claude / Codex / any MCP client; the tool reference |
| [Python](docs/python.md) · [TypeScript/JS](docs/typescript.md) | Library quickstarts (patterns + index API) |
| [Supported languages](docs/LANGUAGES.md) | The full matrix of what each of the 49 grammars extracts |
| [Architecture](docs/wip/ARCHITECTURE.md) | Storage format, memory model, concurrency, scaling roadmap |
| [Index format](docs/INDEX_FORMAT.md) | On-disk compatibility & migration policy |

## How it works

```
parse (tree-sitter, 49 grammars)
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

<h1 align="center">vorpal</h1>
<p align="center"><em>Code analysis and search, swift and sharp.</em></p>

Vorpal indexes a codebase into a knowledge graph and answers questions about it: who calls
this, what implements that, where is the code that does X. It is one binary with 49
tree-sitter grammars compiled in, a structural search and rewrite engine built on [ast-grep],
hybrid semantic search, and an MCP server so coding agents can use all of it.

Point it at a repository:

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

*The output above is captured from running vorpal on its own repository.*

## Install

### Prebuilt binary (recommended)

Every [release](https://github.com/hyper-light/vorpal/releases) attaches one binary per
platform. Download it and make it executable; there is no archive to unpack.

```sh
# macOS (Apple Silicon); other platforms in the table below
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

Run from your project root and the defaults line up:

```console
$ cd my-project
$ vorpal index .                        # build ./.vorpal/index (incremental on re-runs)
$ vorpal search "parse http request"    # hybrid semantic search
$ vorpal graph callers handle_request   # who calls this?
```

> `vorpal index <dir>` writes to `<dir>/.vorpal/index`; `search` and `graph` read
> `./.vorpal/index` relative to your shell. Index `.` from the project root and they match.
> Otherwise pass `--index <dir>/.vorpal/index` to queries.

## Use it with an AI agent (MCP)

`vorpal mcp` is a [Model Context Protocol] server over stdio (revision 2026-07-28, with the
`initialize` handshake kept for older clients). It gives Claude, Codex, Cursor, and other
agents tools for callers, references, reachability, semantic and structural search, and
verbatim source. It builds the index if needed and keeps it current while it runs. What
it saves an agent against plain grep and read, in turns and tokens, is measured under
[How does it compare?](#how-does-it-compare).

The short route, from your project root, writes the config for every client it finds:

```sh
vorpal mcp install            # or --client claude-code|claude-desktop|codex|cursor|vscode|windsurf
```

By hand:

**Claude Code**
```sh
claude mcp add vorpal -- vorpal mcp --index /abs/path/to/project/.vorpal/index
```

**Claude Desktop**, in your MCP config:
```json
{
  "mcpServers": {
    "vorpal": { "command": "vorpal", "args": ["mcp", "--index", "/abs/path/to/project/.vorpal/index"] }
  }
}
```

**Codex CLI**, in `~/.codex/config.toml`:
```toml
[mcp_servers.vorpal]
command = "vorpal"
args = ["mcp", "--index", "/abs/path/to/project/.vorpal/index"]
```

**Cursor and other JSON clients** use the same `mcpServers` block as Claude Desktop (Cursor reads
it from `.cursor/mcp.json`).

> Use an absolute index path: MCP clients launch the server without a working directory. If
> `vorpal` is not on the client's `PATH`, use the binary's absolute path as `command`.
> Claude Code loads each MCP tool's schema in a turn of its own the first time a tool is
> used; the trade-offs of keeping them resident are in [docs/mcp.md](docs/mcp.md).

Tools exposed: `index`, `health`, `schema`, `coverage`, `code_search`, `architecture`,
`compare_generations`, `impact`, `dead_code`, `node`, `graph` (callers, callees, references,
importers, implementors, type_users, similar, observed), `reachable`, `data_flow`, `query`, `structural_search`,
`rule_search`, `ast_dump`, `fetch_span`, `snippet`, `why`, `search`. The whole listing is
under 12 KB on the wire (11.7 KB; a test gates it), because a client either loads each schema
in a model turn or carries the listing in every turn's input; the server's instructions
also carry the CLI one-liner for its index, so a client with a shell can answer a single
lookup in two turns with no schema load at all. Tools that return records
page with cursors and accept `format: "lean" | "toon" | "ids"`; `graph` callers and callees
rows carry the call-site line so "who calls X" and "what does X call" are one call each.
`--profile scout|analysis|full` limits the
tool set for read-only agents. Full descriptions and the wire contract:
**[docs/mcp.md](docs/mcp.md)**.

## Language packages

The pattern engine and index API are also available as libraries:

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
| `vorpal graph <verb> [name] [--index DIR]` | `callers` `callees` `refs` `importers` `implementors` `typeusers` `similar` `observed` `node` `reachable` `flows` `snippet` `schema` `dead` `coverage` `impact` `diff` `architecture` |
| `vorpal query '<cypher>' [--index DIR]` | Cypher-shaped read-only graph queries (`MATCH … WHERE … RETURN … LIMIT`) |
| `vorpal run -p <pattern> [-l lang] [-r fix]` | One-off structural search/rewrite (default command) |
| `vorpal scan [-r rule.yml] [--format github]` | Run configured YAML rules across a project |
| `vorpal outline [paths] [--view signatures]` | File structure: symbols, members, imports/exports |
| `vorpal enable semantic-f16\|semantic-f32` · `disable` · `tune --queries FILE` | Install the neural encoder (274 MB / 547 MB); or measure every ranking tier on your own labelled queries and enable what wins |
| `vorpal mcp [--index DIR]` | Serve the MCP server over stdio |
| `vorpal test` · `new` · `lsp` · `grammars` · `completions` | Rule testing, scaffolding, LSP, grammar list, shell completions |

Every command with examples: **[docs/getting-started.md](docs/getting-started.md)**.

## Performance

Numbers below are release builds of **v0.7.1** on an Apple M5 Max (18 cores, 128 GB,
macOS 26.4.1, rustc 1.98.0), measured 2026-09-03. Times are wall-clock for the whole CLI
invocation including process start; cold times are the best of three runs on a quiet
machine. Every dataset is pinned by commit. Indexing always builds the full graph
(calls, imports, types, data flow, near-clone pairs, request-to-route links, co-change
history), so each number covers the whole product, not a symbol table. Method and
history: `docs/wip/BENCHMARKS.md`.

### How long does indexing take?

```
vorpal index <source-tree> --out <index-dir>
```

| Linux kernel @ `1590cf032971` (75,954 files parsed of 94,843 tracked, ~30 M LOC) | |
|---|---|
| Cold index → **8,891,771 nodes** | **8.2 s** |
| Edit one file, re-index | **0.5 s** |
| `touch` one file (content unchanged) | 0.5 s |
| Nothing changed | **0.13 s** |

Fifteen other repositories, shallow-cloned at the pinned commit. "Files parsed" counts
files a grammar handled, not everything tracked; the kernel row uses the same rule.

| Repo | Language | Files parsed | Nodes | Cold | Unchanged |
|---|---|---:|---:|---:|---:|
| llvm/llvm-project `d37814473` | C++ | 86,124 | 1,444,028 | 8.4 s | 0.30 s |
| ziglang/zig `738d2be9` | Zig | 17,025 | 1,085,567 | 6.3 s | 0.03 s |
| JetBrains/kotlin `9f27f51dd` | Kotlin | 75,448 | 795,719 | 2.7 s | 0.40 s |
| kubernetes/kubernetes `bce953e8` | Go | 26,641 | 692,828 | 2.1 s | 0.08 s |
| dotnet/roslyn `4cac4334` | C# | 19,522 | 490,284 | 2.2 s | 0.07 s |
| rust-lang/rust `5db7f4be8` | Rust | 41,607 | 464,064 | 2.7 s | 0.08 s |
| WordPress/WordPress `c195362` | PHP | 4,195 | 286,824 | 1.9 s | 0.02 s |
| apache/spark `06539777` | Scala | 11,512 | 253,753 | 1.6 s | 0.05 s |
| apache/kafka `6e4c555` | Java | 7,246 | 209,131 | 0.7 s | 0.03 s |
| vercel/next.js `483f8420` | TS/JS | 27,216 | 204,754 | 1.0 s | 0.23 s |
| ghc/ghc `44d7788f` | Haskell | 15,837 | 178,259 | 0.7 s | 0.04 s |
| python/cpython `b86a41cbf63` | Python/C | 3,841 | 162,945 | 1.0 s | 0.01 s |
| rails/rails `4130768` | Ruby | 3,952 | 49,635 | 0.4 s | 0.02 s |
| neovim/neovim `d423675` | C/Lua | 1,476 | 40,507 | 0.3 s | 0.01 s |
| vuejs/core `d63616c` | Vue/TS | 626 | 11,191 | 0.1 s | 0.01 s |

This repository: 1,884 files parsed of 2,868 tracked → 79,567 nodes, 7.8 s cold¹, 0.01 s
unchanged. The vendored tree-sitter runtime and 49 grammars are included in that count.

Disk: the kernel index is a 7.6 GB generation, most of it a parsed-product cache that
makes the sub-second edits above possible. The previous generation is kept until the next
commit, then swept. Indexer peak RSS on the kernel: 5.6 GB.

¹ One 33 MB generated `parser.c` sets the floor; everything else parses in parallel
underneath it. To re-run a pinned row, fetch by the full SHA
(`git fetch --depth 1 origin <sha>`); GitHub refuses abbreviated ones.

### Does it stay current while I edit?

Yes. `vorpal mcp` watches the tree and re-indexes changed files as you save. Changes
apply incrementally, including to the semantic-search tier, so a save never triggers a
full rebuild. Round trips measured from the client side (medians of 30 calls, kernel
index):

| Operation | Time |
|---|---|
| Graph query (`graph`, `node`, …) | **< 1 ms** |
| Hybrid search (default tier; per-tier table below) | **59 ms** |
| Server start → answering queries on an existing index | immediate |
| First search after start (ranking tier warm-up, once) | 3.6 s |
| Save a file → index current again | ~0.5 s |

Repositories with multi-megabyte source files get one more optimization in a long-lived
process (the MCP daemon, a watch loop, an SDK server calling `indexBuild` per save):
files over 1 MiB keep their parse state, so a save re-parses only the changed region and
re-walks only the edited definition. The result is checked byte-for-byte against a full
re-extraction on every row below.

| Edited file (per save) | Fresh | Incremental parse | + walk splice |
|---|---:|---:|---:|
| 54 MB generated C (`tree-sitter-julia` parser), edit between definitions | 4.2 s | 1.9 s | **0.7 s** |
| 54 MB generated C, edit *inside* its single 43 MB parse-table definition | 4.2 s | 1.9 s | **1.7 s** |
| 17 MB generated C (`tree-sitter-cpp` parser) | 1.33 s | **0.58 s** | — |
| 1.4 MB hand-written C (CPython `Parser/parser.c`) | 104 ms | **34 ms** | — |

The granularity is the enclosing definition: an edit inside one giant definition
re-walks that definition. Walk splicing currently ships for C; if any splice check fails,
the file falls back to a full walk. One-shot CLI builds are unaffected because nothing
is retained unless a file is parsed again. `VORPAL_TREE_CACHE=0` disables retention,
`VORPAL_WALK_REUSE=0` disables only the splice; `_MIN` and `_BUDGET` set the 1 MiB floor
and the 256 MiB budget.

### Is search any good?

```
vorpal search "socket buffer alloc" -k 10 --index <index-dir>
echo "semanticTier: learned" >> vorpalconfig.yml # train a ranking model on this corpus at the next index
vorpal enable semantic-f16                       # install the 274 MB neural encoder (or semantic-f32, 547 MB)
vorpal tune --queries my-queries.txt             # measure every tier on your queries; enable what wins
```

Out of the box, search fuses exact and token name matching, hashed name/signature/path
embeddings, and graph in-degree. Nothing to download. Two optional tiers sit on top:

- **Learned tier.** A ranking model trained from your own corpus while the index warms.
  No download. It improves results on every corpus we measure, mostly as recall. Select
  it with `semanticTier: learned` in `vorpalconfig.yml` (or `vorpal-index index
  --semantic-tier learned`).
- **Neural encoder.** CodeRankEmbed (MIT) reranks the top candidates at query time. The
  f16 and f32 downloads rank identically (cosine 1.000000 after conversion) and differ
  only in disk and memory. It also embeds referenced definitions in the background, which
  lets it surface answers the name-based channels never find. That fill runs on the GPU
  when one is present (Metal, Vulkan, or DX12 through `wgpu`; Apple, NVIDIA, AMD, Intel),
  otherwise on the platform BLAS or portable CPU code. Results do not depend on which
  built the embeddings. `VORPAL_ENCODER_GPU=off` forces CPU.

Graded retrieval on three corpora with the bundled labelled query sets (`xtask/labels/`:
54 / 54 / 55 queries across six classes from exact name to paraphrase; every grade cites a
source line in the `.evidence.md` files; NDCG@10 / MRR / recall@5; `cargo xtask searcheval`):

| Corpus (queries) | Default | + learned tier | + encoder (f16 = f32) |
|---|---:|---:|---:|
| Linux kernel, 8.9 M defs (54) | 0.299 / 0.307 / 0.302 | **0.313 / 0.303 / 0.361** | 0.289 / 0.289 / 0.309 |
| CPython, 163 K defs (54) | 0.307 / 0.292 / 0.333 | 0.340 / 0.320 / 0.389¹ | **0.350 / 0.330 / 0.426** |
| This repo, 79 K defs (55) | 0.400 / 0.394 / 0.400 | 0.450 / 0.443 / 0.536 | **0.461 / 0.453 / 0.527** |

Which tier to run is a per-repository decision. The encoder helps on CPython and this
repo but lowers the kernel's short-keyword queries (0.276 → 0.216), because those answers
live in subword identifiers that the encoder re-orders. `vorpal tune` runs this
measurement on your own queries and enables a tier only when it strictly improves the
mean and wins at least as often as it loses.

Two classes stay weak on every tier. Descriptive queries on the kernel score 0.07
because the right definitions rarely enter the candidate set. Paraphrase queries score
0 everywhere until the encoder's background embedding has filled, since nothing else
reads doc comments. The tables above are measured before that fill, so they are a floor.
`--dense-budget-timeout 5m30s` caps one fill round; `<index>/dense.channel = off` opts
out.

¹ The learned tier also runs a per-corpus BM25 check and enables BM25 when paired probes
show a clear win. It enabled itself on CPython, not on the kernel or this repo.

### How fast are queries, and what do they cost in memory?

One-shot CLI (`vorpal search`, process start plus index mmap, page cache warm): kernel
**0.19–0.20 s**, CPython 0.01 s, this repo under 0.01 s. The daemon keeps the index warm.
Measured over 30 stdio round trips per tool from a client process, with the server's
resident memory sampled after every call:

| Index · tier | Search median | Search p95 | First search | Graph query | Peak RSS |
|---|---:|---:|---:|---:|---:|
| Kernel · default | 59 ms | 65 ms | 0.21 s | 0.11 ms | 2.1 GB |
| Kernel · learned | 61 ms | 63 ms | 0.24 s | 0.12 ms | 2.6 GB |
| Kernel · learned + f16 | 96 ms² | 485 ms | 0.69 s³ | 0.12 ms | 2.8 GB |
| Kernel · learned + f32 | 94 ms² | 356 ms | 0.62 s³ | 0.11 ms | 2.7 GB |
| CPython · default | 1.0 ms | 1.3 ms | 5 ms | 0.05 ms | 106 MB |
| CPython · learned | 2.2 ms | 2.6 ms | 15 ms | 0.07 ms | 149 MB |
| CPython · learned + f16 | 36 ms² | 263 ms | 0.38 s | 0.07 ms | 719 MB |
| CPython · learned + f32 | 35 ms² | 245 ms | 0.40 s | 0.08 ms | 677 MB |
| This repo · default | 0.7 ms | 0.8 ms | 3 ms | 0.05 ms | 63 MB |
| This repo · learned + f16 | 35 ms² | 276 ms | 0.42 s | 0.07 ms | 623 MB |
| This repo · learned + f32 | 34 ms² | 244 ms | 0.42 s | 0.07 ms | 603 MB |

² Encoder medians cycle through a small query set, so most calls hit the 4,096-entry
embedding cache. The p95 and first-search columns show the uncached cost: 0.3–0.5 s per
new query at k = 10. Graph queries never touch the encoder. f16 halves the download but
decodes to f32 in memory, so it is not smaller at run time. Encoder rows include the
background embedding fill as it ships by default: complete on CPython and this repo, at
the 10-minute cap on the kernel (40,704 of 717,369 referenced definitions).

³ Weights and index already in the page cache. The first process after a reboot pays a
one-time page-in, about 4.8 s on the kernel.

Index on disk (one committed generation, parsed-product cache included): kernel
**7.6 GB** default / 8.3 GB with the learned model; CPython 200 / 267 MB; this repo
836 / 867 MB. Encoder weights: 547 MB (f32) or 274 MB (f16), stored once under
`~/.vorpal/models`.

### How does it compare?

**Against text search.** Structural scan of the Linux kernel (63,775 C files):

```
vorpal scan --rule rule.yml ~/linux     # kind: call_expression + regex: kmalloc
rg 'kmalloc\(' -t c ~/linux             # comparison
```

| Tool | Time | What you get |
|---|---|---|
| `vorpal scan` | 4.8 s | 42.6k `call_expression` nodes that call `kmalloc` |
| `ripgrep` | 1.0 s | text lines containing `kmalloc(` |

Parsing and AST-matching 63,775 files costs about 5× a text grep of the same tree.

**Against an agent's built-in tools.** An agent already has grep and read, so we
measured vorpal against them, on this repo and on the Linux kernel, with the v0.8.2
binary on 2026-09-05. Each row asks one question of a warm vorpal daemon (one MCP call,
median of five after a first call) and of the ripgrep-plus-read pipeline behind Claude
Code's Grep and Read tools. Wall time is the tool's own work. The last column is what
the model then has to read.

| Question | vorpal | rg + read | Output the model reads |
|---|---:|---:|---|
| This repo: callers of `tool_result` | 0.09 ms | 14 ms | 2 records with call sites vs 3 text lines |
| This repo: callees of `tool_result` | 0.16 ms | no equivalent | 7 records with call sites |
| This repo: what `run_install` reaches | 0.04 ms, 1 call | 6 ms, `rg -A 75` | 4 records vs 76 lines (3 KB) |
| This repo: source of `render_toml` | 0.04 ms | 39 ms, 2 commands | verified body vs 58 lines |
| Kernel: callers of `schedule_timeout_interruptible` (page of 100) | 3.4 ms | 930 ms | 100 of 140 resolved records (25 KB) vs 164 lines (13 KB) |
| Kernel: callers of `vfs_read` | 0.10 ms | 675 ms | 3 records with call sites vs 13 lines |
| Kernel: callees of `vfs_read` | 0.07 ms | 685 ms, then the body | 4 records with call sites vs 41 lines |
| Kernel: find `schedule_timeout` | 0.06 ms | 686 ms | 1 record vs 1 line |
| Kernel: source of `vfs_read` | 0.05 ms | 688 ms, then a read | verified body vs 42 lines |
| Kernel: what `vfs_read` reaches, depth 2 | 0.07 ms | no equivalent | 3 records |

On a small repo both are far under a model turn; the difference is round trips. On the
kernel every grep rescans 75,954 files (0.7 to 0.9 s) while the graph answers in
microseconds to milliseconds, and grep's lines include definitions, comments, and
macros the model must sift, where the graph returns resolved call edges with their
grades. A name with several definitions comes back as the list of candidates rather
than a merged answer: `kmalloc` has six in the kernel tree, and its macro form resolves
no call edges at all, so an earlier version of this table that counted those six
candidates as callers was wrong. The first call in a fresh daemon pays a cold open plus the
tree revalidation sweep (114 ms on this repo, 278 ms on the kernel); a kernel tree that
changed since its generation pays a rebuild on that first call instead (9.4 s measured).

Claude Code feeds the model a tool's `structuredContent`, so `format` decides how much
the model reads. For the three callers of `vfs_read`, the structured result is 817 B by
default (lean, with call sites), 377 B with `format: ids`, and 1,320 B with `toon`; each
page puts its common directory in one `base` field. Before we measured this (v0.8.0),
it was 1,065 B in every format.

**What that costs end to end.** We tested Claude Code 2.1.261 using Opus at high
effort on the same four questions, three ways. The first run could only use Grep, Glob,
and Read. The second could only use vorpal's MCP tools as Claude Code ships them, with
schemas deferred, so the first use of each tool costs a `ToolSearch` turn. The third
could also run the vorpal CLI from the shell; the server's instructions include the
exact command for its index, and the shell tool is never deferred. For that run, allow
the executable by the path the server prints, for example `Bash(/path/to/vorpal:*)`.
Tokens count everything the model processed, cache reads included. Cost is what the API
billed. One run per cell, measured 2026-09-05 with v0.8.2.

| Question | Tools | Turns | Tokens | Cost | Wall |
|---|---|---:|---:|---:|---:|
| This repo: who calls `tool_result` | grep + read | 4 | 71 K | $0.284 | 8.4 s |
| | vorpal MCP tool | 3 | 62 K | $0.177 | 5.0 s |
| | vorpal CLI via shell | 2 | 43 K | $0.031 | 4.7 s |
| This repo: what `run_install` reaches | grep + read | 4 | 76 K | $0.214 | 12.0 s |
| | vorpal MCP tool | 3 | 63 K | $0.158 | 7.1 s |
| | vorpal CLI via shell | 2 | 43 K | $0.040 | 7.3 s |
| Kernel: who calls `vfs_read` | grep + read | 6 | 93 K | $0.291 | 15.4 s |
| | vorpal MCP tool | 3 | 51 K | $0.136 | 4.5 s |
| | vorpal CLI via shell | 2 | 36 K | $0.030 | 7.2 s |
| Kernel: what `vfs_read` calls | grep + read | 3 | 59 K | $0.106 | 6.7 s |
| | vorpal MCP tool | 3 | 52 K | $0.107 | 5.9 s |
| | vorpal CLI via shell | 2 | 36 K | $0.031 | 6.6 s |

Each turn on Opus carries about 20 K tokens of context and takes a few seconds before
any tool runs, so the turn count decides most of the cost. Through the shell every
question took two turns, the command and the reply: 36 K to 43 K tokens against grep's
59 K to 93 K, and less wall time on three of the four. Through the MCP tools every
question took three: the schema load, one `graph` call, and the reply. On the kernel
callees question that is one turn fewer than the previous measurement, where the answer
needed a `reachable` call and then a `snippet`. All twelve answers were correct. On the
kernel callees question grep's read also listed two inline helpers, `fsnotify_access`
and `add_rchar`, that the graph does not resolve as call edges. Earlier versions of this
table, back to the 27-tool surface that took 8 turns and 161 K tokens on the kernel
callers question, are in `docs/wip/BENCHMARKS.md`.

**Against the nearest tool.** [codebase-memory-mcp] (cbm, v0.10.8-dev built from source
at `997d087`) is also a single local binary with tree-sitter parsing, a typed code graph,
BM25, a Cypher subset, and an MCP server, so the same corpora, labelled queries, and
metrics run against both. Both indexed the same checkouts on the same machine. cbm ran
in its `full` mode (the only mode with semantic edges), timed through its scriptable
`cli`; memory is peak RSS over the whole process tree.

| | vorpal | codebase-memory-mcp |
|---|---|---|
| **Linux kernel** cold index (75,954 files) | **8.2 s** · 8.89 M nodes · peak RSS 5.6 GB · 7.6 GB on disk | 265 s · 8.53 M nodes / 16.0 M edges · peak RSS **70.3 GB** · 15.9 GB SQLite |
| Kernel, nothing changed | **0.12 s** | 12.5 s |
| **CPython** cold index (3,841 files) | **1.0 s** · 162,945 nodes · 0.8 GB RSS · 200 MB | 36.2 s · 136,118 nodes · 6.6 GB RSS · 663 MB |
| **This repo** cold index (49 vendored grammar giants) | **7.4 s** · 78,894 nodes · 12.2 GB RSS · 836 MB | 44.8 s · 66,141 nodes · 32.3 GB RSS · 291 MB |
| Search, kernel labels (NDCG@10 / MRR / recall@5) | **0.299 / 0.375 / 0.229** (default tier) | 0.116 / 0.104 / 0.167 (BM25) |
| Search, CPython labels | 0.137 / 0.208 / 0.250 default · **0.410 / 0.556 / 0.500** learned + encoder | 0.274 / 0.246 / 0.167 (BM25) |
| Search, this repo's labels | 0.571 / 0.560 / 0.550 default · **0.648 / 0.625 / 0.750** with the encoder | 0.479 / 0.500 / 0.450 (BM25) |
| cbm `semantic_query` (keyword-vector mode), all three corpora | — | 0.000 on every class |
| One search, one-shot CLI (kernel) | **0.2 s** (daemon: 59 ms) | 3.3–5.5 s |
| Callers of a symbol, one-shot CLI (kernel) | **0.06 s** (daemon: 0.1 ms) | 3.3–4.4 s |
| Ranking tiers | default · learned (trained per corpus) · neural encoder rerank (f16/f32), per-index `tune` | BM25 · regex · static per-token vectors |
| Languages | 49 grammars | 162 grammars |
| Determinism | byte-identical generations, incremental = scratch (release-gated) | not claimed |

cbm ships 162 grammars to vorpal's 49, and its BM25 ranking beats vorpal's default tier
on CPython's descriptive queries. With the learned tier or encoder enabled, vorpal ranks
higher on all three corpora. cbm's `semantic_query` did not return a relevant definition
for any labelled query. cbm's memory use reflects its RAM-first indexing design.

[codebase-memory-mcp]: https://github.com/DeusData/codebase-memory-mcp

### Is the output deterministic?

Yes. Indexing the same tree twice produces byte-identical output: two independent cold
builds commit the same content-addressed generation on every corpus above. Incremental
builds converge to the same bytes as from-scratch builds; a release-gated battery checks
scratch determinism plus six edit shapes across three repositories, and the kernel's
one-shot edit is verified to the same generation id.

## What it does

- **Structural search and rewrite.** Match code by AST pattern instead of regex:
  `vorpal run -p 'console.log($ARG)'`. YAML rules, project scanning, rule testing, LSP,
  and interactive rewrite come from the ast-grep engine.
- **A code knowledge graph.** Every definition is a node; `calls`, `imports`,
  `implements`, `of_type`, `references`, and containment are edges, all derived from the
  AST rather than substring matching.
- **Resolution you can audit.** References resolve with scope precedence and a
  confidence label. Anything that cannot be resolved is counted and reported, not guessed.
- **Hybrid search.** One query fuses exact and token name matching, embedding
  similarity, and graph in-degree (reciprocal rank fusion), with per-channel provenance
  on every hit.
- **Incremental by construction.** Per-file extraction is cached; a re-index re-parses
  only what changed and always re-links the whole graph, so renames and deletions never
  leave stale nodes.
- **49 languages** in one binary. No plugins to install.

## Supported languages

All **49** grammars are compiled into the binary: Astro, Bash, C, C++, C#, CMake, CSS, Dart,
Dockerfile, Elixir, Erlang, Go, GraphQL, Haskell, HCL/Terraform, HTML, INI, Java, JavaScript,
JSDoc, JSON, Julia, Kotlin, Lua, Make, Markdown, Nix, Objective-C, OCaml, Perl, PHP,
PowerShell, Protobuf, Python, R, Ruby, Rust, Scala, Solidity, SQL, Svelte, Swift, TOML, TSX,
TypeScript, Vue, XML, YAML, Zig. Vue, Svelte, and Astro single-file components are parsed with the embedded
script, style, and frontmatter grammars. The relations each language supports are in the
**[language matrix](docs/LANGUAGES.md)**. Anything not extracted is absent, not guessed.

## Documentation

| Doc | What's in it |
|---|---|
| [Getting started](docs/getting-started.md) | Install, first index, every CLI command with examples |
| [MCP setup](docs/mcp.md) | Connect vorpal to Claude, Codex, or any MCP client; the tool reference |
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

Four rules hold throughout. Prefilters may only skip work that provably cannot match.
Incrementality caches extraction, not conclusions, so the graph re-links from complete
inputs every run. Edges are created only on grammar-proven evidence. Builds are
deterministic. Design and the scaling roadmap: **[docs/wip/ARCHITECTURE.md](docs/wip/ARCHITECTURE.md)**.

## Contributing / development

```sh
cargo build -p vorpal            # the main binary
cargo test --workspace           # full suite
cargo clippy --workspace --all-targets -- -D warnings
```

Workspace layout, the extraction pipeline, and how to add a language are in
[docs/wip/ARCHITECTURE.md](docs/wip/ARCHITECTURE.md).

## Acknowledgements

Vorpal's structural search engine began as [ast-grep] by [Herrington Darkholme] and
contributors. The knowledge graph, semantic search, and MCP layers are original to vorpal.

## License

MIT — © 2026 Ada Lundhe; portions © 2022 Herrington Darkholme (ast-grep). See [LICENSE](LICENSE).

[ast-grep]: https://github.com/ast-grep/ast-grep
[Herrington Darkholme]: https://github.com/HerringtonDarkholme
[Model Context Protocol]: https://modelcontextprotocol.io

<p align="center">
  <a href="docs/assets/brand/vorpal-blade-transparent.png">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="docs/assets/brand/vorpal-blade-dark.svg">
      <source media="(prefers-color-scheme: light)" srcset="docs/assets/brand/vorpal-blade-light.svg">
      <img src="docs/assets/brand/vorpal-blade-light.svg" alt="Vorpal blade logo" width="54" height="90">
    </picture>
  </a>
</p>

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
`rule_search`, `ast_dump`, `fetch_span`, `snippet`, `why`, `search`, `text_search`. The whole listing is
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

Numbers below are release builds of **v0.9.0** on an Apple M5 Max (18 cores, 128 GB,
macOS 26.4.1, rustc 1.98.0). The kernel, CPython, and this-repo index rows, the structural
search table, and both tool comparisons were measured 2026-09-07; the other fifteen index
rows and the tier and agent tables on 2026-09-05 and 06 with v0.8.4. Every dataset is
pinned by commit. One cold-index number per corpus appears throughout: the one in the
indexing table.

Times are wall-clock for the whole CLI invocation, process start included. Cold times are
the best of three runs. Every run waited for a quiet machine: two consecutive one-second
`top` samples at least 88 % idle, with nothing above half a core except `WindowServer`
and `fseventsd`. `fseventsd` runs at a full core while an index build streams file
events, so its load is recorded beside each result.

Indexing always builds the full graph: calls, imports, types, data flow, near-clone
pairs, request-to-route links, co-change history. Each number covers the whole product,
not a symbol table. Method and history: `docs/wip/BENCHMARKS.md`.

### How long does indexing take?

```
vorpal index <source-tree> --out <index-dir>
```

| Linux kernel @ `1590cf032971` (75,954 files parsed of 94,843 tracked, ~30 M LOC) | |
|---|---|
| Cold index → **8,891,771 nodes** | **8.1 s** |
| Edit a function body, re-index | **0.4 s** |
| Edit a comment only | 0.2 s |
| Add a function | 0.9 s |
| `touch` one file (content unchanged) | 0.2 s |
| Nothing changed | **0.13 s** |

The edit rows are medians of three saves to `fs/read_write.c`. A body edit or a comment
change replays only that file against the carried graph. Adding a definition takes the
defs-changed compose: the changed file is re-resolved against the carried include graph
and the definitions it affects are patched in place, and the result is checked to equal a
from-scratch build of the same tree. Same-day control on the cold row, interleaved
under the same gate: the installed 0.8.0 binary 8.6 s. A run earlier the same day read
10.6 s for both while a git process held 40 % of a core; the slow day is the machine, not
the code, and both runs are in `docs/wip/BENCHMARKS.md`.

Fifteen other repositories, shallow-cloned at the pinned commit. "Files parsed" counts
files a grammar handled, not everything tracked; the kernel row uses the same rule.

| Repo | Language | Files parsed | Nodes | Cold | Unchanged |
|---|---|---:|---:|---:|---:|
| llvm/llvm-project `d37814473` | C++ | 86,124 | 1,444,028 | 7.3 s | 0.34 s |
| ziglang/zig `738d2be9` | Zig | 17,025 | 1,085,567 | 5.6 s | 0.04 s |
| JetBrains/kotlin `9f27f51dd` | Kotlin | 75,448 | 795,719 | 2.5 s | 0.43 s |
| kubernetes/kubernetes `bce953e8` | Go | 26,641 | 692,828 | 1.9 s | 0.09 s |
| dotnet/roslyn `4cac4334` | C# | 19,522 | 490,284 | 1.9 s | 0.08 s |
| rust-lang/rust `5db7f4be8` | Rust | 41,607 | 464,064 | 2.5 s | 0.09 s |
| WordPress/WordPress `c195362` | PHP | 4,195 | 286,824 | 1.7 s | 0.02 s |
| apache/spark `06539777` | Scala | 11,512 | 253,753 | 1.5 s | 0.06 s |
| apache/kafka `6e4c555` | Java | 7,246 | 209,131 | 0.7 s | 0.04 s |
| vercel/next.js `483f8420` | TS/JS | 27,216 | 204,754 | 0.9 s | 0.25 s |
| ghc/ghc `44d7788f` | Haskell | 15,837 | 178,259 | 0.6 s | 0.05 s |
| python/cpython `b86a41cbf63` | Python/C | 3,841 | 162,945 | 0.9 s | 0.02 s |
| rails/rails `4130768` | Ruby | 3,952 | 49,635 | 0.3 s | 0.03 s |
| neovim/neovim `d423675` | C/Lua | 1,476 | 40,507 | 0.2 s | 0.01 s |
| vuejs/core `d63616c` | Vue/TS | 626 | 11,191 | 0.1 s | 0.01 s |

This repository: 1,917 files parsed of 2,889 tracked → 80,611 nodes, 6.9 s cold¹, 0.02 s
unchanged. The vendored tree-sitter runtime and 49 grammars are included in that count.

Disk: the kernel index is a 4.8 GB generation of 236 files, most of it a parsed-product
cache that makes the sub-second edits above possible; the search tiers a daemon warms on
top of it add about 3.3 GB (table below). The previous generation is kept until the next
commit, then swept. Indexer peak RSS on the kernel: 6.1 GB; a batch build keeps freed
pages until it exits, which costs about 0.3 GB of that and saves 1.7 M page faults.

¹ One 33 MB generated `parser.c` sets the floor; everything else parses in parallel
underneath it. To re-run a pinned row, fetch by the full SHA
(`git fetch --depth 1 origin <sha>`); GitHub refuses abbreviated ones.

### Does it stay current while I edit?

Yes. `vorpal mcp` watches the tree and re-indexes changed files as you save. Changes
apply incrementally, including to the semantic-search tier, so a save never re-parses the
tree. Round trips measured from the client side (medians of 30 calls, kernel index); the
save rows are medians of seven saves to `fs/read_write.c` on a scratch copy of the kernel,
polled every 20 ms until the daemon's answer showed the edit (range 1.9–4.9 s; each save
commits a new generation):

| Operation | Time |
|---|---|
| Graph query (`graph`, `node`, …) | **0.13 ms** |
| Hybrid search (default tier; per-tier table below) | **0.8 ms** |
| Server start → answering queries on an existing index | immediate |
| First search after start (ranking tier warm-up, once) | 0.19 s |
| Save a file → answers include the change | **2.0 s** (a body edit) · 2.2 s (a new function) |

Repositories with multi-megabyte source files get one more optimization in a long-lived
process (the MCP daemon, a watch loop, an SDK server calling `indexBuild` per save):
files over 1 MiB keep their parse state, so a save re-parses only the changed region and
re-walks only the edited definition. The result is checked byte-for-byte against a full
re-extraction on every row below.

| Edited file (per save) | Fresh | Incremental parse | + walk splice |
|---|---:|---:|---:|
| 54 MB generated C (`tree-sitter-julia` parser), edit between definitions | 4.2 s | 1.9 s | **0.7 s** |
| 54 MB generated C, edit *inside* its single 43 MB parse-table definition | 4.2 s | 1.9 s | **1.7 s** |
| 17 MB generated C (`tree-sitter-cpp` parser), edit near the top | 1.35 s | 0.60 s | **0.21 s** |
| 17 MB generated C, edit in the middle | 1.35 s | 0.60 s | **0.47 s** |
| 1.4 MB hand-written C (CPython `Parser/parser.c`) | 107 ms | 37 ms | **17 ms** |

The granularity is the enclosing definition: an edit inside one giant definition
re-walks that definition. Walk splicing currently ships for C; if any splice check fails,
the file falls back to a full walk. One-shot CLI builds are unaffected because nothing
is retained unless a file is parsed again. `VORPAL_TREE_CACHE=0` disables retention,
`VORPAL_WALK_REUSE=0` disables only the splice; `_MIN` and `_BUDGET` set the 1 MiB floor
and the 256 MiB budget.

### How fast is structural search?

`code_search` and `structural_search` run an ast-grep pattern over the indexed tree. The
daemon keeps a trigram index of the source, so a pattern only visits files that contain
its literals. Inside a file it parses only the top-level statements that contain the
literal; the index records where each statement starts. A statement tree-sitter had to
recover from is parsed with its whole file, so the result is the same as parsing every
file whole. A test compares the two on fixtures, and every kernel row below was checked
the same way. `text_search` is grep over the same index; each line comes back with the
symbol it sits in.

Linux kernel, one daemon, median of 3 calls:

| Query | Before the text index | Now |
|---|---:|---:|
| `code_search kmalloc($A, $B)`: 2,715 calls, each with its function | 4.3 s | **35 ms** |
| `code_search kfree($A)`: 40,499 calls | — | **90 ms** |
| `structural_search kmalloc($A, $B)` | 4.1 s, stopped at 100 | **51 ms**, all 2,715 |
| `code_search $R = schedule_timeout($A)` | 4.4 s | **45 ms** |
| `code_search os.path.join($A, $B)` in Python, second call | — | **2 ms** |
| `code_search if ($C) return $X;`: 381,811 matches, first call then a repeat | — | 3.7 s, then **0.4 s** |
| `text_search`, tgrep's 102-query suite, per query | — | **12.8 ms** (tgrep 21 ms, ripgrep about 1 s) |

Call patterns whose arguments are all metavariables, such as `f($A, $B)`, `f()`, or
`f($$$)`, need no parse at all. The index records every call with its argument count, so
the answer comes from the graph's own call references. Calls that tree-sitter had to
recover inside still go through the parser. A pattern run twice replays the unchanged
parts from a memo, so after an edit only the changed files are parsed again. The full
tables and the rules behind them are in `docs/wip/BENCHMARKS.md`.

The graph and the text index also answer two questions neither can alone. `graph` with
`mentions: true` lists every place a name appears in text outside the files the graph
already answered with, so an empty list before a rename means nothing was missed.
`text_search` with `symbol` searches one definition's source and nothing else.

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
  f16 and f32 downloads produce the same embeddings to within rounding (cosine 1.000000
  after conversion); they rank the same on CPython and this repo and differ by 0.001
  NDCG@10 on the kernel, and otherwise differ only in disk. It also embeds referenced
  definitions in the background, which lets it surface answers the name-based channels
  never find. That fill runs on the GPU
  when one is present (Metal, Vulkan, or DX12 through `wgpu`; Apple, NVIDIA, AMD, Intel),
  otherwise on the platform BLAS or portable CPU code. Results do not depend on which
  built the embeddings. `VORPAL_ENCODER_GPU=off` forces CPU.

Graded retrieval on three corpora with the bundled labelled query sets (`xtask/labels/`:
54 / 54 / 55 queries across six classes from exact name to paraphrase; every grade cites a
source line in the `.evidence.md` files; NDCG@10 / MRR / recall@5; `cargo xtask searcheval`):

| Corpus (queries) | Default | + learned tier | + encoder (f32; f16 in a footnote) |
|---|---:|---:|---:|
| Linux kernel, 8.9 M defs (54) | **0.329 / 0.327 / 0.358** | 0.315 / 0.304 / 0.361 | 0.295 / 0.290 / 0.302² |
| CPython, 163 K defs (54) | 0.306 / 0.291 / 0.333 | 0.341 / 0.322 / 0.389¹ | **0.351 / 0.331 / 0.426** |
| This repo, 79 K defs (55) | 0.402 / 0.395 / 0.445 | 0.430 / 0.427 / 0.455 | **0.455 / 0.448 / 0.500** |

The default column was re-measured 2026-09-07: it now includes the body list, which
nominates definitions whose source holds every query word when no definition name does.
That moved the kernel and this-repo rows; CPython did not change. The learned and encoder
columns are from 2026-09-06.

Which tier to run is a per-repository decision. The encoder helps on CPython and this
repo but lowers the kernel's aggregate, mostly on the subset-of-a-name queries (0.540 →
0.438), because those answers live in subword identifiers that the encoder re-orders.
`vorpal tune` runs this measurement on your own queries and enables a tier only when it strictly improves the
mean and wins at least as often as it loses.

Two classes stay weak on every tier. Descriptive queries on the kernel score 0.07
because the right definitions rarely enter the candidate set. Paraphrase queries score
0 everywhere until the encoder's background embedding has filled, since nothing else
reads doc comments. The tables above are measured before that fill, so they are a floor.
`--dense-budget-timeout 5m30s` caps one fill round; `<index>/dense.channel = off` opts
out.

¹ The learned tier also runs a per-corpus BM25 check and enables BM25 when paired probes
show a clear win. It enabled itself on CPython (38 wins to 17 losses over 512
paired probes) and on this repo (34 to 16), not on the kernel (17 to 10, under the gate's
margin).

² f16 on the kernel: 0.296 / 0.290 / 0.302.

### How fast are queries, and what do they cost in memory?

One-shot CLI (`vorpal search`, process start plus index mmap, page cache warm): kernel
**0.20 s**, CPython 0.01 s, this repo under 0.01 s. The daemon keeps the index warm.
Measured over 30 stdio round trips per tool from a client process, with the server's
resident memory sampled after every call:

| Index · tier | Search median | Search p95 | First search | Graph query | Peak RSS |
|---|---:|---:|---:|---:|---:|
| Kernel · default | 0.8 ms | 2.1 ms | 0.19 s | 0.13 ms | 2.1 GB |
| Kernel · learned | 2.1 ms | 2.7 ms | 0.22 s | 0.18 ms | 2.4 GB |
| Kernel · learned + f16 | 36 ms³ | 319 ms | 0.69 s⁴ | 0.13 ms | 3.0 GB |
| Kernel · learned + f32 | 36 ms³ | 324 ms | 0.59 s⁴ | 0.13 ms | 2.9 GB |
| CPython · default | 0.3 ms | 0.7 ms | 5 ms | 0.13 ms | 110 MB |
| CPython · learned | 1.4 ms | 1.7 ms | 11 ms | 0.14 ms | 154 MB |
| CPython · learned + f16 | 36 ms³ | 253 ms | 0.37 s | 0.15 ms | 748 MB |
| CPython · learned + f32 | 35 ms³ | 256 ms | 0.29 s | 0.15 ms | 658 MB |
| This repo · default | 0.3 ms | 0.5 ms | 3 ms | 0.10 ms | 65 MB |
| This repo · learned | 1.3 ms | 1.4 ms | 6 ms | 0.12 ms | 79 MB |
| This repo · learned + f16 | 35 ms³ | 218 ms | 0.42 s | 0.13 ms | 652 MB |
| This repo · learned + f32 | 35 ms³ | 226 ms | 0.33 s | 0.13 ms | 561 MB |

³ Encoder medians cycle through a small query set, so most calls hit the 4,096-entry
embedding cache. The p95 and first-search columns show the uncached cost: 0.2–0.3 s per
new query at k = 10. Graph queries never touch the encoder. f16 halves the download but
decodes to f32 in memory, so it is not smaller at run time. Encoder rows include the
background embedding fill as it ships by default: complete on CPython (35,364 referenced
definitions) and this repo (11,869), at the 10-minute cap on the kernel (36,096 with f16,
44,032 with f32).

⁴ Weights and index already in the page cache. The first process after a reboot pays a
one-time page-in, about 4.8 s on the kernel (measured for v0.7.1).

Index on disk (one committed generation with its parsed-product cache and the warmed
search tiers): kernel **8.1 GB** default / 8.5 GB with the learned model; CPython
210 / 280 MB; this repo 880 / 910 MB. Encoder weights: 547 MB (f32) or 274 MB (f16),
stored once under `~/.vorpal/models`.

### How does it compare?

**Against text search.** Structural scan of the Linux kernel (63,775 C files):

```
vorpal scan --rule rule.yml ~/linux     # kind: call_expression + regex: kmalloc
rg 'kmalloc\(' -t c ~/linux             # comparison
```

| Tool | Time | What you get |
|---|---|---|
| `vorpal scan` | 1.5 s (2.5 s the first time) | 6,819 `call_expression` nodes whose text matches `kmalloc` |
| `ripgrep` | 0.8 s | 3,387 text lines containing `kmalloc(` |

The rule matches any call expression containing the pattern, so it also returns
`kmalloc_array(...)`, `devm_kmalloc(...)`, and every outer call that wraps one; an earlier
version of this row reported 42.6k matches from a broader rule. AST-matching 63,775 files
costs about twice a text grep once the parsed products are banked, three times on the
first run, which parses everything.

**Against an agent's built-in tools.** An agent already has grep and read, so we
measured vorpal against them, on this repo and on the Linux kernel, with the v0.8.3
binary on 2026-09-05. Each row asks one question of a warm vorpal daemon (one MCP call,
median of five after a first call) and of the ripgrep-plus-read pipeline behind Claude
Code's Grep and Read tools. Wall time is the tool's own work. The last column is what
the model then has to read.

| Question | vorpal | rg + read | Output the model reads |
|---|---:|---:|---|
| This repo: callers of `tool_result` | 0.10 ms | 16 ms | 2 records with call sites vs 3 text lines |
| This repo: callees of `tool_result` | 0.14 ms | no equivalent | 7 records with call sites |
| This repo: what `run_install` reaches | 0.05 ms, 1 call | 6 ms, `rg -A 75` | 4 records vs 76 lines (3 KB) |
| This repo: source of `render_toml` | 0.05 ms | 39 ms, 2 commands | verified body vs 58 lines |
| Kernel: callers of `schedule_timeout_interruptible` (page of 100) | 2.7 ms | 748 ms | 100 of 140 resolved records (23 KB) vs 164 lines (13 KB) |
| Kernel: callers of `vfs_read` | 0.10 ms | 692 ms | 3 records with call sites vs 13 lines |
| Kernel: callees of `vfs_read` | 0.11 ms | 763 ms, then the body | 4 records with call sites vs 41 lines |
| Kernel: find `schedule_timeout` | 0.05 ms | 677 ms | 1 record vs 1 line |
| Kernel: source of `vfs_read` | 0.08 ms | 713 ms, then a read | verified body vs 42 lines |
| Kernel: what `vfs_read` reaches, depth 2 | 0.06 ms | no equivalent | 3 records |

On a small repo both are far under a model turn; the difference is round trips. On the
kernel, every grep rescans 75,954 files (0.7 to 0.9 s) while the graph answers in
microseconds to milliseconds. Grep's lines also include definitions, comments, and macros
the model has to sift; the graph returns resolved call edges with their grades.

A name with several definitions comes back as a list of candidates, not a merged answer.
`kmalloc` has six in the kernel tree, and its macro form resolves no call edges at all,
so an earlier version of this table that counted those six as callers was wrong.

The first call in a fresh daemon pays a cold open plus the tree revalidation sweep:
119 ms on this repo, 0.27 s on the kernel with the tree's metadata cached, 2.8 s once
when it was not. A kernel tree that changed since its generation pays a rebuild on that
first call instead (9.4 s measured with v0.8.2).

Claude Code hands the model a tool's `structuredContent` as compact JSON and drops the
text block (checked in the 2.1.261 transcripts), so `format` shapes what the model reads
through the structured half. For the three callers of `vfs_read` that is 753 B by default
(lean: name, kind, path, grade, and the call site with its line) and 355 B with
`format: ids`; each page puts its common directory in one `base` field. `toon` only
rewrites the text half, which this client never shows.

**What that costs end to end.** We asked Claude Code 2.1.261 (Opus 5, `--effort high`)
the same four questions three ways:

- grep only: the model could use Grep, Glob, and Read.
- vorpal MCP tools, as Claude Code ships them. Schemas are deferred, so the first use of
  each tool costs a `ToolSearch` turn.
- vorpal CLI from the shell. The server's instructions carry the exact command for its
  index, and the shell tool is never deferred. For this arm, allow the executable by the
  path the server prints, for example `Bash(/path/to/vorpal:*)`.

Tokens count everything the model processed, cache reads included. Cost is what the API
billed with the prompt cache warm: each cell ran four times back to back, and the table
shows medians of the last three. The first-ask surcharge is measured separately below.
Measured 2026-09-06 with v0.8.3.

| Question | Tools | Turns | Tokens | Cost | Wall |
|---|---|---:|---:|---:|---:|
| This repo: who calls `tool_result` | grep + read | 5 | 73 K | $0.081 | 9.4 s |
|  | vorpal MCP tool | 3 | 63 K | $0.054 | 5.9 s |
|  | vorpal CLI via shell | 2 | 43 K | $0.028 | 5.3 s |
| This repo: what `run_install` reaches | grep + read | 4 | 77 K | $0.136 | 11.8 s |
|  | vorpal MCP tool | 3 | 64 K | $0.045 | 7.0 s |
|  | vorpal CLI via shell | 2 | 44 K | $0.040 | 7.9 s |
| Kernel: who calls `vfs_read` | grep + read | 5 | 104 K | $0.200 | 16.6 s |
|  | vorpal MCP tool | 3 | 51 K | $0.042 | 6.9 s |
|  | vorpal CLI via shell | 2 | 36 K | $0.029 | 6.1 s |
| Kernel: what `vfs_read` calls | grep + read | 3 | 59 K | $0.053 | 8.2 s |
|  | vorpal MCP tool | 3 | 51 K | $0.046 | 5.7 s |
|  | vorpal CLI via shell | 2 | 36 K | $0.026 | 5.8 s |

Each turn on Opus re-reads the whole context, about 17 K tokens here, so the turn count
sets the token column. The shell takes two turns (the command and the reply), the MCP
tools three (the schema load, one `graph` or `reachable` call, the reply), and grep three
to six, depending on how many searches the model decides to run.

The bill follows the turns:

- grep: $0.05 to $0.14 a question. Its runs also write the file contents they read at the
  cache-write price, which is why the kernel callers question costs the most.
- MCP tools: $0.04 to $0.05.
- shell: $0.03 to $0.04.

The first ask of an hour also writes the prefix (system prompt, tool schemas,
instructions) at $10 per million tokens on Opus 5. Measured once per arm after an idle
hour, that added $0.09 to $0.14 to a grep question, $0.10 to $0.13 to an MCP one, and
$0.17 to $0.22 to a shell one. The shell arm's prefix is the largest, 18 K to 22 K tokens,
and none of it was already cached; the grep and MCP arms found 7 K to 10 K of theirs
cached by other Claude Code sessions on the machine.

A two-turn shell run takes 5 to 8 s of wall time. vorpal's own work is under 0.3 s of
that; the rest is the model.

On correctness:

- Every vorpal run named the right callers, callees, and reachable definitions.
- Grep found the right file and line every time. On the repo callers question it named
  the enclosing function correctly in one run of six: `tools_call_multi` came back as
  `Router::call_tool` or was left out.
- On the kernel callees question, grep's read also listed two inline helpers,
  `fsnotify_access` and `add_rchar`, that the graph does not resolve as call edges.
- On the `run_install` question, grep found one transitive call made inside a struct
  literal, `claude_desktop_config`, that the graph does not record. The graph arms instead
  carried three depth-2 records the resolver grades `constrained`, names from vendored
  grammar JSON, which the model flagged as not real targets.

Earlier versions of this table, back to the 27-tool surface that took 8 turns and 161 K
tokens on the kernel callers question, are in `docs/wip/BENCHMARKS.md`.

**Against tgrep.** [tgrep] (1.0.4) is a trigram-indexed grep with a server. You run
`tgrep index .`, then `tgrep serve .`, and each `tgrep <pattern> .` connects to the
server. The vorpal equivalent is `vorpal index .`, one `vorpal mcp` daemon, and one call
per question. Both tools were built from source and run on the same checkouts on
2026-09-07. tgrep's index rows are medians of three builds timed with `/usr/bin/time -l`;
vorpal's are the indexing table's. The driver is `evals/tgrep_bench.py`.

| | tgrep | vorpal |
|---|---|---|
| **Linux kernel**, cold index | 8.2 s, 0.31 GB RSS, 1.0 GB on disk, 94,719 files | 8.1 s, 6.1 GB RSS, 4.8 GB on disk, 75,954 files parsed into 8.9 M nodes |
| **CPython**, cold index | 0.54 s, 0.14 GB, 74 MB | 0.9 s, 0.7 GB, 160 MB |
| **This repo**, cold index (49 vendored grammars) | 0.69 s, 0.26 GB, 28 MB | 6.9 s, 11.6 GB, 860 MB |
| tgrep's 102-query kernel suite, median per query | 21 ms, text lines | `text_search` **12.8 ms**, lines with their symbol; `search` **0.65 ms**, ranked definitions |
| Every call of `kmalloc` in the kernel | 18 ms, 3,387 lines matching `kmalloc\(` | `code_search kmalloc($A, $B)` **35 ms**, 2,715 two-argument calls with their functions |
| Callers of `vfs_read` | 7.7 ms, 13 lines | `graph callers` 0.10 ms, 3 call edges with their sites |
| Save a file, then ask again (kernel clone) | 4.4 s | 4.3 s; 2.0 to 2.2 s when `fseventsd` is quiet |

tgrep indexes bytes, so it builds faster, uses far less memory, and takes any regex. A
grep question over the kernel costs about 20 ms with either tool. vorpal's answers carry
more: a call matched with its argument count and the function it sits in, a caller as a
resolved edge, a definition ranked among definitions. `text_search` returns the same
lines an exhaustive scan would; this was checked on every query in the suite.

[tgrep]: https://github.com/microsoft/tgrep

**Against the nearest tool.** [codebase-memory-mcp] (cbm) is also a single local binary
with tree-sitter parsing, a typed code graph, BM25, a Cypher subset, and an MCP server, so
the same corpora, labelled queries, and metrics run against both. This table was measured
2026-09-07 with cbm `997d087` built from source, on the same checkouts and machine. cbm
ran in its `full` mode, the only mode with semantic edges, through its scriptable `cli`;
its memory is peak RSS over the whole process tree, sampled every 50 ms. Search rows use
the same label files and the same NDCG@10 / MRR / recall@5 math; cbm's `search_graph` in
BM25 mode, vorpal's default tier.

| | vorpal | codebase-memory-mcp |
|---|---|---|
| **Linux kernel** cold index (75,954 files) | **8.1 s**, 8.89 M nodes, 6.1 GB peak RSS, 4.8 GB on disk | 296 s, 8.53 M nodes / 16.0 M edges, 30.8 GB peak RSS, 15.8 GB SQLite |
| Kernel, nothing changed | **0.13 s** | 14.2 s |
| **CPython** cold index (3,841 files) | **0.9 s**, 162,945 nodes, 0.7 GB, 160 MB | 38.5 s, 136,118 nodes, 6.5 GB, 632 MB |
| CPython, nothing changed | 0.02 s | 5.2 s |
| **This repo** cold index (49 vendored grammars) | **6.9 s**, 80,611 nodes, 11.6 GB, 860 MB | 43.1 s, 67,797 nodes, 31.8 GB, 297 MB |
| This repo, nothing changed | 0.02 s | 5.4 s |
| Search, kernel labels (NDCG@10 / MRR / recall@5) | **0.329 / 0.327 / 0.358** | 0.218 / 0.188 / 0.293 |
| Search, CPython labels | **0.306 / 0.291 / 0.333** | 0.200 / 0.205 / 0.222 |
| Search, this repo's labels | 0.402 / 0.395 / 0.445 | **0.462 / 0.466 / 0.473** |
| One search, one-shot CLI (kernel) | **0.15 s** (daemon: 0.7 ms) | 5.6 s |
| Callers of a symbol, one-shot CLI (kernel) | **0.01 s** (daemon: 0.1 ms) | 3.6 to 4.1 s |
| Ranking tiers | default, learned (trained per corpus), neural encoder rerank (f16/f32), per-index `tune` | BM25, regex, static per-token vectors |
| Languages | 49 grammars | 162 grammars |
| Determinism | byte-identical generations, incremental = scratch (release-gated) | not claimed |

cbm ships 162 grammars to vorpal's 49, and its BM25 ranks this repo's labels higher than
vorpal's default tier; vorpal's learned and encoder tiers score 0.455 / 0.448 / 0.500 on
that set (table above). On the kernel and CPython labels vorpal's default tier ranks
higher. cbm's memory use reflects its RAM-first indexing design.

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

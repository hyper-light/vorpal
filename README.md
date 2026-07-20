<h1 align="center">vorpal</h1>
<p align="center"><em>Code analysis and search, swift and sharp.</em></p>

Vorpal is a code **ingest → index → search** engine in a single binary. It fuses a precise,
tree-sitter-powered structural search & rewrite engine (built on [ast-grep]) with a **code
knowledge graph**, **hybrid semantic search**, and a built-in **MCP server** so coding agents can
query your codebase the way you do.

Point it at a repository and ask real questions:

```console
$ vorpal index .
parsed 305 files (0 replayed from cache) → 8101 nodes, 16282 calls resolved, 11883 unresolved
index: ./.vorpal/index

$ vorpal graph callers resolve_import_path
resolve [Method] crates/resolve/src/resolver.rs

$ vorpal graph implementors FileExtractor
OutlineExtractor [Struct] crates/ingest/src/outline_extractor.rs
DefRefStub [Struct] crates/ingest/tests/linking.rs
StubExtractor [Struct] crates/ingest/tests/pipeline.rs

$ vorpal search "stat manifest change detection"
0.0167  manifest::{FileStat, Manifest} [Import] crates/ingest/src/lib.rs
0.0164  FileStat [Struct] crates/ingest/src/manifest.rs
0.0161  Manifest [Struct] crates/ingest/src/manifest.rs

$ vorpal mcp          # serve all of the above to agents over MCP (stdio)
```

Every output above is a verbatim capture from running vorpal on its own repository.

## Highlights

- **Structural search & rewrite** — match code by AST pattern, not regex: `vorpal run -p
  'console.log($ARG)'`. Full rule system (YAML), project scanning, rule testing, LSP, and an
  interactive rewrite mode, inherited from the ast-grep engine.
- **A real code knowledge graph** — every definition becomes a node; `calls`, `imports`,
  `implements`, `of_type`, `references`, and containment become edges. All extraction is
  AST-based — never substring matching.
- **Honest resolution** — cross-file references resolve with scope precedence and confidence
  labels (`LOCAL` > exported `CROSS_FILE` > labeled `AMBIGUOUS`). Unresolvable references are
  *counted, never faked*: no phantom edges, ever.
- **Hybrid search** — one query fuses exact/token name matching, lexical-embedding similarity,
  and graph in-degree via reciprocal rank fusion. Querying a symbol by name always surfaces it;
  among same-named symbols, the most-called wins.
- **Incremental by construction** — per-file extraction products are cached; re-indexing
  re-parses only changed files and always re-links the whole graph, so removals and renames can
  never leave stale nodes behind. An unchanged tree re-indexes in milliseconds.
- **Agent-native** — `vorpal mcp` serves the whole surface as MCP tools over stdio.
- **28 languages** — one extraction pipeline, tree-sitter grammars compiled in. No regex
  fallbacks, no per-language plugins to install.
- **Adaptive at both ends** — data-derived configuration everywhere (index tiers, page policy,
  prefilters). A five-file project stays instant and tiny; the same code path scales up.

## Installation

Build from source (Rust 1.85+):

```console
$ git clone https://github.com/hyper-light/vorpal && cd vorpal
$ cargo build --release -p vorpal
$ ./target/release/vorpal --help    # `vp` is a shorter alias binary
```

npm (`@vorpal/cli`), Python (`vorpal-py`), and WebAssembly (`@vorpal/wasm`) packages are built
from this repository (see [Language bindings](#language-bindings)); registry publication is in
progress.

## Structural search & rewrite

The classic engine: patterns are real code with metavariables, parsed by the same grammar as
your source.

```console
$ vorpal run -p 'fetch($URL)' --rewrite 'await fetch($URL)' src/
$ vorpal run -p 'fn $NAME($$$ARGS) -> Result<$T, $E>' -l rust
$ vorpal scan            # run your project's YAML rules (vorpalconfig.yml)
$ vorpal test            # test those rules against expected snapshots
$ vorpal outline src/    # symbols, members, imports/exports per file
```

Pattern syntax, rule schema (`kind`, `inside`, `has`, `all`/`any`/`not`, constraints,
transformations), and utilities follow the ast-grep model — if you know ast-grep, you know this
half of vorpal. Files that cannot possibly match are skipped **before parsing** via a SIMD
literal prefilter derived from your pattern (a per-token AND over required literals), so
searches with any fixed text in them stay fast on large trees.

## The knowledge graph

### Indexing

```console
$ vorpal index [SRC] [--out DIR]     # default output: <SRC>/.vorpal/index
```

One pass per file: tree-sitter parse → definition extraction (data-driven YAML outline rules) →
AST reference extraction (call sites, imports, type uses, implements clauses) → identity
interning (blake3, path-qualified — same-named symbols in different files stay distinct) →
cross-file resolution → graph sealing. The persisted index cold-opens by `mmap`.

Re-runs are incremental: a stat manifest (path, size, mtime) picks the changed files; unchanged
files replay their cached extraction products with zero parsing; the graph is always re-linked
from the complete product set. A fully unchanged tree short-circuits entirely:

```console
$ vorpal index .                     # again, after touching one file
parsed 1 files (297 replayed from cache) → 7932 nodes, 15924 calls resolved, 11503 unresolved

$ vorpal index .                     # again, no changes
unchanged — reused existing index (7932 nodes)
```

### Graph queries

```console
$ vorpal graph callers <name>        # who calls this symbol           (incoming `calls`)
$ vorpal graph refs <name>           # who references it               (incoming `references`)
$ vorpal graph importers <name>      # which files import it           (incoming `imports`)
$ vorpal graph implementors <name>   # types implementing/extending it (incoming `implements`)
$ vorpal graph typeusers <name>      # defs using it as a type         (incoming `of_type`)
$ vorpal graph node <name>           # look the symbol itself up
```

Imports resolve both by symbol (`use b::target`, `import util.Helper`) and by path
(`import "./util"` resolves to the indexed `util.ts` file node — exact path matches only). The
transitive closure (`reachable`) is available through the library and the MCP server.

### Hybrid search

```console
$ vorpal search "resolve import path"
0.0500  resolve_import_path [Function] crates/resolve/src/resolver.rs
```

Three ranked lists fused by reciprocal rank fusion:

1. **Name** — exact matches, then token-identical names, then names containing every query
   token. `resolveImportPath`, `resolve_import_path`, and `resolve import path` all meet on the
   same tokens.
2. **Semantic** — an adaptive vector index (exact scan for small corpora, quantized/graph
   search tiers for large ones, always with an exact rerank) over a deterministic
   lexical-hashing embedder. Embeddings are pluggable behind a trait; the default is honest
   about being lexical similarity, not a neural model.
3. **Graph** — name-matched candidates reordered by in-degree, so the heavily-used symbol
   outranks a dead-weight namesake. In-degree never overrides semantic rank for descriptive
   queries.

## MCP server guide

`vorpal mcp` speaks the [Model Context Protocol] over stdio (JSON-RPC 2.0, one message per
line; protocol revisions `2024-11-05`, `2025-03-26`, `2025-06-18`).

**Claude Code:**

```console
$ claude mcp add vorpal -- /path/to/vorpal mcp --index /path/to/repo/.vorpal/index
```

**Any MCP client** (generic stdio server config):

```json
{
  "mcpServers": {
    "vorpal": {
      "command": "/path/to/vorpal",
      "args": ["mcp", "--index", "/path/to/repo/.vorpal/index"]
    }
  }
}
```

The daemon holds the graph **warm** across calls — one mmap cold-open, then every query is
served from memory. The `index` tool builds or refreshes the index in place (near-instant when
the tree is unchanged), so an agent can bootstrap from an empty directory.

| Tool | Arguments | Returns |
|---|---|---|
| `index` | `src` | Build/refresh the index, keep it warm |
| `node` | `name` | Nodes matching an exact symbol name |
| `callers` | `name` | Direct callers (incoming `calls`) |
| `references` | `name` | Direct referrers (incoming `references`) |
| `importers` | `name` | Files importing the symbol (incoming `imports`) |
| `implementors` | `name` | Types implementing/extending it (incoming `implements`) |
| `type_users` | `name` | Definitions using it as a type (incoming `of_type`) |
| `reachable` | `name`, `direction: "in"\|"out"` | Transitive closure (e.g. all transitive callers) |
| `search` | `query`, `k?` | Hybrid search, top-k with scores |

A raw session, if you want to script it:

```console
$ printf '%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"callers","arguments":{"name":"target"}}}' \
  | vorpal mcp --index .vorpal/index
```

Tool-level failures come back in-band (`isError: true` with a message), protocol errors as
JSON-RPC errors — an agent never has to parse panics.

## Supported languages

All 28 grammars are compiled into the binary. Every language gets definition extraction
(functions, types, methods, fields — through to document structure for data/markup formats);
the reference columns show which relation edges each grammar's syntax supports today.

| Language | Extensions | Defs | Calls | Imports | Type uses | Implements |
|---|---|:-:|:-:|:-:|:-:|:-:|
| Bash | `sh` `bash` `zsh` `ksh` `bats` … | ✓ | ✓ | ✓ `source` | — | — |
| C | `c` `h` | ✓ | ✓ | ✓ includes | ✓ | — |
| C++ | `cpp` `cc` `hpp` `cxx` `cu` … | ✓ | ✓ | ✓ includes | ✓ | — |
| C# | `cs` | ✓ | ✓ | ✓ | — | ✓ |
| CSS | `css` `scss` | ✓ rules/props | — | — | — | — |
| Dart | `dart` | ✓ | ✓ | ✓ | — | — |
| Elixir | `ex` `exs` | ✓ | ✓ | ✓ `import`/`alias`/`use` | — | — |
| Go | `go` | ✓ | ✓ | ✓ | ✓ | — |
| Haskell | `hs` | ✓ | ✓ | ✓ | — | — |
| HCL / Terraform | `tf` `hcl` `tfvars` … | ✓ blocks/attrs | ✓ | — | — | — |
| HTML | `html` `htm` `xhtml` | ✓ elements | — | — | — | — |
| Java | `java` | ✓ | ✓ | ✓ | ✓ | ✓ |
| JavaScript | `js` `jsx` `mjs` `cjs` | ✓ | ✓ | ✓ ES + `require` | — | ✓ `extends` |
| JSON | `json` | ✓ keys | — | — | — | — |
| Kotlin | `kt` `kts` `ktm` | ✓ | ✓ | ✓ | ✓ | — |
| Lua | `lua` | ✓ | ✓ | ✓ `require` | — | — |
| Markdown | `md` `markdown` | ✓ sections | — | — | — | — |
| Nix | `nix` | ✓ bindings | ✓ | ✓ `import` | — | — |
| PHP | `php` | ✓ | ✓ | ✓ | — | — |
| Python | `py` `pyi` `bzl` `bazel` … | ✓ | ✓ | ✓ | — | ✓ superclasses |
| Ruby | `rb` `gemspec` … | ✓ | ✓ | ✓ `require` | — | — |
| Rust | `rs` | ✓ | ✓ | ✓ | ✓ | ✓ `impl T for` |
| Scala | `scala` `sbt` `sc` | ✓ | ✓ | ✓ | — | — |
| Solidity | `sol` | ✓ | ✓ | ✓ | — | — |
| Swift | `swift` | ✓ | ✓ | ✓ | ✓ | — |
| TSX | `tsx` | ✓ | ✓ | ✓ | ✓ | ✓ |
| TypeScript | `ts` `mts` `cts` | ✓ | ✓ | ✓ | ✓ | ✓ |
| YAML | `yaml` `yml` | ✓ keys | — | — | — | — |

“—” means the relation doesn't exist in that language's syntax (there are no calls in JSON) or
isn't extracted yet. Anything not extracted is simply absent — never guessed.

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

Design principles worth knowing before you rely on it:

- **Necessary-condition filters only.** Prefilters (literal scan, stat manifest) may only skip
  work that provably cannot produce results; correctness never depends on them.
- **Full re-link from complete inputs.** Incrementality caches *extraction*, not conclusions —
  identity, resolution, and edges are recomputed every run, making staleness structurally
  impossible rather than carefully avoided.
- **Determinism.** Same input, same index — bit-identical, including the vector tier's graph
  build.

The index directory (default `<src>/.vorpal/index` — hidden, so it never indexes itself):

```
nodes.vseg      columnar node segment (mmap cold-open, blake3 + xxh3 integrity)
strings.heap    names / paths / signatures
edges.bin       edge list (rebuilt into CSR/CSC on load)
ann.bin         vector index (ids, vectors, tier structure)
manifest.bin    stat manifest driving incremental re-index
products/       per-file extraction product cache (JSON, keyed by blake3(path))
```

The full architecture — storage format, adaptive memory model, concurrency plan, and the
billion-LOC scaling roadmap — lives in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Performance

Indicative numbers from this repository (~300 files, ~8k nodes; debug builds on Apple Silicon):

| Operation | Time |
|---|---|
| Full index (parse + resolve + link + persist) | ~2.5 s |
| Re-index after touching one file | ~0.31 s (1 parsed, 297 replayed) |
| Re-index, nothing changed | ~0.06 s |
| `vorpal run` structural search, no-match pattern | 0.032 s (2.3× faster than pre-prefilter) |
| Graph / search queries | milliseconds (mmap cold-open + in-memory graph) |

## CLI reference

| Command | What it does |
|---|---|
| `vorpal run -p <pattern>` | One-off structural search/rewrite (default command) |
| `vorpal scan` | Run configured YAML rules across a project |
| `vorpal test` | Test rules against snapshots |
| `vorpal new` | Scaffold projects/rules/tests |
| `vorpal lsp` | Start the language server |
| `vorpal outline [paths]` | File structure: symbols, members, imports/exports |
| `vorpal index [src] [--out]` | Build/refresh the knowledge-graph index |
| `vorpal graph <verb> <name> [--index]` | `callers` `refs` `importers` `implementors` `typeusers` `node` |
| `vorpal search <query> [-k] [--index]` | Hybrid (name + semantic + graph) search |
| `vorpal mcp [--index]` | Serve the MCP server over stdio |
| `vorpal completions` | Shell completion scripts |

## Language bindings

| Package | Tech | Surface |
|---|---|---|
| `@vorpal/napi` | napi-rs | Node.js native bindings to the pattern engine |
| `vorpal-py` | PyO3 / maturin | Python bindings (`vorpal_py`) |
| `@vorpal/wasm` | wasm-pack | Browser/WASM pattern engine |
| `@vorpal/cli` | npm wrapper | Ships the `vorpal` / `vp` binaries per platform |

## Developer guide

### Workspace map

```
crates/
  core       pattern engine: matcher, meta-variables, replacer, tree-sitter integration
  config     YAML rule schema, rule composition, severity/fixers
  language   the 28 SupportLang grammars + extension dispatch
  cli        the `vorpal` binary (run/scan/test/new/lsp/outline + index/graph/search/mcp)
  dynamic    dynamic-language loading        lsp: language server        outline: outline rules

  mem        adaptive memory substrate: probes → page/arena/prefetch policy, mmap, CSR
  segment    immutable columnar `.vseg` container + dense-id directory + integrity hashes
  graph      edge LSM (CSR/CSC), locality relabel, transitive closure (direction-optimizing)
  canonical  blake3 → node-id identity/dedup/skip index
  kg         graph assembly + queries + persistence (save / mmap cold-open)
  resolve    cross-file reference resolution (scope precedence, confidence, path imports)
  ingest     streaming pipeline: walk → parse → extract → products → link (incremental cache)
  ann        vector tier: pluggable embedders, adaptive flat/quantized/Vamana + exact rerank
  index      `vorpal-index` CLI library: build_index / graph_query / search_index
  mcp        the MCP server (pure protocol layer + stdio loop)

  napi  pyo3  wasm    language bindings
```

### Building and testing

```console
$ cargo build -p vorpal            # the main binary
$ cargo test --workspace           # full suite (~830 tests)
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo fmt --check
```

Every change in this repository lands with all four green; behavioral slices additionally get
end-to-end verification against the real binaries (see commit messages for the evidence trail).

### Adding or deepening a language

1. **Definitions** — add/extend `crates/outline/src/default_rules/<lang>.yml` (data-driven;
   same schema as user rule files) and register it in `crates/outline/src/default_rule.rs`.
   Grammar node kinds come from the grammar's `node-types.json` — verify, don't guess.
2. **References** — add a row to the per-language table in `crates/ingest/src/references.rs`
   (`RefSpec`: call/import/type/implements node kinds + selectors; selectors handle named
   fields, positional children, and defs-that-are-calls like Elixir's `def`).
3. **Prove it** — add the language to `crates/ingest/tests/lang_matrix.rs`: a minimal fixture
   asserting a resolved `calls` edge end-to-end (and structure nodes for data languages). The
   28-language coverage gate in `crates/ingest/tests/engine.rs` will hold you honest.

## Acknowledgements

Vorpal's structural search engine began as [ast-grep] by [Herrington Darkholme] and
contributors — an exceptional foundation, gratefully built upon. The knowledge-graph, semantic
search, and MCP layers are original to vorpal.

## License

MIT.

[ast-grep]: https://github.com/ast-grep/ast-grep
[Herrington Darkholme]: https://github.com/HerringtonDarkholme
[Model Context Protocol]: https://modelcontextprotocol.io

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
DefRefStub [Struct] ./crates/ingest/tests/linking.rs
StubExtractor [Struct] ./crates/ingest/tests/pipeline.rs

$ vorpal search "stat manifest change detection"
0.0167  FileStat [Struct] ./crates/ingest/src/manifest.rs
0.0164  Manifest [Struct] ./crates/ingest/src/manifest.rs
0.0161  manifest [Module] ./crates/ingest/src/lib.rs

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
literal prefilter derived from your pattern — and, for YAML/inline rules, from `regex`
constraints too (a conservative required-literal analysis of the parsed regex: `[A-Z]+_SUSPEND`
requires `_SUSPEND`). Both `run` and `scan` consult it, so searches with any fixed text stay
fast on large trees: finding every `^[A-Z]+_SUSPEND$` **identifier** in the Linux kernel
(63,775 C files) takes **1.1 s** — parity with `rg -n -w` on the same machine — while
returning AST nodes instead of text lines (383 identifiers; ripgrep's extra lines are
comments, strings, and docs).

**Search feeds the index.** In a tree that has a `.vorpal` index, every file a `run` or `scan`
matches banks its extraction product into the index's cache as a side effect — the parse the
search already paid for is never thrown away. The next `vorpal index` replays those products
instead of re-parsing (products are self-validating; see below), so searching and indexing
converge on the same warm cache. Searches never *create* index state: an un-indexed tree stays
untouched, and a file rewritten by `--update-all` simply re-parses at the next index (its
banked pre-rewrite product no longer matches the file's stat).

## The knowledge graph

### Indexing

```console
$ vorpal index [SRC] [--out DIR]     # default output: <SRC>/.vorpal/index
```

One pass per file: tree-sitter parse → definition extraction (data-driven YAML outline rules) →
AST reference extraction (call sites, imports, type uses, implements clauses) → identity
interning (blake3, path-qualified — same-named symbols in different files stay distinct) →
cross-file resolution → graph sealing. The persisted index cold-opens by `mmap`.

Indexing streams under a **byte budget**: files are admitted in order against a fixed
in-flight byte ceiling, extracted by scoped workers with reused per-worker buffers, and
committed straight into per-shard writers — a product exists in memory only between
extraction and application, so peak transient memory is set by the budget, not the corpus.

Re-runs are incremental: each cached extraction product is **self-validating** — it records
the stat (size, mtime) of the source it was extracted from and replays only while that still
matches. It makes no difference *which run* wrote a product: a completed index, an interrupted
one (killed runs lose no work), or a search that banked its matches. Everything else re-parses,
and the graph is always re-linked from the complete product set. A fully unchanged tree
short-circuits entirely:

```console
$ vorpal index .                     # again, after touching one file
parsed 1 files (354 replayed from cache) → 9795 nodes; refs: 10619 resolved, 1508 ambiguous, 7748 external, 8826 masked

$ vorpal index .                     # again, no changes
unchanged — reused existing index (9795 nodes)
```

Every reference is accounted for in one of four buckets, and an edge is only ever created on
evidence:

- **resolved** — one visible definition binds: same file, exported cross-file, or a
  language-structural private scope (Rust ancestor-module privates are visible to their
  subtree; Java package-privates are visible within their directory). Qualified references
  bind precisely: `Kg::load()`, `self.helper()`, and `Self::assoc()` resolve against the
  *owner's* members, and `util::helper()` against the `util` module file.
- **ambiguous** — several visible definitions tie for a bare name; a deterministic pick is
  emitted with a lowered confidence label, never silently.
- **external** — the name is defined nowhere in the tree (`Ok`, `expect`, `PathBuf`): calls
  into std or dependencies. Expected, honest, and not an error.
- **masked** — same-named definitions exist but none is safely attributable: a method call on
  an untyped receiver (`x.map()`), or a static path whose owner isn't in the tree
  (`Vec::new()` when other `new`s exist). Guessing here would fake edges, so vorpal refuses —
  `graph callers` results stay trustworthy.

Import/alias nodes are wiring, not definitions — they are never resolution targets, and
generic type parameters (`fn f<T>(x: T)`) are binders, not type uses, so neither pollutes the
graph or the unresolved counts.

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

**Always fresh, for free.** When the index lives at the default `<src>/.vorpal/index`
location, the daemon watches `<src>` through the OS (FSEvents on macOS, inotify on Linux) and
revalidates lazily: a query on an untouched tree costs one atomic flag check — measured at
**2.8 µs per complete tool call** — and a query after an edit transparently runs the
incremental re-index first (only changed files parse), so answers are never stale. The watch
is a necessary-condition filter in the §3.4 sense: anything doubtful — watcher errors, event
overflow, changes from before the daemon started — fails open to revalidation, never to
staleness. Reads, hidden trees (`.vorpal`, `.git`), and gitignored churn (`target/`) never
trigger revalidation. Custom `--index` locations have no derivable source root to watch and
keep the explicit `index`-tool behavior.

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
  impossible rather than carefully avoided. Extraction products carry a format-generation
  stamp, so upgrading vorpal re-parses instead of replaying stale-shaped caches.
- **Edges on evidence only.** Resolution binds on what the grammar proves (qualifiers,
  self-receivers, module structure) and refuses to guess otherwise: every reference lands in
  `resolved` / `ambiguous` / `external` / `masked`, and a coin flip is never presented as an
  edge.
- **Determinism.** Same input, same index — bit-identical, including the vector tier's graph
  build and the parallel ingest fan-out.

The index directory (default `<src>/.vorpal/index` — hidden, so it never indexes itself):

```
nodes.vseg      columnar node segment (mmap cold-open, blake3 + xxh3 integrity)
strings.heap    names / paths / signatures
edges.bin       edge list (rebuilt into CSR/CSC on load)
ann.bin         vector index (ids, vectors, tier structure; built lazily by the first search)
manifest.bin    stat manifest driving incremental re-index
products/       per-file extraction product cache (.vpb binary, keyed by blake3(path))
```

The full architecture — storage format, adaptive memory model, concurrency plan, and the
billion-LOC scaling roadmap — lives in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Performance

Measured on this repository (351 files, ~2.8 MB of source, ~9.6k nodes; **release** builds,
Apple Silicon; wall-clock for the whole CLI invocation including process start):

| Operation | Time |
|---|---|
| Full cold index (walk + parse + extract + resolve + link + persist) | **0.05 s** |
| Re-index after touching one file | 0.03 s (1 parsed, 350 replayed) |
| Re-index, nothing changed | 0.01 s |
| `vorpal run` structural search, no-match pattern | 0.017 s |
| `vorpal scan`, regex rule `^[A-Z]+_SUSPEND$` over the **Linux kernel** | 1.1 s — ripgrep parity, structural results |
| Graph / search queries | milliseconds (mmap cold-open + in-memory graph) |

At kernel scale (Linux 7.2-rc4: 72,541 files, ~30M LOC → 2.74M nodes, 6.8M references;
Apple M5 Max, 18 cores): cold index **6.7 s** at a **0.54–0.57 GB** peak footprint
(references spill to disk between commit and resolution, resolved edges stream straight
into the writer, and the merged string heap **writes through to disk** as shards absorb —
the link pass reads it back through a zero-copy map; products append to a single **pack
file** — the loose-file cache cost 72k `open(2)`s per run; small commit shards keep every
committer busy; jemalloc with immediate page return keeps the footprint tracking the live
set), one-file incremental re-index **1.25 s**, unchanged re-index **0.10 s**. Index
artifacts land via tmp + rename, so a rebuild never truncates a file a live reader still
has mapped. The vector tier builds lazily on the first `search`: **~14 s** for 2.4M
definition rows (1.2 GB peak — per-row i8 quantization with exact SDOT integer dot products;
Import nodes are wiring and stay out of the vector tier, while remaining reachable through
the exact-name channel; a full-precision rerank of the candidate pool keeps final ordering
exact), stamp-validated thereafter — graph queries never pay for embeddings, and incremental
re-indexes never rebuild the vector graph. **Every persisted tier — node columns, string
heap, graph CSR, and the quantized vector index — opens by `mmap`, zero-copy**: a warm
search is **0.05 s** end to end, and a graph query (`callers kmalloc` → 2,440 results) is
**0.01 s**, both including process start; only the pages a query touches ever load.

In-process (the MCP daemon and library callers skip process start): a cold full index of this
repository is ~57 ms, and a no-change re-validation by polling is ~6 ms — ~3 ms of which is
the gitignore-aware walk + `stat` of every file, the floor for *proving* nothing changed
without an OS file watcher. The watched `vorpal mcp` daemon removes even that: with FSEvents /
inotify reporting changes, steady-state freshness is one atomic flag check — a complete MCP
tool call (JSON-RPC parse + freshness check + graph query + render) measures **2.8 µs**.

Per-file work fans out across cores (rayon work-stealing, §7.5 of the architecture doc); the
output is bit-identical to a serial build — indexing twice produces byte-for-byte identical
`nodes.vseg`, `edges.bin`, and `ann.bin`. Hot paths are allocation-audited: node attribute
reads (`kg.node`) perform zero heap allocations, and the embedding tier hashes tokens without
materializing them.

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

## TypeScript API

`@vorpal/node` exposes the pattern engine to Node.js as native bindings (napi-rs), with full
TypeScript definitions — including per-language typed node kinds, so `node.kind()` and
`node.field(...)` narrow like you'd hope. Built from `crates/napi`. Every snippet below was
executed against the built module before landing in this README.

### Parse, query, capture

```ts
import { parse, Lang } from '@vorpal/node'

// Patterns are real code; metavariables capture real AST nodes.
const root = parse(Lang.TypeScript, 'console.log(user.name); console.log(count)')
const node = root.root().find('console.log($ARG)')

node.kind()                  // "call_expression"
node.text()                  // 'console.log(user.name)'
node.getMatch('ARG').text()  // "user.name"
node.range()                 // { start: { line, column, index }, end: { ... } }

// `$$$` captures node lists:
const call = root.root().find('console.log($$$ARGS)')
call.getMultipleMatches('ARGS').map(n => n.text())   // ["user.name"]
```

### Rewrite: edits are explicit and composable

```ts
const edits = root
  .root()
  .findAll('console.log($A)')
  .map(n => n.replace(`logger.info(${n.getMatch('A').text()})`))

root.root().commitEdits(edits)
// => "logger.info(user.name); logger.info(count)"
```

### A complete codemod

Migrate a test suite's assertions in a few lines:

```ts
import { parse, Lang } from '@vorpal/node'

function modernizeAsserts(source: string): string {
  const root = parse(Lang.TypeScript, source)
  const edits = root
    .root()
    .findAll('assert.equal($ACTUAL, $EXPECTED)')
    .map(n =>
      n.replace(
        `expect(${n.getMatch('ACTUAL').text()}).toEqual(${n.getMatch('EXPECTED').text()})`,
      ),
    )
  return root.root().commitEdits(edits)
}

modernizeAsserts(`assert.equal(add(1, 2), 3); assert.equal(name, 'ada');`)
// => "expect(add(1, 2)).toEqual(3); expect(name).toEqual('ada');"
```

### Rule objects: the full YAML rule system, inline

Everything the CLI's rule files can express — `kind`, `inside`, `has`, `all`/`any`/`not`,
`stopBy` — works as a plain object:

```ts
// every call expression
root.root().findAll({ rule: { kind: 'call_expression' } })

// console.log calls, but only inside function declarations
root.root().findAll({
  rule: {
    pattern: 'console.log($A)',
    inside: { kind: 'function_declaration', stopBy: 'end' },
  },
})

// relational/composite checks read as node predicates too
node.matches('console.log($A)')
node.inside('function_declaration')
node.has('member_expression')
node.follows('$SOMETHING')
```

### Navigate the tree

```ts
const fn = root.root().find({ rule: { kind: 'function_declaration' } })
fn.field('name').text()        // typed field access: "f"
fn.children()                  // Array<SgNode>
fn.parent() / fn.child(0)      // structural moves
fn.next() / fn.prev()          // siblings (plus nextAll() / prevAll())
call.ancestors().map(n => n.kind())
// ["arguments", "call_expression", "expression_statement", ...]
node.is('call_expression')     // type-guard narrowing for typed kinds
```

### Scale: parse and search off the main thread

Parsing, file discovery, and matching run in Rust worker threads:

```ts
import { parseAsync, parseFiles, findInFiles } from '@vorpal/node'

await parseAsync(Lang.TypeScript, source)   // threaded parse of one source

// Walk directories, parse, and match entirely in Rust; matches stream back per file.
const fileCount = await findInFiles(
  Lang.TypeScript,
  { paths: ['src/'], matcher: { rule: { pattern: 'console.log($MSG)' } } },
  (err, nodes) => {
    for (const n of nodes) {
      console.log(`${n.getRoot().filename()}: ${n.text()}`)
    }
  },
)
```

`registerDynamicLanguage(...)` loads custom tree-sitter grammars at runtime, and
`kind(lang, name)` / `pattern(lang, src)` precompile matchers for reuse.

## Python API

`vorpal-py` (PyO3/maturin, module `vorpal_py`) exposes the same engine with a Pythonic surface:
snake_case methods, and rule objects as keyword arguments. As with the TypeScript section,
every snippet below was executed against a freshly built wheel before landing here.

```python
from vorpal_py import SgRoot

# Parse, query, capture.
root = SgRoot("console.log(user.name); console.log(count)", "typescript")
node = root.root().find(pattern="console.log($ARG)")
node.kind()                    # "call_expression"
node.get_match("ARG").text()   # "user.name"

# Rewrite: explicit edits committed against the source.
r = root.root()
edits = [
    n.replace("logger.info({})".format(n.get_match("A").text()))
    for n in r.find_all(pattern="console.log($A)")
]
r.commit_edits(edits)
# => "logger.info(user.name); logger.info(count)"
```

Rules compose as keyword arguments — the same vocabulary as the YAML schema and the
TypeScript rule objects:

```python
fn_root = SgRoot("function f() { console.log(1) }", "typescript")

# console.log calls, but only inside function declarations
hits = fn_root.root().find_all(
    pattern="console.log($A)",
    inside={"kind": "function_declaration", "stopBy": "end"},
)

# node predicates
call = fn_root.root().find(kind="call_expression")
call.matches(pattern="console.log($A)")        # True
call.inside(kind="function_declaration")       # True
```

Navigation and captures mirror the TypeScript API:

```python
fn = fn_root.root().find(kind="function_declaration")
fn.field("name").text()                        # "f"
fn.children() / fn.parent() / fn.child(0)      # structural moves
fn.next() / fn.prev()                          # siblings (+ next_all / prev_all)

multi = fn_root.root().find(pattern="console.log($$$ARGS)")
[n.text() for n in multi.get_multiple_matches("ARGS")]   # ["1"]

[n.kind() for n in call.ancestors()][:3]
# ["expression_statement", "statement_block", "function_declaration"]
```

`register_dynamic_language(...)` loads custom tree-sitter grammars, and `Range`/`Pos`/`Edit`
are plain classes with the same shapes as their TypeScript counterparts.

## Other bindings

| Package | Tech | Surface |
|---|---|---|
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

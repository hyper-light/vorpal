# Using vorpal with Claude (MCP)

vorpal ships an [MCP](https://modelcontextprotocol.io) server that gives an AI agent
first-class tools for navigating your codebase: jump to a symbol, find its callers,
trace what reaches it, search semantically, run structural (ast-grep-style) patterns,
and pull the exact source behind any result — all backed by vorpal's knowledge-graph
index of your repository.

It speaks MCP over **stdio** (the standard for local servers), so it plugs directly into
Claude Desktop, Claude Code, and any other MCP client.

## Prerequisites

- The `vorpal` CLI on your `PATH` — see [getting-started.md](./getting-started.md) for install.
- Nothing else. The server builds and refreshes the index for you on demand (see below).

## Setup

### Claude Code

```sh
claude mcp add vorpal -- vorpal mcp --index /absolute/path/to/your/project/.vorpal/index
```

### Claude Desktop (and other clients that use a JSON config)

Add this to your MCP servers config:

```json
{
  "mcpServers": {
    "vorpal": {
      "command": "vorpal",
      "args": ["mcp", "--index", "/absolute/path/to/your/project/.vorpal/index"]
    }
  }
}
```

**Use an absolute path.** MCP clients start the server without a working directory, and the
index path (default `.vorpal/index`) is resolved relative to that — so spell it out in full.

> If `vorpal` isn't on the client's `PATH`, set `"command"` to the absolute path of the binary
> (e.g. `/usr/local/bin/vorpal`).

## How it works

- The server watches your project. When the index directory is the default
  `<project>/.vorpal/index` layout, vorpal **auto-builds and incrementally refreshes** the index
  as files change — the first query indexes the repo, later queries are near-instant, and edits
  are picked up automatically. You don't have to run `vorpal index` first.
- If you point `--index` at a **custom** location (not `<src>/.vorpal/index`), there's no file
  watcher; ask the agent to run the **`index`** tool once (or pre-build with `vorpal index`).
- Every result is pinned to a content-addressed *generation* of the index, so answers are
  internally consistent even while you're editing.

## The tools

**Build & health**
| Tool | What it does |
|---|---|
| `index` | Build or refresh the graph index from a source directory (near-instant when unchanged), then hold it warm. |
| `health` | Per-file parse damage: ERROR-node counts, affected-byte ratios, and which definitions overlap damaged regions — the difference between "no edge" and "unknowable here." |

**Graph navigation** (all take a symbol `name`; ambiguous names list candidates)
| Tool | What it does |
|---|---|
| `node` | Nodes matching an exact symbol name. |
| `callers` | Direct callers of a symbol (incoming `calls` edges). |
| `references` | Direct referrers (incoming `references` edges). |
| `importers` | Files importing a symbol (incoming `imports` edges). |
| `implementors` | Types implementing/extending a trait, interface, or base type. |
| `type_users` | Definitions using a type in fields, params, returns, or annotations. |
| `reachable` | Transitive traversal from a symbol — `direction: "in"` (everything reaching it) or `"out"` (everything it reaches), with the path back to the seed. Restrict edge types with `relations` (default `["calls"]`; add `"data_flows"` to follow argument flow). |
| `data_flow` | Where a symbol's arguments flow: per-argument rows (`arg#i` → callee `param#j`, with the argument expression when traceable) joined from the `dataflow.bin` sidecar. Captured for Rust/Python/TypeScript/TSX call sites; older generations without the sidecar answer empty. |
| `query` | Cypher-shaped read-only queries: `MATCH (f:Function)-[:calls*1..3]->(g)-[:imports]->(h) WHERE f.in_degree >= 20 AND (g.path CONTAINS "core" OR NOT g.exported = true) RETURN f.name, g.name LIMIT 20`. Linear patterns up to 8 segments, AND/OR/NOT predicate trees with parentheses and ordered comparisons, projections or `COUNT(*)`/`COUNT(DISTINCT …)` with one grouping key, `ORDER BY`/`SKIP`/`LIMIT`. Runs under explicit work ceilings (16KiB text, depth 10, 5M edge visits, 100k rows) and refuses by naming the ceiling instead of truncating. |

**Search**
| Tool | What it does |
|---|---|
| `search` | Hybrid search over definitions — exact/token name match + lexical-embedding similarity + graph in-degree, fused into a top-k ranking. |
| `structural_search` | ast-grep-style structural pattern search with metavariables (`$X`, `$$$ARGS`), matched on the AST — returns `path:line` + matched text. |
| `rule_search` | Run full YAML rule(s) (composite/relational rules, constraints, `fix` dry-runs) over the watched tree. |
| `ast_dump` | Print the named-node tree (kind, byte span, leaf text) for a file or inline snippet — ground truth for authoring patterns. |

**Evidence** (why the graph says what it says)
| Tool | What it does |
|---|---|
| `fetch_span` | The defining source of a graph node, verbatim and digest-verified — pass a node `id` from any result. |
| `why` | Evidence for the edge(s) between two nodes: edge type, resolution grade, resolver reason, and source span. |

## Example asks

Once connected, you can ask Claude things like:

- *"What calls `parse_config`? Show me the ones in the auth module."*
- *"Trace everything reachable from `handle_request` through calls, up to depth 3."*
- *"Find every place we construct a `Session` — use a structural pattern."*
- *"Who implements the `Storage` trait?"*
- *"Search for functions related to 'retry backoff' and show the top 5."*
- *"Why does the graph think `foo` calls `bar`? Show the evidence."*

The agent picks the right tool, and every claim is grounded in your actual indexed source.

## Troubleshooting

- **"no index loaded … call the 'index' tool first"** — you pointed at a custom `--index` path
  with no build yet. Ask the agent to run `index` with your source dir, or use the default
  `<project>/.vorpal/index` layout for auto-indexing.
- **Server doesn't start** — confirm `vorpal mcp --index <abs path>` runs in your terminal; if it
  does, the issue is the client's `command`/`PATH`. Use an absolute path to the binary.

## Custom languages and the daemon

Custom/dynamic languages (grammar `.so` files) are registered by the **launching process at
startup** — `vorpal mcp` performs the one-shot registration (the only `dlopen`) while reading
`vorpalconfig.yml`, before the first request is served. Nothing reachable through the MCP
surface can load code: the serving loop and every tool run against the fixed set of grammars
registered at launch, and the daemon's rebuilds use the same extraction environment (outline
rules, ref specs, canaries, injection config) that `vorpal index` builds from the project
config. A dynamic language without a `canary` is extracted best-effort and named in every
`index` tool response as unverified — never silently trusted.

## Freshness and crash isolation

The daemon watches the source tree (FSEvents/inotify) and rebuilds **proactively**: after a
save, once the tree is quiet for half a second, a background worker rebuilds the index so the
first query after an edit is already warm (it pays a fast-path check plus an mmap reload, not
the build). Disable with `--no-watch-rebuild` (or `VORPAL_WATCH_REBUILD=0`); queries then
refresh lazily, exactly as before.

Builds run **supervised** whenever an indexer binary can be found (`VORPAL_INDEX_BIN`
override; the daemon's own executable when it is `vorpal`/`vorpal-index`; else a
`vorpal-index` beside it): the indexer runs as a child process, so a pathological input — a
grammar crash, a runaway allocation — costs one build attempt and an error string, never the
server. The served graph keeps answering from the committed generation throughout; only the
atomic `CURRENT` swap publishes new work, and `index` responses are prefixed `(supervised)`
when a child ran. Without a discoverable binary the build runs in-process and says so.
Child builds are killed after `VORPAL_MCP_BUILD_TIMEOUT_S` (default 1800).

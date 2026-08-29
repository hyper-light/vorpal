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
| `reachable` | Transitive traversal from a symbol — `direction: "in"` (everything reaching it) or `"out"` (everything it reaches), with the path back to the seed. Restrict edge types with `relations` (default `["calls"]`). |

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

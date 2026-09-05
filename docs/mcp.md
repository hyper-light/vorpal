# Using vorpal with Claude (MCP)

vorpal ships an [MCP](https://modelcontextprotocol.io) server that gives an AI agent
first-class tools for navigating your codebase: jump to a symbol, find its callers,
trace what reaches it, search semantically, run structural (ast-grep-style) patterns,
and pull the exact source behind any result — all backed by vorpal's knowledge-graph
index of your repository.

It speaks MCP over **stdio** (the standard for local servers), so it plugs directly into
Claude Desktop, Claude Code, Codex, Cursor, and any other MCP client. The server implements
protocol revision **2026-07-28** and keeps the `initialize`-era envelope for clients on
2025-11-25 and earlier; see [Protocol](#protocol) below.

## Prerequisites

- The `vorpal` CLI on your `PATH` — see [getting-started.md](./getting-started.md) for install.
- Nothing else. The server builds and refreshes the index for you on demand (see below).

## Setup

### The fast path: `vorpal mcp install`

```sh
vorpal mcp install
```

Run it from the project root you want served. It writes an entry for every client it can
find (`--client claude-code|claude-desktop|codex|cursor|vscode|windsurf|all`, default all)
that launches this binary by absolute path with `mcp --index <project>/.vorpal/index`,
also absolute. Edits are idempotent; a file that is modified is backed up first
(`*.bak-vorpal-<epoch>`), a file that already holds the entry is left untouched, and a
file that is not valid JSON or TOML aborts the run unchanged. `--dry-run` prints what
would be written. Restart your client afterwards. The manual routes below do the same
thing by hand.

### Claude Code

```sh
claude mcp add vorpal -- vorpal mcp --index /absolute/path/to/your/project/.vorpal/index
```

### Codex CLI

```toml
# ~/.codex/config.toml
[mcp_servers.vorpal]
command = "vorpal"
args = ["mcp", "--index", "/absolute/path/to/your/project/.vorpal/index"]
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

## Protocol

The server speaks JSON-RPC 2.0, one message per line, over stdio. It implements MCP
revision **2026-07-28** and serves the earlier `initialize` handshake alongside it, deciding
per request from the message itself (the versioning page permits a dual-era server):

- **2026-07-28 clients** send `params._meta` with `io.modelcontextprotocol/protocolVersion`
  and `io.modelcontextprotocol/clientCapabilities` on every request. `server/discover`
  answers with the supported versions (`["2026-07-28"]`), capabilities (`tools`),
  `instructions`, and `io.modelcontextprotocol/serverInfo`; every result carries
  `resultType: "complete"` and the server identity in `_meta`; `tools/list` and
  `server/discover` carry `ttlMs` and `cacheScope: "public"` (the tool set is fixed for
  the life of the process). A request naming another version is refused with `-32022`
  and `data.supported`; a request missing the required `_meta` fields is `-32602`.
- **2025-11-25, 2025-06-18, and 2025-03-26 clients** open with `initialize` and get the
  requested version echoed (any other version gets `2025-11-25`), plus `ping`. Claude Code
  2.1 opens this way. `2024-11-05` is not offered: it requires JSON-RPC batching.
- **Methods**: `server/discover`, `tools/list`, `tools/call`, and, for legacy clients,
  `initialize` and `ping`. Everything else is `-32601`, including the removed
  `logging/setLevel` and `resources/subscribe`. The server never sends requests of its own
  and uses none of the deprecated Roots, Sampling, or Logging features; diagnostics go to
  stderr. `notifications/cancelled` is accepted and logged; nothing is cancellable yet, so
  the reply still arrives (a supervised build ends at `VORPAL_MCP_BUILD_TIMEOUT_S`).
- **Framing**: a JSON array (batch) or a non-object message is `-32600`; a request id must
  be a string or an integer; messages without an id are notifications and are never
  answered.
- **Errors**: an unknown tool name, a missing tool `name`, non-object `arguments`, or a
  `tools/list` cursor (the list is one page and issues none) is a protocol error
  (`-32602`). Everything that goes wrong *inside* a tool is an in-band result with
  `isError: true`, a message in `content`, and a stable `code` in `structuredContent`:
  `bad-argument`, `bad-query`, `index-unavailable`, `no-watch`, `stale-source`,
  `internal`, `tool-error`.

### Tool declarations and results

Every tool declares a `title`, `annotations` (`readOnlyHint` is true for everything but
`index`; `destructiveHint` false, `idempotentHint` true, `openWorldHint` false), an
`inputSchema`, and an `outputSchema` for its `structuredContent`. Results always carry
both a text rendering in `content` and `structuredContent`:

- `generation`: the content id of the index generation the answer was read from (`null`
  before any graph is loaded, e.g. `ast_dump`), so ids and spans are attributable to one
  index state.
- Record-bearing tools (everything that lists results) page deterministically:
  `outcome`, `records`, `total`, `truncated`, and `nextCursor` when more remain. Pass
  `cursor` (opaque, from a previous `nextCursor`) and `limit` (default 100, max 1000) as
  arguments. Every page carries `base`, the absolute directory prefix its records share,
  and each record's `path` is relative to it (one prefix instead of one per row).
- `graph` rows for `callers`, `references`, `importers`, `implementors`, and `type_users`
  carry `site_line` and `site`: the line number and text of the first retained occurrence
  of the edge in the caller's own source, read from the generation's pack. "Who calls X"
  needs no follow-up `snippet`.
- Tool descriptions on the wire are deliberately terse: a client either loads a schema in
  a model turn or carries the whole listing in every turn, so the listing is kept under
  10 KB (a test enforces it). The prose is here.
- `format: "lean" | "toon" | "ids"` shapes both the text and `structuredContent`,
  because clients such as Claude Code hand the model the structured half. `lean` keeps
  identity and ranking columns (`name`, `kind`, `path`, `id`, `grade`, …) and drops
  `signature`, `span`, and `external_id`; `ids` keeps `id` and `external_id` only;
  `toon` is a lossless tab grid grouped by directory with every column intact.

## Claude Code defers MCP tool schemas

Claude Code keeps only the *names* of MCP tools in context by default and loads a tool's
schema through its own `ToolSearch` tool the first time the model wants it. That load is
a model turn: every distinct vorpal tool a task needs costs one extra round trip before
the first real call, whatever the tool count (measured 2026-09-04: a server with one tool
still pays it). Three things reduce that cost:

- **Fewer distinct tools per task.** The seven relation tools are now one `graph` tool
  with a `relation` argument, so callers, references, importers, implementors, and the
  rest share a single schema load.
- **One load, not one per turn.** The server's instructions tell the model to load every
  vorpal tool it will need in a single `ToolSearch` call.
- **The server hands the model a schema-free path.** Its `instructions` (which every
  client shows the model once, in context) end with the exact CLI one-liner for this
  daemon's own index: `<vorpal> graph callers <name> --index <abs index> --format lean`,
  with `callees` in the verb position for what a symbol calls (both carry call sites). A
  client's shell tool is never deferred, so a single lookup that way is two model turns
  (one command, one answer) instead of three. Measured 2026-09-04 on Opus: callers of
  `vfs_read` in the kernel, 2 turns, 37 K tokens, 8.6 s; through the MCP tool, 3 turns,
  52 K, 7.3 s; through grep, 3 turns, 62 K, 11.8 s. The note tells the model to prefer
  the shell for one lookup and the MCP tools when it will make several calls.
- **Keeping the schemas resident is possible but not recommended.** Claude Code's
  `ENABLE_TOOL_SEARCH=false` puts MCP schemas in context up front and removes the
  `ToolSearch` turn, but it also makes Claude Code's own deferred tools resident: measured
  2026-09-04, a turn costs about 20 K tokens deferred and about 41 K resident, of which
  vorpal's listing is about 4.5 K. One schema load per distinct tool is cheaper than that
  on every question measured. `auto` and `auto:N` are an optimistic prefetch that sometimes
  defers and sometimes does not. Leave the default unless wall time matters more than
  tokens.

## Tool profiles (least privilege)

Agents don't always deserve the whole surface. `--profile` gates what `tools/list`
offers:

| profile | tools |
|---|---|
| `scout` | `node`, `search`, `snippet`, `schema`, `fetch_span` — read-only navigation |
| `analysis` | scout + `graph`, `reachable`, `why`, `health`, `dead_code`, `coverage`, `impact`, `compare_generations`, `architecture`, `code_search`, `data_flow`, `observed`, `query` |
| `full` (default) | everything: analysis + `index`, `structural_search`, `rule_search`, `ast_dump` |

```json
{ "mcpServers": { "vorpal": { "command": "vorpal", "args": ["mcp", "--profile", "analysis"] } } }
```

## One daemon, many projects

Enroll source roots once, then serve them all from a single server entry:

```sh
vorpal mcp allow ~/src/app --name app   # registry: ~/.config/vorpal/projects.yml
vorpal mcp allow ~/src/lib              # name defaults to the directory name
vorpal mcp projects                     # list enrollments
vorpal mcp deny lib                     # remove one
```

```json
{ "mcpServers": { "vorpal": { "command": "vorpal", "args": ["mcp", "--projects"] } } }
```

In `--projects` mode every tool takes a `project` argument (optional when one project is
enrolled), and `list_projects` enumerates the enrollments. Only enrolled roots are servable — the registry is the
allow-list (`VORPAL_PROJECTS_FILE` overrides its path).

## The tools

**Build & health**
| Tool | What it does |
|---|---|
| `index` | Build or refresh the graph index from a source directory (near-instant when unchanged), then hold it warm. Accepts `parse_health: warn\|exclude\|fail`, `max_error_ratio`, and `semantic_tier: lexical\|learned` policy. |
| `health` | Per-file parse damage: ERROR-node counts, affected-byte ratios, and which definitions overlap damaged regions — the difference between "no edge" and "unknowable here." |
| `coverage` | Per-file parse-coverage overview (error bytes/ratio), worst first. |
| `schema` | What this index contains: kinds, relations, resolution grades, and tier state, with counts — the ground truth for writing `query` patterns. |

**Repo shape & planning**
| Tool | What it does |
|---|---|
| `architecture` | Orientation summary: module mass, hubs by in-degree, entry-point candidates. |
| `impact` | Blast radius of changed files: git-diff-seeded transitive inbound closure (`since` a ref, or uncommitted changes). |
| `dead_code` | Definitions with no semantic in-edges anywhere (suppression-honest dead-code leads; `prefix`/`exported`/`exclude_tests` refinements). |
| `compare_generations` | What changed between two index generations: files, nodes by durable eid, edge counts. |

**Graph navigation** (all take a symbol `name`; ambiguous names list candidates)

> **Route nodes.** HTTP route registrations are first-class `Route` nodes named
> `VERB /path` (`GET /users/:id`, `ROUTE /x` when the verb isn't in the source), extracted
> for Express/Koa/Fastify, NestJS decorators, Flask/FastAPI, Django `urlpatterns`,
> Go `net/http`/gin/echo/chi (Go 1.22 `"GET /x"` patterns included), axum,
> actix-web/Rocket attributes, Spring, ASP.NET attributes, and Rails/Sinatra. A route
> `calls` its handler, so `callers <handler>` names the endpoint, `reachable` from a route
> walks its implementation, and `dead_code` never flags handlers. HTTP client call sites
> with literal URLs (`fetch("/api/users")`, `requests.get(url)`, `http.NewRequest`)
> gain a directional `requests` edge to the route their path uniquely matches — template
> parameters absorb segments, cross-language (a TS frontend into a Go backend is one
> graph), ambiguity refuses and is counted. Event listeners (`bus.on("user.created", h)`,
> `Subscribe`) are `Channel` nodes (`EVENT user.created`) that `call` their handlers, and
> emitters (`emit`, `publish`) gain `notifies` edges to EVERY matching registration —
> pub/sub fan-out is the semantics, capped and counted. Literal strings only; a URL,
> route, or topic built from variables is not extracted (nothing is guessed).
| Tool | What it does |
|---|---|
| `node` | Nodes matching an exact symbol name. |
| `graph` | The direct neighbours of a symbol over one `relation`: `callers` (incoming `calls`, each row with the call-site line in the caller), `callees` (outgoing `calls` — what the symbol calls — each row with the call-site line inside the symbol's own body), `references`, `importers` (files importing it), `implementors` (types implementing/extending a trait, interface, or base type), `type_users` (definitions using a type in fields, params, returns, or annotations), `similar` (near-clones from extraction-time MinHash sketches, ≥ 0.7 estimated Jaccard, confidence = similarity × 100, 8 partners kept per definition, nothing under 32 tokens signed), `observed` (runtime-observed calls from traces ingested with `vorpal-index ingest-traces <index> <folded-stacks>`, each row flagged with whether the static graph has the edge; a rebuild invalidates the sidecar until traces are re-ingested). The result is the complete set of resolved edges at the stated grade and needs no confirmation by search. |
| `reachable` | Transitive traversal from a symbol — `direction: "in"` (everything reaching it) or `"out"` (everything it reaches), with the path back to the seed. Restrict edge types with `relations` (default `["calls"]`; add `"data_flows"` to follow argument flow, `"changes_with"` for git co-change, `"similar_to"` for near-clones). |
| `data_flow` | Where a symbol's arguments flow: per-argument rows (`arg#i` → callee `param#j`, with the argument expression when traceable) joined from the `dataflow.bin` sidecar. Captured for Rust/Python/TypeScript/TSX call sites; older generations without the sidecar answer empty. |
| `query` | Cypher-shaped read-only queries (openCypher read subset): `MATCH (f:Function)-[:calls]->(g) WITH g, count(*) AS n WHERE n >= 20 AND NOT EXISTS { (g)-[:calls]->() } RETURN g.name, n ORDER BY n DESC LIMIT 20`. Linear patterns up to 8 segments with var-length paths and grade floors; `WHERE` trees with `=~`, `IN`, `IS NULL`, `n:Label`, `EXISTS {…}`; `WITH`/`UNWIND` stages; `RETURN [DISTINCT]` of expressions — properties, arithmetic, string/list functions, `CASE`, `count/sum/avg/min/max/collect` with implicit grouping; `ORDER BY`/`SKIP`/`LIMIT`; `UNION [ALL]`. Runs under explicit work ceilings (16KiB text, depth 10, 5M edge visits, 100k rows) and refuses by naming the ceiling instead of truncating. Not supported, by name: `OPTIONAL MATCH`, a second `MATCH`, `XOR`, map literals, path/relationship variables. |

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
| `snippet` | The defining source of a symbol by name (digest-verified slice of its indexed span; `context_lines`, `max_bytes`). |
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

Multi-project mode (`vorpal mcp --projects`) supports custom languages too: at launch the
CLI union-registers every enrolled project's dynamic grammars (still one startup-only
registration) and hands each project its own extraction environment, so a language declared
by one project is never *walked* in another. Conflicts refuse loudly at launch — one
definition per language name, one owner per extension, and no shadowing of builtin
extensions; a project declaring `languageGlobs` (which rebind builtin routing process-wide)
must run as a single-project daemon.

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

Two rules keep the served graph truthful when the tree and the index move independently:

- **"Unchanged" is measured against what is served, never against what is on disk.** When
  a save re-extracts byte-identical to the product the served graph was built from, the
  daemon answers as is and only canonicalizes the file's stamps in the background. That
  comparison uses the daemon's own retained products, not the committed generation's pack:
  a generation can be committed from a tree that had already moved on (the background
  canonicalizer's own read, or a `vorpal index` run beside the daemon), and measuring
  against it would let a real edit pass as "unchanged" and the pre-edit graph serve
  indefinitely. (Fixed 2026-09-04; the regression tests are
  `crates/mcp/tests/watch.rs` and the `live_differential` oracle.)
- **A generation committed behind the daemon's back is adopted.** If `CURRENT` names a
  generation none of the daemon's own committers wrote — an external `vorpal index`, a
  second daemon on the same tree — the next dirty or backstop pass loads it and rebuilds
  the retained tiers from it. Running `vorpal index` beside a live daemon is therefore
  always safe: the daemon converges on the newest committed generation, then keeps
  absorbing edits from there.

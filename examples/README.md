# vorpal examples

Runnable, self-contained examples for every way into vorpal. Each file states its own
prerequisites at the top.

## CLI (`examples/cli/`)

| script | shows |
|---|---|
| `01-structural-search-and-rewrite.sh` | pattern match, captures, rewrite, kind selectors, `--debug-query` |
| `02-index-and-explore.sh` | index a repo, schema, architecture, semantic search, graph verbs |
| `03-impact-and-dead-code.sh` | branch blast-radius, dead-code candidates, parse coverage |
| `04-cypher-queries.sh` | `vorpal query` Cypher patterns, `ids`/`toon` output for scripting and LLMs |
| `05-rules-and-ci.sh` | YAML rule authoring, GitHub/SARIF output for CI |
| `06-semantic-tuning.sh` | `vorpal tune` on your own queries, encoder enablement, `--ranked` |

All scripts are safe to run as-is (rewrites happen in temp dirs; repo scripts default
to the current directory).

## Python SDK (`examples/python/`) — `pip install vorpal-py`

| file | shows |
|---|---|
| `01_parse_and_rewrite.py` | `SgRoot` AST API: patterns, kinds, rule objects, edits |
| `02_index_and_search.py` | `index_build(_report)`, `index_search(_ranked)` |
| `03_graph_walk.py` | `index_graph` relation verbs with CLI-identical semantics |
| `04_async_pipeline.py` | the async bridge: `build`, `search`, concurrent `search_many`, `graph` |

## Node SDK (`examples/node/`) — `npm install @hyper-light/vorpal-node`

| file | shows |
|---|---|
| `01-parse-and-rewrite.mjs` | `parse`/`Lang`, findAll, captures, `commitEdits` |
| `02-index-and-search.mjs` | `indexBuild(Report)`, `indexSearch(Ranked)` |
| `03-graph-walk.mjs` | the session-pinned `Index` class: `nodes`, `related`, `reachable`, `why` |
| `04-find-in-files.mjs` | streaming repo-wide structural search without an index |

## WASM (`examples/wasm/`) — `npm install @hyper-light/vorpal-wasm`

A single-file browser playground (`index.html`) plus a README covering engine boot,
grammar registration, and bundler use. The wasm package is the structural matcher;
indexing and graph queries stay native (CLI / MCP / Node / Python).

## Where the deeper docs live

- CLI walkthrough: `docs/getting-started.md` · languages: `docs/LANGUAGES.md`
- MCP integration (agents): `docs/mcp.md`
- Python API: `docs/python.md` · TypeScript/Node API: `docs/typescript.md`
- Claude Code skills for all of this ship in `.claude/skills/` — open this repo in
  Claude Code and ask it to search, scan, or explore the graph.

---
name: vorpal-mcp
description: Run and configure vorpal's MCP server — stdio serving, client config installation (vorpal mcp install), tool profiles (scout/analysis/full), the multi-project daemon with the allow/deny registry, and watch-driven index freshness. Use when wiring vorpal into Claude Code, Claude Desktop, or any MCP client.
---

# Serving the knowledge graph over MCP

```
vorpal mcp [--index DIR] [--profile scout|analysis|full] [--projects] [--no-watch-rebuild]
```

Serves over **stdio**. On start it holds the index warm, watches the source tree, and
proactively rebuilds when the tree goes quiet — clients never re-index by hand
(`--no-watch-rebuild` switches to lazy refresh on first query after a change).

## Fastest correct setup

```
vorpal mcp install        # writes this machine's MCP client configs (idempotent, backups taken)
```

Or by hand, e.g. Claude Code `.mcp.json` at a repo root:

```json
{ "mcpServers": { "vorpal": { "command": "vorpal", "args": ["mcp"] } } }
```

Run from the repo root so the default `./.vorpal/index` resolves; pass
`--index /abs/path` otherwise.

## Profiles (least privilege for agents)

| profile | tools |
|---|---|
| `scout` | node, search, snippet, schema, fetch_span — read-only navigation |
| `analysis` | scout + graph, reachable, why, health, dead_code, coverage, impact, compare_generations, architecture, code_search, data_flow, query |
| `full` (default) | everything: analysis + index, structural_search, rule_search, ast_dump |

## Multi-project daemon

```
vorpal mcp allow ~/src/app --name app     # enroll (registry: ~/.config/vorpal/projects.yml)
vorpal mcp allow ~/src/lib
vorpal mcp projects                       # list enrolled
vorpal mcp deny lib                       # remove
vorpal mcp --projects                     # ONE daemon serves every enrolled project
```

In `--projects` mode tools take a project selector and `list_projects` enumerates
enrollments. `VORPAL_PROJECTS_FILE` overrides the registry path.

## The tools, briefly

Navigation: `node`, `search`, `snippet`, `fetch_span`, `schema`. Relations: `graph`
(`relation`: callers, callees, references, importers, implementors, type_users, similar,
observed; callers and callees rows carry the call-site line), `reachable`, `data_flow`,
`why` (edge evidence). Graph answers are complete
at the stated grade; they need no confirmation by search or grep. Repo health/planning: `health`,
`coverage`, `dead_code`, `impact`, `architecture`, `compare_generations`. Structural:
`structural_search` (pattern), `rule_search` (full YAML rules, dry-run fixes),
`ast_dump` (parse tree for rule authoring). Graph queries: `query` (Cypher-shaped).
Maintenance: `index` (with `parse_health`/`max_error_ratio`/`semantic_tier` policy).

Full integration guide with per-tool arguments: `docs/MCP.md`.

---
name: vorpal-graph
description: Query the vorpal knowledge graph from the CLI — callers, references, importers, implementors, type users, reachability, snippets, dead code, impact analysis, architecture summaries, data flow, near-clones, and generation diffs. Use to answer "who calls / uses / depends on X", "what breaks if I change this", and "what does this codebase look like".
---

# Graph queries

```
vorpal graph <VERB> [NAME] [--index DIR] [filters...]
```

Requires an index (vorpal-index skill). Default index: `./.vorpal/index`.

## The verbs

| verb | answers |
|---|---|
| `node NAME` | definitions matching a name (with ids, kinds, spans); `--pattern REGEX` lists by regex instead |
| `callers NAME` | direct callers (incoming `calls` edges) |
| `refs NAME` | direct referrers (incoming `references` edges) |
| `importers NAME` | files importing the symbol |
| `implementors NAME` | types implementing/extending it |
| `typeusers NAME` | definitions using it as a type |
| `reachable NAME` | transitive traversal, each reached node with its path to the seed |
| `snippet NAME` | the defining source, sliced from the indexed span, digest-verified (`--context N`, `--max-bytes B`) |
| `flows NAME` | outgoing data-flow rows: which arguments flow into which callees |
| `similar NAME` | near-clones (`similar_to` edges; confidence = estimated similarity) |
| `observed NAME` | runtime-observed calls from ingested traces, both directions |
| `schema` | what this index contains: kinds, relations, grades, tier state, with counts |
| `dead` | definitions with no semantic in-edges anywhere (`--prefix`, `--exported`, `--no-tests`) |
| `coverage` | per-file parse damage, worst first |
| `impact` | blast radius of changed files: git-diff-seeded transitive inbound closure (`--since REF`, `--src DIR`) |
| `diff` | what changed between two generations (`--from prev --to CURRENT`, content ids or paths) |
| `architecture` | orientation: module mass, hubs by in-degree, entry-point candidates (`--top N`) |

## Disambiguation and refinement

Same-named definitions list as candidates. Narrow with:
- `--path SUFFIX` (file path ends with), `--kind function|method|struct|field|…`
- `--id N` — exactly one node (ids shown by `node` and ambiguity listings; stable within a generation)
- `--ids` — append node ids to results, for chaining
- `reachable` extras: `--depth N`, `--min-grade exact|constrained|heuristic` (edge resolution quality floor)

## Recipes

- Orient in an unknown repo: `vorpal graph architecture --top 15`
- Change-safety check before a refactor: `vorpal graph impact --since origin/main`
- Everything that can reach a hot function: `vorpal graph reachable handle_request --depth 3`
- Prune candidates: `vorpal graph dead --exported --no-tests --prefix src/`
- Read a definition without opening the file: `vorpal graph snippet parse_config --context 4`
- Why is this edge believed? Use the MCP `why` tool or `vorpal query` for evidence detail.

## Output

Text is byte-stable; `--format` offers text and paged records (plus `toon`/`lean`
token-lean tabular profiles and `ids` on supporting verbs) for scripts and agents.

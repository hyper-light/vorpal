---
name: vorpal-query
description: Cypher-shaped read-only queries over the vorpal knowledge graph (vorpal query) — MATCH patterns over labeled nodes and typed edges with variable-length paths, property filters, RETURN projections and LIMIT. Use when graph verbs are too coarse and you need precise multi-hop questions.
---

# Cypher-shaped graph queries

```
vorpal query 'MATCH (f:Function)-[:calls*1..3]->(g {name: "resolve_target"}) RETURN f.name LIMIT 20' [--index DIR] [--format text|json|toon|lean|ids]
```

Read-only. Runs against the current generation of the index (default `./.vorpal/index`).

## Shape

- Node patterns: `(x)`, `(x:Function)`, `(x {name: "foo"})` — label = symbol kind
  (Function, Method, Struct, Field, …; check `vorpal graph schema` for this index's kinds).
- Edge patterns: `-[:calls]->`, `-[:references]->`, `-[:imports]->`, `-[:implements]->`,
  `-[:of_type]->`, `-[:similar_to]->` … (relations listed by `graph schema`).
- Variable-length: `-[:calls*1..3]->`.
- `RETURN` projects node properties (`f.name`, `f.path`, …); `LIMIT N` caps rows.

## Formats

- `text` (default) — table.
- `json` — the QueryResult document (stable machine shape).
- `toon` — token-oriented columnar text: columns declared once, directories grouped
  (built for LLM context economy).
- `lean` — LEAN tabular profile, the leanest measured page format.
- `ids` — one durable id per line (eid, falling back to dense id) for piping into
  `vorpal graph … --id` or the MCP tools.

## Recipes

- Call chains into a sink:
  `vorpal query 'MATCH (f:Function)-[:calls*1..2]->(g {name: "kfree"}) RETURN f.name, f.path LIMIT 30'`
- Who implements an interface and where:
  `vorpal query 'MATCH (t)-[:implements]->(i {name: "Iterator"}) RETURN t.name, t.path'`
- Feed ids to another tool:
  `vorpal query '…' --format ids | while read id; do vorpal graph snippet --id "$id"; done`

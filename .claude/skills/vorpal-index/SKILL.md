---
name: vorpal-index
description: Build or refresh a vorpal knowledge-graph index for a directory — the foundation every graph/search/query command reads. Use before any vorpal graph/search/query work, or when results look stale.
---

# Building and refreshing the index

```
vorpal index [SRC] [--out DIR] [--verify]
```

- `SRC` defaults to `.`; the index defaults to `<src>/.vorpal/index`.
- Re-running is **incremental and cheap**: unchanged trees are a no-op (sub-second),
  single-file edits take well under a second on multi-million-line trees. Never fear
  re-running it — it is the "make sure everything is fresh" button.
- `--verify` re-reads every candidate file instead of trusting size+mtime — use after
  operations that preserve mtimes (some checkouts, `rsync -t`, build systems that touch
  files back), or when you suspect a stale replay.

## What one build produces

A content-addressed **generation** under `<index>/gen/<content-id>/`, with `CURRENT`
naming the live one. Identical trees always produce byte-identical generations — if two
builds disagree, the tree differed. The index carries the full relation set: calls,
references, imports, implementations, type usage, data flow, near-clone pairs,
request→route links, and co-change history — not just a symbol table.

## Parse health

Files whose parse produced ERROR nodes still index (spans outside errors are extracted).
Inspect damage with `vorpal graph coverage --index <dir>` (worst files first). The MCP
`index` tool additionally accepts `parse_health: warn|exclude|fail` and
`max_error_ratio` when you need policy enforcement.

## Recipes

- Fresh index for the current repo: `vorpal index`
- Index another tree into a chosen location: `vorpal index ~/src/linux --out /tmp/kidx`
- Paranoid rebuild after suspicious replays: `vorpal index --verify`
- What did the build see? `vorpal graph schema --index <dir>` — kinds, relations,
  grades, tier state, with counts.

## Pitfalls

- Point `--index` of downstream commands at the SAME `--out` you built (they default to
  `./.vorpal/index`, matching a default build from the repo root).
- The previous generation is kept until the next commit then swept — disk spikes briefly
  to two generations.
- Don't hand-edit anything under the index directory; every artifact is digest-checked.

---
name: vorpal-outline
description: Explore code structure with vorpal outline — symbols, members, imports/exports per file or directory, filterable by item type and regex, with JSON output. Use for a fast structural map of unfamiliar files without an index.
---

# Code structure at a glance

```
vorpal outline src/lib.rs
vorpal outline src/ --items exports --type function,struct
vorpal outline --stdin -l python < snippet.py
```

Works WITHOUT an index — it parses on the fly (all 49 grammars).

- `--items structure|exports|imports|all` — which top-level view (auto-picked by default).
- `--type T[,T...]` — keep only these symbol types (function, struct, class, …).
- `--match REGEX` — keep only items whose name matches.
- `--pub-members` / member controls — how deep into type members to go.
- `--json[=pretty|stream|compact]` — machine output; `--lang` forces the parser
  (required for `--stdin`).

## When to prefer which structure tool

- One file / quick map, no index: `outline`.
- Whole-repo symbol questions, relations, importance: index once, then `vorpal graph`
  (architecture, node, snippet) — see vorpal-graph.
- Members and signatures for an LLM prompt: `outline --json=compact` is token-cheap.

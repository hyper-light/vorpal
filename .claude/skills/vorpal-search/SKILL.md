---
name: vorpal-search
description: Semantic + structural search over an indexed codebase (vorpal search) — hybrid free-text retrieval, phrase conjunctions, kind/lang/path filters, and ast-grep pattern mode ranked by graph importance. Use to find definitions by intent, not just by name.
---

# Searching an indexed codebase

Requires an index (see the vorpal-index skill). Default index: `./.vorpal/index`;
override with `--index DIR`.

## Free-text (hybrid) mode

```
vorpal search "retry logic for connection pools" -k 10
```

Fuses exact/token name matches, lexical-embedding similarity, and graph in-degree
(RRF). Results are definitions (functions, methods, types…), each with its file:line.

- **Phrase conjunction**: two or more double-quoted phrases joined by literal `AND`
  intersect their result sets:
  `vorpal search '"retry logic" AND "connection pool"'`
- `-k N` — result count (default 10).

## Filters (compose freely)

| flag | effect |
|---|---|
| `--prefix src/net/` | path must start with prefix (package scoping) |
| `--path .rs` | path must end with suffix |
| `--kind function` | one symbol kind (function, method, struct, field, …) |
| `--lang rust` | language name or alias (rust, py, ts, …) |
| `--exported` | only exported definitions |
| `--no-tests` | exclude test-classified paths |
| `--within PREFIX` | search inside this path only (repeatable; relative to the indexed tree) |
| `--except PREFIX` | leave this path out (repeatable) |
| `--class source\|test\|vendored\|generated` | files of this class only (repeatable) |
| `--changed-since REF` | files changed since a git ref only (`worktree` = uncommitted) |

`--within` and friends generate the candidates inside the filter, so `-k 8 --within
fs/ext4` returns eight ext4 definitions; `--prefix` ranks the whole tree first and keeps
what falls under the path, which can starve on a small directory. Prefer `--within`.
Both modes take them, `--code` included:

```
vorpal search "read file into user buffer" -k 5 --within fs/ext4
vorpal search --code 'kmalloc($A, $B)' --lang c --within fs --except fs/ext4
```

## Structural mode (`--code`)

```
vorpal search --code 'kfree($A);' --lang c -k 20
```

Treats the query as an **ast-grep pattern**, runs it over the generation's own
digest-verified files, and ranks the *enclosing definitions* by semantic in-degree —
"who matches this shape, most-depended-on first". C/C++ call patterns need statement
form (`kfree($A);`) — bare calls parse as declarations.

## Reranked view (`--ranked`)

With the advanced embedder enabled (see the vorpal-semantic skill), `--ranked` shows
the base fused ordering and the encoder-reranked ordering side by side — one search,
two views (text output only).

## Output

Default text output is byte-stable (safe to diff in scripts); `--format` also offers a
paged records envelope for machine consumption (`--cursor` continues a page).

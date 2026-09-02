---
name: vorpal-semantic
description: Manage vorpal's semantic ranking stack — install/enable the advanced encoder (vorpal enable semantic-f32|f16), tune optional ranking features against your own queries (vorpal tune), and read reranked search output (--ranked). Use when search quality matters more than raw speed.
---

# The semantic ranking stack

Search always works without any of this (hybrid lexical retrieval is the default).
These commands add measured, opt-in ranking layers.

## Advanced encoder

```
vorpal enable semantic-f32    # pinned upstream weights, 547 MB download
vorpal enable semantic-f16    # locally converted, ~274 MB on disk
vorpal disable semantic-f32   # switch off; weights stay installed
```

- Enabling makes `vorpal search --ranked` show base fused vs encoder-reranked
  orderings side by side (one search, two views) and lets `vorpal tune` measure
  whether reranking helps YOUR queries.
- Per-index override: an `encoder.dir` switch inside the index (written by `tune`)
  scopes the encoder to that corpus instead of globally.

## Tuning to your queries

```
vorpal tune --queries q.txt --index .vorpal/index          # measure + write switches
vorpal tune --queries q.txt --dry-run                      # report only
```

`q.txt` — one query per line; label expected hits for scoring:

```
socket buffer allocation => alloc_skb
"retry logic" AND "connection pool"
parse configuration file => parse_config
```

Only labelled lines score (reciprocal rank of the expected hit; both rankings compared
from one search each). Verdicts enable/disable the optional features (encoder rerank,
per-corpus BM25) for THIS index; the BM25 override holds until the index content
retrains. `-k N` controls hits examined per query.

## Guidance

- Small/typical corpora: skip all of this — the default fusion is strong.
- Large mixed corpora where naming is unreliable: enable an encoder, run `tune` with
  10–30 real queries you actually ask, and let the verdicts decide.
- CI/scripted environments: never enable globally; use per-index switches via `tune`.

# Adoption plan — what vorpal takes from the codebase-memory-mcp comparison

Source: full architecture review + head-to-head kernel benchmarks of
`../codebase-memory-mcp` (2026-08-29, see README.md#performance hardware; their repo @
`997d087b`). Where vorpal is ahead — cold index ~35×, incremental ~28×, no-change ~74×,
queries ~80–700×, index size 7.5× smaller, bit-reproducible output (theirs drifts across
identical runs) — nothing changes. This document is only the list worth taking, phrased in
vorpal's own architecture: evidence grades, content-addressed generations, the determinism
contract, and the selfcheck gate (`c34b53e`).

Also for the record, the query-surface axes where vorpal is already ahead and stays the
course: resolution-grade floors on traversal (`--min-grade`), per-edge evidence with
no-edge explanations (`why` explains absences, theirs only edges), durable external ids
across rebuilds, and fused hybrid retrieval (their three search modes never fuse).

Effort: S = day-scale, M = week-scale, L = multi-week, XL = quarter-scale.

## P0 — agent-surface quick wins (each S, all high leverage)

1. **`get_code_snippet` analog in MCP** — return the exact source span for a node
   id/eid. Spans are already in `graph.bin`; this is the tool that saves an agent a whole
   `Read` per lookup. Lands in `crates/mcp` + a `vorpal graph snippet` verb.
2. **`get_graph_schema` analog** — node kinds, edge types, resolution grades, counts for
   the open index. Pure introspection over existing headers; teaches agents (and Cypher-less
   humans) what is queryable before they guess.
3. **Dead-code verb** — definitions with zero incoming `calls`/`references`/`imports`,
   filtered by kind and path. One in-degree scan over the CSR we already build.
4. **Regex name matching on the graph** — `graph node` today is exact-name; add a pattern
   selector over `names.idx`. Steal their prefilter trick: extract literal substrings from
   the regex and cut the candidate set on the name index before the regex runs per row.
5. **Traversal ergonomics** — `reachable --direction both` in one call; cursor pagination
   on large results (a depth-3 kernel trace is 91 KB today); a test-path classifier so
   query surfaces can exclude or mark test files and demote vendored paths in ranking.
6. **Tool profiles** — named MCP tool subsets (their `analysis`/`scout`): a slim surface
   for read-only agents. Config plumbing only.
7. **Queryable coverage** (pass-1 item) — elevate `health` data to a first-class MCP tool +
   `graph`-verb filter; every tool description states absence ≠ completeness (their honest
   framing, worth copying verbatim as a norm).

## P1 — differentiating features on the existing graph (S–M each)

8. **`impact --since <ref>`** — git-diff-seeded multi-source reverse reachability
   (their `detect_changes`, our `reachable` machinery). Diff → changed files → definitions →
   transitive inbound closure with per-node hop distance and grade floor. The single most
   agent-valuable tool they have that we lack. (M)
9. **Generation diff (`compare_graphs`, done right)** — we have content-addressed
   immutable generations; diffing two generation ids (nodes/edges added/removed/moved, by
   kind) is natural for us and impossible to do reproducibly for them. Pairs with #8. (M)
10. **Graph-fused text search** — attribute `run`/`scan` pattern hits to their enclosing
    definitions (spans → `SpanCursor` logic already exists) and rank by graph in-degree.
    Their `search_code` shells out to `grep`; ours would be in-process and structural. (M)
11. **`get_architecture` analog** — a computed orientation summary: top modules by
    definition mass and in-degree, entry points, import-layering, hotspot list. Derived
    analytics over existing edges; enormously useful as an agent's first call. (M)
12. **TOON-style output mode** (pass-1 item) — columnar header-once encoding +
    prefix-grouped paths + `detail:ids` for MCP results. They claim 40–60% token reduction
    on homogeneous result sets; our current per-row output leaves that on the table. (S–M)
13. **Complexity/importance node properties** — in-degree importance, transitive loop
    depth, Halstead-lite as stored node properties, filterable in `graph`/search. Cheap
    passes at seal time; keep them deterministic. (S–M)

## P2 — grammar expansion (Waves 1 + 2)

Per-language cost in our discipline: vendor + `grammars/PROVENANCE.json` (license review —
several of their grammars are forks), `SupportLang` variant + feature flag + extension map,
outline rules YAML, ref spec in `crates/ingest/src/references.rs`, selfcheck canary,
upstream corpus tests. Rules + spec are the real work (~a day per language done properly).
Wave 1 costs roughly +25–40 MB binary on today's 47 MB — accepted: EVERY vorpal artifact
ships EVERY grammar (product policy, user-set 2026-08-29; no slim presets, ever).

- **Wave 1 (ubiquitous, low grammar risk):** TOML, Dockerfile, Make, CMake, SQL, XML,
  protobuf, INI/properties, GraphQL, Objective-C, Perl, Groovy (Gradle), Zig, Erlang,
  OCaml, R, Julia, PowerShell.
- **Wave 2 (injection-heavy frameworks):** Vue, Svelte, Astro (reuse the `Html` injection
  machinery), Jinja2, JSDoc.
- **Long-tail lever instead of Wave 3:** plumb `crates/dynamic` (`DynamicLang`, dylib +
  ABI checks — already shipped for pattern matching) into the index path: outline rules are
  already user-suppliable YAML; ref specs need a serialized (non-const) form. Then the tail
  loads at runtime with zero vendoring — a capability cbm does not have. (L, but it
  replaces ~127 vendored grammars' worth of maintenance.)

Context: their "162 languages" is ~35 scored languages (their own quality matrix; OCaml 72%,
Haskell 62%) plus a parse-only tail. Depth beats the badge; Waves 1–2 close the gap that
actually matters.

## P3 — pipeline, ops, and distribution (M–L each)

14. **Filesystem watcher for the MCP daemon** — event-driven re-index instead of
    on-demand. Our no-change path is 0.13 s at kernel scale, so a watcher makes the daemon
    effectively always-fresh for near-zero cost. (M)
15. **Shareable index artifact** — export/import a generation (already immutable,
    content-addressed, bit-reproducible — the hard part exists). CI builds the index once;
    teammates and agents fetch it. Their `graph.db.zst` proves demand; our determinism makes
    it verifiable (id = content). Feeds directly into remote-fleet R2 index-merge plans. (M)
16. **Supervised index worker** (pass-1 item) — fork+exec the build under the daemon with
    quarantine escalation and crash-durable startup header, so one pathological file cannot
    take the server down. (M)
17. **Superlinearity detector** (pass-1 item) — fit `T ~ n^k` at 1/8, 1/4, 1/2, 1 of each
    phase's items; WARN on `k` past threshold in release builds. Complements
    `VORPAL_PHASE_TRACE`; their honest documentation of the blind spot is part of what to
    copy. (S)
18. **Multi-project registry for MCP** — optional central cache dir + `list/delete`
    project tools so one daemon serves many repos (today: one index root per invocation).
    (M)
19. **Human-only root enrollment** (pass-1 item) — an `allow-root`-style gate for MCP
    indexing: enrollment only via a human-typed CLI command, never via the MCP surface
    ("a confirmation delivered through the MCP surface would be answered by the same agent
    that may have been influenced"). (S)
20. **Installer/client-config polish** — `vorpal mcp install` writing detected client
    configs (Claude Code, Cursor, etc.). Their installer covers 45 surfaces; even the top 5
    would remove our largest adoption friction. (M)
21. **Supply-chain posture** — signed releases via the existing `vorpal-sign`, SLSA
    provenance, scorecard. Directly resolves the open registry/signing decision
    (IMPROVEMENTS #12). (M)

## P4 — strategic bets (L–XL)

22. **In-process type resolution** — their "Hybrid LSP": per-language receiver-type
    inference (gopls package summaries, clangd `simplifyType`, rust-analyzer method
    resolution — reimplemented in-process, no LSP servers). The one axis where they beat us
    on edge *quality*. Vorpal shape: a per-language type-binding layer feeding the existing
    resolution grades, upgrading `heuristic` method edges toward `constrained`/`exact`,
    evidence rows intact. (XL; start with Rust + Python + TypeScript.)
23. **Data-flow edges + trace mode** — their `trace_path mode=data_flow` follows
    `CALLS`+`DATA_FLOWS` carrying the argument expression per hop. A genuinely distinct
    query class we lack entirely: static arg→param flow recorded at extraction, traversed
    like any relation, grades applied. (L; per-language flow rules, start where type
    resolution starts.)
24. **Structural query language** — their Cypher subset (5K-line hand-rolled
    lexer→planner→SQL) is real, agents speak it, and it is what closes the ad-hoc
    predicate/aggregation gap (`WHERE f.loop_depth >= 3 … COUNT/ORDER BY/var-length
    paths`). Options: a Cypher-shaped read-only subset compiled to CSR traversals, or a
    typed JSON query IR first (cheaper, less discoverable). Decide after P0 #2 ships schema
    introspection and P1 #13 lands the properties worth querying. (L–XL)
25. **HTTP route nodes + cross-service tracing** — route extraction per framework,
    `Route` nodes, cross-repo edges; unlocks their `cross_service` trace mode for
    multi-repo fleets (pairs with #15 and fleet R2). (L)
26. **Runtime-trace ingestion** — their `ingest_traces`: observed calls from
    profiles/traces recorded as a new evidence class (`observed`) alongside static grades —
    dynamic dispatch and fn-pointer edges (`tcp_v4_rcv`!) that static resolution can never
    prove. Fits our evidence-sidecar design exactly. (L)
27. **MinHash/LSH `SIMILAR_TO` edges + git co-change pass** — clone detection (K=64
    AST-trigram signatures, banded LSH, capped edges/node) and temporal coupling from
    history. Deterministic passes at seal time. (M each)
28. **IaC nodes** — k8s/kustomize/docker as `Resource`/`Module` nodes with `IMPORTS`
    edges to referenced resources; rides on Wave 1's Dockerfile + existing YAML. (M)
29. **Evaluation methodology** — their arXiv-published eval (31 repos: answer quality,
    token spend, tool-call counts vs file-by-file exploration) is why people believe their
    claims. Build the same harness for vorpal MCP; publish the numbers. (L)

## Explicitly not taking

- **SQLite as the store** — their 15 GB kernel DB vs our 2.0 GB warm generation and the
  query-latency gap close that argument. The direct B-tree page writer is admirable
  engineering; it optimizes a format we outgrew. An optional SQLite *export* could ride the
  P3 #15 artifact work if interop demand appears.
- **3D graph UI** — demo value, low agent value; revisit only with product demand.
- **ADR management in the index** — ADRs belong in the repo (docs/, versioned); a graph
  link is a later nicety.
- **Hop-distance "risk labels"** — a cosmetic variant of what `reachable` depths already
  express. (Multi-keyword semantic AND left this list 2026-08-30: taken after all, as the
  semantic-tier plan's Stage AND — quoted-phrase conjunction with min-of-RRF scoring.)
- **RAM-first marketing architecture** — their LZ4 + Aho-Corasick headline features are
  dead code in production; the lesson is the opposite one: keep README.md#performance
  command-reproducible, claims pinned to tests.

## Sequencing sketch

P0 in one sweep (a week-scale sweep total, mostly MCP surface). P1 #8+#9 next — they
compound (diff-seeded impact over generation diffs). Grammar Wave 1 can proceed in parallel
with P1 (independent code paths; selfcheck gates each new language). P3 #15 before fleet R2
lands. P4 #22 is the long pole worth starting early behind a feature flag, with #23
following it language-by-language; #29 should trail the first P0/P1 shipment so the eval
measures the improved surface.

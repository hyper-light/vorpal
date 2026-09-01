# Vorpal vs ast-grep: Current Comparison and Remaining Improvements

> **Assessment date:** 2026-07-29
>
> **Vorpal baseline:** `1ffc94b` plus the current worktree
>
> **ast-grep baseline:** v0.45.0 (`5d439d9`, released 2026-07-23)
>
> **Scope:** structural tooling, local indexing, graph construction, retrieval, MCP, bindings,
> grammar maintenance, and public product claims. `vorpal-wire` and transport-protocol behavior
> are deliberately excluded.

## Executive summary

Vorpal has moved materially beyond the state assessed on July 26. The following are now
implemented and should not remain described as open foundational gaps:

- the concrete ast-grep v0.45.0 behavioral changes have been reconciled;
- same-named callable overloads no longer collapse during graph ingest;
- graph traversal can be restricted by relation and depth;
- extraction products are keyed by grammar and extraction-rule content;
- index readers reject mismatched graph/node sets;
- index publication uses immutable, content-addressed generations and one atomic `CURRENT` swap;
- resolution quality has a shared `exact` / `constrained` / `heuristic` / `unresolved` vocabulary;
- resolver-created edge occurrences persist span, reason, confidence, and candidate-count evidence
  and can be explained through library, CLI, and MCP `why` queries;
- 32-bit storage ceilings are checked and documented instead of silently wrapping;
- parse health reports both affected files and tree-sitter `ERROR`-node counts; and
- README storage, MCP, cache, and scale descriptions have been brought closer to the code.

The current product split is:

- **ast-grep remains the stronger production structural-search, lint, and codemod product.** It
  owns the actively maintained matcher, has established packages, and is the compatibility
  reference for Vorpal's inherited structural surface.
- **Vorpal is already the stronger repository-intelligence substrate.** It persists extraction
  products, symbols, confidence-labelled relations, name indexes, content-addressed generations,
  and ANN data, then exposes them through CLI, MCP, Node, and Python surfaces.
- **Vorpal's remaining gaps are narrower but still important.** They are durable cross-generation
  symbol identity, resolver fidelity, graph-backed rules, complete structured and
  generation-bound APIs, explicit cache-validity modes, grammar supply-chain testing, and
  measured retrieval quality.

The recommended positioning is:

> **ast-grep-compatible structural tooling plus persistent, generation-consistent,
> confidence-labelled repository intelligence.**

Vorpal should still avoid “compiler-grade semantic analysis” and “learned semantic search” as
unqualified descriptions. “Billion-line scale” is also not demonstrated by one 32-bit index,
although the implementation now fails safely at its documented limits.

## Existing code comparison

| Area | ast-grep v0.45.0 | Vorpal today | Assessment |
|---|---|---|---|
| Structural matching | Mature patterns, metavariables, strictness, relational/composite YAML rules, transforms, rewrites, constraints, and language injection | Inherited engine with v0.45.0 `is_extra`, rule-directory ignore, and TypeScript ambient-module behavior adopted | The concrete current-release deltas are closed; systematic differential compatibility testing is not. |
| CLI workflows | `run`, `scan`, `test`, `new`, LSP, and outline | Inherited workflows plus index, graph, search, grammar, and MCP commands | Vorpal's additional repository workflows are real differentiation. |
| Language parsers | Upstream-managed tree-sitter language set | 28 vendored grammars with a runtime ledger and generation digests | Better control and offline reproducibility, but Vorpal owns update and corpus-testing risk. |
| Default outlines | Defaults for 13 languages in the v0.45.0 source tree | Defaults for all 28 supported languages, including the v0.45 TypeScript additions | Vorpal has broader coverage; precision still varies by language and relation. |
| Persistent state | Structural CLI scans are generally stateless; editor services keep process-local state | Manifest, canonical product pack, nodes, strings, names, graph, ANN base/overlay, and parse health | A clear Vorpal advantage. |
| Publication coherence | Not applicable to a persisted repository index | Immutable content-addressed `gen/<id>` directories, atomic `CURRENT`, pinned readers, legacy migration, and bounded GC | The former mixed-generation publication gap is closed. |
| Symbol identity | Syntax nodes are scoped to a parsed document | Overload-safe path/entity layout plus generation-local `NodeId` and public id/name/path/kind selectors | Ingest identity is much safer; durable external identity and owner/signature selectors remain absent. |
| Cross-file relations | No built-in persisted project graph | Containment, calls, references, imports, implements, and type relations with confidence, resolution grades, and persisted evidence for resolver-created occurrences | Useful and auditable repository intelligence, not compiler binding or type resolution. |
| Traversal | Syntax-tree relations within one parsed tree | Relation- and depth-restricted graph closure in the library and MCP | Core relation leakage is fixed; selector consistency, confidence filters, and path explanations remain. |
| Rule semantics | Syntax-tree and text relationships in the ast-grep rule model | Essentially the same; persisted graph facts cannot be used by YAML rules | Vorpal's graph still does not constrain structural rewrites. |
| Retrieval | Structural pattern/rule search | Exact/token name, lexical-hashing vector, graph prior, RRF, quantized ANN, overlay reconciliation, exact reranking, and generation pinning | Technically substantial, but still lexical rather than learned and not backed by a labelled quality suite. |
| MCP | Separate ast-grep MCP exposes AST dump, rule testing, simple structural find, and YAML-rule find | Built-in repository tools plus simple structural search, source-span fetch, and edge `why` | Complementary strengths; Vorpal's structural MCP contract is narrower and results remain mostly text-rendered. |
| Bindings | Mature matcher APIs and established Rust/Node/Python/WASM packages | Matcher APIs plus Node/Python repository build, search, graph, and node functions | Repository functions exist; only node lookup is strongly typed and calls are path/session-less. |
| Scale behavior | Parallel stateless structural scans | Checked 32-bit node/edge/name limits and a checked 4 GiB string heap per index | Safe and documented, but not a single-index billion-scale proof. |
| Distribution | Established releases through npm, PyPI, Cargo, Homebrew, and others | Release workflows exist; README still says registry publication is in progress | Vorpal has automation but not ast-grep's distribution maturity. |

## Status of the July 26 priorities

| Former priority | Current status | What remains |
|---|---|---|
| Reconcile ast-grep v0.45.0 | **Implemented for identified behavioral deltas** | Differential fixtures and the ledger's unaudited v0.44.1 non-outline patches. |
| Stop overload collapse | **Implemented** | Durable external identity and owner/signature selector facets. |
| Key cache on extraction behavior | **Implemented** | Default source validation still uses stat metadata outside the racy window unless verification is enabled. |
| Publish one coherent index generation | **Implemented** | Operational soak/crash testing; the architectural correctness mechanism is present. |
| Expose resolution quality | **Implemented** | Better binding inputs and measured grade precision. |
| Persist per-edge resolution evidence | **Implemented in the current worktree** | Alternative candidate identities, immutable/digest-verified source, and typed generation-pinned delivery. |
| Restrict graph traversal by relation | **Implemented** | Selector-aware MCP traversal, confidence filters, CLI traversal, and returned paths/evidence. |
| Correct storage/MCP/cache documentation | **Implemented for the identified drift** | Keep generated/status documentation synchronized as the generation layout evolves. |
| Fail safely at 32-bit limits | **Implemented** | Sharding or 64-bit formats only if measured workloads require them. |
| Report parse-error magnitude | **Implemented** | Byte coverage, representative spans, thresholds, and policies. |

## Priority 0: preserve trust in identity and compatibility

### 1. Add differential compatibility testing against ast-grep

Vorpal has adopted the concrete v0.45.0 changes previously missing:

- smart strictness uses `SgNode::is_extra`;
- configured rule and utility directories ignore parent ignore files;
- TypeScript and TSX outlines include namespace and ambient-module declarations; and
- CLI alias/toolchain changes are explicitly classified as intentional divergence.

That closes the known behavioral delta, but not the maintenance problem. `docs/UPSTREAM.md` still
records v0.44.1 non-outline patches as not audited case by case, and there is no automated
comparison with a pinned ast-grep binary.

Relevant code and status:

- [`docs/UPSTREAM.md`](UPSTREAM.md)
- [`crates/core/src/match_tree/strictness.rs`](../crates/core/src/match_tree/strictness.rs)
- [`crates/cli/src/config.rs`](../crates/cli/src/config.rs)
- [`crates/outline/src/default_rules/typescript.yml`](../crates/outline/src/default_rules/typescript.yml)

**Improve it**

- Pin one upstream binary/source commit in CI.
- Compare `run`, `scan`, `test`, outline, JSON output, edits, diagnostics, and exit codes.
- Version intentional differences as fixtures rather than prose exceptions.
- Finish the v0.44.1 non-outline audit so the ledger does not contain a historical blind spot.
- Require every later upstream release to be fully classified before the baseline advances.

**Done when:** the synchronization ledger and a differential test job account for every
behavioral change between the fork base and the declared upstream baseline.

### 2. Persist a durable external symbol identity

The overload-collapse bug is fixed. `layout_entity_paths` now adds kind/signature discrimination
for overloadable callables, and both graph writing and reference attribution use the same layout.
Tests prove three C++ overloads remain three nodes.

The remaining identity problem is lifecycle, not ingest deduplication:

- `NodeId` is exact only within one content-addressed index generation;
- the canonical key is not persisted as a public, resolvable external id;
- selectors cannot directly use owner or signature; and
- moves and renames have no explicit identity-transition contract.

Relevant code:

- [`crates/kg/src/writer.rs`](../crates/kg/src/writer.rs)
- [`crates/canonical/src/key.rs`](../crates/canonical/src/key.rs)
- [`crates/kg/src/kg.rs`](../crates/kg/src/kg.rs)
- [`crates/index/src/lib.rs`](../crates/index/src/lib.rs)

**Improve it**

- Persist a versioned external symbol id separately from dense generation-local ids.
- Define stability boundaries for unchanged rebuilds, edits, moves, renames, generated symbols,
  and indistinguishable same-signature declarations.
- Extend the shared selector with external id, owner, and signature.
- Return generation id and external id from every graph/search surface.
- Add migration/versioning rules before clients begin storing external ids.

**Done when:** a client can bookmark a symbol, rebuild the index, and either resolve the same
logical symbol or receive an explicit identity transition—not silently target a reused dense id.

### 3. Make cache validity modes explicit

Cache identity now includes:

- source digest in the product,
- grammar generation,
- extraction-rule content, and
- product format/schema version.

This closes the stale-product bug caused by changing outline rules. The remaining tradeoff is
source validation: the normal no-change and warm-product fast paths trust path/size/mtime outside
the racy window. `VORPAL_VERIFY_CACHE=1` makes content checking authoritative, but it is an
environment convention rather than a first-class correctness mode.

Relevant code:

- [`crates/ingest/src/product.rs`](../crates/ingest/src/product.rs)
- [`crates/ingest/src/manifest.rs`](../crates/ingest/src/manifest.rs)
- [`crates/ingest/src/outline_extractor.rs`](../crates/ingest/src/outline_extractor.rs)
- [`crates/index/src/lib.rs`](../crates/index/src/lib.rs)

**Improve it**

- Define named modes such as `fast-stat`, `verified`, and possibly filesystem-watcher-assisted.
- Make verified mode easy to select in CLI, CI, MCP, and bindings rather than only through an
  environment variable.
- State clearly that fast-stat can miss a same-size edit whose mtime is deliberately preserved.
- Report which validation mode and identity components produced each hit or miss.
- Add adversarial tests for preserved-mtime edits in both build and search-fed cache paths.

This need not force hashing every file by default. The gap is an explicit, testable product
contract for choosing speed versus content-authoritative validation.

## Priority 1: make repository semantics auditable

### 4. Improve resolver fidelity and complete the evidence contract

Resolution grades are now first class:

| Grade | Current meaning |
|---|---|
| `exact` | One same-file definition |
| `constrained` | One visible cross-file definition |
| `heuristic` | Deterministic best guess among ambiguous visible definitions |
| `unresolved` | No edge |

That is a major transparency improvement, but the underlying resolver is still a global
name-to-candidates table refined by path, export, owner/qualifier, and limited Rust/Java visibility.
It does not build complete lexical scopes, restrict every bare name through the import graph,
infer receiver types, or select overloads by argument/signature.

The edge-evidence persistence gap is now substantially closed in the current worktree:

- every resolver-created edge occurrence carries its source span, resolver reason, confidence,
  candidate count, base edge type, and endpoints;
- `evidence.bin` is canonical, memory-mapped, and included in the content-addressed generation
  identity;
- cold-opened graphs expose `edge_evidence` and `evidence_from`; and
- library/CLI/MCP `why` queries render the retained occurrences.

The sidecar round-trip and the end-to-end index explanation test pass. This does not make the
resolver compiler-grade, nor does it complete the public evidence contract:

- only the number of candidates is retained, not their identities or why alternatives lost;
- unresolved/masked references and structural edges such as containment have no evidence rows;
- the displayed snippet is read from the mutable current source path without checking its indexed
  digest;
- MCP `why` returns rendered text and reopens the index path rather than querying the server's
  already pinned `Kg`, so a concurrent `CURRENT` change can separate it from the preceding query;
  and
- the row does not record extraction-rule or language provenance directly.

Relevant code:

- [`crates/resolve/src/resolver.rs`](../crates/resolve/src/resolver.rs)
- [`crates/resolve/src/reference.rs`](../crates/resolve/src/reference.rs)
- [`crates/resolve/src/table.rs`](../crates/resolve/src/table.rs)
- [`crates/kg/src/evidence.rs`](../crates/kg/src/evidence.rs)
- [`crates/kg/src/kg.rs`](../crates/kg/src/kg.rs)
- [`crates/index/src/lib.rs`](../crates/index/src/lib.rs)
- [`crates/mcp/src/server.rs`](../crates/mcp/src/server.rs)

**Improve it**

- Build lexical-scope and file/import tables before global fallback.
- Preserve alternative candidate identities and unresolved/masked evidence rather than requiring
  every tolerated ambiguity to become one unexplained deterministic graph endpoint.
- Add language-specific alias, re-export, visibility, receiver-type, and overload resolution in
  measured increments.
- Return typed evidence bound to the selected generation and indexed source digest.
- Add extraction-rule and language provenance where it materially improves diagnosis.
- Evaluate precision/recall separately by language, relation, and grade.

**Done when:** every resolver-derived relation can answer “why this target and not the
alternatives?” against the exact indexed generation and source, and the published precision of
each grade is measured rather than inferred from its name.

### 5. Let structural rules consume graph facts

Vorpal persists repository facts, but inherited YAML rules still cannot ask whether two captures
resolve to the same binding, whether one symbol calls another, or whether an import crosses a
module boundary. Vorpal's graph therefore improves discovery but not structural rewrite safety.

**Improve it**

Start with a small, indexed predicate layer:

```yaml
all:
  - pattern: $OBJ.$METHOD($ARG)
  - graph:
      capture: $METHOD
      resolvesTo:
        externalId: "..."
      minimumGrade: exact
```

Initial predicates:

- `sameBinding`
- `resolvesTo`
- `calls`
- `imports`
- `implements`

Graph predicates must specify:

- required index generation and freshness;
- behavior when semantics are unavailable;
- accepted resolution grades;
- whether a heuristic edge is an error, non-match, or explicit candidate; and
- evidence returned with the match.

**Done when:** a rename/migration fixture rewrites only references proven to target the selected
symbol and returns auditable unresolved candidates.

### 6. Finish the traversal contract

The important correctness bug is closed: `reachable_via` and MCP `reachable` follow only requested
base edge types and respect `max_depth`. A calls traversal can no longer leak through containment
or imports.

Remaining gaps:

- MCP `reachable` still accepts only a name and unions all namesakes;
- it does not reuse the id/path/kind selector supported by direct graph tools;
- there is no minimum-confidence/grade filter;
- results are reached nodes, not full paths with per-edge type, grade, and evidence; and
- no equivalent `reachable` verb exists in the CLI.

**Improve it**

- Reuse the shared selector everywhere.
- Add minimum grade/confidence and path/language/kind boundaries.
- Return one or more bounded paths, not only the reached set.
- Add the same operation to Rust, CLI, MCP, Node, and Python contracts.

## Priority 2: complete public interfaces and evaluation

### 7. Make MCP structured and generation-safe

Vorpal MCP now includes index, selectors, direct graph queries, relation-specific traversal,
edge `why`, explained hybrid search, simple structural pattern search, and source-span fetch. It
is no longer only a graph-name lookup server.

The remaining gaps are:

- structural search accepts a single pattern, not the full YAML rule/constraint/rewrite model;
- there is no AST dump or isolated rule-testing tool comparable to ast-grep MCP;
- most results are rendered text rather than typed objects;
- large results have caps but no cursor/pagination contract;
- graph/search results do not consistently include generation and durable identity; and
- `fetch_span` and the snippet rendered by `why` reread the live path and apply persisted offsets
  without verifying the indexed content digest, so an edit can make the returned bytes
  inconsistent with the node;
- `why` reopens the index through its path instead of using the MCP server's pinned graph.

Relevant code:

- [`crates/mcp/src/server.rs`](../crates/mcp/src/server.rs)
- [`crates/mcp/src/tools.rs`](../crates/mcp/src/tools.rs)

**Improve it**

- Add AST inspection, YAML rule testing/search, strictness, captures, and dry-run rewrite diffs.
- Return typed matches, candidates, nodes, edges, traversal paths, grades, and generation ids.
- Add pagination, cancellation, declared truncation, and stable error codes.
- Verify source digest before span slicing or read immutable indexed source content.
- Pin one generation and reuse parsed documents across related tool calls.

### 8. Turn Node and Python functions into typed index sessions

Node and Python repository APIs already expose:

- `index_build`
- `index_search`
- `index_graph`
- `index_node`

This earlier gap is partially closed. `index_node` returns structured fields, while build, search,
and graph return CLI-formatted strings. Each call takes a path and opens or resolves the index
again; there is no session object that pins one generation across multiple operations.

**Improve it**

- Add `Index.open(...)` / context-manager objects with explicit generation lifetime.
- Return typed build reports, search hits, graph candidates/edges, and traversal paths.
- Share one schema across Rust, MCP, Node, and Python.
- Expose iterators/streams, cancellation, and pagination.
- Document stale-index, thread, process, and format compatibility behavior.

WASM repository parity should remain deliberate: it needs a credible browser storage/worker model
before a filesystem-shaped API is useful.

### 9. Make retrieval configurable and measurable

Vorpal's retrieval implementation is more mature than the old comparison credited:

- exact-name lookup has a persisted index;
- ANN data is persisted and generation-stamped;
- changed files use an overlay/remap path;
- fallback remains correct and triggers background warming;
- approximate candidates are reranked exactly against the current graph; and
- RRF provenance can be rendered.

Remaining gaps:

- the token/name channel still scans and tokenizes every node per query;
- common filters such as language, kind, package, path prefix, visibility, and grade are absent;
- production paths hard-code `LexicalEmbedder::default()` despite an `Embedder` trait;
- model id/dimensions/normalization/version are not a public configuration contract; and
- latency tests do not establish retrieval quality.

**Improve it**

- Persist token/posting indexes for the lexical channel.
- Add structured filters before ranking.
- Make embedder selection explicit and persist complete model provenance.
- Keep deterministic lexical hashing as the offline default; label learned adapters honestly.
- Build a labelled repository-query suite reporting recall@k, MRR, latency, index size, and update
  cost, with lexical/vector/graph/fusion ablations.

### 10. Test vendored grammars as a supply chain

All supported grammars are vendored and described in `docs/UPSTREAM.md`. Runtime grammar digests
and extraction-rule digests protect cached products from known parser/rule changes.

The repository currently contains upstream corpus files only for the locally patched Python
grammar. There is no CI job running a representative upstream corpus for every vendored grammar.
Structural grammar fingerprints can also miss behavior-only changes in generated actions or
external scanners.

**Improve it**

- Record repository URL, commit, complete source-tree digest, license, generator ABI, and patches
  per grammar.
- Import and run upstream corpora for every grammar.
- Automate reproducible grammar update PRs and generated-parser verification.
- Test injections, error recovery, `is_extra`, external scanners, and outline/relation fixtures.
- Separate parser support, outline support, and measured relation precision in the language matrix.

**Done when:** every vendored grammar update carries reproducible provenance and language-specific
test evidence.

### 11. Finish parse-health policies

Parse health now reports affected files and total tree-sitter `ERROR` nodes. That closes the
boolean-only observability gap.

Still missing:

- affected byte count or covered-byte ratio;
- representative error spans;
- parser/language/version context in query results;
- identification of graph entities built from unhealthy regions; and
- policies such as warn, exclude, or fail above a threshold.

These are lower priority than identity and evidence, but necessary before consumers treat missing
relations as meaningful absence.

## Priority 3: product maturity and conditional scale work

### 12. Complete release/distribution maturity

Vorpal has release workflows for npm and PyPI, but the README still describes registry publication
as in progress. ast-grep is already available through multiple established package managers.

**Improve it**

- Publish signed/versioned CLI and binding artifacts with a tested installation matrix.
- Define index-format compatibility and migration policy.
- Verify licenses and provenance for inherited and vendored components.
- Publish benchmark commands, datasets, hardware, cold/warm state, and raw results.

### 13. Treat larger-than-32-bit indexing as conditional work

The former correctness problem is closed: Vorpal now checks node and heap limits before sealing and
returns an actionable error instead of wrapping. README documents up to `2^32 - 1` definitions,
32-bit graph/name ids, a 4 GiB string heap, and saturated per-file byte spans.

Do not make 64-bit ids, sharding, distributed indexes, or new codecs an immediate priority without
evidence. First:

- publish corpus projections and actual peak counts;
- identify the first limit reached in real workloads;
- measure the memory/cache cost of widening ids; and
- design sharding only around observed query and update patterns.

This is a documented capacity boundary, not a current silent-corruption gap.

## Proposed features compared with ast-grep's public direction

Public issues show interest, not commitments or a roadmap.

| Proposed capability | ast-grep public state | Vorpal state | Recommended Vorpal decision |
|---|---|---|---|
| Semantic factory / richer semantic constraints | Open proposal [#2413](https://github.com/ast-grep/ast-grep/issues/2413) | Persisted graph, grades, and per-occurrence resolution evidence exist; rules cannot consume them | Build narrow graph predicates after durable identity and a generation-bound evidence API. |
| Cross-file/path guards | Open proposal [#2770](https://github.com/ast-grep/ast-grep/issues/2770) | Cross-file graph exists, but resolution remains heuristic and rule integration is absent | Differentiate with indexed predicates while clearly labelling resolution grade. |
| Per-rule performance reporting | Open proposal [#2800](https://github.com/ast-grep/ast-grep/issues/2800) | Product/search phase benchmarks exist; rule-level metrics are limited | Add extraction/rule timings and cache explanations before exotic storage work. |
| Bounded outline work | Upstream bounded queue/traversal work has shipped | Bounded outline queue and broad default rules are present | Keep covered by upstream differential tests. |
| Structural MCP | Separate [ast-grep MCP](https://github.com/ast-grep/ast-grep-mcp) provides AST/rule tools | Built-in MCP emphasizes persistent repository/graph operations plus simple patterns | Unify both workflows behind structured results rather than copying only tool names. |
| Learned retrieval adapters | No core ast-grep commitment identified | Trait exists; production default remains lexical hashing | Keep optional and evidence-driven; do not block core correctness on a model. |
| Distributed/billion-line indexing | No ast-grep product direction identified | Discussed architecturally; one checked 32-bit index is implemented | Defer until measurements show the documented ceiling is binding. |

## Recommended delivery order

1. **Compatibility and identity:** differential ast-grep fixtures, complete upstream ledger, and
   durable external symbol ids.
2. **Semantic trust:** scope/import-aware resolution plus typed, generation-bound evidence and
   alternative-candidate explanations.
3. **Semantic use:** graph-backed rule predicates and completed selector/grade-aware traversal.
4. **API contracts:** structured MCP and typed, generation-pinned Node/Python sessions.
5. **Evaluation and maintenance:** retrieval quality corpus, full grammar corpus CI, parse-health
   policies, and reproducible release/benchmark artifacts.
6. **Conditional architecture:** learned embedders, 64-bit ids, sharding, and alternate storage
   only when measured needs justify them.

## Verification checklist

- Differential fixtures match the pinned ast-grep baseline or name an intentional divergence.
- External symbol identity survives an unchanged rebuild and fails safely across incompatible
  transitions.
- Fast-stat and verified cache modes have adversarial preserved-mtime tests.
- Every resolver-derived edge exposes grade, source evidence, resolver reason, alternative
  candidates, generation, and indexed-content identity.
- Graph-backed rewrites reject stale or insufficient semantic evidence explicitly.
- Every traversal uses one selector contract and cannot cross an unrequested relation or grade.
- MCP and bindings return structured, paginated, generation-labelled data.
- Span fetch proves that returned bytes match the indexed content.
- Grammar updates run upstream corpora and Vorpal outline/relation fixtures.
- Retrieval changes report quality and cost with channel ablations.
- Parse-health policies make incomplete extraction visible to every consumer.
- Published artifacts and benchmark claims are reproducible.

## Sources

Local implementation:

- [`README.md`](../README.md)
- [`ARCHITECTURE.md`](ARCHITECTURE.md)
- [`UPSTREAM.md`](UPSTREAM.md)
- [`crates/canonical`](../crates/canonical)
- [`crates/index`](../crates/index)
- [`crates/ingest`](../crates/ingest)
- [`crates/kg`](../crates/kg)
- [`crates/resolve`](../crates/resolve)
- [`crates/graph`](../crates/graph)
- [`crates/ann`](../crates/ann)
- [`crates/mcp`](../crates/mcp)
- [`crates/napi`](../crates/napi)
- [`crates/pyo3`](../crates/pyo3)

Upstream:

- [ast-grep v0.45.0 release](https://github.com/ast-grep/ast-grep/releases/tag/0.45.0)
- [ast-grep repository](https://github.com/ast-grep/ast-grep)
- [ast-grep outline reference](https://ast-grep.github.io/reference/cli/outline.html)
- [ast-grep API reference](https://ast-grep.github.io/reference/api.html)
- [ast-grep MCP repository](https://github.com/ast-grep/ast-grep-mcp)

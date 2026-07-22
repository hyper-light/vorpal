# Vorpal Improvements: Gaps Relative to ast-grep

> - **Assessment date:** 2026-07-20
> - **Vorpal baseline:** the current repository worktree, originally copied and rebranded from
>   ast-grep v0.44.0.
> - **ast-grep baseline:** v0.44.1 plus public upstream proposals available on the assessment date.

## Executive summary

Vorpal already contains more repository-level machinery than ast-grep: a persistent index, a
syntax-derived code graph, cross-file resolution, hybrid retrieval, and a built-in graph-oriented
MCP server. ast-grep remains the stronger production reference for structural search, linting,
and codemods: it has the mature upstream engine, broader distribution, less fork drift, and a
clearer contract around syntax-only matching.

The most important Vorpal gaps are not missing exotic storage codecs or a larger ANN design. They
are correctness and integration gaps in the repository-intelligence layer that is supposed to
differentiate it:

1. Graph queries identify symbols only by display name and can combine unrelated namesakes.
2. Cross-file resolution is useful but still syntax-derived and heuristic, without complete
   language binding or type information.
3. Graph knowledge is not available to structural rules, so codemods are no safer semantically
   than ast-grep codemods.
4. The default "semantic" vector ranker is lexical feature hashing, not learned semantic search.
5. Cache validation uses size and modification time rather than content identity.
6. The MCP and language-binding surfaces expose only part of Vorpal's combined engine and index.
7. The fork needs a deliberate upstream synchronization and differential-testing process.
8. Several architecture documents describe ambitious future systems as if their implementation
   status were current or obvious.

The recommended product position is therefore:

> **ast-grep-compatible structural tooling plus persistent repository intelligence.**

Until the resolution and identity gaps are closed, Vorpal should describe its graph as
**deterministic, syntax-derived, and confidence-labelled**, not compiler-grade semantic analysis.

## Existing code: where the products differ

| Area | ast-grep | Vorpal today | Assessment |
|---|---|---|---|
| Structural matching and rewriting | Patterns, metavariables, strictness modes, relational and composite YAML rules, transforms, rewriters, fixes, `run`, `scan`, `test`, LSP, and outline | The rebranded v0.44.0 engine plus local optimizations | Substantially shared. This is the compatibility foundation, not the primary differentiation. |
| Languages | The same 28 compiled `SupportLang` variants in the compared source trees | The same 28 grammars, with bundled default outline rules for all 28 | Vorpal currently has broader default outline-rule coverage than upstream, but relation precision still varies by language. |
| Persistent project state | Structural CLI scans are stateless; the LSP maintains editor state | Persistent extraction products, graph segments, edge store, ANN data, manifest, and a warm MCP process | A real Vorpal advantage, subject to cache-integrity and incremental-linking gaps below. |
| Cross-file knowledge | No built-in project symbol/call/import graph | Nodes plus `calls`, `references`, `imports`, `implements`, `of_type`, and containment edges | Useful, but not equivalent to compiler or language-server resolution. |
| Retrieval | Structural search | Exact/token name matching, lexical-hashing vectors, graph in-degree, and reciprocal-rank fusion | Broader discovery, but "semantic" needs careful naming and evaluation. |
| MCP | A separate experimental ast-grep MCP repository exposes structural matching, rule tests, and AST inspection | A built-in server exposes index, graph, reachability, and hybrid-search tools | The two surfaces are complementary; Vorpal does not yet expose its inherited structural engine through MCP. |
| Bindings and packaging | Mature CLI and Rust/Node/Python/WASM distribution | Inherited matcher bindings; registry publication is still described as in progress | Vorpal graph/index/search APIs are not yet available through the inherited bindings. |

## Priority 0: establish a trustworthy foundation

### 1. Give graph queries an unambiguous symbol identity

**Current gap.** `Kg::nodes_named` performs a linear scan, while `incoming_named` applies an edge
query to every node with the same display name. The CLI and MCP accept only an exact `name`.
Consequently, a query such as `callers new`, `references parse`, or `node render` can conflate
unrelated symbols across files, owners, kinds, or overloads. Path-qualified identity exists
internally, but the public query contract does not expose it.

Relevant code:

- [`crates/kg/src/kg.rs`](../crates/kg/src/kg.rs) — `nodes_named` and `incoming_named`
- [`crates/mcp/src/server.rs`](../crates/mcp/src/server.rs) — the `name`-only tool schema

**Improvement.** Define one stable selector used by the library, CLI, MCP, and future bindings:

```text
SymbolSelector {
  id?, external_id?, name?, path?, kind?, owner?, signature?
}
```

Name-only requests should return candidates when they are ambiguous instead of silently merging
their neighborhoods. A follow-up query should accept a stable `NodeId` or external identity.
Preserve display names for ergonomics, but do not use them as identity.

**Acceptance criteria.**

- Two same-named methods in different files can be selected independently.
- Ambiguous queries return structured candidates with enough fields to choose one.
- Every graph result records the selected target identity.
- CLI, MCP, and library semantics are identical.

### 2. Control ast-grep fork drift

**Current gap.** Vorpal began from ast-grep v0.44.0 while upstream has continued moving. Even the
v0.44.1 patch release contains a concrete divergence: upstream outline extraction uses a bounded
`sync_channel` with a queue bound of 256, while Vorpal still uses an unbounded `mpsc::channel` in
[`crates/cli/src/outline/extract.rs`](../crates/cli/src/outline/extract.rs). Small changes like this
accumulate into performance, behavior, and security drift.

**Improvement.** Treat ast-grep compatibility as a maintained subsystem:

- Record the upstream base commit, not only a release label.
- Regularly merge or cherry-pick upstream engine, language, LSP, and binding changes.
- Maintain a short ledger for intentional divergences.
- Run differential fixtures against the matching CLI and public binding APIs.
- Require an explicit decision for every upstream change that cannot be adopted.

**Acceptance criteria.** A compatibility job compares output, edits, diagnostics, and exit codes
for representative `run`, `scan`, `test`, and `outline` cases. Known differences are versioned
fixtures rather than accidental drift.

### 3. Make documentation distinguish implemented, partial, and proposed behavior

**Current gap.** [`IMPROVEMENT_PLAN.md`](../IMPROVEMENT_PLAN.md) still says "No code written yet"
although the literal prefilter, index, graph, cache, resolver, and MCP work now exist.
[`ARCHITECTURE.md`](ARCHITECTURE.md) mixes existing components with targets such as content-hash
ingest, advanced compressed stores, compiler-like scopes, graph-backed rules, learned embedding
adapters, zero-copy file access, and billion-line scaling. [`REMOTE.md`](REMOTE.md) is a remote
execution design, while only an early wire crate is present.

**Improvement.** Maintain a living status table for every architecture commitment and attach
measurements only to the implementation that produced them. README claims should match the
public surface exactly; for example, the built-in MCP server does not currently serve the
structural `run`/`scan`/rule-testing surface, so "serves the whole surface" is too broad.

Use these status labels consistently:

- **Implemented:** shipped through a public surface and covered by tests.
- **Partial:** useful code exists but does not meet the architecture's stated contract.
- **Prototype:** exploratory code without a stable public contract.
- **Proposed:** design only.

### 4. Keep the checked tree green

At the assessment date, `cargo test -p vorpal-wire --lib` reports 11 passed, 1 failed, and 1
ignored. `hash::tests::stable_hash_golden_vectors` disagrees with the pinned `vorpal` hash in
[`crates/wire/src/hash.rs`](../crates/wire/src/hash.rs). This is especially important because the
test is meant to protect distributed byte identity.

Resolve whether the algorithm or the golden value is authoritative, document the compatibility
decision, and gate any remote protocol work on stable cross-version vectors. Do not merely update
the golden value without establishing why it changed.

## Priority 1: make repository intelligence correct and usable

### 5. Upgrade resolution from heuristics to explicit language semantics

**Current gap.** Vorpal extracts binders, qualified references, receiver-shaped calls, imports,
and several language-specific visibility rules. It also records resolved, ambiguous, external,
and masked counts. That is meaningfully better than a bare name join, but it is still not a full
binding or type-resolution system:

- There is no complete per-file lexical scope and binding table for all languages.
- Receiver types and overload/signature selection are incomplete.
- Import, re-export, alias, module, and package rules are not uniformly modeled.
- Relation coverage and precision differ by language.
- A deterministic low-confidence edge can still be emitted for an ambiguous bare reference.

The biggest risk is not approximation itself; it is a consumer treating an approximate edge as a
fact.

**Improvement.** Build language-specific resolver modules on a common evidence model:

1. Persist lexical scopes, declarations, bindings, and imports during extraction.
2. Resolve local bindings before any cross-file name search.
3. Resolve modules and aliases with language-specific path rules.
4. Use owner and receiver information before matching members.
5. Represent candidate sets explicitly; emit an edge only at a documented confidence threshold.
6. Make confidence, resolution reason, and source span queryable on every derived edge.
7. Publish a language/relation precision matrix backed by fixtures.

Compiler or language-server integration can be an optional high-precision adapter for languages
where tree-sitter syntax cannot supply enough information. The deterministic syntax-only resolver
should remain a supported mode.

**Acceptance criteria.** Multi-file fixtures cover shadowing, aliases, re-exports, private
visibility, same-named members, overloads, receiver calls, and renames. No test passes solely
because candidates happen to be inserted in a deterministic order.

### 6. Validate incremental products by content identity

**Current gap.** Cached extraction products are replayed when source size and modification time
match. This is fast, but it can accept stale data when contents change without changing size and
the timestamp is preserved or restored. The current implementation is in
[`crates/index/src/lib.rs`](../crates/index/src/lib.rs) and the product stamp is defined in
[`crates/ingest/src/product.rs`](../crates/ingest/src/product.rs).

**Improvement.** Use a staged validation path:

1. Stat is the cheap rejection hint.
2. A stable content digest is the correctness identity.
3. The cache key also includes extraction-rule, grammar, schema, and relevant engine versions.
4. The manifest is published atomically only after graph and cache data are durable.

Git object IDs may accelerate clean worktrees, but must not be the only path because untracked and
modified files still need correct validation.

**Acceptance criteria.** Tests cover same-size edits with restored mtimes, grammar/rule upgrades,
interrupted commits, and corrupted cache entries.

### 7. Index names and add filters before scaling the vector tier

**Current gap.** Exact graph-name lookup is O(number of nodes). Search has no complete, common
filter contract for path, language, symbol kind, owner, or package. The ANN layer and graph may
therefore spend work producing candidates that the caller could have excluded cheaply.

**Improvement.** Add a persisted name/token index and a shared query filter type. A compact name
map may be sufficient initially; Tantivy or another full-text engine should be adopted only when
benchmarks justify its operational cost. Apply filters during candidate generation where
possible, not only after ranking.

**Acceptance criteria.** Exact lookup is sublinear, all surfaces share filters, and benchmarks
report latency and memory across corpus sizes and name-collision distributions.

### 8. Complete the MCP surface

**Current gap.** Vorpal MCP currently exposes `index`, `node`, `callers`, `references`,
`importers`, `implementors`, `type_users`, `reachable`, and `search`. It does not expose the
inherited structural matcher, rule testing, AST inspection, rewrite previews, document fetching,
or a shared-parse operation. Conversely, ast-grep's separate experimental MCP focuses on
structural search, rule validation, and syntax-tree inspection rather than a repository graph.

**Improvement.** Make the built-in server the union of the two useful models:

- `structural_search` with pattern, rule, language, path filters, and result limits
- `test_rule` and AST/node inspection for authoring rules
- `fetch_document` or `fetch_span` for graph/search results
- graph queries accepting `SymbolSelector`, not only a name
- cursor-based pagination and response-size limits
- result provenance, confidence, index generation, and stable node identity
- shared parsing so one file parse can feed structural matches and extraction

Mutating rewrites should require an explicit method and preview/confirmation contract; read-only
search should remain the default agent path.

### 9. Expose repository features through Node and Python

**Current gap.** The inherited Node, Python, and WASM APIs expose the structural matcher but not
the graph/index/search capabilities that differentiate Vorpal.

**Improvement.** Add a small, stable repository API before attempting complete parity:

```text
openIndex(path)
search(query, filters, limit)
resolveSymbol(selector)
neighbors(id, relation, direction)
fetchNode(id)
```

Use opaque handles and structured values rather than serializing the entire graph. Define index
generation and handle lifetime semantics so daemon refreshes cannot silently retarget a handle.

## Priority 2: integrate the graph with the structural engine

### 10. Add graph-backed rules and binding-aware metavariables

**Current gap.** Vorpal can answer graph questions after indexing, but the YAML matcher cannot use
those answers. It has no predicates such as `calls`, `calledBy`, `imports`, `resolvesTo`, or
`sameBinding`. Repeated metavariables still mean syntactically equivalent captures, not identical
symbols. As a result, Vorpal's existing codemods are not semantically safer than ast-grep's.

**Improvement.** Introduce opt-in project predicates after symbol identity and resolution are
stable. The matcher should receive a read-only index context and explicitly fail or skip with a
diagnostic when a semantic rule is run without a compatible index.

Candidate rule concepts:

```yaml
rule:
  all:
    - pattern: $OBJ.close()
    - resolvesTo:
        metavariable: $OBJ
        selector: { kind: variable }
    - insideSymbol:
        relation: calls
        target: { name: acquire }
```

Names and exact syntax are illustrative. The contract matters more: every semantic predicate must
declare whether approximate edges are accepted, and fixes must default to high-confidence facts.

**Acceptance criteria.** A semantic rule produces the same result from CLI, library, and daemon;
reports why candidates were accepted or rejected; and cannot silently fall back to text equality.

### 11. Stop calling lexical vectors semantic without qualification

**Current gap.** The default `LexicalEmbedder` is a deterministic hashing-trick bag of tokens. It
is useful for connecting `resolve import path` with `resolve_import_path`, but it does not model
meaning the way a learned embedding does. The `Embedder` trait is pluggable, yet the product has no
shipped local-model or remote-model adapter and no retrieval evaluation suite.

**Improvement.** Choose one of two honest product contracts:

- Call the current feature **hybrid lexical/graph retrieval**, reserving "semantic" for an
  optional learned model; or
- Ship model adapters, model/version provenance, dimension compatibility checks, durable rebuild
  rules, and a measured retrieval benchmark.

In either case, every result should expose which rankers contributed and their raw/rank-fusion
scores. Evaluate exact-name recall and descriptive-query recall separately; one should not mask a
regression in the other.

### 12. Make graph updates incremental, not only parsing

**Current gap.** A changed file can be the only file re-parsed, but Vorpal re-links the complete
graph. This favors correctness and prevents stale edges, but its cost scales with the repository
even for a one-file edit.

**Improvement.** First measure where the full re-link becomes material. Then persist enough
dependency and reverse-reference information to invalidate only:

- nodes originating in the changed file,
- references whose candidate set contains changed/removed definitions,
- import/module dependents affected by path or export changes, and
- graph/ANN rows whose source data changed.

Use generation-based publication so readers see either the old complete graph or the new complete
graph, never a partially repaired mixture.

## Priority 3: scale and distribution after semantics stabilize

### 13. Evolve storage from measured bottlenecks

**Current gap.** The architecture proposes compressed columnar `.vseg` data, an on-disk canonical
index, log-structured manifests, compressed CSR/CSC, sophisticated ANN algorithms, epochs, and
compaction. Current code is an earlier and simpler implementation: hot columns are largely raw,
edges are persisted flat and rebuilt into graph indexes, and vector data is held at full precision
with in-memory indexing structures. The output graph itself remains corpus-sized in memory.

**Improvement.** Do not adopt every named storage technique simultaneously. Establish corpus
benchmarks and promote components in this order:

1. Bound and measure cold-open memory, resident memory, and one-file update cost.
2. Move the largest proven resident index to an mmap-friendly representation.
3. Add atomic generations and crash recovery before background compaction.
4. Add compression where decode cost wins end-to-end, not only on disk size.
5. Change ANN architecture only after a labelled retrieval/latency benchmark shows the need.

Claims about billion-line scale require a reproducible scale model and extrapolation limits; they
should not be inferred from bounded transient parsing memory alone.

### 14. Complete zero-copy input only if profiling supports it

**Current gap.** The architecture describes mmap/zero-copy parsing, while CLI and ingest paths
still use `read_to_string` or reusable owned string buffers. Reuse reduces allocations but is not
zero-copy.

**Improvement.** Profile copies and parser time separately. If file copying is material, introduce
an input abstraction that can borrow mmap bytes while preserving encoding validation, file-change
safety, platform behavior, and parser lifetime. Keep buffered reads for small files when mmap setup
is slower.

### 15. Defer remote execution until the local contracts are stable

**Current gap.** [`REMOTE.md`](REMOTE.md) proposes transport, agent, loader, orchestration, and
distributed result merging. The current tree contains an early `vorpal-wire` crate, but not the
complete remote CLI, transport, remote agent, scheduling, or distributed index system.

**Improvement.** Remote execution should depend on stable:

- versioned query and result schemas,
- content/rule/grammar identity,
- deterministic ordering and deduplication,
- bounded frames and cancellation,
- compatibility tests across binary versions, and
- a green wire-format test suite.

Remote parallelism can accelerate scans; it cannot repair ambiguous symbol identity, inaccurate
resolution, or an unstable cache contract. Those remain higher priority.

## Proposed features: comparison with ast-grep's public direction

Neither project publishes a binding long-term roadmap detailed enough for a promise-by-promise
comparison. The ast-grep items below are public issue/proposal signals, not committed delivery
dates.

| Direction | ast-grep public signal | Vorpal opportunity |
|---|---|---|
| Pluggable semantic analysis | A user proposal suggests an analyzer factory and external semantic providers | Vorpal already owns an index/resolver substrate, but should expose it through a clean provider boundary rather than coupling every matcher to the current graph format. |
| Cross-file guards | A user request highlights the absence of path/module-aware project guards | Vorpal can implement this natively after `SymbolSelector`, import resolution, and graph-backed rule contracts are stable. |
| Rule performance diagnostics | A request for per-rule timing has maintainer acknowledgement | Vorpal should inherit or upstream compatible diagnostics; they will also reveal prefilter and semantic-predicate costs. |
| Bounded outline traversal | A maintainer proposal reports a measured improvement from `stopBy` traversal bounds | Adopt upstream-compatible behavior and add it to differential tests instead of carrying a divergent outline engine. |
| Outline language coverage | Requests continue to expand upstream default rule coverage | Vorpal's all-language bundled rules are an advantage only if coverage and precision are tested and published per language. |
| Debugging experience | The visible upstream milestone is focused on better debugging | Vorpal should avoid diverging from improvements that make shared structural rules easier to inspect and explain. |

This suggests a healthy division of emphasis: ast-grep is likely to remain the reference for a
fast, inspectable structural rule engine, while Vorpal should make indexed project context a
well-specified optional extension. Compatibility lets Vorpal benefit from both rather than
reimplementing upstream work.

## Proposed-versus-implemented status

| Capability from the plans | Status in the assessed tree | Missing contract |
|---|---|---|
| Structural ast-grep-compatible engine | **Implemented** | Ongoing upstream parity process |
| Literal prefilter for patterns and required regex literals | **Implemented** | Cross-version benchmarks and false-negative guard corpus |
| Default outline rules for 28 languages | **Implemented** | Published construct/relation coverage matrix |
| Persistent graph and mmap cold-open | **Implemented** | Unambiguous selectors, edge evidence in query results, scale measurements |
| Cross-file resolution | **Partial** | Complete scopes/bindings, receiver/type precision, language coverage |
| Bounded streaming ingest | **Partial** | Content-hash identity and end-to-end corpus-sized memory accounting |
| Incremental indexing | **Partial** | Content validation and affected-subgraph linking |
| Hybrid name/vector/graph retrieval | **Partial** | Filters, provenance, evaluation, learned-model adapters if "semantic" is retained |
| Built-in MCP daemon and watcher | **Partial** | Structural tools, document fetch, selectors, pagination, shared parse |
| Segmented/custom storage | **Partial** | Compression, canonical on-disk index, atomic generations, compaction |
| Graph-backed rules and scoped symbol metavariables | **Proposed** | Matcher/index context and confidence semantics |
| Repository APIs in Node/Python/WASM | **Proposed** | Stable handle and refresh semantics |
| Zero-copy mmap scan path | **Proposed** | Borrowed input abstraction and benchmarks |
| Advanced disk ANN/filtered ANN stack | **Proposed** | Retrieval corpus and evidence that simpler tiers are insufficient |
| Remote/fleet execution | **Prototype/proposed** | Stable wire vectors, transport, agent, orchestration, compatibility suite |
| Billion-line operational scale | **Proposed target** | Reproducible resource model and large-corpus validation |

## Recommended delivery sequence

### P0 — trust and maintainability

1. Fix the wire golden-vector discrepancy and restore a green relevant test suite.
2. Add `SymbolSelector` and ambiguity-preserving graph queries.
3. Establish the upstream base ledger and differential compatibility tests.
4. Correct public claims and maintain the implementation-status matrix.

### P1 — useful repository intelligence

1. Add content-hash cache validation and versioned cache keys.
2. Persist a name/token index and shared filters.
3. Expand scope, binding, module, and receiver-aware resolution with evidence fixtures.
4. Add structural search, rule tests, document fetch, selectors, and pagination to MCP.
5. Expose a minimal repository API to Node and Python.

### P2 — semantic leverage

1. Add graph-backed rules with explicit confidence requirements.
2. Add binding-identity metavariables.
3. Add retrieval provenance and evaluation; then decide whether learned embeddings are valuable.
4. Incrementally repair affected graph regions after changes.

### P3 — scale and fleet

1. Replace proven memory and I/O bottlenecks with mmap/compressed/on-disk structures.
2. Add crash-safe generations and compaction.
3. Select advanced ANN techniques from measured needs.
4. Build remote execution only on stable local and wire contracts.

## Sources

Local implementation and design:

- [README](../README.md)
- [Architecture and phased build plan](ARCHITECTURE.md)
- [Original improvement plan](../IMPROVEMENT_PLAN.md)
- [Remote execution proposal](REMOTE.md)
- [`vorpal-index`](../crates/index/src/lib.rs)
- [`vorpal-kg`](../crates/kg/src/kg.rs)
- [`vorpal-resolve`](../crates/resolve/src/resolver.rs)
- [`vorpal-mcp`](../crates/mcp/src/server.rs)
- [`LexicalEmbedder`](../crates/ann/src/embed.rs)

ast-grep reference and public direction:

- [ast-grep repository](https://github.com/ast-grep/ast-grep)
- [ast-grep v0.44.1 release](https://github.com/ast-grep/ast-grep/releases/tag/0.44.1)
- [Rule configuration](https://ast-grep.github.io/guide/rule-config.html)
- [Relational rules](https://ast-grep.github.io/guide/rule-config/relational-rule.html)
- [Outline command](https://ast-grep.github.io/reference/cli/outline.html)
- [v0.44.1 bounded outline queue](https://github.com/ast-grep/ast-grep/blob/0.44.1/crates/cli/src/outline/extract.rs)
- [Experimental ast-grep MCP server](https://github.com/ast-grep/ast-grep-mcp)
- [Proposal: pluggable semantic analysis](https://github.com/ast-grep/ast-grep/issues/2413)
- [Request: cross-module/file guards](https://github.com/ast-grep/ast-grep/issues/2770)
- [Request: per-rule performance metrics](https://github.com/ast-grep/ast-grep/issues/2800)
- [Proposal: bounded outline traversal](https://github.com/ast-grep/ast-grep/issues/2825)
- [Request: HTML outline support](https://github.com/ast-grep/ast-grep/issues/2813)
- [Milestone: Better Debugging Experience](https://github.com/ast-grep/ast-grep/milestone/2)

# ast-grep Improvement Plan — Speed, Code-First Intelligence, Semantic Depth

> **Audience:** an autonomous Opus 4.8 coding agent picking up this work cold.
> **Status:** HISTORICAL — this was the original design proposal; the bulk of it has since
> been implemented (index, knowledge graph, resolver, prefilter, MCP daemon, ANN tier,
> bindings). Current status lives in `docs/IMPROVEMENTS.md`'s status matrix and
> `docs/ARCHITECTURE.md`'s per-section status blocks; measurements live in `README.md`.
> **Prepared:** 2026-06-29.
> **Repos referenced:**
> - `ast-grep` (Rust workspace): `/Users/adalundhe/Documents/Projects/ast-grep`
> - `sylk` (Go multi-agent client with a knowledge graph): `/Users/adalundhe/Documents/Projects/sylk`
>
> This document is self-contained. It carries the architectural facts an
> implementing agent needs (with file/line anchors), the rationale, and a
> tiered, sequenced plan. Verify every file path and symbol still exists before
> acting — the codebase moves.

---

## 0. TL;DR

ast-grep is a **stateless, per-file, purely syntactic** tree-sitter pattern matcher.
Its statelessness is a genuine strength (zero warm-up, trivially parallel,
deterministic). The opportunity is to add an **optional index/cache layer** that
keeps the fast path fast, plus cheap semantics, plus — as the strategic bet — a
**deterministic, syntax-derived knowledge graph** (def/ref, call graph, imports)
that turns ast-grep from a linter into a refactoring/security engine.

Do **not** turn ast-grep into sylk. No embeddings, no LLM, no vector DB in the
core — that is sylk's lane and would compromise ast-grep's identity as a fast,
deterministic, code-first tool.

**Recommended first move:** Tier 1 #1 (literal pre-filter) — pure speed, no API
change, measurable.
**Recommended strategic bet:** Tier 3 #6–7 (project index + graph-backed rules).

---

## 1. How ast-grep works today (ground truth)

A Rust Cargo workspace (edition 2024, MSRV 1.85). Crates: `cli`, `config`,
`core`, `dynamic`, `language`, `lsp`, `napi`, `outline`, `pyo3`, `wasm`.

### 1.1 The matching engine (`crates/core`)

- **`Matcher` trait** — central abstraction. `crates/core/src/matcher.rs`:
  ```rust
  pub trait Matcher {
    fn match_node_with_env<'tree, D: Doc>(
      &self,
      node: Node<'tree, D>,
      env: &mut Cow<MetaVarEnv<'tree, D>>,
    ) -> Option<Node<'tree, D>>;
    fn potential_kinds(&self) -> Option<BitSet> { None }
    fn get_match_len<D: Doc>(&self, node: Node<'_, D>) -> Option<usize> { None }
  }
  ```
  Implemented by `Pattern`, `KindMatcher`, `RegexMatcher`, composites (`All`,
  `Any`, `Not`), and relational rules (`Inside`, `Has`, `Follows`, `Precedes`).

- **`Pattern` type** — `crates/core/src/matcher/pattern.rs`:
  ```rust
  pub struct Pattern { pub node: PatternNode, root_kind: Option<u16>, pub strictness: MatchStrictness }
  pub enum PatternNode {
    MetaVar { meta_var: MetaVariable },
    Terminal { text: String, is_named: bool, kind_id: u16 },   // <-- fixed literals live here
    Internal { kind_id: u16, children: Vec<PatternNode> },
  }
  ```
  Parsed from strings like `"const $A = $B"` via the language's tree-sitter parser.

- **Match algorithm** — `crates/core/src/match_tree/match_node.rs`,
  `match_node_impl(...)`. Terminals match text+kind (per strictness); metavars
  capture subtrees into `MetaVarEnv`; internal nodes recurse; ellipsis (`$$$`)
  uses clone-the-aggregator lookahead probing (PR #2670) to avoid leaking failed
  bindings.

- **Strictness** — `crates/core/src/match_tree/strictness.rs`: `Cst`, `Smart`
  (default), `Ast`, `Relaxed`, `Signature`, `Template`.

- **Node/Doc abstraction** — `crates/core/src/node.rs`, `crates/core/src/source.rs`.
  `SgNode` (parent/children/dfs/ancestors/kind_id/range/…) and `Doc`
  (`Source`/`Lang`/`Node`, `root_node`, `do_edit`) isolate tree-sitter so the
  same matching logic serves UTF-8 (CLI), UTF-16 (napi), `Vec<char>` (wasm).

- **MetaVariables** — `crates/core/src/meta_var.rs`:
  ```rust
  pub enum MetaVariable { Capture(name, named), MultiCapture(name), Multiple, Dropped(named) }
  ```
  `MetaVarEnv` maps name → captured node(s). **Critically, captures are
  subtrees/text, NOT symbol identities.** `$A ... $A` means "same text."

- **Key existing optimization:** `potential_kinds()` returns a `BitSet` of node
  kind IDs a rule can match. `CombinedScan` builds a kind→rules map so DFS only
  tries applicable rules per node. Composites intersect (All) / union (Any) child
  kind sets (`crates/config/src/rule/ops.rs`).

### 1.2 Per-file scan loop — NO index, NO cache

- **Fresh parse every file** — `crates/core/src/tree_sitter/mod.rs`, `try_new`:
  ```rust
  pub fn try_new(src: &str, lang: L) -> Result<Self, String> {
    let src = src.to_string();                 // <-- full copy
    let ts_lang = lang.get_ts_language();
    let tree = parse_lang(|p| p.parse(src.as_bytes(), None), ts_lang)?;  // old_tree = None
    Ok(Self { src, lang, tree })
  }
  ```
  Incremental parse (`parser.parse(bytes, old_tree)`) exists in `Doc::parse` but
  is used **only** for single-document edits — never during CLI scanning.

- **The scan** — `crates/config/src/combined.rs`, `CombinedScan::scan`:
  ```rust
  for node in root.root().dfs() {                         // one DFS per file
    let kind = node.kind_id() as usize;
    let Some(rule_idx) = self.kind_rule_mapping.get(kind) else { continue; };
    for &idx in rule_idx {
      let rule = &self.rules[idx];
      let Some(ret) = rule.matcher.match_node(node.clone()) else { continue; };
      // collect match / diff, check suppressions
    }
  }
  ```
  (A separate suppression pass also runs; see `Suppressions::collect_all`.)

- **File discovery & parallelism** — `crates/cli/src/utils/worker.rs`. Uses the
  `ignore` crate's `WalkParallel` (respects `.gitignore`). Producer threads parse
  + match each file **independently**; matches flow to a single consumer via
  `mpsc::channel` (deterministic output order). `--max-results` early-terminates
  via an atomic counter (`MaxItemCounter`). **Parallelism is in discovery only;
  matching is per-file and shares no state.**

- **Scan entry** — `crates/cli/src/scan.rs`, `ScanWithConfig` impl `PathWorker`:
  `build_walk` filters to languages used by rules; `produce_item` reads file →
  parses → builds a `CombinedScan` from language-specific rules → scans.

### 1.3 Rule system (`crates/config`)

- **Rule enum** — `crates/config/src/rule/mod.rs`: atomic (`Pattern`, `Kind`,
  `Regex`, `NthChild`, `Range`), relational (`Inside`, `Has`, `Precedes`,
  `Follows`), composite (`All`, `Any`, `Not`), `Matches` (utility/referent rules).
- **Relational rules** — `crates/config/src/rule/relational_rule.rs`. `Inside`
  walks ancestors; `Has` does DFS from children; `Follows`/`Precedes` walk
  siblings. All parameterized by:
  ```rust
  pub enum StopBy { Neighbor, End, Rule(Box<Rule>) }
  ```
- **Constraints / transforms** — `crates/config/src/rule_core.rs`, `do_match`:
  after the main rule matches, `env.match_constraints(&self.constraints)` filters
  on the **same** metavars (kind/regex/etc.), then transforms apply. **All
  predicates are syntactic — never semantic.**

### 1.4 Language system (`crates/language`)

- `crates/language/src/lib.rs`. 27 languages. `Language` trait: `pre_process_pattern`,
  `meta_var_char` (`$`), `expando_char`, `extract_meta_var`, `from_path`,
  `kind_to_id`, `field_to_id`, `build_pattern`.
- **Expando chars:** languages that reject `$` in identifiers substitute at parse
  time (PHP/Python `$`→`µ`, C/C++ `$`→rare Unicode) so metavars parse as
  identifiers. `impl_lang_expando!` vs `impl_lang!` macros.

### 1.5 What ast-grep deliberately does NOT do

Purely syntactic. No type inference, no symbol resolution, no call graphs, no
cross-references, no cross-file/project index, no dataflow/taint, no incremental
re-indexing during scans. Each file is an island.

---

## 2. What sylk does (the inspiration / contrast)

sylk (`/Users/adalundhe/Documents/Projects/sylk`, Go) is a multi-agent terminal
client built around a persistent **knowledge graph**. Relevant pieces:

- **Graph model** — `core/knowledge/graph/node.go`, `edge.go`:
  - `Node{ ID uint32; Domain; Type; Name; Path; Package; Signature; Content []byte; ContentHash uint64; ... }`
  - `Edge{ SourceID, TargetID uint32; Type; Weight float32; ... }`
  - ~50 node types × 11 domains × **29 edge types** (`Calls`, `CalledBy`,
    `Imports`, `Implements`, `Embeds`, `HasField`, `HasMethod`, `Defines`,
    `Returns`, `SimilarTo`, `Supersedes`, …) — `core/vectorgraphdb/types.go`.
- **Parsing/extraction** — tree-sitter (`core/treesitter/`) + language-specific
  extractors (`core/knowledge/extractors/{go,py,ts}_extractor.go`; Go also uses
  native `go/ast`). Four-stage pipeline (`core/knowledge/extraction_pipeline.go`):
  entity extraction → relation extraction → entity linking → relation validation.
- **Relations** — `core/knowledge/relations/{call_graph,import_graph,type_relations}.go`;
  transitive closure via semi-naive inference (`semi_naive.go`).
- **Storage/index** — SQLite (`core/vectorgraphdb/schema.sql`: `nodes`, `edges`,
  `vectors`, `hnsw_meta/edges`, `provenance`, `conflicts`), Vamana/DiskANN ANN
  (`core/vectorgraphdb/vamana/`), Bleve full-text (`bleve_db.go`).
- **Hybrid query** — text (Bleve) + vector (Vamana) + graph pattern, fused (RRF),
  trust-boosted, provenance-attached (`core/knowledge/query/hybrid_query.go`,
  `core/vectorgraphdb/hybrid_query.go`).
- **ast-grep integration today:** sylk **shells out to the `ast-grep` binary**
  (`agents/librarian/skills_tools.go:272`, the `ast_grep_search` skill) *and*
  separately parses the same files with its own tree-sitter. **Double-parse.**

**Takeaway:** sylk proves the value of a persistent semantic graph over tree-sitter
output. ast-grep already has the fast, multi-language syntactic substrate; it lacks
the persistence/graph layer. The double-parse is an integration opportunity (§3,
Tier 3 #9).

---

## 3. The plan — three tiers

Each item: **What / Why / Where / Approach / Risk / Acceptance**. Tiers are ordered
by value-per-effort and by how much they disturb ast-grep's stateless design.
Tier 1 keeps it fully stateless. Tiers 2–3 add an **opt-in** index; the default
fast path must remain unchanged when the index is absent.

### Tier 1 — Pure speed, zero semantic change, no public API change

#### 1.1 Literal pre-filter before parsing  ⭐ recommended first
- **What:** Before parsing a file, scan its raw bytes for literals that any active
  rule *requires*. Skip files that cannot match. ripgrep-style.
- **Why:** Parsing dominates scan time. For `console.log($A)`, files lacking the
  bytes `console.log` never need parsing. Largest expected win on real repos.
- **Where:**
  - Extract required literals by walking `PatternNode::Terminal.text`
    (`crates/core/src/matcher/pattern.rs`) for each `Pattern`. For composites,
    only literals on **all** branches of an `All` are mandatory; an `Any`
    contributes a literal only if every branch has one (union of alternatives).
    `Not`/`Regex`/`Kind`-only/metavar-only rules contribute no mandatory literal →
    they disable prefiltering for that rule (must always parse).
  - Build one Aho-Corasick automaton (add `aho-corasick` crate, already common in
    the Rust ecosystem; ripgrep's). Per file, run `memmem`/AC over bytes in
    `produce_item` (`crates/cli/src/scan.rs`) *before* `StrDoc::try_new`.
  - A file is parsed iff it hits a literal for ≥1 rule, OR ≥1 rule has no
    mandatory literal.
- **Risk:** Correctness — must never skip a file a rule would match. Be
  conservative: any rule lacking a provable mandatory literal forces parsing.
  Unicode/expando: extract literals from the *user* pattern text, not the
  expando-substituted form.
- **Acceptance:** Identical match output to `main` across the test fixtures
  (`fixtures/`); measurable wall-clock reduction on a large repo with a
  selective rule. Add a `--no-prefilter` escape hatch.

#### 1.2 Persistent content-hash cache / scan daemon
- **What:** Cache scan results (or parsed trees) keyed by
  `(path, mtime, blake3(content), rule_set_hash)`. Re-scans of unchanged files
  return cached matches. Optionally a resident daemon holding parsed trees.
- **Why:** CI, `--watch`, and LSP re-scan unchanged files constantly. sylk dedups
  via `ContentHash`; ast-grep redoes everything. Incremental parse
  (`parser.parse(bytes, old_tree)`) is already wired in `Doc::parse` but unused
  during scans — a daemon can exploit it.
- **Where:** New module in `crates/cli` (e.g. `cache.rs`) consulted in
  `produce_item`. Daemon could live behind a new subcommand. LSP
  (`crates/lsp`) is the natural first consumer.
- **Risk:** Cache invalidation correctness (rule changes, config changes, version
  bumps must bust the cache). Keep it opt-in initially (`--cache <dir>`).
- **Acceptance:** Warm re-scan of an unchanged tree is dramatically faster; output
  byte-identical to cold scan; cache busts correctly when rules/content change.

#### 1.3 Zero-copy ingestion
- **What:** Avoid `src.to_string()` in `try_new`; mmap files and parse from bytes.
- **Why:** Removes a full per-file copy. Cheap, mechanical.
- **Where:** `crates/core/src/tree_sitter/mod.rs` and the `Content`/`Source`
  abstraction (`crates/core/src/source.rs`); read path in `crates/cli`.
- **Risk:** Lifetime/ownership plumbing; encoding handling must stay correct.
  Keep `StrDoc` as-is for binding crates; add an mmap-backed `Doc` for the CLI.
- **Acceptance:** No behavior change; reduced allocations/time in profiles.

### Tier 2 — Cheap semantics (high value/effort ratio; opt-in)

#### 2.1 Scoped meta-variables
- **What:** Let a metavar optionally mean "same **binding**," not "same text" —
  respecting scope and shadowing.
- **Why:** Text-equality is wrong for refactors: `$A` matching a shadowed inner
  variable, or two unrelated locals with the same name. This fixes a real
  correctness gap and unlocks safe rewrites.
- **Where:** Extend `MetaVarEnv` (`crates/core/src/meta_var.rs`) to carry an
  optional binding/scope id alongside the captured node; populate from a per-file
  scope table (2.2). New constraint syntax, e.g. `constraints: { A: { same-binding: true } }`.
- **Risk:** Per-language scope rules differ; start with a couple of languages
  (JS/TS, Python) behind the existing `Language` trait. Must be opt-in so default
  matching is unchanged.
- **Acceptance:** A rule distinguishes shadowed vs. same binding on targeted
  fixtures; default behavior unchanged when the feature is unused.

#### 2.2 Per-file symbol table
- **What:** Compute definitions/locals/scopes for a file in the DFS pass that
  already runs.
- **Why:** Enables semantic-ish constraints (`kind-of: local-variable`,
  `is-defined-in-file`) with zero cross-file work. Foundation for 2.1.
- **Where:** Hook into `CombinedScan::scan`'s existing DFS
  (`crates/config/src/combined.rs`); expose to constraint evaluation in
  `crates/config/src/rule_core.rs` (`do_match` / `match_constraints`).
- **Risk:** Keep it lazy — only build when a rule needs it (no cost for plain
  pattern scans).
- **Acceptance:** New constraints evaluate correctly; no measurable cost for rules
  that don't use them.

### Tier 3 — The strategic bet: a deterministic syntactic knowledge graph (opt-in)

> Keep it **deterministic and syntax-derived**. No embeddings/LLM/vectors in the
> core. This is what makes ast-grep a refactoring/security engine while staying
> true to its code-first identity.

#### 3.1 (#6) Project index crate: def/ref + call + import graph
- **What:** A new crate (e.g. `crates/graph`) that walks the workspace once and
  persists nodes (functions, methods, types, files, modules) and edges
  (`defines`, `references`, `calls`, `imports`, `implements`) — a syntactic
  analogue of sylk's `core/knowledge/relations/*` and graph model.
- **Why:** The leap from per-file linter to project-aware tool. Enables queries
  impossible today (cross-file references, callers, unused exports).
- **Where:** New crate, reusing `crates/language` grammars and `crates/core`
  trees. Persist to SQLite or a compact on-disk format keyed by content hashes
  (mirror sylk's `nodes`/`edges` schema conceptually; do NOT pull in vectors).
  Incremental: re-index only changed files (reuse Tier 1.2 hashing).
- **Risk:** Cross-file symbol resolution is genuinely hard and language-specific.
  Scope deliberately: start with intra-language, import-path-based resolution for
  2–3 languages; accept approximate edges with confidence (sylk attaches
  confidence + evidence spans — adopt that honesty rather than pretending to be a
  compiler).
- **Acceptance:** `sg index` builds a graph; basic queries (callers of X, refs to
  Y, importers of Z) return correct results on a sample repo; incremental
  re-index touches only changed files.

#### 3.2 (#7) Graph-backed relational rules + semantic constraints
- **What:** New rule kinds — `calls:`, `called-by:`, `implements:`, `imports:` —
  and constraints that consult the index. New `StopBy::SemanticBoundary` (stop
  ancestor/descendant walks at scope/function/module boundaries).
- **Why:** Enables rules like *"calls to functions defined under `auth/` not
  wrapped in try/catch"* — the kind of cross-file, security-relevant rule that is
  ast-grep's most valuable potential growth area.
- **Where:** Extend `Rule` (`crates/config/src/rule/mod.rs`), the relational rule
  machinery (`crates/config/src/rule/relational_rule.rs`, `StopBy`), and make
  `match_constraints` (`crates/config/src/rule_core.rs`) able to query the index.
  Thread an optional `&ProjectIndex` through the scan (`CombinedScan::scan`); when
  absent, these rules are inert/error clearly.
- **Risk:** API surface and YAML schema design; must degrade gracefully without an
  index. Keep the `Matcher` trait stable — add index access via a side channel,
  not by changing `match_node_with_env`'s signature for everyone.
- **Acceptance:** YAML rules using `calls:`/`imports:` match correctly against the
  index on fixtures; plain rules unaffected.

#### 3.3 (#8) Intra-procedural dataflow-lite / taint
- **What:** Track a captured metavar through assignments within a single function.
- **Why:** Highest-leverage feature for the security-rule audience that drives
  most `sg scan` usage (source→sink within a function).
- **Where:** Builds on 2.2 (symbol table) + 3.1 (graph). New constraint/relation
  expressing "value of `$A` flows to `$B`."
- **Risk:** Even intra-procedural dataflow is nontrivial; keep it flow-insensitive
  / best-effort first, with confidence, and clearly scoped to one function.
- **Acceptance:** Demonstrable source→sink rule on a fixture; documented limits.

#### 3.4 (#9) Shared-parse / library & daemon API for host tools (e.g. sylk)
- **What:** Expose matching + index through `crates/napi` and `crates/pyo3` (and a
  stable daemon protocol) so a host parses once and gets **both** matches and the
  graph — eliminating sylk's current double-parse.
- **Why:** sylk shells out to the `ast-grep` binary *and* re-parses with its own
  tree-sitter (`agents/librarian/skills_tools.go:272`). A shared-parse API makes
  ast-grep the fast syntactic substrate under semantic tools instead of a forked
  CLI. Widens ast-grep's role without changing its core.
- **Where:** `crates/napi`, `crates/pyo3`, plus the daemon from Tier 1.2.
- **Risk:** API stability commitments; cross-language data marshalling
  (UTF-8/16 handled by the `Doc` abstraction already).
- **Acceptance:** A host can submit source once and receive matches + graph edges;
  a sylk-side spike drops its redundant parse.

---

## 4. Recommended sequencing

1. **Tier 1.1 literal pre-filter** — bank a fast, low-risk, no-API-change win;
   prove it with a benchmark vs. `main` on a large repo.
2. **Tier 1.3 zero-copy** + **Tier 1.2 cache/daemon** — compound the speed wins;
   daemon also unblocks LSP responsiveness.
3. **Tier 2.2 symbol table → 2.1 scoped metavars** — first real semantics, cheap,
   fixes correctness gaps.
4. **Tier 3.1 index → 3.2 graph rules** — the strategic leap; design doc first.
5. **Tier 3.3 dataflow**, **3.4 shared-parse API** — once the index exists.

---

## 5. Guardrails (apply to every change)

- **Preserve the stateless fast path.** Default `sg scan`/`sg run` with plain
  patterns must behave and perform as today. All index/cache/semantic features are
  **opt-in**.
- **Match output must stay byte-identical** where behavior is unchanged. Diff
  against `main` on `fixtures/` before/after.
- **No embeddings/LLM/vector DB in core.** Determinism and code-first behavior are
  the product identity. Semantic edges may carry confidence + evidence (sylk-style
  honesty) but must be reproducible.
- **Keep the `Matcher` trait signature stable.** Add capabilities via new rule
  kinds and side-channel index access, not by rewriting `match_node_with_env` for
  all implementors.
- **Language-incremental.** Land semantic features for 1–3 languages first behind
  the `Language` trait; don't block on all 27.
- **Benchmark everything.** There are no benches in-repo today
  (`crates/*/benches/` empty). Add a small benchmark harness alongside Tier 1.1 so
  speed claims are measured, not asserted.

---

## 6. Quick reference — anchor files

| Concern | Path |
|---|---|
| `Matcher` trait | `crates/core/src/matcher.rs` |
| `Pattern` / `PatternNode` (literals) | `crates/core/src/matcher/pattern.rs` |
| Match algorithm | `crates/core/src/match_tree/match_node.rs` |
| Strictness | `crates/core/src/match_tree/strictness.rs` |
| Node/Doc abstraction | `crates/core/src/node.rs`, `crates/core/src/source.rs` |
| MetaVariables / env | `crates/core/src/meta_var.rs` |
| Fresh-parse entry | `crates/core/src/tree_sitter/mod.rs` (`try_new`) |
| Per-file scan loop | `crates/config/src/combined.rs` (`CombinedScan::scan`) |
| Rule enum | `crates/config/src/rule/mod.rs` |
| Relational rules / `StopBy` | `crates/config/src/rule/relational_rule.rs` |
| Composite kind sets | `crates/config/src/rule/ops.rs` |
| Constraints / transforms | `crates/config/src/rule_core.rs` (`do_match`) |
| CLI scan / worker | `crates/cli/src/scan.rs`, `crates/cli/src/utils/worker.rs` |
| Languages | `crates/language/src/lib.rs` |
| sylk graph model | `sylk/core/knowledge/graph/{node,edge}.go` |
| sylk relations | `sylk/core/knowledge/relations/{call_graph,import_graph,type_relations}.go` |
| sylk store/schema | `sylk/core/vectorgraphdb/schema.sql`, `sylk/core/vectorgraphdb/types.go` |
| sylk ast-grep subprocess call | `sylk/agents/librarian/skills_tools.go:272` |

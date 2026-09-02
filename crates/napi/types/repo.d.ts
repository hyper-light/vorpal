/**
 * The knowledge-graph half of @hyper-light/vorpal-node: index building, hybrid search,
 * and typed graph queries over a committed generation. Every blocking operation has an
 * `Async`-suffixed twin returning a Promise that computes on libuv's thread pool — a
 * server's event loop never blocks on an index build, a search, or a traversal.
 */

/** Filters/refinements shared by the name-selector queries. */
export interface GraphOptions {
  /** Definition file path must end with this suffix. */
  path?: string
  /** One symbol kind (function, method, struct, field, …). */
  kind?: string
  /** Query exactly this node id (from `nodes` output or an ambiguity listing). */
  id?: number
  /** Merge results across ALL same-named definitions instead of listing candidates. */
  all?: boolean
  /** Append node ids to rendered result lines (`indexGraph`). */
  ids?: boolean
}

/** Options for `Index.reachable` / `Index.reachableAsync`. */
export interface ReachOptions {
  /** Seed's definition file path must end with this suffix. */
  path?: string
  /** Seed's symbol kind. */
  kind?: string
  /** Seed exactly this node id. */
  id?: number
  /** Merge across all same-named seeds instead of listing candidates. */
  all?: boolean
  /** Edge relations to traverse (default `["calls"]`; e.g. add `"data_flows"`). */
  relations?: Array<string>
  /** Traversal depth cap (unset/0 = unbounded). */
  maxDepth?: number
  /** Only traverse edges at this resolution grade or better: `exact` | `constrained` | `heuristic`. */
  minGrade?: string
}

/** Structured filters for `Index.search` / `Index.searchAsync`. */
export interface SearchOptions {
  /** Definition file path must end with this suffix. */
  path?: string
  /** Definition file path must start with this prefix (package/subtree scoping). */
  prefix?: string
  /** One symbol kind. */
  kind?: string
  /** Language name or alias (rust, py, ts, …). */
  lang?: string
  /** Only exported definitions. */
  exported?: boolean
  /** Exclude test-classified paths. */
  excludeTests?: boolean
}

/** One labelled tuning query for `indexTune`. */
export interface TuneQueryInput {
  query: string
  /** Expected-hit substring; unlabelled queries run but do not score. */
  expected?: string
}

/** Typed one-node record (see `indexNode`). */
export interface NodeInfo {
  id: number
  name: string
  kind: string
  path: string
  signature: string
  exported: boolean
  /** Definition byte range in `path`; `[0, 0]` when unknown. */
  spanStart: number
  spanEnd: number
}

/** Structured build numbers (see `indexBuildReport`). */
export interface BuildReport {
  /** Files re-parsed this run (changed, new, or cache-missing). */
  indexed: number
  /** Files whose cached extraction product was replayed without a parse. */
  skipped: number
  /** Definitions in the committed graph. */
  nodes: number
  /** References resolved to an exact target. */
  resolved: number
  /** References with multiple surviving candidates. */
  ambiguous: number
  /** References resolved to something outside the tree. */
  external: number
  /** Files excluded by parse-health policy. */
  masked: number
  /** The tree was unchanged — index reused without re-parsing. */
  reused: boolean
}

/** Build or refresh the index; returns the rendered one-line build report. */
export declare function indexBuild(src: string, out?: string): string
export declare function indexBuildAsync(src: string, out?: string): Promise<string>

/** `indexBuild` with the typed report instead of the rendered line. */
export declare function indexBuildReport(src: string, out?: string): BuildReport
export declare function indexBuildReportAsync(src: string, out?: string): Promise<BuildReport>

/** Hybrid search, rendered text; `explain` appends per-hit ranking provenance. */
export declare function indexSearch(indexDir: string, query: string, k?: number, explain?: boolean): string
export declare function indexSearchAsync(indexDir: string, query: string, k?: number, explain?: boolean): Promise<string>

/** One search, two orderings: `{fused, reranked | null, encoderStatus | null}`. */
export declare function indexSearchRanked(indexDir: string, query: string, k?: number): unknown
export declare function indexSearchRankedAsync(indexDir: string, query: string, k?: number): Promise<unknown>

/** Graph relation query, rendered text — verbs mirror the CLI (`callers`, `refs`, …). */
export declare function indexGraph(indexDir: string, verb: string, name: string, options?: GraphOptions): string
export declare function indexGraphAsync(indexDir: string, verb: string, name: string, options?: GraphOptions): Promise<string>

/** One node's typed record by id. */
export declare function indexNode(indexDir: string, id: number): NodeInfo
export declare function indexNodeAsync(indexDir: string, id: number): Promise<NodeInfo>

/** Measure optional ranking features on YOUR queries; `apply` writes the switches. */
export declare function indexTune(indexDir: string, queries: Array<TuneQueryInput>, k?: number, apply?: boolean): unknown
export declare function indexTuneAsync(indexDir: string, queries: Array<TuneQueryInput>, k?: number, apply?: boolean): Promise<unknown>

/** Download (or reuse) the pinned encoder weights; returns the model directory. */
export declare function semanticInstall(variant: string, root?: string): string
export declare function semanticInstallAsync(variant: string, root?: string): Promise<string>

/** Install AND enable globally; returns the model directory. */
export declare function semanticEnable(variant: string, root?: string): string
export declare function semanticEnableAsync(variant: string, root?: string): Promise<string>

/** Remove the global enable; returns whether anything was enabled. */
export declare function semanticDisable(): boolean

/**
 * A session-pinned view of one committed generation: open once, query many, and every
 * answer stays mutually consistent even if a rebuild commits underneath. Each query has
 * a sync form (sub-millisecond reads from the mmapped graph) and an `Async` twin that
 * runs the same read on the uv pool.
 */
export declare class Index {
  /** Open `indexDir`, pinning its CURRENT generation for the session's lifetime. */
  static open(indexDir: string): Index
  /** The pinned generation's content id ("" for a legacy flat index). */
  get generation(): string

  /** One node's typed record, or null. */
  node(id: number): unknown
  nodeAsync(id: number): Promise<unknown>

  /** Typed candidate listing for a selector: every match is the answer. */
  nodes(name: string, options?: GraphOptions): unknown
  nodesAsync(name: string, options?: GraphOptions): Promise<unknown>

  /** Typed edge query: `{outcome: "hits"|"ambiguous"|"no-match", records: [...]}`. */
  related(verb: string, name: string, options?: GraphOptions): unknown
  relatedAsync(verb: string, name: string, options?: GraphOptions): Promise<unknown>

  /** Typed relation-restricted traversal with paths back to the seed. */
  reachable(name: string, direction: 'in' | 'out', options?: ReachOptions): unknown
  reachableAsync(name: string, direction: 'in' | 'out', options?: ReachOptions): Promise<unknown>

  /** Typed evidence: edge form (`toId`) or absence form (`name`). */
  why(fromId: number, toId?: number, name?: string): unknown
  whyAsync(fromId: number, toId?: number, name?: string): Promise<unknown>

  /** Typed hybrid search over the pinned generation. */
  search(query: string, k?: number, options?: SearchOptions): unknown
  searchAsync(query: string, k?: number, options?: SearchOptions): Promise<unknown>
}

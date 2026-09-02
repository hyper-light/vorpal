# TypeScript / JavaScript quickstart

vorpal's AST pattern engine ships to JS in two flavors:

| Package | Use it for | Filesystem search |
|---|---|---|
| **`@hyper-light/vorpal-node`** | Node.js (native N-API binding) — the primary API, fully typed | ✅ `findInFiles` |
| **`@hyper-light/vorpal-wasm`** | Browsers / edge / portable runtimes | ❌ (in-memory only) |

```sh
npm install @hyper-light/vorpal-node     # Node
npm install @hyper-light/vorpal-wasm     # browser/portable
```

> **Availability:** `@hyper-light/vorpal-wasm` is published now. `@hyper-light/vorpal-node`
> publishes with each tagged release — if `npm install` can't find it yet, it's because the
> release hasn't been cut; build from source in the meantime (`crates/napi`).

## Node.js (`@hyper-light/vorpal-node`)

Patterns are real code; metavariables (`$X`, `$$$ARGS`) capture real AST nodes. Kinds are typed
per language, so `node.field(...)` and `node.kind()` narrow.

### Parse, query, capture

```ts
import { parse, Lang } from '@hyper-light/vorpal-node'

const root = parse(Lang.TypeScript, 'console.log(user.name); console.log(count)')
const node = root.root().find('console.log($ARG)')

node.kind()                   // "call_expression"
node.text()                   // 'console.log(user.name)'
node.getMatch('ARG').text()   // "user.name"
node.range()                  // { start: { line, column, index }, end: {...} }

// `$$$` captures node lists:
const call = root.root().find('console.log($$$ARGS)')
call.getMultipleMatches('ARGS').map(n => n.text())   // ["user.name"]
```

### Rewrite — a complete codemod

```ts
import { parse, Lang } from '@hyper-light/vorpal-node'

function modernizeAsserts(source: string): string {
  const root = parse(Lang.TypeScript, source)
  const edits = root.root()
    .findAll('assert.equal($ACTUAL, $EXPECTED)')
    .map(n => n.replace(
      `expect(${n.getMatch('ACTUAL').text()}).toEqual(${n.getMatch('EXPECTED').text()})`,
    ))
  return root.root().commitEdits(edits)
}

modernizeAsserts(`assert.equal(add(1, 2), 3); assert.equal(name, 'ada');`)
// => "expect(add(1, 2)).toEqual(3); expect(name).toEqual('ada');"
```

### Rules & navigation

The full YAML rule system works as a plain object:

```ts
// console.log calls, but only inside function declarations
root.root().findAll({
  rule: {
    pattern: 'console.log($A)',
    inside: { kind: 'function_declaration', stopBy: 'end' },
  },
})

// node predicates
node.matches('console.log($A)')
node.inside('function_declaration')
node.has('member_expression')

// navigate
const fn = root.root().find({ rule: { kind: 'function_declaration' } })
fn.field('name').text()
fn.children(); fn.parent(); fn.child(0)
fn.next(); fn.prev()          // (+ nextAll() / prevAll())
```

### Search across files (Node-only)

File discovery, parsing, and matching run in Rust worker threads; matches stream back per file:

```ts
import { parseAsync, findInFiles, Lang, type SgNode } from '@hyper-light/vorpal-node'

await parseAsync(Lang.TypeScript, source)   // threaded parse of one source

await findInFiles(
  Lang.TypeScript,
  { paths: ['src/'], matcher: { rule: { pattern: 'console.log($MSG)' } } },
  (_err, nodes: SgNode[]) => {
    for (const n of nodes) console.log(`${n.getRoot().filename()}: ${n.text()}`)
  },
)
```

`registerDynamicLanguage(...)` loads custom grammars; `kind(lang, name)` / `pattern(lang, src)`
precompile matchers for reuse. `Lang` covers `JavaScript`, `TypeScript`, `Tsx`, `Css`, `Html`;
custom-language strings are also accepted.

## Browser / portable (`@hyper-light/vorpal-wasm`)

Same pattern API, no filesystem. **You must `await initializeTreeSitter()` once before parsing**
(it loads the wasm grammars). Peer dependency: `web-tree-sitter`.

```ts
import { initializeTreeSitter, parse } from '@hyper-light/vorpal-wasm'

await initializeTreeSitter()   // once, before any parse()

const root = parse('typescript', 'const a = 1; const b = 2')
for (const m of root.root().findAll('const $N = $V')) {
  console.log(m.getMatch('N').text(), '=', m.getMatch('V').text())
}
```

The wasm build exposes `parse`, `kind`, `pattern`, `dumpPattern`, `registerDynamicLanguage`, and
the `SgNode` / `SgRoot` / `Pos` / `Range` classes — everything except the filesystem `findInFiles`.

## Async: nothing blocks the event loop

Every blocking repository call has an `Async`-suffixed twin returning a `Promise` that
computes on libuv's thread pool — `indexBuildAsync`, `indexBuildReportAsync`,
`indexSearchAsync`, `indexSearchRankedAsync`, `indexGraphAsync`, `indexNodeAsync`,
`indexTuneAsync`, `semanticInstallAsync`, `semanticEnableAsync` — and the pinned
`Index` class mirrors its queries as `nodeAsync` / `nodesAsync` / `relatedAsync` /
`reachableAsync` / `whyAsync` / `searchAsync`. Concurrency composes freely:

```ts
import { indexBuildAsync, Index } from '@hyper-light/vorpal-node'

await indexBuildAsync('.', '.vorpal/index')
const index = Index.open('.vorpal/index')
const [callers, reach] = await Promise.all([
  index.relatedAsync('callers', 'handleRequest'),
  index.reachableAsync('handleRequest', 'in', { maxDepth: 3 }),
])
```

The sync forms remain for scripts and REPLs; on the `Index` class they read from the
pinned, mmapped generation in well under a millisecond.

## Large files in long-lived processes

A process that calls `indexBuildAsync` repeatedly (a dev server re-indexing on save)
re-parses edited multi-megabyte sources **incrementally** — vorpal retains parse state
for files over 1 MiB and applies tree-sitter's own incremental reparse, 2–3× faster per
save on giant files, byte-identical output. Automatic; `VORPAL_TREE_CACHE=0` disables.

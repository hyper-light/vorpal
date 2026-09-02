// Walk the graph with the session-pinned Index class: open once, query many.
//
//   npm install @hyper-light/vorpal-node
import { indexBuildAsync, Index } from '@hyper-light/vorpal-node'

await indexBuildAsync('.', '.vorpal/index')

// Open pins the CURRENT generation for this session — answers stay mutually
// consistent even if a rebuild commits underneath.
const index = Index.open('.vorpal/index')
console.log('pinned generation:', index.generation)

// Every query has a sync form (sub-millisecond mmapped reads) and an Async twin
// on the uv pool. In a server, prefer the twins; concurrency composes freely.
const [listing, callers, reach] = await Promise.all([
  index.nodesAsync('main'),
  index.relatedAsync('callers', 'main'),
  index.reachableAsync('main', 'in', { maxDepth: 2 }),
])
console.log(JSON.stringify(listing, null, 2))
console.log(JSON.stringify(callers, null, 2))
console.log(JSON.stringify(reach, null, 2))

// Evidence for an edge the graph claims (same contract as the MCP `why` tool):
// index.why(fromId, toId) for edge evidence, index.why(fromId, null, 'name')
// for absence evidence — ids come from nodes()/related() records.

// Walk the graph with the session-pinned Index class: open once, query many.
//
//   npm install @hyper-light/vorpal-node
import { indexBuild, Index } from '@hyper-light/vorpal-node'

indexBuild('.', '.vorpal/index')

// Open pins the CURRENT generation for this session — answers stay mutually
// consistent even if a rebuild commits underneath.
const index = Index.open('.vorpal/index')
console.log('pinned generation:', index.generation)

// Typed candidate listing for a name (records envelope).
console.log(JSON.stringify(index.nodes('main'), null, 2))

// Relations: callers | refs | importers | implementors | typeusers ...
console.log(JSON.stringify(index.related('callers', 'main'), null, 2))

// Transitive reachability with paths back to the seed.
console.log(JSON.stringify(index.reachable('main', { depth: 2 }), null, 2))

// Evidence for an edge the graph claims (same contract as the MCP `why` tool):
// index.why(fromId, toId) for edge evidence, index.why(fromId, null, 'name')
// for absence evidence — ids come from nodes()/related() records.

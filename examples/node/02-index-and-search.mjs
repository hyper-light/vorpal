// Build a knowledge-graph index and search it from Node.
//
//   npm install @hyper-light/vorpal-node
import {
  indexBuildAsync,
  indexBuildReportAsync,
  indexSearchAsync,
  indexSearchRanked,
} from '@hyper-light/vorpal-node'

const SRC = '.'
const IDX = '.vorpal/index'

// Incremental build — instant when nothing changed — off the event loop (every
// blocking call has an Async twin; the sync forms exist for scripts).
console.log(await indexBuildAsync(SRC, IDX))

// Structured numbers when you need them.
const report = await indexBuildReportAsync(SRC, IDX)
console.log(`indexed=${report.indexed} replayed=${report.skipped} errorFiles=${report.errorFiles}`)

// Hybrid search (rendered text; pass explain=true for per-hit provenance).
console.log(await indexSearchAsync(IDX, 'parse configuration', 5))

// One search, two orderings (requires an encoder: `vorpal enable semantic-f16`).
console.log(indexSearchRanked(IDX, 'retry backoff', 5))

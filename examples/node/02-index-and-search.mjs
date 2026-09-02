// Build a knowledge-graph index and search it from Node.
//
//   npm install @hyper-light/vorpal-node
import { indexBuild, indexBuildReport, indexSearch, indexSearchRanked } from '@hyper-light/vorpal-node'

const SRC = '.'
const IDX = '.vorpal/index'

// Incremental build — instant when nothing changed. Returns the generation dir.
console.log('generation:', indexBuild(SRC, IDX))

// Structured numbers when you need them.
const report = indexBuildReport(SRC, IDX)
console.log(`indexed=${report.indexed} replayed=${report.skipped} errorFiles=${report.errorFiles}`)

// Hybrid search (rendered text; pass explain=true for per-hit provenance).
console.log(indexSearch(IDX, 'parse configuration', 5))

// One search, two orderings (requires an encoder: `vorpal enable semantic-f16`).
console.log(indexSearchRanked(IDX, 'retry backoff', 5))

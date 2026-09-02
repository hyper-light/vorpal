// Repo-wide streaming structural search without an index: findInFiles.
//
//   npm install @hyper-light/vorpal-node
import { findInFiles, Lang } from '@hyper-light/vorpal-node'

// Streams matches per file from a parallel Rust walker; resolve when done.
const total = await findInFiles(
  Lang.TypeScript,
  {
    paths: ['.'],
    matcher: {
      rule: { pattern: 'await $PROMISE', inside: { kind: 'for_statement', stopBy: 'end' } },
    },
  },
  (err, matches) => {
    if (err) throw err
    for (const m of matches) {
      const { line } = m.range().start
      console.log(`${m.getRoot().filename()}:${line + 1}  ${m.text().slice(0, 100)}`)
    }
  },
)
console.log(`${total} files matched (await-in-loop candidates)`)

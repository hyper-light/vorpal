// Structural parse, search, and edit with @hyper-light/vorpal-node.
//
//   npm install @hyper-light/vorpal-node
import { parse, Lang } from '@hyper-light/vorpal-node'

const code = `
function fetchUser(id) {
  console.log("fetching", id)
  const res = await client.get('/users/' + id)
  console.log(res.status)
  return res
}
`

const root = parse(Lang.JavaScript, code).root()

// Pattern search with captures.
for (const call of root.findAll('console.log($$$ARGS)')) {
  const { line } = call.range().start
  console.log(`console.log at line ${line + 1}: ${call.text()}`)
}

// Rule-object search: pattern + relational constraint.
const insideFn = root.findAll({
  rule: { pattern: 'console.log($$$A)', inside: { kind: 'function_declaration' } },
})
console.log(`${insideFn.length} logs inside functions`)

// Rewrite: build replacements from captures, commit into a new string.
const edits = root.findAll('console.log($$$ARGS)').map(node => {
  const args = node.getMultipleMatches('ARGS').map(a => a.text()).join(', ')
  return node.replace(`logger.info(${args})`)
})
console.log(root.commitEdits(edits))

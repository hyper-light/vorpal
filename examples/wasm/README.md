# @hyper-light/vorpal-wasm — the matcher in the browser

The wasm package is the **structural half** of vorpal (parse, pattern match, AST
inspection) compiled to WebAssembly — playgrounds, in-browser lint previews, docs that
match real syntax. Indexing/graph queries are native-only (use the CLI, MCP server, or
the Node/Python SDKs for those).

```sh
npm install @hyper-light/vorpal-wasm web-tree-sitter
```

Grammars load as separate `.wasm` files at runtime (the tree-sitter web engine), so you
register the languages you need with URLs you host:

```js
import init, {
  initializeTreeSitter,
  registerDynamicLanguage,
  parse,
  kind,
  dumpPattern,
} from '@hyper-light/vorpal-wasm'

await init()                       // load the vorpal wasm module (bundler target)
await initializeTreeSitter()       // boot the tree-sitter web engine
await registerDynamicLanguage({    // language name -> grammar wasm URL
  javascript: '/wasm/tree-sitter-javascript.wasm',
})

const root = parse('javascript', 'console.log(user.name)').root()
for (const call of root.findAll('console.log($ARG)')) {
  console.log(call.range().start, call.text())
}
```

`index.html` here is a complete single-file playground: paste code, write a pattern,
see matches live. Serve the directory (`npx serve examples/wasm`) after dropping the
two wasm assets alongside it — the header comment lists exactly which files and where
to get them. Bundlers (Vite/webpack) consume the package directly; the grammar `.wasm`
URLs are the only assets you manage.

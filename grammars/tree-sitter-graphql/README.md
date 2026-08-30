# tree-sitter-graphql

GraphQL grammar for [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

### Playground

```shell
# Install tree-sitter CLI
cargo install tree-sitter-cli --locked

# Clone this repository and navigate into it
git clone https://github.com/joowani/tree-sitter-graphql && cd tree-sitter-graphql

# Compile the parser
tree-sitter build --wasm

# Start the playground at http://127.0.0.1:8000
tree-sitter playground
```

### References

- [Tree-sitter Documentation](https://tree-sitter.github.io/tree-sitter/)
- [GraphQL Specification](https://spec.graphql.org/)

### Credits

The grammar is originally based
on [bkegley/tree-sitter-graphql](https://github.com/bkegley/tree-sitter-graphql)
and [dralletje/tree-sitter-graphql](https://github.com/dralletje/tree-sitter-graphql).

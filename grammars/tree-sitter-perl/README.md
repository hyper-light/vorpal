# tree-sitter-perl
a perl parser using tree-sitter

# Getting started

## To just generate the parser output
`npm run generate`

## Tests

`npm run test`

## Run examples

`npm run example`

## To build the wasm file

1. Run pre-build `npm run pre-build`
2. Run build `npm run build`
3. Generate the wasm file `npm run build-wasm` (make sure you are running docker daemon)

# Maintenance  

## After upgrading tree-sitter-cli
- Command to upgrade - `npm i tree-sitter-cli@latest`
- Then do `npm run update-bindings`
- Then generate the grammar again `npm run generate`


#!/bin/sh
# Index a repo, then explore it: search, graph relations, snippets, structure.
# Usage: ./02-index-and-explore.sh [repo-dir]   (defaults to the current repo)
set -eu
SRC="${1:-.}"
IDX="${VORPAL_INDEX:-$SRC/.vorpal/index}"

echo "== build/refresh the index (incremental — safe to run any time):"
vorpal index "$SRC" --out "$IDX"

echo
echo "== what's in here?"
vorpal graph schema --index "$IDX"

echo
echo "== orientation: module mass, hubs, entry points:"
vorpal graph architecture --top 10 --index "$IDX"

echo
echo "== find something by intent:"
vorpal search "parse configuration" -k 5 --index "$IDX"

echo
echo "== pick a symbol from the results, then:"
echo "   vorpal graph callers <name> --index $IDX"
echo "   vorpal graph snippet <name> --context 4 --index $IDX"
echo "   vorpal graph reachable <name> --depth 2 --index $IDX"

#!/bin/sh
# Cypher-shaped questions the graph verbs can't phrase.
set -eu
IDX="${VORPAL_INDEX:-.vorpal/index}"

echo "== functions calling into 'main' within 2 hops:"
vorpal query 'MATCH (f:Function)-[:calls*1..2]->(g {name: "main"}) RETURN f.name, f.path LIMIT 20' --index "$IDX"

echo
echo "== call-count leaders that call nothing themselves (leaf hubs):"
vorpal query 'MATCH (f:Function)-[:calls]->(g) WITH g, count(*) AS n WHERE n >= 10 AND NOT EXISTS { (g)-[:calls]->() } RETURN g.name, n ORDER BY n DESC LIMIT 10' --index "$IDX"

echo
echo "== durable ids for scripting (pipe into graph --id / MCP):"
vorpal query 'MATCH (f:Function) RETURN f.name LIMIT 5' --format ids --index "$IDX"

echo
echo "== token-lean output for LLM contexts:"
vorpal query 'MATCH (t:Struct) RETURN t.name, t.path LIMIT 10' --format toon --index "$IDX"

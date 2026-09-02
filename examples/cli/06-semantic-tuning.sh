#!/bin/sh
# Measure the optional ranking features on YOUR queries; write switches from verdicts.
set -eu
IDX="${VORPAL_INDEX:-.vorpal/index}"

cat > /tmp/vorpal-queries.txt <<'Q'
socket buffer allocation => alloc_skb
parse configuration file => parse_config
"retry logic" AND "connection pool"
Q

echo "== dry run (report only, writes nothing):"
vorpal tune --queries /tmp/vorpal-queries.txt --dry-run --index "$IDX"

echo
echo "== to adopt the verdicts for THIS index:  vorpal tune --queries q.txt --index $IDX"
echo "== to add the advanced encoder first:     vorpal enable semantic-f16"
echo "== then compare both orderings per query: vorpal search 'your query' --ranked --index $IDX"

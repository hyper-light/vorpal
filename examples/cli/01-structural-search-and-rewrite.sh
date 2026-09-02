#!/bin/sh
# Structural search & rewrite: match code by AST shape, not text.
set -eu
cd "$(mktemp -d)"

cat > app.ts <<'TS'
function greet(user) {
  console.log(user.name);
  console.log("greeting sent");
  return debugLog(user.id);
}
TS

echo "== every console.log call (pattern match, one capture):"
vorpal run -p 'console.log($ARG)' app.ts

echo
echo "== rewrite them to a logger, applied without prompting:"
vorpal run -p 'console.log($ARG)' -r 'logger.info($ARG)' -U app.ts
cat app.ts

echo
echo "== kind-based match: every call expression, JSON stream output:"
vorpal run --kind call_expression --lang ts --json=stream app.ts

echo
echo "== how did my pattern parse? (debugging gold):"
vorpal run -p 'debugLog($X)' --lang ts --debug-query=ast app.ts

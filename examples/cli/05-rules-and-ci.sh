#!/bin/sh
# A reusable rule: author it, scan with it, emit CI-friendly output.
set -eu
cd "$(mktemp -d)"

cat > src.py <<'PY'
import subprocess
def run(cmd):
    return subprocess.call(cmd, shell=True)   # finding
def fine(cmd):
    return subprocess.call(cmd)
PY

cat > no-shell-true.yml <<'YML'
id: no-shell-true
language: python
severity: error
message: subprocess with shell=True is an injection foothold
note: pass an argv list instead
rule:
  pattern: subprocess.call($$$ARGS)
  has:
    pattern: shell=True
YML

echo "== human report:"
vorpal scan --rule no-shell-true.yml . || true

echo
echo "== GitHub Actions annotations (drop into a workflow step):"
vorpal scan --rule no-shell-true.yml --format github . || true

echo
echo "== SARIF for code-scanning uploads:"
vorpal scan --rule no-shell-true.yml --format sarif . | head -5 || true

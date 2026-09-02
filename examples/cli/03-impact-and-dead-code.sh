#!/bin/sh
# Change-safety workflow: what does my branch touch, and what looks unused?
# Run from inside an indexed git repo.
set -eu
SRC="${1:-.}"
IDX="$SRC/.vorpal/index"
vorpal index "$SRC" --out "$IDX" >/dev/null

echo "== blast radius of uncommitted changes:"
vorpal graph impact --src "$SRC" --index "$IDX"

echo
echo "== blast radius of the whole branch vs main:"
vorpal graph impact --since origin/main --src "$SRC" --index "$IDX" || true

echo
echo "== exported definitions nothing references (candidates, not verdicts):"
vorpal graph dead --exported --no-tests --index "$IDX"

echo
echo "== which files parsed worst (extraction blind spots):"
vorpal graph coverage --index "$IDX"

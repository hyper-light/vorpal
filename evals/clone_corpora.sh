#!/bin/zsh
# Shallow-clone the README's fifteen pinned corpora at their exact commits into
# <dest>/corpora/<name>. Full SHAs are resolved from the abbreviated pins through the GitHub
# API; `git fetch --depth 1 origin <full sha>` is the only fetch shape GitHub accepts for an
# arbitrary commit. Usage: evals/clone_corpora.sh <dest>   (then VORPAL_BENCH_PROFILE=<dest>)
set -u
DEST=${1:?dest}; mkdir -p "$DEST/corpora"
PINS=(
  "llvm-project llvm/llvm-project d37814473"
  "zig ziglang/zig 738d2be9"
  "kotlin JetBrains/kotlin 9f27f51dd"
  "kubernetes kubernetes/kubernetes bce953e8"
  "roslyn dotnet/roslyn 4cac4334"
  "rust rust-lang/rust 5db7f4be8"
  "WordPress WordPress/WordPress c195362"
  "spark apache/spark 06539777"
  "kafka apache/kafka 6e4c555"
  "next.js vercel/next.js 483f8420"
  "ghc ghc/ghc 44d7788f"
  "rails rails/rails 4130768"
  "neovim neovim/neovim d423675"
  "vue-core vuejs/core d63616c"
)
for pin in "${PINS[@]}"; do
  set -- ${=pin}; name=$1; repo=$2; abbrev=$3; dir="$DEST/corpora/$name"
  if [ -f "$dir/.pinned" ] && [ "$(cat "$dir/.pinned")" = "$abbrev" ]; then echo "$name: present"; continue; fi
  rm -rf "$dir"; mkdir -p "$dir"
  sha=$(gh api "repos/$repo/commits/$abbrev" --jq .sha 2>/dev/null)
  if [ -z "$sha" ]; then echo "$name: could not resolve $abbrev via the API"; continue; fi
  ( cd "$dir" && git init -q && git remote add origin "https://github.com/$repo.git" && git fetch -q --depth 1 origin "$sha" && git checkout -q FETCH_HEAD ) 2>&1 | grep -v "^warning\|^hint" | tail -2
  if [ -d "$dir/.git" ] && [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null)" = "$sha" ]; then echo "$abbrev" > "$dir/.pinned"; echo "$name: $sha ($(git -C "$dir" ls-files | wc -l | tr -d ' ') tracked)"; else echo "$name: FAILED"; fi
done
echo CLONES-DONE

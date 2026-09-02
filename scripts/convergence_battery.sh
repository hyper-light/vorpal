#!/usr/bin/env bash
# P4 real-repo convergence battery — the standing gate for the scoped-build slices
# (docs/wip/SUBSECOND.md §P4.5c): vorpal must byte-converge on REAL repositories and
# REAL edit shapes, not just the kernel benchmark and the six-language fixture.
#
# For each repo given (copies — the originals are never touched), under each format
# lane, the battery:
#   1. builds twice from scratch and demands identical generation ids (determinism);
#   2. applies five real edit shapes to the repo's largest source file — touch,
#      top-of-file comment, comment inside the first definition body, literal flip,
#      function append — and after EVERY edit demands the incremental build's
#      generation id equal a from-scratch build of the same tree (byte convergence:
#      the Merkle id names the artifact bytes; the pack_v2 e2e pins Merkle ≡ full
#      rehash);
#   3. reports the incremental wall and whether the scoped paths fired (names.idx
#      hard-link ⇒ respan/cutoff class) — informational, convergence is the gate.
#
# Usage: scripts/convergence_battery.sh [--vorpal <bin>] [--formats "next flat"] <repo>...
# Default lane: next (the shipped default since the flip); pass --formats "next flat" to
# exercise the deprecated flat writer too.
# Exit: non-zero on ANY convergence failure. Work areas live under ${TMPDIR:-/tmp}
# and are removed on exit.
set -euo pipefail

VORPAL_BIN=""
FORMATS="next"
REPOS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --vorpal) VORPAL_BIN="$2"; shift 2 ;;
    --formats) FORMATS="$2"; shift 2 ;;
    *) REPOS+=("$1"); shift ;;
  esac
done
if [ ${#REPOS[@]} -eq 0 ]; then
  echo "usage: $0 [--vorpal <bin>] [--formats \"next flat\"] <repo>..." >&2
  exit 2
fi
if [ -z "$VORPAL_BIN" ]; then
  VORPAL_BIN="$(command -v vorpal || true)"
fi
if [ -z "$VORPAL_BIN" ] || [ ! -x "$VORPAL_BIN" ]; then
  echo "no vorpal binary (pass --vorpal)" >&2
  exit 2
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/vorpal-battery.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
FAIL=0

gen_id() { basename "$(cat "$1/CURRENT")"; }

wall_build() { # $1 format, $2 src, $3 out — prints wall seconds, builds index
  local fmt="$1" src="$2" out="$3" t
  t=$( { /usr/bin/time -p env VORPAL_FORMAT="$fmt" "$VORPAL_BIN" index "$src" --out "$out" >/dev/null; } 2>&1 | awk '/^real/{print $2}' )
  echo "$t"
}

converge() { # $1 fmt, $2 src, $3 live-out, $4 label — incremental vs scratch twin
  local fmt="$1" src="$2" out="$3" label="$4"
  local prior_gen wall inc scratch_out scr prior_names_ino new_names_ino note=""
  prior_gen="$out/gen/$(gen_id "$out")"
  [ -f "$prior_gen/names.idx" ] && prior_names_ino=$(stat -f %i "$prior_gen/names.idx") || prior_names_ino=""
  wall=$(wall_build "$fmt" "$src" "$out")
  inc=$(gen_id "$out")
  scratch_out="$WORK/scratch-twin"
  rm -rf "$scratch_out"
  wall_build "$fmt" "$src" "$scratch_out" >/dev/null
  scr=$(gen_id "$scratch_out")
  if [ -n "$prior_names_ino" ] && [ -f "$out/gen/$inc/names.idx" ]; then
    new_names_ino=$(stat -f %i "$out/gen/$inc/names.idx")
    [ "$prior_names_ino" = "$new_names_ino" ] && note=" [scoped: names.idx linked]"
  fi
  if [ "$inc" = "$scr" ]; then
    echo "    PASS  $label  ${wall}s$note"
  else
    echo "    FAIL  $label  incremental=$inc scratch=$scr"
    FAIL=1
  fi
}

for repo in "${REPOS[@]}"; do
  name=$(basename "$repo")
  src="$WORK/$name"
  # Copy without VCS state; the battery edits the COPY only.
  rsync -a --exclude .git --exclude .hg --exclude node_modules --exclude target \
    --exclude .vorpal --exclude __pycache__ "$repo/" "$src/"
  # Probe file: the largest source file — most nodes, best signal. Deterministic.
  probe=$(find "$src" -type f \( -name '*.rs' -o -name '*.py' -o -name '*.go' \
    -o -name '*.ts' -o -name '*.js' -o -name '*.java' -o -name '*.c' \) \
    -exec stat -f '%z %N' {} + | sort -rn | sed -n '1p' | cut -d' ' -f2-)
  # (sed, not `head -1`: under pipefail a closed pipe SIGPIPEs sort on big file lists)
  if [ -z "$probe" ]; then
    echo "  $name: no source files — skipped"
    continue
  fi
  ext="${probe##*.}"
  case "$ext" in
    py) C='#' ;;
    *) C='//' ;;
  esac
  echo "== $name (probe: ${probe#"$src"/}, $(wc -l < "$probe" | tr -d ' ') lines)"
  cp "$probe" "$WORK/probe.orig"
  # Second probe (S6 two-file shape): the second-largest source file, same filter.
  probe2=$(find "$src" -type f \( -name '*.rs' -o -name '*.py' -o -name '*.go' \
    -o -name '*.ts' -o -name '*.js' -o -name '*.java' -o -name '*.c' \) \
    -exec stat -f '%z %N' {} + | sort -rn | sed -n '2p' | cut -d' ' -f2-)
  [ -n "$probe2" ] && cp "$probe2" "$WORK/probe2.orig"

  for fmt in $FORMATS; do
    echo "  -- format=${fmt:-flat}"
    [ "$fmt" = "flat" ] && fmt=""
    out="$WORK/live-out"
    rm -rf "$out" "$WORK/scratch-twin"
    cp "$WORK/probe.orig" "$probe"

    # Determinism ×2.
    w=$(wall_build "$fmt" "$src" "$out")
    a=$(gen_id "$out")
    rm -rf "$WORK/scratch-twin"
    wall_build "$fmt" "$src" "$WORK/scratch-twin" >/dev/null
    b=$(gen_id "$WORK/scratch-twin")
    if [ "$a" = "$b" ]; then
      echo "    PASS  scratch-determinism  ${w}s cold"
    else
      echo "    FAIL  scratch-determinism  $a vs $b"
      FAIL=1
    fi

    # S1: touch — same bytes, fresh stamp.
    touch "$probe"
    converge "$fmt" "$src" "$out" "S1 touch"

    # S2: comment at top of file — whole-file span shift.
    printf '%s vorpal battery: top comment\n' "$C" | cat - "$probe" > "$probe.tmp" && mv "$probe.tmp" "$probe"
    converge "$fmt" "$src" "$out" "S2 top-comment"

    # S3: comment INSIDE the first definition body — the respan class where eligible.
    if [ "$ext" = "py" ]; then
      awk -v c="$C" '!done && /^[[:space:]]*def [A-Za-z_].*:[[:space:]]*$/ {print; ind=$0; sub(/[^ ].*$/,"",ind); print ind "    " c " vorpal battery: body comment"; done=1; next} {print}' \
        "$probe" > "$probe.tmp" && mv "$probe.tmp" "$probe"
    else
      awk -v c="$C" '!done && /\{[[:space:]]*$/ {print; print "\t" c " vorpal battery: body comment"; done=1; next} {print}' \
        "$probe" > "$probe.tmp" && mv "$probe.tmp" "$probe"
    fi
    converge "$fmt" "$src" "$out" "S3 body-comment"

    # S4: literal flip — a real body change (first standalone integer literal bumped).
    perl -0pi -e 's/\b1\b/2/ if !$done++;' "$probe" 2>/dev/null || \
      printf '%s vorpal battery: flip fallback\n' "$C" >> "$probe"
    converge "$fmt" "$src" "$out" "S4 literal-flip"

    # S5: function append — new definition, new resolution work.
    case "$ext" in
      rs)   printf '\npub fn vorpal_battery_probe(x: i32) -> i32 {\n    x + 41\n}\n' >> "$probe" ;;
      go)   printf '\nfunc vorpalBatteryProbe(x int) int {\n\treturn x + 41\n}\n' >> "$probe" ;;
      py)   printf '\n\ndef vorpal_battery_probe(x):\n    return x + 41\n' >> "$probe" ;;
      ts)   printf '\nexport function vorpalBatteryProbe(x: number): number {\n  return x + 41;\n}\n' >> "$probe" ;;
      js)   printf '\nfunction vorpalBatteryProbe(x) {\n  return x + 41;\n}\n' >> "$probe" ;;
      c)    printf '\nstatic int vorpal_battery_probe(int x) {\n  return x + 41;\n}\n' >> "$probe" ;;
      java) : ;; # no legal top-level function — S1–S4 cover this probe
    esac
    converge "$fmt" "$src" "$out" "S5 fn-append"

    # S6: TWO files edited at once (S2 multi-file sessions) — a top comment in the probe
    # AND in the second-largest source file, one build. The compose class depends on the
    # repo (respan when span-only per file; defs-stable otherwise); the gate is, as
    # always, convergence.
    if [ -n "$probe2" ]; then
      printf '%s vorpal battery: two-file A\n' "$C" | cat - "$probe" > "$probe.tmp" && mv "$probe.tmp" "$probe"
      ext2="${probe2##*.}"
      case "$ext2" in
        py) C2='#' ;;
        *) C2='//' ;;
      esac
      printf '%s vorpal battery: two-file B\n' "$C2" | cat - "$probe2" > "$probe2.tmp" && mv "$probe2.tmp" "$probe2"
      converge "$fmt" "$src" "$out" "S6 two-file"
      cp "$WORK/probe2.orig" "$probe2"
    fi
  done
  cp "$WORK/probe.orig" "$probe"
done

if [ "$FAIL" -ne 0 ]; then
  echo "BATTERY: FAIL"
  exit 1
fi
echo "BATTERY: PASS"

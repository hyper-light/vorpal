#!/bin/zsh
# Interleaved A/B of the indexed kernel scan: cold inbox (loose bank emptied) and warm inbox.
# The rule is the profile pass's reconstructed kmalloc rule (evals/scan-rule.yml).
# Recorded runs: docs/wip/BENCHMARKS.md ("Profile findings: seven fixes").
# usage: scan_ab.sh <A> <B> <kernel-copy> <rule.yml> <reps>
A=$1; B=$2; SRC=$3; RULE=$4; REPS=${5:-3}
INBOX=$SRC/.vorpal/index/products
for i in $(seq 1 $REPS); do
  for arm in A B; do
    BIN=$A; [ $arm = B ] && BIN=$B
    rm -rf $INBOX; mkdir -p $INBOX
    t0=$(python3 -c 'import time; print(time.time())')
    $BIN scan --rule $RULE --json=stream --inspect summary $SRC </dev/null >/tmp/scan-out.$$ 2>/tmp/scan-err.$$
    t1=$(python3 -c 'import time; print(time.time())')
    n=$(grep -c '"ruleId"' /tmp/scan-out.$$ 2>/dev/null); files=$(ls $INBOX | wc -l | tr -d ' ')
    echo "scan cold-inbox arm=$arm rep=$i wall=$(python3 -c "print(round($t1-$t0,3))") diagnostics=$n banked=$files"
    t0=$(python3 -c 'import time; print(time.time())')
    $BIN scan --rule $RULE --json=stream --inspect summary $SRC </dev/null >/tmp/scan-out.$$ 2>/tmp/scan-err.$$
    t1=$(python3 -c 'import time; print(time.time())')
    n=$(grep -c '"ruleId"' /tmp/scan-out.$$ 2>/dev/null)
    echo "scan warm-inbox arm=$arm rep=$i wall=$(python3 -c "print(round($t1-$t0,3))") diagnostics=$n"
  done
done
rm -f /tmp/scan-out.$$ /tmp/scan-err.$$

#!/usr/bin/env bash
# 100 consecutive runs of the parity determinism test on a frozen build.
#
# Produces: parity-100-runs.txt
#
# Run `cargo build --features ship --tests --bins` to completion FIRST. This
# script never builds; it counts any `Compiling` line as a mid-loop rebuild and
# reports the count, so the capture can state whether the build stayed frozen.
#
# <REPO> is the checkout root; <OUT> is where failure logs are written.
cd <REPO> || exit 1
OUT=<OUT>
pass=0; fail=0; rebuilt=0
echo "commit:      $(git rev-parse HEAD)"
echo "git status:  $(git status --porcelain | wc -l) modified files"
echo "date:        $(date -Is)"
echo "command:     cargo test --features ship --test binary_parity demo_outcome"
echo "runs:        100 (build frozen: cargo build --features ship --tests --bins ran to completion first)"
echo "----"
for i in $(seq 1 100); do
  log=$(cargo test --features ship --test binary_parity demo_outcome 2>&1)
  if grep -q '^ *Compiling' <<<"$log"; then rebuilt=$((rebuilt+1)); echo "run $i: REBUILD DETECTED"; fi
  if grep -q 'test result: ok. 1 passed' <<<"$log"; then
    pass=$((pass+1)); printf 'run %3d: PASS\n' "$i"
  else
    fail=$((fail+1)); printf 'run %3d: FAIL\n' "$i"
    printf '%s\n' "$log" > "$OUT/failure-run-$i.txt"
  fi
done
echo "----"
echo "passed:  $pass / 100"
echo "failed:  $fail / 100"
echo "rebuilds detected mid-loop: $rebuilt"
echo "finished: $(date -Is)"

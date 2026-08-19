#!/usr/bin/env bash
# Run every report in the kit against the committed sample, and fail if any
# report is empty, errors, or silently stops finding the planted facts.
#
# This is the kit's own regression test. CI does not run scripts/** (see the
# path filter in .github/workflows/ci.yml), so this is a manual gate — run it
# after touching any script here, and before quoting any report in evidence/.
#
#   scripts/observability/verify.sh
#
# It regenerates the sample from make_sample.py first, so a drift between the
# generator and the committed sample also shows up as a failure.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

py=${PYTHON:-python3}
ledger="$here/sample/calls.jsonl"
fail=0

step() { printf '\n\033[1m--- %s\033[0m\n' "$1"; }
check() {
  local what="$1" haystack="$2" needle="$3"
  if grep -qF -- "$needle" <<<"$haystack"; then
    printf '    ok   %s\n' "$what"
  else
    printf '    FAIL %s (expected to find %q)\n' "$what" "$needle"
    fail=1
  fi
}

step "the committed sample matches its generator"
"$py" "$here/make_sample.py" >"$work/regenerated.jsonl"
if diff -q "$ledger" "$work/regenerated.jsonl" >/dev/null; then
  printf '    ok   sample/calls.jsonl is make_sample.py output\n'
else
  printf '    FAIL sample/calls.jsonl has drifted from make_sample.py\n'
  diff "$ledger" "$work/regenerated.jsonl" | head -20 || true
  fail=1
fi

step "recall_first.py (metric 1)"
out="$("$py" "$here/recall_first.py" "$ledger")"
echo "$out"
check "reports the planted non-compliant agent" "$out" "agent-beta"
check "computes an overall compliance figure" "$out" "66.7%"
check "sees the planted serve restart" "$out" "1 serve restart"
check "surfaces the dropped-line count" "$out" "LINES DROPPED"
check "reports the torn tail line" "$out" "UNPARSEABLE"

step "dedup_rate.py (metric 2)"
out="$("$py" "$here/dedup_rate.py" "$ledger")"
echo "$out"
check "buckets by day" "$out" "2026-08-18"
check "sees the planted convergence" "$out" "rising"
check "excludes the failed derive" "$out" "1 derive call(s) failed"

step "score_bands.py (metric 4)"
out="$("$py" "$here/score_bands.py" "$ledger")"
echo "$out"
check "quotes G1's true_recall band" "$out" "0.4599"
check "catches the planted floor mask" "$out" "Floor masking (metric 4's recurrence check): 1"
check "counts the expansion-only hit" "$out" "had no phase-1 legs"

step "warnings.py (metric 5)"
out="$("$py" "$here/warnings.py" "$ledger")"
echo "$out"
check "counts both blast-radius warnings" "$out" "blast-radius (load_bearing) warnings fired: 2"
check "flags the one the token budget cut" "$out" "CUT BY TOKEN BUDGET"
check "names the warned concept" "$out" "pagination contract"

step "warnings.py --repo (the git join)"
out="$("$py" "$here/warnings.py" --repo "$repo" --window-minutes 60 "$ledger")"
check "performs the join without erroring" "$out" "Git join"
check "labels the join as correlation only" "$out" "CORRELATION ONLY"

step "duplicates.py (metric 3)"
"$py" "$here/make_sample.py" --store "$work/sample.db"
out="$("$py" "$here/duplicates.py" --store "$work/sample.db" --ledger "$ledger")"
echo "$out"
check "finds the should-have-merged pair" "$out" "AT OR ABOVE the merge threshold (0.85): 1 pair"
check "finds the in-band pair" "$out" "IN THE BAND [0.65, 0.85): 1 pair"
check "warns about the unembedded concept" "$out" "CANNOT be scanned"

step "every report also emits JSON"
for script in recall_first dedup_rate score_bands warnings; do
  if "$py" "$here/$script.py" --json "$ledger" | "$py" -c 'import json,sys; json.load(sys.stdin)'; then
    printf '    ok   %s --json parses\n' "$script"
  else
    printf '    FAIL %s --json did not parse\n' "$script"
    fail=1
  fi
done
if "$py" "$here/duplicates.py" --json --store "$work/sample.db" \
   | "$py" -c 'import json,sys; json.load(sys.stdin)'; then
  printf '    ok   duplicates --json parses\n'
else
  printf '    FAIL duplicates --json did not parse\n'
  fail=1
fi

printf '\n'
if [ "$fail" -eq 0 ]; then
  printf '\033[1mALL CHECKS PASSED\033[0m\n'
else
  printf '\033[1mFAILURES ABOVE\033[0m\n'
fi
exit "$fail"

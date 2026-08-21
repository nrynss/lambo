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
check "reads the recency floor off the ledger" "$out" "recency floor observed in the ledger: 0.35"
if "$py" "$here/score_bands.py" --floor 0.9 "$ledger" >/dev/null 2>&1; then
  printf '    FAIL --floor was accepted; the floor is read off the ledger, not supplied\n'
  fail=1
else
  printf '    ok   --floor is gone (the ledger states the floor)\n'
fi

step "blast_radius.py (metric 5)"
out="$("$py" "$here/blast_radius.py" "$ledger")"
echo "$out"
check "counts both blast-radius warnings" "$out" "blast-radius (load_bearing) warnings fired: 2"
check "flags the one the token budget cut" "$out" "BLOCK CUT BY TOKEN BUDGET"
check "says the warning line was still delivered" "$out" "the WARNING LINE still reached the agent"
check "names the warned concept" "$out" "pagination contract"
# The budget-gated half: the same Canonical concept, returned twice, marker
# rendered once. A canonical_marker computed over every returned hit would make
# this line read 2 and the ledger would be claiming a marker nobody received.
check "separates canonical hits returned from markers rendered" "$out" \
  "Canonical hits: 2 recall(s) returned one, 1 rendered the [canonical] marker"
check "reports the canonical hit the budget cut" "$out" \
  "1 Canonical hit(s) were CUT BY THE TOKEN BUDGET"

step "blast_radius.py --repo (the git join)"
out="$("$py" "$here/blast_radius.py" --repo "$repo" --window-minutes 60 "$ledger")"
check "performs the join without erroring" "$out" "Git join"
check "labels the join as correlation only" "$out" "CORRELATION ONLY"

step "duplicates.py (metric 3)"
"$py" "$here/make_sample.py" --store "$work/sample.db"
out="$("$py" "$here/duplicates.py" --store "$work/sample.db" --ledger "$ledger")"
echo "$out"
check "finds the should-have-merged pair" "$out" "AT OR ABOVE the merge threshold (0.85): 1 pair"
check "finds the in-band pair" "$out" "IN THE BAND [0.65, 0.85): 1 pair"
check "warns about the unembedded concept" "$out" "CANNOT be scanned"

# The committed sample is deliberately clean-v1: these three cases are generated
# here instead, so they cannot perturb the planted facts every check above reads.
step "an unknown schema version warns loudly and is still read"
cat >"$work/mixed.jsonl" <<'MIXED'
{"v":1,"ts":"2026-08-18T09:00:00+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":10,"created":2,"matched":1}
{"v":2,"ts":"2026-08-18T09:01:00+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":10,"created":1,"matched":9}
{"ts":"2026-08-18T09:02:00+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":10}
MIXED
out="$("$py" "$here/dedup_rate.py" --bucket all "$work/mixed.jsonl")"
echo "$out"
check "names the unknown version" "$out" "UNKNOWN SCHEMA VERSION"
check "says which versions it saw" "$out" "saw 2, None"
check "still reports the v1 lines (warn, do not refuse)" "$out" "TOTAL"
check "counts the fact-less derive separately from a zero" "$out" \
  "1 SUCCESSFUL derive call(s) carried NO created/matched facts"
if "$py" "$here/dedup_rate.py" --json "$work/mixed.jsonl" \
   | "$py" -c 'import json,sys
d = json.load(sys.stdin)
u = d["ledger_schema"]["unknown_version_lines"]
assert len(u) == 2, u
assert d["derive_calls_without_facts"] == 1, d["derive_calls_without_facts"]'; then
  printf '    ok   --json carries the schema warning too\n'
else
  printf '    FAIL --json did not carry the schema provenance\n'
  fail=1
fi

step "a J3-shaped derive line (async ack) reports n/a rather than a false zero"
# **J3-R1-12, answered here rather than in the committed sample, deliberately.**
# Since J3 an MCP-driven `lambo_derive` is acked before the write happens, so its
# call line carries `concepts_requested` / `admitted` / `receipt` and NO
# created/matched facts — those moved to the receipt (see the README's metric-2
# note). Without a fixture, all the checks above could pass without one of them
# ever seeing the shape the README now documents.
#
# It is planted here and not in `sample/calls.jsonl` for the reason stated above
# the mixed-version fixture: the committed sample is deliberately clean-v1, and
# fact-less lines in it would move the very numbers the metric-1 and metric-2
# checks read (the 66.7% compliance figure, the "rising" convergence, the
# per-day rates). A fixture that perturbs the plants it shares a file with
# defends one schema by weakening five checks. This defends the schema and
# leaves them alone.
cat >"$work/j3-async.jsonl" <<'J3ASYNC'
{"v":1,"ts":"2026-08-20T09:00:00+00:00","kind":"call","tool":"lambo_recall","agent_id":"agent-alpha","outcome":"ok","duration_us":900,"query":"what did we decide","top_k":8,"hit_count":0,"hits":[]}
{"v":1,"ts":"2026-08-20T09:00:01+00:00","kind":"call","tool":"lambo_derive","agent_id":"agent-alpha","outcome":"ok","duration_us":48,"concepts_requested":3,"admitted":true,"receipt":"lwr1.5b7d1c9e2f0a4413.1a06d6c1a00.1"}
{"v":1,"ts":"2026-08-20T09:00:02+00:00","kind":"call","tool":"lambo_derive","agent_id":"agent-alpha","outcome":"ok","duration_us":51,"concepts_requested":2,"admitted":true,"receipt":"lwr1.5b7d1c9e2f0a4413.1a06d6c1a3c.2"}
{"v":1,"ts":"2026-08-20T09:00:03+00:00","kind":"call","tool":"lambo_record_action","agent_id":"agent-alpha","outcome":"ok","duration_us":44,"admitted":true,"receipt":"lwr1.5b7d1c9e2f0a4413.1a06d6c1a78.3"}
{"v":1,"ts":"2026-08-20T09:00:04+00:00","kind":"call","tool":"lambo_derive","agent_id":"agent-alpha","outcome":"ok","duration_us":47,"concepts_requested":4,"admitted":false,"receipt":"lwr1.5b7d1c9e2f0a4413.1a06d6c1ab4.4"}
J3ASYNC
out="$("$py" "$here/dedup_rate.py" --bucket all "$work/j3-async.jsonl")"
echo "$out"
check "reports no rate at all rather than 0.000" "$out" "n/a"
check "counts every async derive as fact-less" "$out" \
  "3 SUCCESSFUL derive call(s) carried NO created/matched facts"
check "names the async ack as the likely cause" "$out" "acknowledged asynchronously"
check "points at the receipt, not at a renamed field" "$out" "lambo_stats(receipt=...)"
if "$py" "$here/dedup_rate.py" --json "$work/j3-async.jsonl" \
   | "$py" -c 'import json,sys
d = json.load(sys.stdin)
assert d["derive_calls_without_facts"] == 3, d["derive_calls_without_facts"]
t = d["totals"]
assert t["created"] == 0 and t["matched"] == 0, t
assert d["totals"]["dedup_rate"] is None, d["totals"]["dedup_rate"]'; then
  printf '    ok   --json reports a null rate, never a zero\n'
else
  printf '    FAIL --json turned "no facts" into a zero rate\n'
  fail=1
fi
# And metric 1 is unaffected by the shape: recall-then-write is still legible
# when the write facts live on a receipt.
out="$("$py" "$here/recall_first.py" "$work/j3-async.jsonl")"
check "metric 1 still scores an async-acked run" "$out" "100.0%"

step "chrono's nine-digit fractional seconds parse (no Python floor)"
cat >"$work/nanos.jsonl" <<'NANOS'
{"v":1,"ts":"2026-08-18T09:00:00.123456789+00:00","kind":"call","tool":"lambo_recall","agent_id":"a","outcome":"ok","duration_us":10,"query":"q","top_k":8,"hit_count":0,"hits":[]}
{"v":1,"ts":"2026-08-18T09:00:01.987654321+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":10,"created":1,"matched":0}
{"v":1,"ts":"2026-08-18T09:00:02+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":10,"created":0,"matched":1}
{"v":1,"ts":"2026-08-18T09:00:03.123+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":10,"created":0,"matched":1}
NANOS
out="$("$py" "$here/recall_first.py" "$work/nanos.jsonl")"
echo "$out"
check "parses 0/3/9-digit stamps without erroring" "$out" "metric 1"
check "and scores the run it found" "$out" "100.0%"

step "a non-zero queue depth is read from the NEWEST heartbeat, not the largest"
# The committed sample is queued=0 throughout (a healthy writer keeps nothing
# queued), so `header()`'s `queued:` line and its `elif queued:` completeness
# branch are unreachable there. This fixture is the only place they run. The
# older heartbeat carries the LARGER depth deliberately: queue depth is a gauge,
# so the answer must be 3 (newest) and never 9 (maximum or oldest), which pins
# `queued_lines()`'s sort direction and its newest-not-maximum semantics at once.
# File order and stamp order are deliberately OPPOSED — the newer beat is written
# first — so that deleting the `ts` sort and reading file order instead (the
# natural simplification, since `load()` reads lines in order) also goes red.
cat >"$work/queued.jsonl" <<'QUEUED'
{"v":1,"ts":"2026-08-18T09:00:00+00:00","kind":"call","tool":"lambo_recall","agent_id":"a","outcome":"ok","duration_us":10,"query":"q","top_k":8,"hit_count":1,"hits":[]}
{"v":1,"ts":"2026-08-18T09:00:20+00:00","kind":"stats","uptime_secs":20,"version":"0.2.2","git_sha":"aaaa111","stats":{"node_count":4,"ledger_written_lines":7,"ledger_dropped_lines":0,"ledger_dropped_channel_full":0,"ledger_dropped_write_failed":0,"ledger_queued_lines":3}}
{"v":1,"ts":"2026-08-18T09:00:10+00:00","kind":"stats","uptime_secs":10,"version":"0.2.2","git_sha":"aaaa111","stats":{"node_count":4,"ledger_written_lines":1,"ledger_dropped_lines":0,"ledger_dropped_channel_full":0,"ledger_dropped_write_failed":0,"ledger_queued_lines":9}}
QUEUED
out="$("$py" "$here/recall_first.py" "$work/queued.jsonl")"
echo "$out"
check "reports the newest queue depth, not the largest" "$out" "queued:   3 LINE(S) STILL QUEUED"
check "does not call a backlogged ledger complete" "$out" \
  "dropped:  0 — but the ledger is not complete; see \`queued\`"
if "$py" "$here/recall_first.py" --json "$work/queued.jsonl" \
   | "$py" -c 'import json,sys
d = json.load(sys.stdin)
q = d["ledger_schema"]["queued_lines"]
assert q == 3, q'; then
  printf '    ok   --json carries the newest queue depth\n'
else
  printf '    FAIL --json did not carry the newest queue depth\n'
  fail=1
fi

step "every report also emits JSON"
for script in recall_first dedup_rate score_bands blast_radius; do
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

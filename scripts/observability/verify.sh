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

step "the J4 completion join restores metric 2 for an async-acked session"
# **JE2E-5.** J3 moved the derive facts off the call line and J4 put
# created/matched back on a `kind:"completion"` line joined by `receipt`. Without
# a fixture, nothing here exercises the join: the step above proves the fact-less
# case still reports `n/a`, and this one proves the joined case reports a real
# rate — the two halves of the same claim, and the second one is what "restores
# the declared metric-2 regression" actually means.
#
# **The fixture opposes the shape it exists to check** (the J0-R2 lesson: a
# fixture that agrees with the thing it tests cannot see it). Three ways:
#
#   1. The completion lines are written BEFORE their call lines in the file and
#      OUT of receipt order, so a join that read file order, or paired the Nth
#      completion with the Nth call, would produce the wrong numbers rather than
#      no numbers. The planted counts differ per receipt so a mispairing shows.
#   2. Receipt `…4` is deferred and then applied_after_restart, in that order,
#      with DIFFERENT counts — so a join taking the FIRST completion per receipt
#      reads a deferred line with no counts and drops the call, while one taking
#      the last reads the real outcome. Terminal-state selection is asserted.
#   3. Receipt `…5` has a `failed` completion **carrying counts** and `…6` has no
#      completion at all: both must stay fact-LESS. The counts on the failed line
#      are deliberately absurd (40/40) and are the only thing that can see the
#      applied-state guard — nothing lambo emits today puts counts on a failed
#      line, so without them a join that read ANY state would still pass, because
#      the key check alone would find nothing to add. A fixture that cannot fail
#      is not a fixture (J0-R2), and this one failed exactly that way once.
#   4. Receipt `…7` is the same trap one state over (JE2E-R2-7): its TERMINAL
#      completion is `deferred`, carrying absurd counts (90/90), shadowing an
#      earlier `applied` line that carries the real ones (7/7). Round 2 showed
#      that widening APPLIED_STATES to admit `deferred` survived green, because
#      `…4`'s deferred line carries no counts AND is shadowed by a later terminal
#      line — nothing opposed the state. This receipt opposes it from both
#      directions at once: admitting `deferred` reads 90/90 (the totals move),
#      and taking the first completion per receipt rather than the terminal one
#      reads 7/7 (the totals move the other way). The terminal state here is a
#      real shape: a write applied in one process, then a LATER serve deferring
#      a replay it could not drain, both on one shared ledger.
#
# Planted arithmetic: created 3+1+5 = 9, matched 1+2+0 = 3 across the three
# joinable calls, so the rate is 3/12 = 0.250 — a number no zero-fold and no
# mispairing produces.
#
# **#112: `r-1` is a 0.4.0+ completion** carrying `semantic_merged` 2 and
# `reinforced` 1; `r-2` and `r-4` are pre-#12 shapes carrying neither. So the
# joined `sem.merged` is 2 from one call with 2 calls missing — a lower bound,
# rendered `2+`, never a bare 2 and never 0. The failed `r-5` and the shadowed
# deferred `r-7` carry absurd values (40, 90) for the same reason as their
# counts: a join that read them would move these totals too.
cat >"$work/j4-completion.jsonl" <<'J4DONE'
{"v":1,"ts":"2026-08-21T10:00:10+00:00","kind":"completion","agent_id":"agent-alpha","receipt":"r-2","state":"applied","created_count":1,"matched_count":2}
{"v":1,"ts":"2026-08-21T10:00:09+00:00","kind":"completion","agent_id":"agent-alpha","receipt":"r-1","state":"applied","created_count":3,"matched_count":1,"semantic_merged":2,"reinforced":1}
{"v":1,"ts":"2026-08-21T10:00:11+00:00","kind":"completion","agent_id":"agent-alpha","receipt":"r-4","state":"deferred","reason":"close_drain_exceeded"}
{"v":1,"ts":"2026-08-21T10:00:12+00:00","kind":"completion","agent_id":"agent-alpha","receipt":"r-4","state":"applied_after_restart","created_count":5,"matched_count":0}
{"v":1,"ts":"2026-08-21T10:00:13+00:00","kind":"completion","agent_id":"agent-alpha","receipt":"r-5","state":"failed","error":"embedding error (the detail was logged server-side)","created_count":40,"matched_count":40,"semantic_merged":40,"reinforced":40}
{"v":1,"ts":"2026-08-21T10:00:14+00:00","kind":"completion","agent_id":"agent-alpha","receipt":"r-7","state":"applied","created_count":7,"matched_count":7}
{"v":1,"ts":"2026-08-21T10:00:15+00:00","kind":"completion","agent_id":"agent-alpha","receipt":"r-7","state":"deferred","reason":"close_drain_exceeded","created_count":90,"matched_count":90,"semantic_merged":90,"reinforced":90}
{"v":1,"ts":"2026-08-21T10:00:00+00:00","kind":"call","tool":"lambo_recall","agent_id":"agent-alpha","outcome":"ok","duration_us":900,"query":"what did we decide","top_k":8,"hit_count":0,"hits":[]}
{"v":1,"ts":"2026-08-21T10:00:01+00:00","kind":"call","tool":"lambo_derive","agent_id":"agent-alpha","outcome":"ok","duration_us":48,"concepts_requested":4,"admitted":true,"receipt":"r-1"}
{"v":1,"ts":"2026-08-21T10:00:02+00:00","kind":"call","tool":"lambo_derive","agent_id":"agent-alpha","outcome":"ok","duration_us":51,"concepts_requested":3,"admitted":true,"receipt":"r-2"}
{"v":1,"ts":"2026-08-21T10:00:03+00:00","kind":"call","tool":"lambo_record_action","agent_id":"agent-alpha","outcome":"ok","duration_us":44,"admitted":true,"receipt":"r-3"}
{"v":1,"ts":"2026-08-21T10:00:04+00:00","kind":"call","tool":"lambo_derive","agent_id":"agent-alpha","outcome":"ok","duration_us":47,"concepts_requested":5,"admitted":true,"receipt":"r-4"}
{"v":1,"ts":"2026-08-21T10:00:05+00:00","kind":"call","tool":"lambo_derive","agent_id":"agent-alpha","outcome":"ok","duration_us":47,"concepts_requested":2,"admitted":true,"receipt":"r-5"}
{"v":1,"ts":"2026-08-21T10:00:06+00:00","kind":"call","tool":"lambo_derive","agent_id":"agent-alpha","outcome":"ok","duration_us":47,"concepts_requested":2,"admitted":true,"receipt":"r-6"}
{"v":1,"ts":"2026-08-21T10:00:08+00:00","kind":"call","tool":"lambo_derive","agent_id":"agent-alpha","outcome":"ok","duration_us":47,"concepts_requested":2,"admitted":true,"receipt":"r-7"}
{"v":1,"ts":"2026-08-21T10:00:07+00:00","kind":"completion","agent_id":"agent-alpha","receipt":"r-3","state":"applied","created_count":2,"matched_count":0}
J4DONE
out="$("$py" "$here/dedup_rate.py" --bucket all "$work/j4-completion.jsonl")"
echo "$out"
check "counts the completion lines in the header" "$out" "8 completion"
check "reports a real rate for an async-acked session" "$out" "0.250"
check "says the facts were joined from completion lines" "$out" \
  "3 derive call(s) were acknowledged asynchronously"
check "warns that sem.merged is an undercount, not a zero" "$out" \
  "undercount, not a zero"
check "marks the partial sem.merged as a lower bound" "$out" "2+"
check "the failed, deferred and record-less derives stay fact-less" "$out" \
  "3 SUCCESSFUL derive call(s) carried NO created/matched facts"
check "and the message names the replay as a cause" "$out" "still owed a replay"
if "$py" "$here/dedup_rate.py" --json "$work/j4-completion.jsonl" \
   | "$py" -c 'import json,sys
d = json.load(sys.stdin)
t = d["totals"]
# 3 + 1 + 5 created, 1 + 2 + 0 matched. Any mispairing or zero-fold moves these.
assert t["created"] == 9, t
assert t["matched"] == 3, t
assert abs(t["dedup_rate"] - 0.25) < 1e-9, t
# The terminal completion won: r-4 contributed 5/0 (applied_after_restart), not
# the 0/0 its earlier `deferred` line would have implied.
assert d["derive_facts_from_completion"] == 3, d["derive_facts_from_completion"]
assert d["derive_facts_from_line"] == 0, d["derive_facts_from_line"]
assert d["derive_calls_without_facts"] == 3, d["derive_calls_without_facts"]
# record_action rides the same join, or the store cross-check reports a phantom
# surplus for every async session.
assert d["record_action_created"] == 2, d["record_action_created"]
assert d["ledger_schema"]["completion_lines"] == 8, d["ledger_schema"]
# #112: r-1 carried the post-#12 keys, r-2 and r-4 did not -> partial sums.
assert t["semantic_merged"] == 2 and t["semantic_merged_missing"] == 2, t
assert t["reinforced"] == 1 and t["reinforced_missing"] == 2, t'; then
  printf '    ok   --json carries the joined totals, the terminal state, and the split\n'
else
  printf '    FAIL --json did not carry the completion join correctly\n'
  fail=1
fi
# duplicates.py joins through the same helper, and must NOT print its
# "a zero means the vector merge never fired" reading over an incomplete figure.
out="$("$py" "$here/duplicates.py" --store "$work/sample.db" \
      --ledger "$work/j4-completion.jsonl")"
check "duplicates.py joins the same counts" "$out" "derives created 9, matched 3"
check "duplicates.py joins semantic_merged when present (#112)" "$out" \
  "semantically merged 2+ (INCOMPLETE"
check "duplicates.py suppresses the zero-means-never-fired reading" "$out" \
  "does NOT apply to this run"

step "#112: sem.merged / reinf. are n/a before 0.4.0, a lower bound across the upgrade"
# The completion join reads `semantic_merged` / `reinforced` only when the line
# carries them (#12, 0.4.0). Two fixtures, because the three states — n/a, a
# lower bound, an exact figure — have to be told apart both per bucket and in
# TOTAL, and a TOTAL can only be n/a if no bucket contributed.
#
# `pre12`: every completion is the pre-#12 shape. Before #112 this rendered a
# confident `0` per bucket and in TOTAL.
cat >"$work/pre12.jsonl" <<'PRE12'
{"v":1,"ts":"2026-08-22T09:00:00+00:00","kind":"completion","agent_id":"a","receipt":"p-1","state":"applied","created_count":2,"matched_count":1}
{"v":1,"ts":"2026-08-23T09:00:00+00:00","kind":"completion","agent_id":"a","receipt":"p-2","state":"applied","created_count":0,"matched_count":3}
{"v":1,"ts":"2026-08-22T08:59:59+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":40,"concepts_requested":3,"admitted":true,"receipt":"p-1"}
{"v":1,"ts":"2026-08-23T08:59:59+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":40,"concepts_requested":3,"admitted":true,"receipt":"p-2"}
PRE12
out="$("$py" "$here/dedup_rate.py" "$work/pre12.jsonl")"
echo "$out"
check "says sem.merged is unavailable, not zero" "$out" \
  '`sem.merged` is n/a: none of the 2 derive call(s) with facts carried `semantic_merged`'
check "and the same for reinforced" "$out" \
  '`reinf.` is n/a: none of the 2 derive call(s) with facts carried `reinforced`'
if "$py" -c 'import sys
rows = [l.split() for l in sys.stdin.read().splitlines()
        if l.startswith(("2026-08-22", "2026-08-23", "TOTAL"))]
assert len(rows) == 3, rows
for r in rows:
    # bucket calls created matched sem.merged reinf. dedup
    assert r[4:6] == ["n/a", "n/a"], r' <<<"$out"; then
  printf '    ok   every bucket and the TOTAL render n/a, never 0\n'
else
  printf '    FAIL a pre-#12 bucket or TOTAL rendered sem.merged / reinf. as a number\n'
  fail=1
fi
if "$py" "$here/dedup_rate.py" --json "$work/pre12.jsonl" \
   | "$py" -c 'import json,sys
d = json.load(sys.stdin)
for row in d["buckets"] + [d["totals"]] + list(d["agents"].values()):
    assert row["semantic_merged"] is None, row
    assert row["reinforced"] is None, row
t = d["totals"]
assert t["created"] == 2 and t["matched"] == 4, t
assert t["semantic_merged_missing"] == 2, t'; then
  printf '    ok   --json says null (absent), not 0, per bucket, per agent and in totals\n'
else
  printf '    FAIL --json folded an absent semantic_merged into a number\n'
  fail=1
fi

# `upgrade`: a ledger spanning the 0.4.0 upgrade. Day 1 is pre-#12 only (n/a)
# and also holds a fact-less derive with no completion, which must NOT count as
# "missing semantic_merged" (it is missing everything, and is reported once on
# its own line). Day 2 mixes one 0.4.0 completion with one pre-#12 completion
# (a lower bound). Day 3 is 0.4.0 only plus a CLI line-carried derive, and its
# `semantic_merged` really is 0 — the one bucket where `0` is the right answer.
cat >"$work/upgrade.jsonl" <<'UPGRADE'
{"v":1,"ts":"2026-08-22T09:00:00+00:00","kind":"completion","agent_id":"a","receipt":"u-1","state":"applied","created_count":2,"matched_count":1}
{"v":1,"ts":"2026-08-23T09:00:00+00:00","kind":"completion","agent_id":"a","receipt":"u-2","state":"applied","created_count":1,"matched_count":1,"semantic_merged":3,"reinforced":0}
{"v":1,"ts":"2026-08-23T09:01:00+00:00","kind":"completion","agent_id":"a","receipt":"u-3","state":"applied","created_count":0,"matched_count":2}
{"v":1,"ts":"2026-08-24T09:00:00+00:00","kind":"completion","agent_id":"a","receipt":"u-4","state":"applied","created_count":1,"matched_count":0,"semantic_merged":0,"reinforced":2}
{"v":1,"ts":"2026-08-22T08:59:59+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":40,"concepts_requested":3,"admitted":true,"receipt":"u-1"}
{"v":1,"ts":"2026-08-22T09:30:00+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":40,"concepts_requested":3,"admitted":true,"receipt":"u-6"}
{"v":1,"ts":"2026-08-23T08:59:59+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":40,"concepts_requested":2,"admitted":true,"receipt":"u-2"}
{"v":1,"ts":"2026-08-23T09:00:59+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":40,"concepts_requested":2,"admitted":true,"receipt":"u-3"}
{"v":1,"ts":"2026-08-24T08:59:59+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":40,"concepts_requested":1,"admitted":true,"receipt":"u-4"}
{"v":1,"ts":"2026-08-24T09:30:00+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":900,"created":0,"matched":1,"semantic_merged":0,"reinforced":0}
UPGRADE
out="$("$py" "$here/dedup_rate.py" "$work/upgrade.jsonl")"
echo "$out"
check "labels the partial figure a lower bound" "$out" "LOWER BOUND, an undercount, not a zero"
check "still reports the fact-less derive on its own line" "$out" \
  "1 SUCCESSFUL derive call(s) carried NO created/matched facts"
if "$py" -c 'import sys
rows = {l.split()[0]: l.split() for l in sys.stdin.read().splitlines()
        if l.startswith(("2026-08-2", "TOTAL"))}
assert rows["2026-08-22"][4:6] == ["n/a", "n/a"], rows
assert rows["2026-08-23"][4:6] == ["3+", "0+"], rows
assert rows["2026-08-24"][4:6] == ["0", "2"], rows
assert rows["TOTAL"][4:6] == ["3+", "2+"], rows' <<<"$out"; then
  printf '    ok   n/a, lower bound and exact are three different renderings\n'
else
  printf '    FAIL the upgrade-spanning buckets did not render n/a / N+ / N as planted\n'
  fail=1
fi
if "$py" "$here/dedup_rate.py" --json "$work/upgrade.jsonl" \
   | "$py" -c 'import json,sys
d = json.load(sys.stdin)
b = {r["bucket"]: r for r in d["buckets"]}
# Day 1: the fact-less u-6 is NOT a missing semantic_merged; only u-1 is.
assert b["2026-08-22"]["semantic_merged"] is None, b["2026-08-22"]
assert b["2026-08-22"]["semantic_merged_missing"] == 1, b["2026-08-22"]
assert b["2026-08-23"]["semantic_merged"] == 3, b["2026-08-23"]
assert b["2026-08-23"]["semantic_merged_missing"] == 1, b["2026-08-23"]
# A real zero stays a zero (not null) when every contributing call carried it.
assert b["2026-08-24"]["semantic_merged"] == 0, b["2026-08-24"]
assert b["2026-08-24"]["semantic_merged_missing"] == 0, b["2026-08-24"]
assert b["2026-08-24"]["reinforced"] == 2, b["2026-08-24"]
t = d["totals"]
assert t["semantic_merged"] == 3 and t["semantic_merged_missing"] == 2, t
assert t["reinforced"] == 2 and t["reinforced_missing"] == 2, t
assert d["derive_calls_without_facts"] == 1, d
assert d["derive_facts_from_completion"] == 4, d
assert d["derive_facts_from_line"] == 1, d'; then
  printf '    ok   --json separates null, a partial sum and an exact zero\n'
else
  printf '    FAIL --json did not separate null, partial and exact\n'
  fail=1
fi

step "#112: dedup_rate.py --store counts Semantic edges in the ledger window"
out="$("$py" "$here/dedup_rate.py" --store "$work/sample.db" "$ledger")"
echo "$out"
# One Semantic edge inside the window; a Semantic edge before it and a Derives
# edge inside it are planted to oppose a count that ignores either filter.
check "counts only in-window Semantic edges" "$out" \
  "store Semantic edges created 2026-08-18T09:02:59.998+00:00 .. 2026-08-19T09:41:00.000+00:00: 1 (of 2 in the store), ledger sem.merged=1"
"$py" "$here/make_sample.py" --store "$work/no-edges.db"
"$py" -c 'import sqlite3,sys; c = sqlite3.connect(sys.argv[1]); c.execute("DROP TABLE edges"); c.commit()' \
  "$work/no-edges.db"
out="$("$py" "$here/dedup_rate.py" --store "$work/no-edges.db" "$ledger")"
check "a store without an edges table says so rather than reporting 0" "$out" \
  "could not read Semantic edges"
if "$py" "$here/duplicates.py" --json --store "$work/sample.db" \
     --ledger "$work/j4-completion.jsonl" \
   | "$py" -c 'import json,sys
d = json.load(sys.stdin)
assert d["ledger_semantic_merged"] == 2, d["ledger_semantic_merged"]
assert d["ledger_semantic_merged_missing"] == 2, d["ledger_semantic_merged_missing"]'; then
  printf '    ok   duplicates.py --json marks the partial semantic_merged\n'
else
  printf '    FAIL duplicates.py --json did not mark the partial semantic_merged\n'
  fail=1
fi

# An async-acked derive's Semantic edge is stamped at APPLY time, after the
# call line and before its completion line. A window that ended at the last
# call's `ts` would miss it (review M1): the edge at .350 sits between the call
# at .000 and the applied completion at .400.
cat >"$work/late-edge.jsonl" <<'LATE'
{"v":1,"ts":"2026-08-25T10:00:00.000+00:00","kind":"call","tool":"lambo_derive","agent_id":"a","outcome":"ok","duration_us":40,"concepts_requested":1,"admitted":true,"receipt":"w-1"}
{"v":1,"ts":"2026-08-25T10:00:00.400+00:00","kind":"completion","agent_id":"a","receipt":"w-1","state":"applied","created_count":0,"matched_count":0,"semantic_merged":1,"reinforced":0}
LATE
"$py" "$here/make_sample.py" --store "$work/late-edge.db"
"$py" -c 'import sqlite3,sys
c = sqlite3.connect(sys.argv[1])
c.execute("DELETE FROM edges")
c.execute("INSERT INTO edges VALUES (?, ?, ?)", ("late", "Semantic", "2026-08-25T10:00:00.350Z"))
c.commit()' "$work/late-edge.db"
out="$("$py" "$here/dedup_rate.py" --store "$work/late-edge.db" "$work/late-edge.jsonl")"
echo "$out" | tail -3
check "the window reaches the applied completion, not just the last call" "$out" \
  ": 1 (of 1 in the store), ledger sem.merged=1"

step "an empty ledger is a clean error, not a traceback"
: >"$work/empty.jsonl"
if "$py" "$here/dedup_rate.py" "$work/empty.jsonl" >"$work/empty.out" 2>&1; then
  printf '    FAIL dedup_rate.py accepted an empty ledger\n'
  fail=1
elif grep -q Traceback "$work/empty.out"; then
  printf '    FAIL dedup_rate.py crashed on an empty ledger\n'
  fail=1
else
  printf '    ok   %s\n' "$(head -1 "$work/empty.out")"
fi
if "$py" "$here/duplicates.py" --store "$work/sample.db" --ledger "$work/empty.jsonl" \
     >"$work/empty.out" 2>&1; then
  printf '    FAIL duplicates.py accepted an empty ledger\n'
  fail=1
elif grep -q Traceback "$work/empty.out"; then
  printf '    FAIL duplicates.py crashed on an empty ledger\n'
  fail=1
else
  printf '    ok   %s\n' "$(head -1 "$work/empty.out")"
fi

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

# Issue 95: recency span floor

Daemon recency was `(last_touch − start) / span` in whole milliseconds, or
`1.0` when the span was zero. In a session a few milliseconds long that
spreads the dimension across `[0, 1]`. Recency is 0.25 of the daemon
composite and the default recall blend is 0.5 daemon / 0.5 query, so a
derive-order stall could move a final score by up to 0.125. That is what
flipped the #79 graded-look flake.

## Formula

`MIN_RECENCY_SPAN` is 10 minutes, `600_000` integer milliseconds. No
conversion to seconds.

```text
recency = 1 − (end − last_touch) / max(span, MIN_RECENCY_SPAN)
```

clamped to `[0, 1]`. `last_touch` stays `max(last_accessed, created_at)`.

The historical division is taken when `denom == span_ms`, which is
`span_ms >= MIN_RECENCY_SPAN`. A `span_ms > MIN_RECENCY_SPAN` test would send
the exactly-10-minute session down the young formula.
`graph_with_two_concepts` is exactly 10 minutes.

When `max` equals the span, the value is the historical
`(last_touch − start) / span` bits. `1 − (end − last_touch) / span` is the
same real number and not the same `f64` (at a 10-minute span, offset 1 ms,
the two differ). Sessions at or above the floor, including exactly at it,
have to stay bit-identical so the JSON goldens and the dogfood ledger do
not move. When the floor widens the denominator, the value is the formula
above: a short session scores near 1. The zero-span branch is gone. A
single instant returns `1.0`. A concept backdated against that instant
lands slightly under 1, which the old branch hid. A session with no
interactions returns `1.0` explicitly: its `start` falls back to
`Utc::now`, and without the guard the floor formula would age concepts
against the wall clock. The old zero-span branch returned `1.0` there too.

## Why 10 minutes

It is long enough that a stall of tens of milliseconds cannot flip a real
query gap (0.5 vs 0.3 is 0.1 of the default blend; 40 ms / 10 min is about
`7e-5` of recency). It is short enough that the shipped fixtures do not
move: `session-rest-api` is 55 minutes, `session-drift` is 30 minutes,
`graph_with_two_concepts` is exactly 10. A wall-clock half-life would give
a long Dresscode session a real "dismissed 30 seconds ago" signal and would
move dogfood. That is a different change. GC eviction recency
(`eviction_recency`, `GC_RECENCY_WINDOW`, 365 days) is untouched.

## Tests

| Test | What it pins | Mutation |
|---|---|---|
| `a_tens_of_ms_stall_cannot_flip_a_real_query_gap` | 40 ms session, cosines 0.5 then 0.3, identical other dimensions. Final blend keeps 0.5 ahead. | Failed on the historical formula: recency gap was 1. |
| `a_session_at_or_above_the_floor_matches_the_historical_bits` | Spans of exactly the floor, the floor + 1 ms, and 30 minutes, including a 1 ms offset and a `last_accessed` before `created_at`. `to_bits` against a copy of the old function. | Passed on the historical formula, as it should. |
| `recency_is_continuous_at_the_floor` | Oldest concept at the floor is 0. One millisecond under, it is `1 − (floor−1)/floor`, about `1.7e-6`. | Failed on the historical formula (that side was 0). |
| `a_single_instant_session_scores_one_unless_backdated` | Zero span returns 1.0. A concept 40 ms earlier does not. | Failed on the old zero-span branch (it returned 1.0). |
| `the_floor_is_ten_minutes` | `MIN_RECENCY_SPAN == 600_000`, and a 9-minute session's oldest concept is exactly `1 − 540000/600000`. The other tests read the floor symbolically, so a much shorter floor passes them. | Fails on any other floor. |
| `a_touch_after_the_session_end_clamps_to_one` | A touch 5 s after the last interaction scores 1.0 on the floor formula (40 ms) and on the historical division (exactly the floor, 30 minutes). | Fails without the clamp. |
| `a_touch_before_the_session_start_clamps_to_zero` | Before `start` on the historical division, and more than the floor before `end` on the floor formula, scores 0. | Fails without the clamp. |
| `a_session_without_interactions_scores_recency_one` | No interactions: 1.0 for any touch. | Without the guard, an old touch ages against `Utc::now` and scores 0. |
| `a_stall_cannot_flip_a_half_cosine_ahead_of_point_three` | Real derives, best first, 40 ms sleep, both daemon-scored. 0.5 stays ahead of 0.3. This is the flip. | Failed on the historical formula: the 0.3 look scored 0.500 and the 0.5 look 0.475. |
| `a_stall_keeps_the_graded_daemon_scores_together_on_sqlite` (was `graded_similarity_survives_a_stall_between_derives_on_sqlite`, cited by the #79 note) | Worst first, so the order cannot catch a flip. Pins the daemon-score spread across the three graded looks (`< 0.05`) and the full graded result, vector-leg cosines and unrelated looks included. | Failed on the historical formula: spread was about 0.24. |

`script_step_clears_the_recency_floor_without_moving_relative_recency`
checks that each `INTERACTION_CALLS` index has the same `f64` position at
a 60 s step as at a 10 ms step, and that the last edit's offset is at
least the floor. That loop is the scaling identity. The live stamp check
is `assert_shape`. The test also checks that
`POST_EDIT_CLOCK_READS × SCRIPT_STEP` is inside `RECEIPT_RETENTION`. The
count is the hand trace in the demo section, not a count of closes in a
run.

## Short-span tests, and what happened to each

- `graph_with_two_concepts`, `recency_is_monotone_in_created_at_and_clamped`,
  `rescore_ties_order_by_canonical_key_ahead_of_node_id`,
  `an_access_moves_only_the_accessed_concepts_score`: the interaction span
  is exactly 10 minutes. Left alone. The floor does not widen it, so the
  bits are the historical ones.
- `daemon::gc` `hub_session` and `eviction_recency_ignores_the_session_span`:
  spans of 100 minutes and longer. GC reads `eviction_recency`, not
  `ScoreDims.recency`. Untouched.
- `graded_similarity_ranks_by_cosine_not_recency` (memory),
  `graded_similarity_ranks_by_cosine_not_recency_on_sqlite`,
  `graded_similarity_over_the_tier_ranks_by_cosine_not_recency`: wall-clock
  spans of a few milliseconds. They assert cosine order, not a recency
  value. Timestamps were not widened. Widening them to 10 minutes would
  make the tests slow and would put recency back on the session span, which
  is the hazard #79 already worked around. The floor makes that order
  stricter, not weaker.
- `a_stall_keeps_the_graded_daemon_scores_together_on_sqlite` (renamed from
  `graded_similarity_survives_a_stall_between_derives_on_sqlite`): the span
  is the 40 ms stall. It asserted daemon-score spread `> 0.2`, which is the
  old formula. The assertion is now `spread < 0.05`. Cosine order is still
  asserted. A stall long enough to keep `spread > 0.2` under the floor
  would be most of 10 minutes, which is the opposite of the fix.

Hot-list insertion order, `conflict_recency_window`, drift, and the trace
retrieval "recency floor" do not read `ScoreDims.recency`.

## Demo

`lambo demo` stamps from `script_clock`. Call indices, the close reads,
and the ahead-of-wall lead are documented on `SCRIPT_LAST_EDIT_INDEX`.
Hand trace, 2026-10-10, a temporary backtrace, not re-checked on each run:
seventeen reads; interactions at `INTERACTION_CALLS`; closes at 9, 12, 14,
15 and 16. `POST_EDIT_CLOCK_READS` is that count of three closes after the
last edit. Their lead is `3 × SCRIPT_STEP` (180 s), inside
`RECEIPT_RETENTION` (300 s), and they stamp no receipt in this script.
`SCRIPT_STEP` is 60 s, so the span is 13 minutes, above the floor.
`STEP_PACING` stays 10 ms of wall time and spaces nothing on the script
clock.

The clock is backdated by 13 steps so the last edit falls on `Utc::now`
(truncated to whole milliseconds, which SQLite keeps) at construction.
`assert_shape` checks that every interaction lands on `INTERACTION_CALLS`
and names the landed indices if a close read moves them. Positions are
the old 10 ms positions at the same call indices. Dropping the close
reads off this clock moved the printed headroom from 2.06× to 2.10×;
they stay.

The first version of this branch left `conflict_recency_window` at 30 s.
The daemon ages a write on the wall clock, so act II's writes (calls 10
and 11, two to three minutes back) fell out of the window, and agent B's
conflict lines on `redis backend` and `middleware/session.rs` left the
recall block (8 warnings to 6). That was a defect: the demo has to keep
main's recall output. The demo window is now
`CONFLICT_RECENCY_WINDOW + SCRIPT_LAST_EDIT_INDEX × SCRIPT_STEP` (810 s),
so every write the 30 s window covered when the whole run took a second of
wall time is inside it. It is set in `build_config`, so both phases carry
it, and it is in the knob table and the printed header. The high-risk
modification line reads `HIGH_RISK_WRITE_WINDOW` (a fixed 30 s) and is
unaffected. The two ×2 scenario tests assert 8 warnings and both agent B
lines. They failed on the first version.

Compared `lambo demo --scenario rest-api` on main at `212d3c4a` (a
throwaway worktree) and on this branch after the window fix, fixture
embedder, fresh sqlite each, outcome block normalized the way the binary
test normalizes it (`<s>`, `<n>`, `<node>`):

- Outcome block (from `scenario` down): identical.
- `recall_warnings`: 8 on both, in the same order, with `Agent B wrote to
  it <n> seconds ago` under `redis backend` and under
  `middleware/session.rs`.
- GC headroom, both: `user id column` at `2.06×`.
- P90 candidates, both: `add oauth_id to user schema` and
  `wire login endpoint`. `user schema` climbs None → Candidate →
  Venerable → Canonical, blast radius 9.
- Transcript differences: the header now prints the window
  (`30s → 810s`), and agent B's raw lines read `120 seconds ago` where
  main's read about 0. Both ages are masked in the outcome. The cycle
  number printed beside `user schema → Candidate` read `0` once on the
  branch against `1` on main; four more runs of each read `1`. It is read
  after the status poll, so it races the cycle counter, and it is not in
  the outcome.

## Goldens

`fixtures/recall-goldens.json`, `fixtures/recall-h3-goldens.json`,
`fixtures/recall-context-golden.txt`, `fixtures/session-rest-api.json` and
`fixtures/session-drift.json` are not modified. Their sessions are 55 and
30 minutes.

## Live EG2

Not run. No llama-server was started for this change. Port 7700 and the
dogfood rig were not used.

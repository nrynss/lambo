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

`max`, not a `>` branch. A `>` branch would send the exactly-10-minute
session down the young formula. `graph_with_two_concepts` is exactly 10
minutes.

When `max` equals the span, the value is the historical
`(last_touch − start) / span` bits. `1 − (end − last_touch) / span` is the
same real number and not the same `f64` (at a 10-minute span, offset 1 ms,
the two differ). Sessions at or above the floor, including exactly at it,
have to stay bit-identical so the JSON goldens and the dogfood ledger do
not move. When the floor widens the denominator, the value is the formula
above: a short session scores near 1. The zero-span branch is gone. A
single instant returns `1.0`. A concept backdated against that instant
lands slightly under 1, which the old branch hid.

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
| `a_stall_cannot_flip_a_half_cosine_ahead_of_point_three` | Real derives, 40 ms sleep, both daemon-scored. 0.5 stays ahead of 0.3. | Failed on the historical formula: the 0.3 look scored 0.500 and the 0.5 look 0.475. |
| `graded_similarity_survives_a_stall_between_derives_on_sqlite` | Same 40 ms stall. Daemon-score spread must be `< 0.05`, and cosine order holds. | Failed on the historical formula: spread was about 0.24. |

`script_step_clears_the_recency_floor_without_moving_relative_recency` checks
that a 60 s step keeps `k/11` bit-identical to a 10 ms step.

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
- `graded_similarity_survives_a_stall_between_derives_on_sqlite`: the span
  is the 40 ms stall. It asserted daemon-score spread `> 0.2`, which is the
  old formula. The assertion is now `spread < 0.05`. Cosine order is still
  asserted. A stall long enough to keep `spread > 0.2` under the floor
  would be most of 10 minutes, which is the opposite of the fix.

Hot-list insertion order, `conflict_recency_window`, drift, and the trace
retrieval "recency floor" do not read `ScoreDims.recency`.

## Demo

`lambo demo` stamps from `script_clock`. The write pipeline reads that
clock too (a receipt at each act hand-off). Measured call index of agent
A's last edit: 13. `SCRIPT_STEP` is 60 s, so the span is 13 minutes, above
the floor. `STEP_PACING` stays 10 ms. Sleeping 60 s would age the last edit
out of the 30 s conflict window and make the scenario take minutes.

The clock is backdated by 13 steps so that edit falls on `Utc::now` at
construction. Relative positions are the old 10 ms positions scaled by the
same call pattern. Dropping the pipeline reads off this clock moved the
printed headroom from 2.06× to 2.10×; they stay.

Compared `lambo demo --scenario rest-api` on `60adee99` and on this branch,
fixture embedder, fresh sqlite, outcome block normalized the way the binary
test normalizes it (`<s>`, `<n>`, `<node>`):

- GC headroom, both: `user id column` at `2.06×`.
- Statuses, canonization events, canonical set, blast radius: identical.
  P90 candidates remain `add oauth_id to user schema` and
  `wire login endpoint`. `user schema` still climbs None → Candidate →
  Venerable → Canonical.
- Two runs on the branch: outcome blocks identical.
- Diff against main: `redis backend` (Agent B) and `middleware/session.rs`
  (Agent B) leave the recall block, and `recall_warnings` goes from 8 to 6.
  Those writes are minutes older than the last edit, so they are outside
  the 30 s conflict window, which was not widened. They had been
  force-included by that conflict. The spec §13 line (`Agent A wrote to it
  <n> seconds ago` on `user schema`) and the high-risk modification line
  stay.

## Goldens

`fixtures/recall-goldens.json`, `fixtures/recall-h3-goldens.json`,
`fixtures/recall-context-golden.txt`, `fixtures/session-rest-api.json` and
`fixtures/session-drift.json` are not modified. Their sessions are 55 and
30 minutes.

## Live EG2

Not run. No llama-server was started for this change. Port 7700 and the
dogfood rig were not used.

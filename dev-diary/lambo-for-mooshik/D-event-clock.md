# D — Event-time clock

**Goal:** let canonization measure the time a fact is *about* rather than the time it was flushed.

The most substantial change on this branch, and the most reusable afterwards: any
historical-corpus ingest needs it.

---

## Why every time-based gate breaks at bootstrap

Ten years of history arriving in ninety minutes has no temporal structure in ingest time:

* Stage 2's `interaction_span` age floor ignores edges younger than `min_age`
* Supporting edges must span at least **0.3** of the session's temporal extent
  (`src/canon/gate.rs:86`)
* SoloPolicy wants three sessions separated by ≥ 24 hours
* `blast_radius` is measured with a `min_edge_age` floor (`src/canon/gate.rs:132`)

Every one of those reads the bootstrap as "everything happened at once." The gates are not wrong;
they are measuring the wrong clock.

---

## D1 — Injectable clock

Carry the time a fact is *about* — commit date, transcript timestamp — alongside the time it was
flushed.

**Start from what is already parameterized.** `EvalParams` already carries `min_age`,
`min_edge_age` and `cooldown` as `Duration`s, and `gate.rs` takes `now` as an argument rather than
calling the clock itself. The seam is half-built. The work is not extracting constants — it is
deciding what `now` and the edge timestamps *mean*, and threading event time to the places that
currently receive wall-clock time.

Design questions to answer here rather than discover:

1. Where does event time enter — on `derive` / `record_action` at the call, or as session-level
   metadata the ingester sets?
2. What happens to a fact with no event time? A live Mooshik session has no commit date. The
   fallback is presumably wall clock, and mixing the two within one session needs a rule.
3. Is event time persisted, or derived at evaluation? Persisted means a schema change on three
   adapters; derived means recomputing on every load.

**Depends on:** nothing.

---

## D2 — Gates read event time

Move the age floors, the 0.3 temporal-extent coverage, and session separation onto the injected
clock.

**Depends on:** D1.

---

## The fallback, and its cost

If D is larger than it looks: shuffle the ingestion queue so temporal spread acts as a proxy for
source diversity. Cheaper, weaker.

**Decide by end of day 2, not day 4.** And note what taking the fallback costs: C2 can no longer
claim its recurrence signal measures real recurrence, only ingestion order. That changes what the
measurement in `hackathon.md` §8 is entitled to claim, so the decision is a claims decision, not
only a scheduling one.

---

## Interaction with issue #2

Issue #2 (recall ordering tie-breaks on random UUIDs) is filed as latent, and its tie-break is not
worth fixing here. But buried in its "why this is not blocking" section is the finding that the
real `binary_parity` instability was **time-derived `recency` in the daemon score varying between
runs** — two runs of the same demo printing `2.06x` and `2.07x`.

That is the same seam D1 builds. An injectable clock is what would let recency be pinned in tests
and make time-dependent behaviour reproducible. Re-read #2 when D1 lands; the two issues are one
mechanism filed as two unrelated things.

---

## Done when

- [x] Event time can be supplied per fact and is honoured by the age, coverage and separation
      gates — **landed (`d74efc2`).** Age: `fresh_edges_from_aged_interactions_do_not_pass`,
      `aged_edges_from_fresh_interactions_do_not_pass`, `fresh_burst_does_not_inflate_at_min_age_60s`
      (`src/canon/stage2.rs`). Coverage: `coverage_exactly_0_3_passes` (same file). Separation:
      `separation_counts_clusters_once_and_stragglers_each` (`src/canon/event_time.rs`).
      Persistence end to end: `event_time_rides_the_upsert_and_select_shape`
      (`src/store/cockroach.rs`) and `reinforcement_preserves_the_original_edge_event_time`
      (`src/graph/graph.rs`)
- [x] A seeded historical corpus canonizes differently under event time than under ingest time,
      and the difference is measured rather than asserted —
      `a_seeded_historical_corpus_canonizes_differently_under_event_time`
      (`src/canon/event_time.rs`): the same six-turn 2015–2020 corpus, ingested back-to-back in
      one minute, passes Stage 2 on event time and fails it on ingest time at one pinned instant,
      with both halves' numbers asserted
- [x] The no-event-time fallback rule is documented and tested — documented as §2 of the
      `src/canon/event_time.rs` module docs; tested by `mixed_session_ages_each_fact_on_its_own_clock`
      (a fallback-aged edge stays cut in a session that also holds event-timed facts) plus box 2's
      ingest-time half (stripping event times collapses onto flush-time measurement)
- [x] Time-dependent tests can pin the clock — `pinned_now_is_deterministic_and_the_pin_moves_the_answer`
      (`src/canon/event_time.rs`): repeated evaluations at one pinned `now` agree and moving the pin
      re-ages the corpus; stage 3's demotion cooldown likewise runs on a mocked clock
      (`passes_when_mocked_clock_reaches_cooldown`, `src/canon/stage3.rs`)

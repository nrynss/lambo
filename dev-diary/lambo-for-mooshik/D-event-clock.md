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

- [ ] Event time can be supplied per fact and is honoured by the age, coverage and separation gates
- [ ] A seeded historical corpus canonizes differently under event time than under ingest time,
      and the difference is measured rather than asserted
- [ ] The no-event-time fallback rule is documented and tested
- [ ] Time-dependent tests can pin the clock

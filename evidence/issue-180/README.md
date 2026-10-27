# Issue #180 — session_activity

Measured 2026-10-27 on a standalone copy of `~/lambo-dogfood/lambo-dev.db`,
session `lambo-dev`. The copy's embedding blobs were set to NULL before load.
Every blob is the pre-#55 text literal (`[`…`]`), which the current SQLite
loader refuses, and neither the daemon table nor GC's cut reads the vector.
`ranking.json` is the ignored test
`daemon::gc::tests::issue180::issue180_dogfood_ranking_and_gc_cut`.

## Scale

5599 concepts, 2069 interactions. Derivation counts: 5579 concepts once, 19
twice, one concept five times. The old activity for a once-derived concept is
`1/2069`. The new one is `1/5 = 0.2`.

The log form `ln(1+k) / ln(1+k_max)` was not taken. Here `k_max` is 5, so
the denominator is `ln(6)`. A maximum of 5 does not need damping, `1/5` is
already on a usable scale, and against the old table that form inverted more
pairs (14,920) than the linear ratio (13,275).

## Daemon table

Kendall τ against the old per-interaction ratio, on `rescore`'s order
(score descending, then canonical key, then id): **0.9983**
(15,658,326 concordant, 13,275 discordant). 4538 concepts change rank,
because the 20 re-derived concepts climb and everyone they pass shifts down.
The largest climb is 1212 places, the concept derived five times.

## GC

`eviction_score` matches the per-interaction ratio on every concept
(0 bit mismatches), at the measurement clock and with that clock moved 366
days forward. The counts are `gc::run`'s step-2 candidate list: protection
(status, root goal, a concept contradicting a promoted concept, a live
correction) is fixed first, decayed edges are removed, then the score is
read. Orphans, disconnected components, exempt types and Resources with
dependents are not in the candidate list. On this snapshot that structural
prefix is empty and the cap is 280.

| Clock | Below the bar, ratio | Below the bar, max-norm | Step-2 candidates, ratio | Step-2 candidates, max-norm | Only in the ratio set | Taken in one sweep, ratio | Taken in one sweep, max-norm |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 2026-10-27 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| +366 days | 2549 | 1522 | 844 | 154 | 690 | 280 | 154 |

"Below the bar" is every scored concept under its threshold, including ones
the candidate list then drops. One sweep takes orphans and disconnected
components first, then score-cut candidates, and stops at the cap. The +366
day ratio set is larger than the cap, so one sweep takes 280 of it, not 844.
The max-norm set is 154, all of which fit. The shipped cut is the ratio
column. The candidate sets are identical to each other on the measurement
day (both empty).

## Fixture goldens

`session-rest-api` still ranks `user schema` first. The recall context
goldens keep their hit order. Each shown concept is derived once, so its
activity goes from `1/|interactions|` to 1, and the blended score (half
daemon) rises by about 0.09. `fixtures/recall-context-golden.txt` and
`fixtures/recall-context-contradiction-golden.txt` record the new numbers.

## Stage 1

Swarm admission does not read the composite. Membership stays
`gc_survived >= 3` and two aged dependent interactions. The composite only
orders the queue a bounded cycle drains, so the climbs above change who is
taken first and not who is admitted. Solo promotion scores its own evidence
sum and does not read `session_activity`. Neither bar was recalibrated.

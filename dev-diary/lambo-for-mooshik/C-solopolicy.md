# C — SoloPolicy

**Goal:** a promotion policy that works for one human with no peer agents.

**The problem, from spec §3.2:** Lambo's canonization assumes independent multi-agent convergence.
Stage 1 opens a session gate on peer count and cuts at the P90 of the peer score distribution.
With one writer there are no independent peers, so nothing converges and nothing is promoted. The
memory never becomes canonical, and load-bearing warnings — the whole point of the graph — never
fire.

---

## C1 — `PromotionScorer` seam

There is no such seam today. `daemon::score` (`src/daemon/score.rs:127`) and the three-stage
pipeline in `src/canon/` are the only path, and the policy is welded into it.

The work is making the policy selectable — swarm or solo — chosen by `promotion_policy` in config,
with the swarm policy remaining the default so nothing about existing behaviour changes.

Note what is **already** parameterized and must not be duplicated: `EvalParams`
(`src/canon/eval.rs:120`) carries `min_peer_count`, `min_age`, `min_edge_age`, `cooldown`,
`batch_size` and `max_canonical_nodes`. Several of the constants that look like they need
extracting are already inputs. Read `EvalParams` before adding a knob.

**Depends on:** nothing.

---

## C2 — SoloPolicy and eviction resistance

The score from spec §3.2:

```
(Sessions × 1.0) + (Human Confirmed × 4.0) + (Valid Actions × 2.0) − (Reverts × 3.0)
```

with `≥ 10.0` Canonical, `≥ 6.0` Venerable, `≥ 3.0` Candidate, below that None. Plus the
concept-type eviction resistance multipliers: Constraint 1.5, Entity 1.2, Logic 1.1, Resource 1.0,
Observation 0.7.

**Depends on: C1 and D2.**

That second dependency is the one that is easy to miss and expensive to discover. SoloPolicy's
recurrence signal wants three or more distinct sessions separated by at least 24 hours. A bulk
ingest of a decade of history in ninety minutes produces no such separation — every fact arrives
inside one wall-clock window. Build C2 against ingest time and you get a policy that promotes
nothing, plus a passing test that proves it works.

---

## The honest expectation

At bootstrap, almost every fact will have **no human confirmation and no action outcomes**. Three
of the four terms in the formula are zero. Promotion will rest almost entirely on the recurrence
term, which is exactly the term D exists to make meaningful.

If canonization promotes almost nothing on the first full run, that is the expected outcome of
constants tuned for an agent session rather than a decade — not a failure. Tune once, honestly,
and say in the write-up that it was tuned and why. A filter that promotes nothing has perfect
precision and no value; that sentence belongs in the measurement, not in a defence.

---

## Done when

- [ ] `promotion_policy = "solo"` selects the solo scorer; the default stays swarm and existing
      behaviour is unchanged
- [ ] The formula and its four thresholds are tested at their boundaries
- [ ] Eviction resistance multipliers apply per concept type
- [ ] SoloPolicy is evaluated against a corpus with real event-time spread, not an ingest-time one

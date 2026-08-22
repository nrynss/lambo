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

### C1 Status — landed

**What shipped.**

* **`src/canon/policy.rs`** — `PromotionScorer` (the trait), `SwarmScorer`, `SoloScorer`, and
  `PromotionPolicy` (`Swarm` | `Solo`, `#[serde(rename_all = "PascalCase")]`, `Swarm` the
  `#[default]`). The enum lives beside the trait it selects rather than in `types/mod.rs`
  where `MatchStrategy` sits: it is canonization's knob, and `config.rs` already imports
  subsystem types from four other modules (`store`, `embed`, `daemon`, `graph::hybrid`).
  Re-exported from `crate::canon` and `crate::lib`.
* **The seam cuts at Stage 1 only.** That is where the swarm assumption actually is — the
  peer-count session gate and the P90 cut. Stages 2 and 3 measure *evidence*
  (`interaction_span`, `blast_radius`), which means the same thing whether one writer or
  twenty produced it; what breaks them at bootstrap is the clock, which is D, not C.
  Rejected: a trait spanning all three stages. Stages 2/3 are `async` store queries, so a
  three-stage trait would have forced `async` dispatch and a real refactor of the default
  path — against C1's one hard constraint — to accommodate a policy that does not exist yet.
* **`EvalParams` grew exactly one field**, `promotion_policy`, and no thresholds. It is a
  *selector*: it chooses which predicate reads `min_peer_count`, `min_age`, `min_edge_age`,
  `cooldown`, `batch_size`, `max_canonical_nodes` — it restates none of them. `from_config`
  carries it, so `CanonizationTask` needed no change and `eval_cycle` kept its signature.
* **`Evaluator::gather` dispatches through the policy**, so the config key is load-bearing
  rather than decorative. Its `_now` parameter became `now` and is handed to the scorer:
  swarm ignores it, but it means **D2 can give the solo policy event time without widening
  the trait**, and that the eventual solo scorer reads an injected clock instead of reaching
  for `Utc::now()`. `gate.rs` already took `now` as an argument; the new seam keeps that
  discipline unbroken.
* **`Config::promotion_policy`**, defaulted to `Swarm`, documented in the `## Settings` table
  of `docs/reference/api.mdx` and hand-mirrored into `site/src/content/docs/api.mdx` (that
  pair is not covered by `check-mirror-drift.sh`, which gates only `cli`/`mcp` — the script
  was still run and passes). Not added to `lambo.toml`: `match_strategy`, the comparable
  enum-valued key, is library-only, and `DaemonConfig`'s contract is that the file may
  override *cadence* but never a canonization judgement.

**The one real design decision: `Solo` refuses rather than approximating.**

`Config::validate()` rejects `promotion_policy = Solo` at startup, naming C2 and its D2
dependency; `SoloScorer::candidates` is `unimplemented!()` as the backstop behind that gate.
Both alternatives were rejected, and the reasoning is the point:

* **An empty-set stub** — return `vec![]` until C2 lands — is the shape that must not be
  built. "Promoted nothing" is precisely the symptom of the bug D2 exists to prevent, so an
  empty-set stub is *indistinguishable at runtime from a finished, broken SoloPolicy*. It
  would sit behind a green suite and give no signal that the formula was missing rather than
  merely unsatisfied. That is this repo's recurring defect class, pre-installed.
* **A thin plausible formula** is worse: it would read as C2, be measured as C2, and launder
  a guess into the write-up as a tuned result.
* **`#[cfg(test)]`-only** was rejected as making the dispatch table cfg-dependent, i.e. the
  seam less real than the thing it is supposed to prove.

Failing at `validate` rather than at the first eval cycle is deliberate for the same reason:
the failure has to happen where it can still name the reason.

**Left to C2, explicitly.** The score, its four thresholds, the eviction-resistance
multipliers, and any widening of `PromotionScorer` to cover the Venerable/Canonical bars.
Two findings for whoever picks it up:

* **None of the formula's four inputs exist in the data model.** `Concept` carries
  `access_count`, `gc_survived`, `blast_radius`, `last_demotion_time` — there is no
  `human_confirmed`, no valid-action count, no revert count anywhere in `src/`. Three of the
  four terms are not merely zero at bootstrap (as "The honest expectation" below says) —
  they are *unpopulated by any write path*. C2 is a data-plumbing task before it is a
  scoring task. This is also why the trait was not shaped around them: a method signature
  built on data that does not exist is fiction, not a seam.
* **A concept-type modifier already exists, and it is not this one.**
  `ScoreDims::concept_type_modifier` (`src/daemon/score.rs`, `bonus_and_modifier`) is
  **additive** and on a different scale (`MAX_CONCEPT_MODIFIER` is documented as
  "Constraint: 1.15 − 1.0"). C2's eviction resistance is *multiplicative* (1.5 / 1.2 / 1.1 /
  1.0 / 0.7). Reconcile with the existing modifier rather than adding a parallel one — that
  is exactly the duplication C1 was warned about.

**Proof the default path did not move.** The existing canonization tests were not touched;
`cargo test --all --features fixtures` and `cargo test --features store-sqlite,embed-fixture,fixtures`
are green, as is the `binary_parity` demo-determinism bar
(`cargo test --features ship --test binary_parity demo_outcome`), which asserts spec §13's
`canonization_events 5` / `canonical 1` and the ×2 identical-outcome comparison.

**Mutations performed (house rule: a gate must be provable).** Each was applied, observed
red, and reverted:

| # | Mutation | Result |
|---|---|---|
| 1 | `scorer()` maps `Solo` → `SwarmScorer` (the silent fallback) | **red** — 2 tests |
| 2 | `scorer()` maps `Swarm` → `SoloScorer` (wrong arm) | **red** — 26 tests |
| 3 | `#[default]` moved to `Solo` | **red** — `the_default_policy_is_swarm` |
| 4 | `Config::default()` names `Solo` | **red** — 174 tests |
| 5 | `Config::validate`'s policy arm deleted | **red** — `unimplemented_promotion_policy_fails_closed` |
| 6 | `is_implemented()` returns `true` for `Solo` | **red** — 2 tests |
| 7 | `SwarmScorer::candidates` returns `vec![]` | **red** — 14 tests |
| 8 | `validate` refuses unconditionally (over-broad) | **red** — `the_default_promotion_policy_validates` + 147 |
| 9 | `gather` bypasses the seam, calls `stage1_candidates` directly | **red** — exactly 1 test |

Mutation 9 is the one that justifies its test existing. Reverting `gather` to the welded call
leaves swarm behaviour *identical*, so **990 of 991 tests stayed green** while
`promotion_policy` became decorative — the "config selection silently falls back to swarm"
defect, invisible to every other test in the crate.
`canon::eval::tests::gather_dispatches_on_the_configured_promotion_policy` is the only thing
standing between this seam and that revert, which is why it drives a non-default policy
through `gather` rather than asserting on swarm output.

Correspondingly, `canon::policy::tests::the_swarm_arm_cuts_at_p90_of_twenty_peers` asserts a
**hand-computed** expected set (nearest-rank P90 over twenty peers scoring `1.0..=20.0` is
`18.0`, so ids 19 and 20 survive) rather than comparing the dispatch to
`stage1_candidates` — a comparison that would be a tautology surviving every mutation of
either side.

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

- [x] `promotion_policy = "solo"` selects the solo scorer; the default stays swarm and existing
      behaviour is unchanged — **C1, landed.** The selection is real and proven load-bearing
      (mutation 9); the scorer it selects is a deliberate refusal until C2, and
      `Config::validate` refuses the value at startup. Read the C1 status section for why a
      refusal rather than a stub.
- [ ] The formula and its four thresholds are tested at their boundaries — **C2**
- [ ] Eviction resistance multipliers apply per concept type — **C2**; reconcile with the
      existing additive `concept_type_modifier` rather than adding a parallel knob
- [ ] SoloPolicy is evaluated against a corpus with real event-time spread, not an ingest-time
      one — **C2, blocked on D2**

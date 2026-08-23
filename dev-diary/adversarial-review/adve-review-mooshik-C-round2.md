# Adversarial review — mooshik C (SoloPolicy / C2), round 2

**Reviewer**: independent adversarial reviewer, agent_id `CReview2`. Wrote nothing under
review except this file and its commit. Mutation probes ran in the reviewed tree
(`/home/nryn/work/lambo`) and were reverted byte-for-byte after each cycle
(`git checkout --`; clean tree verified between every mutant and at the end — the only
untracked path, `local:/`, predates this review and was left untouched).
**Scope**: remediation commits `ad8e065` (fix(c2): bands drive ladder admission) +
`6152443` (docs(c2): revert_count docstring) against the two findings of round 1
(`adve-review-mooshik-C-round1.md`; verdict REQUEST_CHANGES, 0 P1 / 1 P2 / 1 P3), plus
a new-defect hunt over the code `ad8e065` added to the apply path.
**Worktree**: `/home/nryn/work/lambo`, branch `lambo-for-mooshik` @ `4861035`
(verified before starting).
**Verdict**: **ACCEPT** — zero failed closures, zero new findings. Both closures
**HOLD**, one mutation-verified, one traced + counting-logic-diffed. All seven gates
pass under my own run (`set -o pipefail` throughout).

## Method

1. Read the `## Round 1 closures` section of the round-1 doc, then both remediation
   diffs in full (`git show ad8e065 6152443`, including the 194 added test lines in
   `eval.rs`).
2. Traced the seam end to end in the current source:
   `apply` takes the scorer once from `params.promotion_policy.scorer()`
   (`eval.rs:593`); stage 2 walks `plan.stage2` filtered by
   `verdicts.stage2_pass.contains(&id)` through `scorer.admits_hop(.., Venerable,
   evidence)` (`eval.rs:617-624`); stage 3 walks `plan.stage3` (the evaluated window,
   score-descending), recovers the optional blast measurement from
   `verdicts.stage3_pass`, cooldown-gates the score-admitted arm itself
   (`eval.rs:665-669`, `stage3::in_repromotion_cooldown` made `pub(crate)` in
   `stage3.rs:87`), and asks `admits_hop(.., Canonical, evidence.is_some())`
   (`eval.rs:653-677`). `SwarmScorer::admits_hop` returns `evidence` verbatim
   (`policy.rs:211-214`); `SoloScorer::admits_hop` computes
   `status_rank(classify(solo_score(graph, c))) >= status_rank(to)` with a
   not-a-concept → false guard (`policy.rs:444-450`); non-concept/gone nodes refuse.
3. **Swarm-invariance proof, static half**: under swarm the loop rewrite is
   set-and-order identical by construction — `verdicts.stage2_pass` / `.stage3_pass`
   are built by iterating `plan.stage2` / `plan.stage3` **in that same order**
   (`eval.rs:538-557`), so iterating the plan and filtering on membership reproduces
   exactly the pre-seam iteration sequence. The dynamic half is the binary_parity
   canary (gates below): the ship build's demo outcome — driven entirely by the
   default swarm pipeline — is unchanged.
4. **One-hop-per-cycle preserved** (the main structural risk of putting admission in
   apply): all three windows are read in `gather` from the same pre-cycle graph state;
   the Candidate ring (`plan.stage2`) and Venerable ring (`plan.stage3`) are disjoint
   at read time and `apply` iterates only those fixed vectors, so a node promoted by
   stage 1 or stage 2 in cycle *n* is not re-examined by a later stage of the same
   cycle even when its band would admit the next hop (`eval.rs:410-483`,
   module docs `eval.rs:7-25`). No multi-hop path exists.
5. **Mutation-tested the C-R1-1 closure**, one mutation at a time, each reverted
   before the next (Method preamble). A closure counts as verified only if its claimed
   guards FAIL under the mutation.
6. **New-defect hunt over `ad8e065`'s apply-path changes**: budget arithmetic
   (`remaining` recomputed under the write guard, spent in window order, cut before
   the overflow demotion loop — shape unchanged), the audit stamp on score-admitted
   Canonical hops (`promotion_event`'s `None` arm falls back to the concept's current
   `blast_radius`, `eval.rs:782-788` — no wipe), cooldown symmetry (evidence-passing
   nodes were already gated inside `stage3_passes` during the verdict phase, so the
   apply-side gate correctly fires only on `evidence.is_none()`), mid-apply
   recomputation of `solo_score` (reads concept fields + the rank-decreasing audit
   filter; stages 1/2 append only rank-*increasing* rows and demotions run after the
   stage-3 loop, so the value cannot shift under the loop's feet), and the trait
   contract "`to` below the node's current status is never asked" (both call sites
   ask exactly one step up). Findings: none — see New-defect notes for one deliberate
   observation that does not rise to a finding.
7. Re-ran all seven gates personally on `4861035` (results below).

## Closure verdicts

| Finding | Verdict | Evidence |
| --- | --- | --- |
| C-R1-1 | **HOLDS** (mutation-verified ×5) | The bands drive admission via
  `PromotionScorer::admits_hop`; solo substitutes its band, swarm defers to the
  evidence. Mutations, each applied → observed red → reverted byte-for-byte:  <br>•
  **A** — `SoloScorer::admits_hop` body → `_evidence`: red ×3 —
  `solo_admission_climbs_with_the_score_bands`,
  `solo_band_drives_the_candidate_to_venerable_hop`,
  `solo_band_drives_the_venerable_to_canonical_hop` (FAILED. 2 passed; 3 failed).  <br>•
  **B** — solo's rank comparison `>=` → `>`: red ×3, killing at the exact bars
  (6.0 hop test and the exactly-10.0 boundary among them). Note: `classify`'s own
  unit tests (`band_boundaries_are_inclusive`,
  `integer_evidence_lands_exactly_on_bars`) stay green under B — correctly so, since
  `classify` is untouched and the exclusive comparison lives in `admits_hop`; the
  kill comes through the hop consumers, which is where the comparison actually runs.  <br>•
  **C** — `SwarmScorer::admits_hop` → `true` (ignores the verdict): red ×6 across the
  lib suite (`swarm_admission_is_the_evidence_verdict`,
  `a_blocked_top_scorer_does_not_starve_the_rest_of_the_ring`,
  `stage3_batch_is_capped_and_round_robins_score_desc`,
  `rest_api_api_layer_reaches_venerable_never_canonical`, two anti-starvation tests) —
  the closure table claimed ×5; there is one more guard than claimed.  <br>• **D** —
  apply-side cooldown gate dropped: red ×1 —
  `solo_score_admission_still_honors_the_repromotion_cooldown`.  <br>• **E** — apply
  reverted to evidence-only stage-3 admission (`if evidence.is_none() { continue; }`):
  red ×1 — `solo_band_drives_the_venerable_to_canonical_hop`. The apply-side seam is
  load-bearing independently of `admits_hop`.  <br>The swarm arm provably never reads
  the score: its `admits_hop` binds `_graph`/`_node`/`_to` as unused and returns the
  verdict (static), and binary_parity's demo outcome is identical across runs on the
  swarm default (dynamic canary, gates below). |
| C-R1-2 | **HOLDS** (traced + diffed) | `revert_count`'s doc
  (`policy.rs:318-327`) now names Canonical→None as the single rank-decreasing
  transition `legal_canonization_transition` admits (`graph/graph.rs:1774-1783`;
  the matrix has exactly five edges and only that one decreases rank) and records that
  the rank-comparison filter is deliberate for future additions to the legal set —
  round 1's second fix direction. `git show 6152443` touches doc-comment lines only
  (8+/3−, all inside the docstring); the filter body
  (`status_rank(ev.to_status) < status_rank(ev.from_status)`,
  `policy.rs:332`) and its three assertions (`policy.rs:914/925/936`, including the
  climbs-don't-count matrix walk) are untouched. |

## Gates (rerun personally, `set -o pipefail`, CARGO_TARGET_DIR=/home/nryn/.cache/creview2-target)

- `cargo fmt --all -- --check` — **pass**.
- `cargo clippy --all-targets -- -D warnings` — **pass**.
- `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` — **pass**.
- `cargo clippy --all-targets --features store-cockroach -- -D warnings` — **pass**.
- `cargo test` (default) — **pass** (888 lib + integration + doc-tests; 0 failed).
- `cargo test --features store-sqlite,fixtures` — **pass** (1019 lib + all targets; 0 failed).
- `cargo test --features ship --test binary_parity demo_outcome` — **pass**
  (`demo_outcome_meets_spec_13_and_is_identical_across_two_runs`; the swarm-default
  canary holds — the seam did not move the shipped behavior).

## New defects introduced by `ad8e065`

None found. The attacked surfaces, all clean:

- **Multi-hop per cycle** — structurally impossible (Method step 4); the fix did not
  open a None→Candidate→Venerable→Canonical fast path.
- **Audit stamp on score-admitted Canonical hops** — falls back to the concept's
  current `blast_radius`; no measurement is wiped, and evidence-passed hops still
  stamp the narrowed measurement (F9 preserved for the swarm path byte-for-byte).
- **Cooldown asymmetry** — correct, not a gap: the evidence arm was gated in the
  verdict phase; the score-admitted arm is gated in apply and tested (mutation D red).
- **Budget discipline** — `remaining` still recomputed under the write guard; spend
  order (window, score-descending) and the overflow demotion cut are unchanged.
- **Mid-apply `solo_score` staleness** — no input the loop can change affects it
  before the post-loop demotion phase.

Observation, not a finding: under solo, stage-3 budget contention is still spent in
the daemon composite score's descending order (spec §10 ranking), while admission is
band-driven. That ordering predates solo, is policy-independent by design, and the
closure explicitly keeps the budget cut unchanged; noting it here so a future
"solo-aware contention order" change is a decision, not an accident.

## Verdict

**ACCEPT** — 0 failed closures, 0 new findings. Both round-1 residues are closed with
real, mutation-killed coverage (five distinct mutants, all red, all reverted), the
swarm default is provably untouched (static trace + binary_parity canary), the
cooldown re-check exists in apply and is tested, the docstring now matches the state
machine, and all seven gates pass under my own run. Workstream C is clean.

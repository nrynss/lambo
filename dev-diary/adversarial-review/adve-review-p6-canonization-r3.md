# P6 Canonization — Adversarial Review R3

Branch: `phase/p6-canonization` @ `a743350` (merge of `task/p6-phase-r1`).
Scope: `src/canon/**` (eval.rs, mod.rs, stage1.rs, stage2.rs, stage3.rs).

## Verdict: ACCEPT

No patch-introduced bugs found. The R2 cap fix is correct and the phase is clean.

## Verification notes

### Cap correctness (Stage 3 budget)
- `remaining = max_canonical_nodes.saturating_sub(canonical_count(graph))` is computed after Stage 1/2, which never touch `Canonical`, so it equals the pre-cycle Canonical count — correct.
- Loop `if remaining == 0 { break }` at the top, `remaining -= 1` only after a successful `commit_transition`. Failed predicates (`stage3_passes` false, status no longer `Venerable`) `continue` without consuming budget. No off-by-one: `count == max` → 0 promotions; `count == max - 1` → exactly one.
- `saturating_sub` covers pre-existing over-budget: `remaining == 0`, Stage 3 promotes nothing.

### Pre-existing over-budget (seeded > max)
- `demote_over_budget` still handles it: collects all `Canonical`, returns early if `len <= max`, else ranks by blast (NodeId asc tie-break) and demotes `overflow = len - max` lowest-blast nodes to `None`. The removed `hopped` param is now dead because, post-cap, an over-budget session is always pre-existing (Stage 3 promotes 0 when already over). The defensive `concept_status == Canonical` guard inside the loop is a no-op for distinct ids — no under-demotion.

### Same-tick promote-then-demote (original P1-1) — structurally ruled out
- After the cap, Stage 3 leaves `canonical_count <= max`, so `demote_over_budget` always returns early after a promotion cycle. A same-cycle promotion can no longer become the demotion victim. The P1-1 concern is subsumed, not merely worked around.

### One-hop rule (None → Candidate → Venerable)
- `hopped` is still inserted in Stage 1 and Stage 2; Stage 2 filters `!hopped.contains`, Stage 3 filters `!hopped.contains`. Removing `hopped.insert(id)` from Stage 3 is safe — nothing downstream reads `hopped` after Stage 3 (the only post-Stage-3 consumer, `demote_over_budget`, no longer takes it). No one-hop violation.

### Fixture 3-hop progression
- `max_canonical_nodes` default is `1000` (config.rs:152). `rest_api_user_schema_progresses_three_hops_with_audit` rewinds all Canonicals to None then runs 3 cycles; `remaining` is ~1000 throughout, so the cap never triggers. `user schema` still reaches Canonical with `blast_radius == Some(8)`. `rest_api_api_layer_reaches_venerable_never_canonical` is unaffected (blast=1 fails Stage 3 regardless).

### Regression test strength
- `stage3_promotion_capped_at_remaining_budget` (eval.rs:891) seeds exactly one Canonical (blast 8) + one Stage-3-clearing Venerable (blast 6) with `max_canonical_nodes = 1`. It asserts the Venerable stays `Venerable`, the Canonical is untouched, no promotions/demotions, count == 1, no emitted events. Under the old `hopped`-skip demotion this would instead promote id 20 and demote id 10, so the test fails without the cap — the claim holds.

### Dead code / imports
- `HashSet` and `HashMap` still used (`hopped`, `score_map`). No unused import from dropping the `hopped` param. `canonical_count` is used by `eval_cycle` and one test. No `TODO`/`FIXME`/`unimplemented!`/`dbg!`/`println!` anywhere in `src/canon`.

### Whole-phase re-scan
- Stage 1 gate (≥20 non-Canonical, gc_survived ≥ 3, score > nearest-rank P90), Stage 2 (distinct ≥ 3, coverage ≥ 0.3), Stage 3 (blast > 5 u64, cooldown guard), and `commit_transition` (graph apply → store record → emit) are all correct. `narrow_blast_radius` uses `i32::try_from` (CON-6). No new cross-boundary consumers: `eval_cycle` is currently called only from tests (the daemon tick wiring is outside this branch's diff).

## Findings
None.

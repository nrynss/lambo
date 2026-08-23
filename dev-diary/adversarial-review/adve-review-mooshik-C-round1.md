# Adversarial review — mooshik C (SoloPolicy / C2), round 1

**Reviewer**: independent adversarial reviewer, agent_id `CReview`. Wrote nothing under
review except this file and its commit (mutation probes ran in the reviewed tree and
were reverted byte-for-byte; `git status` clean afterwards, verified).
**Scope**: three commits `0a710f9..6f04553` on branch `c-solopolicy` (`7683fc6` solo
score + plumbing, `90f3640` round-trip/SQL-contract proofs + docs, `6f04553` DDL fix).
Against §C2 and Done-when of `dev-diary/lambo-for-mooshik/C-solopolicy.md`.
**Worktree**: `/tmp/lambo-c`, branch `c-solopolicy`, HEAD `6f04553`.
**Verdict**: **REQUEST_CHANGES** — zero **P1**, one **P2**, one **P3**.

The load-bearing claims survive the attack. The score arithmetic is exactly the spec
formula with inclusive bands applied **after** the type multiplier; the recurrence term
reads event time through `about_time()` and never a clock (zero non-test `Utc::now()`
in the canon path); the DDL-leak history is fully repaired — `human_confirmed` is in
both dialects' fresh-install `CREATE TABLE`, both convergence ALTERs, every INSERT
column list, every `DO UPDATE SET`, and both read-backs; the sqlite chunk arithmetic
(58 × 17 = 986 ≤ 999) is const-asserted; the dispatch is real end-to-end through
`gather`; and all four mutation classes I ran personally were killed by existing tests.
What fails is scope-shape: the Venerable/Canonical bands are **published but
unreachable** — no pipeline code consumes them — which §C2's own framing flags as a
finding even though the trait docstring is honest about it. Plus one doc/code
disagreement on which demotions can actually count as reverts.

## Method

1. Read the workstream spec (`C-solopolicy.md` §C1-status + §C2 + Done-when), then the
   full diff `git diff 0a710f9..6f04553` (36 files) plus `src/canon/policy.rs` in full
   (966 lines), `src/canon/event_time.rs`, and the concept SQL of both adapters.
2. **DDL integrity** (hottest spot per the leak-and-fix history): verified
   `human_confirmed` in `migrations/sqlite/001_init.sql:90` (inline in CREATE TABLE,
   `INTEGER NOT NULL DEFAULT 0`) and `migrations/cockroach/001_init.sql:52` (+ the
   idempotent `ADD COLUMN IF NOT EXISTS` at :71); traced the full statement path in
   both adapters — sqlite INSERT column list ends `chunk_group_id, human_confirmed`
   (`sqlite.rs:2169`), bind #17 (`:2189`), `human_confirmed = excluded.human_confirmed`
   in DO UPDATE SET (`:2218`), positional read-back `try_get(16)` (`:2637`); cockroach
   `INSERT_CONCEPT_PREFIX_SQL` closes with it (`cockroach.rs:318`), rides
   `ON_CONFLICT_CONCEPT_SQL` (`:335`), read by name in `SELECT_CONCEPTS_SQL` (`:475`),
   bound after the `$15::VECTOR` cast (`:1539-1541`); SQL-shape tests pin all of it
   (`cockroach.rs:3292`, `:3404`). Column consts consistent everywhere:
   `CONCEPT_COLUMNS = 17` (`batch.rs:77`), sqlite `BULK_LIMITS.concepts = 58`
   (58 × 17 = 986 ≤ `SQLITE_MAX_VARIABLE_NUMBER` 999, const-asserted at
   `sqlite.rs:288-296`); cockroach asserts 256 × 17 ≤ 65 535 (`cockroach.rs:174-177`);
   `INTERACTION_COLUMNS = 7` / `EDGE_COLUMNS = 10` unchanged and still asserted.
   Serde `#[serde(default)]` on the field (`types/mod.rs:339`) keeps old fixture JSON
   loading. The round-trip test is genuinely non-zero (confirmed = **7** survives a
   real adapter flush→load, `sqlite.rs:8051-8118`).
3. **Score arithmetic**: recomputed by hand against the test corpora — four ≥48h-apart
   event-timed supports → raw 4.0; Entity 4.8 admitted vs Observation 2.8 refused;
   4 sessions + 1 confirm = 8.0 Venerable, + 1 valid action = 10.0 → Canonical exactly;
   4 sessions − 1 revert = 1.0 → None; `classify(-4.0)` = None (no flooring needed:
   the None band is open-bottomed, so negatives cannot land in a weird band).
   Saturation: `confirm_human` uses `saturating_add` on i32 (`graph.rs:699`) — no
   overflow path.
4. **Revert legality**: enumerated the write gate's matrix (`legal_canonization_transition`,
   `graph.rs:1774`: None→Candidate, None→Venerable, Candidate→Venerable,
   Venerable→Canonical, Canonical→None). The only rank-*decreasing* legal transition is
   Canonical→None, so every rank-decreasing row in the audit log is legal — the
   `status_rank(to) < status_rank(from)` filter cannot over-count through the gate.
   (But see C-R1-2: the docstring names transitions the gate rejects.)
5. **Recurrence honesty**: re-ran its own mutations (see Method step 8, mutant M3);
   confirmed `separated_session_count` (`event_time.rs:111-128`) is the greedy
   sort-dedup-anchor counter D2 documented, and that the scorer reads only stored
   instants — grep for `Utc::now|Local::now` across `policy.rs`/`eval.rs`/`event_time.rs`
   hits only comments and `#[cfg(test)]` bodies (`eval.rs:1193,1242,1249`).
6. **Dispatch liveness**: `Config::validate`'s Solo refusal is gone (`config.rs:242-249`,
   body now falls through to `Ok(())`); `PromotionPolicy::is_implemented` is gone from
   the tree; `unimplemented!()` is gone from `SoloScorer::candidates`; `gather` dispatches
   through `params.promotion_policy.scorer()` (`eval.rs:421`) and the paired tests drive
   Solo through `gather` to a non-empty `plan.stage1` while swarm refuses the same rows
   (`eval.rs:932-963`, `:984-988`).
7. **Scope-shape**: traced the full promote path. Stage 1 apply promotes None→Candidate
   via `apply_canonization_transition` regardless of policy (`eval.rs:723`); Stages 2/3
   are status-ring + store-evidence predicates that never read the policy (C1's design).
   So a solo candidate *can* climb to Canonical — reachability of the **status ladder**
   holds mechanically. But the **bands** do not drive anything: grep for
   `classify|VENERABLE_BAR|CANONICAL_BAR|solo_score|raw_solo_score` outside
   `canon/policy.rs` returns zero production consumers. → C-R1-1.
8. **Gates rerun personally** (`CARGO_TARGET_DIR=/home/nryn/.cache/creview-target`;
   `/tmp` disk pressure avoided):
   - `cargo fmt --check` — **pass**.
   - `cargo clippy --all-targets -- -D warnings` — **pass**.
   - `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` — **pass**.
   - `cargo clippy --all-targets --features store-cockroach -- -D warnings` — **pass**.
   - `cargo test` (default) — **pass** (883 lib + 6 + 2 integration + doc-tests; 0 failed).
   - `cargo test --features store-sqlite,fixtures` — **pass** (1053 passed, 0 failed).
   - `cargo test --features ship --test binary_parity demo_outcome` — **pass**
     (spec §13 determinism bar holds; swarm default provably unmoved).
9. **Mutation probes** (applied in-tree, observed red, reverted; `git status` clean after):
   - **M1 — silent fallback**: `scorer()` maps `Solo => &SwarmScorer`. Red ×2:
     `solo_admits_what_swarm_refuses_on_the_same_corpus`,
     `a_bulk_ingest_recurs_only_under_event_time`. The C1-era defect class stays guarded.
   - **M2 — exclusive Canonical bar**: `>=` → `>` on `CANONICAL_BAR`. Red ×2:
     `band_boundaries_are_inclusive`, `integer_evidence_lands_exactly_on_bars`.
   - **M3 — ingest-time reading**: `times.push(i.about_time())` → `i.created_at` in
     `supporting_interaction_times`. Red ×6, including the differing-outcome pair
     (`a_bulk_ingest_recurs_only_under_event_time`) — the DONE-WHEN Box 3 test kills
     both directions of the mutant, as claimed.
   - **M4 — multiplier dropped**: `solo_score` returns the raw sum. Red ×1:
     `eviction_resistance_multiplies_before_the_band_comparison`.

Verified-holds notes (attacked, not broken): the multiplier ordering is genuinely
before comparison and the daemon's additive modifier stays a separate column of the same
v0.6.0 table (`eviction_resistance` vs `score_multiplier`, `types/mod.rs:140-159`) — no
parallel knob; `valid_action_count` counts distinct source concepts of
Causal/Dependency edges, and `record_action` is the only production writer of those edge
types (always sourced at an action Resource node), so "recording = validation" is sound;
one action with produces+depends_on edges counts once (HashSet dedup, tested).

## Findings

### C-R1-1 (P2) — the Venerable and Canonical bands are published but unreachable: nothing in the pipeline consumes them

**Evidence**: `classify` (`policy.rs:333-343`), `VENERABLE_BAR = 6.0` and
`CANONICAL_BAR = 10.0` (`policy.rs:196-198`) are public API with boundary tests, but the
only production consumer of any solo-score function is `SoloScorer::candidates`
(`policy.rs:372`), which reduces `classify(...) != None` to "`resistant >= 3.0`". Grep
across `src/` for `classify|VENERABLE_BAR|CANONICAL_BAR|solo_score|raw_solo_score`
outside `canon/policy.rs`: zero hits. `gather` admits every stage-1 candidate
uniformly as Candidate (`eval.rs:421-431`, apply at `eval.rs:723`); a resistant-12.0
concept and a resistant-3.0 one enter, climb, and are budgeted identically — Stages 2/3
read `interaction_span`/blast radius and never the solo score.

**Why it is a finding anyway**: C1's handoff listed "any widening of `PromotionScorer`
to cover the Venerable/Canonical bars" as C2 work; §C2 ships the bars as tested public
surface without that widening. The trait docstring is honest ("their *pipeline*
consumption point stays open", `policy.rs:140-147`), so this is published-but-decorative,
not concealed — but an operator reading the docs or `classify()` concludes a solo score
of 10 makes a concept Canonical; in the shipped pipeline it becomes Candidate and may sit
there forever without stage-2/3 evidence, while `classify` keeps reporting Canonical.

**Scenario**: solo session, Constraint concept, 4 separated sessions + 1 confirmation →
resistant 12.0. Operator inspects `classify(solo_score(...))` → `Canonical`; the graph's
actual status after stage 1 → `Candidate`. Any consumer keying behavior off the published
bands diverges from the pipeline.

**Fix direction (either)**: consume the bands — widen the scorer seam so stage 1 admits
at Candidate and the solo policy's own predicate drives the Venerable/Canonical hops when
the score clears 6.0/10.0 (stages 2/3 then become evidence-or-score per policy); or stop
publishing what does not run — reduce the public surface to the Candidate admission bar
and state in the docs that 6.0/10.0 are reserved until a consumption point exists.
Remediation owner's call; the current half-state is the residue.

### C-R1-2 (P3) — `revert_count` docstring names demotions the state machine rejects

**Evidence**: `revert_count`'s doc (`policy.rs:280-284`) says it counts "a budget
demotion Canonical → None, conflict demotion Venerable → None, a partial Canonical →
Venerable step-down". The write gate (`legal_canonization_transition`, `graph.rs:1774-1783`)
admits only Canonical→None as rank-decreasing — `Venerable→None` and
`Canonical→Venerable` are rejected ("stage skips, downgrades, and self-loops …
rejected"), and `load.rs:339` replays audit rows through the same gate. So exactly one
transition can ever be counted, and the doc's other two examples describe impossible log
rows. Counting logic itself is correct (and the promotion-does-not-count direction is
tested); this is a doc/code disagreement — per house rule, the disagreement is the
finding.

**Fix direction**: rewrite the docstring to the real set (the sole rank-decreasing legal
transition, Canonical→None), or cite the matrix so future additions to the legal set get
audited against this filter deliberately.

## Operator-leg items (deferred surface, honest expectation)

- **Confirm surface is library-only today.** `Memory::confirm_human` →
  `Graph::confirm_human` has no MCP tool and no CLI verb (grep over `src/mcp`, `src/cli`:
  zero callers outside memory/graph/tests). Consequence: in every current deployment the
  heaviest term is structurally pinned at 0 — `human_confirmed` can never leave 0 in
  production, so solo admission rests entirely on recurrence (+ valid actions − reverts),
  and the effective Canonical bar is raw ≥ 10.0 ÷ resistance purely from session spread.
  This deferral is deliberate per the implementation claims and the docs name the verb
  (`api.mdx:87`), but until the MCP/CLI leg ships, `Human Confirmed × 4.0` is dead weight
  in a live formula. The remediation round should either ship the surface or record the
  deferral in the reference docs next to the formula, so the gap is a decision, not an
  accident.
- **Bootstrap sparsity — honest expectation.** At first run, three of four terms start
  near zero for most facts (no confirmations possible — see above; few recorded actions;
  no reverts because nothing was promoted yet). Promotion will rest almost entirely on
  the recurrence term, which needs ≥24h-apart *event times* on supporting interactions.
  A corpus whose about-times cluster tighter than 24h promotes nothing under solo —
  perfectly precise, zero value, exactly §"The honest expectation"'s warning. First real
  run should publish the admit rate alongside the constants and say plainly whether they
  were tuned. The `a_bulk_ingest_recurs_only_under_event_time` pair proves the term
  measures the right clock; it cannot prove the constants fit anyone's corpus.

## Verdict

**REQUEST_CHANGES** — 0 P1 / 1 P2 / 1 P3. All five gates pass under my own run; all four
mutation classes I applied were killed by the existing suite; the DDL ships correctly in
both dialects. Residue: the decorative upper bands (P2) and the revert-count docstring
(P3). Per the binding operator rule, remediation must close both severities before this
workstream is clean.

## Round 1 closures

| Finding | Closure (commit + evidence) |
| --- | --- |
| C-R1-1 | `ad8e065` — the published Venerable/Canonical bands now drive the promotion
  ladder's admission under the solo policy, via fix direction (a) from this review:
  `PromotionScorer` grows `admits_hop(graph, node, to, evidence)`, giving the active
  policy the final word on each hop given a stage's evidence verdict. Swarm keeps the
  evidence verdict verbatim (the pre-seam pipeline byte-for-byte); solo substitutes its
  §3.2 bands — `status_rank(classify(solo_score)) >= status_rank(to)` — so the ladder
  cannot lift a concept past its band, and a band at or above the target admits without
  store evidence (`src/canon/policy.rs`, `eval.rs`; stage predicates themselves stay
  policy-independent per C1). Stage 3's re-promotion cooldown is re-checked in `apply`
  for score-admitted hops, since those bypass the verdict phase where the gate normally
  runs; the Canonical budget cut is unchanged; both api.mdx mirrors updated. Tests:
  unit-level `solo_admission_climbs_with_the_score_bands` /
  `swarm_admission_is_the_evidence_verdict`; end-to-end through `eval_cycle`,
  `solo_band_drives_the_candidate_to_venerable_hop` (resistant exactly 6.0 with stage-2
  evidence failing), `solo_band_drives_the_venerable_to_canonical_hop` (10.8 without
  blast evidence), and `solo_score_admission_still_honors_the_repromotion_cooldown`.
  Per the remediator's report, five mutation classes were applied-observed-red-reverted
  during development: `admits_hop` returning `_evidence` → red ×3; exclusive band
  comparison (`>=` → `>`) → red ×3; swarm arm ignoring evidence → red ×5; apply reverted
  to evidence-only → red ×1; cooldown gate dropped → red ×1. |
| C-R1-2 | `6152443` — `revert_count`'s docstring now names Canonical→None as the only
  legal rank-decreasing transition, replacing the two impossible examples this finding
  cited (conflict demotion Venerable→None; partial Canonical→Venerable step-down), both
  rejected by `legal_canonization_transition`. The doc additionally records that the
  rank-comparison filter is deliberate: any future addition to the legal set that
  decreases rank lands in this count and must be audited against it — matching this
  finding's second fix direction. Counting logic untouched (it was correct);
  `src/canon/policy.rs` only. |

# #36 PR 3: Swarm admission, the Solo gates and the fair-test fixture

Recorded 2026-10-13 for 0.6.0. Design of record:
[design-36-canonization.md](design-36-canonization.md), section 3 ("Shared
proposed predicates") and section 4, PR 3; operator decisions 8 (the Stage 1
evidence floor), 9 (Solo is evidence-gated) and 13 (the CI fixture) on
[#36](https://github.com/nrynss/lambo/issues/36) (2026-10-08). PR 1's
[dependents note](feature-36-pr1-dependents.md) is the contract for which
edges count; PR 2's [denominator note](feature-36-pr2-denominator.md) for
the Stage 2 cap; [#19(a)](feature-19a-contradicts-edges.md) for
`Contradicts`. Branch `feat/36-pr3-admission` from `origin/main` 9b768768.

## Decisions

### Swarm Stage 1 admits on evidence

A concept becomes a Candidate under Swarm when `gc_survived >= 3` **and**
its aged dependents originate in at least two interactions other than its
own (`MIN_DEPENDENT_INTERACTIONS = 2`, `src/canon/stage1.rs`). The 20-peer
minimum and the nearest-rank P90 cut are gone.

**Why.** The P90 cut was a quota (the top tenth of whatever the session
held), and the design measured it moving with read traffic: replaying recall
at k = 0/1/2 moved the Canonical count 68/74/78 and flipped which fair-test
ruling reached Venerable, while the evidence floor gave 83/83/83 (design
note, Stage 1). It also locked out every session under 20 concepts, which is
every young Dresscode-style per-user session.

**The count is Stage 2's `distinct`, with a lower bar.** "Dependents from at
least two other interactions" uses exactly the rules Stage 2's span uses
(PR 1's dependents relation, own origin excluded, edge about-time and
interaction about-time at or before `now - min_age`, a dependent reached by
two edges counted once). One definition, two bars (2 at Stage 1, 3 at
Stage 2), so a concept cannot be "supported" at one stage and not the other
for a different reason.

**Batched, over the graph.** `dependent_interaction_counts` computes the
count for every concept in one pass over the edges of the in-RAM graph the
cycle already holds under its read guard; `dependent_interactions` is the
`O(degree)` single-node form for inspect. Stage 1 therefore stays I/O-free
(the evaluator's R2-4 property: a store fault still lands Stage 1's hops).
Parity with the stores is a test, not an assumption:
`graph_counts_equal_memory_store_span_distinct` and
`stage1_graph_counts_equal_sqlite_span_distinct` compare the graph count
with each store's `interaction_span(..).distinct` for every concept of the
shared #36 dependents case at both ages.

**Composite orders, it does not select.** `stage1_candidates` returns the
admitted set ordered by the daemon composite (highest first), then canonical
key, then `NodeId`. The order only decides which members a cycle bounded by
`batch_size` takes first; the set drains, so the rest follow.

### Solo is evidence-gated

Solo keeps score-band admission to Candidate, and every later hop now needs
the band **and** the stage verdict: Stage 2 for Venerable; for Canonical the
Stage 3 verdict, which is "Stage 2 still holds, blast radius > 5, cooldown
clear" under both policies now. The score's valid-action term
(`valid_action_count`, distinct actions with a `Causal` or `Dependency` edge
in) is replaced by distinct dependent interactions, weight 2.0
(`DEPENDENT_INTERACTION_WEIGHT`), the same aged count Swarm's Stage 1 reads.

**Why.** On the rig snapshot Solo by bands alone canonized 121 file paths
and short names with blast radius 0, ranked by how often actions touched
them, because valid actions counted `Causal` edges that PR 1 decided confer
no load; evidence-gated Solo matched Swarm's set in the design measurement
(56 vs 56). This also resolves cause 6: Solo now uses the Stage 2/3 queries
it used to pay for and ignore.

**Structural, not advisory.** `canon::eval::apply` skips a node whose stage
verdict failed **before** asking the policy, so a policy can narrow the
evidence, never replace it. (Review L4 then dropped `admits_hop`'s
always-true `evidence` argument.) The pre-PR 3 cooldown gate on the "score-admitted" Stage 3
path is gone with that path: the cooldown is part of every Stage 3 verdict.
`verdicts` now asks the Stage 2 span under both policies (PR 2 review M1 had
Solo skip it because Solo then ignored it), and a store without
`interaction_span_with_window` degrades Solo the way it degrades Swarm: no
Stage 2 evidence for that node, one warning per cycle.

The recurrence term no longer counts `Causal` touches either (review,
orchestrator decision 1): `supporting_interaction_times` keeps `Derives`
and `Dependency`. Before that change a file modified on five separated days
scored five sessions and was a Solo Candidate on the fixture and on the rig.

**Spec §3.2 errata.** The solo score's "Valid Actions" term reads as
distinct dependent interactions (aged, own origin excluded), and its
"Sessions" term counts the ≥ 24 h-separated about-times of the concept's
derivations and of the interactions of concepts that declared they depend
on it, not of actions that only produced or modified it (`Causal`).

### Inspect explains admission per node

`canon::explain` (re-exported: `admission_inputs`, `explain_admission`,
`render_admission`, `Admission`, `AdmissionCheck`, `Comparison`) answers
"would the next hop's gates pass if this node were evaluated now", with the
evaluator's own predicates and inputs: Stage 1 from `gc_survived` and the
single-node dependent count, Solo from `solo_breakdown`, Stage 2 from
`interaction_span_with_window` with the cycle's age floor and cap, Stage 3
from Stage 2 again plus `blast_radius` and the cooldown. Each gate carries
its value, bar, comparison and whether it holds, and each failing gate one
sentence. The graph-side inputs are read under the read guard and the store
queries run with no lock held (spec §6.4).

Surfaces, all additive:

- `lambo_inspect`: text section `canonization (<policy>): <status> -> <next>
  eligible|refused` plus `  - <reason>` rows, spliced between the dependents
  (and reservation) lines and the hop listing (`NeighbourhoodView` in
  `src/surface/neighbourhood.rs`); structured result key `admission`
  (`null` when the store fails, with a warning; the call does not fail).
- `lambo inspect`: the same text; a store fault prints one
  `canonization: explanation unavailable` line. The reader explains under the
  policy `lambo.toml`/`LAMBO_PROMOTION_POLICY` select (`Resolved::StoreOnly`
  now carries it; `cli::inspect::run_with_policy`). The other canonization
  knobs are not file-settable, so their defaults are the writer's.
- `/api/inspect`: `admission` beside `gate_progress`, present whenever the
  focus is found and the store answers, Canonical included (no next status,
  no store query).

`gate_progress` keeps its C2 shape on purpose: its absence of the four gates
under Solo and `met_count() == None` there are pinned contracts. Its module
docs now say that the four-gate block is no longer the whole story (Swarm's
Stage 1 dependent floor is not in it, and Solo now reads three of the four),
and point at `admission` for the complete account.

### `canonization_min_peer_count` is inert

`Config::canonization_min_peer_count` and `EvalParams::min_peer_count` stay
(library callers and serialized configs keep working) but no policy reads
them, and `Config::warnings` names a non-default value, the existing
mechanism for accepted-but-inert settings (#29's idle floor).

## Alternatives rejected

- **A score threshold instead of the P90 quota.** The design measured it:
  Candidates balloon from 601 to 1,294 on the rig snapshot, and it still
  moves with reads.
- **Per-node store queries for Stage 1** (`interaction_span` per concept).
  The design asked for batched inputs: one SQL round trip per concept per
  cycle over 5,000 concepts, and it would put I/O in front of the one stage
  that stays up through a store outage. The graph-side pass plus a parity
  test gives the same number.
- **Keeping the peer minimum as a floor.** It is what locks small sessions
  out, and survival plus two other interactions already requires a session
  with history.
- **Solo bands stay sufficient, verdicts advisory.** That is the pre-PR 3
  behaviour that canonized artifacts.
- **Explaining inspect from graph-only measurements.** It would be a
  parallel metric (`gate.rs` forbids that): the stores age by about-time
  with their own floors. Stage 1's graph-side count is the evaluator's own
  input, so it is used; Stages 2 and 3 use the store queries the cycle uses.
- **Reshaping `gate_progress` to include the new gates.** It would break
  the pinned C2 contract (Solo's `met_count() == None`, key absence) for
  clients that read it; an additive `admission` key breaks nothing.
- **A Python generator for the fixture** (like `scripts/gen-fixtures.py`).
  Real canonical keys need the Rust stemmer; building the fixture through
  the graph's own write path in Rust gives real keys, real `Derives` and
  `Temporal` rows, and a currency test, with one source of truth.

## The fair-test fixture

`fixtures/session-fair-test.json`, built by `src/test_util/fair_test.rs`
(the module docs carry the layout and how to extend it; regenerate with
`LAMBO_UPDATE_GOLDEN=1 cargo test --features store-memory,fixtures --lib fair_test_fixture_is_current`).
21 interactions over 19 days (longer than the 14-day cap), every concept at
`None` with `gc_survived = 3`.

| Role | Rows | Expected at the fixed point (Swarm / Solo) |
|---|---|---|
| Supported rulings | `lease_stays`, `wedge`, `identity`, `async_ack`: six dependents each, 4 to 6 other interactions, 11 to 13 days of spread | Canonical / Canonical, blast 6 |
| Five dependents | `wedge_five` | Venerable / Venerable |
| Burst | `identity_burst`: six dependents, three interactions inside two hours | Candidate / Candidate |
| One interaction | `ack_single`: six dependents from one interaction | None / Candidate |
| Self-vouching | `lease_self`: six `parent_of` children from its own interaction | None / None |
| File chaff | three file Resources modified by actions on five separated days | None / None (Candidate before `Causal` left recurrence) |
| Read chaff | an Observation read 17 times; an association hub | None / None |
| Alias | a short Entity cited by three actions on three days | Venerable / Venerable |
| Contradiction | `lease_revoke -Contradicts-> lease_stays` | changes nothing |

Dependence is real-shaped: `dependent -Dependency-> ruling`, which is what
`record_action`'s `depends_on` (and PR 4's citation) writes, plus the
`Causal` edges actions write to the files they modify. No ruling has an
outbound `Dependency`, and no `Dependency` pair is reciprocal (asserted).

#20 extends this fixture: add rows to the tables in `fair_test.rs`
(append, so ids stay stable), add their expected statuses, regenerate.

## Re-verified against current main

The design predates 0.5.0's later refactors. Checked at 9b768768:

- Stage 1 was `src/canon/stage1.rs` (`stage1_candidates`, P90 at the lines
  the design cites, shifted); the composite is `src/daemon/score.rs`
  (`rescore`).
- Swarm and Solo live in `src/canon/policy.rs` (`SwarmScorer`,
  `SoloScorer`, `valid_action_count`, `classify`, `admits_hop`); the
  verdict phase in `src/canon/eval.rs` (`verdicts`, renamed helper
  `swarm_stage2` -> `stage2_verdict`). The design's "eval.rs:560-578 pays
  Stage 2/3 under Solo" was already half-changed by PR 2 (Solo skipped
  Stage 2, kept its Stage 3 probe); PR 3 makes Solo use both.
- Inspect is `src/surface/neighbourhood.rs` (shared renderer),
  `src/mcp/server/tools/inspect.rs`, `src/cli/inspect.rs` and
  `src/cli/serve_web/routes.rs` + `dto.rs`; `src/canon/gate.rs` holds the
  portal's `gate_progress`.
- No file the lane forbids was needed: no `Cargo.toml`, `Cargo.lock`,
  workflow or `src/store/pg/postgres/tests.rs` change, and no store SQL
  changed (Stage 1 is graph-side; Stages 2/3 reuse PR 1/2's queries).

## Tests

Design list:

| Requirement | Test |
|---|---|
| Fewer than 20 peers qualify on evidence | `a_graph_with_fewer_than_twenty_peers_qualifies_on_evidence` (three concepts), `the_swarm_arm_admits_on_evidence_not_score` |
| < 3 survivals or < 2 other origins refused | `fewer_than_three_survivals_is_refused`, `fewer_than_two_other_dependent_interactions_is_refused` (one interaction thrice; own origin; young origin), `the_swarm_arm_still_enforces_gc_survived_through_the_seam` |
| Replayed reads preserve eligibility | `replayed_reads_preserve_eligibility` (graph), `replayed_reads_preserve_the_fixed_point` (fixture, k = 1/10/100, both policies) |
| Solo cannot bypass Stage 2, Stage 3, cooldown | `solo_cannot_bypass_stage_2_on_its_band`, `solo_cannot_bypass_stage_3_on_its_band`, `solo_cannot_bypass_the_repromotion_cooldown`, `solo_needs_stage_2_again_at_stage_3_and_stamps_the_measured_blast`, `solo_admission_climbs_with_the_band` |
| Five dependents fail, six pass | `five_dependents_fail_and_six_pass` (fixture, both policies) |
| Bounded probes stay fair | `bounded_probes_stay_fair` (batch 1, both policies, reaches the unbounded fixed point) |
| Fixture promotes supported invariants, no seeded statuses, no reciprocal tricks | `both_policies_promote_the_supported_rulings_on_memory`, `..._on_sqlite`, `the_fixture_is_real_shaped`, `fair_test_fixture_is_current` |
| Contradicts changes nothing | `the_contradiction_changes_nothing` (statuses under both policies, and every concept's dependent count, blast and span) |

Also: `only_aged_declared_dependence_counts`,
`hierarchical_children_count_and_double_declarations_count_once`,
`the_composite_orders_the_admitted_set_and_never_filters_it`,
`batched_and_single_node_counts_agree_on_the_dependents_case`, the two
store parity tests above, `an_absurd_age_floor_counts_nothing`,
`dependent_interactions_dedup_and_ignore_causal_touches`,
`solo_without_the_window_span_gives_no_evidence_and_keeps_the_cycle`,
`the_explanation_matches_the_evaluator_on_a_store_without_the_window_span`,
`inspect_explains_every_outcome` (from the all-`None` start, explained
eligible exactly when the policy's Stage 1 admits; at the fixed point nothing
eligible and each stop names its gate), the MCP, CLI and portal inspect
assertions, the regenerated inspect golden, and
`a_non_default_peer_minimum_warns_that_it_is_inert`.

**Mutation checks** (each restored from a copy): letting Solo's band bypass
a failing Stage 2 verdict fails 7 tests (the fair tests on both stores, the
Solo bypass tests); a Stage 1 floor of one other interaction fails 8.

**Goldens and expectations that moved**: `fixtures/inspect-dependents-golden.txt`
gains the two-line canonization section (the cloudops parser self-test still
passes); `tool_schemas.golden.json` (the `lambo_inspect` description);
`tests/binary_parity.rs` `canonization_events 5 -> 15` (ten Candidates,
four Venerable, `user schema` Canonical as before; the demo's narration line
of untouched thresholds changed, and the CANONIZATION section narrates the
nine other promotions); `tests/t84_demo.rs` agent B's recall warnings
`9 -> 8` after review M2 (only Canonical or more than five live dependents
is high-value, so `auth middleware`, five dependents, no longer raises one;
the first version of this PR had `9 -> 10` because any Venerable was
high-value). Tests rewritten because they pinned the
P90 quota or Solo's band bypass: `the_default_policy_gathers_the_evidence_led_stage1_set`,
the twenty-peer fixtures in `eval/tests` and `canon/task.rs` (now give the
pillar real dependents), the Solo band tests (inverted into the bypass
tests), and `a_single_writer_historical_constraint_is_admitted_only_under_solo`
(Solo now stops at Candidate with no dependents).

## Gate (local)

Final, after the review round and the merge of `origin/main` (#142, #143),
each command with a fresh `XDG_RUNTIME_DIR`: `cargo fmt --check`; `cargo
clippy --all-targets -- -D warnings` on `store-sqlite,fixtures`,
`ship,fixtures`, `--no-default-features store-postgres`,
`store-postgres,store-sqlite,fixtures`, `embed-eg2`, `recall-elastic`,
`embed-candle` and `store-cockroach,fixtures` (for the new conformance
leg); `cargo test --features store-sqlite,fixtures` (2277 passed, 0
failed, 7 ignored); `cargo test --no-run --no-default-features --features
store-sqlite`; `cargo check --no-default-features`; `cargo check
--features demo`; `scripts/docs/check-mirror-drift.sh`; the cloudops
parser self-test (`python3 scripts/cloudops/_lambo.py`). All pass. Also
`cargo test --no-default-features --features store-postgres`: 1194 / 0 /
18 (the 18th ignored is the new live Postgres fair test). No live Postgres
or Cockroach run.

Before the review round (first submission, on 9b768768): sqlite,fixtures
2225 / 0 / 7 (main 2214 / 0 / 7) and store-postgres 1166 / 0 / 17 (main
1170 / 0 / 17; Stage 1's thirteen ungated P90 tests became nine evidence
tests, the inspect golden test now needs `store-memory`, and the
inert-peer-minimum config test is new).

## Rig dry run (aggregates only)

Redone after the review round on a fresh copy of
`rig-backup-2026-10-12-0726.db` in the lane's scratch directory (SHA-256
`1c9d3f5d…c43d`, identical before and after the runs, deleted afterwards),
session `lambo-dev`, 5,005 concepts, 1,786 interactions, 12,195 edges,
evaluated at 2026-10-12T12:00Z with `examples/canon36_admission.rs`
(release build). Starting statuses: 404 Candidate (164 Resource, 156
Constraint, 71 Logic, 13 Entity), 7 Venerable (6 Resource, 1 Entity), 0
Canonical.

| | Swarm | Solo |
|---|---:|---:|
| First evaluation, newly eligible Candidate (unbatched) | 63 | 218 (first run: 244) |
| of which Resource / non-Resource | 42 / 21 (20 Entity, 1 Constraint) | 140 / 78 (68 Entity, 8 Constraint, 2 Logic) |
| First evaluation, newly eligible Venerable / Canonical | 0 / 0 | 0 / 0 |
| First real cycle (batch 50) | 50 Candidates (35 Resource) | 50 Candidates (29 Resource) |
| Fixed point (35 cycles each): new Candidates, new Venerables | 63, 2 (both Resource) | 218, 2 (both Resource) |
| **Turns Canonical** | **0** | **0** |

Dropping `Causal` from Solo's recurrence removed 26 Solo Candidates (24
Resources, 2 Entities); the review estimated about 40 Resources. Swarm is
unchanged by the round. With `--assume-survived` (433 concepts raised to
`gc_survived = 3`) every number is identical: dependence binds, not
survival. The two new Venerables are PR 2's two status-None Resources that
the cap moved past Stage 2.

**0 Canonical is a snapshot, not a forecast (review M1).** Two Entities
already have more than five dependents (7 and 10, from 6 and 9 other
interactions) and fail Stage 2 only on spread (coverage 0.065 and 0.119
against 0.3; they need about 4.2 days between their first and last citing
interaction). A few more days of `depends_on` on them would take them, and
any artifact like them, to Canonical with the load-bearing warning. The
example now lists such concepts by id. **This PR queues depended-on
artifacts and names for promotion: the rig must not be re-pinned to a build
containing it until #36 PR 5's dry run has been reviewed** (on the owner's
pre-tag checklist).

Against the design's 0% artifact target: **the newly admitted set is almost
entirely artifacts** (Swarm 62 of 63 Resource or Entity; Solo 208 of 218).
On today's history only `record_action` declares dependence, and it declares
it on files and names, so evidence-led admission admits what actions depend
on. Rulings need PR 4's citations. Existing Candidates are not demoted, and
none of the 404 passes Stage 2, so none of them climbs. PR 5's dry run
should carry this split per stage at each re-pin.

## Review round (2026-10-13)

Adversarial review: 0 High, 3 Medium, 9 Low, plus the Solo recurrence
question. Each resolved in its own commit unless noted.

- **Solo recurrence** (orchestrator decision 1): `Causal` dropped from
  `supporting_interaction_times`; §3.2 errata above; fixture expectation
  for the three file rows under Solo is now None.
- **M1 (rig upgrade)**: documented above and in the CHANGELOG Upgrading
  section; the example lists concepts one gate from Canonical.
- **M2 (high-value)**: `is_high_value` is Canonical or more than five live
  dependents (`> HIGH_RISK_BLAST_RADIUS`, Stage 3's bar; it was any
  Venerable or `>= 5`). Edge-write freshness unchanged, per the owner's
  #19(c) decision. The demo narrates its other promotions; t84 expects eight
  warnings.
- **M3 (live stores)**: `store::pg::fair_test_live` runs the fair test on a
  live store with the shared `fixed_point`/`assert_expected` (moved into
  `test_util::fair_test`). Each run uses a fresh `session-fair-test-<uuid>`
  session whose ids are seeded by that name (`fair_test::snapshot_in`); the
  first version erased the default session first, which tombstones the id
  and refuses every later flush (Sonnet review, High), so it could never
  pass. Seeding the ids by session name, rather than reusing the default's
  ids under another session column, keeps runs disjoint on stores whose ids
  are unique across sessions; `the_fixture_runs_under_another_session_id`
  checks the ids are disjoint and that the run meets `assert_expected` on
  MemoryStore and SQLite. By hand only:
  `LAMBO_POSTGRES_DSN=<dsn> cargo test --no-default-features --features store-postgres --lib store::pg::fair_test_live -- --ignored`;
  Cockroach: `LAMBO_COCKROACH_DSN=<dsn> cargo test --features store-cockroach,fixtures --lib conformance_suite -- --ignored`.
  Not run here (no live database).
- **L1**: `gather` builds the Stage 2 and Stage 3 rings from members
  `admits_hop` would admit, so Solo never probes a node its band refuses;
  Swarm's rings are unchanged. Test `solo_probes_only_candidates_its_band_could_admit`.
- **L2**: `solo_breakdown` only under Solo; `canon::Measurements` taken
  once and shared by `explain_measured` and `gate_progress_measured` in
  `/api/inspect` (test `inspect_measures_once_for_the_gate_block_and_the_admission`).
- **L3 (per-cycle Stage 1 recount): left as is.** Caching by graph epoch is
  not trivial and would be wrong: the count is aged against the injected
  clock, so it changes as edges age past `min_age` with no epoch change. A
  cache would need `(epoch, cutoff)` and invalidation on every cycle's new
  `now`. The recount is one pass over the edges per cycle (60 s default),
  O(E) with hash lookups (12,195 edges on the rig snapshot; not separately
  benchmarked, but the whole 35-cycle dry run took a few seconds).
- **L4**: `admits_hop` lost its always-true `evidence` argument (CHANGELOG,
  Breaking).
- **L5**: `#[deprecated]` on `EvalParams::min_peer_count` and
  `Config::canonization_min_peer_count`; the binary still warns.
- **L6**: fixture ids are SHA-256-derived (version-8) UUIDs of each row's
  natural key, so insertions renumber nothing; the "time order" and
  "append" instructions no longer conflict. (v5 needs the `uuid` crate's
  `v5` feature, which would be a `Cargo.toml` change.)
- **L7**: `lease_alias` reaching Venerable is labelled in
  `fair_test::expected` as the known name-resolution defect PR 4 or the
  backfill should flip.
- **L8**: CHANGELOG Upgrading names that a third-party store without
  `interaction_span_with_window` stops every Solo concept at Candidate,
  and that Solo Canonicals from 0.5.0 are kept.
- **L9**: exact-cutoff rows added to the graph/MemoryStore/SQLite parity
  tests (inclusive cutoff); the explanation now has a `canonical_budget`
  gate, so a Venerable is not explained eligible while no Stage 3 window
  runs, and the site's "never disagrees" sentence is corrected to say timing
  (batch, ring order) is not predicted. **Bounded-probe starvation is not
  reachable**: Stage 1's queue drains (an admitted node leaves it, and the
  next cycle takes the next members of the same ordered set); the Stage 2
  and 3 rings are walked by identity cursors that advance past every member
  evaluated, so with `batch_size >= 1` every ring member is evaluated
  within `ceil(ring / batch_size)` cycles (`bounded_probes_stay_fair`,
  `sustained_inflow_does_not_starve_the_waiting_ring_members`). L1's
  filtering keeps the rings NodeId-ascending, so the cursor argument holds.
- **Merge integration**: #33 PR 3 gave `lambo_inspect` a `scope` that reads
  a layer's Memory; the explanation now reads the same Memory (it read the
  serving session's, so a layer hit's explanation was null). Test in
  `mcp::server::tests::layered`.

## Follow-ups

- PR 4 (citations) is what gives rulings dependents; re-run this example on
  a snapshot after citations have been live.
- PR 5's `lambo canon dry-run` can reuse `canon::explain_admission` for its
  per-stage failure counts.
- Live Postgres/Cockroach: no SQL changed in this PR. Add the
  `fair_test_live` commands above to the owner's pre-tag checklist beside
  PR 2's.
- Do not re-pin the rig to a build containing this PR before PR 5's dry run
  is reviewed (M1).

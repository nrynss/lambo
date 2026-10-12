# #36 PR 2: the fourteen-day Stage 2 denominator

Recorded 2026-10-12 for 0.6.0. Design of record:
[design-36-canonization.md](design-36-canonization.md), section 4, PR 2
("14-day denominator"); operator approval on
[#36](https://github.com/nrynss/lambo/issues/36) (2026-10-08, decision 7).
PR 1's [dependents note](feature-36-pr1-dependents.md) remains the contract
for which edges and origins count. This PR changes only the Stage 2
denominator, its configuration and the shared comparison. Swarm admission,
the Solo gates and the fair-test fixture (PR 3), citations and the shared
resolver (PR 4), and the dry-run tool (PR 5) are later PRs.

Branch `codex/36-pr2-denominator`, started from `422ed101` and merged with
`origin/main` at `92c0f4af` (the merge was clean; see "Merge" below).

## Decision

Stage 2 keeps both bars, `distinct >= 3` and `coverage >= 0.3`, and changes
what coverage divides by:

```
coverage = eligible dependent-origin spread / min(session extent, 14 days)
```

clamped to `0.0..=1.0`. The session extent is the **about-time extent**:
earliest to latest of all the session's interactions, each at its
`event_time`, else its `created_at`. A session whose extent is fourteen days
or less (equality included) measures exactly as before. Empty evidence is
`0.0`. Nonempty evidence in a session whose interactions all share one
instant is `1.0` (the F1 rule).

The extent is not age-gated: it counts interactions younger than the age
floor and ones whose about-time is after the evaluation clock (a backdated
or future `event_time`), while only the evidence side is age-gated. That
predates this PR (review L6); the cap now bounds it, since a far-future
interaction can stretch the denominator to fourteen days at most.
Every guard from PR 1 holds: dependents only, the node's own origin
excluded, session scoping, the edge and interaction age floors, and
`event_time` falling back to `created_at`.

**Why.** On the 2026-10-08 rig snapshot the whole-session denominator
demanded 15.6 days of spread in a 52-day session, and the bar keeps rising
as the session ages: with simulated citations the lease-stays ruling scored
0.25 on a 49-day session. A fourteen-day cap asks for 4.2 days whenever
the extent exceeds fourteen days, and leaves every other session, the demo
and the REST API fixture included, measuring exactly as before.

**This is a denominator cap, not an expiry.** Evidence older than fourteen
days still counts toward the spread. A rolling window would demote support
for a ruling that stopped being cited because it was settled, which is the
opposite of what canon is for.

### Errata

This is the frozen spec's §4.1 and §10 errata for the denominator, on top of
PR 1's: §4.1's "share of the session's temporal extent" and §10's
`coverage >= 0.3` now read against min(session extent, 14 days) for Stage 2.
The raw `interaction_span` measurement keeps the whole-extent definition.

## Alternatives rejected

- **A separated-days count** (three event-time clusters 24 hours apart). An
  earlier recommendation in memory; superseded by the operator-approved
  design. On the rig snapshot it gave 56 Canonicals against 69 for the cap,
  a precision difference indistinguishable at that sample size, and a fixed
  24-hour separation would break the 55-minute fixture and the demo unless it
  became a knob.
- **Concept-lifetime coverage.** It decays exactly like the session span
  (the lease-stays ruling also scores 0.25).
- **A rolling fourteen-day window.** It expires settled support; see above.
- **Capping the existing `interaction_span`.** The previous implementer's
  first draft (recorded in memory) did this. It was dropped: callers that
  read `interaction_span` as "coverage of the session" would silently get a
  different number, and the change could not be seen at the type level.
- **A default `interaction_span_with_window` that delegates to the raw
  accessor.** Raw coverage is never larger than capped coverage, so for
  promotion this is conservative, but inspect would then present an uncapped
  number as the measurement Stage 2 used, and a long session on a
  third-party store would never pass with no signal why. The default returns
  `StoreError::Capability` instead, the same fail-closed pattern the design
  prescribes for #20's `interaction_span_since`. What a store without the
  override sees is under "Stores without the accessor" below.
- **A `lambo.toml` key.** The file-config doctrine keeps numeric promotion
  settings out of the file (`src/config.rs`, the `DaemonConfig` doc: "the
  bar a concept has to clear ... is not settable from a file"): the cap is
  a library setting only, and `lambo_file_refuses_the_coverage_cap` pins
  that the key under `[daemon]` (or at the top level) fails closed as an
  unknown field. Adding the key later is additive.
- **No lower bound on the cap.** Rejected in review (L3). A cap of minutes
  inverts its purpose: in any longer session three citations minutes apart
  cover the whole denominator, the burst Stage 2 refuses. The floor is one
  day (`MIN_COVERAGE_WINDOW`), chosen over "at least
  `canonization_edge_min_age`" because that floor defaults to 60 seconds
  and would not prevent the inversion.

### Stores without the accessor

The first version let the trait default's `Capability` error fail the verdict
phase on the first Candidate. The cycle's error path keeps only Stage 1, so
Stage 3 and the Canonical budget's demotions were dropped every cycle, under
Solo too, whose hop never reads the Stage 2 verdict (review M1). Now:

- **Solo** asks the store nothing for Stage 2 (an exhaustive match on the
  policy in `verdicts`); its ladder is unchanged on any store.
- **Swarm** treats `Capability` as a per-node "no evidence" verdict
  (`swarm_stage2` in `src/canon/eval.rs`): the node fails Stage 2 or the
  Venerable re-check, the rest of the cycle runs (Stage 1, the Stage 3
  window, the budget), and one `CanonizationStage2Unsupported` warning per
  cycle names the missing capability. Any other store error still fails
  the cycle.

So on such a store under Swarm, nothing climbs past Candidate, nothing is
lost, and the log says why. The portal's inspect omits the Swarm gates for
it.

Because that degradation is quiet, every in-tree `GraphStore` impl that
defines `interaction_span` now forwards `interaction_span_with_window` too
(about 30 test wrappers and stubs were missing it; review L4), a source scan
(`every_in_tree_store_forwards_the_window_aware_span`, `src/store/span.rs`)
keeps it that way, and `cycles_through_a_forwarding_wrapper_log_no_capability_warning`
(`src/canon/task.rs`) asserts a cycle loop logs neither warning; it fails if
that wrapper stops forwarding. The scan reads only `impl ... GraphStore for`
header lines (47 impls at the 2026-10-13 merge), so its own literals never
count. It cannot see `src/store/pg/postgres/tests.rs`, which it skips and no
agent in this lane may read, or anything under `tests/` and `examples/`,
which it does not walk; a wrapper there that runs cycles needs the same
forward by hand. The trait default
itself is tested on a dedicated `Legacy` stub in `src/store/span.rs`.

## Shape

| Piece | Where |
|---|---|
| `DEFAULT_COVERAGE_WINDOW` (14 days), `validate_coverage_window`, shared millisecond `coverage` helper | `src/store/span.rs`, re-exported from `crate::store` |
| `GraphStore::interaction_span_with_window` (default: `Capability`) | `src/store/mod.rs` |
| Memory / SQLite | `src/store/memory.rs`, `src/store/sqlite/structural.rs` through `span::coverage` |
| Postgres / Cockroach | `INTERACTION_SPAN_SQL` in `src/store/pg/sql.rs` binds `$4`, the cap in seconds, as `least(extent, $4)`, with every `extract(epoch ...)` cast to double precision so neither dialect's numeric typing decides the comparison or division (review M2); `src/store/pg/structural.rs` |
| Tiered | forwards to its durable primary (`src/store/tiered/mod.rs`) |
| Raw `interaction_span` on the native stores | calls the same query with an `i64::MAX`-millisecond cap, longer than chrono's whole timestamp range, so it is never the smaller term |
| `Config::canonization_coverage_window`, `EvalParams::coverage_window` | `src/config.rs`, `src/canon/eval.rs`; `Config::validate` (run by `Memory::builder`) rejects less than one day (`MIN_COVERAGE_WINDOW`), sub-millisecond fractions and more than `i64::MAX` ms; native store boundaries validate again |
| `stage2_evidence_passes` (the comparison, no I/O, nonfinite fails), `stage2_passes` (default cap), `stage2_passes_with_window` | `src/canon/stage2.rs` |
| Candidate admission and the Swarm Venerable re-check | `src/canon/eval.rs` `verdicts` through `swarm_stage2`, both with `params.coverage_window`; Solo skips the query |
| Inspect | `gate_progress_with_config` (`src/canon/gate.rs`), used by the portal's `/api/inspect` (`src/cli/serve_web/routes.rs`); `gate_progress` keeps its signature and the default cap |

`GateMetric` also refuses a nonfinite measurement (commit `36291522`), so a
malformed third-party span can never show a gate as met.

Re-verified against current main: the design names no file for PR 2 beyond
the Stage 2 predicate; the files #36's issue body names for the span query
(`src/store/sqlite/structural.rs`, `src/store/pg/sql.rs` `INTERACTION_SPAN_SQL`
executed from `pg/structural.rs`, `MemoryStore` in `src/store/memory.rs`)
are where the change lives. `src/store/pg/postgres/tests.rs` was not read or
changed.

## Merge

`git merge origin/main` (22 commits: the anchor word-boundary fix #139, plus
its review and remediation commits) merged with no conflicts. `CHANGELOG.md`
auto-merged into one `## Unreleased` with one each of Upgrading from 0.5.0,
Breaking (library API), Added, Changed, Fixed and Performance; the
duplicate-bullet check prints nothing. No PR 2 symbol is touched by the
merged commits.

## Tests

Against the design's PR 2 test list:

| Requirement | Test |
|---|---|
| Short-fixture coverage unchanged | `rest_api_api_layer_passes_stage2`, `rest_api_user_schema_passes_stage2` (`src/canon/stage2.rs`): capped span equals raw (3 and 7 origins over 30/55). `check_extents_case`: every concept of the 120-minute dependents session, at both ages, under 14- and 28-day caps, answers capped == raw. `span::tests::cap_preserves_short_sessions_and_accepts_four_point_two_days`. Demo goldens (`tests/t84_demo.rs`, `tests/binary_parity.rs`) unchanged and green. |
| Zero-point and single-point extents | `check_extents_case`: a pillar with no support (0, 0.0); one supporting interaction in a 60-day session (1, 0.0: one instant is not spread); a single-instant session with three supports (3, 1.0, passes). |
| Long sessions need 4.2 days | `check_window_case`: a 60-day session with three supports spanning exactly 4.2 days gives raw 0.07, capped 0.3 (passes), 28-day cap 0.15 (fails). |
| Equality at three origins / 0.3 passes | `check_window_case` (capped exactly 0.3, `stage2_passes` true); `check_extents_case` (a session of exactly 14 days, raw and capped both exactly 0.3); `reusable_predicate_keeps_threshold_equality_and_refuses_nonfinite`; `coverage_exactly_0_3_passes`. |
| Injected clock, event time and age parity across adapters | `check_window_case`: support aged only by `event_time`, a young edge (whose dependent has its own aged origin at day 30, outside the support span, so an adapter that drops the edge-age filter counts four origins and a 29-day spread; review L2, verified by removing the filter on MemoryStore), a young origin and the own origin excluded at the injected clock; 30 days later the young rows count and coverage saturates at 1.0; 70 days earlier there is no evidence. Both shared cases run on Memory (`capped_span_follows_*_case`), SQLite (`store::sqlite::tests::structural::capped_span_follows_*_case`), Tiered (`tier_forwards_*`, `recall-elastic`), and in the live `pg::dependents_live` check for Postgres and Cockroach. |
| Cap labelled distinct from expiry | Docs (below), rustdoc on `DEFAULT_COVERAGE_WINDOW`, `Config::canonization_coverage_window` and `interaction_span_with_window`, and the window case's support older than fourteen days still counting. |

Also: `configured_window_controls_both_stage_2_paths_and_inspect` (Candidate
admission, Swarm re-check and inspect agree under 14 and 28 days);
`the_trait_default_fails_closed_without_the_raw_span` (`src/store/span.rs`:
the default returns `Capability` for every valid cap, the default included,
without touching the raw span); `swarm_without_the_window_span_gives_no_evidence_and_keeps_the_cycle`
and `solo_never_asks_a_store_for_the_window_aware_span`
(`src/canon/eval/tests/with_store.rs`, M1);
`every_in_tree_store_forwards_the_window_aware_span` and
`cycles_through_a_forwarding_wrapper_log_no_capability_warning` (L4);
`coverage_window_config_is_validated_and_backward_deserializable`;
`reject_below_a_day_fractional_milliseconds_and_overflow` (L3);
`lambo_file_refuses_the_coverage_cap` (L5);
`nonfinite_gate_metrics_never_claim_evidence_passes`; and the PG SQL shape
(`structural_query_placeholder_order_and_counts` pins `$4`, the `least`, and
that all three epoch extractions are cast; M2).

**Mutation checks** (each restored from a copy): making `span::coverage`
always divide by the cap fails `capped_span_follows_extents_case` on Memory
and SQLite; dropping MemoryStore's edge-age filter in the capped span fails
`capped_span_follows_window_case` (distinct 4 for 3); making the canon task
suite's wrapper answer `Capability` fails
`cycles_through_a_forwarding_wrapper_log_no_capability_warning`.

**Precision at the bar (review L7, informational).** Memory and SQLite
measure span and extent in whole milliseconds (`num_milliseconds`,
truncating); the pg family measures seconds from `extract(epoch ...)` with
microsecond resolution. Both give exactly 0.3 for the shared cases, whose
offsets are whole seconds. A spread within about a millisecond of the bar
could round to opposite sides on different adapters. That is far below any
meaningful spread of evidence and is not corrected.

**Live Postgres/Cockroach not run**: no live database in this lane, and by
the owner's decision these stay manual and out of CI.

### Owner checklist before the 0.6.0 tag (review M2)

The pg-family SQL changed (the `$4` cap, then the double-precision casts)
and has only been text-checked. Before tagging 0.6.0, hand-run the live
dependents check, which now also runs `check_window_case` and
`check_extents_case`, on both dialects:

- [ ] Postgres:
  `LAMBO_POSTGRES_DSN=<dsn> cargo test --no-default-features --features store-postgres --lib store::pg::dependents_live -- --ignored`
- [ ] Cockroach (the `dependents_live` leg inside the conformance suite):
  `LAMBO_COCKROACH_DSN=<dsn> cargo test --features store-cockroach,fixtures --lib conformance_suite -- --ignored`

Each must pass with exact equality at 0.3 and the expected raw/capped
answers; a failure names the session (`deps-window`, `deps-extent-*`) and
cap.

## Rig measurement

On a copy of the 2026-10-12 07:26 rig backup (one session, `lambo-dev`,
5,005 concepts, interactions 2026-08-19 to 2026-10-11, about 53.5 days),
through the real `SqliteStore` with a 60-second age floor: 29 concepts have
at least three dependent origins; 0 pass Stage 2 raw and 3 pass capped
(all three `Resource`, three origins, raw coverage 0.27 to 0.29, capped
1.0). Two are status None and one is Venerable with blast radius 3, so no
transition follows on upgrade. No Logic or Constraint passes either way:
rulings need PR 4's citations before they have dependents.

The program is committed as `examples/canon36_stage2_cap.rs` (review L8). It
takes the store path and session id, opens the database read-only, and
prints counts plus the concepts the cap flips by id, type, status and
numbers, never their text; it holds no data. Reproduce with
`cargo run --features store-sqlite --example canon36_stage2_cap -- <copy.db> lambo-dev --at 2026-10-12T12:00:00Z`.

**For PR 3.** The cap loosens Stage 2, and on today's history it loosens it
**toward Resources**: all three concepts it flips are artifacts with three
action dependents, none a ruling. PR 3's evidence-led Swarm admission must
watch that this does not turn into artifact Candidates: its dry run should
re-measure on a fresh rig copy with this example (raw vs capped, by type),
then with PR 3's admission applied, and report the Resource/Entity share of
each status next to the design's 0% artifact target. PR 5's
`lambo canon dry-run` should carry the same raw/capped split per stage so
the re-pin record shows it.

## Docs

`docs/reference/api.mdx` and `docs/reference/config.mdx` with their site
mirrors (the site link carries `/lambo/`), and the site's
`canonization.mdx` Stage 2 formula, which still divided by the whole
extent. All of them define the extent by about-time (review L1), state the
one-day floor, and say what a store without the accessor sees. CHANGELOG: Upgrading (long sessions, rig measurement), Breaking
(the new `Config`/`EvalParams` fields, the window-aware accessor and what a
store without it sees), Changed (the cap).

## Review fixes in this PR

- Low (`36291522`): `GateMetric` accepted positive infinity as meeting the
  coverage bar. It now requires a finite value.
- Low (`d49e000a`): Stage 2, gate and evaluation rustdoc still named the raw
  `interaction_span` as the query Stage 2 runs.
- `4f3f6e94`: the window-case test had been inserted between the PR 1
  dependents-case doc comment and its test, so the comment described the
  wrong test.
- Second adversarial review (no High, 2 Medium, 9 Low), each fixed in its
  own commit: M1 (a missing accessor no longer fails the cycle; Solo skips
  the query), M2 (pg epoch casts, owner checklist), L1 (about-time extent
  wording), L2 (window case catches a missing edge-age filter), L3
  (one-day floor), L4 (every wrapper forwards; scan and log assertion), L5
  (the key is refused in `lambo.toml`), L6 (extent includes future
  interactions, documented), L7 (precision note above), L8 (the example
  and the PR 3 re-measure), L9 (the #20 fit below).
- `b7a5de79`'s commit body carries literal `\n` sequences instead of line
  breaks. It is left as is: rewriting it would need a rebase, which the lane
  rules forbid.

## Follow-ups

- **#20 fit (review L9; not implemented here).** #20's
  `interaction_span_since` takes the window too, and filters its cutoff
  **before** counting origins and spread. The native implementations then
  share one query: today's `interaction_span_with_window` becomes
  `interaction_span_since(None, ...)`, or both take a small params struct
  (`min_age`, `now`, `window`, optional `since`), so the age, own-origin and
  extent logic is not duplicated. The denominator stays
  `least(extent, cap)` over the **whole** session's about-time extent, not
  a short post-cutoff span, and the verdict reuses `stage2_evidence_passes`.
  Its trait default fails with `Capability` like this one, and the
  evaluator should treat it the same way (per-node no evidence, one
  warning), not as a cycle failure.
- PR 3 builds the fair-test fixture on this predicate.

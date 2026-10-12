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

clamped to `0.0..=1.0`. Empty evidence is `0.0`. Nonempty evidence in a
session whose interactions all share one instant is `1.0` (the F1 rule).
Every guard from PR 1 holds: dependents only, the node's own origin
excluded, session scoping, the edge and interaction age floors, and
`event_time` falling back to `created_at`.

**Why.** On the 2026-10-08 rig snapshot the whole-session denominator
demanded 15.6 days of spread in a 52-day session, and the bar keeps rising
as the session ages: with simulated citations the lease-stays ruling scored
0.25 on a 49-day session. A fourteen-day cap asks for 4.2 days in any long session and
leaves every session of up to fourteen days, the demo and the REST API
fixture included, measuring exactly as before.

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
  prescribes for #20's `interaction_span_since`. The cost is recorded under
  Breaking: a third-party store without the override fails every
  canonization cycle that has a Candidate (or, under Swarm, a Venerable) to
  check, logged as `CanonizationCycleFailed`.
- **A `lambo.toml` key.** The file-config doctrine keeps numeric promotion
  settings out of the file (`src/config.rs`, the `DaemonConfig` doc: "the
  bar a concept has to clear ... is not settable from a file"): the cap is
  a library setting only. Adding the key later is
  additive.

## Shape

| Piece | Where |
|---|---|
| `DEFAULT_COVERAGE_WINDOW` (14 days), `validate_coverage_window`, shared millisecond `coverage` helper | `src/store/span.rs`, re-exported from `crate::store` |
| `GraphStore::interaction_span_with_window` (default: `Capability`) | `src/store/mod.rs` |
| Memory / SQLite | `src/store/memory.rs`, `src/store/sqlite/structural.rs` through `span::coverage` |
| Postgres / Cockroach | `INTERACTION_SPAN_SQL` in `src/store/pg/sql.rs` binds `$4`, the cap in seconds, as `least(extent, $4)`; `src/store/pg/structural.rs` |
| Tiered | forwards to its durable primary (`src/store/tiered/mod.rs`) |
| Raw `interaction_span` on the native stores | calls the same query with an `i64::MAX`-millisecond cap, longer than chrono's whole timestamp range, so it is never the smaller term |
| `Config::canonization_coverage_window`, `EvalParams::coverage_window` | `src/config.rs`, `src/canon/eval.rs`; `Config::validate` (run by `Memory::builder`) rejects zero, sub-millisecond fractions and more than `i64::MAX` ms; native store boundaries validate again |
| `stage2_evidence_passes` (the comparison, no I/O, nonfinite fails), `stage2_passes` (default cap), `stage2_passes_with_window` | `src/canon/stage2.rs` |
| Candidate admission and the Swarm Venerable re-check | `src/canon/eval.rs` `verdicts`, both with `params.coverage_window` |
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
| Injected clock, event time and age parity across adapters | `check_window_case`: support aged only by `event_time`, a young edge, a young origin and the own origin excluded at the injected clock; 30 days later the young rows count and coverage saturates at 1.0; 70 days earlier there is no evidence. Both shared cases run on Memory (`capped_span_follows_*_case`), SQLite (`store::sqlite::tests::structural::capped_span_follows_*_case`), Tiered (`tier_forwards_*`, `recall-elastic`), and in the live `pg::dependents_live` check for Postgres and Cockroach. |
| Cap labelled distinct from expiry | Docs (below), rustdoc on `DEFAULT_COVERAGE_WINDOW`, `Config::canonization_coverage_window` and `interaction_span_with_window`, and the window case's support older than fourteen days still counting. |

Also: `configured_window_controls_both_stage_2_paths_and_inspect` (Candidate
admission, Swarm re-check and inspect agree under 14 and 28 days);
`legacy_adapter_fails_closed_for_capped_stage_2` (a store without the
override returns `Capability` for every cap, the default included);
`coverage_window_config_is_validated_and_backward_deserializable`;
`reject_zero_fractional_milliseconds_and_overflow`;
`nonfinite_gate_metrics_never_claim_evidence_passes`; and the PG SQL shape
(`structural_query_placeholder_order_and_counts` pins `$4` and the `least`).

**Mutation check.** Making `span::coverage` always divide by the cap fails
`capped_span_follows_extents_case` on Memory and SQLite; restored from a
copy.

**Live Postgres/Cockroach not run**: no live database in this lane, and by
the owner's decision these stay manual and out of CI. Run by hand:
`LAMBO_POSTGRES_DSN=<dsn> cargo test --no-default-features --features store-postgres --lib store::pg::dependents_live -- --ignored`.

## Rig measurement

On a copy of the 2026-10-12 07:26 rig backup (one session, `lambo-dev`,
5,005 concepts, interactions 2026-08-19 to 2026-10-11, about 53.5 days),
through the real `SqliteStore` with a 60-second age floor: 29 concepts have
at least three dependent origins; 0 pass Stage 2 raw and 3 pass capped
(all three `Resource`, three origins, raw coverage 0.27 to 0.29, capped
1.0). Two are status None and one is Venerable with blast radius 3, so no
transition follows on upgrade. No Logic or Constraint passes either way:
rulings need PR 4's citations before they have dependents. The measuring
program was a temporary example, not committed.

## Docs

`docs/reference/api.mdx` and `docs/reference/config.mdx` with their site
mirrors (the site link carries `/lambo/`), and the site's
`canonization.mdx` Stage 2 formula, which still divided by the whole
extent. CHANGELOG: Upgrading (long sessions, rig measurement), Breaking
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
- `b7a5de79`'s commit body carries literal `\n` sequences instead of line
  breaks. It is left as is: rewriting it would need a rebase, which the lane
  rules forbid.

## Follow-ups

- #20 filters its cutoff **before** counting and spread, and keeps this
  capped whole-session denominator rather than a short post-cutoff span; it
  reuses `stage2_evidence_passes`.
- PR 3 builds the fair-test fixture on this predicate.

# #36 PR2: Stage 2 coverage denominator

2026-10-12, baseline `422ed101`. Approved design:
[design-36-canonization.md](design-36-canonization.md), PR2. PR1's
[dependents note](feature-36-pr1-dependents.md) remains the relation/age
contract. This PR implements only the denominator, configuration and reusable
predicate; Swarm admission, Solo evidence gates, citations and dry-run tooling
remain later PRs. No #129 changes or rig rollout.

## Decision and errata

Stage 2 keeps `distinct >= 3` and `coverage >= 0.3`, but divides eligible
dependent-origin time spread by **min(full session extent, fourteen days)**.
This is the frozen spec §4.1/§10 errata for the denominator. It is not a
rolling window: old eligible evidence never expires at fourteen days. Short
sessions, including both fixture spans and the demo, retain their coverage.
Long sessions need at least 4.2 days of spread at the default cap. Empty
evidence stays zero, nonempty evidence in a single-point session stays one,
and every ratio is clamped to one. The own-origin, session, edge-age,
interaction-age and event-time fallback guards are unchanged.

The older memory recommendation for separated days conflicts with and is
superseded by the approved issue/design. It would impose a new separation
gate on legitimate short sessions. A rolling expiry was also rejected: the
approved denominator cap deliberately preserves old support.

`Config::canonization_coverage_window` is a **library** duration, default
fourteen days. `Config::validate` rejects zero, fractional milliseconds and
values above `i64::MAX` milliseconds. Native store boundaries repeat this
validation, so standalone calls cannot divide by an invalid cap. It is not
a new TOML key or environment overlay; the current file-config doctrine
reserves numeric promotion settings for library callers. Older serialized
Config values default the absent field. Public exhaustive Config and
EvalParams literals need the new fields (listed in Breaking library API).

The legacy `GraphStore::interaction_span` remains **raw and uncapped**.
`interaction_span_with_window` is a compile-compatible default trait method
that returns `Capability` until an adapter opts in, for every cap including
the default. Root chose this over legacy default delegation: delegation
would silently present uncapped coverage as a capped measurement. Rebuilding
it from `load_session` would duplicate evidence selection and precision
assumptions. Built-in Memory/SQLite/PG/Cockroach and Tiered implement it;
Tiered delegates to its durable primary. Native legacy entry points use an
upper bound beyond chrono's full timestamp domain, preserving raw answers.

`stage2_evidence_passes` is the reusable comparison, independent of store
I/O, status or policy. The configured Stage 2 call selects the window-aware
accessor; `stage2_passes` retains its public signature and selects fourteen
days. Evaluation forwards the cap both for Candidate admission and the Swarm
Venerable upgrade re-check. The portal uses `gate_progress_with_config`, and
the existing `gate_progress` wrapper keeps default-cap compatibility.

## Implementation and tests

SQLite and Memory share the millisecond coverage helper. The PG-family SQL
binds a fourth parameter for the cap in seconds, keeps its outer
double-precision cast, age/session/own-origin filters and empty/zero extent
branches. SQL-shape tests pin the new bind and capped denominator. No schema,
migration, dependency, feature, workflow or fixture golden changes.

The independent shared adapter oracle builds a 60-day session with three
eligible support origins spanning exactly 4.2 days. Event-time support is
flushed at the evaluation clock, an own-origin dependent is excluded, a young
edge and young origin are excluded, and support older than fourteen days
still counts. Raw coverage is 4.2/60; fourteen-day coverage is exactly 0.3;
twenty-eight-day coverage is 0.15. Advancing the injected clock lets the young
evidence qualify and caps coverage at one. A premature clock yields empty
evidence. The case runs on Memory/SQLite/Tiered and through the existing
manually ignored live PG and Cockroach checks. Live checks stay manual.

Other tests cover the inclusive threshold, nonfinite predicate refusal,
short/single-point extents, invalid duration and legacy serde defaults,
third-party Capability refusal, and configured Candidate/Venerable/inspect
agreement. Existing fixture and event-time/age tests remain.

Targeted results before the gate: Stage2 10 passed / 0 failed / 0 ignored;
Memory+SQLite window oracle 2/0/0; configured evaluation/inspect 1/0/0. Baseline
Stage2 was 9/0/0. A first full suite found three portal failures because its
shared test adapter did not yet delegate the new accessor; delegation was
added. No production regression or golden was hidden by that test fix.

## Review and validation

Fresh adversarial review reports two Low findings: inspector infinity parity
and stale raw-query rustdocs. Each is resolved in its own labeled commit;
details and final gate totals follow below. Full lane gate is in progress.
Live PG/Cockroach tests are not run here: no live database is provisioned for
this lane. Manifests/lock/workflows and forbidden live/secret paths are untouched.

## Follow-ups

#36 PR3 owns evidence-led Swarm admission, Solo gates and the fair-test
fixture. #20 must filter its future cutoff **before** counting/spread and
reuse the capped full-session denominator and shared comparison. PR4 owns
citations/shared resolver; no cutoff accessor or resolver is added here.

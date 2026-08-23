# Workstream B, end-to-end remediation round 1 (2026-08-24)

**Against**: `adve-review-mooshik-B-e2e-round1.md`, REQUEST_CHANGES, 11 findings
(2 P1 / 2 P2 / 7 P3), plus E2E-1 and E2E-2 from `E2E-orchestrator-notes.md`.
**Result**: ten closed, one declined with reasoning (F11). All gates green.
**No round 2 review has run.** Stopped here by operator decision.

## Provenance, stated plainly

The remediation agent hit an account spend limit and died partway through F9,
having written no report. The orchestrator finished the round: completing F9's
guard, closing the two clippy regressions that F9's half-written edit and F8's new
CI row exposed, and writing F10, the F2 and F11 documentation, and this file.
Everything from F1 through F9's code is the agent's work; the finishing and every
gate number below is the orchestrator's own run. Split recorded because "who
verified this" is exactly what a review round is for, and this round did not get one.

## Per-finding closure

| Finding | Closed | What changed | Reverting it breaks |
| --- | --- | --- | --- |
| **F1** (P1) CI red on merged tree | yes | `run_fixture_grid`'s 8 parameters restructured; `store-sqlite,fixtures` and `ship,fixtures` compile again | `cargo clippy --all-targets --features ship,fixtures -- -D warnings` |
| **F2** (P1) env DSN outranks `store.dsn` | yes | Per-kind DSN env (`LAMBO_POSTGRES_DSN` / `LAMBO_COCKROACH_DSN`), refusal in `overlay_env` on identity mismatch, canonicaliser moved to `src/store/dsn.rs`, resolved DSN pushed into `provision.sh` | `store::tests` DSN-precedence pins; verified live four ways below |
| **F3** (P2) H3 not self-verifying | yes | `index_present` probed by `EXPLAIN` instead of hardcoded; new `h3_postgres_hnsw_envelope_at_scale` at 5,000 rows / dim 768 measuring a real envelope; forced-exact lane checked against a locally computed cosine ground truth | mutation M3 (`forced_exact_scan_sql() -> None`) now kills **both** H3 tests |
| **F4** (P2) `EXPLAIN` on empty table | yes | `explain_recall_uses_hnsw` seeds a real corpus past the planner crossover and asserts the natural plan names the index, the forced-exact plan a seq scan | shrinking the corpus below the crossover fails the natural-plan assertion |
| **F5** (P3) width check skips reader attach | yes | `preflight_schema()` on the reader funnel, so `stats` / `saints` / `inspect` refuse an unprovisioned or width-mismatched store | reader-refusal test, offline and live |
| **F6** (P3) `provision --help` omits Postgres | yes | help text names the Postgres arm | mirror-drift check |
| **F7** (P3) private-item doc link | yes | intra-doc link demoted to plain text | `cargo doc --document-private-items` warning count |
| **F8** (P3) no CI row lints `store-postgres` test code | yes | two clippy rows added; they immediately caught real dead code (`index_present` unused without `store-sqlite`), now precisely `cfg_attr`-gated rather than blanket-allowed | the new rows |
| **F9** (P3) unit-norm contract unenforced | yes | zero-norm refused in the shared codec, where both dialects and SQLite pass through | zero-vector round-trip test |
| **F10** (P3) `CYCLE.md` counts stale | yes | gate table replaced with measured post-remediation numbers | n/a, documentation |
| **F11** (P3) park-and-fail-over unimplemented | **declined** | recorded in `B-postgres-store.md` under a dated heading | n/a |
| **E2E-1 / E2E-2** | yes | merged into F2 per the reviewer's ruling | as F2 |

## F2 verified live, four ways

Against the pinned container, production DSN cleared throughout.

1. **Conflict refuses.** `store.dsn` naming `lambo`, `LAMBO_POSTGRES_DSN` naming
   `decoy`: rc 1, both canonical forms printed, passwords stripped, remedy named.
2. **Cross-kind leak closed.** `kind = "postgres"` with `LAMBO_COCKROACH_DSN` set to
   `decoy` no longer touches `decoy`. Confirmed by `\dt` on `decoy`: no relations.
   This is the exact reproduction that made F2 a P1.
3. **Agreement accepted.** `postgres://` against `postgresql://` for the same
   database resolves and provisions. Identity, not spelling.
4. **Secret path intact.** No `store.dsn` at all, `LAMBO_POSTGRES_DSN` supplying it:
   provisions normally. CI and existing deployments keep working.

Both dialects refuse: the Cockroach direction was checked the same way, with both
DSNs pointed at local databases.

## F3, the design decision

The reviewer's charge was that the forced-exact lane asserted its own exactness.
Two changes, because the finding had two halves:

* **Self-verification.** `index_present` is now the result of an `EXPLAIN` through
  the same forced-exact path production uses, not a literal. The lane proves it is
  exact rather than claiming it.
* **Scale.** The fixture grid runs at 9 and 22 concepts at dim 8, where the planner
  picks a seq scan for **both** Postgres lanes, so the old envelope was structurally
  zero and could not have detected divergence. The fixture grid keeps its real job,
  cross-adapter exact agreement, and now reports `index_present: false` honestly for
  both lanes at that size. The envelope moved to a new live test at 5,000 rows and
  dim 768, gated `#[ignore]` like the other live tests.

The envelope is now a measurement that moves: at k=5 one probe shows **zero** Jaccard
overlap between the hnsw and exact top-5, and at k=10 the minimum is 0.176. At k=20
and k=40 they agree, which is pgvector's `ef_search` default of 40 doing its job. That
is the honest shape of hnsw approximation, and it is what B chose hnsw-from-day-one in
order to discover early. Runtime 19 s.

## Gates, all run on the finished tree

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo clippy --all-targets --features store-cockroach,fixtures -- -D warnings` | pass |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | pass |
| `cargo clippy --all-targets --features store-postgres,fixtures -- -D warnings` | pass |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | pass (was red) |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | pass (was red) |
| `cargo test --features store-cockroach` | 947 / 0 / 4 |
| `cargo test --features store-cockroach,fixtures` | 1007 / 0 / 12 |
| `cargo test --features store-postgres` | 934 / 0 / 7 |
| `cargo test --features store-postgres,fixtures` | 991 / 0 / 7 |
| `cargo test --features store-sqlite,fixtures` | 1072 / 0 / 3 |
| `cargo test --no-default-features --features store-cockroach` | 610 / 0 / 0 |
| live Postgres `-- --ignored` | 7 / 0 |

## Still not verified

* **No round 2 review.** Ten closures are self-reported by the party that made them.
  The K and J workstreams both had closures fail on re-review, so this is a real gap,
  not a formality. It is the first thing to do on resuming.
* **The live Cockroach leg.** Every `#[ignore]`d Cockroach test remains unrun: no DSN
  reachable here that is not the production cluster, and `cockroach-live` is gated off
  on this branch. F2 changed how the Cockroach DSN resolves and how `provision.sh` is
  invoked, so that leg carries more risk after this round than before it.
* **`postgres-live` in real CI.** The job is defined and the tests pass locally
  against the same pinned image, but no push has exercised it on a GitHub runner.
* **F11**, declined by decision rather than closed.

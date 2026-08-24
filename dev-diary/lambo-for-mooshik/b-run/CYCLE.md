# Workstream B run protocol (2026-08-23)

> **Branch:** this run lives on `b0-pg-extraction` until **B0 through B4 are
> all closed**. One merge to `lambo-for-mooshik` at the end of B, not after
> each phase. `main` does not move before 2026-09-15. Branched off `bd3e9ac`.
>
> **Status, 2026-08-23:** **B0–B4 closed.** Each went REQUEST_CHANGES then
> round 2 **APPROVE**, zero residue
> (`adve-review-mooshik-B-B{0,1,2,3,4}-round{1,2}.md`). Ready to merge
> once to `lambo-for-mooshik`.
>
> **B0-R1-1 (P2) landed:** the composed-SQL byte-identity proof is
> `store::pg::cockroach::tests::b0_composed_sql_is_byte_identical_to_the_pre_carve_constants`.
> Listed counts after that pin: **939 / 598 / 1007** (one above the pre-B0
> baseline). `DISTANCE_OP` is still B3's dangerous row; the pin is what makes a
> mutation of it fail offline.

> **Carried into the cycle, 2026-08-23 (orchestrator, not a review finding):** four
> interactions between workstream J and B are recorded in
> [B-postgres-store.md](../B-postgres-store.md), "What workstream J left in B's path".
> They are **not** B0 defects and are deliberately absent from round 1's findings — B0's
> artifact is correct on all four, and back-filling a completed review would misrepresent
> what that round found. Two want action before or during B1:
>
> * **DSN identity vs DSN spelling** — J2-R1-2's defect in Postgres clothes. Two spellings
>   of one database must derive one session endpoint, or two serves on a machine each
>   believe they are alone. Needs a normalisation rule beside `store_identity` and a test.
> * **`store_is_shareable` matches `StoreKind` exhaustively**, so B1 cannot compile without
>   ruling on Postgres. Expected answer `true`; the point is that it is ruled, not defaulted.
>
> The other two are context (the extraction surface grew, mostly into genuine base material)
> and one **open design decision for B as a whole**: one shared store across several machines
> versus a single-writer lease. DOGFOOD-SETUP's "nothing else changes" promise is falsified
> by B either way, and moves with whichever option is chosen.

Agentic cycle, no worktree: every agent works directly in `/home/nryn/work/lambo`,
on `b0-pg-extraction` for this phase. Agents run strictly one at a time.

Per phase (B0, B1, B2, B3, B4):

1. **Implementation agent** builds the phase to the spec in
   [B-postgres-store.md](../B-postgres-store.md), runs its own gates, and writes
   `b-run/BN-implementation.md` recording what it built and every gate result.
2. **Review agent** reviews adversarially against the phase spec, in the house
   style of `dev-diary/adversarial-review/adve-review-mooshik-K-round2.md`:
   mutation-test every claimed regression pin, re-run every gate itself, hunt for
   defects the change introduced. Writes
   `dev-diary/adversarial-review/adve-review-mooshik-B-BN-roundR.md` with a verdict
   of APPROVE or REQUEST_CHANGES and P1/P2/P3-graded findings. Reviews only; edits
   nothing but its own file.
3. **Remediation agent** closes every finding, records closures, re-runs gates.
4. Back to 2 with round R+1 until a round returns APPROVE with zero residue.
5. Orchestrator commits and pushes the phase.

## Gate set (post-remediation round 4, 2026-08-24)

Every offline row below was run on the round-4 remediated tree, on the MacBook.
**All green.** Suite numbers are the sum across each invocation's test binaries
(lib + bins + integration + doctests), which is how the round-1 table counted and how
the round-2 review reconciled it.

**The live Postgres row is the exception and is marked as such**: remediation rounds 3
and 4 changed only string handling in `store/dsn.rs` and its two call sites, no SQL and
no adapter, so neither re-ran the container leg. That row still carries the round-3
*review's* measurement, not the remediated tree's.

**The doc row is standing and may not be dropped.** B0-R1-2 added it because it is the
only gate that sees private-item doc rot; the round-1 remediation table omitted it and
shipped two new warnings of the exact class it had just closed (E2E-F7, then
B-E2E-R2-6). Every future round reproduces this table in full, doc row included.

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo clippy --all-targets --features store-cockroach,fixtures -- -D warnings` | pass |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | pass (added by F8) |
| `cargo clippy --all-targets --features store-postgres,fixtures -- -D warnings` | pass (added by F8) |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | pass (was RED, E2E-F1) |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | pass (was RED, E2E-F1) |
| `cargo test --features store-cockroach` | 959 passed / 0 failed / 4 ignored (was 952) |
| `cargo test --features store-cockroach,fixtures` | 1019 passed / 0 failed / 12 ignored (was 1012) |
| `cargo test --features store-postgres` | 946 passed / 0 failed / 7 ignored (was 939) |
| `cargo test --features store-postgres,fixtures` | 1003 passed / 0 failed / 7 ignored (was 996) |
| `cargo test --features store-sqlite,fixtures` | 1084 passed / 0 failed / 3 ignored (was 1078; +6 not +7, see below) |
| `cargo test --no-default-features --features store-cockroach` | 622 passed / 0 failed / 0 ignored (was 615) |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | **53 warnings** (round 1: 54, round 2 review measured 55; held through round 4) |
| `cargo doc --no-deps --document-private-items --features store-postgres,store-cockroach,store-sqlite,fixtures` | **53 warnings** (round 2 review measured 55; held through round 4) |
| live Postgres, `-- --ignored` against the pinned container | 7 passed / 0 failed, 20.2 s (last measured by the round-3 review; not re-run in remediation rounds 3 or 4) |

The +3 across every suite is round 2's own three always-compiled tests
(`provision_script_prefers_the_pushed_dsn_over_dotenv`,
`an_unparseable_dsn_still_has_its_password_stripped`,
`a_parseable_dsn_has_its_password_stripped`); `store-sqlite,fixtures` is +4 because
`vector_candidates_refuse_a_zero_norm_probe` needs that adapter.

The further **+2 across every suite** is remediation round 3's two always-compiled
tests (`a_recognised_shape_still_shows_the_operator_the_typo`,
`an_unrecognised_shape_is_replaced_wholesale`), which split the two halves of the
"(passwords stripped)" promise apart so each can fail on its own. Nothing there is
feature-gated, so every suite moves by the same +2. See
`B-e2e-remediation-round3.md`.

The further **+7, except `store-sqlite,fixtures` at +6**, is remediation round 4's
seven tests: six in `store::dsn` (`a_libpq_key_is_echoed_only_if_it_is_on_the_list`,
`an_unencoded_slash_in_a_password_does_not_reach_the_identity`,
`two_unparseable_spellings_are_two_identities`,
`the_echo_and_the_identity_do_not_borrow_each_others_answers`,
`a_parsed_echo_still_may_not_carry_a_control_character`,
`sqlx_dials_what_this_module_cannot_parse`) and
`store::tests::two_unquotable_dsns_still_reach_the_disagreement_refusal`, which drives
E2E-F2's refusal end to end on two DSNs neither parser can quote.

**This is the first round whose delta is not uniform, and the reason is deliberate.**
`sqlx_dials_what_this_module_cannot_parse` executes the claim round 3 asserted — that
no driver dials the shapes we cannot parse — against `PgConnectOptions::from_str`. A
measurement of sqlx cannot be made without sqlx, so it is
`#[cfg(any(feature = "store-postgres", feature = "store-cockroach"))]` and does not
compile under `store-sqlite,fixtures`. See `B-e2e-remediation-round4.md`.

The live row is the one B's container decision bought: `fencing_refuses_stale_write_and_upserts_replay`,
`live_schema_width_refuses_a_config_that_disagrees`, `explain_recall_uses_hnsw`,
`init_schema_at_two_widths_creates_hnsw`, `h3_postgres_recall_parity`, and
`h3_postgres_hnsw_envelope_at_scale`. Run it with the production DSN cleared:

```
env -u LAMBO_COCKROACH_DSN -u DATABASE_URL \
  LAMBO_POSTGRES_DSN="postgresql://lambo:lambo@127.0.0.1:55432/livetest?sslmode=disable" \
  cargo test --features store-postgres,store-sqlite,store-memory,fixtures --lib -- --ignored
```

## Baseline, pinned before B0 (commit `7937de7`, historical)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | pass |
| `cargo test --features store-cockroach` | 934 passed / 0 failed / 4 ignored (938 listed) |
| `cargo test --no-default-features --features store-cockroach` | 597 listed |
| `cargo test --features store-cockroach,fixtures` | 994 passed / 0 failed / 12 ignored (1006 listed) |

**The `fixtures` row is what compiles the larger half of the adapter**, and it was
missing from this table until B0-R1-5. `mod conformance` (~1,850 lines) and
`mod h2_cockroach_parity` (~900 lines) are both
`#[cfg(all(test, feature = "store-cockroach", feature = "fixtures"))]`, so the two
rows above never compiled roughly 2,750 of `cockroach.rs`'s 6,566 lines. A baseline
that does not compile the code under refactor is not a baseline. Numbers measured
independently, twice, from a `git archive 7937de7` extraction. **B0-R1-5 is closed
by this edit** (verified at round-1 remediation: the row, the note, and both
listing files are present; no third fixtures listing was promised, none invented).

Baseline test-name listings live outside the repo at
`$SCRATCH/b-baseline/tests-cockroach.txt` and
`tests-nodefault-cockroach.txt`, where `$SCRATCH` is
`/tmp/claude-1000/-home-nryn-work-lambo/ac914df5-36d6-4129-a5b3-1952f3b20fc0/scratchpad`.
Both files exist at that path.

## Expected counts after B0-R1-1

The standing pin
`store::pg::cockroach::tests::b0_composed_sql_is_byte_identical_to_the_pre_carve_constants`
moves listed counts by one from the pre-B0 baseline:

| Gate | Listed | Passed / ignored |
| --- | ---: | --- |
| `cargo test --features store-cockroach` | 939 | 935 / 4 |
| `cargo test --no-default-features --features store-cockroach` | 598 | 598 / 0 |
| `cargo test --features store-cockroach,fixtures` | 1007 | 995 / 12 |

## Phase gates (every B0+ agent re-runs)

* `cargo fmt --all -- --check`
* `cargo clippy --all-targets -- -D warnings`
* `cargo clippy --all-targets --features store-cockroach -- -D warnings`
* `cargo test --features store-cockroach` (expect 939 listed)
* `cargo test --no-default-features --features store-cockroach` (expect 598 listed)
* `cargo test --features store-cockroach,fixtures` (expect 1007 listed)
* H1 lock: `h1_cross_store_parity` still green; `git diff src/store/sqlite.rs` empty
* `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures`
  (the only gate that sees private-item doc rot; B0-R1-2)

## Standing rules for every agent in this run

* Never commit. The orchestrator commits at phase close. Leave the tree dirty.
* Never touch `.env`, `models/`, or anything outside the repo.
* No em dashes in any prose, doc, comment, or commit text you write. Use a colon,
  a comma, or a full stop. This overrides the surrounding house style.
* Postgres runs only in the pinned container `pgvector/pgvector:pg17` (digest
  `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`, pulled
  2026-08-23), never a
  host install. `docker` is available and no containers are running.
* Do not re-litigate decisions already recorded in B-postgres-store.md. If one has
  to change, say so explicitly and say why.

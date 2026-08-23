# Adversarial review: mooshik B, phase B4 (live-schema width), round 2

**Reviewer**: independent adversarial reviewer, agent_id `B4Review2`. Wrote
nothing under review except this file. No commit, no push.
**Scope**: the uncommitted B4 round-1 remediation on branch `b0-pg-extraction`
against `adve-review-mooshik-B-B4-round1.md` (0 P1 / 1 P2 / 0 P3,
REQUEST_CHANGES) and the remediator's claims in `b-run/B4-remediation.md`.
**Worktree**: `/home/nryn/work/lambo`, branch `b0-pg-extraction` @ `e219488`
(B3 closed, round 2 APPROVE). Dirty tree at review start is the B4
implementation plus the round-1 review, its closures appendix, and the
round-1 remediation. Ignored: `local:/` and the orchestrator briefs.
**Verdict**: **APPROVE**. The single closure holds. Zero failed closures.
Zero new findings.

Reviewed-state SHA-256, taken before any mutation and re-verified after every
mutation cycle and again at the end of the review:

| File | SHA-256 |
| --- | --- |
| `src/store/mod.rs` | `f96df098de9e2a236bb6fee4b9401d481467f643303961213605a04344a782a5` |
| `src/store/pg/postgres.rs` | `3f3ca8cfd922dc05c27c36726900533f5bd79934b08eba0c01ecb88d1204dcc8` |
| `src/store/pg/mod.rs` | `b42105515ab59b03f11a177457e9108237613b6055b36f3349d2d98b3e06d42e` |
| `src/store/pg/dialect.rs` | `3332966b3d3b7dc2627bc44e36653832fc5c4ba7c4d6fb0645ecd55c2a704d3e` |
| `src/store/pg/cockroach.rs` | `d8333564fc1a895cc106b0647f4289f5c14dd61049ef76c52e9b0481cff3e135` |
| `src/store/sqlite.rs` | `b040b47cb13fab6228bee2cebb354a21e180ab064b7c70ad5a3051e8dae03dda` |
| `src/resolve.rs` | `602df0274ffd760f39140bdd0bef7a7a08d13fb9a83f1838188c9cfd1508080f` |
| `.github/workflows/ci.yml` | `5bd31293d860976dc6eead252eb8a62487bb6caf4e643607084e9a11b280fc36` |
| `CHANGELOG.md` | `214d5a3a3b6d76acba2e9d63240df1bee9bdfaba8f51608d1735e0aa4abd0408` |

`src/store/mod.rs`, `postgres.rs`, `pg/mod.rs`, `dialect.rs`, `cockroach.rs`,
`sqlite.rs`, `src/resolve.rs`, and `CHANGELOG.md` are byte-identical to the
round-1 reviewed state. `ci.yml` differs by the R1-1 harness-filter move on
the two multi-name `postgres-live` steps.

Copied those files to `/tmp/b4-r2-review-snapshot/` before any mutation.
Every mutation cycle ends with a restore and a `sha256sum` comparison against
the table above.

## Method

1. Recalled dogfood memory (`B4Review2`). Read `B4-round2-review-brief.md`,
   `B4-remediation.md`, the round-1 review plus its closures appendix,
   `B-postgres-store.md` B4, `b-run/CYCLE.md`, and house style
   `adve-review-mooshik-B-B3-round2.md`.
2. Verified the R1-1 closure against cargo 1.97.1 on this tree: both
   `postgres-live` multi-name steps put names after `--`; `--list` is rc 0
   and lists both tests for each pair; two names before `--` is still rc 1.
   Re-mutated the round-1 unit pins (M2, M4, M5) to prove the live-probe
   source was not weakened. A closure holds only if the cited check FAILS
   under the mutation (or, for R1-1, if cargo itself rejects the old shape).
3. Independently re-ran every CYCLE gate on the restored tree, plus the
   `store-postgres` compile/unit row and the extra `store-postgres` clippy.
   Live Postgres: not started (optional this round). Live Cockroach tests:
   not run. `.env` and `models/`: not touched.
4. Hunted for: vacuous `--list`, H3 one-TESTNAME step broken, live-schema
   probe reopened, `sqlite.rs` edits, leftover `:latest` on the pgvector
   image, leftover mutation, em dashes in remediator-authored lines, CYCLE
   merge-once wording dropped, B0/B1/B2/B3 reopened.

Unmutated pins were green before the first mutation
(`--no-default-features --features store-postgres` for parse and formula;
`--features store-cockroach` for B0; `--features store-sqlite,store-cockroach,fixtures`
for H1). After restore, incremental compile was forced with `touch` of the
restored files.

## Part A: per-finding closure verification

| Finding | Verdict | Verification |
| --- | --- | --- |
| B4-R1-1 (P2) two-TESTNAME cargo invocation | **HOLDS** | `postgres-live` first step (`ci.yml:305-311`) and the B3 two-name step (`ci.yml:315-321`) pass both fully-qualified names as libtest harness filters after `--`. `--ignored --nocapture --exact` and both greps kept. Cargo 1.97.1 (`c980f4866 2026-06-30`; `Usage: cargo test [OPTIONS] [TESTNAME] [-- [ARGS]...]`, singular) rejects two names before `--` (rc 1). The new shape plus `--list` is rc 0 and lists both tests for each pair. H3 one-TESTNAME step (`ci.yml:325-327`) was not edited and still lists (rc 0). |

Pinned toolchain on this tree: `cargo 1.97.1 (c980f4866 2026-06-30)`,
`rust-toolchain.toml` channel `1.97.1`.

First step, `ci.yml:305-311`:

```
cargo test --no-default-features --features store-postgres --lib \
  -- --ignored --nocapture --exact \
  store::pg::postgres::tests::init_schema_at_two_widths_creates_hnsw \
  store::pg::postgres::tests::live_schema_width_refuses_a_config_that_disagrees
```

B3 step, `ci.yml:315-321`:

```
cargo test --no-default-features --features store-postgres --lib \
  -- --ignored --nocapture --exact \
  store::pg::postgres::tests::explain_recall_uses_hnsw \
  store::pg::postgres::tests::fencing_refuses_stale_write_and_upserts_replay
```

### B4-R1-1 cargo 1.97.1 proof (no file mutation)

Each command was run against this tree. `--list` is the proof cargo accepts
the invocation and the harness sees both filters. Live Postgres was not
started.

| # | Invocation | rc | Result |
| --- | --- | ---: | --- |
| old-first | two names **before** `--` (init + mismatch) | **1** | `error: unexpected argument 'store::pg::postgres::tests::live_schema_width_refuses_a_config_that_disagrees' found` |
| old-B3 | two names **before** `--` (EXPLAIN + fencing) | **1** | `error: unexpected argument 'store::pg::postgres::tests::fencing_refuses_stale_write_and_upserts_replay' found` |
| new-first | names **after** `--` plus `--list` | **0** | lists `init_schema_at_two_widths_creates_hnsw` and `live_schema_width_refuses_a_config_that_disagrees`. `2 tests, 0 benchmarks` |
| new-B3 | names **after** `--` plus `--list` | **0** | lists `explain_recall_uses_hnsw` and `fencing_refuses_stale_write_and_upserts_replay`. `2 tests, 0 benchmarks` |
| H3 | one TESTNAME **before** `--` plus `--list` | **0** | lists `h3_postgres_recall_parity`. `1 test, 0 benchmarks`. Not a finding: singular TESTNAME is valid cargo 1.97.1, and round 1 said leave this step |

`--list` listing two tests is not vacuous: the old shape never starts the
harness, so the greps at `ci.yml:310-311` and `ci.yml:320-321` cannot run.
Two filters after `--` with `--exact` are OR, not AND (a 0-test list would
have been a new finding).

### Round-1 live-probe unit pins (restored after each)

Source hashes of `postgres.rs`, `pg/mod.rs`, and `dialect.rs` match round 1.
The remediator did not edit `src/store/pg/`. Re-mutated anyway. Cited tests
use fully-qualified names **after** `--` (a short name plus `--exact` lists
zero tests and stays green: the same class of mistake R1-1 closed).

| # | Claim | Cited test | Mutation | Result |
| --- | --- | --- | --- | --- |
| M2 | parse rejects `VECTOR(n)` | `parse_pgvector_format_type_reads_vector_n` | `to_ascii_lowercase` before `strip_prefix("vector(")` | **RED** at `postgres.rs:589`: left `Some(768)`, right `None` (`Cockroach spelling is not pgvector`) |
| M4 | Postgres SQL is present | same | `PostgresDialect::live_schema_vector_width_sql` returns `None` | **RED** at `postgres.rs:591`: `assert!(...is_some())` |
| M5 | B3 Postgres score is still `1 - d` | `distance_to_score_is_one_minus_d` | body `(1.0 - dist).clamp` to `(1.0 - 0.5 * dist * dist).clamp` | **RED** at `postgres.rs:253`: left `0.5`, right `0.0` (`d=1`) |

M1 (delete `assert_live_schema_width` from `preflight_schema`) was not
re-run: live container optional this round. Both production calls remain
(`pg/mod.rs:2274` init, `pg/mod.rs:2326` preflight). `pg/mod.rs` hash
matches round 1, so the wiring M1 caught is the same bytes.

After restore the unmutated parse and formula tests passed (FQ names after
`--`). Production parse has no `to_ascii_lowercase`. Production formula is
`(1.0 - dist).clamp(-1.0, 1.0)` (`postgres.rs:97`).

Mutation score: **1/1 on the R1-1 cargo shape. 2/2 live-probe unit pins
still caught (M2, M4). B3 formula pin still caught (M5).**

## Part B: hunt for defects introduced by the remediation

No new findings. Specific attack vectors examined:

- **Vacuous `--list`.** Ruled out: each new pair lists exactly the two
  named ignored tests. The old shape is rc 1 before the harness starts.
- **H3 step broken.** `git diff` versus HEAD (and versus the round-1
  `ci.yml` shape) does not touch `ci.yml:322-329`. One TESTNAME before
  `--` plus `--list` is rc 0.
- **Live-schema probe reopened.** `postgres.rs`, `pg/mod.rs`, `dialect.rs`
  hashes match round 1. `CockroachDialect` still has no
  `live_schema_vector_width_sql` override; the trait default is `None`
  (`dialect.rs:88-90`). Parse still rejects `VECTOR(n)` (M2 red).
- **H1 / sqlite.rs.** sqlite.rs hash is byte-identical to round 1. This
  remediator did not edit it (`git diff HEAD -- src/store/sqlite.rs` is 0
  bytes). `h1_sqlite_and_memory_oracle_agree_exactly` passed under
  `store-sqlite,store-cockroach,fixtures` (0.07s).
- **Pin at `resolve_backends`.** `src/resolve.rs` and `src/store/mod.rs`
  hashes match round 1.
- **Leftover `:latest`.** `.github/workflows/ci.yml:278` is still
  `pgvector/pgvector:pg17@sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
  The `:latest` tokens in that file are the comment that the image is never
  `:latest`, plus the pre-existing GHA runner labels `ubuntu-latest` /
  `macos-latest`.
- **Leftover mutation.** After restore: parse has no `to_ascii_lowercase`.
  Postgres SQL is the `format_type` `Some(...)`. Postgres formula is
  `1.0 - dist`. All nine SHA-256 values match the header.
- **CYCLE merge-once.** Intact: B stays on `b0-pg-extraction` until B0
  through B4 are all closed; one merge to `lambo-for-mooshik` at the end of
  B. Status line still says "B4 is next"; that is orchestrator-owned. B0,
  B1, B2, and B3 were not reopened.
- **Em dashes in remediator-authored lines.** `git diff -U0` added lines of
  every B4-touched tracked file: zero U+2014. `B4-remediation.md`: zero.
- **Files outside the finding.** Versus the round-1 reviewed hashes, only
  `ci.yml` changed. Live-probe source, conversion formula bodies,
  composed-SQL tokens, H3 harness, digest-pinned image, and the B0
  composed-SQL pin were not retouched.
- **B0 composed-SQL pin.**
  `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` passed on
  the restored tree.
- **Continuation / redirect.** postgres-live `run: |` backslash lines have
  no trailing spaces. `2>&1 | tee` is a shell redirect, not a harness
  filter (same pattern as the sqlite-vectors row).

## Part C: gates rerun (my own runs, restored tree)

All rows are my runs on the hashes in the header. Nothing is copied from
`B4-remediation.md`. Live Cockroach tests: not run. Postgres container: not
started. `.env` and `models/`: not touched.

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | 0 | **pass** |
| `cargo test --features store-cockroach` | 0 | **940 passed / 0 failed / 4 ignored**, **944 listed**. Lib: 928 passed / 2 ignored |
| `cargo test --no-default-features --features store-cockroach` | 0 | **603 passed / 0 failed / 0 ignored**, **603 listed**. Lib: 594 passed |
| `cargo test --features store-cockroach,fixtures` | 0 | **1000 passed / 0 failed / 12 ignored**, **1012 listed**. Lib: 987 passed / 10 ignored |
| `cargo test --no-default-features --features store-postgres` | 0 | **590 passed / 0 failed / 4 ignored**, **594 listed**. Lib: 581 passed / 4 ignored (`init_schema_at_two_widths_creates_hnsw`, `explain_recall_uses_hnsw`, `fencing_refuses_stale_write_and_upserts_replay`, `live_schema_width_refuses_a_config_that_disagrees`). Parse unit `parse_pgvector_format_type_reads_vector_n` ok. Same listed count as B4 round 1 |
| B0 composed-SQL pin `--lib` `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` | 0 | **passed** |
| H1 lock: `--lib` `h1_sqlite_and_memory_oracle_agree_exactly` under `store-sqlite,store-cockroach,fixtures` | 0 | **passed** (0.07s). sqlite.rs hash matches round 1 |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54** `^warning:` lines including cargo's summary line; cargo reports **53** rustdoc warnings. **none** naming `store/pg` / `postgres.rs` / `PostgresDialect` / `assert_live_schema_width` / `parse_pgvector_format_type` |

No gate finding. Remediator's measured counts match mine. Cockroach listed
counts match B3 round 2 and B4 round 1.

## Summary of closures

| ID | Grade | Status |
| --- | --- | --- |
| B4-R1-1 | P2 | **HOLDS** (cargo 1.97.1: two names before `--` rc 1; both pairs after `--` plus `--list` rc 0 listing both tests; M2/M4/M5 still red) |

**0 P1, 0 P2, 0 P3 residue.** Round 1's P2 is now two `postgres-live` cargo
invocations that pass multiple live test names as libtest harness filters
after `--`. Two names before `--` is still invalid cargo 1.97.1. The live
check itself is the same bytes as round 1: parse rejects `VECTOR(n)`,
Postgres still ships the `format_type` probe, and B3 `1 - d` is unchanged.
sqlite.rs, the `resolve_backends` pin, digest-pinned `postgres-live`, and
the B0 composed-SQL pin still hold and were not reopened.

**Tree state at close**: restored to the reviewed state and verified. All
SHA-256 values in the header match, and `git status --short` is identical
to session start except for this file. Nothing was committed or pushed.

B4Review2, 2026-08-23

# Adversarial review: mooshik B, phase B4 (live-schema width), round 1

**Reviewer**: independent adversarial reviewer, agent_id `B4Review1`. Wrote
nothing under review except this file. No commit, no push.
**Scope**: the uncommitted B4 implementation on branch `b0-pg-extraction`
against `B-postgres-store.md` B4, plus the implementer's claims in
`b-run/B4-implementation.md`. Orchestrator-authored after the B4
implementor 402'd. Ignored: `local:/` and the orchestrator briefs.
**Worktree**: `/home/nryn/work/lambo`, branch `b0-pg-extraction` @ `e219488`
(B3 closed, round 2 APPROVE). Dirty tree at review start is the B4
implementation.
**Verdict**: **REQUEST_CHANGES**. 0 P1 / 1 P2 / 0 P3.

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
| `.github/workflows/ci.yml` | `cad743a1e7ae1af6d589a369b181163ee48dd597fc4f029fcbc8a209ab31bdba` |
| `CHANGELOG.md` | `214d5a3a3b6d76acba2e9d63240df1bee9bdfaba8f51608d1735e0aa4abd0408` |

`src/store/mod.rs`, `src/store/pg/cockroach.rs`, `src/store/sqlite.rs`,
`src/resolve.rs`, and `CHANGELOG.md` are byte-identical to the B3 round-2
reviewed state. Copied those files to `/tmp/b4-r1-review-snapshot/` before
any mutation. Every mutation cycle ends with a restore and a `sha256sum`
comparison against the table above.

## Method

1. Recalled dogfood memory (`B4Review1`). Read `B4-round1-review-brief.md`,
   `B4-implementation.md`, `B-postgres-store.md` B4, `b-run/CYCLE.md`, and
   house style `adve-review-mooshik-B-B3-round2.md`.
2. Mutation-tested every claimed pin. A pin holds only if the cited test
   FAILS under the mutation.
3. Independently re-ran every CYCLE gate on the restored tree, plus the
   `store-postgres` compile/unit row and the extra `store-postgres` clippy.
   Live Postgres: started, used, and removed only the pinned
   `pgvector/pgvector:pg17` digest
   `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
   Live Cockroach tests: not run. `.env` and `models/`: not touched.
4. Hunted for: echo-only reporting, `sqlite.rs` edits, B3 formula change,
   pin check at `resolve_backends` moved, leftover `:latest` on the
   pgvector image, leftover mutation, em dashes in B4-authored added
   lines, B0 composed-SQL pin broken.

Unmutated pins were green before the first mutation
(`--no-default-features --features store-postgres` for parse and the live
mismatch; `--features store-cockroach` for the Cockroach module and B0
pin). After restore, incremental compile was forced with `touch` of the
restored files.

## Part A: claimed pins, mutation-tested

| # | Claim | Cited test | Mutation | Result |
| --- | --- | --- | --- | --- |
| M1 | skip `assert_live_schema_width` in `preflight_schema` | `live_schema_width_refuses_a_config_that_disagrees` | delete the call at `pg/mod.rs:2326` | **RED** at `postgres.rs:621`: `expect_err("1536 process against vector(768) must fail")` got `Ok(())` |
| M1b | same helper is also issued from `init_schema` | same, plus `init_schema_at_two_widths_creates_hnsw` | delete the call at `pg/mod.rs:2274` only | **GREEN**. Both live tests still pass. Not elevated: the mismatch test attaches via `preflight_schema`, which still calls the helper. |
| M2 | parse rejects `VECTOR(n)` | `parse_pgvector_format_type_reads_vector_n` | `to_ascii_lowercase` before `strip_prefix("vector(")` | **RED** at `postgres.rs:589`: left `Some(768)`, right `None` (`Cockroach spelling is not pgvector`) |
| M3 | Cockroach `live_schema_vector_width_sql` stays `None` | none exists | add a `Some(format_type ...)` override on `CockroachDialect` | **GREEN**. All 27 `store::pg::cockroach::tests` unit tests pass. Dual-feature parse test still passes (`is_some()` is Postgres-only). Not elevated: default at `dialect.rs:88-90` is `None`, Cockroach does not override, and a copied probe would fail loud on `VECTOR(n)` (M2) rather than mis-rank. |
| M4 | Postgres SQL is present | `parse_pgvector_format_type_reads_vector_n` | `PostgresDialect::live_schema_vector_width_sql` returns `None` | **RED** at `postgres.rs:591`: `assert!(...is_some())` |
| M5 | B3 Postgres score is still `1 - d` | `distance_to_score_is_one_minus_d` | body `(1.0 - dist).clamp` to `(1.0 - 0.5 * dist * dist).clamp` | **RED** at `postgres.rs:253`: left `0.5`, right `0.0` (`d=1`) |

M1 is not vacuous: with the preflight call gone, `preflight_schema` returns
`Ok(())` against a 768 column, so `expect_err` is the failure, not a
string-contains tautology. M2 fails at the named `VECTOR(768)` assert, not
only at a happy-path `vector(768)`. M4 is a unit pin for "Postgres probes";
M1 is the wiring pin that SQLite cannot have. There is no unit equivalent
of M1: the parse test stayed green under M1.

Unmutated live mismatch and two-width, before the first mutation, both
passed on the pinned digest (0.71s). After restore they passed again
(0.46s).

Mutation score: **2/2 required load-bearing pins were caught (M1, M2).
M4 extra SQL-presence pin caught. M5 B3 formula pin still caught. 0/1
on Cockroach `None` (no test).**

## Part B: hunt for defects introduced by B4

### B4-R1-1 (P2) `postgres-live` first step is not a valid cargo 1.97.1 invocation

B4 adds the mismatch test to the `postgres-live` first step
(`.github/workflows/ci.yml:305-311`) by passing **two** fully-qualified
names as cargo `TESTNAME` arguments before `--`:

```
cargo test --no-default-features --features store-postgres --lib \
  store::pg::postgres::tests::init_schema_at_two_widths_creates_hnsw \
  store::pg::postgres::tests::live_schema_width_refuses_a_config_that_disagrees \
  -- --ignored --nocapture --exact
```

Pinned toolchain is cargo 1.97.1 (`Usage: cargo test [OPTIONS] [TESTNAME]
[-- [ARGS]...]`, singular). Reproduced against this tree:

```
error: unexpected argument
'store::pg::postgres::tests::live_schema_width_refuses_a_config_that_disagrees'
found
```

The same two names *after* `--` list both tests (rc 0). So the greps at
`:310-311` never run: cargo never starts. The claimed CI pin of
`live_schema_width_refuses_a_config_that_disagrees` is dead, and the
previously valid one-name two-width invocation is broken with it.

The matrix `postgres` row (`cargo test --no-default-features --features
store-postgres`) still compiles and runs the unit parse pin. The live
mismatch is `#[ignore]` and exists only on this job.

Pre-existing, not reopened as a separate finding: the B3 step at
`ci.yml:315-318` has the same two-`TESTNAME` shape
(`explain_recall_uses_hnsw` + `fencing_refuses_stale_write_and_upserts_replay`)
and is rejected the same way. The H3 step has one name and is valid.
A green `postgres-live` job still requires the B3 step to be valid cargo;
fix both while here.

**Closure.** Pass the test names as harness filters after `--`, not as a
second cargo `TESTNAME`. Keep `--ignored --nocapture --exact` and both
greps. Apply the same shape to the B3 step in this job.

### Attack vectors examined, not elevated

- **Echo-only reporting.** Ruled out for the attach check: M1 turns the
  live mismatch test red because preflight returns `Ok(())`.
  `GraphStore::vector_dimensions` still returns construction dim
  (`pg/mod.rs:2334-2338`). That is honest: construction is I/O-free, and
  after a successful probe construction dim equals live `vector(n)`. The
  two-width `assert_eq!(store.vector_dimensions(), Some(dim))` is
  tautological as a live-read (the store was constructed at `dim`); the
  mismatch test is the load-bearing pin. Skipping only the `init_schema`
  call (M1b) stays green; not elevated, because attach still probes.
- **`sqlite.rs` / H1.** sqlite.rs hash matches B3 round 2.
  `git diff HEAD -- src/store/sqlite.rs` is empty.
  `h1_sqlite_and_memory_oracle_agree_exactly` passed (0.08s) under
  `store-sqlite,store-cockroach,fixtures`.
- **B3 formula change.** `distance_to_score` is not in the B4 diff.
  Production body is still `(1.0 - dist).clamp(-1.0, 1.0)`
  (`postgres.rs:97`). Cockroach.rs hash matches B3 round 2. M5 goes red
  at `d=1`.
- **Pin at `resolve_backends`.** `src/resolve.rs` and `src/store/mod.rs`
  hashes match B3 round 2. The kind-agnostic pin comparison is still
  `resolve.rs:143-154`, before `check_vector_compatibility`.
- **Cockroach live probe.** `CockroachDialect` does not override
  `live_schema_vector_width_sql`; the trait default is `None`
  (`dialect.rs:88-90`). M3 is green (unpinned). Not elevated: a copied
  Postgres probe would fail loud on `VECTOR(n)` (M2), not silently
  mis-rank.
- **Leftover `:latest`.** `.github/workflows/ci.yml:278` is still
  `pgvector/pgvector:pg17@sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
  The only `:latest` token in that file is the comment that the image is
  never `:latest`. My live container was
  `Image=sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`,
  then removed. Pre-existing exited `lambo-crdb` was not started or
  removed.
- **B0 composed-SQL pin.**
  `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` passed
  on the restored tree.
- **Em dashes in B4-authored added lines.** `git diff -U0` added lines of
  every B4-touched tracked file: zero U+2014. `B4-implementation.md`:
  zero.
- **Leftover mutation.** After restore: preflight and init both call
  `assert_live_schema_width`. Parse has no `to_ascii_lowercase`.
  Postgres formula is `1.0 - dist`. Cockroach has no
  `live_schema_vector_width_sql` override. All SHA-256 values in the
  header match.
- **CYCLE merge-once.** Intact: B stays on `b0-pg-extraction` until B0
  through B4 are all closed; one merge to `lambo-for-mooshik` at the end
  of B. Status line still says "B4 is next"; that is orchestrator-owned.

## Part C: gates rerun (my own runs, restored tree)

All rows are my runs on the hashes in the header. Nothing is copied from
`B4-implementation.md`. Live Cockroach tests: not run. `.env` and
`models/`: not touched. Live Postgres: pinned digest only, container
removed at the end.

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | 0 | **pass** |
| `cargo test --features store-cockroach` | 0 | **940 passed / 0 failed / 4 ignored**, **944 listed**. Lib: 928 passed / 2 ignored. Same listed count as B3 round 2 |
| `cargo test --no-default-features --features store-cockroach` | 0 | **603 passed / 0 failed / 0 ignored**, **603 listed**. Lib: 594 passed. Same as B3 round 2 |
| `cargo test --features store-cockroach,fixtures` | 0 | **1000 passed / 0 failed / 12 ignored**, **1012 listed**. Lib: 987 passed / 10 ignored. Same as B3 round 2 |
| `cargo test --no-default-features --features store-postgres` | 0 | **590 passed / 0 failed / 4 ignored**, **594 listed**. Lib: 581 passed / 4 ignored. +1 unit (`parse_pgvector_format_type_reads_vector_n`) and +1 ignored (`live_schema_width_refuses_a_config_that_disagrees`) over B3 round 2's 592 listed |
| B0 composed-SQL pin `--lib` `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` | 0 | **passed** |
| H1 lock: `--lib` `h1_sqlite_and_memory_oracle_agree_exactly` under `store-sqlite,store-cockroach,fixtures` | 0 | **passed** (0.08s). sqlite.rs hash matches B3 round 2 |
| Live mismatch + two-width on pinned digest | 0 | **passed** unmutated (0.71s) and after restore (0.46s). M1 was **RED** first |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54** `^warning:` lines including cargo's summary line; cargo reports **53** rustdoc warnings. **none** naming `store/pg` / `postgres.rs` / `PostgresDialect` / `assert_live_schema_width` / `parse_pgvector_format_type` |

No gate finding. Cockroach listed counts match B3 round 2. CYCLE.md still
quotes the B0-R1-1 939 / 598 / 1007 numbers; that table is stale since B3
and is orchestrator-owned.

## Summary of findings

| ID | Grade | Status |
| --- | --- | --- |
| B4-R1-1 | P2 | **OPEN**. `postgres-live` first step is invalid cargo 1.97.1 (two `TESTNAME` args). The live mismatch grep cannot run, and two-width on that step is broken with it. |

**0 P1, 1 P2, 0 P3 residue.** The live check itself holds: skipping
`assert_live_schema_width` in `preflight_schema` turns
`live_schema_width_refuses_a_config_that_disagrees` red, parse rejects
`VECTOR(n)`, and B3 `1 - d` is unchanged. CI is what does not hold.
Cockroach still skips the live probe (static DDL). sqlite.rs, the
`resolve_backends` pin, and H1 are untouched.

**Tree state at close**: restored to the reviewed state and verified. All
SHA-256 values in the header match, and `git status --short` is identical
to session start except for this file. Nothing was committed or pushed.

B4Review1, 2026-08-23

---

## Closures (round-1 remediation, 2026-08-23)

Remediator: `b4-remediator`. Work on `b0-pg-extraction`, tree left dirty, not
committed. This section records what closed; it does **not** change the
**REQUEST_CHANGES** verdict or the Findings table above.

| ID | Grade | Status | What |
| --- | --- | --- | --- |
| B4-R1-1 | P2 | **closed** | `postgres-live` first step (`ci.yml:305-308`) and the B3 two-name step (`ci.yml:315-318`) now pass both names as harness filters after `--`. `--ignored --nocapture --exact` and both greps kept. Cargo 1.97.1 (`Usage: cargo test [OPTIONS] [TESTNAME] [-- [ARGS]...]`) rejects two `TESTNAME` args (rc 1); the new shape plus `--list` lists both tests (rc 0). H3 step and the live-schema probe were not edited. |

Gates re-run by the remediator: fmt, three clippy `-D warnings` rows (default,
store-cockroach, store-postgres), the four test commands (944 / 603 / 1012 /
594 listed), H1 lock green with no sqlite.rs remediator hunk, B0 composed-SQL
pin green, and `cargo doc --document-private-items` at 54 warnings. Live
Postgres was not started. Detail in
`dev-diary/lambo-for-mooshik/b-run/B4-remediation.md`.

# Adversarial review: mooshik B, phase B1 (Postgres kind and alias split), round 2

**Reviewer**: independent adversarial reviewer, agent_id `B1Review2`. Wrote
nothing under review except this file. No commit, no push.
**Scope**: the uncommitted B1 round-1 remediation on branch `b0-pg-extraction`
against `adve-review-mooshik-B-B1-round1.md` (0 P1 / 1 P2 / 4 P3,
REQUEST_CHANGES) and the remediator's claims in `b-run/B1-remediation.md`.
**Worktree**: `/home/nryn/work/lambo`, branch `b0-pg-extraction` @ `9d9d8d7`
(B0 closed, round 2 APPROVE). Dirty tree at review start is the B1
implementation plus the round-1 remediation and the round-1 review closures
appendix. Ignored: `local:/` and the orchestrator briefs.
**Verdict**: **APPROVE**. All five closures hold. Zero failed closures. Zero
new findings.

Reviewed-state SHA-256, taken before any mutation and re-verified after every
mutation cycle and again at the end of the review:

| File | SHA-256 |
| --- | --- |
| `src/store/mod.rs` | `95110b504080a6aa90b1dfecf05ec4ea1fb79e9392550f95f040fb3a285cd369` |
| `src/store/pg/postgres.rs` | `0310eeb8ab98dcb9a29e51b7f35624f5125665ff7f4de25936ec54b5cd5686f9` |
| `src/store/pg/mod.rs` | `c80449a7e9dcaf42227f87a37df10c0bf3ebe9f7188a8931d71a3a300d2dae9b` |
| `src/store/pg/dialect.rs` | `7db79531d406e1119059215e5e3b0b10b5cb29658324e3303e4834f0619b3ed4` |
| `src/mcp/endpoint.rs` | `6280048ecd0c83efeef80317c307ff9156f0327e48fc785d3a32f17aa99e20b9` |
| `src/cli/provision.rs` | `a9680770a7a496747c3925607a54eb9150c1d4d02d9243d8ee0deb18458ab55e` |

`postgres.rs` and `dialect.rs` are byte-identical to the round-1 reviewed
state. `mod.rs` differs by the R1-4 colon in `postgres_build_behavior`.
`pg/mod.rs` differs by the R1-4 colon in the B1 dialect module comment.
`endpoint.rs` and `provision.rs` differ by the R1-1 / R1-2 / R1-3 pins.

Copied the six files to `/tmp/b1-r2-review-snapshot/` before any mutation.
Every mutation cycle ends with a restore and a `sha256sum` comparison against
the table above.

## Method

1. Recalled dogfood memory (`B1Review2`). Read `B1-round2-review-brief.md`,
   `B1-remediation.md`, the round-1 review plus its closures appendix,
   `B-postgres-store.md` B1, `CYCLE.md`, and house style
   `adve-review-mooshik-B-B0-round2.md`.
2. Mutation-tested every closure that has an offline pin. A closure holds only
   if the cited test FAILS under the mutation. R1-4 and R1-5 have no test to
   fail: they are labelled **trace**.
3. Independently re-ran every CYCLE gate on the restored tree, plus the
   `store-postgres` compile/unit row and the extra `store-postgres` clippy.
   Live Cockroach tests: not run. Postgres container: not started. `.env` and
   `models/`: not touched.
4. Hunted for: vacuous pin, DSN split remaining, Cockroach SQL in
   `postgres.rs`, `sqlite.rs` edits, `init_schema` / `connect_options` split,
   leftover mutation, em dashes in remediator-authored lines, CYCLE merge-once
   wording dropped, B0 reopened.

Unmutated pins were green before the first mutation (`--features
store-cockroach`). After restore, incremental compile was forced with `touch`
of the restored files.

## Part A: per-finding closure verification

| Finding | Verdict | Verification |
| --- | --- | --- |
| B1-R1-1 (P2) omitted-database and libpq pairs | **HOLDS** | `two_spellings_of_one_database_derive_one_endpoint` (`endpoint.rs:1495`) now uses a DSN without `/db`. Omitted pair: `postgres://u@host` vs `postgres://u@host/u` (`endpoint.rs:1550-1567`). Libpq pair: `host=host user=u dbname=db` vs `postgres://u@host/db` (`endpoint.rs:1568-1580`). `canonical_store_dsn` pins the emitted identity (`endpoint.rs:1621-1628`). Production rule is still `DsnParts::to_identity` (`endpoint.rs:731-735`): empty database becomes the explicit username. Mutations below: M8, port, password, and libpq parser-off all red. |
| B1-R1-2 (P3) shareable exhaustiveness | **HOLDS** | New test `store_is_shareable_is_ruled_not_defaulted` (`endpoint.rs:1636`) names every `StoreKind` via an exhaustive helper and forbids `_ =>` in the `store_is_shareable` body (`include_str` split, not the test's own `arm` helper: collapsing production arms to `_ => true` went red, so the split is not vacuous). Value pin `a_process_private_store_advertises_no_endpoint` still goes red when Postgres is `false`. Complementary: `_ => true` leaves the value pin green; Postgres `false` leaves the exhaustiveness pin green. |
| B1-R1-3 (P3) provision Postgres arm | **HOLDS** | `cli::provision::postgres_arm_tests::provision_postgres_fails_closed_naming_b2` (`provision.rs:215`) calls `run(..., StoreKind::Postgres)` with a dummy `GraphStore` that panics if touched. Expects B2, names postgres, refuses `provision.sh`. Arm `Ok(...)` red at `expect_err`. `Sqlite \| Postgres` sharing `init_schema` red at the dummy panic. Did not merge into the Cockroach arm: `.env` exists in the workdir and `scripts/provision.sh` would source it. |
| B1-R1-4 (P3) two em dashes | **HOLDS (trace)** | `src/store/mod.rs:1218` is `expected err: silent fallback forbidden`. `src/store/pg/mod.rs:65` is `B1: PostgreSQL + pgvector dialect`. Zero U+2014 in remediator-added lines (`git diff -U0` of every B1-touched tracked file, plus untracked `postgres.rs` / `CHANGELOG.md` / `B1-remediation.md`). The Cockroach sibling panic at `mod.rs:1185` still has an em dash; that is pre-existing and was left alone, as round 1 asked. |
| B1-R1-5 (P3) config.mdx names postgres | **HOLDS (trace)** | Both copies say use `sqlite`, `cockroach`, or `postgres` when a separate command-line process has to see the same session: `docs/reference/config.mdx:50` and `site/src/content/docs/config.mdx:52`. Edited in lockstep. |

### B1-R1-1 / R1-2 / R1-3 mutations (restored after each)

Each cycle: mutate, run the cited `--lib` test, restore from snapshot,
`sha256sum` check, `touch` so cargo cannot reuse a mutated artifact.

| # | Closure | Mutation | Cited test | Result |
| --- | --- | --- | --- | --- |
| M8 | R1-1 | `to_identity` uses raw `database` even when empty | `two_spellings_of_one_database_derive_one_endpoint` | **RED** at `endpoint.rs:1552` (restored pin `endpoint.rs:1556`): omitted `/db` vs `/u` mint two sockets (`omitting the database must not mint a second endpoint`) |
| port | R1-1 | URL omitted port `unwrap_or(0)` | same | **RED** at `endpoint.rs:1504`: omitted vs `:5432` |
| password | R1-1 | userinfo kept as `user:password` | same | **RED** at `endpoint.rs:1524` (restored pin `endpoint.rs:1525`): `"password is a credential, not the database"` |
| libpq | R1-1 | `parse_libpq_kv` always `None` | same | **RED** at `endpoint.rs:1572`: `"libpq key=value must derive the same endpoint as the URL form"` |
| `_ =>` | R1-2 | Sqlite still special-cased; Cockroach+Postgres collapsed to `_ => true` | `store_is_shareable_is_ruled_not_defaulted` | **RED** at `endpoint.rs:1653`: must name `StoreKind::Cockroach =>`. Value pin `a_process_private_store_advertises_no_endpoint` stayed **GREEN** |
| value | R1-2 | Postgres arm `true` to `false` | `a_process_private_store_advertises_no_endpoint` | **RED** at `endpoint.rs:1483`: Postgres `for_store` is `None`. Exhaustiveness test stayed **GREEN** (arm still named) |
| Ok | R1-3 | Postgres arm returns `Ok(...)` | `provision_postgres_fails_closed_naming_b2` | **RED** at `provision.rs:215`: `expect_err` |
| sqlite-merge | R1-3 | `Sqlite \| Postgres` share `init_schema` | same | **RED** at `provision.rs:146`: dummy panics `"must not touch the store"` |

M8 is the hole round 1 opened. Port and password were re-proved red on the
same test after the omitted-database assertions landed. Libpq is the cheap
extra pair round 1 asked for; disabling the parser is a distinct red from M8.

Mutation score: **8/8 attempted closure mutations were caught.** None of the
eight pins passed regardless of the fix.

## Part B: hunt for defects introduced by the remediation

No new findings. Specific attack vectors examined:

- **Vacuous pin.** Ruled out for R1-1: M8 fails the `at()` endpoint compare,
  not only `canonical_store_dsn` string equality, and the two sockets hash
  differently. Ruled out for R1-2: `include_str` is split to the production
  `store_is_shareable` body; if it had included the test's `arm` helper,
  `_ => true` would have stayed green. It went red. Ruled out for R1-3: both
  silent-success and wrong-arm-SQL go red; the dummy is what makes the
  Sqlite-merge fail without a real adapter.
- **DSN split remaining.** Query-overlay of identity keys (`host` / `port` /
  `dbname` / `user` in the URL query) is implemented (`apply_query_overlays`,
  `endpoint.rs:835`) and claimed in the B1 rule, but `two_spellings` never
  uses a `?dbname=` / `?host=` pair. No-op'ing the overlay left the cited
  test **GREEN** (the sslmode pair still matches because sslmode is dropped).
  Not elevated: this hole was already present in round 1; the fix asked for
  omitted-database plus one libpq spelling, both now red; operator docs still
  show the path-form DSN with `sslmode` (pinned as dropped). Unix-socket
  `?host=/path` is the same unpinned overlay, not a remediation regression.
- **Copied Cockroach SQL.** `src/store/pg/postgres.rs` is byte-identical to
  round 1. No DDL, no `VECTOR(1024)`, no `<->`, no `::STRING` in executable
  SQL. Tokens remain `::TEXT` / `::vector` / `<=>`.
- **H1 / sqlite.rs.** `git diff -- src/store/sqlite.rs` is 0 bytes.
  `h1_sqlite_and_memory_oracle_agree_exactly` passed under
  `store-sqlite,store-cockroach,fixtures`.
- **Forbidden split of `init_schema` / `connect_options`.** Still one
  function each (`pg/mod.rs:2156` and `:1247`). `init_schema` still executes
  `ALTER TABLE session_leases ADD COLUMN IF NOT EXISTS endpoint STRING`
  (`pg/mod.rs:2187`). `git diff` of `pg/mod.rs` is module-doc, cfg gates for
  the postgres dialect, a fixtures-SQL cfg tighten, and the R1-4 colon.
  No split.
- **Leftover mutation.** After restore: `to_identity` still defaults empty
  database to username, `unwrap_or(PG_DEFAULT_PORT)` is 5432, userinfo still
  strips the password, `StoreKind::Postgres => true`, provision arm still
  returns `postgres_not_ready_msg("provision")`. All six SHA-256 values
  match the header.
- **CYCLE merge-once.** Intact: B stays on `b0-pg-extraction` until B0
  through B4 are all closed; one merge to `lambo-for-mooshik` at the end of
  B. B0 was not reopened. CYCLE listed-count table is still the B0-R1-1
  939 / 598 / 1007 figures; B1's new tests move the measured counts (Part C)
  and CYCLE does not claim otherwise for B1.
- **Em dashes in remediator prose.** `B1-remediation.md`, `CHANGELOG.md`,
  `postgres.rs`: zero. Tracked-file `git diff` added lines: zero U+2014.
- **CI row.** `.github/workflows/ci.yml` `feature-matrix` row `postgres` is
  still `cargo test --no-default-features --features store-postgres`. My run
  lists 580 and runs `store::pg::postgres::tests::*` plus the two new
  closure tests. No live database. Not in `ship` / `demo`.

## Part C: gates rerun (my own runs, restored tree)

All rows are my runs on the hashes in the header. Nothing is copied from
`B1-remediation.md`. Live Cockroach tests: not run. Postgres container: not
started. `.env` and `models/`: not touched.

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | 0 | **pass** |
| `cargo test --features store-cockroach` | 0 | **939 passed / 0 failed / 4 ignored**, **943 listed** (`: test$`). +2 over B1 implementation 941: `store_is_shareable_is_ruled_not_defaulted`, `provision_postgres_fails_closed_naming_b2` |
| `cargo test --no-default-features --features store-cockroach` | 0 | **602 passed / 0 failed / 0 ignored**, **602 listed**. Same +2 over 600 |
| `cargo test --features store-cockroach,fixtures` | 0 | **999 passed / 0 failed / 12 ignored**, **1011 listed**. Same +2 over 1009 |
| `cargo test --no-default-features --features store-postgres` | 0 | **580 passed / 0 failed / 0 ignored**, **580 listed**. `store::pg::postgres::tests::*` present, plus the two new closure tests |
| H1 lock: `--lib` `h1_sqlite_and_memory_oracle_agree_exactly` under `store-sqlite,store-cockroach,fixtures` | 0 | **passed**; `git diff src/store/sqlite.rs` **empty** (0 bytes) |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54** `^warning:` lines (B0/B1 counting method); **none** naming `store/pg` / `postgres.rs` / `PostgresDialect` |

No gate finding. Remediator's measured counts match mine.

## Summary of closures

| ID | Grade | Status |
| --- | --- | --- |
| B1-R1-1 | P2 | **HOLDS** (mutation: M8, port, password, libpq all red) |
| B1-R1-2 | P3 | **HOLDS** (mutation: `_ => true` red on exhaustiveness; Postgres `false` red on value) |
| B1-R1-3 | P3 | **HOLDS** (mutation: `Ok(...)` and Sqlite-merge both red) |
| B1-R1-4 | P3 | **HOLDS (trace)** |
| B1-R1-5 | P3 | **HOLDS (trace)** |

**0 P1, 0 P2, 0 P3 residue.** Round 1's P2 is now an omitted-database (and
libpq) pin on the same `two_spellings` test that already held for port and
password. Alias split, fail-closed dialect, uncompiled feature, shareable
value, CI row, H1 lock, and no `init_schema` split still hold from round 1
and were not reopened.

**Tree state at close**: restored to the reviewed state and verified. All six
SHA-256 values match the table in the header, and `git status --short` is
identical to session start except for this file. Nothing was committed or
pushed.

B1Review2, 2026-08-23

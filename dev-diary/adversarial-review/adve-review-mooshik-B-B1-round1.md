# Adversarial review: mooshik B, phase B1 (Postgres kind and alias split), round 1

**Reviewer**: independent adversarial reviewer, agent_id `B1Review1`. Wrote
nothing under review except this file. No commit, no push.
**Scope**: the uncommitted B1 implementation on branch `b0-pg-extraction`
against `B-postgres-store.md` B1 and "What workstream J left in B's path"
items 2 and 3, plus the implementer's claims in `b-run/B1-implementation.md`.
**Worktree**: `/home/nryn/work/lambo`, branch `b0-pg-extraction` @ `9d9d8d7`
(B0 closed, round 2 APPROVE). Dirty tree at review start is the B1
implementation (mod.rs, postgres.rs, endpoint.rs, CI, docs, CHANGELOG, etc.).
Ignored: `local:/` and the orchestrator briefs. `CYCLE.md` picked up a
one-hunk orchestrator edit during this review (merge-once wording); I did not
author it and did not revert it.
**Verdict**: **REQUEST_CHANGES**. 0 P1 / 1 P2 / 4 P3.

Reviewed-state SHA-256, taken before any mutation and re-verified after every
mutation cycle and again at the end of the review:

| File | SHA-256 |
| --- | --- |
| `src/store/mod.rs` | `2735f2d56081d8d022e509c1b4a7023f1e3793e1ae3d26b8d5e508d9f8ad4738` |
| `src/store/pg/postgres.rs` | `0310eeb8ab98dcb9a29e51b7f35624f5125665ff7f4de25936ec54b5cd5686f9` |
| `src/store/pg/mod.rs` | `314fbde4e938d3df304526cf08009e0dc03fc1bc9f28f2c45f49229f059f4563` |
| `src/store/pg/dialect.rs` | `7db79531d406e1119059215e5e3b0b10b5cb29658324e3303e4834f0619b3ed4` |
| `src/mcp/endpoint.rs` | `a7e505782f84eb50c3bd5319281936a4b9d0145e3d3ec563015b1d57c89c2aa9` |
| `src/cli/provision.rs` | `fad966c3bc24f96e921af9a3ada84b9cdaa1f882bd4f6a74fa8b6968cef28b1b` |

Copied the six files (plus Cargo.toml / ci.yml / error.rs) to
`/tmp/b1-r1-review-snapshot/` before any mutation. Every mutation cycle ends
with a restore and a `sha256sum` comparison against the table above.

## Method

1. Recalled dogfood memory (`B1Review1`). Read `B1-round1-review-brief.md`,
   `B1-implementation.md`, `B-postgres-store.md` (B1 and J's path items 2/3),
   `CYCLE.md`, and house style `adve-review-mooshik-B-B0-round2.md`.
2. Mutation-tested every claimed pin. A pin holds only if the cited test
   FAILS under the mutation.
3. Re-ran every CYCLE gate myself on the restored tree, plus the new
   `store-postgres` compile/unit row and the extra `store-postgres` clippy
   the implementer claimed. Live Cockroach tests: not run. Postgres
   container: not started. `.env` and `models/`: not touched.
4. Hunted for: copied Cockroach SQL, vacuous pin, default-port vs
   explicit-port still splitting, shareable defaulted via `_`, changelog/docs
   drift, CI row that does not compile `store-postgres`, H1 sqlite.rs
   touched, `init_schema` / `connect_options` split.

Unmutated pins were green before the first mutation (both
`--features store-cockroach` and `--no-default-features --features
store-postgres`). After restore, incremental compile was forced with `touch`
of the restored files.

## Part A: claimed pins, mutation-tested

| Claim | Cited test | Mutation | Result |
| --- | --- | --- | --- |
| Alias split: `postgres`/`pg` are Postgres, not Cockroach | `store::tests::parses_store_kind` | `"postgres" \| "pg"` mapped back onto `Cockroach` | **RED** at `mod.rs:1096`: left `Cockroach`, right `Postgres` (`"pg"` parse) |
| `store_is_shareable(Postgres) = true` | `mcp::endpoint::tests::a_process_private_store_advertises_no_endpoint` | Postgres arm `true` to `false` | **RED** at `endpoint.rs:1483`: Postgres `for_store` is `None` |
| Exhaustive match, no `_ =>` | same test | Sqlite still special-cased; Cockroach+Postgres collapsed to `_ => true` | **GREEN**. Finding **B1-R1-2** |
| Two DSN spellings of one database derive one endpoint | `mcp::endpoint::tests::two_spellings_of_one_database_derive_one_endpoint` | `canonical_store_dsn` returns `raw.trim()` (hash spelling) | **RED** at `endpoint.rs:1494`: omitted port vs `:5432` mint two sockets |
| Password stripped from identity | same test | userinfo kept as `user:password` | **RED** at `endpoint.rs:1522`: `"password is a credential, not the database"`; two credentials, two sockets |
| Omitted port is 5432, not Cockroach 26257 | same test | omitted port `0`; separately default port `26257` | **RED** both times at `endpoint.rs:1504`: omitted vs `:5432` split |
| Omitted database defaults to username (claimed rule) | same test | `to_identity` uses raw `database` even when empty | **GREEN**. Finding **B1-R1-1** |
| Dialect tokens are not Cockroach SQL | `store::pg::postgres::tests::dialect_tokens_are_not_cockroach_sql` | `STRING_CAST`/`VECTOR_CAST`/`DISTANCE_OP` copied from Cockroach | **RED** at `postgres.rs:77`: `"::STRING" == "::STRING"` |
| `init_sql` fails closed naming B2 | `store::pg::postgres::tests::init_sql_and_vector_dim_name_b2` | `init_sql` returns `Ok("CREATE TABLE concepts (embedding VECTOR(1024));")` | **RED** at `postgres.rs:99`: `unwrap_err` on `Ok` |
| `vector_dim` fails closed naming B2 | same test | `vector_dim` returns `Ok(1024)` | **RED** at `postgres.rs:101` |
| `distance_to_score` does not guess a formula | `distance_to_score_does_not_guess_a_formula` (`should_panic` B3) | Cockroach formula `1 - d²/2` | **RED**: test did not panic |
| `build_store(Postgres)` fail-closed at the arm | `store::tests::postgres_build_behavior` | arm calls `PostgresStore::new(cfg)?` | **GREEN** (dialect backstop still returns B2). Hunt **B1-R1-4** |
| Uncompiled `StoreKind::Postgres` fails loud | `postgres_build_behavior` on `--features store-cockroach` | `is_compiled` always `true` | **RED** at `mod.rs:1213`: compiled branch expected B2, got `not compiled` / `store-postgres` |
| `lambo provision` Postgres arm does not run Cockroach SQL | none | `Cockroach \| Postgres` share the `provision.sh` arm | **GREEN** on the tests that exist. Finding **B1-R1-3** |

Alias split, shareable *value*, headline DSN identity (port / scheme / host
case / sslmode / password), dialect fail-closed, silent-mis-rank backstop,
and uncompiled-feature loudness all hold. Default-port vs explicit-port does
**not** still split.

`toml_kind_aliases_match_from_str` was not separately mutated: Deserialize
calls `FromStr`, so M1 is the same mapping.

## Part B: hunt

Specific attack vectors:

- **Copied Cockroach SQL.** `src/store/pg/postgres.rs` has no DDL, no
  `VECTOR(1024)`, no `<->`, no `::STRING` in executable SQL. Tokens are
  `::TEXT` / `::vector` / `<=>`. M9/M10/M11 all red. Not a finding.
- **Vacuous token pin.** `dialect_tokens_are_not_cockroach_sql` is
  `assert_ne!` against Cockroach spellings, not `assert_eq!` to the B3
  table. A `::VARCHAR` would stay green. Appropriate for B1's "not
  Cockroach" claim; B3 owns the positive spellings. Not elevated.
- **Default-port vs explicit-port still splitting.** Ruled out (M4, M6, M7).
- **Password in the published path.** The path is an FNV-1a hex filename.
  The test also asserts `store_identity` itself does not contain `s3cret`
  (pre-hash). M5 red. Holds.
- **Kind is in the identity.** Same DSN on Cockroach vs Postgres is two
  endpoints, asserted in `two_spellings`. Not mutated separately; the
  `{:?}` of `StoreKind` is in `store_identity`.
- **Shareable defaulted via `_`.** Proven: M3 green. **B1-R1-2**.
- **CI row does not compile `store-postgres`.** `.github/workflows/ci.yml`
  `feature-matrix` row `postgres` is `cargo test --no-default-features
  --features store-postgres`. My run: **578 listed**, 578 passed, and
  `store::pg::postgres::tests::*` are in `--list`. Compiles the feature.
  Not in `ship` / `demo`. No `postgres-live` container. Holds.
- **H1 / sqlite.rs.** `git diff -- src/store/sqlite.rs` is 0 bytes.
  `h1_sqlite_and_memory_oracle_agree_exactly` passed under
  `store-sqlite,store-cockroach,fixtures`.
- **Forbidden split of `init_schema` / `connect_options`.** Still one
  function each (`pg/mod.rs:1247` and `:2156`). `init_schema` still
  executes `ALTER TABLE session_leases ADD COLUMN IF NOT EXISTS endpoint
  STRING`. `git diff` of `pg/mod.rs` is module-doc, cfg gates for the
  postgres dialect, and a fixtures-SQL cfg tighten so store-postgres-only
  is dead-code-clean. No split.
- **Changelog.** New untracked `CHANGELOG.md` (never in HEAD) records the
  alias split as 0.3.0 breaking, DSN identity as Added, shareable ruling.
  DSN normalisation also moves Cockroach endpoint hashes on upgrade; both
  processes on 0.3.0 still agree. Not elevated: 0.3.0 is already a break.
- **Docs drift.** Alias split is on `lambo.example.toml`, both config.mdx
  copies, and `level-b-pluggability.md`. Residual: config.mdx still tells
  operators to use `sqlite` or `cockroach` for a second process, omitting
  the new shareable kind. **B1-R1-5**.
- **Em dashes in B1-authored lines.** CYCLE standing rule. Two added:
  `postgres_build_behavior`'s copied panic string (`mod.rs:1218`), and
  the `pg/mod.rs:65` dialect module comment (`B1` then U+2014 then
  `PostgreSQL + pgvector dialect`). Pre-existing dashes in endpoint.rs
  / store/mod.rs were not part of this phase. **B1-R1-4**.
- **`build_store` arm vs dialect backstop.** Calling `PostgresStore::new`
  still fails closed through `vector_dim` when the DSN parses, so
  `postgres_build_behavior` stays green. The stated reason for skipping
  construction (do not launder `PgStore::new`'s `"invalid Cockroach DSN"` /
  `"CockroachStore requires a DSN"` into a Postgres miss) is untested: every
  postgres construction test supplies a valid DSN. Not a silent-mis-rank
  path today. Noted under **B1-R1-3**'s neighbour: the operator CLI still
  dies at `build_store` with B2.

## Part C: gates rerun (my own runs, restored tree)

All rows are my runs on the hashes in the header. Nothing is copied from
`B1-implementation.md`.

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | 0 | **pass** (extra, claimed) |
| `cargo test --features store-cockroach` | 0 | **937 passed / 0 failed / 4 ignored**, 941 listed (`: test$`). +2 over B0's 939: `postgres_build_behavior`, `two_spellings_of_one_database_derive_one_endpoint` |
| `cargo test --no-default-features --features store-cockroach` | 0 | **600 passed / 0 failed / 0 ignored**, 600 listed. Same +2 over B0's 598 |
| `cargo test --features store-cockroach,fixtures` | 0 | **997 passed / 0 failed / 12 ignored**, 1009 listed. Same +2 over B0's 1007 |
| `cargo test --no-default-features --features store-postgres` | 0 | **578 passed / 0 failed / 0 ignored**, 578 listed. `store::pg::postgres::tests::*` present |
| H1 lock: `--lib` `h1_sqlite_and_memory_oracle_agree_exactly` under `store-sqlite,store-cockroach,fixtures` | 0 | **passed**; `git diff src/store/sqlite.rs` **empty** (0 bytes) |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54** `^warning:` lines (B0 baseline counting method); **none** naming `store/pg` / `postgres.rs` / `PostgresDialect` |

`#[ignore]`d live Cockroach tests: **not run**. Postgres container: **not
started**. No gate finding. Implementer's measured counts match mine.

## Findings

### B1-R1-1 (P2) omitted-database identity is claimed and unpinned

`canonical_store_dsn` / `DsnParts::to_identity` (`endpoint.rs:731-735`)
defaults an omitted database to the explicit username, matching libpq, and
the implementation record lists that rule next to default port and host
case. `two_spellings_of_one_database_derive_one_endpoint` never uses a DSN
without `/db`.

**Mutation M8:** delete the default-db-to-username branch. Cited test:
**GREEN**. `postgres://u@host` and `postgres://u@host/u` are one database
under libpq and would derive two session endpoints if that branch vanished,
which is the J2-R1-2 hazard in a second spelling.

The headline pair (omitted port vs `:5432`) is well pinned (M4/M6/M7 red),
as is password stripping (M5 red). This is the hole in the same pin.

The same test also never exercises the claimed libpq `key=value` parser
(`host=host user=u dbname=db` vs the URL form). Not separately mutated;
there is no assertion to go red.

**Fix:** add the omitted-database pair (and, cheaply, one libpq spelling)
to `two_spellings_of_one_database_derive_one_endpoint` so M8 goes red.

### B1-R1-2 (P3) shareable pin does not pin exhaustiveness

`store_is_shareable(Postgres) = true` is pinned (M2 red). Replacing the
Cockroach and Postgres arms with `_ => true`, leaving Sqlite's in-memory
check in place, leaves the cited test **green**. A future `StoreKind` would
then inherit shareable without a ruling, which is the shape J's path item 2
asked the compiler to prevent.

The current match is exhaustive. The test pins the B1 value, not the "ruled,
not defaulted" shape the implementation claims it pins.

**Fix:** a compile-fail or an exhaustive helper that names every variant in
the test, or drop the "pinned by" sentence so the compiler remains the
ruling.

### B1-R1-3 (P3) `provision::run` Postgres arm has no test

`lambo provision` fails closed for operators because `resolve_store_only`
calls `build_store`, and `postgres_build_behavior` pins that. The
`provision::run` arm itself (`provision.rs:34-40`) that refuses to execute
`scripts/provision.sh` is untested. Merging `StoreKind::Postgres` into the
Cockroach arm compiles; `provision_memory_store_succeeds_without_sql` stays
green.

Defense in depth for B2, when `build_store` starts constructing. Cheap to
pin with a `run(..., StoreKind::Postgres)` that expects B2 and does not
need a real store.

### B1-R1-4 (P3) two em dashes in B1-authored lines

CYCLE standing rule: no em dashes in prose, docs, comments, or commit text
this run writes. Added by this phase:

* `src/store/mod.rs:1218` panic string `expected err` then U+2014 then
  `silent fallback forbidden` (copied from the Cockroach sibling)
* `src/store/pg/mod.rs:65` comment `B1` then U+2014 then
  `PostgreSQL + pgvector dialect`

`postgres.rs`, `CHANGELOG.md`, and `B1-implementation.md` are clean.

### B1-R1-5 (P3) config.mdx still omits Postgres as a shareable store

Both `docs/reference/config.mdx:50` and
`site/src/content/docs/config.mdx` (same sentence) still say: use `sqlite`
or `cockroach` when a separate command-line process has to see the same
session. B1's shareable ruling makes `postgres` the same class. Alias-split
sentences on the same page were updated.

## Mutation score

**10 of 14** attempted mutations were caught. The four greens are
B1-R1-1 (M8), B1-R1-2 (M3), B1-R1-3 (M14), and the build_store-arm
construct (M13) which is absorbed into B1-R1-3's neighbourhood rather than
graded separately: construction still fails closed through the dialect, so
it is not a silent success.

Caught (red): M1 alias, M2 shareable false, M4 hash spelling, M5 password,
M6 omitted port, M7 port 26257, M9 Cockroach tokens, M10 init_sql SQL,
M11 guessed formula, M12 vector_dim Ok, M15 uncompiled `is_compiled` lie.

## Summary

| ID | Grade | Status |
| --- | --- | --- |
| B1-R1-1 | P2 | **OPEN** (DSN omitted-database identity unpinned; M8 green) |
| B1-R1-2 | P3 | **OPEN** (`_ => true` does not fail the shareable test) |
| B1-R1-3 | P3 | **OPEN** (provision Postgres arm untested) |
| B1-R1-4 | P3 | **OPEN** (two B1 em dashes) |
| B1-R1-5 | P3 | **OPEN** (config.mdx shareable sentence) |

**0 P1, 1 P2, 4 P3.** Alias split, fail-closed dialect, uncompiled feature,
headline DSN (port / password / scheme / host case), shareable *value*, CI
row, H1 lock, and no `init_schema` split all hold. The P2 is a pin hole in
the DSN identity rule J forced onto B1, not a behaviour bug in the current
tree.

**Tree state at close**: restored to the reviewed state and verified. All
six SHA-256 values match the table in the header. `git status --short` is
identical to session start except for this file and the orchestrator's
unrelated `CYCLE.md` hunk. Nothing was committed or pushed.

B1Review1, 2026-08-23

---

## Closures (round-1 remediation, 2026-08-23)

Remediator: `b1-remediator`. Work on `b0-pg-extraction`, tree left dirty, not
committed. This section records what closed; it does **not** change the
**REQUEST_CHANGES** verdict or the Summary table above.

| ID | Grade | Status | What |
| --- | --- | --- | --- |
| B1-R1-1 | P2 | **closed** | `two_spellings_of_one_database_derive_one_endpoint` now includes `postgres://u@host` vs `postgres://u@host/u` (`endpoint.rs:1550`) and libpq `host=host user=u dbname=db` vs the URL form (`endpoint.rs:1568`). M8 (delete default-db-to-username) goes **RED** at `endpoint.rs:1556`. Omitted-port (`endpoint.rs:1504`) and password (`endpoint.rs:1525`) pins stay red. Reverted. |
| B1-R1-2 | P3 | **closed** | `store_is_shareable_is_ruled_not_defaulted` (`endpoint.rs:1636`) names every `StoreKind` and forbids `_ =>` in the function body. `_ => true` with Sqlite special-cased goes **RED** at `endpoint.rs:1653`. Value pin unchanged. |
| B1-R1-3 | P3 | **closed** | `provision_postgres_fails_closed_naming_b2` (`provision.rs:215`): `run(..., StoreKind::Postgres)` with a dummy store, expects B2, refuses `provision.sh`. Arm `Ok(...)` and Sqlite-merge both red. Cockroach-arm merge not used: `.env` is present. |
| B1-R1-4 | P3 | **closed** | `mod.rs:1218` panic uses a colon. `pg/mod.rs:65` comment uses a colon. |
| B1-R1-5 | P3 | **closed** | `docs/reference/config.mdx:50` and `site/src/content/docs/config.mdx:52` name postgres as shareable, in lockstep. |

Gates re-run by the remediator: fmt, three clippy `-D warnings` rows (default,
store-cockroach, store-postgres), the four test commands (943 / 602 / 1011 /
580 listed), H1 lock green with `sqlite.rs` diff empty, and
`cargo doc --document-private-items` at 54 warnings. Detail in
`dev-diary/lambo-for-mooshik/b-run/B1-remediation.md`.

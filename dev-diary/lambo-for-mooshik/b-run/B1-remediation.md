# B1 round-1 remediation

**Agent:** `b1-remediator`. Branch `b0-pg-extraction`. Tree left dirty, **not
committed**. Closes every finding in
`dev-diary/adversarial-review/adve-review-mooshik-B-B1-round1.md`. Does not
reopen B0, does not split `init_schema` or `connect_options`, does not start
Postgres, does not run live Cockroach, does not touch `.env` or `models/`.
Keeps the CYCLE.md merge-once line: B stays on this branch until B0-B4 are
all closed; one merge to `lambo-for-mooshik` at the end of B.

Authority: spec of record, then B-postgres-store.md, then source, then the
review file.

---

## Per-finding

### B1-R1-1 (P2): closed

`two_spellings_of_one_database_derive_one_endpoint`
(`src/mcp/endpoint.rs:1495`) now uses a DSN without `/db`.

* omitted-database pair: `postgres://u@host` vs `postgres://u@host/u`
  (`endpoint.rs:1550-1567`). One endpoint. Distinct from the `/db` headline
  pair.
* libpq spelling vs URL form: `host=host user=u dbname=db` vs
  `postgres://u@host/db` (`endpoint.rs:1568-1580`).
* `canonical_store_dsn` pins: omitted database emits
  `postgres://u@host:5432/u`; libpq emits `postgres://u@host:5432/db`
  (`endpoint.rs:1621-1628`).

The production rule is unchanged: `DsnParts::to_identity` (`endpoint.rs:731`)
defaults an empty database to the explicit username.

Mutation evidence below. Reverted after each cycle. Headline port and
password pins stayed red.

### B1-R1-2 (P3): closed

New test `store_is_shareable_is_ruled_not_defaulted`
(`src/mcp/endpoint.rs:1636`). An exhaustive helper names every `StoreKind`
variant (compile error if a kind is added and not listed). The function body
must contain each `StoreKind::… =>` arm and must not contain `_ =>`.

`a_process_private_store_advertises_no_endpoint` still pins the Postgres
*value*. The new test pins the "ruled, not defaulted" shape. `B1-implementation.md`
§1.3 now names this test.

### B1-R1-3 (P3): closed

`cli::provision::postgres_arm_tests::provision_postgres_fails_closed_naming_b2`
(`src/cli/provision.rs:215`). Calls `run(..., StoreKind::Postgres)` with a
dummy `GraphStore` that panics if touched. Expects B2, names postgres, and
refuses `provision.sh`. No real store. Compiles on every feature set
(including `--no-default-features --features store-postgres`).

Did not merge the Postgres arm into the Cockroach arm during mutation:
`.env` exists in the workdir, and `scripts/provision.sh` would source it.
Mutated to `Ok(...)` and, separately, into the Sqlite arm instead.

### B1-R1-4 (P3): closed

* `src/store/mod.rs:1218` panic string: `expected err: silent fallback forbidden`
* `src/store/pg/mod.rs:65` comment: `B1: PostgreSQL + pgvector dialect`

The Cockroach sibling panic at `mod.rs:1185` is pre-existing and was left
alone.

### B1-R1-5 (P3): closed

Both copies now say use `sqlite`, `cockroach`, or `postgres` when a separate
command-line process has to see the same session:

* `docs/reference/config.mdx:50`
* `site/src/content/docs/config.mdx:52`

`scripts/docs/check-mirror-drift.sh` does not gate this pair (cli/mcp only).
The two sentences were edited in lockstep.

---

## Mutations (reverted)

| ID | Mutation | Cited test | Result |
| --- | --- | --- | --- |
| R1-1 M8 | `to_identity` uses raw `database` even when empty | `two_spellings_of_one_database_derive_one_endpoint` | **RED** at `endpoint.rs:1552` (restored pin `endpoint.rs:1556`): omitted `/db` vs `/u` mint two sockets |
| R1-1 port | URL omitted port `unwrap_or(0)` | same | **RED** at `endpoint.rs:1504`: omitted vs `:5432` |
| R1-1 password | userinfo kept as `user:password` | same | **RED** at `endpoint.rs:1524` (restored pin `endpoint.rs:1525`): `"password is a credential, not the database"` |
| R1-2 | Sqlite still special-cased; Cockroach+Postgres collapsed to `_ => true` | `store_is_shareable_is_ruled_not_defaulted` | **RED** at `endpoint.rs:1653`: must name `StoreKind::Cockroach =>`. Value pin `a_process_private_store_advertises_no_endpoint` stayed **GREEN** |
| R1-3 | Postgres arm returns `Ok(...)` | `provision_postgres_fails_closed_naming_b2` | **RED** at `provision.rs:215`: `expect_err` |
| R1-3 | `Sqlite \| Postgres` share `init_schema` | same | **RED** at `provision.rs:146`: dummy panics "must not touch the store" |

M8 is the hole the review opened. Port and password were re-proved red on the
same test after the omitted-database assertions landed.

---

## Gates

All rows are this remediator's runs on the restored tree after the five
closures. Live Cockroach tests: not run. Postgres container: not started.
`.env` and `models/`: not touched. `src/store/sqlite.rs` diff: empty (0 bytes).

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | 0 | **pass** |
| `cargo test --features store-cockroach` | 0 | **939 passed / 0 failed / 4 ignored**, **943 listed** (`: test$`). +2 over B1 implementation 941: `store_is_shareable_is_ruled_not_defaulted`, `provision_postgres_fails_closed_naming_b2` |
| `cargo test --no-default-features --features store-cockroach` | 0 | **602 passed / 0 failed / 0 ignored**, **602 listed**. Same +2 over 600 |
| `cargo test --features store-cockroach,fixtures` | 0 | **999 passed / 0 failed / 12 ignored**, **1011 listed**. Same +2 over 1009 |
| `cargo test --no-default-features --features store-postgres` | 0 | **580 passed / 0 failed / 0 ignored**, **580 listed**. Same +2 over 578 |
| H1 lock: `--lib` `h1_sqlite_and_memory_oracle_agree_exactly` under `store-sqlite,store-cockroach,fixtures` | 0 | **passed**; `git diff src/store/sqlite.rs` **empty** |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54** `^warning:` lines (B0/B1 counting method); **none** naming `store/pg` / `postgres.rs` / `PostgresDialect` |

CYCLE.md merge-once wording is intact. B0 was not reopened. `init_schema` /
`connect_options` were not split.

Nothing was committed or pushed.

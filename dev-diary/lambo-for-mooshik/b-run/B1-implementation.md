# B1: StoreKind::Postgres and the alias split, implementation record

**Phase:** B1 (workstream B, `lambo-for-mooshik`).
**Baseline:** B0 closed at `9d9d8d7` (round 2 APPROVE).
**Tree left dirty and uncommitted**, per the run protocol. Do not split
`init_schema` / `connect_options`. sqlite.rs untouched.

---

## 1. What was built

### 1.1 `StoreKind::Postgres` and feature `store-postgres`

New variant, Cargo feature `store-postgres = ["dep:sqlx", "sqlx/postgres"]`.
Same driver as `store-cockroach`; no second driver. The `pg/` family module
now compiles under `any(store-cockroach, store-postgres)`. Cockroach's
dialect file stays behind `store-cockroach`; the new dialect file stays
behind `store-postgres`.

`is_compiled` follows the feature. `is_ready` is **false** until B2 lands a
working dialect (Bedrock's shape: compiled is not ready).

Not added to `ship` or `demo`. Those wait for a working adapter.

### 1.2 Alias split

| Config string | Resolves to |
| --- | --- |
| `"postgres"`, `"pg"` | `Postgres` |
| `"cockroach"`, `"crdb"` | `Cockroach` |

No string maps across. Recorded on the enum's doc comment so git history of
`"postgres" | "pg" => Cockroach` reads as a choice, not a bug. The two tests
that asserted the old mapping (`parses_store_kind`,
`toml_kind_aliases_match_from_str`) were updated **deliberately** and now
also pin the negative: `postgres`/`pg` are not Cockroach, `cockroach`/`crdb`
are not Postgres.

Expected-kind error strings (empty and unknown) now say
`memory | cockroach | postgres | sqlite`, from one `STORE_KIND_EXPECTED`
constant. The same list is in `lambo.example.toml`. Breaking note is in
`CHANGELOG.md` under Unreleased (0.3.0).

### 1.3 Shareable ruling

`store_is_shareable(Postgres) = true`. Exhaustive match, same reasoning as
Cockroach: a networked store another process can open. Ruled, not defaulted.
Value pinned by `a_process_private_store_advertises_no_endpoint`. Exhaustiveness
pinned by `store_is_shareable_is_ruled_not_defaulted` (names every variant;
`_ => true` goes red).

### 1.4 DSN identity normalisation

Beside `store_identity` in `src/mcp/endpoint.rs`. Two spellings of one
database derive one session endpoint.

**The rule:**

* Scheme `postgres` and `postgresql` are the same (emitted as `postgres`).
* Host is lowercased (DNS is case-insensitive).
* Omitted port is **5432**, the libpq/sqlx default the driver will actually
  dial. Cockroach's conventional 26257 is not the implicit port.
* Omitted database defaults to the explicit username (libpq). The OS user is
  never substituted: identity must not depend on who launched the process.
* Password is **stripped** from the identity string. Hashing already keeps it
  out of the filesystem and the lease row; stripping means the pre-hash
  string is not a secret either, and two credentials for one database still
  derive one endpoint.
* Query parameters that do not name the database are dropped (`sslmode`,
  `sslrootcert`, `connect_timeout`, `application_name`, `options`, …).
  `host` / `port` / `dbname` / `user` in the query overlay the authority,
  matching sqlx.
* Username is kept. Two roles on one cluster can be two deployments; the
  motivating example keeps `u`.
* Libpq `key=value` DSNs are parsed for the same fields. Anything else is
  returned with a `password=` token stripped.

Applies to any DSN-bearing kind (Postgres and Cockroach). Kind remains in
the identity, so the same DSN on two kinds is still two stores.

Test: `two_spellings_of_one_database_derive_one_endpoint`.

### 1.5 Fail-closed `PostgresDialect`

`src/store/pg/postgres.rs` names the dialect. **It does not copy Cockroach
SQL.** C1's recorded precedent: a stub that promotes nothing, or a dialect
that speaks Cockroach, is indistinguishable from a finished broken Postgres.

* `init_sql` and `vector_dim` return a hard error naming B2.
* `build_store(Postgres)` fails closed with the same message rather than
  constructing. `PgStore::new` still speaks Cockroach in its DSN errors
  (B0-R1-3 debt, not split in B1); skipping construction avoids laundering
  that into a Postgres miss.
* `lambo provision` on `kind = "postgres"` fails closed naming B2. It does
  not run `scripts/provision.sh` (Cockroach SQL).
* Cast / operator tokens are the B3 PostgreSQL spellings (`::TEXT`,
  `::vector`, `<=>`), not Cockroach's. If they ever reached a statement
  against Cockroach they would fail loud rather than rank wrong.
* `distance_to_score` is `unimplemented!` naming B3. That row cannot return
  `Result`, and a guessed formula would silently mis-rank.

`PostgresStore::new` is the backstop if a caller constructs
`PgStore<PostgresDialect>` directly: `vector_dim` fails naming B2 even with
a valid DSN.

### 1.6 CI

`feature-matrix` row `postgres`:
`cargo test --no-default-features --features store-postgres`. Compile plus
unit. No live database. `postgres-live` is B2/B3.

---

## 2. What was not built (on purpose)

* Templated width, hnsw-from-init, distance conversion (B2/B3).
* Split of `init_schema` / `connect_options` (B0-R1-3 debt).
* Park-and-fail-over (B-wide ruling, FUTURE.md).
* `postgres-live` service container.
* Adding `store-postgres` to `ship` / `demo`.
* No Postgres container was started.

---

## 3. New tests

On every feature set that compiles `store/mod.rs` and `mcp/endpoint.rs`
(including the three Cockroach CYCLE rows):

| Test | Why the Cockroach listed counts move |
| --- | --- |
| `store::tests::postgres_build_behavior` | fail-closed construction, both compiled and uncompiled |
| `mcp::endpoint::tests::two_spellings_of_one_database_derive_one_endpoint` | DSN identity vs spelling; password not in identity; omitted database; libpq spelling |
| `mcp::endpoint::tests::store_is_shareable_is_ruled_not_defaulted` | shareable match names every kind; `_ => true` goes red |
| `cli::provision::postgres_arm_tests::provision_postgres_fails_closed_naming_b2` | `lambo provision` Postgres arm names B2, does not run `provision.sh` |

On `store-postgres` only (`src/store/pg/postgres.rs`):

* `dialect_tokens_are_not_cockroach_sql`
* `distance_to_score_does_not_guess_a_formula` (should_panic B3)
* `init_sql_and_vector_dim_name_b2`
* `postgres_store_new_fails_closed_even_with_a_dsn`
* `build_store_fails_closed_naming_b2`

Existing tests updated in place (no count change): `parses_store_kind`,
`toml_kind_aliases_match_from_str`, `feature_names`,
`a_process_private_store_advertises_no_endpoint`.

---

## 4. Gates

| Gate | Claimed | Measured |
| --- | --- | --- |
| `cargo fmt --all -- --check` | pass | **pass** (rc 0) |
| `cargo clippy --all-targets -- -D warnings` | pass | **pass** (rc 0) |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | pass | **pass** (rc 0) |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | pass (extra) | **pass** (rc 0) |
| `cargo test --features store-cockroach` | 939 listed unless new tests on this set | **941 listed**, 937 passed / 0 failed / 4 ignored. +2: `postgres_build_behavior`, `two_spellings_of_one_database_derive_one_endpoint` |
| `cargo test --no-default-features --features store-cockroach` | 598 listed unless new tests on this set | **600 listed**, 600 passed / 0 failed / 0 ignored. Same +2 |
| `cargo test --features store-cockroach,fixtures` | 1007 listed unless new tests on this set | **1009 listed**, 997 passed / 0 failed / 12 ignored. Same +2 |
| `cargo test --no-default-features --features store-postgres` | compile + unit, no live DB | **578 listed**, 578 passed / 0 failed / 0 ignored |
| H1 lock: `--lib h1_cross_store_parity` | green; `git diff src/store/sqlite.rs` empty | `h1_sqlite_and_memory_oracle_agree_exactly` **passed**; sqlite.rs diff **empty** |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | no new pg/postgres warnings | **54** warnings (B0 baseline 54); **none** naming `store/pg` / `postgres.rs` / `PostgresDialect` |

---

## 5. Files touched

* `src/store/mod.rs`: `StoreKind::Postgres`, alias split, `build_store` arm,
  expected-kind strings, tests
* `src/store/pg/postgres.rs`: **new**, fail-closed dialect
* `src/store/pg/mod.rs`: feature-gate `cockroach` / `postgres`; tighten
  fixtures-SQL cfg so store-postgres-only is dead-code-clean
* `src/store/pg/dialect.rs`: module doc: B1 stub, B2 fills DDL
* `src/store/error.rs`: cfg comment names `store-postgres`
* `src/mcp/endpoint.rs`: shareable ruling, DSN identity, tests
* `src/cli/provision.rs`: Postgres arm fails closed naming B2
* `Cargo.toml`: feature `store-postgres`
* `.github/workflows/ci.yml`: `postgres` compile+unit row
* `lambo.example.toml`, `docs/reference/config.mdx`,
  `site/src/content/docs/config.mdx`,
  `dev-diary/notes/level-b-pluggability.md`: alias split on the product
  surface
* `CHANGELOG.md`: 0.3.0 breaking note
* `dev-diary/lambo-for-mooshik/b-run/B1-implementation.md`: this file

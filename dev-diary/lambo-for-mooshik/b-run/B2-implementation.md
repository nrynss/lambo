# B2: PostgresDialect DDL (templated width, hnsw from init), implementation record

**Phase:** B2 (workstream B, `lambo-for-mooshik`).
**Baseline:** B1 closed at `e14ba49` (round 2 APPROVE).
**Tree left dirty and uncommitted**, per the run protocol. sqlite.rs untouched.
Did not merge to `lambo-for-mooshik`.

---

## 1. Decisions

### 1.1 Template at init (not generate-in-code)

`migrations/postgres/001_init.sql` is the schema contract. The dense-vector
width is the placeholder `__LAMBO_VECTOR_DIM__`, substituted by
`PostgresDialect::init_sql(dim)`. The file on disk is **not valid SQL**;
applying it with psql without substitution fails loudly.

Generate-in-code was rejected: a 300-line schema built with `format!` is
worse to review than a file with one placeholder, and the inverted data
flow is still honest (width goes *into* the SQL). Cockroach keeps its
static file and parse-out authority.

Recorded in `B-postgres-store.md` B2.

### 1.2 dim > 2000: refuse, naming the ceiling and the halfvec hatch

pgvector hnsw on type `vector` supports at most 2000 dimensions. 768 and
1536 pass; 2000 passes; 2001 and Gemini 3072 refuse at `init_sql` and
`vector_dim`, naming the ceiling and the unimplemented `halfvec` hatch
(hnsw on `halfvec`, ceiling 4000). `CREATE INDEX` is never the discovery.

halfvec is **not** implemented in B2: it would change the stored type, the
operator class, and B3's cast/distance rows.

### 1.3 Over-merge split

A function stays in `PgStore` only when its SQL is byte-identical.

* **`init_schema`:** still `raw_sql(ddl)` then N `query()` calls (Cockroach
  shape unchanged). The statements come from
  `Dialect::post_init_statements`. Cockroach: `endpoint STRING` and
  `current_token INT` (byte-identical to B0). Postgres: `endpoint TEXT`
  and `current_token BIGINT` (Cockroach `INT` is INT8; the shared decoder
  reads i64; PostgreSQL `INT` is int4).
* **`connect_options`:** shared `statement_timeout`, then
  `Dialect::apply_connect_options`. Cockroach sets
  `vector_search_beam_size`. Postgres is identity: pgvector
  `hnsw.ef_search` stays at default 40. No knobs. The Cockroach beam
  parser is `cfg(feature = "store-cockroach")` so `store-postgres`-only
  is dead-code-clean.

DSN / preflight strings that named Cockroach moved onto `Dialect::NAME`,
`STORE_TYPE_NAME`, `DSN_ENV`, `DSN_LABEL`. Cockroach error text is
byte-identical. Postgres construction no longer launders
`CockroachStore requires a DSN`.

### 1.4 Ranking left to B3

`STRING_CAST` (`::TEXT`), `VECTOR_CAST` (`::vector`), `DISTANCE_OP`
(`<=>`) were already B1 tokens. `distance_to_score` stays
`unimplemented!` naming B3. No ranking conversion was invented.

### 1.5 Width source is not B4

`Dialect::vector_dim` reads `[store] vector_dim`, else the embedder width
copied in by `build_store_with_vector_dim` when the pin is absent, else
1024 (BGE demo default). `GraphStore::vector_dimensions` still echoes the
construction dim. Live-schema reporting is B4.

---

## 2. What was built

* `PostgresDialect::init_sql(dim)`: template substitution + hnsw from
  init (`USING hnsw (embedding vector_cosine_ops) WHERE embedding IS NOT
  NULL`). `CREATE EXTENSION vector`. ivfflat rejected (comment + test).
  Index parameters: pgvector defaults (`m=16`, `ef_construction=64`);
  `ef_search` left at 40.
* `StoreKind::Postgres` constructs (`is_ready` follows the feature).
  `build_store` copies the resolved embedder width into the pin slot when
  absent.
* `lambo provision` for `kind = "postgres"` runs `init_schema`. It does
  not run `scripts/provision.sh`.
* CI `postgres-live` job: service container
  `pgvector/pgvector:pg17@sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
  Never `:latest`. Guard greps the named live test's `... ok` line.

Integer columns in the Postgres DDL are `BIGINT`: Cockroach `INT` is INT8
on the wire and the shared decoder reads i64.

---

## 3. What was not built (on purpose)

* Ranking conversion / H3 (B3).
* Live-schema `vector_dimensions()` authority (B4).
* halfvec path.
* ANN knobs (`hnsw.ef_search`, `m`, `ef_construction`).
* `store-postgres` in `ship` / `demo`.
* Park-and-fail-over (B-wide, FUTURE.md).
* H1 harness Postgres leg (H3).

---

## 4. Live container

Ran locally against:

```
pgvector/pgvector:pg17@sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f
```

`docker inspect` Image:
`sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.

DSN: `postgres://lambo:lambo@127.0.0.1:5432/lambo?sslmode=disable`.
Container started for the live test, then removed. Port 5432 was free.

`init_schema_at_two_widths_creates_hnsw` **passed** (0.75s): databases at
768 and 1536, `pg_am.amname = hnsw`, `format_type = vector(n)`,
`session_leases.endpoint` is `text`.

---

## 5. New / updated tests

On `store-postgres` (`src/store/pg/postgres.rs`):

* `init_sql_templates_width_and_creates_hnsw` (768 / 1024 / 1536 / 2000)
* `template_contains_the_placeholder_exactly_once`
* `dim_above_hnsw_ceiling_is_refused_naming_halfvec` (2001, 3072, 0)
* `vector_dim_reads_config_and_defaults_to_1024`
* `postgres_store_new_constructs_with_a_dsn`
* `postgres_store_new_names_postgres_on_a_missing_dsn`
* `build_store_constructs_a_working_adapter`
* `connect_options_do_not_set_cockroach_beam_size`
* `init_schema_at_two_widths_creates_hnsw` (`#[ignore]`, live)

B1 fail-closed tests in this file were replaced (construction now works).
`distance_to_score_does_not_guess_a_formula` and
`dialect_tokens_are_not_cockroach_sql` stay.

On every feature set that compiles `cli/provision.rs`:
`provision_postgres_calls_init_schema_not_provision_sh` replaces
`provision_postgres_fails_closed_naming_b2` (same count).

Cockroach pin added **inside**
`served_migration_converges_event_time_and_human_confirmed` (no new test):
`post_init_statements` still emit `endpoint STRING`.

`store::tests::postgres_build_behavior` now constructs (mirrors
Cockroach). Same test, still on the three CYCLE rows.

---

## 6. Gates

| Gate | Claimed | Measured |
| --- | --- | --- |
| `cargo fmt --all -- --check` | pass | **pass** (rc 0) |
| `cargo clippy --all-targets -- -D warnings` | pass | **pass** (rc 0) |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | pass | **pass** (rc 0) |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | pass (extra) | **pass** (rc 0) |
| `cargo test --features store-cockroach` | 941 listed unless new tests on this set | **pass**. Summed `test result` lines: 939 passed / 4 ignored / **943 listed** (includes 2 doctests). Lib: 927 passed / 2 ignored. No new Cockroach-row tests; comparable to B1's 941 listed without the 2 doctests. |
| `cargo test --no-default-features --features store-cockroach` | 600 listed unless new tests | **pass**. 602 listed including 2 doctests (B1 600). |
| `cargo test --features store-cockroach,fixtures` | 1009 listed unless new tests | **pass**. 999 passed / 12 ignored / 1011 listed including 2 doctests (B1 1009). |
| `cargo test --no-default-features --features store-postgres` | compile + unit; live ignored | **pass**. 585 passed / 1 ignored / **586 listed** including 2 doctests. Lib: 576 passed / 1 ignored. +6 vs B1's 580: 5 postgres.rs tests became 11 (10 unit + 1 ignored). |
| Live `init_schema_at_two_widths_creates_hnsw` | two widths, hnsw from init | **pass** against digest `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f` |
| H1 lock: `h1_sqlite_and_memory_oracle_agree_exactly` | green; `git diff src/store/sqlite.rs` empty | **passed**; sqlite.rs diff **empty** |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | no new pg/postgres warnings | **53** warnings (B0/B1 baseline 54); **none** naming `store/pg` / `postgres.rs` / `PostgresDialect` |

---

## 7. Files touched

* `migrations/postgres/001_init.sql`: **new**, templated pgvector schema
* `src/store/pg/postgres.rs`: working dialect, unit + live tests
* `src/store/pg/dialect.rs`: B2-discovered rows (`NAME`, DSN labels,
  `post_init_statements`, `apply_connect_options`)
* `src/store/pg/mod.rs`: split `init_schema` / `connect_options`; DSN and
  preflight use dialect names; beam parser cfg-gated
* `src/store/pg/cockroach.rs`: implements the new rows; Cockroach SQL
  byte-identical
* `src/store/mod.rs`: `is_ready`, `build_store` constructs Postgres,
  `postgres_build_behavior`
* `src/cli/provision.rs`: Postgres arm calls `init_schema`
* `.github/workflows/ci.yml`: `postgres-live` service container, digest-pinned
* `Cargo.toml`, `CHANGELOG.md`,
  `dev-diary/notes/level-b-pluggability.md`,
  `dev-diary/lambo-for-mooshik/B-postgres-store.md` (decisions recorded)
* `dev-diary/lambo-for-mooshik/b-run/B2-implementation.md`: this file

# Adversarial review: mooshik B, phase B2 (PostgresDialect DDL), round 1

**Reviewer**: independent adversarial reviewer, agent_id `B2Review1`. Wrote
nothing under review except this file. No commit, no push.
**Scope**: the uncommitted B2 implementation on branch `b0-pg-extraction`
against `B-postgres-store.md` B2 (and the B3 table, to catch ranking theft),
plus the implementer's claims in `b-run/B2-implementation.md`.
**Worktree**: `/home/nryn/work/lambo`, branch `b0-pg-extraction` @ `e14ba49`
(B1 closed, round 2 APPROVE). Dirty tree at review start is the B2
implementation. Ignored: `local:/` and the orchestrator briefs.
**Verdict**: **REQUEST_CHANGES**. 0 P1 / 1 P2 / 0 P3.

Reviewed-state SHA-256, taken before any mutation and re-verified after every
mutation cycle and again at the end of the review:

| File | SHA-256 |
| --- | --- |
| `src/store/mod.rs` | `efa9490b5bfbf886010110708e2debefee858c3e5c4da49621550f5ae36ff169` |
| `src/store/pg/postgres.rs` | `dd4955a373347b19f6c27226a2795f01e141e64f2b856a6412c60af6430e90ee` |
| `src/store/pg/mod.rs` | `17c45b2ca980248983fe8a5182572f16ce1b461432ff5e18415755e50385bb5a` |
| `src/store/pg/dialect.rs` | `b3c0e8e1338e4dea431f2e3b9a8ca903b5b62d8fcb594168f6b2024fbc553b1f` |
| `src/store/pg/cockroach.rs` | `35e7e5cec9fba1872d4d6924e471fe83417b5513ead7c2bb00154087ffd3ebe6` |
| `src/cli/provision.rs` | `c25c29af597112e82bfb65da1fd4aad2f2759d79825eeefca5dd2dded0f99f6d` |
| `migrations/postgres/001_init.sql` | `c7ecd3cf18b55a9d4ee4015b59498aacc4ea86ed47436dad39a16566153aff98` |
| `.github/workflows/ci.yml` | `6000666458f85b22c6d341e186900744f10b06ece8480c7d5ec0ed3eb4e15504` |

Copied those files (plus `Cargo.toml`, `CHANGELOG.md`,
`B-postgres-store.md`, `level-b-pluggability.md`) to
`/tmp/b2-r1-review-snapshot/` before any mutation. Every mutation cycle ends
with a restore and a SHA-256 comparison against the table above.

## Method

1. Recalled dogfood memory (`B2Review1`). Read `B2-round1-review-brief.md`,
   `B2-implementation.md`, `B-postgres-store.md` B2 and B3, `CYCLE.md`, and
   house style `adve-review-mooshik-B-B1-round2.md`.
2. Mutation-tested every claimed pin. A pin holds only if the cited test
   FAILS under the mutation.
3. Independently re-ran every CYCLE gate on the restored tree, plus the
   `store-postgres` compile/unit row and the extra `store-postgres` clippy.
   Live Postgres: started, used, and removed only the pinned
   `pgvector/pgvector:pg17` digest
   `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
   Live Cockroach tests: not run. `.env` and `models/`: not touched.
4. Hunted for: leftover Cockroach `STRING` in Postgres init, placeholder not
   substituted, dim 2001 creating an index, hnsw missing, ivfflat, `:latest`
   in CI, silent distance formula, `init_schema` still byte-identical SQL
   that Postgres cannot run, Cockroach SQL in postgres init, over-merge
   remaining, sqlite.rs edits, B0 byte-identity pin broken, halfvec silently
   used.

Unmutated pins were green before the first mutation (`--no-default-features
--features store-postgres` for the new dialect tests;
`--features store-cockroach` for the B0 and STRING pins). After restore,
incremental compile was forced with `touch` of the restored files.

## Part A: claimed pins, mutation-tested

| # | Claim | Cited test | Mutation | Result |
| --- | --- | --- | --- | --- |
| M1 | Placeholder substituted | `init_sql_templates_width_and_creates_hnsw` | `init_sql` returns the template without `replace` | **RED** at `postgres.rs:190`: placeholder still present |
| M2 | Width is `dim`, not 1024 | same | `replace(..., "1024")` | **RED** at `postgres.rs:195`: `vector(768)` missing |
| M14 | File on disk is not valid SQL (placeholder required) | same | `vector(1024)` in the file, no placeholder | **RED** at `postgres.rs:190`: `init_sql` `expect` (hits != 1) |
| M3 | dim 2001/3072 refused at `init_sql` before CREATE INDEX | `dim_above_hnsw_ceiling_is_refused_naming_halfvec` | drop `refuse_over_hnsw_ceiling` from `init_sql` | **RED** at `postgres.rs:233`: `unwrap_err` on `Ok` SQL that already contains `vector(2001)` and `USING hnsw` |
| M3b | same refuse at `vector_dim` | same | drop refuse from `vector_dim` only | **RED** at `postgres.rs:243`: `vector_dim` `unwrap_err` |
| M11 | 2000 still passes | `init_sql_templates_width_and_creates_hnsw` | ceiling `>= 2000` | **RED** at `postgres.rs:190`: `init_sql(2000)` errors |
| M4 | hnsw from init, not ivfflat | same | `USING ivfflat` | **RED** at `postgres.rs:203`: `USING hnsw` missing |
| M13 | halfvec not silently used | same | `embedding halfvec(__LAMBO_VECTOR_DIM__)` | **RED** at `postgres.rs:195`: `vector({dim})` missing |
| M5 | Postgres post_init is TEXT, not STRING | `dialect_tokens_are_not_cockroach_sql` | `endpoint STRING` | **RED** at `postgres.rs:166` |
| M6 | Postgres connect options do not set Cockroach beam_size | `connect_options_do_not_set_cockroach_beam_size` | `apply_connect_options` sets `vector_search_beam_size=64` | **RED** at `postgres.rs:321` (Debug of `PgConnectOptions` does show the options string; the pin is not vacuous) |
| M7 | `distance_to_score` does not guess `1-d` | `distance_to_score_does_not_guess_a_formula` (`should_panic` B3) | `return 1.0 - dist` | **RED**: test did not panic |
| M8 | does not guess `1-d^2/2` | same | Cockroach formula `1 - 0.5 * d * d` | **RED**: test did not panic |
| M9 | Cockroach post_init still emits `endpoint STRING` | `served_migration_converges_event_time_and_human_confirmed` | production `endpoint TEXT` | **RED** at `cockroach.rs:387`: left TEXT, right STRING |
| M10 | B0 composed-SQL pin still holds | `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` | Cockroach `DISTANCE_OP` `<=>` | **RED** at `cockroach.rs:780`: `vector_candidates` left `<=>`, right `<->` |
| M12 | provision Postgres arm calls `init_schema`, not `provision.sh` | `provision_postgres_calls_init_schema_not_provision_sh` | arm returns `Ok(...)` without touching the store | **RED** at `provision.rs:226`: `init_schema must run once` (left 0, right 1) |

768 / 1024 / 1536 / 2000 still pass the cited tests on the restored tree.

CI `:latest`: no cargo pin (trace). `.github/workflows/ci.yml:278` is
`pgvector/pgvector:pg17@sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
Never `:latest` on that image. Swapping the image to `:latest` would not fail
any unit test; the claim is verified by reading the workflow.

Mutation score on cited pins: **15/15 attempted closure mutations were
caught.** None of those fifteen pins passed regardless of the fix.

## Part B: hunt for defects the change introduced

### Finding B2-R1-1 (P2): embedder-width copy is unpinned, and the rustdoc denies it

B2's recorded width source (`B-postgres-store.md` B2, `B2-implementation.md`
§1.5, `postgres.rs:88-94`) is: `[store] vector_dim`, else the embedder width
copied in by `build_store_with_vector_dim`, else 1024. The production path
is `resolve_backends` (`resolve.rs:114-116`) calling
`build_store_with_vector_dim(store_cfg, Some(embedder_cfg.dim))`. The copy
itself is `mod.rs:954-961`.

Every listed B2 unit test either sets the pin (`Some(768)` / `Some(1536)`)
or uses `build_store` / `PostgresStore::new` with `vector_dim: None` (the
1024 default). None call `build_store_with_vector_dim` with pin absent and
param `Some(768)`.

**M-copy**: delete the `if cfg.vector_dim.is_none() { cfg.vector_dim = vector_dim; }`
block. Result: **GREEN** on all of `store::pg::postgres::tests::*` (10 passed,
1 ignored live) and on `postgres_build_behavior`. A process with
`[embedder] dim = 768` and no `[store] vector_dim` would then init at 1024.
The live two-width test would not catch it: it sets the pin.

The same function's rustdoc still says the opposite:

```
Only adapters whose vector column carries **no** width of its own consume it:
today that is SQLite, whose `concepts.embedding` is a `BLOB` (Cockroach parses
`VECTOR(n)` out of its own DDL and ignores this).
```

(`mod.rs:887-889`; same claim in the body comment at `mod.rs:922-926`:
"Consumed only by the width-agnostic adapters below".) Postgres now both
has a width of its own and consumes the argument. A cleanup that trusts
that rustdoc deletes the only copy, and no listed test goes red.

Closure: a test that `build_store_with_vector_dim` with `vector_dim: None`
and param `Some(768)` reports `vector_dimensions() == Some(768)` (and that
pin `Some(1536)` still wins over param `Some(768)`). Update the rustdoc
and the body comment so they name Postgres as a consumer. The copy-delete
mutation must go red on that test.

### Attack vectors examined, not elevated

- **Leftover Cockroach STRING in Postgres init.** Executable (non-comment)
  lines of `migrations/postgres/001_init.sql` contain no `STRING`, no
  `VECTOR INDEX`, no bare `INT` type, no `halfvec`, no `ivfflat`. The
  placeholder appears once. Tables match Cockroach (11 names). M5 / M13
  red.
- **Placeholder not substituted / 1024 hardcoded.** M1, M2, M14 red.
- **dim 2001 creating an index.** M3 returns SQL that already contains
  `CREATE INDEX` / `USING hnsw` / `vector(2001)` if the refuse is dropped;
  the cited test fails at `unwrap_err` so `init_schema` never receives that
  SQL. M3b pins the `vector_dim` path independently. 2000 still passes
  (M11).
- **hnsw missing / ivfflat.** M4 red.
- **`:latest` in CI.** Image line is digest-pinned. Runner `ubuntu-latest`
  is the pre-existing GHA runner label, not the pgvector image.
- **Silent distance formula.** M7 and M8 red. The `unimplemented!` message
  names B3's planned `1 - d` as documentation of the B3 table, not as a
  returned score. Ranking conversion is still unimplemented.
- **`init_schema` still byte-identical SQL Postgres cannot run.** Split
  holds: `init_schema` is still `raw_sql(ddl)` then N `query()` calls
  (`pg/mod.rs:2155-2172`); statements come from
  `Dialect::post_init_statements` (Cockroach STRING+INT, Postgres
  TEXT+BIGINT). `connect_options` applies shared `statement_timeout` then
  `Dialect::apply_connect_options` (Cockroach beam_size; Postgres identity).
  Beam parser is `cfg(feature = "store-cockroach")`. M5, M6, M9 red.
- **B0 byte-identity pin broken.** M10 red on `DISTANCE_OP`. Cockroach DSN
  error text is now `format!` of `STORE_TYPE_NAME` / `DSN_ENV` / `DSN_LABEL`
  whose values equal the old literals. Preflight uses `D::NAME ==
  "cockroach"`. Not reopened.
- **sqlite.rs / H1 / alias split.** `git diff -- src/store/sqlite.rs` is 0
  bytes. `h1_sqlite_and_memory_oracle_agree_exactly` passed.
  `src/mcp/endpoint.rs` not in the dirty tree.
- **`store-postgres` in `ship` / `demo`.** Still absent (`Cargo.toml:133`
  and `:136`).
- **Em dashes in B2-authored added lines.** `git diff -U0` added lines: zero
  U+2014. `001_init.sql` and `B2-implementation.md`: zero.
- **post_init INT instead of BIGINT (M15).** `dialect_tokens_are_not_cockroach_sql`
  stayed **GREEN**. Not elevated: `CREATE TABLE session_leases` already
  declares `current_token BIGINT`, so `ADD COLUMN IF NOT EXISTS` would not
  change the type on a fresh database, and the live test checks `endpoint`
  is `text`. The DDL file uses BIGINT on integer columns.
- **`hnsw.ef_search=100` knob (M16).** `connect_options_do_not_set_cockroach_beam_size`
  stayed **GREEN**. Not elevated: the implementation does not set it (trait
  default is identity). The cited pin is the Cockroach GUC, which it
  catches (M6).

## Part C: gates rerun (my own runs, restored tree)

All rows are my runs on the hashes in the header. Nothing is copied from
`B2-implementation.md`. `.env` and `models/`: not touched. Live Cockroach:
not run. Live Postgres: pinned digest only, container removed afterwards.

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | 0 | **pass** |
| `cargo test --features store-cockroach` | 0 | **939 passed / 0 failed / 4 ignored**, **943 listed**. Lib: 927 passed / 2 ignored. Matches B1 round 2 and the implementer (no new tests on this row: the provision test replaced the B1 fail-closed one) |
| `cargo test --no-default-features --features store-cockroach` | 0 | **602 passed / 0 failed / 0 ignored**, **602 listed** |
| `cargo test --features store-cockroach,fixtures` | 0 | **999 passed / 0 failed / 12 ignored**, **1011 listed** |
| `cargo test --no-default-features --features store-postgres` | 0 | **585 passed / 0 failed / 1 ignored**, **586 listed**. Lib: 576 passed / 1 ignored. The ignored test is `init_schema_at_two_widths_creates_hnsw`. +6 vs B1's 580 |
| Live `init_schema_at_two_widths_creates_hnsw` | 0 | **passed** (0.69s) against digest `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`. `docker inspect` Image matched. DSN `postgres://lambo:lambo@127.0.0.1:5432/lambo?sslmode=disable`. Container started for the test and removed. Port 5432 was free before and after |
| H1 lock: `--lib` `h1_sqlite_and_memory_oracle_agree_exactly` under `store-sqlite,store-cockroach,fixtures` | 0 | **passed**; `git diff src/store/sqlite.rs` **empty** (0 bytes) |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54** `^warning:` lines including cargo's summary line (B0/B1 counting method); cargo reports **53** rustdoc warnings. **none** naming `store/pg` / `postgres.rs` / `PostgresDialect` |

No gate finding. Implementer's measured counts match mine.

CYCLE merge-once wording is intact: B stays on `b0-pg-extraction` until B0
through B4 are all closed. Nothing was committed or pushed.

## Findings

| ID | Grade | Status |
| --- | --- | --- |
| B2-R1-1 | P2 | **OPEN**. Embedder-width copy in `build_store_with_vector_dim` is unpinned (M-copy green on every listed B2 unit test). Function rustdoc still says only SQLite consumes the argument. Silent init at 1024 against a 768 embedder is the failure mode. |

Template-at-init, hnsw from init (not ivfflat), dim > 2000 refuse naming
halfvec, 768/1536/2000 generation, over-merge split, B3 ranking left
unimplemented, B0 composed-SQL pin, H1 lock, digest-pinned `postgres-live`,
and provision calling `init_schema` all hold under mutation.

**0 P1, 1 P2, 0 P3 residue.**

**Tree state at close**: restored to the reviewed state and verified. All
SHA-256 values in the header match, and `git status --short` is identical
to session start except for this file. Live container removed. Nothing was
committed or pushed.

B2Review1, 2026-08-23

---

## Closures (round-1 remediation, 2026-08-23)

Remediator: `b2-remediator`. Work on `b0-pg-extraction`, tree left dirty, not
committed. This section records what closed; it does **not** change the
**REQUEST_CHANGES** verdict or the Findings table above.

| ID | Grade | Status | What |
| --- | --- | --- | --- |
| B2-R1-1 | P2 | **closed** | `postgres_copies_embedder_width_when_pin_is_absent` (`mod.rs:1228`) calls `build_store_with_vector_dim` with pin `None` and param `Some(768)` and reports `Some(768)` (`mod.rs:1256`). Pin `Some(1536)` still wins over param `Some(768)` (`mod.rs:1271`). M-copy (delete the Postgres-arm copy at `mod.rs:961-963`) goes **RED** at `mod.rs:1257` (left 1024, right 768). M-pin (`cfg.vector_dim = Some(768)` always) goes **RED** at `mod.rs:1271`. Rustdoc at `mod.rs:886-889` and the body comment at `mod.rs:923-926` now name Postgres as a consumer. Reverted. |

Gates re-run by the remediator: fmt, three clippy `-D warnings` rows (default,
store-cockroach, store-postgres), the four test commands (944 / 603 / 1012 /
587 listed), H1 lock green with `sqlite.rs` diff empty, and
`cargo doc --document-private-items` at 54 warnings. Detail in
`dev-diary/lambo-for-mooshik/b-run/B2-remediation.md`.

# B4 round-1 remediation

**Agent:** `b4-remediator`. Branch `b0-pg-extraction`. Tree left dirty, **not
committed**. Closes the single finding in
`dev-diary/adversarial-review/adve-review-mooshik-B-B4-round1.md`. Does not
reopen the live-schema probe, does not edit `src/store/pg/`, does not start
Postgres, does not run live Cockroach, does not touch `.env`, `models/`, or
`src/store/sqlite.rs`.

Keeps the CYCLE.md merge-once line: B stays on this branch until B0-B4 are
all closed; one merge to `lambo-for-mooshik` at the end of B.

Authority: spec of record, then B-postgres-store.md, then source, then the
review file.

---

## Per-finding

### B4-R1-1 (P2): closed

`.github/workflows/ci.yml` `postgres-live` now passes both live test names as
libtest harness filters after `--`, not as a second cargo `TESTNAME`.

First step (init + mismatch), `ci.yml:305-311`:

```
cargo test --no-default-features --features store-postgres --lib \
  -- --ignored --nocapture --exact \
  store::pg::postgres::tests::init_schema_at_two_widths_creates_hnsw \
  store::pg::postgres::tests::live_schema_width_refuses_a_config_that_disagrees
```

Greps at `ci.yml:310-311` unchanged.

B3 step in the same job (pre-existing two-name shape), `ci.yml:315-321`:

```
cargo test --no-default-features --features store-postgres --lib \
  -- --ignored --nocapture --exact \
  store::pg::postgres::tests::explain_recall_uses_hnsw \
  store::pg::postgres::tests::fencing_refuses_stale_write_and_upserts_replay
```

Greps at `ci.yml:320-321` unchanged. `--ignored --nocapture --exact` kept on
both steps. H3 step (`ci.yml:325-327`) already had one `TESTNAME` and was
left alone.

Cargo 1.97.1 (`c980f4866 2026-06-30`; `rust-toolchain.toml` channel
`1.97.1`) takes one `[TESTNAME]`. The test binary takes `[FILTERS...]`.
Proof below. Live-check code was not opened.

---

## Cargo 1.97.1 proof

Pinned toolchain:

```
cargo 1.97.1 (c980f4866 2026-06-30)
Usage: cargo test [OPTIONS] [TESTNAME] [-- [ARGS]...]
```

Harness (`cargo test -- --help`):

```
Usage: .../lambo-<hash> [OPTIONS] [FILTERS...]
```

Old first-step shape (two names before `--`): **rc 1**

```
error: unexpected argument
'store::pg::postgres::tests::live_schema_width_refuses_a_config_that_disagrees'
found
Usage: cargo test [OPTIONS] [TESTNAME] [-- [ARGS]...]
```

Old B3 step: **rc 1**, same error on
`fencing_refuses_stale_write_and_upserts_replay`.

New first-step shape plus `--list`: **rc 0**, lists both ignored tests:

```
store::pg::postgres::tests::init_schema_at_two_widths_creates_hnsw: test
store::pg::postgres::tests::live_schema_width_refuses_a_config_that_disagrees: test
2 tests, 0 benchmarks
```

New B3 shape plus `--list`: **rc 0**, lists `explain_recall_uses_hnsw` and
`fencing_refuses_stale_write_and_upserts_replay`.

`--list` is the proof cargo accepts the invocation and the harness sees
both filters. Live Postgres was not started.

---

## Gates

All rows are this remediator's runs after the CI closure. Live Cockroach
tests: not run. Postgres container: not started. `.env` and `models/`: not
touched. `src/store/sqlite.rs` remediator diff: empty (0 bytes). Live-schema
probe: not edited.

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | 0 | **pass** |
| `cargo test --features store-cockroach` | 0 | **940 passed / 0 failed / 4 ignored**, **944 listed**. Lib: 928 passed / 2 ignored |
| `cargo test --no-default-features --features store-cockroach` | 0 | **603 passed / 0 failed / 0 ignored**, **603 listed**. Lib: 594 passed |
| `cargo test --features store-cockroach,fixtures` | 0 | **1000 passed / 0 failed / 12 ignored**, **1012 listed**. Lib: 987 passed / 10 ignored |
| `cargo test --no-default-features --features store-postgres` | 0 | **590 passed / 0 failed / 4 ignored**, **594 listed**. Lib: 581 passed / 4 ignored |
| B0 composed-SQL pin `--lib` `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` | 0 | **passed** |
| H1 lock: `--lib` `h1_sqlite_and_memory_oracle_agree_exactly` under `store-sqlite,store-cockroach,fixtures` | 0 | **passed** (0.09s). This remediator did not edit `sqlite.rs` |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54** `^warning:` lines including cargo's summary line; cargo reports **53** rustdoc warnings. **none** naming `store/pg` / `postgres.rs` / `PostgresDialect` / `assert_live_schema_width` / `parse_pgvector_format_type` |

CYCLE.md merge-once wording is intact. B0/B1/B2/B3 were not reopened. The
live-schema probe was not reopened.

Nothing was committed or pushed.

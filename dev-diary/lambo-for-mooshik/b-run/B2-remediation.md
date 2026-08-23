# B2 round-1 remediation

**Agent:** `b2-remediator`. Branch `b0-pg-extraction`. Tree left dirty, **not
committed**. Closes the single finding in
`dev-diary/adversarial-review/adve-review-mooshik-B-B2-round1.md`. Does not
reopen B0/B1, does not implement B3 ranking, does not start Postgres, does
not run live Cockroach, does not touch `.env`, `models/`, or
`src/store/sqlite.rs`.

Keeps the CYCLE.md merge-once line: B stays on this branch until B0-B4 are
all closed; one merge to `lambo-for-mooshik` at the end of B.

Authority: spec of record, then B-postgres-store.md, then source, then the
review file.

---

## Per-finding

### B2-R1-1 (P2): closed

`store::tests::postgres_copies_embedder_width_when_pin_is_absent`
(`src/store/mod.rs:1228`) now calls `build_store_with_vector_dim` the way
production does.

* pin absent, param `Some(768)`: `vector_dimensions() == Some(768)`
  (`mod.rs:1256`). Not the 1024 default.
* pin `Some(1536)`, param `Some(768)`: `vector_dimensions() == Some(1536)`
  (`mod.rs:1271`). Pin still outranks param.

The production copy is unchanged: `mod.rs:961-963` writes the resolved
embedder width into the pin slot only when the operator did not set one.

Rustdoc no longer says only SQLite consumes the argument:

* function doc `mod.rs:886-889` names SQLite **and** Postgres as consumers
  (Postgres templates `vector(n)` at init; the copy fills the pin when
  absent). Cockroach still parses `VECTOR(n)` out of its own DDL.
* body comment `mod.rs:923-926` says the same.

Uncompiled `StoreKind::Postgres` still fail-closes on this call, same
shape as `postgres_build_behavior`.

Mutation evidence below. Reverted after each cycle.

---

## Mutations (reverted)

| ID | Mutation | Cited test | Result |
| --- | --- | --- | --- |
| M-copy | delete `let mut cfg = cfg;` and `if cfg.vector_dim.is_none() { cfg.vector_dim = vector_dim; }` | `postgres_copies_embedder_width_when_pin_is_absent` | **RED** at restored `mod.rs:1257`: left `Some(1024)`, right `Some(768)` (`absent pin must take the embedder width, not default 1024`) |
| M-pin | `cfg.vector_dim = Some(768)` always (param overwrites pin) | same | **RED** at restored `mod.rs:1271`: left `Some(768)`, right `Some(1536)` (`pin still outranks the embedder-width param`) |

M-copy is the hole the review opened. M-pin proves the second assertion is
not vacuous. Both restored.

---

## Gates

All rows are this remediator's runs on the restored tree after the closure.
Live Cockroach tests: not run. Postgres container: not started. `.env` and
`models/`: not touched. `src/store/sqlite.rs` diff: empty (0 bytes). B3
`distance_to_score` was not implemented.

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | 0 | **pass** |
| `cargo test --features store-cockroach` | 0 | **940 passed / 0 failed / 4 ignored**, **944 listed**. Lib: 928 passed / 2 ignored. +1 over B2 review 943: `postgres_copies_embedder_width_when_pin_is_absent` |
| `cargo test --no-default-features --features store-cockroach` | 0 | **603 passed / 0 failed / 0 ignored**, **603 listed**. +1 over 602 |
| `cargo test --features store-cockroach,fixtures` | 0 | **1000 passed / 0 failed / 12 ignored**, **1012 listed**. +1 over 1011 |
| `cargo test --no-default-features --features store-postgres` | 0 | **586 passed / 0 failed / 1 ignored**, **587 listed**. Lib: 577 passed / 1 ignored. +1 over 586. Ignored remains `init_schema_at_two_widths_creates_hnsw` |
| H1 lock: `--lib` `h1_sqlite_and_memory_oracle_agree_exactly` under `store-sqlite,store-cockroach,fixtures` | 0 | **passed**; `git diff src/store/sqlite.rs` **empty** (0 bytes) |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54** `^warning:` lines including cargo's summary line (B0/B1 counting method); cargo reports **53** rustdoc warnings. **none** naming `store/pg` / `postgres.rs` / `PostgresDialect` |

CYCLE.md merge-once wording is intact. B0/B1 were not reopened. B3 ranking
was not started.

Nothing was committed or pushed.

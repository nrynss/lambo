# #24: test extraction by subject (decisions)

Refactor 1/5. Base: v0.3.0 (`d93c28f`). Landed via PR #41 (merge `9e6863a`).
Decisions and why; the commits carry the mechanics.

## Subject files, not one `tests.rs` per module

Each large inline `#[cfg(test)] mod tests` moved to `<module>/tests/mod.rs`
(shared fixtures and imports, `use super::*`) plus `<module>/tests/<subject>.rs`.
Subjects follow the production splits that #26 and #27 were going to make
(SQLite: `schema`, `connection`, `persistence`, `access`, `vectors`,
`structural`, `leases`; Memory: `attach`, `leases`, `replay`, `shutdown`,
`writes`, `reads`, `access`; and so on), so those phases could split production
code without moving test files again. They only rewrite imports in each
`tests/mod.rs`, which is what #26 did.

Pre-existing inner modules kept their names and got their own files:
`vector_e2e`, `h1_cross_store_parity`, `conformance`, `h2_cockroach_parity`, and
serve's `refuses_to_start`, `carries_the_resolved_config`,
`proxy_releases_the_model`, `close_runs`.

## 548 test paths gained one segment; we accepted that

A subject file is a child module, so each test in a flat suite gained one path
segment (`memory::tests::the_keep_warm_…` became
`memory::tests::shutdown::the_keep_warm_…`). Leaf names, pass/ignore outcomes
and counts are unchanged in every CI row, and every name CI greps is unchanged.
The only layout that keeps every old name is `include!()`, and rustfmt does not
format included files, so the fmt gate would have stopped checking about 38k
lines of tests. Formatting coverage won over name stability.

The CI vector row selects by prefix (`store::sqlite::tests::vector`), so every
vector test must stay under `tests::vectors` or `tests::vector_e2e`, and no
other subject may start with `vector`.

## `store/pg/postgres.rs` tests: one file, names kept

`postgres-live` runs seven Postgres tests by exact name. The Postgres suite
moved whole to `postgres/tests.rs` (no subject split), which keeps every
`store::pg::postgres::tests::<name>` and needed no CI change. Its `cfg(test)`
helpers (`postgres_dsn_or_skip`, `dsn_for_database`, `corpus`) stay shared with
the SQLite H1/H3 parity tests.

## Include paths anchored

Every moved `include_str!`/`include_bytes!` now uses
`concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/...")`, so later moves cannot
silently re-target a fixture. Fixture bytes are unchanged.

Two tests read production source by path and must be repointed whenever their
target code moves: the `serve_web` route scans (#28 owns them), and the
Postgres camera-proof scan (#26 repointed it).

## Defects found and fixed along the way

Each fix is in its own `fix(...)` commit:

- `scripts/loadtest/check_durability.py` could never fail. Since J3, a write ack
  says "accepted N concept(s) for background write" plus a receipt. Its pre-J3
  regexes matched nothing, so it expected zero writes and called any store
  durable, an empty one included. It now counts acks and resolves outcomes from
  settled receipts. It opens the store read-only and fails with documented exit
  codes (2 shortfall, 3 wording drift, 4 nothing verified, 5 unverifiable,
  6 bad input, 64 usage). A stdlib unittest covers it.
- `scripts/loadtest/capture_sigterm.sh` drove the live dogfood writer. Whatever
  `--port` it was given, the load driver defaulted to 7700, and serve's socket
  landed in the shared runtime dir. It now defaults to 17700 and refuses 7700
  (compared as an integer) unless explicitly allowed. It gives serve a private
  runtime dir, reaps serve and the driver on every exit path, and exits with the
  durability result. The live ledger and store showed it had never actually
  run against the rig.
- `examples/drive_mcp_soak.py` defaulted to 7700. `--endpoint` is now required.
- The Postgres camera-proof source scan could never fail: three of its four
  checks matched their own string literals. It now checks each production call
  site inside its own function and ignores comments.

## Not done here

Merging the per-backend conformance suites into one harness changes test
behaviour, so it was deferred until after #26. `evidence/swarm/control/compute_metrics.py`
keeps its pre-J3 regex, because it scores a finished pre-J3 experiment.

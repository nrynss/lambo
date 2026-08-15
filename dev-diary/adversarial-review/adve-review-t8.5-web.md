# T85-3 — FIXED: writer-published flush stats for `serve-web`

**Finding (adve-review-t8.5-web.md, T85-3):** `serve-web`'s `/api/stats` and the
`web/app.js` tiles hardcoded `flush_lag_ms` / `log_depth` to `null` / `n/a` with a
"writer-only" tooltip because a read-only reader process cannot see the writer's
in-memory `FlushTask::stats()`. The page could not show real durability numbers.

**Remediation (option a — store-mediated, pre-merge):** the writer's `FlushTask`
now publishes its observable flush stats into the shared store after each
completed flush cycle, and the reader fetches them. The reader never writes;
only the lease-holding writer's flush loop publishes. Single-writer/liveness
semantics and the lease are untouched.

## What changed

1. **New `session_stats` table** (one row per session, absent = "no writer yet"):
   - `migrations/sqlite/001_init.sql` — `session_id TEXT PRIMARY KEY,
     flush_lag_ms INTEGER NOT NULL, log_depth INTEGER NOT NULL,
     updated_at TEXT NOT NULL` (SQLite TIMESTAMPTZ-as-TEXT convention, stamped
     via `strftime('%Y-%m-%dT%H:%M:%fZ','now')`).
   - `migrations/cockroach/001_init.sql` — `session_id STRING PRIMARY KEY,
     flush_lag_ms INT NOT NULL, log_depth INT NOT NULL,
     updated_at TIMESTAMPTZ NOT NULL DEFAULT now()`.
   - `MemoryStore` — `RwLock<HashMap<String, SessionFlushStats>>` keyed by
     session id.

2. **`GraphStore` trait** (`src/store/mod.rs`): new `SessionFlushStats
   { flush_lag_ms: u64, log_depth: u64 }` and two trait methods with **no-op
   defaults** (so non-target backends and test doubles keep prior behaviour):
   - `write_flush_stats(&SessionId, &SessionFlushStats)` — called only by the
     writer's `FlushTask`.
   - `read_flush_stats(&SessionId) -> Result<Option<SessionFlushStats>>` —
     called by readers; `None` is the honest `n/a`.
   All three real stores override both: `MemoryStore` (map insert/read),
   `SqliteStore` and `CockroachStore` (single idempotent `INSERT ... ON
   CONFLICT DO UPDATE` upsert / `SELECT`).

3. **`FlushTask` publishes** (`src/store/flush.rs`): after each completed flush
   cycle the loop calls `store.write_flush_stats(&session, …)` with its current
   `FlushStats` (`flush_lag_ms = lag.as_millis()`, `log_depth = depth`),
   best-effort (a publish failure is traced and ignored — it must never perturb
   the flush path).

4. **`serve-web` reads real numbers** (`src/cli/serve_web.rs`): `read_stats`
   fetches `store.read_flush_stats(&state.session)`; `stats_from` renders
   `flush_lag_ms` / `log_depth` from the published value when `Some`, and keeps
   the honest `null` / `writer_only` tooltip when `None`. A transient read
   failure degrades to `n/a` rather than failing the whole endpoint; the
   existing session/counts output is unchanged.

5. **Web tile render** (`web/app.js`): when a published value is present the
   tiles show the real number (no `n/a` styling / tooltip); when absent they
   keep `n/a` and the writer-only tooltip. The stats note text now reflects the
   two cases.

## Tests added

- `store::tests::flush_stats_write_then_read_round_trips_memory` — writer
  publishes, reader reads back; absent = `None`; re-publish converges.
- `store::sqlite::tests::flush_stats_write_then_read_round_trips_sqlite` — same
  round-trip on the durable table.
- `store::flush::tests::writer_publishes_flush_stats_into_shared_store` — a
  spawned `FlushTask` flushes and the store then returns `Some` stats for the
  session (writer-publish ⇒ reader-read, end to end).
- `cli::serve_web::tests::stats_endpoint_renders_writer_published_flush_stats` —
  `/api/stats` returns the published real numbers.
- The pre-existing
  `stats_endpoint_counts_the_session_and_refuses_to_fake_writer_fields` still
  pins the `n/a` fallback when no writer has published.

(CockroachStore compiles under `store-cockroach`; its round-trip needs a live
cluster, so it is covered at the compile gate only, as the phase convention for
unavailable infra.)

## Gate block

- `cargo fmt` clean.
- `cargo clippy --all-targets -- -D warnings` — clean (default, `store-sqlite`,
  `store-cockroach`).
- `cargo test` — 714 passed, 0 failed (1 ignored).
- `cargo test --features store-sqlite` — 762 passed, 0 failed (1 ignored).
- `RUSTFLAGS="-D warnings" cargo check --no-default-features`
  (`--features store-memory`, `store-sqlite`, `store-cockroach`) — all clean.
- `cargo check --no-default-features` — clean.

## Live confirmation

On a sqlite store, `cargo build --features demo,store-sqlite`:

- Writer: `LAMBO_STORE=sqlite LAMBO_SQLITE_PATH=… lambo demo --session t85stats`
  published a `session_stats` row (`flush_lag_ms=0, log_depth=0`).
- Reader: `lambo serve-web --session t85stats` → `GET /api/stats` returned
  **`flush_lag_ms: 0`, `log_depth: 0`** (real numbers, not `n/a`).
- Absent: same store, `--session nosuch` → `GET /api/stats` returned
  **`flush_lag_ms: null`, `log_depth: null`** (honest `n/a` fallback).

Nothing committed.

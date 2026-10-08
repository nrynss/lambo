# #26: store decomposition and the fencing fixes (decisions)

Refactor 3/5. Base: main `9e6863a`. Landed via PR #45 (merge `168e7a4`).
Decisions and why; the commits carry the mechanics.

## Split by responsibility, with transaction ownership in one place

`store/sqlite.rs` (3,038 lines) became `sqlite/{mod, schema, persistence,
write_rows, session_load, vector_candidates, structural, leases, codec}`.
`store/pg/mod.rs` (3,290 lines) became `pg/{mod, pool, sql, schema, persistence,
write_rows, session_load, vector_candidates, structural, leases, codec}`, plus the
dialect files.

- Every write transaction begins and commits in `persistence.rs`.
  `write_rows.rs` holds statements only, run on the caller's connection, and
  never begins or commits. That made fencing auditable: the gate, the writes and
  the commit sit in one function. It is how the gaps below were found.
- All Postgres-family statement text lives in `pg/sql.rs`. Shared SQL only where
  the statements were byte-identical; no backend booleans and no generic
  repository layer.
- `cockroach.rs` stays one file: it is only the dialect.
- The `GraphStore` facades in `mod.rs` delegate to inherent methods with
  *different* names (`flush_batch`, `load_snapshot`, and so on). A private
  inherent method with a trait method's name would shadow the trait method for
  callers outside the module.

Moved function bodies were checked by script against the base: SQLite 69
identical, Postgres family 108 identical. The few differences are the vector
codec call sites, which changed in their own commit with identical bytes, and
one module path. Begin, commit and `tx_retry` counts are unchanged.

## The #8 seam

`store/vector_source.rs` holds `VectorCandidateSource` (session, probe, expected
contract, limit in; scored ids out) and the exact scorer `rank_by_cosine`.
`rank_by_cosine` takes borrowed `(NodeId, &[f32], &str)` items and is compiled
in every build, Memory-only included. That way a graph-backed source (#8, ranking
against the vectors the in-memory graph already holds) can plug in without
copying about 15 MB per recall. The vector codec is all in `store/vector.rs`, so
#8's packed-f32 migration touches one module.

## Defects found and fixed along the way (pre-existing)

- **Delete-only flushes skipped the fence.** `DeleteNode`/`DeleteEdge` name no
  session, and the fence set came from the mutations. So a delete-only batch, the
  shape a GC sweep produces, never consulted the lease, and a writer that had lost
  its lease could delete rows in a session another writer held.
  - Each adapter now resolves, inside the flush transaction and before any
    delete, the owning session of every row the deletes will remove. That
    includes incident edges in other sessions.
  - The sessions are added to the fence set. We resolve rather than carry the
    session on the mutation, so no wire or log format changes and old logged
    batches replay unchanged, and fenced.
  - Memory now does the same. It also had left cross-session incident edges
    dangling.
- **The Postgres fence did not lock.** Under READ COMMITTED, a plain
  `SELECT current_token` let a takeover commit between our check and our commit.
  - The read now takes `FOR SHARE` on the lease row, held to commit. Takeover and
    renew (`INSERT ... ON CONFLICT DO UPDATE`) wait for the flush; concurrent
    flushes of one session do not block each other.
  - Lease rows are locked in sorted session order, so two multi-session flushes
    cannot deadlock.
  - `record_canonization` uses the same read.
  - Cockroach (SERIALIZABLE) accepts the clause and was already safe.
  - Verified on live Postgres: the takeover blocked until the flush committed.
    A control leg shows the plain read does not block.
  - Two windows remain and are documented beside the code. The deleted-row
    lookup is a plain read. An unleased session has no lease row to lock.
- **Concurrent first-time schema init failed.** `apply_schema` called itself
  idempotent, which only holds serially. Two connections can both pass
  `CREATE ... IF NOT EXISTS`, and the loser fails on a catalog unique index. The
  batch is one implicit transaction, so the loser rolls back cleanly; it now
  retries on SQLSTATE 23505, 42P07 and 42710 only, at most three times. The new
  live tests initialising one database in parallel surfaced it.

`postgres-live` now runs `store::pg::delete_fencing::postgres_fences_a_delete_only_batch`
and `store::pg::lease_race::postgres_takeover_waits_for_the_fence_check`.
Cockroach's conformance step exercises the delete-only check, but CI's Cockroach
row is disabled, so that leg has not run live.

## Not done here

Three stale `store/sqlite.rs` mentions remain in files this phase did not
touch: the SQLite migration's DDL comment (part of the embedded schema text),
`Cargo.toml`, and a `ci.yml` comment.

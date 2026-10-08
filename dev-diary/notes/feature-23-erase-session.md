# #23: erase a session (decisions)

Base: main `3df6812` (refactor series #24-#28 and #50 merged). Branch
`feat/23-erase-session`. Decisions and why; the commits carry the mechanics.

## Shape

- `GraphStore::erase_session(session, eraser) -> Result<EraseOutcome, StoreError>`.
  `EraseOutcome` is `Erased(EraseReport)` or `Held { current, age }`, mirroring
  `LeaseOutcome`: a live writer is an outcome reported as data, not a backend
  error. The default implementation returns `Capability`, so a third-party
  adapter fails closed instead of letting a deletion fan-out mark it done.
- `eraser: &LeaseHolder` is in the signature (the issue sketched only the
  session) so an erase by the session's own holder is allowed. The CLI passes a
  fresh `lambo-erase-session@host#pid` holder that never holds the lease. #32's
  in-process erase passes the serving holder.
- Shared rules live in `store/erase.rs`: the report and counts, the lease gate
  (`erase_gate`), the tombstone constants and the fence's refusal text
  (`fence_refusal`, `erased_session_error`). `already_absent` is derived from
  the counts in `EraseReport::new`, never set by an adapter.
- Placement follows #26: the transaction in each adapter's `persistence.rs`
  (pg inside `tx_retry`), the per-table DELETEs in `write_rows.rs` (pg statement
  text in `pg/sql.rs`), a one-line delegation in each facade.

## Fencing: take the lease in the erase transaction, tombstone it, refuse a live writer

**The tombstone.** Erasure replaces the `session_leases` row instead of
deleting it: holder `lambo:erased`, token = prior + 1 (1 on a never-leased id),
`expires_at` = 9999-12-31T23:59:59Z, endpoint NULL, `acquired_at` = first erase.
Deleting the row would make the session read as *unleased*, and an unleased
session passes the fence (`lease_permits_write(0, _)`), so a lapsed writer's
next flush would recreate everything it held in RAM. With the tombstone:

- every pre-erase token is below the tombstone's (this holds only because a
  release keeps the row and its token; see "Review remediation" below), and an
  unleased write (`None`) is refused because the row exists;
- no acquire can take the session over (the expiry guard never fires, and
  `lambo:erased` has neither `@` nor `#`, so no `LeaseHolder::token` equals it);
- a zombie `serve`'s heartbeat gets `Held(tombstone)`, latches its fence and
  winds down through the existing JE2E-4 path.

The cost is one row holding the session id. That is the minimum that can
refuse later writes, and it is #32's "implicit creation must not recreate an
erased session" rule. Reusing an id is an operator act: an UPDATE that hands
the tombstone back as a released row and keeps `current_token` (statement in
`cli.mdx` and the `store::erase` module docs), never a DELETE. No schema change
was needed, so existing stores need no re-provision.

**The gate.** Proceed on no row, a lapsed lease, an earlier tombstone, or the
eraser's own lease; refuse a live lease held by anyone else, touching nothing.
Why refuse rather than preempt: a live holder has an in-RAM graph, a write
queue with durable intents, a pending access dirty set (#30) and a recall cache
(#14). Preempting it means reaching into another process. Refusing keeps one
rule: stop the writer (its `close()` flushes the tail and releases the lease),
then erase, and the erase removes the flushed tail too. A crashed writer's
lease lapses within one TTL and the erase then proceeds.

**Per store.**

- SQLite: `BEGIN IMMEDIATE`, so the lease read, the decision, the tombstone and
  the deletes all hold the write lock. A flush in another connection or process
  serialises behind it: its fence read happens in its own transaction, and
  either that transaction committed before the erase took the lock (the erase
  then deletes what it wrote) or it cannot write until the erase commits, after
  which a fresh attempt reads the tombstone. Not separately tested across two
  processes; the single-connection tests cover the fence itself.
- Postgres family: `SELECT ... FOR UPDATE` on the lease row. Every flush's fence
  reads the row `FOR SHARE` (#26), so a flush already past its fence commits
  first, and a later one waits for the erase and then reads the tombstone.
  With no row there is nothing to lock; the tombstone insert's own
  `ON CONFLICT ... WHERE` guard (the acquire's guard) settles a race with a first
  acquire, and an empty `RETURNING` is reported as `Held`. The one residual is
  the flush fence's existing one: an *unleased* flush (seed / fixture parity)
  racing an erase of a never-leased id under READ COMMITTED. Cockroach is
  SERIALIZABLE and aborts one side.
- Memory: one critical section over `inner`, `leases`, `flush_stats`,
  `refusals`, in the store's one lock order.

**Stragglers.** Flush-stats publication and lease-refusal recording are not
fenced writes, so a fenced writer could put a `session_stats` row back in the
window before its heartbeat sees the tombstone. Both are now suppressed for a
tombstoned session (an `INSERT ... SELECT ... WHERE NOT EXISTS` on SQL, a check
on Memory). A rerun would sweep such a row anyway, but the guard makes "nothing
recreates an erased session" hold without one.

**Refusal text.** The fence reads the lease holder with the token (since the
review remediation; it used to read it on the refusal path only), so a write to
an erased session gets `StaleWrite("session X was erased ...")`, and an
ordinary stale write keeps its message byte for byte. `StaleWrite` rather
than a new `StoreError` variant: `StoreError` is public and not
`#[non_exhaustive]`, a new variant would break downstream matches, and every
caller already treats `StaleWrite` as terminal (not retried, writer fenced),
which is the right handling. The decision that turns on "erased" is made on
typed data (the lease row's holder), never on the message.

## Idempotent, complete, and crash-safe

- One transaction on every store, so there is no partial state to resume: a
  failure at any step leaves the session as it was, and the rerun does the
  whole job. The SQL adapters take an `EraseStepHook` called after each step;
  production passes `no_fault`, tests pass `FailAt(n)` for every `n` (an
  `Invariant` error, because pg's `tx_retry` replays `Backend` errors).
- DELETE order is dependency order: `write_intents`, `canonization_events`,
  `reservations`, `synonyms`, `edges`, `concepts`, `interactions`,
  `session_stats`, `lease_refusals`, `sessions`. The interactions self-reference
  is satisfied by deleting the whole session's interactions in one statement
  (both engines check at end of statement; SQLite with `foreign_keys` on
  likewise).
- `already_absent` is "no rows removed", so a repeat and a never-used id both
  report it, and a rerun that swept a straggler reports `false` (it did
  remove something). The tombstone's token does not move on a repeat.
- Completion: `Erased` is returned only after `commit`.

## Coverage: every table, provably

- `ERASE_STATEMENTS` (SQLite `write_rows.rs`, pg `sql.rs`) plus `session_leases`
  is asserted equal to `tables_in_ddl` of every shipped migration
  (`erase_covers_every_table_in_the_ddl`, `erase_covers_every_table_in_both_ddls`,
  both offline). A new table fails a test until erasure covers it.
- The behavioural tests plant a row in **every** DDL table (asserting the
  census is non-zero everywhere first, so a new table the fixture does not
  plant also fails), erase, and assert zero rows everywhere except the
  tombstone. A second session is planted and compared before and after.
- Memory: the census destructures `MemoryStore` exhaustively, so a new
  per-session field does not compile until the census (and erase) cover it.
- Per table, what each holds: interactions (prompt text), concepts (content and
  the vector column, counted as `vectors`), edges, synonyms, reservations,
  canonization_events (the canonization state and history), write_intents
  (durable intents; consumed rows are retained 300 s and their payloads carry
  concept text, so they must go), session_stats (writer flush stats),
  lease_refusals, sessions (embedding contract, root goal, mutation epoch, GC
  mark: the #29 GC state). Access counts (#30) are columns of `concepts`.

## In-process state and running writers

- `MemoryBuilder::build_attach` turns `Held(tombstone)` into the erased error.
  Left as `Held`, `serve` would treat the tombstone as a holder to proxy to,
  find no endpoint, wait forever, and record lease refusals against the erased
  id. `derive`, `record-action`, `reserve`, `re-embed` and `serve` all attach
  through it.
- A handle fenced *by an erase* (`Memory::erased()`: lease lost, winner is the
  tombstone) refuses reads as well as writes (`ensure_open`), with the erased
  error, because its graph is a copy of deleted data. An ordinary takeover keeps
  its old behaviour. `serve` already exits on the latch, discarding the tail.
- Write queue (#27): nothing to abort in the CLI process, and a live holder's
  queue is never erased under it (the gate). Its durable intents become
  `write_intents` rows at close and are deleted; the next attach cannot replay
  them because there is no next attach.
- Recall cache (#14), access dirty set (#30), and the in-memory vectors #8 moves
  into `graph/graph/embeddings.rs`: all are per-`Memory` state, so they go with
  the process that holds them. No code path here touches the graph types, so
  #8 can land in either order.

## Surface

- CLI only: `lambo erase-session --session <s> --confirm <s>`, store-only (no
  embedder), JSON report on stdout, exit 2 on a mismatched confirm, exit 1 when a
  live writer holds the session.
- No MCP tool and no `serve-web` route. The issue asks for one "behind an
  operator credential", and no operator credential exists yet: `serve`'s bearer
  token is the agent's credential, and `agent_id` is caller-asserted. #32
  settles authority (decision 2) for multi-session serving; an in-serve erase
  belongs there, built on `erase_session(session, serving_holder)` after the
  session is detached (close, then erase as the same holder, or erase first and
  let the heartbeat fence). The portal stays read-only.

## Not reached by erasure

- Backups and snapshots taken before the erase (operator retention).
- The `serve --ledger` call ledger file: JSONL outside the store, rotated by the
  operator, which can carry recall queries and truncated concept text. Erasure
  does not rewrite it. Documented in `cli.mdx` and the changelog.

## User decisions (2026-10-08)

- Erasure stays CLI-only. The MCP and portal surface is deferred to #32's
  authority design (decision 2), not built here.
- The tombstone keeps the plain session id (no hashing): it is the minimum
  that can refuse later writes, and an operator must be able to find it.
- The `serve --ledger` file and backups are operator-owned and documented as
  such. Erasure does not scrub them.

## Review remediation (Opus review, `scratchpad/refactor/23/review-opus.md`)

**H2 (pre-existing on main): a release reset the fencing token.** A release
deleted the lease row, so the next acquire minted token 1 again and a lapsed
writer still holding an older token passed `presented >= current` against the
new holder. Rule now: **a `session_leases` row is never deleted and its
`current_token` only goes up, for the life of the session id.** A release is a
holder-scoped UPDATE to holder `lambo:released`, `expires_at` = the store
clock, `endpoint` NULL, token kept, on all three stores; the next acquire takes
the expired-row arm and mints `current + 1`. The marker holder (rather than
keeping the releaser's id) makes the same identity's re-acquire a takeover too,
because an acquire by the row's own holder is a refresh that keeps the token
and `acquired_at`. `OPERATOR_OVERRIDE` is the matching UPDATE (`expires_at =
acquired_at` is in the past in every dialect; guarded with `holder <>
'lambo:erased'` so it never lifts a tombstone). Consequences: a session that
was ever leased refuses unleased (`None`) writes forever, as it already did
while an expired row lingered after a crash; the erase gate counts a released
row as no lease; the proxy's dial treats one as "no holder". Tests
(`lease::testkit::check_release_keeps_the_fencing_token`) on SQLite and Memory,
and live pg legs (`postgres_release_keeps_the_fencing_token`, Cockroach
conformance).

**H1: the tombstone fenced by arithmetic only.** With H2 the tombstone is
`current + 1` of a preserved row, which refuses every pre-erase token. Defense
in depth on typed data: every fence (`store::erase::check_fence`, all stores,
flush and canonization) refuses a tombstoned session whatever token is
presented, the tombstone's own and higher included. On pg the holder rides the
existing `FOR SHARE` read. The store's holder values (`lambo:released`,
`lambo:erased`) are reserved: acquire, refresh and erase refuse a caller whose
holder token equals one (unreachable through `LeaseHolder::token`, which always
has `@` and `#`; tested at the function). Test: the reviewer's scenario on every
store (`erase::testkit::check_erase_after_release_fences`).

**L1:** the pg erase locks `sessions` before the lease row, the order a flush
takes them in, so a zombie flush and an erase cannot deadlock (40P01).
**L2:** a background flush or canonization cycle that fails on an erased
session (confirmed by re-reading the lease row) latches the fence and the
serve wake-up at once, so `erased()` holds and reads stop without waiting for
the heartbeat. **L3:** a proxy whose session was erased answers the call with
an erased reply (`-32003`) and exits with the erased error. **L4:** edges in
other sessions incident to the erased nodes are deleted with them and counted
in `edges` (consistent with `DeleteNode`; a live writer of the other session
could still re-upsert one from RAM, the same residual `DeleteNode` has).
**L5:** a fixture `seed` of an erased id is refused. **L6:** no change (the
residual is reachable only from fixtures, as the note above says). **L7:**
`EraseCounts` and `EraseReport` are `#[non_exhaustive]`.

## Not run here

The Postgres and Cockroach live legs (`store::pg::erase::postgres_erases_a_session`,
`postgres_erase_after_a_release_fences_every_token`,
`store::pg::release_fencing::postgres_release_keeps_the_fencing_token`, and their
checks inside the Cockroach conformance suite) need a live engine; no local
Postgres was available to either the implementer or the remediator. CI's `postgres-live` job runs only
named tests, so it needs a step for the new test (diff in the implementer's
report; workflow files are not edited by agents).

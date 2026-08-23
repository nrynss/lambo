# B3 implementation brief

Orchestrator: grok-agent. B0-B2 closed on `b0-pg-extraction` (`97ee28c`).
You implement B3. **Do not commit. Do not merge to lambo-for-mooshik.**

## Read
`B-postgres-store.md` **B3**. CYCLE.md. `src/store/pg/postgres.rs` (B3
ranking is still `unimplemented!`). H-cross-store-parity.md (H3 *is*
B3's parity box). Do not write a second harness: extend H1
`build_adapters`.

## Build
The Dialect table for Postgres:

| method | Postgres |
| --- | --- |
| string_cast | `::TEXT` |
| vector cast | pgvector's own cast |
| distance_op | `<=>` (cosine distance) |
| distance_to_score | `1 - d` |

Cockroach stays `<->` L2 and `1 - d^2/2`. Unit-norm Embedder output is
what makes that equivalent to cosine. Getting Postgres's conversion
wrong does not fail: it ranks wrongly. Pin it with a test that goes red
if someone copies the Cockroach formula onto Postgres, and vice versa.

**H3:** same harness, pgvector. Forced-exact lane
(`SET LOCAL enable_indexscan = off`) must show zero adapter skew.
hnsw lane's divergence is an envelope, stated with numbers. Run against
the pinned `pgvector/pgvector:pg17` digest
`sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.

Preserve in PgStore, unchanged: fencing StaleWrite, idempotent upserts,
created_at divergence, NULL-only quarantine.

B4 is live-schema `vector_dimensions()`. Do not close B4 here.

sqlite.rs: only extend `build_adapters` if that is where H1 lives; do
not change SQLite vector behaviour.

## Do not
Re-litigate hnsw-from-init, ivfflat, alias split, park-and-fail-over.
Copy Cockroach `<->` + `1-d^2/2` onto Postgres. Touch `.env`. No em
dashes. No commit.

## Gates
CYCLE + store-postgres + H3/live container if you run it. B0 composed-SQL
pin must still hold for Cockroach.

## Deliverable
`b-run/B3-implementation.md`: the conversion reasoning, H3 numbers or
why deferred, gates Claimed/Measured.

agent_id `b3-implementor`.

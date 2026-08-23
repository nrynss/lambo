# B1 implementation brief

Orchestrator: grok-agent. B0 is **closed** (round 2 APPROVE, `9d9d8d7`).
You implement B1 on `/home/nryn/work/lambo`, branch `b0-pg-extraction`.
**Do not commit. Do not push.** Leave the tree dirty.

## Read first
1. `B-postgres-store.md` **B1** and **What workstream J left in B's path**
   (items 2 and 3 are B1-forced). Park-and-fail-over (item 4) is a B-wide
   ruling already recorded in FUTURE.md: do **not** build it in B1.
2. `b-run/CYCLE.md` (standing rules, expected B0 counts 939 / 598 / 1007).
3. `src/store/mod.rs` `StoreKind`, `build_store`, tests around the
   `"postgres" | "pg" => Cockroach` mapping.
4. `src/mcp/endpoint.rs` `store_is_shareable` (exhaustive match).
5. J2-R1-2: hash store **identity**, not spelling (`store_identity`).

## What to build
New `StoreKind::Postgres`, Cargo feature `store-postgres` (sqlx postgres
driver already compiles under `store-cockroach`; do not add a second
driver). Clean alias split:

| Config string | Resolves to |
| --- | --- |
| `"postgres"`, `"pg"` | `Postgres` |
| `"cockroach"`, `"crdb"` | `Cockroach` |

No string maps across. Fail **loud, not wrong**: a leftover
`kind = "postgres"` pointed at Cockroach must die at provision or first
vector query, never silently mis-rank.

Record the decision in the variant's doc comment so git history of
`"postgres" | "pg" => Cockroach` reads as a choice. Update the tests
that assert the old mapping **deliberately**. Update the
`expected memory | cockroach | sqlite` error strings. Note the break
in the 0.3.0 changelog.

**B1-forced from J:**
- `store_is_shareable(Postgres) = true` (same reasoning as Cockroach:
  networked store another process can open). Rule it in the match;
  do not let `_ =>` hide it.
- DSN identity vs spelling: `postgres://u@host/db` and
  `postgres://u@host:5432/db` must derive **one** session endpoint.
  Normalise beside `store_identity` (default port, default database,
  host case, ignored params as you justify). Test two spellings, one
  endpoint. Password must stay out of the filesystem and the lease row.

**PostgresDialect file:** add `src/store/pg/postgres.rs` so the kind
can name a dialect. **Do not copy Cockroach SQL.** B2 owns templated
width, hnsw-from-init, and the distance conversion. If `build_store`
can construct today, construction must not emit Cockroach DDL. Fail
closed at init/provision naming B2 rather than approximating. C1
precedent: a stub that promotes nothing / a dialect that speaks
Cockroach is indistinguishable from a finished broken Postgres.

**CI:** add a `store-postgres` compile + unit matrix row (no live
database). `postgres-live` service container is later (B2/B3).

## Do not
- Split `init_schema` / `connect_options` (B0-R1-3 debt, B2/B3).
- Touch NULL-only quarantine, width-from-DDL on Cockroach, H1, sqlite.
- Re-litigate alias split, hnsw-from-init, park-and-fail-over.
- Start a Postgres container unless a unit test truly needs it.
  If you do, only `pgvector/pgvector:pg17` digest
  `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
- Touch `.env` or `models/`. No em dashes. No commit.

## Gates
Re-run CYCLE gates including fixtures. Cockroach listed counts should
stay 939 / 598 / 1007 unless you add tests on that feature set: if they
move, say which tests and why. Add `store-postgres` compile/unit and
report counts. H1 lock: sqlite.rs untouched.

## Deliverable
`b-run/B1-implementation.md`: what you built, every gate Claimed/Measured,
the DSN-normalisation rule and its test, the shareable ruling, the
fail-closed construction story.

agent_id if you dogfood: `b1-implementor`.

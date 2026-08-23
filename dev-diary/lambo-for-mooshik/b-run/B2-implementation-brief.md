# B2 implementation brief

Orchestrator: grok-agent. B0 and B1 are **closed** on `b0-pg-extraction`
(`9d9d8d7`, `e14ba49`). Round 2 APPROVE both. You implement B2.
**Do not commit. Do not merge to lambo-for-mooshik.**

## Read
`B-postgres-store.md` **B2** (and B3 table so you do not steal B3's
distance conversion). CYCLE.md. `src/store/pg/postgres.rs` (fail-closed
today). B0-R1-3: `init_schema` and `connect_options` are known over-merged
debt; B2 may split what Postgres init actually needs.

## Build
`PostgresDialect::init_sql(dim)`: pgvector schema at a width taken from
config, **hnsw index in the same init**. ivfflat is rejected (recorded).
pgvector defaults for m / ef_construction / ef_search; no knobs yet.

**dim > 2000:** pgvector hnsw ceiling on `vector` is 2000. 768 and 1536
pass; Gemini 3072 does not. Handle **at init, loudly**: refuse naming the
ceiling and the `halfvec` hatch, **or** implement `halfvec`. Decide and
record in B-postgres-store.md B2. Never let `CREATE INDEX` be the discovery.

**Width data flow:** Cockroach parses width *out* of static DDL. Postgres
substitutes width *into* SQL. Choose template-at-init or generate-in-code,
record the choice in B-postgres-store.md. `vector_dimensions()` authority
is B4; do not pretend B2 closed B4.

B3 still owns `STRING_CAST` / vector cast / `DISTANCE_OP` /
`distance_to_score`. You may need those tokens to exist for init+compile;
do not invent a ranking conversion "good enough" and call B3 done. If you
must pick a compile-time token so init runs, fail closed on the ranking
path the way B1 did, or implement the B3 table *only if* you cannot
otherwise init. Prefer leaving ranking to B3 unless the suite forces it.

**Over-merge:** shared `init_schema` runs `ALTER ... endpoint STRING` and
`connect_options` sets `vector_search_beam_size`. Those are Cockroach.
Postgres init cannot keep them. Split per the byte-identical rule.

**CI:** `postgres-live` with service container `pgvector/pgvector:pg17`
digest `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`
is this workstream's live job. If B2 can init a real schema, add or
skeleton the job; do not use `:latest`. Local docker: same image only.

**H1 lock:** sqlite.rs untouched. H3 is B3's parity box: do not invent a
second harness.

## Do not
Re-litigate hnsw-from-init, ivfflat, alias split, park-and-fail-over.
Copy Cockroach `VECTOR(1024)` / `<->` as if they were Postgres.
Touch `.env` or `models/`. No em dashes. No commit.

## Gates
CYCLE + store-postgres. If you run against the container, say so and
quote the digest. Schema must initialise at more than one width with
hnsw present from init; dim > 2000 loud.

## Deliverable
`b-run/B2-implementation.md`: choice of template vs generate, dim>2000
decision, over-merge split, gates Claimed/Measured.

agent_id `b2-implementor`.

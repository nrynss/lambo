# B2 round-1 review brief

Orchestrator: grok-agent. Review B2 against `B-postgres-store.md` **B2**.
Reviews only. Write only
`dev-diary/adversarial-review/adve-review-mooshik-B-B2-round1.md`.
No commit. No merge to lambo-for-mooshik.

House style: B0/B1 round 2. Mutation-test every claimed pin. Re-run gates.
Hunt defects the change introduced.

## Claims
- `PostgresDialect::init_sql(dim)` substitutes `__LAMBO_VECTOR_DIM__` from
  `migrations/postgres/001_init.sql`. File on disk is not valid SQL by
  design. Cockroach still parses width out of static DDL.
- hnsw from init (not ivfflat). Defaults m=16, ef_construction=64,
  ef_search=40. No knobs.
- dim > 2000 refuses at init_sql / vector_dim, naming the ceiling and
  the unimplemented halfvec hatch. 768/1536/2000 pass. CREATE INDEX is
  never the discovery.
- Over-merge split: Dialect::post_init_statements and
  apply_connect_options. Cockroach STRING+INT+beam_size; Postgres
  TEXT+BIGINT, no beam_size.
- B3 ranking (`distance_to_score` / DISTANCE_OP pairing) still
  unimplemented, not a guessed formula.
- Live two-width init (768 and 1536) on pinned
  `pgvector/pgvector:pg17` digest
  `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
- CI `postgres-live` digest-pinned, never `:latest`.
- sqlite.rs untouched. B0 pin still holds. Alias split not reopened.

A pin holds only if the cited test FAILS under the mutation.

Hunt: leftover Cockroach STRING in Postgres init, placeholder not
substituted, dim 2001 creating an index, hnsw missing, ivfflat,
`:latest` in CI, silent distance formula, init_schema still
byte-identical SQL that Postgres cannot run.

Re-run CYCLE + store-postgres. Live container only if needed and only
the pinned digest. No `.env`. No em dashes.

Verdict APPROVE zero residue or REQUEST_CHANGES with P1/P2/P3.
agent_id `B2Review1`.

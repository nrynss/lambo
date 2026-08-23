# B1 round-1 review brief

Orchestrator: grok-agent. Review B1 implementation on `b0-pg-extraction`
against `B-postgres-store.md` B1 and "What workstream J left in B's path"
items 2 and 3.

**Reviews only.** Write only
`dev-diary/adversarial-review/adve-review-mooshik-B-B1-round1.md`.
Transient mutations, then full revert. No commit.

House style: `adve-review-mooshik-K-round2.md` / B0 round 2.

## Claims to mutation-test
- Alias split: postgres/pg are Postgres; cockroach/crdb are Cockroach;
  no cross mapping. Fail loud, not silent mis-rank.
- `store_is_shareable(Postgres) = true` (exhaustive; no `_ =>`).
- Two DSN spellings of one database derive one endpoint; password not
  hashed into the path; Cockroach vs Postgres with the same DSN string
  are different stores if that is claimed.
- PostgresDialect does not emit Cockroach SQL (`::STRING`, `<->`,
  `VECTOR(1024)` DDL). Construction/init fails closed naming B2.
- `postgres_build_behavior` for compiled vs uncompiled feature.

Hunt: copied Cockroach SQL, vacuous pin, DSN normalisation that still
splits default-port vs explicit-port, shareable defaulted via `_`,
changelog/docs drift, CI row that does not compile the feature, H1
sqlite.rs touched, init_schema split (must NOT have happened).

Re-run CYCLE gates plus `store-postgres` compile/unit. No live
Cockroach. No Postgres container unless you must; if so, pinned image
only. No `.env`. No em dashes.

Verdict APPROVE zero residue or REQUEST_CHANGES with P1/P2/P3.
agent_id `B1Review1`.

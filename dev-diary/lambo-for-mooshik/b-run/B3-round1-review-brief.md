# B3 round-1 review brief

Orchestrator: grok-agent. Review B3 against `B-postgres-store.md` **B3**.
Reviews only. Write only
`dev-diary/adversarial-review/adve-review-mooshik-B-B3-round1.md`.
No commit. No merge to lambo-for-mooshik.

House style: B2 round 2. Mutation-test every claimed pin. Re-run gates.

## Claims
- Postgres: `::TEXT`, pgvector vector cast, `<=>`, score `1 - d` (clamped).
- Cockroach unchanged: `::STRING`, `::VECTOR`, `<->`, `1 - d^2/2`.
- Copying either formula onto the other dialect goes red.
- Composed SQL rejects the other dialect's tokens.
- H3 extends H1 `build_adapters` (not a second harness): postgres-hnsw
  and postgres-exact (`SET LOCAL enable_indexscan = off`).
- Forced-exact vs sqlite/memory-oracle: zero adapter skew.
- hnsw envelope stated with numbers; EXPLAIN proves index vs seq scan.
- Fencing, upsert replay, created_at, NULL-only quarantine still hold.
- B4 not closed. B0 composed-SQL pin still holds.
- sqlite.rs only for H1 adapter list / H3; vector behaviour unchanged.

Hunt: silent mis-rank (Cockroach formula on Postgres), H1 regression,
second harness, `:latest` image, B4 accidentally claimed, quarantine
restamp inherited from SQLite.

Pinned image only. No `.env`. No em dashes.

Verdict APPROVE zero residue or REQUEST_CHANGES with P1/P2/P3.
agent_id `B3Review1`.

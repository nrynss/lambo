# B4 round-1 review brief

Orchestrator: grok-agent. Review B4 against `B-postgres-store.md` **B4**.
The B4 implementor died on a 402; this code is orchestrator-authored.
Reviews only. Write
`dev-diary/adversarial-review/adve-review-mooshik-B-B4-round1.md`.
No commit. No merge to lambo-for-mooshik.

Claims:
- Postgres probes live `vector(n)` at init and preflight.
- Cockroach does not (static DDL).
- Mismatch (construct 1536 against 768 schema) fails naming both numbers.
- parse rejects `VECTOR(n)`.
- vector_dimensions is not an unchecked echo: skipping the live check
  turns `live_schema_width_refuses_a_config_that_disagrees` red.
- Pin at resolve_backends unchanged. B3 ranking unchanged. sqlite.rs
  untouched.

Pinned image. No em dashes. agent_id `B4Review1`.

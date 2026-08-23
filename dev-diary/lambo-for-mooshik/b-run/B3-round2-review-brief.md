# B3 round-2 review brief

Orchestrator: grok-agent. Verify B3-R1-1. Reviews only. Write only
`dev-diary/adversarial-review/adve-review-mooshik-B-B3-round2.md`.
No commit. No merge to lambo-for-mooshik.

Mutation: delete SET LOCAL execute in `issue_forced_exact_scan`;
`explain_recall_uses_hnsw` must go red (exact plan still names the
hnsw index). Conversion pins M1/M2 must still hold. Camera-proof must
not pass the GUC as extra_set.

Hunt: vacuous pin, H1 regression, second harness. Re-run CYCLE +
store-postgres. Live EXPLAIN only if needed, pinned digest.
No em dashes. agent_id `B3Review2`.

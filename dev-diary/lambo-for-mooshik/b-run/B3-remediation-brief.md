# B3 round-1 remediation brief

Orchestrator: grok-agent. Close B3-R1-1. Work on `b0-pg-extraction`.
**Do not commit. Do not merge to lambo-for-mooshik.**

## B3-R1-1 (P2)

Forced-exact GUC is issued in `vector_candidates_checked` when
`force_exact_scan` is set. The camera-proof does not go through that
path: `explain_vector_candidates` ignores `store.forced_exact_scan()`
and takes `extra_set`. `explain_recall_uses_hnsw` constructs
`with_forced_exact_scan()` then passes the GUC as `extra_set`, so the
flag is dead. H3 hardcodes `index_present` and does not EXPLAIN the
exact lane. At fixture size, deleting the execute in
`vector_candidates_checked` leaves H3 green while `postgres-exact` is
still ANN.

**Fix:** `explain_vector_candidates` must apply
`D::forced_exact_scan_sql()` when `store.forced_exact_scan()` is true.
The live EXPLAIN of the exact lane must **not** pass the GUC as
`extra_set`. Deleting the execute at `pg/mod.rs` ~2737-2741 must turn
`explain_recall_uses_hnsw` red (forced-exact plan still names
`concepts_embedding_idx`). Optional: H3 EXPLAIN the exact store or
probe `index_present` from the plan.

Do not reopen the conversion pins. No em dashes. No `.env`. sqlite.rs
only if H3 probe requires it.

## Report
`b-run/B3-remediation.md` plus closures appendix on round-1 review.
agent_id `b3-remediator`.

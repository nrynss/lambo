# B2 round-1 remediation brief

Orchestrator: grok-agent. Close B2-R1-1 in
`dev-diary/adversarial-review/adve-review-mooshik-B-B2-round1.md`.
Work on `b0-pg-extraction`. **Do not commit. Do not merge to
lambo-for-mooshik.**

No em dashes. No `.env`. sqlite.rs untouched. Do not reopen B0/B1.
Do not implement B3 ranking. Do not split more than the review asks.

## B2-R1-1 (P2)

`build_store_with_vector_dim` copies embedder width into the pin when
absent (`src/store/mod.rs` ~954-961). Deleting that copy leaves every
listed B2 unit test green. Production is `resolve_backends` passing
`Some(embedder_cfg.dim)`. Silent init at 1024 against a 768 embedder.

Rustdoc still says only SQLite consumes the argument (`mod.rs` ~887-889
and ~922-926). Postgres now has a width of its own **and** consumes the
argument.

**Fix:**
1. Test: `build_store_with_vector_dim` with `vector_dim: None` and param
   `Some(768)` reports `vector_dimensions() == Some(768)`. Pin `Some(1536)`
   still wins over param `Some(768)`.
2. Mutation that deletes the copy must go red.
3. Update the rustdoc so it no longer says only SQLite consumes the
   argument.

## Gates
CYCLE + store-postgres. Mutation-prove the new pin, revert. Live
container not required unless the test needs it.

## Report
`b-run/B2-remediation.md` and a closures appendix on the round-1 review
(do not rewrite the verdict). agent_id `b2-remediator`.

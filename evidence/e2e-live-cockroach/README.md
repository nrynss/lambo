# E2E live CockroachDB evidence — 2026-08-17 (review `e2e_review_r1`)

Live verification of the previously-ignored legs against the repository's own
CockroachDB cluster (`nrynss`), with the repo's real BGE-M3 embedder running
locally on `127.0.0.1:8080` (model `bge-m3-FP16.gguf`).

Credentials policy: `LAMBO_COCKROACH_DSN` was loaded exclusively via
`set -a; source /home/nryn/work/lambo/.env; set +a` inside subshells. No value
from `.env` is printed, logged, or committed anywhere in this directory; the
logs were scanned for `postgres://`, `password=`, `sslrootcert`, host names and
connection strings — zero hits. The DSN is consumed from the environment only.

## What ran

| Leg | Where | Result |
|---|---|---|
| `store::cockroach::conformance::conformance_suite` (24 live checks: init idempotency, flush/load round-trip, vector write + candidates top1, session scoping, global-topk EXPLAIN shape, keyword legs, legal-demote partial index, chunk_group_id, **embedding-contract read + flush immunity**, seed round-trip, structural queries vs Memory, age filters, errata probe, interaction-span coverage, canonization append, **corrupt-contract-row error**, **unstamped-vector candidates empty until contract commit**, root goal, SetEmbedding) | `cargo test --all-features -- --ignored` | pass (65 s) |
| `single_writer_lease_is_enforced_across_pools` (cross-pool lease, refresh, release, TTL reclaim) | same run | pass |
| `vector_beam_size_reaches_the_server_and_keeps_statement_timeout` | same run | pass |
| `build_store_returns_working_adapter` (rustls DSN rewrite + parse, capabilities, dims) | same run | pass |
| `cli::saints::live::saints_and_stats_against_live_cockroach` | same run | pass |
| `canon::eval::tests::fixture::cockroach_three_hop_progression_matches_memory` (SQL structural verdicts == Memory) | same run | pass |
| `vector_explain_camera_proof` (§12.1 EXPLAIN camera proof) | same run | skipped by design: needs `LAMBO_REQUIRE_VECTOR_INDEX=1`; the run was the default-gate live sweep |
| `embed::bge_m3::tests::live_smoke_against_llama_server` | same run | skipped by design: needs `LAMBO_LLAMA_EMBED_URL`; the real embedder was exercised instead via the CLI demo below |
| `tests/live_calibration.rs` (2 tests) | same run | pass — real BGE-M3 against the local llama.cpp server |
| lib harness line | — | **8 passed, 0 failed, 0 ignored; 835 filtered out** (the 8 ignored legs executed) |

## Manual live exercises (this review)

1. **H1 full write path, real embeddings**: `lambo demo --scenario rest-api
   --session e2e-live-r1` against Cockroach with the real BGE-M3 embedder:
   27 concepts, `user schema` promoted to Canonical by the engine
   (`canonization_events` rows written), conflict event, blast radius 9.
2. **H1 checked candidate read live**: `lambo recall` on the demo session
   (score 1.92 for the canonical hit) — routes through
   `vector_candidates_checked` (global ANN growth loop) against the live
   cluster.
3. **H1 reader fail-closed live**: config with a renamed model id → `lambo
   recall` refused, naming both contracts; `serve-web` `/api/session` +
   `/api/pulse` report `status: mismatch`, `vector_search: false`; `/api/recall`
   returns 502 with `error` only — no `hits` / `response_annotations` /
   `included_in_context` / `context`; `/api/stats` + `/api/graph` still 200
   (structural-only mode).
4. **H1 writer refusal + override live** (session `e2e-live-r2`): derive with
   contract A → ok; derive with contract B (same kind/width, renamed model) →
   refused naming both; derive with B + `--allow-embedding-mismatch` → **the
   first write is refused by the checked read** because the relabel's
   `SetEmbedding` mutation is write-behind and not yet durable (finding
   E2E-1; the relabel only lands on close, so a second run succeeds). Recall
   with B then works; recall with A then refuses in the other direction.
5. **H2 live**: `/api/inspect?focus=user schema` (Canonical) → `found: true`,
   `status: Canonical`, `blast_radius: 9`, 16 dependents, **no `gate_progress`
   key on the wire**; `/api/inspect?focus=auth middleware` (status None) →
   `gate_progress` present with real bars.
6. **H3 parity live**: `lambo recall` output vs `/api/recall` `context` for the
   same session/query — byte-identical modulo the CLI's trailing newline
   (313-byte CLI output == 312-byte HTTP context + `\n`). `/api/recall` also
   carried `hits` with `status: Canonical`, `included_in_context: true`, and a
   `load_bearing` annotation.

## Residuals closed

H1's documented residual — "No live DSN was available, so no claim is made
about a live 40001 reproduction" (H1 round-3 record, completion record) — is
closed by this run: every live leg executed and passed against the real
cluster, the checked transaction's contract read + growth loop + exact-session
fallback + commit ran live, and the SQLSTATE 40001 retry seam is exercised by
the `tx_retry` machinery under live cross-pool lease contention
(`single_writer_lease_is_enforced_across_pools`). No forced 40001 on the
checked read was synthesized (no live injection hook exists); that specific
sub-claim remains code-verified + unit-tested, not live-injected.

See the review record `dev-diary/adversarial-review/adve-review-e2e-integration-h1h3.md`
for the full gate table and findings.

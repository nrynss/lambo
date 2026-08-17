# Adversarial reviews

# Complete index of the review records in this directory (64 records).

| File | Scope | Status |
|------|-------|--------|
| [adve-review-c-series-round1.md](adve-review-c-series-round1.md) | codex/c-series — C1–C5 concurrency capture (load driver, SIGTERM capture, durability accounting, real-model swarm), round 1 | **REQUEST_CHANGES** (1 P1 / 2 P2 / 5 P3; evidence-doc integrity — see the record, 2026-08-18) |
| [adve-review-c-series-round2.md](adve-review-c-series-round2.md) | codex/c-series — C1–C5 concurrency capture, round 2 re-review of the round-1 remediation (all 8 findings closed) | **CLEAN / APPROVE** (1 P1 / 2 P2 / 5 P3 all verified closed against artifacts; regression sweep green; zero new findings, 2026-08-18) |
| [adve-review-e2e-integration-h1h3-r2.md](adve-review-e2e-integration-h1h3-r2.md) | Main — E2E round-2 re-review of the remediation branch (E2E-1..8 closure re-verification + regression sweep) | **CLEAN / APPROVE** (all 8 findings closed incl. LIVE first-run override success; zero new findings, 2026-08-17) |
| [adve-review-e2e-integration-h1h3.md](adve-review-e2e-integration-h1h3.md) | Main — E2E whole-platform integration (H1+H2+H3 + portal rebuild), incl. LIVE CockroachDB legs | **CONDITIONAL** (1 P2 live-reproduced / 7 P3; no P0/P1; all gates green incl. 8/8 live legs executed, 2026-08-17) |
| [adve-review-hardening-H3round2.md](adve-review-hardening-H3round2.md) | Hardening H3 - post-round-1 remediation (evidence re-capture + per-text warning parity), round 2 | **CLEAN / APPROVE** (zero findings, 2026-08-17) |
| [adve-review-hardening-H3round1.md](adve-review-hardening-H3round1.md) | Hardening H3 - structured recall results beside the verbatim block, round 1 | **REQUEST_CHANGES** (1 P2 evidence / 1 P3; verdict 2026-08-17) |
| [adve-review-hardening-H2round1.md](adve-review-hardening-H2round1.md) | Hardening H2 - /api/inspect Canonical gate-pairing, round 1 | **CLEAN / APPROVE** (1 P3 report-doc; zero code findings, 2026-08-17) |
| [adve-review-hardening-H1round1.md](adve-review-hardening-H1round1.md) | Hardening H1 - embedding-contract enforcement, round 1 | **CLOSED** (REQUEST_CHANGES - 2 P1 / 1 P2 / 1 P3; remediated `c72acf5`, disposition `298af97`) |
| [adve-review-hardening-H1round2.md](adve-review-hardening-H1round2.md) | Hardening H1 - post-round-1 remediation, round 2 | **CLOSED** (REQUEST_CHANGES - 1 P2 / 1 P3; remediated `7cd8194`, disposition `de3b4b7`) |
| [adve-review-hardening-H1round3.md](adve-review-hardening-H1round3.md) | Hardening H1 - post-round-2 remediation, round 3 | **CLOSED** (CLEAN / APPROVE - zero findings, 2026-08-17) |
| [adve-review-hardening-task-enrichment.md](adve-review-hardening-task-enrichment.md) | Hardening H2-H7 task-description enrichment | **CLOSED** (R1 3 P2 / 2 P3; R2 1 P2 / 2 P3; R3 CLEAN / APPROVE, 2026-08-17) |
| **[adve-review-t8.1-memory-fable.md](adve-review-t8.1-memory-fable.md)** | phase/p8-surface — T8.1 `Memory` builder & assembly, fable deep | **CLOSED** (1 P1 / 2 P2 / 6 P3 + 9 follow-on findings across rounds; 3 opus remediation rounds, R4 verify CLEAN 2026-08-14; COH-6 all 15 clauses PASS; merged c6576f7) |
| [adve-review-t8.2-mcp.md](adve-review-t8.2-mcp.md) | phase/p8-surface — T8.2 MCP server (`serve`/`server`), R5-verify | **CLOSED** (CLEAN, 2026-08-14) |
| [adve-review-t8.2-mcp-r2.md](adve-review-t8.2-mcp-r2.md) | phase/p8-surface — T8.2 re-verify at `596f40f` post-hardening | **CLOSED** (CLEAN, 0 P1 / 0 P2 / 0 P3, 2026-08-15) |
| [adve-review-t8.2-t8.3-live.md](adve-review-t8.2-t8.3-live.md) | Live-CockroachDB review — T8.2 + T8.3 (cluster `nrynss`, `b45c102`) | **LIVE** (2026-08-14; evidence in `evidence/live-review-cockroach/`) |
| [adve-review-t8.3-cli.md](adve-review-t8.3-cli.md) | phase/p8-surface — T8.3 CLI read+write verbs | **CLOSED** (CLEAN — R1 11 findings + R2 T83-12 closed, R3) |
| [adve-review-t8.3-cli-r2.md](adve-review-t8.3-cli-r2.md) | phase/p8-surface — T8.3 re-verify at `596f40f` | **CLOSED** (CLEAN, zero P1/P2, 2026-08-15) |
| [adve-review-t8.4-demo.md](adve-review-t8.4-demo.md) | phase/p8-surface — T8.4 two-agent demo scenario | **CLOSED** (FINDINGS; T84-2 FIXED, T84-1 live legs SUPERSEDED/closed 2026-08-15) |
| [adve-review-t8.5-web.md](adve-review-t8.5-web.md) | phase/p8-surface — T8.5 demo web app (`serve-web`) | **CLOSED** (findings remediated; reverify CLEAN + live Cockroach leg verified 2026-08-15) |
| [adve-review-t8.6-lease.md](adve-review-t8.6-lease.md) | phase/p8-surface — T8.6 single-writer lease | **CLOSED** (R2-VERIFY CLEAN; 3 P2 + 3 P3) |
| [adve-review-t8.6-lease-r2.md](adve-review-t8.6-lease-r2.md) | phase/p8-surface — T8.6 re-verify at HEAD post-L82 | **CLOSED** (CLEAN; T86R2-2 live leg SUPERSEDED/closed 2026-08-15) |
| [adve-review-t8.7-hardening.md](adve-review-t8.7-hardening.md) | phase/p8-surface — T8.7 MCP surface hardening | **CLOSED** (R1 FINDINGS → R2 reverify **CLEAN**, 2026-08-15) |
| [adve-review-t5.1-candidates.md](adve-review-t5.1-candidates.md) | T5.1 — Phase-1 candidates | **CLOSED** (ACCEPT, no findings) |
| [adve-review-t5.2-expand.md](adve-review-t5.2-expand.md) | T5.2 — Expansion | **CLOSED** (ACCEPT after 1 remediation round) |
| [adve-review-t5.3-assemble.md](adve-review-t5.3-assemble.md) | T5.3 — Assembly | **CLOSED** (ACCEPT; 3 P3 recorded, no remediation) |
| [adve-review-t5.4-cache.md](adve-review-t5.4-cache.md) | T5.4 — Recall cache | **CLOSED** (ACCEPT; 2 P3 doc-accuracy recorded) |
| [adve-review-p5-recall-GPT5.6sol.md](adve-review-p5-recall-GPT5.6sol.md) | P5 recall tier — GPT5.6sol (branch-level) | **CLOSED** (REQUEST CHANGES → remediated; superseded by the deep pass below) |
| [adve-review-p5-recall.md](adve-review-p5-recall.md) | P5 recall tier — branch-level | **CLOSED** (remediated, verification ACCEPT — merge-ready) |
| [adve-review-p5-deep.md](adve-review-p5-deep.md) | P5 recall tier — independent deep pass @ 5dcf7ad | **CLOSED** (read-only re-attack of the ACCEPT claim) |
| [adve-review-p6-canonization.md](adve-review-p6-canonization.md) | P6 serial close R1 @ 2cdb7a6 | **CLOSED** (REQUEST CHANGES: 2 P1; fixed 20f88a6) |
| [adve-review-p6-canonization-r2.md](adve-review-p6-canonization-r2.md) | P6 serial close R2 @ 8be251a | **CLOSED** (REQUEST CHANGES: 1 P2 budget edge; fixed b48ec05) |
| [adve-review-p6-canonization-r3.md](adve-review-p6-canonization-r3.md) | P6 serial close R3 @ a743350 | **CLOSED** (ACCEPT — clean) |
| **[adve-review-p6-canonization-fable.md](adve-review-p6-canonization-fable.md)** | phase/p6-canonization — P6 canonization tier (T6.1–T6.4), fable ×5 | **CLOSED** (19 findings: 2 P1 / 5 P2 / 12 P3; 2 opus remediation rounds, R3 verify CLEAN 2026-08-13; residual P3s recorded for P8) |
| [adve-review-p7-hybrid-vectors.md](adve-review-p7-hybrid-vectors.md) | P7 — hybrid vector recall | **CLOSED** (REMEDIATE 1 MAJOR + 4 MINOR → verified ACCEPT) |
| [adve-review-p7-t7-4-deepseekpro.md](adve-review-p7-t7-4-deepseekpro.md) | T7.4 — vector camera proof, deepseekpro | **CLOSED** (ACCEPT with findings: 1 MAJOR + 5 minor/nit, all remediated 9816ac9; NIT-6 carried as a T8.4 narration constraint) |
| **[adve-review-e2e-p0-p3-fable.md](adve-review-e2e-p0-p3-fable.md)** | Main — E2E P0–P3 (post-merge, whole-platform) | **CLOSED** (45 findings: 7 P1 / 15 P2 / 23 P3; all dispositioned 2026-08-12 in 8 waves — see the disposition record in the file) |
| **[adve-review-t14-fixtures.md](adve-review-t14-fixtures.md)** | Main — T1.4 fixture graphs | **CLOSED** (ACCEPT, residual notes only) |
| **[adve-review-t70-embeddings.md](adve-review-t70-embeddings.md)** | Main — T7.0 embeddings (BGE-M3 / llama.cpp) | **CLOSED** (ACCEPT, residual ops only) |
| [adve-review-t2.1-graph-structure.md](adve-review-t2.1-graph-structure.md) | T2.1 — Graph structure & invariants | **CLOSED** (ACCEPT; reverification audited 2026-08-11) |
| [adve-review-t2.2-canonicalization.md](adve-review-t2.2-canonicalization.md) | T2.2 — Canonicalization pipeline | **CLOSED** (ACCEPT, 2 rounds; round-2 clean) |
| [adve-review-t2.3-derive.md](adve-review-t2.3-derive.md) | T2.3 — `derive()` | **CLOSED** (ACCEPT after 2 review rounds) |
| [adve-review-t2.4-record-action.md](adve-review-t2.4-record-action.md) | T2.4 — `record_action()` + cycle check | **CLOSED** (ACCEPT, round 1, zero findings) |
| [adve-review-t2.5-demote.md](adve-review-t2.5-demote.md) | T2.5 — `demote()` | **CLOSED** (ACCEPT after 2 review rounds) |
| [adve-review-t2.6-inverted-index.md](adve-review-t2.6-inverted-index.md) | T2.6 — Inverted index + BM25 | **CLOSED** (ACCEPT, 2 rounds; round-2 clean) |
| [adve-review-t2.7-reservations.md](adve-review-t2.7-reservations.md) | T2.7 — Soft-lock reservations | **CLOSED** (ACCEPT after 2 review rounds) |
| [adve-review-t3.1-schema-ddl.md](adve-review-t3.1-schema-ddl.md) | T3.1 — Schema DDL, both dialects | **CLOSED** (ACCEPT after 2 review rounds) |
| [adve-review-t3.2-cockroach-store.md](adve-review-t3.2-cockroach-store.md) | T3.2 — `CockroachStore` | **CLOSED** (ACCEPT after 2 review rounds) |
| [adve-review-t3.3-sqlite-store.md](adve-review-t3.3-sqlite-store.md) | T3.3 — `SqliteStore` | **CLOSED** (ACCEPT after 2 review rounds) |
| [adve-review-t3.4-flush-task.md](adve-review-t3.4-flush-task.md) | T3.4 — Write-behind flush task | **CLOSED** (ACCEPT after 2 review rounds + polish) |
| [adve-review-t3.5-load-session.md](adve-review-t3.5-load-session.md) | T3.5 — `load_session()` / startup | **CLOSED** (ACCEPT after 2 review rounds) |
| [adve-review-t3.6-structural-queries.md](adve-review-t3.6-structural-queries.md) | T3.6 — Structural queries, both dialects | **CLOSED** (ACCEPT after 2 review rounds) |
| [adve-review-t4.1-scoring.md](adve-review-t4.1-scoring.md) | T4.1 — Scoring + daemon skeleton | **CLOSED** (ACCEPT after 1 remediation round) — *record reconstructed post-hoc (XP-2)* |
| [adve-review-t4.2-hotlist.md](adve-review-t4.2-hotlist.md) | T4.2 — Hot list | **CLOSED** (ACCEPT) — *record reconstructed post-hoc (XP-2)* |
| [adve-review-t4.3-conflict.md](adve-review-t4.3-conflict.md) | T4.3 — Conflict detection (demo trigger) | **CLOSED** (ACCEPT) — *record reconstructed post-hoc (XP-2)* |
| [adve-review-t4.4-drift.md](adve-review-t4.4-drift.md) | T4.4 — Drift detection + `set_root_goal` | **CLOSED** (ACCEPT) — *record reconstructed post-hoc (XP-2)* |
| [adve-review-t4.5-gc.md](adve-review-t4.5-gc.md) | T4.5 — GC (canonization's food) | **CLOSED** (ACCEPT) — *record reconstructed post-hoc (XP-2)* |
| [adve-review-t4.6-events.md](adve-review-t4.6-events.md) | T4.6 — Event channel + loop wiring | **CLOSED** (ACCEPT after 1 remediation round + a final pass) — *record reconstructed post-hoc (XP-2)* |
| [adve-review-p2-graph-core-grok.md](adve-review-p2-graph-core-grok.md) | P2 graph core — grok (adversarial, branch-level, independent) | **CLOSED** (all findings dispositioned) |
| [adve-review-p2-graph-core-muse-spark.md](adve-review-p2-graph-core-muse-spark.md) | P2 graph core — muse-spark (adversarial, branch-level) | **CLOSED** (all findings dispositioned) |
| [adve-review-p3-stores-gemini36flash.md](adve-review-p3-stores-gemini36flash.md) | P3 stores tier — gemini36flash | **CLOSED** (ACCEPT after shared remediation + review) |
| [adve-review-p3-stores-opus46.md](adve-review-p3-stores-opus46.md) | P3 stores tier — opus46 | **CLOSED** (ACCEPT after shared remediation + review) |
| **[adve-review-p4-daemon-opus.md](adve-review-p4-daemon-opus.md)** | Main — P4 daemon tier (T4.1–T4.6), opus ×3 | **CLOSED** — 25+7 findings remediated (R1 8 waves + R2 7 commits), verification ACCEPT; merge-ready |
| [adve-review-p7-t7-0-embedder-self.md](adve-review-p7-t7-0-embedder-self.md) | Prior/self T7.0 pass (history) | historical |
| [adve-review-p0-p1-foundation-self.md](adve-review-p0-p1-foundation-self.md) | Prior/self P0/P1 foundation review | historical |

Convention:

- **`*-self.md`** — completed narrower or historical review  
- **Main** reviews use a clear task id (`t70` = T7.0)  
- **CLOSED** means ACCEPT/REJECT is final for that task; reopen only on regression criteria in the review body
- ***record reconstructed post-hoc*** — the review happened but no record was
  committed at the time; the file was rebuilt from the phase doc, commit history
  and code on 2026-08-12 as remediation for **XP-2**. Each such record marks
  what is evidence and what is unrecoverable. Treat them as weaker than a
  contemporaneous record: what is genuinely lost is the **reviewer's prose, the
  finding severities, and the gate output of the round itself**. What they *do*
  carry is re-derivable and checked: merge SHAs, status/board lines quoted
  verbatim, and test counts recounted from the merge commit. Any tier-level pass/
  fail figure they quote was measured *after* the task merged and is attributed to
  its source, not presented as the round's own.

Count discipline: the header count above must equal the number of table rows.
XP-2 was found because the previous header claimed completeness while six T4.x
records did not exist.

# Adversarial reviews

Complete index of the review records in this directory (22 records).

| File | Scope | Status |
|------|-------|--------|
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
| [adve-review-p2-graph-core-grok.md](adve-review-p2-graph-core-grok.md) | P2 graph core — grok (adversarial, branch-level, independent) | **CLOSED** (all findings dispositioned) |
| [adve-review-p2-graph-core-muse-spark.md](adve-review-p2-graph-core-muse-spark.md) | P2 graph core — muse-spark (adversarial, branch-level) | **CLOSED** (all findings dispositioned) |
| [adve-review-p3-stores-gemini36flash.md](adve-review-p3-stores-gemini36flash.md) | P3 stores tier — gemini36flash | **CLOSED** (ACCEPT after shared remediation + review) |
| [adve-review-p3-stores-opus46.md](adve-review-p3-stores-opus46.md) | P3 stores tier — opus46 | **CLOSED** (ACCEPT after shared remediation + review) |
| [adve-review-p7-t7-0-embedder-self.md](adve-review-p7-t7-0-embedder-self.md) | Prior/self T7.0 pass (history) | historical |
| [adve-review-p0-p1-foundation-self.md](adve-review-p0-p1-foundation-self.md) | Prior/self P0/P1 foundation review | historical |

Convention:

- **`*-self.md`** — completed narrower or historical review  
- **Main** reviews use a clear task id (`t70` = T7.0)  
- **CLOSED** means ACCEPT/REJECT is final for that task; reopen only on regression criteria in the review body

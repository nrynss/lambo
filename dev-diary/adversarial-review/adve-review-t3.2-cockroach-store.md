# Adversarial Review: T3.2 — CockroachStore

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                          ║
║  Disposition: ACCEPT after 2 review rounds               ║
║  Opened: 2026-08-11                                      ║
║  Closed: 2026-08-11                                      ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T3.2 — CockroachStore (spec §3.2/§3.3, §4)
**Scope:** `src/store/cockroach.rs`, store/mod.rs registry arms, `migrations/cockroach/001_init.sql`
**Implementer:** T32Cockroach (`0e76a9f`); remediation `a51d21a`
**Reviewer:** ReviewT32Cockroach (round 1), Review2T32Cockroach (round 2)

## Round 1 — findings

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| R1 | P2 | `keyword_candidates` scoring loop counted hits on RAW row strings while the SQL predicate lowercases → mixed-case rows selected but scored 0.0, diverging from MemoryStore and its own predicate | **Fixed** (`a51d21a`): `score_keyword_hits()` lowercases content+canonical_key before counting (MemoryStore parity); regression `keyword_score_folds_case_like_memory_store` + live `check_keyword_mixed_case_ranks_like_memory_store` prove "Register User" scores 2.0 (was 0.0) and ranks identically to MemoryStore |
| R2 | coverage | No dedicated legal-demote flush test locking the partial-index path | **Fixed** (`a51d21a`): `check_legal_demote_flush_partial_index` — two same-key Observations flush + survive reload; duplicate-key Entity fails. **The test exposed a real schema bug**: pre-errata clusters carry the auto-named table-level UNIQUE (`concepts_session_id_canonical_key_key`) rejecting legal demotes — fixed via idempotent `ALTER TABLE ... DROP CONSTRAINT IF EXISTS` in the migration (fresh installs no-op) |

## Round 2 — verified clean

Verdict ACCEPT, no findings. Verified (reviewer observed the live run): 227 lib passed / 0 failed / 0 warnings, conformance suite 19.9s on the real cluster, 0 SKIPs; chunk_group_id survives flush→load (`Some("chunk-42")`, NULL stays None, would-fail-if-dropped on both INSERT and SELECT); embedding-contract columns read on load (typed Backend error if kind set but dim NULL), flush immunity structural (sessions upsert ON CONFLICT DO NOTHING) and live-asserted; migration idempotency proven by init_schema ×2 on a pre-existing cluster; remediation change set = cockroach.rs + cockroach migration + phase doc only.

## Notable decisions recorded (handoff)

- Submission-order batch replay (T2.1 M2 contract — never re-group by §2.4 kind).
- VECTOR: text bind + `$n::VECTOR` cast (T0.3 spike), `<->` L2, score = cosine from L2 (1 − d²/2, clamped) comparable to `semantic_match_threshold` 0.85; dim parsed from the embedded DDL (`Some(1024)`), not a global constant.
- Client-side 40001 serializable-conflict retry (×5, 50–200ms backoff).
- Lazy pool (sync `build_store`); rustls DSN rewrite (`sslrootcert=system` → real CA); sessions rows ensured on flush (REFERENCES enforcement); canonization insert `ON CONFLICT (id) DO NOTHING` (retried-flush safe).
- Spec errata (Main): chunk_group_id + embedding columns + §4.1 span both-timestamp pin.

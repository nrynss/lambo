# Adversarial Review: T3.3 — SqliteStore

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                          ║
║  Disposition: ACCEPT after 2 review rounds               ║
║  Opened: 2026-08-11                                      ║
║  Closed: 2026-08-11                                      ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T3.3 — SqliteStore (spec §3.2/§3.3, §4)
**Scope:** `src/store/sqlite.rs`, store/mod.rs registry arms, `migrations/sqlite/001_init.sql`
**Implementer:** T33Sqlite (`9d95230` + `fa528ca`); remediation `a5fdbae`
**Reviewer:** ReviewT33Sqlite (round 1), Review2T33Sqlite (round 2)

## Round 1 — findings

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| R1 | INFO | keyword_candidates is ASCII-only case folding (SQLite `lower()`); MemoryStore is Unicode — divergence on non-ASCII | **Fixed** (`a5fdbae`): ASCII-only scope note in module doc |
| R2 | INFO | Same-instant load tie-break ((occurred_at, id) / (created_at, id)) can diverge from MemoryStore insertion order | **Fixed** (`a5fdbae`): deterministic SQL order documented; equality by value, not position |
| R3 | INFO | Possible raw-contains scoring bug class (as T3.2 R1) | **Verified NO bug**: scoring is entirely in SQL (`instr(lower(...))`); added mixed-case regression ("Register User" scores 1.0 and deep-equals MemoryStore) that would fail under the raw-contains bug |

## Round 2 — verified clean

Verdict ACCEPT, no findings. Verified: chunk_group_id persists end-to-end (inline CREATE TABLE + `pragma_table_info`-guarded ALTER for pre-existing DBs — the brief's `ADD COLUMN IF NOT EXISTS` assumption is WRONG for SQLite, empirically disproved and worked around; upsert binds it; round-trip asserts `Some("chunk-1")` survival, would fail if dropped); embedding-contract columns with corruption checks (kind+dim → Some, kind XOR dim → error, both absent → None, negative dim rejected); idempotency convergence test (old T3.1-shape DDL through 2× init_schema, columns asserted, "legacy-chunk" round-trips); pinned contract intact (batch-order replay, timestamps/ON CONFLICT targets, three-way structural agreement, demote-duplicate tests). `cargo test --features store-sqlite`: 223 passed / 0 failed / 0 warnings, stable ×3.

## Notable decisions recorded (handoff)

- Lazy `OnceLock` pool (sync `build_store`); cross-runtime connection-return quirk (current-thread runtime + blocked acquire can time out — production multi-thread unaffected).
- Concepts upsert `ON CONFLICT (id)`; the partial unique index is a separate constraint, NOT targeted (bare target errors — T3.1 verified); edges `ON CONFLICT (source, target, edge_type) DO UPDATE SET id=excluded.id` (MemoryStore natural-key preference; graph core preserves id/created_at on reinforcement).
- Fixed ms timestamps (`to_rfc3339_opts(SecondsFormat::Millis, true)`); age cutoffs computed in Rust and bound as TEXT (lex-valid).
- Structural queries MemoryStore-exact (both-timestamp span gating + `c.id <> $node`); spec errata pinned by Main.
- Not persisted by design (S5): root_goal/created_at/closed_at, synonyms/reservations, embedding contract metadata (columns added; flush never writes them).

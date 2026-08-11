# Adversarial Review: T3.1 — Schema DDL, both dialects

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                          ║
║  Disposition: ACCEPT after 2 review rounds               ║
║  Opened: 2026-08-11                                      ║
║  Closed: 2026-08-11                                      ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T3.1 — Schema DDL (spec §4 + partial-UNIQUE errata)
**Scope:** `migrations/cockroach/001_init.sql` (modified), `migrations/sqlite/001_init.sql` (new)
**Implementer:** T31Schema (`c2c3c86`); remediation `4a5611d`
**Reviewer:** ReviewT31Schema (round 1), Review2T31Schema (round 2)

## Round 1 — findings

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| R1 | SHOULD | SQLite upserts against the partial unique index MUST spell the conflict target with the exact WHERE clause: `ON CONFLICT (session_id, canonical_key) WHERE concept_type <> 'Observation'` — a bare target errors ("does not match any PRIMARY KEY or UNIQUE constraint") since the table-level UNIQUE was removed | **Fixed** (`4a5611d`): documented in the T3.3 notes bullet, error quoted, verified on SQLite 3.53.4 |
| R2 | INFO | chrono's default `to_rfc3339()` emits `+00:00` + variable-width fractional seconds, breaking SQL lex comparisons against the documented ISO-8601 shape | **Fixed** (`4a5611d`): handoff mandates `to_rfc3339_opts(SecondsFormat::Millis, true)` (24-char `...SSS.Z`) and forbids the default |

## Round 2 — verified clean

Verdict ACCEPT, no findings. Verified: cockroach diff vs base = exactly the partial-UNIQUE errata (1 del + 11 ins, nothing else drifted; all 7 tables + VECTOR(1024) + vector index + edges UNIQUE + IF NOT EXISTS intact); SQLite translation faithful to spec §4 with every divergence documented in the file header; idempotency (ran twice, exit 0); duplicate Entity key rejected AND duplicate Observations accepted (partial-UNIQUE semantics proven on sqlite3 3.53.4); timestamp lex-ordering verified; migrations byte-identical between rounds; remediation doc-only.

## Notable decisions recorded (handoff)

- SQLite: STRING/UUID/TIMESTAMPTZ/JSONB→TEXT (ISO-8601 UTC), INT→INTEGER, FLOAT→REAL, now()→`strftime('%Y-%m-%dT%H:%M:%fZ','now')` (fixed-width, lex-comparable), VECTOR(1024)→BLOB unused, no vector index, INDEX clauses as separate CREATE INDEX IF NOT EXISTS, REFERENCES kept (PRAGMA foreign_keys=ON is the adapter's job).
- Cockroach live-cluster behavior (partial index + vector-index IF NOT EXISTS re-runs) is T3.2 conformance's verification (offline here).
- Adapters MUST write timestamps with a fixed serialization (`to_rfc3339_opts(SecondsFormat::Millis, true)`).

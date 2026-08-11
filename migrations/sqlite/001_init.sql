-- Lambo v0.1 schema (spec §4), SQLite translation. Idempotent: every statement
-- is IF NOT EXISTS, so this file runs twice cleanly. Executed by
-- SqliteStore::init_schema().
--
-- Post-T3.1 columns (P3 wave 2 remediation) are part of the CREATE TABLEs
-- below, so fresh databases carry them inline. SQLite has NO
-- `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` (verified: 3.53.4 rejects the
-- syntax), so pre-existing databases are converged by SqliteStore::init_schema
-- instead: after executing this file it inspects `pragma_table_info` per
-- column and issues a plain `ALTER TABLE ... ADD COLUMN` only when the column
-- is missing. This file itself therefore stays idempotent (every statement
-- IF NOT EXISTS) and runs twice cleanly on any database.
--
-- Dialect mapping (spec §4 Cockroach types -> SQLite):
--   STRING        -> TEXT
--   UUID          -> TEXT (canonical 36-char lowercase form)
--   TIMESTAMPTZ   -> TEXT, ISO-8601 UTC, e.g. '2026-08-11T18:00:00.123Z'.
--                    Choice: a fixed ISO-8601 UTC string is unambiguous,
--                    chrono-parsable, and sortable (RFC 3339 ordering
--                    equals lexicographic ordering), so the adapter can
--                    compare timestamps directly in SQL.
--   JSONB         -> TEXT (serialized JSON; adapter marshals/unmarshals)
--   INT           -> INTEGER
--   FLOAT         -> REAL
--   VECTOR(1024)  -> BLOB. UNUSED: SQLite has no VECTOR_SEARCH capability;
--                    adapters never read or write this column.
--   now() default -> strftime('%Y-%m-%dT%H:%M:%fZ','now') as the ISO-8601 UTC
--                    equivalent of Cockroach's now(). (Plain SQLite
--                    CURRENT_TIMESTAMP would yield 'YYYY-MM-DD HH:MM:SS' UTC;
--                    strftime is used instead to keep the documented
--                    TIMESTAMPTZ format uniform and add millisecond
--                    precision.)
--
-- No vector index: SQLite has no VECTOR_SEARCH. All spec INDEX clauses are
-- separate CREATE INDEX IF NOT EXISTS statements below. Table-level UNIQUE /
-- PRIMARY KEY constraints stay inline — SQLite autoindexes them, which is
-- what ON CONFLICT targets require. REFERENCES clauses are kept for schema
-- fidelity; SQLite enforces them only when the connection sets
-- PRAGMA foreign_keys = ON (the adapter's job).
--
-- Partial UNIQUE index (spec §4 errata, muse-spark M1-M2): canonical-key
-- uniqueness is partial — non-Observation concepts only. Demoted Observations
-- (spec §7) skip the match step and may legitimately share a canonical key
-- (identical sentences from different chunks are distinct context-overflow
-- records). SQLite supports partial indexes.

CREATE TABLE IF NOT EXISTS sessions (
    session_id      TEXT PRIMARY KEY,
    root_goal       TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    closed_at       TEXT,
    embedding_kind  TEXT,   -- session embedding contract (S5 read-only; no write path yet)
    embedding_model TEXT,
    embedding_dim   INTEGER
);

CREATE TABLE IF NOT EXISTS interactions (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(session_id),
    agent_id        TEXT NOT NULL,
    prompt_text     TEXT,
    previous_id     TEXT REFERENCES interactions(id),
    created_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS concepts (
    id                  TEXT PRIMARY KEY,
    session_id          TEXT NOT NULL REFERENCES sessions(session_id),
    content             TEXT NOT NULL,
    canonical_key       TEXT NOT NULL,
    concept_type        TEXT NOT NULL,
    origin_interaction  TEXT NOT NULL REFERENCES interactions(id),
    origin_agent        TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    access_count        INTEGER NOT NULL DEFAULT 0,
    last_accessed       TEXT,
    gc_survived         INTEGER NOT NULL DEFAULT 0,
    canonization_status TEXT NOT NULL DEFAULT 'None',
    blast_radius        INTEGER,
    last_demotion_time  TEXT,
    embedding           BLOB,   -- unused: SQLite has no VECTOR_SEARCH; adapters never read/write it
    chunk_group_id      TEXT    -- T2.5 demote sibling co-retrieval key (spec §7/§8, read by T5.2)
);

-- Partial unique index on canonical keys (spec §4 errata): non-Observation only.
CREATE UNIQUE INDEX IF NOT EXISTS concepts_key_non_obs_idx
    ON concepts (session_id, canonical_key)
    WHERE concept_type <> 'Observation';

CREATE TABLE IF NOT EXISTS edges (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(session_id),
    source          TEXT NOT NULL,
    target          TEXT NOT NULL,
    edge_type       TEXT NOT NULL,
    weight          REAL NOT NULL,
    reinforcements  INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL,
    last_reinforced TEXT NOT NULL,
    UNIQUE (source, target, edge_type)
);

CREATE TABLE IF NOT EXISTS synonyms (
    session_id      TEXT NOT NULL REFERENCES sessions(session_id),
    source_key      TEXT NOT NULL,
    canonical_key   TEXT NOT NULL,
    PRIMARY KEY (session_id, source_key)
);

CREATE TABLE IF NOT EXISTS canonization_events (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    node_id         TEXT NOT NULL,
    from_status     TEXT NOT NULL,
    to_status       TEXT NOT NULL,
    blast_radius    INTEGER,
    last_demotion_time TEXT,
    occurred_at     TEXT NOT NULL
);

-- Soft locks (S5): an expired row persists until a later write overwrites it,
-- so external SQL readers MUST filter
-- `WHERE expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')` — never read the
-- table without the expiry predicate.
CREATE TABLE IF NOT EXISTS reservations (
    session_id      TEXT NOT NULL,
    node_id         TEXT NOT NULL,
    agent_id        TEXT NOT NULL,
    expires_at      TEXT NOT NULL,
    PRIMARY KEY (session_id, node_id)
);

-- Spec §4 INDEX clauses, as separate statements (order preserved):
CREATE INDEX IF NOT EXISTS interactions_session_created_idx ON interactions (session_id, created_at);
CREATE INDEX IF NOT EXISTS concepts_session_status_idx ON concepts (session_id, canonization_status);
CREATE INDEX IF NOT EXISTS edges_session_target_type_idx ON edges (session_id, target, edge_type);
CREATE INDEX IF NOT EXISTS edges_session_source_type_idx ON edges (session_id, source, edge_type);
CREATE INDEX IF NOT EXISTS canonization_events_session_time_idx ON canonization_events (session_id, occurred_at);

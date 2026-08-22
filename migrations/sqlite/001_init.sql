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
--   VECTOR(1024)  -> BLOB. Written and read for flush->load round-trip parity
--                    (CON-8) as the shared '[x,y,z]' text form; SQLite has no
--                    VECTOR_SEARCH capability, so the column is never queried.
--   now() default -> strftime('%Y-%m-%dT%H:%M:%fZ','now') as the ISO-8601 UTC
--                    equivalent of Cockroach's now(). (Plain SQLite
--                    CURRENT_TIMESTAMP would yield 'YYYY-MM-DD HH:MM:SS' UTC;
--                    strftime is used instead to keep the documented
--                    TIMESTAMPTZ format uniform and add millisecond
--                    precision.)
--
-- No vector index: VECTOR_SEARCH is served by an exact cosine scan over
-- `concepts.embedding` in the adapter (F1, issue #5), not by an index. An ANN
-- index would mean sqlite-vec — a C toolchain across four cross-compiled release
-- targets plus auto-extension registration before sqlx opens a pool — bought
-- against a latency number nobody has measured; see store/sqlite.rs, "The scan is
-- a seam", for the replacement point and the trigger to revisit.
-- All spec INDEX clauses are separate CREATE INDEX IF NOT EXISTS statements
-- below. Table-level UNIQUE / PRIMARY KEY constraints stay inline — SQLite
-- autoindexes them, which is what ON CONFLICT targets require. REFERENCES
-- clauses are kept for schema fidelity; SQLite enforces them only when the
-- connection sets PRAGMA foreign_keys = ON (the adapter's job).
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
    embedding_kind  TEXT,   -- session embedding contract (S5; written by seed, read by load_session)
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
    embedding           BLOB,   -- shared text form (CON-8); width-agnostic, scanned by vector_candidates_checked
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
-- Single-writer lease (spec §2.2, T8.6): one row per session, holder =
-- 'agent@host#pid', timestamps stamped from the store clock. `current_token`
-- is the monotonic fencing token (issue #1): minted on takeover, PRESERVED on a
-- same-holder refresh, 0 = never leased (seed/fixture parity bypass). The
-- adapter's acquire is a single INSERT ... ON CONFLICT guarded by an expiry
-- check, so two processes opening the same DB file serialize on this row.
-- `endpoint` (J2) is where the holder can be reached — a unix socket path for a
-- `lambo serve` holder, NULL for every other writer (a CLI verb holds the lease
-- for one command and is not proxyable, so NULL means "no hub here", not
-- missing data). It is written by the same acquire that takes the lease, so a
-- live row always carries the current holder's address.
-- External SQL readers of a *live* holder MUST filter
-- `WHERE expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')` — an expired row
-- persists until the next acquire overwrites it. Operator override (force a
-- takeover from a wedged-but-heartbeating holder):
--   DELETE FROM session_leases WHERE session_id = '<session>';
CREATE TABLE IF NOT EXISTS session_leases (
    session_id  TEXT PRIMARY KEY,
    holder      TEXT NOT NULL,
    acquired_at TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    current_token INTEGER NOT NULL DEFAULT 0,
    endpoint    TEXT
);

-- J4 lease refusals (dev-diary/lambo-for-mooshik/J-multi-client.md §J4): a
-- writer that was refused the single-writer lease records the refusal here so
-- the incumbent holder can learn it turned away a takeover ("from both sides").
-- refused_at is the STORE clock (F18 — never a caller instant). The incumbent
-- polls these and appends its own ledger line; rows are read by the poller and
-- need no retention (each poller dedups by refused_by+refused_at).
CREATE TABLE IF NOT EXISTS lease_refusals (
    session_id     TEXT NOT NULL,
    refused_at     TEXT NOT NULL,
    refused_by     TEXT NOT NULL,
    current_holder TEXT NOT NULL
);

-- Flush stats published by the writer's FlushTask (T85-3): one row per
-- session, upserted after each flush cycle so a reader in another process can
-- render real flush_lag_ms / log_depth instead of n/a. Writers WRITE, readers
-- READ. An absent row means "no writer has published yet" (the honest n/a).
-- updated_at follows the SQLite TIMESTAMPTZ-as-TEXT convention.
CREATE TABLE IF NOT EXISTS session_stats (
    session_id   TEXT PRIMARY KEY,
    flush_lag_ms INTEGER NOT NULL,
    log_depth    INTEGER NOT NULL,
    updated_at   TEXT NOT NULL
);

-- J3 durable write intents (dev-diary/lambo-for-mooshik/J3-durability-redesign.md):
-- a validated, acked background write that has not yet been applied. Written
-- at ack time through the write-behind log, so the close-time final flush
-- carries it — at a clean close every acked write is either applied or here.
-- An unconsumed row (consumed_at IS NULL) is owed a replay by the next serve
-- of the session; a consumed row is retained for the receipt-retention window
-- (300 s) so a restarted session can answer applied_after_restart, then purged
-- by the adapter's consume step. payload is the serialized WriteIntentPayload
-- JSON; receipt is the ReceiptId display form and the idempotency key.
CREATE TABLE IF NOT EXISTS write_intents (
    session_id      TEXT NOT NULL REFERENCES sessions(session_id),
    receipt         TEXT NOT NULL,
    agent           TEXT NOT NULL,
    interaction_id  TEXT NOT NULL,
    lane_seq        INTEGER NOT NULL,
    issued_ms       INTEGER NOT NULL,
    payload         TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    consumed_at     TEXT,
    outcome_tag     TEXT,
    outcome_summary TEXT,
    PRIMARY KEY (session_id, receipt)
);

-- Spec §4 INDEX clauses, as separate statements (order preserved):
CREATE INDEX IF NOT EXISTS interactions_session_created_idx ON interactions (session_id, created_at);
CREATE INDEX IF NOT EXISTS concepts_session_status_idx ON concepts (session_id, canonization_status);
CREATE INDEX IF NOT EXISTS edges_session_target_type_idx ON edges (session_id, target, edge_type);
CREATE INDEX IF NOT EXISTS edges_session_source_type_idx ON edges (session_id, source, edge_type);
CREATE INDEX IF NOT EXISTS canonization_events_session_time_idx ON canonization_events (session_id, occurred_at);

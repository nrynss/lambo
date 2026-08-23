-- Lambo v0.1 schema (spec §4), PostgreSQL + pgvector translation.
--
-- THIS FILE IS NOT VALID SQL AS SHIPPED. The dense-vector width is a
-- placeholder (the unique token substituted by PostgresDialect::init_sql
-- before execute). Applying this file with psql without substitution
-- fails loudly (the placeholder is not a number).
-- That is the documented cost of keeping a reviewable schema file while
-- inverting Cockroach's parse-out data flow: Cockroach reads VECTOR(n)
-- out of a static file; Postgres writes vector(n) in.
--
-- Idempotent: every statement is IF NOT EXISTS (or DROP IF EXISTS), so
-- init_schema can re-run. Executed by PostgresStore::init_schema via
-- sqlx raw_sql, then two convergence ALTERs from
-- PostgresDialect::post_init_statements (endpoint TEXT, current_token
-- BIGINT). Those two stay out of this file so the shared init_schema
-- shape stays raw_sql + two query() calls, matching Cockroach.
--
-- Type mapping (Cockroach -> PostgreSQL):
--   STRING        -> TEXT
--   INT           -> BIGINT   (Cockroach INT is INT8 on the wire; the
--                              shared decoder reads i64. PostgreSQL INT
--                              is int4 and would break those reads.)
--   FLOAT         -> FLOAT    (float8 on both)
--   VECTOR(n)     -> vector(n)  (pgvector; n substituted at init)
--   UUID, TIMESTAMPTZ, JSONB stay.
-- Inline INDEX clauses are Cockroach-only; they become CREATE INDEX
-- IF NOT EXISTS below (same shape SQLite already uses).
--
-- hnsw from init (B2, recorded 2026-08-19): the index is created in this
-- same file, not by a later migration. ivfflat is rejected: it clusters
-- at build time and Lambo's tables start empty. Index parameters are
-- pgvector defaults (m=16, ef_construction=64; ef_search=40 is a GUC,
-- not an index option, and is left at the default: no knobs yet).
--
-- The hnsw ceiling on type vector is 2000 dimensions. That bound is
-- enforced in Rust (PostgresDialect::init_sql) so CREATE INDEX is never
-- the thing that discovers it. The halfvec hatch is named in that error
-- and is not implemented here.

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS sessions (
    session_id      TEXT PRIMARY KEY,
    root_goal       JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at       TIMESTAMPTZ,
    embedding_kind  TEXT,
    embedding_model TEXT,
    embedding_dim   BIGINT
);

ALTER TABLE sessions ADD COLUMN IF NOT EXISTS embedding_kind TEXT;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS embedding_model TEXT;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS embedding_dim BIGINT;

CREATE TABLE IF NOT EXISTS interactions (
    id              UUID PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(session_id),
    agent_id        TEXT NOT NULL,
    prompt_text     TEXT,
    previous_id     UUID REFERENCES interactions(id),
    created_at      TIMESTAMPTZ NOT NULL,
    event_time      TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS interactions_session_created_idx
    ON interactions (session_id, created_at);

CREATE TABLE IF NOT EXISTS concepts (
    id                  UUID PRIMARY KEY,
    session_id          TEXT NOT NULL REFERENCES sessions(session_id),
    content             TEXT NOT NULL,
    canonical_key       TEXT NOT NULL,
    concept_type        TEXT NOT NULL,
    origin_interaction  UUID NOT NULL REFERENCES interactions(id),
    origin_agent        TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL,
    access_count        BIGINT NOT NULL DEFAULT 0,
    last_accessed       TIMESTAMPTZ,
    gc_survived         BIGINT NOT NULL DEFAULT 0,
    canonization_status TEXT NOT NULL DEFAULT 'None',
    blast_radius        BIGINT,
    last_demotion_time  TIMESTAMPTZ,
    embedding           vector(__LAMBO_VECTOR_DIM__),
    chunk_group_id      TEXT,
    human_confirmed     BIGINT NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS concepts_session_status_idx
    ON concepts (session_id, canonization_status);

ALTER TABLE concepts ADD COLUMN IF NOT EXISTS chunk_group_id TEXT;

ALTER TABLE concepts ADD COLUMN IF NOT EXISTS human_confirmed BIGINT NOT NULL DEFAULT 0;

-- Spec §4 errata: canonical-key uniqueness is partial (non-Observation
-- only). Demoted Observations may share a key. The DROP covers a
-- table-level UNIQUE a pre-errata cluster might still carry.
CREATE UNIQUE INDEX IF NOT EXISTS concepts_key_non_obs_idx
    ON concepts (session_id, canonical_key)
    WHERE concept_type <> 'Observation';
ALTER TABLE concepts DROP CONSTRAINT IF EXISTS concepts_session_id_canonical_key_key;

-- Partial: NULL embeddings must not enter the index. The recall query
-- filters embedding IS NOT NULL and decodes dist as f64; a NULL dist
-- hard-errors the whole query. vector_cosine_ops matches DISTANCE_OP
-- `<=>` (cosine distance). Do not use vector_l2_ops: that is Cockroach's
-- metric in pgvector clothes.
CREATE INDEX IF NOT EXISTS concepts_embedding_idx
    ON concepts
    USING hnsw (embedding vector_cosine_ops)
    WHERE embedding IS NOT NULL;

CREATE TABLE IF NOT EXISTS edges (
    id              UUID PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(session_id),
    source          UUID NOT NULL,
    target          UUID NOT NULL,
    edge_type       TEXT NOT NULL,
    weight          FLOAT NOT NULL,
    reinforcements  BIGINT NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL,
    event_time      TIMESTAMPTZ,
    last_reinforced TIMESTAMPTZ NOT NULL,
    UNIQUE (source, target, edge_type)
);

CREATE INDEX IF NOT EXISTS edges_session_target_type_idx
    ON edges (session_id, target, edge_type);
CREATE INDEX IF NOT EXISTS edges_session_source_type_idx
    ON edges (session_id, source, edge_type);

ALTER TABLE interactions ADD COLUMN IF NOT EXISTS event_time TIMESTAMPTZ;
ALTER TABLE edges ADD COLUMN IF NOT EXISTS event_time TIMESTAMPTZ;

CREATE TABLE IF NOT EXISTS synonyms (
    session_id      TEXT NOT NULL REFERENCES sessions(session_id),
    source_key      TEXT NOT NULL,
    canonical_key   TEXT NOT NULL,
    PRIMARY KEY (session_id, source_key)
);

CREATE TABLE IF NOT EXISTS canonization_events (
    id              UUID PRIMARY KEY,
    session_id      TEXT NOT NULL,
    node_id         UUID NOT NULL,
    from_status     TEXT NOT NULL,
    to_status       TEXT NOT NULL,
    blast_radius    BIGINT,
    last_demotion_time TIMESTAMPTZ,
    occurred_at     TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS canonization_events_session_time_idx
    ON canonization_events (session_id, occurred_at);

ALTER TABLE canonization_events ADD COLUMN IF NOT EXISTS last_demotion_time TIMESTAMPTZ;

CREATE TABLE IF NOT EXISTS reservations (
    session_id      TEXT NOT NULL,
    node_id         UUID NOT NULL,
    agent_id        TEXT NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (session_id, node_id)
);

CREATE TABLE IF NOT EXISTS session_leases (
    session_id  TEXT PRIMARY KEY,
    holder      TEXT NOT NULL,
    acquired_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,
    current_token BIGINT NOT NULL DEFAULT 0,
    endpoint    TEXT
);

CREATE TABLE IF NOT EXISTS lease_refusals (
    session_id     TEXT NOT NULL,
    refused_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    refused_by     TEXT NOT NULL,
    current_holder TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS lease_refusals_session_time_idx
    ON lease_refusals (session_id, refused_at);

CREATE TABLE IF NOT EXISTS write_intents (
    session_id      TEXT NOT NULL REFERENCES sessions(session_id),
    receipt         TEXT NOT NULL,
    agent           TEXT NOT NULL,
    interaction_id  UUID NOT NULL,
    lane_seq        BIGINT NOT NULL,
    issued_ms       BIGINT NOT NULL,
    payload         TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL,
    consumed_at     TIMESTAMPTZ,
    outcome_tag     TEXT,
    outcome_summary TEXT,
    PRIMARY KEY (session_id, receipt)
);

CREATE TABLE IF NOT EXISTS session_stats (
    session_id   TEXT PRIMARY KEY,
    flush_lag_ms BIGINT NOT NULL,
    log_depth    BIGINT NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Lambo v0.1 schema (spec §4). Idempotent for re-runs of scripts/provision.sh.
-- CockroachDB-specific: VECTOR(1024), CREATE VECTOR INDEX.

CREATE TABLE IF NOT EXISTS sessions (
    session_id      STRING PRIMARY KEY,
    root_goal       JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at       TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS interactions (
    id              UUID PRIMARY KEY,
    session_id      STRING NOT NULL REFERENCES sessions(session_id),
    agent_id        STRING NOT NULL,
    prompt_text     STRING,
    previous_id     UUID REFERENCES interactions(id),
    created_at      TIMESTAMPTZ NOT NULL,
    INDEX (session_id, created_at)
);

CREATE TABLE IF NOT EXISTS concepts (
    id                  UUID PRIMARY KEY,
    session_id          STRING NOT NULL REFERENCES sessions(session_id),
    content             STRING NOT NULL,
    canonical_key       STRING NOT NULL,
    concept_type        STRING NOT NULL,
    origin_interaction  UUID NOT NULL REFERENCES interactions(id),
    origin_agent        STRING NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL,
    access_count        INT NOT NULL DEFAULT 0,
    last_accessed       TIMESTAMPTZ,
    gc_survived         INT NOT NULL DEFAULT 0,
    canonization_status STRING NOT NULL DEFAULT 'None',
    blast_radius        INT,
    last_demotion_time  TIMESTAMPTZ,
    embedding           VECTOR(1024),
    UNIQUE (session_id, canonical_key),
    INDEX (session_id, canonization_status)
);

-- Vector index: may require feature.vector_index.enabled on some plans.
-- IF NOT EXISTS is supported on CockroachDB vector indexes in recent versions;
-- provision.sh also tolerates "already exists" errors.
CREATE VECTOR INDEX IF NOT EXISTS concepts_embedding_idx ON concepts (embedding);

CREATE TABLE IF NOT EXISTS edges (
    id              UUID PRIMARY KEY,
    session_id      STRING NOT NULL REFERENCES sessions(session_id),
    source          UUID NOT NULL,
    target          UUID NOT NULL,
    edge_type       STRING NOT NULL,
    weight          FLOAT NOT NULL,
    reinforcements  INT NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL,
    last_reinforced TIMESTAMPTZ NOT NULL,
    UNIQUE (source, target, edge_type),
    INDEX (session_id, target, edge_type),
    INDEX (session_id, source, edge_type)
);

CREATE TABLE IF NOT EXISTS synonyms (
    session_id      STRING NOT NULL REFERENCES sessions(session_id),
    source_key      STRING NOT NULL,
    canonical_key   STRING NOT NULL,
    PRIMARY KEY (session_id, source_key)
);

CREATE TABLE IF NOT EXISTS canonization_events (
    id              UUID PRIMARY KEY,
    session_id      STRING NOT NULL,
    node_id         UUID NOT NULL,
    from_status     STRING NOT NULL,
    to_status       STRING NOT NULL,
    blast_radius    INT,
    occurred_at     TIMESTAMPTZ NOT NULL,
    INDEX (session_id, occurred_at)
);

CREATE TABLE IF NOT EXISTS reservations (
    session_id      STRING NOT NULL,
    node_id         UUID NOT NULL,
    agent_id        STRING NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (session_id, node_id)
);

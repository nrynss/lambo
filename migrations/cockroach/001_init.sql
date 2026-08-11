-- Lambo v0.1 schema (spec §4). Idempotent for re-runs of scripts/provision.sh.
-- CockroachDB-specific: VECTOR(1024), CREATE VECTOR INDEX.

CREATE TABLE IF NOT EXISTS sessions (
    session_id      STRING PRIMARY KEY,
    root_goal       JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at       TIMESTAMPTZ,
    embedding_kind  STRING,
    embedding_model STRING,
    embedding_dim   INT
);

-- P3 review round 1 (schema persistence): sessions now carries the embedding
-- contract (kind/model/dim) as nullable snapshot metadata (S5-class — no mutation
-- kind writes it; load_session materializes GraphSnapshot.embedding when present).
-- Existing clusters predate these columns, so the ALTER (idempotent — Cockroach
-- supports IF NOT EXISTS on ADD COLUMN) covers them; fresh installs get the columns
-- from the CREATE TABLE above and the ALTER is a no-op.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS embedding_kind STRING;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS embedding_model STRING;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS embedding_dim INT;

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
    chunk_group_id      STRING,
    INDEX (session_id, canonization_status)
);

-- P3 review round 1 (schema persistence): concepts now persists chunk_group_id
-- (T2.5 — Observations demoted from one context-overflow chunk share this id for
-- sibling co-retrieval, spec §7/§8, read by T5.2). Without the column a flush→load
-- cycle silently dropped it. Existing clusters predate the column, so the ALTER
-- (idempotent — IF NOT EXISTS on ADD COLUMN) covers them; fresh installs get it
-- from the CREATE TABLE above and the ALTER is a no-op.
ALTER TABLE concepts ADD COLUMN IF NOT EXISTS chunk_group_id STRING;

-- Errata (2026-08-11, P2 integration / muse-spark M1-M2): the schema's
-- table-level UNIQUE (session_id, canonical_key) is **partial** — it
-- constrains non-Observation concepts only (spec §4 errata):
CREATE UNIQUE INDEX IF NOT EXISTS concepts_key_non_obs_idx
    ON concepts (session_id, canonical_key)
    WHERE concept_type <> 'Observation';
-- Demoted Observations (spec §7) skip the match step and may legitimately
-- share a canonical key (identical sentences from different chunks are distinct
-- context-overflow records). `Graph::insert_concept` and
-- `Graph::assert_invariants` enforce the same rule in RAM.
-- Clusters provisioned before the errata still carry the auto-named table-level
-- UNIQUE from the original DDL (P3 review R2 proved it live: legal demotes were
-- rejected with `concepts_session_id_canonical_key_key`). Drop it — idempotent;
-- fresh installs (no legacy constraint) no-op. The partial index above is the
-- only uniqueness authority on (session_id, canonical_key).
ALTER TABLE concepts DROP CONSTRAINT IF EXISTS concepts_session_id_canonical_key_key;

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
    last_demotion_time TIMESTAMPTZ,
    occurred_at     TIMESTAMPTZ NOT NULL,
    INDEX (session_id, occurred_at)
);

-- Soft locks (S5): an expired row persists until a later write overwrites it,
-- so external SQL readers MUST filter `WHERE expires_at > now()` — never read
-- the table without the expiry predicate.
CREATE TABLE IF NOT EXISTS reservations (
    session_id      STRING NOT NULL,
    node_id         UUID NOT NULL,
    agent_id        STRING NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (session_id, node_id)
);

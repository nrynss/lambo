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

-- Vector index (spec §12.1 — CockroachDB Distributed Vector Indexing).
-- May require feature.vector_index.enabled on some plans.
--
-- T7.4 (2026-08-13): this index MUST be PARTIAL on `embedding IS NOT NULL`.
-- The production query (`VECTOR_CANDIDATES_SQL`) carries that same predicate —
-- it is load-bearing, because 715+ rows hold a NULL embedding and the adapter
-- decodes `dist` as f64, so a NULL-`dist` row hard-errors the whole query the
-- moment k exceeds the non-null count. Against a NON-partial vector index the
-- optimizer cannot prove the predicate is implied by the index, so it plans a
-- FULL SCAN on concepts_pkey and the §12.1 "we used the vector index" claim is
-- false. Against this partial index the UNCHANGED query plans as
-- `vector search` on concepts@concepts_embedding_idx. Evidence:
-- dev-diary/evidence/20260813-130218-vector-index-predicate-finding.txt and
-- …-131108-vector-index-camera-proof-diagnosis.txt.
--
-- !! UPGRADING A PRE-T7.4 CLUSTER IS A ONE-TIME MANUAL STEP — READ THIS.
--
-- Clusters provisioned before T7.4 carry a NON-partial index under this same
-- canonical name, and the statement below will NOT fix them:
-- `CREATE VECTOR INDEX IF NOT EXISTS` matches on NAME ONLY. Measured live
-- 2026-08-13 against a legacy non-partial index: it reports `CREATE INDEX`,
-- succeeds in ~1s, and leaves the non-partial index exactly as it was. It does
-- not error — it silently reports success. So on such a cluster, run this ONCE,
-- by hand, before provisioning (the DROP is ~3s; the rebuild is ~85-96s):
--
--     DROP INDEX IF EXISTS concepts@concepts_embedding_idx;
--     ./scripts/provision.sh
--
-- WHY THE DROP IS NOT IN THIS FILE (this is deliberate, do not "fix" it by
-- adding one): this file is not only applied by provision.sh — it is embedded
-- verbatim as `INIT_SQL` (`include_str!`) and executed by
-- `CockroachStore::init_schema()` on store construction, over a pool whose every
-- connection carries a hard 20s `statement_timeout` (`STATEMENT_TIMEOUT`,
-- src/store/cockroach.rs). `CREATE VECTOR INDEX` takes ~85-96s on the demo
-- cluster. An unconditional `DROP INDEX` here would therefore make EVERY
-- `init_schema()` call destroy the vector index and then time out rebuilding it,
-- leaving the cluster with no vector index at all. Measured: it broke
-- `conformance_suite` and `cockroach_three_hop_progression_matches_memory` with
-- "query execution canceled due to statement timeout".
--
-- The invariant this file must satisfy: EVERY statement here is a no-op in
-- steady state and completes well inside 20s. provision.sh (psql, no statement
-- timeout) is the only thing allowed to do slow schema work. CockroachDB has no
-- DO blocks and provision.sh's splitter rejects dollar-quoting, so a conditional
-- "drop only if non-partial" cannot be expressed in this file at all; it belongs
-- in the applier. See the T7.4 Handoff Log for the proposed provision.sh change.
--
-- A cluster that misses this upgrade fails LOUDLY and specifically, not silently:
-- `vector_explain_camera_proof` asserts `vector search`, the canonical index name,
-- and the absence of `FULL SCAN`.
CREATE VECTOR INDEX IF NOT EXISTS concepts_embedding_idx
    ON concepts (embedding)
    WHERE embedding IS NOT NULL;

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
-- Wave 3 (COH-3): canonization_events carries the demotion timestamp. Existing
-- clusters predate the column, so the idempotent ALTER (same pattern as the
-- sessions embedding-contract columns above) covers them; fresh installs get it
-- from the CREATE TABLE and the ALTER is a no-op.
ALTER TABLE canonization_events ADD COLUMN IF NOT EXISTS last_demotion_time TIMESTAMPTZ;

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

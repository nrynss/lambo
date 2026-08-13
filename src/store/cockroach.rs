//! T3.2 — `CockroachStore`: durable [`GraphStore`] over `sqlx::PgPool` (spec §3.2/§3.3, §4).
//!
//! Feature: `store-cockroach` (pulls `sqlx` + the `postgres` driver). Registered in
//! [`super::build_store`] for [`super::StoreKind::Cockroach`]. All SQL is runtime
//! `sqlx::query` (spec §3.2 — no compile-time macros); this module owns every statement.
//!
//! # Design decisions (see PHASE-3-stores.md Handoff Log T3.2)
//!
//! - **Batch replay order (flush).** Mutations are replayed **in submission order**
//!   inside ONE transaction — never re-grouped by kind. The graph tier guarantees
//!   spec §2.4's grouping (nodes → edges → deletions → transitions) *within a single
//!   logical write*, but the drained log is chronological *across* writes, and a node
//!   upsert may legally follow a `DeleteNode` of the same id in one batch
//!   (create → delete → create within one flush interval). `src/graph/mod.rs` (T2.1
//!   M2 review close) is explicit: *"Store adapters (T3.4+) MUST replay batches in
//!   order and MUST NOT re-sort them."* `MemoryStore` does the same.
//! - **Concept upsert target is `ON CONFLICT (id)`** — not the canonical-key partial
//!   index. Rationale: a re-upsert of an existing concept (id present) must replace the
//!   whole row, and the id is the only *total* uniqueness constraint. The partial
//!   `concepts_key_non_obs_idx` (`(session_id, canonical_key) WHERE concept_type <>
//!   'Observation'`, spec §4 errata / muse-spark M1-M2) can only reject an INSERT whose
//!   key collides with a **non-Observation** concept — exactly the case the RAM graph
//!   already rejects as an invariant (`Graph::insert_concept`). A legal demote writes an
//!   **Observation** sharing a key; the partial index excludes it, so the INSERT
//!   succeeds. No `ON CONFLICT (session_id, canonical_key) WHERE ...` spelling is
//!   needed and none is used (that target would not cover id-based re-upserts).
//! - **DeleteNode cleans incident edges explicitly.** `edges.source`/`target` carry no
//!   `REFERENCES` (spec §4) — Cockroach enforces only declared FKs, so a deleted node
//!   would leave dangling edges unless removed. We delete edges where
//!   `source = $1 OR target = $1 OR id = $1` (MemoryStore parity). `canonization_events`
//!   is append-only (the demo artifact) and is *not* cleaned. Interactions are
//!   append-only in v0.1 (graph contract) so `DeleteNode` on an interaction id cannot
//!   legally occur; if it ever does, the enforced FK from
//!   `concepts.origin_interaction`/`interactions.previous_id` fails the delete loudly
//!   rather than corrupting the graph.
//! - **VECTOR encode/decode (T0.3 spike, Attempt A).** Bind the embedding as a text
//!   literal and cast server-side (`$n::VECTOR`); read back via `embedding::STRING` and
//!   parse. Text form is `[x,y,z]` with Rust's shortest-round-trip `f32` `Display`
//!   (spike-verified exact at eps=1e-4 over 1024 dims). Non-finite elements are
//!   rejected at encode time.
//! - **vector_candidates score = cosine similarity** derived from the L2 distance the
//!   `<->` operator returns: `1 - d²/2`, clamped to [-1, 1]. This is the metric
//!   `semantic_match_threshold` (spec §7.1 step 6, 0.85) is written against; a raw
//!   distance would be backwards. Exact only for unit-normalized embeddings (the
//!   pipeline normalizes — Titan `normalize=true`, spike normalizes).
//! - **§4.1 queries bind a Rust-computed cutoff, not an `INTERVAL` literal.** T3.3/T3.6
//!   contract: SQLite has no `INTERVAL`, so both dialects compute
//!   `now - min_age` in Rust and bind it, keeping the two queries twin-shaped. The
//!   interaction_span query additionally filters `edges.created_at` (not just
//!   `i.created_at` as the spec's literal SQL shows) to match `MemoryStore`'s naive
//!   answer — T3.6's three-way agreement test defines agreement against MemoryStore.
//!   Full semantics (locked by the T3.6 matrix on BOTH fixtures × every node ×
//!   min-age {0, 3600s} × MemoryStore equality, plus the errata/aged-edge probes):
//!   (1) **errata exclusions (2026-08-11 / T1.4)** — only concept-sourced
//!   `Dependency`/`Causal`/`Hierarchical` count; provenance `Derives`
//!   (interaction → concept, mandatory §5.7) and `Temporal` must never un-orphan a
//!   concept, or Stage-3 blast radius would collapse to ~0 on every legal graph;
//!   (2) `c.id <> $node` self-exclusion, matching MemoryStore's skip;
//!   (3) **aged edges only** — `e.created_at <= cutoff` in BOTH queries, and span
//!   also gates `i.created_at <= cutoff` (spec §4.1 second errata); (4) **F1
//!   single-point rule** — `coverage` is `0.0` only when `distinct == 0`; a non-empty
//!   span over a single-point session extent reports `1.0` (canonization Stage 2
//!   parity in short sessions).
//! - **`chunk_group_id` (T2.5) is a first-class column** (P3 review round 1
//!   remediation). `concepts.chunk_group_id STRING` (nullable) is declared in the
//!   `CREATE TABLE` for fresh installs AND added via `ALTER TABLE concepts ADD COLUMN
//!   IF NOT EXISTS chunk_group_id STRING` for existing clusters (Cockroach supports
//!   `IF NOT EXISTS` on `ADD COLUMN`), so `init_schema` stays idempotent either way.
//!   The concept upsert writes it and `load_session` reads it back — a flush→load
//!   cycle now PRESERVES the T5.2 sibling co-retrieval key (regression-locked in the
//!   live conformance suite).
//! - **`GraphSnapshot::embedding` (the `EmbeddingContract`) is durable.** The
//!   sessions table carries `embedding_kind
//!   STRING`, `embedding_model STRING`, `embedding_dim INT` (nullable; same
//!   CREATE + ALTER `ADD COLUMN IF NOT EXISTS` idempotency pattern as
//!   `chunk_group_id`), `seed` upserts all three from the snapshot's contract,
//!   and `load_session` materializes `GraphSnapshot.embedding` when
//!   `embedding_kind` is present (STORE-1 remediation). **Corruption parity
//!   (STORE-7):** a row with exactly one of `embedding_kind` / `embedding_dim`
//!   set (kind XOR dim) is a `Backend` corruption error from `load_session` —
//!   mirroring sqlite — never a silent `None`. `flush` applies the ordered
//!   `Mutation::SetEmbedding` in the same transaction as concept vectors, so
//!   ordinary write-behind and full-snapshot seed converge on the same columns.
//!   The DDL column width *is* read from the schema: `vector_dimensions()` parses
//!   `VECTOR(n)` out of the embedded `001_init.sql` (not a global constant), so
//!   `resolve::check_vector_compatibility` can reject mismatched embedders.
//! - **rustls DSN rewrite.** sqlx's rustls stack cannot open libpq's magic
//!   `sslrootcert=system` path; the `.env` DSN uses it. [`dsn_for_rustls`] rewrites it
//!   to a real CA bundle (or downgrades `verify-full` → `require`) before pooling
//!   (T0.3 spike, proven against the cloud cluster).

// Clippy's `explicit_auto_deref` suggestion is wrong for sqlx: `&mut *tx` reborrows
// the `Transaction` (which implements `sqlx::Executor`), while the suggested `&mut tx`
// produces `&mut &mut Transaction` (which does not). Known sqlx+clippy false-positive;
// kept explicit on purpose.
#![allow(clippy::explicit_auto_deref)]

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::future::Future;
use std::time::Duration;
use uuid::Uuid;

use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};

use super::vector::{decode_vector, encode_vector};
use super::{
    map_write_err, validate_vector_candidate_limit, Capabilities, GraphStore, StoreConfig,
};
use crate::types::{
    CanonizationEvent, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, EmbeddingContract,
    GraphSnapshot, Interaction, InteractionSpan, Mutation, MutationBatch, Node, NodeId,
    Reservation, Scored, SessionId, StoreError, Synonym,
};

/// T3.1 DDL — embedded and executed verbatim by [`CockroachStore::init_schema`].
/// Idempotent by construction (`CREATE ... IF NOT EXISTS` everywhere).
const INIT_SQL: &str = include_str!("../../migrations/cockroach/001_init.sql");

/// Pool size is deliberately small: Lambo is single-writer per session (spec §2.4) and
/// the demo runs one process.
const MAX_POOL_CONNECTIONS: u32 = 4;

// ---------------------------------------------------------------------------
// vector_candidates — global fetch sizing (DECISION D1)
// ---------------------------------------------------------------------------
// DECISION D1 (PHASE-7-embeddings.md T7.3): the vector query is GLOBAL
// (`ORDER BY embedding <-> $1::VECTOR LIMIT $k`, no session predicate) so the
// planner uses `concepts@concepts_embedding_idx` (the session-filtered shape scans
// `concepts_session_id_canonical_key_key` and bypasses the index — evidence in
// `dev-diary/evidence/t0.3-vector-spike.txt`). Session filtering happens in Rust.
// Because the trait passes only `limit`, the global fetch `$k` must be sized
// generously enough that the caller's in-session top candidates are not crowded out
// of the global top-k by foreign-session concepts. The sizing is a documented,
// deterministic approximation of "global top-k + session filter":
//
//   - BASE: the first global fetch is `limit × VECTOR_FETCH_MULTIPLIER` (a generous
//     headroom over the session's expected concept population), floored at the
//     multiplier and CAPPED at `VECTOR_FETCH_CAP` ([`initial_fetch_k`]) — `limit` is
//     caller-supplied but validated at the public 2,048-result bound, so the cap
//     keeps even the base fetch within the
//     documented worst-case bound (T7.3 remediation).
//   - GROW-AND-RETRY: the adapter requests `k + 1`; when that lookahead exists yet
//     the first `k` rows yield fewer than `limit` in-session hits, more global rows
//     exist beyond this window, so
//     re-query with `k` doubled (cheap: index-backed top-k is O(log n + k)), up to
//     `VECTOR_FETCH_CAP` ([`next_fetch_k`]).
//   - Completeness bound: retry STOPS EARLY when the `k + 1` lookahead is absent.
//     Under an EXACT scan (non-partial index) that means the global population is
//     exhausted, so no further in-session candidate can exist. Under the PARTIAL
//     ANN index (T7.4) `vector search` visits a bounded set of neighbourhoods, so a
//     lookahead-absent page means the BEAM exhausted its visited neighbourhoods,
//     not the table — a true near neighbour can be missed (see the ANN accuracy
//     dial doc above); that miss is accepted for v0.1, not "provably complete".
//   - Cap fallback: if the global page is still full and crowded at CAP, run an
//     exact session-scoped query. That path may not use the global vector index,
//     but it preserves the GraphStore completeness contract for adversarial
//     multi-tenant distributions. Normal traffic stays on the indexed fast path.
const VECTOR_FETCH_MULTIPLIER: usize = 10;
const VECTOR_FETCH_GROWTH: usize = 2;
const VECTOR_FETCH_CAP: usize = 2048;

/// GLOBAL vector top-k — deliberately omits any session predicate so the planner
/// uses `concepts@concepts_embedding_idx` (DECISION D1). `session_id` is selected so
/// the Rust side can drop foreign-session rows. Ordering is L2 distance ascending
/// (`<->`), i.e. similarity (score) descending. The adapter requests `k + 1`:
/// a lookahead tied with the kth distance triggers the exact, UUID-ordered
/// session fallback, while an untied boundary remains on this index-friendly path.
const VECTOR_CANDIDATES_SQL: &str = r#"
SELECT id::STRING AS id, session_id::STRING AS session_id,
       embedding <-> $1::VECTOR AS dist
FROM concepts
WHERE embedding IS NOT NULL
ORDER BY dist ASC
LIMIT $2
"#;

/// Correctness fallback when foreign-session rows crowd the caller out of the
/// capped global index query. This deliberately prioritizes exact session-local
/// top-k over index use; it runs only after the bounded fast path is exhausted.
const SESSION_VECTOR_CANDIDATES_SQL: &str = r#"
SELECT id::STRING AS id, embedding <-> $1::VECTOR AS dist
FROM concepts
WHERE session_id = $2 AND embedding IS NOT NULL
ORDER BY dist ASC, id ASC
LIMIT $3
"#;

// ---------------------------------------------------------------------------
// SQL statements (each adapter owns its SQL — spec §3.2)
// ---------------------------------------------------------------------------

/// Session row exists primarily to satisfy `interactions.session_id` / `concepts.session_id`
/// `REFERENCES sessions(session_id)` — `flush` upserts a bare row per new session
/// (created_at defaults to `now()`), mirroring `MemoryStore::ensure_session`.
const UPSERT_SESSION_ROW_SQL: &str = r#"
INSERT INTO sessions (session_id)
VALUES ($1)
ON CONFLICT (session_id) DO NOTHING
"#;

/// Full-snapshot sessions upsert (fixtures `seed` path): root_goal JSONB, created_at,
/// closed_at, and the `EmbeddingContract` columns (STORE-1). `COALESCE($3, now())`
/// keeps the NOT NULL default when a snapshot omits it.
#[cfg(any(test, feature = "fixtures"))]
const UPSERT_SESSION_SQL: &str = r#"
INSERT INTO sessions (
    session_id, root_goal, created_at, closed_at,
    embedding_kind, embedding_model, embedding_dim
) VALUES ($1, $2::JSONB, COALESCE($3, now()), $4, $5::STRING, $6::STRING, $7::INT)
ON CONFLICT (session_id) DO UPDATE SET
    root_goal = EXCLUDED.root_goal,
    created_at = EXCLUDED.created_at,
    closed_at = EXCLUDED.closed_at,
    embedding_kind = EXCLUDED.embedding_kind,
    embedding_model = EXCLUDED.embedding_model,
    embedding_dim = EXCLUDED.embedding_dim
"#;

const UPSERT_INTERACTION_SQL: &str = r#"
INSERT INTO interactions (
    id, session_id, agent_id, prompt_text, previous_id, created_at
) VALUES ($1, $2, $3, $4, $5, $6)
ON CONFLICT (id) DO UPDATE SET
    session_id = EXCLUDED.session_id,
    agent_id = EXCLUDED.agent_id,
    prompt_text = EXCLUDED.prompt_text,
    previous_id = EXCLUDED.previous_id,
    created_at = EXCLUDED.created_at
"#;

/// 16 columns; `embedding` is bound as text and cast server-side (`$15::VECTOR`);
/// `chunk_group_id` (T2.5 sibling co-retrieval key) is the 16th, bound nullable.
///
/// **R2-1 — canonization columns are insert-only here.**
/// `canonization_status` / `blast_radius` / `last_demotion_time` are in the
/// INSERT column list (a brand-new row must carry them) but deliberately
/// **absent from the `DO UPDATE SET` list**: on an existing row the
/// canonization path (`UPDATE_CONCEPT_STATUS_SQL`) is their only writer.
/// A `Mutation::UpsertNode` carries a snapshot of the concept taken when it
/// was appended — a GC `bump_gc_survived` from before a hop would otherwise
/// overwrite the hop's effect from `EXCLUDED`, and the transition replaying
/// behind it in the same batch is a no-op by design (its audit row is already
/// recorded), so nothing repairs it. Full rationale on `Mutation::UpsertNode`.
const UPSERT_CONCEPT_SQL: &str = r#"
INSERT INTO concepts (
    id, session_id, content, canonical_key, concept_type,
    origin_interaction, origin_agent, created_at, access_count, last_accessed,
    gc_survived, canonization_status, blast_radius, last_demotion_time, embedding,
    chunk_group_id
) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15::VECTOR, $16)
ON CONFLICT (id) DO UPDATE SET
    session_id = EXCLUDED.session_id,
    content = EXCLUDED.content,
    canonical_key = EXCLUDED.canonical_key,
    concept_type = EXCLUDED.concept_type,
    origin_interaction = EXCLUDED.origin_interaction,
    origin_agent = EXCLUDED.origin_agent,
    created_at = EXCLUDED.created_at,
    access_count = EXCLUDED.access_count,
    last_accessed = EXCLUDED.last_accessed,
    gc_survived = EXCLUDED.gc_survived,
    embedding = EXCLUDED.embedding,
    chunk_group_id = EXCLUDED.chunk_group_id
"#;

/// Natural-key conflict target `(source, target, edge_type)` matches the graph tier's
/// `record_edge` dedup: a duplicate natural key **reinforces** (replaces) the row while
/// preserving nothing — the incoming record is authoritative (I2 convention: graph core
/// counts creation as the first write, `reinforcements = 1`; we store its values, never
/// the DDL default 0). Updating `id` on conflict mirrors MemoryStore's whole-record
/// replace; the graph never reuses an id with a different natural key.
const UPSERT_EDGE_SQL: &str = r#"
INSERT INTO edges (
    id, session_id, source, target, edge_type, weight, reinforcements,
    created_at, last_reinforced
) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
ON CONFLICT (source, target, edge_type) DO UPDATE SET
    id = EXCLUDED.id,
    session_id = EXCLUDED.session_id,
    weight = EXCLUDED.weight,
    reinforcements = EXCLUDED.reinforcements,
    created_at = EXCLUDED.created_at,
    last_reinforced = EXCLUDED.last_reinforced
"#;

const DELETE_NODE_EDGES_SQL: &str = r#"
DELETE FROM edges WHERE source = $1 OR target = $1 OR id = $1
"#;

/// XP-8: persist a session's `root_goal` from the mutation path. Same column and
/// same JSONB cast `UPSERT_SESSION_SQL` (the `seed` path) uses, so a goal set
/// through a mutation and one seeded from a snapshot are indistinguishable on
/// reload. The bare-row upsert above has already created the row.
const SET_ROOT_GOAL_SQL: &str = r#"
UPDATE sessions SET root_goal = $2::JSONB WHERE session_id = $1
"#;

const SET_EMBEDDING_SQL: &str = r#"
UPDATE sessions
SET embedding_kind = $2::STRING,
    embedding_model = $3::STRING,
    embedding_dim = $4::INT
WHERE session_id = $1
"#;

const QUARANTINE_LEGACY_EMBEDDINGS_SQL: &str = r#"
UPDATE concepts SET embedding = NULL
WHERE session_id = $1 AND EXISTS (
    SELECT 1 FROM sessions
    WHERE session_id = $1 AND embedding_kind IS NULL AND embedding_dim IS NULL
)
"#;

const DELETE_NODE_CONCEPTS_SQL: &str = r#"
DELETE FROM concepts WHERE id = $1
"#;

const DELETE_NODE_INTERACTIONS_SQL: &str = r#"
DELETE FROM interactions WHERE id = $1
"#;

const DELETE_EDGE_SQL: &str = r#"
DELETE FROM edges WHERE id = $1
"#;

/// Canonization transition: update the concept (parity with MemoryStore's
/// `CanonizationTransition` application) and append the audit row. The event insert is
/// `ON CONFLICT (id) DO NOTHING` so a retried flush (same batch, already-committed
/// response lost) cannot duplicate the demo's on-screen artifact.
///
/// COH-3: `last_demotion_time = COALESCE($5, last_demotion_time)` — a demotion
/// event (which always carries `Some`) stamps the concept; non-demotion events
/// (`None`) leave a previously demoted value untouched (spec §10).
const UPDATE_CONCEPT_STATUS_SQL: &str = r#"
UPDATE concepts
SET canonization_status = $2, blast_radius = $3,
    last_demotion_time = COALESCE($5, last_demotion_time)
WHERE id = $1 AND session_id = $4
"#;

const INSERT_CANONIZATION_EVENT_SQL: &str = r#"
INSERT INTO canonization_events (
    id, session_id, node_id, from_status, to_status, blast_radius,
    last_demotion_time, occurred_at
) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
ON CONFLICT (id) DO NOTHING
"#;

#[cfg(any(test, feature = "fixtures"))]
const UPSERT_SYNONYM_SQL: &str = r#"
INSERT INTO synonyms (session_id, source_key, canonical_key)
VALUES ($1, $2, $3)
ON CONFLICT (session_id, source_key) DO UPDATE SET
    canonical_key = EXCLUDED.canonical_key
"#;

#[cfg(any(test, feature = "fixtures"))]
const UPSERT_RESERVATION_SQL: &str = r#"
INSERT INTO reservations (session_id, node_id, agent_id, expires_at)
VALUES ($1, $2, $3, $4)
ON CONFLICT (session_id, node_id) DO UPDATE SET
    agent_id = EXCLUDED.agent_id,
    expires_at = EXCLUDED.expires_at
"#;

const SELECT_SESSION_SQL: &str = r#"
SELECT root_goal::STRING AS root_goal, created_at, closed_at,
       embedding_kind, embedding_model, embedding_dim
FROM sessions
WHERE session_id = $1
"#;

const SELECT_INTERACTIONS_SQL: &str = r#"
SELECT id::STRING AS id, session_id, agent_id, prompt_text,
       previous_id::STRING AS previous_id, created_at
FROM interactions
WHERE session_id = $1
ORDER BY created_at, id
"#;

const SELECT_CONCEPTS_SQL: &str = r#"
SELECT id::STRING AS id, session_id, content, canonical_key, concept_type,
       origin_interaction::STRING AS origin_interaction, origin_agent, created_at,
       access_count, last_accessed, gc_survived, canonization_status, blast_radius,
       last_demotion_time, embedding::STRING AS embedding, chunk_group_id
FROM concepts
WHERE session_id = $1
ORDER BY id
"#;

const SELECT_EDGES_SQL: &str = r#"
SELECT id::STRING AS id, session_id, source::STRING AS source,
       target::STRING AS target, edge_type, weight, reinforcements,
       created_at, last_reinforced
FROM edges
WHERE session_id = $1
ORDER BY id
"#;

const SELECT_SYNONYMS_SQL: &str = r#"
SELECT session_id, source_key, canonical_key
FROM synonyms
WHERE session_id = $1
"#;

const SELECT_CANONIZATION_EVENTS_SQL: &str = r#"
SELECT id::STRING AS id, session_id, node_id::STRING AS node_id,
       from_status, to_status, blast_radius, last_demotion_time, occurred_at
FROM canonization_events
WHERE session_id = $1
ORDER BY occurred_at, id
"#;

const SELECT_RESERVATIONS_SQL: &str = r#"
SELECT session_id, node_id::STRING AS node_id, agent_id, expires_at
FROM reservations
WHERE session_id = $1
"#;

/// Blast radius (spec §4.1 + errata, `MemoryStore`-equivalent): count concepts that have
/// at least one **aged** inbound structural edge (`Dependency`/`Causal`/`Hierarchical`)
/// from `node` and no aged inbound structural edge from any other concept. Provenance
/// `Derives`/`Temporal` edges must not un-orphan (errata; T1.4). `src.session_id = $1`
/// keeps the source-concept check session-scoped (MemoryStore's `concept_ids`), and
/// `c.id <> $2` excludes a hypothetical self-loop, matching MemoryStore's skip.
/// Placeholders: `$1` session, `$2` node, `$3` cutoff timestamp (Rust-computed).
const BLAST_RADIUS_SQL: &str = r#"
SELECT count(*) AS n
FROM concepts c
WHERE c.session_id = $1
  AND c.id <> $2
  AND EXISTS (
      SELECT 1 FROM edges e
      JOIN concepts src ON src.id = e.source AND src.session_id = $1
      WHERE e.target = c.id AND e.source = $2
        AND e.edge_type IN ('Dependency', 'Causal', 'Hierarchical')
        AND e.created_at <= $3
  )
  AND NOT EXISTS (
      SELECT 1 FROM edges e2
      JOIN concepts src2 ON src2.id = e2.source AND src2.session_id = $1
      WHERE e2.target = c.id AND e2.source <> $2
        AND e2.edge_type IN ('Dependency', 'Causal', 'Hierarchical')
        AND e2.created_at <= $3
  )
"#;

/// Interaction span + temporal coverage (spec §4.1, `MemoryStore`-equivalent): distinct
/// origin interactions of concepts reachable via aged structural inbound edges, and the
/// share of the session's temporal extent those interactions cover. Filters BOTH the
/// edge and the interaction age (MemoryStore parity — see module doc). The `CASE` keeps
/// the coverage `0.0` when no spans match (a `NULL` epoch-arithmetic result would
/// otherwise surface as a decode error). The extent is never `NULL` while the span is
/// non-empty (the span's interactions belong to the same session), so the `ELSE` arm
/// covers exactly one case: a non-empty span over a **single-point session extent**
/// (extent <= 0) — that interaction spans the whole session, so coverage is `1.0`
/// (F1: canonization Stage 2 parity with MemoryStore in short sessions).
///
/// **Session scope (F5).** `i.session_id = $1` is not redundant with
/// `e.session_id = $1`: `concepts.origin_interaction` is a **global** FK, so a
/// concept in session S may legally point at an interaction in session S′.
/// Without the filter the span counted those foreign interactions — inflating
/// `distinct` against a session `MemoryStore` never sees, while the extent CTE
/// stayed session-filtered. The `least`/`greatest` clamp is the same guard
/// `MemoryStore` and SQLite apply in Rust (`clamp(0.0, 1.0)`): a span whose
/// endpoints fall outside the session extent must not report a ratio above 1.0.
/// Placeholders: `$1` session, `$2` node, `$3` cutoff timestamp.
const INTERACTION_SPAN_SQL: &str = r#"
WITH span AS (
    SELECT DISTINCT i.id AS iid, i.created_at AS ts
    FROM edges e
    JOIN concepts src ON src.id = e.source AND src.session_id = $1
    JOIN interactions i ON i.id = src.origin_interaction AND i.session_id = $1
    WHERE e.target = $2
      AND e.session_id = $1
      AND e.edge_type IN ('Dependency', 'Causal', 'Hierarchical')
      AND e.created_at <= $3
      AND i.created_at <= $3
),
extent AS (
    SELECT min(created_at) AS lo, max(created_at) AS hi
    FROM interactions WHERE session_id = $1
)
SELECT
    (SELECT count(*) FROM span) AS distinct_count,
    CASE
        WHEN (SELECT count(*) FROM span) = 0 THEN 0.0
        WHEN extract(epoch FROM (extent.hi - extent.lo)) > 0
            THEN least(1.0, greatest(0.0,
                 extract(epoch FROM ((SELECT max(ts) FROM span) - (SELECT min(ts) FROM span)))
                 / extract(epoch FROM (extent.hi - extent.lo))))
        -- F1: non-empty span over a single-point session extent covers the
        -- whole session -> 1.0 (the count = 0 arm above handles empty spans).
        ELSE 1.0
    END AS coverage
FROM extent
"#;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn backend<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// STORE-7 — session-row embedding-contract parsing, mirroring sqlite's XOR
/// corruption handling. A row with exactly one of `embedding_kind` /
/// `embedding_dim` set (as direct SQL can manufacture) is a corruption error,
/// never a silent `None`; `embedding_model` alone is inert (model without a
/// kind has nothing to label).
fn session_embedding_from_parts(
    kind: Option<String>,
    model: Option<String>,
    dim: Option<i64>,
    session_id: &str,
) -> Result<Option<EmbeddingContract>, StoreError> {
    match (kind, dim) {
        (Some(kind), Some(dim)) => Ok(Some(EmbeddingContract {
            kind,
            model,
            dim: usize::try_from(dim).map_err(|_| {
                StoreError::Backend(format!(
                    "sessions row for {session_id} has negative embedding_dim"
                ))
            })?,
        })),
        (None, None) => Ok(None),
        (Some(_), None) => Err(StoreError::Backend(format!(
            "sessions row for {session_id} has embedding_kind without embedding_dim"
        ))),
        (None, Some(_)) => Err(StoreError::Backend(format!(
            "sessions row for {session_id} has embedding_dim without embedding_kind"
        ))),
    }
}

fn vector_contract_allows_query(
    kind: Option<String>,
    model: Option<String>,
    dim: Option<i64>,
    session_id: &str,
) -> Result<bool, StoreError> {
    Ok(session_embedding_from_parts(kind, model, dim, session_id)?.is_some())
}

/// CockroachDB serializable transactions abort with SQLSTATE 40001
/// (`restart transaction: ... RETRY_SERIALIZABLE ...`) when they conflict with a
/// concurrent commit; sqlx does not auto-retry, so the client must replay the whole
/// transaction. Bounded backoff; a genuine (non-conflict) error is returned
/// immediately.
const TX_RETRY_ATTEMPTS: usize = 5;

/// STORE-2: server-side per-statement bound (`statement_timeout`), applied to
/// every connection in the pool. `statement_timeout` applies per statement,
/// not per transaction — a multi-statement flush batch can run N x 20s. The
/// whole-batch bound is the client-side flush attempt timeout
/// (`flush.rs` `FLUSH_ATTEMPT_TIMEOUT`); the per-statement bound stays below
/// it so the database aborts a hung statement before the client gives up on
/// the attempt. It also bounds every other statement on the pool — well
/// under the 30s `LOAD_SESSION_TIMEOUT`.
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(20);

/// T7.4: accuracy dial for CockroachDB's **approximate** vector search.
///
/// Once `concepts_embedding_idx` is partial (spec §12.1), `vector_candidates`
/// is served by an ANN index instead of an exact full scan: the search visits
/// a bounded number of index neighbourhoods rather than every row, so a true
/// near neighbour sitting in an unvisited neighbourhood can be missed. In Lambo
/// that surfaces as a *silent* quality loss, not an error — hybrid matching
/// fails to merge a genuine near-duplicate and writes a new concept instead,
/// leaving the graph slightly less connected.
///
/// `vector_search_beam_size` is how many neighbourhoods the search visits:
/// higher is more accurate and slower. CockroachDB's own default is 32 and the
/// server enforces **1..=2048** (verified live 2026-08-13; out-of-range is a
/// server-side error, not a clamp).
///
/// **Default 64, chosen from measurement** (adve-review MAJOR-1, 2026-08-13).
/// Recall was measured against exact top-k on the live cluster, where exact
/// ground truth is forced with the `concepts@concepts_pkey` hint (a FULL SCAN).
/// Two 3,000-row datasets: uniform-random vectors, and clustered unit-norm
/// vectors matching the geometry real embeddings actually have.
///
/// ```text
/// beam:      1     2     4     8    16    32*    64    128    256
/// recall@10 .19   .23   .32   .47   .70   .93    .96   .96    .86
/// recall@50 .07   .13   .22   .40   .64   .94    .99   .99    .97
///                                        *server default
/// ```
///
/// Two findings drove the value:
/// * At the server default (32) roughly **6-7% of true nearest neighbours are
///   missed**. For Lambo that is not a latency question — a missed neighbour is
///   a near-duplicate that hybrid matching fails to merge, so the concept is
///   silently re-created and the graph ends up less connected.
/// * **Higher is not monotonically better.** Beam 256 scored *worse* than 64 in
///   BOTH datasets, reproducibly (recall@10 .86 vs .96). So "crank it up" is
///   wrong advice, and 64 — not the maximum — is the measured knee.
///
/// Recall never reached 1.000 at any beam: this index is approximate by
/// construction and no setting makes it exact. Exactness is available only by
/// giving up index use, which spec §12.1 requires us to demonstrate.
///
/// Override per process (still `1..=2048`, the server's own bound, verified
/// live — out-of-range is a server-side error, not a clamp):
///
/// ```text
/// LAMBO_VECTOR_BEAM_SIZE=32 lambo serve …   # back to the server default
/// ```
///
/// Applied per connection alongside `statement_timeout`, so it costs no
/// per-query round trip. An invalid value is a hard error at pool construction
/// (Level B fails closed — a silently ignored tuning knob is worse than none,
/// because the operator believes accuracy was raised when it was not).
///
/// Caveat kept deliberately: both datasets are synthetic. The clustered set
/// mimics embedding geometry but is not BGE-M3 output, so treat 64 as an
/// evidence-based default rather than a tuned optimum.
const DEFAULT_VECTOR_BEAM_SIZE: u32 = 64;
const VECTOR_BEAM_SIZE_ENV: &str = "LAMBO_VECTOR_BEAM_SIZE";
const VECTOR_BEAM_SIZE_MIN: u32 = 1;
const VECTOR_BEAM_SIZE_MAX: u32 = 2048;

/// Parse `LAMBO_VECTOR_BEAM_SIZE`. `Ok(None)` = unset, inherit the server
/// default. Empty is treated as unset so an exported-but-blank var behaves like
/// absence (same convention as `LAMBO_STORE`).
fn vector_beam_size_from_env() -> Result<Option<u32>, StoreError> {
    let raw = match std::env::var(VECTOR_BEAM_SIZE_ENV) {
        Ok(v) => v,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(e) => return Err(backend(format!("{VECTOR_BEAM_SIZE_ENV}: {e}"))),
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let n: u32 = raw.parse().map_err(|_| {
        backend(format!(
            "{VECTOR_BEAM_SIZE_ENV} must be an integer in \
             {VECTOR_BEAM_SIZE_MIN}..={VECTOR_BEAM_SIZE_MAX}, got {raw:?}"
        ))
    })?;
    if !(VECTOR_BEAM_SIZE_MIN..=VECTOR_BEAM_SIZE_MAX).contains(&n) {
        return Err(backend(format!(
            "{VECTOR_BEAM_SIZE_ENV} must be in \
             {VECTOR_BEAM_SIZE_MIN}..={VECTOR_BEAM_SIZE_MAX}, got {n}"
        )));
    }
    Ok(Some(n))
}

/// STORE-4: structured retry decision for `tx_retry` — no message-text
/// matching. Constraint violations (SQLSTATE 23xxx) are deterministic and are
/// mapped to [`StoreError::Constraint`] by the write path: never replay them.
/// Typed variants are permanent. A [`StoreError::Backend`] may be a transient
/// (serialization conflict, connection exception, server shutdown) that
/// replaying the transaction can fix; the replay is bounded by
/// [`TX_RETRY_ATTEMPTS`] with backoff.
fn tx_retryable(e: &StoreError) -> bool {
    match e {
        StoreError::Constraint(_) => false,
        StoreError::Backend(_) => true,
        _ => false,
    }
}

/// Run `body` inside a transaction, replaying the whole body on a fresh transaction when
/// Cockroach aborts it with a serializable-conflict retry (SQLSTATE 40001). The `body`
/// closure opens its own transaction (via a captured pool handle), performs the writes,
/// and commits; a dropped transaction rolls back automatically. Returning a retryable
/// error from any statement aborts the attempt; the wrapper sleeps with bounded backoff
/// and replays the whole body. A non-retryable error is returned immediately.
async fn tx_retry<T, F, Fut>(mut body: F) -> Result<T, StoreError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, StoreError>>,
{
    let mut last_err: Option<StoreError> = None;
    for attempt in 0..TX_RETRY_ATTEMPTS {
        match body().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if tx_retryable(&err) && attempt + 1 < TX_RETRY_ATTEMPTS {
                    last_err = Some(err);
                    tokio::time::sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        StoreError::Backend("transaction retry exhausted (Cockroach serializable conflict)".into())
    }))
}

/// T0.3 spike: make a libpq DSN usable with sqlx's rustls stack. libpq's magic
/// `sslrootcert=system` is not a real path; point at an actual CA bundle or drop to
/// `require`. Returns the DSN unchanged when no rewrite is needed.
fn dsn_for_rustls(dsn: &str) -> String {
    let ca_candidates = [
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/cert.pem",
        "/etc/ssl/ca-bundle.pem",
    ];
    let ca = ca_candidates
        .iter()
        .find(|p| std::path::Path::new(p).is_file())
        .copied();

    let mut out = dsn.to_string();
    if out.contains("sslrootcert=system") {
        if let Some(path) = ca {
            out = out.replace("sslrootcert=system", &format!("sslrootcert={path}"));
        } else {
            out = out.replace("sslrootcert=system", "");
            out = out.replace("&&", "&");
            if out.contains("sslmode=verify-full") {
                out = out.replace("sslmode=verify-full", "sslmode=require");
            }
        }
    }
    out = out.replace("?&", "?").trim_end_matches('&').to_string();
    if out.ends_with('?') {
        out.pop();
    }
    out
}

/// `spec §4.1` L2 distance → cosine similarity (`1 - d²/2`); see module doc.
fn distance_to_score(dist: f64) -> f64 {
    (1.0 - 0.5 * dist * dist).clamp(-1.0, 1.0)
}

/// Keep only rows belonging to the caller's session from one global top-k fetch.
/// Input rows arrive in L2-distance-ascending order (SQL `ORDER BY dist ASC`), so
/// the survivors keep that order — the trait's score-descending ordering contract.
/// Pure & deterministic: unit-tested without a cluster.
fn filter_session_rows(session: &SessionId, rows: &[(NodeId, f64, String)]) -> Vec<Scored<NodeId>> {
    let mut scored: Vec<_> = rows
        .iter()
        .filter(|(_, dist, sid)| sid == &session.0 && dist.is_finite())
        .map(|(id, dist, _)| Scored::new(*id, distance_to_score(*dist)))
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.item.0.cmp(&b.item.0))
    });
    scored
}

fn has_boundary_tie(rows: &[(NodeId, f64, String)], k: usize) -> bool {
    k > 0
        && rows.len() > k
        && rows[k - 1].1.is_finite()
        && rows[k].1.is_finite()
        && rows[k - 1].1.total_cmp(&rows[k].1).is_eq()
}

/// DECISION D1 base global fetch size. `limit × multiplier`, floored at the
/// multiplier so a non-trivial query always pulls some headroom, and CAPPED at
/// [`VECTOR_FETCH_CAP`] — `limit` is validated at the public boundary, and the
/// cap additionally ensures the first global fetch cannot exceed the
/// documented 2048-row worst-case bound. The growth step in [`next_fetch_k`] is
/// already capped; this extends the same bound to the BASE. `limit == 0` is
/// short-circuited by the caller before reaching here.
fn initial_fetch_k(limit: usize) -> usize {
    // VECTOR_FETCH_MULTIPLIER <= VECTOR_FETCH_CAP is guaranteed, so clamp cannot panic.
    limit
        .saturating_mul(VECTOR_FETCH_MULTIPLIER)
        .clamp(VECTOR_FETCH_MULTIPLIER, VECTOR_FETCH_CAP)
}

/// Grow-and-retry decision for the global vector fetch (DECISION D1). Given
/// whether the `k + 1` lookahead found another row, how many rows were
/// in-session, and the current `k`, return
/// the next `k` to fetch, or `None` when the current result is final.
///
/// Final when any of:
///   - at least `limit` in-session hits were surfaced (`in_session >= limit`);
///   - lookahead found no more row (`has_more == false`). Exact scan: the global
///     population is exhausted, so no further in-session candidate can exist. Partial
///     ANN index (T7.4): the beam exhausted its visited neighbourhoods, so a true
///     near neighbour may be missed — not provably complete (see the ANN dial doc);
///   - `k` is at `VECTOR_FETCH_CAP`.
///
/// Otherwise the page has more rows yet under-delivered — more global rows may hold
/// in-session candidates — so double `k` (capped) and retry.
fn next_fetch_k(in_session: usize, has_more: bool, k: usize, limit: usize) -> Option<usize> {
    if in_session >= limit || !has_more || k >= VECTOR_FETCH_CAP {
        None
    } else {
        Some((k.saturating_mul(VECTOR_FETCH_GROWTH)).min(VECTOR_FETCH_CAP))
    }
}

fn needs_session_fallback(in_session: usize, has_more: bool, k: usize, limit: usize) -> bool {
    in_session < limit && has_more && k >= VECTOR_FETCH_CAP
}

/// Parse the dense-vector column width out of the DDL (`VECTOR(n)`). The schema is the
/// authority on the width (spec §3.3 "Vector width: not a global constant"), so this is
/// read from the embedded `001_init.sql`, never a separate constant.
fn schema_vector_dim(ddl: &str) -> Option<usize> {
    let start = ddl.find("VECTOR(")?;
    let rest = &ddl[start + "VECTOR(".len()..];
    let end = rest.find(')')?;
    rest[..end].trim().parse().ok()
}

// `encode_vector` / `decode_vector` live in the shared `super::vector` module
// (CON-8: SQLite stores the same text form as a BLOB, so both adapters share
// one codec — see `store/vector.rs`).

/// Embeddings must match the schema column width before they ever reach SQL.
fn check_embedding_dim(v: &[f32], dim: usize) -> Result<(), StoreError> {
    if v.len() != dim {
        return Err(StoreError::Invariant(format!(
            "embedding dimension {} does not match store vector width {dim} (see vector_dimensions())",
            v.len()
        )));
    }
    Ok(())
}

/// `now - age`, mirroring `MemoryStore::cutoff` (error vocabulary included).
fn cutoff(now: DateTime<Utc>, age: Duration) -> Result<DateTime<Utc>, StoreError> {
    let d = chrono::Duration::from_std(age)
        .map_err(|e| StoreError::Backend(format!("age duration out of range: {e}")))?;
    Ok(now - d)
}

/// Keyword hit count for one candidate row, case-folded on BOTH sides (MemoryStore
/// parity). The SQL predicate matches `lower(content)`/`lower(canonical_key)`, so the
/// score must apply the same folding to the raw row text — a mixed-case row ("Register
/// User") matched by token "register" would otherwise be selected yet score 0.0
/// (P3 review R1). Tokens arrive pre-normalized (lowercased) from
/// [`CockroachStore::normalize_tokens`].
fn score_keyword_hits(content: &str, canonical_key: &str, tokens: &[String]) -> usize {
    let content = content.to_lowercase();
    let key = canonical_key.to_lowercase();
    tokens
        .iter()
        .filter(|t| content.contains(t.as_str()) || key.contains(t.as_str()))
        .count()
}

/// Build the keyword-candidate SQL for `n` tokens: `$1` = session, `$2..$n+1` = tokens
/// (each bound once, used twice — content and canonical_key). `strpos(lower(col), $k) > 0`
/// is exact-substring matching with no `LIKE` wildcard semantics (Rust `contains`
/// parity). Full scan is acceptable here — the RAM inverted index is the real path.
fn keyword_candidates_sql(n_tokens: usize) -> String {
    debug_assert!(n_tokens > 0);
    let mut sql = String::with_capacity(64 + n_tokens * 96);
    sql.push_str(
        "SELECT id::STRING AS id, content, canonical_key FROM concepts WHERE session_id = $1 AND (",
    );
    for i in 0..n_tokens {
        if i > 0 {
            sql.push_str(" OR ");
        }
        let n = i + 2;
        sql.push_str(&format!(
            "strpos(lower(content), ${n}) > 0 OR strpos(lower(canonical_key), ${n}) > 0"
        ));
    }
    sql.push(')');
    sql
}

// --- enum <-> STRING column mapping (SQL stores the serde PascalCase spellings) ---

fn concept_type_sql(ct: ConceptType) -> &'static str {
    match ct {
        ConceptType::Entity => "Entity",
        ConceptType::Logic => "Logic",
        ConceptType::Constraint => "Constraint",
        ConceptType::Resource => "Resource",
        ConceptType::Observation => "Observation",
    }
}

fn parse_concept_type(s: &str) -> Result<ConceptType, StoreError> {
    Ok(match s {
        "Entity" => ConceptType::Entity,
        "Logic" => ConceptType::Logic,
        "Constraint" => ConceptType::Constraint,
        "Resource" => ConceptType::Resource,
        "Observation" => ConceptType::Observation,
        other => return Err(backend(format!("unknown concept_type {other:?} in store"))),
    })
}

fn edge_type_sql(et: EdgeType) -> &'static str {
    match et {
        EdgeType::Temporal => "Temporal",
        EdgeType::Derives => "Derives",
        EdgeType::CoOccurrence => "CoOccurrence",
        EdgeType::Causal => "Causal",
        EdgeType::Dependency => "Dependency",
        EdgeType::Hierarchical => "Hierarchical",
        EdgeType::Semantic => "Semantic",
    }
}

fn parse_edge_type(s: &str) -> Result<EdgeType, StoreError> {
    Ok(match s {
        "Temporal" => EdgeType::Temporal,
        "Derives" => EdgeType::Derives,
        "CoOccurrence" => EdgeType::CoOccurrence,
        "Causal" => EdgeType::Causal,
        "Dependency" => EdgeType::Dependency,
        "Hierarchical" => EdgeType::Hierarchical,
        "Semantic" => EdgeType::Semantic,
        other => return Err(backend(format!("unknown edge_type {other:?} in store"))),
    })
}

fn canonization_status_sql(cs: CanonizationStatus) -> &'static str {
    match cs {
        CanonizationStatus::None => "None",
        CanonizationStatus::Candidate => "Candidate",
        CanonizationStatus::Venerable => "Venerable",
        CanonizationStatus::Canonical => "Canonical",
    }
}

fn parse_canonization_status(s: &str) -> Result<CanonizationStatus, StoreError> {
    Ok(match s {
        "None" => CanonizationStatus::None,
        "Candidate" => CanonizationStatus::Candidate,
        "Venerable" => CanonizationStatus::Venerable,
        "Canonical" => CanonizationStatus::Canonical,
        other => {
            return Err(backend(format!(
                "unknown canonization_status {other:?} in store"
            )))
        }
    })
}

// --- row -> Lambo-type mapping (load_session) ---

fn parse_node_id(s: &str) -> Result<NodeId, StoreError> {
    Uuid::parse_str(s)
        .map(NodeId)
        .map_err(|e| backend(format!("invalid node id {s:?}: {e}")))
}

fn row_to_interaction(row: &PgRow) -> Result<Interaction, StoreError> {
    let id: String = row.try_get("id").map_err(backend)?;
    let previous: Option<String> = row.try_get("previous_id").map_err(backend)?;
    Ok(Interaction {
        id: parse_node_id(&id)?,
        session_id: SessionId(row.try_get("session_id").map_err(backend)?),
        agent_id: crate::types::AgentId(row.try_get("agent_id").map_err(backend)?),
        prompt_text: row.try_get("prompt_text").map_err(backend)?,
        previous_id: previous.as_deref().map(parse_node_id).transpose()?,
        created_at: row.try_get("created_at").map_err(backend)?,
    })
}

fn row_to_concept(row: &PgRow) -> Result<Concept, StoreError> {
    let id: String = row.try_get("id").map_err(backend)?;
    let origin: String = row.try_get("origin_interaction").map_err(backend)?;
    let embedding: Option<String> = row.try_get("embedding").map_err(backend)?;
    // Cockroach `INT` is INT8 on the wire (all integer columns); Lambo types are i32.
    let access_count: i64 = row.try_get("access_count").map_err(backend)?;
    let gc_survived: i64 = row.try_get("gc_survived").map_err(backend)?;
    let blast_radius: Option<i64> = row.try_get("blast_radius").map_err(backend)?;
    Ok(Concept {
        id: parse_node_id(&id)?,
        session_id: SessionId(row.try_get("session_id").map_err(backend)?),
        content: row.try_get("content").map_err(backend)?,
        canonical_key: row.try_get("canonical_key").map_err(backend)?,
        concept_type: parse_concept_type(
            &row.try_get::<String, _>("concept_type").map_err(backend)?,
        )?,
        origin_interaction: parse_node_id(&origin)?,
        origin_agent: crate::types::AgentId(row.try_get("origin_agent").map_err(backend)?),
        created_at: row.try_get("created_at").map_err(backend)?,
        access_count: access_count as i32,
        last_accessed: row.try_get("last_accessed").map_err(backend)?,
        gc_survived: gc_survived as i32,
        canonization_status: parse_canonization_status(
            &row.try_get::<String, _>("canonization_status")
                .map_err(backend)?,
        )?,
        blast_radius: blast_radius.map(|v| v as i32),
        last_demotion_time: row.try_get("last_demotion_time").map_err(backend)?,
        embedding: embedding.as_deref().map(decode_vector).transpose()?,
        chunk_group_id: row.try_get("chunk_group_id").map_err(backend)?,
    })
}

fn row_to_edge(row: &PgRow) -> Result<Edge, StoreError> {
    let id: String = row.try_get("id").map_err(backend)?;
    let source: String = row.try_get("source").map_err(backend)?;
    let target: String = row.try_get("target").map_err(backend)?;
    let reinforcements: i64 = row.try_get("reinforcements").map_err(backend)?;
    Ok(Edge {
        id: parse_node_id(&id)?,
        session_id: SessionId(row.try_get("session_id").map_err(backend)?),
        source: parse_node_id(&source)?,
        target: parse_node_id(&target)?,
        edge_type: parse_edge_type(&row.try_get::<String, _>("edge_type").map_err(backend)?)?,
        weight: row.try_get("weight").map_err(backend)?,
        reinforcements: reinforcements as i32,
        created_at: row.try_get("created_at").map_err(backend)?,
        last_reinforced: row.try_get("last_reinforced").map_err(backend)?,
    })
}

fn row_to_synonym(row: &PgRow) -> Result<Synonym, StoreError> {
    Ok(Synonym {
        session_id: SessionId(row.try_get("session_id").map_err(backend)?),
        source_key: row.try_get("source_key").map_err(backend)?,
        canonical_key: row.try_get("canonical_key").map_err(backend)?,
    })
}

fn row_to_reservation(row: &PgRow) -> Result<Reservation, StoreError> {
    let node_id: String = row.try_get("node_id").map_err(backend)?;
    Ok(Reservation {
        session_id: SessionId(row.try_get("session_id").map_err(backend)?),
        node_id: parse_node_id(&node_id)?,
        agent_id: crate::types::AgentId(row.try_get("agent_id").map_err(backend)?),
        expires_at: row.try_get("expires_at").map_err(backend)?,
    })
}

fn row_to_canonization_event(row: &PgRow) -> Result<CanonizationEvent, StoreError> {
    let id: String = row.try_get("id").map_err(backend)?;
    let node_id: String = row.try_get("node_id").map_err(backend)?;
    let blast_radius: Option<i64> = row.try_get("blast_radius").map_err(backend)?;
    Ok(CanonizationEvent {
        id: parse_node_id(&id)?,
        session_id: SessionId(row.try_get("session_id").map_err(backend)?),
        node_id: parse_node_id(&node_id)?,
        from_status: parse_canonization_status(
            &row.try_get::<String, _>("from_status").map_err(backend)?,
        )?,
        to_status: parse_canonization_status(
            &row.try_get::<String, _>("to_status").map_err(backend)?,
        )?,
        blast_radius: blast_radius.map(|v| v as i32),
        last_demotion_time: row.try_get("last_demotion_time").map_err(backend)?,
        occurred_at: row.try_get("occurred_at").map_err(backend)?,
    })
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// Durable `GraphStore` over a CockroachDB cluster (spec §3.3 "hackathon primary").
///
/// Constructed by [`super::build_store`] from a [`StoreConfig`]. Pool creation is
/// deferred to the first query ([`tokio::sync::OnceCell`]): sqlx pools require a Tokio
/// context at creation (they spawn a maintenance task), and `build_store` is a sync,
/// I/O-free constructor — it must work from `#[test]`s and process start alike. The DSN
/// is still parse-validated at construction (fail fast on typos), and the pool itself is
/// `connect_lazy`, so even the first creation never touches the network.
pub struct CockroachStore {
    /// rustls-rewritten DSN (see [`dsn_for_rustls`]).
    dsn: String,
    /// Dense-vector column width parsed from the embedded DDL (`VECTOR(n)`).
    vector_dim: usize,
    pool: tokio::sync::OnceCell<PgPool>,
}

impl CockroachStore {
    pub fn new(cfg: StoreConfig) -> Result<Self, StoreError> {
        let dsn = cfg.dsn.ok_or_else(|| {
            StoreError::Backend(
                "CockroachStore requires a DSN (store.dsn or LAMBO_COCKROACH_DSN)".into(),
            )
        })?;
        // sqlx + rustls cannot open libpq's `sslrootcert=system`; see module doc.
        let dsn = dsn_for_rustls(&dsn);
        // Parse-validate without a runtime; the actual pool is built lazily on first use.
        dsn.parse::<sqlx::postgres::PgConnectOptions>()
            .map_err(|e| backend(format!("invalid Cockroach DSN: {e}")))?;
        let vector_dim = schema_vector_dim(INIT_SQL).ok_or_else(|| {
            StoreError::Backend(
                "could not parse VECTOR(n) column width from migrations/cockroach/001_init.sql"
                    .into(),
            )
        })?;
        Ok(Self {
            dsn,
            vector_dim,
            pool: tokio::sync::OnceCell::new(),
        })
    }

    /// The lazily-created pool (Tokio context required — call from an async method).
    async fn pool(&self) -> Result<&PgPool, StoreError> {
        self.pool
            .get_or_try_init(|| async {
                Ok(PgPoolOptions::new()
                    .max_connections(MAX_POOL_CONNECTIONS)
                    .connect_lazy_with(Self::connect_options(&self.dsn)?))
            })
            .await
    }

    /// Build the per-connection options. **Synchronous on purpose:** it reads
    /// the environment, and a test that wants to pin an env-driven option must
    /// be able to do so without holding a lock across an `.await` (spec §6.4,
    /// enforced by `clippy::await_holding_lock`).
    fn connect_options(dsn: &str) -> Result<sqlx::postgres::PgConnectOptions, StoreError> {
        let options = dsn
            .parse::<sqlx::postgres::PgConnectOptions>()
            .map_err(|e| backend(format!("invalid Cockroach DSN: {e}")))?
            // STORE-2: bound every statement server-side.
            // statement_timeout applies per statement, not per
            // transaction — a multi-statement flush batch can take
            // N x 20s. The whole-batch bound is the client-side
            // flush attempt timeout (FLUSH_ATTEMPT_TIMEOUT); the
            // per-statement bound stays below it so the DB aborts a
            // hung statement before the client gives up on the
            // attempt (a hung statement must never wedge the flush
            // loop).
            .options([(
                "statement_timeout",
                format!("{}s", STATEMENT_TIMEOUT.as_secs()),
            )]);
        // T7.4: ANN accuracy dial, applied per connection so it costs no
        // per-query round trip. Unset => DEFAULT_VECTOR_BEAM_SIZE (measured —
        // see its doc comment); an invalid value already failed closed above.
        let beam = vector_beam_size_from_env()?.unwrap_or(DEFAULT_VECTOR_BEAM_SIZE);
        let options = options.options([("vector_search_beam_size", beam.to_string())]);
        Ok(options)
    }
    /// Seed a prebuilt snapshot directly (fixtures track, MemoryStore parity). Writes all
    /// seven tables in one transaction — the full-snapshot path that carries synonyms and
    /// reservations (they have no `Mutation` kind, S5 contract). Also persists
    /// `GraphSnapshot.embedding` into `sessions.embedding_{kind,model,dim}` (STORE-1),
    /// so a seeded contract survives restarts instead of being dropped.
    #[cfg(feature = "fixtures")]
    pub async fn seed(&self, snapshot: &GraphSnapshot) -> Result<(), StoreError> {
        let sid = &snapshot.session_id.0;
        let embedding_dim = snapshot
            .embedding
            .as_ref()
            .map(|contract| i64::try_from(contract.dim))
            .transpose()
            .map_err(|_| StoreError::Invariant("embedding dimension does not fit i64".into()))?;
        let pool = self.pool().await?;
        let root_goal = snapshot
            .root_goal
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| backend(format!("serialize root_goal: {e}")))?;
        // Copy handle (Option<&str>): the FnMut body runs once per retry attempt and
        // must not move the owned String into the first attempt's future.
        let root_goal = root_goal.as_deref();
        let embedding = snapshot.embedding.as_ref();
        // Copy handles for the same FnMut-reborrow reason as root_goal.
        let embedding_kind = embedding.map(|c| c.kind.as_str());
        let embedding_model = embedding.and_then(|c| c.model.as_deref());
        tx_retry(|| async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| map_write_err(e, |m| format!("begin seed transaction: {m}")))?;
            sqlx::query(UPSERT_SESSION_SQL)
                .bind(sid)
                .bind(root_goal)
                .bind(snapshot.created_at)
                .bind(snapshot.closed_at)
                .bind(embedding_kind)
                .bind(embedding_model)
                .bind(embedding_dim)
                .execute(&mut *tx)
                .await
                .map_err(|e| map_write_err(e, |m| format!("upsert session row: {m}")))?;
            for i in &snapshot.interactions {
                upsert_interaction(&mut *tx, i).await?;
            }
            for c in &snapshot.concepts {
                upsert_concept(&mut *tx, c).await?;
            }
            for e in &snapshot.edges {
                upsert_edge(&mut *tx, e).await?;
            }
            for s in &snapshot.synonyms {
                sqlx::query(UPSERT_SYNONYM_SQL)
                    .bind(&s.session_id.0)
                    .bind(&s.source_key)
                    .bind(&s.canonical_key)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| map_write_err(e, |m| format!("upsert synonym: {m}")))?;
            }
            for r in &snapshot.reservations {
                sqlx::query(UPSERT_RESERVATION_SQL)
                    .bind(&r.session_id.0)
                    .bind(r.node_id.0)
                    .bind(&r.agent_id.0)
                    .bind(r.expires_at)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| map_write_err(e, |m| format!("upsert reservation: {m}")))?;
            }
            for ev in &snapshot.canonization_events {
                insert_canonization_event(&mut *tx, ev).await?;
            }
            tx.commit()
                .await
                .map_err(|e| map_write_err(e, |m| format!("commit seed transaction: {m}")))?;
            Ok(())
        })
        .await
    }

    async fn session_exists(&self, session: &SessionId) -> Result<bool, StoreError> {
        let pool = self.pool().await?;
        let row = sqlx::query("SELECT 1 AS one FROM sessions WHERE session_id = $1")
            .bind(&session.0)
            .fetch_optional(pool)
            .await
            .map_err(backend)?;
        Ok(row.is_some())
    }

    /// Return whether vector candidates are trusted for this session. Missing
    /// and fully unstamped rows are safe empty-search states: hybrid first use
    /// must gather before its atomic commit emits `SetEmbedding`. Partial
    /// metadata remains a hard corruption error.
    async fn has_vector_contract(&self, session: &SessionId) -> Result<bool, StoreError> {
        let pool = self.pool().await?;
        let Some(row) = sqlx::query(SELECT_SESSION_SQL)
            .bind(session.as_str())
            .fetch_optional(pool)
            .await
            .map_err(backend)?
        else {
            return Ok(false);
        };
        let trusted = vector_contract_allows_query(
            row.try_get("embedding_kind").map_err(backend)?,
            row.try_get("embedding_model").map_err(backend)?,
            row.try_get("embedding_dim").map_err(backend)?,
            session.as_str(),
        )?;
        Ok(trusted)
    }

    /// Normalized keyword tokens (MemoryStore parity: trim + lowercase, drop empties).
    fn normalize_tokens(tokens: &[String]) -> Vec<String> {
        tokens
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect()
    }
}

// --- statement helpers (shared by flush and seed) ---

async fn upsert_interaction(
    tx: &mut sqlx::PgConnection,
    i: &Interaction,
) -> Result<(), StoreError> {
    sqlx::query(UPSERT_INTERACTION_SQL)
        .bind(i.id.0)
        .bind(&i.session_id.0)
        .bind(&i.agent_id.0)
        .bind(&i.prompt_text)
        .bind(i.previous_id.map(|n| n.0))
        .bind(i.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert interaction: {m}")))?;
    Ok(())
}

async fn upsert_concept(tx: &mut sqlx::PgConnection, c: &Concept) -> Result<(), StoreError> {
    let embedding = match &c.embedding {
        Some(v) => Some(encode_vector(v)?),
        None => None,
    };
    sqlx::query(UPSERT_CONCEPT_SQL)
        .bind(c.id.0)
        .bind(&c.session_id.0)
        .bind(&c.content)
        .bind(&c.canonical_key)
        .bind(concept_type_sql(c.concept_type))
        .bind(c.origin_interaction.0)
        .bind(&c.origin_agent.0)
        .bind(c.created_at)
        .bind(c.access_count)
        .bind(c.last_accessed)
        .bind(c.gc_survived)
        .bind(canonization_status_sql(c.canonization_status))
        .bind(c.blast_radius)
        .bind(c.last_demotion_time)
        .bind(embedding)
        .bind(c.chunk_group_id.clone())
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert concept: {m}")))?;
    Ok(())
}

async fn upsert_edge(tx: &mut sqlx::PgConnection, e: &Edge) -> Result<(), StoreError> {
    sqlx::query(UPSERT_EDGE_SQL)
        .bind(e.id.0)
        .bind(&e.session_id.0)
        .bind(e.source.0)
        .bind(e.target.0)
        .bind(edge_type_sql(e.edge_type))
        .bind(e.weight)
        .bind(e.reinforcements)
        .bind(e.created_at)
        .bind(e.last_reinforced)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert edge: {m}")))?;
    Ok(())
}

/// Append the audit row; `false` when the id was already there (the
/// `ON CONFLICT (id) DO NOTHING` dedupe fired).
async fn insert_canonization_event(
    tx: &mut sqlx::PgConnection,
    ev: &CanonizationEvent,
) -> Result<bool, StoreError> {
    let res = sqlx::query(INSERT_CANONIZATION_EVENT_SQL)
        .bind(ev.id.0)
        .bind(&ev.session_id.0)
        .bind(ev.node_id.0)
        .bind(canonization_status_sql(ev.from_status))
        .bind(canonization_status_sql(ev.to_status))
        .bind(ev.blast_radius)
        .bind(ev.last_demotion_time)
        .bind(ev.occurred_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("insert canonization event: {m}")))?;
    Ok(res.rows_affected() > 0)
}

/// Apply a canonization transition: append the audit event, then update the
/// concept row. Missing concept → `NotFound` (MemoryStore parity).
///
/// **F12 — the audit row is the idempotency key.** The evaluator dual-writes
/// (`record_canonization` immediately, the same transition again when the
/// write-behind log flushes), and the two are not ordered against each other:
/// a lagging flush of hop 1 landing after hop 2's immediate write would
/// otherwise *regress* the durable status, and a crash before hop 2's own
/// flush would leave the reload showing a status the audit already moved past
/// — after which the evaluator re-promotes under a fresh event id and the same
/// hop appears twice in the on-screen audit. So the INSERT goes first: if its
/// `ON CONFLICT (id) DO NOTHING` fires, this transition's effect is already in
/// the row and the UPDATE is skipped. Both statements share the caller's
/// transaction, so the ordering swap costs nothing on the first write.
///
/// **R2-1 — what makes "already in the row" true.** Skipping the UPDATE is
/// only sound while nothing else writes those three columns.
/// `UPSERT_CONCEPT_SQL` used to, from a possibly stale
/// `Mutation::UpsertNode` snapshot, so a batch shaped
/// `[UpsertNode(stale), CanonizationTransition(already recorded)]` left the
/// row regressed *and* the repair skipped. It no longer does — see
/// `UPSERT_CONCEPT_SQL` and `Mutation::UpsertNode`.
async fn apply_canonization(
    tx: &mut sqlx::PgConnection,
    ev: &CanonizationEvent,
) -> Result<(), StoreError> {
    if !insert_canonization_event(tx, ev).await? {
        return Ok(());
    }
    let res = sqlx::query(UPDATE_CONCEPT_STATUS_SQL)
        .bind(ev.node_id.0)
        .bind(canonization_status_sql(ev.to_status))
        .bind(ev.blast_radius)
        .bind(&ev.session_id.0)
        .bind(ev.last_demotion_time)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("apply canonization transition: {m}")))?;
    if res.rows_affected() == 0 {
        return Err(StoreError::NotFound(format!(
            "concept {} for canonization",
            ev.node_id
        )));
    }
    Ok(())
}

#[async_trait]
impl GraphStore for CockroachStore {
    async fn init_schema(&self) -> Result<(), StoreError> {
        // Multi-statement DDL via the simple protocol (raw_sql); every statement is
        // `IF NOT EXISTS`, so this is idempotent by construction (T3.1 acceptance).
        let pool = self.pool().await?;
        sqlx::raw_sql(INIT_SQL)
            .execute(pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::VECTOR_SEARCH
    }

    fn vector_dimensions(&self) -> Option<usize> {
        Some(self.vector_dim)
    }

    async fn flush(&self, batch: &MutationBatch) -> Result<(), StoreError> {
        let pool = self.pool().await?;
        tx_retry(|| async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| map_write_err(e, |m| format!("begin flush transaction: {m}")))?;
            // Ensure a sessions row for every session the batch writes into — the DDL
            // enforces `REFERENCES sessions(session_id)` on interactions/concepts, and the
            // graph tier creates sessions implicitly (MemoryStore::ensure_session parity).
            let mut sids: Vec<String> = Vec::new();
            for m in &batch.mutations {
                let sid = match m {
                    Mutation::UpsertNode { node } => node.session_id().as_str(),
                    Mutation::UpsertEdge { edge } => edge.session_id.as_str(),
                    Mutation::CanonizationTransition { event } => event.session_id.as_str(),
                    Mutation::SetRootGoal { session_id, .. } => session_id.as_str(),
                    Mutation::SetEmbedding { session_id, .. } => session_id.as_str(),
                    Mutation::DeleteNode { .. } | Mutation::DeleteEdge { .. } => continue,
                };
                if !sids.iter().any(|s| s == sid) {
                    sids.push(sid.to_string());
                }
            }
            for sid in &sids {
                sqlx::query(UPSERT_SESSION_ROW_SQL)
                    .bind(sid)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| map_write_err(e, |m| format!("upsert session row: {m}")))?;
            }

            // Replay in submission order — see module doc (T2.1 M2: MUST NOT re-sort).
            for m in &batch.mutations {
                match m {
                    Mutation::UpsertNode { node } => match node {
                        Node::Interaction(i) => upsert_interaction(&mut *tx, i).await?,
                        Node::Concept(c) => upsert_concept(&mut *tx, c).await?,
                    },
                    Mutation::UpsertEdge { edge } => upsert_edge(&mut *tx, edge).await?,
                    Mutation::DeleteNode { id } => {
                        // Explicit incident-edge cleanup: edges carry no FK on source/target
                        // (spec §4); delete the node row from both node tables (interaction
                        // deletes are unreachable under the graph contract — see module doc).
                        sqlx::query(DELETE_NODE_EDGES_SQL)
                            .bind(id.0)
                            .execute(&mut *tx)
                            .await
                            .map_err(|e| map_write_err(e, |m| format!("delete node edges: {m}")))?;
                        sqlx::query(DELETE_NODE_CONCEPTS_SQL)
                            .bind(id.0)
                            .execute(&mut *tx)
                            .await
                            .map_err(|e| {
                                map_write_err(e, |m| format!("delete node concepts: {m}"))
                            })?;
                        sqlx::query(DELETE_NODE_INTERACTIONS_SQL)
                            .bind(id.0)
                            .execute(&mut *tx)
                            .await
                            .map_err(|e| {
                                map_write_err(e, |m| format!("delete node interactions: {m}"))
                            })?;
                    }
                    Mutation::DeleteEdge { id } => {
                        sqlx::query(DELETE_EDGE_SQL)
                            .bind(id.0)
                            .execute(&mut *tx)
                            .await
                            .map_err(|e| map_write_err(e, |m| format!("delete edge: {m}")))?;
                    }
                    Mutation::CanonizationTransition { event } => {
                        apply_canonization(&mut *tx, event).await?;
                    }
                    Mutation::SetRootGoal { session_id, goal } => {
                        let encoded = goal
                            .as_ref()
                            .map(serde_json::to_string)
                            .transpose()
                            .map_err(|e| backend(format!("serialize root_goal: {e}")))?;
                        let res = sqlx::query(SET_ROOT_GOAL_SQL)
                            .bind(session_id.as_str())
                            .bind(encoded.as_deref())
                            .execute(&mut *tx)
                            .await
                            .map_err(|e| map_write_err(e, |m| format!("set root_goal: {m}")))?;
                        if res.rows_affected() == 0 {
                            return Err(StoreError::NotFound(format!(
                                "sessions row for {session_id} while setting root_goal"
                            )));
                        }
                    }
                    Mutation::SetEmbedding {
                        session_id,
                        embedding,
                    } => {
                        if embedding.is_some() {
                            sqlx::query(QUARANTINE_LEGACY_EMBEDDINGS_SQL)
                                .bind(session_id.as_str())
                                .execute(&mut *tx)
                                .await
                                .map_err(|e| {
                                    map_write_err(e, |m| {
                                        format!("quarantine legacy embeddings: {m}")
                                    })
                                })?;
                        }
                        let res = sqlx::query(SET_EMBEDDING_SQL)
                            .bind(session_id.as_str())
                            .bind(embedding.as_ref().map(|e| e.kind.as_str()))
                            .bind(embedding.as_ref().and_then(|e| e.model.as_deref()))
                            .bind(
                                embedding
                                    .as_ref()
                                    .map(|e| i64::try_from(e.dim))
                                    .transpose()
                                    .map_err(|_| {
                                        StoreError::Invariant(format!(
                                            "embedding dimension does not fit i64 for {session_id}"
                                        ))
                                    })?,
                            )
                            .execute(&mut *tx)
                            .await
                            .map_err(|e| map_write_err(e, |m| format!("set embedding: {m}")))?;
                        if res.rows_affected() == 0 {
                            return Err(StoreError::NotFound(format!(
                                "sessions row for {session_id} while setting embedding"
                            )));
                        }
                    }
                }
            }
            tx.commit()
                .await
                .map_err(|e| map_write_err(e, |m| format!("commit flush transaction: {m}")))?;
            Ok(())
        })
        .await
    }

    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        let pool = self.pool().await?;
        let session_id = session.clone();
        // Copy handle (&SessionId): the FnMut body runs once per retry attempt.
        let sid = &session_id;
        tx_retry(|| async move {
            let mut tx = pool.begin().await.map_err(backend)?;
            let session_row = sqlx::query(SELECT_SESSION_SQL)
                .bind(sid.0.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(backend)?;
            let Some(session_row) = session_row else {
                return Err(StoreError::SessionNotFound(sid.0.clone()));
            };
            let root_goal: Option<String> = session_row.try_get("root_goal").map_err(backend)?;
            let root_goal = root_goal
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|e| backend(format!("parse root_goal JSONB: {e}")))?;

            // Ordered `SetEmbedding` mutations and full-snapshot seed both persist
            // the nullable kind/model/dim columns. STORE-7: a row with exactly one of
            // embedding_kind / embedding_dim set (kind XOR dim) is a corruption
            // error — mirroring sqlite — never a silent `None` (see
            // `session_embedding_from_parts`).
            let embedding_kind: Option<String> =
                session_row.try_get("embedding_kind").map_err(backend)?;
            let embedding_model: Option<String> =
                session_row.try_get("embedding_model").map_err(backend)?;
            let embedding_dim: Option<i64> =
                session_row.try_get("embedding_dim").map_err(backend)?;
            let embedding = session_embedding_from_parts(
                embedding_kind,
                embedding_model,
                embedding_dim,
                sid.0.as_str(),
            )?;

            let interactions = sqlx::query(SELECT_INTERACTIONS_SQL)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_interaction)
                .collect::<Result<Vec<_>, _>>()?;

            let concepts = sqlx::query(SELECT_CONCEPTS_SQL)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_concept)
                .collect::<Result<Vec<_>, _>>()?;

            let edges = sqlx::query(SELECT_EDGES_SQL)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_edge)
                .collect::<Result<Vec<_>, _>>()?;

            let synonyms = sqlx::query(SELECT_SYNONYMS_SQL)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_synonym)
                .collect::<Result<Vec<_>, _>>()?;

            let reservations = sqlx::query(SELECT_RESERVATIONS_SQL)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_reservation)
                .collect::<Result<Vec<_>, _>>()?;

            let canonization_events = sqlx::query(SELECT_CANONIZATION_EVENTS_SQL)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_canonization_event)
                .collect::<Result<Vec<_>, _>>()?;

            tx.commit().await.map_err(backend)?;
            Ok(GraphSnapshot {
                session_id: sid.clone(),
                root_goal,
                created_at: session_row.try_get("created_at").map_err(backend)?,
                closed_at: session_row.try_get("closed_at").map_err(backend)?,
                interactions,
                concepts,
                edges,
                synonyms,
                reservations,
                canonization_events,
                embedding,
            })
        })
        .await
    }

    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        let tokens = Self::normalize_tokens(tokens);
        if tokens.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        if !self.session_exists(session).await? {
            return Err(StoreError::SessionNotFound(session.0.clone()));
        }
        let pool = self.pool().await?;
        let sql = keyword_candidates_sql(tokens.len());
        let mut q = sqlx::query(&sql).bind(&session.0);
        for t in &tokens {
            q = q.bind(t);
        }
        let rows = q.fetch_all(pool).await.map_err(backend)?;

        let mut scored: Vec<Scored<NodeId>> = rows
            .iter()
            .map(|r| {
                let id: String = r.try_get("id").map_err(backend)?;
                let content: String = r.try_get("content").map_err(backend)?;
                let key: String = r.try_get("canonical_key").map_err(backend)?;
                let hits = score_keyword_hits(&content, &key, &tokens);
                Ok(Scored::new(parse_node_id(&id)?, hits as f64))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;

        // MemoryStore parity: score desc, id asc tie-break.
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.item.0.cmp(&b.item.0))
        });
        scored.truncate(limit);
        Ok(scored)
    }

    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        validate_vector_candidate_limit(limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        check_embedding_dim(embedding, self.vector_dim)?;
        if !self.has_vector_contract(session).await? {
            return Ok(Vec::new());
        }
        let pool = self.pool().await?;
        let probe = encode_vector(embedding)?;

        // DECISION D1: GLOBAL index-backed top-k (`concepts@concepts_embedding_idx`),
        // then Rust-side session filter. `k` starts generous (limit × multiplier) and
        // grows via [`next_fetch_k`] when a full page still under-delivers in-session
        // hits — bounding under-return while never reading outside the global top-k.
        let mut k = initial_fetch_k(limit);
        loop {
            let fetch = k
                .checked_add(1)
                .ok_or_else(|| StoreError::Invariant("vector fetch window overflow".into()))?;
            let fetch = i64::try_from(fetch).map_err(|_| {
                StoreError::Invariant("vector fetch window does not fit i64".into())
            })?;
            let rows = sqlx::query(VECTOR_CANDIDATES_SQL)
                .bind(&probe)
                .bind(fetch)
                .fetch_all(pool)
                .await
                .map_err(backend)?;

            // (id, dist, session_id) — session_id selected so foreign rows can be dropped.
            let parsed = rows
                .iter()
                .map(|r| {
                    let id: String = r.try_get("id").map_err(backend)?;
                    let dist: f64 = r.try_get("dist").map_err(backend)?;
                    let sid: String = r.try_get("session_id").map_err(backend)?;
                    Ok((parse_node_id(&id)?, dist, sid))
                })
                .collect::<Result<Vec<_>, StoreError>>()?;

            // Fetch one lookahead row. If it ties the kth boundary distance,
            // SQL's arbitrary subset of that tie group cannot be made
            // deterministic in Rust; switch to the exact session query.
            let boundary_tie = has_boundary_tie(&parsed, k);
            let has_more = parsed.len() > k;
            let page_len = parsed.len().min(k);
            let mut in_session = filter_session_rows(session, &parsed[..page_len]);
            if boundary_tie || needs_session_fallback(in_session.len(), has_more, k, limit) {
                let exact_limit = i64::try_from(limit).map_err(|_| {
                    StoreError::Invariant("vector candidate limit does not fit i64".into())
                })?;
                let fallback_rows = sqlx::query(SESSION_VECTOR_CANDIDATES_SQL)
                    .bind(&probe)
                    .bind(session.as_str())
                    .bind(exact_limit)
                    .fetch_all(pool)
                    .await
                    .map_err(backend)?;
                return fallback_rows
                    .iter()
                    .map(|row| {
                        let id: String = row.try_get("id").map_err(backend)?;
                        let dist: f64 = row.try_get("dist").map_err(backend)?;
                        let score = distance_to_score(dist);
                        if !score.is_finite() {
                            return Err(StoreError::Backend(format!(
                                "non-finite vector distance for concept {id}"
                            )));
                        }
                        Ok(Scored::new(parse_node_id(&id)?, score))
                    })
                    .collect();
            }
            match next_fetch_k(in_session.len(), has_more, k, limit) {
                None => {
                    // Query returns rows in dist-asc (= score-desc); filter preserves that
                    // order (filter_session_rows). Truncate to the requested limit.
                    in_session.truncate(limit);
                    return Ok(in_session);
                }
                Some(next) => {
                    k = next;
                }
            }
        }
    }

    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        if !self.session_exists(session).await? {
            return Err(StoreError::SessionNotFound(session.0.clone()));
        }
        let pool = self.pool().await?;
        // F8: the cutoff anchor is the caller's `now`, never a wall clock here.
        let cutoff = cutoff(now, min_edge_age)?;
        let row = sqlx::query(BLAST_RADIUS_SQL)
            .bind(&session.0)
            .bind(node.0)
            .bind(cutoff)
            .fetch_one(pool)
            .await
            .map_err(backend)?;
        let n: i64 = row.try_get("n").map_err(backend)?;
        Ok(n as u64)
    }

    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<InteractionSpan, StoreError> {
        if !self.session_exists(session).await? {
            return Err(StoreError::SessionNotFound(session.0.clone()));
        }
        let pool = self.pool().await?;
        // F8: the cutoff anchor is the caller's `now`, never a wall clock here.
        let cutoff = cutoff(now, min_age)?;
        let row = sqlx::query(INTERACTION_SPAN_SQL)
            .bind(&session.0)
            .bind(node.0)
            .bind(cutoff)
            .fetch_one(pool)
            .await
            .map_err(backend)?;
        let distinct: i64 = row.try_get("distinct_count").map_err(backend)?;
        let coverage: f64 = row.try_get("coverage").map_err(backend)?;
        Ok(InteractionSpan {
            distinct: distinct as u64,
            coverage,
        })
    }

    async fn record_canonization(&self, event: &CanonizationEvent) -> Result<(), StoreError> {
        let pool = self.pool().await?;
        tx_retry(|| async move {
            let mut tx = pool.begin().await.map_err(|e| {
                map_write_err(e, |m| format!("begin record_canonization transaction: {m}"))
            })?;
            apply_canonization(&mut *tx, event).await?;
            tx.commit().await.map_err(|e| {
                map_write_err(e, |m| {
                    format!("commit record_canonization transaction: {m}")
                })
            })?;
            Ok(())
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Pure-logic unit tests (no cluster)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// T7.4: the ANN accuracy dial parses and fails closed. A tuning knob that
    /// is silently ignored on a typo is worse than no knob — the operator
    /// believes accuracy was raised when it was not.
    #[test]
    fn vector_beam_size_env_parses_and_fails_closed() {
        let _g = crate::test_util::env_lock();
        let restore = std::env::var(VECTOR_BEAM_SIZE_ENV).ok();

        std::env::remove_var(VECTOR_BEAM_SIZE_ENV);
        assert_eq!(
            vector_beam_size_from_env().unwrap(),
            None,
            "the parser reports absence; the DEFAULT is applied at the call site"
        );
        // Pin the measured default (adve-review MAJOR-1). 32 is CockroachDB's
        // default and measured ~6-7% worse on recall; 256 measured WORSE than
        // 64. If this constant changes, the measurement in its doc must be
        // redone — it is evidence-backed, not a taste call.
        assert_eq!(DEFAULT_VECTOR_BEAM_SIZE, 64);
        assert!(
            (VECTOR_BEAM_SIZE_MIN..=VECTOR_BEAM_SIZE_MAX).contains(&DEFAULT_VECTOR_BEAM_SIZE),
            "default must satisfy the server's own bounds"
        );

        // Exported-but-blank behaves as absent (same convention as LAMBO_STORE).
        std::env::set_var(VECTOR_BEAM_SIZE_ENV, "");
        assert_eq!(vector_beam_size_from_env().unwrap(), None);
        std::env::set_var(VECTOR_BEAM_SIZE_ENV, "   ");
        assert_eq!(vector_beam_size_from_env().unwrap(), None);

        std::env::set_var(VECTOR_BEAM_SIZE_ENV, "128");
        assert_eq!(vector_beam_size_from_env().unwrap(), Some(128));
        // Server-enforced bounds, verified live 2026-08-13.
        std::env::set_var(VECTOR_BEAM_SIZE_ENV, "1");
        assert_eq!(vector_beam_size_from_env().unwrap(), Some(1));
        std::env::set_var(VECTOR_BEAM_SIZE_ENV, "2048");
        assert_eq!(vector_beam_size_from_env().unwrap(), Some(2048));

        for bad in ["0", "2049", "-1", "64.5", "many", "1e3"] {
            std::env::set_var(VECTOR_BEAM_SIZE_ENV, bad);
            assert!(
                vector_beam_size_from_env().is_err(),
                "{bad:?} must be rejected at pool construction, not silently dropped"
            );
        }

        match restore {
            Some(v) => std::env::set_var(VECTOR_BEAM_SIZE_ENV, v),
            None => std::env::remove_var(VECTOR_BEAM_SIZE_ENV),
        }
    }

    // Vector codec tests (roundtrip / rendering / non-finite) live in the shared
    // `super::vector` module — SQLite stores the same text form, so the codec's
    // coverage must run under either store feature (CON-8).

    #[test]
    fn schema_vector_dim_reads_ddl_width() {
        assert_eq!(schema_vector_dim(INIT_SQL), Some(1024));
        assert_eq!(schema_vector_dim("embedding VECTOR(768)"), Some(768));
        assert_eq!(schema_vector_dim("no vector here"), None);
        assert_eq!(schema_vector_dim("VECTOR(x)"), None);
        // The DDL is the authority: a schema change flows into vector_dimensions().
        assert_eq!(schema_vector_dim(INIT_SQL).unwrap(), 1024);
    }

    #[test]
    fn dsn_for_rustls_rewrites_sslrootcert_system() {
        let out =
            dsn_for_rustls("postgresql://u:p@h:26257/db?sslmode=verify-full&sslrootcert=system");
        assert!(!out.contains("sslrootcert=system"), "{out}");
        let has_bundle = [
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/pki/tls/certs/ca-bundle.crt",
            "/etc/ssl/cert.pem",
            "/etc/ssl/ca-bundle.pem",
        ]
        .iter()
        .any(|p| std::path::Path::new(p).is_file());
        if has_bundle {
            assert!(
                out.contains("sslrootcert=/") && !out.contains("system"),
                "{out}"
            );
        } else {
            assert!(!out.contains("sslmode=verify-full"), "downgraded: {out}");
        }
        // Dangling separators cleaned.
        assert!(
            !out.contains("?&") && !out.ends_with('&') && !out.ends_with('?'),
            "{out}"
        );
        // Untouched DSN passes through unchanged.
        let plain = "postgresql://u:p@h:26257/db?sslmode=require";
        assert_eq!(dsn_for_rustls(plain), plain);
    }

    #[test]
    fn age_cutoff_computation() {
        let now = Utc.with_ymd_and_hms(2026, 8, 11, 12, 0, 0).unwrap();
        assert_eq!(cutoff(now, Duration::ZERO).unwrap(), now);
        assert_eq!(
            cutoff(now, Duration::from_secs(3600)).unwrap(),
            now - chrono::Duration::hours(1)
        );
        assert_eq!(
            cutoff(now, Duration::from_secs(90)).unwrap(),
            now - chrono::Duration::seconds(90)
        );
        // Out-of-range (chrono i64 seconds) -> typed error, not panic.
        assert!(cutoff(now, Duration::from_secs(u64::MAX)).is_err());
    }

    #[test]
    fn distance_to_score_is_cosine() {
        assert_eq!(distance_to_score(0.0), 1.0);
        assert!((distance_to_score(1.0) - 0.5).abs() < 1e-12);
        assert_eq!(distance_to_score(2.0), -1.0);
        assert_eq!(distance_to_score(3.0), -1.0, "clamped");
        assert!((distance_to_score(0.5) - 0.875).abs() < 1e-12);
    }

    #[test]
    fn embedding_dim_check() {
        assert!(check_embedding_dim(&[0.0; 1024], 1024).is_ok());
        let err = check_embedding_dim(&[0.0; 8], 1024).unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)));
    }
    #[test]
    fn session_filter_keeps_only_caller_and_preserves_order() {
        // Rows arrive from SQL in L2-distance-ascending order (score-descending).
        // Foreign-session rows must be dropped without reordering the survivors.
        let sid = SessionId::from("caller-session");
        let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
        let (fx, fy) = (NodeId::new(), NodeId::new());
        let foreign = "other-session".to_string();
        let mine = sid.0.clone();
        // dist asc: a(0.0), fx(0.5), b(1.0), fy(1.5), c(2.0)
        let rows = vec![
            (a, 0.0, mine.clone()),
            (fx, 0.5, foreign.clone()),
            (b, 1.0, mine.clone()),
            (fy, 1.5, foreign.clone()),
            (c, 2.0, mine.clone()),
        ];
        let got = filter_session_rows(&sid, &rows);
        let items: Vec<_> = got.iter().map(|s| s.item).collect();
        assert_eq!(
            items,
            vec![a, b, c],
            "foreign rows dropped, order preserved"
        );
        // Scores follow distance_to_score: 0.0→1.0, 1.0→0.5, 2.0→-1.0 (descending).
        assert!((got[0].score - 1.0).abs() < 1e-12);
        assert!((got[1].score - 0.5).abs() < 1e-12);
        assert!((got[2].score - (-1.0)).abs() < 1e-12);
    }

    #[test]
    fn vector_ties_are_ordered_by_uuid_or_trigger_exact_fallback() {
        let sid = SessionId::from("ties");
        let low = NodeId(Uuid::from_u64_pair(0, 1));
        let mid = NodeId(Uuid::from_u64_pair(0, 2));
        let high = NodeId(Uuid::from_u64_pair(0, 3));
        let rows_a = vec![
            (high, 0.25, sid.0.clone()),
            (low, 0.25, sid.0.clone()),
            (mid, 0.25, sid.0.clone()),
        ];
        let mut rows_b = rows_a.clone();
        rows_b.reverse();
        let ids = |rows: &[(NodeId, f64, String)]| {
            filter_session_rows(&sid, rows)
                .into_iter()
                .map(|s| s.item)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&rows_a), vec![low, mid, high]);
        assert_eq!(ids(&rows_b), vec![low, mid, high]);

        // More equal-distance rows than the fetch window: k+1 exposes that the
        // kth subset is arbitrary, so the caller must use exact fallback.
        assert!(has_boundary_tie(&rows_a, 2));
        assert!(has_boundary_tie(&rows_b, 2));
        assert!(SESSION_VECTOR_CANDIDATES_SQL.contains("ORDER BY dist ASC, id ASC"));
        assert!(!has_boundary_tie(
            &[
                (low, 0.1, sid.0.clone()),
                (mid, 0.2, sid.0.clone()),
                (high, 0.3, sid.0.clone()),
            ],
            2
        ));
    }

    #[test]
    fn grow_retry_is_final_when_satisfied_exhausted_or_capped() {
        // Satisfied: enough in-session hits -> final, no retry.
        assert_eq!(next_fetch_k(3, true, 30, 3), None);
        // Exhausted: no k+1 lookahead -> no more global rows exist.
        assert_eq!(next_fetch_k(1, false, 30, 3), None);
        // Capped: k already at the cap -> never grow past it.
        assert_eq!(next_fetch_k(0, true, VECTOR_FETCH_CAP, 5), None);
        // Page full + under-delivered + room to grow -> double (capped at VECTOR_FETCH_CAP).
        assert_eq!(
            next_fetch_k(1, true, 30, 5),
            Some(60),
            "grow retry doubles k"
        );
        let near_cap = VECTOR_FETCH_CAP / 2;
        assert_eq!(
            next_fetch_k(1, true, near_cap, 5),
            Some(VECTOR_FETCH_CAP),
            "growth clamps at the cap"
        );
    }

    #[test]
    fn cap_crowd_out_uses_exact_session_fallback() {
        // Adversarial distribution: 2,048 closer foreign-session rows put the
        // caller's nearest concept at global rank 2,049. The capped fast path
        // must not silently return empty; it switches to the exact session query.
        let caller = SessionId::from("caller");
        let mut globally_ranked: Vec<(NodeId, f64, String)> = (0..VECTOR_FETCH_CAP)
            .map(|rank| (NodeId::new(), rank as f64 / 10_000.0, "foreign".to_string()))
            .collect();
        globally_ranked.push((NodeId::new(), 0.3, caller.0.clone()));
        let capped_page = &globally_ranked[..VECTOR_FETCH_CAP];
        let local = filter_session_rows(&caller, capped_page);
        assert!(local.is_empty(), "local row is exactly global rank 2,049");
        assert!(needs_session_fallback(
            local.len(),
            true,
            VECTOR_FETCH_CAP,
            1
        ));
        assert!(SESSION_VECTOR_CANDIDATES_SQL.contains("WHERE session_id = $2"));
        assert!(SESSION_VECTOR_CANDIDATES_SQL.contains("ORDER BY dist ASC, id ASC"));

        assert!(!needs_session_fallback(1, true, VECTOR_FETCH_CAP, 1));
        assert!(!needs_session_fallback(0, false, VECTOR_FETCH_CAP, 1));
    }

    #[test]
    fn initial_fetch_k_is_floor_but_capped_at_same_bound_as_growth() {
        // The BASE global fetch must be floored at the multiplier (a non-trivial
        // query always pulls some headroom) and CAPPED at VECTOR_FETCH_CAP — the
        // same worst-case bound the growth step enforces. The public caller validates
        // `limit`; this pure helper still saturates defensively. `limit == 0` is
        // short-circuited by the caller, so 0 maps to
        // the floor here.
        assert_eq!(
            initial_fetch_k(0),
            VECTOR_FETCH_MULTIPLIER,
            "0 floors at multiplier"
        );
        assert_eq!(
            initial_fetch_k(1),
            VECTOR_FETCH_MULTIPLIER,
            "floor at multiplier"
        );
        assert_eq!(initial_fetch_k(5), 50);
        assert_eq!(initial_fetch_k(7), 70);
        let over = VECTOR_FETCH_CAP / VECTOR_FETCH_MULTIPLIER + 1;
        assert_eq!(
            initial_fetch_k(over),
            VECTOR_FETCH_CAP,
            "limit just over cap clamps"
        );
        assert_eq!(
            initial_fetch_k(usize::MAX),
            VECTOR_FETCH_CAP,
            "defensive saturation clamps to the cap"
        );
    }

    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn oversized_seed_embedding_dimension_fails_before_pool_use() {
        let store = CockroachStore::new(StoreConfig {
            kind: super::super::StoreKind::Cockroach,
            dsn: Some("postgresql://localhost:26257/defaultdb?sslmode=disable".into()),
            path: None,
        })
        .unwrap();
        let err = store
            .seed(&GraphSnapshot {
                session_id: SessionId::from("oversized-dim"),
                embedding: Some(EmbeddingContract {
                    kind: "fixture".into(),
                    model: None,
                    dim: usize::MAX,
                }),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)));
        assert!(
            store.pool.get().is_none(),
            "dimension check precedes pool use"
        );
    }

    /// Count `$n` placeholders in a statement and return the max `n` (1-based).
    fn placeholder_max(sql: &str) -> usize {
        let mut max = 0;
        let bytes = sql.as_bytes();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'$' && bytes[i + 1].is_ascii_digit() {
                let mut n = 0usize;
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    n = n * 10 + (bytes[j] - b'0') as usize;
                    j += 1;
                }
                max = max.max(n);
                i = j;
            } else {
                i += 1;
            }
        }
        max
    }

    #[test]
    fn upsert_placeholder_shapes_match_structs() {
        // Snapshot->row mapping shapes: every struct column has exactly one placeholder
        // and the column counts match the INSERT column lists.
        assert_eq!(placeholder_max(UPSERT_INTERACTION_SQL), 6);
        assert_eq!(placeholder_max(UPSERT_CONCEPT_SQL), 16);
        assert_eq!(placeholder_max(UPSERT_EDGE_SQL), 9);
        // STORE-1: the full-snapshot upsert now carries the embedding contract
        // (kind/model/dim) alongside root_goal/created_at/closed_at.
        assert_eq!(placeholder_max(UPSERT_SESSION_SQL), 7);
        assert!(UPSERT_SESSION_SQL.contains("embedding_kind = EXCLUDED.embedding_kind"));
        assert!(UPSERT_SESSION_SQL.contains("embedding_dim = EXCLUDED.embedding_dim"));
        assert_eq!(placeholder_max(INSERT_CANONIZATION_EVENT_SQL), 8);
        assert_eq!(placeholder_max(UPDATE_CONCEPT_STATUS_SQL), 5);
        assert_eq!(placeholder_max(UPSERT_SYNONYM_SQL), 3);
        assert_eq!(placeholder_max(UPSERT_RESERVATION_SQL), 4);
        // COH-3: the canonization surface carries last_demotion_time end to end.
        assert!(UPDATE_CONCEPT_STATUS_SQL.contains("COALESCE($5, last_demotion_time)"));
        assert!(INSERT_CANONIZATION_EVENT_SQL.contains("last_demotion_time"));
        // The vector column carries the ::VECTOR cast; chunk_group_id (T2.5) is the
        // 16th, nullable, and included in the conflict UPDATE.
        assert!(UPSERT_CONCEPT_SQL.contains("$15::VECTOR"));
        assert!(UPSERT_CONCEPT_SQL.contains("embedding = EXCLUDED.embedding"));
        assert!(UPSERT_CONCEPT_SQL.contains("chunk_group_id = EXCLUDED.chunk_group_id"));
        // Edge conflict targets the natural key; id is replaceable on conflict.
        assert!(UPSERT_EDGE_SQL.contains("ON CONFLICT (source, target, edge_type)"));
    }

    /// R2-1 (no live cluster: SQL text is the contract). The three
    /// canonization columns are INSERT-only — present in the column list so a
    /// brand-new row carries them, absent from `DO UPDATE SET` so a stale
    /// `Mutation::UpsertNode` snapshot cannot take an already-recorded hop
    /// back out of the row (or erase a demotion cooldown). `gc_survived`,
    /// which shares the same appenders, must still be updated: the property
    /// is column ownership, not a blanket skip.
    #[test]
    fn concept_upsert_does_not_write_the_canonization_columns_on_conflict() {
        let (insert, on_conflict) = UPSERT_CONCEPT_SQL
            .split_once("ON CONFLICT")
            .expect("the concept upsert has a conflict clause");
        for col in ["canonization_status", "blast_radius", "last_demotion_time"] {
            assert!(
                insert.contains(col),
                "{col} must stay in the INSERT column list (new rows carry it)"
            );
            assert!(
                !on_conflict.contains(col),
                "{col} is written only by the canonization path (R2-1)"
            );
        }
        assert!(
            UPSERT_CONCEPT_SQL.contains("gc_survived = EXCLUDED.gc_survived"),
            "the upsert's own columns must still update on conflict"
        );
        // The canonization path is that single writer.
        assert!(UPDATE_CONCEPT_STATUS_SQL.contains("canonization_status"));
        assert!(UPDATE_CONCEPT_STATUS_SQL.contains("blast_radius"));
        assert!(UPDATE_CONCEPT_STATUS_SQL.contains("last_demotion_time"));
    }

    #[test]
    fn structural_query_placeholder_order_and_counts() {
        // §4.1 SQL construction: exactly three binds, in (session, node, cutoff) order.
        assert_eq!(placeholder_max(BLAST_RADIUS_SQL), 3);
        assert_eq!(placeholder_max(INTERACTION_SPAN_SQL), 3);
        for sql in [BLAST_RADIUS_SQL, INTERACTION_SPAN_SQL] {
            let p1 = sql.find("$1").unwrap();
            let p2 = sql.find("$2").unwrap();
            let p3 = sql.find("$3").unwrap();
            assert!(p1 < p2 && p2 < p3, "placeholders must appear in bind order");
        }
        // Errata: structural edge types only; provenance must not appear.
        for sql in [BLAST_RADIUS_SQL, INTERACTION_SPAN_SQL] {
            assert!(
                sql.contains("'Dependency', 'Causal', 'Hierarchical'"),
                "{sql}"
            );
            assert!(!sql.contains("Derives"), "{sql}");
            assert!(!sql.contains("Temporal"), "{sql}");
            // Concept-sourced only: source JOIN pins src to a concept row.
            assert!(sql.contains("JOIN concepts src"), "{sql}");
        }
        // MemoryStore parity: interaction_span filters BOTH edge and interaction age.
        assert!(INTERACTION_SPAN_SQL.contains("e.created_at <= $3"));
        assert!(INTERACTION_SPAN_SQL.contains("i.created_at <= $3"));
    }

    /// F5: `origin_interaction` is a global FK, so the span CTE must scope the
    /// joined interaction to the queried session — the extent CTE already is.
    /// Without it, concepts pointing at another session's interactions inflate
    /// `distinct` and push the ratio above 1.0 (MemoryStore never sees them).
    /// Text-level like its `structural_query_placeholder_order_and_counts`
    /// neighbour: the behavioural twin runs on SQLite
    /// (`interaction_span_ignores_cross_session_origin_interactions`), which
    /// needs no live cluster.
    #[test]
    fn span_sql_is_session_scoped_and_coverage_is_clamped() {
        assert!(
            INTERACTION_SPAN_SQL.contains("i.session_id = $1"),
            "span CTE must scope the origin interaction to the session (F5)"
        );
        assert!(
            INTERACTION_SPAN_SQL.contains("least(1.0, greatest(0.0,"),
            "coverage must be clamped to [0,1] like MemoryStore/SQLite (F5)"
        );
    }

    #[test]
    fn keyword_sql_placeholder_counts_and_order() {
        for n in [1usize, 2, 3, 7] {
            let sql = keyword_candidates_sql(n);
            // $1 (session) once; each of $2..$n+1 (token) exactly twice (content + key).
            assert_eq!(placeholder_max(&sql), n + 1);
            let mut counts = std::collections::HashMap::new();
            for m in sql.split('$').skip(1) {
                let digits: String = m.chars().take_while(|c| c.is_ascii_digit()).collect();
                if !digits.is_empty() {
                    *counts
                        .entry(digits.parse::<usize>().unwrap())
                        .or_insert(0usize) += 1;
                }
            }
            assert_eq!(counts[&1], 1);
            for k in 2..=n + 1 {
                assert_eq!(
                    counts[&k], 2,
                    "token placeholder ${k} used for content and key"
                );
            }
        }
        // No LIKE wildcards: strpos(lower(...)) is exact substring (MemoryStore contains).
        let sql = keyword_candidates_sql(1);
        assert!(sql.contains("strpos(lower(content), $2) > 0"));
        assert!(!sql.contains("ILIKE") && !sql.contains("LIKE"), "{sql}");
    }

    #[test]
    fn enum_column_strings_roundtrip_all_variants() {
        for ct in [
            ConceptType::Entity,
            ConceptType::Logic,
            ConceptType::Constraint,
            ConceptType::Resource,
            ConceptType::Observation,
        ] {
            assert_eq!(parse_concept_type(concept_type_sql(ct)).unwrap(), ct);
        }
        for et in [
            EdgeType::Temporal,
            EdgeType::Derives,
            EdgeType::CoOccurrence,
            EdgeType::Causal,
            EdgeType::Dependency,
            EdgeType::Hierarchical,
            EdgeType::Semantic,
        ] {
            assert_eq!(parse_edge_type(edge_type_sql(et)).unwrap(), et);
        }
        for cs in [
            CanonizationStatus::None,
            CanonizationStatus::Candidate,
            CanonizationStatus::Venerable,
            CanonizationStatus::Canonical,
        ] {
            assert_eq!(
                parse_canonization_status(canonization_status_sql(cs)).unwrap(),
                cs
            );
        }
        assert!(parse_concept_type("Bogus").is_err());
        assert!(parse_edge_type("Bogus").is_err());
        assert!(parse_canonization_status("Bogus").is_err());
    }

    #[test]
    fn tx_retryable_is_structured_not_substring() {
        // STORE-4: the tx-replay decision matches the TYPED error — never
        // message text. Constraint violations (SQLSTATE 23xxx) are
        // deterministic: never replayed, dead-lettered upstream. Typed
        // variants are permanent. A Backend error may be a transient
        // (serialization conflict, connection exception, server shutdown);
        // the replay is bounded by TX_RETRY_ATTEMPTS + backoff, so a
        // non-constraint Backend (e.g. a schema bug) at worst re-runs the tx
        // body a bounded number of times before surfacing.
        assert!(!tx_retryable(&StoreError::Constraint("23505".into())));
        assert!(!tx_retryable(&StoreError::SessionNotFound("x".into())));
        assert!(!tx_retryable(&StoreError::Capability("nope".into())));
        assert!(!tx_retryable(&StoreError::NotFound("nope".into())));
        assert!(!tx_retryable(&StoreError::Invariant("nope".into())));
        assert!(tx_retryable(&StoreError::Backend(
            "restart transaction: TransactionRetryWithProtoRefreshError: TransactionRetryError: \
             retry txn (RETRY_SERIALIZABLE - failed preemptive refresh...)"
                .into(),
        )));
        assert!(tx_retryable(&StoreError::Backend(
            "db error: SQLSTATE 40001".into()
        )));
        assert!(tx_retryable(&StoreError::Backend(
            "relation \"concepts\" does not exist".into()
        )));
    }

    #[test]
    fn keyword_score_folds_case_like_memory_store() {
        // Regression (P3 review R1): the SQL predicate lowercases the columns, so the
        // score must fold the row text the same way — a mixed-case row ("Register
        // User") selected by token "register" scores its hits, not 0.0.
        assert_eq!(
            score_keyword_hits("Register User", "Register User", &["register".into()]),
            1,
            "mixed-case content + key must still count the lowercase token"
        );
        assert_eq!(
            score_keyword_hits(
                "Register User",
                "Register User",
                &["register".into(), "user".into()]
            ),
            2
        );
        assert_eq!(
            score_keyword_hits("Register User", "Register User", &["schema".into()]),
            0
        );
        assert_eq!(
            score_keyword_hits("register user", "register user", &["register".into()]),
            1
        );
        // Key-only hit still counts (SQL predicate is content OR canonical_key).
        assert_eq!(
            score_keyword_hits("Foo", "register user", &["register".into()]),
            1
        );
        // Tokens are pre-normalized lowercase; an uppercase token matches nothing.
        assert_eq!(
            score_keyword_hits("register user", "register user", &["Register".into()]),
            0
        );
        // Empty rows/tokens (post-normalization) contribute nothing.
        assert_eq!(score_keyword_hits("", "", &["a".into()]), 0);
    }

    #[test]
    fn normalize_tokens_matches_memory_store() {
        let tokens = vec!["  Schema ".to_string(), "".to_string(), "  ".to_string()];
        assert_eq!(
            CockroachStore::normalize_tokens(&tokens),
            vec!["schema".to_string()]
        );
        assert!(CockroachStore::normalize_tokens(&[]).is_empty());
    }

    #[test]
    fn session_embedding_xor_corruption_errors_not_silent_none() {
        // STORE-7: a sessions row with embedding_dim set but embedding_kind NULL
        // (what direct SQL on a corrupt/migrated row would produce) must error like
        // sqlite, not silently return `embedding: None`.
        let sid = "session-store7";
        // The old silent-None shape (kind absent, dim present) — the STORE-7 bug.
        let err = session_embedding_from_parts(None, None, Some(1024), sid).unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains("embedding_dim without embedding_kind"),
            "corruption error must name the shape: {err}"
        );
        // Mirror image: kind present, dim absent — sqlite errors here too.
        let err = session_embedding_from_parts(Some("bge_m3".into()), None, None, sid).unwrap_err();
        assert!(
            err.to_string()
                .contains("embedding_kind without embedding_dim"),
            "{err}"
        );
        // Negative dim is an error, not an `as usize` wrap (sqlite parity).
        let err =
            session_embedding_from_parts(Some("bge_m3".into()), None, Some(-1), sid).unwrap_err();
        assert!(err.to_string().contains("negative embedding_dim"), "{err}");
        // Well-formed rows still parse.
        let got = session_embedding_from_parts(
            Some("bge_m3".into()),
            Some("BAAI/bge-m3".into()),
            Some(1024),
            sid,
        )
        .unwrap();
        assert_eq!(
            got,
            Some(EmbeddingContract {
                kind: "bge_m3".into(),
                model: Some("BAAI/bge-m3".into()),
                dim: 1024,
            })
        );
        assert_eq!(
            session_embedding_from_parts(None, None, None, sid).unwrap(),
            None
        );
    }

    #[test]
    fn vector_contract_gate_distinguishes_unstamped_trusted_and_corrupt() {
        assert!(!vector_contract_allows_query(None, None, None, "fresh").unwrap());
        assert!(vector_contract_allows_query(
            Some("fixture".into()),
            Some("fixture-v1".into()),
            Some(1024),
            "trusted",
        )
        .unwrap());
        assert!(vector_contract_allows_query(None, None, Some(1024), "corrupt").is_err());
        assert!(
            vector_contract_allows_query(Some("fixture".into()), None, None, "corrupt",).is_err()
        );
    }
}

// ---------------------------------------------------------------------------
// Live conformance (feature-gated; honest skips without LAMBO_COCKROACH_DSN)
// ---------------------------------------------------------------------------

/// Runs under `cargo test --features store-cockroach` (with `fixtures`, which is in the
/// default set). The two live tests are `#[ignore]`d, so a run without
/// `LAMBO_COCKROACH_DSN` reports them as **ignored** — never a silent, skip-as-green
/// `ok`. To actually run them: `cargo test --features store-cockroach -- --ignored`.
/// With `LAMBO_REQUIRE_LIVE=1` (evidence capture) a missing DSN is a hard failure:
/// [`dsn_or_skip`] panics and [`live_dsn_gate_fails_loudly_when_required`] fails even
/// when the ignored tests did not run. The DSN is read from the environment only and
/// never printed.
#[cfg(all(test, feature = "store-cockroach", feature = "fixtures"))]
mod conformance {
    use super::*;
    use crate::fixtures::{load_mutation_batch, load_snapshot};
    use crate::types::{AgentId, ConceptType, Interaction, Node};
    use crate::MemoryStore;
    use chrono::TimeZone;
    use std::env;
    use std::str::FromStr;
    use std::sync::Arc;
    use uuid::Uuid;

    fn dsn() -> Option<String> {
        env::var("LAMBO_COCKROACH_DSN")
            .ok()
            .filter(|s| !s.is_empty())
    }

    fn missing_dsn_is_fatal(require_live: bool, require_vector_index: bool) -> bool {
        require_live || require_vector_index
    }

    /// Returns the DSN when present. Without one: under `LAMBO_REQUIRE_LIVE=1` or
    /// `LAMBO_REQUIRE_VECTOR_INDEX=1` this PANICS (an evidence-captured live run must
    /// never silently skip); otherwise it prints a skip notice and returns None.
    /// Callers are `#[ignore]`d, so a missing DSN surfaces as an ignored test in the
    /// default run, not a false `ok`.
    fn dsn_or_skip(test: &str) -> Option<String> {
        match dsn() {
            Some(d) => Some(d),
            None => {
                let require_live = env::var_os("LAMBO_REQUIRE_LIVE").is_some();
                let require_vector_index = env::var_os("LAMBO_REQUIRE_VECTOR_INDEX").is_some();
                if missing_dsn_is_fatal(require_live, require_vector_index) {
                    panic!(
                        "{test}: LAMBO_COCKROACH_DSN is unset but a required-live flag \
                         is set (LAMBO_REQUIRE_LIVE={require_live}, \
                         LAMBO_REQUIRE_VECTOR_INDEX={require_vector_index}) — refusing \
                         to skip a live cockroach test"
                    );
                }
                eprintln!("SKIP {test}: LAMBO_COCKROACH_DSN not set");
                None
            }
        }
    }

    /// Non-ignored honesty gate: the two live tests above are `#[ignore]`d, so the
    /// default run reports them as ignored instead of passing while skipping. With
    /// `LAMBO_REQUIRE_LIVE=1` a missing DSN must fail loudly — this gate fails even
    /// though the ignored tests themselves did not run.
    #[test]
    fn live_dsn_gate_fails_loudly_when_required() {
        let require_live = env::var_os("LAMBO_REQUIRE_LIVE").is_some();
        let require_vector_index = env::var_os("LAMBO_REQUIRE_VECTOR_INDEX").is_some();
        if !missing_dsn_is_fatal(require_live, require_vector_index) {
            return;
        }
        assert!(
            dsn().is_some(),
            "LAMBO_REQUIRE_LIVE=1 or LAMBO_REQUIRE_VECTOR_INDEX=1 requires \
             LAMBO_COCKROACH_DSN: live cockroach tests must not be silently skipped \
             (run with -- --ignored and a real DSN)"
        );
    }

    #[test]
    fn vector_index_requirement_makes_missing_dsn_fatal() {
        assert!(!missing_dsn_is_fatal(false, false));
        assert!(missing_dsn_is_fatal(true, false));
        assert!(missing_dsn_is_fatal(false, true));
        assert!(missing_dsn_is_fatal(true, true));
    }

    fn cfg(dsn: String) -> StoreConfig {
        StoreConfig {
            kind: super::super::StoreKind::Cockroach,
            dsn: Some(dsn),
            path: None,
        }
    }

    /// Fresh store for the suite. All checks run inside ONE `#[tokio::test]`, so the
    /// pool is created and every connection is used on the same (single-test) Tokio
    /// runtime — connections never cross runtimes, which avoids both "pool timed out
    /// while waiting for an open connection" (one pool, ≤ `MAX_POOL_CONNECTIONS` conns)
    /// and "A Tokio 1.x context was found, but it is being shutdown" (a pooled
    /// connection registered with a dead per-test runtime).
    fn new_store(dsn: &str) -> CockroachStore {
        CockroachStore::new(cfg(dsn.to_string())).unwrap()
    }

    /// T7.4: prove the ANN accuracy dial actually reaches the server **through
    /// sqlx**, not merely that it parses.
    ///
    /// This test exists because the obvious way to check it by hand is wrong:
    /// `PGOPTIONS=-c vector_search_beam_size=128 psql …` is silently IGNORED on
    /// this deployment (measured 2026-08-13: still reports 32, and a
    /// `statement_timeout` set the same way reports 0). Only the `options`
    /// **connection parameter** in the startup message is honoured — which is
    /// what `PgConnectOptions::options()` sets, and what `pool()` uses for both
    /// `statement_timeout` and this dial. A hand-check with `PGOPTIONS` would
    /// therefore "disprove" a setting that works perfectly.
    ///
    /// Also pins that `options()` APPENDS rather than replaces: sqlx 0.8 builds
    /// one space-joined `-c k=v` string, so adding the beam size must not drop
    /// the STORE-2 `statement_timeout` bound.
    #[tokio::test]
    #[ignore = "live: requires LAMBO_COCKROACH_DSN"]
    async fn vector_beam_size_reaches_the_server_and_keeps_statement_timeout() {
        let Some(dsn) = dsn_or_skip("vector_beam_size_reaches_the_server") else {
            return;
        };
        // Build the options under the env lock and RELEASE it before any await
        // (spec §6.4 / clippy::await_holding_lock). This is exactly why
        // `connect_options` is synchronous.
        let options = {
            let _g = crate::test_util::env_lock();
            let restore = env::var(VECTOR_BEAM_SIZE_ENV).ok();
            env::set_var(VECTOR_BEAM_SIZE_ENV, "128");
            // Same normalization `CockroachStore::new` applies: sqlx + rustls
            // cannot open libpq's `sslrootcert=system`, and `connect_options`
            // is fed `self.dsn`, which is already rewritten.
            let built = CockroachStore::connect_options(&dsn_for_rustls(&dsn));
            match restore {
                Some(v) => env::set_var(VECTOR_BEAM_SIZE_ENV, v),
                None => env::remove_var(VECTOR_BEAM_SIZE_ENV),
            }
            built.unwrap()
        };

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(options);
        let beam: String = sqlx::query_scalar("SHOW vector_search_beam_size")
            .fetch_one(&pool)
            .await
            .unwrap();
        let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(
            beam, "128",
            "explicit LAMBO_VECTOR_BEAM_SIZE did not reach the server"
        );
        assert_ne!(
            timeout, "0",
            "adding the beam-size option dropped the STORE-2 statement_timeout \
             — options() must append, not replace"
        );
    }

    fn embed(seed: f32) -> Vec<f32> {
        let mut v: Vec<f32> = (0..1024)
            .map(|i| ((i as f32 + 1.0) * seed).sin() * 0.5)
            .collect();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        for x in &mut v {
            *x /= norm;
        }
        v
    }

    fn plant_interaction(sid: &SessionId, id: NodeId, ts: DateTime<Utc>) -> Mutation {
        Mutation::UpsertNode {
            node: Node::Interaction(Interaction {
                id,
                session_id: sid.clone(),
                agent_id: AgentId::from("agent-a"),
                prompt_text: Some("seed".into()),
                previous_id: None,
                created_at: ts,
            }),
        }
    }

    fn plant_concept(
        sid: &SessionId,
        id: NodeId,
        origin: NodeId,
        content: &str,
        ts: DateTime<Utc>,
        embedding: Option<Vec<f32>>,
    ) -> Mutation {
        plant_concept_full(
            sid,
            id,
            origin,
            content,
            &content.to_lowercase(),
            ConceptType::Entity,
            ts,
            embedding,
            None,
        )
    }

    /// Full-shape concept planter: verbatim canonical_key, concept type, and
    /// chunk_group_id — used by the legal-demote (R2), chunk_group_id round-trip,
    /// and mixed-case keyword checks.
    // Test helper: every parameter is a distinct planter field (same precedent as
    // derive.rs resolve_concept) — a params struct would obscure the call sites.
    #[allow(clippy::too_many_arguments)]
    fn plant_concept_full(
        sid: &SessionId,
        id: NodeId,
        origin: NodeId,
        content: &str,
        canonical_key: &str,
        concept_type: ConceptType,
        ts: DateTime<Utc>,
        embedding: Option<Vec<f32>>,
        chunk_group_id: Option<String>,
    ) -> Mutation {
        Mutation::UpsertNode {
            node: Node::Concept(Concept {
                id,
                session_id: sid.clone(),
                content: content.into(),
                canonical_key: canonical_key.into(),
                concept_type,
                origin_interaction: origin,
                origin_agent: AgentId::from("agent-a"),
                created_at: ts,
                access_count: 0,
                last_accessed: None,
                gc_survived: 0,
                canonization_status: CanonizationStatus::None,
                blast_radius: None,
                last_demotion_time: None,
                embedding,
                chunk_group_id,
            }),
        }
    }

    fn plant_edge(
        sid: &SessionId,
        source: NodeId,
        target: NodeId,
        edge_type: EdgeType,
        ts: DateTime<Utc>,
    ) -> Mutation {
        Mutation::UpsertEdge {
            edge: Edge {
                id: NodeId::new(),
                session_id: sid.clone(),
                source,
                target,
                edge_type,
                weight: 1.0,
                reinforcements: 1,
                created_at: ts,
                last_reinforced: ts,
            },
        }
    }

    fn sorted_snap_parts(snap: &GraphSnapshot) -> (Vec<NodeId>, Vec<NodeId>, Vec<NodeId>) {
        let mut ii: Vec<NodeId> = snap.interactions.iter().map(|i| i.id).collect();
        let mut cc: Vec<NodeId> = snap.concepts.iter().map(|c| c.id).collect();
        let mut ee: Vec<NodeId> = snap.edges.iter().map(|e| e.id).collect();
        ii.sort_by_key(|n| n.0);
        cc.sort_by_key(|n| n.0);
        ee.sort_by_key(|n| n.0);
        (ii, cc, ee)
    }

    async fn check_init_schema_idempotent(store: &CockroachStore) {
        store.init_schema().await.unwrap();
        store.init_schema().await.unwrap();
    }

    /// No Tokio needed: `build_store` constructs the adapter without creating the pool
    /// (lazy OnceCell), so this runs as a plain sync test. The real DSN also exercises
    /// the rustls rewrite + parse validation at construction. `#[ignore]`d: without
    /// `LAMBO_COCKROACH_DSN` this must report as ignored, not skip-as-green.
    #[test]
    #[ignore = "requires LAMBO_COCKROACH_DSN (run live via -- --ignored)"]
    fn build_store_returns_working_adapter() {
        let Some(dsn) = dsn_or_skip("build_store_returns_working_adapter") else {
            return;
        };
        let s = super::super::build_store(cfg(dsn)).unwrap();
        assert!(s.capabilities().contains(Capabilities::VECTOR_SEARCH));
        assert_eq!(s.vector_dimensions(), Some(1024));
    }

    async fn check_flush_mutations_batch_roundtrip(store: &CockroachStore) {
        let batch = load_mutation_batch("mutations-batch").unwrap();
        let sid = SessionId::from("session-mutations");
        store.flush(&batch).await.unwrap();

        // Direct snapshot read-back.
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.interactions.len(), 2, "both interactions survive");
        assert_eq!(snap.concepts.len(), 1, "deleted concept removed");
        assert_eq!(snap.concepts[0].content, "kept concept");
        assert_eq!(
            snap.concepts[0].canonization_status,
            CanonizationStatus::Candidate,
            "canonization transition applied"
        );
        assert_eq!(snap.edges.len(), 1, "delete_edge + incident-edge cleanup");
        assert_eq!(
            snap.edges[0].id,
            NodeId(Uuid::from_str("f0000000-0000-4000-8000-000000007052").unwrap()),
            "only the Derives edge survives"
        );
        assert_eq!(snap.canonization_events.len(), 1);
        assert_eq!(
            snap.canonization_events[0].node_id,
            NodeId(Uuid::from_str("f0000000-0000-4000-8000-000000007002").unwrap())
        );
        // NOTE: this fixture's final state is intentionally NOT a legal §5.7 graph — it
        // deletes the Temporal edge between the two interactions, so the loaded snapshot
        // cannot be materialized via Graph::from_snapshot (would fail the Temporal-edge
        // invariant). Snapshot-level round-trip is the correct conformance here; graph
        // materialization of legal batches is covered by load.rs tests.
    }

    async fn check_load_missing_session_is_session_not_found(store: &CockroachStore) {
        let err = store
            .load_session(&SessionId::from(format!("no-such-{}", Uuid::new_v4())))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound(_)));
    }

    async fn check_vector_write_and_candidates_top1(store: &CockroachStore) {
        let sid = SessionId::from(format!("conformance-vector-{}", Uuid::new_v4()));
        let i1 = NodeId::new();
        let a = NodeId::new();
        let b = NodeId::new();
        let ts = Utc::now();
        let probe = embed(0.17);
        store
            .flush(&MutationBatch {
                mutations: vec![
                    Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(EmbeddingContract {
                            kind: "fixture".into(),
                            model: Some("fixture-v1".into()),
                            dim: store.vector_dim,
                        }),
                    },
                    plant_interaction(&sid, i1, ts),
                    plant_concept(&sid, a, i1, "alpha concept", ts, Some(probe.clone())),
                    plant_concept(&sid, b, i1, "beta concept", ts, Some(embed(0.5))),
                ],
            })
            .await
            .unwrap();

        let hits = store.vector_candidates(&sid, &probe, 3).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].item, a, "identical embedding must rank first");
        assert!(
            (hits[0].score - 1.0).abs() < 1e-4,
            "score {}",
            hits[0].score
        );
        assert!(hits[0].score > hits[1].score);

        // Round-trip the stored vector through load_session.
        let snap = store.load_session(&sid).await.unwrap();
        let back = snap
            .concepts
            .iter()
            .find(|c| c.id == a)
            .unwrap()
            .embedding
            .as_ref()
            .unwrap();
        assert_eq!(back.len(), 1024);
        let max_diff = probe
            .iter()
            .zip(back.iter())
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max);
        assert!(max_diff < 1e-4, "vector round-trip max diff {max_diff}");
    }

    async fn check_vector_candidates_are_session_scoped(store: &CockroachStore) {
        // DECISION D1: the SQL queries GLOBALLY, so a closer foreign-session concept is
        // in the raw top-k — the Rust session filter must drop it, never return it.
        // Two near-paraphrase concepts in session A should retrieve each other via the
        // index while staying session-scoped.
        let sid_a = SessionId::from(format!("conformance-vecscope-a-{}", Uuid::new_v4()));
        let sid_b = SessionId::from(format!("conformance-vecscope-b-{}", Uuid::new_v4()));
        let (i1, i2) = (NodeId::new(), NodeId::new());
        let (a, c, b) = (NodeId::new(), NodeId::new(), NodeId::new());
        let ts = Utc::now();
        let probe = embed(0.11);
        store
            .flush(&MutationBatch {
                mutations: vec![
                    Mutation::SetEmbedding {
                        session_id: sid_a.clone(),
                        embedding: Some(EmbeddingContract {
                            kind: "fixture".into(),
                            model: Some("fixture-v1".into()),
                            dim: store.vector_dim,
                        }),
                    },
                    plant_interaction(&sid_a, i1, ts),
                    plant_concept(&sid_a, a, i1, "register user", ts, Some(probe.clone())),
                    plant_concept(&sid_a, c, i1, "create account", ts, Some(embed(0.115))),
                ],
            })
            .await
            .unwrap();
        // Foreign session B holds a concept whose vector is EXACTLY the probe — the
        // globally-closest row — so it would rank first in the raw query.
        store
            .flush(&MutationBatch {
                mutations: vec![
                    Mutation::SetEmbedding {
                        session_id: sid_b.clone(),
                        embedding: Some(EmbeddingContract {
                            kind: "fixture".into(),
                            model: Some("fixture-v1".into()),
                            dim: store.vector_dim,
                        }),
                    },
                    plant_interaction(&sid_b, i2, ts),
                    plant_concept(&sid_b, b, i2, "foreign closer", ts, Some(probe.clone())),
                ],
            })
            .await
            .unwrap();

        let hits = store.vector_candidates(&sid_a, &probe, 10).await.unwrap();
        let items: Vec<_> = hits.iter().map(|h| h.item).collect();
        assert!(
            !items.contains(&b),
            "foreign-session node {b} leaked into results: {items:?}"
        );
        assert!(
            items.contains(&a) && items.contains(&c),
            "near paraphrases should retrieve each other; got {items:?}"
        );
        assert_eq!(
            hits[0].item, a,
            "own exact candidate should rank first in-session; got {items:?}"
        );
    }

    async fn check_vector_explain_is_global_topk(store: &CockroachStore) {
        // DECISION D1 shape on the live plan: the query is GLOBAL (no session predicate),
        // so the plan must NOT scan the anti-pattern `concepts_session_id_canonical_key_key`
        // (the T0.3-spike shape that bypassed the vector index). The plan is an ordered
        // top-k over the whole table; whether the optimizer accelerates that top-k with
        // the vector index (`vector search`) is asserted separately by the standalone
        // `vector_explain_camera_proof` gate.
        //
        // T7.4 correction (2026-08-13): this comment used to say that gate was
        // "PENDING where the optimizer scans a small table". That was wrong on both
        // counts — table size was never the cause. The proof failed because it asserted
        // the spaced `vector search` against `EXPLAIN (OPT, VERBOSE)`, which spells the
        // operator `vector-search`, and because a NON-partial vector index cannot imply
        // the query's `embedding IS NOT NULL` predicate, forcing a FULL SCAN. With the
        // index partial (migrations/cockroach/001_init.sql) the proof is green.
        //
        // T7.3 remediation (planner-variance hardening): EXPLAIN with a LITERAL
        // `LIMIT 5` (not a parameterized `LIMIT $2`) to match the captured T0.3
        // evidence, which reproduced a `top-k` node under the literal shape. With a
        // placeholder limit the optimizer MAY fall back to a `limit` + `sort` plan
        // instead — the same correct global-ordered semantics, different node name.
        // So the positive assertion accepts EITHER a `top-k` or a `limit` ordering
        // construct; the assertion that MUST always hold — and is the DECISION D1
        // non-negotiable — is that the plan does NOT reference the session-filtered
        // anti-pattern index. This keeps the gate green against planner variance
        // without weakening the no-anti-pattern guarantee.
        let pool = store.pool().await.unwrap();
        let probe = encode_vector(&embed(0.5)).unwrap();
        let rows = sqlx::query(
            "EXPLAIN (OPT, VERBOSE) \
             SELECT id, session_id, embedding <-> $1::VECTOR AS dist \
             FROM concepts WHERE embedding IS NOT NULL ORDER BY dist ASC LIMIT 5",
        )
        .bind(&probe)
        .fetch_all(pool)
        .await
        .map_err(backend)
        .unwrap();
        let plan: Vec<String> = rows
            .iter()
            .map(|r| r.try_get::<String, usize>(0).map_err(backend))
            .collect::<Result<_, _>>()
            .unwrap();
        let text = plan.join("\n");
        assert!(
            text.contains("top-k") || text.contains("limit"),
            "EXPLAIN must be a global ordered top-k/limit query, got:\n{text}"
        );
        assert!(
            !text.contains("concepts_session_id_canonical_key_key"),
            "EXPLAIN must NOT scan the session-filtered index (DECISION D1 anti-pattern):\n{text}"
        );
    }

    async fn check_keyword_candidates_on_planted_concept(store: &CockroachStore) {
        let sid = SessionId::from(format!("conformance-kw-{}", Uuid::new_v4()));
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, i1, Utc::now()),
                    plant_concept(&sid, c1, i1, "user schema", Utc::now(), None),
                ],
            })
            .await
            .unwrap();
        let hits = store
            .keyword_candidates(&sid, &["schema".into()], 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item, c1);
        assert_eq!(hits[0].score, 1.0);
        // Empty tokens match nothing (MemoryStore parity).
        assert!(store
            .keyword_candidates(&sid, &["   ".into()], 5)
            .await
            .unwrap()
            .is_empty());
        // Missing session -> SessionNotFound (MemoryStore parity).
        let err = store
            .keyword_candidates(&SessionId::from("nope"), &["schema".into()], 5)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound(_)));
    }

    async fn check_keyword_mixed_case_ranks_like_memory_store(store: &CockroachStore) {
        // Regression (P3 review R1): the SQL predicate lowercases content/key, so the
        // score must fold case too. A mixed-case concept ("Register User") matched by
        // token "register" must score > 0 and rank exactly like MemoryStore — a
        // raw-`contains` score would give it 0.0 and sink it below "user schema".
        let sid = SessionId::from(format!("conformance-kwcase-{}", Uuid::new_v4()));
        let i1 = NodeId::new();
        let mixed = NodeId::new();
        let lower = NodeId::new();
        let ts = Utc::now();
        let batch = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, i1, ts),
                // Mixed-case content AND canonical_key — selected by the SQL's lower()
                // predicate, scored only if the score loop folds case.
                plant_concept_full(
                    &sid,
                    mixed,
                    i1,
                    "Register User",
                    "Register User",
                    ConceptType::Entity,
                    ts,
                    None,
                    None,
                ),
                plant_concept_full(
                    &sid,
                    lower,
                    i1,
                    "user schema",
                    "user schema",
                    ConceptType::Entity,
                    ts,
                    None,
                    None,
                ),
            ],
        };
        store.flush(&batch).await.unwrap();
        let mem = MemoryStore::new();
        mem.flush(&batch).await.unwrap();

        let tokens: Vec<String> = vec!["register".into(), "user".into()];
        let crdb = store.keyword_candidates(&sid, &tokens, 5).await.unwrap();
        let mem_res = mem.keyword_candidates(&sid, &tokens, 5).await.unwrap();
        assert_eq!(
            crdb, mem_res,
            "mixed-case scoring must match MemoryStore exactly"
        );
        assert_eq!(crdb.len(), 2);
        assert_eq!(
            crdb[0].item, mixed,
            "Register User (2 hits) must rank above user schema (1 hit)"
        );
        assert_eq!(crdb[0].score, 2.0);
        assert_eq!(crdb[1].item, lower);
        assert_eq!(crdb[1].score, 1.0);
    }

    async fn check_legal_demote_flush_partial_index(store: &CockroachStore) {
        // R2 (P3 review): the schema's canonical-key unique index is PARTIAL
        // (`WHERE concept_type <> 'Observation'`, spec §4 errata / muse-spark M1-M2).
        // A legal demote (T2.5) writes Observations that share a canonical key
        // (identical sentences from different chunks); those must flush successfully
        // against the live store instead of colliding on the index.
        let sid = SessionId::from(format!("conformance-demote-{}", Uuid::new_v4()));
        let i1 = NodeId::new();
        let o1 = NodeId::new();
        let o2 = NodeId::new();
        let ts = Utc::now();
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, i1, ts),
                    plant_concept_full(
                        &sid,
                        o1,
                        i1,
                        "identical sentence",
                        "identical sentence",
                        ConceptType::Observation,
                        ts,
                        None,
                        Some("chunk-1".into()),
                    ),
                    plant_concept_full(
                        &sid,
                        o2,
                        i1,
                        "identical sentence",
                        "identical sentence",
                        ConceptType::Observation,
                        ts,
                        None,
                        Some("chunk-1".into()),
                    ),
                ],
            })
            .await
            .unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.concepts.len(), 2, "both demoted Observations survive");
        assert!(snap
            .concepts
            .iter()
            .all(|c| c.concept_type == ConceptType::Observation));
        assert!(snap
            .concepts
            .iter()
            .all(|c| c.canonical_key == "identical sentence"));
        assert!(snap
            .concepts
            .iter()
            .all(|c| c.chunk_group_id.as_deref() == Some("chunk-1")));

        // Negative lock on the same index: a duplicate-key NON-Observation must be
        // rejected (the RAM graph rejects it as an invariant; the store fails loudly).
        let e1 = NodeId::new();
        let e2 = NodeId::new();
        let bad = MutationBatch {
            mutations: vec![
                plant_concept_full(
                    &sid,
                    e1,
                    i1,
                    "dup key",
                    "dup key",
                    ConceptType::Entity,
                    ts,
                    None,
                    None,
                ),
                plant_concept_full(
                    &sid,
                    e2,
                    i1,
                    "dup key",
                    "dup key",
                    ConceptType::Entity,
                    ts,
                    None,
                    None,
                ),
            ],
        };
        assert!(
            store.flush(&bad).await.is_err(),
            "duplicate-key non-Observation must violate concepts_key_non_obs_idx"
        );
    }

    async fn check_chunk_group_id_survives_flush_load(store: &CockroachStore) {
        // T5.2 contract (schema persistence): flush→load must PRESERVE
        // chunk_group_id — the implementer's snapshot normalization to None is gone;
        // the round-trip now asserts survival.
        let sid = SessionId::from(format!("conformance-cgid-{}", Uuid::new_v4()));
        let i1 = NodeId::new();
        let obs = NodeId::new();
        let plain = NodeId::new();
        let ts = Utc::now();
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, i1, ts),
                    plant_concept_full(
                        &sid,
                        obs,
                        i1,
                        "overflow sentence",
                        "overflow sentence",
                        ConceptType::Observation,
                        ts,
                        None,
                        Some("chunk-42".into()),
                    ),
                    plant_concept_full(
                        &sid,
                        plain,
                        i1,
                        "plain concept",
                        "plain concept",
                        ConceptType::Entity,
                        ts,
                        None,
                        None,
                    ),
                ],
            })
            .await
            .unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        let obs_row = snap.concepts.iter().find(|c| c.id == obs).unwrap();
        assert_eq!(
            obs_row.chunk_group_id.as_deref(),
            Some("chunk-42"),
            "chunk_group_id must survive flush→load (T5.2 sibling co-retrieval key)"
        );
        let plain_row = snap.concepts.iter().find(|c| c.id == plain).unwrap();
        assert_eq!(
            plain_row.chunk_group_id, None,
            "NULL chunk_group_id stays None"
        );
    }

    async fn check_embedding_contract_read_and_flush_immunity(store: &CockroachStore) {
        // Embedding contract (S5-class snapshot metadata): seed — the PRODUCTION
        // full-snapshot path (STORE-1 remediation) — persists embedding_kind/model/dim,
        // and load_session materializes GraphSnapshot.embedding when present. A later
        // flush (which only ensures the session row; there is no session-metadata
        // Mutation kind) must NOT clobber the stamped contract.
        let sid = SessionId::from(format!("conformance-embed-{}", Uuid::new_v4()));
        store
            .seed(&GraphSnapshot {
                session_id: sid.clone(),
                embedding: Some(EmbeddingContract {
                    kind: "bge_m3".into(),
                    model: Some("BAAI/bge-m3".into()),
                    dim: 1024,
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(
            snap.embedding,
            Some(EmbeddingContract {
                kind: "bge_m3".into(),
                model: Some("BAAI/bge-m3".into()),
                dim: 1024,
            }),
            "load_session must materialize the seeded embedding contract"
        );
        // A subsequent flush without a metadata mutation must not clobber the
        // seeded contract.
        let i1 = NodeId::new();
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, i1, Utc::now()),
                    plant_concept(&sid, NodeId::new(), i1, "seed concept", Utc::now(), None),
                ],
            })
            .await
            .unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        let emb = snap
            .embedding
            .expect("embedding contract survives a later flush");
        assert_eq!(emb.kind, "bge_m3");
        assert_eq!(emb.model.as_deref(), Some("BAAI/bge-m3"));
        assert_eq!(emb.dim, 1024);
        assert_eq!(
            snap.concepts.len(),
            1,
            "flush content still lands alongside the contract"
        );
        // Session with no stamp reads back None.
        let plain_sid = SessionId::from(format!("conformance-embed-none-{}", Uuid::new_v4()));
        store
            .flush(&MutationBatch {
                mutations: vec![plant_interaction(&plain_sid, NodeId::new(), Utc::now())],
            })
            .await
            .unwrap();
        let snap = store.load_session(&plain_sid).await.unwrap();
        assert_eq!(snap.embedding, None, "unstamped session has no contract");
    }

    async fn check_seed_load_full_snapshot_roundtrip(store: &CockroachStore) {
        let snap = load_snapshot("session-rest-api").unwrap();
        store.seed(&snap).await.unwrap();
        let loaded = store.load_session(&snap.session_id).await.unwrap();

        let (li, lc, le) = sorted_snap_parts(&loaded);
        let (si, sc, se) = sorted_snap_parts(&snap);
        assert_eq!(li, si, "interactions round-trip");
        assert_eq!(lc, sc, "concepts round-trip");
        assert_eq!(le, se, "edges round-trip");

        // Field-level equality on representative rows.
        assert_eq!(loaded.session_id, snap.session_id);
        assert_eq!(loaded.root_goal, snap.root_goal);
        assert_eq!(loaded.created_at, snap.created_at);
        assert_eq!(loaded.closed_at, snap.closed_at);
        assert_eq!(
            loaded.synonyms.len(),
            snap.synonyms.len(),
            "synonym persisted via seed"
        );
        assert_eq!(loaded.synonyms[0].source_key, snap.synonyms[0].source_key);
        assert_eq!(
            loaded.synonyms[0].canonical_key,
            snap.synonyms[0].canonical_key
        );
        assert!(loaded.reservations.is_empty());
        assert!(loaded.canonization_events.is_empty());

        // Full deep-equality of the whole graph (fixture order is id-ordered).
        let mut a = loaded.concepts.clone();
        let mut b = snap.concepts.clone();
        a.sort_by_key(|c| c.id.0);
        b.sort_by_key(|c| c.id.0);
        assert_eq!(a, b, "concept rows deep-equal (incl. timestamps)");
        let mut ai = loaded.interactions.clone();
        let mut bi = snap.interactions.clone();
        ai.sort_by_key(|i| i.id.0);
        bi.sort_by_key(|i| i.id.0);
        assert_eq!(ai, bi);
        let mut ae = loaded.edges.clone();
        let mut be = snap.edges.clone();
        ae.sort_by_key(|e| e.id.0);
        be.sort_by_key(|e| e.id.0);
        assert_eq!(ae, be);
    }

    /// T3.6 matrix for ONE fixture: seed it into the live Cockroach store AND a
    /// fresh MemoryStore, then assert EVERY node (concepts + interactions) ×
    /// min-age {0, 3600s} × both queries answers EXACTLY like MemoryStore's
    /// naive scan on the same snapshot. Seeding is an idempotent upsert, so
    /// re-runs against a persistent cluster converge. Returns the number of
    /// equality assertions performed (2 per node × age cell).
    async fn check_fixture_structural_agreement(store: &CockroachStore, fixture: &str) -> usize {
        let snap = load_snapshot(fixture).unwrap();
        let sid = snap.session_id.clone();
        store.seed(&snap).await.unwrap();

        let mem = {
            let m = MemoryStore::new();
            m.seed(snap.clone()).unwrap();
            Arc::new(m)
        };

        let node_ids: Vec<NodeId> = snap
            .concepts
            .iter()
            .map(|c| c.id)
            .chain(snap.interactions.iter().map(|i| i.id))
            .collect();
        let ages = [Duration::ZERO, Duration::from_secs(3600)];
        let mut assertions = 0usize;
        for node in &node_ids {
            for age in ages {
                let mem_br = mem
                    .blast_radius(&sid, *node, age, Utc::now())
                    .await
                    .unwrap();
                let crdb_br = store
                    .blast_radius(&sid, *node, age, Utc::now())
                    .await
                    .unwrap();
                assert_eq!(mem_br, crdb_br, "blast_radius({fixture}, {node}, {age:?})");
                let mem_span = mem
                    .interaction_span(&sid, *node, age, Utc::now())
                    .await
                    .unwrap();
                let crdb_span = store
                    .interaction_span(&sid, *node, age, Utc::now())
                    .await
                    .unwrap();
                assert_eq!(
                    mem_span, crdb_span,
                    "interaction_span({fixture}, {node}, {age:?}): mem={mem_span:?} crdb={crdb_span:?}"
                );
                assertions += 2;
            }
        }

        // Deterministic anchors on rest-api (independent of the oracle): eight
        // concepts depend on the Canonical hub 1001 exclusively; span = 6
        // distinct interactions over 25 of 55 minutes.
        if fixture == "session-rest-api" {
            let hub: NodeId = snap
                .concepts
                .iter()
                .find(|c| c.id.0.to_string().ends_with("001001"))
                .unwrap()
                .id;
            assert_eq!(
                store
                    .blast_radius(&sid, hub, Duration::ZERO, Utc::now())
                    .await
                    .unwrap(),
                8,
                "hub 1001 blast radius anchor"
            );
            let span = store
                .interaction_span(&sid, hub, Duration::ZERO, Utc::now())
                .await
                .unwrap();
            assert_eq!(span.distinct, 6);
            assert!(
                (span.coverage - 25.0 / 55.0).abs() < 1e-9,
                "hub 1001 span anchor: {span:?}"
            );
        }
        assertions
    }

    /// T3.6 acceptance: the three-way agreement matrix on BOTH fixture graphs —
    /// `session-rest-api` (22 concepts incl. Canonical hub 1001, Venerable
    /// 1012, D1–D8 orphans, P1/P2 peers) and `session-drift` (9 concepts) —
    /// every node × min-age {0, 3600s}, blast_radius + interaction_span
    /// (distinct AND coverage) exactly equal to MemoryStore.
    async fn check_structural_queries_agree_with_memory_store(store: &CockroachStore) {
        let mut total_assertions = 0usize;
        for fixture in ["session-rest-api", "session-drift"] {
            total_assertions += check_fixture_structural_agreement(store, fixture).await;
        }
        // Lock the matrix dimensions like the sqlite suite (round-1 review F2):
        // rest-api 34 nodes (22 concepts + 12 interactions) + drift 11 nodes
        // (9 + 2) = 45 nodes; 2 ages; 2 queries each -> 180 equality assertions.
        // A future narrowing of either matrix loop silently shrinks coverage,
        // so the count must be verified, not just implied by the loop shape.
        assert_eq!(total_assertions, 180, "matrix dimensions drifted");
    }

    async fn check_structural_queries_age_filter_agrees(store: &CockroachStore) {
        // T3.6 matrix + round-1 review F1 remediation: the span's TWO timestamp
        // gates are discriminated behaviorally, not just textually —
        // * e-gate: the fresh edge's source carries a DISTINCT origin
        //   interaction (`i2`), so the span set genuinely shrinks at
        //   min_age = 3600s: `span(orphan).distinct` is 2 at min-age 0 and 1 at
        //   1h (before the fix every origin was `i1`, so the span was identical
        //   with or without either gate);
        // * i-gate probe: an AGED edge (`probe_src -> probe_victim`) whose
        //   origin interaction `i3` is FRESH (created after the 1h cutoff) must
        //   be excluded from the span — `span(probe_victim).distinct` is 1 at
        //   min-age 0 and 0 at 1h.
        // blast_radius is origin-agnostic by contrast (the i-gate is span-only,
        // matching MemoryStore).
        let sid = SessionId::from(format!("conformance-age-{}", Uuid::new_v4()));
        let old_ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
        let now = Utc::now();
        let i1 = NodeId::new();
        let i2 = NodeId::new();
        let i3 = NodeId::new();
        let pillar = NodeId::new();
        let orphan = NodeId::new();
        let other = NodeId::new();
        let probe_src = NodeId::new();
        let probe_victim = NodeId::new();

        // Base: aged graph — pillar -> orphan (aged edge, aged origin i1) and
        // probe_src -> probe_victim (aged edge, FRESH origin i3: the i-gate
        // probe). `other`'s origin is DISTINCT i2 — the fresh edge's source.
        let base = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, i1, old_ts),
                plant_interaction(&sid, i2, old_ts),
                plant_interaction(&sid, i3, now),
                plant_concept(&sid, pillar, i1, "pillar", old_ts, None),
                plant_concept(&sid, orphan, i1, "orphan", old_ts, None),
                plant_concept(&sid, other, i2, "other", old_ts, None),
                plant_concept(&sid, probe_src, i3, "probe-src", old_ts, None),
                plant_concept(&sid, probe_victim, i1, "probe-victim", old_ts, None),
                plant_edge(&sid, pillar, orphan, EdgeType::Dependency, old_ts),
                plant_edge(&sid, probe_src, probe_victim, EdgeType::Dependency, old_ts),
            ],
        };
        store.flush(&base).await.unwrap();
        let mem = MemoryStore::new();
        mem.flush(&base).await.unwrap();
        // Then a genuinely FRESH other -> orphan dependency (created now).
        let fresh = MutationBatch {
            mutations: vec![plant_edge(&sid, other, orphan, EdgeType::Dependency, now)],
        };
        store.flush(&fresh).await.unwrap();
        mem.flush(&fresh).await.unwrap();

        let one_hour = Duration::from_secs(3600);
        // Aged-vs-fresh edge interaction: EVERY node × both cutoffs must agree
        // with MemoryStore exactly (the aged pillar -> orphan edge vs the
        // freshly created other -> orphan edge, plus the i-gate probe).
        for node in [pillar, orphan, other, probe_src, probe_victim, i1, i2, i3] {
            for min_age in [Duration::ZERO, one_hour] {
                let mem_br = mem
                    .blast_radius(&sid, node, min_age, Utc::now())
                    .await
                    .unwrap();
                let crdb_br = store
                    .blast_radius(&sid, node, min_age, Utc::now())
                    .await
                    .unwrap();
                assert_eq!(mem_br, crdb_br, "blast_radius({node}, {min_age:?})");
                let mem_span = mem
                    .interaction_span(&sid, node, min_age, Utc::now())
                    .await
                    .unwrap();
                let crdb_span = store
                    .interaction_span(&sid, node, min_age, Utc::now())
                    .await
                    .unwrap();
                assert_eq!(
                    mem_span, crdb_span,
                    "interaction_span({node}, {min_age:?}): mem={mem_span:?} crdb={crdb_span:?}"
                );
            }
        }
        // e-gate discrimination on the SPAN (independent of the oracle):
        // `other`'s DISTINCT origin i2 counts at min-age 0 and must vanish at
        // 1h when the fresh edge is filtered.
        assert_eq!(
            store
                .interaction_span(&sid, orphan, Duration::ZERO, Utc::now())
                .await
                .unwrap()
                .distinct,
            2,
            "e-gate: fresh edge's distinct origin counts at min_age=0"
        );
        assert_eq!(
            store
                .interaction_span(&sid, orphan, one_hour, Utc::now())
                .await
                .unwrap()
                .distinct,
            1,
            "e-gate: fresh edge's distinct origin filtered at min_age=1h"
        );
        // i-gate probe: the AGED probe_src -> probe_victim edge's origin i3 is
        // FRESH, so it is in the span at min-age 0 and must be excluded at 1h.
        assert_eq!(
            store
                .interaction_span(&sid, probe_victim, Duration::ZERO, Utc::now())
                .await
                .unwrap()
                .distinct,
            1,
            "i-gate: fresh origin counts at min_age=0"
        );
        assert_eq!(
            store
                .interaction_span(&sid, probe_victim, one_hour, Utc::now())
                .await
                .unwrap()
                .distinct,
            0,
            "i-gate: aged edge with fresh origin excluded at min_age=1h"
        );
        // blast_radius is origin-agnostic: the aged probe edge counts at 1h
        // even though its origin is fresh (span-only i-gate, MemoryStore parity).
        assert_eq!(
            store
                .blast_radius(&sid, probe_src, one_hour, Utc::now())
                .await
                .unwrap(),
            1,
            "blast_radius ignores origin age"
        );
        // The age filter is doing real work for blast_radius: with min_age=0 the
        // fresh edge un-orphans; with min_age=1h it is filtered and the orphan
        // still counts.
        assert_eq!(
            store
                .blast_radius(&sid, pillar, Duration::ZERO, Utc::now())
                .await
                .unwrap(),
            0,
            "fresh edge counts at min_age=0"
        );
        assert_eq!(
            store
                .blast_radius(&sid, pillar, one_hour, Utc::now())
                .await
                .unwrap(),
            1,
            "fresh edge filtered at min_age=1h"
        );
    }

    /// §4.1 errata probe (T3.6): mirror of MemoryStore's
    /// `blast_radius_ignores_provenance_derives_edges` against the live SQL
    /// adapter. §5.7 requires every concept to carry a `Derives` edge
    /// (interaction → concept); if blast_radius counted that inbound edge as
    /// "another source", every concept would look non-orphaned and Stage-3
    /// blast radius would collapse to ~0. Cockroach must ignore provenance
    /// (`Derives`/`Temporal`) edges exactly like MemoryStore — never
    /// un-orphaning a concept through them.
    async fn check_structural_queries_errata_derives_probe(store: &CockroachStore) {
        let sid = SessionId::from(format!("conformance-errata-{}", Uuid::new_v4()));
        let old_ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
        let i1 = NodeId::new();
        let pillar = NodeId::new();
        let orphan = NodeId::new();
        let alone = NodeId::new();
        let batch = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, i1, old_ts),
                plant_concept(&sid, pillar, i1, "pillar", old_ts, None),
                plant_concept(&sid, orphan, i1, "orphan", old_ts, None),
                plant_concept(&sid, alone, i1, "alone", old_ts, None),
                // pillar -> orphan (Dependency): the only structural inbound.
                plant_edge(&sid, pillar, orphan, EdgeType::Dependency, old_ts),
                // orphan ALSO has the mandatory §5.7 Derives from its origin
                // interaction — counting it would un-orphan orphan.
                plant_edge(&sid, i1, orphan, EdgeType::Derives, old_ts),
                // alone has ONLY the Derives provenance: never an orphan of
                // anyone (no structural inbound edge exists at all).
                plant_edge(&sid, i1, alone, EdgeType::Derives, old_ts),
            ],
        };
        store.flush(&batch).await.unwrap();
        let mem = MemoryStore::new();
        mem.flush(&batch).await.unwrap();

        for min_age in [Duration::ZERO, Duration::from_secs(3600)] {
            let want = mem
                .blast_radius(&sid, pillar, min_age, Utc::now())
                .await
                .unwrap();
            assert_eq!(want, 1, "oracle sanity: Derives must not un-orphan");
            let got = store
                .blast_radius(&sid, pillar, min_age, Utc::now())
                .await
                .unwrap();
            assert_eq!(
                got, want,
                "Cockroach must ignore provenance Derives exactly like MemoryStore (min_age {min_age:?})"
            );
        }
    }

    /// F1: a single-interaction session (temporal extent is one point) with a
    /// supported inbound dependency reports coverage 1.0, not 0.0 — parity
    /// with the MemoryStore fix (canonization Stage 2 in short sessions).
    async fn check_interaction_span_single_point_session_coverage(store: &CockroachStore) {
        let sid = SessionId::from(format!("conformance-span-single-{}", Uuid::new_v4()));
        let ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
        let i1 = NodeId::new();
        let pillar = NodeId::new();
        let orphan = NodeId::new();
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, i1, ts),
                    plant_concept(&sid, pillar, i1, "pillar", ts, None),
                    plant_concept(&sid, orphan, i1, "orphan", ts, None),
                    plant_edge(&sid, pillar, orphan, EdgeType::Dependency, ts),
                ],
            })
            .await
            .unwrap();

        let mem = MemoryStore::new();
        mem.flush(&MutationBatch {
            mutations: vec![
                plant_interaction(&sid, i1, ts),
                plant_concept(&sid, pillar, i1, "pillar", ts, None),
                plant_concept(&sid, orphan, i1, "orphan", ts, None),
                plant_edge(&sid, pillar, orphan, EdgeType::Dependency, ts),
            ],
        })
        .await
        .unwrap();

        let crdb_span = store
            .interaction_span(&sid, orphan, Duration::ZERO, Utc::now())
            .await
            .unwrap();
        let mem_span = mem
            .interaction_span(&sid, orphan, Duration::ZERO, Utc::now())
            .await
            .unwrap();
        assert_eq!(
            crdb_span, mem_span,
            "three-way parity on the single-point session"
        );
        assert_eq!(crdb_span.distinct, 1);
        assert_eq!(crdb_span.coverage, 1.0, "F1: {crdb_span:?}");

        // Unsupported target: no inbound structural edges -> 0.0 on both.
        let empty_crdb = store
            .interaction_span(&sid, pillar, Duration::ZERO, Utc::now())
            .await
            .unwrap();
        let empty_mem = mem
            .interaction_span(&sid, pillar, Duration::ZERO, Utc::now())
            .await
            .unwrap();
        assert_eq!(empty_crdb, empty_mem);
        assert_eq!(empty_crdb.distinct, 0);
        assert_eq!(empty_crdb.coverage, 0.0);
    }

    async fn check_record_canonization_appends_and_is_idempotent(store: &CockroachStore) {
        let sid = SessionId::from(format!("conformance-canon-{}", Uuid::new_v4()));
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, i1, Utc::now()),
                    plant_concept(&sid, c1, i1, "pillar", Utc::now(), None),
                ],
            })
            .await
            .unwrap();

        let event = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid.clone(),
            node_id: c1,
            from_status: CanonizationStatus::None,
            to_status: CanonizationStatus::Venerable,
            blast_radius: Some(9),
            last_demotion_time: None,
            occurred_at: Utc::now(),
        };
        store.record_canonization(&event).await.unwrap();
        // Same event re-recorded (retried flush) must not duplicate the audit row.
        store.record_canonization(&event).await.unwrap();

        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.canonization_events.len(), 1, "idempotent append");
        assert_eq!(
            snap.canonization_events[0].to_status,
            CanonizationStatus::Venerable
        );
        assert_eq!(snap.canonization_events[0].blast_radius, Some(9));
        assert_eq!(
            snap.concepts[0].canonization_status,
            CanonizationStatus::Venerable
        );
        assert_eq!(snap.concepts[0].blast_radius, Some(9));

        // Missing concept -> NotFound (MemoryStore parity).
        let ghost = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid.clone(),
            node_id: NodeId::new(),
            from_status: CanonizationStatus::None,
            to_status: CanonizationStatus::Canonical,
            blast_radius: None,
            last_demotion_time: None,
            occurred_at: Utc::now(),
        };
        let err = store.record_canonization(&ghost).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    async fn check_corrupt_contract_row_load_errors(store: &CockroachStore) {
        // STORE-7: a sessions row with embedding_dim set but embedding_kind NULL
        // (manufactured here via DIRECT SQL — the store's write path can never
        // produce it) must make load_session error like sqlite, never return a
        // silent `embedding: None`. The offline unit test
        // `session_embedding_xor_corruption_errors_not_silent_none` covers the
        // same arms without a cluster; this check proves the end-to-end read path.
        let sid = SessionId::from(format!("conformance-store7-a-{}", Uuid::new_v4()));
        let pool = store.pool().await.unwrap();
        sqlx::query("INSERT INTO sessions (session_id, embedding_dim) VALUES ($1, 1024)")
            .bind(sid.0.as_str())
            .execute(pool)
            .await
            .unwrap();
        let err = store.load_session(&sid).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("embedding_dim without embedding_kind"),
            "dim-without-kind row must error, not silently load: {err}"
        );

        // Mirror image: kind set, dim NULL.
        let sid2 = SessionId::from(format!("conformance-store7-b-{}", Uuid::new_v4()));
        sqlx::query("INSERT INTO sessions (session_id, embedding_kind) VALUES ($1, 'bge_m3')")
            .bind(sid2.0.as_str())
            .execute(pool)
            .await
            .unwrap();
        let err = store.load_session(&sid2).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("embedding_kind without embedding_dim"),
            "{err}"
        );
    }

    async fn check_unstamped_vector_candidates_are_empty_until_contract_commit(
        store: &CockroachStore,
    ) {
        let probe = embed(0.23);
        let missing = SessionId::from(format!("conformance-vector-fresh-{}", Uuid::new_v4()));
        assert!(store
            .vector_candidates(&missing, &probe, 5)
            .await
            .unwrap()
            .is_empty());

        let legacy = SessionId::from(format!("conformance-vector-legacy-{}", Uuid::new_v4()));
        let interaction = NodeId::new();
        let concept = NodeId::new();
        let now = Utc::now();
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&legacy, interaction, now),
                    plant_concept(
                        &legacy,
                        concept,
                        interaction,
                        "legacy unknown vector",
                        now,
                        Some(probe.clone()),
                    ),
                ],
            })
            .await
            .unwrap();
        assert!(store
            .vector_candidates(&legacy, &probe, 5)
            .await
            .unwrap()
            .is_empty());

        let contract = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-v1".into()),
            dim: store.vector_dim,
        };
        store
            .flush(&MutationBatch {
                mutations: vec![Mutation::SetEmbedding {
                    session_id: legacy.clone(),
                    embedding: Some(contract.clone()),
                }],
            })
            .await
            .unwrap();
        let loaded = store.load_session(&legacy).await.unwrap();
        assert_eq!(loaded.embedding, Some(contract));
        assert!(loaded.concepts[0].embedding.is_none());
        assert!(store
            .vector_candidates(&legacy, &probe, 5)
            .await
            .unwrap()
            .is_empty());

        let corrupt = SessionId::from(format!("conformance-vector-corrupt-{}", Uuid::new_v4()));
        sqlx::query("INSERT INTO sessions (session_id, embedding_dim) VALUES ($1, 1024)")
            .bind(corrupt.as_str())
            .execute(store.pool().await.unwrap())
            .await
            .unwrap();
        assert!(store.vector_candidates(&corrupt, &probe, 5).await.is_err());
    }

    /// P4 residual closure: `SET_ROOT_GOAL_SQL` (the UPDATE path) was
    /// compile-verified only until a live DSN was available. A `SetRootGoal`
    /// mutation through `flush` must persist the JSONB goal and read back
    /// identical; `None` clears it; the flush's session-row upsert creates the
    /// row so a bare goal write works without a prior seed.
    async fn check_set_root_goal_mutation_persists(store: &CockroachStore) {
        let sid = SessionId::from("live-set-root-goal");
        let goal = serde_json::json!({"text": "finish the demo", "n": 1});
        store
            .flush(&MutationBatch {
                mutations: vec![Mutation::SetRootGoal {
                    session_id: sid.clone(),
                    goal: Some(goal.clone()),
                }],
            })
            .await
            .expect("flush SetRootGoal");
        let snap = store.load_session(&sid).await.expect("load after set");
        assert_eq!(snap.root_goal, Some(goal), "goal read back identical");

        store
            .flush(&MutationBatch {
                mutations: vec![Mutation::SetRootGoal {
                    session_id: sid.clone(),
                    goal: None,
                }],
            })
            .await
            .expect("flush SetRootGoal clear");
        let snap = store.load_session(&sid).await.expect("load after clear");
        assert_eq!(snap.root_goal, None, "None clears the goal");
    }

    async fn check_set_embedding_mutation_persists(store: &CockroachStore) {
        let sid = SessionId::from(format!("live-set-embedding-{}", Uuid::new_v4()));
        let embedding = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-v1".into()),
            dim: store
                .vector_dimensions()
                .expect("Cockroach has a vector dimension"),
        };
        store
            .flush(&MutationBatch {
                mutations: vec![Mutation::SetEmbedding {
                    session_id: sid.clone(),
                    embedding: Some(embedding.clone()),
                }],
            })
            .await
            .expect("flush SetEmbedding");
        let snap = store.load_session(&sid).await.expect("load after stamp");
        assert_eq!(snap.embedding, Some(embedding));
    }

    /// All live checks run inside ONE test/runtime — see [`new_store`] for why (pool
    /// and connections must never cross Tokio runtimes). `#[ignore]`d: without
    /// `LAMBO_COCKROACH_DSN` this must report as ignored, not skip-as-green. Each
    /// check has a distinct session namespace so re-runs against a persistent cluster
    /// are idempotent.
    #[tokio::test]
    #[ignore = "requires LAMBO_COCKROACH_DSN (run live via -- --ignored)"]
    async fn conformance_suite() {
        let Some(dsn) = dsn_or_skip("conformance_suite") else {
            return;
        };
        let store = new_store(&dsn);
        check_init_schema_idempotent(&store).await;
        check_flush_mutations_batch_roundtrip(&store).await;
        check_load_missing_session_is_session_not_found(&store).await;
        check_vector_write_and_candidates_top1(&store).await;
        check_vector_candidates_are_session_scoped(&store).await;
        check_vector_explain_is_global_topk(&store).await;
        check_keyword_candidates_on_planted_concept(&store).await;
        check_keyword_mixed_case_ranks_like_memory_store(&store).await;
        check_legal_demote_flush_partial_index(&store).await;
        check_chunk_group_id_survives_flush_load(&store).await;
        check_embedding_contract_read_and_flush_immunity(&store).await;
        check_seed_load_full_snapshot_roundtrip(&store).await;
        check_structural_queries_agree_with_memory_store(&store).await;
        check_structural_queries_age_filter_agrees(&store).await;
        check_structural_queries_errata_derives_probe(&store).await;
        check_interaction_span_single_point_session_coverage(&store).await;
        check_record_canonization_appends_and_is_idempotent(&store).await;
        check_corrupt_contract_row_load_errors(&store).await;
        check_unstamped_vector_candidates_are_empty_until_contract_commit(&store).await;
        check_set_root_goal_mutation_persists(&store).await;
        check_set_embedding_mutation_persists(&store).await;
    }

    /// DECISION D1 item 3 camera-proof: the global vector query must execute as
    /// `vector search` on `concepts@concepts_embedding_idx` (spec §12.1 — "we used the
    /// CockroachDB distributed vector index", on camera).
    ///
    /// **T7.4 (2026-08-13) — this gate is no longer deployment-conditional.** The
    /// historical "PENDING on an index-favorable cluster" reading was wrong on both
    /// counts, and both causes are now fixed:
    ///
    /// 1. The assertion could never match its own output. The test used
    ///    `EXPLAIN (OPT, VERBOSE)`, whose operator is spelled `vector-search`
    ///    (hyphenated), and asserted the spaced `vector search` — so it failed at the
    ///    first assertion on ANY cluster, behind ANY index, even with a perfect vector
    ///    plan. See `dev-diary/evidence/20260813-131108-…-camera-proof-diagnosis.txt`.
    /// 2. `WHERE embedding IS NOT NULL` — which is load-bearing and must not be removed,
    ///    since NULL-`dist` rows hard-error the `f64` decode — cannot be proven implied
    ///    by a NON-partial vector index, so the optimizer planned a FULL SCAN. T7.4 made
    ///    `concepts_embedding_idx` itself PARTIAL on that same predicate in
    ///    `migrations/cockroach/001_init.sql`; the production query is unchanged.
    ///
    /// **Why plain `EXPLAIN` and not `EXPLAIN (OPT, VERBOSE)`** (a deliberate choice —
    /// each format spells the operator differently, and asserting the union of both
    /// spellings would be an assertion that cannot fail informatively):
    /// - Plain `EXPLAIN` is the format that literally emits `vector search`, the wording
    ///   DECISION D1 and spec §12.1 use, and it renders the proof in ~17 readable lines:
    ///   `• vector search / table: concepts@concepts_embedding_idx (partial index)`.
    /// - `OPT, VERBOSE` inlines the full 1024-element probe vector into the plan text,
    ///   producing a ~52 KB blob (measured). That is unusable as an on-camera artifact
    ///   and would make any assertion failure message unreadable — which is the entire
    ///   argument that had favoured it.
    ///
    /// The plan below was re-verified against the test's **bound** `$1`/`$2` over the
    /// extended protocol, not against literals: T7.3 round R2 established that a
    /// parameterized `LIMIT` can change plan shape, so a literal-`LIMIT` measurement
    /// would not have proven this test green.
    #[tokio::test]
    #[ignore = "camera-proof: set LAMBO_REQUIRE_VECTOR_INDEX=1 (spec §12.1 vector-index proof)"]
    async fn vector_explain_camera_proof() {
        // Kept behind its own env gate (not merged into `conformance_suite`) so the
        // §12.1 claim is asserted only when someone is deliberately capturing it, and
        // so a cluster provisioned from an older migration fails LOUDLY here rather
        // than reporting a green suite. `conformance_suite`'s
        // `check_vector_explain_is_global_topk` independently proves the DECISION D1
        // *shape* (global top-k, no session-filtered anti-pattern index) on every run.
        if env::var_os("LAMBO_REQUIRE_VECTOR_INDEX").is_none() {
            eprintln!(
                "vector_explain_camera_proof: skipped; set LAMBO_REQUIRE_VECTOR_INDEX=1 \
                 against a cluster provisioned from migrations/cockroach/001_init.sql"
            );
            return;
        }
        let Some(dsn) = dsn_or_skip("vector_explain_camera_proof") else {
            return;
        };
        let store = new_store(&dsn);
        let pool = store.pool().await.unwrap();
        let probe = encode_vector(&embed(0.5)).unwrap();
        // EXPLAIN the production constant ITSELF, not a hand-copied lookalike.
        // adve-review MINOR-4 caught the previous version claiming to be
        // "byte-for-byte" `VECTOR_CANDIDATES_SQL` while actually dropping its
        // `::STRING` output casts. The casts cannot change index selection, so
        // the finding was cosmetic — but the entire value of this proof is that
        // it explains the query production runs, so the claim has to be true by
        // construction rather than by careful copying. Interpolating the
        // constant also means a future edit to the query cannot silently
        // desynchronize the camera proof from it.
        let rows = sqlx::query(&format!("EXPLAIN {VECTOR_CANDIDATES_SQL}"))
            .bind(&probe)
            .bind(5i64)
            .fetch_all(pool)
            .await
            .map_err(backend)
            .unwrap();
        let plan: Vec<String> = rows
            .iter()
            .map(|r| r.try_get::<String, usize>(0).map_err(backend))
            .collect::<Result<_, _>>()
            .unwrap();
        let text = plan.join("\n");
        // The plan IS the artifact — print it so a `--nocapture` run is the camera shot.
        eprintln!("vector_explain_camera_proof plan:\n{text}");
        assert!(
            text.contains("vector search"),
            "EXPLAIN must show `vector search` (plain-EXPLAIN spelling), got:\n{text}"
        );
        assert!(
            text.contains("concepts@concepts_embedding_idx"),
            "EXPLAIN must show the vector search on concepts@concepts_embedding_idx, got:\n{text}"
        );
        // The exact regression T7.4 fixed: with a NON-partial index the predicate forces
        // `spans: FULL SCAN` on concepts_pkey. Assert it is gone, so a cluster that
        // silently reverts to a non-partial index fails with a pointed message.
        assert!(
            !text.contains("FULL SCAN"),
            "EXPLAIN must not fall back to a full scan (non-partial index?), got:\n{text}"
        );
    }
}

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
//! - **`chunk_group_id` (T2.5) is a first-class column** (P3 review round 1
//!   remediation). `concepts.chunk_group_id STRING` (nullable) is declared in the
//!   `CREATE TABLE` for fresh installs AND added via `ALTER TABLE concepts ADD COLUMN
//!   IF NOT EXISTS chunk_group_id STRING` for existing clusters (Cockroach supports
//!   `IF NOT EXISTS` on `ADD COLUMN`), so `init_schema` stays idempotent either way.
//!   The concept upsert writes it and `load_session` reads it back — a flush→load
//!   cycle now PRESERVES the T5.2 sibling co-retrieval key (regression-locked in the
//!   live conformance suite).
//! - **`GraphSnapshot::embedding` (the `EmbeddingContract`) is snapshot-only
//!   (S5-class).** The sessions table carries `embedding_kind STRING`,
//!   `embedding_model STRING`, `embedding_dim INT` (nullable; same CREATE + ALTER
//!   `ADD COLUMN IF NOT EXISTS` idempotency pattern as `chunk_group_id`), and
//!   `load_session` materializes `GraphSnapshot.embedding` when `embedding_kind` is
//!   present. `flush` does NOT write them — there is no session-metadata `Mutation`
//!   kind, so the write path is pending a future mutation (documented in the T3.2
//!   handoff; the live suite stamps them via direct SQL and asserts flush immunity).
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

use super::{Capabilities, GraphStore, StoreConfig};
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
/// closed_at. `COALESCE($3, now())` keeps the NOT NULL default when a snapshot omits it.
const UPSERT_SESSION_SQL: &str = r#"
INSERT INTO sessions (session_id, root_goal, created_at, closed_at)
VALUES ($1, $2::JSONB, COALESCE($3, now()), $4)
ON CONFLICT (session_id) DO UPDATE SET
    root_goal = EXCLUDED.root_goal,
    created_at = EXCLUDED.created_at,
    closed_at = EXCLUDED.closed_at
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
    canonization_status = EXCLUDED.canonization_status,
    blast_radius = EXCLUDED.blast_radius,
    last_demotion_time = EXCLUDED.last_demotion_time,
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
const UPDATE_CONCEPT_STATUS_SQL: &str = r#"
UPDATE concepts
SET canonization_status = $2, blast_radius = $3
WHERE id = $1 AND session_id = $4
"#;

const INSERT_CANONIZATION_EVENT_SQL: &str = r#"
INSERT INTO canonization_events (
    id, session_id, node_id, from_status, to_status, blast_radius, occurred_at
) VALUES ($1, $2, $3, $4, $5, $6, $7)
ON CONFLICT (id) DO NOTHING
"#;

const UPSERT_SYNONYM_SQL: &str = r#"
INSERT INTO synonyms (session_id, source_key, canonical_key)
VALUES ($1, $2, $3)
ON CONFLICT (session_id, source_key) DO UPDATE SET
    canonical_key = EXCLUDED.canonical_key
"#;

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
       from_status, to_status, blast_radius, occurred_at
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
/// Placeholders: `$1` session, `$2` node, `$3` cutoff timestamp.
const INTERACTION_SPAN_SQL: &str = r#"
WITH span AS (
    SELECT DISTINCT i.id AS iid, i.created_at AS ts
    FROM edges e
    JOIN concepts src ON src.id = e.source AND src.session_id = $1
    JOIN interactions i ON i.id = src.origin_interaction
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
            THEN extract(epoch FROM ((SELECT max(ts) FROM span) - (SELECT min(ts) FROM span)))
                 / extract(epoch FROM (extent.hi - extent.lo))
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

/// CockroachDB serializable transactions abort with SQLSTATE 40001
/// (`restart transaction: ... RETRY_SERIALIZABLE ...`) when they conflict with a
/// concurrent commit; sqlx does not auto-retry, so the client must replay the whole
/// transaction. Detection is on Cockroach's stable message markers — the sqlx error is
/// already flattened into [`StoreError::Backend`] by the statement helpers. Bounded
/// backoff; a genuine (non-conflict) error is returned immediately.
const TX_RETRY_ATTEMPTS: usize = 5;

fn is_retryable(e: &StoreError) -> bool {
    match e {
        StoreError::Backend(msg) => {
            msg.contains("restart transaction")
                || msg.contains("RETRY_SERIALIZABLE")
                || msg.contains("TransactionRetry")
                || msg.contains("40001")
        }
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
                if is_retryable(&err) && attempt + 1 < TX_RETRY_ATTEMPTS {
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

/// Parse the dense-vector column width out of the DDL (`VECTOR(n)`). The schema is the
/// authority on the width (spec §3.3 "Vector width: not a global constant"), so this is
/// read from the embedded `001_init.sql`, never a separate constant.
fn schema_vector_dim(ddl: &str) -> Option<usize> {
    let start = ddl.find("VECTOR(")?;
    let rest = &ddl[start + "VECTOR(".len()..];
    let end = rest.find(')')?;
    rest[..end].trim().parse().ok()
}

/// Encode an embedding as Cockroach's `VECTOR` text literal `[x,y,z]` (T0.3 Attempt A:
/// bind text, cast `$n::VECTOR` server-side). Rust `f32` `Display` is shortest-round-trip
/// so encode→decode is exact. Rejects non-finite elements (a `NaN`/`Inf` vector is not a
/// legal embedding and Cockroach would reject the literal).
fn encode_vector(v: &[f32]) -> Result<String, StoreError> {
    if let Some(bad) = v.iter().find(|x| !x.is_finite()) {
        return Err(StoreError::Backend(format!(
            "embedding contains non-finite value {bad} (at index {:?})",
            v.iter().position(|x| !x.is_finite())
        )));
    }
    let mut s = String::with_capacity(v.len() * 8);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{x}"));
    }
    s.push(']');
    Ok(s)
}

/// Inverse of [`encode_vector`] — parses the `embedding::STRING` read-back form.
fn decode_vector(s: &str) -> Result<Vec<f32>, StoreError> {
    let t = s.trim().trim_start_matches('[').trim_end_matches(']');
    if t.is_empty() {
        return Ok(Vec::new());
    }
    t.split(',')
        .map(|p| {
            p.trim().parse::<f32>().map_err(|e| {
                StoreError::Backend(format!("decode VECTOR element {p:?} from {s:?}: {e}"))
            })
        })
        .collect()
}

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

/// `spec §4.1` L2 distance → cosine similarity (`1 - d²/2`); see module doc.
fn distance_to_score(dist: f64) -> f64 {
    (1.0 - 0.5 * dist * dist).clamp(-1.0, 1.0)
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
                PgPoolOptions::new()
                    .max_connections(MAX_POOL_CONNECTIONS)
                    .connect_lazy(&self.dsn)
                    .map_err(|e| backend(format!("connect Cockroach pool: {e}")))
            })
            .await
    }

    /// Seed a prebuilt snapshot directly (fixtures track, MemoryStore parity). Writes all
    /// seven tables in one transaction — the full-snapshot path that carries synonyms and
    /// reservations (they have no `Mutation` kind, S5 contract).
    #[cfg(feature = "fixtures")]
    pub async fn seed(&self, snapshot: &GraphSnapshot) -> Result<(), StoreError> {
        let sid = &snapshot.session_id.0;
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
        tx_retry(|| async move {
            let mut tx = pool.begin().await.map_err(backend)?;
            sqlx::query(UPSERT_SESSION_SQL)
                .bind(sid)
                .bind(root_goal)
                .bind(snapshot.created_at)
                .bind(snapshot.closed_at)
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
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
                    .map_err(backend)?;
            }
            for r in &snapshot.reservations {
                sqlx::query(UPSERT_RESERVATION_SQL)
                    .bind(&r.session_id.0)
                    .bind(r.node_id.0)
                    .bind(&r.agent_id.0)
                    .bind(r.expires_at)
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
            }
            for ev in &snapshot.canonization_events {
                insert_canonization_event(&mut *tx, ev).await?;
            }
            tx.commit().await.map_err(backend)?;
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
        .map_err(backend)?;
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
        .map_err(backend)?;
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
        .map_err(backend)?;
    Ok(())
}

async fn insert_canonization_event(
    tx: &mut sqlx::PgConnection,
    ev: &CanonizationEvent,
) -> Result<(), StoreError> {
    sqlx::query(INSERT_CANONIZATION_EVENT_SQL)
        .bind(ev.id.0)
        .bind(&ev.session_id.0)
        .bind(ev.node_id.0)
        .bind(canonization_status_sql(ev.from_status))
        .bind(canonization_status_sql(ev.to_status))
        .bind(ev.blast_radius)
        .bind(ev.occurred_at)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
    Ok(())
}

/// Apply a canonization transition: update the concept row, then append the audit event.
/// Missing concept → `NotFound` (MemoryStore parity).
async fn apply_canonization(
    tx: &mut sqlx::PgConnection,
    ev: &CanonizationEvent,
) -> Result<(), StoreError> {
    let res = sqlx::query(UPDATE_CONCEPT_STATUS_SQL)
        .bind(ev.node_id.0)
        .bind(canonization_status_sql(ev.to_status))
        .bind(ev.blast_radius)
        .bind(&ev.session_id.0)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
    if res.rows_affected() == 0 {
        return Err(StoreError::NotFound(format!(
            "concept {} for canonization",
            ev.node_id
        )));
    }
    insert_canonization_event(tx, ev).await
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
            let mut tx = pool.begin().await.map_err(backend)?;
            // Ensure a sessions row for every session the batch writes into — the DDL
            // enforces `REFERENCES sessions(session_id)` on interactions/concepts, and the
            // graph tier creates sessions implicitly (MemoryStore::ensure_session parity).
            let mut sids: Vec<String> = Vec::new();
            for m in &batch.mutations {
                let sid = match m {
                    Mutation::UpsertNode { node } => node.session_id().as_str(),
                    Mutation::UpsertEdge { edge } => edge.session_id.as_str(),
                    Mutation::CanonizationTransition { event } => event.session_id.as_str(),
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
                    .map_err(backend)?;
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
                            .map_err(backend)?;
                        sqlx::query(DELETE_NODE_CONCEPTS_SQL)
                            .bind(id.0)
                            .execute(&mut *tx)
                            .await
                            .map_err(backend)?;
                        sqlx::query(DELETE_NODE_INTERACTIONS_SQL)
                            .bind(id.0)
                            .execute(&mut *tx)
                            .await
                            .map_err(backend)?;
                    }
                    Mutation::DeleteEdge { id } => {
                        sqlx::query(DELETE_EDGE_SQL)
                            .bind(id.0)
                            .execute(&mut *tx)
                            .await
                            .map_err(backend)?;
                    }
                    Mutation::CanonizationTransition { event } => {
                        apply_canonization(&mut *tx, event).await?;
                    }
                }
            }
            tx.commit().await.map_err(backend)?;
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

            // Snapshot-only embedding contract (S5-class, see module doc): read the
            // nullable kind/model/dim columns into GraphSnapshot.embedding when a
            // contract is stamped. `flush` never writes these — there is no
            // session-metadata Mutation kind.
            let embedding_kind: Option<String> =
                session_row.try_get("embedding_kind").map_err(backend)?;
            let embedding_model: Option<String> =
                session_row.try_get("embedding_model").map_err(backend)?;
            let embedding_dim: Option<i64> =
                session_row.try_get("embedding_dim").map_err(backend)?;
            let embedding = match embedding_kind {
                Some(kind) => Some(EmbeddingContract {
                    kind,
                    model: embedding_model,
                    dim: embedding_dim.ok_or_else(|| {
                        StoreError::Backend(
                            "embedding_kind present but embedding_dim NULL in sessions row".into(),
                        )
                    })? as usize,
                }),
                None => None,
            };

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
        if limit == 0 {
            return Ok(Vec::new());
        }
        check_embedding_dim(embedding, self.vector_dim)?;
        if !self.session_exists(session).await? {
            return Err(StoreError::SessionNotFound(session.0.clone()));
        }
        let pool = self.pool().await?;
        let probe = encode_vector(embedding)?;
        let rows = sqlx::query(
            r#"
            SELECT id::STRING AS id, embedding <-> $2::VECTOR AS dist
            FROM concepts
            WHERE session_id = $1 AND embedding IS NOT NULL
            ORDER BY dist ASC
            LIMIT $3
            "#,
        )
        .bind(&session.0)
        .bind(&probe)
        .bind(limit as i64)
        .fetch_all(pool)
        .await
        .map_err(backend)?;

        let mut scored = Vec::with_capacity(rows.len());
        for r in &rows {
            let id: String = r.try_get("id").map_err(backend)?;
            let dist: f64 = r.try_get("dist").map_err(backend)?;
            scored.push(Scored::new(parse_node_id(&id)?, distance_to_score(dist)));
        }
        Ok(scored)
    }

    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
    ) -> Result<u64, StoreError> {
        if !self.session_exists(session).await? {
            return Err(StoreError::SessionNotFound(session.0.clone()));
        }
        let pool = self.pool().await?;
        let cutoff = cutoff(Utc::now(), min_edge_age)?;
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
    ) -> Result<InteractionSpan, StoreError> {
        if !self.session_exists(session).await? {
            return Err(StoreError::SessionNotFound(session.0.clone()));
        }
        let pool = self.pool().await?;
        let cutoff = cutoff(Utc::now(), min_age)?;
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
            let mut tx = pool.begin().await.map_err(backend)?;
            apply_canonization(&mut *tx, event).await?;
            tx.commit().await.map_err(backend)?;
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

    fn sample_vec(dim: usize, seed: f32) -> Vec<f32> {
        (0..dim)
            .map(|i| ((i as f32 + 1.0) * seed).sin() * 0.5)
            .collect()
    }

    #[test]
    fn vector_encode_decode_roundtrip_exact() {
        for dim in [0usize, 1, 8, 1024] {
            let v = sample_vec(dim, 0.17);
            let text = encode_vector(&v).unwrap();
            assert!(text.starts_with('[') && text.ends_with(']'));
            let back = decode_vector(&text).unwrap();
            assert_eq!(
                v, back,
                "dim {dim}: encode -> decode must be exact (shortest f32 repr)"
            );
        }
    }

    #[test]
    fn vector_decode_accepts_cockroach_renderings() {
        // Cockroach `embedding::STRING` output has no spaces; tolerate any whitespace.
        assert_eq!(
            decode_vector("[0.5,-0.25,1e2]").unwrap(),
            vec![0.5, -0.25, 100.0]
        );
        assert_eq!(decode_vector("[]").unwrap(), Vec::<f32>::new());
        assert_eq!(decode_vector(" [ 1 , 2 ] ").unwrap(), vec![1.0, 2.0]);
        assert!(decode_vector("[1,oops]").is_err());
    }

    #[test]
    fn vector_encode_rejects_non_finite() {
        let mut v = sample_vec(4, 1.0);
        v[2] = f32::NAN;
        assert!(encode_vector(&v).is_err());
        v[2] = f32::INFINITY;
        assert!(encode_vector(&v).is_err());
    }

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
        assert_eq!(placeholder_max(UPSERT_SESSION_SQL), 4);
        assert_eq!(placeholder_max(INSERT_CANONIZATION_EVENT_SQL), 7);
        assert_eq!(placeholder_max(UPSERT_SYNONYM_SQL), 3);
        assert_eq!(placeholder_max(UPSERT_RESERVATION_SQL), 4);
        // The vector column carries the ::VECTOR cast; chunk_group_id (T2.5) is the
        // 16th, nullable, and included in the conflict UPDATE.
        assert!(UPSERT_CONCEPT_SQL.contains("$15::VECTOR"));
        assert!(UPSERT_CONCEPT_SQL.contains("embedding = EXCLUDED.embedding"));
        assert!(UPSERT_CONCEPT_SQL.contains("chunk_group_id = EXCLUDED.chunk_group_id"));
        // Edge conflict targets the natural key; id is replaceable on conflict.
        assert!(UPSERT_EDGE_SQL.contains("ON CONFLICT (source, target, edge_type)"));
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
    fn serializable_retry_detection() {
        // Cockroach prefixes every retriable serialization abort with this marker.
        let retry = StoreError::Backend(
            "restart transaction: TransactionRetryWithProtoRefreshError: TransactionRetryError: \
             retry txn (RETRY_SERIALIZABLE - failed preemptive refresh...)"
                .into(),
        );
        assert!(is_retryable(&retry));
        let code = StoreError::Backend("db error: SQLSTATE 40001".into());
        assert!(is_retryable(&code));
        let real = StoreError::Backend("relation \"concepts\" does not exist".into());
        assert!(!is_retryable(&real));
        assert!(!is_retryable(&StoreError::SessionNotFound("x".into())));
        assert!(!is_retryable(&StoreError::Capability("nope".into())));
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
}

// ---------------------------------------------------------------------------
// Live conformance (feature-gated; skips cleanly without LAMBO_COCKROACH_DSN)
// ---------------------------------------------------------------------------

/// Runs under `cargo test --features store-cockroach` (with `fixtures`, which is in the
/// default set). Every test SKIPS (prints a notice, returns Ok) when
/// `LAMBO_COCKROACH_DSN` is unset — never fails. The DSN is read from the environment
/// only and never printed.
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

    /// Skip-not-fail helper; returns the DSN when present.
    fn dsn_or_skip(test: &str) -> Option<String> {
        match dsn() {
            Some(d) => Some(d),
            None => {
                eprintln!("SKIP {test}: LAMBO_COCKROACH_DSN not set");
                None
            }
        }
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
    /// the rustls rewrite + parse validation at construction.
    #[test]
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
        // Embedding contract (S5-class snapshot metadata): load_session reads
        // embedding_kind/model/dim into GraphSnapshot.embedding when present, and
        // flush does NOT write or clobber them (no session-metadata Mutation kind).
        // The live suite stamps them via direct SQL because no flush path exists.
        let sid = SessionId::from(format!("conformance-embed-{}", Uuid::new_v4()));
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, i1, Utc::now()),
                    plant_concept(&sid, c1, i1, "seed concept", Utc::now(), None),
                ],
            })
            .await
            .unwrap();
        let pool = store.pool().await.unwrap();
        sqlx::query(
            "UPDATE sessions SET embedding_kind = $2, embedding_model = $3, embedding_dim = $4 \
             WHERE session_id = $1",
        )
        .bind(&sid.0)
        .bind("bge_m3")
        .bind("BAAI/bge-m3")
        .bind(1024_i32)
        .execute(pool)
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
            "load_session must materialize the stamped embedding contract"
        );
        // A subsequent flush (which only ensures the session row) must not clobber the
        // snapshot-only metadata.
        store
            .flush(&MutationBatch {
                mutations: vec![plant_concept(
                    &sid,
                    NodeId::new(),
                    i1,
                    "more concepts",
                    Utc::now(),
                    None,
                )],
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

    async fn check_structural_queries_agree_with_memory_store(store: &CockroachStore) {
        let snap = load_snapshot("session-rest-api").unwrap();
        let sid = snap.session_id.clone();
        store.seed(&snap).await.unwrap();

        let mem = {
            let m = MemoryStore::new();
            m.seed(snap.clone()).unwrap();
            Arc::new(m)
        };

        // Three-way agreement probe (T3.6 will formalize): every concept, both queries,
        // min-age 0 (fixture timestamps are in the past, so both cutoffs admit all).
        for c in &snap.concepts {
            let mem_br = mem.blast_radius(&sid, c.id, Duration::ZERO).await.unwrap();
            let crdb_br = store
                .blast_radius(&sid, c.id, Duration::ZERO)
                .await
                .unwrap();
            assert_eq!(mem_br, crdb_br, "blast_radius({})", c.id);
            let mem_span = mem
                .interaction_span(&sid, c.id, Duration::ZERO)
                .await
                .unwrap();
            let crdb_span = store
                .interaction_span(&sid, c.id, Duration::ZERO)
                .await
                .unwrap();
            assert_eq!(
                mem_span, crdb_span,
                "interaction_span({}): mem={mem_span:?} crdb={crdb_span:?}",
                c.id
            );
        }
    }

    async fn check_structural_queries_age_filter_agrees(store: &CockroachStore) {
        let sid = SessionId::from(format!("conformance-age-{}", Uuid::new_v4()));
        let old_ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
        let i1 = NodeId::new();
        let pillar = NodeId::new();
        let orphan = NodeId::new();
        let other = NodeId::new();

        // Base: only an aged pillar -> orphan dependency.
        let base = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, i1, old_ts),
                plant_concept(&sid, pillar, i1, "pillar", old_ts, None),
                plant_concept(&sid, orphan, i1, "orphan", old_ts, None),
                plant_concept(&sid, other, i1, "other", old_ts, None),
                plant_edge(&sid, pillar, orphan, EdgeType::Dependency, old_ts),
            ],
        };
        store.flush(&base).await.unwrap();
        let mem = MemoryStore::new();
        mem.flush(&base).await.unwrap();
        // Then a genuinely FRESH other -> orphan dependency (created now).
        let fresh = MutationBatch {
            mutations: vec![plant_edge(
                &sid,
                other,
                orphan,
                EdgeType::Dependency,
                Utc::now(),
            )],
        };
        store.flush(&fresh).await.unwrap();
        mem.flush(&fresh).await.unwrap();

        let one_hour = Duration::from_secs(3600);
        for min_age in [Duration::ZERO, one_hour] {
            let mem_br = mem.blast_radius(&sid, pillar, min_age).await.unwrap();
            let crdb_br = store.blast_radius(&sid, pillar, min_age).await.unwrap();
            assert_eq!(mem_br, crdb_br, "blast_radius(min_age={min_age:?})");
            let mem_span = mem.interaction_span(&sid, pillar, min_age).await.unwrap();
            let crdb_span = store.interaction_span(&sid, pillar, min_age).await.unwrap();
            assert_eq!(mem_span, crdb_span, "interaction_span(min_age={min_age:?})");
        }
        // The age filter is doing real work: with min_age=0 the fresh edge un-orphans;
        // with min_age=1h it is filtered and the orphan still counts.
        assert_eq!(
            store
                .blast_radius(&sid, pillar, Duration::ZERO)
                .await
                .unwrap(),
            0,
            "fresh edge counts at min_age=0"
        );
        assert_eq!(
            store.blast_radius(&sid, pillar, one_hour).await.unwrap(),
            1,
            "fresh edge filtered at min_age=1h"
        );
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
            .interaction_span(&sid, orphan, Duration::ZERO)
            .await
            .unwrap();
        let mem_span = mem
            .interaction_span(&sid, orphan, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(crdb_span, mem_span, "three-way parity on the single-point session");
        assert_eq!(crdb_span.distinct, 1);
        assert_eq!(crdb_span.coverage, 1.0, "F1: {crdb_span:?}");

        // Unsupported target: no inbound structural edges -> 0.0 on both.
        let empty_crdb = store
            .interaction_span(&sid, pillar, Duration::ZERO)
            .await
            .unwrap();
        let empty_mem = mem
            .interaction_span(&sid, pillar, Duration::ZERO)
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
            occurred_at: Utc::now(),
        };
        let err = store.record_canonization(&ghost).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    /// All live checks run inside ONE test/runtime — see [`new_store`] for why (pool
    /// and connections must never cross Tokio runtimes). Skips cleanly, never fails,
    /// when `LAMBO_COCKROACH_DSN` is unset. Each check has a distinct session namespace
    /// so re-runs against a persistent cluster are idempotent.
    #[tokio::test]
    async fn conformance_suite() {
        let Some(dsn) = dsn_or_skip("conformance_suite") else {
            return;
        };
        let store = new_store(&dsn);
        check_init_schema_idempotent(&store).await;
        check_flush_mutations_batch_roundtrip(&store).await;
        check_load_missing_session_is_session_not_found(&store).await;
        check_vector_write_and_candidates_top1(&store).await;
        check_keyword_candidates_on_planted_concept(&store).await;
        check_keyword_mixed_case_ranks_like_memory_store(&store).await;
        check_legal_demote_flush_partial_index(&store).await;
        check_chunk_group_id_survives_flush_load(&store).await;
        check_embedding_contract_read_and_flush_immunity(&store).await;
        check_seed_load_full_snapshot_roundtrip(&store).await;
        check_structural_queries_agree_with_memory_store(&store).await;
        check_structural_queries_age_filter_agrees(&store).await;
        check_interaction_span_single_point_session_coverage(&store).await;
        check_record_canonization_appends_and_is_idempotent(&store).await;
    }
}

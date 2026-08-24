//! B0: the Postgres-wire-protocol store **family**, `PgStore<D: Dialect>`.
//!
//! `pg` is the family, not an implementation. CockroachDB and PostgreSQL both
//! speak the Postgres wire protocol through the *same* `sqlx` driver, the same
//! pool and the same row types, so everything below is written once and only
//! the [`Dialect`] differs. One naming collision, killed here so nobody has to
//! re-derive it: the config alias `"pg"` means the **PostgreSQL
//! implementation**, while the module `pg/` means the **family** (PostgreSQL,
//! CockroachDB, and future wire-compatible stores).
//!
//! # What lives here, and what does not
//!
//! Here: the pool and its lazy construction, the retry wrapper, flush planning
//! and the fencing gate, session load, the structural queries, quarantine, the
//! statement helpers, and the single `impl GraphStore`. Statements are written
//! **once**, in this file, either as constants (identical on every engine) or
//! in `DialectSql` (identical except for a cast token).
//!
//! In `dialect.rs`: the §B3 table plus the B2-discovered over-merge split
//! (post-init statements, connect-option session settings, operator-facing
//! names). **The over-merging trap, named so it is not walked into:** a
//! function belongs here only when its SQL is byte-identical for both dialects.
//! If it differs by one cast it is composed from the dialect's tokens; if it
//! differs by more, it does not belong in the shared base at all, even where a
//! `bool` parameter could force it into one body. A base full of `if cockroach`
//! branches recreates the drift problem inside the shared code, where it is
//! harder to see.
//!
//! In `cockroach` (feature `store-cockroach`): `CockroachDialect`, the embedded
//! `001_init.sql`, and the width-from-DDL authority. **The T3.2 design log**
//! for everything in this file, including the batch replay order, the
//! `ON CONFLICT` targets, the §4.1 query semantics and the vector
//! encode/decode contract, is the module doc on that dialect: it was written
//! and reviewed against this code and B0 moves the code without rewriting the
//! record of why it is shaped this way.
//!
//! In `postgres` (feature `store-postgres`): `PostgresDialect`, templated
//! width + hnsw from init (B2), cosine-distance ranking (B3: `<=>` and
//! score `1 - d`). It does not copy Cockroach SQL.
//!
//! # Dialect-aware as of B2, still recorded where B3 owns the rest
//!
//! B0 shipped **one** working dialect. B2 splits the two over-merged
//! functions (`init_schema` endpoint type, `connect_options` ANN session
//! setting) now that a second dialect exists. Operator-facing strings that
//! named Cockroach (DSN errors, preflight `NAME`) moved onto [`Dialect`]
//! with them. `tx_retry`'s exhaustion wording (B0-N5) is still inline:
//! the retry mechanism is shared; only the message names Cockroach.

// Clippy's `explicit_auto_deref` suggestion is wrong for sqlx: `&mut *tx` reborrows
// the `Transaction` (which implements `sqlx::Executor`), while the suggested `&mut tx`
// produces `&mut &mut Transaction` (which does not). Known sqlx+clippy false-positive;
// kept explicit on purpose.
#![allow(clippy::explicit_auto_deref)]

mod dialect;
pub use dialect::Dialect;

// T3.2 — CockroachDB durable adapter (spec §3.2/§3.3, §4), the family's first
// dialect. Feature: store-cockroach.
#[cfg(feature = "store-cockroach")]
pub mod cockroach;

// B2: PostgreSQL + pgvector dialect. Feature: store-postgres. Templated
// width and hnsw from init; do not copy Cockroach SQL (see postgres.rs).
#[cfg(feature = "store-postgres")]
pub mod postgres;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::borrow::Cow;
use std::future::Future;
use std::marker::PhantomData;
use std::time::Duration;
use uuid::Uuid;

use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};

use crate::store::batch::{
    batch_session_ids, plan_flush, BulkLimits, ConceptRow, FlushStep, CONCEPT_COLUMNS,
    EDGE_COLUMNS, INTERACTION_COLUMNS,
};
#[cfg(feature = "fixtures")]
use crate::store::batch::{seed_concept_rows, seed_edge_rows};
use crate::store::lease::{lease_permits_write, LeaseHolder, LeaseInfo, LeaseOutcome};
use crate::store::vector::{decode_vector, encode_vector};
use crate::store::{
    columns_in_ddl, map_write_err, tables_in_ddl, unprovisioned_column_err,
    unprovisioned_store_err, validate_vector_candidate_limit, Capabilities, GraphStore,
    SessionFlushStats, StoreConfig,
};
use crate::types::{
    CanonizationEvent, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, EmbeddingContract,
    GraphSnapshot, Interaction, InteractionSpan, Mutation, MutationBatch, Node, NodeId,
    Reservation, Scored, SessionId, StoreError, Synonym,
};

/// Pool size is deliberately small: Lambo is single-writer per session (spec §2.4) and
/// the demo runs one process.
const MAX_POOL_CONNECTIONS: u32 = 4;

/// Opt-in to Cloud SQL IAM database authentication as the shared service account.
#[cfg(feature = "store-postgres")]
pub(crate) const LAMBO_POSTGRES_IAM_ENV: &str = "LAMBO_POSTGRES_IAM";

/// Whether this process was told to log in with an IAM token instead of a password.
#[cfg(feature = "store-postgres")]
fn iam_auth_requested() -> bool {
    std::env::var_os(LAMBO_POSTGRES_IAM_ENV).is_some_and(|v| !v.is_empty())
}

/// The IAM opt-in as it stood when the store was constructed.
///
/// Read in `PgStore::new`, which is **synchronous on purpose**, for the same reason
/// [`PgStore::connect_options`] is: a test that pins an env-driven option must be able to
/// do it without holding a lock across an `.await` (spec §6.4, enforced by
/// `clippy::await_holding_lock`). It also means the login mode is decided once, at the
/// same moment the DSN is, rather than re-read on every query.
#[cfg(feature = "store-postgres")]
#[derive(Debug, Clone)]
struct IamSetup {
    /// Shared credential file, or `None` when neither variable named one (which is an
    /// error the first pool reports, naming both variables).
    credentials: Option<std::path::PathBuf>,
}

/// The shared-service-account login state: the token source, and the pool the current
/// token authorised together with the instant that token stops being handed out.
///
/// Held behind a `tokio::sync::Mutex` because rotating it is an `await` (the token mint),
/// and because two concurrent callers must not mint two tokens and build two pools.
#[cfg(feature = "store-postgres")]
struct IamAuth {
    source: crate::gcp_auth::GoogleOAuthTokenSource,
    live: Option<(PgPool, std::time::Instant)>,
}

/// Rows per multi-row upsert statement (L82-1).
///
/// The hard ceiling is PostgreSQL's wire-protocol limit of 65535 bind
/// parameters per statement: 16 columns caps `concepts` at 4095 rows and 9
/// columns caps `edges` at 7281. These sit an order of magnitude below that —
/// enough that the live finding's 784-mutation tail plans into single-digit
/// statements, while keeping any one statement small enough that CockroachDB
/// plans it without trouble and a retry re-sends little.
///
/// **`interactions` batches now too.** `interactions.previous_id REFERENCES
/// interactions(id)` is a *self* foreign key, and a batch's interactions form a
/// chain — which is why this used to be 1 ("row-at-a-time costs nothing").
/// F4 disproved the "costs nothing": `record_action` emits one interaction per
/// call and concepts/edges batch, so the *un-batched* interactions became the
/// largest per-statement contributor to the close-flush round-trip count (30 of
/// ~37 at a K=30 deferred tail). Batching them is safe because
/// `dedupe_last_at_first_position` (R1-1) emits each interaction at its first
/// occurrence — reference-before-use — and both engines check the self-FK at
/// end-of-statement, so a chain inside one multi-row statement is satisfied; a
/// chain split across statements still passes because the reference is written
/// in an earlier statement of the same transaction. Verified against the live
/// Cockroach cluster (F4 re-measurement).
const BULK_LIMITS: BulkLimits = BulkLimits {
    interactions: 256,
    concepts: 256,
    edges: 512,
};

/// PostgreSQL's wire-protocol ceiling on bind parameters per statement, which
/// CockroachDB inherits by speaking the same protocol.
const PG_MAX_BIND_PARAMETERS: usize = 65535;

// R1-4: the ceiling arithmetic above is prose, and prose does not fail a build.
// A column added to `concepts` without revisiting the row limit would push a
// chunk over the wire limit and only be discovered against a real server; these
// turn it into a compile error.
const _: () = assert!(
    BULK_LIMITS.interactions * INTERACTION_COLUMNS <= PG_MAX_BIND_PARAMETERS,
    "interactions chunk exceeds the PostgreSQL bind-parameter limit"
);
const _: () = assert!(
    BULK_LIMITS.concepts * CONCEPT_COLUMNS <= PG_MAX_BIND_PARAMETERS,
    "concepts chunk exceeds the PostgreSQL bind-parameter limit"
);
const _: () = assert!(
    BULK_LIMITS.edges * EDGE_COLUMNS <= PG_MAX_BIND_PARAMETERS,
    "edges chunk exceeds the PostgreSQL bind-parameter limit"
);

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

/// Upserts are issued as **multi-row** statements (L82-1), so each is built as
/// `PREFIX` + a `VALUES` list of however many rows the plan put in the chunk +
/// `ON CONFLICT`. Splitting the statement in two named halves is what lets one
/// definition serve a 1-row seed and a 256-row flush chunk without the two
/// drifting apart. `sqlx::QueryBuilder` numbers the placeholders.
const INSERT_INTERACTION_PREFIX_SQL: &str = r#"
INSERT INTO interactions (
    id, session_id, agent_id, prompt_text, previous_id, created_at, event_time
) "#;

const ON_CONFLICT_INTERACTION_SQL: &str = r#"
ON CONFLICT (id) DO UPDATE SET
    session_id = EXCLUDED.session_id,
    agent_id = EXCLUDED.agent_id,
    prompt_text = EXCLUDED.prompt_text,
    previous_id = EXCLUDED.previous_id,
    created_at = EXCLUDED.created_at,
    event_time = EXCLUDED.event_time
"#;

/// 17 columns; `embedding` is bound as text and cast server-side with the
/// dialect's `VECTOR_CAST` (`$15::VECTOR` on Cockroach);
/// `chunk_group_id` (T2.5 sibling co-retrieval key) is the 16th, bound nullable;
/// `human_confirmed` (C2 solo-score input) is the 17th, bound as an INT count.
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
const INSERT_CONCEPT_PREFIX_SQL: &str = r#"
INSERT INTO concepts (
    id, session_id, content, canonical_key, concept_type,
    origin_interaction, origin_agent, created_at, access_count, last_accessed,
    gc_survived, canonization_status, blast_radius, last_demotion_time, embedding,
    chunk_group_id, human_confirmed
) "#;

const ON_CONFLICT_CONCEPT_SQL: &str = r#"
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
    chunk_group_id = EXCLUDED.chunk_group_id,
    human_confirmed = EXCLUDED.human_confirmed
"#;

/// Natural-key conflict target `(source, target, edge_type)` matches the graph tier's
/// `record_edge` dedup: a duplicate natural key **reinforces** (replaces) the row while
/// preserving nothing — the incoming record is authoritative (I2 convention: graph core
/// counts creation as the first write, `reinforcements = 1`; we store its values, never
/// the DDL default 0). Updating `id` on conflict mirrors MemoryStore's whole-record
/// replace; the graph never reuses an id with a different natural key.
const INSERT_EDGE_PREFIX_SQL: &str = r#"
INSERT INTO edges (
    id, session_id, source, target, edge_type, weight, reinforcements,
    created_at, last_reinforced, event_time
) "#;

const ON_CONFLICT_EDGE_SQL: &str = r#"
ON CONFLICT (source, target, edge_type) DO UPDATE SET
    id = EXCLUDED.id,
    session_id = EXCLUDED.session_id,
    weight = EXCLUDED.weight,
    reinforcements = EXCLUDED.reinforcements,
    created_at = EXCLUDED.created_at,
    last_reinforced = EXCLUDED.last_reinforced,
    event_time = EXCLUDED.event_time
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

/// Stamping a contract over an **unstamped** session NULLs its vectors: nothing
/// attested which space they were in, so they are unreadable by construction.
///
/// **Deliberate divergence from SQLite (F-R2-1), recorded here so it reads as a
/// decision and not an oversight.** `sqlite.rs`'s `set_embedding` widened the same
/// predicate to fire on any *width* change, not only over a NULL contract, because a
/// restamp there could leave earlier vectors under a width they no longer match. That
/// shape cannot arise on Cockroach: `concepts.embedding` is `VECTOR(1024)` in the DDL
/// (`migrations/cockroach/001_init.sql`), so every stored vector is exactly that wide
/// or NULL and no row can decode to an unexpected width — a restamp to some other
/// width instead makes the session refuse loudly at `check_embedding_dim` against the
/// DDL-parsed authority, before a single candidate row is read. SQLite's width-agnostic
/// `BLOB` has no such authority, which is why only it needs the wider rule. Widen this
/// statement too if Cockroach's width ever becomes configurable per deployment (B2's
/// Postgres split is where that would land).
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

// Fixtures `seed` and the Cockroach SQL-shape tests. Not part of the B1
// postgres stub's surface: compiling them under store-postgres-only would
// be dead_code (no reader).
#[cfg(any(feature = "fixtures", all(test, feature = "store-cockroach")))]
const UPSERT_SYNONYM_SQL: &str = r#"
INSERT INTO synonyms (session_id, source_key, canonical_key)
VALUES ($1, $2, $3)
ON CONFLICT (session_id, source_key) DO UPDATE SET
    canonical_key = EXCLUDED.canonical_key
"#;

#[cfg(any(feature = "fixtures", all(test, feature = "store-cockroach")))]
const UPSERT_RESERVATION_SQL: &str = r#"
INSERT INTO reservations (session_id, node_id, agent_id, expires_at)
VALUES ($1, $2, $3, $4)
ON CONFLICT (session_id, node_id) DO UPDATE SET
    agent_id = EXCLUDED.agent_id,
    expires_at = EXCLUDED.expires_at
"#;

const SELECT_SYNONYMS_SQL: &str = r#"
SELECT session_id, source_key, canonical_key
FROM synonyms
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
        AND COALESCE(e.event_time, e.created_at) <= $3
  )
  AND NOT EXISTS (
      SELECT 1 FROM edges e2
      JOIN concepts src2 ON src2.id = e2.source AND src2.session_id = $1
      WHERE e2.target = c.id AND e2.source <> $2
        AND e2.edge_type IN ('Dependency', 'Causal', 'Hierarchical')
        AND COALESCE(e2.event_time, e2.created_at) <= $3
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
    SELECT DISTINCT i.id AS iid, COALESCE(i.event_time, i.created_at) AS ts
    FROM edges e
    JOIN concepts src ON src.id = e.source AND src.session_id = $1
    JOIN interactions i ON i.id = src.origin_interaction AND i.session_id = $1
    WHERE e.target = $2
      AND e.session_id = $1
      AND e.edge_type IN ('Dependency', 'Causal', 'Hierarchical')
      AND COALESCE(e.event_time, e.created_at) <= $3
      AND COALESCE(i.event_time, i.created_at) <= $3
),
extent AS (
    SELECT min(COALESCE(event_time, created_at)) AS lo,
           max(COALESCE(event_time, created_at)) AS hi
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

/// Every statement whose only dialect variation is a cast token or the distance
/// operator, built once per store from [`Dialect::STRING_CAST`],
/// [`Dialect::VECTOR_CAST`] and [`Dialect::DISTANCE_OP`].
///
/// **Built once, not per call.** `vector_candidates` re-issues its statement
/// inside a grow-and-retry loop and `load_session` issues six of these in one
/// transaction, so composing them on every query would put an allocation on
/// the hot recall path that the constants it replaces did not have.
///
/// **The SQL text is written exactly once**, here, so two dialects cannot drift
/// by anything except the tokens they substitute. That is the whole point of
/// the extraction: a statement that differs by *more* than a token does not
/// belong in this struct, and does not belong in [`PgStore`] either.
struct DialectSql {
    /// This dialect's cast to its dense-vector type, carried as a token rather
    /// than baked into a statement: it rides the concept upsert's embedding
    /// placeholder, which `sqlx::QueryBuilder` numbers at build time.
    vector_cast: &'static str,

    /// GLOBAL vector top-k — deliberately omits any session predicate so the planner
    /// uses `concepts@concepts_embedding_idx` (DECISION D1). `session_id` is selected so
    /// the Rust side can drop foreign-session rows. Ordering is distance ascending,
    /// i.e. similarity (score) descending. The adapter requests `k + 1`:
    /// a lookahead tied with the kth distance triggers the exact, UUID-ordered
    /// session fallback, while an untied boundary remains on this index-friendly path.
    vector_candidates: String,

    /// Correctness fallback when foreign-session rows crowd the caller out of the
    /// capped global index query. This deliberately prioritizes exact session-local
    /// top-k over index use; it runs only after the bounded fast path is exhausted.
    session_vector_candidates: String,

    /// Full-snapshot sessions upsert (fixtures `seed` path): root_goal JSONB, created_at,
    /// closed_at, and the `EmbeddingContract` columns (STORE-1). `COALESCE($3, now())`
    /// keeps the NOT NULL default when a snapshot omits it.
    #[cfg(any(feature = "fixtures", all(test, feature = "store-cockroach")))]
    upsert_session: String,

    /// `Mutation::SetEmbedding`'s session-column write.
    set_embedding: String,

    /// The session row `load_session` and the checked vector read both start from.
    select_session: String,
    select_interactions: String,
    select_concepts: String,
    select_edges: String,
    select_canonization_events: String,
    select_reservations: String,
}

impl DialectSql {
    fn for_dialect<D: Dialect>() -> Self {
        let s = D::STRING_CAST;
        let v = D::VECTOR_CAST;
        let op = D::DISTANCE_OP;
        Self {
            vector_cast: v,
            vector_candidates: format!(
                r#"
SELECT id{s} AS id, session_id{s} AS session_id,
       embedding {op} $1{v} AS dist
FROM concepts
WHERE embedding IS NOT NULL
ORDER BY dist ASC
LIMIT $2
"#
            ),
            session_vector_candidates: format!(
                r#"
SELECT id{s} AS id, embedding {op} $1{v} AS dist
FROM concepts
WHERE session_id = $2 AND embedding IS NOT NULL
ORDER BY dist ASC, id ASC
LIMIT $3
"#
            ),
            #[cfg(any(feature = "fixtures", all(test, feature = "store-cockroach")))]
            upsert_session: format!(
                r#"
INSERT INTO sessions (
    session_id, root_goal, created_at, closed_at,
    embedding_kind, embedding_model, embedding_dim
) VALUES ($1, $2::JSONB, COALESCE($3, now()), $4, $5{s}, $6{s}, $7::INT)
ON CONFLICT (session_id) DO UPDATE SET
    root_goal = EXCLUDED.root_goal,
    created_at = EXCLUDED.created_at,
    closed_at = EXCLUDED.closed_at,
    embedding_kind = EXCLUDED.embedding_kind,
    embedding_model = EXCLUDED.embedding_model,
    embedding_dim = EXCLUDED.embedding_dim
"#
            ),
            set_embedding: format!(
                r#"
UPDATE sessions
SET embedding_kind = $2{s},
    embedding_model = $3{s},
    embedding_dim = $4::INT
WHERE session_id = $1
"#
            ),
            select_session: format!(
                r#"
SELECT root_goal{s} AS root_goal, created_at, closed_at,
       embedding_kind, embedding_model, embedding_dim
FROM sessions
WHERE session_id = $1
"#
            ),
            select_interactions: format!(
                r#"
SELECT id{s} AS id, session_id, agent_id, prompt_text,
       previous_id{s} AS previous_id, created_at, event_time
FROM interactions
WHERE session_id = $1
ORDER BY created_at, id
"#
            ),
            select_concepts: format!(
                r#"
SELECT id{s} AS id, session_id, content, canonical_key, concept_type,
       origin_interaction{s} AS origin_interaction, origin_agent, created_at,
       access_count, last_accessed, gc_survived, canonization_status, blast_radius,
       last_demotion_time, embedding{s} AS embedding, chunk_group_id, human_confirmed
FROM concepts
WHERE session_id = $1
ORDER BY id
"#
            ),
            select_edges: format!(
                r#"
SELECT id{s} AS id, session_id, source{s} AS source,
       target{s} AS target, edge_type, weight, reinforcements,
       created_at, last_reinforced, event_time
FROM edges
WHERE session_id = $1
ORDER BY id
"#
            ),
            select_canonization_events: format!(
                r#"
SELECT id{s} AS id, session_id, node_id{s} AS node_id,
       from_status, to_status, blast_radius, last_demotion_time, occurred_at
FROM canonization_events
WHERE session_id = $1
ORDER BY occurred_at, id
"#
            ),
            select_reservations: format!(
                r#"
SELECT session_id, node_id{s} AS node_id, agent_id, expires_at
FROM reservations
WHERE session_id = $1
"#
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse pgvector `format_type` output (`vector(768)`). Rejects Cockroach
/// `VECTOR(n)` so a live probe cannot silently accept the wrong dialect.
pub(crate) fn parse_pgvector_format_type(formatted: &str) -> Option<usize> {
    let rest = formatted.trim().strip_prefix("vector(")?;
    let inner = rest.strip_suffix(')')?;
    inner.trim().parse().ok()
}

fn backend<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// STORE-7 — session-row embedding-contract parsing. A row with exactly one
/// of `embedding_kind` / `embedding_dim` set (as direct SQL can manufacture)
/// is a corruption error, never a silent `None`; `embedding_model` alone is
/// inert (model without a kind has nothing to label). The kind-XOR-dim arms
/// classify as [`StoreError::Invariant`] (E2E-2): the corruption is
/// deterministic, so `tx_retry` must not replay it — both consumers (the
/// checked vector read and the load path) go through this helper.
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
        // E2E-2: a kind-XOR-dim row is DETERMINISTIC corruption — replaying
        // the transaction cannot change the parse, so classifying it as
        // `Backend` made `tx_retry` replay it 5× with backoff (~500 ms)
        // before surfacing (STORE-4: deterministic failures are never
        // replayed). `Invariant` returns on the first attempt. Same class on
        // the load path: both consumers (load_session and the checked read)
        // go through this helper.
        (Some(_), None) => Err(StoreError::Invariant(format!(
            "sessions row for {session_id} has embedding_kind without embedding_dim"
        ))),
        (None, Some(_)) => Err(StoreError::Invariant(format!(
            "sessions row for {session_id} has embedding_dim without embedding_kind"
        ))),
    }
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
/// PostgreSQL does not have this GUC; B2 leaves `hnsw.ef_search` at the
/// pgvector default and does not compile this dial into `store-postgres`.
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
#[cfg(feature = "store-cockroach")]
const DEFAULT_VECTOR_BEAM_SIZE: u32 = 64;
#[cfg(feature = "store-cockroach")]
const VECTOR_BEAM_SIZE_ENV: &str = "LAMBO_VECTOR_BEAM_SIZE";
#[cfg(feature = "store-cockroach")]
const VECTOR_BEAM_SIZE_MIN: u32 = 1;
#[cfg(feature = "store-cockroach")]
const VECTOR_BEAM_SIZE_MAX: u32 = 2048;

/// Parse `LAMBO_VECTOR_BEAM_SIZE`. `Ok(None)` = unset, inherit the server
/// default. Empty is treated as unset so an exported-but-blank var behaves like
/// absence (same convention as `LAMBO_STORE`).
#[cfg(feature = "store-cockroach")]
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
        // B2/B3: the message names Cockroach. PostgreSQL aborts with the same
        // SQLSTATE 40001 under `SERIALIZABLE`, so the mechanism is shared and
        // only the wording is wrong for a second dialect.
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

/// Keep only rows belonging to the caller's session from one global top-k fetch.
/// Input rows arrive in L2-distance-ascending order (SQL `ORDER BY dist ASC`), so
/// the survivors keep that order — the trait's score-descending ordering contract.
/// Pure & deterministic: unit-tested without a cluster.
fn filter_session_rows<D: Dialect>(
    session: &SessionId,
    rows: &[(NodeId, f64, String)],
) -> Vec<Scored<NodeId>> {
    let mut scored: Vec<_> = rows
        .iter()
        .filter(|(_, dist, sid)| sid == &session.0 && dist.is_finite())
        .map(|(id, dist, _)| Scored::new(*id, D::distance_to_score(*dist)))
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

// `encode_vector` / `decode_vector` live in the shared `crate::store::vector` module
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
/// [`PgStore::normalize_tokens`].
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
fn keyword_candidates_sql<D: Dialect>(n_tokens: usize) -> String {
    debug_assert!(n_tokens > 0);
    let mut sql = String::with_capacity(64 + n_tokens * 96);
    let s = D::STRING_CAST;
    sql.push_str(&format!(
        "SELECT id{s} AS id, content, canonical_key FROM concepts WHERE session_id = $1 AND ("
    ));
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
        event_time: row.try_get("event_time").map_err(backend)?,
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
    let human_confirmed: i64 = row.try_get("human_confirmed").map_err(backend)?;
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
        human_confirmed: human_confirmed as i32,
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
        event_time: row.try_get("event_time").map_err(backend)?,
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

/// Durable `GraphStore` over a Postgres-wire-protocol engine (Cockroach or
/// PostgreSQL), selected by [`Dialect`].
///
/// Constructed by [`crate::store::build_store`] from a [`StoreConfig`]. Pool creation is
/// deferred to the first query ([`tokio::sync::OnceCell`]): sqlx pools require a Tokio
/// context at creation (they spawn a maintenance task), and `build_store` is a sync,
/// I/O-free constructor — it must work from `#[test]`s and process start alike. The DSN
/// is still parse-validated at construction (fail fast on typos), and the pool itself is
/// `connect_lazy`, so even the first creation never touches the network.
pub struct PgStore<D: Dialect> {
    /// rustls-rewritten DSN (see `dsn_for_rustls`).
    dsn: String,
    /// Dense-vector column width, from [`Dialect::vector_dim`].
    vector_dim: usize,
    /// The schema this build provisions, from [`Dialect::init_sql`] at
    /// `vector_dim`. Held rather than recomputed because `preflight_schema`
    /// diffs it against the live database on every attach.
    ddl: Cow<'static, str>,
    /// The cast-bearing statements, composed once (see [`DialectSql`]).
    sql: DialectSql,
    pool: tokio::sync::OnceCell<PgPool>,
    /// The Cloud SQL IAM opt-in (`LAMBO_POSTGRES_IAM`) as it stood at construction.
    /// `None` is the ordinary password path.
    #[cfg(feature = "store-postgres")]
    iam_setup: Option<IamSetup>,
    /// Live IAM login state: `None` until the first query on that path. See
    /// [`PgStore::iam_pool`] for why the pool it holds is rotated rather than built once.
    #[cfg(feature = "store-postgres")]
    iam: tokio::sync::Mutex<Option<IamAuth>>,
    /// H3 forced-exact lane. Production construction is always false.
    /// When true, the vector-search transaction runs
    /// [`Dialect::forced_exact_scan_sql`] after the contract read so the
    /// planner cannot use the hnsw index. Shared flag, dialect SQL: the
    /// base does not name PostgreSQL GUCs.
    force_exact_scan: bool,
    /// `D` is a compile-time selector, never a value.
    dialect: PhantomData<D>,
}

impl<D: Dialect> PgStore<D> {
    pub fn new(cfg: StoreConfig) -> Result<Self, StoreError> {
        let dsn = cfg.dsn.as_deref().ok_or_else(|| {
            StoreError::Backend(format!(
                "{} requires a DSN (store.dsn or {})",
                D::STORE_TYPE_NAME,
                D::DSN_ENV,
            ))
        })?;
        // sqlx + rustls cannot open libpq's `sslrootcert=system`; see module doc.
        let dsn = dsn_for_rustls(dsn);
        // Parse-validate without a runtime; the actual pool is built lazily on first use.
        dsn.parse::<sqlx::postgres::PgConnectOptions>()
            .map_err(|e| backend(format!("invalid {}: {e}", D::DSN_LABEL)))?;
        // The width authority, then the DDL it implies: same order the static
        // `schema_vector_dim(INIT_SQL)` parse ran in before the carve.
        let vector_dim = D::vector_dim(&cfg)?;
        let ddl = D::init_sql(vector_dim)?;
        Ok(Self {
            dsn,
            vector_dim,
            ddl,
            sql: DialectSql::for_dialect::<D>(),
            pool: tokio::sync::OnceCell::new(),
            #[cfg(feature = "store-postgres")]
            iam_setup: (D::SUPPORTS_CLOUD_SQL_IAM_AUTH && iam_auth_requested()).then(|| IamSetup {
                credentials: crate::gcp_auth::credentials_path_from_env(),
            }),
            #[cfg(feature = "store-postgres")]
            iam: tokio::sync::Mutex::new(None),
            force_exact_scan: false,
            dialect: PhantomData,
        })
    }

    /// H3 forced-exact lane: the vector-search transaction will run
    /// [`Dialect::forced_exact_scan_sql`] after the contract read.
    /// Production construction leaves this off. Approximation must come
    /// from the index, never from the dialect SQL.
    #[cfg(all(test, feature = "store-postgres"))]
    pub fn with_forced_exact_scan(mut self) -> Self {
        self.force_exact_scan = true;
        self
    }

    /// Whether [`Self::with_forced_exact_scan`] was set. Test/harness use.
    #[cfg(all(test, feature = "store-postgres"))]
    pub(crate) fn forced_exact_scan(&self) -> bool {
        self.force_exact_scan
    }

    /// Test helper: the rustls-rewritten DSN this store will open.
    #[cfg(all(test, feature = "store-postgres"))]
    pub(crate) fn dsn(&self) -> &str {
        &self.dsn
    }

    /// B4: dialects that substitute width into DDL must prove the live
    /// column matches construction dim. Cockroach skips this: its authority
    /// is the static file parsed at construction.
    async fn assert_live_schema_width(&self, pool: &PgPool) -> Result<(), StoreError> {
        let Some(sql) = D::live_schema_vector_width_sql() else {
            return Ok(());
        };
        let formatted: Option<String> = sqlx::query_scalar(sql)
            .fetch_optional(pool)
            .await
            .map_err(backend)?;
        let Some(formatted) = formatted else {
            return Err(StoreError::Backend(format!(
                "{}: concepts.embedding is missing; store is unprovisioned \
                 or not a vector schema",
                D::NAME
            )));
        };
        let live = parse_pgvector_format_type(&formatted).ok_or_else(|| {
            StoreError::Backend(format!(
                "{}: concepts.embedding type {formatted:?} is not vector(n)",
                D::NAME
            ))
        })?;
        if live != self.vector_dim {
            return Err(StoreError::Backend(format!(
                "{}: live schema width is vector({live}) but this process \
                 constructed at dim {}. DDL outranks the pin for reporting; \
                 they must match on an initialized store. Re-init at the \
                 schema width, or migrate.",
                D::NAME,
                self.vector_dim
            )));
        }
        Ok(())
    }

    /// Issue [`Dialect::forced_exact_scan_sql`] on `tx` when the H3
    /// forced-exact flag is set. Shared by production search and the
    /// camera-proof EXPLAIN helper so the GUC is not a lookalike extra_set.
    pub(crate) async fn issue_forced_exact_scan(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), StoreError> {
        if self.force_exact_scan {
            if let Some(sql) = D::forced_exact_scan_sql() {
                sqlx::query(sql).execute(&mut **tx).await.map_err(backend)?;
            }
        }
        Ok(())
    }

    /// The production vector-candidates statement this store issues.
    /// Camera-proofs EXPLAIN this string, not a hand-copied lookalike.
    #[cfg(all(test, feature = "store-postgres"))]
    pub(crate) fn vector_candidates_sql(&self) -> &str {
        &self.sql.vector_candidates
    }

    /// Session-scoped fallback of [`Self::vector_candidates_sql`].
    #[cfg(all(test, feature = "store-postgres"))]
    pub(crate) fn session_vector_candidates_sql(&self) -> &str {
        &self.sql.session_vector_candidates
    }

    /// The lazily-created pool (Tokio context required: call from an async method).
    ///
    /// Returns an owned handle rather than a borrow because the IAM path **replaces** the
    /// pool when its token expires (see [`Self::iam_pool`]); a `&PgPool` into a slot that
    /// can be swapped is not a reference the borrow checker can hand out. `PgPool` is an
    /// `Arc` internally, so the clone costs a refcount and every call site keeps using it
    /// as `&PgPool`.
    pub(crate) async fn pool(&self) -> Result<PgPool, StoreError> {
        #[cfg(feature = "store-postgres")]
        if self.iam_setup.is_some() {
            return self.iam_pool().await;
        }
        let pool = self
            .pool
            .get_or_try_init(|| async {
                let options = Self::connect_options(&self.dsn)?;
                Ok::<_, StoreError>(
                    PgPoolOptions::new()
                        .max_connections(MAX_POOL_CONNECTIONS)
                        .connect_lazy_with(options),
                )
            })
            .await?;
        Ok(pool.clone())
    }

    /// The shared-service-account pool: Cloud SQL IAM database authentication, where the
    /// "password" is an OAuth access token that **expires in about an hour**.
    ///
    /// This is why the pool is rotated rather than created once. Postgres checks the
    /// password at connect time only, so a pool built with an expired token keeps working
    /// on its open connections and fails on the next one it has to open: a lease refresh
    /// two hours into a `serve` fails with an authentication error that looks nothing like
    /// an expiry. Instead the token source reports the instant it stops handing the token
    /// out, and this method builds a new lazy pool at that instant.
    ///
    /// The superseded pool is dropped, not closed: an in-flight query holds its connection
    /// (and through it the inner pool) until it finishes, while nothing new is ever handed
    /// out from it. `connect_lazy_with` means the replacement opens no connection until
    /// someone queries it, so a rotation costs one token mint and nothing else.
    #[cfg(feature = "store-postgres")]
    async fn iam_pool(&self) -> Result<PgPool, StoreError> {
        let mut guard = self.iam.lock().await;
        if guard.is_none() {
            let path = self
                .iam_setup
                .as_ref()
                .and_then(|s| s.credentials.clone())
                .ok_or_else(|| {
                    backend(format!(
                        "{} IAM auth setup: {LAMBO_POSTGRES_IAM_ENV} is set but \
                         GCP_LAMBO_CREDENTIALS / GOOGLE_APPLICATION_CREDENTIALS is unset",
                        D::STORE_TYPE_NAME
                    ))
                })?;
            let creds = crate::gcp_auth::load_credentials(&path)
                .map_err(|e| backend(format!("{} IAM auth setup: {e}", D::STORE_TYPE_NAME)))?;
            let client = crate::gcp_auth::build_client()
                .map_err(|e| backend(format!("{} IAM auth setup: {e}", D::STORE_TYPE_NAME)))?;
            let source = crate::gcp_auth::GoogleOAuthTokenSource::new(
                creds,
                client,
                crate::gcp_auth::SCOPES_CLOUD_SQL_LOGIN,
            )
            .map_err(|e| backend(format!("{} IAM auth setup: {e}", D::STORE_TYPE_NAME)))?;
            *guard = Some(IamAuth { source, live: None });
        }
        let state = guard.as_mut().expect("initialised directly above");
        if let Some((pool, expires_at)) = &state.live {
            if std::time::Instant::now() < *expires_at {
                return Ok(pool.clone());
            }
        }
        let (token, expires_at) = state
            .source
            .access_token_with_expiry()
            .await
            .map_err(|e| backend(format!("{} IAM token: {e}", D::STORE_TYPE_NAME)))?;
        let options = Self::connect_options(&self.dsn)?.password(&token);
        let pool = PgPoolOptions::new()
            .max_connections(MAX_POOL_CONNECTIONS)
            .connect_lazy_with(options);
        state.live = Some((pool.clone(), expires_at));
        Ok(pool)
    }

    /// Build the per-connection options. **Synchronous on purpose:** it reads
    /// the environment, and a test that wants to pin an env-driven option must
    /// be able to do so without holding a lock across an `.await` (spec §6.4,
    /// enforced by `clippy::await_holding_lock`).
    fn connect_options(dsn: &str) -> Result<sqlx::postgres::PgConnectOptions, StoreError> {
        let options = dsn
            .parse::<sqlx::postgres::PgConnectOptions>()
            .map_err(|e| backend(format!("invalid {}: {e}", D::DSN_LABEL)))?
            // STORE-2: bound every statement server-side.
            // statement_timeout applies per statement, not per
            // transaction: a multi-statement flush batch can take
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
        // Dialect-specific session settings (Cockroach: vector_search_beam_size;
        // Postgres: none, pgvector hnsw.ef_search stays at its default).
        D::apply_connect_options(options)
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
        let pool = &self.pool().await?;
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
            sqlx::query(&self.sql.upsert_session)
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
            // Interactions before concepts (`concepts.origin_interaction`
            // REFERENCES interactions(id)); each chunked the same way a flush
            // is, so the seed path exercises the same statements (L82-1).
            for i in &snapshot.interactions {
                bulk_upsert_interactions(&mut *tx, &[i]).await?;
            }
            // Deduplicated first (R1-6): a multi-row statement rejects colliding
            // input rows outright, where the row-at-a-time seed this replaced
            // simply last-wins'd them.
            for chunk in seed_concept_rows(&snapshot.concepts).chunks(BULK_LIMITS.concepts) {
                bulk_upsert_concepts(&mut *tx, chunk, &self.sql).await?;
            }
            for chunk in seed_edge_rows(&snapshot.edges).chunks(BULK_LIMITS.edges) {
                bulk_upsert_edges(&mut *tx, chunk).await?;
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
            // J3 write intents ride the seed for adapter parity (see the
            // SQLite seed).
            for intent in &snapshot.write_intents {
                put_write_intent(&mut *tx, intent).await?;
            }
            tx.commit()
                .await
                .map_err(|e| map_write_err(e, |m| format!("commit seed transaction: {m}")))?;
            Ok(())
        })
        .await
    }

    async fn session_exists(&self, session: &SessionId) -> Result<bool, StoreError> {
        let pool = &self.pool().await?;
        let row = sqlx::query("SELECT 1 AS one FROM sessions WHERE session_id = $1")
            .bind(&session.0)
            .fetch_optional(pool)
            .await
            .map_err(backend)?;
        Ok(row.is_some())
    }

    /// Atomic single-writer lease acquire / refresh (T8.6).
    ///
    /// ONE statement — `INSERT ... ON CONFLICT DO UPDATE ... WHERE ... RETURNING`
    /// — so two processes acquiring on the same session serialize under
    /// Cockroach's concurrency control with no read-then-write race. The update
    /// fires only when the current lease is expired or already ours; a refresh
    /// keeps the original `acquired_at`. Every timestamp comes from the cluster's
    /// `now()` (the clock two processes share) — never a caller argument (F18).
    /// `ttl` is a duration multiplied into an INTERVAL, so no client instant is
    /// ever stored.
    ///
    /// An empty RETURNING means the guard was false — a live lease is held by
    /// someone else — so we read it back and report [`LeaseOutcome::Held`] with
    /// the holder and its age. A row released in the gap is retried a bounded
    /// number of times.
    async fn acquire_or_refresh_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        const ACQUIRE_SQL: &str = "\
            INSERT INTO session_leases \
                (session_id, holder, acquired_at, expires_at, current_token, endpoint) \
            VALUES ($1, $2, now(), now() + ($3 * INTERVAL '1 second'), 1, $4) \
            ON CONFLICT (session_id) DO UPDATE SET \
                holder = excluded.holder, \
                acquired_at = CASE WHEN session_leases.holder = excluded.holder \
                                   THEN session_leases.acquired_at ELSE excluded.acquired_at END, \
                expires_at = excluded.expires_at, \
                current_token = CASE WHEN session_leases.holder = excluded.holder \
                                     THEN session_leases.current_token \
                                     ELSE session_leases.current_token + 1 END, \
                endpoint = excluded.endpoint \
            WHERE session_leases.expires_at <= now() \
               OR session_leases.holder = excluded.holder \
            RETURNING holder, acquired_at, expires_at, current_token, endpoint";
        let pool = &self.pool().await?;
        let token = holder.token();
        let ttl_secs = ttl.as_secs_f64();
        // T86-3: wrap the acquire in `tx_retry`, exactly like every other
        // contended write in this file. sqlx does not auto-retry a SQLSTATE 40001
        // `RETRY_SERIALIZABLE` abort, which `map_write_err` maps to a retryable
        // `StoreError::Backend`; without this wrapper a genuine cross-node acquire
        // conflict surfaced as an opaque `Backend` error (→ `LamboError::Store`)
        // instead of transparently replaying — diverging from the SQLite backend
        // (which absorbs contention via `busy_timeout`) and from the rest of
        // `cockroach.rs`. The inner `for 0..3` still handles the orthogonal
        // vanished-row case (empty RETURNING then empty read-back).
        let session_id = &session.0;
        let token_ref = token.as_str();
        let endpoint_ref = holder.endpoint.as_deref();
        tx_retry(|| async move {
            for _ in 0..3 {
                let won: Option<LeaseRowTs> = sqlx::query_as(ACQUIRE_SQL)
                    .bind(session_id)
                    .bind(token_ref)
                    .bind(ttl_secs)
                    .bind(endpoint_ref)
                    .fetch_optional(pool)
                    .await
                    .map_err(|e| map_write_err(e, |m| format!("acquire lease: {m}")))?;
                if let Some(row) = won {
                    return Ok(LeaseOutcome::Acquired(lease_info_from_ts(row)?));
                }
                let current: Option<LeaseRowTs> = sqlx::query_as(LEASE_ROW_SQL)
                    .bind(session_id)
                    .fetch_optional(pool)
                    .await
                    .map_err(backend)?;
                match current {
                    Some(row) => {
                        let current = lease_info_from_ts(row)?;
                        let age = (Utc::now() - current.acquired_at)
                            .to_std()
                            .unwrap_or(Duration::ZERO);
                        return Ok(LeaseOutcome::Held { current, age });
                    }
                    None => continue,
                }
            }
            Err(StoreError::Backend(
                "acquire lease: contended row kept changing under us (retries exhausted)".into(),
            ))
        })
        .await
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

/// Multi-row upsert of one planned [`FlushStep::Interactions`] chunk (L82-1).
///
/// `rows` is already deduplicated on `id` and capped at
/// [`BULK_LIMITS`]`.interactions`.
async fn bulk_upsert_interactions(
    tx: &mut sqlx::PgConnection,
    rows: &[&Interaction],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    interaction_upsert_query(rows)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert interaction: {m}")))?;
    Ok(())
}

/// The statement [`bulk_upsert_interactions`] runs, built but not executed.
///
/// Separate from the execution so the generated SQL — the one part of this
/// change no local test can reach through a cluster — is inspectable by
/// `sql_shape_is_a_multi_row_upsert`.
fn interaction_upsert_query<'a>(
    rows: &'a [&'a Interaction],
) -> sqlx::QueryBuilder<'a, sqlx::Postgres> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(INSERT_INTERACTION_PREFIX_SQL);
    qb.push_values(rows.iter(), |mut b, i| {
        b.push_bind(i.id.0)
            .push_bind(i.session_id.0.as_str())
            .push_bind(i.agent_id.0.as_str())
            .push_bind(i.prompt_text.as_deref())
            .push_bind(i.previous_id.map(|n| n.0))
            .push_bind(i.created_at)
            .push_bind(i.event_time);
    });
    qb.push(ON_CONFLICT_INTERACTION_SQL);
    qb
}

/// Multi-row upsert of one planned [`FlushStep::Concepts`] chunk (L82-1).
///
/// Each row's three canonization columns come from its
/// [`ConceptRow::canonization`], **not** from `row.concept` — see [`ConceptRow`]
/// for why a deduplicated row splits them.
///
/// Vectors are encoded up front because `push_values`' closure cannot fail.
async fn bulk_upsert_concepts(
    tx: &mut sqlx::PgConnection,
    rows: &[ConceptRow<'_>],
    sql: &DialectSql,
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut embeddings: Vec<Option<String>> = Vec::with_capacity(rows.len());
    for r in rows {
        embeddings.push(match &r.concept.embedding {
            Some(v) => Some(encode_vector(v)?),
            None => None,
        });
    }

    concept_upsert_query(rows, &embeddings, sql.vector_cast)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert concept: {m}")))?;
    Ok(())
}

/// The statement [`bulk_upsert_concepts`] runs, built but not executed.
fn concept_upsert_query<'a>(
    rows: &'a [ConceptRow<'a>],
    embeddings: &'a [Option<String>],
    vector_cast: &'static str,
) -> sqlx::QueryBuilder<'a, sqlx::Postgres> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(INSERT_CONCEPT_PREFIX_SQL);
    qb.push_values(
        rows.iter().zip(embeddings.iter()),
        |mut b, (r, embedding)| {
            let c = r.concept;
            b.push_bind(c.id.0)
                .push_bind(c.session_id.0.as_str())
                .push_bind(c.content.as_str())
                .push_bind(c.canonical_key.as_str())
                .push_bind(concept_type_sql(c.concept_type))
                .push_bind(c.origin_interaction.0)
                .push_bind(c.origin_agent.0.as_str())
                .push_bind(c.created_at)
                .push_bind(c.access_count)
                .push_bind(c.last_accessed)
                .push_bind(c.gc_survived)
                .push_bind(canonization_status_sql(r.canonization.status))
                .push_bind(r.canonization.blast_radius)
                .push_bind(r.canonization.last_demotion_time)
                .push_bind(embedding.as_deref())
                // The dense-vector column takes a text literal, so the cast is
                // part of the value expression — it must ride with this
                // placeholder, not with the separator.
                .push_unseparated(vector_cast)
                .push_bind(c.chunk_group_id.as_deref())
                .push_bind(c.human_confirmed);
        },
    );
    qb.push(ON_CONFLICT_CONCEPT_SQL);
    qb
}

/// Multi-row upsert of one planned [`FlushStep::Edges`] chunk (L82-1).
///
/// `rows` is already deduplicated on the **natural** key
/// `(source, target, edge_type)` — the conflict target below — because two rows
/// colliding there in one statement is an error, not a last-write-wins.
async fn bulk_upsert_edges(tx: &mut sqlx::PgConnection, rows: &[&Edge]) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    edge_upsert_query(rows)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert edge: {m}")))?;
    Ok(())
}

/// The statement [`bulk_upsert_edges`] runs, built but not executed.
fn edge_upsert_query<'a>(rows: &'a [&'a Edge]) -> sqlx::QueryBuilder<'a, sqlx::Postgres> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(INSERT_EDGE_PREFIX_SQL);
    qb.push_values(rows.iter(), |mut b, e| {
        b.push_bind(e.id.0)
            .push_bind(e.session_id.0.as_str())
            .push_bind(e.source.0)
            .push_bind(e.target.0)
            .push_bind(edge_type_sql(e.edge_type))
            .push_bind(e.weight)
            .push_bind(e.reinforcements)
            .push_bind(e.created_at)
            .push_bind(e.last_reinforced)
            .push_bind(e.event_time);
    });
    qb.push(ON_CONFLICT_EDGE_SQL);
    qb
}
/// Multi-row upsert of one planned [`FlushStep::PutIntents`] chunk (J3/F4).
///
/// `intents` is already deduplicated on `(session_id, receipt)` and capped at
/// [`crate::store::batch::INTENT_BATCH`].
async fn bulk_put_write_intents(
    tx: &mut sqlx::PgConnection,
    intents: &[&crate::types::WriteIntent],
) -> Result<(), StoreError> {
    if intents.is_empty() {
        return Ok(());
    }
    let payloads: Vec<String> = intents
        .iter()
        .map(|i| {
            serde_json::to_string(&i.payload)
                .map_err(|e| backend(format!("serialize write intent payload: {e}")))
        })
        .collect::<Result<_, _>>()?;
    put_write_intents_upsert(intents, &payloads)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("put write intent: {m}")))?;
    Ok(())
}

/// The statement [`bulk_put_write_intents`] runs, built but not executed.
fn put_write_intents_upsert<'a>(
    intents: &'a [&'a crate::types::WriteIntent],
    payloads: &'a [String],
) -> sqlx::QueryBuilder<'a, sqlx::Postgres> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(INSERT_WRITE_INTENT_PREFIX_SQL);
    qb.push_values(intents.iter().zip(payloads), |mut b, (i, payload)| {
        b.push_bind(i.session_id.0.as_str())
            .push_bind(&i.receipt)
            .push_bind(i.agent.0.as_str())
            .push_bind(i.interaction.0)
            .push_bind(i64::try_from(i.lane_seq).unwrap_or(i64::MAX))
            .push_bind(i.issued_ms)
            .push_bind(payload)
            .push_bind(i.created_at)
            .push_bind(i.outcome.as_ref().map(|o| o.consumed_at))
            .push_bind(i.outcome.as_ref().map(|o| o.tag.clone()))
            .push_bind(i.outcome.as_ref().map(|o| o.summary.clone()));
    });
    qb.push(ON_CONFLICT_WRITE_INTENT_SQL);
    qb
}

/// Multi-row `write_intents` consume of one planned [`FlushStep::ConsumeIntents`]
/// chunk (J3/F4): one UPDATE marking each receipt consumed, then the lazy
/// retention purge — one DELETE per distinct session in the chunk, clocked by
/// the chunk's oldest `consumed_at` minus the retention window.
async fn bulk_consume_write_intents(
    tx: &mut sqlx::PgConnection,
    consumes: &[(
        &crate::types::SessionId,
        &str,
        &crate::types::WriteIntentOutcome,
    )],
) -> Result<(), StoreError> {
    if consumes.is_empty() {
        return Ok(());
    }
    consume_write_intents_update(consumes)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("consume write intent: {m}")))?;

    // Lazy retention purge (J3-R2R-5), clocked by the chunk's oldest
    // `consumed_at` minus the retention window, one DELETE per distinct session.
    let cutoff = consumes
        .iter()
        .map(|c| c.2.consumed_at)
        .min()
        .expect("consume batch is non-empty")
        - chrono::Duration::from_std(crate::types::WRITE_INTENT_RETENTION)
            .map_err(|e| backend(format!("retention duration out of range: {e}")))?;
    let sessions: std::collections::HashSet<&crate::types::SessionId> =
        consumes.iter().map(|c| c.0).collect();
    for session in sessions {
        sqlx::query(
            "DELETE FROM write_intents \
             WHERE session_id = $1 AND consumed_at IS NOT NULL AND consumed_at < $2",
        )
        .bind(session.0.as_str())
        .bind(cutoff)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("purge consumed write intents: {m}")))?;
    }
    Ok(())
}

const INSERT_WRITE_INTENT_PREFIX_SQL: &str = r#"
INSERT INTO write_intents (
    session_id, receipt, agent, interaction_id, lane_seq, issued_ms, payload,
    created_at, consumed_at, outcome_tag, outcome_summary
) "#;

const ON_CONFLICT_WRITE_INTENT_SQL: &str = r#"
ON CONFLICT (session_id, receipt) DO UPDATE SET
    agent = EXCLUDED.agent,
    interaction_id = EXCLUDED.interaction_id,
    lane_seq = EXCLUDED.lane_seq,
    issued_ms = EXCLUDED.issued_ms,
    payload = EXCLUDED.payload,
    created_at = EXCLUDED.created_at,
    consumed_at = EXCLUDED.consumed_at,
    outcome_tag = EXCLUDED.outcome_tag,
    outcome_summary = EXCLUDED.outcome_summary
"#;

// `sqlx::QueryBuilder::push_values` emits the `VALUES` keyword itself, so the
// prefix ends after `FROM (` and the suffix begins after the closing paren.
const CONSUME_WRITE_INTENT_UPDATE_PREFIX_SQL: &str = r#"
UPDATE write_intents SET
    consumed_at = v.consumed_at, outcome_tag = v.outcome_tag, outcome_summary = v.outcome_summary
    FROM ("#;

const CONSUME_WRITE_INTENT_UPDATE_SUFFIX_SQL: &str = r#") AS v(
    session_id, receipt, consumed_at, outcome_tag, outcome_summary
) WHERE write_intents.session_id = v.session_id AND write_intents.receipt = v.receipt"#;

/// The statement [`bulk_consume_write_intents`] runs, built but not executed.
fn consume_write_intents_update<'a>(
    consumes: &'a [(
        &'a crate::types::SessionId,
        &'a str,
        &'a crate::types::WriteIntentOutcome,
    )],
) -> sqlx::QueryBuilder<'a, sqlx::Postgres> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(CONSUME_WRITE_INTENT_UPDATE_PREFIX_SQL);
    qb.push_values(consumes.iter(), |mut b, (sid, receipt, outcome)| {
        b.push_bind(sid.0.as_str())
            .push_bind(receipt)
            .push_bind(outcome.consumed_at)
            .push_bind(&outcome.tag)
            .push_bind(&outcome.summary);
    });
    qb.push(CONSUME_WRITE_INTENT_UPDATE_SUFFIX_SQL);
    qb
}

/// Apply one planned [`FlushStep`].
async fn apply_step(
    tx: &mut sqlx::PgConnection,
    step: &FlushStep<'_>,
    sql: &DialectSql,
) -> Result<(), StoreError> {
    match step {
        FlushStep::Interactions(rows) => bulk_upsert_interactions(&mut *tx, rows).await,
        FlushStep::Concepts(rows) => bulk_upsert_concepts(&mut *tx, rows, sql).await,
        FlushStep::Edges(rows) => bulk_upsert_edges(&mut *tx, rows).await,
        FlushStep::Single(m) => apply_single(&mut *tx, m, sql).await,
        FlushStep::PutIntents(intents) => bulk_put_write_intents(&mut *tx, intents).await,
        FlushStep::ConsumeIntents(consumes) => bulk_consume_write_intents(&mut *tx, consumes).await,
    }
}

/// Apply one mutation the planner could not bulk — a deletion, a canonization
/// transition, or a session-column write.
///
/// Every one of these can *observe* a row an upsert may have written, which is
/// exactly why [`plan_flush`] emits them alone and in place (see
/// `store::batch`). The upsert arms are unreachable for the same reason, but
/// they are handled rather than `unreachable!()`d: a planner change must not be
/// able to turn into a panic inside a flush.
async fn apply_single(
    tx: &mut sqlx::PgConnection,
    m: &Mutation,
    sql: &DialectSql,
) -> Result<(), StoreError> {
    match m {
        Mutation::UpsertNode {
            node: Node::Interaction(i),
        } => bulk_upsert_interactions(&mut *tx, &[i]).await?,
        Mutation::UpsertNode {
            node: Node::Concept(c),
        } => bulk_upsert_concepts(&mut *tx, &[ConceptRow::new(c)], sql).await?,
        Mutation::UpsertEdge { edge } => bulk_upsert_edges(&mut *tx, &[edge]).await?,
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
                .map_err(|e| map_write_err(e, |m| format!("delete node concepts: {m}")))?;
            sqlx::query(DELETE_NODE_INTERACTIONS_SQL)
                .bind(id.0)
                .execute(&mut *tx)
                .await
                .map_err(|e| map_write_err(e, |m| format!("delete node interactions: {m}")))?;
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
                        map_write_err(e, |m| format!("quarantine legacy embeddings: {m}"))
                    })?;
            }
            let res = sqlx::query(&sql.set_embedding)
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
        Mutation::PutWriteIntent { intent } => {
            put_write_intent(&mut *tx, intent).await?;
        }
        Mutation::ConsumeWriteIntent {
            session_id,
            receipt,
            outcome,
        } => {
            consume_write_intent(&mut *tx, session_id, receipt, outcome).await?;
        }
    }
    Ok(())
}

/// Upsert one durable write intent (J3). Keyed by (session, receipt); a re-put
/// replaces the row, matching the SQLite and memory adapters.
async fn put_write_intent(
    tx: &mut sqlx::PgConnection,
    intent: &crate::types::WriteIntent,
) -> Result<(), StoreError> {
    let payload = serde_json::to_string(&intent.payload)
        .map_err(|e| backend(format!("serialize write intent payload: {e}")))?;
    sqlx::query(
        "INSERT INTO write_intents \
             (session_id, receipt, agent, interaction_id, lane_seq, issued_ms, payload, \
              created_at, consumed_at, outcome_tag, outcome_summary) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (session_id, receipt) DO UPDATE SET \
             agent = excluded.agent, \
             interaction_id = excluded.interaction_id, \
             lane_seq = excluded.lane_seq, \
             issued_ms = excluded.issued_ms, \
             payload = excluded.payload, \
             created_at = excluded.created_at, \
             consumed_at = excluded.consumed_at, \
             outcome_tag = excluded.outcome_tag, \
             outcome_summary = excluded.outcome_summary",
    )
    .bind(intent.session_id.as_str())
    .bind(&intent.receipt)
    .bind(intent.agent.0.as_str())
    .bind(intent.interaction.0)
    .bind(i64::try_from(intent.lane_seq).unwrap_or(i64::MAX))
    .bind(intent.issued_ms)
    .bind(payload)
    .bind(intent.created_at)
    .bind(intent.outcome.as_ref().map(|o| o.consumed_at))
    .bind(intent.outcome.as_ref().map(|o| o.tag.clone()))
    .bind(intent.outcome.as_ref().map(|o| o.summary.clone()))
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("put write intent: {m}")))?;
    Ok(())
}

/// Mark one intent consumed with its outcome, then purge consumed rows older
/// than [`crate::types::WRITE_INTENT_RETENTION`] — clocked by the mutation's
/// own `consumed_at`. Consuming an absent receipt is a no-op (idempotent
/// replay, same as the canonization dedupe).
async fn consume_write_intent(
    tx: &mut sqlx::PgConnection,
    session_id: &SessionId,
    receipt: &str,
    outcome: &crate::types::WriteIntentOutcome,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE write_intents SET consumed_at = $1, outcome_tag = $2, outcome_summary = $3 \
         WHERE session_id = $4 AND receipt = $5",
    )
    .bind(outcome.consumed_at)
    .bind(&outcome.tag)
    .bind(&outcome.summary)
    .bind(session_id.as_str())
    .bind(receipt)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("consume write intent: {m}")))?;
    let cutoff = outcome.consumed_at
        - chrono::Duration::from_std(crate::types::WRITE_INTENT_RETENTION)
            .map_err(|e| backend(format!("retention duration out of range: {e}")))?;
    sqlx::query(
        "DELETE FROM write_intents \
         WHERE session_id = $1 AND consumed_at IS NOT NULL AND consumed_at < $2",
    )
    .bind(session_id.as_str())
    .bind(cutoff)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("purge consumed write intents: {m}")))?;
    Ok(())
}

/// Load a session's write intents (J3), in replay order — (`issued_ms`,
/// `lane_seq`), exact admission order within one issuing process and
/// wall-clock order across processes.
async fn load_write_intents(
    tx: &mut sqlx::PgConnection,
    session: &SessionId,
) -> Result<Vec<crate::types::WriteIntent>, StoreError> {
    let rows = sqlx::query(
        "SELECT receipt, agent, interaction_id, lane_seq, issued_ms, payload, created_at, \
                consumed_at, outcome_tag, outcome_summary \
         FROM write_intents WHERE session_id = $1 ORDER BY issued_ms ASC, lane_seq ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| backend(format!("load write intents: {e}")))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let receipt: String = row
            .try_get(0)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let agent: String = row
            .try_get(1)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let interaction: uuid::Uuid = row
            .try_get(2)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let lane_seq: i64 = row
            .try_get(3)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let issued_ms: i64 = row
            .try_get(4)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let payload: String = row
            .try_get(5)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let created_at: DateTime<Utc> = row
            .try_get(6)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let consumed_at: Option<DateTime<Utc>> = row
            .try_get(7)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let outcome_tag: Option<String> = row
            .try_get(8)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let outcome_summary: Option<String> = row
            .try_get(9)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let payload: crate::types::WriteIntentPayload = serde_json::from_str(&payload)
            .map_err(|e| backend(format!("parse write intent payload: {e}")))?;
        let outcome = match (consumed_at, outcome_tag, outcome_summary) {
            (Some(consumed_at), Some(tag), Some(summary)) => {
                Some(crate::types::WriteIntentOutcome {
                    tag,
                    summary,
                    consumed_at,
                })
            }
            (None, None, None) => None,
            _ => {
                return Err(StoreError::Invariant(format!(
                    "write intent {receipt}: consumed_at/outcome columns are partially set"
                )))
            }
        };
        out.push(crate::types::WriteIntent {
            session_id: session.clone(),
            receipt,
            agent: crate::types::AgentId::new(&agent),
            interaction: NodeId(interaction),
            lane_seq: u64::try_from(lane_seq).unwrap_or(u64::MAX),
            issued_ms,
            payload,
            created_at,
            outcome,
        });
    }
    Ok(out)
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

/// The lease row as Cockroach hands it back — the column order
/// [`LEASE_ROW_SQL`] and the acquire's `RETURNING` both use. Named so the two
/// duplicated column lists cannot drift in shape (J2 added a sixth column).
type LeaseRowTs = (String, DateTime<Utc>, DateTime<Utc>, i64, Option<String>);

/// Every column [`LeaseInfo`] needs, in [`LeaseRowTs`] order.
const LEASE_ROW_SQL: &str = "\
    SELECT holder, acquired_at, expires_at, current_token, endpoint \
    FROM session_leases WHERE session_id = $1";

fn lease_info_from_ts(row: LeaseRowTs) -> Result<LeaseInfo, StoreError> {
    let (holder, acquired_at, expires_at, current_token, endpoint) = row;
    Ok(LeaseInfo {
        holder,
        token: u64::try_from(current_token)
            .map_err(|_| StoreError::Backend("lease row has a negative current_token".into()))?,
        acquired_at,
        expires_at,
        endpoint,
    })
}

#[async_trait]
impl<D: Dialect> GraphStore for PgStore<D> {
    async fn init_schema(&self) -> Result<(), StoreError> {
        // Multi-statement DDL via the simple protocol (raw_sql); every statement is
        // `IF NOT EXISTS`, so this is idempotent by construction (T3.1 acceptance).
        let pool = &self.pool().await?;
        sqlx::raw_sql(self.ddl.as_ref())
            .execute(pool)
            .await
            .map_err(backend)?;

        // Post-DDL convergence ALTERs. Not folded into `init_sql`: that would
        // turn this from raw_sql + N query() calls into one raw_sql, which is
        // a Cockroach behaviour change B0 forbade and B2 does not make. The
        // statements themselves are not byte-identical (STRING vs TEXT, INT
        // vs BIGINT), so they live on the dialect.
        for stmt in D::post_init_statements() {
            sqlx::query(stmt).execute(pool).await.map_err(backend)?;
        }
        self.assert_live_schema_width(pool).await?;
        Ok(())
    }

    /// J3 F5 + J3-R2R-3. An `information_schema.tables` read in the
    /// connection's current schema, diffed against the table names in the DDL
    /// this build ships, then an `information_schema.columns` read per required
    /// table, diffed against the column set the same DDL declares. Cockroach is
    /// provisioned by `scripts/provision.sh`, not by `init_schema` on the attach
    /// path, so the same upgrade-without-reprovision hazard applies — and here
    /// every failed statement is also a round trip. The column half is the
    /// Cockroach-dialect side of J3-R2R-3 (source-correct here; live-cluster
    /// verification is the named follow-up the brief records).
    async fn preflight_schema(&self) -> Result<(), StoreError> {
        let pool = &self.pool().await?;
        let present: Vec<String> = sqlx::query_scalar(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = current_schema()",
        )
        .fetch_all(pool)
        .await
        .map_err(backend)?;
        let required = tables_in_ddl(self.ddl.as_ref());
        let missing_tables: Vec<&str> = required
            .into_iter()
            .filter(|t| !present.iter().any(|p| p == t))
            .collect();
        if !missing_tables.is_empty() {
            return Err(unprovisioned_store_err(D::NAME, &missing_tables));
        }
        let mut by_table: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for (table, col) in columns_in_ddl(self.ddl.as_ref()) {
            by_table.entry(table).or_default().push(col);
        }
        for (table, cols) in by_table {
            let present_cols: Vec<String> = sqlx::query_scalar(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_schema = current_schema() AND table_name = $1",
            )
            .bind(table)
            .fetch_all(pool)
            .await
            .map_err(backend)?;
            let missing: Vec<&str> = cols
                .iter()
                .copied()
                .filter(|c| !present_cols.iter().any(|p| p == c))
                .collect();
            if !missing.is_empty() {
                return Err(unprovisioned_column_err(D::NAME, table, &missing));
            }
        }
        self.assert_live_schema_width(pool).await?;
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::VECTOR_SEARCH
    }

    fn vector_dimensions(&self) -> Option<usize> {
        // Construction width. For Postgres, [`Self::assert_live_schema_width`]
        // has checked this against live `vector(n)` on init and attach, so
        // the number is the schema's, not an unchecked echo of config.
        Some(self.vector_dim)
    }

    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.acquire_or_refresh_lease(session, holder, ttl).await
    }

    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.acquire_or_refresh_lease(session, holder, ttl).await
    }

    async fn read_lease(&self, session: &SessionId) -> Result<Option<LeaseInfo>, StoreError> {
        let pool = &self.pool().await?;
        let row: Option<LeaseRowTs> = sqlx::query_as(LEASE_ROW_SQL)
            .bind(&session.0)
            .fetch_optional(pool)
            .await
            .map_err(backend)?;
        row.map(lease_info_from_ts).transpose()
    }

    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        let pool = &self.pool().await?;
        // Holder-scoped so a stale release cannot evict the writer that took
        // over after our lease lapsed.
        sqlx::query("DELETE FROM session_leases WHERE session_id = $1 AND holder = $2")
            .bind(&session.0)
            .bind(holder.token())
            .execute(pool)
            .await
            .map_err(|e| map_write_err(e, |m| format!("release lease: {m}")))?;
        Ok(())
    }
    // J4. Record a lease refusal against this session at the store's clock,
    // then purge this session's refusals older than
    // `LEASE_REFUSAL_RETENTION` (JE2E-1).
    //
    // The purge is **lazy, on the write path**, mirroring `write_intents`'
    // `consume_write_intent`: it rides the one statement that was going to
    // touch this table anyway, so no adapter grows a clock and no task grows a
    // timer. Two statements here rather than one, and each is a round trip on
    // Cockroach — the same cost shape F4 records for the intent consume, and
    // acceptable for the same reason: this path runs once per *refused start*,
    // not once per write.
    async fn record_lease_refusal(
        &self,
        session: &SessionId,
        refused_by: &str,
        current_holder: &str,
    ) -> Result<(), StoreError> {
        let pool = &self.pool().await?;
        sqlx::query(
            "INSERT INTO lease_refusals (session_id, refused_at, refused_by, current_holder) \
             VALUES ($1, now(), $2, $3)",
        )
        .bind(&session.0)
        .bind(refused_by)
        .bind(current_holder)
        .execute(pool)
        .await
        .map_err(|e| map_write_err(e, |m| format!("record lease refusal: {m}")))?;
        sqlx::query(
            "DELETE FROM lease_refusals \
             WHERE session_id = $1 AND refused_at < now() - $2::INTERVAL",
        )
        .bind(&session.0)
        .bind(format!(
            "{} seconds",
            crate::store::lease::LEASE_REFUSAL_RETENTION.as_secs()
        ))
        .execute(pool)
        .await
        .map_err(|e| map_write_err(e, |m| format!("purge expired lease refusals: {m}")))?;
        Ok(())
    }

    // J4. Refusals recorded against this session at/after `since`, newest first.
    async fn pending_lease_refusals(
        &self,
        session: &SessionId,
        since: DateTime<Utc>,
    ) -> Result<Vec<crate::store::lease::LeaseRefusal>, StoreError> {
        let pool = &self.pool().await?;
        type Row = (DateTime<Utc>, String, String);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT refused_at, refused_by, current_holder FROM lease_refusals \
             WHERE session_id = $1 AND refused_at >= $2 ORDER BY refused_at DESC",
        )
        .bind(&session.0)
        .bind(since)
        .fetch_all(pool)
        .await
        .map_err(backend)?;
        Ok(rows
            .into_iter()
            .map(
                |(at, refused_by, current_holder)| crate::store::lease::LeaseRefusal {
                    session: session.clone(),
                    at,
                    refused_by,
                    current_holder,
                },
            )
            .collect())
    }
    async fn write_flush_stats(
        &self,
        session: &SessionId,
        stats: &SessionFlushStats,
    ) -> Result<(), StoreError> {
        let pool = &self.pool().await?;
        // Upsert the whole row so re-publishes converge (idempotency, same
        // contract as `flush`). Only the writer's FlushTask calls this;
        // readers only read. `updated_at` is stamped from the cluster clock
        // (now()).
        sqlx::query(
            "INSERT INTO session_stats (session_id, flush_lag_ms, log_depth, updated_at) \
             VALUES ($1, $2, $3, now()) \
             ON CONFLICT (session_id) DO UPDATE SET \
               flush_lag_ms = excluded.flush_lag_ms, \
               log_depth = excluded.log_depth, \
               updated_at = excluded.updated_at",
        )
        .bind(&session.0)
        .bind(stats.flush_lag_ms as i64)
        .bind(stats.log_depth as i64)
        .execute(pool)
        .await
        .map_err(|e| map_write_err(e, |m| format!("write flush stats: {m}")))?;
        Ok(())
    }

    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionFlushStats>, StoreError> {
        let pool = &self.pool().await?;
        let row =
            sqlx::query("SELECT flush_lag_ms, log_depth FROM session_stats WHERE session_id = $1")
                .bind(&session.0)
                .fetch_optional(pool)
                .await
                .map_err(backend)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let flush_lag_ms: i64 = row
            .try_get("flush_lag_ms")
            .map_err(|e| backend(format!("read flush stats: flush_lag_ms: {e}")))?;
        let log_depth: i64 = row
            .try_get("log_depth")
            .map_err(|e| backend(format!("read flush stats: log_depth: {e}")))?;
        Ok(Some(SessionFlushStats {
            flush_lag_ms: u64::try_from(flush_lag_ms).unwrap_or(u64::MAX),
            log_depth: u64::try_from(log_depth).unwrap_or(u64::MAX),
        }))
    }

    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        let pool = &self.pool().await?;
        tx_retry(|| async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| map_write_err(e, |m| format!("begin flush transaction: {m}")))?;
            // Ensure a sessions row for every session the batch writes into — the DDL
            // enforces `REFERENCES sessions(session_id)` on interactions/concepts, and the
            // graph tier creates sessions implicitly (MemoryStore::ensure_session parity).
            for sid in batch_session_ids(&batch.mutations) {
                sqlx::query(UPSERT_SESSION_ROW_SQL)
                    .bind(sid)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| map_write_err(e, |m| format!("upsert session row: {m}")))?;
            }

            // Fencing-token gate (#1): reject a stale/missing token for every
            // session the batch touches, INSIDE the same transaction as the
            // writes (atomic with them; a takeover cannot slip between the
            // check and the commit — on rejection `?` drops `tx`, rolling back).
            // An unleased session (no row / current_token 0) passes — seed /
            // fixture parity.
            for sid in batch_session_ids(&batch.mutations) {
                let current: Option<i64> = sqlx::query_scalar(
                    "SELECT current_token FROM session_leases WHERE session_id = $1",
                )
                .bind(sid)
                .fetch_optional(&mut *tx)
                .await
                .map_err(backend)?;
                if let Some(cur) = current {
                    let cur = u64::try_from(cur).map_err(|_| {
                        StoreError::Invariant(format!("session {sid}: negative lease current_token"))
                    })?;
                    if !lease_permits_write(cur, token) {
                        return Err(StoreError::StaleWrite(format!(
                            "session {sid}: presented token {token:?} is stale (lease token {cur}) — \
                             single-writer fence (GitHub issue #1)"
                        )));
                    }
                }
            }


            // Replay the batch as planned statements rather than one statement
            // per mutation (L82-1). Order is still the batch's own — see
            // `store::batch` for why bucketing upserts by table preserves it,
            // and why every mutation that could *observe* a row is a barrier.
            //
            // This is the fix for the live finding: against a serverless
            // cluster the old loop cost one network round-trip per mutation, so
            // a 784-mutation shutdown tail could not drain inside `close()`'s
            // 10 s grace window and was discarded.
            for step in plan_flush(&batch.mutations, BULK_LIMITS) {
                apply_step(&mut *tx, &step, &self.sql).await?;
            }
            tx.commit()
                .await
                .map_err(|e| map_write_err(e, |m| format!("commit flush transaction: {m}")))?;
            Ok(())
        })
        .await
    }

    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        let pool = &self.pool().await?;
        let session_id = session.clone();
        // Copy handle (&SessionId): the FnMut body runs once per retry attempt.
        let sid = &session_id;
        tx_retry(|| async move {
            let mut tx = pool.begin().await.map_err(backend)?;
            let session_row = sqlx::query(&self.sql.select_session)
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

            let interactions = sqlx::query(&self.sql.select_interactions)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_interaction)
                .collect::<Result<Vec<_>, _>>()?;

            let concepts = sqlx::query(&self.sql.select_concepts)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_concept)
                .collect::<Result<Vec<_>, _>>()?;

            let edges = sqlx::query(&self.sql.select_edges)
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

            let reservations = sqlx::query(&self.sql.select_reservations)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_reservation)
                .collect::<Result<Vec<_>, _>>()?;

            let canonization_events = sqlx::query(&self.sql.select_canonization_events)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_canonization_event)
                .collect::<Result<Vec<_>, _>>()?;

            let write_intents = load_write_intents(&mut tx, sid).await?;

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
                write_intents,
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
        let pool = &self.pool().await?;
        let sql = keyword_candidates_sql::<D>(tokens.len());
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
        // Frozen v0.2.0 compatibility surface. It cannot attest which contract
        // produced `embedding`, so production code never calls it. Reusing the
        // checked transaction with the currently stored contract preserves the
        // legacy result shape while still preventing a contract/vector snapshot
        // race inside this adapter.
        validate_vector_candidate_limit(limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        check_embedding_dim(embedding, self.vector_dim)?;
        let stored = match self.load_session(session).await {
            Ok(snapshot) => snapshot.embedding,
            Err(StoreError::SessionNotFound(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let Some(stored) = stored else {
            return Ok(Vec::new());
        };
        self.vector_candidates_checked(session, embedding, &stored, limit)
            .await
    }

    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        validate_vector_candidate_limit(limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        check_embedding_dim(embedding, self.vector_dim)?;
        let pool = &self.pool().await?;
        let probe = encode_vector(embedding)?;
        // One retried serializable read transaction binds contract validation
        // to every candidate statement. Cockroach may abort this hot read with
        // SQLSTATE 40001 while a writer replaces the contract and vectors, so
        // every retry must replay the contract read, global growth loop, exact
        // fallback, and commit as one unit.
        tx_retry(|| async {
            let mut tx = pool.begin().await.map_err(backend)?;
            let Some(contract_row) = sqlx::query(&self.sql.select_session)
                .bind(session.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(backend)?
            else {
                return Ok(Vec::new());
            };
            let stored = session_embedding_from_parts(
                contract_row.try_get("embedding_kind").map_err(backend)?,
                contract_row.try_get("embedding_model").map_err(backend)?,
                contract_row.try_get("embedding_dim").map_err(backend)?,
                session.as_str(),
            )?;
            let Some(stored) = stored else {
                return Ok(Vec::new());
            };
            stored.ensure_compatible(expected_contract).map_err(|err| {
                StoreError::Invariant(format!(
                    "vector candidate lookup refused after embedding contract changed: {err}"
                ))
            })?;

            // H3 forced-exact: after the contract/PK read so that lookup still
            // uses its index, before the vector query so hnsw cannot serve it.
            // Shared with the camera-proof EXPLAIN helper: do not re-issue the
            // GUC as extra_set (B3-R1-1).
            self.issue_forced_exact_scan(&mut tx).await?;

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
                let rows = sqlx::query(&self.sql.vector_candidates)
                    .bind(&probe)
                    .bind(fetch)
                    .fetch_all(&mut *tx)
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
                let mut in_session = filter_session_rows::<D>(session, &parsed[..page_len]);
                if boundary_tie || needs_session_fallback(in_session.len(), has_more, k, limit) {
                    let exact_limit = i64::try_from(limit).map_err(|_| {
                        StoreError::Invariant("vector candidate limit does not fit i64".into())
                    })?;
                    let fallback_rows = sqlx::query(&self.sql.session_vector_candidates)
                        .bind(&probe)
                        .bind(session.as_str())
                        .bind(exact_limit)
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(backend)?;
                    let hits = fallback_rows
                        .iter()
                        .map(|row| {
                            let id: String = row.try_get("id").map_err(backend)?;
                            let dist: f64 = row.try_get("dist").map_err(backend)?;
                            let score = D::distance_to_score(dist);
                            if !score.is_finite() {
                                return Err(StoreError::Backend(format!(
                                    "non-finite vector distance for concept {id}"
                                )));
                            }
                            Ok(Scored::new(parse_node_id(&id)?, score))
                        })
                        .collect::<Result<Vec<_>, StoreError>>()?;
                    tx.commit().await.map_err(backend)?;
                    return Ok(hits);
                }
                match next_fetch_k(in_session.len(), has_more, k, limit) {
                    None => {
                        // Query returns rows in dist-asc (= score-desc); filter preserves that
                        // order (filter_session_rows). Truncate to the requested limit.
                        in_session.truncate(limit);
                        tx.commit().await.map_err(backend)?;
                        return Ok(in_session);
                    }
                    Some(next) => {
                        k = next;
                    }
                }
            }
        })
        .await
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
        let pool = &self.pool().await?;
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
        let pool = &self.pool().await?;
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

    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        let pool = &self.pool().await?;
        tx_retry(|| async move {
            let mut tx = pool.begin().await.map_err(|e| {
                map_write_err(e, |m| format!("begin record_canonization transaction: {m}"))
            })?;
            // Fencing-token gate (#1): this durable write path HAD no lease
            // check at all — the canon task bypassed `lease_lost`. Check the
            // token inside this transaction, atomically with the write
            // (rolls back on `?`).
            let current: Option<i64> = sqlx::query_scalar(
                "SELECT current_token FROM session_leases WHERE session_id = $1",
            )
            .bind(event.session_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(backend)?;
            if let Some(cur) = current {
                let cur = u64::try_from(cur).map_err(|_| {
                    StoreError::Invariant(format!(
                        "session {}: negative lease current_token",
                        event.session_id
                    ))
                })?;
                if !lease_permits_write(cur, token) {
                    return Err(StoreError::StaleWrite(format!(
                        "session {}: presented token {token:?} is stale (lease token {cur}) — \
                         single-writer fence (GitHub issue #1)",
                        event.session_id,
                    )));
                }
            }
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

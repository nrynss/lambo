//! T3.3 — SQLite GraphStore adapter (offline / test tier, spec §3.2–§3.3, §4).
//!
//! Same trait surface as [`super::memory::MemoryStore`] over `sqlx::SqlitePool`. SQLite has
//! **no** `VECTOR_SEARCH` capability, so `capabilities()` is empty and
//! `vector_candidates` returns [`StoreError::Capability`]; `vector_dimensions()` is `None`.
//! `Concept.embedding` IS written and read for flush→load round-trip parity (CON-8 — the
//! shared text form lives in the `embedding BLOB`), but it is never queried.
//!
//! ## Dialect notes (T3.1 handoff, binding)
//!
//! - **Timestamps** are stored as fixed ISO-8601 UTC text via
//!   `to_rfc3339_opts(SecondsFormat::Millis, true)` →
//!   `YYYY-MM-DDTHH:MM:SS.SSSZ` (24 chars, milliseconds always present, `Z`
//!   not `+00:00`). RFC 3339 ordering equals lexicographic ordering, so every
//!   age/span comparison is a TEXT `<`/`<=` in SQL. chrono's default
//!   `to_rfc3339()` is NOT used (variable-width fraction + `+00:00` would
//!   break lex comparisons).
//! - **Placeholders** are `?` (positional). **Intervals** don't exist in
//!   SQLite: cutoff timestamps are computed in Rust and bound as TEXT (T3.6
//!   doc note — twin-shaped with Cockroach).
//! - **`ON CONFLICT` targets** follow T3.1: concepts conflict on the `id`
//!   primary key; the *partial* unique index
//!   `(session_id, canonical_key) WHERE concept_type <> 'Observation'` is a
//!   separate constraint that must NOT be targeted — a bare
//!   `ON CONFLICT (session_id, canonical_key)` errors at runtime. Legal
//!   duplicate Observation keys (demote) never conflict with the partial
//!   index. Edges conflict on the natural key `(source, target, edge_type)`
//!   (table-level UNIQUE → autoindexed), matching MemoryStore's key
//!   preference.
//! - **FK enforcement** is the adapter's job: every connection opens with
//!   `PRAGMA foreign_keys = ON` (via `SqliteConnectOptions::foreign_keys`).
//!   `edges.source/target` deliberately carry no FK (spec §4), so deleting a
//!   concept leaves dangling edges — matching MemoryStore.
//! - **One connection** (`max_connections(1)`): `sqlite::memory:` is
//!   per-connection, and a single connection also serializes SQLite's
//!   single-writer model. sqlx's `:memory:` uses a shared-cache URI, but one
//!   connection removes all cross-connection state questions.
//! - **Millisecond precision is the round-trip contract.** The fixed format
//!   truncates sub-millisecond instants; `Utc::now()` timestamps therefore do
//!   NOT round-trip exactly (write `2026-…T12:00:00.867Z`, read back the same
//!   — never `.867053068Z`). Whole-second or ms-aligned instants are exact.
//! - **Cross-runtime pool quirk (affects `load_session`, see `load.rs`).**
//!   sqlx returns a pool connection via a spawned task. A current-thread
//!   Tokio runtime that is *blocked* (e.g. the sync `load_session` worker
//!   thread joined from a `#[tokio::test]` main thread) never runs that task,
//!   so an acquire from another runtime can time out against an in-flight
//!   return. Multi-thread runtimes (production) are unaffected; the T3.5
//!   round-trip test uses the multi-thread flavor for this reason.
//!
//! ## Case folding (keyword_candidates — ASCII-only)
//!
//! Matching lowercases the **column** with SQLite's `lower()`, which is
//! **ASCII-only**, while MemoryStore lowercases with Rust's Unicode
//! `to_lowercase()`. ASCII text agrees exactly (regression-locked by a
//! mixed-case concept in the keyword test); non-ASCII case pairs
//! (`Ä`/`ä`, `İ`/`i`) may diverge. The SQL predicate also lowercases the
//! column itself, so mixed-case rows score like MemoryStore — there is no
//! raw-row `contains` path like Cockroach's pre-remediation loop.
//!
//! ## Structural queries (spec §4.1 + errata — T3.6 three-way gate)
//!
//! `blast_radius` and `interaction_span` are **MemoryStore-exact**, not
//! spec-text-exact (the spec's literal SQL is Cockroach-shaped; SQLite binds a
//! Rust-computed cutoff TEXT and the two queries stay twin-shaped with
//! Cockroach's — T3.3/T3.6 contract). Semantics, all locked by the T3.6
//! three-way agreement matrix against `MemoryStore` on both fixture graphs:
//!
//! - **Errata exclusions (2026-08-11 / T1.4):** only concept-sourced
//!   `Dependency` / `Causal` / `Hierarchical` edges count (the
//!   [`STRUCTURAL_EDGE_IN`] predicate). Provenance `Derives`
//!   (interaction → concept, mandatory §5.7) and `Temporal`
//!   (interaction → interaction) edges must **never un-orphan** a concept —
//!   counting them as "another inbound source" would zero Stage-3 blast
//!   radius on every legal graph. The `JOIN concepts src ON src.id =
//!   e.source` also pins the source to a concept row, so an interaction id
//!   can never be mistaken for a structural source.
//! - **Aged edges only (`e.created_at <= cutoff`).** An inbound structural
//!   edge younger than the cutoff is invisible to both queries, exactly like
//!   MemoryStore's naive scan (`cutoff = now - min_age`, Rust-computed).
//! - **`c.id <> $node` self-exclusion (`blast_radius`).** A hypothetical
//!   structural self-loop is not counted (MemoryStore's skip; semantically
//!   equivalent to the spec text — the graph tier rejects structural
//!   self-loops as cycle invariants).
//! - **Span gates BOTH timestamps** (`e.created_at <= cutoff AND
//!   i.created_at <= cutoff`): the span is built from edges, so an edge
//!   younger than the cutoff is excluded even when its origin interaction is
//!   older (spec §4.1 second errata, 2026-08-11 / P3 T3.3 review — do not
//!   "simplify" back to the literal text). Coverage is computed in Rust in
//!   milliseconds — the identical formula to MemoryStore (span of the
//!   distinct origin-interaction timestamps over the session extent).
//! - **F1 single-point rule:** `coverage` is `0.0` only when no interaction
//!   matches (`distinct == 0`). A non-empty span over a single-point session
//!   extent (one interaction, or all interactions sharing a timestamp)
//!   reports `1.0` — that interaction spans the whole session (canonization
//!   Stage 2 parity in short sessions).
//!
//! ## Load ordering (same-instant tie-breaks)
//!
//! Load queries impose deterministic SQL order: interactions by
//! `(created_at, id)`, concepts/edges by `id`, canonization events by
//! `(occurred_at, id)`, synonyms by `source_key`. MemoryStore preserves
//! insertion order, so rows sharing an instant may reorder relative to it —
//! equality is by value, not by position.
//!
//! ## chunk_group_id (persisted — P3 wave 2 remediation)
//!
//! `concepts.chunk_group_id` (T2.5 demote sets it on Observations, spec §8
//! sibling co-retrieval) is now part of the DDL: the migration carries it
//! inline in the CREATE TABLE and `init_schema` converges pre-existing
//! databases with a `PRAGMA table_info`-guarded `ALTER TABLE` (SQLite has no
//! `ADD COLUMN IF NOT EXISTS` — see the migration header). `flush` upserts it
//! and `load_session` reads it back; the flush→load round-trip test asserts
//! it SURVIVES.
//!
//! ## Session-level metadata
//!
//! `root_goal` **is** carried by the mutation path since XP-8
//! (`Mutation::SetRootGoal`): `flush` updates `sessions.root_goal` with `seed`'s
//! exact JSON encoding and `load_session` reads it back, so a reload no longer
//! silently clears the drift anchor. `created_at`/`closed_at` still have no
//! `Mutation` kind — like MemoryStore, `load_session` returns `None` for those
//! two, and their columns stay inert (the row is created as an FK anchor with a
//! `created_at` DB-default) until a full-snapshot save path exists.
//!
//! ## Embedding contract (session metadata)
//!
//! The `sessions` row carries `embedding_kind` / `embedding_model` /
//! `embedding_dim` (nullable, converged the same guarded way as
//! `chunk_group_id`). The full-snapshot `seed` path (fixtures track, STORE-1)
//! persists `GraphSnapshot.embedding` into those columns; `load_session` reads
//! them back into `GraphSnapshot.embedding` when present, treating a row with
//! `embedding_kind` XOR `embedding_dim` as a corruption error. Ordinary
//! write-behind persists first-use stamps through `Mutation::SetEmbedding`, in
//! the same transaction as vector-bearing concepts; flush/load and incompatible
//! restart regressions cover this path.

// Clippy's `explicit_auto_deref` suggestion is wrong for sqlx: `&mut *tx` reborrows
// the `Transaction` (which implements `sqlx::Executor`), while the suggested `&mut tx`
// produces `&mut &mut Transaction` (which does not). Known sqlx+clippy false-positive;
// kept explicit on purpose.
#![allow(clippy::explicit_auto_deref)]

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use super::lease::{LeaseHolder, LeaseInfo, LeaseOutcome};
use super::vector::{decode_vector, encode_vector};
use super::{map_write_err, validate_vector_candidate_limit, Capabilities, GraphStore};
use crate::types::{
    CanonizationEvent, Concept, Edge, EmbeddingContract, GraphSnapshot, Interaction,
    InteractionSpan, Mutation, MutationBatch, Node, NodeId, Scored, SessionId, StoreError,
};

/// Structural edge types counted by both structural queries (spec §4.1 errata:
/// concept-to-concept `Dependency`/`Causal`/`Hierarchical` only — provenance
/// `Derives`/`Temporal` must not un-orphan concepts).
const STRUCTURAL_EDGE_IN: &str = "'Dependency', 'Causal', 'Hierarchical'";

/// §4.1 interaction-span SQL (twin-shaped with Cockroach's
/// `INTERACTION_SPAN_SQL`; `?` placeholders). The span gates on BOTH the edge
/// and the origin-interaction timestamp (`e.created_at <= ? AND
/// i.created_at <= ?` — spec §4.1 second errata, MemoryStore parity): a
/// structural inbound edge is invisible to the span when EITHER its own
/// timestamp or its source concept's origin interaction is younger than the
/// cutoff. `{STRUCTURAL_EDGE_IN}` is substituted at the call site (the
/// predicate is shared with blast_radius); the substitution keeps this const
/// assertable verbatim in tests.
///
/// **Session scope (F5).** `i.session_id = ?` is not redundant with
/// `e.session_id = ?`: `concepts.origin_interaction` is a **global** FK, so a
/// concept in session S may legally point at an interaction in session S′.
/// Without the filter the span counted those foreign interactions — inflating
/// `distinct` and (since their timestamps sit outside S's extent) the coverage
/// ratio, both against a session `MemoryStore` never sees. The extent CTE was
/// already session-filtered, so the two halves of the ratio disagreed.
const INTERACTION_SPAN_SQL: &str = "WITH span AS ( \
     SELECT DISTINCT i.id, i.created_at \
     FROM edges e \
     JOIN concepts src ON src.id = e.source \
     JOIN interactions i ON i.id = src.origin_interaction \
     WHERE e.target = ? AND e.session_id = ? AND i.session_id = ? \
       AND e.edge_type IN ({STRUCTURAL_EDGE_IN}) \
       AND e.created_at <= ? AND i.created_at <= ? \
 ), \
 extent AS ( \
     SELECT min(created_at) AS lo, max(created_at) AS hi \
     FROM interactions WHERE session_id = ? \
 ) \
 SELECT \
     (SELECT count(*) FROM span), \
     (SELECT min(created_at) FROM span), \
     (SELECT max(created_at) FROM span), \
     extent.lo, extent.hi \
 FROM extent";

/// SQLite GraphStore. Cheap, correct, single-connection.
pub struct SqliteStore {
    options: SqliteConnectOptions,
    pool: OnceLock<SqlitePool>,
}

impl SqliteStore {
    pub fn new(options: SqliteConnectOptions) -> Self {
        Self {
            options,
            pool: OnceLock::new(),
        }
    }

    /// Open a SQLite database — `sqlite::memory:` or a file path.
    ///
    /// File-backed targets are opened with `create_if_missing` (CON-1: sqlx's
    /// default is `false`, so a fresh path failed on first use with
    /// `(code: 14) unable to open database file`) plus WAL / busy_timeout
    /// tuning (STORE-9); in-memory targets get neither WAL nor busy_timeout
    /// (WAL is meaningless there — SQLite silently reports `memory` for
    /// `journal_mode`; `create_if_missing` is applied to both, harmlessly).
    pub fn connect(path: &str) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::from_str(path)
            .map_err(|e| StoreError::Backend(format!("sqlite connect options {path:?}: {e}")))?
            .create_if_missing(true);
        let options = if Self::is_in_memory_uri(path) {
            options
        } else {
            // File-backed durability (STORE-9): WAL keeps the schema readable
            // by a concurrent external reader (spec §2.2) instead of failing a
            // flush with SQLITE_BUSY, and busy_timeout makes a momentarily
            // locked DB wait rather than error. 8s is deliberately non-default
            // (sqlx's default is 5s) so the wiring stays observable in tests.
            // Never applied to in-memory spellings.
            options
                .journal_mode(SqliteJournalMode::Wal)
                .busy_timeout(Duration::from_secs(8))
        };
        Ok(Self::new(options))
    }

    /// Whether `path` names an in-memory database as far as sqlx's `FromStr`
    /// is concerned (database part `:memory:` or a `mode=memory` query
    /// parameter, position-independent). These spellings must never receive
    /// the file-backed WAL / busy_timeout tuning (STORE-9 guard). Mirror
    /// sqlx-sqlite's grammar exactly: strip the `sqlite://`/`sqlite:` prefixes,
    /// split the database part from the query at the first `?`, then treat the
    /// URI as in-memory when the database part is `:memory:` or any
    /// `&`-separated query parameter is `mode=memory`. Note sqlx executes the
    /// pragmas unconditionally — SQLite itself silently returns `memory` for
    /// `journal_mode` on an in-memory database — so a guard miss is benign but
    /// violates this contract.
    fn is_in_memory_uri(path: &str) -> bool {
        let t = path.trim();
        let stripped = t
            .trim_start_matches("sqlite://")
            .trim_start_matches("sqlite:");
        let (database, params) = match stripped.split_once('?') {
            Some((db, query)) => (db, Some(query)),
            None => (stripped, None),
        };
        if database == ":memory:" {
            return true;
        }
        let Some(params) = params else {
            return false;
        };
        params.split('&').any(|param| {
            let (key, value) = param.split_once('=').unwrap_or((param, ""));
            key == "mode" && value == "memory"
        })
    }

    /// The lazily-created pool. `SqlitePoolOptions::connect_lazy_with` spawns
    /// a background maintenance task via `tokio::spawn`, which panics outside
    /// a Tokio context — and `build_store` runs at process start in a **sync**
    /// context (see `main.rs`). Every `GraphStore` method is async, so the
    /// pool is created on first use, from inside a runtime. Race-safe via
    /// `OnceLock::set` (losers drop their duplicate, never-used pool).
    fn pool(&self) -> &SqlitePool {
        if let Some(p) = self.pool.get() {
            return p;
        }
        let pool = SqlitePoolOptions::new()
            // One connection: sqlite::memory: is per-connection, and a single
            // connection also serializes SQLite's single-writer model. (See
            // module doc for the cross-runtime caveat.)
            .max_connections(1)
            .connect_lazy_with(self.options.clone());
        let _ = self.pool.set(pool);
        self.pool.get().expect("pool set just above")
    }

    /// Ensure every session touched by the batch has a `sessions` row (FK
    /// anchor; `created_at` DB-default, metadata columns inert — see module
    /// doc). Idempotent, so once per unique session per batch is enough.
    async fn ensure_sessions(
        &self,
        tx: &mut sqlx::SqliteConnection,
        sessions: &HashSet<String>,
    ) -> Result<(), StoreError> {
        for sid in sessions {
            sqlx::query(
                "INSERT INTO sessions (session_id) VALUES (?) \
                 ON CONFLICT (session_id) DO NOTHING",
            )
            .bind(sid)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_write_err(e, |m| format!("ensure session row: {m}")))?;
        }
        Ok(())
    }

    /// Mirror MemoryStore: queries against a session that was never written
    /// fail with `SessionNotFound`, not an empty answer.
    async fn require_session(&self, session: &SessionId) -> Result<(), StoreError> {
        let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM sessions WHERE session_id = ?")
            .bind(&session.0)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| db_err("lookup session", e))?;
        if found.is_none() {
            return Err(StoreError::SessionNotFound(session.0.clone()));
        }
        Ok(())
    }

    /// Seed a prebuilt snapshot directly (fixtures track, MemoryStore/Cockroach
    /// parity). Writes all seven tables in one transaction — the full-snapshot
    /// path that carries synonyms and reservations (they have no `Mutation` kind,
    /// S5 contract). Persists `GraphSnapshot.embedding` into
    /// `sessions.embedding_{kind,model,dim}` (STORE-1), so a seeded contract
    /// survives `load_session` instead of being dropped.
    #[cfg(feature = "fixtures")]
    pub async fn seed(&self, snapshot: &GraphSnapshot) -> Result<(), StoreError> {
        let embedding_dim = snapshot
            .embedding
            .as_ref()
            .map(|contract| i64::try_from(contract.dim))
            .transpose()
            .map_err(|_| StoreError::Invariant("embedding dimension does not fit i64".into()))?;
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map_write_err(e, |m| format!("begin seed transaction: {m}")))?;
        let root_goal = snapshot
            .root_goal
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| StoreError::Backend(format!("serialize root_goal: {e}")))?;
        let embedding = snapshot.embedding.as_ref();
        let (embedding_kind, embedding_model) = match embedding {
            Some(c) => (Some(c.kind.as_str()), c.model.as_deref()),
            None => (None, None),
        };
        sqlx::query(
            "INSERT INTO sessions (\
                 session_id, root_goal, created_at, closed_at, \
                 embedding_kind, embedding_model, embedding_dim) \
             VALUES (?, ?, COALESCE(?, strftime('%Y-%m-%dT%H:%M:%fZ','now')), ?, ?, ?, ?) \
             ON CONFLICT (session_id) DO UPDATE SET \
                 root_goal = excluded.root_goal, \
                 created_at = excluded.created_at, \
                 closed_at = excluded.closed_at, \
                 embedding_kind = excluded.embedding_kind, \
                 embedding_model = excluded.embedding_model, \
                 embedding_dim = excluded.embedding_dim",
        )
        .bind(&snapshot.session_id.0)
        .bind(root_goal.as_deref())
        .bind(snapshot.created_at.map(ts_to_text))
        .bind(snapshot.closed_at.map(ts_to_text))
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
            sqlx::query(
                "INSERT INTO synonyms (session_id, source_key, canonical_key) \
                 VALUES (?, ?, ?) \
                 ON CONFLICT (session_id, source_key) DO UPDATE SET \
                     canonical_key = excluded.canonical_key",
            )
            .bind(&s.session_id.0)
            .bind(&s.source_key)
            .bind(&s.canonical_key)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_write_err(e, |m| format!("upsert synonym: {m}")))?;
        }
        for r in &snapshot.reservations {
            sqlx::query(
                "INSERT INTO reservations (session_id, node_id, agent_id, expires_at) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT (session_id, node_id) DO UPDATE SET \
                     agent_id = excluded.agent_id, \
                     expires_at = excluded.expires_at",
            )
            .bind(&r.session_id.0)
            .bind(r.node_id.0.to_string())
            .bind(&r.agent_id.0)
            .bind(ts_to_text(r.expires_at))
            .execute(&mut *tx)
            .await
            .map_err(|e| map_write_err(e, |m| format!("upsert reservation: {m}")))?;
        }
        for ev in &snapshot.canonization_events {
            let from_status = enum_to_text(&ev.from_status, "from_status")?;
            let to_status = enum_to_text(&ev.to_status, "to_status")?;
            sqlx::query(
                "INSERT INTO canonization_events (\
                     id, session_id, node_id, from_status, to_status, blast_radius, \
                     last_demotion_time, occurred_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (id) DO NOTHING",
            )
            .bind(ev.id.0.to_string())
            .bind(&ev.session_id.0)
            .bind(ev.node_id.0.to_string())
            .bind(from_status)
            .bind(to_status)
            .bind(ev.blast_radius)
            .bind(ev.last_demotion_time.map(ts_to_text))
            .bind(ts_to_text(ev.occurred_at))
            .execute(&mut *tx)
            .await
            .map_err(|e| map_write_err(e, |m| format!("append canonization event: {m}")))?;
        }
        tx.commit()
            .await
            .map_err(|e| map_write_err(e, |m| format!("commit seed transaction: {m}")))?;
        Ok(())
    }
}

#[async_trait]
impl GraphStore for SqliteStore {
    async fn init_schema(&self) -> Result<(), StoreError> {
        // The T3.1 DDL is idempotent (every statement IF NOT EXISTS); the
        // SQLite driver executes multi-statement strings statement-by-statement
        // and aborts on the first error.
        sqlx::query(include_str!("../../migrations/sqlite/001_init.sql"))
            .execute(self.pool())
            .await
            .map_err(|e| db_err("init_schema (migrations/sqlite/001_init.sql)", e))?;

        // Post-T3.1 columns (P3 wave 2 remediation): fresh databases carry them
        // inline from the DDL above; pre-existing databases converge here.
        // SQLite has no `ADD COLUMN IF NOT EXISTS` (verified: 3.53.4 rejects
        // the syntax), so each column is inspected via `pragma_table_info` and
        // a plain ALTER is issued only when it is missing — making the whole
        // init idempotent on any database state. See the migration header.
        ensure_column(
            self.pool(),
            "concepts",
            "chunk_group_id",
            "ALTER TABLE concepts ADD COLUMN chunk_group_id TEXT",
        )
        .await?;
        ensure_column(
            self.pool(),
            "sessions",
            "embedding_kind",
            "ALTER TABLE sessions ADD COLUMN embedding_kind TEXT",
        )
        .await?;
        ensure_column(
            self.pool(),
            "sessions",
            "embedding_model",
            "ALTER TABLE sessions ADD COLUMN embedding_model TEXT",
        )
        .await?;
        ensure_column(
            self.pool(),
            "sessions",
            "embedding_dim",
            "ALTER TABLE sessions ADD COLUMN embedding_dim INTEGER",
        )
        .await?;
        ensure_column(
            self.pool(),
            "canonization_events",
            "last_demotion_time",
            "ALTER TABLE canonization_events ADD COLUMN last_demotion_time TEXT",
        )
        .await?;
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::empty()
    }

    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        acquire_or_refresh(self.pool(), session, holder, ttl).await
    }

    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        acquire_or_refresh(self.pool(), session, holder, ttl).await
    }

    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        // Holder-scoped: only our own row (a stale release after our lease was
        // stolen must not evict the new holder).
        sqlx::query("DELETE FROM session_leases WHERE session_id = ?1 AND holder = ?2")
            .bind(&session.0)
            .bind(holder.token())
            .execute(self.pool())
            .await
            .map_err(|e| db_err("release lease", e))?;
        Ok(())
    }

    async fn flush(&self, batch: &MutationBatch) -> Result<(), StoreError> {
        if batch.mutations.is_empty() {
            return Ok(());
        }
        // Replay in batch order — the graph contract (§2.4 / drain_log) says
        // chronological order is the order, and stores MUST NOT re-sort.
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map_write_err(e, |m| format!("begin flush transaction: {m}")))?;

        let mut sessions: HashSet<String> = HashSet::new();
        for m in &batch.mutations {
            match m {
                Mutation::UpsertNode { node } => {
                    sessions.insert(node.session_id().0.clone());
                }
                Mutation::UpsertEdge { edge } => {
                    sessions.insert(edge.session_id.0.clone());
                }
                Mutation::CanonizationTransition { event } => {
                    sessions.insert(event.session_id.0.clone());
                }
                Mutation::SetRootGoal { session_id, .. } => {
                    sessions.insert(session_id.0.clone());
                }
                Mutation::SetEmbedding { session_id, .. } => {
                    sessions.insert(session_id.0.clone());
                }
                Mutation::DeleteNode { .. } | Mutation::DeleteEdge { .. } => {}
            }
        }
        self.ensure_sessions(&mut *tx, &sessions).await?;

        for m in &batch.mutations {
            match m {
                Mutation::UpsertNode { node } => match node {
                    Node::Interaction(i) => upsert_interaction(&mut *tx, i).await?,
                    Node::Concept(c) => upsert_concept(&mut *tx, c).await?,
                },
                Mutation::UpsertEdge { edge } => upsert_edge(&mut *tx, edge).await?,
                Mutation::DeleteNode { id } => {
                    delete_node(&mut *tx, *id).await?;
                }
                Mutation::DeleteEdge { id } => {
                    sqlx::query("DELETE FROM edges WHERE id = ?")
                        .bind(id.0.to_string())
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| map_write_err(e, |m| format!("delete edge: {m}")))?;
                }
                Mutation::CanonizationTransition { event } => {
                    apply_canonization_transition(&mut *tx, event).await?;
                }
                Mutation::SetRootGoal { session_id, goal } => {
                    set_root_goal(&mut *tx, session_id, goal.as_ref()).await?;
                }
                Mutation::SetEmbedding {
                    session_id,
                    embedding,
                } => {
                    set_embedding(&mut *tx, session_id, embedding.as_ref()).await?;
                }
            }
        }

        tx.commit()
            .await
            .map_err(|e| map_write_err(e, |m| format!("commit flush transaction: {m}")))?;
        Ok(())
    }

    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        // One read transaction so the session materializes from a consistent
        // view (startup path; single-connection pool would otherwise interleave
        // with a concurrent flush between SELECTs).
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| db_err("begin load transaction", e))?;

        // The existence probe doubles as the embedding-contract read. Both
        // snapshot seed and `SetEmbedding` flush write these columns; root_goal
        // likewise has its ordered mutation path (XP-8).
        let row = sqlx::query(
            "SELECT embedding_kind, embedding_model, embedding_dim, root_goal \
             FROM sessions WHERE session_id = ?",
        )
        .bind(&session.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| db_err("lookup session", e))?;
        let row = match row {
            Some(row) => row,
            None => return Err(StoreError::SessionNotFound(session.0.clone())),
        };
        let embedding_kind: Option<String> =
            row.try_get(0).map_err(|e| db_err("lookup session", e))?;
        let embedding_model: Option<String> =
            row.try_get(1).map_err(|e| db_err("lookup session", e))?;
        let embedding_dim: Option<i64> = row.try_get(2).map_err(|e| db_err("lookup session", e))?;
        // XP-8: `root_goal` survives a reload — `Mutation::SetRootGoal` writes
        // it, so replaying the log no longer silently clears the drift anchor.
        let root_goal: Option<String> = row.try_get(3).map_err(|e| db_err("lookup session", e))?;
        let root_goal = root_goal
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()
            .map_err(|e| StoreError::Backend(format!("parse root_goal JSON: {e}")))?;
        let embedding = match (embedding_kind, embedding_dim) {
            (Some(kind), Some(dim)) => Some(EmbeddingContract {
                kind,
                model: embedding_model,
                dim: usize::try_from(dim).map_err(|_| {
                    StoreError::Backend(format!(
                        "sessions row for {} has negative embedding_dim",
                        session.0
                    ))
                })?,
            }),
            (None, None) => None,
            (Some(_), None) => {
                return Err(StoreError::Backend(format!(
                    "sessions row for {} has embedding_kind without embedding_dim",
                    session.0
                )));
            }
            (None, Some(_)) => {
                return Err(StoreError::Backend(format!(
                    "sessions row for {} has embedding_dim without embedding_kind",
                    session.0
                )));
            }
        };

        let interactions = load_interactions(&mut *tx, session).await?;
        let concepts = load_concepts(&mut *tx, session).await?;
        let edges = load_edges(&mut *tx, session).await?;
        let synonyms = load_synonyms(&mut *tx, session).await?;
        let reservations = load_reservations(&mut *tx, session).await?;
        let canonization_events = load_canonization_events(&mut *tx, session).await?;

        tx.commit()
            .await
            .map_err(|e| db_err("commit load transaction", e))?;

        Ok(GraphSnapshot {
            session_id: session.clone(),
            root_goal,
            // `created_at`/`closed_at` are still snapshot-only (no Mutation
            // kind) — None, matching MemoryStore (see module doc).
            created_at: None,
            closed_at: None,
            interactions,
            concepts,
            edges,
            synonyms,
            reservations,
            canonization_events,
            embedding,
        })
    }

    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        // MemoryStore parity: trim/lowercase, drop empties; empty tokens or
        // limit 0 match nothing (a bare `contains("")` would match everything).
        let tokens_l: Vec<String> = tokens
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        if tokens_l.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        self.require_session(session).await?;

        // Exact substring semantics (memory's `contains`) via instr() on
        // lowercased content/key — no LIKE wildcard interpretation. Score =
        // number of tokens hitting content OR canonical_key; ties by id.
        let mut sql = String::from("SELECT id, ");
        for (i, _) in tokens_l.iter().enumerate() {
            if i > 0 {
                sql.push_str(" + ");
            }
            sql.push_str("(instr(lower(content), ?) > 0 OR instr(lower(canonical_key), ?) > 0)");
        }
        sql.push_str(" AS score FROM concepts WHERE session_id = ? AND (");
        for (i, _) in tokens_l.iter().enumerate() {
            if i > 0 {
                sql.push_str(" OR ");
            }
            sql.push_str("(instr(lower(content), ?) > 0 OR instr(lower(canonical_key), ?) > 0)");
        }
        sql.push_str(") ORDER BY score DESC, id ASC LIMIT ?");

        let mut q = sqlx::query(&sql);
        for tok in &tokens_l {
            q = q.bind(tok).bind(tok);
        }
        q = q.bind(&session.0);
        for tok in &tokens_l {
            q = q.bind(tok).bind(tok);
        }
        q = q.bind(limit as i64);

        let rows = q
            .fetch_all(self.pool())
            .await
            .map_err(|e| db_err("keyword_candidates", e))?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row
                .try_get(0)
                .map_err(|e| db_err("keyword_candidates", e))?;
            let score: i64 = row
                .try_get(1)
                .map_err(|e| db_err("keyword_candidates", e))?;
            out.push(Scored::new(node_id(&id, "concept id")?, score as f64));
        }
        Ok(out)
    }

    async fn vector_candidates(
        &self,
        _session: &SessionId,
        _embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        validate_vector_candidate_limit(limit)?;
        Err(StoreError::Capability(
            "SqliteStore has no VECTOR_SEARCH".into(),
        ))
    }

    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        // Spec §4.1 ported to `?` placeholders; the cutoff is computed in Rust
        // (SQLite has no INTERVAL) and bound as the fixed ISO-8601 TEXT.
        // Divergence from the spec text (for MemoryStore agreement): `c.id <> ?`
        // excludes the node itself, and `e.created_at <= ?` gates the edge age
        // exactly like MemoryStore (the spec's span query gates only the
        // interaction age — see interaction_span).
        //
        // **Session scope (R2-3).** Both structural subqueries scope their
        // source concept with `src.session_id = ?` / `src2.session_id = ?`,
        // matching Cockroach's `BLAST_RADIUS_SQL` and MemoryStore's
        // `concept_ids` (built from the session snapshot). Edges carry a
        // `session_id` but the join to `concepts` did not, so a cross-session
        // edge into a dependent satisfied the `NOT EXISTS` arm and
        // **un-orphaned** it here and nowhere else — SQLite under-counted
        // blast against both other backends, suppressing Stage-3 promotions
        // and mis-ranking budget demotion.
        self.require_session(session).await?;
        // F8: the cutoff anchor is the caller's `now`, never a wall clock here.
        let cutoff = cutoff_text(now, min_edge_age)?;
        let node_text = node.0.to_string();

        let row = sqlx::query(&format!(
            "SELECT count(*) \
             FROM concepts c \
             WHERE c.session_id = ? \
               AND c.id <> ? \
               AND EXISTS ( \
                   SELECT 1 FROM edges e \
                   JOIN concepts src ON src.id = e.source AND src.session_id = ? \
                   WHERE e.target = c.id AND e.source = ? \
                     AND e.edge_type IN ({STRUCTURAL_EDGE_IN}) \
                     AND e.created_at <= ?) \
               AND NOT EXISTS ( \
                   SELECT 1 FROM edges e2 \
                   JOIN concepts src2 ON src2.id = e2.source AND src2.session_id = ? \
                   WHERE e2.target = c.id AND e2.source <> ? \
                     AND e2.edge_type IN ({STRUCTURAL_EDGE_IN}) \
                     AND e2.created_at <= ?)"
        ))
        .bind(&session.0)
        .bind(&node_text)
        .bind(&session.0)
        .bind(&node_text)
        .bind(&cutoff)
        .bind(&session.0)
        .bind(&node_text)
        .bind(&cutoff)
        .fetch_one(self.pool())
        .await
        .map_err(|e| db_err("blast_radius", e))?;
        let n: i64 = row.try_get(0).map_err(|e| db_err("blast_radius", e))?;
        Ok(n as u64)
    }

    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<InteractionSpan, StoreError> {
        // Spec §4.1 span query: distinct origin interactions of concept-sourced
        // structural edges into `node`, aged on BOTH the edge and the origin
        // interaction (MemoryStore agreement — the spec text ages only the
        // interaction; the fixture data satisfies both, but the three-way gate
        // is MemoryStore's naive answer). Coverage is computed in Rust in ms,
        // identical to MemoryStore's formula.
        self.require_session(session).await?;
        // F8: the cutoff anchor is the caller's `now`, never a wall clock here.
        let cutoff = cutoff_text(now, min_age)?;
        let node_text = node.0.to_string();

        let row =
            sqlx::query(&INTERACTION_SPAN_SQL.replace("{STRUCTURAL_EDGE_IN}", STRUCTURAL_EDGE_IN))
                .bind(&node_text)
                .bind(&session.0)
                .bind(&session.0)
                .bind(&cutoff)
                .bind(&cutoff)
                .bind(&session.0)
                .fetch_one(self.pool())
                .await
                .map_err(|e| db_err("interaction_span", e))?;

        let distinct: i64 = row.try_get(0).map_err(|e| db_err("interaction_span", e))?;
        let span_lo: Option<String> = row.try_get(1).map_err(|e| db_err("interaction_span", e))?;
        let span_hi: Option<String> = row.try_get(2).map_err(|e| db_err("interaction_span", e))?;
        let sess_lo: Option<String> = row.try_get(3).map_err(|e| db_err("interaction_span", e))?;
        let sess_hi: Option<String> = row.try_get(4).map_err(|e| db_err("interaction_span", e))?;

        let coverage = match (span_lo, span_hi) {
            (Some(lo_s), Some(hi_s)) => {
                let lo = text_to_ts(&lo_s)?;
                let hi = text_to_ts(&hi_s)?;
                let sess_lo = match sess_lo {
                    Some(s) => text_to_ts(&s)?,
                    None => lo,
                };
                let sess_hi = match sess_hi {
                    Some(s) => text_to_ts(&s)?,
                    None => hi,
                };
                let sess_span = (sess_hi - sess_lo).num_milliseconds().max(0) as f64;
                if sess_span <= 0.0 {
                    // F1: single-point session extent (one interaction, or all
                    // interactions sharing a timestamp) with at least one
                    // supported interaction (span_lo/hi are Some here, so
                    // distinct >= 1) -> coverage 1.0, mirroring MemoryStore
                    // and the Cockroach SQL (canonization Stage 2 parity).
                    1.0
                } else {
                    let span = (hi - lo).num_milliseconds().max(0) as f64;
                    (span / sess_span).clamp(0.0, 1.0)
                }
            }
            _ => 0.0,
        };
        Ok(InteractionSpan {
            distinct: distinct as u64,
            coverage,
        })
    }

    async fn record_canonization(&self, event: &CanonizationEvent) -> Result<(), StoreError> {
        let mut tx = self.pool().begin().await.map_err(|e| {
            map_write_err(e, |m| format!("begin record_canonization transaction: {m}"))
        })?;
        apply_canonization_transition(&mut *tx, event).await?;
        tx.commit().await.map_err(|e| {
            map_write_err(e, |m| {
                format!("commit record_canonization transaction: {m}")
            })
        })?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Statement helpers
// ---------------------------------------------------------------------------

fn db_err(context: &str, e: sqlx::Error) -> StoreError {
    StoreError::Backend(format!("{context}: {e}"))
}

/// Atomic single-writer lease acquire / refresh (T8.6).
///
/// ONE statement — `INSERT ... ON CONFLICT DO UPDATE ... WHERE ... RETURNING` —
/// so the decision is made under SQLite's write lock with no read-then-write
/// race. The update fires only when the existing lease is expired or is already
/// ours; on a refresh we keep the original `acquired_at`. All timestamps come
/// from SQLite's own `strftime(...,'now')` — never a caller argument (F18).
///
/// * A returned row whose holder is ours ⇒ [`LeaseOutcome::Acquired`] (fresh
///   insert, expired steal, or our refresh).
/// * An empty RETURNING ⇒ the guard was false: a live lease is held by someone
///   else. We read it back to report the holder + age ([`LeaseOutcome::Held`]).
///   If the row vanished in between (released concurrently) we retry a bounded
///   number of times.
async fn acquire_or_refresh(
    pool: &SqlitePool,
    session: &SessionId,
    holder: &LeaseHolder,
    ttl: Duration,
) -> Result<LeaseOutcome, StoreError> {
    // Fractional seconds keep sub-second TTLs (tests) honest; whole seconds for
    // the production 45s. Bound as a strftime modifier, e.g. "+45 seconds".
    let ttl_modifier = format!("+{} seconds", ttl.as_secs_f64());
    let token = holder.token();
    const ACQUIRE_SQL: &str = "\
        INSERT INTO session_leases (session_id, holder, acquired_at, expires_at) \
        VALUES (?1, ?2, \
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), \
                strftime('%Y-%m-%dT%H:%M:%fZ','now', ?3)) \
        ON CONFLICT (session_id) DO UPDATE SET \
            holder = excluded.holder, \
            acquired_at = CASE WHEN session_leases.holder = excluded.holder \
                               THEN session_leases.acquired_at ELSE excluded.acquired_at END, \
            expires_at = excluded.expires_at \
        WHERE session_leases.expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now') \
           OR session_leases.holder = excluded.holder \
        RETURNING holder, acquired_at, expires_at";

    for _ in 0..3 {
        let won: Option<(String, String, String)> = sqlx::query_as(ACQUIRE_SQL)
            .bind(&session.0)
            .bind(&token)
            .bind(&ttl_modifier)
            .fetch_optional(pool)
            .await
            .map_err(|e| db_err("acquire lease", e))?;
        if let Some(row) = won {
            return Ok(LeaseOutcome::Acquired(lease_info_from_text(row)?));
        }
        // Guard was false — someone else holds a live lease. Read it back.
        let current: Option<(String, String, String)> = sqlx::query_as(
            "SELECT holder, acquired_at, expires_at FROM session_leases WHERE session_id = ?1",
        )
        .bind(&session.0)
        .fetch_optional(pool)
        .await
        .map_err(|e| db_err("read current lease", e))?;
        match current {
            Some(row) => {
                let info = lease_info_from_text(row)?;
                let age = (Utc::now() - info.acquired_at)
                    .to_std()
                    .unwrap_or(Duration::ZERO);
                return Ok(LeaseOutcome::Held { current: info, age });
            }
            // Released between our upsert and this read: retry the acquire.
            None => continue,
        }
    }
    Err(StoreError::Backend(
        "acquire lease: contended row kept changing under us (retries exhausted)".into(),
    ))
}

fn lease_info_from_text(row: (String, String, String)) -> Result<LeaseInfo, StoreError> {
    let (holder, acquired_at, expires_at) = row;
    Ok(LeaseInfo {
        holder,
        acquired_at: text_to_ts(&acquired_at)?,
        expires_at: text_to_ts(&expires_at)?,
    })
}

/// Idempotent post-T3.1 column convergence: SQLite has no
/// `ADD COLUMN IF NOT EXISTS`, so check `pragma_table_info` first and ALTER
/// only when the column is absent. Safe to call on every `init_schema` (fresh
/// databases already carry the columns from the DDL — no-op).
async fn ensure_column(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    alter_ddl: &str,
) -> Result<(), StoreError> {
    let present: Option<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info(?) WHERE name = ?")
            .bind(table)
            .bind(column)
            .fetch_optional(pool)
            .await
            .map_err(|e| db_err(&format!("init_schema: inspect {table}.{column}"), e))?;
    if present.is_none() {
        sqlx::query(alter_ddl)
            .execute(pool)
            .await
            .map_err(|e| db_err(&format!("init_schema: add {table}.{column}"), e))?;
    }
    Ok(())
}

/// Fixed ISO-8601 UTC serialization (T3.1 contract):
/// `YYYY-MM-DDTHH:MM:SS.SSSZ` — 24 chars, ms always present, `Z` suffix.
fn ts_to_text(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn text_to_ts(s: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| StoreError::Backend(format!("invalid stored timestamp {s:?}: {e}")))
}

/// Cutoff timestamp for age filters, computed in Rust (SQLite has no INTERVAL)
/// and bound as the fixed TEXT so lex comparison in SQL is valid.
fn cutoff_text(now: DateTime<Utc>, age: Duration) -> Result<String, StoreError> {
    let d = chrono::Duration::from_std(age)
        .map_err(|e| StoreError::Backend(format!("age duration out of range: {e}")))?;
    Ok(ts_to_text(now - d))
}

fn node_id(s: &str, what: &str) -> Result<NodeId, StoreError> {
    uuid::Uuid::parse_str(s)
        .map(NodeId)
        .map_err(|e| StoreError::Backend(format!("invalid stored {what} {s:?}: {e}")))
}

fn enum_to_text<T: serde::Serialize>(v: &T, what: &str) -> Result<String, StoreError> {
    let value = serde_json::to_value(v)
        .map_err(|e| StoreError::Backend(format!("serialize {what}: {e}")))?;
    match value {
        serde_json::Value::String(s) => Ok(s),
        other => Err(StoreError::Backend(format!(
            "serialize {what}: expected string, got {other:?}"
        ))),
    }
}

fn text_to_enum<T: serde::de::DeserializeOwned>(s: &str, what: &str) -> Result<T, StoreError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| StoreError::Backend(format!("invalid stored {what} {s:?}: {e}")))
}

async fn upsert_interaction(
    tx: &mut sqlx::SqliteConnection,
    i: &Interaction,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO interactions (id, session_id, agent_id, prompt_text, previous_id, created_at) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT (id) DO UPDATE SET \
             session_id = excluded.session_id, \
             agent_id = excluded.agent_id, \
             prompt_text = excluded.prompt_text, \
             previous_id = excluded.previous_id, \
             created_at = excluded.created_at",
    )
    .bind(i.id.0.to_string())
    .bind(&i.session_id.0)
    .bind(&i.agent_id.0)
    .bind(i.prompt_text.as_deref())
    .bind(i.previous_id.map(|id| id.0.to_string()))
    .bind(ts_to_text(i.created_at))
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("upsert interaction: {m}")))?;
    Ok(())
}

async fn upsert_concept(tx: &mut sqlx::SqliteConnection, c: &Concept) -> Result<(), StoreError> {
    // Conflict target is the `id` PRIMARY KEY. The partial unique index
    // (session_id, canonical_key) WHERE concept_type <> 'Observation' is NOT a
    // valid target (bare ON CONFLICT errors); legal duplicate Observation keys
    // (demote) never conflict with it, and a genuine duplicate non-Observation
    // key surfaces as an error (the graph tier already forbids it in RAM).
    //
    // R2-1: `canonization_status` / `blast_radius` / `last_demotion_time` are
    // in the INSERT column list (a brand-new row must carry them) but
    // deliberately **absent from the DO UPDATE SET list** — on an existing row
    // the canonization path is their only writer. Rationale on
    // `Mutation::UpsertNode`.
    let concept_type = enum_to_text(&c.concept_type, "concept_type")?;
    let status = enum_to_text(&c.canonization_status, "canonization_status")?;
    // CON-8: the embedding is written for flush→load round-trip parity. Same
    // wire form as Cockroach's VECTOR text literal (shared store::vector codec),
    // stored in the BLOB column; never NULL for a present vector, NULL otherwise.
    let embedding = c
        .embedding
        .as_ref()
        .map(|v| encode_vector(v))
        .transpose()?
        .map(|s| s.into_bytes());
    sqlx::query(
        "INSERT INTO concepts (\
             id, session_id, content, canonical_key, concept_type, origin_interaction, \
             origin_agent, created_at, access_count, last_accessed, gc_survived, \
             canonization_status, blast_radius, last_demotion_time, embedding, \
             chunk_group_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (id) DO UPDATE SET \
             session_id = excluded.session_id, \
             content = excluded.content, \
             canonical_key = excluded.canonical_key, \
             concept_type = excluded.concept_type, \
             origin_interaction = excluded.origin_interaction, \
             origin_agent = excluded.origin_agent, \
             created_at = excluded.created_at, \
             access_count = excluded.access_count, \
             last_accessed = excluded.last_accessed, \
             gc_survived = excluded.gc_survived, \
             embedding = excluded.embedding, \
             chunk_group_id = excluded.chunk_group_id",
    )
    .bind(c.id.0.to_string())
    .bind(&c.session_id.0)
    .bind(&c.content)
    .bind(&c.canonical_key)
    .bind(concept_type)
    .bind(c.origin_interaction.0.to_string())
    .bind(&c.origin_agent.0)
    .bind(ts_to_text(c.created_at))
    .bind(c.access_count)
    .bind(c.last_accessed.map(ts_to_text))
    .bind(c.gc_survived)
    .bind(status)
    .bind(c.blast_radius)
    .bind(c.last_demotion_time.map(ts_to_text))
    .bind(embedding.as_deref())
    .bind(&c.chunk_group_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("upsert concept: {m}")))?;
    Ok(())
}

async fn upsert_edge(tx: &mut sqlx::SqliteConnection, e: &Edge) -> Result<(), StoreError> {
    // Natural-key preference (MemoryStore parity): the table-level
    // UNIQUE (source, target, edge_type) autoindexes and is a legal target.
    let edge_type = enum_to_text(&e.edge_type, "edge_type")?;
    sqlx::query(
        "INSERT INTO edges (\
             id, session_id, source, target, edge_type, weight, reinforcements, \
             created_at, last_reinforced) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (source, target, edge_type) DO UPDATE SET \
             id = excluded.id, \
             session_id = excluded.session_id, \
             weight = excluded.weight, \
             reinforcements = excluded.reinforcements, \
             created_at = excluded.created_at, \
             last_reinforced = excluded.last_reinforced",
    )
    .bind(e.id.0.to_string())
    .bind(&e.session_id.0)
    .bind(e.source.0.to_string())
    .bind(e.target.0.to_string())
    .bind(edge_type)
    .bind(e.weight)
    .bind(e.reinforcements)
    .bind(ts_to_text(e.created_at))
    .bind(ts_to_text(e.last_reinforced))
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("upsert edge: {m}")))?;
    Ok(())
}

async fn delete_node(tx: &mut sqlx::SqliteConnection, id: NodeId) -> Result<(), StoreError> {
    // MemoryStore parity: a node delete removes the node plus every incident
    // edge (edges carry no FK, so dangling edges would otherwise survive).
    let id_text = id.0.to_string();
    sqlx::query("DELETE FROM interactions WHERE id = ?")
        .bind(&id_text)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("delete interaction: {m}")))?;
    sqlx::query("DELETE FROM concepts WHERE id = ?")
        .bind(&id_text)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("delete concept: {m}")))?;
    sqlx::query("DELETE FROM edges WHERE source = ? OR target = ? OR id = ?")
        .bind(&id_text)
        .bind(&id_text)
        .bind(&id_text)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("delete incident edges: {m}")))?;
    Ok(())
}

/// XP-8: persist a session's `root_goal`. The column already exists (both
/// schemas carry it, `seed` writes it) and the JSON encoding is `seed`'s
/// exactly, so a goal set through the mutation path and one seeded from a
/// snapshot are indistinguishable on reload. `ensure_sessions` has already
/// created the row, so a zero-row update means the session vanished mid-batch.
async fn set_root_goal(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
    goal: Option<&serde_json::Value>,
) -> Result<(), StoreError> {
    let encoded = goal
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| StoreError::Backend(format!("serialize root_goal: {e}")))?;
    let res = sqlx::query("UPDATE sessions SET root_goal = ? WHERE session_id = ?")
        .bind(encoded.as_deref())
        .bind(&session.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("set root_goal: {m}")))?;
    if res.rows_affected() == 0 {
        return Err(StoreError::NotFound(format!(
            "sessions row for {session} while setting root_goal"
        )));
    }
    Ok(())
}

/// Persist the embedding-space identity in the same ordered transaction as
/// concept vectors. A reload must never observe vectors without their contract.
async fn set_embedding(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
    embedding: Option<&crate::types::EmbeddingContract>,
) -> Result<(), StoreError> {
    let dim = embedding
        .map(|e| i64::try_from(e.dim))
        .transpose()
        .map_err(|_| {
            StoreError::Invariant(format!(
                "embedding dimension does not fit i64 for {session}"
            ))
        })?;
    if embedding.is_some() {
        sqlx::query(
            "UPDATE concepts SET embedding = NULL WHERE session_id = ? AND EXISTS (\
             SELECT 1 FROM sessions WHERE session_id = ? \
             AND embedding_kind IS NULL AND embedding_dim IS NULL)",
        )
        .bind(&session.0)
        .bind(&session.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("quarantine legacy embeddings: {m}")))?;
    }
    let res = sqlx::query(
        "UPDATE sessions SET embedding_kind = ?, embedding_model = ?, embedding_dim = ? \
         WHERE session_id = ?",
    )
    .bind(embedding.map(|e| e.kind.as_str()))
    .bind(embedding.and_then(|e| e.model.as_deref()))
    .bind(dim)
    .bind(&session.0)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("set embedding: {m}")))?;
    if res.rows_affected() == 0 {
        return Err(StoreError::NotFound(format!(
            "sessions row for {session} while setting embedding"
        )));
    }
    Ok(())
}

/// Shared by the `CanonizationTransition` mutation and `record_canonization`:
/// append the event row — the demo's on-screen artifact — then update the
/// concept's status/blast_radius (NotFound if absent, like MemoryStore).
///
/// **F12 — the audit row is the idempotency key.** The evaluator dual-writes
/// (`record_canonization` immediately, the same transition again when the
/// write-behind log flushes), and the two are not ordered against each other:
/// a lagging flush of hop 1 landing after hop 2's immediate write would
/// otherwise *regress* the durable status, and a crash before hop 2's own
/// flush would leave the reload showing a status the audit already moved past
/// — after which the evaluator re-promotes under a fresh event id and the same
/// hop appears twice on screen. So the INSERT goes first: if its
/// `ON CONFLICT (id) DO NOTHING` fires, this transition's effect is already in
/// the row and the UPDATE is skipped. Both statements share the caller's
/// transaction, so the ordering swap costs nothing on the first write.
///
/// **R2-1 — what makes "already in the row" true.** Skipping the UPDATE is
/// only sound while nothing else writes those three columns. `upsert_concept`
/// used to, from a possibly stale `Mutation::UpsertNode` snapshot, so a batch
/// shaped `[UpsertNode(stale), CanonizationTransition(already recorded)]` left
/// the row regressed *and* the repair skipped. It no longer does — see
/// `upsert_concept` and `Mutation::UpsertNode`.
async fn apply_canonization_transition(
    tx: &mut sqlx::SqliteConnection,
    event: &CanonizationEvent,
) -> Result<(), StoreError> {
    let to_status = enum_to_text(&event.to_status, "to_status")?;
    let from_status = enum_to_text(&event.from_status, "from_status")?;
    let appended = sqlx::query(
        "INSERT INTO canonization_events (\
             id, session_id, node_id, from_status, to_status, blast_radius, \
             last_demotion_time, occurred_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(event.id.0.to_string())
    .bind(&event.session_id.0)
    .bind(event.node_id.0.to_string())
    .bind(from_status)
    .bind(&to_status)
    .bind(event.blast_radius)
    .bind(event.last_demotion_time.map(ts_to_text))
    .bind(ts_to_text(event.occurred_at))
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("append canonization event: {m}")))?;
    if appended.rows_affected() == 0 {
        return Ok(());
    }

    // COH-3: last_demotion_time = COALESCE(?, last_demotion_time) — a demotion
    // event (Some) stamps the concept; non-demotion events (None) leave a
    // previously demoted value untouched (spec §10).
    let res = sqlx::query(
        "UPDATE concepts SET canonization_status = ?, blast_radius = ?, \
         last_demotion_time = COALESCE(?, last_demotion_time) \
         WHERE id = ? AND session_id = ?",
    )
    .bind(&to_status)
    .bind(event.blast_radius)
    .bind(event.last_demotion_time.map(ts_to_text))
    .bind(event.node_id.0.to_string())
    .bind(&event.session_id.0)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("apply canonization transition: {m}")))?;
    if res.rows_affected() == 0 {
        return Err(StoreError::NotFound(format!(
            "concept {} for canonization",
            event.node_id
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// load_session row readers
// ---------------------------------------------------------------------------

async fn load_interactions(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<Interaction>, StoreError> {
    let rows = sqlx::query(
        "SELECT id, session_id, agent_id, prompt_text, previous_id, created_at \
         FROM interactions WHERE session_id = ? ORDER BY created_at ASC, id ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load interactions", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.try_get(0).map_err(|e| db_err("load interactions", e))?;
        let sid: String = row.try_get(1).map_err(|e| db_err("load interactions", e))?;
        let agent: String = row.try_get(2).map_err(|e| db_err("load interactions", e))?;
        let prompt: Option<String> = row.try_get(3).map_err(|e| db_err("load interactions", e))?;
        let prev: Option<String> = row.try_get(4).map_err(|e| db_err("load interactions", e))?;
        let created: String = row.try_get(5).map_err(|e| db_err("load interactions", e))?;
        out.push(Interaction {
            id: node_id(&id, "interaction id")?,
            session_id: SessionId::from(sid),
            agent_id: crate::types::AgentId::new(agent),
            prompt_text: prompt,
            previous_id: prev.as_deref().map(node_id_str).transpose()?,
            created_at: text_to_ts(&created)?,
        });
    }
    Ok(out)
}

fn node_id_str(s: &str) -> Result<NodeId, StoreError> {
    node_id(s, "node id")
}

async fn load_concepts(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<Concept>, StoreError> {
    let rows = sqlx::query(
        "SELECT id, session_id, content, canonical_key, concept_type, origin_interaction, \
                origin_agent, created_at, access_count, last_accessed, gc_survived, \
                canonization_status, blast_radius, last_demotion_time, embedding, \
                chunk_group_id \
         FROM concepts WHERE session_id = ? ORDER BY id ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load concepts", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.try_get(0).map_err(|e| db_err("load concepts", e))?;
        let sid: String = row.try_get(1).map_err(|e| db_err("load concepts", e))?;
        let content: String = row.try_get(2).map_err(|e| db_err("load concepts", e))?;
        let key: String = row.try_get(3).map_err(|e| db_err("load concepts", e))?;
        let ctype: String = row.try_get(4).map_err(|e| db_err("load concepts", e))?;
        let origin: String = row.try_get(5).map_err(|e| db_err("load concepts", e))?;
        let agent: String = row.try_get(6).map_err(|e| db_err("load concepts", e))?;
        let created: String = row.try_get(7).map_err(|e| db_err("load concepts", e))?;
        let access_count: i32 = row.try_get(8).map_err(|e| db_err("load concepts", e))?;
        let last_accessed: Option<String> =
            row.try_get(9).map_err(|e| db_err("load concepts", e))?;
        let gc_survived: i32 = row.try_get(10).map_err(|e| db_err("load concepts", e))?;
        let status: String = row.try_get(11).map_err(|e| db_err("load concepts", e))?;
        let blast_radius: Option<i32> = row.try_get(12).map_err(|e| db_err("load concepts", e))?;
        let last_demotion: Option<String> =
            row.try_get(13).map_err(|e| db_err("load concepts", e))?;
        // CON-8: decode the BLOB back to the shared text form. A corrupt blob
        // (invalid UTF-8 / unparseable elements) is a backend error, not a panic.
        let embedding: Option<Vec<u8>> = row.try_get(14).map_err(|e| db_err("load concepts", e))?;
        let embedding = match embedding {
            Some(bytes) => {
                let text = std::str::from_utf8(&bytes).map_err(|e| {
                    StoreError::Backend(format!(
                        "concepts.embedding for {} is not valid UTF-8: {e}",
                        id
                    ))
                })?;
                Some(decode_vector(text)?)
            }
            None => None,
        };
        let chunk_group_id: Option<String> =
            row.try_get(15).map_err(|e| db_err("load concepts", e))?;
        out.push(Concept {
            id: node_id(&id, "concept id")?,
            session_id: SessionId::from(sid),
            content,
            canonical_key: key,
            concept_type: text_to_enum(&ctype, "concept_type")?,
            origin_interaction: node_id(&origin, "origin_interaction")?,
            origin_agent: crate::types::AgentId::new(agent),
            created_at: text_to_ts(&created)?,
            access_count,
            last_accessed: last_accessed.as_deref().map(text_to_ts).transpose()?,
            gc_survived,
            canonization_status: text_to_enum(&status, "canonization_status")?,
            blast_radius,
            last_demotion_time: last_demotion.as_deref().map(text_to_ts).transpose()?,
            embedding,
            chunk_group_id,
        });
    }
    Ok(out)
}

async fn load_edges(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<Edge>, StoreError> {
    let rows = sqlx::query(
        "SELECT id, session_id, source, target, edge_type, weight, reinforcements, \
                created_at, last_reinforced \
         FROM edges WHERE session_id = ? ORDER BY id ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load edges", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.try_get(0).map_err(|e| db_err("load edges", e))?;
        let sid: String = row.try_get(1).map_err(|e| db_err("load edges", e))?;
        let source: String = row.try_get(2).map_err(|e| db_err("load edges", e))?;
        let target: String = row.try_get(3).map_err(|e| db_err("load edges", e))?;
        let etype: String = row.try_get(4).map_err(|e| db_err("load edges", e))?;
        let weight: f64 = row.try_get(5).map_err(|e| db_err("load edges", e))?;
        let reinforcements: i32 = row.try_get(6).map_err(|e| db_err("load edges", e))?;
        let created: String = row.try_get(7).map_err(|e| db_err("load edges", e))?;
        let last_reinforced: String = row.try_get(8).map_err(|e| db_err("load edges", e))?;
        out.push(Edge {
            id: node_id(&id, "edge id")?,
            session_id: SessionId::from(sid),
            source: node_id(&source, "edge source")?,
            target: node_id(&target, "edge target")?,
            edge_type: text_to_enum(&etype, "edge_type")?,
            weight,
            reinforcements,
            created_at: text_to_ts(&created)?,
            last_reinforced: text_to_ts(&last_reinforced)?,
        });
    }
    Ok(out)
}

async fn load_synonyms(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<crate::types::Synonym>, StoreError> {
    let rows = sqlx::query(
        "SELECT session_id, source_key, canonical_key \
         FROM synonyms WHERE session_id = ? ORDER BY source_key ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load synonyms", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let sid: String = row.try_get(0).map_err(|e| db_err("load synonyms", e))?;
        let src: String = row.try_get(1).map_err(|e| db_err("load synonyms", e))?;
        let canon: String = row.try_get(2).map_err(|e| db_err("load synonyms", e))?;
        out.push(crate::types::Synonym {
            session_id: SessionId::from(sid),
            source_key: src,
            canonical_key: canon,
        });
    }
    Ok(out)
}

async fn load_reservations(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<crate::types::Reservation>, StoreError> {
    let rows = sqlx::query(
        "SELECT session_id, node_id, agent_id, expires_at \
         FROM reservations WHERE session_id = ?",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load reservations", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let sid: String = row.try_get(0).map_err(|e| db_err("load reservations", e))?;
        let node: String = row.try_get(1).map_err(|e| db_err("load reservations", e))?;
        let agent: String = row.try_get(2).map_err(|e| db_err("load reservations", e))?;
        let expires: String = row.try_get(3).map_err(|e| db_err("load reservations", e))?;
        out.push(crate::types::Reservation {
            session_id: SessionId::from(sid),
            node_id: node_id(&node, "reservation node")?,
            agent_id: crate::types::AgentId::new(agent),
            expires_at: text_to_ts(&expires)?,
        });
    }
    Ok(out)
}

async fn load_canonization_events(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<CanonizationEvent>, StoreError> {
    let rows = sqlx::query(
        "SELECT id, session_id, node_id, from_status, to_status, blast_radius, \
             last_demotion_time, occurred_at \
         FROM canonization_events WHERE session_id = ? ORDER BY occurred_at ASC, id ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load canonization events", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row
            .try_get(0)
            .map_err(|e| db_err("load canonization events", e))?;
        let sid: String = row
            .try_get(1)
            .map_err(|e| db_err("load canonization events", e))?;
        let node: String = row
            .try_get(2)
            .map_err(|e| db_err("load canonization events", e))?;
        let from: String = row
            .try_get(3)
            .map_err(|e| db_err("load canonization events", e))?;
        let to: String = row
            .try_get(4)
            .map_err(|e| db_err("load canonization events", e))?;
        let blast_radius: Option<i32> = row
            .try_get(5)
            .map_err(|e| db_err("load canonization events", e))?;
        let last_demotion: Option<String> = row
            .try_get(6)
            .map_err(|e| db_err("load canonization events", e))?;
        let occurred: String = row
            .try_get(7)
            .map_err(|e| db_err("load canonization events", e))?;
        out.push(CanonizationEvent {
            id: node_id(&id, "canonization event id")?,
            session_id: SessionId::from(sid),
            node_id: node_id(&node, "canonization node")?,
            from_status: text_to_enum(&from, "from_status")?,
            to_status: text_to_enum(&to, "to_status")?,
            blast_radius,
            last_demotion_time: last_demotion.as_deref().map(text_to_ts).transpose()?,
            occurred_at: text_to_ts(&occurred)?,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests — full offline conformance on sqlite::memory: (feature-gated; the
// module itself only compiles under `store-sqlite`, so these always are too).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::demote::demote;
    use crate::graph::derive::{derive, ParentOf};
    use crate::graph::reserve::reserve;
    use crate::store::load::{load_session, load_session_async};
    #[cfg(feature = "store-memory")]
    use crate::store::memory::MemoryStore;
    use crate::types::{AgentId, CanonizationStatus, ConceptType, EdgeType, Node as NodeKind};
    use chrono::TimeZone;

    fn test_store() -> SqliteStore {
        SqliteStore::connect("sqlite::memory:").unwrap()
    }

    fn plant_concept(
        sid: &SessionId,
        id: NodeId,
        origin: NodeId,
        content: &str,
        concept_type: ConceptType,
        ts: DateTime<Utc>,
    ) -> Mutation {
        Mutation::UpsertNode {
            node: NodeKind::Concept(Concept {
                id,
                session_id: sid.clone(),
                content: content.into(),
                canonical_key: content.to_lowercase(),
                concept_type,
                origin_interaction: origin,
                origin_agent: AgentId::from("a"),
                created_at: ts,
                access_count: 0,
                last_accessed: None,
                gc_survived: 0,
                canonization_status: CanonizationStatus::None,
                blast_radius: None,
                last_demotion_time: None,
                embedding: None,
                chunk_group_id: None,
            }),
        }
    }

    fn plant_interaction(
        sid: &SessionId,
        id: NodeId,
        prev: Option<NodeId>,
        ts: DateTime<Utc>,
    ) -> Mutation {
        Mutation::UpsertNode {
            node: NodeKind::Interaction(Interaction {
                id,
                session_id: sid.clone(),
                agent_id: AgentId::from("a"),
                prompt_text: Some("prompt".into()),
                previous_id: prev,
                created_at: ts,
            }),
        }
    }

    #[cfg(feature = "store-memory")]
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

    #[tokio::test]
    async fn init_schema_runs_twice_cleanly() {
        // Acceptance: init_schema twice on a fresh target (T3.1 idempotency).
        let store = test_store();
        store.init_schema().await.unwrap();
        store.init_schema().await.unwrap();
    }

    #[tokio::test]
    async fn load_missing_session_errors() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let err = store
            .load_session(&SessionId::from("nope"))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound(_)));
    }

    #[tokio::test]
    async fn vector_candidates_capability_error_and_no_dimensions() {
        let store = test_store();
        store.init_schema().await.unwrap();
        assert!(store.capabilities().is_empty());
        assert_eq!(store.vector_dimensions(), None);
        let err = store
            .vector_candidates(&SessionId::from("x"), &[0.0; 8], 5)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Capability(_)));
        assert!(matches!(
            store
                .vector_candidates(
                    &SessionId::from("x"),
                    &[],
                    crate::store::MAX_VECTOR_CANDIDATE_LIMIT + 1,
                )
                .await
                .unwrap_err(),
            StoreError::Invariant(_)
        ));
    }

    /// XP-8: `root_goal` survives flush → load through the **mutation path**.
    ///
    /// Before `Mutation::SetRootGoal` the goal reached a store only via the
    /// full-snapshot `seed` path, so a session reloaded from the write-behind log
    /// came back with no goal — drift detection silently stopped and GC's
    /// root-goal exclusion emptied. Covers the array shape (ALGO-6), a
    /// last-write-wins replacement, and the explicit clear.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn root_goal_roundtrips_flush_and_load() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let sid = SessionId::from("goal-roundtrip");
        let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        let goal = serde_json::json!(["launch the product", "ship the API"]);
        let batch = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, NodeId::new(), None, ts),
                Mutation::SetRootGoal {
                    session_id: sid.clone(),
                    goal: Some(goal.clone()),
                },
            ],
        };
        store.flush(&batch).await.unwrap();
        let loaded = store.load_session(&sid).await.unwrap();
        assert_eq!(
            loaded.root_goal.as_ref(),
            Some(&goal),
            "the array goal must survive flush→load (XP-8 / ALGO-6)"
        );

        // Last write wins, and a clear is durable (not "no change").
        let replace = MutationBatch {
            mutations: vec![Mutation::SetRootGoal {
                session_id: sid.clone(),
                goal: Some(serde_json::json!("only this one")),
            }],
        };
        store.flush(&replace).await.unwrap();
        assert_eq!(
            store.load_session(&sid).await.unwrap().root_goal,
            Some(serde_json::json!("only this one"))
        );
        let clear = MutationBatch {
            mutations: vec![Mutation::SetRootGoal {
                session_id: sid.clone(),
                goal: None,
            }],
        };
        store.flush(&clear).await.unwrap();
        assert_eq!(store.load_session(&sid).await.unwrap().root_goal, None);

        #[cfg(feature = "store-memory")]
        {
            let memory = MemoryStore::new();
            memory.flush(&batch).await.unwrap();
            let want = memory.load_session(&sid).await.unwrap();
            assert_eq!(
                want.root_goal.as_ref(),
                Some(&goal),
                "MemoryStore applies SetRootGoal identically"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn embedding_contract_roundtrips_flush_and_load() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let sid = SessionId::from("embedding-roundtrip");
        let contract = crate::types::EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-v1".into()),
            dim: 1024,
        };
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(
                        &sid,
                        NodeId::new(),
                        None,
                        Utc.timestamp_opt(1_752_000_000, 0).unwrap(),
                    ),
                    Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(contract.clone()),
                    },
                ],
            })
            .await
            .unwrap();

        let loaded = store.load_session(&sid).await.unwrap();
        assert_eq!(loaded.embedding, Some(contract.clone()));
        let reloaded = crate::graph::Graph::from_snapshot(loaded).unwrap();
        let incompatible = crate::types::EmbeddingContract {
            kind: "bedrock".into(),
            model: Some("amazon.titan-embed-text-v2:0".into()),
            dim: 1024,
        };
        assert!(reloaded
            .embedding()
            .unwrap()
            .ensure_compatible(&incompatible)
            .is_err());

        #[cfg(feature = "store-memory")]
        {
            let memory = MemoryStore::new();
            memory
                .flush(&MutationBatch {
                    mutations: vec![Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(contract.clone()),
                    }],
                })
                .await
                .unwrap();
            assert_eq!(
                memory.load_session(&sid).await.unwrap().embedding,
                Some(contract)
            );
        }
    }

    /// Acceptance (CON-8): `Concept.embedding` survives flush → load on SQLite
    /// (the column is now written and read), and the loaded snapshot deep-equals
    /// the MemoryStore oracle on the same batch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concept_embedding_roundtrips_flush_and_load() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let sid = SessionId::from("embed-roundtrip");
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        // Whole-second timestamps: the SQLite round-trip contract truncates to
        // milliseconds, so a fresh Utc::now() would break snapshot equality.
        let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        let emb = vec![0.25, -0.5, 1.0, 0.0];
        let batch = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, i1, None, ts),
                Mutation::UpsertNode {
                    node: NodeKind::Concept(Concept {
                        id: c1,
                        session_id: sid.clone(),
                        content: "embedded concept".into(),
                        canonical_key: "embedded concept".into(),
                        concept_type: ConceptType::Entity,
                        origin_interaction: i1,
                        origin_agent: AgentId::from("a"),
                        created_at: ts,
                        access_count: 0,
                        last_accessed: None,
                        gc_survived: 0,
                        canonization_status: CanonizationStatus::None,
                        blast_radius: None,
                        last_demotion_time: None,
                        embedding: Some(emb.clone()),
                        chunk_group_id: None,
                    }),
                },
            ],
        };
        store.flush(&batch).await.unwrap();
        let loaded = store.load_session(&sid).await.unwrap();
        assert_eq!(loaded.concepts.len(), 1);
        assert_eq!(
            loaded.concepts[0].embedding.as_deref(),
            Some(emb.as_slice()),
            "Concept.embedding must survive flush→load (CON-8)"
        );
        #[cfg(feature = "store-memory")]
        {
            let memory = MemoryStore::new();
            memory.flush(&batch).await.unwrap();
            let want = memory.load_session(&sid).await.unwrap();
            assert_eq!(
                loaded, want,
                "sqlite snapshot deep-equals the MemoryStore oracle (embedding included)"
            );
        }
    }

    /// Upgrade regression: pre-contract durable rows may contain vectors. They
    /// remain loadable, but startup quarantines the unknown vectors rather than
    /// guessing a model from their width.
    #[tokio::test]
    async fn legacy_vectors_without_contract_are_quarantined_on_materialization() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let sid = SessionId::from("legacy-vector-upgrade");
        let interaction = NodeId::new();
        let concept = NodeId::new();
        let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        let mut concept_mutation = plant_concept(
            &sid,
            concept,
            interaction,
            "legacy embedded concept",
            ConceptType::Entity,
            ts,
        );
        let Mutation::UpsertNode {
            node: NodeKind::Concept(ref mut value),
        } = concept_mutation
        else {
            unreachable!()
        };
        value.embedding = Some(vec![0.1, 0.2, 0.3]);
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, interaction, None, ts),
                    concept_mutation,
                    Mutation::UpsertEdge {
                        edge: Edge {
                            id: NodeId::new(),
                            session_id: sid.clone(),
                            source: interaction,
                            target: concept,
                            edge_type: EdgeType::Derives,
                            weight: 1.0,
                            reinforcements: 1,
                            created_at: ts,
                            last_reinforced: ts,
                        },
                    },
                ],
            })
            .await
            .unwrap();

        let raw = store.load_session(&sid).await.unwrap();
        assert!(raw.embedding.is_none());
        assert!(raw.concepts[0].embedding.is_some());
        let loaded = load_session_async(&store, &sid).await.unwrap();
        assert!(loaded.graph.embedding().is_none());
        assert!(loaded.graph.concepts().all(|c| c.embedding.is_none()));
        assert!(loaded.graph.snapshot().concepts[0].embedding.is_none());

        store
            .flush(&MutationBatch {
                mutations: vec![Mutation::SetEmbedding {
                    session_id: sid.clone(),
                    embedding: Some(EmbeddingContract {
                        kind: "fixture".into(),
                        model: Some("fixture-v1".into()),
                        dim: 3,
                    }),
                }],
            })
            .await
            .unwrap();
        let migrated = store.load_session(&sid).await.unwrap();
        assert!(migrated.concepts[0].embedding.is_none());
        assert!(migrated.embedding.is_some());
    }

    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn oversized_seed_embedding_dimension_fails_before_transaction() {
        let store = test_store();
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
    }

    /// Acceptance (STORE-1, offline gate): the full-snapshot `seed` path persists
    /// `GraphSnapshot.embedding` into `sessions.embedding_{kind,model,dim}`, and a
    /// later flush (which only ensures the session row) does not clobber it — the
    /// SQLite twin of the live cockroach conformance check.
    #[cfg(feature = "fixtures")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seed_load_preserves_embedding_contract() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let sid = SessionId::from("seed-embed-contract");
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        let contract = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("BAAI/bge-m3".into()),
            dim: 1024,
        };
        store
            .seed(&GraphSnapshot {
                session_id: sid.clone(),
                interactions: vec![Interaction {
                    id: i1,
                    session_id: sid.clone(),
                    agent_id: AgentId::from("a"),
                    prompt_text: Some("seed".into()),
                    previous_id: None,
                    created_at: ts,
                }],
                concepts: vec![Concept {
                    id: c1,
                    session_id: sid.clone(),
                    content: "seeded".into(),
                    canonical_key: "seeded".into(),
                    concept_type: ConceptType::Entity,
                    origin_interaction: i1,
                    origin_agent: AgentId::from("a"),
                    created_at: ts,
                    access_count: 0,
                    last_accessed: None,
                    gc_survived: 0,
                    canonization_status: CanonizationStatus::None,
                    blast_radius: None,
                    last_demotion_time: None,
                    embedding: Some(vec![0.1, 0.2, 0.3]),
                    chunk_group_id: None,
                }],
                embedding: Some(contract.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        let loaded = store.load_session(&sid).await.unwrap();
        assert_eq!(
            loaded.embedding.as_ref(),
            Some(&contract),
            "seed must persist the embedding contract (STORE-1)"
        );
        assert_eq!(
            loaded.concepts[0].embedding.as_deref(),
            Some([0.1f32, 0.2, 0.3].as_slice()),
            "seeded concept embedding round-trips through the full-snapshot path"
        );
        // A later flush (session-row ensure only) must not clobber the contract.
        store
            .flush(&MutationBatch {
                mutations: vec![plant_interaction(
                    &sid,
                    NodeId::new(),
                    None,
                    Utc.timestamp_opt(1_752_003_600, 0).unwrap(),
                )],
            })
            .await
            .unwrap();
        let loaded = store.load_session(&sid).await.unwrap();
        assert_eq!(
            loaded.embedding.as_ref(),
            Some(&contract),
            "flush must not clobber a seeded embedding contract"
        );
        assert_eq!(loaded.interactions.len(), 2);
    }

    /// Acceptance: mutations-batch.json flush + load round-trip — the SQLite
    /// snapshot deep-equals the MemoryStore oracle on the same batch.
    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn mutations_batch_roundtrip_matches_memory() {
        let batch: MutationBatch =
            serde_json::from_str(include_str!("../../fixtures/mutations-batch.json")).unwrap();

        let sqlite = test_store();
        sqlite.init_schema().await.unwrap();
        sqlite.flush(&batch).await.unwrap();

        let memory = MemoryStore::new();
        memory.flush(&batch).await.unwrap();

        let sid = SessionId::from("session-mutations");
        let got = sqlite.load_session(&sid).await.unwrap();
        let want = memory.load_session(&sid).await.unwrap();
        assert_eq!(got, want);

        // Spot-check the expected shape (fixture: delete_edge 70051 removes
        // the Temporal edge; delete_node 7003 also removes the incident
        // Dependency edge 70053 → target 7003, matching MemoryStore; only the
        // Derives edge 70052 survives).
        assert_eq!(got.interactions.len(), 2);
        assert_eq!(got.concepts.len(), 1, "deleted concept must be gone");
        assert_eq!(got.concepts[0].content, "kept concept");
        assert_eq!(got.edges.len(), 1, "deleted edge must be gone");
        assert_eq!(got.edges[0].edge_type, EdgeType::Derives);
        assert_eq!(
            got.canonization_events[0].to_status,
            CanonizationStatus::Candidate
        );
    }

    /// Acceptance: flush -> load round-trip deep-equals the graph (T3.5 shape
    /// reused against SqliteStore). The graph is built through the real write
    /// path (derive / demote / transition), drained, flushed, and loaded back
    /// via `load_session`; the loaded session must deep-equal the pre-flush
    /// snapshot (minus RAM-local synonyms/reservations — S5), including the
    /// demoted observations' `chunk_group_id` (T5.2 contract).
    // Multi-thread flavor: load_session runs the store future on a worker
    // thread with its own current-thread runtime (see load.rs). sqlx returns
    // pool connections via a spawned task; a current-thread runtime that is
    // blocked joining that worker never runs the return task, so a cross-
    // runtime acquire would time out. Multi-thread keeps other workers polling.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_load_roundtrip_deep_equals_graph() {
        let store = test_store();
        store.init_schema().await.unwrap();

        let sid = SessionId::from("roundtrip-sqlite");
        let mut g = crate::graph::Graph::new(sid.clone());

        let ts = |minutes: i64| {
            let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
            base + chrono::Duration::minutes(minutes)
        };
        let i1 = NodeId::new();
        g.insert_interaction(Interaction {
            id: i1,
            session_id: sid.clone(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("prompt 1".into()),
            previous_id: None,
            created_at: ts(0),
        })
        .unwrap();
        let i2 = NodeId::new();
        g.insert_interaction(Interaction {
            id: i2,
            session_id: sid.clone(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("prompt 2".into()),
            previous_id: Some(i1),
            created_at: ts(5),
        })
        .unwrap();
        derive(
            &mut g,
            i2,
            &AgentId::from("agent-a"),
            &[
                ("user schema", ConceptType::Entity),
                ("api layer", ConceptType::Logic),
            ],
            &ParentOf::from_pairs(&[("user schema", "api layer")]),
            10,
        )
        .unwrap();
        let observations = demote(
            &mut g,
            i2,
            &AgentId::from("agent-a"),
            "Drift note. Second drift note.",
            "chunk-1",
        )
        .unwrap();
        assert_eq!(observations.len(), 2);
        let user_schema_id = g
            .concepts()
            .find(|c| c.content == "user schema")
            .expect("derive created it")
            .id;
        g.apply_canonization_transition(CanonizationEvent {
            id: NodeId::new(),
            session_id: sid.clone(),
            node_id: user_schema_id,
            from_status: CanonizationStatus::None,
            to_status: CanonizationStatus::Candidate,
            blast_radius: Some(2),
            last_demotion_time: None,
            occurred_at: ts(12),
        })
        .unwrap();
        // RAM-local metadata that has no Mutation kind (S5).
        g.declare_synonym("us", "user schema");
        reserve(
            &mut g,
            user_schema_id,
            &AgentId::from("agent-b"),
            Duration::from_secs(3600),
            ts(10),
        )
        .unwrap();

        g.assert_invariants().unwrap();
        let mut expected = g.snapshot();
        expected.synonyms.clear();
        expected.reservations.clear();
        let batch = g.drain_log();
        assert!(!batch.is_empty());

        store.flush(&batch).await.unwrap();
        let loaded = load_session(&store, &sid).unwrap();

        assert_eq!(loaded.graph.snapshot(), expected);
        assert_eq!(loaded.graph.log_len(), 0, "load must not seed mutations");
        assert_eq!(loaded.graph.epoch(), 0);
        loaded.graph.assert_invariants().unwrap();
        assert_eq!(loaded.graph.synonyms().count(), 0);
        assert_eq!(loaded.graph.reservations().len(), 0);
        // T5.2 contract: the demote chunk id SURVIVES the flush→load round-trip
        // (the P3 wave 2 schema remediation added concepts.chunk_group_id).
        for c in loaded.graph.concepts() {
            if c.concept_type == ConceptType::Observation {
                assert_eq!(
                    c.chunk_group_id.as_deref(),
                    Some("chunk-1"),
                    "demoted observation must keep its chunk_group_id across flush→load"
                );
            } else {
                assert_eq!(
                    c.chunk_group_id, None,
                    "non-Observation concepts carry no chunk group"
                );
            }
        }
        // Index rebuilt from the snapshot agrees with a reference and finds
        // both observations.
        let reference = crate::graph::index::InvertedIndex::from_snapshot(&expected);
        for q in ["user schema", "api layer", "drift"] {
            assert_eq!(loaded.index.search(q, 10), reference.search(q, 10));
        }
        let drift: Vec<NodeId> = loaded
            .index
            .search("drift", 10)
            .into_iter()
            .map(|s| s.item)
            .collect();
        assert_eq!(drift.len(), 2, "both observations indexed");
    }

    /// Migration path for pre-existing databases (P3 wave 2): a database built
    /// from the T3.1 DDL (no chunk_group_id / embedding columns) converges on
    /// `init_schema` — the guarded ALTERs add the columns — a second
    /// `init_schema` is a no-op, and chunk_group_id then round-trips. The
    /// regular tests always start from a fresh schema, so this is the only
    /// place the ALTER convergence is exercised. Multi-thread flavor:
    /// `load_session` runs on a worker thread (see module doc pool quirk).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn init_schema_converges_preexisting_database() {
        let store = test_store();
        let old = r#"
            CREATE TABLE sessions (
                session_id TEXT PRIMARY KEY,
                root_goal TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                closed_at TEXT
            );
            CREATE TABLE concepts (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(session_id),
                content TEXT NOT NULL,
                canonical_key TEXT NOT NULL,
                concept_type TEXT NOT NULL,
                origin_interaction TEXT NOT NULL REFERENCES interactions(id),
                origin_agent TEXT NOT NULL,
                created_at TEXT NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0,
                last_accessed TEXT,
                gc_survived INTEGER NOT NULL DEFAULT 0,
                canonization_status TEXT NOT NULL DEFAULT 'None',
                blast_radius INTEGER,
                last_demotion_time TEXT,
                embedding BLOB
            );
            CREATE TABLE canonization_events (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                node_id TEXT NOT NULL,
                from_status TEXT NOT NULL,
                to_status TEXT NOT NULL,
                blast_radius INTEGER,
                occurred_at TEXT NOT NULL
            );
        "#;
        sqlx::query(old).execute(store.pool()).await.unwrap();

        // Convergence + idempotency: columns appear, second init is a no-op.
        store.init_schema().await.unwrap();
        store.init_schema().await.unwrap();
        let concept_cols: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('concepts')")
                .fetch_all(store.pool())
                .await
                .unwrap();
        assert!(
            concept_cols.iter().any(|c| c == "chunk_group_id"),
            "chunk_group_id must be added to a pre-existing concepts table"
        );
        let session_cols: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('sessions')")
                .fetch_all(store.pool())
                .await
                .unwrap();
        for want in ["embedding_kind", "embedding_model", "embedding_dim"] {
            assert!(
                session_cols.iter().any(|c| c == want),
                "{want} must be added to a pre-existing sessions table"
            );
        }
        let ce_cols: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('canonization_events')")
                .fetch_all(store.pool())
                .await
                .unwrap();
        assert!(
            ce_cols.iter().any(|c| c == "last_demotion_time"),
            "last_demotion_time must be added to a pre-existing canonization_events table"
        );

        // The converged column actually round-trips a demoted observation.
        let sid = SessionId::from("legacy-session");
        let i1 = NodeId::new();
        let o1 = NodeId::new();
        let ts = Utc::now();
        let batch = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, i1, None, ts),
                Mutation::UpsertNode {
                    node: NodeKind::Concept(Concept {
                        id: o1,
                        session_id: sid.clone(),
                        content: "legacy drift note".into(),
                        canonical_key: "legacy drift note".into(),
                        concept_type: ConceptType::Observation,
                        origin_interaction: i1,
                        origin_agent: AgentId::from("a"),
                        created_at: ts,
                        access_count: 0,
                        last_accessed: None,
                        gc_survived: 0,
                        canonization_status: CanonizationStatus::None,
                        blast_radius: None,
                        last_demotion_time: None,
                        embedding: None,
                        chunk_group_id: Some("legacy-chunk".into()),
                    }),
                },
                // The rebuilt graph requires the Derives edge (invariant),
                // exactly as demote would create it.
                Mutation::UpsertEdge {
                    edge: crate::types::Edge {
                        id: NodeId::new(),
                        session_id: sid.clone(),
                        source: i1,
                        target: o1,
                        edge_type: EdgeType::Derives,
                        weight: 1.0,
                        reinforcements: 1,
                        created_at: ts,
                        last_reinforced: ts,
                    },
                },
            ],
        };
        store.flush(&batch).await.unwrap();
        let loaded = load_session(&store, &sid).unwrap();
        let obs = loaded
            .graph
            .concepts()
            .find(|c| c.concept_type == ConceptType::Observation)
            .expect("observation loaded from converged database");
        assert_eq!(obs.chunk_group_id.as_deref(), Some("legacy-chunk"));
    }

    #[cfg(feature = "store-memory")]
    #[tokio::test]
    async fn keyword_candidates_match_memory_and_guard_inputs() {
        let sqlite = test_store();
        sqlite.init_schema().await.unwrap();
        let memory = MemoryStore::new();

        let sid = SessionId::from("kw");
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        let c2 = NodeId::new();
        let c3 = NodeId::new();
        let ts = Utc::now();
        let batch = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, i1, None, ts),
                plant_concept(&sid, c1, i1, "user schema design", ConceptType::Entity, ts),
                plant_concept(&sid, c2, i1, "API rate limits", ConceptType::Entity, ts),
                // Mixed-case row (cockroach R1 bug class): a raw `contains`
                // on row strings would score this 0.0 for "register"; the SQL
                // predicate lowercases the column, so it must score like
                // MemoryStore's Rust-side lowercase.
                plant_concept(&sid, c3, i1, "Register User", ConceptType::Entity, ts),
            ],
        };
        sqlite.flush(&batch).await.unwrap();
        memory.flush(&batch).await.unwrap();

        for tokens in [
            vec!["schema".to_string()],
            vec!["user".to_string(), "schema".to_string()],
            vec!["rate".to_string()],
            vec!["api".to_string()],
            vec!["nope".to_string()],
            vec!["  USER  ".to_string()],
            vec!["register".to_string()],
        ] {
            let got = sqlite.keyword_candidates(&sid, &tokens, 10).await.unwrap();
            let want = memory.keyword_candidates(&sid, &tokens, 10).await.unwrap();
            assert_eq!(got, want, "tokens {tokens:?}");
        }

        // Explicit mixed-case lock: "Register User" scores 1.0 for "register"
        // and ranks exactly like MemoryStore.
        let got = sqlite
            .keyword_candidates(&sid, &["register".into()], 10)
            .await
            .unwrap();
        let want = memory
            .keyword_candidates(&sid, &["register".into()], 10)
            .await
            .unwrap();
        assert_eq!(got, want);
        assert_eq!(got.len(), 1, "only the mixed-case concept matches");
        assert_eq!(got[0].item, c3);
        assert_eq!(got[0].score, 1.0, "mixed-case content must score, not 0.0");

        // Empty / whitespace tokens match nothing; limit 0 matches nothing.
        assert!(sqlite
            .keyword_candidates(&sid, &["".into(), "  ".into()], 5)
            .await
            .unwrap()
            .is_empty());
        assert!(sqlite
            .keyword_candidates(&sid, &["schema".into()], 0)
            .await
            .unwrap()
            .is_empty());

        // Missing session errors (MemoryStore parity).
        let err = sqlite
            .keyword_candidates(&SessionId::from("ghost"), &["schema".into()], 5)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound(_)));
    }

    /// Snapshot -> mutation batch (nodes then edges; §2.4 order). The
    /// fixtures carry no canonization events; synonyms and reservations are
    /// RAM-local (S5) and never part of the structural queries.
    #[cfg(feature = "fixtures")]
    fn snapshot_to_batch(snap: &GraphSnapshot) -> MutationBatch {
        let mut batch = MutationBatch::new();
        for i in &snap.interactions {
            batch.push(Mutation::UpsertNode {
                node: NodeKind::Interaction(i.clone()),
            });
        }
        for c in &snap.concepts {
            batch.push(Mutation::UpsertNode {
                node: NodeKind::Concept(c.clone()),
            });
        }
        for e in &snap.edges {
            batch.push(Mutation::UpsertEdge { edge: e.clone() });
        }
        batch
    }

    /// T3.6 three-way agreement matrix: EVERY node (concepts + interactions)
    /// × min-age {0, 3600s} × both queries, each answer asserted EXACTLY
    /// equal to MemoryStore's naive computation on the same snapshot.
    /// Returns the number of equality assertions performed.
    #[cfg(feature = "fixtures")]
    async fn assert_structural_agreement_matrix(
        store: &SqliteStore,
        memory: &MemoryStore,
        sid: &SessionId,
        snap: &GraphSnapshot,
    ) -> usize {
        let node_ids: Vec<NodeId> = snap
            .concepts
            .iter()
            .map(|c| c.id)
            .chain(snap.interactions.iter().map(|i| i.id))
            .collect();
        let ages = [Duration::from_secs(0), Duration::from_secs(3600)];
        let mut assertions = 0;
        for node in &node_ids {
            for age in ages {
                let br = store
                    .blast_radius(sid, *node, age, Utc::now())
                    .await
                    .unwrap();
                let br_want = memory
                    .blast_radius(sid, *node, age, Utc::now())
                    .await
                    .unwrap();
                assert_eq!(br, br_want, "blast_radius {node} age {age:?}");

                let span = store
                    .interaction_span(sid, *node, age, Utc::now())
                    .await
                    .unwrap();
                let span_want = memory
                    .interaction_span(sid, *node, age, Utc::now())
                    .await
                    .unwrap();
                assert_eq!(span, span_want, "interaction_span {node} age {age:?}");
                assertions += 2;
            }
        }
        assertions
    }

    /// Acceptance (T3.6): the three-way agreement matrix on BOTH fixture
    /// graphs — `session-rest-api` (the Canonical hub 1001, the Venerable
    /// 1012, the D1–D8 orphans C1013–C1020, the P1/P2 peers C1021/C1022 and
    /// C1002–C1007, 22 concepts) and `session-drift` (two interaction chains,
    /// 9 concepts) — flushed into the store, every node × min-age {0, 3600s},
    /// blast_radius + interaction_span (distinct AND coverage) exactly equal
    /// to MemoryStore's answers on the same snapshot.
    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn structural_queries_agree_with_memory_on_both_fixtures() {
        let mut total_assertions = 0;
        for fixture in ["session-rest-api", "session-drift"] {
            let snap: GraphSnapshot = crate::fixtures::load_snapshot(fixture).unwrap();
            let sid = snap.session_id.clone();

            let batch = snapshot_to_batch(&snap);
            let sqlite = test_store();
            sqlite.init_schema().await.unwrap();
            sqlite.flush(&batch).await.unwrap();
            let memory = MemoryStore::new();
            memory.flush(&batch).await.unwrap();

            total_assertions +=
                assert_structural_agreement_matrix(&sqlite, &memory, &sid, &snap).await;

            // Deterministic sanity anchors on rest-api (independent of the
            // oracle): eight concepts depend on the Canonical hub 1001
            // exclusively; span = 6 distinct interactions over 25 of 55
            // minutes.
            if fixture == "session-rest-api" {
                let hub: NodeId = snap
                    .concepts
                    .iter()
                    .find(|c| c.id.0.to_string().ends_with("001001"))
                    .unwrap()
                    .id;
                assert_eq!(
                    sqlite
                        .blast_radius(&sid, hub, Duration::from_secs(0), Utc::now())
                        .await
                        .unwrap(),
                    8
                );
                let span = sqlite
                    .interaction_span(&sid, hub, Duration::from_secs(0), Utc::now())
                    .await
                    .unwrap();
                assert_eq!(span.distinct, 6);
                assert!((span.coverage - 25.0 / 55.0).abs() < 1e-9, "{span:?}");
            }
        }
        // Matrix dimensions: rest-api 34 nodes (22 concepts + 12
        // interactions), drift 11 nodes (9 + 2); 2 ages; 2 queries each ->
        // 45 nodes × 2 × 2 = 180 equality assertions.
        assert_eq!(total_assertions, 180, "matrix dimensions drifted");
    }

    /// §4.1 errata probe (T3.6): mirror of MemoryStore's
    /// `blast_radius_ignores_provenance_derives_edges` against the SQL
    /// adapter. §5.7 requires every concept to carry a `Derives` edge
    /// (interaction → concept); if blast_radius counted that inbound edge as
    /// "another source", every concept would look non-orphaned and Stage-3
    /// blast radius would collapse to ~0. The adapter must ignore provenance
    /// (`Derives`/`Temporal`) edges exactly like MemoryStore — never
    /// un-orphaning a concept through them.
    #[cfg(feature = "store-memory")]
    #[tokio::test]
    async fn blast_radius_errata_derives_must_not_un_orphan() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let sid = SessionId::from("errata-derives");
        let ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
        let i1 = NodeId::new();
        let pillar = NodeId::new();
        let orphan = NodeId::new();
        let alone = NodeId::new();
        let batch = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, i1, None, ts),
                plant_concept(&sid, pillar, i1, "pillar", ConceptType::Entity, ts),
                plant_concept(&sid, orphan, i1, "orphan", ConceptType::Entity, ts),
                plant_concept(&sid, alone, i1, "alone", ConceptType::Entity, ts),
                // pillar -> orphan (Dependency): the only structural inbound.
                plant_edge(&sid, pillar, orphan, EdgeType::Dependency, ts),
                // orphan ALSO has the mandatory §5.7 Derives from its origin
                // interaction — counting it would un-orphan orphan.
                plant_edge(&sid, i1, orphan, EdgeType::Derives, ts),
                // alone has ONLY the Derives provenance: never an orphan of
                // anyone (no structural inbound edge exists at all).
                plant_edge(&sid, i1, alone, EdgeType::Derives, ts),
            ],
        };
        store.flush(&batch).await.unwrap();
        let memory = MemoryStore::new();
        memory.flush(&batch).await.unwrap();

        for min_age in [Duration::from_secs(0), Duration::from_secs(3600)] {
            let want = memory
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
                "SQLite must ignore provenance Derives exactly like MemoryStore (min_age {min_age:?})"
            );
        }
    }

    /// R2-3: `blast_radius` must be session-scoped on the **source** side of
    /// both structural subqueries, exactly as Cockroach's `BLAST_RADIUS_SQL`
    /// is and as MemoryStore is by construction (it walks one session's
    /// snapshot, and its `concept_ids` set holds that session's concepts
    /// only).
    ///
    /// `hub -> dep` in session `here` makes `dep` an exclusive dependent, so
    /// blast is 1. A second structural edge into `dep` whose **source concept
    /// lives in another session** must not un-orphan it: MemoryStore skips
    /// that source (not in `concept_ids`), and SQLite used to join `concepts`
    /// with no session predicate, satisfy the `NOT EXISTS` arm, and answer 0
    /// — under-counting blast, which suppresses Stage-3 promotions and
    /// mis-ranks budget demotion. The session-local answer is the contract on
    /// every backend.
    #[cfg(feature = "store-memory")]
    #[tokio::test]
    async fn blast_radius_ignores_cross_session_sources_like_memory() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let here = SessionId::from("here");
        let there = SessionId::from("there");
        let ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
        let i1 = NodeId::new();
        let i2 = NodeId::new();
        let hub = NodeId::new();
        let dep = NodeId::new();
        let foreign = NodeId::new();

        let local = MutationBatch {
            mutations: vec![
                plant_interaction(&here, i1, None, ts),
                plant_concept(&here, hub, i1, "hub", ConceptType::Entity, ts),
                plant_concept(&here, dep, i1, "dep", ConceptType::Entity, ts),
                plant_edge(&here, hub, dep, EdgeType::Dependency, ts),
                // An edge recorded in `here` whose source concept belongs to
                // `there` — the schema permits it (edges carry no FK) and
                // `GraphStore::flush` is public.
                plant_edge(&here, foreign, dep, EdgeType::Dependency, ts),
            ],
        };
        let elsewhere = MutationBatch {
            mutations: vec![
                plant_interaction(&there, i2, None, ts),
                plant_concept(&there, foreign, i2, "foreign", ConceptType::Entity, ts),
            ],
        };
        store.flush(&local).await.unwrap();
        store.flush(&elsewhere).await.unwrap();
        let memory = MemoryStore::new();
        memory.flush(&local).await.unwrap();
        memory.flush(&elsewhere).await.unwrap();

        let want = memory
            .blast_radius(&here, hub, Duration::from_secs(0), Utc::now())
            .await
            .unwrap();
        assert_eq!(
            want, 1,
            "oracle sanity: a foreign-session source cannot un-orphan `dep`"
        );
        let got = store
            .blast_radius(&here, hub, Duration::from_secs(0), Utc::now())
            .await
            .unwrap();
        assert_eq!(
            got, want,
            "SQLite must scope the structural sources to the session, like \
             MemoryStore and Cockroach"
        );
    }

    /// Edge-age interaction (T3.6 matrix; round-1 review F1 remediation): an
    /// AGED inbound structural edge vs a freshly-created one. At min-age 0 the
    /// fresh edge counts and un-orphans the target; at 3600s it is filtered
    /// out and the orphan still counts. Both cutoffs must agree with
    /// MemoryStore on every node.
    ///
    /// Review F1: the span's TWO timestamp gates are discriminated
    /// behaviorally, not just textually —
    /// * **e-gate:** the fresh edge's source carries a DISTINCT origin
    ///   interaction (`i2`), so the span set genuinely shrinks at min_age =
    ///   3600s (aged edge included, fresh edge excluded): `span(orphan).distinct`
    ///   is 2 at min-age 0 and 1 at 1h. Dropping `e.created_at <= ?` would keep
    ///   `i2` in the span and fail the anchor (before the fix every origin was
    ///   `i1`, so the span was identical with or without either gate);
    /// * **i-gate probe:** an AGED edge (`probe_src -> probe_victim`) whose
    ///   origin interaction `i3` is FRESH (created after the 1h cutoff) must be
    ///   excluded from the span — `span(probe_victim).distinct` is 1 at min-age
    ///   0 and 0 at 1h. Dropping `i.created_at <= ?` would keep `i3` in the span.
    ///
    /// `blast_radius` is origin-agnostic by contrast: the aged probe edge counts
    /// at both ages even though its origin is fresh (the i-gate is span-only,
    /// matching MemoryStore).
    #[cfg(feature = "store-memory")]
    #[tokio::test]
    async fn structural_queries_aged_vs_fresh_edge_agree_with_memory() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let sid = SessionId::from("aged-vs-fresh");
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
                plant_interaction(&sid, i1, None, old_ts),
                plant_interaction(&sid, i2, None, old_ts),
                plant_interaction(&sid, i3, None, now),
                plant_concept(&sid, pillar, i1, "pillar", ConceptType::Entity, old_ts),
                plant_concept(&sid, orphan, i1, "orphan", ConceptType::Entity, old_ts),
                plant_concept(&sid, other, i2, "other", ConceptType::Entity, old_ts),
                plant_concept(
                    &sid,
                    probe_src,
                    i3,
                    "probe-src",
                    ConceptType::Entity,
                    old_ts,
                ),
                plant_concept(
                    &sid,
                    probe_victim,
                    i1,
                    "probe-victim",
                    ConceptType::Entity,
                    old_ts,
                ),
                plant_edge(&sid, pillar, orphan, EdgeType::Dependency, old_ts),
                plant_edge(&sid, probe_src, probe_victim, EdgeType::Dependency, old_ts),
            ],
        };
        store.flush(&base).await.unwrap();
        let memory = MemoryStore::new();
        memory.flush(&base).await.unwrap();
        // Then a genuinely FRESH other -> orphan dependency (created now).
        let fresh = MutationBatch {
            mutations: vec![plant_edge(&sid, other, orphan, EdgeType::Dependency, now)],
        };
        store.flush(&fresh).await.unwrap();
        memory.flush(&fresh).await.unwrap();

        let one_hour = Duration::from_secs(3600);
        for node in [pillar, orphan, other, probe_src, probe_victim, i1, i2, i3] {
            for min_age in [Duration::from_secs(0), one_hour] {
                let br = store
                    .blast_radius(&sid, node, min_age, Utc::now())
                    .await
                    .unwrap();
                let br_want = memory
                    .blast_radius(&sid, node, min_age, Utc::now())
                    .await
                    .unwrap();
                assert_eq!(br, br_want, "blast_radius {node} age {min_age:?}");
                let span = store
                    .interaction_span(&sid, node, min_age, Utc::now())
                    .await
                    .unwrap();
                let span_want = memory
                    .interaction_span(&sid, node, min_age, Utc::now())
                    .await
                    .unwrap();
                assert_eq!(span, span_want, "interaction_span {node} age {min_age:?}");
            }
        }
        // e-gate discrimination on the SPAN (independent of the oracle):
        // `other`'s DISTINCT origin i2 counts at min-age 0 and must vanish at
        // 1h when the fresh edge is filtered.
        assert_eq!(
            store
                .interaction_span(&sid, orphan, Duration::from_secs(0), Utc::now())
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
                .interaction_span(&sid, probe_victim, Duration::from_secs(0), Utc::now())
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
        // The age filter is doing real work for blast_radius (independent of
        // the oracle): with min-age 0 the fresh edge un-orphans; with min-age
        // 1h it is filtered and the orphan still counts.
        assert_eq!(
            store
                .blast_radius(&sid, pillar, Duration::from_secs(0), Utc::now())
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

    /// T3.6 round-1 review F1: text-level lock that the span SQL gates on BOTH
    /// timestamps — the edge's AND the origin interaction's (spec §4.1 second
    /// errata). Mirror of Cockroach's
    /// `structural_query_placeholder_order_and_counts`: a future narrowing that
    /// drops either clause fails here even while the fixtures keep every origin
    /// older than the cutoff (the behavioral discrimination is covered by
    /// [`structural_queries_aged_vs_fresh_edge_agree_with_memory`]).
    #[test]
    fn structural_span_sql_gates_both_timestamps() {
        assert!(
            INTERACTION_SPAN_SQL.contains("e.created_at <= ?"),
            "span SQL must gate the EDGE timestamp"
        );
        assert!(
            INTERACTION_SPAN_SQL.contains("i.created_at <= ?"),
            "span SQL must gate the ORIGIN-INTERACTION timestamp"
        );
    }

    /// F5: `concepts.origin_interaction` is a **global** FK, so a concept in
    /// session S may legally point at an interaction in session S′. The span
    /// CTE must scope the joined interaction to S (the extent CTE always was),
    /// otherwise foreign interactions inflate `distinct` and — since their
    /// timestamps sit outside S's extent — the coverage ratio, on a population
    /// `MemoryStore` (which resolves origins inside the session snapshot only)
    /// never sees. Pre-fix this returned `distinct = 3`, coverage clamped from
    /// 200.0; MemoryStore returned `distinct = 0`.
    #[cfg(feature = "store-memory")]
    #[tokio::test]
    async fn interaction_span_ignores_cross_session_origin_interactions() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let here = SessionId::from("span-here");
        let there = SessionId::from("span-there");
        let t0 = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
        let t = |secs: i64| t0 + chrono::Duration::seconds(secs);

        // `here` extent is 100s; `there` straddles it by ±10000s, so an
        // unscoped ratio is 20000/100 = 200.0.
        let (h0, h1) = (NodeId::new(), NodeId::new());
        let (b0, b1, b2) = (NodeId::new(), NodeId::new(), NodeId::new());
        let target = NodeId::new();
        let (s0, s1, s2) = (NodeId::new(), NodeId::new(), NodeId::new());
        let mut mutations = vec![
            plant_interaction(&here, h0, None, t(0)),
            plant_interaction(&here, h1, Some(h0), t(100)),
            plant_interaction(&there, b0, None, t(-10_000)),
            plant_interaction(&there, b1, Some(b0), t(0)),
            plant_interaction(&there, b2, Some(b1), t(10_000)),
            plant_concept(&here, target, h0, "target", ConceptType::Entity, t(0)),
        ];
        // Supports live in `here` but their origins point across sessions.
        for (id, origin, name) in [(s0, b0, "s0"), (s1, b1, "s1"), (s2, b2, "s2")] {
            mutations.push(plant_concept(
                &here,
                id,
                origin,
                name,
                ConceptType::Entity,
                t(0),
            ));
            mutations.push(plant_edge(&here, id, target, EdgeType::Dependency, t(0)));
        }
        let batch = MutationBatch { mutations };
        store.flush(&batch).await.unwrap();
        let memory = MemoryStore::new();
        memory.flush(&batch).await.unwrap();

        let got = store
            .interaction_span(&here, target, Duration::from_secs(0), Utc::now())
            .await
            .unwrap();
        let want = memory
            .interaction_span(&here, target, Duration::from_secs(0), Utc::now())
            .await
            .unwrap();
        assert_eq!(
            got, want,
            "cross-session origins must not diverge from MemoryStore"
        );
        assert_eq!(
            got.distinct, 0,
            "foreign-session origin interactions must not count"
        );
        assert_eq!(got.coverage, 0.0);
        assert!(
            got.coverage <= 1.0,
            "coverage is a ratio of the session's own extent"
        );
    }

    /// F1: a single-interaction session (temporal extent is one point) with a
    /// supported inbound dependency reports coverage 1.0, not 0.0 — parity
    /// with MemoryStore and Cockroach (canonization Stage 2 in short
    /// sessions), and agreement with MemoryStore's naive answer.
    #[cfg(feature = "store-memory")]
    #[tokio::test]
    async fn interaction_span_single_point_session_coverage_is_one() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let sid = SessionId::from("single-span");
        let ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
        let i1 = NodeId::new();
        let pillar = NodeId::new();
        let orphan = NodeId::new();
        let batch = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, i1, None, ts),
                plant_concept(&sid, pillar, i1, "pillar", ConceptType::Entity, ts),
                plant_concept(&sid, orphan, i1, "orphan", ConceptType::Entity, ts),
                Mutation::UpsertEdge {
                    edge: Edge {
                        id: NodeId::new(),
                        session_id: sid.clone(),
                        source: pillar,
                        target: orphan,
                        edge_type: EdgeType::Dependency,
                        weight: 1.0,
                        reinforcements: 1,
                        created_at: ts,
                        last_reinforced: ts,
                    },
                },
            ],
        };
        store.flush(&batch).await.unwrap();
        let memory = MemoryStore::new();
        memory.flush(&batch).await.unwrap();

        let span = store
            .interaction_span(&sid, orphan, Duration::from_secs(0), Utc::now())
            .await
            .unwrap();
        let span_want = memory
            .interaction_span(&sid, orphan, Duration::from_secs(0), Utc::now())
            .await
            .unwrap();
        assert_eq!(span, span_want, "MemoryStore parity");
        assert_eq!(span.distinct, 1);
        assert_eq!(span.coverage, 1.0);

        // Unsupported target: no inbound structural edges -> 0.0 on both.
        let empty = store
            .interaction_span(&sid, pillar, Duration::from_secs(0), Utc::now())
            .await
            .unwrap();
        let empty_want = memory
            .interaction_span(&sid, pillar, Duration::from_secs(0), Utc::now())
            .await
            .unwrap();
        assert_eq!(empty, empty_want);
        assert_eq!(empty.distinct, 0);
        assert_eq!(empty.coverage, 0.0);
    }

    /// Acceptance: canonization_events append + concept status update, via
    /// both the mutation and `record_canonization`.
    #[tokio::test]
    async fn canonization_events_append_and_update_concept() {
        let store = test_store();
        store.init_schema().await.unwrap();

        let sid = SessionId::from("canon");
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        // Whole-second clock: the store persists ms precision (T3.1 fixed
        // format), so Utc::now()'s nanoseconds would not round-trip.
        let ts = Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, i1, None, ts),
                    plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts),
                ],
            })
            .await
            .unwrap();

        let ev1 = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid.clone(),
            node_id: c1,
            from_status: CanonizationStatus::None,
            to_status: CanonizationStatus::Candidate,
            blast_radius: Some(2),
            last_demotion_time: None,
            occurred_at: ts,
        };
        store
            .flush(&MutationBatch {
                mutations: vec![Mutation::CanonizationTransition { event: ev1.clone() }],
            })
            .await
            .unwrap();

        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(
            snap.concepts[0].canonization_status,
            CanonizationStatus::Candidate
        );
        assert_eq!(snap.concepts[0].blast_radius, Some(2));
        assert_eq!(snap.canonization_events.len(), 1);
        assert_eq!(snap.canonization_events[0], ev1);

        // record_canonization appends a second event and re-updates the node.
        let ev2 = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid.clone(),
            node_id: c1,
            from_status: CanonizationStatus::Candidate,
            to_status: CanonizationStatus::Venerable,
            blast_radius: Some(4),
            last_demotion_time: None,
            occurred_at: ts + chrono::Duration::minutes(1),
        };
        store.record_canonization(&ev2).await.unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(
            snap.concepts[0].canonization_status,
            CanonizationStatus::Venerable
        );
        assert_eq!(snap.canonization_events.len(), 2);
        assert_eq!(snap.canonization_events[1], ev2);

        // Transition on a missing concept is a typed NotFound.
        let ghost = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid.clone(),
            node_id: NodeId::new(),
            from_status: CanonizationStatus::None,
            to_status: CanonizationStatus::Candidate,
            blast_radius: None,
            last_demotion_time: None,
            occurred_at: ts,
        };
        let err = store.record_canonization(&ghost).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));

        // F12: the write-behind log now replays ev1, which was already
        // recorded. The replay must be a no-op — not a status rollback to
        // Candidate, and not a duplicate audit row.
        store
            .flush(&MutationBatch {
                mutations: vec![Mutation::CanonizationTransition { event: ev1.clone() }],
            })
            .await
            .unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(
            snap.concepts[0].canonization_status,
            CanonizationStatus::Venerable,
            "a replayed hop must not roll the durable status back (F12)"
        );
        assert_eq!(snap.concepts[0].blast_radius, Some(4));
        assert_eq!(snap.canonization_events.len(), 2);
    }

    /// COH-3 acceptance: a demotion event (Canonical -> None) carries
    /// `last_demotion_time`; it lands on the concept and round-trips through the
    /// event table; a later non-demotion transition leaves it untouched.
    #[tokio::test]
    async fn demotion_event_sets_and_roundtrips_last_demotion_time() {
        let store = test_store();
        store.init_schema().await.unwrap();

        let sid = SessionId::from("demote-canon");
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        let ts = Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, i1, None, ts),
                    plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts),
                ],
            })
            .await
            .unwrap();

        let demote_at = ts + chrono::Duration::minutes(5);
        let ev = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid.clone(),
            node_id: c1,
            from_status: CanonizationStatus::Canonical,
            to_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: Some(demote_at),
            occurred_at: demote_at,
        };
        store.record_canonization(&ev).await.unwrap();

        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(
            snap.concepts[0].canonization_status,
            CanonizationStatus::None
        );
        assert_eq!(snap.concepts[0].blast_radius, None);
        assert_eq!(snap.concepts[0].last_demotion_time, Some(demote_at));
        assert_eq!(snap.canonization_events.len(), 1);
        assert_eq!(snap.canonization_events[0], ev);

        // A promotion after the demotion must NOT clobber the field (COALESCE).
        let promo = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid.clone(),
            node_id: c1,
            from_status: CanonizationStatus::None,
            to_status: CanonizationStatus::Candidate,
            blast_radius: Some(2),
            last_demotion_time: None,
            occurred_at: demote_at + chrono::Duration::minutes(1),
        };
        store.record_canonization(&promo).await.unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(
            snap.concepts[0].last_demotion_time,
            Some(demote_at),
            "non-demotion transitions leave last_demotion_time untouched"
        );
        assert_eq!(
            snap.concepts[0].canonization_status,
            CanonizationStatus::Candidate
        );
        assert_eq!(snap.canonization_events.len(), 2);
        assert_eq!(snap.canonization_events[1], promo);
    }

    /// Re-stamp a planted `UpsertNode` with a **stale** canonization snapshot
    /// and a bumped `gc_survived` — exactly the shape T4.5's
    /// `bump_gc_survived` appends: the concept as it stood when the mutation
    /// was queued, not as it stands now (R2-1).
    fn with_stale_canonization(
        m: Mutation,
        status: CanonizationStatus,
        blast: Option<i32>,
        last_demotion_time: Option<DateTime<Utc>>,
    ) -> Mutation {
        match m {
            Mutation::UpsertNode {
                node: NodeKind::Concept(mut c),
            } => {
                c.canonization_status = status;
                c.blast_radius = blast;
                c.last_demotion_time = last_demotion_time;
                c.gc_survived += 1;
                Mutation::UpsertNode {
                    node: NodeKind::Concept(c),
                }
            }
            other => panic!("expected a concept upsert, got {other:?}"),
        }
    }

    /// R2-1 on the durable tier: a stale `UpsertNode` flushed **ahead of** an
    /// already-recorded transition must not regress the concept row.
    ///
    /// `apply_canonization_transition` returns before the UPDATE when its
    /// audit INSERT dedupes ("the effect is already in the row"). That premise
    /// only holds while the canonization path owns the three columns —
    /// `upsert_concept`'s `ON CONFLICT` list used to write them from a
    /// snapshot queued before the hop, so this batch left the row wrong with
    /// no repair. `gc_survived` still lands: the fix is column ownership, not
    /// a blanket skip.
    #[tokio::test]
    async fn stale_upsert_before_a_recorded_transition_does_not_regress_the_status() {
        let store = test_store();
        store.init_schema().await.unwrap();

        let sid = SessionId::from("r2-1-status");
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        let ts = Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
        let plant = plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts);
        store
            .flush(&MutationBatch {
                mutations: vec![plant_interaction(&sid, i1, None, ts), plant.clone()],
            })
            .await
            .unwrap();

        let hop = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid.clone(),
            node_id: c1,
            from_status: CanonizationStatus::None,
            to_status: CanonizationStatus::Candidate,
            blast_radius: None,
            last_demotion_time: None,
            occurred_at: ts,
        };
        store.record_canonization(&hop).await.unwrap();

        store
            .flush(&MutationBatch {
                mutations: vec![
                    with_stale_canonization(plant, CanonizationStatus::None, None, None),
                    Mutation::CanonizationTransition { event: hop },
                ],
            })
            .await
            .unwrap();

        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(
            snap.concepts[0].canonization_status,
            CanonizationStatus::Candidate,
            "a stale upsert must not take a recorded hop back out of the row"
        );
        assert_eq!(
            snap.concepts[0].gc_survived, 1,
            "the upsert's own columns must still land — only the canonization \
             columns are excluded"
        );
        assert_eq!(snap.canonization_events.len(), 1, "no duplicate audit row");
    }

    /// R2-1, demotion variant — the worse half. The stale snapshot carries
    /// `last_demotion_time: None` and the pre-demotion blast, so the demoted
    /// node used to reload `Canonical` with the re-promotion cooldown erased
    /// (COH-3, "cooldown survives restart").
    #[tokio::test]
    async fn stale_upsert_before_a_recorded_demotion_does_not_erase_the_cooldown() {
        let store = test_store();
        store.init_schema().await.unwrap();

        let sid = SessionId::from("r2-1-cooldown");
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        let ts = Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
        let plant = plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts);
        store
            .flush(&MutationBatch {
                mutations: vec![plant_interaction(&sid, i1, None, ts), plant.clone()],
            })
            .await
            .unwrap();

        store
            .record_canonization(&CanonizationEvent {
                id: NodeId::new(),
                session_id: sid.clone(),
                node_id: c1,
                from_status: CanonizationStatus::Venerable,
                to_status: CanonizationStatus::Canonical,
                blast_radius: Some(8),
                last_demotion_time: None,
                occurred_at: ts,
            })
            .await
            .unwrap();

        let demote_at = ts + chrono::Duration::minutes(5);
        let demote = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid.clone(),
            node_id: c1,
            from_status: CanonizationStatus::Canonical,
            to_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: Some(demote_at),
            occurred_at: demote_at,
        };
        store.record_canonization(&demote).await.unwrap();

        store
            .flush(&MutationBatch {
                mutations: vec![
                    with_stale_canonization(plant, CanonizationStatus::Canonical, Some(8), None),
                    Mutation::CanonizationTransition { event: demote },
                ],
            })
            .await
            .unwrap();

        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(
            snap.concepts[0].canonization_status,
            CanonizationStatus::None,
            "a demoted node must not reload as Canonical"
        );
        assert_eq!(snap.concepts[0].blast_radius, None);
        assert_eq!(
            snap.concepts[0].last_demotion_time,
            Some(demote_at),
            "the re-promotion cooldown must survive the stale upsert"
        );
    }

    /// Acceptance: ISO-8601 timestamps use the FIXED 24-char ms format and
    /// lex ordering == time ordering (T3.1 contract).
    #[tokio::test]
    async fn timestamps_fixed_format_and_lex_ordering() {
        let store = test_store();
        store.init_schema().await.unwrap();

        let sid = SessionId::from("ts");
        let t1 = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let t2 = t1 + chrono::Duration::milliseconds(250);
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, NodeId::new(), None, t1),
                    plant_interaction(&sid, NodeId::new(), None, t2),
                ],
            })
            .await
            .unwrap();

        // Raw TEXT round-trips exactly and in the fixed 24-char form.
        let raw: String = sqlx::query_scalar(
            "SELECT created_at FROM interactions WHERE session_id = ? ORDER BY created_at ASC",
        )
        .bind(&sid.0)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(raw, "2026-01-01T12:00:00.000Z");
        assert_eq!(raw.len(), 24);
        assert!(raw.ends_with('Z'));

        // Lex order of the stored TEXT equals time order (contract that makes
        // SQL age comparisons valid).
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT created_at FROM interactions WHERE session_id = ? ORDER BY created_at ASC",
        )
        .bind(&sid.0)
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec!["2026-01-01T12:00:00.000Z", "2026-01-01T12:00:00.250Z"]
        );
        assert!(rows[0] < rows[1]);

        // Load parses back to the exact instants.
        let snap = store.load_session(&sid).await.unwrap();
        let times: Vec<_> = snap.interactions.iter().map(|i| i.created_at).collect();
        assert_eq!(times, vec![t1, t2]);
    }

    /// Acceptance: a legal demote (duplicate Observation canonical keys) must
    /// not fail the flush (partial-UNIQUE semantics); a duplicate
    /// non-Observation key is a real conflict and must fail.
    #[tokio::test]
    async fn partial_unique_demote_duplicates_pass_but_entity_duplicates_fail() {
        let store = test_store();
        store.init_schema().await.unwrap();

        let sid = SessionId::from("uniq");
        let i1 = NodeId::new();
        let o1 = NodeId::new();
        let o2 = NodeId::new();
        let e1 = NodeId::new();
        let ts = Utc::now();

        // Two Observations sharing a canonical key + one Entity sharing that
        // same key: all legal (the partial index only constrains
        // concept_type <> 'Observation').
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, i1, None, ts),
                    plant_concept(
                        &sid,
                        o1,
                        i1,
                        "duplicate sentence",
                        ConceptType::Observation,
                        ts,
                    ),
                    plant_concept(
                        &sid,
                        o2,
                        i1,
                        "duplicate sentence",
                        ConceptType::Observation,
                        ts,
                    ),
                    plant_concept(&sid, e1, i1, "duplicate sentence", ConceptType::Entity, ts),
                ],
            })
            .await
            .unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.concepts.len(), 3, "all three rows must persist");

        // A second Entity with the same canonical key violates the partial
        // unique index and must fail the flush (transaction rolled back).
        let e2 = NodeId::new();
        let err = store
            .flush(&MutationBatch {
                mutations: vec![plant_concept(
                    &sid,
                    e2,
                    i1,
                    "duplicate sentence",
                    ConceptType::Entity,
                    ts,
                )],
            })
            .await
            .unwrap_err();
        // STORE-4: constraint violations are classified (never flattened
        // into Backend) so the flush loop can dead-letter them.
        assert!(
            matches!(err, StoreError::Constraint(_)),
            "expected Constraint, got {err:?}"
        );
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.concepts.len(), 3, "failed flush must not persist rows");
    }

    #[tokio::test]
    async fn concurrent_flushes_across_sessions_do_not_fail() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let store = std::sync::Arc::new(store);

        let mut handles = Vec::new();
        for n in 0..8 {
            let s = std::sync::Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                let sid = SessionId::from(format!("c{n}"));
                let i1 = NodeId::new();
                let c1 = NodeId::new();
                let ts = Utc::now();
                s.flush(&MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, None, ts),
                        plant_concept(&sid, c1, i1, &format!("n{n}"), ConceptType::Entity, ts),
                    ],
                })
                .await
                .unwrap();
                s.load_session(&sid).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }

    /// Acceptance: build_store(kind = sqlite) returns a working adapter and
    /// is_ready() is true under the feature.
    #[tokio::test]
    async fn build_store_registry_returns_working_sqlite_adapter() {
        let store = crate::store::build_store(crate::store::StoreConfig {
            kind: crate::store::StoreKind::Sqlite,
            dsn: None,
            path: Some("sqlite::memory:".into()),
        })
        .unwrap();
        assert!(crate::store::StoreKind::Sqlite.is_ready());
        assert!(store.capabilities().is_empty());

        store.init_schema().await.unwrap();
        let sid = SessionId::from("registry");
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        let ts = Utc::now();
        store
            .flush(&MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, i1, None, ts),
                    plant_concept(&sid, c1, i1, "registry concept", ConceptType::Entity, ts),
                ],
            })
            .await
            .unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.concepts.len(), 1);
        assert_eq!(snap.concepts[0].content, "registry concept");
    }

    /// RAII cleanup for a file-backed test database: removes the db file and
    /// any WAL/SHM sidecars on drop (the pool is dropped with the store, so
    /// WAL sidecars are normally already checkpointed away).
    struct TempDb(std::path::PathBuf);

    impl TempDb {
        fn new() -> Self {
            Self(
                std::env::temp_dir().join(format!("lambo-sqlite-test-{}.db", uuid::Uuid::new_v4())),
            )
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for sidecar in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{}", self.0.display(), sidecar));
            }
        }
    }

    /// CON-1: a fresh file-backed database must bootstrap (create_if_missing)
    /// and round-trip a flush → load, surviving a reopen. Pre-fix, connect on
    /// a fresh path failed with `(code: 14) unable to open database file`.
    #[tokio::test]
    async fn file_backed_roundtrip_survives_reopen() {
        let db = TempDb::new();
        let path = db.path().to_str().unwrap();
        {
            let store = SqliteStore::connect(path).unwrap();
            store.init_schema().await.unwrap();
            let sid = SessionId::from("file-backed");
            let i1 = NodeId::new();
            let c1 = NodeId::new();
            let ts = Utc::now();
            store
                .flush(&MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, None, ts),
                        plant_concept(&sid, c1, i1, "file-backed concept", ConceptType::Entity, ts),
                    ],
                })
                .await
                .unwrap();
            let snap = store.load_session(&sid).await.unwrap();
            assert_eq!(snap.concepts.len(), 1);
            assert_eq!(snap.concepts[0].content, "file-backed concept");
        }
        // Reopen from disk: the data must be there (durability, not just
        // in-process memory).
        let store = SqliteStore::connect(path).unwrap();
        store.init_schema().await.unwrap();
        let snap = store
            .load_session(&SessionId::from("file-backed"))
            .await
            .unwrap();
        assert_eq!(snap.concepts.len(), 1);
        assert_eq!(snap.concepts[0].content, "file-backed concept");
        drop(store);
    }

    /// STORE-9: a file-backed database is opened with WAL journal mode and an
    /// 8s busy_timeout (deliberately non-default — sqlx's default is 5s, so
    /// the assertion below fails if the `.busy_timeout()` wiring is removed),
    /// so a concurrent external reader (spec §2.2) can't turn a flush into a
    /// SQLITE_BUSY failure.
    #[tokio::test]
    async fn file_backed_wal_and_busy_timeout_applied() {
        let db = TempDb::new();
        let store = SqliteStore::connect(db.path().to_str().unwrap()).unwrap();
        store.init_schema().await.unwrap();
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        let busy: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert!(busy >= 8_000, "busy_timeout was {busy} ms");
        drop(store);
    }

    /// STORE-9 guard contract: `is_in_memory_uri` must classify exactly the
    /// spellings sqlx's `FromStr` treats as in-memory (database part
    /// `:memory:` or a `mode=memory` query param, position-independent) —
    /// including exotic spellings the old four-literal guard missed, such as
    /// `:memory:?cache=shared` and `mode=memory` in a non-first query slot.
    #[test]
    fn is_in_memory_uri_matches_sqlx_grammar() {
        for mem in [
            ":memory:",
            ":memory:?cache=shared",
            "sqlite::memory:",
            "sqlite://:memory:",
            "sqlite://?mode=memory",
            "sqlite://db.db?cache=shared&mode=memory",
            "sqlite://db.db?mode=memory&cache=private",
        ] {
            assert!(
                SqliteStore::is_in_memory_uri(mem),
                "sqlx treats {mem:?} as in-memory; the guard must too"
            );
        }
        for file in [
            "db.db",
            "sqlite://db.db",
            "sqlite://db.db?mode=rwc",
            "sqlite://db.db?cache=shared",
            "sqlite://db.db?mode=rw&cache=private",
        ] {
            assert!(
                !SqliteStore::is_in_memory_uri(file),
                "sqlx treats {file:?} as file-backed; the guard must too"
            );
        }
    }

    /// STORE-9 guard behavior: in-memory databases must NOT receive the
    /// file-backed WAL / busy_timeout tuning. Opened via the exotic
    /// `:memory:?cache=shared` spelling so a guard regression would route this
    /// through the file branch. `PRAGMA journal_mode` alone cannot detect that
    /// (WAL on an in-memory DB is a silent no-op — SQLite reports `memory`
    /// either way), so also assert busy_timeout stayed at sqlx's 5s default
    /// rather than the file branch's 8s; that check fails on a guard miss.
    #[tokio::test]
    async fn memory_database_does_not_get_wal() {
        let store = SqliteStore::connect(":memory:?cache=shared").unwrap();
        store.init_schema().await.unwrap();
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(mode, "memory");
        let busy: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert!(
            busy < 8_000,
            "file-backed tuning leaked into in-memory DB: busy_timeout={busy} ms \
             (sqlx default is 5000, file branch sets 8000)"
        );
    }

    // -----------------------------------------------------------------------
    // Single-writer lease (T8.6)
    // -----------------------------------------------------------------------

    fn lease_holder(agent: &str, pid: u32) -> LeaseHolder {
        LeaseHolder {
            agent: AgentId::new(agent),
            pid,
            host: "test-host".into(),
        }
    }

    /// A scratch sqlite file path this test owns; the caller removes the dir.
    fn scratch_db() -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!(
            "lambo-lease-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lease.sqlite");
        let s = path.to_str().unwrap().to_string();
        (dir, s)
    }

    /// T8.6: the acquire/Held/release/expiry contract on a single connection.
    #[tokio::test]
    async fn lease_lifecycle_on_one_connection() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let sid = SessionId::from("s");
        let a = lease_holder("agent-a", 100);
        let b = lease_holder("agent-b", 200);
        let ttl = Duration::from_secs(30);

        assert!(store
            .acquire_lease(&sid, &a, ttl)
            .await
            .unwrap()
            .is_acquired());
        match store.acquire_lease(&sid, &b, ttl).await.unwrap() {
            LeaseOutcome::Held { current, .. } => assert_eq!(current.holder, a.token()),
            other => panic!("expected Held, got {other:?}"),
        }
        // Refresh keeps acquired_at.
        let LeaseOutcome::Acquired(first) = store.acquire_lease(&sid, &a, ttl).await.unwrap()
        else {
            panic!("A refresh must succeed");
        };
        let LeaseOutcome::Acquired(refreshed) = store.refresh_lease(&sid, &a, ttl).await.unwrap()
        else {
            panic!("refresh must succeed");
        };
        assert_eq!(first.acquired_at, refreshed.acquired_at);

        store.release_lease(&sid, &a).await.unwrap();
        assert!(store
            .acquire_lease(&sid, &b, ttl)
            .await
            .unwrap()
            .is_acquired());
    }

    /// T8.6: expiry-after-crash on sqlite — an unreleased lease blocks before the
    /// TTL and is reclaimable after it. Uses a 1s TTL to stay well clear of any
    /// SQLite fractional-second rounding.
    #[tokio::test]
    async fn an_unreleased_lease_expires_on_sqlite() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let sid = SessionId::from("s");
        let dead = lease_holder("dead", 1);
        let live = lease_holder("live", 2);
        let ttl = Duration::from_secs(1);

        store.acquire_lease(&sid, &dead, ttl).await.unwrap();
        assert!(matches!(
            store.acquire_lease(&sid, &live, ttl).await.unwrap(),
            LeaseOutcome::Held { .. }
        ));
        tokio::time::sleep(Duration::from_millis(1_300)).await;
        assert!(store
            .acquire_lease(&sid, &live, ttl)
            .await
            .unwrap()
            .is_acquired());
    }

    /// T8.6: **two independent connections to one DB file** — the cross-process
    /// shape in miniature (a subprocess variant lives in
    /// `tests/serve_single_writer_lease.rs`). One acquires, the other is refused
    /// and told the holder; after a release the second wins.
    #[tokio::test]
    async fn two_connections_on_one_file_serialize_on_the_lease() {
        let (dir, path) = scratch_db();
        let sid = SessionId::from("shared");
        let a = lease_holder("proc-a", 111);
        let b = lease_holder("proc-b", 222);
        let ttl = Duration::from_secs(30);

        let store_a = SqliteStore::connect(&path).unwrap();
        store_a.init_schema().await.unwrap();
        let store_b = SqliteStore::connect(&path).unwrap();

        assert!(store_a
            .acquire_lease(&sid, &a, ttl)
            .await
            .unwrap()
            .is_acquired());
        match store_b.acquire_lease(&sid, &b, ttl).await.unwrap() {
            LeaseOutcome::Held { current, .. } => assert_eq!(current.holder, a.token()),
            other => panic!("the second connection must be refused, got {other:?}"),
        }
        store_a.release_lease(&sid, &a).await.unwrap();
        assert!(store_b
            .acquire_lease(&sid, &b, ttl)
            .await
            .unwrap()
            .is_acquired());

        drop(store_a);
        drop(store_b);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

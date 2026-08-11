//! T3.3 — SQLite GraphStore adapter (offline / test tier, spec §3.2–§3.3, §4).
//!
//! Same trait surface as [`super::memory::MemoryStore`] over//! `sqlx::SqlitePool`. SQLite has **no** `VECTOR_SEARCH` capability, so
//! `capabilities()` is empty, `vector_candidates` returns
//! [`StoreError::Capability`], and `vector_dimensions()` is `None` (the
//! `embedding BLOB` column is never read or written — see T3.1 handoff).
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
//! The mutation path (spec §2.4) has no `Mutation` kind for
//! `root_goal`/`created_at`/`closed_at` — like MemoryStore, `load_session`
//! returns `None` for all three. The `sessions` row is created (FK anchor,
//! `created_at` DB-default) but its metadata columns are inert until a
//! full-snapshot save path exists.
//!
//! ## Embedding contract (schema completeness — S5-class, read-only)
//!
//! The `sessions` row carries `embedding_kind` / `embedding_model` /
//! `embedding_dim` (nullable, converged the same guarded way as
//! `chunk_group_id`). `load_session` reads them into
//! `GraphSnapshot.embedding` when present; a row with `embedding_kind` XOR
//! `embedding_dim` is treated as a corruption error. `flush` does NOT write
//! them — no `Mutation` kind carries session metadata (S5: snapshot-only; the
//! write path awaits a future session-metadata mutation, so today the columns
//! are always NULL after a flush).

// Clippy's `explicit_auto_deref` suggestion is wrong for sqlx: `&mut *tx` reborrows
// the `Transaction` (which implements `sqlx::Executor`), while the suggested `&mut tx`
// produces `&mut &mut Transaction` (which does not). Known sqlx+clippy false-positive;
// kept explicit on purpose.
#![allow(clippy::explicit_auto_deref)]

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use super::{Capabilities, GraphStore};
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
const INTERACTION_SPAN_SQL: &str = "WITH span AS ( \
     SELECT DISTINCT i.id, i.created_at \
     FROM edges e \
     JOIN concepts src ON src.id = e.source \
     JOIN interactions i ON i.id = src.origin_interaction \
     WHERE e.target = ? AND e.session_id = ? \
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
    pub fn connect(path: &str) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::from_str(path)
            .map_err(|e| StoreError::Backend(format!("sqlite connect options {path:?}: {e}")))?
            // REFERENCES clauses are kept in the DDL for fidelity; enforcing
            // them is the adapter's job (T3.1 handoff).
            .foreign_keys(true);
        Ok(Self::new(options))
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
            .map_err(|e| db_err("ensure session row", e))?;
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
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::empty()
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
            .map_err(|e| db_err("begin flush transaction", e))?;

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
                        .map_err(|e| db_err("delete edge", e))?;
                }
                Mutation::CanonizationTransition { event } => {
                    apply_canonization_transition(&mut *tx, event).await?;
                }
            }
        }

        tx.commit()
            .await
            .map_err(|e| db_err("commit flush transaction", e))?;
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

        // The existence probe doubles as the embedding-contract read (S5-class:
        // snapshot-only — flush never writes these columns; see module doc).
        let row = sqlx::query(
            "SELECT embedding_kind, embedding_model, embedding_dim \
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
        let embedding_kind: Option<String> = row.get(0);
        let embedding_model: Option<String> = row.get(1);
        let embedding_dim: Option<i64> = row.get(2);
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
            // Session-level metadata is not carried by the mutation path —
            // None, matching MemoryStore (see module doc).
            root_goal: None,
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
            let id: String = row.get(0);
            let score: i64 = row.get(1);
            out.push(Scored::new(node_id(&id, "concept id")?, score as f64));
        }
        Ok(out)
    }

    async fn vector_candidates(
        &self,
        _session: &SessionId,
        _embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        Err(StoreError::Capability(
            "SqliteStore has no VECTOR_SEARCH".into(),
        ))
    }

    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
    ) -> Result<u64, StoreError> {
        // Spec §4.1 ported to `?` placeholders; the cutoff is computed in Rust
        // (SQLite has no INTERVAL) and bound as the fixed ISO-8601 TEXT.
        // Divergence from the spec text (for MemoryStore agreement): `c.id <> ?`
        // excludes the node itself, and `e.created_at <= ?` gates the edge age
        // exactly like MemoryStore (the spec's span query gates only the
        // interaction age — see interaction_span).
        self.require_session(session).await?;
        let cutoff = cutoff_text(Utc::now(), min_edge_age)?;
        let node_text = node.0.to_string();

        let row = sqlx::query(&format!(
            "SELECT count(*) \
             FROM concepts c \
             WHERE c.session_id = ? \
               AND c.id <> ? \
               AND EXISTS ( \
                   SELECT 1 FROM edges e \
                   JOIN concepts src ON src.id = e.source \
                   WHERE e.target = c.id AND e.source = ? \
                     AND e.edge_type IN ({STRUCTURAL_EDGE_IN}) \
                     AND e.created_at <= ?) \
               AND NOT EXISTS ( \
                   SELECT 1 FROM edges e2 \
                   JOIN concepts src2 ON src2.id = e2.source \
                   WHERE e2.target = c.id AND e2.source <> ? \
                     AND e2.edge_type IN ({STRUCTURAL_EDGE_IN}) \
                     AND e2.created_at <= ?)"
        ))
        .bind(&session.0)
        .bind(&node_text)
        .bind(&node_text)
        .bind(&cutoff)
        .bind(&node_text)
        .bind(&cutoff)
        .fetch_one(self.pool())
        .await
        .map_err(|e| db_err("blast_radius", e))?;
        let n: i64 = row.get(0);
        Ok(n as u64)
    }

    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
    ) -> Result<InteractionSpan, StoreError> {
        // Spec §4.1 span query: distinct origin interactions of concept-sourced
        // structural edges into `node`, aged on BOTH the edge and the origin
        // interaction (MemoryStore agreement — the spec text ages only the
        // interaction; the fixture data satisfies both, but the three-way gate
        // is MemoryStore's naive answer). Coverage is computed in Rust in ms,
        // identical to MemoryStore's formula.
        self.require_session(session).await?;
        let cutoff = cutoff_text(Utc::now(), min_age)?;
        let node_text = node.0.to_string();

        let row = sqlx::query(
            &INTERACTION_SPAN_SQL.replace("{STRUCTURAL_EDGE_IN}", STRUCTURAL_EDGE_IN),
        )
        .bind(&node_text)
        .bind(&session.0)
        .bind(&cutoff)
        .bind(&cutoff)
        .bind(&session.0)
        .fetch_one(self.pool())
        .await
        .map_err(|e| db_err("interaction_span", e))?;

        let distinct: i64 = row.get(0);
        let span_lo: Option<String> = row.get(1);
        let span_hi: Option<String> = row.get(2);
        let sess_lo: Option<String> = row.get(3);
        let sess_hi: Option<String> = row.get(4);

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
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| db_err("begin record_canonization transaction", e))?;
        apply_canonization_transition(&mut *tx, event).await?;
        tx.commit()
            .await
            .map_err(|e| db_err("commit record_canonization transaction", e))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Statement helpers
// ---------------------------------------------------------------------------

fn db_err(context: &str, e: sqlx::Error) -> StoreError {
    StoreError::Backend(format!("{context}: {e}"))
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
    .map_err(|e| db_err("upsert interaction", e))?;
    Ok(())
}

async fn upsert_concept(tx: &mut sqlx::SqliteConnection, c: &Concept) -> Result<(), StoreError> {
    // Conflict target is the `id` PRIMARY KEY. The partial unique index
    // (session_id, canonical_key) WHERE concept_type <> 'Observation' is NOT a
    // valid target (bare ON CONFLICT errors); legal duplicate Observation keys
    // (demote) never conflict with it, and a genuine duplicate non-Observation
    // key surfaces as an error (the graph tier already forbids it in RAM).
    let concept_type = enum_to_text(&c.concept_type, "concept_type")?;
    let status = enum_to_text(&c.canonization_status, "canonization_status")?;
    sqlx::query(
        "INSERT INTO concepts (\
             id, session_id, content, canonical_key, concept_type, origin_interaction, \
             origin_agent, created_at, access_count, last_accessed, gc_survived, \
             canonization_status, blast_radius, last_demotion_time, chunk_group_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
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
             canonization_status = excluded.canonization_status, \
             blast_radius = excluded.blast_radius, \
             last_demotion_time = excluded.last_demotion_time, \
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
    .bind(&c.chunk_group_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| db_err("upsert concept", e))?;
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
    .map_err(|e| db_err("upsert edge", e))?;
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
        .map_err(|e| db_err("delete interaction", e))?;
    sqlx::query("DELETE FROM concepts WHERE id = ?")
        .bind(&id_text)
        .execute(&mut *tx)
        .await
        .map_err(|e| db_err("delete concept", e))?;
    sqlx::query("DELETE FROM edges WHERE source = ? OR target = ? OR id = ?")
        .bind(&id_text)
        .bind(&id_text)
        .bind(&id_text)
        .execute(&mut *tx)
        .await
        .map_err(|e| db_err("delete incident edges", e))?;
    Ok(())
}

/// Shared by the `CanonizationTransition` mutation and `record_canonization`:
/// update the concept's status/blast_radius (NotFound if absent, like
/// MemoryStore) and append the event row — the demo's on-screen artifact.
async fn apply_canonization_transition(
    tx: &mut sqlx::SqliteConnection,
    event: &CanonizationEvent,
) -> Result<(), StoreError> {
    let to_status = enum_to_text(&event.to_status, "to_status")?;
    let res = sqlx::query(
        "UPDATE concepts SET canonization_status = ?, blast_radius = ? \
         WHERE id = ? AND session_id = ?",
    )
    .bind(&to_status)
    .bind(event.blast_radius)
    .bind(event.node_id.0.to_string())
    .bind(&event.session_id.0)
    .execute(&mut *tx)
    .await
    .map_err(|e| db_err("apply canonization transition", e))?;
    if res.rows_affected() == 0 {
        return Err(StoreError::NotFound(format!(
            "concept {} for canonization",
            event.node_id
        )));
    }

    let from_status = enum_to_text(&event.from_status, "from_status")?;
    sqlx::query(
        "INSERT INTO canonization_events (\
             id, session_id, node_id, from_status, to_status, blast_radius, occurred_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(event.id.0.to_string())
    .bind(&event.session_id.0)
    .bind(event.node_id.0.to_string())
    .bind(from_status)
    .bind(to_status)
    .bind(event.blast_radius)
    .bind(ts_to_text(event.occurred_at))
    .execute(&mut *tx)
    .await
    .map_err(|e| db_err("append canonization event", e))?;
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
        let id: String = row.get(0);
        let sid: String = row.get(1);
        let agent: String = row.get(2);
        let prompt: Option<String> = row.get(3);
        let prev: Option<String> = row.get(4);
        let created: String = row.get(5);
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
                canonization_status, blast_radius, last_demotion_time, chunk_group_id \
         FROM concepts WHERE session_id = ? ORDER BY id ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load concepts", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.get(0);
        let sid: String = row.get(1);
        let content: String = row.get(2);
        let key: String = row.get(3);
        let ctype: String = row.get(4);
        let origin: String = row.get(5);
        let agent: String = row.get(6);
        let created: String = row.get(7);
        let access_count: i32 = row.get(8);
        let last_accessed: Option<String> = row.get(9);
        let gc_survived: i32 = row.get(10);
        let status: String = row.get(11);
        let blast_radius: Option<i32> = row.get(12);
        let last_demotion: Option<String> = row.get(13);
        let chunk_group_id: Option<String> = row.get(14);
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
            embedding: None,
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
        let id: String = row.get(0);
        let sid: String = row.get(1);
        let source: String = row.get(2);
        let target: String = row.get(3);
        let etype: String = row.get(4);
        let weight: f64 = row.get(5);
        let reinforcements: i32 = row.get(6);
        let created: String = row.get(7);
        let last_reinforced: String = row.get(8);
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
        let sid: String = row.get(0);
        let src: String = row.get(1);
        let canon: String = row.get(2);
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
        let sid: String = row.get(0);
        let node: String = row.get(1);
        let agent: String = row.get(2);
        let expires: String = row.get(3);
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
        "SELECT id, session_id, node_id, from_status, to_status, blast_radius, occurred_at \
         FROM canonization_events WHERE session_id = ? ORDER BY occurred_at ASC, id ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load canonization events", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.get(0);
        let sid: String = row.get(1);
        let node: String = row.get(2);
        let from: String = row.get(3);
        let to: String = row.get(4);
        let blast_radius: Option<i32> = row.get(5);
        let occurred: String = row.get(6);
        out.push(CanonizationEvent {
            id: node_id(&id, "canonization event id")?,
            session_id: SessionId::from(sid),
            node_id: node_id(&node, "canonization node")?,
            from_status: text_to_enum(&from, "from_status")?,
            to_status: text_to_enum(&to, "to_status")?,
            blast_radius,
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
    use crate::store::load::load_session;
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
                let br = store.blast_radius(sid, *node, age).await.unwrap();
                let br_want = memory.blast_radius(sid, *node, age).await.unwrap();
                assert_eq!(br, br_want, "blast_radius {node} age {age:?}");

                let span = store.interaction_span(sid, *node, age).await.unwrap();
                let span_want = memory.interaction_span(sid, *node, age).await.unwrap();
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
                        .blast_radius(&sid, hub, Duration::from_secs(0))
                        .await
                        .unwrap(),
                    8
                );
                let span = sqlite
                    .interaction_span(&sid, hub, Duration::from_secs(0))
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
                .blast_radius(&sid, pillar, min_age)
                .await
                .unwrap();
            assert_eq!(want, 1, "oracle sanity: Derives must not un-orphan");
            let got = store.blast_radius(&sid, pillar, min_age).await.unwrap();
            assert_eq!(
                got, want,
                "SQLite must ignore provenance Derives exactly like MemoryStore (min_age {min_age:?})"
            );
        }
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
                plant_edge(
                    &sid,
                    probe_src,
                    probe_victim,
                    EdgeType::Dependency,
                    old_ts,
                ),
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
                let br = store.blast_radius(&sid, node, min_age).await.unwrap();
                let br_want = memory.blast_radius(&sid, node, min_age).await.unwrap();
                assert_eq!(br, br_want, "blast_radius {node} age {min_age:?}");
                let span = store.interaction_span(&sid, node, min_age).await.unwrap();
                let span_want = memory.interaction_span(&sid, node, min_age).await.unwrap();
                assert_eq!(span, span_want, "interaction_span {node} age {min_age:?}");
            }
        }
        // e-gate discrimination on the SPAN (independent of the oracle):
        // `other`'s DISTINCT origin i2 counts at min-age 0 and must vanish at
        // 1h when the fresh edge is filtered.
        assert_eq!(
            store
                .interaction_span(&sid, orphan, Duration::from_secs(0))
                .await
                .unwrap()
                .distinct,
            2,
            "e-gate: fresh edge's distinct origin counts at min_age=0"
        );
        assert_eq!(
            store
                .interaction_span(&sid, orphan, one_hour)
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
                .interaction_span(&sid, probe_victim, Duration::from_secs(0))
                .await
                .unwrap()
                .distinct,
            1,
            "i-gate: fresh origin counts at min_age=0"
        );
        assert_eq!(
            store
                .interaction_span(&sid, probe_victim, one_hour)
                .await
                .unwrap()
                .distinct,
            0,
            "i-gate: aged edge with fresh origin excluded at min_age=1h"
        );
        // blast_radius is origin-agnostic: the aged probe edge counts at 1h
        // even though its origin is fresh (span-only i-gate, MemoryStore parity).
        assert_eq!(
            store.blast_radius(&sid, probe_src, one_hour).await.unwrap(),
            1,
            "blast_radius ignores origin age"
        );
        // The age filter is doing real work for blast_radius (independent of
        // the oracle): with min-age 0 the fresh edge un-orphans; with min-age
        // 1h it is filtered and the orphan still counts.
        assert_eq!(
            store
                .blast_radius(&sid, pillar, Duration::from_secs(0))
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

    /// F1: a single-interaction session (temporal extent is one point) with a
    /// supported inbound dependency reports coverage 1.0, not 0.0 — parity
    /// with MemoryStore and Cockroach (canonization Stage 2 in short
    /// sessions), and agreement with MemoryStore's naive answer.
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
            .interaction_span(&sid, orphan, Duration::from_secs(0))
            .await
            .unwrap();
        let span_want = memory
            .interaction_span(&sid, orphan, Duration::from_secs(0))
            .await
            .unwrap();
        assert_eq!(span, span_want, "MemoryStore parity");
        assert_eq!(span.distinct, 1);
        assert_eq!(span.coverage, 1.0);

        // Unsupported target: no inbound structural edges -> 0.0 on both.
        let empty = store
            .interaction_span(&sid, pillar, Duration::from_secs(0))
            .await
            .unwrap();
        let empty_want = memory
            .interaction_span(&sid, pillar, Duration::from_secs(0))
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
            occurred_at: ts,
        };
        let err = store.record_canonization(&ghost).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
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
        assert!(matches!(err, StoreError::Backend(_)), "{err:?}");
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
}

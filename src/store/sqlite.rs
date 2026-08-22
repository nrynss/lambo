//! T3.3 — SQLite GraphStore adapter (offline / test tier, spec §3.2–§3.3, §4).
//!
//! Same trait surface as [`super::memory::MemoryStore`] over `sqlx::SqlitePool`.
//! `Concept.embedding` is written and read for flush→load round-trip parity (CON-8 — the
//! shared text form lives in the `embedding BLOB`) **and**, since F1/F2, queried: the
//! adapter advertises `VECTOR_SEARCH` and answers
//! [`GraphStore::vector_candidates_checked`] with an exact cosine scan over that column.
//!
//! ## Vector search (F1/F2 — issue #5)
//!
//! See the [seam note](#the-scan-is-a-seam) below for why the scan is split in two.
//! `capabilities()` and `vector_dimensions()` are two halves of one contract
//! (`resolve::check_vector_search_contract`), so they landed together with the query path:
//! the trait's fail-closed default for `vector_candidates_checked` turns recall into an
//! error for a store that advertises the capability without implementing the atomic
//! contract check, so advertising alone would have been worse than not advertising.
//!
//! ### Width authority
//!
//! SQLite's `concepts.embedding` is a width-agnostic `BLOB` — unlike Cockroach's
//! `VECTOR(n)` there is no schema number to parse, so the adapter has **no schema
//! authority of its own to report from**. Three numbers are therefore in play, and
//! only two of them are authorities (F-R1-2 / F-R1-8):
//!
//! * **`vector_dimensions()`** (sync, session-less) reports, in precedence order:
//!   `[store] vector_dim` when the operator set it, else the resolved
//!   `[embedder] dim` threaded in by `resolve::resolve_backends` via
//!   `store::build_store_with_vector_dim`, else the `EmbedderConfig` default (so no
//!   *third* hardcoded width is minted — issue #5 as filed asked for a literal
//!   `Some(1024)`; F amends that).
//!   - With a **pin** set, the number is a real authority: an operator assertion about
//!     the width this deployment's vectors use, which `resolve_backends` refuses to
//!     contradict — via an **explicit pin comparison written there**, which runs and
//!     returns before `check_vector_compatibility` (F-R2-3).
//!   - `resolve::check_vector_compatibility` is an **echo** for this adapter either
//!     way, and a pin does not change that: with no pin it receives the embedder width
//!     the adapter was handed, and with a pin it receives the pin — a number against
//!     itself in both cases. Never describe it as a store-side check for SQLite.
//!   - On the `build_store` / `resolve_store_only` path (provision and other
//!     store-only verbs, tests) the value is the `EmbedderConfig` **default** —
//!     nothing configured or verified it, and it may disagree with every session in
//!     the file. It is inert because store-only verbs never embed.
//! * **The session's durable contract** (`sessions.embedding_{kind,model,dim}`) is the
//!   authority that matters, and the only one that can attest which space the stored
//!   vectors are in. It is enforced twice: on **every candidate read**, in the same
//!   transaction as the candidate query (`vector_candidates_checked` refuses a
//!   mismatch), and at the **write gate**, where `enforce_concept_vector_widths`
//!   refuses a concept whose vector width disagrees with it — the check Cockroach's
//!   DDL performs for free. The gate has a second half in [`set_embedding`], which
//!   NULLs every vector of a different width when it stamps a contract (F-R2-1): the
//!   gate alone could not stop a **restamp** from leaving earlier vectors under a
//!   width they no longer match, because each of them was valid when written. The
//!   two together give the property — *no vector whose width disagrees with the
//!   session contract survives a write through this adapter* — and the per-read
//!   check remains the only defence against a hand-edited database.
//!
//! ### The scan is a seam
//!
//! Candidate *selection* ([`select_session_vectors`]) is separated from candidate
//! *scoring* ([`rank_by_cosine`]) on purpose. Today selection is a full session scan and
//! scoring is exact cosine, which is right while `n` is small: at 1024 f32 a concept
//! vector is 4 KB and the largest measured session held ~1,400 of them. But "n is small
//! by construction" was a property of *session-scoped* graphs, and a single unified
//! autobiographical session is not bounded that way. An ANN index replaces
//! `select_session_vectors` — whose signature already takes the probe and the limit for
//! exactly that reason, even though an exact scan ignores both — and `rank_by_cosine`
//! keeps re-ranking the survivors exactly. No caller and no other adapter method
//! changes.
//!
//! **Trigger to revisit: `hybrid::derive`, not recall** (F-R1-3). Recall runs one scan
//! per query. `derive` calls `vector_candidates_checked` *inside* its per-unmatched-
//! concept loop (`graph/hybrid.rs`), so a derive of `k` concepts over `n` stored vectors
//! is **k×n** BLOB decodes and text→`f32` parses, with no caching and no reuse between
//! iterations. All `k` of those scans share **one 30s deadline**
//! (`HYBRID_IO_TIMEOUT`, computed once per `derive` call) and contend for the **single
//! pooled connection** (`max_connections(1)`), which the write-behind flush also needs.
//! Overrunning the deadline is not a degradation — it returns `Backend("hybrid vector
//! candidate lookup timed out…")`, which propagates and fails the whole derive before
//! its commit phase. So measure the scan on the derive path first: that is where the
//! cliff is, and it is a path that could not run on SQLite at all before F2.
//!
//! The cheapest mitigation short of an index is to **hoist one scan per `derive` call**
//! instead of one per unmatched concept (same pool, same session, same contract check).
//! It is deliberately not done here: the probes are produced by per-concept `embed`
//! calls interleaved with the lookups, so hoisting means splitting `derive` into an
//! embed-all phase and a scan-once phase, and it needs a trait method that returns the
//! raw candidate pool rather than `Vec<Scored<NodeId>>`. Both restructure `derive`'s
//! per-concept error handling (each arm degrades a *single* concept on embed failure or
//! capability miss today) and the `GraphStore` trait, which is frozen. Named as the next
//! mitigation, with the trigger above, rather than smuggled in.
//!
//! `sqlite-vec` is deliberately not that index yet: it is a C toolchain dependency across
//! four cross-compiled release targets plus `sqlite3_auto_extension` registration before
//! sqlx opens a pool, bought against a latency number nobody has measured.
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
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use super::batch::{
    plan_flush, BulkLimits, ConceptRow, FlushStep, CONCEPT_COLUMNS, EDGE_COLUMNS,
    INTERACTION_COLUMNS,
};
#[cfg(feature = "fixtures")]
use super::batch::{seed_concept_rows, seed_edge_rows};
use super::lease::{lease_permits_write, LeaseHolder, LeaseInfo, LeaseOutcome};
use super::vector::{decode_vector, encode_vector};
use super::{
    columns_in_ddl, map_write_err, tables_in_ddl, unprovisioned_column_err,
    unprovisioned_store_err, validate_vector_candidate_limit, Capabilities, GraphStore,
    SessionFlushStats,
};
use crate::types::{
    CanonizationEvent, Concept, Edge, EmbeddingContract, GraphSnapshot, Interaction,
    InteractionSpan, Mutation, MutationBatch, Node, NodeId, Scored, SessionId, StoreError,
};

/// Structural edge types counted by both structural queries (spec §4.1 errata:
/// concept-to-concept `Dependency`/`Causal`/`Hierarchical` only — provenance
/// `Derives`/`Temporal` must not un-orphan concepts).
/// Rows per multi-row upsert statement (L82-1).
///
/// Chosen against SQLite's *most conservative* `SQLITE_MAX_VARIABLE_NUMBER` of
/// 999 rather than the 32766 a modern build ships: 16 columns × 60 rows = 960
/// and 9 × 100 = 900 both fit either way, and a statement that silently depends
/// on how the library was compiled is not worth the extra rows. There is no
/// network round-trip to save here — the limits exist so the shape matches
/// Cockroach's, not to hit a latency target. `interactions` stays 1 for the
/// same self-foreign-key reason documented on the Cockroach constant.
const BULK_LIMITS: BulkLimits = BulkLimits {
    interactions: 1,
    concepts: 60,
    edges: 100,
};

/// The conservative `SQLITE_MAX_VARIABLE_NUMBER` the limits above are sized
/// against. Pre-3.32 builds ship this; 3.32+ ship 32766.
const SQLITE_MAX_VARIABLE_NUMBER: usize = 999;

// R1-4: the arithmetic in the doc comment above is prose, and prose does not
// fail a build. Raising `concepts` to 70 (70 × 16 = 1120) passes the whole local
// suite against a modern bundled SQLite and only breaks on an old one, in
// production. These turn that into a compile error.
const _: () = assert!(
    BULK_LIMITS.interactions * INTERACTION_COLUMNS <= SQLITE_MAX_VARIABLE_NUMBER,
    "interactions chunk exceeds SQLITE_MAX_VARIABLE_NUMBER"
);
const _: () = assert!(
    BULK_LIMITS.concepts * CONCEPT_COLUMNS <= SQLITE_MAX_VARIABLE_NUMBER,
    "concepts chunk exceeds SQLITE_MAX_VARIABLE_NUMBER"
);
const _: () = assert!(
    BULK_LIMITS.edges * EDGE_COLUMNS <= SQLITE_MAX_VARIABLE_NUMBER,
    "edges chunk exceeds SQLITE_MAX_VARIABLE_NUMBER"
);

const STRUCTURAL_EDGE_IN: &str = "'Dependency', 'Causal', 'Hierarchical'";

/// T3.1 DDL — embedded and executed verbatim by [`SqliteStore::init_schema`],
/// and read for its table names by [`SqliteStore::preflight_schema`] (J3 F5).
/// Idempotent by construction (`IF NOT EXISTS` everywhere).
const INIT_SQL: &str = include_str!("../../migrations/sqlite/001_init.sql");

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
    /// Width reported by [`GraphStore::vector_dimensions`] — see the module doc's
    /// "Width authority". Not a schema constraint (the column is a `BLOB`) and NOT
    /// what the candidate path enforces; that is the session's durable contract.
    vector_dim: usize,
}

impl SqliteStore {
    pub fn new(options: SqliteConnectOptions) -> Self {
        Self {
            options,
            pool: OnceLock::new(),
            // The configured embedder width is the process's statement of the width
            // SQLite will persist; absent a caller-supplied one, use the same default
            // the `[embedder] dim` key has rather than minting a new literal here.
            vector_dim: crate::embed::EmbedderConfig::default().dim,
        }
    }

    /// Report `dim` from [`GraphStore::vector_dimensions`] instead of the
    /// `EmbedderConfig` default.
    ///
    /// `resolve::resolve_backends` calls this (through
    /// [`super::build_store_with_vector_dim`]) with the resolved `[embedder] dim`, so
    /// `check_vector_compatibility` compares the store against the embedder the process
    /// actually configured. A zero width is refused: the capability/width pair must stay
    /// consistent (`resolve::check_vector_search_contract`), and `Some(0)` would advertise
    /// a store that can hold no vector.
    pub fn with_vector_dim(mut self, dim: usize) -> Result<Self, StoreError> {
        if dim == 0 {
            return Err(StoreError::Invariant(
                "SqliteStore vector width must be > 0".into(),
            ));
        }
        self.vector_dim = dim;
        Ok(self)
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
        // Interactions before concepts (`concepts.origin_interaction`
        // REFERENCES interactions(id)); chunked exactly as a flush is, so the
        // seed path runs the same statements (L82-1).
        for i in &snapshot.interactions {
            upsert_interactions(&mut *tx, &[i]).await?;
        }
        // Deduplicated first (R1-6): a multi-row statement rejects colliding
        // input rows outright, where the row-at-a-time seed this replaced simply
        // last-wins'd them.
        for chunk in seed_concept_rows(&snapshot.concepts).chunks(BULK_LIMITS.concepts) {
            upsert_concepts(&mut *tx, chunk).await?;
        }
        for chunk in seed_edge_rows(&snapshot.edges).chunks(BULK_LIMITS.edges) {
            upsert_edges(&mut *tx, chunk).await?;
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
        // J3 write intents ride the seed for adapter parity: MemoryStore's
        // seed stores the whole snapshot, so dropping them here would make a
        // seeded-then-loaded session differ by adapter.
        for intent in &snapshot.write_intents {
            put_write_intent(&mut *tx, intent).await?;
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
        sqlx::query(INIT_SQL)
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
        ensure_column(
            self.pool(),
            "session_leases",
            "current_token",
            "ALTER TABLE session_leases ADD COLUMN current_token INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        // J2. Additive and nullable, so an ALREADY-PROVISIONED store (the
        // dogfood rig's `lambo-dev.db` among them) converges here on the next
        // attach without a re-provision: existing rows get NULL, which reads as
        // "this holder published no endpoint" — exactly what a pre-J2 holder
        // did. No default, deliberately: a fabricated address would be worse
        // than an honest absence.
        ensure_column(
            self.pool(),
            "session_leases",
            "endpoint",
            "ALTER TABLE session_leases ADD COLUMN endpoint TEXT",
        )
        .await?;
        Ok(())
    }

    /// J3 F5 + J3-R2R-3. A `sqlite_master` read, diffed against the **table**
    /// names in the DDL this build ships, then a `pragma_table_info` read per
    /// required table, diffed against the **column** set the same DDL declares.
    /// Cheap (no DDL, no write) and run before the lease is taken, so a refusal
    /// leaves no lease to release. The column half matters because a missing
    /// *column* is F5's exact consequence — attaches, acks, and loses
    /// everything, loud only at close — and the table check cannot see it
    /// (J3-R2R-3 measured it at the same magnitude as a missing table).
    async fn preflight_schema(&self) -> Result<(), StoreError> {
        let present: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
                .fetch_all(self.pool())
                .await
                .map_err(|e| db_err("preflight_schema: list tables", e))?;
        let required = tables_in_ddl(INIT_SQL);
        let missing_tables: Vec<&str> = required
            .into_iter()
            .filter(|t| !present.iter().any(|p| p == t))
            .collect();
        if !missing_tables.is_empty() {
            return Err(unprovisioned_store_err("sqlite", &missing_tables));
        }
        // Column preflight (J3-R2R-3): every required (table, column) from the
        // same DDL source, diffed per table against `pragma_table_info`.
        let mut by_table: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for (table, col) in columns_in_ddl(INIT_SQL) {
            by_table.entry(table).or_default().push(col);
        }
        for (table, cols) in by_table {
            let present_cols: Vec<String> =
                sqlx::query_scalar("SELECT name FROM pragma_table_info(?)")
                    .bind(table)
                    .fetch_all(self.pool())
                    .await
                    .map_err(|e| db_err("preflight_schema: list columns", e))?;
            let missing: Vec<&str> = cols
                .iter()
                .copied()
                .filter(|c| !present_cols.iter().any(|p| p == c))
                .collect();
            if !missing.is_empty() {
                return Err(unprovisioned_column_err("sqlite", table, &missing));
            }
        }
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        // F2: the query path exists (`vector_candidates_checked` below), so the
        // capability is honest. It must never be advertised without that
        // implementation — the trait's fail-closed default would turn every recall on
        // this store into `StoreError::Capability`, which is worse than the
        // keyword-only degradation it replaced.
        Capabilities::VECTOR_SEARCH
    }

    /// Configured width, never a schema parse — the `BLOB` column has no width.
    /// See the module doc's "Width authority": this answers process resolution, while
    /// the session's durable `sessions.embedding_dim` is what candidate reads enforce.
    fn vector_dimensions(&self) -> Option<usize> {
        Some(self.vector_dim)
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

    async fn read_lease(&self, session: &SessionId) -> Result<Option<LeaseInfo>, StoreError> {
        let row: Option<LeaseRowText> = sqlx::query_as(LEASE_ROW_SQL)
            .bind(&session.0)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| db_err("read lease", e))?;
        row.map(lease_info_from_text).transpose()
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

    async fn write_flush_stats(
        &self,
        session: &SessionId,
        stats: &SessionFlushStats,
    ) -> Result<(), StoreError> {
        // Upsert the whole row so re-publishes converge (idempotency, same
        // contract as `flush`). Only the writer's FlushTask calls this;
        // readers only read. `updated_at` is stamped from the store clock
        // (strftime), matching the SQLite TIMESTAMPTZ-as-TEXT convention.
        sqlx::query(
            "INSERT INTO session_stats (session_id, flush_lag_ms, log_depth, updated_at) \
             VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ','now')) \
             ON CONFLICT (session_id) DO UPDATE SET \
               flush_lag_ms = excluded.flush_lag_ms, \
               log_depth = excluded.log_depth, \
               updated_at = excluded.updated_at",
        )
        .bind(&session.0)
        .bind(stats.flush_lag_ms as i64)
        .bind(stats.log_depth as i64)
        .execute(self.pool())
        .await
        .map_err(|e| db_err("write flush stats", e))?;
        Ok(())
    }

    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionFlushStats>, StoreError> {
        let row =
            sqlx::query("SELECT flush_lag_ms, log_depth FROM session_stats WHERE session_id = ?1")
                .bind(&session.0)
                .fetch_optional(self.pool())
                .await
                .map_err(|e| db_err("read flush stats", e))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let flush_lag_ms: i64 = row
            .try_get("flush_lag_ms")
            .map_err(|e| db_err("read flush stats: flush_lag_ms", e))?;
        let log_depth: i64 = row
            .try_get("log_depth")
            .map_err(|e| db_err("read flush stats: log_depth", e))?;
        Ok(Some(SessionFlushStats {
            flush_lag_ms: u64::try_from(flush_lag_ms).unwrap_or(u64::MAX),
            log_depth: u64::try_from(log_depth).unwrap_or(u64::MAX),
        }))
    }

    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
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
                Mutation::PutWriteIntent { intent } => {
                    sessions.insert(intent.session_id.0.clone());
                }
                Mutation::ConsumeWriteIntent { session_id, .. } => {
                    sessions.insert(session_id.0.clone());
                }
                Mutation::DeleteNode { .. } | Mutation::DeleteEdge { .. } => {}
            }
        }
        self.ensure_sessions(&mut *tx, &sessions).await?;

        // Fencing-token gate (#1): reject a stale/missing token for every
        // session the batch touches, INSIDE the same transaction as the writes
        // (atomic with them — a takeover cannot slip between the check and the
        // commit; on rejection the `?` drops `tx`, rolling the batch back). A
        // session with a lease row (current_token >= 1) must present a token
        // that is current; an unleased session has no row and passes (seed /
        // fixture parity).
        for sid in &sessions {
            let current: Option<i64> = sqlx::query_scalar(
                "SELECT current_token FROM session_leases WHERE session_id = ?1",
            )
            .bind(sid)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| db_err("flush: read lease token", e))?;
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

        // Same planned-statement replay as the Cockroach adapter (L82-1). There
        // is no network here, so this is not the latency fix it is there — it is
        // kept identical on purpose. `store::batch`'s deduplication and
        // canonization-column rules are subtle enough that they need a real SQL
        // engine executing them in CI, and this is the adapter that can.
        for step in plan_flush(&batch.mutations, BULK_LIMITS) {
            apply_step(&mut *tx, &step).await?;
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
        let embedding = session_embedding_from_parts(
            embedding_kind,
            embedding_model,
            embedding_dim,
            &session.0,
        )?;

        let interactions = load_interactions(&mut *tx, session).await?;
        let concepts = load_concepts(&mut *tx, session).await?;
        let edges = load_edges(&mut *tx, session).await?;
        let synonyms = load_synonyms(&mut *tx, session).await?;
        let reservations = load_reservations(&mut *tx, session).await?;
        let canonization_events = load_canonization_events(&mut *tx, session).await?;
        let write_intents = load_write_intents(&mut *tx, session).await?;

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
            write_intents,
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
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        // Frozen v0.2.0 compatibility surface (Cockroach parity). It cannot attest
        // which contract produced `embedding`, so production code never calls it;
        // re-entering the checked path with the session's currently stored contract
        // preserves the legacy result shape while keeping the contract/vector snapshot
        // race closed inside this adapter.
        validate_vector_candidate_limit(limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
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

    /// Exact cosine over the session's flushed embeddings (F1, issue #5).
    ///
    /// **One transaction covers the contract read and the candidate read.** The race
    /// it closes is **cross-process**, not in-process (F-R1-9): within one process the
    /// single pooled connection (`max_connections(1)`) is a mutex, so a same-process
    /// flush cannot land between two statements of a transaction that already holds
    /// the only connection. What the transaction closes is a concurrent *writer
    /// process* on the same file database — `lambo serve` writing while `lambo recall`
    /// or `serve-web` reads, the documented topology — where WAL snapshot isolation
    /// guarantees both statements observe one snapshot and a commit in between is
    /// invisible until this transaction ends. (It follows that a future
    /// `max_connections(n) > 1` would make the in-process interleave real as well;
    /// the transaction already covers it, but the reason would change.) The refusal is
    /// `StoreError::Invariant`, matching Cockroach and the `VectorSearchStore`
    /// reference so callers classify it identically on all three.
    ///
    /// **An empty answer is not an error.** An unknown session, a session with no
    /// durable contract yet, and a session whose concepts carry no vectors all return
    /// an empty candidate list — the shape a vector-capable store returns before its
    /// first embedding lands. Only a corrupt row or a contract change is an error.
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
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| db_err("begin vector candidate transaction", e))?;

        let row = sqlx::query(
            "SELECT embedding_kind, embedding_model, embedding_dim \
             FROM sessions WHERE session_id = ?",
        )
        .bind(&session.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| db_err("vector_candidates: session contract", e))?;
        let Some(row) = row else {
            return Ok(Vec::new());
        };
        let stored = session_embedding_from_parts(
            row.try_get(0)
                .map_err(|e| db_err("vector_candidates: session contract", e))?,
            row.try_get(1)
                .map_err(|e| db_err("vector_candidates: session contract", e))?,
            row.try_get(2)
                .map_err(|e| db_err("vector_candidates: session contract", e))?,
            &session.0,
        )?;
        let Some(stored) = stored else {
            return Ok(Vec::new());
        };
        stored.ensure_compatible(expected_contract).map_err(|err| {
            StoreError::Invariant(format!(
                "vector candidate lookup refused after embedding contract changed: {err}"
            ))
        })?;
        // The probe is the caller's; the contract it claims is now known to be the
        // durable one, so a probe of a different width is a caller bug rather than a
        // store state. `cosine` would silently score it 0.0 on every row.
        if embedding.len() != stored.dim {
            return Err(StoreError::Invariant(format!(
                "query embedding has {} dimensions but session {} stores vectors of {} \
                 (the session's durable embedding contract is the authority here, not \
                 the process-wide vector_dimensions())",
                embedding.len(),
                session.0,
                stored.dim
            )));
        }

        let candidates =
            select_session_vectors(&mut *tx, session, embedding, limit, stored.dim).await?;
        tx.commit()
            .await
            .map_err(|e| db_err("commit vector candidate transaction", e))?;
        Ok(rank_by_cosine(embedding, candidates, limit))
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

    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool().begin().await.map_err(|e| {
            map_write_err(e, |m| format!("begin record_canonization transaction: {m}"))
        })?;
        // Fencing-token gate (#1): this durable write path HAD no lease check
        // at all — the canon task bypassed `lease_lost`. Check the token inside
        // this transaction, atomically with the write (rolls back on `?`).
        let current: Option<i64> =
            sqlx::query_scalar("SELECT current_token FROM session_leases WHERE session_id = ?1")
                .bind(&event.session_id.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| db_err("record_canonization: read lease token", e))?;
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

/// Rebuild the session's [`EmbeddingContract`] from the three nullable columns.
///
/// Shared by `load_session` and the checked candidate read so both classify a corrupt
/// row identically (STORE-7 parity with Cockroach's `session_embedding_from_parts`): a
/// row with `embedding_kind` XOR `embedding_dim` set — which direct SQL can manufacture —
/// is a corruption error, never a silent `None`. `embedding_model` alone is legal (an
/// embedder with no model identifier).
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

/// One decoded stored vector, before scoring.
type VectorCandidate = (NodeId, Vec<f32>);

/// **Candidate selection** — the swappable half of the vector query path (F1).
///
/// Today: every non-null `concepts.embedding` in the session, decoded. `probe` and
/// `limit` are part of the signature although an exact scan cannot use them, so that an
/// ANN index (see the module doc's "The scan is a seam") replaces this function's body
/// without touching [`rank_by_cosine`], `vector_candidates_checked`, or any caller.
///
/// Runs on the caller's transaction: the contract read that authorised this scan and the
/// scan itself must observe one snapshot.
///
/// **Width is checked, not truncated.** The BLOB holds the shared `[x,y,z]` text codec
/// (CON-8), so a row whose decoded element count disagrees with the session contract is
/// a corrupt row — returned as [`StoreError::Backend`]. `cosine` refuses length
/// mismatches by scoring 0.0, which would silently rank a corrupt concept last instead of
/// reporting it.
async fn select_session_vectors(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
    _probe: &[f32],
    _limit: usize,
    dim: usize,
) -> Result<Vec<VectorCandidate>, StoreError> {
    let rows = sqlx::query(
        "SELECT id, embedding FROM concepts \
         WHERE session_id = ? AND embedding IS NOT NULL ORDER BY id ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("vector_candidates: session vectors", e))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row
            .try_get(0)
            .map_err(|e| db_err("vector_candidates: concept id", e))?;
        let blob: Vec<u8> = row
            .try_get(1)
            .map_err(|e| db_err("vector_candidates: concept embedding", e))?;
        let text = std::str::from_utf8(&blob).map_err(|e| {
            StoreError::Backend(format!(
                "concepts.embedding for {id} is not valid UTF-8: {e}"
            ))
        })?;
        let vector = decode_vector(text)?;
        if vector.len() != dim {
            return Err(StoreError::Backend(format!(
                "concepts.embedding for {id} decodes to {} dimensions but session {} \
                 declares {dim}",
                vector.len(),
                session.0
            )));
        }
        out.push((node_id(&id, "concept id")?, vector));
    }
    Ok(out)
}

/// **Candidate scoring** — the fixed half. Exact cosine, best first, ties broken by the
/// smaller node id so the answer is deterministic (MemoryStore / Cockroach parity).
/// Stays exact whatever [`select_session_vectors`] becomes: an approximate index would
/// prune the pool, never the ranking.
fn rank_by_cosine(
    probe: &[f32],
    candidates: Vec<VectorCandidate>,
    limit: usize,
) -> Vec<Scored<NodeId>> {
    let mut scored: Vec<Scored<NodeId>> = candidates
        .into_iter()
        .map(|(id, vector)| Scored::new(id, f64::from(crate::embed::cosine(probe, &vector))))
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.item.0.cmp(&b.item.0))
    });
    scored.truncate(limit);
    scored
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
        INSERT INTO session_leases \
            (session_id, holder, acquired_at, expires_at, current_token, endpoint) \
        VALUES (?1, ?2, \
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), \
                strftime('%Y-%m-%dT%H:%M:%fZ','now', ?3), \
                1, ?4) \
        ON CONFLICT (session_id) DO UPDATE SET \
            holder = excluded.holder, \
            acquired_at = CASE WHEN session_leases.holder = excluded.holder \
                               THEN session_leases.acquired_at ELSE excluded.acquired_at END, \
            expires_at = excluded.expires_at, \
            current_token = CASE WHEN session_leases.holder = excluded.holder \
                                 THEN session_leases.current_token \
                                 ELSE session_leases.current_token + 1 END, \
            endpoint = excluded.endpoint \
        WHERE session_leases.expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now') \
           OR session_leases.holder = excluded.holder \
        RETURNING holder, acquired_at, expires_at, current_token, endpoint";

    for _ in 0..3 {
        let won: Option<LeaseRowText> = sqlx::query_as(ACQUIRE_SQL)
            .bind(&session.0)
            .bind(&token)
            .bind(&ttl_modifier)
            .bind(holder.endpoint.as_deref())
            .fetch_optional(pool)
            .await
            .map_err(|e| db_err("acquire lease", e))?;
        if let Some(row) = won {
            return Ok(LeaseOutcome::Acquired(lease_info_from_text(row)?));
        }
        // Guard was false — someone else holds a live lease. Read it back.
        let current: Option<LeaseRowText> = sqlx::query_as(LEASE_ROW_SQL)
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

/// The lease row as SQLite hands it back — see [`LEASE_ROW_SQL`] for the
/// column order this tuple mirrors. Named so the acquire's `RETURNING` and the
/// standalone read cannot drift apart in shape (J2 added a sixth column and the
/// two lists were already duplicated).
type LeaseRowText = (String, String, String, i64, Option<String>);

/// Every column [`LeaseInfo`] needs, in [`LeaseRowText`] order.
const LEASE_ROW_SQL: &str = "\
    SELECT holder, acquired_at, expires_at, current_token, endpoint \
    FROM session_leases WHERE session_id = ?1";

fn lease_info_from_text(row: LeaseRowText) -> Result<LeaseInfo, StoreError> {
    let (holder, acquired_at, expires_at, current_token, endpoint) = row;
    Ok(LeaseInfo {
        holder,
        token: u64::try_from(current_token)
            .map_err(|_| StoreError::Backend("lease row has a negative current_token".into()))?,
        acquired_at: text_to_ts(&acquired_at)?,
        expires_at: text_to_ts(&expires_at)?,
        endpoint,
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

/// Apply one planned [`FlushStep`].
async fn apply_step(
    tx: &mut sqlx::SqliteConnection,
    step: &FlushStep<'_>,
) -> Result<(), StoreError> {
    match step {
        FlushStep::Interactions(rows) => upsert_interactions(&mut *tx, rows).await,
        FlushStep::Concepts(rows) => upsert_concepts(&mut *tx, rows).await,
        FlushStep::Edges(rows) => upsert_edges(&mut *tx, rows).await,
        FlushStep::Single(m) => apply_single(&mut *tx, m).await,
        // Durable intents (J3/F4). SQLite is a local file — no network round-trip
        // per statement — so there is nothing to batch for and the existing
        // per-intent statements are both simpler and exactly the old behaviour.
        // The F4 win is Cockroach-specific; the planner's steps are the same here.
        FlushStep::PutIntents(intents) => {
            for intent in intents {
                put_write_intent(&mut *tx, intent).await?;
            }
            Ok(())
        }
        FlushStep::ConsumeIntents(consumes) => {
            for (session_id, receipt, outcome) in consumes {
                consume_write_intent(&mut *tx, session_id, receipt, outcome).await?;
            }
            Ok(())
        }
    }
}

/// Apply one mutation the planner could not bulk. See the Cockroach adapter's
/// `apply_single` for why the upsert arms are handled rather than
/// `unreachable!()`d.
async fn apply_single(tx: &mut sqlx::SqliteConnection, m: &Mutation) -> Result<(), StoreError> {
    match m {
        Mutation::UpsertNode {
            node: Node::Interaction(i),
        } => upsert_interactions(&mut *tx, &[i]).await?,
        Mutation::UpsertNode {
            node: Node::Concept(c),
        } => upsert_concepts(&mut *tx, &[ConceptRow::new(c)]).await?,
        Mutation::UpsertEdge { edge } => upsert_edges(&mut *tx, &[edge]).await?,
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
/// replaces the row, matching the memory adapter.
async fn put_write_intent(
    tx: &mut sqlx::SqliteConnection,
    intent: &crate::types::WriteIntent,
) -> Result<(), StoreError> {
    let payload = serde_json::to_string(&intent.payload)
        .map_err(|e| StoreError::Backend(format!("serialize write intent payload: {e}")))?;
    sqlx::query(
        "INSERT INTO write_intents \
             (session_id, receipt, agent, interaction_id, lane_seq, issued_ms, payload, \
              created_at, consumed_at, outcome_tag, outcome_summary) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
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
    .bind(intent.interaction.0.to_string())
    .bind(i64::try_from(intent.lane_seq).unwrap_or(i64::MAX))
    .bind(intent.issued_ms)
    .bind(payload)
    .bind(ts_to_text(intent.created_at))
    .bind(intent.outcome.as_ref().map(|o| ts_to_text(o.consumed_at)))
    .bind(intent.outcome.as_ref().map(|o| o.tag.clone()))
    .bind(intent.outcome.as_ref().map(|o| o.summary.clone()))
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("put write intent: {m}")))?;
    Ok(())
}

/// Mark one intent consumed with its outcome, then purge consumed rows older
/// than [`crate::types::WRITE_INTENT_RETENTION`] — clocked by the mutation's
/// own `consumed_at`, so the adapter needs no clock. Consuming an absent
/// receipt is a no-op (the put may already be purged; replay is idempotent).
async fn consume_write_intent(
    tx: &mut sqlx::SqliteConnection,
    session_id: &SessionId,
    receipt: &str,
    outcome: &crate::types::WriteIntentOutcome,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE write_intents SET consumed_at = ?, outcome_tag = ?, outcome_summary = ? \
         WHERE session_id = ? AND receipt = ?",
    )
    .bind(ts_to_text(outcome.consumed_at))
    .bind(&outcome.tag)
    .bind(&outcome.summary)
    .bind(session_id.as_str())
    .bind(receipt)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("consume write intent: {m}")))?;
    let cutoff = cutoff_text(outcome.consumed_at, crate::types::WRITE_INTENT_RETENTION)?;
    sqlx::query(
        "DELETE FROM write_intents \
         WHERE session_id = ? AND consumed_at IS NOT NULL AND consumed_at < ?",
    )
    .bind(session_id.as_str())
    .bind(cutoff)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("purge consumed write intents: {m}")))?;
    Ok(())
}

/// Load a session's write intents (J3), in replay order — (`issued_ms`,
/// `lane_seq`), which is exact admission order within one issuing process and
/// wall-clock order across processes.
async fn load_write_intents(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<crate::types::WriteIntent>, StoreError> {
    let rows = sqlx::query(
        "SELECT receipt, agent, interaction_id, lane_seq, issued_ms, payload, created_at, \
                consumed_at, outcome_tag, outcome_summary \
         FROM write_intents WHERE session_id = ? ORDER BY issued_ms ASC, lane_seq ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load write intents", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let receipt: String = row
            .try_get(0)
            .map_err(|e| db_err("load write intents", e))?;
        let agent: String = row
            .try_get(1)
            .map_err(|e| db_err("load write intents", e))?;
        let interaction: String = row
            .try_get(2)
            .map_err(|e| db_err("load write intents", e))?;
        let lane_seq: i64 = row
            .try_get(3)
            .map_err(|e| db_err("load write intents", e))?;
        let issued_ms: i64 = row
            .try_get(4)
            .map_err(|e| db_err("load write intents", e))?;
        let payload: String = row
            .try_get(5)
            .map_err(|e| db_err("load write intents", e))?;
        let created_at: String = row
            .try_get(6)
            .map_err(|e| db_err("load write intents", e))?;
        let consumed_at: Option<String> = row
            .try_get(7)
            .map_err(|e| db_err("load write intents", e))?;
        let outcome_tag: Option<String> = row
            .try_get(8)
            .map_err(|e| db_err("load write intents", e))?;
        let outcome_summary: Option<String> = row
            .try_get(9)
            .map_err(|e| db_err("load write intents", e))?;
        let payload: crate::types::WriteIntentPayload = serde_json::from_str(&payload)
            .map_err(|e| StoreError::Backend(format!("parse write intent payload: {e}")))?;
        let outcome = match (consumed_at, outcome_tag, outcome_summary) {
            (Some(at), Some(tag), Some(summary)) => Some(crate::types::WriteIntentOutcome {
                tag,
                summary,
                consumed_at: text_to_ts(&at)?,
            }),
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
            interaction: node_id(&interaction, "write intent interaction")?,
            lane_seq: u64::try_from(lane_seq).unwrap_or(u64::MAX),
            issued_ms,
            payload,
            created_at: text_to_ts(&created_at)?,
            outcome,
        });
    }
    Ok(out)
}

async fn upsert_interactions(
    tx: &mut sqlx::SqliteConnection,
    rows: &[&Interaction],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "INSERT INTO interactions (id, session_id, agent_id, prompt_text, previous_id, created_at) ",
    );
    qb.push_values(rows.iter(), |mut b, i| {
        b.push_bind(i.id.0.to_string())
            .push_bind(i.session_id.0.clone())
            .push_bind(i.agent_id.0.clone())
            .push_bind(i.prompt_text.clone())
            .push_bind(i.previous_id.map(|id| id.0.to_string()))
            .push_bind(ts_to_text(i.created_at));
    });
    qb.push(
        " ON CONFLICT (id) DO UPDATE SET \
             session_id = excluded.session_id, \
             agent_id = excluded.agent_id, \
             prompt_text = excluded.prompt_text, \
             previous_id = excluded.previous_id, \
             created_at = excluded.created_at",
    );
    qb.build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert interaction: {m}")))?;
    Ok(())
}

/// **The write gate for vector width** (F-R1-1).
///
/// Refuse a concept whose vector width disagrees with the session's durable
/// embedding contract, in the same transaction as the upsert that would store it.
/// This is what Cockroach's `VECTOR(1024)` DDL does for free
/// (`migrations/cockroach/001_init.sql`); SQLite's `concepts.embedding` is a
/// width-agnostic `BLOB`, so the adapter has to do it by hand or not at all.
///
/// # Why the write gate and not only the read check
///
/// [`select_session_vectors`] detects a width-mismatched row, but detection is
/// terminal and session-wide: it returns on the first bad row, so **one** corrupt
/// concept makes the whole session's vector leg — and therefore
/// `recall::candidates::gather` and `hybrid::derive`, both of which propagate a
/// `Backend` error rather than degrading — fail permanently, until someone edits
/// the row by hand. `Concept::embedding`'s own contract is *"width = session
/// `EmbeddingContract::dim`"*, and before this gate nothing on the SQLite write
/// path enforced it: one public `GraphStore::flush` could durably poison a
/// session. Refusing the write costs the caller one batch; accepting it costs the
/// session its recall.
///
/// # What the contract is at the moment a concept is validated
///
/// The contract read here is the one **visible in `sessions` when this step
/// executes**, which makes the intra-batch ordering well defined rather than
/// incidental: [`super::batch::plan_flush`] treats [`Mutation::SetEmbedding`] as a
/// barrier that drains every open bucket before it and is then emitted alone. So
/// within one [`MutationBatch`]:
///
/// * concepts submitted **after** a `SetEmbedding` are validated against the width
///   that `SetEmbedding` just stamped — a batch that stamps `dim` and then upserts
///   concepts of that `dim` **passes** (this is the shape `seed_vectors` and every
///   real `hybrid::derive` flush use);
/// * concepts submitted **before** it are validated against the contract that was
///   durable when they were written — which is **not** necessarily the contract a
///   reader will interpret them under, because the later `SetEmbedding` can move
///   it. That gap is closed on the other side, in [`set_embedding`]: stamping a
///   contract NULLs every vector of a different width, so a concept validated
///   against a contract that has since changed width is erased rather than
///   orphaned (F-R2-1 — round 2 reproduced the orphan through one public `flush`
///   when the quarantine only fired over a NULL contract).
///
/// So the two halves together, and neither alone, give the property this gate is
/// for: **no vector whose width disagrees with the session contract can survive a
/// write through this adapter's `GraphStore` surface.** The gate refuses a mismatch
/// against a contract that already exists; the quarantine erases one that a contract
/// change would otherwise leave behind.
///
/// The scoping to the trait is deliberate, and it leaves **two** residuals (F-R3-1):
/// a hand-edited database, which no write-side rule can cover and which the read
/// path's per-row width check is the defence against; and `SqliteStore::seed`, the
/// adapter's other `sessions.embedding_dim` writer, which restamps the contract with
/// no quarantine at all — `#[cfg(feature = "fixtures")]`, absent from the trait, and
/// reached by no in-tree caller outside tests. Because it *upserts* where
/// `MemoryStore::seed` *replaces*, a second seed over a live session can leave the
/// first seed's vectors orphaned under the new width. Named rather than closed
/// because it is fixtures scaffolding, not a shipped path.
///
/// # A vector arriving with no contract stamped is accepted
///
/// Deliberate, and the one place SQLite cannot mirror Cockroach: Cockroach's DDL
/// width is a property of the *table*, so it refuses a wrong-width insert even
/// with no session contract. SQLite has no such number — with `embedding_kind` /
/// `embedding_dim` still NULL there is no authority to check against, and the
/// process-configured `vector_dim` is explicitly not one (it is a resolution-time
/// pin, see the module doc's "Width authority"). Accepting is safe because such a
/// vector is unreachable *and* cannot survive to become the fatal mismatch above:
/// the read path returns an empty pool while the contract is NULL, and
/// [`set_embedding`] NULLs every vector of a different width when it stamps —
/// which from a NULL contract means all of them. The width becomes enforceable
/// exactly when it becomes meaningful.
async fn enforce_concept_vector_widths(
    tx: &mut sqlx::SqliteConnection,
    rows: &[ConceptRow<'_>],
) -> Result<(), StoreError> {
    // Cache per session: a batch normally touches one, and only vector-bearing
    // rows can violate anything, so a vector-free flush costs zero extra reads.
    let mut widths: HashMap<&str, Option<usize>> = HashMap::new();
    for r in rows {
        let Some(vector) = r.concept.embedding.as_ref() else {
            continue;
        };
        let sid = r.concept.session_id.0.as_str();
        if !widths.contains_key(sid) {
            let row = sqlx::query(
                "SELECT embedding_kind, embedding_model, embedding_dim \
                 FROM sessions WHERE session_id = ?",
            )
            .bind(sid)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| db_err("upsert concept: session contract", e))?;
            // Same classifier as `load_session` and the checked read, so a
            // kind-XOR-dim corrupt row is reported identically on all three paths
            // instead of being silently treated as unstamped here.
            let contract = match row {
                Some(row) => session_embedding_from_parts(
                    row.try_get(0)
                        .map_err(|e| db_err("upsert concept: session contract", e))?,
                    row.try_get(1)
                        .map_err(|e| db_err("upsert concept: session contract", e))?,
                    row.try_get(2)
                        .map_err(|e| db_err("upsert concept: session contract", e))?,
                    sid,
                )?,
                // `ensure_sessions` runs before every flush step, so a missing row
                // here means the session vanished mid-batch; treat it as unstamped
                // and let the upsert itself produce the real error.
                None => None,
            };
            widths.insert(sid, contract.map(|c| c.dim));
        }
        let Some(dim) = widths[sid] else {
            continue;
        };
        if vector.len() != dim {
            return Err(StoreError::Invariant(format!(
                "concept {} carries a {}-dimensional embedding but session {} stores \
                 vectors of {} — refusing the write: one width-mismatched row makes the \
                 session's entire vector leg fail on every read (re-embed the session or \
                 start a new one; `Concept::embedding` must match the session contract)",
                r.concept.id,
                vector.len(),
                sid,
                dim
            )));
        }
    }
    Ok(())
}

async fn upsert_concepts(
    tx: &mut sqlx::SqliteConnection,
    rows: &[ConceptRow<'_>],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    // Before any encoding: a refusal must leave the transaction with nothing
    // written, and `?` here rolls the whole batch back (F-R1-1).
    enforce_concept_vector_widths(&mut *tx, rows).await?;
    let mut encoded: Vec<ConceptBinds> = Vec::with_capacity(rows.len());
    for r in rows {
        encoded.push(concept_binds(r)?);
    }
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "INSERT INTO concepts (\
             id, session_id, content, canonical_key, concept_type, origin_interaction, \
             origin_agent, created_at, access_count, last_accessed, gc_survived, \
             canonization_status, blast_radius, last_demotion_time, embedding, \
             chunk_group_id) ",
    );
    qb.push_values(rows.iter().zip(encoded.iter()), |mut b, (r, enc)| {
        let c = r.concept;
        b.push_bind(c.id.0.to_string())
            .push_bind(c.session_id.0.clone())
            .push_bind(c.content.clone())
            .push_bind(c.canonical_key.clone())
            .push_bind(enc.concept_type.clone())
            .push_bind(c.origin_interaction.0.to_string())
            .push_bind(c.origin_agent.0.clone())
            .push_bind(ts_to_text(c.created_at))
            .push_bind(c.access_count)
            .push_bind(c.last_accessed.map(ts_to_text))
            .push_bind(c.gc_survived)
            .push_bind(enc.status.clone())
            .push_bind(r.canonization.blast_radius)
            .push_bind(r.canonization.last_demotion_time.map(ts_to_text))
            .push_bind(enc.embedding.clone())
            .push_bind(c.chunk_group_id.clone());
    });
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
    // `Mutation::UpsertNode`; the *values* bound above come from
    // `ConceptRow::canonization`, not from the concept, for the deduplication
    // reason spelled out on `store::batch::ConceptRow`.
    qb.push(
        " ON CONFLICT (id) DO UPDATE SET \
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
    );
    qb.build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert concept: {m}")))?;
    Ok(())
}

async fn upsert_edges(tx: &mut sqlx::SqliteConnection, rows: &[&Edge]) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut types: Vec<String> = Vec::with_capacity(rows.len());
    for e in rows {
        types.push(enum_to_text(&e.edge_type, "edge_type")?);
    }
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "INSERT INTO edges (\
             id, session_id, source, target, edge_type, weight, reinforcements, \
             created_at, last_reinforced) ",
    );
    qb.push_values(rows.iter().zip(types.iter()), |mut b, (e, edge_type)| {
        b.push_bind(e.id.0.to_string())
            .push_bind(e.session_id.0.clone())
            .push_bind(e.source.0.to_string())
            .push_bind(e.target.0.to_string())
            .push_bind(edge_type.clone())
            .push_bind(e.weight)
            .push_bind(e.reinforcements)
            .push_bind(ts_to_text(e.created_at))
            .push_bind(ts_to_text(e.last_reinforced));
    });
    // Natural-key preference (MemoryStore parity): the table-level
    // UNIQUE (source, target, edge_type) autoindexes and is a legal target.
    qb.push(
        " ON CONFLICT (source, target, edge_type) DO UPDATE SET \
             id = excluded.id, \
             session_id = excluded.session_id, \
             weight = excluded.weight, \
             reinforcements = excluded.reinforcements, \
             created_at = excluded.created_at, \
             last_reinforced = excluded.last_reinforced",
    );
    qb.build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert edge: {m}")))?;
    Ok(())
}

/// The fallible per-row encodings, done before `push_values`' infallible closure.
struct ConceptBinds {
    concept_type: String,
    status: String,
    /// CON-8: the embedding is written for flush→load round-trip parity. Same
    /// wire form as Cockroach's VECTOR text literal (shared store::vector
    /// codec), stored in the BLOB column; never NULL for a present vector, NULL
    /// otherwise.
    embedding: Option<Vec<u8>>,
}

fn concept_binds(r: &ConceptRow<'_>) -> Result<ConceptBinds, StoreError> {
    Ok(ConceptBinds {
        concept_type: enum_to_text(&r.concept.concept_type, "concept_type")?,
        status: enum_to_text(&r.canonization.status, "canonization_status")?,
        embedding: r
            .concept
            .embedding
            .as_ref()
            .map(|v| encode_vector(v))
            .transpose()?
            .map(|s| s.into_bytes()),
    })
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
///
/// # Stamping a contract quarantines every vector of a different width (F-R2-1)
///
/// The `UPDATE concepts SET embedding = NULL` below fires whenever the width
/// being stamped differs from the width durable *before* this statement —
/// `embedding_dim IS NOT ?` is SQLite's null-safe comparison, so it is true both
/// for an unstamped session (`embedding_dim IS NULL`, the original case) and for
/// a **restamp** from one width to another. Round 2 reproduced why the narrower
/// NULL-only predicate was not enough: a batch of
/// `SetEmbedding{4}`, `Concept{4-wide}`, `SetEmbedding{3}`, `Concept{3-wide}`
/// passes [`enforce_concept_vector_widths`] at every step — each concept really
/// does match the contract of its own moment — and still commits a 4-wide vector
/// under a `dim = 3` contract, which is the durable, session-wide,
/// permanent-until-hand-edited recall failure the gate exists to prevent. With
/// this predicate the second stamp NULLs the earlier vector, so the batch
/// self-heals instead: it is accepted, and the terminal state is a `dim = 3`
/// contract beside only 3-wide vectors. Same for the two-flush shape.
///
/// Together with the gate this closes the property across the trait: **no vector
/// whose width disagrees with the session contract can survive a write through this
/// adapter's `GraphStore` surface.** The gate refuses a mismatch against an existing
/// contract; this statement erases one that a contract change would otherwise
/// orphan.
///
/// Two residuals sit outside that surface (F-R3-1). The read path's per-row width
/// check remains the defence against an externally edited database, which no
/// write-side rule can cover. And `SqliteStore::seed` is a second
/// `sessions.embedding_dim` writer that this quarantine does not run: it restamps
/// the contract through `INSERT … ON CONFLICT (session_id) DO UPDATE SET …
/// embedding_dim = excluded.embedding_dim` with no quarantine, and because it
/// *upserts* where `MemoryStore::seed` *replaces*, concepts already in the session
/// but absent from the new snapshot are never revisited — so a second seed over a
/// live session can leave the first seed's vectors orphaned under the new width
/// (round-3 PROBE G reproduced exactly this terminal state through two `seed` calls
/// and no direct SQL). It is named rather than closed because the surface is
/// fixtures scaffolding: `seed` is `#[cfg(feature = "fixtures")]`, `fixtures` is off
/// both `default` and `ship`, `seed` is not on the `GraphStore` trait, and no
/// in-tree caller outside tests reaches it.
///
/// # Why *width*, and not any contract change
///
/// A kind or model change at the **same** width does **not** quarantine, and that
/// is deliberate rather than an omission:
///
/// * The graph tier already treats those cases differently on purpose.
///   `Graph::replace_embedding_with_operator_override` — the
///   `--allow-embedding-mismatch` writer attach path — *requires* equal widths,
///   refuses a `kind` change while any vector remains, and explicitly permits a
///   same-kind **model identifier rename** with the vectors left in place. Erasing
///   them here would destroy data on the one migration path built to keep it.
/// * Width is the only contract property this storage can enforce. A same-width
///   relabel leaves every BLOB decodable and every read correct; a width change
///   makes the stored bytes uninterpretable. Semantic space identity (kind/model)
///   is checked where it is knowable — `EmbeddingContract::ensure_compatible`, at
///   the graph tier and in `vector_candidates_checked` against the caller's
///   expected contract.
///
/// # Cockroach parity: deliberate divergence, with the reason
///
/// `cockroach.rs`'s `QUARANTINE_LEGACY_EMBEDDINGS_SQL` keeps the NULL-contract-only
/// predicate. That is a divergence, and it is sound because the shape it would
/// close cannot arise there: `concepts.embedding` is `VECTOR(1024)` in the DDL, so
/// every stored vector is exactly that wide or NULL, and a restamp to any other
/// width cannot produce a row that decodes to an unexpected width — it instead
/// makes the whole session refuse loudly at `check_embedding_dim` against the
/// DDL-parsed authority, before any row is read. SQLite's `BLOB` has no such
/// authority, which is why the adapter has to hold this line by hand. (The second
/// reason is honest rather than structural: this worktree has no Cockroach DSN, so
/// a change to that statement could not be executed, and an unrun SQL edit is
/// worse than a documented asymmetry.)
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
        // F-R2-1: `IS NOT` is SQLite's null-safe inequality, so this covers both
        // "no contract yet" (embedding_dim IS NULL) and "a different width was
        // durable a moment ago" — a restamp. Equal widths quarantine nothing, which
        // is what keeps a same-width model rename non-destructive. See the doc above.
        sqlx::query(
            "UPDATE concepts SET embedding = NULL WHERE session_id = ? AND EXISTS (\
             SELECT 1 FROM sessions WHERE session_id = ? \
             AND embedding_dim IS NOT ?)",
        )
        .bind(&session.0)
        .bind(&session.0)
        .bind(dim)
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

    /// J3 F5, at the adapter. An un-provisioned target fails the preflight; a
    /// provisioned one passes it; and dropping the one table this branch added
    /// (a pre-J3 store, exactly) fails it by name.
    #[tokio::test]
    async fn preflight_schema_refuses_an_unprovisioned_or_unmigrated_target() {
        let store = test_store();
        let err = store
            .preflight_schema()
            .await
            .expect_err("an empty database must not pass the preflight");
        assert!(
            matches!(err, StoreError::Capability(_)),
            "an un-migrated store is a missing capability, not a backend fault: {err:?}"
        );

        store.init_schema().await.unwrap();
        store
            .preflight_schema()
            .await
            .expect("a provisioned store must pass");

        sqlx::query("DROP TABLE write_intents")
            .execute(store.pool())
            .await
            .unwrap();
        let err = store
            .preflight_schema()
            .await
            .expect_err("a pre-J3 store must not pass")
            .to_string();
        assert!(err.contains("write_intents"), "{err}");
        assert!(err.contains("lambo provision"), "{err}");
    }

    /// J3-R2R-3, at the adapter: F5's uncovered half. A store whose *tables*
    /// are all present but one *column* is missing — the exact shape round 2
    /// measured attaching, acking and losing everything, loud only at close —
    /// must now be refused by the preflight, by table and column name.
    #[tokio::test]
    async fn preflight_schema_refuses_a_missing_column() {
        let store = test_store();
        store.init_schema().await.unwrap();
        store
            .preflight_schema()
            .await
            .expect("a provisioned store must pass the column preflight");
        // Render `concepts.chunk_group_id` absent under the required name the
        // way an older build's store would have it. RENAME is the clean
        // analogue of round-2's measured "one column missing" — all ten tables
        // present, one column gone under the name the DDL requires.
        sqlx::query("ALTER TABLE concepts RENAME COLUMN chunk_group_id TO chunk_group_id_old")
            .execute(store.pool())
            .await
            .unwrap();
        let err = store
            .preflight_schema()
            .await
            .expect_err("a store missing a column the build requires must not pass")
            .to_string();
        assert!(err.contains("concepts"), "names the table: {err}");
        assert!(err.contains("chunk_group_id"), "names the column: {err}");
        assert!(err.contains("lambo provision"), "actionable: {err}");
    }

    #[test]
    fn columns_in_ddl_parses_the_shipped_migration() {
        // The parser and the shipped DDL must agree, or the column preflight
        // silently checks nothing. Assert a column from each idiom: an inline
        // post-T3.1 column, a plain column, and that table-level constraints
        // (PRIMARY KEY / UNIQUE) are never read as columns.
        let cols = super::columns_in_ddl(INIT_SQL);
        assert!(
            cols.contains(&("concepts", "chunk_group_id")),
            "the J3-R2R-3 missing-column case must be in the parsed set"
        );
        assert!(cols.contains(&("concepts", "embedding")));
        assert!(cols.contains(&("sessions", "embedding_kind")));
        assert!(cols.contains(&("write_intents", "outcome_summary")));
        // The parse must not manufacture a column out of a table-level clause.
        assert!(!cols.contains(&("edges", "UNIQUE")));
        assert!(!cols.contains(&("synonyms", "PRIMARY")));
        assert!(!cols.contains(&("write_intents", "PRIMARY")));
    }

    #[tokio::test]
    async fn flush_stats_write_then_read_round_trips_sqlite() {
        // T85-3: a writer publishes flush stats into the durable table; a
        // reader (possibly another process) reads them back. Absent row =
        // honest `None` (n/a).
        let store = test_store();
        store.init_schema().await.unwrap();
        let sid = SessionId::from("stats-roundtrip");

        assert_eq!(store.read_flush_stats(&sid).await.unwrap(), None);

        let stats = SessionFlushStats {
            flush_lag_ms: 42,
            log_depth: 7,
        };
        store.write_flush_stats(&sid, &stats).await.unwrap();
        assert_eq!(store.read_flush_stats(&sid).await.unwrap(), Some(stats));

        // A different session has no row → None (n/a), never fabricated 0.
        assert_eq!(
            store
                .read_flush_stats(&SessionId::from("other"))
                .await
                .unwrap(),
            None
        );

        // Re-publish converges (idempotent upsert), the whole row is replaced.
        let later = SessionFlushStats {
            flush_lag_ms: 99,
            log_depth: 1,
        };
        store.write_flush_stats(&sid, &later).await.unwrap();
        assert_eq!(store.read_flush_stats(&sid).await.unwrap(), Some(later));
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

    // -- F1/F2 vector search (issue #5) -------------------------------------

    /// The 8-d contract every vector test below writes and queries under.
    fn vec_contract(dim: usize) -> EmbeddingContract {
        EmbeddingContract {
            kind: "fixture".into(),
            model: Some("test-model".into()),
            dim,
        }
    }

    /// A store whose reported width matches `dim`, so a small test vector is a
    /// legal probe (the default width is the `[embedder] dim` default, not 8).
    fn vec_test_store(dim: usize) -> SqliteStore {
        SqliteStore::connect("sqlite::memory:")
            .unwrap()
            .with_vector_dim(dim)
            .unwrap()
    }

    fn plant_concept_with_vector(
        sid: &SessionId,
        id: NodeId,
        origin: NodeId,
        content: &str,
        ts: DateTime<Utc>,
        embedding: Vec<f32>,
    ) -> Mutation {
        let Mutation::UpsertNode {
            node: NodeKind::Concept(mut concept),
        } = plant_concept(sid, id, origin, content, ConceptType::Entity, ts)
        else {
            unreachable!("plant_concept builds a concept upsert");
        };
        concept.embedding = Some(embedding);
        Mutation::UpsertNode {
            node: NodeKind::Concept(concept),
        }
    }

    /// Seed a session with a durable contract and the given vector-bearing concepts.
    async fn seed_vectors(
        store: &SqliteStore,
        sid: &SessionId,
        contract: &EmbeddingContract,
        vectors: &[(NodeId, &str, Vec<f32>)],
    ) {
        let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        let origin = NodeId::new();
        let mut mutations = vec![
            plant_interaction(sid, origin, None, ts),
            Mutation::SetEmbedding {
                session_id: sid.clone(),
                embedding: Some(contract.clone()),
            },
        ];
        for (id, content, vector) in vectors {
            mutations.push(plant_concept_with_vector(
                sid,
                *id,
                origin,
                content,
                ts,
                vector.clone(),
            ));
        }
        store
            .flush(&MutationBatch { mutations }, None)
            .await
            .unwrap();
    }

    /// F2: the capability and a concrete width land together, and the width is the
    /// configured one — never a constant minted inside the adapter (the amendment to
    /// issue #5, which asked for a literal `Some(1024)`).
    #[tokio::test]
    async fn vector_search_capability_reports_a_configured_width() {
        let store = test_store();
        store.init_schema().await.unwrap();
        assert_eq!(store.capabilities(), Capabilities::VECTOR_SEARCH);
        assert_eq!(
            store.vector_dimensions(),
            Some(crate::embed::EmbedderConfig::default().dim)
        );
        // The pair must satisfy CON-5 — advertising without a width makes recall
        // refuse to resolve at all.
        crate::resolve::check_vector_search_contract(&store, crate::store::StoreKind::Sqlite)
            .unwrap();

        // A configured width flows through, including a width no adapter hardcodes.
        let wide = vec_test_store(1536);
        assert_eq!(wide.vector_dimensions(), Some(1536));
        crate::resolve::check_vector_compatibility(wide.vector_dimensions(), 1536).unwrap();
        assert!(crate::resolve::check_vector_compatibility(wide.vector_dimensions(), 768).is_err());

        // Zero is refused: `Some(0)` would advertise a store that can hold no vector.
        let zero = SqliteStore::connect("sqlite::memory:")
            .unwrap()
            .with_vector_dim(0);
        assert!(
            matches!(zero, Err(StoreError::Invariant(_))),
            "{:?}",
            zero.err()
        );

        // The caller-limit guard survives the capability flip.
        assert!(matches!(
            store
                .vector_candidates_checked(
                    &SessionId::from("x"),
                    &[0.0; 8],
                    &vec_contract(8),
                    crate::store::MAX_VECTOR_CANDIDATE_LIMIT + 1,
                )
                .await
                .unwrap_err(),
            StoreError::Invariant(_)
        ));
    }

    /// F1: the scan scores exact cosine over the flushed BLOBs, best first, ties by
    /// the smaller node id — the ordering contract MemoryStore and Cockroach share.
    #[tokio::test]
    async fn vector_candidates_score_exact_cosine_in_rank_order() {
        let store = vec_test_store(4);
        store.init_schema().await.unwrap();
        let sid = SessionId::from("vec-rank");
        let contract = vec_contract(4);
        // `near` is the probe itself, `mid` is 45° away, `far` is orthogonal.
        let near = NodeId::new();
        let mid = NodeId::new();
        let far = NodeId::new();
        seed_vectors(
            &store,
            &sid,
            &contract,
            &[
                (near, "near", vec![1.0, 0.0, 0.0, 0.0]),
                (mid, "mid", vec![1.0, 1.0, 0.0, 0.0]),
                (far, "far", vec![0.0, 1.0, 0.0, 0.0]),
            ],
        )
        .await;

        let probe = [1.0f32, 0.0, 0.0, 0.0];
        let hits = store
            .vector_candidates_checked(&sid, &probe, &contract, 10)
            .await
            .unwrap();
        assert_eq!(
            hits.iter().map(|s| s.item).collect::<Vec<_>>(),
            vec![near, mid, far]
        );
        assert!((hits[0].score - 1.0).abs() < 1e-6, "{hits:?}");
        assert!(
            (hits[1].score - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6,
            "{hits:?}"
        );
        assert!(hits[2].score.abs() < 1e-6, "{hits:?}");

        // A concept with no vector is not a candidate at all (NULL, not score 0).
        let bare = NodeId::new();
        let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        let origin = store.load_session(&sid).await.unwrap().interactions[0].id;
        store
            .flush(
                &MutationBatch {
                    mutations: vec![plant_concept(
                        &sid,
                        bare,
                        origin,
                        "no vector",
                        ConceptType::Entity,
                        ts,
                    )],
                },
                None,
            )
            .await
            .unwrap();
        let hits = store
            .vector_candidates_checked(&sid, &probe, &contract, 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 3, "the vector-less concept must not appear");

        // Ties break by the smaller id, deterministically, whichever order the two
        // identical vectors were written in.
        let (lo, hi) = {
            let a = NodeId::new();
            let b = NodeId::new();
            if a.0 < b.0 {
                (a, b)
            } else {
                (b, a)
            }
        };
        let tie_sid = SessionId::from("vec-ties");
        seed_vectors(
            &store,
            &tie_sid,
            &contract,
            &[
                (hi, "written first", vec![1.0, 0.0, 0.0, 0.0]),
                (lo, "written second", vec![1.0, 0.0, 0.0, 0.0]),
            ],
        )
        .await;
        let hits = store
            .vector_candidates_checked(&tie_sid, &probe, &contract, 10)
            .await
            .unwrap();
        assert_eq!(
            hits.iter().map(|s| s.item).collect::<Vec<_>>(),
            vec![lo, hi]
        );

        // The frozen unchecked surface answers with the session's own contract.
        let legacy = store.vector_candidates(&sid, &probe, 10).await.unwrap();
        assert_eq!(legacy[0].item, near);
    }

    /// F1: SQLite's score is on the **same scale** as Cockroach's, so
    /// `semantic_match_threshold` and every recall rank mean the same thing on both.
    ///
    /// Cockroach ranks by the L2 distance its `<->` operator returns and converts with
    /// `1 - d²/2`; for the L2-normalized vectors every embedder is required to emit,
    /// that identity *is* cosine. Getting a distance/score conversion wrong does not
    /// fail — it just ranks differently — so the equality is asserted rather than
    /// assumed. This is the part of SQLite↔Cockroach recall parity that needs no
    /// cluster; ANN-vs-exact divergence in *which* candidates come back does.
    #[tokio::test]
    async fn vector_scores_match_the_cockroach_distance_conversion() {
        /// Cockroach's `distance_to_score`, transcribed.
        fn distance_to_score(dist: f64) -> f64 {
            (1.0 - 0.5 * dist * dist).clamp(-1.0, 1.0)
        }
        fn unit(v: [f32; 4]) -> Vec<f32> {
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter().map(|x| x / norm).collect()
        }
        fn l2(a: &[f32], b: &[f32]) -> f64 {
            f64::from(
                a.iter()
                    .zip(b)
                    .map(|(x, y)| (x - y) * (x - y))
                    .sum::<f32>()
                    .sqrt(),
            )
        }

        let store = vec_test_store(4);
        store.init_schema().await.unwrap();
        let sid = SessionId::from("vec-scale");
        let contract = vec_contract(4);
        let vectors = [
            unit([1.0, 0.0, 0.0, 0.0]),
            unit([1.0, 1.0, 0.0, 0.0]),
            unit([0.0, 1.0, 0.3, 0.0]),
            unit([-1.0, 0.0, 0.0, 0.0]),
        ];
        let seeded: Vec<(NodeId, &str, Vec<f32>)> = vectors
            .iter()
            .enumerate()
            .zip(["v0", "v1", "v2", "v3"])
            .map(|((_, v), label)| (NodeId::new(), label, v.clone()))
            .collect();
        seed_vectors(&store, &sid, &contract, &seeded).await;

        let probe = unit([1.0, 0.2, 0.0, 0.0]);
        let hits = store
            .vector_candidates_checked(&sid, &probe, &contract, 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 4);
        for (id, label, vector) in &seeded {
            let got = hits
                .iter()
                .find(|s| s.item == *id)
                .unwrap_or_else(|| panic!("{label} missing from {hits:?}"))
                .score;
            let want = distance_to_score(l2(&probe, vector));
            assert!(
                (got - want).abs() < 1e-6,
                "{label}: sqlite scored {got}, cockroach's conversion gives {want}"
            );
        }
    }

    /// F1: the checked path refuses a durable/expected contract change — kind, model
    /// and dim alike — instead of ranking vectors the caller cannot interpret. Same
    /// `Invariant` classification as Cockroach and `VectorSearchStore`, so a caller
    /// handles all three identically.
    #[tokio::test]
    async fn vector_candidates_refuse_a_changed_embedding_contract() {
        let store = vec_test_store(4);
        store.init_schema().await.unwrap();
        let sid = SessionId::from("vec-contract");
        let durable = vec_contract(4);
        let id = NodeId::new();
        seed_vectors(
            &store,
            &sid,
            &durable,
            &[(id, "stored", vec![1.0, 0.0, 0.0, 0.0])],
        )
        .await;
        let probe = [1.0f32, 0.0, 0.0, 0.0];

        // Sanity: the matching contract is served.
        assert_eq!(
            store
                .vector_candidates_checked(&sid, &probe, &durable, 5)
                .await
                .unwrap()
                .len(),
            1
        );

        let mismatches = [
            (
                "kind",
                EmbeddingContract {
                    kind: "bge_m3".into(),
                    ..durable.clone()
                },
            ),
            (
                "model",
                EmbeddingContract {
                    model: Some("other-model".into()),
                    ..durable.clone()
                },
            ),
            (
                "model cleared",
                EmbeddingContract {
                    model: None,
                    ..durable.clone()
                },
            ),
        ];
        for (what, expected) in mismatches {
            let err = store
                .vector_candidates_checked(&sid, &probe, &expected, 5)
                .await
                .unwrap_err();
            assert!(
                matches!(err, StoreError::Invariant(_)),
                "{what} mismatch must be Invariant, got {err:?}"
            );
            assert!(
                err.to_string().contains("embedding contract changed"),
                "{what}: {err}"
            );
        }

        // A dim mismatch is refused by the same comparison; the probe is sized to the
        // expected contract so the refusal is the contract check, not the width guard.
        let err = store
            .vector_candidates_checked(&sid, &[1.0, 0.0], &vec_contract(2), 5)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
        assert!(
            err.to_string().contains("embedding contract changed"),
            "{err}"
        );

        // A probe that disagrees with the contract it claims is a caller bug, not a
        // silently zero-scored scan (`cosine` returns 0.0 on a length mismatch).
        let err = store
            .vector_candidates_checked(&sid, &[1.0, 0.0], &durable, 5)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
        assert!(err.to_string().contains("dimensions"), "{err}");
    }

    /// F1: nothing-to-search is an empty answer, never an error — the shape a
    /// vector-capable store returns before its first embedding lands. Recall must not
    /// fail on a fresh database.
    #[tokio::test]
    async fn vector_candidates_are_empty_before_the_first_vector() {
        let store = vec_test_store(4);
        store.init_schema().await.unwrap();
        let contract = vec_contract(4);
        let probe = [1.0f32, 0.0, 0.0, 0.0];

        // A session no writer has touched.
        assert!(store
            .vector_candidates_checked(&SessionId::from("never-written"), &probe, &contract, 5)
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .vector_candidates(&SessionId::from("never-written"), &probe, 5)
            .await
            .unwrap()
            .is_empty());

        // A session with rows but no durable contract: legacy vectors are quarantined
        // at materialization, so an unstamped session is an empty pool.
        let sid = SessionId::from("vec-unstamped");
        let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        let origin = NodeId::new();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, origin, None, ts),
                        plant_concept(
                            &sid,
                            NodeId::new(),
                            origin,
                            "unstamped",
                            ConceptType::Entity,
                            ts,
                        ),
                    ],
                },
                None,
            )
            .await
            .unwrap();
        assert!(store
            .vector_candidates_checked(&sid, &probe, &contract, 5)
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .vector_candidates(&sid, &probe, 5)
            .await
            .unwrap()
            .is_empty());

        // A stamped session whose concepts carry no vectors yet.
        let stamped = SessionId::from("vec-stamped-empty");
        seed_vectors(&store, &stamped, &contract, &[]).await;
        assert!(store
            .vector_candidates_checked(&stamped, &probe, &contract, 5)
            .await
            .unwrap()
            .is_empty());
    }

    /// F1: a stored BLOB whose decoded width disagrees with the session contract is
    /// corruption, reported — not truncated, and not silently ranked last by `cosine`'s
    /// length-mismatch 0.0.
    #[tokio::test]
    async fn vector_candidates_refuse_a_malformed_stored_blob() {
        let store = vec_test_store(4);
        store.init_schema().await.unwrap();
        let sid = SessionId::from("vec-corrupt");
        let contract = vec_contract(4);
        let id = NodeId::new();
        seed_vectors(
            &store,
            &sid,
            &contract,
            &[(id, "stored", vec![1.0, 0.0, 0.0, 0.0])],
        )
        .await;
        let probe = [1.0f32, 0.0, 0.0, 0.0];

        // Direct SQL is used here because it is the only way *left* to produce these:
        // the write path encodes through the shared codec, and since F-R1-1
        // `enforce_concept_vector_widths` refuses a width-mismatched vector at the
        // flush gate too (`vector_write_gate_refuses_a_concept_of_the_wrong_width` covers
        // that surface). What remains reachable is an externally edited database —
        // which is exactly why the read path still cannot trust the stored width.
        for (what, blob) in [
            ("short", b"[1,0,0]".to_vec()),
            ("long", b"[1,0,0,0,0]".to_vec()),
            ("unparseable", b"[1,0,oops,0]".to_vec()),
            ("not utf-8", vec![0xff, 0xfe, 0x00]),
        ] {
            sqlx::query("UPDATE concepts SET embedding = ? WHERE id = ?")
                .bind(&blob)
                .bind(id.0.to_string())
                .execute(store.pool())
                .await
                .unwrap();
            let err = store
                .vector_candidates_checked(&sid, &probe, &contract, 5)
                .await
                .unwrap_err();
            assert!(
                matches!(err, StoreError::Backend(_)),
                "{what} blob must be a Backend error, got {err:?}"
            );
        }
    }

    /// F-R1-1: the **write gate**. The review reproduced session-wide, permanent
    /// recall failure from one `flush` through the public `GraphStore` surface — no
    /// direct SQL: a batch stamping `dim = 4` and then upserting one 4-wide and one
    /// 3-wide concept was accepted, after which `vector_candidates_checked`,
    /// `vector_candidates` and `recall::candidates::gather` all failed for the whole
    /// session, including the *good* concept. This is that repro, expressed through
    /// the same public types, asserting the batch is now refused instead.
    ///
    /// The external-crate form the reviewer used is expressible in-crate: `flush`,
    /// `MutationBatch`, `Mutation`, `Node` and `Concept` are all public, and
    /// `SqliteStore` reaches them through the same trait. What the in-crate form
    /// cannot show is a *foreign* caller, which is immaterial — the gate is in the
    /// adapter, below the trait boundary.
    #[tokio::test]
    async fn vector_write_gate_refuses_a_concept_of_the_wrong_width() {
        let store = vec_test_store(4);
        store.init_schema().await.unwrap();
        let sid = SessionId::from("vec-write-gate");
        let contract = vec_contract(4);
        let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        let origin = NodeId::new();
        let good = NodeId::new();
        let bad = NodeId::new();

        // The reviewer's batch, verbatim in shape: interaction, SetEmbedding{dim:4},
        // a 4-wide concept, then a 3-wide one.
        let batch = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, origin, None, ts),
                Mutation::SetEmbedding {
                    session_id: sid.clone(),
                    embedding: Some(contract.clone()),
                },
                plant_concept_with_vector(&sid, good, origin, "good", ts, vec![1.0, 0.0, 0.0, 0.0]),
                plant_concept_with_vector(&sid, bad, origin, "bad", ts, vec![1.0, 0.0, 0.0]),
            ],
        };
        let err = store.flush(&batch, None).await.unwrap_err();
        assert!(
            matches!(err, StoreError::Invariant(_)),
            "a width-mismatched concept must be refused as Invariant, got {err:?}"
        );
        let msg = err.to_string();
        for needle in ["3-dimensional", "stores vectors of 4"] {
            assert!(msg.contains(needle), "message must name both widths: {msg}");
        }

        // The refusal is atomic: `?` inside the flush transaction rolls the whole
        // batch back, so the *good* concept did not land either. That matters — a
        // half-applied batch would leave the caller unable to reason about retry.
        assert!(matches!(
            store.load_session(&sid).await.unwrap_err(),
            StoreError::SessionNotFound(_)
        ));

        // And the session is still usable: the same batch without the bad row
        // succeeds, and its vector leg answers. Before the gate, the session was
        // permanently poisoned at this point.
        seed_vectors(
            &store,
            &sid,
            &contract,
            &[(good, "good", vec![1.0, 0.0, 0.0, 0.0])],
        )
        .await;
        let hits = store
            .vector_candidates_checked(&sid, &[1.0, 0.0, 0.0, 0.0], &contract, 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "the surviving session still answers");
        assert_eq!(hits[0].item, good);
    }

    /// F-R1-1, the ordering half: what the contract *is* at the moment each concept
    /// is validated. `plan_flush` makes `SetEmbedding` a barrier, so this is a
    /// property of the planner rather than of statement luck.
    #[tokio::test]
    async fn vector_write_gate_reads_the_contract_each_step_sees() {
        let store = vec_test_store(4);
        store.init_schema().await.unwrap();
        let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();

        // (a) Stamp-then-upsert inside ONE batch passes: the concepts are planned
        // after the barrier, so they are validated against the width just stamped —
        // a session's very first vectors always arrive this way.
        let fresh = SessionId::from("gate-stamp-then-upsert");
        let origin = NodeId::new();
        let c = NodeId::new();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&fresh, origin, None, ts),
                        Mutation::SetEmbedding {
                            session_id: fresh.clone(),
                            embedding: Some(vec_contract(4)),
                        },
                        plant_concept_with_vector(
                            &fresh,
                            c,
                            origin,
                            "after the stamp",
                            ts,
                            vec![1.0, 0.0, 0.0, 0.0],
                        ),
                    ],
                },
                None,
            )
            .await
            .expect("stamp-then-upsert of a matching width must pass");

        // (b) A concept upserted BEFORE the stamp is validated against the contract
        // that was durable when it was written — here none, so it is accepted, and
        // `set_embedding`'s quarantine then NULLs it rather than leaving a vector the
        // new contract cannot interpret. This is why "no contract stamped yet" is
        // safe to accept at the gate.
        let pre = SessionId::from("gate-upsert-then-stamp");
        let origin2 = NodeId::new();
        let c2 = NodeId::new();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&pre, origin2, None, ts),
                        plant_concept_with_vector(
                            &pre,
                            c2,
                            origin2,
                            "before any stamp",
                            ts,
                            // Deliberately not 4 wide: with no contract there is no
                            // authority to check it against.
                            vec![1.0, 0.0, 0.0],
                        ),
                        Mutation::SetEmbedding {
                            session_id: pre.clone(),
                            embedding: Some(vec_contract(4)),
                        },
                    ],
                },
                None,
            )
            .await
            .expect("a vector written before any contract is accepted");
        let loaded = store.load_session(&pre).await.unwrap();
        assert_eq!(loaded.embedding.as_ref(), Some(&vec_contract(4)));
        assert_eq!(
            loaded.concepts[0].embedding, None,
            "quarantined by set_embedding: stamping a width NULLs every vector of a \
             different width, and from a NULL contract that is all of them"
        );
        // Consequently the read path is clean rather than poisoned.
        assert!(store
            .vector_candidates_checked(&pre, &[1.0, 0.0, 0.0, 0.0], &vec_contract(4), 5)
            .await
            .unwrap()
            .is_empty());
    }

    /// F-R2-1, the ordering round 2 found: a **restamp**. Every concept can match the
    /// contract of its own moment — so `enforce_concept_vector_widths` has nothing to
    /// refuse — while the *final* contract disagrees with a vector written earlier in
    /// the same batch. Round 2 reproduced exactly this through one public `flush` with
    /// no direct SQL and reached the terminal state round 1 called fatal: a 4-wide
    /// vector under a `dim = 3` contract, after which the whole session's vector leg
    /// fails on every read because `select_session_vectors` returns on the first bad
    /// row.
    ///
    /// The fix is on the other side of the barrier — `set_embedding` now NULLs every
    /// vector of a different width when it stamps — so the batch is **accepted and
    /// self-heals** rather than refused: the earlier vector is erased, and the
    /// terminal state is a `dim = 3` contract beside 3-wide vectors only. This test
    /// asserts the terminal state is clean, which is the property that matters; it
    /// would fail identically if a future change made the batch pass *and* keep the
    /// orphan.
    #[tokio::test]
    async fn vector_write_gate_restamp_inside_one_batch_cannot_orphan_earlier_vectors() {
        let store = vec_test_store(4);
        store.init_schema().await.unwrap();
        let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        let sid = SessionId::from("gate-restamp-one-batch");
        let origin = NodeId::new();
        let wide = NodeId::new();
        let narrow = NodeId::new();

        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, origin, None, ts),
                        Mutation::SetEmbedding {
                            session_id: sid.clone(),
                            embedding: Some(vec_contract(4)),
                        },
                        plant_concept_with_vector(
                            &sid,
                            wide,
                            origin,
                            "written under dim 4",
                            ts,
                            vec![1.0, 0.0, 0.0, 0.0],
                        ),
                        Mutation::SetEmbedding {
                            session_id: sid.clone(),
                            embedding: Some(vec_contract(3)),
                        },
                        plant_concept_with_vector(
                            &sid,
                            narrow,
                            origin,
                            "written under dim 3",
                            ts,
                            vec![1.0, 0.0, 0.0],
                        ),
                    ],
                },
                None,
            )
            .await
            .expect("every concept matches the contract of its own step, so nothing is refused");

        assert_restamp_left_no_orphan(&store, &sid, wide, narrow).await;
    }

    /// F-R2-1 across a flush boundary — the shape an operator actually produces,
    /// since a restamp normally follows a commit rather than sharing a batch with the
    /// vectors it invalidates. Round 2 reproduced both; the quarantine is a property
    /// of the `SetEmbedding` statement, so both must end clean for the same reason.
    #[tokio::test]
    async fn vector_write_gate_restamp_across_two_flushes_cannot_orphan_earlier_vectors() {
        let store = vec_test_store(4);
        store.init_schema().await.unwrap();
        let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        let sid = SessionId::from("gate-restamp-two-flushes");
        let origin = NodeId::new();
        let wide = NodeId::new();
        let narrow = NodeId::new();

        // Flush 1: an ordinary, entirely legal dim-4 session.
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, origin, None, ts),
                        Mutation::SetEmbedding {
                            session_id: sid.clone(),
                            embedding: Some(vec_contract(4)),
                        },
                        plant_concept_with_vector(
                            &sid,
                            wide,
                            origin,
                            "written under dim 4",
                            ts,
                            vec![1.0, 0.0, 0.0, 0.0],
                        ),
                    ],
                },
                None,
            )
            .await
            .expect("a dim-4 session with 4-wide vectors is legal");
        // Its read path answers, so the state being repaired below is a live one.
        assert_eq!(
            store
                .vector_candidates_checked(&sid, &[1.0, 0.0, 0.0, 0.0], &vec_contract(4), 5)
                .await
                .unwrap()
                .len(),
            1
        );

        // Flush 2: restamp to dim 3 and write a 3-wide concept. The already-committed
        // 4-wide vector is the one the old NULL-only quarantine left behind.
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        Mutation::SetEmbedding {
                            session_id: sid.clone(),
                            embedding: Some(vec_contract(3)),
                        },
                        plant_concept_with_vector(
                            &sid,
                            narrow,
                            origin,
                            "written under dim 3",
                            ts,
                            vec![1.0, 0.0, 0.0],
                        ),
                    ],
                },
                None,
            )
            .await
            .expect("the restamp and the concepts that follow it are each locally valid");

        assert_restamp_left_no_orphan(&store, &sid, wide, narrow).await;
    }

    /// Shared terminal-state assertion for the two restamp orderings: the durable
    /// contract is the restamped one, the vector written under the *old* width is
    /// gone rather than orphaned, the vector written under the new width survives,
    /// and the read path answers cleanly instead of failing session-wide.
    async fn assert_restamp_left_no_orphan(
        store: &SqliteStore,
        sid: &SessionId,
        wide: NodeId,
        narrow: NodeId,
    ) {
        let loaded = store.load_session(sid).await.unwrap();
        assert_eq!(
            loaded.embedding.as_ref(),
            Some(&vec_contract(3)),
            "the last stamp is the durable contract"
        );
        let find = |id: NodeId| {
            loaded
                .concepts
                .iter()
                .find(|c| c.id == id)
                .expect("concept present")
                .embedding
                .clone()
        };
        assert_eq!(
            find(wide),
            None,
            "the 4-wide vector must be quarantined by the dim-3 stamp, not left \
             under a contract that says 3"
        );
        assert_eq!(
            find(narrow),
            Some(vec![1.0f32, 0.0, 0.0]),
            "a vector written under the new contract is untouched"
        );
        // The whole point: before F-R2-1 both of these were `Backend` errors for the
        // entire session — including for the good concept — permanently.
        let hits = store
            .vector_candidates_checked(sid, &[1.0, 0.0, 0.0], &vec_contract(3), 5)
            .await
            .expect("the session's vector leg still answers after a restamp");
        assert_eq!(
            hits.len(),
            1,
            "only the surviving 3-wide vector is a candidate"
        );
        assert_eq!(hits[0].item, narrow);
        // The frozen surface reads the stored contract itself, so it must agree.
        let frozen = store
            .vector_candidates(sid, &[1.0, 0.0, 0.0], 5)
            .await
            .expect("the frozen surface is clean too");
        assert_eq!(frozen, hits);
    }

    /// The scope decision F-R2-1 forced, pinned so it cannot be "fixed" into a
    /// data-losing quarantine-on-any-change: the quarantine keys on **width**, so a
    /// same-width `kind`/`model` relabel leaves every vector in place. That is not an
    /// oversight — `Graph::replace_embedding_with_operator_override`, the
    /// `--allow-embedding-mismatch` writer attach path, requires equal widths and
    /// deliberately permits a same-kind model-identifier rename *with the vectors
    /// intact*. Erasing them here would destroy data on the one migration path built
    /// to keep it, and width is the only contract property this storage can enforce:
    /// a same-width relabel leaves every BLOB decodable.
    ///
    /// Both halves of the `kind`/`model` property are restamped here (F-R3-2): a
    /// model-identifier rename first, then a `kind` change at the same width. The
    /// second is the case the two tiers deliberately disagree on — the graph tier's
    /// `replace_embedding_with_operator_override` *refuses* a `kind` change while any
    /// vector remains, where storage keys on width alone and keeps them.
    #[tokio::test]
    async fn vector_write_gate_same_width_relabel_keeps_the_vectors() {
        let store = vec_test_store(4);
        store.init_schema().await.unwrap();
        let sid = SessionId::from("gate-same-width-relabel");
        let c = NodeId::new();
        seed_vectors(
            &store,
            &sid,
            &vec_contract(4),
            &[(c, "kept across a rename", vec![1.0, 0.0, 0.0, 0.0])],
        )
        .await;

        let renamed = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("test-model-v2".into()),
            dim: 4,
        };
        store
            .flush(
                &MutationBatch {
                    mutations: vec![Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(renamed.clone()),
                    }],
                },
                None,
            )
            .await
            .unwrap();

        let loaded = store.load_session(&sid).await.unwrap();
        assert_eq!(loaded.embedding.as_ref(), Some(&renamed));
        assert_eq!(
            loaded.concepts[0].embedding.as_deref(),
            Some([1.0f32, 0.0, 0.0, 0.0].as_slice()),
            "a same-width relabel must not erase vectors the operator declared compatible"
        );
        let hits = store
            .vector_candidates_checked(&sid, &[1.0, 0.0, 0.0, 0.0], &renamed, 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item, c);

        // F-R3-2: the `kind` half, at the same width. Without this a regression
        // widening the predicate to fire on a `kind` change would pass the rename
        // above unchanged.
        let rekinded = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("test-model-v2".into()),
            dim: 4,
        };
        store
            .flush(
                &MutationBatch {
                    mutations: vec![Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(rekinded.clone()),
                    }],
                },
                None,
            )
            .await
            .unwrap();
        let loaded = store.load_session(&sid).await.unwrap();
        assert_eq!(loaded.embedding.as_ref(), Some(&rekinded));
        assert_eq!(
            loaded.concepts[0].embedding.as_deref(),
            Some([1.0f32, 0.0, 0.0, 0.0].as_slice()),
            "a same-width kind change must not erase vectors either"
        );
        let hits = store
            .vector_candidates_checked(&sid, &[1.0, 0.0, 0.0, 0.0], &rekinded, 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item, c);
    }

    /// F1 boundaries: a top-k above the candidate count returns the whole pool, and
    /// `k = 0` returns empty without touching the database.
    #[tokio::test]
    async fn vector_candidates_boundaries_top_k_and_zero() {
        let store = vec_test_store(4);
        store.init_schema().await.unwrap();
        let sid = SessionId::from("vec-bounds");
        let contract = vec_contract(4);
        // Distinct content: the partial unique index on (session_id, canonical_key)
        // rejects duplicate non-Observation keys.
        let labels = ["c0", "c1", "c2"];
        let seeded: Vec<(NodeId, &str, Vec<f32>)> = labels
            .iter()
            .enumerate()
            .map(|(i, label)| (NodeId::new(), *label, vec![1.0, i as f32 * 0.25, 0.0, 0.0]))
            .collect();
        seed_vectors(&store, &sid, &contract, &seeded).await;
        let probe = [1.0f32, 0.0, 0.0, 0.0];

        let all = store
            .vector_candidates_checked(&sid, &probe, &contract, 100)
            .await
            .unwrap();
        assert_eq!(all.len(), 3, "top-k above the pool returns the whole pool");
        let one = store
            .vector_candidates_checked(&sid, &probe, &contract, 1)
            .await
            .unwrap();
        assert_eq!(one, all[..1].to_vec(), "k truncates the same ranking");
        assert!(store
            .vector_candidates_checked(&sid, &probe, &contract, 0)
            .await
            .unwrap()
            .is_empty());
        // k = 0 short-circuits before the contract is even read, so a contract that
        // would otherwise be refused still returns empty rather than erroring.
        assert!(store
            .vector_candidates_checked(&sid, &probe, &vec_contract(999), 0)
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .vector_candidates(&sid, &probe, 0)
            .await
            .unwrap()
            .is_empty());
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
        store.flush(&batch, None).await.unwrap();
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
        store.flush(&replace, None).await.unwrap();
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
        store.flush(&clear, None).await.unwrap();
        assert_eq!(store.load_session(&sid).await.unwrap().root_goal, None);

        #[cfg(feature = "store-memory")]
        {
            let memory = MemoryStore::new();
            memory.flush(&batch, None).await.unwrap();
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
            .flush(
                &MutationBatch {
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
                },
                None,
            )
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
        let err = crate::resolve::assert_session_embedding_compatible(
            reloaded.embedding(),
            &incompatible,
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("fixture-v1"), "{text}");
        assert!(text.contains("amazon.titan-embed-text-v2:0"), "{text}");
        assert!(text.contains("--allow-embedding-mismatch"), "{text}");

        #[cfg(feature = "store-memory")]
        {
            let memory = MemoryStore::new();
            memory
                .flush(
                    &MutationBatch {
                        mutations: vec![Mutation::SetEmbedding {
                            session_id: sid.clone(),
                            embedding: Some(contract.clone()),
                        }],
                    },
                    None,
                )
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
        store.flush(&batch, None).await.unwrap();
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
            memory.flush(&batch, None).await.unwrap();
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
            .flush(
                &MutationBatch {
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
                },
                None,
            )
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
            .flush(
                &MutationBatch {
                    mutations: vec![Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(EmbeddingContract {
                            kind: "fixture".into(),
                            model: Some("fixture-v1".into()),
                            dim: 3,
                        }),
                    }],
                },
                None,
            )
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
        // `dim` matches the seeded concept's vector width below. It used to read
        // 1024 against a 3-wide vector — incidental to what this test asserts
        // (STORE-1 contract persistence + CON-8 embedding round-trip), but a
        // snapshot no writer should be able to produce: since F-R1-1 the width gate
        // refuses it on the seed path too, which is the point of the gate.
        let contract = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("BAAI/bge-m3".into()),
            dim: 3,
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
            .flush(
                &MutationBatch {
                    mutations: vec![plant_interaction(
                        &sid,
                        NodeId::new(),
                        None,
                        Utc.timestamp_opt(1_752_003_600, 0).unwrap(),
                    )],
                },
                None,
            )
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
        sqlite.flush(&batch, None).await.unwrap();

        let memory = MemoryStore::new();
        memory.flush(&batch, None).await.unwrap();

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

        store.flush(&batch, None).await.unwrap();
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
        store.flush(&batch, None).await.unwrap();
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
        sqlite.flush(&batch, None).await.unwrap();
        memory.flush(&batch, None).await.unwrap();

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

    /// An **exact-cosine oracle** with the ordering contract of `MemoryStore`'s
    /// `VectorSearchStore` and of `rank_by_cosine`: best score first by `total_cmp`
    /// descending, ties broken by the smaller `NodeId`, then truncated to `limit`.
    ///
    /// Reimplemented here rather than reused because `VectorSearchStore` is private to
    /// `memory.rs`'s test module. That is not a weakness of the comparison: the oracle
    /// is deliberately the naive formulation — score everything, sort, truncate — with
    /// no transaction, no BLOB codec, no SQL and no width checking, so agreement is
    /// evidence about the adapter rather than about shared code.
    #[cfg(feature = "fixtures")]
    fn cosine_oracle(
        probe: &[f32],
        pool: &[(NodeId, Vec<f32>)],
        limit: usize,
    ) -> Vec<Scored<NodeId>> {
        let mut scored: Vec<Scored<NodeId>> = pool
            .iter()
            .map(|(id, v)| Scored::new(*id, f64::from(crate::embed::cosine(probe, v))))
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.item.0.cmp(&b.item.0))
        });
        scored.truncate(limit);
        scored
    }

    /// A deterministic **unit-norm** vector for fixture concept `i`.
    ///
    /// Unit norm is the `Embedder` output contract (see `embed::Embedder::embed`), and
    /// the property the SQLite/Cockroach score identity rests on, so synthetic vectors
    /// that stand in for embedder output must honour it too. The spread is deliberately
    /// uneven — not one-hot — so distinct concepts produce distinct, non-orthogonal
    /// scores and a ranking has something to get wrong.
    #[cfg(feature = "fixtures")]
    fn synthetic_unit_vector(i: usize, dim: usize) -> Vec<f32> {
        let mut v: Vec<f32> = (0..dim)
            .map(|j| {
                // Cheap deterministic spread; no rand dependency, stable across runs
                // and platforms (integer math, then one divide).
                let k = ((i + 1) * 37 + (j + 1) * 11) % 97;
                (k as f32) / 97.0 - 0.5
            })
            .collect();
        // Guard the degenerate case rather than dividing by ~0.
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(norm > 1e-6, "degenerate synthetic vector for i={i}");
        for x in &mut v {
            *x /= norm;
        }
        v
    }

    /// **Acceptance: Done-when box 5, the cluster-free half** (F-R1-4).
    ///
    /// The vector agreement matrix, in the shape of
    /// `assert_structural_agreement_matrix`: both committed fixture graphs, plus a
    /// stamped contract and synthetic unit vectors, seeded into `SqliteStore` and into
    /// an exact-cosine oracle; every returned `Vec<Scored<NodeId>>` asserted **exactly
    /// equal** across probes × limits — same ids, same order, same `f64` scores.
    ///
    /// What this replaces: `vector_scores_match_the_cockroach_distance_conversion`
    /// *transcribes* Cockroach's `distance_to_score` into its own body, so it proves
    /// the formula was copied correctly and nothing about which candidates or which
    /// ranks an adapter returns. This asserts the answers themselves, over real
    /// committed graphs, through the whole adapter path — flush → BLOB codec →
    /// transaction → scan → rank.
    ///
    /// The probe set is chosen so ranking mistakes are visible: a stored vector itself
    /// (score 1.0, must rank first), its negation (score −1.0, must rank *last* — this
    /// is what catches a `sort` that mishandles sign or a `total_cmp` swapped for
    /// `partial_cmp`), a midpoint between two stored vectors, and an off-axis probe.
    /// Limits sweep 1 → past the pool size to pin truncation and the whole-pool case.
    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn vector_candidates_agree_with_an_exact_cosine_oracle_on_both_fixtures() {
        const DIM: usize = 8;
        let mut total_assertions = 0usize;
        for fixture in ["session-rest-api", "session-drift"] {
            let snap: GraphSnapshot = crate::fixtures::load_snapshot(fixture).unwrap();
            let sid = snap.session_id.clone();
            let contract = vec_contract(DIM);

            // The fixtures carry no contract and no vectors (asserted below, so this
            // stays honest if a fixture ever gains them). Attach both.
            assert!(
                snap.embedding.is_none() && snap.concepts.iter().all(|c| c.embedding.is_none()),
                "{fixture}: fixture is expected to carry no vectors; \
                 this test supplies them"
            );

            let pool: Vec<(NodeId, Vec<f32>)> = snap
                .concepts
                .iter()
                .enumerate()
                .map(|(i, c)| (c.id, synthetic_unit_vector(i, DIM)))
                .collect();

            let store = vec_test_store(DIM);
            store.init_schema().await.unwrap();

            // Structure first (interactions are FK targets for concepts), then the
            // contract, then the vector-bearing concepts. `SetEmbedding` is a planner
            // barrier, so the concepts are validated against the width it just
            // stamped — the ordering the write gate depends on (F-R1-1).
            let mut mutations = snapshot_to_batch(&snap).mutations;
            mutations.push(Mutation::SetEmbedding {
                session_id: sid.clone(),
                embedding: Some(contract.clone()),
            });
            for (i, c) in snap.concepts.iter().enumerate() {
                let mut concept = c.clone();
                concept.embedding = Some(synthetic_unit_vector(i, DIM));
                mutations.push(Mutation::UpsertNode {
                    node: NodeKind::Concept(concept),
                });
            }
            store
                .flush(&MutationBatch { mutations }, None)
                .await
                .unwrap();

            // Probes, in the pool's own space so scores span [-1, 1].
            let first = &pool[0].1;
            let second = &pool[1].1;
            let negated: Vec<f32> = first.iter().map(|x| -x).collect();
            let midpoint: Vec<f32> = {
                let mut m: Vec<f32> = first
                    .iter()
                    .zip(second.iter())
                    .map(|(a, b)| (a + b) / 2.0)
                    .collect();
                let n = m.iter().map(|x| x * x).sum::<f32>().sqrt();
                for x in &mut m {
                    *x /= n;
                }
                m
            };
            let off_axis = synthetic_unit_vector(usize::from(u8::MAX), DIM);
            let probes: [(&str, Vec<f32>); 4] = [
                ("stored-itself", first.clone()),
                ("negated", negated),
                ("midpoint", midpoint),
                ("off-axis", off_axis),
            ];

            for (label, probe) in &probes {
                for limit in [1usize, 3, 5, pool.len(), pool.len() + 7] {
                    let got = store
                        .vector_candidates_checked(&sid, probe, &contract, limit)
                        .await
                        .unwrap();
                    let want = cosine_oracle(probe, &pool, limit);
                    assert_eq!(
                        got, want,
                        "{fixture}: probe {label}, limit {limit} — SQLite disagrees \
                         with the exact-cosine oracle"
                    );
                    total_assertions += 1;
                }
            }

            // Anchors independent of the oracle, so a bug shared by both would still
            // be caught: a stored vector scores 1.0 against itself and ranks first;
            // its negation scores -1.0 and ranks LAST of the whole pool.
            let top = store
                .vector_candidates_checked(&sid, first, &contract, pool.len())
                .await
                .unwrap();
            assert_eq!(top[0].item, pool[0].0);
            assert!((top[0].score - 1.0).abs() < 1e-6, "{:?}", top[0]);
            let bottom = store
                .vector_candidates_checked(
                    &sid,
                    &first.iter().map(|x| -x).collect::<Vec<f32>>(),
                    &contract,
                    pool.len(),
                )
                .await
                .unwrap();
            assert_eq!(bottom.len(), pool.len());
            assert_eq!(bottom[pool.len() - 1].item, pool[0].0);
            assert!(
                (bottom[pool.len() - 1].score + 1.0).abs() < 1e-6,
                "{:?}",
                bottom[pool.len() - 1]
            );
        }
        // 2 fixtures × 4 probes × 5 limits.
        assert_eq!(total_assertions, 40, "matrix dimensions drifted");
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
            sqlite.flush(&batch, None).await.unwrap();
            let memory = MemoryStore::new();
            memory.flush(&batch, None).await.unwrap();

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
        store.flush(&batch, None).await.unwrap();
        let memory = MemoryStore::new();
        memory.flush(&batch, None).await.unwrap();

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
        store.flush(&local, None).await.unwrap();
        store.flush(&elsewhere, None).await.unwrap();
        let memory = MemoryStore::new();
        memory.flush(&local, None).await.unwrap();
        memory.flush(&elsewhere, None).await.unwrap();

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
        store.flush(&base, None).await.unwrap();
        let memory = MemoryStore::new();
        memory.flush(&base, None).await.unwrap();
        // Then a genuinely FRESH other -> orphan dependency (created now).
        let fresh = MutationBatch {
            mutations: vec![plant_edge(&sid, other, orphan, EdgeType::Dependency, now)],
        };
        store.flush(&fresh, None).await.unwrap();
        memory.flush(&fresh, None).await.unwrap();

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
        store.flush(&batch, None).await.unwrap();
        let memory = MemoryStore::new();
        memory.flush(&batch, None).await.unwrap();

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
        store.flush(&batch, None).await.unwrap();
        let memory = MemoryStore::new();
        memory.flush(&batch, None).await.unwrap();

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
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, None, ts),
                        plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts),
                    ],
                },
                None,
            )
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
            .flush(
                &MutationBatch {
                    mutations: vec![Mutation::CanonizationTransition { event: ev1.clone() }],
                },
                None,
            )
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
        store.record_canonization(&ev2, None).await.unwrap();
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
        let err = store.record_canonization(&ghost, None).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));

        // F12: the write-behind log now replays ev1, which was already
        // recorded. The replay must be a no-op — not a status rollback to
        // Candidate, and not a duplicate audit row.
        store
            .flush(
                &MutationBatch {
                    mutations: vec![Mutation::CanonizationTransition { event: ev1.clone() }],
                },
                None,
            )
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
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, None, ts),
                        plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts),
                    ],
                },
                None,
            )
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
        store.record_canonization(&ev, None).await.unwrap();

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
        store.record_canonization(&promo, None).await.unwrap();
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
            .flush(
                &MutationBatch {
                    mutations: vec![plant_interaction(&sid, i1, None, ts), plant.clone()],
                },
                None,
            )
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
        store.record_canonization(&hop, None).await.unwrap();

        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        with_stale_canonization(plant, CanonizationStatus::None, None, None),
                        Mutation::CanonizationTransition { event: hop },
                    ],
                },
                None,
            )
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

    /// **L82-1 on a real SQL engine.** Two upserts of the same concept in ONE
    /// batch must collapse to the durable row row-by-row replay produced.
    ///
    /// This is the case the multi-row rewrite could silently get wrong, and the
    /// one no reasoning-by-inspection settles: both engines *reject* a
    /// statement whose input rows collide on the conflict target, so the rows
    /// must be deduplicated — and a naive "last wins" would take the second
    /// snapshot's canonization columns, which the row-by-row `ON CONFLICT DO
    /// UPDATE` (R2-1: canonization columns are INSERT-only) would have
    /// discarded. `store::batch::ConceptRow` keeps the first occurrence's
    /// canonization and the last occurrence's everything-else; this executes
    /// that against SQLite and reads the row back.
    #[tokio::test]
    async fn a_repeated_concept_in_one_batch_collapses_like_row_by_row_replay() {
        let store = test_store();
        store.init_schema().await.unwrap();

        let sid = SessionId::from("l82-1-dedupe");
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        let ts = Utc.with_ymd_and_hms(2026, 8, 14, 12, 0, 0).unwrap();

        // The concept is born mid-progression: its first appearance in the
        // batch already carries a status, and a later appearance (a GC
        // `bump_gc_survived` snapshot taken before the hop) does not.
        let born = with_stale_canonization(
            plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts),
            CanonizationStatus::Canonical,
            Some(9),
            Some(ts),
        );
        let Mutation::UpsertNode {
            node: NodeKind::Concept(mut later),
        } = plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts)
        else {
            unreachable!("plant_concept builds a concept upsert")
        };
        later.gc_survived = 4;
        later.content = "pillar (revised)".into();

        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, None, ts),
                        born,
                        Mutation::UpsertNode {
                            node: NodeKind::Concept(later),
                        },
                    ],
                },
                None,
            )
            .await
            .expect("a batch with a repeated id must not be rejected as a duplicate conflict");

        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.concepts.len(), 1, "one row, not two");
        let row = &snap.concepts[0];
        assert_eq!(
            row.content, "pillar (revised)",
            "ordinary columns: last wins"
        );
        assert_eq!(row.gc_survived, 4, "ordinary columns: last wins");
        assert_eq!(
            row.canonization_status,
            CanonizationStatus::Canonical,
            "the INSERT carries the FIRST occurrence's canonization columns — row-by-row \
             replay would have inserted them and then skipped them on the DO UPDATE (R2-1)"
        );
        assert_eq!(row.blast_radius, Some(9));
        assert_eq!(row.last_demotion_time, Some(ts));
    }

    /// **R1-1, the pin — against a real SQL engine with foreign keys on.**
    ///
    /// `interactions.previous_id REFERENCES interactions(id)` is a *self* FK.
    /// A planner that collapses a repeated interaction at its LAST position
    /// re-emits `i1` after `i2(prev=i1)`; with `BULK_LIMITS.interactions == 1`
    /// each is its own statement, SQLite checks the FK at end-of-statement, and
    /// `i2` fails with `SQLITE_CONSTRAINT_FOREIGNKEY` (787). `Constraint` is
    /// terminal, so the flush loop dead-letters the whole batch — the same loss
    /// class L82-1 was raised for.
    ///
    /// The second half is the control the reviewer used to prove *relocation* is
    /// the cause and not the duplicate: with the repeat adjacent, nothing moves
    /// past `i2`, and even the broken planner returns `Ok`.
    #[tokio::test]
    async fn a_repeated_interaction_does_not_outrun_the_row_that_chains_onto_it() {
        let store = test_store();
        store.init_schema().await.unwrap();

        let ts = Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).unwrap();

        // CONTROL FIRST, so that it still executes when the assertion below
        // fails: the same duplicate, adjacent. Nothing is relocated past i2
        // under either rule, so this passes with or without the fix — which is
        // what makes the second half evidence about *relocation* specifically
        // rather than about duplicates.
        let sid = SessionId::from("r1-1-adjacent-control");
        let i1 = NodeId::new();
        let i2 = NodeId::new();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, None, ts),
                        plant_interaction(&sid, i1, None, ts),
                        plant_interaction(&sid, i2, Some(i1), ts),
                    ],
                },
                None,
            )
            .await
            .expect("the adjacent-repeat control must always pass");
        assert_eq!(
            store.load_session(&sid).await.unwrap().interactions.len(),
            2
        );

        // Non-adjacent re-upsert: i1, then i2 chaining onto i1, then i1 again.
        // `Graph::insert_interaction` permits a re-upsert that does not move the
        // interaction within the temporal chain, so `previous_id` is the same on
        // both occurrences.
        let sid = SessionId::from("r1-1-self-fk");
        let i1 = NodeId::new();
        let i2 = NodeId::new();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, None, ts),
                        plant_interaction(&sid, i2, Some(i1), ts),
                        plant_interaction(&sid, i1, None, ts),
                    ],
                },
                None,
            )
            .await
            .expect(
                "the repeated interaction must not be relocated past the row that references \
                 it — a self-FK violation here dead-letters the whole batch (R1-1)",
            );

        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.interactions.len(), 2, "one row per id");
        let chained = snap
            .interactions
            .iter()
            .find(|i| i.id == i2)
            .expect("i2 must be durable");
        assert_eq!(
            chained.previous_id,
            Some(i1),
            "the chain must survive the collapse"
        );
    }

    /// **L82-1.** A batch far larger than the per-statement row limit must
    /// round-trip whole — the chunking is what keeps a statement inside the
    /// backend's bind-parameter cap, and an off-by-one there loses rows
    /// silently rather than loudly.
    #[tokio::test]
    async fn a_batch_larger_than_the_chunk_limit_round_trips_whole() {
        let store = test_store();
        store.init_schema().await.unwrap();

        let sid = SessionId::from("l82-1-chunking");
        let i1 = NodeId::new();
        let ts = Utc.with_ymd_and_hms(2026, 8, 14, 12, 0, 0).unwrap();
        // Several chunks' worth on both buckets.
        let concepts = BULK_LIMITS.concepts * 3 + 7;
        let mut mutations = vec![plant_interaction(&sid, i1, None, ts)];
        let mut ids = Vec::with_capacity(concepts);
        for n in 0..concepts {
            let id = NodeId::new();
            ids.push(id);
            mutations.push(plant_concept(
                &sid,
                id,
                i1,
                &format!("concept {n}"),
                ConceptType::Entity,
                ts,
            ));
        }
        for n in 0..ids.len() - 1 {
            mutations.push(Mutation::UpsertEdge {
                edge: Edge {
                    id: NodeId::new(),
                    session_id: sid.clone(),
                    source: ids[n],
                    target: ids[n + 1],
                    edge_type: EdgeType::Causal,
                    weight: 1.0,
                    reinforcements: 1,
                    created_at: ts,
                    last_reinforced: ts,
                },
            });
        }
        assert!(
            concepts > BULK_LIMITS.concepts && ids.len() - 1 > BULK_LIMITS.edges,
            "both buckets must actually span multiple statements"
        );

        store
            .flush(&MutationBatch { mutations }, None)
            .await
            .unwrap();

        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(
            snap.concepts.len(),
            concepts,
            "every concept must be durable"
        );
        assert_eq!(
            snap.edges.len(),
            ids.len() - 1,
            "every edge must be durable"
        );
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
            .flush(
                &MutationBatch {
                    mutations: vec![plant_interaction(&sid, i1, None, ts), plant.clone()],
                },
                None,
            )
            .await
            .unwrap();

        store
            .record_canonization(
                &CanonizationEvent {
                    id: NodeId::new(),
                    session_id: sid.clone(),
                    node_id: c1,
                    from_status: CanonizationStatus::Venerable,
                    to_status: CanonizationStatus::Canonical,
                    blast_radius: Some(8),
                    last_demotion_time: None,
                    occurred_at: ts,
                },
                None,
            )
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
        store.record_canonization(&demote, None).await.unwrap();

        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        with_stale_canonization(
                            plant,
                            CanonizationStatus::Canonical,
                            Some(8),
                            None,
                        ),
                        Mutation::CanonizationTransition { event: demote },
                    ],
                },
                None,
            )
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
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, NodeId::new(), None, t1),
                        plant_interaction(&sid, NodeId::new(), None, t2),
                    ],
                },
                None,
            )
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
            .flush(
                &MutationBatch {
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
                },
                None,
            )
            .await
            .unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.concepts.len(), 3, "all three rows must persist");

        // A second Entity with the same canonical key violates the partial
        // unique index and must fail the flush (transaction rolled back).
        let e2 = NodeId::new();
        let err = store
            .flush(
                &MutationBatch {
                    mutations: vec![plant_concept(
                        &sid,
                        e2,
                        i1,
                        "duplicate sentence",
                        ConceptType::Entity,
                        ts,
                    )],
                },
                None,
            )
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
                s.flush(
                    &MutationBatch {
                        mutations: vec![
                            plant_interaction(&sid, i1, None, ts),
                            plant_concept(&sid, c1, i1, &format!("n{n}"), ConceptType::Entity, ts),
                        ],
                    },
                    None,
                )
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
            vector_dim: None,
        })
        .unwrap();
        assert!(crate::store::StoreKind::Sqlite.is_ready());
        // F2: the registry hands back a vector-capable adapter that reports a width.
        assert_eq!(store.capabilities(), Capabilities::VECTOR_SEARCH);
        assert_eq!(
            store.vector_dimensions(),
            Some(crate::embed::EmbedderConfig::default().dim),
            "a registry build with no configured width falls back to the `[embedder] dim` \
             default, never a width constant of its own"
        );

        store.init_schema().await.unwrap();
        let sid = SessionId::from("registry");
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        let ts = Utc::now();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, None, ts),
                        plant_concept(&sid, c1, i1, "registry concept", ConceptType::Entity, ts),
                    ],
                },
                None,
            )
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
                .flush(
                    &MutationBatch {
                        mutations: vec![
                            plant_interaction(&sid, i1, None, ts),
                            plant_concept(
                                &sid,
                                c1,
                                i1,
                                "file-backed concept",
                                ConceptType::Entity,
                                ts,
                            ),
                        ],
                    },
                    None,
                )
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
            endpoint: None,
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

    /// J2: the endpoint a holder publishes round-trips through the real SQL —
    /// out of the acquire's `RETURNING`, out of the loser's `Held` read-back,
    /// and out of the standalone `read_lease` the proxy path uses — and a
    /// refresh republishes it rather than dropping it.
    ///
    /// The NULL half is asserted too, and it is the load-bearing one: every
    /// writer that is not a `serve` process leaves the column NULL, and a
    /// refused serve must be able to tell "no hub here" from "a hub at <path>".
    #[tokio::test]
    async fn the_lease_endpoint_round_trips_and_a_refresh_republishes_it() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let sid = SessionId::from("s");
        let hub = lease_holder("agent-a", 100).reachable_at("/run/lambo/s-abc.sock");
        let loser = lease_holder("agent-b", 200);
        let ttl = Duration::from_secs(30);

        let LeaseOutcome::Acquired(taken) = store.acquire_lease(&sid, &hub, ttl).await.unwrap()
        else {
            panic!("the hub must win a fresh lease");
        };
        assert_eq!(taken.endpoint.as_deref(), Some("/run/lambo/s-abc.sock"));

        // The refusal a proxying serve reads: the endpoint arrives with the
        // holder, so one round trip tells the loser where to forward.
        match store.acquire_lease(&sid, &loser, ttl).await.unwrap() {
            LeaseOutcome::Held { current, .. } => {
                assert_eq!(current.holder, hub.token());
                assert_eq!(current.endpoint.as_deref(), Some("/run/lambo/s-abc.sock"));
            }
            other => panic!("expected Held, got {other:?}"),
        }

        // The read the proxy repeats on every reconnect attempt.
        let row = store.read_lease(&sid).await.unwrap().expect("a live row");
        assert_eq!(row.endpoint.as_deref(), Some("/run/lambo/s-abc.sock"));
        assert_eq!(row.holder, hub.token());

        // A refresh is the heartbeat; it must not blank the address.
        let LeaseOutcome::Acquired(refreshed) = store.refresh_lease(&sid, &hub, ttl).await.unwrap()
        else {
            panic!("the hub's own refresh must succeed");
        };
        assert_eq!(refreshed.endpoint.as_deref(), Some("/run/lambo/s-abc.sock"));

        // A writer that is not a serve process publishes nothing, and the row
        // says so — "no hub here" is a fact, not missing data.
        store.release_lease(&sid, &hub).await.unwrap();
        let LeaseOutcome::Acquired(cli) = store.acquire_lease(&sid, &loser, ttl).await.unwrap()
        else {
            panic!("a released lease must be re-acquirable");
        };
        assert_eq!(cli.endpoint, None);
        assert_eq!(
            store.read_lease(&sid).await.unwrap().unwrap().endpoint,
            None
        );
    }

    /// J2: `read_lease` on a session no writer has ever leased is `None`, not an
    /// error — a proxy must be able to distinguish "nobody holds this" from a
    /// store failure, because only the first one is worth retrying.
    #[tokio::test]
    async fn read_lease_is_none_for_an_unleased_session() {
        let store = test_store();
        store.init_schema().await.unwrap();
        assert!(store
            .read_lease(&SessionId::from("never-leased"))
            .await
            .unwrap()
            .is_none());
    }

    /// J2: an ALREADY-PROVISIONED store converges on the next attach — the
    /// dogfood rig's `lambo-dev.db` must not need a re-provision. Built by
    /// creating the pre-J2 five-column table by hand, then running the real
    /// `init_schema` over it.
    #[tokio::test]
    async fn a_pre_j2_lease_table_gains_the_endpoint_column_on_init() {
        let store = test_store();
        // The exact DDL that shipped before J2 (five columns).
        sqlx::query(
            "CREATE TABLE session_leases (\
                 session_id  TEXT PRIMARY KEY, \
                 holder      TEXT NOT NULL, \
                 acquired_at TEXT NOT NULL, \
                 expires_at  TEXT NOT NULL, \
                 current_token INTEGER NOT NULL DEFAULT 0)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO session_leases VALUES \
                 ('legacy', 'old@host#1', '2026-01-01T00:00:00.000Z', \
                  '2026-01-01T00:00:01.000Z', 7)",
        )
        .execute(store.pool())
        .await
        .unwrap();

        store.init_schema().await.unwrap();

        // The pre-existing row survives and reads as "published no endpoint",
        // which is exactly what a pre-J2 holder did.
        let legacy = store
            .read_lease(&SessionId::from("legacy"))
            .await
            .unwrap()
            .expect("the pre-J2 row must survive the ALTER");
        assert_eq!(legacy.endpoint, None);
        assert_eq!(legacy.token, 7);
        // And the column is now writable.
        let sid = SessionId::from("fresh");
        let hub = lease_holder("a", 1).reachable_at("/run/lambo/fresh.sock");
        assert!(store
            .acquire_lease(&sid, &hub, Duration::from_secs(30))
            .await
            .unwrap()
            .is_acquired());
        assert_eq!(
            store
                .read_lease(&sid)
                .await
                .unwrap()
                .unwrap()
                .endpoint
                .as_deref(),
            Some("/run/lambo/fresh.sock")
        );
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

    // -----------------------------------------------------------------------
    // F1/F2 end to end: derive -> flush -> vector recall, on SQLite
    // -----------------------------------------------------------------------

    /// Everything the end-to-end test needs beyond `store-sqlite`: a deterministic
    /// embedder with a documented near/far pair.
    #[cfg(feature = "embed-fixture")]
    mod vector_e2e {
        use super::*;
        use crate::embed::{Embedder, FixtureEmbedder, NEAR_A, NEAR_B};
        use crate::memory::Memory;
        use crate::store::{Capabilities, GraphStore, SessionFlushStats};
        use crate::store::{LeaseHolder, LeaseOutcome};
        use crate::types::InteractionSpan;
        use crate::types::{MatchStrategy, RecallQuery};
        use std::sync::{Arc, Mutex};

        /// [`SqliteStore`] with every checked vector answer recorded, in call order.
        ///
        /// The twin of `memory.rs`'s `VectorSearchStore`, pointed at the real adapter:
        /// it proves the vector leg **fired** on SQLite and what SQLite returned, rather
        /// than inferring a vector hit from where a node landed in a rank. It adds no
        /// behaviour — every method delegates.
        struct RecordingSqlite {
            inner: SqliteStore,
            answers: Mutex<Vec<Vec<Scored<NodeId>>>>,
        }

        impl RecordingSqlite {
            fn new(inner: SqliteStore) -> Self {
                Self {
                    inner,
                    answers: Mutex::new(Vec::new()),
                }
            }

            fn answers(&self) -> Vec<Vec<Scored<NodeId>>> {
                self.answers.lock().unwrap().clone()
            }
        }

        #[async_trait]
        impl GraphStore for RecordingSqlite {
            async fn init_schema(&self) -> Result<(), StoreError> {
                self.inner.init_schema().await
            }
            fn capabilities(&self) -> Capabilities {
                self.inner.capabilities()
            }
            fn vector_dimensions(&self) -> Option<usize> {
                self.inner.vector_dimensions()
            }
            async fn flush(
                &self,
                batch: &MutationBatch,
                token: Option<u64>,
            ) -> Result<(), StoreError> {
                self.inner.flush(batch, token).await
            }
            async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
                self.inner.load_session(session).await
            }
            async fn keyword_candidates(
                &self,
                session: &SessionId,
                tokens: &[String],
                limit: usize,
            ) -> Result<Vec<Scored<NodeId>>, StoreError> {
                self.inner.keyword_candidates(session, tokens, limit).await
            }
            async fn vector_candidates(
                &self,
                session: &SessionId,
                embedding: &[f32],
                limit: usize,
            ) -> Result<Vec<Scored<NodeId>>, StoreError> {
                self.inner
                    .vector_candidates(session, embedding, limit)
                    .await
            }
            async fn vector_candidates_checked(
                &self,
                session: &SessionId,
                embedding: &[f32],
                expected_contract: &EmbeddingContract,
                limit: usize,
            ) -> Result<Vec<Scored<NodeId>>, StoreError> {
                let hits = self
                    .inner
                    .vector_candidates_checked(session, embedding, expected_contract, limit)
                    .await?;
                self.answers.lock().unwrap().push(hits.clone());
                Ok(hits)
            }
            async fn blast_radius(
                &self,
                session: &SessionId,
                node: NodeId,
                min_edge_age: Duration,
                now: DateTime<Utc>,
            ) -> Result<u64, StoreError> {
                self.inner
                    .blast_radius(session, node, min_edge_age, now)
                    .await
            }
            async fn interaction_span(
                &self,
                session: &SessionId,
                node: NodeId,
                min_age: Duration,
                now: DateTime<Utc>,
            ) -> Result<InteractionSpan, StoreError> {
                self.inner
                    .interaction_span(session, node, min_age, now)
                    .await
            }
            async fn record_canonization(
                &self,
                event: &CanonizationEvent,
                token: Option<u64>,
            ) -> Result<(), StoreError> {
                self.inner.record_canonization(event, token).await
            }
            async fn acquire_lease(
                &self,
                session: &SessionId,
                holder: &LeaseHolder,
                ttl: Duration,
            ) -> Result<LeaseOutcome, StoreError> {
                self.inner.acquire_lease(session, holder, ttl).await
            }
            async fn refresh_lease(
                &self,
                session: &SessionId,
                holder: &LeaseHolder,
                ttl: Duration,
            ) -> Result<LeaseOutcome, StoreError> {
                self.inner.refresh_lease(session, holder, ttl).await
            }
            async fn release_lease(
                &self,
                session: &SessionId,
                holder: &LeaseHolder,
            ) -> Result<(), StoreError> {
                self.inner.release_lease(session, holder).await
            }
            async fn write_flush_stats(
                &self,
                session: &SessionId,
                stats: &SessionFlushStats,
            ) -> Result<(), StoreError> {
                self.inner.write_flush_stats(session, stats).await
            }
            async fn read_flush_stats(
                &self,
                session: &SessionId,
            ) -> Result<Option<SessionFlushStats>, StoreError> {
                self.inner.read_flush_stats(session).await
            }
        }

        /// Hybrid embeds a concept **with** its origin context (`"register user — <prompt>"`)
        /// while recall embeds the bare query (`"create account"`). A real semantic
        /// embedder still scores those two as near; `FixtureEmbedder` is hash-seeded per
        /// exact phrase and cannot. This reduces the framing back to the concept label
        /// before delegating — the same wrapper `memory.rs` uses for the MemoryStore
        /// version of this test, and nothing else about the embedding path changes.
        #[derive(Debug)]
        struct ContextTolerantEmbedder(FixtureEmbedder);

        #[async_trait]
        impl Embedder for ContextTolerantEmbedder {
            fn dimensions(&self) -> usize {
                self.0.dimensions()
            }
            async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
                let label = text
                    .strip_prefix("Concept: ")
                    .unwrap_or(text)
                    .split(" — ")
                    .next()
                    .unwrap_or(text);
                self.0.embed(label).await
            }
        }

        /// **F, end to end.** A concept created by the ordinary derive surface on SQLite
        /// persists its vector, the vector survives the write-behind flush and a session
        /// reload, and recall's vector leg finds it from a query that shares no token
        /// with it. Before F this was impossible on the default local store: hybrid
        /// derive logged `hybrid matching disabled: store lacks VECTOR_SEARCH` and
        /// recall was keyword/recency only, which made semantic recall a property of the
        /// cloud tier.
        ///
        /// The vector leg is proven to have **fired** by `RecordingSqlite::answers`, not
        /// by the recalled node's rank.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn sqlite_vector_leg_fires_on_an_organically_derived_concept() {
            let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::WARN);
            let session = SessionId::from("sqlite-organic-vectors");
            let (dir, path) = scratch_db();
            let store = Arc::new(RecordingSqlite::new(SqliteStore::connect(&path).unwrap()));
            store.init_schema().await.unwrap();

            let contract = EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: FixtureEmbedder::new().dimensions(),
            };
            let session_name = session.0.clone();
            let open = |store: Arc<RecordingSqlite>, contract: EmbeddingContract| {
                let session_name = session_name.clone();
                async move {
                    Memory::builder()
                        .session(session_name)
                        .agent("agent-a")
                        .flush_interval(Duration::from_secs(3_600))
                        .match_strategy(MatchStrategy::Hybrid)
                        .store(store as Arc<dyn GraphStore>)
                        .embedder(Arc::new(ContextTolerantEmbedder(FixtureEmbedder::new()))
                            as Arc<dyn Embedder>)
                        .embedding_contract(contract)
                        .build()
                        .await
                        .expect("build")
                }
            };

            let mem = open(store.clone(), contract.clone()).await;
            let out = mem
                .derive(
                    &[(NEAR_A, ConceptType::Entity)],
                    &crate::graph::derive::ParentOf::none(),
                )
                .await
                .unwrap();
            assert_eq!(out.created.len(), 1);
            let organic = out.created[0];
            // close() drains the tail: the vector must be durable, not RAM-only.
            mem.close().await.unwrap();

            let snapshot = store.load_session(&session).await.unwrap();
            let stored = snapshot
                .concepts
                .iter()
                .find(|c| c.id == organic)
                .expect("the derived concept is durable");
            assert_eq!(
                stored.embedding.as_ref().map(Vec::len),
                Some(contract.dim),
                "an organically-derived concept persists its vector through SQLite"
            );
            assert_eq!(
                snapshot.embedding.as_ref(),
                Some(&contract),
                "and the contract that makes it interpretable is durable with it"
            );
            assert_eq!(
                store.answers(),
                vec![Vec::new()],
                "the derive's own hybrid gather queried SQLite and found an empty pool"
            );

            // Reopen — proving the vector round-trips through load_session — and recall
            // with text sharing NO token with the stored concept but near it in the
            // embedding space. The keyword leg cannot score it; only the vector leg can.
            let reopened = open(store.clone(), contract.clone()).await;
            let result = reopened
                .recall(RecallQuery {
                    query: NEAR_B.into(),
                    top_k: 5,
                    max_tokens: 500,
                    traversal_depth: 1,
                })
                .await
                .unwrap();

            let answers = store.answers();
            assert_eq!(answers.len(), 2, "recall issued exactly one vector query");
            let scored = answers[1]
                .iter()
                .find(|s| s.item == organic)
                .expect("SQLite's vector leg returned the organically-derived concept");
            assert!(
                scored.score >= 0.85,
                "scored by real cosine similarity, got {}",
                scored.score
            );
            assert!(
                result.hits.iter().any(|h| h.node_id == organic),
                "the vector-leg candidate reaches the assembled result: {result:?}"
            );
            assert!(
                !logs.contains("store lacks VECTOR_SEARCH"),
                "SQLite must no longer log the hybrid degradation warning: {}",
                logs.contents()
            );
            assert!(
                !logs.contains("capability miss"),
                "nor the capability-refusal degradation: {}",
                logs.contents()
            );

            reopened.close().await.unwrap();
            drop(store);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// The same store, the same session, a renamed embedder: the checked read
        /// refuses rather than ranking vectors from another space. On SQLite this path
        /// was previously unreachable (no capability meant hybrid never called it).
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_mid_session_model_swap_is_refused_on_the_recall_path() {
            let _quiet = crate::test_util::quiet_logs();
            let session = SessionId::from("sqlite-contract-swap");
            let (dir, path) = scratch_db();
            let store = Arc::new(RecordingSqlite::new(SqliteStore::connect(&path).unwrap()));
            store.init_schema().await.unwrap();
            let dim = FixtureEmbedder::new().dimensions();

            let mem = Memory::builder()
                .session(session.0.clone())
                .agent("agent-a")
                .flush_interval(Duration::from_secs(3_600))
                .match_strategy(MatchStrategy::Hybrid)
                .store(store.clone() as Arc<dyn GraphStore>)
                .embedder(
                    Arc::new(ContextTolerantEmbedder(FixtureEmbedder::new())) as Arc<dyn Embedder>
                )
                .embedding_contract(EmbeddingContract {
                    kind: "fixture".into(),
                    model: Some("model-v1".into()),
                    dim,
                })
                .build()
                .await
                .expect("build");
            mem.derive(
                &[(NEAR_A, ConceptType::Entity)],
                &crate::graph::derive::ParentOf::none(),
            )
            .await
            .unwrap();
            mem.close().await.unwrap();

            let renamed = EmbeddingContract {
                kind: "fixture".into(),
                model: Some("model-v2".into()),
                dim,
            };
            let err = store
                .vector_candidates_checked(&session, &vec![0.0; dim], &renamed, 5)
                .await
                .unwrap_err();
            assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
            assert!(
                err.to_string().contains("embedding contract changed"),
                "{err}"
            );

            drop(store);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

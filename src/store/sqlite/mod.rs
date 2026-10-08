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
//!   DDL performs for free. The gate has a second half in [`write_rows::set_embedding`], which
//!   NULLs every vector of a different width when it stamps a contract (F-R2-1): the
//!   gate alone could not stop a **restamp** from leaving earlier vectors under a
//!   width they no longer match, because each of them was valid when written. The
//!   two together give the property — *no vector whose width disagrees with the
//!   session contract survives a write through this adapter* — and the per-read
//!   check remains the only defence against a hand-edited database.
//!
//! ### The scan is a seam
//!
//! Candidate *selection* ([`vector_candidates::select_session_vectors`]) is separated from candidate
//! *scoring* ([`vector_candidates::rank_by_cosine`]) on purpose. Today selection is a full session scan and
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
//!   [`structural::STRUCTURAL_EDGE_IN`] predicate). Provenance `Derives`
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
//!
//! ## Mutation epoch (issue #17)
//!
//! `sessions.mutation_epoch` (`NOT NULL DEFAULT 0`, converged the same guarded
//! way) is the session's durable mutation counter. Every flush stamps the
//! batch's absolute watermark onto the touched sessions' rows
//! (`ensure_sessions`, monotonic `max`) in the batch's own transaction, and
//! `load_session` returns it in `GraphSnapshot.mutation_epoch` so
//! `Graph::from_snapshot` resumes the accounting instead of a restart
//! resetting it — GC's `gc_interval` measures deployment-lifetime mutations.
//!
//! ## GC sweep mark (issue #29)
//!
//! `sessions.last_gc_epoch` (`NOT NULL DEFAULT 0`) and `sessions.last_gc_at`
//! (nullable fixed-width UTC text) carry GC's sweep watermark and the time of
//! the last sweep. They ride the same `ensure_sessions` statement as the epoch
//! with a field-wise monotonic merge (`GcMark::merge`), are converged on
//! pre-existing databases by the same guarded ALTER, and come back from
//! `load_session` in `GraphSnapshot.gc_mark`, so a writer restart neither
//! resets GC's measure nor the `gc_max_interval` clock (it sweeps only if a
//! sweep was already due by the stored mark).

// Clippy's `explicit_auto_deref` suggestion is wrong for sqlx: `&mut *tx` reborrows
// the `Transaction` (which implements `sqlx::Executor`), while the suggested `&mut tx`
// produces `&mut &mut Transaction` (which does not). Known sqlx+clippy false-positive;
// kept explicit on purpose.
#![allow(clippy::explicit_auto_deref)]

mod codec;
mod leases;
mod schema;
mod session_load;
mod structural;
mod vector_candidates;
mod write_rows;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use super::batch::{
    plan_flush, BulkLimits, ACCESS_COLUMNS, CONCEPT_COLUMNS, EDGE_COLUMNS, INTERACTION_COLUMNS,
};
#[cfg(feature = "fixtures")]
use super::batch::{seed_concept_rows, seed_edge_rows};
use super::lease::{lease_permits_write, LeaseHolder, LeaseInfo, LeaseOutcome};
use super::{map_write_err, Capabilities, GraphStore, SessionFlushStats};
// The unit tests in `tests/` reach the adapter's items through `use super::*`.
// These keep the names they use that now live in submodules or are otherwise
// unused by the facade itself.
#[cfg(test)]
use super::batch::AccessUpdate;
#[cfg(test)]
use super::columns_in_ddl;
#[cfg(all(test, feature = "fixtures"))]
use super::validate_vector_candidate_limit;
#[cfg(all(test, feature = "fixtures"))]
use crate::types::tie_break_by_key;
use crate::types::{
    CanonizationEvent, EmbeddingContract, GcMark, GraphSnapshot, InteractionSpan, Mutation,
    MutationBatch, NodeId, Scored, SessionId, StoreError,
};
#[cfg(test)]
use crate::types::{Concept, Edge, Interaction};
#[cfg(feature = "fixtures")]
use codec::enum_to_text;
use codec::{db_err, ts_to_text};
use leases::acquire_or_refresh;
#[cfg(test)]
use schema::INIT_SQL;
#[cfg(all(test, feature = "fixtures"))]
use std::collections::HashMap;
#[cfg(test)]
use structural::INTERACTION_SPAN_SQL;
#[cfg(test)]
use write_rows::update_accesses_query;
use write_rows::{apply_canonization_transition, apply_step};
#[cfg(feature = "fixtures")]
use write_rows::{put_write_intent, upsert_concepts, upsert_edges, upsert_interactions};

/// Rows per multi-row upsert statement (L82-1).
///
/// Chosen against SQLite's *most conservative* `SQLITE_MAX_VARIABLE_NUMBER` of
/// 999 rather than the 32766 a modern build ships: 16 columns × 60 rows = 960
/// and 10 × 99 = 990 both fit either way, and a statement that silently depends
/// on how the library was compiled is not worth the extra rows. The limits exist so
/// the shape matches Cockroach's, not to hit a latency target. `interactions`
/// batches too (100): its self-foreign-key chain is safe under the R1-1
/// first-position dedupe (reference-before-use) with end-of-statement FK checks,
/// mirroring the Cockroach constant (F4).
const BULK_LIMITS: BulkLimits = BulkLimits {
    interactions: 100,
    // C2 added a 17th concept column (`human_confirmed`); 60 × 17 = 1020
    // breaches the conservative 999-variable ceiling, so the chunk drops to 58
    // (58 × 17 = 986) to keep the R1-4 assert honest on pre-3.32 SQLite.
    concepts: 58,
    edges: 99,
    // Issue #30: 4 binds per row; 240 × 4 = 960.
    accesses: 240,
};

/// The conservative `SQLITE_MAX_VARIABLE_NUMBER` the limits above are sized
/// against. Pre-3.32 builds ship this; 3.32+ ship 32766.
const SQLITE_MAX_VARIABLE_NUMBER: usize = 999;

// R1-4: the arithmetic in the doc comment above is prose, and prose does not
// fail a build. Raising `concepts` past the 58-row chunk (see `BULK_LIMITS`)
// passes the whole local suite against a modern bundled SQLite and only breaks
// on an old one, in production. These turn that into a compile error.
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
const _: () = assert!(
    BULK_LIMITS.accesses * ACCESS_COLUMNS <= SQLITE_MAX_VARIABLE_NUMBER,
    "accesses chunk exceeds SQLITE_MAX_VARIABLE_NUMBER"
);

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
    /// anchor; `created_at` DB-default) and stamp the batch's absolute
    /// `mutation_epoch` watermark onto it (issue #17). Idempotent, so once per
    /// unique session per batch is enough. The upsert is monotonic
    /// (`max(existing, stamped)`), so a replayed batch converges to the same
    /// final state instead of regressing the counter — the flush-replay
    /// contract — and the stamp commits in the batch's own transaction, so a
    /// crash can never leave durable content ahead of its durable count. A
    /// batch of pure `DeleteNode`/`DeleteEdge` mutations resolves no session
    /// here; its epoch contribution lands with the next batch that names one
    /// (the stamp is absolute, so the counter only ever lags, never rewinds).
    async fn ensure_sessions(
        &self,
        tx: &mut sqlx::SqliteConnection,
        sessions: &HashSet<String>,
        mutation_epoch: u64,
        gc_mark: GcMark,
    ) -> Result<(), StoreError> {
        let epoch = i64::try_from(mutation_epoch).unwrap_or(i64::MAX);
        let last_gc_epoch = i64::try_from(gc_mark.last_gc_epoch).unwrap_or(i64::MAX);
        let last_gc_at = gc_mark.last_gc_at.map(ts_to_text);
        for sid in sessions {
            // Issue #29: GC's sweep mark rides the same statement with the
            // store-side merge (`GcMark::apply_to_stored`). `last_gc_at` is
            // fixed-width millisecond UTC text (`ts_to_text`), so the
            // lexicographic MAX is the chronological one; SQLite's two-argument
            // MAX returns NULL if either side is NULL, hence the COALESCE
            // fallbacks. The one exception to the max is a re-anchored mark
            // (`last_gc_at_reset`, the last bind) at least as current as the
            // stored mark (`last_gc_epoch >=`, read from the pre-update row):
            // its time replaces the stored one, so a future time left by a
            // corrected clock jump cannot keep the time trigger off, while a
            // stale reset replayed after a later sweep cannot rewind it
            // (`GcMark::reset_is_current_for`). `last_gc_epoch` is a max either way.
            sqlx::query(
                "INSERT INTO sessions (session_id, mutation_epoch, last_gc_epoch, last_gc_at) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT (session_id) DO UPDATE SET \
                     mutation_epoch = MAX(mutation_epoch, excluded.mutation_epoch), \
                     last_gc_epoch = MAX(last_gc_epoch, excluded.last_gc_epoch), \
                     last_gc_at = CASE WHEN ? AND excluded.last_gc_epoch >= last_gc_epoch \
                         THEN COALESCE(excluded.last_gc_at, last_gc_at) \
                         ELSE COALESCE(MAX(last_gc_at, excluded.last_gc_at), \
                                       last_gc_at, excluded.last_gc_at) END",
            )
            .bind(sid)
            .bind(epoch)
            .bind(last_gc_epoch)
            .bind(last_gc_at.as_deref())
            .bind(gc_mark.last_gc_at_reset)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_write_err(e, |m| format!("ensure session row: {m}")))?;
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
                 embedding_kind, embedding_model, embedding_dim, mutation_epoch, \
                 last_gc_epoch, last_gc_at) \
             VALUES (?, ?, COALESCE(?, strftime('%Y-%m-%dT%H:%M:%fZ','now')), ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (session_id) DO UPDATE SET \
                 root_goal = excluded.root_goal, \
                 created_at = excluded.created_at, \
                 closed_at = excluded.closed_at, \
                 embedding_kind = excluded.embedding_kind, \
                 embedding_model = excluded.embedding_model, \
                 embedding_dim = excluded.embedding_dim, \
                 mutation_epoch = excluded.mutation_epoch, \
                 last_gc_epoch = excluded.last_gc_epoch, \
                 last_gc_at = excluded.last_gc_at",
        )
        .bind(&snapshot.session_id.0)
        .bind(root_goal.as_deref())
        .bind(snapshot.created_at.map(ts_to_text))
        .bind(snapshot.closed_at.map(ts_to_text))
        .bind(embedding_kind)
        .bind(embedding_model)
        .bind(embedding_dim)
        .bind(i64::try_from(snapshot.mutation_epoch).unwrap_or(i64::MAX))
        .bind(i64::try_from(snapshot.gc_mark.last_gc_epoch).unwrap_or(i64::MAX))
        .bind(snapshot.gc_mark.last_gc_at.map(ts_to_text))
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
        self.apply_schema().await
    }

    async fn preflight_schema(&self) -> Result<(), StoreError> {
        self.verify_schema().await
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
        self.read_lease_row(session).await
    }

    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        self.delete_lease_row(session, holder).await
    }

    async fn record_lease_refusal(
        &self,
        session: &SessionId,
        refused_by: &str,
        current_holder: &str,
    ) -> Result<(), StoreError> {
        self.insert_lease_refusal(session, refused_by, current_holder)
            .await
    }

    async fn pending_lease_refusals(
        &self,
        session: &SessionId,
        since: DateTime<Utc>,
    ) -> Result<Vec<crate::store::lease::LeaseRefusal>, StoreError> {
        self.select_lease_refusals(session, since).await
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
                Mutation::ConsumeWriteIntent { session_id, .. }
                | Mutation::RecordAccess { session_id, .. } => {
                    sessions.insert(session_id.0.clone());
                }
                Mutation::DeleteNode { .. } | Mutation::DeleteEdge { .. } => {}
            }
        }
        self.ensure_sessions(&mut *tx, &sessions, batch.mutation_epoch, batch.gc_mark)
            .await?;

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
        self.load_snapshot(session).await
    }

    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.select_keyword_candidates(session, tokens, limit).await
    }

    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.legacy_vector_candidates(session, embedding, limit)
            .await
    }

    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.checked_vector_candidates(session, embedding, expected_contract, limit)
            .await
    }

    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.count_blast_radius(session, node, min_edge_age, now)
            .await
    }

    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<InteractionSpan, StoreError> {
        self.select_interaction_span(session, node, min_age, now)
            .await
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
// Tests — full offline conformance on sqlite::memory: (feature-gated; the
// module itself only compiles under `store-sqlite`, so these always are too).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

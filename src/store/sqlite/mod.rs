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

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use super::batch::{
    plan_flush, AccessUpdate, BulkLimits, ConceptRow, FlushStep, ACCESS_COLUMNS, CONCEPT_COLUMNS,
    EDGE_COLUMNS, INTERACTION_COLUMNS,
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
    tie_break_by_key, CanonizationEvent, Concept, Edge, EmbeddingContract, GcMark, GraphSnapshot,
    Interaction, InteractionSpan, Mutation, MutationBatch, Node, NodeId, Scored, SessionId,
    StoreError,
};
use codec::{
    cutoff_text, db_err, enum_to_text, node_id, node_id_str, session_embedding_from_parts,
    text_to_enum, text_to_ts, ts_to_text,
};

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

/// Structural edge types counted by both structural queries (spec §4.1 errata:
/// concept-to-concept `Dependency`/`Causal`/`Hierarchical` only — provenance
/// `Derives`/`Temporal` must not un-orphan concepts).
const STRUCTURAL_EDGE_IN: &str = "'Dependency', 'Causal', 'Hierarchical'";

/// T3.1 DDL — embedded and executed verbatim by [`SqliteStore::init_schema`],
/// and read for its table names by [`SqliteStore::preflight_schema`] (J3 F5).
/// Idempotent by construction (`IF NOT EXISTS` everywhere).
const INIT_SQL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/migrations/sqlite/001_init.sql"
));

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
     SELECT DISTINCT i.id, COALESCE(i.event_time, i.created_at) AS about_ts \
     FROM edges e \
     JOIN concepts src ON src.id = e.source \
     JOIN interactions i ON i.id = src.origin_interaction \
     WHERE e.target = ? AND e.session_id = ? AND i.session_id = ? \
       AND e.edge_type IN ({STRUCTURAL_EDGE_IN}) \
       AND COALESCE(e.event_time, e.created_at) <= ? \
       AND COALESCE(i.event_time, i.created_at) <= ? \
 ), \
 extent AS ( \
     SELECT min(COALESCE(event_time, created_at)) AS lo, \
            max(COALESCE(event_time, created_at)) AS hi \
     FROM interactions WHERE session_id = ? \
 ) \
 SELECT \
     (SELECT count(*) FROM span), \
     (SELECT min(about_ts) FROM span), \
     (SELECT max(about_ts) FROM span), \
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
        // C2 (SoloPolicy): the explicit human-confirmation count. Existing
        // databases converge here; fresh ones carry the column inline from the
        // DDL and this is a no-op.
        ensure_column(
            self.pool(),
            "concepts",
            "human_confirmed",
            "ALTER TABLE concepts ADD COLUMN human_confirmed INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        // D (about-time): the nullable about-time of interactions and edges
        // (NULL = live fact, fallback created_at; an edge inherits it from the
        // writing interaction). Existing pre-D databases converge here; fresh
        // ones carry the columns inline from the DDL and these are no-ops.
        ensure_column(
            self.pool(),
            "interactions",
            "event_time",
            "ALTER TABLE interactions ADD COLUMN event_time TEXT",
        )
        .await?;
        ensure_column(
            self.pool(),
            "edges",
            "event_time",
            "ALTER TABLE edges ADD COLUMN event_time TEXT",
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
        // Issue #17: the durable mutation counter. NOT NULL DEFAULT 0, so the
        // ALTER backfills existing rows and the accounting accumulates forward
        // from a session's first post-upgrade flush.
        ensure_column(
            self.pool(),
            "sessions",
            "mutation_epoch",
            "ALTER TABLE sessions ADD COLUMN mutation_epoch INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        // Issue #29: GC's sweep accounting. The epoch backfills 0 (never
        // swept) and the time NULL, which the daemon treats as "anchor the
        // `gc_max_interval` clock on first attach", never "sweep now".
        ensure_column(
            self.pool(),
            "sessions",
            "last_gc_epoch",
            "ALTER TABLE sessions ADD COLUMN last_gc_epoch INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        ensure_column(
            self.pool(),
            "sessions",
            "last_gc_at",
            "ALTER TABLE sessions ADD COLUMN last_gc_at TEXT",
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

    // J4. Record a lease refusal against this session at the store's clock,
    // then purge this session's refusals older than
    // `LEASE_REFUSAL_RETENTION` (JE2E-1).
    //
    // The purge is **lazy, on the write path**, mirroring `write_intents`'
    // `consume_write_intent`: the retention sweep rides the one statement that
    // was going to touch this table anyway, so no adapter grows a clock, no
    // task grows a timer, and a session nobody contends keeps its rows (which
    // is free — nothing is being appended to sweep them out of the way of).
    // The cutoff is the store's own clock, the same `strftime` that stamps the
    // row, so the comparison is between two store instants and never a
    // caller's (F18).
    async fn record_lease_refusal(
        &self,
        session: &SessionId,
        refused_by: &str,
        current_holder: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO lease_refusals \
                 (session_id, refused_at, refused_by, current_holder) \
             VALUES (?1, strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?2, ?3)",
        )
        .bind(&session.0)
        .bind(refused_by)
        .bind(current_holder)
        .execute(self.pool())
        .await
        .map_err(|e| db_err("record lease refusal", e))?;
        sqlx::query(
            "DELETE FROM lease_refusals \
             WHERE session_id = ?1 \
               AND refused_at < strftime('%Y-%m-%dT%H:%M:%fZ','now',?2)",
        )
        .bind(&session.0)
        .bind(format!(
            "-{} seconds",
            crate::store::lease::LEASE_REFUSAL_RETENTION.as_secs()
        ))
        .execute(self.pool())
        .await
        .map_err(|e| db_err("purge expired lease refusals", e))?;
        Ok(())
    }

    // J4. Refusals recorded against this session at/after `since`, newest first.
    async fn pending_lease_refusals(
        &self,
        session: &SessionId,
        since: DateTime<Utc>,
    ) -> Result<Vec<crate::store::lease::LeaseRefusal>, StoreError> {
        type Row = (String, String, String); // refused_at, refused_by, current_holder
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT refused_at, refused_by, current_holder FROM lease_refusals \
             WHERE session_id = ?1 AND refused_at >= ?2 ORDER BY refused_at DESC",
        )
        .bind(&session.0)
        .bind(ts_to_text(since))
        .fetch_all(self.pool())
        .await
        .map_err(|e| db_err("pending lease refusals", e))?;
        rows.into_iter()
            .map(|(refused_at, refused_by, current_holder)| {
                Ok(crate::store::lease::LeaseRefusal {
                    session: session.clone(),
                    at: text_to_ts(&refused_at)?,
                    refused_by,
                    current_holder,
                })
            })
            .collect()
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
        // likewise has its ordered mutation path (XP-8). `mutation_epoch` is
        // the durable mutation counter (issue #17): flush stamps it, and this
        // read is what a writer restart resumes it from.
        let row = sqlx::query(
            "SELECT embedding_kind, embedding_model, embedding_dim, root_goal, mutation_epoch, \
                    last_gc_epoch, last_gc_at \
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
        let mutation_epoch: i64 = row.try_get(4).map_err(|e| db_err("lookup session", e))?;
        // Issue #29: GC's sweep accounting, resumed with the epoch.
        let last_gc_epoch: i64 = row.try_get(5).map_err(|e| db_err("lookup session", e))?;
        let last_gc_at: Option<String> = row.try_get(6).map_err(|e| db_err("lookup session", e))?;
        let gc_mark = GcMark {
            last_gc_epoch: u64::try_from(last_gc_epoch).unwrap_or(0),
            last_gc_at: last_gc_at.as_deref().map(text_to_ts).transpose()?,
            last_gc_at_reset: false,
        };
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
            mutation_epoch: u64::try_from(mutation_epoch).unwrap_or(u64::MAX),
            gc_mark,
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
        // number of tokens hitting content OR canonical_key. Ties: canonical
        // key asc, then id (issue #2; SQLite's default BINARY collation
        // compares the same UTF-8 bytes Rust's `str` ordering does, so this
        // matches MemoryStore's Rust-side tie-break).
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
        sql.push_str(") ORDER BY score DESC, canonical_key ASC, id ASC LIMIT ?");

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
        // B-E2E-R2-3: the probe has to be an embedding on this adapter too.
        // The pg family gets this for free by encoding the probe before it
        // binds it; SQLite hands the probe straight to `rank_by_cosine`, whose
        // `cosine` clamps the denominator, so a zero-norm probe used to score
        // every row 0.0 and return candidates in tie-break order while the
        // same call refused loudly on Postgres. Placed here, before the store
        // is read, because that is where the pg family encodes: same input,
        // same error, same point in the sequence.
        crate::store::vector::ensure_is_an_embedding(embedding)?;
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
        // excludes the node itself, and the edge about-time (D fallback rule:
        // `COALESCE(e.event_time, e.created_at)`) gates the edge age
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
                     AND COALESCE(e.event_time, e.created_at) <= ?) \
               AND NOT EXISTS ( \
                   SELECT 1 FROM edges e2 \
                   JOIN concepts src2 ON src2.id = e2.source AND src2.session_id = ? \
                   WHERE e2.target = c.id AND e2.source <> ? \
                     AND e2.edge_type IN ({STRUCTURAL_EDGE_IN}) \
                     AND COALESCE(e2.event_time, e2.created_at) <= ?)"
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

/// One decoded stored vector, before scoring, with the concept's canonical key
/// riding along (the issue-2 tie-break consumes it on exact score ties).
type VectorCandidate = (NodeId, Vec<f32>, String);

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
        "SELECT id, canonical_key, embedding FROM concepts \
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
        let key: String = row
            .try_get(1)
            .map_err(|e| db_err("vector_candidates: concept canonical key", e))?;
        let blob: Vec<u8> = row
            .try_get(2)
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
        out.push((node_id(&id, "concept id")?, vector, key));
    }
    Ok(out)
}

/// **Candidate scoring**, the fixed half. Exact cosine, best first; ties
/// broken by canonical key ascending, then the smaller node id
/// ([`tie_break_by_key`], issue #2), so the answer is deterministic across
/// runs as well as within one (MemoryStore / Cockroach parity). Stays exact
/// whatever [`select_session_vectors`] becomes: an approximate index would
/// prune the pool, never the ranking.
fn rank_by_cosine(
    probe: &[f32],
    candidates: Vec<VectorCandidate>,
    limit: usize,
) -> Vec<Scored<NodeId>> {
    let mut scored: Vec<(Scored<NodeId>, String)> = candidates
        .into_iter()
        .map(|(id, vector, key)| {
            (
                Scored::new(id, f64::from(crate::embed::cosine(probe, &vector))),
                key,
            )
        })
        .collect();
    scored.sort_by(|(a, a_key), (b, b_key)| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| tie_break_by_key(Some(a_key), &a.item, Some(b_key), &b.item))
    });
    let mut scored: Vec<Scored<NodeId>> = scored.into_iter().map(|(s, _)| s).collect();
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
        FlushStep::Accesses(rows) => update_accesses(&mut *tx, rows).await,
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
        Mutation::RecordAccess {
            session_id,
            id,
            access_count,
            last_accessed,
        } => {
            let row = AccessUpdate {
                session_id,
                id: *id,
                access_count: *access_count,
                last_accessed: *last_accessed,
            };
            update_accesses(&mut *tx, &[row]).await?;
        }
    }
    Ok(())
}

/// The batched access update (issue #30) up to the `VALUES` list: SQLite names
/// a bare `VALUES` table's columns `column1..4`, so they are renamed in a
/// subquery. `push_values` emits the `VALUES` keyword itself.
const UPDATE_ACCESSES_PREFIX_SQL: &str = "UPDATE concepts SET \
     access_count = MAX(concepts.access_count, v.access_count), \
     last_accessed = MAX(COALESCE(concepts.last_accessed, v.last_accessed), v.last_accessed) \
     FROM (SELECT column1 AS id, column2 AS session_id, column3 AS access_count, \
     column4 AS last_accessed FROM (";

/// Closes [`UPDATE_ACCESSES_PREFIX_SQL`]. The join names the session as well
/// as the id, so a row can only ever count against its own session.
const UPDATE_ACCESSES_SUFFIX_SQL: &str =
    ")) AS v WHERE concepts.id = v.id AND concepts.session_id = v.session_id";

/// Apply one chunk of read accesses (issue #30) as ONE narrow `UPDATE`: the
/// two access columns only — no embedding rewrite — and **monotonic**, so a
/// replayed batch, or an access landing after a concept upsert that already
/// carried a higher count, can never lower what is stored. Existing rows only:
/// it never inserts.
///
/// `last_accessed` is TEXT in the fixed-width `ts_to_text` form (UTC, millis,
/// `Z`), so the string `MAX` is the chronological one. SQLite's multi-argument
/// `MAX` is NULL if any argument is, hence the `COALESCE` for a row never read.
async fn update_accesses(
    tx: &mut sqlx::SqliteConnection,
    rows: &[AccessUpdate<'_>],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    update_accesses_query(rows)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("record accesses: {m}")))?;
    Ok(())
}

/// The statement [`update_accesses`] runs, built but not executed.
fn update_accesses_query<'a>(rows: &'a [AccessUpdate<'a>]) -> sqlx::QueryBuilder<'a, sqlx::Sqlite> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(UPDATE_ACCESSES_PREFIX_SQL);
    qb.push_values(rows.iter(), |mut b, r| {
        b.push_bind(r.id.0.to_string())
            .push_bind(r.session_id.0.as_str())
            .push_bind(r.access_count)
            .push_bind(ts_to_text(r.last_accessed));
    });
    qb.push(UPDATE_ACCESSES_SUFFIX_SQL);
    qb
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
        "INSERT INTO interactions (id, session_id, agent_id, prompt_text, previous_id, created_at, event_time) ",
    );
    qb.push_values(rows.iter(), |mut b, i| {
        b.push_bind(i.id.0.to_string())
            .push_bind(i.session_id.0.clone())
            .push_bind(i.agent_id.0.clone())
            .push_bind(i.prompt_text.clone())
            .push_bind(i.previous_id.map(|id| id.0.to_string()))
            .push_bind(ts_to_text(i.created_at))
            .push_bind(i.event_time.map(ts_to_text));
    });
    qb.push(
        " ON CONFLICT (id) DO UPDATE SET \
             session_id = excluded.session_id, \
             agent_id = excluded.agent_id, \
             prompt_text = excluded.prompt_text, \
             previous_id = excluded.previous_id, \
             created_at = excluded.created_at, \
             event_time = excluded.event_time",
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
             chunk_group_id, human_confirmed) ",
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
            .push_bind(c.chunk_group_id.clone())
            .push_bind(c.human_confirmed);
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
             chunk_group_id = excluded.chunk_group_id, \
             human_confirmed = excluded.human_confirmed",
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
             created_at, last_reinforced, event_time) ",
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
            .push_bind(ts_to_text(e.last_reinforced))
            .push_bind(e.event_time.map(ts_to_text));
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
             last_reinforced = excluded.last_reinforced, \
             event_time = excluded.event_time",
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
        "SELECT id, session_id, agent_id, prompt_text, previous_id, created_at, event_time \
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
        let event_time: Option<String> =
            row.try_get(6).map_err(|e| db_err("load interactions", e))?;
        out.push(Interaction {
            id: node_id(&id, "interaction id")?,
            session_id: SessionId::from(sid),
            agent_id: crate::types::AgentId::new(agent),
            prompt_text: prompt,
            previous_id: prev.as_deref().map(node_id_str).transpose()?,
            created_at: text_to_ts(&created)?,
            event_time: event_time.as_deref().map(text_to_ts).transpose()?,
        });
    }
    Ok(out)
}

async fn load_concepts(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<Concept>, StoreError> {
    let rows = sqlx::query(
        "SELECT id, session_id, content, canonical_key, concept_type, origin_interaction, \
                origin_agent, created_at, access_count, last_accessed, gc_survived, \
                canonization_status, blast_radius, last_demotion_time, embedding, \
                chunk_group_id, human_confirmed \
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
        let human_confirmed: i32 = row.try_get(16).map_err(|e| db_err("load concepts", e))?;
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
            human_confirmed,
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
                created_at, last_reinforced, event_time \
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
        let event_time: Option<String> = row.try_get(9).map_err(|e| db_err("load edges", e))?;
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
            event_time: event_time.as_deref().map(text_to_ts).transpose()?,
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
mod tests;

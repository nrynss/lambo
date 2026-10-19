//! T3.3 — SQLite GraphStore adapter (offline / test tier, spec §3.2–§3.3, §4).
//!
//! Same trait surface as [`super::memory::MemoryStore`] over `sqlx::SqlitePool`.
//! `Concept.embedding` is written and read for flush→load round-trip parity (CON-8 — packed
//! little-endian `f32` in the `embedding BLOB` since #55) **and**, since F1/F2, queried: the
//! adapter advertises `VECTOR_SEARCH` and answers
//! [`GraphStore::vector_candidates_checked`] with an exact cosine scan over that column.
//!
//! ## Module map
//!
//! This file holds the store type, its construction and lazy pool, and the
//! single `impl GraphStore`, whose methods only delegate. The work lives in:
//!
//! * `schema` — the embedded DDL, `init_schema`'s guarded convergence, preflight.
//! * `persistence` — **every write transaction** (`flush`, `record_canonization`,
//!   `seed`): the session stamp, the fencing gate and the commit.
//! * `write_rows` — the statements those transactions run. Never begins or
//!   commits; always runs on the caller's connection.
//! * `session_load` — `load_session`'s single read transaction and row readers.
//! * `vector_candidates` — the checked vector read (contract and scan in one
//!   transaction) and candidate selection.
//! * `structural` — keyword candidates, `blast_radius`, `interaction_span`.
//! * `leases` — lease rows and the refusal log.
//! * `split_import` — the offline split's destination (#33 D2): its own
//!   transaction per target, receipt and in-transaction verification.
//! * `codec` — value codecs shared by all of the above.
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
//! *scoring* ([`crate::store::vector_source::rank_by_cosine`]) on purpose. Today selection is a full session scan and
//! scoring is exact cosine, which is right while `n` is small: at 1024 f32 a concept
//! vector is 4 KB and the largest measured session held ~1,400 of them. But "n is small
//! by construction" was a property of *session-scoped* graphs, and a single unified
//! autobiographical session is not bounded that way. An ANN index replaces
//! `select_session_vectors` behind this adapter's `VectorCandidateSource`
//! implementation, which already receives the probe and the limit (the scan itself
//! takes neither, since an exact scan can use neither), and `rank_by_cosine` keeps
//! re-ranking the survivors exactly. No caller and no other adapter method changes.
//!
//! Since #26 the checked read is this adapter's implementation of the
//! store-wide seam, `store::vector_source::VectorCandidateSource`, and the exact
//! scorer is shared from there, so #8's graph-backed source ranks identically.
//!
//! **Since #8 the session holder does not run this scan.** The adapter declares it
//! exact (`GraphStore::exact_vector_scan`), so the holder's recall and hybrid derive
//! rank the vectors its in-memory graph already holds, with the same scorer and
//! the same answer, and never call this store for vectors. What follows describes
//! the scan as readers without a graph (`lambo recall`, `serve-web`) still run it,
//! and as a holder ran it before #8.
//!
//! **Trigger to revisit: `hybrid::derive`, not recall** (F-R1-3). Recall runs one scan
//! per query. `derive` calls `vector_candidates_checked` *inside* its per-unmatched-
//! concept loop (`graph/hybrid.rs`), so a derive of `k` concepts over `n` stored vectors
//! is **k×n** BLOB decodes and text→`f32` parses, with no caching and no reuse between
//! iterations. All `k` of those scans share **one 30s deadline**
//! (`HYBRID_IO_TIMEOUT`, computed once per `derive` call) and contend for the **single
//! pooled connection** (`max_connections(1)`), which the write-behind flush also needs.
//! Overrunning the deadline is not a degradation — it returns
//! `EmbedUnavailable("hybrid vector candidate lookup timed out…")` (transient since
//! #125, so the write's durable intent survives for the next serve's replay, which
//! settles it `failed` if the lookup times out again), and fails the whole derive
//! before its commit phase. So measure the scan on the derive path first: that is
//! where the cliff is, and it is a path that could not run on SQLite at all before F2.
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
//! - **Dependents (#36 errata, read-time, no migration):** both queries
//!   read one set, `node`'s dependents
//!   ([`structural::DEPENDENTS_CTE`]): the sources of `Dependency` edges into
//!   `node` (`record_action`'s `action -> depends_on` direction) plus the
//!   targets of `node`'s `Hierarchical` edges (its children). Blast radius
//!   counts **all** of them, not only those `node` would orphan. `Causal`
//!   (provenance of an artifact), `Derives`/`Temporal` (provenance) and
//!   `CoOccurrence`/`Semantic` (association) declare no dependence. A
//!   dependent must be a concept row of the session, so an interaction id
//!   can never count.
//! - **Aged edges only (`COALESCE(e.event_time, e.created_at) <= cutoff`).**
//!   A dependent declared only by edges younger than the cutoff is invisible
//!   to both queries, exactly like MemoryStore's scan (`cutoff = now -
//!   min_age`, Rust-computed).
//! - **`d.id <> $node` self-exclusion.** A structural self-loop is never its
//!   own dependent (MemoryStore's `Edge::dependence`; the graph tier also
//!   rejects structural self-loops as cycle invariants).
//! - **Own origin excluded from the span.** `node`'s own origin interaction
//!   is not support for it.
//! - **Span gates BOTH timestamps** (edge and origin interaction, each on
//!   its about-time): the span is built from edges, so an edge younger than
//!   the cutoff is excluded even when its origin interaction is older (spec §4.1 second errata, 2026-08-11 / P3 T3.3 review — do not
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
//! silently clears the drift anchor. `created_at`/`closed_at` have no
//! `Mutation` kind, but full-snapshot `seed` stores them and `load_session`
//! preserves those columns. A session first created as an FK anchor keeps
//! its `created_at` DB-default. Read-only classification binds this metadata
//! along with the rest of the durable snapshot.
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
mod persistence;
mod rekey;
mod schema;
mod session_load;
mod split_import;
mod structural;
mod vector_candidates;
mod write_rows;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use super::lease::{LeaseHolder, LeaseInfo, LeaseOutcome};
use super::{Capabilities, GraphStore, SessionFlushStats};
use crate::store::vector_source::VectorCandidateSource;
use crate::types::{
    CanonizationEvent, EmbeddingContract, GraphSnapshot, InteractionSpan, MutationBatch, NodeId,
    Scored, SessionId, StoreError,
};
use leases::acquire_or_refresh;
pub(crate) use split_import::{SplitImportOutcome, SplitReceipt};

// The unit tests in `tests/` reach the adapter's items through `use super::*`,
// as they did when this was one file. These test-only imports keep every name
// they use in scope now that the items live in the submodules above.
#[cfg(test)]
use {
    super::batch::{AccessUpdate, ACCESS_COLUMNS},
    super::columns_in_ddl,
    crate::types::{Concept, Edge, GcMark, Interaction, Mutation},
    persistence::BULK_LIMITS,
    schema::INIT_SQL,
    structural::{with_dependents, BLAST_RADIUS_SQL, INTERACTION_SPAN_SQL},
    write_rows::update_accesses_query,
};
#[cfg(all(test, feature = "fixtures"))]
use {
    super::validate_vector_candidate_limit, crate::types::tie_break_by_key,
    std::collections::HashMap, std::collections::HashSet,
};

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

    /// Open a completed standalone file (a backup copy, or a split output)
    /// without creation, migrations, journal changes or sidecars, for the
    /// session classify, split and verify inputs. The caller must copy a
    /// finished backup first: an empty WAL alone does not prove a live
    /// database is immutable.
    pub fn connect_read_only(path: &std::path::Path) -> Result<Self, StoreError> {
        if !path.is_file() {
            return Err(StoreError::Invariant(
                "a read-only input database (session classify, split or verify) must be an existing \
                 standalone file".into(),
            ));
        }
        let info = std::fs::symlink_metadata(path)
            .map_err(|e| StoreError::Backend(format!("inspect backup file: {e}")))?;
        use std::os::unix::fs::MetadataExt;
        if info.file_type().is_symlink() || info.nlink() != 1 {
            return Err(StoreError::Invariant(
                "a read-only input database must be a standalone copy, not a symlink or hardlink"
                    .into(),
            ));
        }
        let mut wal = path.as_os_str().to_os_string();
        wal.push("-wal");
        match std::fs::metadata(&wal) {
            Ok(m) if m.len() > 0 => {
                return Err(StoreError::Invariant(
                    "a read-only input database must not have a nonempty WAL; use a completed \
                     standalone copy"
                        .into(),
                ))
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(StoreError::Backend(format!("inspect backup WAL: {e}"))),
        }
        Ok(Self::new(
            SqliteConnectOptions::new()
                .filename(path)
                .read_only(true)
                .create_if_missing(false)
                .immutable(true),
        ))
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
}

#[async_trait]
impl GraphStore for SqliteStore {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.apply_schema().await
    }

    async fn preflight_schema(&self) -> Result<(), StoreError> {
        self.verify_schema().await
    }

    async fn rekey_canonical_keys(
        &self,
        force: bool,
    ) -> Result<crate::store::rekey::RekeyPlan, StoreError> {
        self.rekey_keys(force).await
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

    /// #8: the checked read is an exact scan, every stored vector of the
    /// session scored by the shared `rank_by_cosine` (see
    /// `vector_candidates.rs`), and the BLOB codec round-trips `f32` exactly
    /// (CON-8). So the session holder ranks the same vectors in its graph and
    /// never calls this store for them; readers without a graph still do.
    fn exact_vector_scan(&self) -> bool {
        true
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
        self.expire_lease_row(session, holder).await
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
        self.upsert_flush_stats(session, stats).await
    }

    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionFlushStats>, StoreError> {
        self.select_flush_stats(session).await
    }

    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.flush_batch(batch, token).await
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
        VectorCandidateSource::checked_vector_candidates(
            self,
            session,
            embedding,
            expected_contract,
            limit,
        )
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
        // Preserve the raw accessor: chrono's full timestamp domain spans
        // less than i64::MAX milliseconds, so this cap cannot shorten it.
        self.select_interaction_span(
            session,
            node,
            min_age,
            now,
            Duration::from_millis(i64::MAX as u64),
        )
        .await
    }

    async fn interaction_span_with_window(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
        window: Duration,
    ) -> Result<InteractionSpan, StoreError> {
        self.select_interaction_span(session, node, min_age, now, window)
            .await
    }

    async fn dependent_evidence(
        &self,
        session: &SessionId,
        node: NodeId,
    ) -> Result<crate::store::DependentEvidence, StoreError> {
        self.select_evidence_rows(session, node).await
    }

    async fn erase_session(
        &self,
        session: &SessionId,
        eraser: &LeaseHolder,
    ) -> Result<crate::store::EraseOutcome, StoreError> {
        self.erase(session, eraser, &crate::store::erase::no_fault)
            .await
    }

    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.record_canonization_event(event, token).await
    }
}

// ---------------------------------------------------------------------------
// Tests — full offline conformance on sqlite::memory: (feature-gated; the
// module itself only compiles under `store-sqlite`, so these always are too).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

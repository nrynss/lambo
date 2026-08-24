//! GraphStore trait, Level B factory, and optional adapters (P1 / P3).
//!
//! Packaging: Cargo features gate adapters (`store-memory`, `store-cockroach`,
//! `store-postgres`, `store-sqlite`); `lambo.toml` / env select among compiled
//! kinds. See `dev-diary/notes/level-b-pluggability.md`.

#[cfg(feature = "store-memory")]
mod memory;

#[cfg(feature = "store-memory")]
pub use memory::MemoryStore;

// B0: the Postgres-wire-protocol store family (`pg`), of which the T3.2
// CockroachDB durable adapter (spec §3.2/§3.3, §4) is the first dialect and
// B2's PostgreSQL adapter is the second (templated width, hnsw from init).
// Features: store-cockroach, store-postgres.
#[cfg(any(feature = "store-cockroach", feature = "store-postgres"))]
pub mod pg;
// The adapter's public path is unchanged by the move: `crate::store::cockroach`
// still names the Cockroach dialect's module, now re-exported from `pg`.
#[cfg(feature = "store-cockroach")]
pub use pg::cockroach;
#[cfg(feature = "store-postgres")]
pub use pg::postgres;
// T3.3 — SQLite offline / test tier (spec §3.2–§3.3, §4). VECTOR_SEARCH since F1/F2.
#[cfg(feature = "store-sqlite")]
mod sqlite;

#[cfg(feature = "store-sqlite")]
pub use sqlite::SqliteStore;

// STORE-4 — shared backend-error classification (sqlx-backed adapters only;
// `store-cockroach` / `store-postgres` / `store-sqlite` pull `sqlx`).
#[cfg(any(
    feature = "store-cockroach",
    feature = "store-postgres",
    feature = "store-sqlite"
))]
mod error;

#[cfg(any(
    feature = "store-cockroach",
    feature = "store-postgres",
    feature = "store-sqlite"
))]
pub(crate) use error::map_write_err;

// STORE-1/CON-8 — shared vector codec for the sqlx-backed adapters (Cockroach
// VECTOR text literal; SQLite stores the same text as a BLOB).
#[cfg(any(
    feature = "store-cockroach",
    feature = "store-postgres",
    feature = "store-sqlite"
))]
pub(crate) mod vector;

// DSN spelling -> DSN identity. Store-agnostic and always compiled: both the
// session endpoint (J2) and `StoreConfig::overlay_env` (E2E-F2) need the rule,
// and neither is feature-gated.
pub(crate) mod dsn;

// L82-1 — statement planning for the SQL adapters' `flush()`. Store-agnostic
// (it only reads `Mutation`), so it compiles and is tested under every feature
// row, including the `--no-default-features` minimal ones.
pub mod batch;
// T3.5 — `load_session()` / startup materialization (see `load.rs`).
pub mod load;
// T3.4 — write-behind flush task (spec §2.4–§2.5); drains any GraphStore.
pub mod flush;
// T8.6 — single-writer lease (spec §2.2, store-enforced). Holder identity,
// TTL/heartbeat constants, acquire/refresh/release outcomes.
pub mod lease;

pub use lease::{LeaseHolder, LeaseInfo, LeaseOutcome};

use async_trait::async_trait;
use bitflags::bitflags;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::env;
use std::str::FromStr;
use std::time::Duration;

use crate::types::{
    CanonizationEvent, EmbeddingContract, GraphSnapshot, InteractionSpan, MutationBatch, NodeId,
    Scored, SessionId, StoreError,
};

/// Maximum caller-requested vector result count. This bounds adapter queries,
/// allocation, and integer narrowing at the public store/recall boundary.
pub const MAX_VECTOR_CANDIDATE_LIMIT: usize = 2048;

/// Observable flush stats published by the lease-holding writer and readable
/// by a reader in another process (T85-3).
///
/// This is the durable/remote snapshot of the writer's in-memory
/// [`crate::store::flush::FlushStats`] (lag / depth / dead-lettered), which is
/// process-local and therefore invisible to a separate reader. A reader that
/// sees `None` reports the honest `n/a`, never a fabricated `0`. Written ONLY
/// by the writer's `FlushTask`; readers must never write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionFlushStats {
    /// Milliseconds since the writer's last successful flush — the observable
    /// durability-lag bound.
    pub flush_lag_ms: u64,
    /// Mutations not yet durable (in-graph log + pending batch) at publish time.
    pub log_depth: u64,
}
pub fn validate_vector_candidate_limit(limit: usize) -> Result<(), StoreError> {
    if limit > MAX_VECTOR_CANDIDATE_LIMIT {
        return Err(StoreError::Invariant(format!(
            "vector candidate limit {limit} exceeds maximum {MAX_VECTOR_CANDIDATE_LIMIT}"
        )));
    }
    Ok(())
}

/// Every table name the embedded DDL creates, in DDL order.
///
/// Derived from the migration text an adapter already `include_str!`s rather
/// than from a hand-kept list, so [`GraphStore::preflight_schema`] cannot drift
/// behind a future schema addition — which is the whole failure mode F5 is: a
/// table added to the DDL and no path that notices it is absent. Matches the
/// project's one DDL idiom, `CREATE TABLE IF NOT EXISTS <name> (`; a statement
/// written any other way is deliberately not matched, because a preflight that
/// guesses is worse than one whose coverage is stated.
pub fn tables_in_ddl(ddl: &str) -> Vec<&str> {
    const NEEDLE: &str = "CREATE TABLE IF NOT EXISTS ";
    ddl.lines()
        .filter_map(|line| line.trim().strip_prefix(NEEDLE))
        .filter_map(|rest| {
            let name = rest
                .split(|c: char| c.is_whitespace() || c == '(')
                .next()
                .unwrap_or("");
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

/// Every `(table, column)` pair the embedded DDL requires, in DDL order.
///
/// Like [`tables_in_ddl`], derived from the migration text an adapter already
/// `include_str!`s, so the *column* preflight (J3-R2R-3 — the half of F5 the
/// table check cannot see) cannot drift behind a future schema change either.
/// Preflight that guesses is worse than one whose coverage is stated, so the
/// parser matches the project's two DDL idioms only:
///
/// * a `CREATE TABLE IF NOT EXISTS <name> ( … )` block — a non-column line
///   inside it (a table-level `PRIMARY KEY`/`UNIQUE`/`CONSTRAINT`/`FOREIGN`/
///   `CHECK`/`INDEX` clause, or a `--` comment) is skipped by its first token;
/// * an `ALTER TABLE <t> ADD COLUMN [IF NOT EXISTS] <col> <type>` statement —
///   Cockroach's post-provision convergence path, which the
///   `ensure_column`/`ADD COLUMN` idiom feeds.
///
/// A statement written any other way — a `CREATE VECTOR INDEX`, a
/// `::STRING` cast, a `DROP CONSTRAINT` — is deliberately not matched. This is
/// the same stated-coverage limit as [`tables_in_ddl`].
pub fn columns_in_ddl(ddl: &str) -> Vec<(&str, &str)> {
    const TABLE_NEEDLE: &str = "CREATE TABLE IF NOT EXISTS ";
    const ADD_COLUMN_NEEDLE: &str = "ADD COLUMN";
    // First token that marks a *table-level* clause, not a column. `--` is a
    // comment; `INDEX` is Cockroach's inline table-level index clause.
    const SKIP_FIRST: [&str; 7] = [
        "--",
        "CONSTRAINT",
        "PRIMARY",
        "UNIQUE",
        "FOREIGN",
        "CHECK",
        "INDEX",
    ];
    let mut out: Vec<(&str, &str)> = Vec::new();
    let mut in_table: Option<&str> = None;
    for line in ddl.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(TABLE_NEEDLE) {
            let table = rest
                .split(|c: char| c.is_whitespace() || c == '(')
                .next()
                .unwrap_or("");
            in_table = Some(table);
            continue;
        }
        if let Some(table) = in_table {
            if trimmed.starts_with(')') || trimmed == ");" {
                in_table = None;
                continue;
            }
            let first = trimmed.split_whitespace().next().unwrap_or("");
            if !SKIP_FIRST.contains(&first) {
                out.push((table, first));
            }
            continue;
        }
        if let Some(idx) = trimmed.find(ADD_COLUMN_NEEDLE) {
            let after = trimmed[idx + ADD_COLUMN_NEEDLE.len()..].trim();
            let after = after.strip_prefix("IF NOT EXISTS").unwrap_or(after).trim();
            let col = after
                .split(|c: char| c.is_whitespace() || c == ';')
                .next()
                .unwrap_or("");
            if !col.is_empty() {
                if let Some(table) = trimmed
                    .strip_prefix("ALTER TABLE ")
                    .and_then(|t| t.split_whitespace().next())
                {
                    out.push((table, col));
                }
            }
        }
    }
    out
}

/// The column-shaped sibling of [`unprovisioned_store_err`] (J3-R2R-3): a store
/// whose **table** is present but a **column** is missing. Same refusal channel,
/// same actionable `lambo provision` sentence — a missing column converges on
/// the provision path (`ensure_column` / `ADD COLUMN IF NOT EXISTS`).
pub fn unprovisioned_column_err(kind: &str, table: &str, missing: &[&str]) -> StoreError {
    StoreError::Capability(format!(
        "{kind} store's table {table} is missing {} the current build requires ({}) — its \
         schema was provisioned by an older lambo and never migrated. Nothing would become \
         durable: a write's mutations and its durable intent share one flush transaction, so \
         one missing column rolls every batch back whole and the failure surfaces only at \
         close. Run `lambo provision` (idempotent) against this store, then retry",
        if missing.len() == 1 {
            "a column"
        } else {
            "columns"
        },
        missing.join(", "),
    ))
}

/// The refusal [`GraphStore::preflight_schema`] returns, shared by both SQL
/// adapters so the operator reads one sentence whichever store they run.
pub fn unprovisioned_store_err(kind: &str, missing: &[&str]) -> StoreError {
    StoreError::Capability(format!(
        "{kind} store is missing {} the current build requires ({}) — its schema was \
         provisioned by an older lambo and never migrated. Nothing would become durable: \
         a write's mutations and its durable intent share one flush transaction, so one \
         missing table rolls every batch back whole and the failure surfaces only at \
         close. Run `lambo provision` (idempotent) against this store, then retry",
        if missing.len() == 1 {
            "a table"
        } else {
            "tables"
        },
        missing.join(", "),
    ))
}

bitflags! {
    /// Adapter capabilities (spec §3.2).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Capabilities: u32 {
        const VECTOR_SEARCH = 0b0001;
        const HISTORY = 0b0010;
    }
}

/// Durable / query surface — Lambo vocabulary only (spec §3.2).
#[async_trait]
pub trait GraphStore: Send + Sync {
    async fn init_schema(&self) -> Result<(), StoreError>;
    fn capabilities(&self) -> Capabilities;

    /// Refuse an **un-provisioned or un-migrated** store before it can accept a
    /// write (J3 round-1, F5).
    ///
    /// `init_schema` runs from exactly one place in the product — `lambo
    /// provision` (`crate::cli::provision`) — and never on the attach path. So a
    /// store provisioned by an older build, whose operator upgraded the binary
    /// without re-provisioning, is missing whatever tables the newer DDL added.
    /// Measured consequence when the missing table is `write_intents`: the
    /// session attaches, every write is **acked**, and nothing whatever becomes
    /// durable — not the intents and not the concepts — because
    /// `Mutation::PutWriteIntent` rides the same flush transaction as the
    /// write's own mutations, so one missing table rolls each batch back whole.
    /// The operator learns at `close()`, from a failed final flush.
    ///
    /// This runs before the single-writer lease is acquired (nothing in it
    /// depends on the lease) and returns [`StoreError::Capability`]: an
    /// un-migrated store genuinely does not offer the durable surface asked of
    /// it, and the message names `lambo provision` so the refusal is actionable.
    ///
    /// **Tables and columns.** Missing *tables* are refused via the DDL-derived set;
    /// missing *columns* are also refused, diffed from the same DDL source: SQLite via
    /// `PRAGMA table_info`, Cockroach via `information_schema.columns`. A store missing
    /// either cannot offer the durable surface asked of it, so the refusal is loud rather
    /// than a silent ack into a void.
    ///
    /// Default `Ok(())`: an adapter with no external schema (`MemoryStore`, the
    /// test doubles) has nothing to check.
    async fn preflight_schema(&self) -> Result<(), StoreError> {
        Ok(())
    }

    /// Dense-vector width this store will accept, when it persists embeddings.
    ///
    /// * `Some(n)` — embedder output must be exactly `n`. Cockroach's `n` is its
    ///   `VECTOR(n)` DDL: a genuine schema authority, independent of any config.
    /// * `None` — no vector column at all (MemoryStore). Must be paired with an
    ///   absent [`Capabilities::VECTOR_SEARCH`].
    ///
    /// **How much authority this carries depends on the adapter, and for one of them
    /// it can be none** (F-R1-2). An adapter whose column has no width of its own
    /// (SQLite's `BLOB`) has no schema number to report. It reports, in precedence
    /// order: the operator's [`StoreConfig::vector_dim`] pin when set, else the
    /// resolved `[embedder] dim`, else the [`crate::embed::EmbedderConfig`] default.
    /// Only the first is an independent authority. **With no pin set, the value is an
    /// echo of the embedder width** — so `check_vector_compatibility` is comparing a
    /// number to itself and cannot fail. Say "echo", not "authority", when describing
    /// that case.
    ///
    /// What such an adapter enforces instead is the **session's durable
    /// contract** (`sessions.embedding_{kind,model,dim}`), on every candidate read,
    /// in the read's own transaction. That is the only authority that can attest
    /// which space the stored vectors actually occupy.
    ///
    /// Dim is **not** a global product constant. Checked at process resolution
    /// (`crate::resolve::check_vector_compatibility`).
    fn vector_dimensions(&self) -> Option<usize> {
        None
    }

    /// Persist a mutation batch durably (spec §2.4–§2.5).
    ///
    /// **Fencing token (GitHub issue #1).** `token` is the holder's monotonic
    /// fencing token, minted by [`Self::acquire_lease`] at takeover and
    /// preserved on refresh. The store MUST reject any write whose token is
    /// below the session lease's `current_token` (or `None` when a token has
    /// been set) with [`StoreError::StaleWrite`] — never silently drop it. Pass
    /// `None` only for an unleased write (a session with no lease row, e.g.
    /// `seed()` / fixture parity), which the store permits. The three real
    /// backends enforce this; the advisory default (no lease) lets everything
    /// through.
    ///
    /// **Idempotency contract (flush replay, F5):** adapters MUST implement
    /// every mutation kind with upsert / `ON CONFLICT` semantics. The flush
    /// task may replay a batch that partially succeeded (a mid-batch backend
    /// failure, a retried flush, or a retained batch re-attempted after a
    /// store outage), so plain INSERTs are not acceptable: a replayed batch
    /// must converge to the same final state — never duplicate rows, never
    /// error on re-insertion. All Lambo mutation kinds are naturally
    /// idempotent this way (node/edge upserts by natural key, canonization
    /// transitions by event id); adapters must preserve that property.
    ///
    /// **`created_at` parity (STORE-5, accepted divergence):** after an identical
    /// flush history, `load_session` returns `created_at: Some(now)` on Cockroach
    /// (schema `TIMESTAMPTZ NOT NULL DEFAULT now()`) but `None` on SQLite/Memory
    /// (no session-metadata `Mutation` kind exists to bind it — S5 snapshot-only).
    /// Adapters may differ here; do NOT rely on `created_at` presence.
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError>;
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError>;

    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError>;

    /// Unchecked vector lookup retained as the frozen v0.2.0 adapter surface.
    ///
    /// Requires [`Capabilities::VECTOR_SEARCH`]; adapters without it return
    /// [`StoreError::Capability`]. `limit` must not exceed
    /// [`MAX_VECTOR_CANDIDATE_LIMIT`].
    ///
    /// New Lambo production code must call [`GraphStore::vector_candidates_checked`]
    /// instead. This method cannot bind the query's embedding contract to the
    /// durable candidate read and therefore cannot prevent a concurrent model
    /// replacement from making a ranking meaningless.
    ///
    /// **Duplicate `NodeId`s in the returned list are tolerated, and the highest
    /// score wins** (recall's phase-1 vector leg max-merges them); an adapter need
    /// not deduplicate, and a `Vec` carrying the same id twice will not change the
    /// ranking from what the best of those entries would have produced alone.
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError>;

    /// Race-safe vector lookup for query embeddings produced under
    /// `expected_contract`.
    ///
    /// A vector-capable adapter must compare the expected contract to the
    /// session's durable contract in the same transactional snapshot as every
    /// candidate query and refuse a mismatch. The fail-closed default preserves
    /// source compatibility for third-party v0.2.0 adapters without silently
    /// weakening H1: adapters that advertise vector search must explicitly add
    /// an atomic implementation before Lambo's production recall paths use it.
    /// Non-vector adapters retain their original capability refusal.
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        _expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        if self.capabilities().contains(Capabilities::VECTOR_SEARCH) {
            return Err(StoreError::Capability(
                "store advertises VECTOR_SEARCH but does not implement atomic embedding-contract validation"
                    .into(),
            ));
        }
        self.vector_candidates(session, embedding, limit).await
    }

    /// Count of concepts that would be orphaned by removing `node` (spec §4.1).
    ///
    /// **Type split (CON-6):** this surface returns `u64`, but the frozen
    /// [`crate::types::Concept::blast_radius`] /
    /// [`crate::types::CanonizationEvent::blast_radius`] fields are
    /// `Option<i32>` by pinned contract. Implementers MUST narrow at the write
    /// gate with a typed error — `u32::try_from(value).map_err(|_| invariant(
    /// ...))` — never a silent `as` cast: an out-of-range radius is an invariant
    /// violation, not a value to truncate. (P6's canonization sweep consumes
    /// this method and writes the i32 field.)
    ///
    /// **Injected clock (F8).** The age cutoff is `now - min_edge_age`, and
    /// `now` is the **caller's** instant — adapters must not reach for
    /// [`chrono::Utc::now`]. One canonization eval cycle takes its clock once
    /// (P6 §"`now` is injected"); an adapter-internal wall clock made a single
    /// cycle read two clocks and made the 60s inflation guard un-simulatable
    /// under a mocked clock, so every eval-level test had to zero it.
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError>;

    /// Distinct origin interactions behind `node`'s aged structural inbound
    /// edges, plus the share of the session's temporal extent they cover
    /// (spec §4.1). `now` is the caller's clock — see [`Self::blast_radius`].
    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<InteractionSpan, StoreError>;

    /// Durable write gate for a canonization transition (spec §10). **Fencing
    /// token (GitHub issue #1):** like [`Self::flush`], `token` must be the
    /// holder's monotonic token, and the store MUST reject a stale/missing one
    /// with [`StoreError::StaleWrite`]. This is the second of the two durable
    /// write gates — it previously had NO lease check at all (the canon task
    /// bypassed `lease_lost`), a real fencing gap this token closes.
    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError>;

    // -----------------------------------------------------------------------
    // Single-writer lease (spec §2.2, T8.6)
    // -----------------------------------------------------------------------

    /// Atomically acquire the session's single-writer lease for `holder`,
    /// valid for `ttl` (spec §2.2).
    ///
    /// **Atomic, fail-closed, no read-then-write race.** A backend implements
    /// this as ONE statement/transaction — an `INSERT ... ON CONFLICT` whose
    /// update is guarded by an expiry check — so the decision is made under the
    /// store's own concurrency control, never by reading the row and writing it
    /// back. The result is exactly one of:
    ///
    /// * [`LeaseOutcome::Acquired`] — the row was fresh, the prior lease had
    ///   expired, or it was already ours (a refresh). It is now ours until
    ///   `ttl` from the store's clock.
    /// * [`LeaseOutcome::Held`] — a *live* lease is held by someone else. The
    ///   caller must fail closed; the outcome carries the current holder and its
    ///   age for a diagnostic.
    ///
    /// **Clock discipline.** `ttl` is a duration, not a timestamp: the backend
    /// stamps `acquired_at`/`expires_at` from its own clock (spec §6.4 / F18).
    /// A caller-supplied absolute instant must never reach a lease row.
    ///
    /// **Default is advisory (non-enforcing).** The provided implementation
    /// always grants, persisting nothing — it exists so test doubles and any
    /// store that has not implemented enforcement keep their prior behaviour
    /// (single-writer was advisory before T8.6). The three real backends
    /// (`MemoryStore`, `SqliteStore`, `CockroachStore`) override it with true
    /// enforcement. A store that grants here provides no cross-process
    /// guarantee — the in-process `ACTIVE_SESSIONS` log is the only catch.
    async fn acquire_lease(
        &self,
        _session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        let now = Utc::now();
        Ok(LeaseOutcome::Acquired(LeaseInfo {
            holder: holder.token(),
            // Advisory default: no fencing token is minted (nothing is stored),
            // so the sentinel 0 means "never leased" — unleased writes pass.
            token: 0,
            acquired_at: now,
            expires_at: now
                + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::seconds(0)),
            // Advisory default: nothing is stored, so nothing can be read back
            // — see `read_lease`, whose default is `None` for the same reason.
            endpoint: holder.endpoint.clone(),
        }))
    }

    /// Read the session's lease row without touching it (J2).
    ///
    /// **Read-only, and that is load-bearing.** This is how a `serve` that lost
    /// the lease finds the current holder's `endpoint` so it can proxy, and how
    /// it re-finds it on every reconnect attempt. It must never acquire,
    /// refresh, or steal: a proxy that took the lease would heartbeat and hold
    /// a session it cannot serve, wedging every process on the machine — which
    /// is strictly worse than the exit-1 J2 exists to replace. See
    /// `mcp::serve`'s proxy loop, where the invariant is restated at the site
    /// that could violate it.
    ///
    /// `Ok(None)` means "no row", which the default returns unconditionally:
    /// an advisory store stores no lease, so it can report none. The three real
    /// backends override it. A caller must treat `None` as "no reachable
    /// holder" and fail honestly rather than guess an address.
    ///
    /// **A real adapter MUST override this** (J2-R1-13). The default exists so
    /// the in-tree `GraphStore` test doubles — which store no lease — need no
    /// edit, not as a behaviour any adapter should inherit. An adapter that
    /// keeps it silently disables proxying for every session on that store, and
    /// the symptom is indistinguishable from a legitimate one: a refused serve
    /// reports `HolderPublishedNoEndpoint` ("that holder published no
    /// endpoint"), which is exactly what a CLI verb holding the lease looks
    /// like. So the failure reads as "wait it out" and never resolves. If you
    /// are writing an adapter, the check is that a `read_lease` immediately
    /// after your own `acquire_lease` returns `Some` with the endpoint you
    /// published.
    async fn read_lease(&self, _session: &SessionId) -> Result<Option<LeaseInfo>, StoreError> {
        Ok(None)
    }

    /// Heartbeat: extend our own lease by `ttl` (spec §2.2). Same atomic
    /// upsert shape as [`Self::acquire_lease`] — a refresh is just a re-acquire
    /// by the same holder, so a holder whose lease was stolen after expiry
    /// learns it lost the session ([`LeaseOutcome::Held`]) instead of silently
    /// squatting a row it no longer owns. Default is advisory (always grants).
    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.acquire_lease(session, holder, ttl).await
    }

    /// Release our lease — the pair of a successful [`Self::acquire_lease`]
    /// (spec §2.2). A graceful close **releases** rather than waiting out the
    /// TTL, so the next writer takes over immediately.
    ///
    /// Idempotent and holder-scoped: it clears the row only if `holder` still
    /// owns it, so a stale release (after our lease already expired and was
    /// re-taken) can never evict the *new* holder. Default is a no-op.
    async fn release_lease(
        &self,
        _session: &SessionId,
        _holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    /// **J4.** Record that the session's lease was just refused to `refused_by`
    /// (the refused writer's holder token) against `current_holder` (the
    /// incumbent, from the lease row at refusal time). `at` is stamped by the
    /// store's own clock — never a caller instant (F18).
    ///
    /// The refused writer is alive on this path and calls this when it learns
    /// it lost; the incumbent learns the fact here too (its
    /// [`Self::pending_lease_refusals`] poll), which is what lets a refused
    /// acquisition appear in the ledger **from both sides** (J4). Best-effort: a
    /// store that cannot record (advisory default) is still a store that
    /// refuses; the refusal simply carries no cross-process record.
    async fn record_lease_refusal(
        &self,
        _session: &SessionId,
        _refused_by: &str,
        _current_holder: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    /// **J4.** Refusals recorded against `session` at or after store-clock
    /// `since`, newest first. The incumbent holder polls this to learn it
    /// turned away a takeover and to append its own ledger line. Default:
    /// none (advisory stores record nothing).
    async fn pending_lease_refusals(
        &self,
        _session: &SessionId,
        _since: DateTime<Utc>,
    ) -> Result<Vec<lease::LeaseRefusal>, StoreError> {
        Ok(vec![])
    }

    // -----------------------------------------------------------------------
    // Flush stats publication (T85-3)
    // -----------------------------------------------------------------------

    /// Persist this session's observable flush stats (T85-3).
    ///
    /// Called ONLY by the lease-holding writer's `FlushTask` after a flush
    /// cycle, so a reader in another process can render real
    /// `flush_lag_ms` / `log_depth` instead of `n/a`. A reader must never
    /// call this. Best-effort by callers: a publish failure must not perturb
    /// the flush path.
    ///
    /// **Default is a no-op** so non-target backends, test doubles, and the
    /// advisory-default stores keep their prior behaviour; the three real
    /// stores (`MemoryStore`, `SqliteStore`, `CockroachStore`) override it.
    async fn write_flush_stats(
        &self,
        _session: &SessionId,
        _stats: &SessionFlushStats,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    /// Read the session's persisted flush stats (T85-3), for a reader.
    ///
    /// Returns `None` when no writer has published yet, or when the store does
    /// not support it — the honest `n/a` fallback, not a fabricated value.
    /// Default returns `None` (no-op read) so non-target backends and test
    /// doubles keep their prior behaviour; the three real stores override it.
    async fn read_flush_stats(
        &self,
        _session: &SessionId,
    ) -> Result<Option<SessionFlushStats>, StoreError> {
        Ok(None)
    }
}

/// Operator-facing list of kinds, kept in one place so the empty-kind and
/// unknown-kind errors cannot drift apart.
const STORE_KIND_EXPECTED: &str = "memory | cockroach | postgres | sqlite";

/// Environment variable carrying the Cockroach DSN.
///
/// Must equal `CockroachDialect::DSN_ENV`; `store::pg::cockroach`'s test module
/// asserts it, so the operator-facing error and the variable configuration
/// actually reads cannot drift apart.
pub const COCKROACH_DSN_ENV: &str = "LAMBO_COCKROACH_DSN";

/// Environment variable carrying the PostgreSQL DSN.
///
/// Must equal `PostgresDialect::DSN_ENV`; `store::pg::postgres`'s test module
/// asserts it. Before E2E-F2 this name was printed in the missing-DSN error and
/// read by nothing in configuration resolution.
pub const POSTGRES_DSN_ENV: &str = "LAMBO_POSTGRES_DSN";

/// Shared fallback DSN variable, honoured by both wire-Postgres kinds.
///
/// Deliberately not narrowed to one kind: this is how CI and existing
/// deployments hand a DSN to a process without putting it in the file.
pub const FALLBACK_DSN_ENV: &str = "DATABASE_URL";

/// Durable store selector (TOML `store.kind` / `LAMBO_STORE`).
///
/// Deserialize accepts the same aliases as [`FromStr`] (trimmed, case-insensitive):
/// `memory|mem|ram`, `cockroach|crdb`, `postgres|pg`, `sqlite|sqlite3`.
///
/// **Alias split (B1).** Until 0.3.0, `"postgres"` and `"pg"` mapped onto
/// [`StoreKind::Cockroach`]. That was a choice: both engines speak the Postgres
/// wire protocol through the same sqlx driver, and there was no PostgreSQL
/// dialect yet. It is also a lie the Cockroach adapter's `VECTOR(n)`,
/// `CREATE VECTOR INDEX`, and `::STRING` never meant on PostgreSQL. B1 splits
/// the aliases so no string maps across the boundary; a leftover
/// `kind = "postgres"` pointed at Cockroach fails loud at construction (B2
/// has not landed DDL) rather than silently ranking with Cockroach SQL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreKind {
    /// In-RAM adapter (tests, fixture-ok tracks). Feature: `store-memory`.
    #[default]
    Memory,
    /// CockroachDB primary (P3 / T3.2). Feature: `store-cockroach`.
    /// Config strings: `"cockroach"`, `"crdb"`. Not `"postgres"` / `"pg"`.
    Cockroach,
    /// PostgreSQL + pgvector (workstream B). Feature: `store-postgres`.
    /// Config strings: `"postgres"`, `"pg"`. Fail-closed until B2 lands DDL.
    Postgres,
    /// SQLite offline / test tier (P3 / T3.3). Feature: `store-sqlite`.
    Sqlite,
}

impl<'de> Deserialize<'de> for StoreKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse()
            .map_err(|e: StoreError| serde::de::Error::custom(e.to_string()))
    }
}

impl StoreKind {
    /// The environment variable this kind reads its DSN from, if it has one.
    ///
    /// `None` for `memory` and `sqlite`: they have no DSN, so no environment
    /// variable can select their storage (SQLite uses `LAMBO_SQLITE_PATH`).
    pub const fn dsn_env(self) -> Option<&'static str> {
        match self {
            Self::Cockroach => Some(COCKROACH_DSN_ENV),
            Self::Postgres => Some(POSTGRES_DSN_ENV),
            Self::Memory | Self::Sqlite => None,
        }
    }

    pub const fn feature_name(self) -> &'static str {
        match self {
            Self::Memory => "store-memory",
            Self::Cockroach => "store-cockroach",
            Self::Postgres => "store-postgres",
            Self::Sqlite => "store-sqlite",
        }
    }

    /// Whether this kind's Cargo feature is compiled into the current binary.
    ///
    /// Note: `true` does not mean the adapter is fully implemented (see [`Self::is_ready`]).
    pub const fn is_compiled(self) -> bool {
        match self {
            Self::Memory => cfg!(feature = "store-memory"),
            Self::Cockroach => cfg!(feature = "store-cockroach"),
            Self::Postgres => cfg!(feature = "store-postgres"),
            Self::Sqlite => cfg!(feature = "store-sqlite"),
        }
    }

    /// Whether [`build_store`] can return a working adapter (feature on **and** impl exists).
    pub const fn is_ready(self) -> bool {
        match self {
            Self::Memory => cfg!(feature = "store-memory"),
            // T3.2: CockroachStore landed.
            Self::Cockroach => cfg!(feature = "store-cockroach"),
            // B2: templated width + hnsw from init. Compiled is ready.
            Self::Postgres => cfg!(feature = "store-postgres"),
            // T3.3: SqliteStore lands with feature store-sqlite.
            Self::Sqlite => cfg!(feature = "store-sqlite"),
        }
    }
}

// Registry design note (do not "simplify" away):
// * `is_compiled()` is a *message* pre-check so we can say "rebuild with --features X"
//   without naming uncompiled types.
// * The real gate is the `#[cfg(feature = "...")]` arm that constructs the adapter.
// * Both are required: pre-check alone cannot reference uncompiled types; cfg alone
//   yields a poorer error when the arm is missing.

impl FromStr for StoreKind {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        if t.is_empty() {
            return Err(StoreError::Backend(format!(
                "empty store kind (expected {STORE_KIND_EXPECTED})"
            )));
        }
        match t.to_ascii_lowercase().as_str() {
            "memory" | "mem" | "ram" => Ok(Self::Memory),
            "cockroach" | "crdb" => Ok(Self::Cockroach),
            "postgres" | "pg" => Ok(Self::Postgres),
            "sqlite" | "sqlite3" => Ok(Self::Sqlite),
            other => Err(StoreError::Backend(format!(
                "unknown store kind {other:?} (expected {STORE_KIND_EXPECTED})"
            ))),
        }
    }
}

impl std::fmt::Display for StoreKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Memory => write!(f, "memory"),
            Self::Cockroach => write!(f, "cockroach"),
            Self::Postgres => write!(f, "postgres"),
            Self::Sqlite => write!(f, "sqlite"),
        }
    }
}

/// Configuration for [`build_store`].
///
/// `Debug` redacts `dsn` (often carries a password). Serde still round-trips the real
/// value so configs can be written intentionally — never log `toml::to_string` of a live DSN.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    #[serde(default)]
    pub kind: StoreKind,
    /// Cockroach / Postgres DSN.
    ///
    /// Each DSN-bearing kind reads its **own** environment variable when this is
    /// absent: `LAMBO_COCKROACH_DSN` for `cockroach`, `LAMBO_POSTGRES_DSN` for
    /// `postgres`, with `DATABASE_URL` as a shared fallback for both. When this
    /// **is** set and the environment names a different database,
    /// [`StoreConfig::overlay_env`] refuses rather than picking a winner
    /// (E2E-F2).
    #[serde(default)]
    pub dsn: Option<String>,
    /// SQLite file path or `sqlite::memory:`.
    #[serde(default)]
    pub path: Option<String>,
    /// **Operator-asserted pre-ingest width pin** for a store whose vector column
    /// carries no width of its own (SQLite's `BLOB`). `None` — the default — means
    /// the store echoes the resolved `[embedder] dim`.
    ///
    /// Setting it is an assertion about the width this deployment's vectors use,
    /// which is why it can disagree with the embedder and why that disagreement is an
    /// error: `resolve::resolve_backends` refuses to resolve when the pin and the
    /// resolved `[embedder] dim` differ, naming both. That refusal is an **explicit
    /// comparison in `resolve_backends`**, not something `check_vector_compatibility`
    /// performs — the latter stays an *echo* for a width-agnostic store either way,
    /// since `vector_dimensions()` reports the pin itself once one is set, so the
    /// comparison is `x == x` with a pin exactly as it is without one (F-R1-2 /
    /// F-R2-3).
    ///
    /// **Reporting** ignores it on Cockroach: `VECTOR(n)` is parsed out of its own
    /// DDL, which is a real schema authority and outranks a config assertion. The
    /// **resolution check does not** — it is deliberately kind-agnostic, so a pin
    /// left behind in `lambo.toml` refuses a Cockroach or `memory` resolve too. A
    /// stale pin failing loud is the pin's whole job; on a store that persists no
    /// vectors at all (`memory`) the pin still asserts a width, and asserting one
    /// this process cannot honour is a config error worth refusing rather than
    /// silently ignoring (F-R2-4).
    #[serde(default)]
    pub vector_dim: Option<usize>,
}

impl std::fmt::Debug for StoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreConfig")
            .field("kind", &self.kind)
            .field(
                "dsn",
                &self.dsn.as_ref().map(|_| "***REDACTED***".to_string()),
            )
            .field("path", &self.path)
            .field("vector_dim", &self.vector_dim)
            .finish()
    }
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            kind: StoreKind::Memory,
            dsn: None,
            path: None,
            vector_dim: None,
        }
    }
}

impl StoreConfig {
    /// Resolve a non-empty DSN from env only, for the Cockroach kind.
    ///
    /// Kept as the Cockroach-shaped accessor the live Cockroach tests use.
    /// Configuration resolution goes through [`Self::dsn_from_env_for_kind`],
    /// which is kind-aware; this one is not, and must not be used to decide
    /// which database a process opens.
    pub fn dsn_from_env() -> Option<String> {
        Self::dsn_from_env_for_kind(StoreKind::Cockroach).map(|(_, dsn)| dsn)
    }

    /// Resolve a non-empty DSN from env for one store kind, with the name of
    /// the variable it came from.
    ///
    /// # Why this is kind-aware (E2E-F2)
    ///
    /// It used to read `LAMBO_COCKROACH_DSN` then `DATABASE_URL` for every
    /// kind. B1 then added `postgres`, so a `kind = "postgres"` deployment took
    /// its DSN from a variable named after the other engine: on a machine whose
    /// `.env` carries a production Cockroach DSN, every verb of a Postgres
    /// config pointed at that cluster. Meanwhile `LAMBO_POSTGRES_DSN` existed
    /// as the Postgres dialect's `DSN_ENV` associated constant (named rather
    /// than linked: `store::pg::dialect` is a private module, and a doc link
    /// into one is the warning class E2E-F7 closed and B-E2E-R2-6 re-closed),
    /// was named in the missing-DSN error an operator is told to act on, and
    /// was read by nothing in configuration resolution.
    ///
    /// So each DSN-bearing kind now reads its own variable, and the error
    /// message names a variable that works.
    ///
    /// `DATABASE_URL` stays a fallback for **both** wire-Postgres kinds: it is
    /// how CI and existing deployments pass a DSN they do not want in the file,
    /// and narrowing it would break the secret path while fixing the precedence
    /// path. It is not consulted for `memory` or `sqlite`, which have no DSN:
    /// a `DATABASE_URL` in the environment of a SQLite deployment described
    /// some other program's database, never this one's.
    ///
    /// Empty strings are treated as **unset** (same as other env knobs), so an
    /// empty `LAMBO_COCKROACH_DSN=` placeholder in `.env` neither selects a
    /// database nor triggers the disagreement refusal.
    pub fn dsn_from_env_for_kind(kind: StoreKind) -> Option<(&'static str, String)> {
        let primary = kind.dsn_env()?;
        if let Some(v) = env::var(primary).ok().filter(|s| !s.is_empty()) {
            return Some((primary, v));
        }
        env::var(FALLBACK_DSN_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .map(|v| (FALLBACK_DSN_ENV, v))
    }

    fn env_kind() -> Result<Option<StoreKind>, StoreError> {
        match env::var("LAMBO_STORE") {
            Ok(s) if !s.trim().is_empty() => Ok(Some(s.parse()?)),
            _ => Ok(None),
        }
    }

    /// Build from environment only. Equivalent to `Self::default().overlay_env()`.
    pub fn from_env() -> Result<Self, StoreError> {
        Self::default().overlay_env()
    }

    /// Merge env over a base (e.g. from `lambo.toml`).
    ///
    /// Non-empty env values win over file. Empty env values are treated as unset and leave
    /// the file value intact (including empty `LAMBO_COCKROACH_DSN=` placeholders in `.env`).
    ///
    /// # The one exception: a DSN the file already chose (E2E-F2)
    ///
    /// An explicitly configured `store.dsn` is **not** silently replaced by the
    /// environment. If the two name different databases this refuses and names
    /// both, in the same spirit as `resolve::resolve_backends`' `vector_dim`
    /// pin refusal: two authorities disagree about something load-bearing, and
    /// guessing which one the operator meant is how `lambo provision` came to
    /// issue DDL against a production cluster the config never mentioned.
    ///
    /// "Different databases" is measured by `store_dsn_identity` in
    /// `src/store/dsn.rs` (private, so named rather than linked from this
    /// public item: E2E-F7's class again), not by string equality, so
    /// the common secret-handling shape keeps working: a file DSN that names
    /// the database and an environment DSN that adds the password are the same
    /// database, the environment's spelling wins, and nothing is refused. Two
    /// spellings that differ in host, port, database or user are two databases
    /// and are refused.
    ///
    /// # What is compared is not what is printed (B-E2E-R4-2)
    ///
    /// The message quotes `store_dsn_echo`, not the identity. A spelling
    /// neither parser recognises is not quotable — it prints as
    /// `<unparseable dsn>`, because the only way to promise "(passwords
    /// stripped)" about a string we could not parse is to not echo it
    /// (B-E2E-R3-1) — but it is still *comparable*, and comparing it is this
    /// refusal's entire job. Round 3 used one function for both and so compared
    /// the placeholder: two malformed DSNs then looked identical here, the
    /// refusal fell silent, and the environment took a different real host than
    /// the file named. That is E2E-F2 reopened, and it is why the two halves are
    /// two functions. When a quote is withheld the message says so, so that two
    /// identical-looking placeholders do not read as a spurious refusal.
    pub fn overlay_env(mut self) -> Result<Self, StoreError> {
        if let Some(k) = Self::env_kind()? {
            self.kind = k;
        }
        if let Some((var, env_dsn)) = Self::dsn_from_env_for_kind(self.kind) {
            if let Some(file_dsn) = self.dsn.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                // Compared on identity, printed on echo: two different jobs and
                // two different functions since B-E2E-R4-2. Collapsing them was
                // what let two malformed-but-dialable DSNs naming different
                // hosts compare equal here and reopen E2E-F2.
                let id_file = crate::store::dsn::store_dsn_identity(file_dsn);
                let id_env = crate::store::dsn::store_dsn_identity(&env_dsn);
                if id_file != id_env {
                    let from_file = crate::store::dsn::store_dsn_echo(file_dsn);
                    let from_env = crate::store::dsn::store_dsn_echo(&env_dsn);
                    return Err(StoreError::Backend(format!(
                        "store.dsn and {var} name different databases: the config file says \
                         {from_file} and {var} says {from_env} (passwords stripped).{withheld} \
                         Refusing to guess which one you meant: the file is the single \
                         construction site for `store.kind = {kind}`, and letting the \
                         environment outrank it in silence means every verb, including \
                         `provision`, acts on a database the config never names. Unset {var}, \
                         drop store.dsn, or make them the same database.",
                        kind = self.kind,
                        withheld = crate::store::dsn::withheld_note(&from_file, &from_env),
                    )));
                }
            }
            self.dsn = Some(env_dsn);
        }
        if let Ok(v) = env::var("LAMBO_SQLITE_PATH") {
            if !v.is_empty() {
                self.path = Some(v);
            }
        }
        Ok(self)
    }
}

fn missing_feature(kind: StoreKind) -> StoreError {
    StoreError::Backend(format!(
        "store kind `{kind}` is not compiled into this binary; rebuild with \
         `--features {}` (see dev-diary/notes/level-b-pluggability.md)",
        kind.feature_name()
    ))
}

/// Level B store registry. Fail-closed when the kind's feature is off or the adapter
/// is not implemented yet.
///
/// Prefer [`crate::resolve::resolve_backends`] at process start so store×embedder
/// compatibility is checked once and the same instances are handed to the command.
pub fn build_store(cfg: StoreConfig) -> Result<Box<dyn GraphStore>, StoreError> {
    build_store_with_vector_dim(cfg, None)
}

/// [`build_store`], plus the process's configured dense-vector width.
///
/// SQLite consumes it: `concepts.embedding` is a `BLOB` with no width of its
/// own. Postgres consumes it too: B2 templates the width into `vector(n)` at
/// init, so this function copies the argument into the pin slot when the pin
/// is absent. Cockroach parses `VECTOR(n)` out of its own DDL and ignores this.
///
/// # Precedence
///
/// 1. [`StoreConfig::vector_dim`] — the operator's explicit pin, an assertion about
///    what the database holds. Wins over the embedder because it is the only one of
///    the three that is an independent authority (F-R1-2).
/// 2. the `vector_dim` argument — `resolve::resolve_backends` passes
///    `Some(embedder_cfg.dim)`, so with no pin the store reports the width the
///    process configured rather than a constant baked into the adapter. Note this
///    makes [`crate::resolve::check_vector_compatibility`] an **echo** rather than a
///    check on that path — and setting a pin does not change that: the pin becomes
///    the reported width, so the echo echoes the pin. What bites on a pin/embedder
///    disagreement is the explicit comparison in `resolve::resolve_backends`, which
///    runs *before* `check_vector_compatibility` and returns first (F-R2-3).
/// 3. the [`crate::embed::EmbedderConfig`] default — a **default, not a configured
///    width** (F-R1-8). Direct `build_store` callers (provision tools, store-only
///    verbs such as `resolve_store_only`, tests) land here, and nothing on that path
///    configured or verified the number: it may disagree with every session in the
///    database. It exists so no adapter needs a width constant of its own, and it is
///    inert because store-only verbs never embed.
///
/// **The pin is not enforced here.** Store construction must stay able to open a
/// database whose sessions carry a different contract — a future `lambo reembed`
/// migration verb needs exactly that. The refusal on a pin/embedder disagreement
/// belongs to the serving verbs' resolution path (`resolve::resolve_backends`).
pub fn build_store_with_vector_dim(
    cfg: StoreConfig,
    vector_dim: Option<usize>,
) -> Result<Box<dyn GraphStore>, StoreError> {
    // Pre-check for a clear rebuild hint (see module comment above is_compiled).
    if !cfg.kind.is_compiled() {
        return Err(missing_feature(cfg.kind));
    }
    // Precedence: the operator's pin outranks the resolved embedder width (see the
    // doc comment). SQLite and Postgres consume it (Postgres copies into the pin
    // slot below when the pin is absent). Cockroach ignores it. The binding stays
    // used under feature rows that compile none of them.
    let vector_dim = cfg.vector_dim.or(vector_dim);
    let _ = vector_dim;
    match cfg.kind {
        StoreKind::Memory => {
            // Real gate: type only exists under this feature.
            #[cfg(feature = "store-memory")]
            {
                Ok(Box::new(MemoryStore::new()))
            }
            #[cfg(not(feature = "store-memory"))]
            {
                Err(missing_feature(StoreKind::Memory))
            }
        }
        StoreKind::Cockroach => {
            // Real gate: type only exists under this feature. The pool is created lazily
            // (connect_lazy) so constructing the adapter never touches the network.
            #[cfg(feature = "store-cockroach")]
            {
                Ok(Box::new(cockroach::CockroachStore::new(cfg)?))
            }
            #[cfg(not(feature = "store-cockroach"))]
            {
                Err(missing_feature(StoreKind::Cockroach))
            }
        }
        StoreKind::Postgres => {
            #[cfg(feature = "store-postgres")]
            {
                // Copy the resolved embedder width into the pin slot when the
                // operator did not set one, so init_sql substitutes the width
                // this process actually embeds. The pin still wins when set
                // (precedence is already `cfg.vector_dim.or(param)` above).
                let mut cfg = cfg;
                if cfg.vector_dim.is_none() {
                    cfg.vector_dim = vector_dim;
                }
                Ok(Box::new(postgres::PostgresStore::new(cfg)?))
            }
            #[cfg(not(feature = "store-postgres"))]
            {
                Err(missing_feature(StoreKind::Postgres))
            }
        }
        StoreKind::Sqlite => {
            // Real gate: type only exists under this feature. The pool is
            // created lazily on first async use (build_store runs in a sync
            // startup context; see sqlite.rs).
            #[cfg(feature = "store-sqlite")]
            {
                // CON-3 (D2): a missing path is a hard error, mirroring
                // CockroachStore's missing-DSN check above — the durable tier
                // must never silently fall back to an ephemeral in-memory
                // database whose data evaporates at exit. Direct
                // `SqliteStore::connect("sqlite::memory:")` calls (tests)
                // bypass this: the check lives in config resolution, not in
                // `connect`.
                let path = cfg.path.clone().ok_or_else(|| {
                    StoreError::Backend(
                        "SqliteStore requires a path (store.path or LAMBO_SQLITE_PATH)".into(),
                    )
                })?;
                let store = SqliteStore::connect(&path)?;
                let store = match vector_dim {
                    Some(dim) => store.with_vector_dim(dim)?,
                    None => store,
                };
                Ok(Box::new(store))
            }
            #[cfg(not(feature = "store-sqlite"))]
            {
                Err(missing_feature(StoreKind::Sqlite))
            }
        }
    }
}

/// Build a store from environment variables only.
pub fn store_from_env() -> Result<Box<dyn GraphStore>, StoreError> {
    build_store(StoreConfig::from_env()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// J3 F5. The preflight's coverage is derived from the DDL, so this pins the
    /// derivation — including that it reads EVERY table in the shipped
    /// migration, which is what stops the check drifting behind the next schema
    /// addition the way `write_intents` drifted past every existing path.
    #[test]
    fn ddl_table_names_are_derived_from_the_shipped_migration() {
        let ddl = "\
-- a comment mentioning CREATE TABLE IF NOT EXISTS decoy (
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT NOT NULL
);
  CREATE TABLE IF NOT EXISTS write_intents(
    receipt TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_idx ON sessions (session_id);
";
        assert_eq!(tables_in_ddl(ddl), vec!["sessions", "write_intents"]);

        // The real migrations, so a DDL rewrite that breaks the idiom fails here
        // rather than silently emptying the preflight.
        #[cfg(feature = "store-sqlite")]
        {
            let sqlite = tables_in_ddl(include_str!("../../migrations/sqlite/001_init.sql"));
            assert!(
                sqlite.contains(&"write_intents") && sqlite.contains(&"sessions"),
                "{sqlite:?}"
            );
            assert_eq!(sqlite.len(), 11, "sqlite DDL table count: {sqlite:?}");
        }
        #[cfg(feature = "store-cockroach")]
        {
            let crdb = tables_in_ddl(include_str!("../../migrations/cockroach/001_init.sql"));
            assert!(
                crdb.contains(&"write_intents") && crdb.contains(&"sessions"),
                "{crdb:?}"
            );
            assert_eq!(crdb.len(), 11, "cockroach DDL table count: {crdb:?}");
        }
        #[cfg(feature = "store-postgres")]
        {
            let pg = tables_in_ddl(include_str!("../../migrations/postgres/001_init.sql"));
            assert!(
                pg.contains(&"write_intents") && pg.contains(&"sessions"),
                "{pg:?}"
            );
            assert_eq!(pg.len(), 11, "postgres DDL table count: {pg:?}");
        }
    }

    /// The refusal an operator reads must name the missing table AND the command
    /// that fixes it — a bare "capability not supported" would send them
    /// hunting.
    #[test]
    fn the_unprovisioned_refusal_is_actionable() {
        let one = unprovisioned_store_err("sqlite", &["write_intents"]).to_string();
        assert!(one.contains("missing a table"), "{one}");
        assert!(one.contains("write_intents"), "{one}");
        assert!(one.contains("lambo provision"), "{one}");
        let two = unprovisioned_store_err("cockroach", &["write_intents", "sessions"]).to_string();
        assert!(two.contains("missing tables"), "{two}");
        assert!(two.contains("write_intents, sessions"), "{two}");
    }

    #[test]
    fn vector_candidate_limit_is_bounded_and_checked() {
        validate_vector_candidate_limit(0).unwrap();
        validate_vector_candidate_limit(MAX_VECTOR_CANDIDATE_LIMIT).unwrap();
        assert!(validate_vector_candidate_limit(MAX_VECTOR_CANDIDATE_LIMIT + 1).is_err());
    }
    use crate::test_util::env_lock;

    #[test]
    fn parses_store_kind() {
        assert_eq!("memory".parse::<StoreKind>().unwrap(), StoreKind::Memory);
        assert_eq!("mem".parse::<StoreKind>().unwrap(), StoreKind::Memory);
        assert_eq!("ram".parse::<StoreKind>().unwrap(), StoreKind::Memory);
        assert_eq!(
            "cockroach".parse::<StoreKind>().unwrap(),
            StoreKind::Cockroach
        );
        assert_eq!("crdb".parse::<StoreKind>().unwrap(), StoreKind::Cockroach);
        assert_eq!("pg".parse::<StoreKind>().unwrap(), StoreKind::Postgres);
        assert_eq!("sqlite".parse::<StoreKind>().unwrap(), StoreKind::Sqlite);
        assert_eq!("sqlite3".parse::<StoreKind>().unwrap(), StoreKind::Sqlite);
        assert_eq!(
            "  postgres  ".parse::<StoreKind>().unwrap(),
            StoreKind::Postgres
        );
        // B1 alias split: no string maps across. A leftover kind = "postgres"
        // must not silently become Cockroach.
        assert_ne!(
            "postgres".parse::<StoreKind>().unwrap(),
            StoreKind::Cockroach
        );
        assert_ne!("pg".parse::<StoreKind>().unwrap(), StoreKind::Cockroach);
        assert_ne!(
            "cockroach".parse::<StoreKind>().unwrap(),
            StoreKind::Postgres
        );
        assert_ne!("crdb".parse::<StoreKind>().unwrap(), StoreKind::Postgres);
        let unknown = "oracle".parse::<StoreKind>().unwrap_err().to_string();
        assert!(
            unknown.contains(STORE_KIND_EXPECTED),
            "unknown-kind error must list postgres: {unknown}"
        );
        let empty = "".parse::<StoreKind>().unwrap_err().to_string();
        assert!(
            empty.contains(STORE_KIND_EXPECTED),
            "empty-kind error must list postgres: {empty}"
        );
        assert!("   ".parse::<StoreKind>().is_err());
    }

    #[test]
    fn toml_kind_aliases_match_from_str() {
        #[derive(Deserialize)]
        struct Wrap {
            kind: StoreKind,
        }
        let w: Wrap = toml::from_str(r#"kind = "mem""#).unwrap();
        assert_eq!(w.kind, StoreKind::Memory);
        let w: Wrap = toml::from_str(r#"kind = "crdb""#).unwrap();
        assert_eq!(w.kind, StoreKind::Cockroach);
        let w: Wrap = toml::from_str(r#"kind = "  pg  ""#).unwrap();
        assert_eq!(w.kind, StoreKind::Postgres);
        let w: Wrap = toml::from_str(r#"kind = "postgres""#).unwrap();
        assert_eq!(w.kind, StoreKind::Postgres);
        let w: Wrap = toml::from_str(r#"kind = "sqlite3""#).unwrap();
        assert_eq!(w.kind, StoreKind::Sqlite);
    }

    #[test]
    fn partial_store_toml_defaults() {
        let cfg: StoreConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.kind, StoreKind::Memory);
        assert!(cfg.dsn.is_none());

        // Empty table body still defaults kind.
        let cfg: StoreConfig = toml::from_str("path = \"./x.db\"").unwrap();
        assert_eq!(cfg.kind, StoreKind::Memory);
        assert_eq!(cfg.path.as_deref(), Some("./x.db"));
    }

    #[test]
    #[cfg(feature = "store-memory")]
    fn builds_memory_store() {
        let s = build_store(StoreConfig::default()).unwrap();
        assert_eq!(s.capabilities(), Capabilities::empty());
        assert!(StoreKind::Memory.is_ready());
    }

    #[test]
    fn cockroach_build_behavior() {
        // T3.2: with the feature compiled, build_store returns a working adapter
        // (constructed lazily — no connection at build time); without it, fail closed
        // with a rebuild hint and never fall back to memory.
        let cfg = StoreConfig {
            kind: StoreKind::Cockroach,
            dsn: Some("postgresql://localhost/lambo".into()),
            path: None,
            vector_dim: None,
        };
        if StoreKind::Cockroach.is_compiled() {
            let s = build_store(cfg).unwrap();
            assert!(s.capabilities().contains(Capabilities::VECTOR_SEARCH));
            assert_eq!(s.vector_dimensions(), Some(1024));
            assert!(StoreKind::Cockroach.is_ready());
        } else {
            let Err(err) = build_store(cfg) else {
                panic!("expected err — silent fallback forbidden");
            };
            let msg = err.to_string();
            assert!(
                msg.contains("not compiled") && msg.contains("store-cockroach"),
                "{msg}"
            );
            assert!(!msg.to_ascii_lowercase().contains("memory store"));
            assert!(!StoreKind::Cockroach.is_ready());
        }
    }

    #[test]
    fn postgres_build_behavior() {
        // B2: with the feature compiled, build_store returns a working adapter
        // (constructed lazily: no connection at build time); without it, fail
        // closed with a rebuild hint and never fall back to memory.
        let cfg = StoreConfig {
            kind: StoreKind::Postgres,
            dsn: Some("postgresql://localhost/lambo".into()),
            path: None,
            vector_dim: None,
        };
        if StoreKind::Postgres.is_compiled() {
            let s = build_store(cfg).expect("postgres store must build under store-postgres");
            assert!(s.capabilities().contains(Capabilities::VECTOR_SEARCH));
            assert_eq!(s.vector_dimensions(), Some(1024));
            assert!(StoreKind::Postgres.is_ready());
        } else {
            let Err(err) = build_store(cfg) else {
                panic!("expected err: silent fallback forbidden");
            };
            let msg = err.to_string();
            assert!(
                msg.contains("not compiled") && msg.contains("store-postgres"),
                "{msg}"
            );
            assert!(!msg.to_ascii_lowercase().contains("memory store"));
            assert!(!StoreKind::Postgres.is_ready());
        }
    }

    /// B2-R1-1: production is `resolve_backends` passing `Some(embedder_cfg.dim)`.
    /// Deleting the Postgres-arm copy of that argument into the pin slot must
    /// make this go red (silent init at 1024 against a 768 embedder).
    #[test]
    fn postgres_copies_embedder_width_when_pin_is_absent() {
        if !StoreKind::Postgres.is_compiled() {
            let cfg = StoreConfig {
                kind: StoreKind::Postgres,
                dsn: Some("postgresql://localhost/lambo".into()),
                path: None,
                vector_dim: None,
            };
            let Err(err) = build_store_with_vector_dim(cfg, Some(768)) else {
                panic!("expected err: silent fallback forbidden");
            };
            let msg = err.to_string();
            assert!(
                msg.contains("not compiled") && msg.contains("store-postgres"),
                "{msg}"
            );
            return;
        }
        let s = build_store_with_vector_dim(
            StoreConfig {
                kind: StoreKind::Postgres,
                dsn: Some("postgresql://localhost/lambo".into()),
                path: None,
                vector_dim: None,
            },
            Some(768),
        )
        .expect("pin-absent copy must construct");
        assert_eq!(
            s.vector_dimensions(),
            Some(768),
            "absent pin must take the embedder width, not default 1024"
        );
        let s = build_store_with_vector_dim(
            StoreConfig {
                kind: StoreKind::Postgres,
                dsn: Some("postgresql://localhost/lambo".into()),
                path: None,
                vector_dim: Some(1536),
            },
            Some(768),
        )
        .expect("pin must construct");
        assert_eq!(
            s.vector_dimensions(),
            Some(1536),
            "pin still outranks the embedder-width param"
        );
    }

    #[test]
    fn sqlite_builds_or_fails_closed_by_feature() {
        let r = build_store(StoreConfig {
            kind: StoreKind::Sqlite,
            dsn: None,
            path: Some("sqlite::memory:".into()),
            vector_dim: None,
        });
        if cfg!(feature = "store-sqlite") {
            // T3.3: with the feature on, build_store returns a working adapter.
            let s = r.expect("sqlite store must build under store-sqlite");
            // F2: SQLite advertises VECTOR_SEARCH, and the capability/width pair must
            // be self-consistent (CON-5) even on the bare `build_store` path that has
            // no embedder config to take a width from.
            assert_eq!(s.capabilities(), Capabilities::VECTOR_SEARCH);
            assert!(s.vector_dimensions().is_some());
            crate::resolve::check_vector_search_contract(s.as_ref(), StoreKind::Sqlite)
                .expect("capability and width must resolve together");
            assert!(StoreKind::Sqlite.is_ready());
        } else {
            let Err(err) = r else {
                panic!("expected err — silent fallback forbidden");
            };
            let msg = err.to_string();
            assert!(
                msg.contains("not compiled") && msg.contains("store-sqlite"),
                "{msg}"
            );
            assert!(!msg.to_ascii_lowercase().contains("memory store"));
            assert!(!StoreKind::Sqlite.is_ready());
        }
    }

    /// CON-3 (D2): selecting the sqlite tier without a path is a hard error —
    /// never a silent fallback to an ephemeral `sqlite::memory:` DB whose data
    /// evaporates at exit.
    #[test]
    fn sqlite_without_path_is_hard_error() {
        if !cfg!(feature = "store-sqlite") {
            // Without the feature the fail-closed arm reports "not compiled"
            // (covered by `sqlite_builds_or_fails_closed_by_feature`); the
            // no-path check only exists under the feature.
            return;
        }
        let err = build_store(StoreConfig {
            kind: StoreKind::Sqlite,
            dsn: None,
            path: None,
            vector_dim: None,
        })
        .err()
        .expect("sqlite without a path must hard-error, never fall back to memory");
        let msg = err.to_string();
        assert!(
            msg.contains("path") && msg.contains("LAMBO_SQLITE_PATH"),
            "{msg}"
        );
    }

    #[test]
    fn no_silent_fallback_to_memory() {
        // Selecting cockroach must not return Ok(MemoryStore).
        let r = build_store(StoreConfig {
            kind: StoreKind::Cockroach,
            dsn: None,
            path: None,
            vector_dim: None,
        });
        assert!(r.is_err());
    }

    #[test]
    fn feature_names() {
        assert_eq!(StoreKind::Memory.feature_name(), "store-memory");
        assert_eq!(StoreKind::Cockroach.feature_name(), "store-cockroach");
        assert_eq!(StoreKind::Postgres.feature_name(), "store-postgres");
        assert_eq!(StoreKind::Sqlite.feature_name(), "store-sqlite");
    }

    #[test]
    fn empty_toml_kind_rejected() {
        let r = toml::from_str::<StoreConfig>(r#"kind = """#);
        assert!(r.is_err(), "empty kind string must not parse");
    }

    #[test]
    fn dsn_from_env_and_overlay_aligned() {
        let _g = env_lock();
        // Clean slate.
        env::remove_var("LAMBO_COCKROACH_DSN");
        env::remove_var("DATABASE_URL");
        env::remove_var("LAMBO_STORE");
        env::remove_var("LAMBO_SQLITE_PATH");

        // E2E-F2: the base kind used to be `Memory`, a kind with no DSN at all,
        // and the overlay handed it one anyway. The overlay is kind-aware now,
        // so this half has to run on a kind that really reads a DSN. `dsn: None`
        // is the shape CI and existing deployments use (secret in the
        // environment, not in the file), and it is the behaviour that must not
        // change.
        let base = StoreConfig {
            kind: StoreKind::Cockroach,
            dsn: None,
            path: None,
            vector_dim: None,
        };

        // No DSN env, no file DSN.
        assert_eq!(base.clone().overlay_env().unwrap().dsn, None);
        assert_eq!(StoreConfig::dsn_from_env(), None);

        // Empty primary (placeholder in .env) is unset, not a selection.
        env::set_var("LAMBO_COCKROACH_DSN", "");
        env::remove_var("DATABASE_URL");
        assert_eq!(StoreConfig::dsn_from_env(), None);
        assert_eq!(base.clone().overlay_env().unwrap().dsn, None);

        // Empty primary, secondary set: DATABASE_URL still works, which is the
        // secret path E2E-F2's fix must not break.
        env::set_var("DATABASE_URL", "from-database-url");
        assert_eq!(
            StoreConfig::dsn_from_env().as_deref(),
            Some("from-database-url")
        );
        assert_eq!(
            base.clone().overlay_env().unwrap().dsn.as_deref(),
            Some("from-database-url")
        );

        // Primary non-empty beats secondary.
        env::set_var("LAMBO_COCKROACH_DSN", "from-primary");
        assert_eq!(StoreConfig::dsn_from_env().as_deref(), Some("from-primary"));
        assert_eq!(
            base.clone().overlay_env().unwrap().dsn.as_deref(),
            Some("from-primary")
        );

        // Both empty (vars present) is unset.
        env::set_var("LAMBO_COCKROACH_DSN", "");
        env::set_var("DATABASE_URL", "");
        assert_eq!(StoreConfig::dsn_from_env(), None);
        assert_eq!(base.overlay_env().unwrap().dsn, None);

        env::remove_var("LAMBO_COCKROACH_DSN");
        env::remove_var("DATABASE_URL");
    }

    /// E2E-F2, the secret path: a file DSN that names the database and an
    /// environment DSN that adds the password are the **same** database. That
    /// must not be refused, and the environment's spelling (the one with the
    /// credentials) is what the process opens.
    #[test]
    fn env_dsn_that_only_adds_credentials_overlays_the_file_dsn() {
        let _g = env_lock();
        env::remove_var("LAMBO_COCKROACH_DSN");
        env::remove_var("DATABASE_URL");
        env::remove_var("LAMBO_STORE");

        let file = StoreConfig {
            kind: StoreKind::Cockroach,
            dsn: Some("postgresql://u@db.example:26257/lambo?sslmode=verify-full".into()),
            path: None,
            vector_dim: None,
        };
        env::set_var(
            "LAMBO_COCKROACH_DSN",
            "postgres://u:s3cret@db.example:26257/lambo",
        );
        let o = file
            .clone()
            .overlay_env()
            .expect("same database, no refusal");
        assert_eq!(
            o.dsn.as_deref(),
            Some("postgres://u:s3cret@db.example:26257/lambo"),
            "the environment's spelling wins when both name one database"
        );

        // Same story through DATABASE_URL.
        env::remove_var("LAMBO_COCKROACH_DSN");
        env::set_var("DATABASE_URL", "postgres://u:s3cret@db.example:26257/lambo");
        assert!(file.overlay_env().is_ok(), "DATABASE_URL must stay usable");

        env::remove_var("DATABASE_URL");
    }

    /// E2E-F2, the precedence path: an explicitly configured `store.dsn` is
    /// never silently replaced by an environment DSN naming a **different**
    /// database. The refusal names both sides and the variable.
    ///
    /// This is the regression that let `lambo provision` issue DDL against a
    /// production Cockroach cluster from `.env` while the operator read a local
    /// container's DSN in their `lambo.toml`.
    #[test]
    fn env_dsn_naming_a_different_database_is_refused_not_preferred() {
        let _g = env_lock();
        env::remove_var("LAMBO_COCKROACH_DSN");
        env::remove_var("LAMBO_POSTGRES_DSN");
        env::remove_var("DATABASE_URL");
        env::remove_var("LAMBO_STORE");

        for (kind, var) in [
            (StoreKind::Cockroach, "LAMBO_COCKROACH_DSN"),
            (StoreKind::Postgres, "LAMBO_POSTGRES_DSN"),
            (StoreKind::Cockroach, "DATABASE_URL"),
            (StoreKind::Postgres, "DATABASE_URL"),
        ] {
            let cfg = StoreConfig {
                kind,
                dsn: Some("postgresql://lambo:lambo@127.0.0.1:55432/lambo?sslmode=disable".into()),
                path: None,
                vector_dim: None,
            };
            env::set_var(var, "postgresql://prod:hunter2@cluster.example:26257/decoy");
            let err = cfg
                .overlay_env()
                .expect_err("a different database must be refused, not silently preferred")
                .to_string();
            env::remove_var(var);

            assert!(
                err.contains(var),
                "the refusal must name the variable: {err}"
            );
            assert!(err.contains("store.dsn"), "{err}");
            assert!(
                err.contains("127.0.0.1:55432/lambo"),
                "must name the file's database: {err}"
            );
            assert!(
                err.contains("cluster.example:26257/decoy"),
                "must name the environment's database: {err}"
            );
            assert!(
                !err.contains("hunter2"),
                "the refusal must not leak a password: {err}"
            );
            assert!(
                !err.contains("lambo:lambo"),
                "the refusal must not leak a password: {err}"
            );
        }
    }

    /// B-E2E-R4-2: E2E-F2's refusal fires even when neither side can be quoted.
    ///
    /// This is the round-4 review's constructed reopening, run end to end. Both
    /// DSNs defeat `parse_postgres_url` (the empty port) and both fail the echo
    /// gate (the literal "password" in the database name), so under round 3 —
    /// where one function served as both the printer and the identity — the two
    /// sides canonicalised to the same `<unparseable dsn>`, `overlay_env` found
    /// them equal, skipped this refusal, and set `self.dsn` to the
    /// environment's. `sqlx::postgres::PgConnectOptions::from_str` resolves them
    /// to `host-a`/`db_password_a` and `host-b`/`db_password_b`, so that silence
    /// handed every verb, `provision` included, a different real database than
    /// the config file named. That is the P1 this whole refusal exists for.
    ///
    /// To watch this go red, make `store::dsn::unaccounted_identity` return the
    /// placeholder — which is exactly what round 3 shipped.
    #[test]
    fn two_unquotable_dsns_still_reach_the_disagreement_refusal() {
        let _g = env_lock();
        env::remove_var("LAMBO_COCKROACH_DSN");
        env::remove_var("DATABASE_URL");
        env::remove_var("LAMBO_STORE");

        let cfg = StoreConfig {
            kind: StoreKind::Postgres,
            dsn: Some("postgres://app@host-a:/db_password_a".into()),
            path: None,
            vector_dim: None,
        };
        env::set_var("LAMBO_POSTGRES_DSN", "postgres://app@host-b:/db_password_b");
        let err = cfg
            .clone()
            .overlay_env()
            .expect_err("two unquotable spellings are still two databases")
            .to_string();

        assert!(err.contains("LAMBO_POSTGRES_DSN"), "{err}");
        assert!(err.contains("store.dsn"), "{err}");
        // Neither side is quoted, and the message says why rather than
        // presenting two identical placeholders as a disagreement.
        assert!(err.contains("<unparseable dsn>"), "{err}");
        assert!(
            err.contains("is not a quotation"),
            "a refusal whose two quotes read the same must explain itself: {err}"
        );
        assert!(
            !err.contains("host-a") && !err.contains("host-b"),
            "the withheld spelling must stay withheld: {err}"
        );

        // The same spelling on both sides is not a disagreement, and still is
        // not one when it is a spelling we cannot parse. This is the half of
        // round 3's behaviour that the digest deliberately keeps.
        env::set_var("LAMBO_POSTGRES_DSN", "postgres://app@host-a:/db_password_a");
        assert_eq!(
            cfg.overlay_env().unwrap().dsn.as_deref(),
            Some("postgres://app@host-a:/db_password_a"),
        );
        env::remove_var("LAMBO_POSTGRES_DSN");
    }

    /// E2E-F2: `LAMBO_POSTGRES_DSN` is the Postgres kind's variable and
    /// `LAMBO_COCKROACH_DSN` is the Cockroach kind's. Neither reaches across,
    /// which is what stopped a `kind = "postgres"` config on a machine with a
    /// production `LAMBO_COCKROACH_DSN` in `.env` from opening that cluster.
    #[test]
    fn each_kind_reads_its_own_dsn_env_var() {
        let _g = env_lock();
        env::remove_var("DATABASE_URL");
        env::remove_var("LAMBO_STORE");
        env::set_var("LAMBO_COCKROACH_DSN", "crdb-dsn");
        env::set_var("LAMBO_POSTGRES_DSN", "pg-dsn");

        let of = |kind| {
            StoreConfig {
                kind,
                dsn: None,
                path: None,
                vector_dim: None,
            }
            .overlay_env()
            .unwrap()
            .dsn
        };
        assert_eq!(of(StoreKind::Cockroach).as_deref(), Some("crdb-dsn"));
        assert_eq!(of(StoreKind::Postgres).as_deref(), Some("pg-dsn"));
        // Kinds with no DSN take none, from any variable.
        assert_eq!(of(StoreKind::Sqlite), None);
        assert_eq!(of(StoreKind::Memory), None);
        env::set_var("DATABASE_URL", "shared-dsn");
        assert_eq!(of(StoreKind::Sqlite), None);
        assert_eq!(of(StoreKind::Memory), None);

        assert_eq!(
            StoreConfig::dsn_from_env_for_kind(StoreKind::Postgres),
            Some(("LAMBO_POSTGRES_DSN", "pg-dsn".to_string()))
        );
        assert_eq!(
            StoreConfig::dsn_from_env_for_kind(StoreKind::Cockroach),
            Some(("LAMBO_COCKROACH_DSN", "crdb-dsn".to_string()))
        );
        assert_eq!(StoreConfig::dsn_from_env_for_kind(StoreKind::Sqlite), None);

        env::remove_var("LAMBO_COCKROACH_DSN");
        env::remove_var("LAMBO_POSTGRES_DSN");
        env::remove_var("DATABASE_URL");
    }

    #[test]
    fn dsn_redacted_in_debug() {
        let cfg = StoreConfig {
            kind: StoreKind::Cockroach,
            dsn: Some("postgresql://user:s3cret@host:26257/lambo".into()),
            path: None,
            vector_dim: None,
        };
        let s = format!("{cfg:?}");
        assert!(s.contains("REDACTED"), "{s}");
        assert!(!s.contains("s3cret"), "{s}");
    }

    #[test]
    fn from_env_is_default_overlay() {
        let _g = env_lock();
        env::remove_var("LAMBO_STORE");
        env::remove_var("LAMBO_COCKROACH_DSN");
        env::remove_var("DATABASE_URL");
        env::remove_var("LAMBO_SQLITE_PATH");
        assert_eq!(
            StoreConfig::from_env().unwrap(),
            StoreConfig::default().overlay_env().unwrap()
        );
        env::set_var("LAMBO_STORE", "sqlite");
        env::set_var("LAMBO_SQLITE_PATH", "/tmp/x.db");
        assert_eq!(
            StoreConfig::from_env().unwrap(),
            StoreConfig::default().overlay_env().unwrap()
        );
        env::remove_var("LAMBO_STORE");
        env::remove_var("LAMBO_SQLITE_PATH");
    }

    #[test]
    fn unknown_toml_field_rejected() {
        assert!(toml::from_str::<StoreConfig>(r#"knd = "cockroach""#).is_err());
    }

    #[test]
    fn empty_lambo_store_env_keeps_default_kind() {
        let _g = env_lock();
        env::set_var("LAMBO_STORE", "");
        env::remove_var("LAMBO_COCKROACH_DSN");
        env::remove_var("DATABASE_URL");
        let cfg = StoreConfig::from_env().unwrap();
        assert_eq!(cfg.kind, StoreKind::Memory);
        let o = StoreConfig {
            kind: StoreKind::Sqlite,
            dsn: None,
            path: None,
            vector_dim: None,
        }
        .overlay_env()
        .unwrap();
        // Empty LAMBO_STORE is unset → keep file kind.
        assert_eq!(o.kind, StoreKind::Sqlite);
        env::remove_var("LAMBO_STORE");
    }
    #[tokio::test]
    #[cfg(feature = "store-memory")]
    async fn flush_stats_write_then_read_round_trips_memory() {
        // T85-3: a writer publishes flush stats; a reader (same store, possibly
        // another process) reads them back. Absent = honest `None` (n/a).
        let store = MemoryStore::new();
        let sid = SessionId::from("stats-roundtrip");

        assert_eq!(store.read_flush_stats(&sid).await.unwrap(), None);

        let stats = SessionFlushStats {
            flush_lag_ms: 42,
            log_depth: 7,
        };
        store.write_flush_stats(&sid, &stats).await.unwrap();
        assert_eq!(store.read_flush_stats(&sid).await.unwrap(), Some(stats));

        // A different session stays `None` (absent = n/a, never fabricated 0).
        assert_eq!(
            store
                .read_flush_stats(&SessionId::from("other"))
                .await
                .unwrap(),
            None
        );

        // Re-publish converges (idempotent): the whole row is replaced.
        let later = SessionFlushStats {
            flush_lag_ms: 99,
            log_depth: 1,
        };
        store.write_flush_stats(&sid, &later).await.unwrap();
        assert_eq!(store.read_flush_stats(&sid).await.unwrap(), Some(later));
    }
}

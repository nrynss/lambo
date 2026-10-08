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
//! Here, in the family's shared base: [`PgStore`], its construction, and the
//! single `impl GraphStore`, whose methods only delegate. The work lives in
//! submodules:
//!
//! * `pool` — the lazy pool, Cloud SQL IAM rotation, connect options and
//!   `tx_retry`.
//! * `sql` — statement text, written **once**: constants (identical on every
//!   engine), `DialectSql` (identical except for a cast token) and the
//!   multi-row query builders.
//! * `schema` — `init_schema`, preflight, the live width check.
//! * `persistence` — **every write transaction** (`flush`,
//!   `record_canonization`, `seed`): session stamp, fencing gate, commit, all
//!   inside `tx_retry`.
//! * `write_rows` — the statements those transactions run. Never begins or
//!   commits.
//! * `session_load` — `load_session`'s one read transaction.
//! * `vector_candidates` — the checked vector read and DECISION D1 sizing.
//! * `structural` — keyword candidates, `blast_radius`, `interaction_span`.
//! * `leases` — lease rows and the refusal log.
//! * `codec` — value and row codecs shared by all of the above.
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

mod codec;
mod dialect;
mod leases;
mod persistence;
mod pool;
mod schema;
mod session_load;
mod sql;
mod structural;
mod vector_candidates;
mod write_rows;
pub use dialect::Dialect;

// #1 fencing on delete-only batches: the shared live check (Postgres test
// here, Cockroach leg in its conformance suite, which needs `fixtures`).
#[cfg(all(
    test,
    any(
        feature = "store-postgres",
        all(feature = "store-cockroach", feature = "fixtures")
    )
))]
pub(crate) mod delete_fencing;

// #23 session erasure: the offline DDL-coverage check plus the shared live
// check (Postgres test here, Cockroach leg in its conformance suite).
#[cfg(all(
    test,
    any(
        feature = "store-postgres",
        all(feature = "store-cockroach", feature = "fixtures")
    )
))]
pub(crate) mod erase;

// #1 fencing across a clean release (#23 review H2): the shared live check
// (Postgres test here, Cockroach leg in its conformance suite).
#[cfg(all(
    test,
    any(
        feature = "store-postgres",
        all(feature = "store-cockroach", feature = "fixtures")
    )
))]
pub(crate) mod release_fencing;

// Postgres-only: the flush fence's lease row stays locked until commit.
#[cfg(all(test, feature = "store-postgres"))]
mod lease_race;

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
use std::marker::PhantomData;
use std::time::Duration;

use sqlx::PgPool;

use crate::store::lease::{LeaseHolder, LeaseInfo, LeaseOutcome};
use crate::store::vector_source::VectorCandidateSource;
use crate::store::{Capabilities, GraphStore, SessionFlushStats, StoreConfig};
use crate::types::{
    CanonizationEvent, EmbeddingContract, GraphSnapshot, InteractionSpan, MutationBatch, NodeId,
    Scored, SessionId, StoreError,
};
use codec::backend;
use pool::dsn_for_rustls;
#[cfg(feature = "store-postgres")]
pub(crate) use pool::iam_auth_requested;
#[cfg(feature = "store-postgres")]
use pool::{IamAuth, IamSetup};
use sql::DialectSql;

// The dialect test modules (`cockroach::tests`, `postgres::tests` and the
// live suites) reach the family's items through `use super::*` and
// `crate::store::pg::<name>`, as they did when this was one file. This
// test-only surface keeps every such name in scope now that the items live in
// submodules. Which names a given feature row's tests use varies, hence the
// allow.
#[cfg(test)]
#[allow(unused_imports)]
use {
    crate::store::batch::*, crate::store::columns_in_ddl, crate::store::lease::lease_permits_write,
    crate::store::map_write_err, crate::store::tables_in_ddl,
    crate::store::unprovisioned_column_err, crate::store::unprovisioned_store_err,
    crate::store::validate_vector_candidate_limit, crate::store::vector::*, crate::types::*,
    codec::*, leases::*, persistence::*, pool::*, schema::*, session_load::*, sql::*,
    sqlx::postgres::PgPoolOptions, sqlx::Row, std::future::Future, structural::*, uuid::Uuid,
    vector_candidates::*, write_rows::*,
};

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
}

#[async_trait]
impl<D: Dialect> GraphStore for PgStore<D> {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.apply_schema().await
    }

    async fn preflight_schema(&self) -> Result<(), StoreError> {
        self.verify_schema().await
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
        self.select_interaction_span(session, node, min_age, now)
            .await
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

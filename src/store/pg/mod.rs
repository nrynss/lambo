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

mod codec;
mod dialect;
mod leases;
mod pool;
mod session_load;
mod sql;
mod structural;
mod vector_candidates;
mod write_rows;
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
use std::marker::PhantomData;
use std::time::Duration;

use sqlx::{PgPool, Row};

use crate::store::batch::{
    batch_session_ids, plan_flush, BulkLimits, ACCESS_COLUMNS, CONCEPT_COLUMNS, EDGE_COLUMNS,
    INTERACTION_COLUMNS,
};
#[cfg(feature = "fixtures")]
use crate::store::batch::{seed_concept_rows, seed_edge_rows};
use crate::store::lease::{lease_permits_write, LeaseHolder, LeaseInfo, LeaseOutcome};
use crate::store::{
    columns_in_ddl, map_write_err, tables_in_ddl, unprovisioned_column_err,
    unprovisioned_store_err, Capabilities, GraphStore, SessionFlushStats, StoreConfig,
};
use crate::types::{
    CanonizationEvent, EmbeddingContract, GraphSnapshot, InteractionSpan, MutationBatch, NodeId,
    Scored, SessionId, StoreError,
};
pub(crate) use codec::parse_pgvector_format_type;
use codec::*;
#[cfg(feature = "store-postgres")]
pub(crate) use pool::iam_auth_requested;
use pool::{dsn_for_rustls, tx_retry};
#[cfg(feature = "store-postgres")]
use pool::{IamAuth, IamSetup};
use sql::*;
use write_rows::*;

// The dialect test modules (`cockroach::tests`, `postgres::tests` and the
// live suites) reach the family's items through `use super::*` and
// `crate::store::pg::<name>`, as they did when this was one file. This
// test-only surface keeps every such name in scope now that the items live in
// submodules. Which names a given feature row's tests use varies, hence the
// allow.
#[cfg(test)]
#[allow(unused_imports)]
use {
    crate::store::batch::*, crate::store::validate_vector_candidate_limit, crate::store::vector::*,
    crate::types::*, codec::*, leases::*, pool::*, session_load::*, sql::*,
    sqlx::postgres::PgPoolOptions, std::future::Future, structural::*, uuid::Uuid,
    vector_candidates::*, write_rows::*,
};

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
    // Issue #30: 4 binds per row; a whole realistic tick's accesses in one
    // round-trip on a serverless cluster.
    accesses: 1024,
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
const _: () = assert!(
    BULK_LIMITS.accesses * ACCESS_COLUMNS <= PG_MAX_BIND_PARAMETERS,
    "accesses chunk exceeds the PostgreSQL bind-parameter limit"
);

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
        // Issue #17: the seeded snapshot carries the mutation accounting.
        let mutation_epoch = i64::try_from(snapshot.mutation_epoch).unwrap_or(i64::MAX);
        // Issue #29: and GC's sweep mark.
        let last_gc_epoch = i64::try_from(snapshot.gc_mark.last_gc_epoch).unwrap_or(i64::MAX);
        let last_gc_at = snapshot.gc_mark.last_gc_at;
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
                .bind(mutation_epoch)
                .bind(last_gc_epoch)
                .bind(last_gc_at)
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
            // Issue #17: the same statement stamps the batch's absolute mutation-epoch
            // watermark, monotonically, in this transaction.
            for sid in batch_session_ids(&batch.mutations) {
                sqlx::query(UPSERT_SESSION_ROW_SQL)
                    .bind(sid)
                    .bind(i64::try_from(batch.mutation_epoch).unwrap_or(i64::MAX))
                    .bind(i64::try_from(batch.gc_mark.last_gc_epoch).unwrap_or(i64::MAX))
                    .bind(batch.gc_mark.last_gc_at)
                    .bind(batch.gc_mark.last_gc_at_reset)
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

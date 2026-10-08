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
mod sql;
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
    batch_session_ids, plan_flush, AccessUpdate, BulkLimits, ConceptRow, FlushStep, ACCESS_COLUMNS,
    CONCEPT_COLUMNS, EDGE_COLUMNS, INTERACTION_COLUMNS,
};
#[cfg(feature = "fixtures")]
use crate::store::batch::{seed_concept_rows, seed_edge_rows};
use crate::store::lease::{lease_permits_write, LeaseHolder, LeaseInfo, LeaseOutcome};
use crate::store::vector::encode_vector;
use crate::store::{
    columns_in_ddl, map_write_err, tables_in_ddl, unprovisioned_column_err,
    unprovisioned_store_err, validate_vector_candidate_limit, Capabilities, GraphStore,
    SessionFlushStats, StoreConfig,
};
use crate::types::{
    CanonizationEvent, Edge, EmbeddingContract, GraphSnapshot, Interaction, InteractionSpan,
    Mutation, MutationBatch, Node, NodeId, Scored, SessionId, StoreError,
};
pub(crate) use codec::parse_pgvector_format_type;
use codec::*;
#[cfg(feature = "store-postgres")]
pub(crate) use pool::iam_auth_requested;
use pool::{dsn_for_rustls, tx_retry};
#[cfg(feature = "store-postgres")]
use pool::{IamAuth, IamSetup};
use sql::*;

// The dialect test modules (`cockroach::tests`, `postgres::tests` and the
// live suites) reach the family's items through `use super::*` and
// `crate::store::pg::<name>`, as they did when this was one file. This
// test-only surface keeps every such name in scope now that the items live in
// submodules. Which names a given feature row's tests use varies, hence the
// allow.
#[cfg(test)]
#[allow(unused_imports)]
use {
    crate::types::*, codec::*, leases::*, pool::*, sql::*, sqlx::postgres::PgPoolOptions,
    std::future::Future, uuid::Uuid,
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
// Helpers
// ---------------------------------------------------------------------------

/// Keep only rows belonging to the caller's session from one global top-k fetch.
/// Input rows arrive in L2-distance-ascending order (SQL `ORDER BY dist ASC`), so
/// the survivors keep that order: the trait's score-descending ordering contract,
/// with the issue-2 tie-break ([`order_candidates`]) deciding equal scores.
/// Pure & deterministic: unit-tested without a cluster.
fn filter_session_rows<D: Dialect>(
    session: &SessionId,
    rows: &[(NodeId, f64, String, String)],
) -> Vec<Scored<NodeId>> {
    order_candidates(
        rows.iter()
            .filter(|(_, dist, sid, _)| sid == &session.0 && dist.is_finite())
            .map(|(id, dist, _, key)| (Scored::new(*id, D::distance_to_score(*dist)), key.clone()))
            .collect(),
    )
}

/// True when the global fetch's kth and lookahead distances tie. The canonical
/// key tie-break can only order rows the fetch actually returned, so a tie
/// group cut by SQL's LIMIT has an arbitrary subset — this still forces the
/// exact session query, which re-fetches the session's own rows and re-orders
/// them with the same comparator (the forcing rule is unchanged by issue #2).
/// That makes the re-fetch authoritative, not unconditionally deterministic
/// (remediation round 1 doc alignment): a tie group outgrowing the exact
/// query's own LIMIT is still cut by its SQL `id` order, and rows sharing one
/// canonical key fall through to that run-minted id — the residual per-run
/// arbitrariness no in-process tie-break can remove.
fn has_boundary_tie(rows: &[(NodeId, f64, String, String)], k: usize) -> bool {
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

    async fn session_exists(&self, session: &SessionId) -> Result<bool, StoreError> {
        let pool = &self.pool().await?;
        let row = sqlx::query("SELECT 1 AS one FROM sessions WHERE session_id = $1")
            .bind(&session.0)
            .fetch_optional(pool)
            .await
            .map_err(backend)?;
        Ok(row.is_some())
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
        FlushStep::Accesses(rows) => bulk_update_accesses(&mut *tx, rows).await,
    }
}

/// Apply one chunk of read accesses (issue #30) as ONE narrow `UPDATE` of the
/// two access columns: no embedding rewrite, so on PostgreSQL the row update
/// changes no indexed column and is eligible for a HOT update, and neither
/// engine re-touches the vector index for a read. Existing rows only.
async fn bulk_update_accesses(
    tx: &mut sqlx::PgConnection,
    rows: &[AccessUpdate<'_>],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    access_update_query(rows)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("record accesses: {m}")))?;
    Ok(())
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
            bulk_update_accesses(&mut *tx, &[row]).await?;
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
            // Issue #17: the durable mutation counter, stamped by flush and
            // seeded by `seed`; the loading writer resumes it so GC's
            // `gc_interval` measures deployment-lifetime mutations.
            let mutation_epoch: i64 = session_row.try_get("mutation_epoch").map_err(backend)?;
            // Issue #29: GC's sweep accounting, resumed with the epoch so a
            // restart neither re-sweeps nor resets the `gc_max_interval` clock.
            let last_gc_epoch: i64 = session_row.try_get("last_gc_epoch").map_err(backend)?;
            let gc_mark = crate::types::GcMark {
                last_gc_epoch: u64::try_from(last_gc_epoch).unwrap_or(0),
                last_gc_at: session_row.try_get("last_gc_at").map_err(backend)?,
                last_gc_at_reset: false,
            };

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
                mutation_epoch: u64::try_from(mutation_epoch).unwrap_or(u64::MAX),
                gc_mark,
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

        let scored: Vec<(Scored<NodeId>, String)> = rows
            .iter()
            .map(|r| {
                let id: String = r.try_get("id").map_err(backend)?;
                let content: String = r.try_get("content").map_err(backend)?;
                let key: String = r.try_get("canonical_key").map_err(backend)?;
                let hits = score_keyword_hits(&content, &key, &tokens);
                Ok((Scored::new(parse_node_id(&id)?, hits as f64), key))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;

        // MemoryStore parity: score desc, then canonical key asc, then id asc
        // (the issue-2 tie-break; the key rides along from the row).
        let mut scored = order_candidates(scored);
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

                // (id, dist, session_id, canonical_key): session_id selected so
                // foreign rows can be dropped, canonical_key so equal distances
                // order stably across runs (issue #2).
                let parsed = rows
                    .iter()
                    .map(|r| {
                        let id: String = r.try_get("id").map_err(backend)?;
                        let dist: f64 = r.try_get("dist").map_err(backend)?;
                        let sid: String = r.try_get("session_id").map_err(backend)?;
                        let key: String = r.try_get("canonical_key").map_err(backend)?;
                        Ok((parse_node_id(&id)?, dist, sid, key))
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
                    let hits = order_candidates(
                        fallback_rows
                            .iter()
                            .map(|row| {
                                let id: String = row.try_get("id").map_err(backend)?;
                                let dist: f64 = row.try_get("dist").map_err(backend)?;
                                let key: String = row.try_get("canonical_key").map_err(backend)?;
                                let score = D::distance_to_score(dist);
                                if !score.is_finite() {
                                    return Err(StoreError::Backend(format!(
                                        "non-finite vector distance for concept {id}"
                                    )));
                                }
                                Ok((Scored::new(parse_node_id(&id)?, score), key))
                            })
                            .collect::<Result<Vec<_>, StoreError>>()?,
                    );
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

//! Pool lifecycle and transaction mechanics for the Postgres-wire family: the
//! lazily-built pool, Cloud SQL IAM login with token-expiry pool rotation,
//! per-connection options (`statement_timeout`, the dialect's session
//! settings), the rustls DSN rewrite, and `tx_retry`, which replays a whole
//! transaction body on a retryable error.
//!
//! `tx_retry` owns no transaction itself: each `body` opens, uses and commits
//! its own, so a replay always starts from a fresh transaction.

use std::future::Future;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use super::codec::backend;
use super::{Dialect, PgStore};
use crate::types::StoreError;

/// Pool size is deliberately small: Lambo is single-writer per session (spec §2.4) and
/// the demo runs one process.
pub(super) const MAX_POOL_CONNECTIONS: u32 = 4;

/// Opt-in to Cloud SQL IAM database authentication as the shared service account.
#[cfg(feature = "store-postgres")]
pub(crate) const LAMBO_POSTGRES_IAM_ENV: &str = "LAMBO_POSTGRES_IAM";

/// Whether this process was told to log in with an IAM token instead of a password.
///
/// `pub(crate)` so `resolve`'s hermeticity table can observe the real read site
/// rather than re-reading the variable itself, which would pin nothing.
#[cfg(feature = "store-postgres")]
pub(crate) fn iam_auth_requested() -> bool {
    std::env::var_os(LAMBO_POSTGRES_IAM_ENV).is_some_and(|v| !v.is_empty())
}

/// The IAM opt-in as it stood when the store was constructed.
///
/// Read in `PgStore::new`, which is **synchronous on purpose**, for the same reason
/// [`PgStore::connect_options`] is: a test that pins an env-driven option must be able to
/// do it without holding a lock across an `.await` (spec §6.4, enforced by
/// `clippy::await_holding_lock`). It also means the login mode is decided once, at the
/// same moment the DSN is, rather than re-read on every query.
#[cfg(feature = "store-postgres")]
#[derive(Debug, Clone)]
pub(super) struct IamSetup {
    /// Shared credential file, or `None` when neither variable named one (which is an
    /// error the first pool reports, naming both variables).
    pub(super) credentials: Option<std::path::PathBuf>,
}

/// The shared-service-account login state: the token source, and the pool the current
/// token authorised together with the instant that token stops being handed out.
///
/// Held behind a `tokio::sync::Mutex` because rotating it is an `await` (the token mint),
/// and because two concurrent callers must not mint two tokens and build two pools.
#[cfg(feature = "store-postgres")]
pub(super) struct IamAuth {
    source: crate::gcp_auth::GoogleOAuthTokenSource,
    live: Option<(PgPool, std::time::Instant)>,
}

/// CockroachDB serializable transactions abort with SQLSTATE 40001
/// (`restart transaction: ... RETRY_SERIALIZABLE ...`) when they conflict with a
/// concurrent commit; sqlx does not auto-retry, so the client must replay the whole
/// transaction. Bounded backoff; a genuine (non-conflict) error is returned
/// immediately.
pub(super) const TX_RETRY_ATTEMPTS: usize = 5;

/// STORE-2: server-side per-statement bound (`statement_timeout`), applied to
/// every connection in the pool. `statement_timeout` applies per statement,
/// not per transaction — a multi-statement flush batch can run N x 20s. The
/// whole-batch bound is the client-side flush attempt timeout
/// (`flush.rs` `FLUSH_ATTEMPT_TIMEOUT`); the per-statement bound stays below
/// it so the database aborts a hung statement before the client gives up on
/// the attempt. It also bounds every other statement on the pool — well
/// under the 30s `LOAD_SESSION_TIMEOUT`.
pub(super) const STATEMENT_TIMEOUT: Duration = Duration::from_secs(20);

/// T7.4: accuracy dial for CockroachDB's **approximate** vector search.
///
/// PostgreSQL does not have this GUC; B2 leaves `hnsw.ef_search` at the
/// pgvector default and does not compile this dial into `store-postgres`.
///
/// Once `concepts_embedding_idx` is partial (spec §12.1), `vector_candidates`
/// is served by an ANN index instead of an exact full scan: the search visits
/// a bounded number of index neighbourhoods rather than every row, so a true
/// near neighbour sitting in an unvisited neighbourhood can be missed. In Lambo
/// that surfaces as a *silent* quality loss, not an error — hybrid matching
/// fails to merge a genuine near-duplicate and writes a new concept instead,
/// leaving the graph slightly less connected.
///
/// `vector_search_beam_size` is how many neighbourhoods the search visits:
/// higher is more accurate and slower. CockroachDB's own default is 32 and the
/// server enforces **1..=2048** (verified live 2026-08-13; out-of-range is a
/// server-side error, not a clamp).
///
/// **Default 64, chosen from measurement** (adve-review MAJOR-1, 2026-08-13).
/// Recall was measured against exact top-k on the live cluster, where exact
/// ground truth is forced with the `concepts@concepts_pkey` hint (a FULL SCAN).
/// Two 3,000-row datasets: uniform-random vectors, and clustered unit-norm
/// vectors matching the geometry real embeddings actually have.
///
/// ```text
/// beam:      1     2     4     8    16    32*    64    128    256
/// recall@10 .19   .23   .32   .47   .70   .93    .96   .96    .86
/// recall@50 .07   .13   .22   .40   .64   .94    .99   .99    .97
///                                        *server default
/// ```
///
/// Two findings drove the value:
/// * At the server default (32) roughly **6-7% of true nearest neighbours are
///   missed**. For Lambo that is not a latency question — a missed neighbour is
///   a near-duplicate that hybrid matching fails to merge, so the concept is
///   silently re-created and the graph ends up less connected.
/// * **Higher is not monotonically better.** Beam 256 scored *worse* than 64 in
///   BOTH datasets, reproducibly (recall@10 .86 vs .96). So "crank it up" is
///   wrong advice, and 64 — not the maximum — is the measured knee.
///
/// Recall never reached 1.000 at any beam: this index is approximate by
/// construction and no setting makes it exact. Exactness is available only by
/// giving up index use, which spec §12.1 requires us to demonstrate.
///
/// Override per process (still `1..=2048`, the server's own bound, verified
/// live — out-of-range is a server-side error, not a clamp):
///
/// ```text
/// LAMBO_VECTOR_BEAM_SIZE=32 lambo serve …   # back to the server default
/// ```
///
/// Applied per connection alongside `statement_timeout`, so it costs no
/// per-query round trip. An invalid value is a hard error at pool construction
/// (Level B fails closed — a silently ignored tuning knob is worse than none,
/// because the operator believes accuracy was raised when it was not).
///
/// Caveat kept deliberately: both datasets are synthetic. The clustered set
/// mimics embedding geometry but is not BGE-M3 output, so treat 64 as an
/// evidence-based default rather than a tuned optimum.
#[cfg(feature = "store-cockroach")]
pub(super) const DEFAULT_VECTOR_BEAM_SIZE: u32 = 64;

#[cfg(feature = "store-cockroach")]
pub(super) const VECTOR_BEAM_SIZE_ENV: &str = "LAMBO_VECTOR_BEAM_SIZE";

#[cfg(feature = "store-cockroach")]
pub(super) const VECTOR_BEAM_SIZE_MIN: u32 = 1;

#[cfg(feature = "store-cockroach")]
pub(super) const VECTOR_BEAM_SIZE_MAX: u32 = 2048;

/// Parse `LAMBO_VECTOR_BEAM_SIZE`. `Ok(None)` = unset, inherit the server
/// default. Empty is treated as unset so an exported-but-blank var behaves like
/// absence (same convention as `LAMBO_STORE`).
#[cfg(feature = "store-cockroach")]
pub(super) fn vector_beam_size_from_env() -> Result<Option<u32>, StoreError> {
    let raw = match std::env::var(VECTOR_BEAM_SIZE_ENV) {
        Ok(v) => v,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(e) => return Err(backend(format!("{VECTOR_BEAM_SIZE_ENV}: {e}"))),
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let n: u32 = raw.parse().map_err(|_| {
        backend(format!(
            "{VECTOR_BEAM_SIZE_ENV} must be an integer in \
             {VECTOR_BEAM_SIZE_MIN}..={VECTOR_BEAM_SIZE_MAX}, got {raw:?}"
        ))
    })?;
    if !(VECTOR_BEAM_SIZE_MIN..=VECTOR_BEAM_SIZE_MAX).contains(&n) {
        return Err(backend(format!(
            "{VECTOR_BEAM_SIZE_ENV} must be in \
             {VECTOR_BEAM_SIZE_MIN}..={VECTOR_BEAM_SIZE_MAX}, got {n}"
        )));
    }
    Ok(Some(n))
}

/// STORE-4: structured retry decision for `tx_retry` — no message-text
/// matching. Constraint violations (SQLSTATE 23xxx) are deterministic and are
/// mapped to [`StoreError::Constraint`] by the write path: never replay them.
/// Typed variants are permanent. A [`StoreError::Backend`] may be a transient
/// (serialization conflict, connection exception, server shutdown) that
/// replaying the transaction can fix; the replay is bounded by
/// [`TX_RETRY_ATTEMPTS`] with backoff.
pub(super) fn tx_retryable(e: &StoreError) -> bool {
    match e {
        StoreError::Constraint(_) => false,
        StoreError::Backend(_) => true,
        _ => false,
    }
}

/// Run `body` inside a transaction, replaying the whole body on a fresh transaction when
/// Cockroach aborts it with a serializable-conflict retry (SQLSTATE 40001). The `body`
/// closure opens its own transaction (via a captured pool handle), performs the writes,
/// and commits; a dropped transaction rolls back automatically. Returning a retryable
/// error from any statement aborts the attempt; the wrapper sleeps with bounded backoff
/// and replays the whole body. A non-retryable error is returned immediately.
pub(super) async fn tx_retry<T, F, Fut>(mut body: F) -> Result<T, StoreError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, StoreError>>,
{
    let mut last_err: Option<StoreError> = None;
    for attempt in 0..TX_RETRY_ATTEMPTS {
        match body().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if tx_retryable(&err) && attempt + 1 < TX_RETRY_ATTEMPTS {
                    last_err = Some(err);
                    tokio::time::sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        // B2/B3: the message names Cockroach. PostgreSQL aborts with the same
        // SQLSTATE 40001 under `SERIALIZABLE`, so the mechanism is shared and
        // only the wording is wrong for a second dialect.
        StoreError::Backend("transaction retry exhausted (Cockroach serializable conflict)".into())
    }))
}

/// T0.3 spike: make a libpq DSN usable with sqlx's rustls stack. libpq's magic
/// `sslrootcert=system` is not a real path; point at an actual CA bundle or drop to
/// `require`. Returns the DSN unchanged when no rewrite is needed.
pub(super) fn dsn_for_rustls(dsn: &str) -> String {
    let ca_candidates = [
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/cert.pem",
        "/etc/ssl/ca-bundle.pem",
    ];
    let ca = ca_candidates
        .iter()
        .find(|p| std::path::Path::new(p).is_file())
        .copied();

    let mut out = dsn.to_string();
    if out.contains("sslrootcert=system") {
        if let Some(path) = ca {
            out = out.replace("sslrootcert=system", &format!("sslrootcert={path}"));
        } else {
            out = out.replace("sslrootcert=system", "");
            out = out.replace("&&", "&");
            if out.contains("sslmode=verify-full") {
                out = out.replace("sslmode=verify-full", "sslmode=require");
            }
        }
    }
    out = out.replace("?&", "?").trim_end_matches('&').to_string();
    if out.ends_with('?') {
        out.pop();
    }
    out
}

impl<D: Dialect> PgStore<D> {
    /// The lazily-created pool (Tokio context required: call from an async method).
    ///
    /// Returns an owned handle rather than a borrow because the IAM path **replaces** the
    /// pool when its token expires (see [`Self::iam_pool`]); a `&PgPool` into a slot that
    /// can be swapped is not a reference the borrow checker can hand out. `PgPool` is an
    /// `Arc` internally, so the clone costs a refcount and every call site keeps using it
    /// as `&PgPool`.
    pub(crate) async fn pool(&self) -> Result<PgPool, StoreError> {
        #[cfg(feature = "store-postgres")]
        if self.iam_setup.is_some() {
            return self.iam_pool().await;
        }
        let pool = self
            .pool
            .get_or_try_init(|| async {
                let options = Self::connect_options(&self.dsn)?;
                Ok::<_, StoreError>(
                    PgPoolOptions::new()
                        .max_connections(MAX_POOL_CONNECTIONS)
                        .connect_lazy_with(options),
                )
            })
            .await?;
        Ok(pool.clone())
    }

    /// The shared-service-account pool: Cloud SQL IAM database authentication, where the
    /// "password" is an OAuth access token that **expires in about an hour**.
    ///
    /// This is why the pool is rotated rather than created once. Postgres checks the
    /// password at connect time only, so a pool built with an expired token keeps working
    /// on its open connections and fails on the next one it has to open: a lease refresh
    /// two hours into a `serve` fails with an authentication error that looks nothing like
    /// an expiry. Instead the token source reports the instant it stops handing the token
    /// out, and this method builds a new lazy pool at that instant.
    ///
    /// The superseded pool is dropped, not closed: an in-flight query holds its connection
    /// (and through it the inner pool) until it finishes, while nothing new is ever handed
    /// out from it. `connect_lazy_with` means the replacement opens no connection until
    /// someone queries it, so a rotation costs one token mint and nothing else.
    #[cfg(feature = "store-postgres")]
    async fn iam_pool(&self) -> Result<PgPool, StoreError> {
        let mut guard = self.iam.lock().await;
        if guard.is_none() {
            let path = self
                .iam_setup
                .as_ref()
                .and_then(|s| s.credentials.clone())
                .ok_or_else(|| {
                    backend(format!(
                        "{} IAM auth setup: {LAMBO_POSTGRES_IAM_ENV} is set but \
                         GCP_LAMBO_CREDENTIALS / GOOGLE_APPLICATION_CREDENTIALS is unset",
                        D::STORE_TYPE_NAME
                    ))
                })?;
            let creds = crate::gcp_auth::load_credentials(&path)
                .map_err(|e| backend(format!("{} IAM auth setup: {e}", D::STORE_TYPE_NAME)))?;
            let client = crate::gcp_auth::build_client()
                .map_err(|e| backend(format!("{} IAM auth setup: {e}", D::STORE_TYPE_NAME)))?;
            let source = crate::gcp_auth::GoogleOAuthTokenSource::for_cloud_sql(creds, client)
                .map_err(|e| backend(format!("{} IAM auth setup: {e}", D::STORE_TYPE_NAME)))?;
            *guard = Some(IamAuth { source, live: None });
        }
        let state = guard.as_mut().expect("initialised directly above");
        if let Some((pool, expires_at)) = &state.live {
            if std::time::Instant::now() < *expires_at {
                return Ok(pool.clone());
            }
        }
        let (token, expires_at) = state
            .source
            .access_token_with_expiry()
            .await
            .map_err(|e| backend(format!("{} IAM token: {e}", D::STORE_TYPE_NAME)))?;
        let options = Self::connect_options(&self.dsn)?.password(&token);
        let pool = PgPoolOptions::new()
            .max_connections(MAX_POOL_CONNECTIONS)
            .connect_lazy_with(options);
        state.live = Some((pool.clone(), expires_at));
        Ok(pool)
    }

    /// Build the per-connection options. **Synchronous on purpose:** it reads
    /// the environment, and a test that wants to pin an env-driven option must
    /// be able to do so without holding a lock across an `.await` (spec §6.4,
    /// enforced by `clippy::await_holding_lock`).
    pub(super) fn connect_options(
        dsn: &str,
    ) -> Result<sqlx::postgres::PgConnectOptions, StoreError> {
        let options = dsn
            .parse::<sqlx::postgres::PgConnectOptions>()
            .map_err(|e| backend(format!("invalid {}: {e}", D::DSN_LABEL)))?
            // STORE-2: bound every statement server-side.
            // statement_timeout applies per statement, not per
            // transaction: a multi-statement flush batch can take
            // N x 20s. The whole-batch bound is the client-side
            // flush attempt timeout (FLUSH_ATTEMPT_TIMEOUT); the
            // per-statement bound stays below it so the DB aborts a
            // hung statement before the client gives up on the
            // attempt (a hung statement must never wedge the flush
            // loop).
            .options([(
                "statement_timeout",
                format!("{}s", STATEMENT_TIMEOUT.as_secs()),
            )]);
        // Dialect-specific session settings (Cockroach: vector_search_beam_size;
        // Postgres: none, pgvector hnsw.ef_search stays at its default).
        D::apply_connect_options(options)
    }
}

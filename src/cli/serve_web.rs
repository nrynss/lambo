//! `lambo serve-web` — the T8.5 demo window: a read-only page onto one session.
//!
//! # What it is
//!
//! A single axum server that renders three things and nothing else: the T5.3
//! **recall context block verbatim**, the T6.4 **canonization event feed**, and
//! durable session counts. It is a window onto the product's real output, not a
//! product — no framework, no build step, no client state beyond a poll cursor.
//!
//! # Read-only, by construction
//!
//! **The HTTP surface is unauthenticated (T8.7 is still pending).** Two
//! consequences this module is built around:
//!
//! 1. **This app must stay read-only.** Every route is registered with
//!    `routing::get` and every handler reads. There is deliberately no
//!    `derive` / `record_action` / `reserve` path reachable from the browser:
//!    an unauthenticated write surface is a stranger with a pen in your
//!    session's memory. `read_only_router_has_no_mutating_route` and
//!    `the_module_registers_only_get_routes` fail the build's test gate if a
//!    later edit adds one.
//! 2. **Public exposure requires T8.7 first.** Even read access leaks the whole
//!    session to anyone who can reach the port. `--bind` defaults to loopback
//!    for that reason, and a non-loopback bind prints a warning on stderr and
//!    raises a banner on the page. Until T8.7 lands, "hosted" means *behind a
//!    private network or an authenticating proxy* — not a public URL.
//!
//! # Reader, not writer (spec §2.2)
//!
//! This process is a **reader**: it never constructs a [`Memory`], never takes
//! the T8.6 writer lease, and never spawns GC — same discipline as
//! [`crate::cli::recall`] and [`crate::cli::stats`]. Recall reuses
//! `cli::recall::run` outright, so the page cannot drift from what the CLI and
//! MCP surfaces return.
//!
//! The honest cost of least privilege, stated on the page rather than papered
//! over:
//!
//! * **The live feed is a store poll, not the daemon broadcast.**
//!   [`Memory::events`] is an in-process `broadcast` owned by the writer, and a
//!   separate reader process cannot subscribe to it. The feed instead tails
//!   `GraphSnapshot::canonization_events`, which is the same audit trail the
//!   writer durably records — one hop behind the broadcast (bounded by the
//!   writer's flush interval) and a *superset* across writer restarts. Taking
//!   the broadcast would mean becoming the writer, which would mean holding the
//!   lease, which would mean this page could not run beside a live `lambo serve`.
//! * **`flush_lag` / `log_depth` are reported as `n/a`.** They live in the
//!   writer's flush task; a reader that printed `0` would be claiming a
//!   durability bound it cannot see. Same call [`crate::cli::stats`] makes.
//! * **Graph `epoch` is not surfaced at all.** `Graph::from_snapshot` starts a
//!   loaded graph at epoch 0, so a reader's epoch is always 0 — a number that
//!   looks live and is not.
//!
//! What *is* live: node / edge / concept / canonical counts, the canonization
//! feed, and `durable_change_age_ms` (how long since this reader last observed
//! the durable snapshot change) — all of which move during a demo scenario.
//!
//! # Deployment (P9 target: AWS)
//!
//! * **Self-contained binary.** `web/index.html`, `web/app.css` and `web/app.js`
//!   are `include_str!`-embedded. No CDN, no webfont, no asset directory to
//!   ship; the page renders on a host with zero egress.
//! * **Polling, not SSE.** The page polls `/api/pulse` every 1.5 s. Beyond
//!   there being no `Stream` implementation in the dependency set to hand
//!   `axum::response::Sse`, a short poll survives ALB/CloudFront idle timeouts
//!   and connection recycling, which a long-lived SSE channel does not.
//! * **`GET /healthz`** answers ALB / ECS health checks without touching the
//!   store, so a slow database degrades the page instead of failing the target.
//! * **No secrets in the page.** `/api/session` reports store and embedder
//!   *kind* only — never the DSN, the SQLite path, or the embedder URL.
//!   `session_info_never_leaks_the_dsn_path_or_embedder_url` pins that.
//!
//! [`Memory`]: crate::memory::Memory
//! [`Memory::events`]: crate::memory::Memory::events

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::caps::{check_size_cli, require_nonempty, CliError};
use super::load_reader_graph;
use crate::graph::Graph;
use crate::resolve::ResolvedBackends;
use crate::store::{Capabilities, GraphStore, StoreKind};
use crate::types::{CanonizationStatus, GraphSnapshot, NodeId, SessionId, StoreError};

// ---------------------------------------------------------------------------
// Embedded assets — the whole client, compiled into the binary (P9/AWS).
// ---------------------------------------------------------------------------

const INDEX_HTML: &str = include_str!("../../web/index.html");
const APP_CSS: &str = include_str!("../../web/app.css");
const APP_JS: &str = include_str!("../../web/app.js");

/// How often the page re-reads `/api/pulse`. Served to the client so the
/// interval has exactly one definition.
const POLL_INTERVAL: Duration = Duration::from_millis(1_500);

/// Cap on how long a graceful shutdown may take before the process stops
/// waiting for in-flight connections.
///
/// A reader holds no writer lease and no un-flushed tail, so an abandoned
/// shutdown loses **nothing** — the bound exists purely so Ctrl-C always exits
/// rather than blocking behind a client that will not let go.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

/// `lambo serve-web` arguments, mirroring `lambo serve`'s bind/port conventions.
#[derive(Debug, Clone)]
pub struct Args {
    /// Session to open a window onto. Read as a reader; never written.
    pub session: String,
    /// TCP port to listen on.
    pub port: u16,
    /// Bind address. Loopback by default — the server is unauthenticated.
    pub bind: IpAddr,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// When this reader last saw the durable snapshot *change*.
struct Freshness {
    fingerprint: u64,
    observed_at: Instant,
}

struct AppState {
    session: SessionId,
    backends: ResolvedBackends,
    /// True when `--bind` reaches beyond loopback on an unauthenticated server.
    exposed: bool,
    freshness: Mutex<Freshness>,
}

impl AppState {
    fn store(&self) -> &dyn GraphStore {
        self.backends.store.as_ref()
    }

    /// Record the durable state's count fingerprint; return how long the
    /// current one has been standing.
    ///
    /// Counts only: two different graphs with identical counts read as
    /// "unchanged". That is the right trade for a freshness indicator — it is a
    /// hint about writer activity, not a consistency claim.
    fn observe(&self, fingerprint: u64) -> Duration {
        let mut f = self.freshness.lock();
        if f.fingerprint != fingerprint {
            f.fingerprint = fingerprint;
            f.observed_at = Instant::now();
        }
        f.observed_at.elapsed()
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Session identity and backend *kinds*.
///
/// Deliberately kind-only: `StoreConfig::dsn`, `StoreConfig::path` and
/// `EmbedderConfig::llama_url` are credentials or internal topology and never
/// appear here. `StoreConfig`'s own `Debug` redacts the DSN for the same reason.
#[derive(Debug, Serialize)]
struct SessionInfo {
    session: String,
    store: String,
    embedder: String,
    embedding_dim: usize,
    vector_search: bool,
    /// Always `"reader"` — this process holds no writer lease.
    mode: &'static str,
    /// Always `true`. The router registers `GET` routes only.
    read_only: bool,
    /// The in-RAM store is per-process: a reader cannot see another process's
    /// writes through it. Surfaced so the page can say so instead of looking broken.
    store_is_process_local: bool,
    /// `--bind` reaches beyond loopback while the server is unauthenticated (T8.7).
    exposed_beyond_loopback: bool,
    poll_interval_ms: u64,
    version: &'static str,
}

/// One canonization transition, as the writer durably recorded it.
#[derive(Debug, Serialize)]
struct WebEvent {
    /// Position in the session's ordered event list — the poll cursor.
    seq: usize,
    occurred_at: String,
    node_id: String,
    /// `None` when the concept is no longer in the snapshot (GC'd since).
    content: Option<String>,
    from_status: &'static str,
    to_status: &'static str,
    blast_radius: Option<i32>,
}

#[derive(Debug, Serialize)]
struct EventsPayload {
    /// Every transition recorded for the session.
    total: usize,
    /// The cursor this response answered.
    since: usize,
    events: Vec<WebEvent>,
}

#[derive(Debug, Serialize)]
struct WebStats {
    session: String,
    nodes: usize,
    edges: usize,
    concepts: usize,
    canonical: usize,
    canonization_events: usize,
    /// Always `null` — see the module docs. A reader cannot observe the
    /// writer's flush task, and `0` would be a lie shaped like a measurement.
    flush_lag_ms: Option<u64>,
    /// Always `null`, same reason.
    log_depth: Option<usize>,
    /// How long the durable counts above have been unchanged, as seen here.
    durable_change_age_ms: u64,
    mode: &'static str,
    writer_only: &'static str,
}

#[derive(Debug, Serialize)]
struct Pulse {
    stats: WebStats,
    events: EventsPayload,
}

#[derive(Debug, Serialize)]
struct RecallResponse {
    session: String,
    query: String,
    /// The T5.3 context block **verbatim** — canonical markers, `⚑` warnings
    /// and conflict lines exactly as an agent would receive them.
    context: String,
    elapsed_ms: u64,
}

#[derive(Debug, Deserialize)]
struct RecallParams {
    q: Option<String>,
    top_k: Option<usize>,
    max_tokens: Option<usize>,
    traversal_depth: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SinceParams {
    since: Option<usize>,
}

const WRITER_ONLY: &str = "flush_lag / log_depth / daemon_cycles live in the writer process; \
                           this is a lease-free reader and cannot observe them";

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Raw snapshot for the event tail. A missing session is a first use — an
/// empty session, not an error (same rule as `store::load::load_session_async`).
async fn load_snapshot(
    store: &dyn GraphStore,
    session: &SessionId,
) -> Result<GraphSnapshot, CliError> {
    match store.load_session(session).await {
        Ok(snap) => Ok(snap),
        Err(StoreError::SessionNotFound(_)) => Ok(GraphSnapshot {
            session_id: session.clone(),
            ..GraphSnapshot::default()
        }),
        Err(e) => Err(CliError::Runtime(e.to_string())),
    }
}

fn status_str(s: CanonizationStatus) -> &'static str {
    match s {
        CanonizationStatus::None => "None",
        CanonizationStatus::Candidate => "Candidate",
        CanonizationStatus::Venerable => "Venerable",
        CanonizationStatus::Canonical => "Canonical",
    }
}

/// Canonization events at or after `since`, in a total order that does not
/// depend on which adapter produced them.
fn events_from(snap: &GraphSnapshot, since: usize) -> EventsPayload {
    let content: HashMap<NodeId, &str> = snap
        .concepts
        .iter()
        .map(|c| (c.id, c.content.as_str()))
        .collect();

    let mut ordered: Vec<&crate::types::CanonizationEvent> =
        snap.canonization_events.iter().collect();
    // SQLite orders by (occurred_at, id) on load and MemoryStore by insertion;
    // sorting here makes `seq` mean the same thing on every backend, which is
    // what lets the page use it as a cursor.
    ordered.sort_by(|a, b| a.occurred_at.cmp(&b.occurred_at).then(a.id.0.cmp(&b.id.0)));

    let total = ordered.len();
    let start = since.min(total);
    let events = ordered[start..]
        .iter()
        .enumerate()
        .map(|(offset, ev)| WebEvent {
            seq: start + offset,
            occurred_at: ev.occurred_at.to_rfc3339(),
            node_id: ev.node_id.0.to_string(),
            content: content.get(&ev.node_id).map(|s| (*s).to_string()),
            from_status: status_str(ev.from_status),
            to_status: status_str(ev.to_status),
            blast_radius: ev.blast_radius,
        })
        .collect();

    EventsPayload {
        total,
        since: start,
        events,
    }
}

fn stats_from(state: &AppState, g: &Graph, event_total: usize) -> WebStats {
    let concepts = g.concepts().count();
    let canonical = g
        .concepts()
        .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
        .count();
    let nodes = g.node_count();
    let edges = g.edge_count();

    let mut fingerprint = 0u64;
    for part in [nodes, edges, concepts, canonical, event_total] {
        // FNV-1a over the counts: cheap, stable, and only ever compared to
        // itself (never persisted, never a key).
        fingerprint = (fingerprint ^ part as u64).wrapping_mul(0x100_0000_01b3);
    }

    WebStats {
        session: state.session.as_str().to_string(),
        nodes,
        edges,
        concepts,
        canonical,
        canonization_events: event_total,
        flush_lag_ms: None,
        log_depth: None,
        durable_change_age_ms: state.observe(fingerprint).as_millis() as u64,
        mode: "reader",
        writer_only: WRITER_ONLY,
    }
}

async fn read_events(state: &AppState, since: usize) -> Result<EventsPayload, CliError> {
    let snap = load_snapshot(state.store(), &state.session).await?;
    Ok(events_from(&snap, since))
}

async fn read_stats(state: &AppState, event_total: usize) -> Result<WebStats, CliError> {
    let loaded = load_reader_graph(state.store(), state.session.as_str()).await?;
    // Scoped so the (`!Send`) read guard provably never spans an await.
    let stats = {
        let g = loaded.graph.read();
        stats_from(state, &g, event_total)
    };
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Handlers — all GET, all read.
// ---------------------------------------------------------------------------

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// JSON with `no-store`: session memory must never be served from a cache.
fn json<T: Serialize>(status: StatusCode, body: T) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn fail(err: CliError) -> Response {
    let status = match &err {
        // A bad query string is the caller's fault; a store that will not
        // answer is upstream's.
        CliError::Usage(_) => StatusCode::BAD_REQUEST,
        CliError::Runtime(_) => StatusCode::BAD_GATEWAY,
    };
    json(status, serde_json::json!({ "error": err.to_string() }))
}

async fn index() -> Response {
    asset("text/html; charset=utf-8", INDEX_HTML)
}

async fn stylesheet() -> Response {
    asset("text/css; charset=utf-8", APP_CSS)
}

async fn script() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS)
}

/// Liveness for an ALB / ECS target group. Deliberately does **not** touch the
/// store: a slow database should degrade the page, not fail the health check
/// and take the task out of rotation.
async fn healthz() -> Response {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "ok").into_response()
}

async fn api_session(State(state): State<Arc<AppState>>) -> Response {
    json(
        StatusCode::OK,
        SessionInfo {
            session: state.session.as_str().to_string(),
            store: state.backends.store_cfg.kind.to_string(),
            embedder: state.backends.embedder_cfg.kind.to_string(),
            embedding_dim: state.backends.embedding.dim,
            vector_search: state
                .backends
                .store
                .capabilities()
                .contains(Capabilities::VECTOR_SEARCH),
            mode: "reader",
            read_only: true,
            store_is_process_local: state.backends.store_cfg.kind == StoreKind::Memory,
            exposed_beyond_loopback: state.exposed,
            poll_interval_ms: POLL_INTERVAL.as_millis() as u64,
            version: env!("CARGO_PKG_VERSION"),
        },
    )
}

async fn api_events(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SinceParams>,
) -> Response {
    match read_events(&state, params.since.unwrap_or(0)).await {
        Ok(payload) => json(StatusCode::OK, payload),
        Err(e) => fail(e),
    }
}

async fn api_stats(State(state): State<Arc<AppState>>) -> Response {
    // `usize::MAX` asks for the count without the rows.
    let total = match read_events(&state, usize::MAX).await {
        Ok(p) => p.total,
        Err(e) => return fail(e),
    };
    match read_stats(&state, total).await {
        Ok(stats) => json(StatusCode::OK, stats),
        Err(e) => fail(e),
    }
}

/// Stats + the event tail in one round trip — what the page actually polls.
async fn api_pulse(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SinceParams>,
) -> Response {
    let events = match read_events(&state, params.since.unwrap_or(0)).await {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    match read_stats(&state, events.total).await {
        Ok(stats) => json(StatusCode::OK, Pulse { stats, events }),
        Err(e) => fail(e),
    }
}

/// Recall, straight through [`crate::cli::recall::run`].
///
/// Reusing the CLI reader verbatim is the point: the page cannot show a
/// prettier or staler context block than the one an agent receives, because it
/// is running the same code with the same validators and the same caps.
async fn api_recall(
    State(state): State<Arc<AppState>>,
    Query(params): Query<RecallParams>,
) -> Response {
    let query = params.q.unwrap_or_default();
    let started = Instant::now();
    let result = super::recall::run(
        &state.backends,
        state.session.as_str(),
        query.trim(),
        params.top_k,
        params.max_tokens,
        params.traversal_depth,
    )
    .await;

    match result {
        Ok(context) => json(
            StatusCode::OK,
            RecallResponse {
                session: state.session.as_str().to_string(),
                query,
                context,
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
        ),
        Err(e) => fail(e),
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Every route, `GET`-only.
///
/// Adding a mutating method here is what `read_only_router_has_no_mutating_route`
/// exists to catch: this server is unauthenticated until T8.7, so a write route
/// is a stranger with a pen. A new path must also be added to the tests' `ROUTES`
/// list, which `routes_constant_covers_every_registered_route` enforces.
fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.css", get(stylesheet))
        .route("/app.js", get(script))
        .route("/healthz", get(healthz))
        .route("/api/session", get(api_session))
        .route("/api/recall", get(api_recall))
        .route("/api/events", get(api_events))
        .route("/api/stats", get(api_stats))
        .route("/api/pulse", get(api_pulse))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Process
// ---------------------------------------------------------------------------

/// SIGINT / SIGTERM, registered **eagerly** so a signal arriving during startup
/// is not missed (same discipline as `lambo serve`).
fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let int = signal(SignalKind::interrupt());
        let term = signal(SignalKind::terminate());
        async move {
            match (int, term) {
                (Ok(mut int), Ok(mut term)) => {
                    tokio::select! {
                        _ = int.recv() => {}
                        _ = term.recv() => {}
                    }
                }
                (Ok(mut int), Err(_)) => {
                    let _ = int.recv().await;
                }
                (Err(_), Ok(mut term)) => {
                    let _ = term.recv().await;
                }
                (Err(_), Err(_)) => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        async {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// Serve the read-only session window until SIGINT / SIGTERM.
pub async fn run(backends: ResolvedBackends, args: Args) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;

    let exposed = !args.bind.is_loopback();
    let state = Arc::new(AppState {
        session: SessionId::new(args.session.as_str()),
        backends,
        exposed,
        freshness: Mutex::new(Freshness {
            fingerprint: 0,
            observed_at: Instant::now(),
        }),
    });

    let addr = SocketAddr::new(args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| CliError::Runtime(format!("bind {addr}: {e}")))?;
    let local = listener
        .local_addr()
        .map_err(|e| CliError::Runtime(format!("local_addr: {e}")))?;

    println!(
        "lambo serve-web: read-only window on session '{}' at http://{local}/",
        args.session
    );
    println!("lambo serve-web: reader process — no writer lease, no write routes");
    if exposed {
        eprintln!(
            "⚑ lambo serve-web: bound to {} — the HTTP surface is UNAUTHENTICATED (T8.7 pending). \
             Anyone who can reach this port can read the whole session. Keep it on a private \
             network or behind an authenticating proxy.",
            args.bind
        );
    }
    if state.backends.store_cfg.kind == StoreKind::Memory {
        eprintln!(
            "⚑ lambo serve-web: the 'memory' store is process-local — this reader has its own \
             empty copy and cannot see another process's writes. Use sqlite or cockroach to \
             watch a live session."
        );
    }

    serve_bounded(listener, router(state), shutdown_signal(), SHUTDOWN_GRACE).await
}

/// `axum::serve` under a shutdown signal, with the grace window applied to the
/// **drain only**.
///
/// The bound belongs after the signal, never around the running server:
/// wrapping the whole server in the timeout kills it at the deadline whether or
/// not anyone asked it to stop — which is exactly what the first cut of this
/// function did, and what `the_grace_window_bounds_the_drain_not_the_server`
/// now fails on. `grace` is injectable so that test runs in milliseconds.
async fn serve_bounded(
    listener: tokio::net::TcpListener,
    app: Router,
    shutdown: impl std::future::Future<Output = ()>,
    grace: Duration,
) -> Result<String, CliError> {
    // axum's graceful shutdown takes a future; this oneshot is how the signal
    // arm below reaches it.
    let (graceful_tx, graceful_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = graceful_rx.await;
    });
    // `WithGracefulShutdown` is `IntoFuture`, not `Future`.
    let mut running = std::pin::pin!(async move { server.await });

    tokio::select! {
        // If the server is already done, take that answer over a signal that
        // landed in the same poll.
        biased;
        r = &mut running => {
            return r
                .map(|()| STOPPED.to_string())
                .map_err(|e| CliError::Runtime(format!("serve: {e}")));
        }
        () = shutdown => {}
    }
    let _ = graceful_tx.send(());

    // A reader holds no writer lease and no un-flushed tail, so abandoning the
    // drain loses nothing; the bound only guarantees Ctrl-C actually exits.
    match tokio::time::timeout(grace, &mut running).await {
        Ok(Ok(())) => Ok(STOPPED.to_string()),
        Ok(Err(e)) => Err(CliError::Runtime(format!("serve: {e}"))),
        Err(_) => Ok(format!(
            "{STOPPED} (connections dropped at the grace deadline)"
        )),
    }
}

const STOPPED: &str = "lambo serve-web: stopped";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod tests {
    use super::*;
    use crate::cli::caps::ConceptKind;
    use crate::embed::{EmbedderConfig, EmbedderKind, FixtureEmbedder};
    use crate::store::{StoreConfig, StoreKind};
    use crate::types::{
        CanonizationEvent, CanonizationStatus, EmbeddingContract, MutationBatch, NodeId, Scored,
    };
    use crate::MemoryStore;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use std::net::{Ipv4Addr, SocketAddr};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Every path `router` answers. The read-only method sweep iterates this,
    /// and `routes_constant_covers_every_registered_route` proves it is not
    /// missing one — so a new route cannot be added without being checked.
    const ROUTES: &[&str] = &[
        "/",
        "/app.css",
        "/app.js",
        "/healthz",
        "/api/session",
        "/api/recall",
        "/api/events",
        "/api/stats",
        "/api/pulse",
    ];

    /// `Arc<MemoryStore>` as a `GraphStore`, so the seeding CLI writes and the
    /// web reader share one in-RAM store the way two processes share a file.
    #[derive(Clone)]
    struct Shared(Arc<MemoryStore>);

    #[async_trait]
    impl GraphStore for Shared {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.0.init_schema().await
        }
        fn capabilities(&self) -> Capabilities {
            self.0.capabilities()
        }
        fn vector_dimensions(&self) -> Option<usize> {
            self.0.vector_dimensions()
        }
        async fn flush(&self, batch: &MutationBatch) -> Result<(), StoreError> {
            self.0.flush(batch).await
        }
        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.0.load_session(session).await
        }
        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.0.keyword_candidates(session, tokens, limit).await
        }
        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.0.vector_candidates(session, embedding, limit).await
        }
        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<u64, StoreError> {
            self.0.blast_radius(session, node, min_edge_age, now).await
        }
        async fn interaction_span(
            &self,
            session: &SessionId,
            node: NodeId,
            min_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<crate::types::InteractionSpan, StoreError> {
            self.0.interaction_span(session, node, min_age, now).await
        }
        async fn record_canonization(&self, event: &CanonizationEvent) -> Result<(), StoreError> {
            self.0.record_canonization(event).await
        }
        async fn acquire_lease(
            &self,
            session: &SessionId,
            holder: &crate::store::lease::LeaseHolder,
            ttl: Duration,
        ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
            self.0.acquire_lease(session, holder, ttl).await
        }
        async fn refresh_lease(
            &self,
            session: &SessionId,
            holder: &crate::store::lease::LeaseHolder,
            ttl: Duration,
        ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
            self.0.refresh_lease(session, holder, ttl).await
        }
        async fn release_lease(
            &self,
            session: &SessionId,
            holder: &crate::store::lease::LeaseHolder,
        ) -> Result<(), StoreError> {
            self.0.release_lease(session, holder).await
        }
    }

    fn backends_on(store: Arc<MemoryStore>) -> ResolvedBackends {
        ResolvedBackends {
            store: Box::new(Shared(store)),
            embedder: Box::new(FixtureEmbedder::new()),
            store_cfg: StoreConfig {
                kind: StoreKind::Memory,
                dsn: None,
                path: None,
            },
            embedder_cfg: EmbedderConfig {
                kind: EmbedderKind::Fixture,
                dim: 1024,
                llama_url: None,
                llama_model: None,
            },
            embedding: EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 1024,
            },
        }
    }

    fn state_on(store: Arc<MemoryStore>, session: &str) -> Arc<AppState> {
        Arc::new(AppState {
            session: SessionId::new(session),
            backends: backends_on(store),
            exposed: false,
            freshness: Mutex::new(Freshness {
                fingerprint: 0,
                observed_at: Instant::now(),
            }),
        })
    }

    /// A session with real content: two concepts in a hierarchy, an action, and
    /// an audited promotion of "user schema" all the way to Canonical.
    async fn seed(session: &str) -> Arc<MemoryStore> {
        let store = Arc::new(MemoryStore::new());
        crate::cli::derive::run(
            backends_on(store.clone()),
            crate::cli::derive::Args {
                session: session.into(),
                agent: "agent-a".into(),
                content: "user schema".into(),
                kind: ConceptKind::Entity,
                parent_of: vec!["auth middleware:user schema".into()],
                concept: vec!["auth middleware:entity".into()],
            },
        )
        .await
        .expect("derive");

        crate::cli::record_action::run(
            backends_on(store.clone()),
            crate::cli::record_action::Args {
                session: session.into(),
                agent: "agent-a".into(),
                action: "create user".into(),
                produces: vec!["user schema".into()],
                modifies: vec![],
                depends_on: vec!["auth middleware".into()],
            },
        )
        .await
        .expect("record-action");

        promote(&store, session, "user schema").await;
        store
    }

    /// Walk a concept through the audited transition path, recording each hop
    /// exactly as the canonization task does.
    async fn promote(store: &Arc<MemoryStore>, session: &str, content: &str) {
        let sid = SessionId::new(session);
        let snap = store.load_session(&sid).await.expect("snapshot");
        let node = snap
            .concepts
            .iter()
            .find(|c| c.content == content)
            .map(|c| c.id)
            .unwrap_or_else(|| panic!("{content} must exist"));

        for (from, to) in [
            (CanonizationStatus::None, CanonizationStatus::Candidate),
            (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
            (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
        ] {
            store
                .record_canonization(&CanonizationEvent {
                    id: NodeId::new(),
                    session_id: sid.clone(),
                    node_id: node,
                    from_status: from,
                    to_status: to,
                    blast_radius: Some(1),
                    occurred_at: Utc::now(),
                    last_demotion_time: None,
                })
                .await
                .expect("record canonization");
        }
    }

    // ---- a minimal HTTP/1.1 client -------------------------------------
    //
    // Dependency-free on purpose: `reqwest` is gated behind `embed-bge`, and
    // these tests must run under any feature combination that builds the web
    // module. One request, `Connection: close`, read to EOF.

    struct HttpResponse {
        status: u16,
        headers: String,
        body: String,
    }

    async fn request(addr: SocketAddr, method: &str, path: &str) -> HttpResponse {
        let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
        );
        sock.write_all(req.as_bytes()).await.expect("write");
        sock.flush().await.expect("flush");
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).await.expect("read");
        let raw = String::from_utf8_lossy(&raw).into_owned();

        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let status = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<u16>().ok())
            .unwrap_or_else(|| panic!("no status line in: {head}"));

        // `Connection: close` means no chunked framing from hyper, so the body
        // is the bytes after the header block, verbatim.
        HttpResponse {
            status,
            headers: head.to_string(),
            body: body.to_string(),
        }
    }

    async fn get_json(addr: SocketAddr, path: &str) -> serde_json::Value {
        let r = request(addr, "GET", path).await;
        assert_eq!(r.status, 200, "GET {path} -> {}\n{}", r.status, r.body);
        serde_json::from_str(&r.body)
            .unwrap_or_else(|e| panic!("GET {path} body is not JSON ({e}): {}", r.body))
    }

    /// Bind an ephemeral port and serve `state` until the guard is dropped.
    async fn spawn(state: Arc<AppState>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind ephemeral");
        let addr = listener.local_addr().expect("addr");
        let app = router(state);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, handle)
    }

    // ---- (a) the router serves the page and the JSON endpoints ----------

    #[tokio::test]
    async fn serves_the_page_and_its_embedded_assets() {
        let store = Arc::new(MemoryStore::new());
        let (addr, handle) = spawn(state_on(store, "t85-assets")).await;

        let page = request(addr, "GET", "/").await;
        assert_eq!(page.status, 200);
        assert!(
            page.headers
                .to_lowercase()
                .contains("content-type: text/html"),
            "{}",
            page.headers
        );
        assert!(page.body.contains("<title>Lambo"), "{}", page.body);
        assert!(
            page.body.contains("/app.css") && page.body.contains("/app.js"),
            "the page must reference the embedded assets, not a CDN"
        );

        let css = request(addr, "GET", "/app.css").await;
        assert_eq!(css.status, 200);
        assert!(css.body.contains("--bg:"), "stylesheet body: {}", css.body);

        let js = request(addr, "GET", "/app.js").await;
        assert_eq!(js.status, 200);
        assert!(js.body.contains("/api/pulse"), "script body: {}", js.body);

        let health = request(addr, "GET", "/healthz").await;
        assert_eq!(health.status, 200);
        assert_eq!(health.body, "ok");

        handle.abort();
    }

    /// No asset may reach off-host: a stripped AWS task has no egress, and the
    /// demo URL must not depend on a third party being up.
    #[test]
    fn embedded_assets_reference_no_external_origin() {
        for (name, body) in [
            ("index.html", INDEX_HTML),
            ("app.css", APP_CSS),
            ("app.js", APP_JS),
        ] {
            for needle in ["http://", "https://", "//cdn", "@import url("] {
                assert!(
                    !body.contains(needle),
                    "{name} references an external origin ('{needle}') — assets must be self-contained"
                );
            }
        }
    }

    // ---- (b) endpoints against a seeded session ------------------------

    #[tokio::test]
    async fn recall_endpoint_returns_the_context_block_verbatim() {
        let store = seed("t85-recall").await;
        let state = state_on(store.clone(), "t85-recall");
        let (addr, handle) = spawn(state).await;

        let body = get_json(addr, "/api/recall?q=update%20user%20schema").await;
        let context = body["context"].as_str().expect("context string");

        // Same reader path the CLI runs — the page must not be able to show a
        // different answer than `lambo recall`.
        let expected = crate::cli::recall::run(
            &backends_on(store),
            "t85-recall",
            "update user schema",
            None,
            None,
            None,
        )
        .await
        .expect("cli recall");

        assert!(
            context.contains("user schema"),
            "recall must name the seeded concept: {context}"
        );
        assert!(
            context.contains(", canonical]"),
            "the canonical marker must survive to the page verbatim: {context}"
        );
        assert!(
            context.contains('⚑'),
            "the ⚑ blast-radius warning must survive to the page verbatim: {context}"
        );
        // Same hit set, same order. Scores carry a recency term that ticks
        // between the two calls, so the comparison is on the concept lines
        // rather than the floating-point suffix.
        let hits = |s: &str| -> Vec<String> {
            s.lines()
                .filter(|l| !l.trim().is_empty() && !l.starts_with('⚑') && !l.contains(" ago"))
                .filter_map(|l| l.split(" [").next().map(|c| c.trim().to_string()))
                .filter(|c| !c.is_empty())
                .collect()
        };
        assert_eq!(
            hits(context),
            hits(&expected),
            "the page must render the CLI's context block, not a reformatted one\
             \npage:\n{context}\ncli:\n{expected}"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn recall_endpoint_rejects_a_missing_query_without_touching_the_store() {
        let store = seed("t85-recall-usage").await;
        let (addr, handle) = spawn(state_on(store, "t85-recall-usage")).await;

        let r = request(addr, "GET", "/api/recall").await;
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("error"), "{}", r.body);

        let blank = request(addr, "GET", "/api/recall?q=%20%20").await;
        assert_eq!(blank.status, 400, "{}", blank.body);

        handle.abort();
    }

    #[tokio::test]
    async fn events_endpoint_tails_the_canonization_feed() {
        let store = seed("t85-events").await;
        let (addr, handle) = spawn(state_on(store.clone(), "t85-events")).await;

        let all = get_json(addr, "/api/events").await;
        assert_eq!(all["total"], 3, "three audited hops were seeded: {all}");
        let events = all["events"].as_array().expect("events array");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["from_status"], "None");
        assert_eq!(events[0]["to_status"], "Candidate");
        assert_eq!(events[2]["to_status"], "Canonical");
        assert_eq!(
            events[2]["content"], "user schema",
            "the feed must name the concept, not just its uuid: {all}"
        );
        assert_eq!(events[0]["seq"], 0);
        assert_eq!(events[2]["seq"], 2);

        // The cursor the page polls with: nothing new since the last read.
        let caught_up = get_json(addr, "/api/events?since=3").await;
        assert_eq!(caught_up["total"], 3);
        assert!(caught_up["events"].as_array().expect("array").is_empty());

        // A new transition appends, and only the new one comes back.
        promote(&store, "t85-events", "auth middleware").await;
        let tail = get_json(addr, "/api/events?since=3").await;
        assert_eq!(tail["total"], 6, "{tail}");
        let tail_events = tail["events"].as_array().expect("array");
        assert_eq!(tail_events.len(), 3);
        assert_eq!(tail_events[0]["content"], "auth middleware");
        assert_eq!(tail_events[0]["seq"], 3);

        handle.abort();
    }

    #[tokio::test]
    async fn stats_endpoint_counts_the_session_and_refuses_to_fake_writer_fields() {
        let store = seed("t85-stats").await;
        let (addr, handle) = spawn(state_on(store, "t85-stats")).await;

        let stats = get_json(addr, "/api/stats").await;
        assert!(stats["nodes"].as_u64().expect("nodes") >= 3, "{stats}");
        assert!(stats["edges"].as_u64().expect("edges") >= 1, "{stats}");
        assert_eq!(stats["concepts"], 3, "{stats}"); // user schema, auth middleware, create user
        assert_eq!(stats["canonical"], 1, "{stats}");
        assert_eq!(stats["canonization_events"], 3, "{stats}");
        assert_eq!(stats["mode"], "reader");

        // The load-bearing honesty: a reader must say n/a, never 0.
        assert!(
            stats["flush_lag_ms"].is_null(),
            "a reader cannot observe flush lag; reporting a number would be a lie: {stats}"
        );
        assert!(stats["log_depth"].is_null(), "{stats}");
        assert!(
            stats["writer_only"]
                .as_str()
                .expect("writer_only")
                .contains("reader"),
            "{stats}"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn pulse_returns_stats_and_events_in_one_round_trip() {
        let store = seed("t85-pulse").await;
        let (addr, handle) = spawn(state_on(store, "t85-pulse")).await;

        let pulse = get_json(addr, "/api/pulse?since=0").await;
        assert_eq!(pulse["events"]["total"], 3, "{pulse}");
        assert_eq!(pulse["stats"]["canonical"], 1, "{pulse}");
        assert_eq!(
            pulse["stats"]["canonization_events"], pulse["events"]["total"],
            "stats and the feed must agree within one response: {pulse}"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn an_unwritten_session_is_an_empty_window_not_an_error() {
        let store = Arc::new(MemoryStore::new());
        let (addr, handle) = spawn(state_on(store, "t85-never-written")).await;

        let pulse = get_json(addr, "/api/pulse").await;
        assert_eq!(pulse["stats"]["nodes"], 0, "{pulse}");
        assert_eq!(pulse["events"]["total"], 0, "{pulse}");

        handle.abort();
    }

    // ---- (c) read-only guarantee ---------------------------------------

    #[tokio::test]
    async fn read_only_router_has_no_mutating_route() {
        let store = seed("t85-readonly").await;
        let (addr, handle) = spawn(state_on(store, "t85-readonly")).await;

        for path in ROUTES {
            for method in ["POST", "PUT", "PATCH", "DELETE"] {
                let r = request(addr, method, path).await;
                assert_eq!(
                    r.status, 405,
                    "{method} {path} must be Method Not Allowed — the HTTP surface is \
                     unauthenticated (T8.7 pending) and this app is a read window, so a \
                     mutating route here is a stranger with a pen. Got {} / {}",
                    r.status, r.body
                );
            }
        }

        handle.abort();
    }

    /// Source-level backstop for the behavioural test above: catches a mutating
    /// route registered on a path that was also left out of [`ROUTES`].
    #[test]
    fn the_module_registers_only_get_routes() {
        let src = include_str!("serve_web.rs");
        let prod = src.split("#[cfg(all(test").next().unwrap_or(src);
        for banned in [
            "routing::post",
            "routing::put",
            "routing::patch",
            "routing::delete",
            "post(",
            "put(",
            "patch(",
            "delete(",
        ] {
            assert!(
                !prod.contains(banned),
                "serve_web registers '{banned}' — the demo app must stay read-only until T8.7 \
                 authenticates the HTTP surface"
            );
        }
        // A reader never opens a writer, never takes the lease, never spawns GC.
        for banned in [
            "Memory::builder",
            "open_writer",
            "acquire_lease",
            ".spawn()",
        ] {
            assert!(
                !prod.contains(banned),
                "serve_web contains '{banned}' — serve-web is a lease-free reader (spec §2.2)"
            );
        }
    }

    /// Every route the router answers is listed in [`ROUTES`], so the method
    /// sweep above cannot silently miss one.
    #[test]
    fn routes_constant_covers_every_registered_route() {
        let src = include_str!("serve_web.rs");
        let body = src
            .split("fn router(")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("router body");
        let registered: Vec<String> = body
            .match_indices(".route(\"")
            .filter_map(|(i, _)| {
                let rest = &body[i + ".route(\"".len()..];
                rest.find('"').map(|end| rest[..end].to_string())
            })
            .collect();
        assert!(!registered.is_empty(), "parsed no routes from the router");
        for path in &registered {
            assert!(
                ROUTES.contains(&path.as_str()),
                "route '{path}' is not in ROUTES — the read-only method sweep would skip it"
            );
        }
        assert_eq!(registered.len(), ROUTES.len(), "ROUTES has stale entries");
    }

    // ---- no secrets in the page ----------------------------------------

    #[tokio::test]
    async fn session_info_never_leaks_the_dsn_path_or_embedder_url() {
        let store = Arc::new(MemoryStore::new());
        let mut backends = backends_on(store);
        backends.store_cfg.dsn = Some("postgresql://demo:hunter2@crdb.internal:26257/lambo".into());
        backends.store_cfg.path = Some("/var/lib/lambo/private.sqlite".into());
        backends.embedder_cfg.llama_url = Some("http://embed.internal:8080".into());

        let state = Arc::new(AppState {
            session: SessionId::new("t85-secrets"),
            backends,
            exposed: false,
            freshness: Mutex::new(Freshness {
                fingerprint: 0,
                observed_at: Instant::now(),
            }),
        });
        let (addr, handle) = spawn(state).await;

        let raw = request(addr, "GET", "/api/session").await;
        assert_eq!(raw.status, 200);
        for secret in [
            "hunter2",
            "crdb.internal",
            "postgresql://",
            "/var/lib/lambo",
            "embed.internal",
        ] {
            assert!(
                !raw.body.contains(secret),
                "/api/session leaked '{secret}': {}",
                raw.body
            );
        }

        let info: serde_json::Value = serde_json::from_str(&raw.body).expect("json");
        assert_eq!(info["store"], "memory");
        assert_eq!(info["embedder"], "fixture");
        assert_eq!(info["read_only"], true);
        assert_eq!(info["mode"], "reader");

        handle.abort();
    }

    #[tokio::test]
    async fn api_responses_are_not_cacheable() {
        let store = seed("t85-cache").await;
        let (addr, handle) = spawn(state_on(store, "t85-cache")).await;

        for path in ["/api/session", "/api/stats", "/api/events", "/api/pulse"] {
            let r = request(addr, "GET", path).await;
            assert!(
                r.headers.to_lowercase().contains("cache-control: no-store"),
                "{path} must not be cacheable — it is session memory: {}",
                r.headers
            );
        }

        handle.abort();
    }

    // ---- shutdown -------------------------------------------------------

    /// The grace window bounds the **drain**, not the server.
    ///
    /// Regression: the first cut wrapped the whole `axum::serve` future in
    /// `timeout(SHUTDOWN_GRACE, ..)`, so the process served happily for five
    /// seconds and then exited 0 on its own — which is how it was caught, by
    /// running it. The unit tests could not see it because they call
    /// `axum::serve` directly.
    #[tokio::test]
    async fn the_grace_window_bounds_the_drain_not_the_server() {
        let store = Arc::new(MemoryStore::new());
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let grace = Duration::from_millis(40);

        let server = tokio::spawn(async move {
            serve_bounded(
                listener,
                router(state_on(store, "t85-grace")),
                std::future::pending::<()>(), // no signal, ever
                grace,
            )
            .await
        });

        // Well past the grace window with no shutdown signal: still serving.
        tokio::time::sleep(grace * 6).await;
        assert!(!server.is_finished(), "the server exited without a signal");
        let alive = request(addr, "GET", "/healthz").await;
        assert_eq!(
            alive.status, 200,
            "the grace window must bound the post-signal drain, not the server's lifetime"
        );

        server.abort();
    }

    #[tokio::test]
    async fn a_shutdown_signal_stops_the_server_within_the_grace_window() {
        let store = Arc::new(MemoryStore::new());
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind");

        let out = tokio::time::timeout(
            Duration::from_secs(5),
            serve_bounded(
                listener,
                router(state_on(store, "t85-signal")),
                std::future::ready(()), // signal already pending
                Duration::from_millis(50),
            ),
        )
        .await
        .expect("a signalled server must return, not hang")
        .expect("clean stop");
        assert!(out.starts_with(STOPPED), "{out}");
    }

    // ---- freshness ------------------------------------------------------

    #[test]
    fn durable_change_age_resets_only_when_the_counts_move() {
        let state = state_on(Arc::new(MemoryStore::new()), "t85-freshness");
        let first = state.observe(7);
        std::thread::sleep(Duration::from_millis(12));
        let same = state.observe(7);
        assert!(
            same > first,
            "an unchanged snapshot must keep ageing: {first:?} -> {same:?}"
        );
        let moved = state.observe(8);
        assert!(
            moved < same,
            "a changed snapshot must reset the age: {same:?} -> {moved:?}"
        );
    }
}

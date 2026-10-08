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
//! **Auth, mirroring T8.7's fail-closed rule.** Two consequences this module
//! is built around:
//!
//! 1. **This app must stay read-only.** Every route is registered with
//!    `routing::get` and every handler reads. There is deliberately no
//!    `derive` / `record_action` / `reserve` path reachable from the browser:
//!    a write surface is a stranger with a pen in your session's memory.
//!    `read_only_router_has_no_mutating_route` and
//!    `the_module_registers_only_get_routes` fail the build's test gate if a
//!    later edit adds one.
//! 2. **Loopback is unauthenticated by default; anywhere else fails closed.**
//!    Reading still leaks the whole session to whoever can reach the port, so
//!    `--bind` defaults to loopback and needs **no token** — a judge's browser
//!    just works. A non-loopback bind (LAN or public) is refused at startup
//!    unless a bearer token is configured (`LAMBO_AUTH_TOKEN` env or
//!    `--auth-token`); when a token is set, every request must send
//!    `Authorization: Bearer <token>` (mirrors `crate::mcp::serve`'s
//!    `authorize_bind`). The surface stays read-only either way, and a
//!    token-protected bind should still sit behind a private network or an
//!    authenticating proxy.
//!
//! # Reader, not writer (spec §2.2)
//!
//! This process is a **reader**: it never constructs a [`Memory`], never takes
//! the T8.6 writer lease, and never spawns GC — same discipline as
//! [`crate::cli::recall`] and [`crate::cli::stats`]. Recall reuses
//! `cli::recall::run_detailed` outright (the H3 single-execution seam — CLI
//! string, `hits` and `response_annotations` come from one run), so the page
//! cannot drift from what the CLI and MCP surfaces return.
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
//! * **`flush_lag` / `log_depth` are reported as `n/a` only when no writer has
//!   published them yet.** The writer's `FlushTask` publishes its flush stats
//!   into the shared store after each cycle (T85-3), and this reader fetches
//!   them — so a live writer shows real numbers. When no writer has published
//!   yet (or the store doesn't support it), the page reports `n/a`. A reader
//!   that fabricated `0` would be claiming a durability bound it cannot see
//!   (same call [`crate::cli::stats`] makes); a published value is a real
//!   measurement this reader *can* see.
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
//!
//! # Modules
//!
//! [`run`] is the composition root: it resolves auth, loads the reader graph
//! once to report the embedding contract, builds the one `AppState` and
//! serves `router` until a signal. Everything a request touches is in a
//! child module.
//!
//! | module | holds |
//! |---|---|
//! | this file | the embedded assets, [`Args`], [`run`], the bounded serve, the signal registration |
//! | `auth` | [`AuthToken`], env-over-flag resolution, the non-loopback refusal, the bearer gate |
//! | `state` | `AppState` and the freshness tracker |
//! | `dto` | every response and query type |
//! | `projections` | the reads: hop-1 structural dependents, the ordered event feed, the stats |
//! | `routes` | the handlers and `router`, `GET` only |
//!
//! Request bounds are the shared ones (`crate::surface::limits`, through
//! `crate::cli::caps`); the only portal-local caps are `/api/graph`'s node
//! and edge ceilings, beside that handler.
//!
//! [`Memory`]: crate::memory::Memory
//! [`Memory::events`]: crate::memory::Memory::events

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use parking_lot::Mutex;

use super::caps::{check_size_cli, require_nonempty, CliError};
use super::load_reader_graph;
// `routes::api_recall` names it as `super::recall`, unchanged from when it
// lived here.
use super::recall;
use crate::mcp::AUTH_TOKEN_ENV;
use crate::resolve::ResolvedBackends;
use crate::store::StoreKind;
use crate::types::SessionId;

mod auth;
mod dto;
mod projections;
mod routes;
mod state;

pub use auth::AuthToken;

use auth::{authorize_bind_web, resolve_auth_token};
use dto::EmbeddingStatus;
use routes::router;
use state::{AppState, Freshness};

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
    /// Bind address. Loopback by default — no token required. A non-loopback
    /// bind requires a token (see `authorize_bind_web`).
    pub bind: IpAddr,
    /// Optional bearer token required on every request. Prefer the
    /// [`AUTH_TOKEN_ENV`] env var, which overrides this flag — a token in argv
    /// is visible in `ps` and shell history. Mandatory on any non-loopback bind.
    pub auth_token: Option<AuthToken>,
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

    // Env beats flag (mirrors `mcp::serve`). A set-but-empty LAMBO_AUTH_TOKEN
    // is a usage error, not a silent fallback to the flag.
    let auth = match resolve_auth_token(args.auth_token) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("lambo serve-web: {e}");
            return Err(e);
        }
    };
    // Fail closed: a non-loopback bind with no token is a config error, not a
    // warning (same posture as `mcp::serve::authorize_bind`).
    authorize_bind_web(args.bind, auth.as_ref())?;

    // H1 reader policy: keep the structural portal available, but never let a
    // model mismatch look healthy. `/api/session` carries the stored and live
    // contracts plus a loud message, the page renders it as a banner, and the
    // recall route remains fail-closed through `cli::recall`. Structural
    // stats/graph/inspect deliberately load without an embedder contract.
    let startup = load_reader_graph(backends.store.as_ref(), &args.session).await?;
    let embedding_status = {
        let graph = startup.graph.read();
        EmbeddingStatus::inspect(graph.embedding(), &backends.embedding)
    };
    if embedding_status.status == "mismatch" {
        eprintln!(
            "lambo serve-web: WARNING — vector recall is disabled for this session: {}",
            embedding_status
                .message
                .as_deref()
                .unwrap_or("stored and configured embedding contracts differ")
        );
    }

    let exposed = !args.bind.is_loopback();
    let state = Arc::new(AppState {
        session: SessionId::new(args.session.as_str()),
        backends,
        exposed,
        auth,
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
    // A non-loopback bind always carries a token (`authorize_bind_web`), so
    // the two branches below are exhaustive: token configured, or loopback.
    if state.auth.is_some() {
        eprintln!(
            "⚑ lambo serve-web: authentication is ON — every request must send \
             'Authorization: Bearer <token>' (from {AUTH_TOKEN_ENV} or --auth-token)."
        );
    } else {
        eprintln!(
            "⚑ lambo serve-web: bound to {} — no auth token configured, so the surface is \
             unauthenticated. Anyone who can reach this port can read the whole session; keep \
             it on a private network or behind an authenticating proxy.",
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
mod tests;

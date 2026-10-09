//! `lambo serve-web` — the T8.5 demo window: a read-only page onto a session.
//!
//! # What it is
//!
//! A single axum server that renders three things and nothing else: the T5.3
//! **recall context block verbatim**, the T6.4 **canonization event feed**, and
//! durable session counts. It is a window onto the product's real output, not a
//! product — no framework, no build step, no client state beyond a poll cursor.
//!
//! # Which sessions (#4 PR 2)
//!
//! It serves an explicit allowlist: the ordered union of the repeatable
//! `--session` and `[web] sessions` ([`plan_sessions`]). There is no store
//! discovery. Each served session is read at `/s/{session}/` and
//! `/s/{session}/api/...`; the unscoped `/` and `/api/...` are aliases for the
//! first (the default), so a one-session portal is exactly what it was. With
//! more than one session every name must pass the strict addressed-id charset
//! (`surface::session::parse_addressed`); one session keeps `--session`'s
//! looser rule and is reached through the aliases. A request for a session the
//! portal does not serve, a malformed or percent-encoded id, and an unrouted
//! path all answer the same bytes (`surface::session`'s uniform 404), before
//! any store call.
//!
//! # Host check (#4 PR 2, DNS rebinding)
//!
//! While no bearer token is configured (the loopback default), the portal
//! answers only requests whose `Host` is `localhost`, `127.0.0.1` or `[::1]`
//! (any port), or a name given with `--allowed-host` / `[web] allowed_hosts`;
//! anything else gets one fixed 403. Otherwise a web page the local user
//! visits could rebind its own name to 127.0.0.1 and read every served
//! session. A proxy that forwards a public `Host` (Caddy's default) must
//! name it with `--allowed-host`. With a token configured, any `Host` is
//! accepted: a rebound page cannot present the token.
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
//! [`crate::cli::recall`] and [`crate::cli::stats`]. Recall runs
//! `cli::recall::run_detailed_on` on the session's reader view, the same
//! pipeline `lambo recall` runs through `run_detailed` (the H3
//! single-execution seam — CLI string, `hits` and `response_annotations` come
//! from one run), so the page cannot drift from what the CLI and MCP surfaces
//! return.
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
//! [`run`] is the composition root: it resolves auth, preflights the store
//! schema, builds the one `AppState` (with its view cache, which loads
//! nothing until a request asks) and serves `router` until a signal.
//! Everything a request touches is in a child module.
//!
//! | module | holds |
//! |---|---|
//! | this file | the embedded assets, [`Args`], [`run`], the bounded serve, the signal registration |
//! | `auth` | [`AuthToken`], env-over-flag resolution, the non-loopback refusal, the credential set and the bearer gate |
//! | `scope` | which session a request reads: the per-request resolution, before routing (#4) |
//! | `state` | `AppState`: served sessions, backends, credential set, view cache |
//! | `views` | the per-session reader views: one load per TTL, single-flight, bounded (#4) |
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
use std::time::Duration;

use axum::Router;

use super::caps::{check_size_cli, require_nonempty, CliError};
use crate::surface::session::{parse_addressed, MAX_ADDRESSED_LEN};
// `routes::api_recall` names it as `super::recall`, unchanged from when it
// lived here.
use super::recall;
use crate::config::{AllowedHost, WebConfig};
use crate::mcp::AUTH_TOKEN_ENV;
use crate::resolve::ResolvedBackends;
use crate::store::StoreKind;
use crate::types::SessionId;

mod auth;
mod dto;
mod projections;
mod routes;
mod scope;
mod state;
mod views;

pub use auth::AuthToken;

use auth::{authorize_bind_web, resolve_auth_token};
use routes::router;
use state::AppState;

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
    /// The default session: what the unscoped routes (`/`, `/api/...`)
    /// serve. Read as a reader; never written. Must be one of
    /// [`Args::sessions`] when that is not empty.
    pub session: String,
    /// Every served session, in order, the default first (#4 PR 2): the
    /// allowlist. Empty serves [`Args::session`] alone. The CLI builds both
    /// fields with [`plan_sessions`] from `--session` and `[web] sessions`;
    /// [`run`] re-checks them, so a library caller meets the same rules, and
    /// reads neither `[web] sessions` nor `[web] allowed_hosts` from
    /// [`Args::web`] itself.
    pub sessions: Vec<String>,
    /// TCP port to listen on.
    pub port: u16,
    /// Bind address. Loopback by default — no token required. A non-loopback
    /// bind requires a token (see `authorize_bind_web`).
    pub bind: IpAddr,
    /// Optional bearer token required on every request. Prefer the
    /// [`AUTH_TOKEN_ENV`] env var, which overrides this flag — a token in argv
    /// is visible in `ps` and shell history. Mandatory on any non-loopback bind.
    pub auth_token: Option<AuthToken>,
    /// Extra `Host` values accepted while no token is configured, beside
    /// the loopback names (#4 PR 2): the CLI passes the union of
    /// `--allowed-host` and `[web] allowed_hosts` ([`plan_allowed_hosts`]).
    /// Ignored, with a startup note, once a token is configured.
    pub allowed_hosts: Vec<String>,
    /// `[web]` from `lambo.toml`: the view TTL and the load and recall
    /// bounds (#4). [`WebConfig::default`] when the file has no table.
    pub web: WebConfig,
}

/// The sessions a portal serves, and the default among them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServedSessions {
    /// What the unscoped routes serve: the first `--session`, else the first
    /// `[web] sessions` entry.
    pub default: String,
    /// Every served session, in order: the `--session` values first, then
    /// `[web] sessions`, each once.
    pub sessions: Vec<String>,
}

/// The served sessions from the repeatable `--session` and `[web]
/// sessions`: their ordered union, each once, the first the default.
///
/// Refuses (a usage error, exit 2) an empty union, and every name
/// [`check_served`] refuses. The CLI runs it before any backend is built,
/// so a bad name costs no model load.
pub fn plan_sessions(cli: &[String], web: &WebConfig) -> Result<ServedSessions, CliError> {
    let mut sessions: Vec<String> = Vec::new();
    for name in cli.iter().chain(web.sessions.iter()) {
        if !sessions.contains(name) {
            sessions.push(name.clone());
        }
    }
    let Some(default) = sessions.first().cloned() else {
        return Err(CliError::Usage(
            "--session <SESSION> is required, or name the sessions to serve in lambo.toml \
             [web] sessions"
                .into(),
        ));
    };
    check_served(&default, &sessions)?;
    Ok(ServedSessions { default, sessions })
}

/// The extra accepted `Host` values: `--allowed-host` then `[web]
/// allowed_hosts`, each once. A malformed entry is a usage error (exit 2)
/// naming it; the CLI runs this before any backend is built.
pub fn plan_allowed_hosts(cli: &[String], web: &WebConfig) -> Result<Vec<String>, CliError> {
    let mut hosts: Vec<String> = Vec::new();
    for host in cli.iter().chain(web.allowed_hosts.iter()) {
        parse_allowed_host(host)?;
        if !hosts.contains(host) {
            hosts.push(host.clone());
        }
    }
    Ok(hosts)
}

fn parse_allowed_host(host: &str) -> Result<AllowedHost, CliError> {
    AllowedHost::parse(host).map_err(|e| CliError::Usage(format!("--allowed-host {host:?} {e}")))
}

/// The rules every served set meets (#4 design 3.1, Q12): the default is
/// served, nothing is served twice, every name is non-empty and within the
/// size rule, and with more than one session every name can be addressed
/// by URL (`/s/{session}/`). One session keeps `--session`'s looser rule and
/// is served at the unscoped routes, so no deployed name breaks.
fn check_served(default: &str, sessions: &[String]) -> Result<(), CliError> {
    if !sessions.iter().any(|s| s == default) {
        return Err(CliError::Usage(format!(
            "the default session {default:?} is not one of the served sessions"
        )));
    }
    for (i, name) in sessions.iter().enumerate() {
        require_nonempty("session", name)?;
        check_size_cli("session", name)?;
        if sessions[..i].contains(name) {
            return Err(CliError::Usage(format!("session {name:?} is listed twice")));
        }
    }
    if sessions.len() > 1 {
        for name in sessions {
            if parse_addressed(name).is_err() {
                return Err(CliError::Usage(format!(
                    "session {name:?} cannot be addressed by URL: with more than one session, \
                     each is served at /s/<session>/ and must be 1 to {MAX_ADDRESSED_LEN} \
                     bytes of [A-Za-z0-9._:-], not starting with '.'. A session outside that \
                     rule can still be served on its own with `lambo serve-web --session <name>`"
                )));
            }
        }
    }
    Ok(())
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
    let served = if args.sessions.is_empty() {
        vec![args.session.clone()]
    } else {
        args.sessions.clone()
    };
    check_served(&args.session, &served)?;
    let allowed_hosts = args
        .allowed_hosts
        .iter()
        .map(|h| parse_allowed_host(h))
        .collect::<Result<Vec<_>, _>>()?;

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

    // Fail fast on an unprovisioned or unreachable store, as the startup load
    // used to. Sessions themselves load lazily, on the first request that
    // needs one (#4 Q15), and the H1 mismatch warning moves to that load,
    // naming the session. H1 reader policy is otherwise unchanged: the
    // structural portal stays available, `/api/session` and `/api/pulse`
    // carry the stored and live contracts (the page renders a banner), and
    // recall stays fail-closed on a mismatched session.
    backends
        .store
        .preflight_schema()
        .await
        .map_err(|e| CliError::Runtime(e.to_string()))?;

    let exposed = !args.bind.is_loopback();
    let state = Arc::new(AppState::new(
        SessionId::new(args.session.as_str()),
        served.iter().map(|s| SessionId::new(s.as_str())),
        backends,
        exposed,
        auth,
        &allowed_hosts,
        &args.web,
    ));

    let addr = SocketAddr::new(args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| CliError::Runtime(format!("bind {addr}: {e}")))?;
    let local = listener
        .local_addr()
        .map_err(|e| CliError::Runtime(format!("local_addr: {e}")))?;

    if served.len() == 1 {
        println!(
            "lambo serve-web: read-only window on session '{}' at http://{local}/",
            args.session
        );
    } else {
        println!(
            "lambo serve-web: read-only window on {} sessions at http://{local}/s/<session>/ \
             (the default, '{}', also at http://{local}/)",
            served.len(),
            args.session
        );
    }
    println!("lambo serve-web: reader process — no writer lease, no write routes");
    // The count line (#4 design 4.1): which credential reaches how many
    // sessions. Names a credential, never a token.
    println!(
        "lambo serve-web: credential '{}' reads {} session{}",
        auth::credential_label(&state.authority),
        served.len(),
        if served.len() == 1 { "" } else { "s" }
    );
    let bounds = state.views.bounds();
    println!(
        "lambo serve-web: session views — refreshed after {} ms, at most {} loaded, {} load(s) \
         and {} recall(s) at a time",
        bounds.ttl.as_millis(),
        bounds.max_loaded_sessions,
        bounds.load_concurrency,
        bounds.recall_concurrency,
    );
    // A non-loopback bind always carries a token (`authorize_bind_web`), so
    // the two branches below are exhaustive: token configured, or loopback.
    if state.authority.requires_bearer() {
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
    if state.authority.requires_bearer() && !allowed_hosts.is_empty() {
        eprintln!(
            "⚑ lambo serve-web: --allowed-host / [web] allowed_hosts are not checked while a \
             token is configured: any Host is accepted, since a rebound page cannot present \
             the token."
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

//! The two client-facing transports, stdio and streamable HTTP, and the
//! bounded wind-down both share: race the transport against the shutdown
//! future, cancel on the signal, give it [`SHUTDOWN_GRACE`] to finish, then
//! drop it so the close can run.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::service::ServerInitializeError;
use rmcp::transport::io::stdio;
use rmcp::ServiceExt;

use super::authority::{authorize_default, Authenticated, ServeAuthority};
use super::http_guards::{guard_request, HttpGuard, RateLimiter};
use super::registry::{Lookup, SessionRegistry};
use super::shutdown::{HolderShutdown, SHUTDOWN_GRACE};
use super::ServeOptions;
use crate::mcp::server::LamboServer;
use crate::surface::session::{
    not_found_response, RefusalReason, SessionGrant, SessionNeed, SessionRefusal,
};
use crate::types::LamboError;

/// Run a setup step (the stdio handshake, the HTTP `bind`) but bail the moment
/// the shutdown signal fires first (R2-a).
///
/// Before this, a signal that landed in the pre-handshake window — after the
/// session is attached (a clean run already has `mutations=1` to flush at that
/// point) but before the transport's own signal handling is live — hit the
/// default disposition and killed the process with `close()` un-run. `None`
/// means the signal won: the caller returns so `serve` still reaches close.
pub(super) async fn setup_or_shutdown<T>(
    setup: impl Future<Output = T>,
    shutdown: impl Future<Output = ()>,
) -> Option<T> {
    tokio::select! {
        biased;
        v = setup => Some(v),
        () = shutdown => None,
    }
}

/// Did this failed handshake just mean "the client hung up"?
///
/// [`ServerInitializeError::ConnectionClosed`] is the one variant rmcp raises
/// when the transport stream *ended* while it was waiting for a handshake frame
/// — `expect_next_message` seeing `None`, which is its only construction site
/// in the crate. On stdio that is EOF on stdin: the client went away between
/// launching this process and sending `initialize`. That is a disconnect,
/// indistinguishable in kind from the post-handshake EOF [`serve_stdio`]
/// already logs as `client disconnected` and exits 0 on.
///
/// One honest caveat, because the variant is very slightly broader than "clean
/// EOF": rmcp's `AsyncRwTransport::receive` also returns `None` when the
/// underlying read *fails*, not only when it hits end-of-stream. So a genuine
/// I/O fault on stdin arrives here wearing the same variant, and is treated as
/// a hangup. That is accepted deliberately rather than overlooked — rmcp logs
/// the fault itself at ERROR (`Error reading from stream`) so it is never
/// silent, and a process whose stdin is unreadable cannot serve a stdio client
/// by any route: there is no configuration an operator could fix in response,
/// which is what `LamboError::Config` would have been claiming. The variants
/// that *do* describe a fixable fault stay fatal, below.
///
/// Every other variant stays fatal on purpose, because each one is a live peer
/// saying something wrong rather than a peer leaving: `ExpectedInitializeRequest`
/// is a client that opened with the wrong frame, `InitializeFailed` /
/// `UnexpectedInitializeResponse` are protocol violations, and `TransportError`
/// is an I/O fault worth an operator's attention. Folding those into a clean
/// exit would hide real breakage behind a zero exit status.
pub(super) fn is_pre_handshake_disconnect(e: &ServerInitializeError) -> bool {
    matches!(e, ServerInitializeError::ConnectionClosed(_))
}

/// Outcome of running a transport under a shutdown signal.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Exit<T> {
    /// The transport finished on its own, or wound down within the grace window.
    Finished(T),
    /// The grace window expired with the transport still running; it was
    /// dropped so shutdown could proceed.
    Forced,
}

/// Run `running` to completion, but never past `shutdown` + `grace`.
///
/// The shape both transports need and the fix for R1/T82-1 and T82-2:
///
/// 1. race the transport against the shutdown signal;
/// 2. on the signal, call `cancel` — for stdio that cancels the rmcp service
///    loop, for HTTP it triggers axum's graceful shutdown;
/// 3. give the transport `grace` to actually finish, and if it does not,
///    return [`Exit::Forced`] so the caller can drop it and close the session.
///
/// Step 3 is the part that matters: a graceful shutdown with no deadline is
/// indistinguishable from a hang when a client holds a long-lived stream open,
/// and a hang here means the write-behind tail is never flushed.
pub(super) async fn run_until_shutdown<T>(
    running: impl Future<Output = T>,
    cancel: impl FnOnce(),
    shutdown: impl Future<Output = ()>,
    grace: Duration,
) -> Exit<T> {
    tokio::pin!(running);
    tokio::select! {
        // Bias is deliberate: if the transport is already done, take that
        // answer rather than a signal that arrived in the same poll.
        biased;
        v = &mut running => return Exit::Finished(v),
        () = shutdown => {}
    }
    tracing::info!("lambo serve: shutdown signal received, winding down");
    cancel();
    match tokio::time::timeout(grace, &mut running).await {
        Ok(v) => Exit::Finished(v),
        Err(_) => Exit::Forced,
    }
}

/// stdio transport — the shape an MCP client launches as a subprocess.
///
/// **stdout is the protocol channel.** Nothing but JSON-RPC may be written to
/// it; diagnostics go to stderr (see [`crate::mcp::init_tracing`]).
///
/// Returns on client disconnect (EOF on stdin) **or** on SIGINT/SIGTERM, so
/// `Memory::close` runs either way. Before R1/T82-1 this awaited only the
/// service, and the default signal disposition killed the process outright with
/// the tail still in the log.
pub(super) async fn serve_stdio(
    server: LamboServer,
    mut shutdown: Pin<&mut HolderShutdown>,
) -> Result<(), LamboError> {
    // Race the handshake against the shutdown signal (R2-a). `shutdown.as_mut()`
    // reborrows, so the same registration is still live for the transport race
    // below if the handshake wins.
    let service = match setup_or_shutdown(server.serve(stdio()), shutdown.as_mut()).await {
        Some(Ok(service)) => service,
        // A client that hangs up *before* it finishes `initialize` has
        // disconnected; it has not misconfigured anything. Reporting that as
        // `LamboError::Config` made `serve` exit non-zero on a completely
        // ordinary lifecycle event, and it split the contract across the
        // handshake boundary: an EOF one frame later lands in
        // `Exit::Finished(Ok(reason))` below and exits 0 with an INFO line.
        //
        // This is the defect CI run 33085161710 collected on, and it is worth
        // spelling out why it presents as a *flaky* test rather than a
        // deterministic one. `Child::wait()` closes the child's stdin before it
        // waits, so the pre-handshake durability test's holder gets two
        // shutdown stimuli in a rush: the `SIGTERM` it sends explicitly, and
        // the stdin EOF that `wait()` causes an instant later. Whichever the
        // holder observes first decides the exit status, because
        // `setup_or_shutdown` is `biased` toward the setup future — so on an
        // unloaded box the signal usually wins and the process exits 0 down the
        // `None` arm, while on a loaded runner the holder is descheduled long
        // enough for the EOF to be sitting there ready when it next polls, the
        // biased arm takes it, and the same run exits 1. Nothing about the test
        // was timing-dependent except which of two correct-to-handle events got
        // there first; only one of them was actually handled.
        //
        // So the fix is here rather than in the test: both orderings are a
        // client going away pre-handshake, and both must close the session and
        // exit 0. `ConnectionClosed` is rmcp's "the transport ended while I was
        // waiting for a frame" — a peer hangup and never a config fault — so it
        // is the only variant folded in; a malformed or rejected `initialize`
        // still fails loudly.
        Some(Err(e)) if is_pre_handshake_disconnect(&e) => {
            tracing::info!(
                reason = %e,
                "mcp stdio: client disconnected before the handshake completed — \
                 closing the session without serving"
            );
            return Ok(());
        }
        Some(Err(e)) => return Err(LamboError::Config(format!("mcp stdio: {e}"))),
        None => {
            tracing::info!(
                "mcp stdio: shutdown signal during handshake — closing the session without serving"
            );
            return Ok(());
        }
    };

    // Taken before `waiting()` consumes the service — it is the only handle
    // left once the service is inside the future.
    let cancel_token = service.cancellation_token();
    match run_until_shutdown(
        service.waiting(),
        move || cancel_token.cancel(),
        shutdown,
        SHUTDOWN_GRACE,
    )
    .await
    {
        Exit::Finished(Ok(reason)) => {
            tracing::info!(?reason, "mcp stdio: client disconnected");
            Ok(())
        }
        Exit::Finished(Err(e)) => Err(LamboError::Config(format!("mcp stdio: {e}"))),
        Exit::Forced => {
            tracing::warn!(
                grace_secs = SHUTDOWN_GRACE.as_secs(),
                "mcp stdio: service did not stop within the grace window — \
                 dropping the transport and closing the session anyway"
            );
            Ok(())
        }
    }
}

/// Streamable HTTP transport, served by the axum already in the tree.
///
/// Each attached session has its own `StreamableHttpService`
/// (`session::AttachedSession::http`), which shares the session's one
/// `LamboServer` clone per request — it never builds a second
/// [`Memory`](crate::memory::Memory). The router ([`session_router`]) picks
/// the session from the path; the guards run first, on every route, and the
/// credential set (`authority`, #32 PR 5) decides which sessions a request
/// may reach.
pub(super) async fn serve_http(
    registry: Arc<SessionRegistry>,
    authority: Arc<ServeAuthority>,
    opts: &ServeOptions,
    mut shutdown: Pin<&mut HolderShutdown>,
) -> Result<(), LamboError> {
    // The session cap counts every attached session's MCP sessions: one
    // `--max-sessions` for the process (#32 design §3.6), read from the
    // managers rmcp mutates — see [`LiveSessions`](super::http_guards::LiveSessions).
    let guard = HttpGuard {
        authority: Arc::clone(&authority),
        max_sessions: opts.max_sessions,
        live: registry.clone(),
        rate: RateLimiter::new(opts.rate_limit_rps, Instant::now()).map(Arc::new),
    };
    // T8.7 posture, logged once at startup so an operator can see what this
    // process is actually enforcing. No token is ever logged: only whether
    // one is required and the credentials' names (#32 PR 5).
    tracing::info!(
        auth_required = authority.requires_bearer(),
        credentials = %authority.credential_names().join(", "),
        max_sessions = guard.max_sessions,
        rate_limit_rps = opts.rate_limit_rps,
        "mcp http: request guard armed"
    );

    let hosted = registry.hosted().to_vec();
    let app = http_app(registry, authority, guard);
    let addr = SocketAddr::new(opts.bind, opts.port);
    // Race `bind` against the shutdown signal too (R2-a): the ~5 ms bind window
    // is small but non-zero, and a signal in it must still reach `close()`.
    let listener =
        match setup_or_shutdown(tokio::net::TcpListener::bind(addr), shutdown.as_mut()).await {
            Some(r) => r.map_err(|e| LamboError::Config(format!("mcp http: bind {addr}: {e}")))?,
            None => {
                tracing::info!(
                    "mcp http: shutdown signal before bind — closing the session without serving"
                );
                return Ok(());
            }
        };
    // The bound address, not the requested one: they differ only for
    // `--port 0`, where the kernel picks the port and this line is the only
    // place it is named.
    let addr = listener.local_addr().unwrap_or(addr);
    tracing::info!(%addr, "mcp http: listening on /mcp");
    if hosted.len() > 1 {
        tracing::info!(
            %addr,
            sessions = ?hosted,
            "mcp http: serving each pinned session at /mcp/s/{{session}}"
        );
    }

    serve_http_bounded(listener, app, shutdown, SHUTDOWN_GRACE).await
}

/// The serve's whole HTTP app: [`session_router`] behind the guards, every
/// guard on `.layer` (so an unrouted path passes through them too and a
/// refused session stays indistinguishable from it). `serve_http` serves
/// exactly this; the tests serve it too, so they exercise the same
/// composition.
pub(super) fn http_app(
    registry: Arc<SessionRegistry>,
    authority: Arc<ServeAuthority>,
    guard: HttpGuard,
) -> axum::Router {
    session_router(registry, authority)
        .layer(axum::middleware::from_fn_with_state(guard, guard_request))
}

/// What the session routes need: the sessions, and the credential set that
/// decides which of them a request may reach.
#[derive(Clone)]
struct Routes {
    registry: Arc<SessionRegistry>,
    authority: Arc<ServeAuthority>,
}

/// The MCP routes (#32 PR 4, design §2.1), without the guards:
///
/// | route | serves |
/// |---|---|
/// | `/mcp` | the default session (the first `--session`, else `[serve] default_session`, else the first pinned) |
/// | `/mcp/s/{session}` | that session |
///
/// Both answer every method, so a refused id is the uniform 404 whatever
/// the method (PR 1's wire claim: a refused session and an unrouted path
/// are indistinguishable). Every other path is axum's own 404.
///
/// Each route runs §6.2's steps 2 and 3 (#32 PR 5) **before** the registry
/// is consulted, in memory: the id's shape, then the request's grant (set
/// by the guard as [`Authenticated`]) against it. A refusal there is the
/// uniform 404 whatever state the session is in, so only a caller inside a
/// session's scope can learn that it is held, detaching or failed (503).
pub(super) fn session_router(
    registry: Arc<SessionRegistry>,
    authority: Arc<ServeAuthority>,
) -> axum::Router {
    axum::Router::new()
        .route("/mcp", axum::routing::any(default_session))
        .route("/mcp/s/{session}", axum::routing::any(addressed_session))
        .with_state(Routes {
            registry,
            authority,
        })
}

/// The route prefix an addressed session id follows.
const ADDRESSED_PREFIX: &str = "/mcp/s/";

/// The grant the guard resolved for `req`. A request without one never
/// passed the guard, and is refused.
fn granted(req: &axum::extract::Request) -> Option<Arc<SessionGrant>> {
    req.extensions()
        .get::<Authenticated>()
        .map(|Authenticated(grant)| Arc::clone(grant))
}

/// The uniform 404 for `refusal`, logged for the operator by reason and
/// credential name, never by the probed id.
fn refused(grant: Option<&SessionGrant>, refusal: SessionRefusal) -> axum::response::Response {
    tracing::debug!(
        credential = grant.map_or("(none)", SessionGrant::name),
        reason = %refusal,
        "mcp http: refused a session request"
    );
    refusal.not_found_response()
}

async fn default_session(
    axum::extract::State(routes): axum::extract::State<Routes>,
    req: axum::extract::Request,
) -> axum::response::Response {
    let Some(grant) = granted(&req) else {
        return refused(None, SessionRefusal::new(RefusalReason::OutOfScope));
    };
    let Some(id) = routes.registry.default_session().map(str::to_string) else {
        return not_found_response();
    };
    match authorize_default(&routes.authority, &grant, &id) {
        Ok(()) => serve_session(&routes.registry, &id, &grant, req).await,
        Err(refusal) => refused(Some(&grant), refusal),
    }
}

async fn addressed_session(
    axum::extract::State(routes): axum::extract::State<Routes>,
    req: axum::extract::Request,
) -> axum::response::Response {
    let Some(grant) = granted(&req) else {
        return refused(None, SessionRefusal::new(RefusalReason::OutOfScope));
    };
    // The raw path segment, not axum's percent-decoded `Path`: an addressed
    // id is never percent-decoded (#32 decision 16), so `%2E` is refused by
    // the charset rather than read as a dot.
    let raw = req
        .uri()
        .path()
        .strip_prefix(ADDRESSED_PREFIX)
        .unwrap_or_default()
        .to_string();
    match routes.authority.authorize(&grant, &raw, SessionNeed::Use) {
        Ok(id) => serve_session(&routes.registry, id.as_str(), &grant, req).await,
        Err(refusal) => refused(Some(&grant), refusal),
    }
}

/// Hand `req` to session `id`'s own MCP service, or refuse it. Reached only
/// once `grant` is authorized for `id`, so every answer here is one the
/// caller is entitled to (§6.2: inside scope the answers can differ).
async fn serve_session(
    registry: &SessionRegistry,
    id: &str,
    grant: &SessionGrant,
    req: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match registry.lookup(id) {
        Lookup::Live(session) => session
            .http
            .handle(req)
            .await
            .map(axum::body::Body::new)
            .into_response(),
        Lookup::Unavailable { retry_after } => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            [(
                axum::http::header::RETRY_AFTER,
                retry_after.as_secs().max(1).to_string(),
            )],
            "this session is not available on this server right now: retry later\n",
        )
            .into_response(),
        // Hosted, so not the uniform 404; no `Retry-After`, because a retry
        // gets the same answer until an operator acts (#32 review L1).
        Lookup::Failed => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "this session could not be attached on this server: an operator must act (see the \
             serve log)\n",
        )
            .into_response(),
        // In scope but absent. Pinned sessions only until #32 PR 6, so even
        // a credential with `create` gets the 404: nothing attaches on
        // demand yet.
        Lookup::NotHosted => refused(Some(grant), SessionRefusal::new(RefusalReason::Absent)),
    }
}

/// `axum::serve` with a **bounded** graceful shutdown (R1/T82-2).
///
/// Split out from [`serve_http`] so the bound is testable without a `Memory`:
/// the test holds a never-ending response open, exactly as a streamable-HTTP
/// MCP client's SSE channel does, and asserts this returns anyway.
pub(super) async fn serve_http_bounded(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    shutdown: impl Future<Output = ()>,
    grace: Duration,
) -> Result<(), LamboError> {
    // axum's graceful shutdown takes a future; this oneshot is how the shared
    // `cancel` step reaches it.
    let (graceful_tx, graceful_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = graceful_rx.await;
    });

    // `WithGracefulShutdown` is `IntoFuture`, not `Future` — the async block is
    // what turns it into one.
    match run_until_shutdown(
        async move { server.await },
        move || {
            let _ = graceful_tx.send(());
        },
        shutdown,
        grace,
    )
    .await
    {
        Exit::Finished(r) => r.map_err(|e| LamboError::Config(format!("mcp http: {e}"))),
        Exit::Forced => {
            tracing::warn!(
                grace_secs = grace.as_secs(),
                "mcp http: connections still open after the grace window (an SSE stream \
                 never finishes on its own) — forcing the close so the tail is flushed"
            );
            Ok(())
        }
    }
}

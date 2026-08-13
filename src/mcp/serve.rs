//! `lambo serve` — process lifecycle for the MCP server.
//!
//! One process owns one session (spec §2.2). This module builds **one**
//! [`Memory`] from **one** [`ResolvedBackends`], serves it over stdio or
//! streamable HTTP, and guarantees [`Memory::close`] runs on the way out so the
//! final flush happens.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rmcp::transport::io::stdio;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServiceExt;

use crate::mcp::server::LamboServer;
use crate::memory::Memory;
use crate::resolve::{resolve_from_config_path, ResolvedBackends};
use crate::types::{DaemonEvent, LamboError};

/// How long a transport gets to wind itself down after the shutdown signal
/// before it is dropped and `close()` runs anyway.
///
/// This bound is the whole point (R1/T82-2). `axum::serve(..).with_graceful_shutdown`
/// waits for **every in-flight connection to finish**, and a streamable-HTTP MCP
/// client holds its server→client SSE channel open for the life of the session
/// (kept alive by `sse_keep_alive`, so it never idles out). Without a deadline,
/// graceful shutdown never returns, `Memory::close` never runs, and the tail is
/// lost — the exact durability failure the signal handling exists to prevent.
/// A dropped connection is recoverable; a dropped write-behind tail is not.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Which transport `lambo serve` should listen on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Newline-delimited JSON-RPC over stdin/stdout — what an MCP client
    /// launching `lambo serve` as a subprocess speaks.
    Stdio,
    /// Streamable HTTP (`POST /mcp`), for the T8.5 demo app and remote clients.
    Http,
}

impl std::str::FromStr for Transport {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stdio" => Ok(Self::Stdio),
            "http" => Ok(Self::Http),
            other => Err(format!(
                "unknown transport '{other}' (expected 'stdio' or 'http')"
            )),
        }
    }
}

/// Everything `serve` needs that is not in `lambo.toml`.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    /// Session this process owns.
    pub session: String,
    /// Agent identity this process writes as. See the attribution note on
    /// [`LamboServer`] — `Memory` binds one agent per session handle.
    pub agent: String,
    pub transport: Transport,
    pub port: u16,
    /// Bind address for `--transport http`. Defaults to loopback: the HTTP
    /// transport has no authentication, and this process is a *writer* on the
    /// session.
    pub bind: IpAddr,
}

impl ServeOptions {
    pub fn new(session: impl Into<String>, agent: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            agent: agent.into(),
            transport: Transport::Stdio,
            port: 7700,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        }
    }
}

/// Build the single [`Memory`] this process owns from an **already-resolved**
/// [`ResolvedBackends`].
///
/// **Level B, single construction site.** This function deliberately does *not*
/// resolve: the caller (`main`) resolves once and hands the result in, so there
/// is exactly one store and one embedder per process and no second config pass.
/// Fail-closed behaviour — uncompiled `kind`, unknown TOML key, store×embedder
/// dim mismatch — lives in that one resolve; see [`resolve_serve_backends`].
///
/// [`ResolvedBackends`]: crate::resolve::ResolvedBackends
pub async fn build_memory(
    opts: &ServeOptions,
    backends: ResolvedBackends,
) -> Result<Memory, LamboError> {
    Memory::builder()
        .session(opts.session.clone())
        .agent(opts.agent.clone())
        .backends(backends)
        .build()
        .await
        .map_err(explain_startup_failure)
}

/// Turn a raw driver error at attach time into an actionable message.
///
/// Pointing `serve` at a fresh SQLite file or an unmigrated Cockroach database
/// failed with nothing but `no such table: sessions` (R1/T82-10). Schema
/// bootstrap belongs to `lambo provision` (T8.3) and `serve` deliberately does
/// not auto-init — but the *message* is T8.2's, and "run provision" is the one
/// thing the operator needs to be told.
fn explain_startup_failure(err: LamboError) -> LamboError {
    let text = err.to_string();
    let lower = text.to_lowercase();
    // The shapes the two SQL backends use for "the schema isn't there":
    // SQLite says `no such table`, Postgres/Cockroach say `relation "x" does
    // not exist` (SQLSTATE 42P01) or `undefined_table`.
    let unprovisioned = lower.contains("no such table")
        || lower.contains("does not exist")
        || lower.contains("undefined_table")
        || lower.contains("42p01");
    if unprovisioned {
        LamboError::Config(format!(
            "session store is not provisioned — run 'lambo provision' \
             (or scripts/provision.sh) against this store first, then retry. \
             Underlying error: {text}"
        ))
    } else {
        err
    }
}

/// The one resolve a serve process performs (Level B).
///
/// Thin by design — it exists so the single construction site is named and
/// greppable, not to add behaviour. Config precedence (`--config`, then
/// `LAMBO_CONFIG`, then `./lambo.toml`, then defaults; env overrides file) and
/// every fail-closed check live inside `resolve_from_config_path`.
pub fn resolve_serve_backends(config: Option<&Path>) -> Result<ResolvedBackends, LamboError> {
    resolve_from_config_path(config).map_err(|e| LamboError::Config(e.to_string()))
}

/// Run the MCP server to completion, then close the session.
///
/// [`Memory::close`] runs on **every** exit path — clean client disconnect,
/// SIGINT/SIGTERM, or a transport error — because the tail is only durable once
/// it has run. Its error is surfaced: an `Err` from `close()` means the tail is
/// *not* durable and was kept (T8.1 semantics).
///
/// Both transports route their shutdown through [`run_until_shutdown`], so the
/// signal path is the same on each: cancel, wait up to [`SHUTDOWN_GRACE`], then
/// drop the transport and close regardless. A client that will not let go
/// cannot hold the tail hostage.
pub async fn serve(opts: ServeOptions, backends: ResolvedBackends) -> Result<(), LamboError> {
    let mem = Arc::new(build_memory(&opts, backends).await?);

    // Exactly once, at startup: `events()` is stateful on its first call — it
    // hands out the receiver subscribed *before* the daemon spawned, so the
    // spec §2.5 warm-up condition set (on a resumed session, the whole restored
    // set) is not lost. Draining it here also stops the broadcast channel from
    // filling and lagging the daemon.
    let events = mem.events();
    let event_pump = tokio::spawn(log_events(events));

    tracing::info!(
        session = %opts.session,
        agent = %opts.agent,
        transport = ?opts.transport,
        "lambo serve: session attached"
    );

    let outcome = match opts.transport {
        Transport::Stdio => serve_stdio(mem.clone()).await,
        Transport::Http => serve_http(mem.clone(), &opts).await,
    };

    // Close on every path, including the error one.
    //
    // The event pump is aborted *after* `close()` (R1/T82-17): canonization and
    // conflict events emitted during the final drain are exactly what an
    // operator debugging a failed close wants on stderr, and aborting first
    // threw them away.
    let closed = mem.close().await;
    event_pump.abort();

    match (outcome, closed) {
        (Err(e), _) => Err(e),
        (Ok(()), Err(e)) => {
            tracing::error!(error = %e, "lambo serve: final flush failed — tail kept, not durable");
            Err(e)
        }
        (Ok(()), Ok(())) => {
            tracing::info!("lambo serve: session closed, tail durable");
            Ok(())
        }
    }
}

/// Drain the daemon's event stream into the log.
///
/// A dropped or lagging receiver is not an error (spec §6.1); a lagging one
/// re-syncs.
async fn log_events(mut rx: tokio::sync::broadcast::Receiver<DaemonEvent>) {
    loop {
        match rx.recv().await {
            Ok(ev) => tracing::info!(event = ?ev, "daemon event"),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "daemon event stream lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Outcome of running a transport under a shutdown signal.
#[derive(Debug, PartialEq, Eq)]
enum Exit<T> {
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
async fn run_until_shutdown<T>(
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
async fn serve_stdio(mem: Arc<Memory>) -> Result<(), LamboError> {
    let service = LamboServer::new(mem)
        .serve(stdio())
        .await
        .map_err(|e| LamboError::Config(format!("mcp stdio: {e}")))?;

    // Taken before `waiting()` consumes the service — it is the only handle
    // left once the service is inside the future.
    let cancel_token = service.cancellation_token();
    match run_until_shutdown(
        service.waiting(),
        move || cancel_token.cancel(),
        shutdown_signal(),
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
/// The service factory clones an `Arc<Memory>` per request — it never builds a
/// second [`Memory`].
async fn serve_http(mem: Arc<Memory>, opts: &ServeOptions) -> Result<(), LamboError> {
    let factory_mem = mem.clone();
    let service = StreamableHttpService::new(
        move || Ok(LamboServer::new(factory_mem.clone())),
        Arc::new(LocalSessionManager::default()),
        {
            // `#[non_exhaustive]` — mutate the SDK default rather than
            // constructing, so a new field cannot silently break the build.
            let mut cfg = StreamableHttpServerConfig::default();
            cfg.sse_keep_alive = Some(Duration::from_secs(15));
            cfg
        },
    );

    let app = axum::Router::new().nest_service("/mcp", service);
    let addr = SocketAddr::new(opts.bind, opts.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| LamboError::Config(format!("mcp http: bind {addr}: {e}")))?;
    tracing::info!(%addr, "mcp http: listening on /mcp");

    serve_http_bounded(listener, app, shutdown_signal(), SHUTDOWN_GRACE).await
}

/// `axum::serve` with a **bounded** graceful shutdown (R1/T82-2).
///
/// Split out from [`serve_http`] so the bound is testable without a `Memory`:
/// the test holds a never-ending response open, exactly as a streamable-HTTP
/// MCP client's SSE channel does, and asserts this returns anyway.
async fn serve_http_bounded(
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

/// Ctrl-C (and SIGTERM on unix), so `close()` still runs.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => {
                    ctrl_c.await;
                    return;
                }
            };
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_parses_both_and_rejects_junk() {
        assert_eq!("stdio".parse::<Transport>().unwrap(), Transport::Stdio);
        assert_eq!("  HTTP ".parse::<Transport>().unwrap(), Transport::Http);
        assert!("grpc".parse::<Transport>().is_err());
    }

    #[test]
    fn serve_options_default_to_stdio_on_loopback() {
        let o = ServeOptions::new("s", "a");
        assert_eq!(o.transport, Transport::Stdio);
        assert_eq!(o.port, 7700);
        assert!(o.bind.is_loopback());
    }

    /// A transport that ends on its own is never cancelled and never waits on
    /// the signal — the ordinary client-disconnect path.
    #[tokio::test]
    async fn a_transport_that_finishes_on_its_own_is_not_cancelled() {
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = cancelled.clone();
        let exit = run_until_shutdown(
            async { 7u32 },
            move || c.store(true, std::sync::atomic::Ordering::SeqCst),
            std::future::pending::<()>(),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(exit, Exit::Finished(7));
        assert!(
            !cancelled.load(std::sync::atomic::Ordering::SeqCst),
            "cancel must not fire when the transport ended by itself"
        );
    }

    /// **R1/T82-1 pinned.** A shutdown signal cancels the transport and the
    /// function returns, so the caller reaches `Memory::close`.
    #[tokio::test]
    async fn a_shutdown_signal_cancels_the_transport_and_returns() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let exit = run_until_shutdown(
            async move {
                // Stands in for the rmcp service loop: it ends only when
                // cancelled, exactly like `service.waiting()`.
                let _ = rx.await;
                "cancelled"
            },
            move || {
                let _ = tx.send(());
            },
            async {},
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(exit, Exit::Finished("cancelled"));
    }

    /// **R1/T82-2 pinned, mechanism level.** A transport that ignores
    /// cancellation is abandoned when the grace window expires rather than
    /// holding the session open forever.
    #[tokio::test(start_paused = true)]
    async fn a_transport_that_ignores_cancellation_is_forced_after_the_grace_window() {
        let exit = run_until_shutdown(
            std::future::pending::<()>(),
            || {},
            async {},
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(exit, Exit::Forced, "the tail must not be held hostage");
    }

    /// **R1/T82-2 pinned, end to end through axum.** The reviewer's
    /// reproduction in miniature: a client holds a connection open with a
    /// request in flight that never completes — which is what a
    /// streamable-HTTP MCP client's SSE channel is to hyper, a connection that
    /// never goes idle — the signal fires, and `serve_http_bounded` must still
    /// return so `close()` runs. Before the fix `with_graceful_shutdown` waited
    /// on this connection forever and `Memory::close` was never reached.
    ///
    /// Verified to be a real pin: with the grace window raised past the test
    /// timeout, this test hangs.
    #[tokio::test]
    async fn http_shutdown_is_bounded_even_with_a_request_in_flight() {
        use tokio::io::AsyncWriteExt;

        let app = axum::Router::new().route(
            "/stream",
            axum::routing::get(|| async {
                // Outlives the test by far: the connection never goes idle.
                tokio::time::sleep(Duration::from_secs(3_600)).await;
                "unreachable"
            }),
        );
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let (sig_tx, sig_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            serve_http_bounded(
                listener,
                app,
                async move {
                    let _ = sig_rx.await;
                },
                Duration::from_millis(200),
            )
            .await
        });

        let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
        sock.write_all(
            b"GET /stream HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n",
        )
        .await
        .expect("request");
        // Let the server accept the connection and enter the handler.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let _ = sig_tx.send(());
        let out = tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect(
                "serve_http_bounded must return within the grace window, not block on the \
                 open connection",
            )
            .expect("server task");
        assert!(out.is_ok(), "a forced close is not an error: {out:?}");
        drop(sock);
    }

    /// A missing schema must name the remedy, not just the driver's complaint.
    #[test]
    fn an_unprovisioned_store_names_the_provision_step() {
        for raw in [
            "backend: lookup session: error returned from database: (code: 1) no such table: sessions",
            "relation \"sessions\" does not exist",
            "undefined_table",
        ] {
            let out = explain_startup_failure(LamboError::Config(raw.into())).to_string();
            assert!(
                out.contains("lambo provision"),
                "unprovisioned store must name the remedy, got: {out}"
            );
            assert!(
                out.contains(raw),
                "the underlying error must be kept, got: {out}"
            );
        }
    }

    /// …and an unrelated failure must be passed through untouched, so the
    /// hint never masks a different root cause.
    #[test]
    fn an_unrelated_startup_failure_is_passed_through() {
        let out =
            explain_startup_failure(LamboError::Config("connection refused".into())).to_string();
        assert!(out.contains("connection refused"), "{out}");
        assert!(
            !out.contains("lambo provision"),
            "an unrelated failure must not be relabelled as a provisioning problem: {out}"
        );
    }
}

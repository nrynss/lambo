//! Transport selection, startup errors and shutdown coordination.

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

/// **Test-gap (c).** The grace windows are sane bounds, not accidents: both
/// are non-zero (a zero window would force-drop instantly, defeating the
/// point) and short enough that a supervisor's kill escalation (commonly
/// ~30 s) never beats them, and `CLOSE_GRACE` — the last thing before exit —
/// is at least as generous as the transport window.
///
/// It also pins the **aggregate** (R4): `SHUTDOWN_GRACE + CLOSE_GRACE` is the
/// whole end-to-end shutdown cost and must stay within [`SHUTDOWN_BUDGET`],
/// the number an operator sizes their SIGKILL timeout against. Checking each
/// window alone let the sum drift past a tight supervisor window unnoticed.
#[test]
fn the_grace_windows_are_sane() {
    assert!(
        !SHUTDOWN_GRACE.is_zero(),
        "a zero transport grace force-drops instantly"
    );
    assert!(
        !CLOSE_GRACE.is_zero(),
        "a zero close grace never lets the flush finish"
    );
    assert!(
        SHUTDOWN_GRACE < Duration::from_secs(30),
        "must beat SIGKILL escalation"
    );
    assert!(
        CLOSE_GRACE < Duration::from_secs(30),
        "must beat SIGKILL escalation"
    );
    assert!(
        CLOSE_GRACE >= SHUTDOWN_GRACE,
        "the final close gets at least the transport window"
    );
    assert!(
        SHUTDOWN_GRACE + CLOSE_GRACE <= SHUTDOWN_BUDGET,
        "the end-to-end shutdown cost ({}s + {}s) must fit the documented budget ({}s) — \
             a supervisor's SIGKILL timeout is sized against SHUTDOWN_BUDGET",
        SHUTDOWN_GRACE.as_secs(),
        CLOSE_GRACE.as_secs(),
        SHUTDOWN_BUDGET.as_secs(),
    );
    // T8.6: the lease TTL must outlast the whole shutdown budget, so a
    // graceful close releases the lease rather than letting it expire while
    // the final flush is still running.
    assert!(
        lease::LEASE_TTL > SHUTDOWN_BUDGET,
        "LEASE_TTL ({}s) must exceed SHUTDOWN_BUDGET ({}s)",
        lease::LEASE_TTL.as_secs(),
        SHUTDOWN_BUDGET.as_secs(),
    );
    // L82-1: the lease-release window is carved OUT of the close budget,
    // not added to it, so the operator-facing SHUTDOWN_BUDGET is unchanged.
    assert_eq!(
        CLOSE_FLUSH_GRACE + LEASE_RELEASE_GRACE,
        CLOSE_GRACE,
        "the close phase is the flush attempt then the lease release, and nothing else"
    );
    assert!(
        !LEASE_RELEASE_GRACE.is_zero(),
        "a zero release window makes the abandoned-close release a no-op"
    );
    assert!(
        CLOSE_FLUSH_GRACE > LEASE_RELEASE_GRACE,
        "the flush keeps the bulk of the close budget; the release is one statement"
    );
}

/// A peer that leaves mid-handshake is a disconnect; a peer that says
/// something wrong is not.
///
/// This is the classification CI run 33085161710 turned red on. The
/// pre-handshake durability test reaps its holder with `Child::wait()`,
/// which closes stdin first, so the holder saw a stdin EOF racing the
/// `SIGTERM` it had just been sent. The EOF arrived as
/// `ConnectionClosed("initialize request")`, was reported as
/// `LamboError::Config`, and exited the process 1 — on a loaded runner,
/// where the EOF won the race, and only there. Both stimuli mean the client
/// is gone, so both must close the session and exit 0.
///
/// The negative half of the test is the load-bearing half: widening this to
/// "any handshake failure exits 0" would green the same CI run while hiding
/// a client that opened with the wrong frame, a rejected `initialize`, or a
/// genuine transport fault behind a successful exit status.
#[test]
fn only_a_mid_handshake_hangup_counts_as_a_disconnect() {
    assert!(
        is_pre_handshake_disconnect(&ServerInitializeError::ConnectionClosed(
            "initialize request".to_string()
        )),
        "a stdin EOF while waiting for `initialize` is the client leaving"
    );
    assert!(
        !is_pre_handshake_disconnect(&ServerInitializeError::ExpectedInitializeRequest(None)),
        "a client that opens with the wrong frame is still talking — that is a real fault \
             and must not be laundered into a clean exit"
    );
    assert!(
        !is_pre_handshake_disconnect(&ServerInitializeError::InitializeFailed(
            rmcp::model::ErrorData::invalid_request("nope", None)
        )),
        "a rejected `initialize` is a protocol failure, not a hangup"
    );
    assert!(
        !is_pre_handshake_disconnect(&ServerInitializeError::Cancelled),
        "cancellation is not a client hangup; `serve_stdio` handles the shutdown signal \
             through `setup_or_shutdown`, not through this classifier"
    );
}

/// **R2-a pinned, mechanism level.** A setup step that beats the signal is
/// kept; a signal that fires before setup finishes bails with `None`, which
/// is the caller's cue to skip serving and go straight to `close()`.
#[tokio::test]
async fn setup_or_shutdown_prefers_setup_but_bails_on_an_early_signal() {
    // Setup wins: the signal is `pending`, so the value comes through.
    assert_eq!(
        setup_or_shutdown(async { 5u32 }, std::future::pending::<()>()).await,
        Some(5)
    );
    // Signal wins: setup never finishes, so the caller is told to bail.
    assert_eq!(
        setup_or_shutdown(std::future::pending::<u32>(), async {}).await,
        None
    );
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
    sock.write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n")
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
    let out = explain_startup_failure(LamboError::Config("connection refused".into())).to_string();
    assert!(out.contains("connection refused"), "{out}");
    assert!(
        !out.contains("lambo provision"),
        "an unrelated failure must not be relabelled as a provisioning problem: {out}"
    );
}

/// A server whose one tool runs until its request is cancelled: the probe
/// for "a client disconnect cancels the call".
#[derive(Clone)]
struct CancelProbe {
    started: Arc<tokio::sync::Notify>,
    cancelled: Arc<tokio::sync::Notify>,
}

impl rmcp::ServerHandler for CancelProbe {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
    }

    async fn call_tool(
        &self,
        _request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        self.started.notify_one();
        tokio::select! {
            () = context.ct.cancelled() => self.cancelled.notify_one(),
            () = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        Ok(rmcp::model::CallToolResult::success(vec![]).into())
    }
}

/// A sessionless `tools/call` carrying its protocol version per request (the
/// 2026-07-28 shape), which rmcp answers directly, without an MCP session.
const PER_REQUEST_CALL: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"wait","arguments":{},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;

/// #32 PR 5 second review L1: a client that disconnects from a sessionless
/// request rmcp answers directly (`serve_negotiated_request_directly`)
/// cancels the call, as rmcp does on its own (rmcp #857). The request goes
/// through `serve_attributed`, the path every serve request takes, on a
/// real socket so the disconnect is axum dropping the handler's future.
///
/// Mutation: run `handle` on a spawned task (the first S1 fix) and the
/// call is never cancelled.
#[tokio::test]
async fn a_disconnected_sessionless_request_is_cancelled() {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    use tokio::io::AsyncWriteExt;

    let probe = CancelProbe {
        started: Arc::new(tokio::sync::Notify::new()),
        cancelled: Arc::new(tokio::sync::Notify::new()),
    };
    let (started, cancelled) = (probe.started.clone(), probe.cancelled.clone());
    let http = StreamableHttpService::new(
        move || Ok(probe.clone()),
        Arc::new(crate::mcp::serve::openers::AttributingSessions::new(
            Arc::new(LocalSessionManager::default()),
            Arc::new(crate::mcp::serve::openers::Openers::default()),
        )),
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::any(move |req: axum::extract::Request| {
            let http = http.clone();
            async move {
                crate::mcp::serve::transport::serve_attributed(&http, "scoped", req).await
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2026-07-28\r\n\
         Mcp-Method: tools/call\r\nMcp-Name: wait\r\nContent-Length: {}\r\n\r\n{PER_REQUEST_CALL}",
        PER_REQUEST_CALL.len()
    );
    client.write_all(head.as_bytes()).await.expect("write");
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("the call starts");

    // The client goes away mid-call.
    drop(client);
    tokio::time::timeout(Duration::from_secs(10), cancelled.notified())
        .await
        .expect("a disconnect cancels a sessionless call");
}

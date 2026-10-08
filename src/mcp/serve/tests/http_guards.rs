//! T8.7 HTTP surface hardening: tokens, bind authorization, rate limits
//! and the session cap.

use super::*;

// -----------------------------------------------------------------------
// T8.7 — HTTP surface hardening
// -----------------------------------------------------------------------

/// A secret must not be printable, because `ServeOptions` and clap's
/// `Commands` both derive `Debug` and a future edit will eventually log one.
#[test]
fn a_secret_token_never_prints_itself() {
    let t = SecretToken::new("hunter2-the-real-secret").expect("valid");
    let shown = format!("{t:?}");
    assert!(
        !shown.contains("hunter2"),
        "the token leaked through Debug: {shown}"
    );
    assert_eq!(shown, "SecretToken(<redacted>)");
    // And it must not be `Display`-able either — that is the other way a
    // secret reaches a log line. (Compile-time: no `Display` impl exists.)
}

/// Empty and whitespace-only tokens are refused rather than accepted as a
/// credential — an unset variable that expanded to nothing must not
/// authenticate `Authorization: Bearer `.
#[test]
fn an_empty_token_is_refused() {
    assert!(SecretToken::new("").is_err());
    assert!(SecretToken::new("   \t\n ").is_err());
    assert!(SecretToken::new("s").is_ok());
}

/// The comparison must be correct first — constant-time is worthless if it
/// gets the answer wrong.
#[test]
fn token_comparison_is_correct_including_lengths() {
    assert!(tokens_match(b"abc123", b"abc123"));
    assert!(!tokens_match(b"abc123", b"abc124"));
    assert!(!tokens_match(b"abc", b"abc123"), "prefix must not match");
    assert!(!tokens_match(b"abc123", b"abc"), "extension must not match");
    assert!(!tokens_match(b"", b"abc"));
    assert!(
        !tokens_match(b"abc", b""),
        "an empty expected token must never match"
    );
}

/// The `Authorization` header parse: scheme case-insensitive per RFC 7235,
/// credential exact.
#[test]
fn bearer_header_is_parsed_strictly() {
    let expected = SecretToken::new("s3cret").expect("valid");
    assert!(bearer_ok(Some("Bearer s3cret"), &expected));
    assert!(bearer_ok(Some("bearer s3cret"), &expected), "RFC 7235 §2.1");
    assert!(bearer_ok(Some("BEARER s3cret"), &expected));
    assert!(bearer_ok(Some("  Bearer s3cret  "), &expected));

    assert!(!bearer_ok(None, &expected), "a missing header is a refusal");
    assert!(!bearer_ok(Some("Bearer wrong"), &expected));
    assert!(!bearer_ok(Some("Basic s3cret"), &expected), "wrong scheme");
    assert!(!bearer_ok(Some("s3cret"), &expected), "no scheme at all");
    assert!(!bearer_ok(Some("Bearer"), &expected));
    assert!(!bearer_ok(Some(""), &expected));
}

/// **The environment wins.** A token in argv is visible in `ps` and shell
/// history, so the deployment channel takes precedence — and a set-but-empty
/// variable is an error, not a silent fallback to the flag.
#[test]
fn the_environment_overrides_the_flag() {
    let flag = || Some(SecretToken::new("from-flag").expect("valid"));

    let out = resolve_auth_token_from(flag(), Some("from-env".into())).expect("ok");
    assert_eq!(out, Some(SecretToken::new("from-env").unwrap()));

    let out = resolve_auth_token_from(flag(), None).expect("ok");
    assert_eq!(out, Some(SecretToken::new("from-flag").unwrap()));

    let out = resolve_auth_token_from(None, None).expect("ok");
    assert_eq!(out, None);

    let err = resolve_auth_token_from(flag(), Some("   ".into()))
        .expect_err("a set-but-empty env var must fail closed, not fall back to the flag");
    assert!(err.to_string().contains(AUTH_TOKEN_ENV), "{err}");
}

/// **T82-16 pinned, the load-bearing half.** A non-loopback bind without a
/// token must refuse to start, and the message must tell the operator both
/// ways out.
#[test]
fn a_non_loopback_bind_without_a_token_refuses_to_start() {
    let public: IpAddr = "203.0.113.7".parse().unwrap();
    let any: IpAddr = "0.0.0.0".parse().unwrap();
    let any_v6: IpAddr = "::".parse().unwrap();
    let token = SecretToken::new("t").expect("valid");

    for bind in [public, any, any_v6] {
        let err = authorize_bind(Transport::Http, bind, None)
            .expect_err("{bind} without a token must not start");
        let msg = err.to_string();
        assert!(msg.contains("refusing to start"), "{msg}");
        assert!(msg.contains(AUTH_TOKEN_ENV), "must name the env var: {msg}");
        assert!(msg.contains("--auth-token"), "must name the flag: {msg}");
        assert!(msg.contains("127.0.0.1"), "must name the safe bind: {msg}");
        // The same bind WITH a token is fine.
        authorize_bind(Transport::Http, bind, Some(&token)).expect("token satisfies the rule");
    }
}

/// The other three legs of the rule: loopback keeps today's optional-auth
/// behaviour, and stdio is untouched because it is process-local.
#[test]
fn loopback_and_stdio_do_not_require_a_token() {
    let loopback: IpAddr = "127.0.0.1".parse().unwrap();
    let loopback_v6: IpAddr = "::1".parse().unwrap();
    let public: IpAddr = "203.0.113.7".parse().unwrap();

    authorize_bind(Transport::Http, loopback, None).expect("loopback stays optional-auth");
    authorize_bind(Transport::Http, loopback_v6, None).expect("::1 is loopback too");
    // 127.0.0.0/8 in full, not just the .1 host.
    authorize_bind(Transport::Http, "127.9.9.9".parse().unwrap(), None).expect("127/8");
    // stdio ignores --bind entirely: there is no socket to protect.
    authorize_bind(Transport::Stdio, public, None).expect("stdio is process-local");
}

/// The bucket allows a burst, refuses past it, and refills with time.
#[test]
fn the_rate_limiter_bounds_a_sustained_excess_but_allows_a_burst() {
    let t0 = Instant::now();
    assert!(
        RateLimiter::new(0, t0).is_none(),
        "0 rps is the documented way to disable the limit"
    );

    let rl = RateLimiter::new(10, t0).expect("enabled");
    // Capacity is rps * burst factor, all available at t0.
    for i in 0..20 {
        assert!(rl.try_acquire_at(t0), "burst token {i} must be allowed");
    }
    assert!(
        !rl.try_acquire_at(t0),
        "the 21st in the same instant is out"
    );

    // Half a second later, 10 rps has put ~5 tokens back.
    let t1 = t0 + Duration::from_millis(500);
    for i in 0..5 {
        assert!(rl.try_acquire_at(t1), "refilled token {i}");
    }
    assert!(!rl.try_acquire_at(t1), "but only what was refilled");

    // A long idle refills to capacity and no further.
    let t2 = t1 + Duration::from_secs(3_600);
    for i in 0..20 {
        assert!(rl.try_acquire_at(t2), "post-idle token {i}");
    }
    assert!(
        !rl.try_acquire_at(t2),
        "the bucket must cap at capacity, not accumulate an unbounded idle credit"
    );
}

/// Only the request that can mint a session counts against the cap.
#[test]
fn only_a_sessionless_post_opens_a_new_session() {
    let req = |method: axum::http::Method, session: Option<&str>| {
        let mut b = axum::http::Request::builder().method(method).uri("/mcp");
        if let Some(s) = session {
            b = b.header("Mcp-Session-Id", s);
        }
        b.body(axum::body::Body::empty()).expect("request")
    };
    assert!(opens_a_new_session(&req(axum::http::Method::POST, None)));
    assert!(
        !opens_a_new_session(&req(axum::http::Method::POST, Some("abc"))),
        "a POST inside an existing session must not be counted again"
    );
    assert!(
        !opens_a_new_session(&req(axum::http::Method::GET, None)),
        "a GET opens an SSE stream, it does not mint a session"
    );
    assert!(!opens_a_new_session(&req(
        axum::http::Method::DELETE,
        Some("abc")
    )));
}

/// A fixed live-session count, so the cap is testable without standing up
/// real MCP sessions.
struct FakeSessions(usize);

#[async_trait::async_trait]
impl LiveSessions for FakeSessions {
    async fn live(&self) -> usize {
        self.0
    }
}

fn guard_with(auth: Option<&str>, max_sessions: usize, live: usize, rps: u32) -> HttpGuard {
    HttpGuard {
        auth: auth.map(|t| SecretToken::new(t).expect("valid")),
        max_sessions,
        live: Arc::new(FakeSessions(live)),
        rate: RateLimiter::new(rps, Instant::now()).map(Arc::new),
    }
}

/// Stand the guard up in front of a marker route on a real socket.
///
/// Raw TCP rather than an HTTP client: this crate has no client in
/// dev-dependencies, the status line is all these tests assert on, and the
/// file already drives axum this way in
/// `http_shutdown_is_bounded_even_with_a_request_in_flight`.
async fn spawn_guarded(guard: HttpGuard) -> (SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
    let reached = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits = reached.clone();
    let app = axum::Router::new()
        .route(
            "/mcp",
            axum::routing::any(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    "inner service reached"
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(guard, guard_request));

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, reached)
}

/// Fire one request and return `(status_code, body_ish)`.
async fn request(addr: SocketAddr, head: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    sock.write_all(head.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    // The marker route and every refusal close or complete promptly; the
    // timeout keeps a regression from hanging the suite.
    let _ = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut raw)).await;
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in response: {text:?}"));
    (status, text)
}

fn post(auth: Option<&str>, session: Option<&str>) -> String {
    let mut h = String::from("POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n");
    if let Some(a) = auth {
        h.push_str(&format!("Authorization: {a}\r\n"));
    }
    if let Some(s) = session {
        h.push_str(&format!("Mcp-Session-Id: {s}\r\n"));
    }
    h.push_str("Connection: close\r\n\r\n");
    h
}

/// **T82-16 pinned, request path.** Without a token the request is refused
/// with 401 and — the part that matters — the inner service is never
/// reached, so no session is minted and no body is parsed on behalf of an
/// unauthenticated caller.
#[tokio::test]
async fn an_unauthenticated_request_is_refused_before_the_session() {
    let (addr, reached) = spawn_guarded(guard_with(Some("s3cret"), 32, 0, 0)).await;

    let (status, body) = request(addr, &post(None, None)).await;
    assert_eq!(status, 401, "no credential must be 401: {body}");
    assert!(
        body.to_lowercase().contains("www-authenticate: bearer"),
        "a 401 must advertise the scheme: {body}"
    );

    let (status, _) = request(addr, &post(Some("Bearer wrong"), None)).await;
    assert_eq!(status, 401, "a wrong token must be 401");

    let (status, _) = request(addr, &post(Some("Basic s3cret"), None)).await;
    assert_eq!(
        status, 401,
        "the right secret under the wrong scheme is 401"
    );

    assert_eq!(
        reached.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "an unauthenticated request must never reach the MCP service"
    );
    // And the token is never echoed back to the caller.
    let (_, body) = request(addr, &post(None, None)).await;
    assert!(!body.contains("s3cret"), "the 401 leaked the token: {body}");
}

/// **T82-16 request-size limit.** An oversized declared body is refused
/// up front with 413 and never reaches the MCP service; a normal-size body
/// still gets through.
#[tokio::test]
async fn an_oversized_request_body_is_refused_before_the_service() {
    let (addr, reached) = spawn_guarded(guard_with(Some("s3cret"), 32, 0, 0)).await;

    let mut h = String::from("POST /mcp HTTP/1.1\r\nHost: localhost\r\n");
    h.push_str(&format!("Content-Length: {}\r\n", MAX_HTTP_BODY_BYTES + 1));
    h.push_str("Authorization: Bearer s3cret\r\nConnection: close\r\n\r\n");
    let (status, body) = request(addr, &h).await;
    assert_eq!(
        status, 413,
        "an oversized declared body must be refused with 413: {body}"
    );
    assert!(
        body.to_lowercase().contains("too large"),
        "the refusal should name the reason: {body}"
    );
    assert_eq!(
        reached.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "an oversized body must never reach the MCP service"
    );

    // A body within the cap is still served.
    let (status, _) = request(addr, &post(Some("Bearer s3cret"), None)).await;
    assert_eq!(
        status, 200,
        "a normal-size body must still be served: {status}"
    );
}

/// The accepted path: the right token gets through to the service.
#[tokio::test]
async fn an_authenticated_request_is_accepted() {
    let (addr, reached) = spawn_guarded(guard_with(Some("s3cret"), 32, 0, 0)).await;
    let (status, body) = request(addr, &post(Some("Bearer s3cret"), None)).await;
    assert_eq!(status, 200, "the right token must be accepted: {body}");
    assert!(body.contains("inner service reached"), "{body}");
    assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Loopback with no token configured keeps working exactly as before — this
/// hardening must not break the default local workflow.
#[tokio::test]
async fn an_unauthenticated_server_still_serves_loopback() {
    let (addr, reached) = spawn_guarded(guard_with(None, 32, 0, 0)).await;
    let (status, _) = request(addr, &post(None, None)).await;
    assert_eq!(status, 200, "no auth configured means no auth required");
    assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// **T82-16 pinned, the unbounded-session half.** At the cap the next
/// `initialize` is refused with an honest 503 that says what the limit is
/// and how to get under it — and requests belonging to sessions that
/// already exist keep working.
#[tokio::test]
async fn the_thirty_third_session_is_refused_honestly() {
    // 32 live against a cap of 32: the next new session is the 33rd.
    let (addr, reached) = spawn_guarded(guard_with(None, 32, 32, 0)).await;

    let (status, body) = request(addr, &post(None, None)).await;
    assert_eq!(status, 503, "past the cap must refuse: {body}");
    assert!(body.contains("32/32"), "say what the limit is: {body}");
    assert!(
        body.contains("--max-sessions"),
        "name the way to raise it: {body}"
    );
    assert!(
        body.contains("DELETE /mcp"),
        "name the way to free one: {body}"
    );
    assert!(
        body.to_lowercase().contains("retry-after"),
        "a 503 should tell the client when to come back: {body}"
    );
    assert_eq!(
        reached.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a refused session must not reach the service"
    );

    // The cap bounds NEW sessions only — the 32 already established must
    // keep being served, or the cap becomes an outage.
    let (status, _) = request(addr, &post(None, Some("existing-session"))).await;
    assert_eq!(status, 200, "an established session must keep working");
    assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// One under the cap still opens.
#[tokio::test]
async fn a_new_session_under_the_cap_is_admitted() {
    let (addr, reached) = spawn_guarded(guard_with(None, 32, 31, 0)).await;
    let (status, _) = request(addr, &post(None, None)).await;
    assert_eq!(status, 200, "31 live against a cap of 32 has room");
    assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// The rate limit refuses with 429 once the bucket is dry.
#[tokio::test]
async fn a_flood_is_refused_by_the_rate_limit() {
    // 1 rps => capacity 2. The third request in the same moment is out.
    let (addr, _) = spawn_guarded(guard_with(None, 32, 0, 1)).await;
    for i in 0..2 {
        let (status, _) = request(addr, &post(None, None)).await;
        assert_eq!(status, 200, "burst request {i} must be allowed");
    }
    let (status, body) = request(addr, &post(None, None)).await;
    assert_eq!(status, 429, "a sustained excess must be refused: {body}");
    assert!(
        body.to_lowercase().contains("retry-after"),
        "a 429 must tell the client when to retry: {body}"
    );
}

/// **Ordering pinned.** Authentication runs before the rate limit and
/// before the session-count read, so an anonymous flood cannot exhaust the
/// budget for authenticated callers or probe how loaded the server is.
#[tokio::test]
async fn auth_is_checked_before_the_rate_limit_and_the_cap() {
    // A dry-able bucket (capacity 2) AND a server already at its cap.
    let (addr, _) = spawn_guarded(guard_with(Some("s3cret"), 32, 32, 1)).await;

    // Five anonymous requests: all 401, none of them 429 or 503 — so none
    // of them spent a token or learned the session count.
    for i in 0..5 {
        let (status, _) = request(addr, &post(None, None)).await;
        assert_eq!(status, 401, "anonymous request {i} must be refused as 401");
    }

    // The authenticated caller still has its full burst budget.
    let (status, body) = request(addr, &post(Some("Bearer s3cret"), None)).await;
    assert_eq!(
        status, 503,
        "the authenticated caller reaches the cap check with budget intact \
             (503, not 429): {body}"
    );
}

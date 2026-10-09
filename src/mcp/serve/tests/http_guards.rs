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

/// The serve's bearer check: does the credential set of a serve whose only
/// credential is `expected` (the legacy token) accept `header`? Since #32
/// PR 5 that set is what `guard_request` asks.
fn bearer_ok(header: Option<&str>, expected: &SecretToken) -> bool {
    legacy_authority(Some(expected.clone()))
        .authenticate(header)
        .is_some()
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
    HttpGuard::new(
        legacy_authority(auth.map(|t| SecretToken::new(t).expect("valid"))),
        max_sessions,
        Arc::new(FakeSessions(live)),
        rps,
    )
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

// -----------------------------------------------------------------------
// #32 PR 5 review M2: fairness between credentials
// -----------------------------------------------------------------------

/// A fake token for credential `name`, built at runtime so no
/// token-shaped literal sits in the source.
fn fake_token(name: &str) -> String {
    ["fake", name, "guard", "value"].join("-")
}

/// A loopback HTTP serve's credential set: one configured credential per
/// name, each over every pinned session.
fn credentials_authority(names: &[&str]) -> Arc<ServeAuthority> {
    use crate::surface::session::{SessionCapabilities, SessionGrant, SessionScope};
    let mut opts = ServeOptions::new("lambo-test", "agent-test");
    opts.transport = Transport::Http;
    opts.credentials = names
        .iter()
        .map(|name| crate::config::ServeCredential {
            grant: SessionGrant::new(
                *name,
                SessionScope::pinned(),
                SessionCapabilities::default(),
            ),
            token: SecretToken::new(fake_token(name)).expect("non-empty"),
        })
        .collect();
    authority_for(&opts)
}

/// Live MCP sessions per credential.
struct FakeOpeners(Vec<(&'static str, usize)>);

#[async_trait::async_trait]
impl LiveSessions for FakeOpeners {
    async fn live(&self) -> usize {
        self.0.iter().map(|(_, n)| n).sum()
    }

    async fn live_opened_by(&self, credential: &str) -> usize {
        self.0
            .iter()
            .filter(|(name, _)| *name == credential)
            .map(|(_, n)| n)
            .sum()
    }
}

/// The share: the cap divided evenly, rounded down so the shares fit
/// inside it, never 0, and the whole cap for one credential (so a
/// one-credential serve is unchanged).
#[test]
fn each_credential_gets_an_even_share_of_the_session_cap() {
    for (max, credentials, share) in [
        (32, 1, 32),
        (32, 2, 16),
        (32, 3, 10),
        (8, 4, 2),
        (2, 3, 1),
        (0, 1, 1),
    ] {
        assert_eq!(
            credential_share(max, credentials),
            share,
            "{max} over {credentials}"
        );
        if share > 1 {
            assert!(share * credentials <= max, "{max} over {credentials}");
        }
    }
    assert_eq!(credential_share(32, 0), 32, "an implicit grant is one");
}

/// Each credential draws on its own bucket: draining one leaves every
/// other full, and the drained one refills on its own clock.
#[test]
fn each_credential_has_its_own_rate_bucket() {
    assert!(CredentialRates::new(0).is_none(), "0 disables the limit");
    let rates = CredentialRates::new(1).expect("enabled");
    let t0 = Instant::now();
    // 1 rps => capacity 2.
    assert!(rates.try_acquire_at("tenant", t0));
    assert!(rates.try_acquire_at("tenant", t0));
    assert!(!rates.try_acquire_at("tenant", t0), "tenant is dry");
    assert!(rates.try_acquire_at("operator", t0), "operator is not");
    assert!(rates.try_acquire_at("operator", t0));
    assert!(
        rates.try_acquire_at("tenant", t0 + Duration::from_secs(1)),
        "tenant refills"
    );
}

/// On the request path: one credential flooding past its bucket gets 429,
/// and another credential's next request is still served.
#[tokio::test]
async fn one_credential_flooding_does_not_rate_limit_another() {
    let guard = HttpGuard::new(
        credentials_authority(&["tenant", "operator"]),
        32,
        Arc::new(FakeSessions(0)),
        1,
    );
    let (addr, _) = spawn_guarded(guard).await;
    let tenant = format!("Bearer {}", fake_token("tenant"));
    let operator = format!("Bearer {}", fake_token("operator"));
    let mut refused = false;
    for _ in 0..4 {
        let (status, _) = request(addr, &post(Some(&tenant), Some("s"))).await;
        refused |= status == 429;
    }
    assert!(refused, "the tenant's flood is refused");
    let (status, body) = request(addr, &post(Some(&operator), Some("s"))).await;
    assert_eq!(status, 200, "the operator is still served: {body}");
}

/// On the request path: a credential at its share of the session cap gets
/// a 503 naming the share, while another credential, and the first one's
/// existing MCP sessions, are still served. Mutation: drop the share check
/// and the tenant's 17th `initialize` reaches the service.
#[tokio::test]
async fn a_credential_at_its_share_of_the_session_cap_is_refused_alone() {
    let guard = HttpGuard::new(
        credentials_authority(&["tenant", "operator"]),
        32,
        Arc::new(FakeOpeners(vec![("tenant", 16), ("operator", 1)])),
        0,
    );
    assert_eq!(guard.credential_sessions, 16);
    let (addr, reached) = spawn_guarded(guard).await;
    let tenant = format!("Bearer {}", fake_token("tenant"));
    let operator = format!("Bearer {}", fake_token("operator"));

    let (status, body) = request(addr, &post(Some(&tenant), None)).await;
    assert_eq!(status, 503, "the tenant is at its share: {body}");
    assert!(
        body.contains("16/16 of 32"),
        "say what the share is: {body}"
    );
    assert!(body.contains("--max-sessions"), "{body}");
    assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 0);

    let (status, _) = request(addr, &post(Some(&tenant), Some("existing"))).await;
    assert_eq!(status, 200, "the tenant's open sessions keep working");
    let (status, body) = request(addr, &post(Some(&operator), None)).await;
    assert_eq!(status, 200, "the operator still opens one: {body}");
}

/// One credential (the legacy `default`, the dogfood rig's) has the whole
/// cap and one bucket: exactly the limits a single-token serve always had.
#[test]
fn one_credential_keeps_the_whole_cap() {
    let guard = guard_with(Some("s3cret"), 32, 0, 50);
    assert_eq!(guard.credential_sessions, 32);
    let local = guard_with(None, 32, 0, 50);
    assert_eq!(local.credential_sessions, 32);
}

// -----------------------------------------------------------------------
// #32 PR 5 review L2: bounded work and logging for bad tokens
// -----------------------------------------------------------------------

/// A configured token over the presented-credential cap could never be
/// presented, so it is refused when it is made (and so at startup); one at
/// the cap is accepted.
#[test]
fn a_token_over_the_credential_cap_is_refused() {
    use crate::surface::bearer::MAX_BEARER_CREDENTIAL_BYTES;
    assert!(SecretToken::new("t".repeat(MAX_BEARER_CREDENTIAL_BYTES)).is_ok());
    let err =
        SecretToken::new("t".repeat(MAX_BEARER_CREDENTIAL_BYTES + 1)).expect_err("over the cap");
    assert!(err.contains("4096"), "{err}");
    assert!(!err.contains("ttt"), "the value is never quoted: {err}");
}

/// The 401's WARN line: the first refusal in a window is logged with the
/// count held back before it, the rest of the window is not.
#[test]
fn the_refusal_warning_is_logged_once_per_window() {
    let log = RefusalLog::new(Duration::from_secs(10));
    let t0 = Instant::now();
    assert_eq!(log.note_at(t0), Some(0));
    for i in 1..=5 {
        assert_eq!(log.note_at(t0 + Duration::from_secs(i)), None);
    }
    assert_eq!(log.note_at(t0 + Duration::from_secs(10)), Some(5));
    assert_eq!(log.note_at(t0 + Duration::from_secs(11)), None);
    assert_eq!(log.note_at(t0 + Duration::from_secs(25)), Some(1));
}

/// On the request path: an oversized bearer header is the ordinary 401.
#[tokio::test]
async fn an_oversized_bearer_is_the_ordinary_401() {
    let (addr, reached) = spawn_guarded(guard_with(Some("s3cret"), 32, 0, 0)).await;
    let long = format!(
        "Bearer {}",
        "x".repeat(crate::surface::bearer::MAX_BEARER_CREDENTIAL_BYTES + 1)
    );
    let (status, long_body) = request(addr, &post(Some(&long), None)).await;
    let (_, wrong_body) = request(addr, &post(Some("Bearer wrong"), None)).await;
    assert_eq!(status, 401);
    let tail = |b: &str| b.split_once("\r\n\r\n").map(|(_, t)| t.to_string());
    assert_eq!(tail(&long_body), tail(&wrong_body));
    assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// #32 PR 5 review I2: a request with two `Authorization` headers is the
/// ordinary 401, even when both carry the right token, so no proxy that
/// keeps the first or the last can disagree with the guard about who is
/// calling. Without a credential (the implicit `local`) no header is read.
#[tokio::test]
async fn two_authorization_headers_are_the_ordinary_401() {
    let (addr, reached) = spawn_guarded(guard_with(Some("s3cret"), 32, 0, 0)).await;
    let twice = "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\
                 Authorization: Bearer s3cret\r\nAuthorization: Bearer s3cret\r\n\
                 Connection: close\r\n\r\n";
    let (status, body) = request(addr, twice).await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 0);

    let (addr, reached) = spawn_guarded(guard_with(None, 32, 0, 0)).await;
    let (status, body) = request(addr, twice).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 1);
}

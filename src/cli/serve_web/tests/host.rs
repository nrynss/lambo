//! The `Host` check (#4 PR 2, design 4.5): DNS-rebinding defence for the
//! unauthenticated loopback portal.

use super::*;
use crate::config::{AllowedHost, WebConfig};

/// [`request`] with a chosen `Host` header (or none) and an optional
/// `Authorization` value.
async fn request_with(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: Option<&str>,
    authorization: Option<&str>,
) -> HttpResponse {
    let host = host.map(|h| format!("Host: {h}\r\n")).unwrap_or_default();
    let auth = authorization
        .map(|a| format!("Authorization: {a}\r\n"))
        .unwrap_or_default();
    let req = format!("{method} {path} HTTP/1.1\r\n{host}{auth}Connection: close\r\n\r\n");
    send_raw(addr, &req).await
}

/// Send `req` verbatim (a whole request head, ending in a blank line) and
/// read the response to EOF.
async fn send_raw(addr: SocketAddr, req: &str) -> HttpResponse {
    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
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
    HttpResponse {
        status,
        headers: head.to_string(),
        body: body.to_string(),
    }
}

fn without_date(r: &HttpResponse) -> (String, String) {
    (
        r.headers
            .lines()
            .filter(|l| !l.to_ascii_lowercase().starts_with("date:"))
            .collect::<Vec<_>>()
            .join("\r\n"),
        r.body.clone(),
    )
}

/// A two-session portal on `t4-a`/`t4-b`, with `auth` and `allowed` hosts.
async fn portal_with(
    auth: Option<AuthToken>,
    allowed: &[&str],
) -> (SocketAddr, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
    let store = seed("t4-a").await;
    let loads = Arc::new(AtomicUsize::new(0));
    let allowed: Vec<AllowedHost> = allowed
        .iter()
        .map(|h| AllowedHost::parse(h).expect("host"))
        .collect();
    let state = Arc::new(AppState::new(
        SessionId::new("t4-a"),
        [SessionId::new("t4-b")],
        backends_with_store(Box::new(CountLoads(Shared(store), loads.clone()))),
        auth.is_some(),
        auth,
        &allowed,
        &WebConfig::default(),
    ));
    let (addr, handle) = spawn(state).await;
    (addr, handle, loads)
}

/// [`Shared`], counting `load_session` and `read_flush_stats`, the reads a
/// served data route makes.
#[derive(Clone)]
struct CountLoads(Shared, Arc<AtomicUsize>);

#[async_trait]
impl GraphStore for CountLoads {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.0.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.0.flush(batch, token).await
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.1.fetch_add(1, Ordering::SeqCst);
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
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0
            .vector_candidates_checked(session, embedding, expected_contract, limit)
            .await
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
    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.0.record_canonization(event, token).await
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
    async fn write_flush_stats(
        &self,
        session: &SessionId,
        stats: &crate::store::SessionFlushStats,
    ) -> Result<(), StoreError> {
        self.0.write_flush_stats(session, stats).await
    }
    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<crate::store::SessionFlushStats>, StoreError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.read_flush_stats(session).await
    }
}

const PATHS: &[&str] = &[
    "/",
    "/healthz",
    "/app.js",
    "/api/pulse",
    "/s/t4-a/",
    "/s/t4-a/api/pulse",
    "/s/t4-c/api/pulse",
    "/no/such/path",
];

/// Under the implicit loopback grant, only the loopback names (any port,
/// any case) are answered; every other `Host`, a missing one and a malformed
/// one get one fixed 403 on every path and method (so it says nothing about
/// sessions), before any store read.
#[tokio::test]
async fn the_implicit_grant_answers_only_loopback_hosts() {
    let (addr, handle, loads) = portal_with(None, &[]).await;
    let port = addr.port();
    for host in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("LocalHost:{port}"),
        format!("[::1]:{port}"),
        "localhost".to_string(),
        "127.0.0.1".to_string(),
    ] {
        let r = request_with(addr, "GET", "/api/stats", Some(&host), None).await;
        assert_eq!(r.status, 200, "Host {host}: {}", r.body);
    }

    let refused = request_with(addr, "GET", "/api/stats", Some("rebind.example"), None).await;
    assert_eq!(refused.status, 403);
    assert!(refused.body.contains("--allowed-host"), "{}", refused.body);
    let expected = without_date(&refused);
    let before = loads.load(Ordering::SeqCst);
    for host in [
        Some("rebind.example".to_string()),
        Some(format!("rebind.example:{port}")),
        Some(format!("127.0.0.1.nip.io:{port}")),
        Some(format!("evil.localhost:{port}")),
        Some(format!("10.0.0.7:{port}")),
        Some(format!("localhost.:{port}")),
        Some("a b".to_string()),
        Some(String::new()),
        // Review L1: malformed, though the host part is loopback.
        Some("localhost:abc".to_string()),
        Some("localhost:".to_string()),
        Some("127.0.0.1:".to_string()),
        Some(format!("[::1]:x{port}")),
        Some("evil@localhost".to_string()),
        Some(format!("evil@localhost:{port}")),
        Some(format!("u:p@127.0.0.1:{port}")),
        Some(format!("localhost:{port}/x")),
        None,
    ] {
        // Review L5: every shape reaches the portal and gets its 403; none
        // is skipped (hyper refuses none of them itself).
        for path in PATHS {
            for method in ["GET", "POST"] {
                let r = request_with(addr, method, path, host.as_deref(), None).await;
                assert_eq!(
                    without_date(&r),
                    expected,
                    "{method} {path} with Host {host:?}"
                );
            }
        }
    }
    // A request with no Host reaches the portal (hyper does not refuse it)
    // and gets the same 403, not a fallback to "local, so fine".
    let none = request_with(addr, "GET", "/api/stats", None, None).await;
    assert_eq!(without_date(&none), expected, "no Host at all");
    assert_eq!(
        loads.load(Ordering::SeqCst),
        before,
        "a refused Host makes no store read"
    );
    handle.abort();
}

/// Review L2: a request with two `Host` headers gets the fixed 403, in
/// either order and even when both are loopback, so a proxy in front that
/// reads the other one cannot disagree with the portal about the target.
#[tokio::test]
async fn two_host_headers_are_refused() {
    let (addr, handle, loads) = portal_with(None, &[]).await;
    let port = addr.port();
    let expected =
        without_date(&request_with(addr, "GET", "/api/stats", Some("rebind.example"), None).await);
    assert!(expected.0.starts_with("HTTP/1.1 403"), "{}", expected.0);
    let before = loads.load(Ordering::SeqCst);
    let loopback = format!("localhost:{port}");
    for (first, second) in [
        ("rebind.example", loopback.as_str()),
        (loopback.as_str(), "rebind.example"),
        (loopback.as_str(), loopback.as_str()),
    ] {
        for path in PATHS {
            let req = format!(
                "GET {path} HTTP/1.1\r\nHost: {first}\r\nHost: {second}\r\nConnection: close\r\n\r\n"
            );
            let r = send_raw(addr, &req).await;
            assert_eq!(
                without_date(&r),
                expected,
                "{path}: Host {first} then {second}"
            );
        }
    }
    assert_eq!(loads.load(Ordering::SeqCst), before, "no store read");
    handle.abort();
}

/// Review L1 for the absolute-form fallback: a request with no `Host` whose
/// target authority carries user info is refused, while a plain loopback
/// one is answered.
#[tokio::test]
async fn a_malformed_target_authority_is_refused() {
    let (addr, handle, loads) = portal_with(None, &[]).await;
    let port = addr.port();
    let before = loads.load(Ordering::SeqCst);
    let user_info = send_raw(
        addr,
        &format!("GET http://evil@localhost:{port}/api/stats HTTP/1.0\r\n\r\n"),
    )
    .await;
    assert_eq!(user_info.status, 403, "{}", user_info.body);
    assert_eq!(loads.load(Ordering::SeqCst), before, "no store read");
    let plain = send_raw(
        addr,
        &format!("GET http://localhost:{port}/api/stats HTTP/1.0\r\n\r\n"),
    )
    .await;
    assert_eq!(plain.status, 200, "{}", plain.body);
    handle.abort();
}

/// `--allowed-host` / `[web] allowed_hosts` extend the loopback names: an
/// entry without a port matches any port, one with a port only that port.
#[tokio::test]
async fn allowed_hosts_extend_the_loopback_names() {
    let (addr, handle, _) = portal_with(None, &["lambo.example.com", "proxy.internal:8443"]).await;
    for (host, status) in [
        ("lambo.example.com", 200),
        ("LAMBO.example.com:443", 200),
        ("proxy.internal:8443", 200),
        ("proxy.internal:443", 403),
        ("proxy.internal", 403),
        ("other.example.com", 403),
    ] {
        let r = request_with(addr, "GET", "/s/t4-a/api/stats", Some(host), None).await;
        assert_eq!(r.status, status, "Host {host}: {}", r.body);
    }
    handle.abort();
}

/// With a token configured any `Host` is accepted (a rebound page cannot
/// present the token), and the bearer check decides: 401 without it, 200
/// with it, on a foreign `Host` too.
#[tokio::test]
async fn a_configured_token_accepts_any_host() {
    let secret = ["t4", "host", "check", "secret"].join("-");
    let token = AuthToken::new(secret.as_str()).expect("token");
    let (addr, handle, _) = portal_with(Some(token), &[]).await;
    let bearer = format!("Bearer {secret}");
    for host in ["rebind.example", "lambo.example.com:443", "127.0.0.1"] {
        let r = request_with(addr, "GET", "/api/stats", Some(host), None).await;
        assert_eq!(r.status, 401, "Host {host} without a token");
        let r = request_with(addr, "GET", "/s/t4-a/api/stats", Some(host), Some(&bearer)).await;
        assert_eq!(r.status, 200, "Host {host} with the token: {}", r.body);
    }
    handle.abort();
}

/// The check is chosen from the credential set: loopback names (plus the
/// extras) under the implicit grant, any host once a bearer is required.
#[test]
fn the_host_check_follows_the_credential_set() {
    let sessions = [SessionId::new("t4-a")];
    let extra = [AllowedHost::parse("lambo.example.com").expect("host")];
    let implicit = portal_authority(None, &sessions);
    let HostCheck::Only(hosts) = HostCheck::for_authority(&implicit, &extra) else {
        panic!("the implicit grant checks Host");
    };
    assert_eq!(hosts.len(), LOOPBACK_HOSTS.len() + 1);
    let token = AuthToken::new(["t4", "k"].concat()).expect("token");
    let bearer = portal_authority(Some(token), &sessions);
    assert_eq!(HostCheck::for_authority(&bearer, &extra), HostCheck::Any);
}

/// `--allowed-host` ∪ `[web] allowed_hosts`, each once; a malformed entry
/// is exit 2 naming it, before any backend.
#[test]
fn allowed_hosts_are_planned_and_checked() {
    let web = WebConfig {
        allowed_hosts: vec!["b.example".into(), "a.example".into()],
        ..Default::default()
    };
    let hosts = plan_allowed_hosts(&["a.example".into()], &web).expect("plan");
    assert_eq!(hosts, ["a.example", "b.example"]);
    for bad in ["", "user@host", "https://host", "host/path"] {
        let err = plan_allowed_hosts(&[bad.into()], &WebConfig::default()).expect_err(bad);
        assert_eq!(err.exit_code(), 2, "{bad:?}");
        assert!(err.to_string().contains("--allowed-host"), "{err}");
    }
}

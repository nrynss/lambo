//! Multi-session routing (#4 PR 2): the allowlist, `/s/{session}/...`, the
//! uniform 404 with zero store calls, the aliases, the read-only sweep under
//! a scope, and two sessions kept apart.

use super::*;
use crate::config::WebConfig;

/// Every store call, by method name, so a test can say "none at all".
#[derive(Clone)]
pub(super) struct Recording {
    inner: Shared,
    calls: Arc<parking_lot::Mutex<Vec<&'static str>>>,
}

impl Recording {
    pub(super) fn new(store: Arc<MemoryStore>) -> Self {
        Self {
            inner: Shared(store),
            calls: Arc::default(),
        }
    }

    fn note(&self, method: &'static str) {
        self.calls.lock().push(method);
    }

    pub(super) fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().clone()
    }

    pub(super) fn loads(&self) -> usize {
        self.calls()
            .iter()
            .filter(|m| **m == "load_session")
            .count()
    }
}

#[async_trait]
impl GraphStore for Recording {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.note("init_schema");
        self.inner.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.note("capabilities");
        self.inner.capabilities()
    }
    async fn preflight_schema(&self) -> Result<(), StoreError> {
        self.note("preflight_schema");
        self.inner.preflight_schema().await
    }
    fn vector_dimensions(&self) -> Option<usize> {
        self.note("vector_dimensions");
        self.inner.vector_dimensions()
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.note("flush");
        self.inner.flush(batch, token).await
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.note("load_session");
        self.inner.load_session(session).await
    }
    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.note("keyword_candidates");
        self.inner.keyword_candidates(session, tokens, limit).await
    }
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.note("vector_candidates");
        self.inner
            .vector_candidates(session, embedding, limit)
            .await
    }
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.note("vector_candidates_checked");
        self.inner
            .vector_candidates_checked(session, embedding, expected_contract, limit)
            .await
    }
    fn exact_vector_scan(&self) -> bool {
        self.note("exact_vector_scan");
        self.inner.exact_vector_scan()
    }
    fn holder_derives_from_graph(&self) -> bool {
        self.note("holder_derives_from_graph");
        self.inner.holder_derives_from_graph()
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.note("blast_radius");
        self.inner
            .blast_radius(session, node, min_edge_age, now)
            .await
    }
    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        self.note("interaction_span");
        self.inner
            .interaction_span(session, node, min_age, now)
            .await
    }
    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.note("record_canonization");
        self.inner.record_canonization(event, token).await
    }
    async fn erase_session(
        &self,
        session: &SessionId,
        eraser: &crate::store::LeaseHolder,
    ) -> Result<crate::store::EraseOutcome, StoreError> {
        self.note("erase_session");
        self.inner.erase_session(session, eraser).await
    }
    async fn backfill_recall_index(
        &self,
        session: &SessionId,
        holder: &crate::store::LeaseHolder,
    ) -> Result<Option<crate::store::RecallBackfillReport>, StoreError> {
        self.note("backfill_recall_index");
        self.inner.backfill_recall_index(session, holder).await
    }
    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::LeaseOutcome, StoreError> {
        self.note("acquire_lease");
        self.inner.acquire_lease(session, holder, ttl).await
    }
    async fn read_lease(
        &self,
        session: &SessionId,
    ) -> Result<Option<crate::store::LeaseInfo>, StoreError> {
        self.note("read_lease");
        self.inner.read_lease(session).await
    }
    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::LeaseOutcome, StoreError> {
        self.note("refresh_lease");
        self.inner.refresh_lease(session, holder, ttl).await
    }
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::LeaseHolder,
    ) -> Result<(), StoreError> {
        self.note("release_lease");
        self.inner.release_lease(session, holder).await
    }
    async fn record_lease_refusal(
        &self,
        session: &SessionId,
        refused_by: &str,
        current_holder: &str,
    ) -> Result<(), StoreError> {
        self.note("record_lease_refusal");
        self.inner
            .record_lease_refusal(session, refused_by, current_holder)
            .await
    }
    async fn pending_lease_refusals(
        &self,
        session: &SessionId,
        since: DateTime<Utc>,
    ) -> Result<Vec<crate::store::lease::LeaseRefusal>, StoreError> {
        self.note("pending_lease_refusals");
        self.inner.pending_lease_refusals(session, since).await
    }
    async fn write_flush_stats(
        &self,
        session: &SessionId,
        stats: &crate::store::SessionFlushStats,
    ) -> Result<(), StoreError> {
        self.note("write_flush_stats");
        self.inner.write_flush_stats(session, stats).await
    }
    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<crate::store::SessionFlushStats>, StoreError> {
        self.note("read_flush_stats");
        self.inner.read_flush_stats(session).await
    }
}

/// A portal serving `sessions` (the first is the default).
pub(super) fn portal(
    backends: ResolvedBackends,
    sessions: &[&str],
    auth: Option<AuthToken>,
    web: &WebConfig,
) -> Arc<AppState> {
    let ids: Vec<SessionId> = sessions.iter().map(|s| SessionId::new(*s)).collect();
    Arc::new(AppState::new(
        ids[0].clone(),
        ids,
        backends,
        auth.is_some(),
        auth,
        Vec::new(),
        &[],
        web,
    ))
}

/// Two sessions in one store: `t4-a` from [`seed`] ("user schema", a
/// hierarchy, an action, a promotion to Canonical) and `t4-b` (one concept,
/// "billing ledger").
pub(super) async fn two_sessions() -> Arc<MemoryStore> {
    let store = seed("t4-a").await;
    crate::cli::derive::run(
        backends_on(store.clone()),
        crate::cli::derive::Args {
            session: "t4-b".into(),
            agent: "agent-b".into(),
            content: "billing ledger".into(),
            kind: ConceptKind::Entity,
            parent_of: vec![],
            concept: vec![],
        },
    )
    .await
    .expect("derive b");
    store
}

/// The response head without its `date` line, which differs per second.
fn head_without_date(r: &HttpResponse) -> String {
    r.headers
        .lines()
        .filter(|l| !l.to_ascii_lowercase().starts_with("date:"))
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// Status line, headers (no date) and body: what "byte-identical" compares.
pub(super) fn wire(r: &HttpResponse) -> (String, String) {
    (head_without_date(r), r.body.clone())
}

pub(super) const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// Every id kind the portal must refuse, as raw path segments: served
/// nowhere, malformed, percent-encoded (one of them a served name with an
/// encoded `-`), dot-led, oversized, and empty.
pub(super) fn refused_ids() -> Vec<String> {
    vec![
        "t4-c".into(),
        "a%2Fb".into(),
        "t4%2Da".into(),
        "..".into(),
        ".hidden".into(),
        "x".repeat(crate::surface::session::MAX_ADDRESSED_LEN + 1),
        "a*b".into(),
        String::new(),
    ]
}

// ---- planning the served set ----------------------------------------

/// `--session` ∪ `[web] sessions`, in order, each once; the first is the
/// default; an empty union is exit 2.
#[test]
fn the_served_set_is_the_ordered_union_and_needs_one() {
    let web = WebConfig {
        sessions: vec!["t4-a".into(), "t4-c".into()],
        ..Default::default()
    };
    let plan = plan_sessions(&["t4-b".into(), "t4-a".into()], &web).expect("plan");
    assert_eq!(plan.default, "t4-b");
    assert_eq!(plan.sessions, ["t4-b", "t4-a", "t4-c"]);

    let plan = plan_sessions(&[], &web).expect("config alone");
    assert_eq!(plan.default, "t4-a");
    assert_eq!(plan.sessions, ["t4-a", "t4-c"]);

    let err = plan_sessions(&[], &WebConfig::default()).expect_err("nothing to serve");
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("[web] sessions"), "{err}");
}

/// Q12: one session keeps `--session`'s loose rule; with more than one,
/// every name must be addressable, and startup names the one that is not.
#[test]
fn loose_names_are_served_only_alone() {
    let web = WebConfig::default();
    let plan = plan_sessions(&["team notes/v1".into()], &web).expect("one loose name");
    assert_eq!(plan.sessions, ["team notes/v1"]);

    let err = plan_sessions(&["t4-a".into(), "team notes/v1".into()], &web)
        .expect_err("a loose name beside another");
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("\"team notes/v1\""), "{err}");
    assert!(err.to_string().contains("/s/<session>/"), "{err}");

    for bad in ["", "a\u{7}b"] {
        let err = plan_sessions(&[bad.into()], &web).expect_err("bad name");
        assert_eq!(err.exit_code(), 2, "{bad:?}");
    }
}

/// A library caller building `Args` by hand meets the same rules before any
/// store call: a default outside the set, a repeat, a loose name beside
/// another.
#[tokio::test]
async fn run_refuses_a_bad_served_set_before_the_store() {
    for (session, sessions) in [
        ("t4-x", vec!["t4-a", "t4-b"]),
        ("t4-a", vec!["t4-a", "t4-a"]),
        ("t4-a", vec!["t4-a", "no good"]),
    ] {
        let recording = Recording::new(Arc::new(MemoryStore::new()));
        let err = run(
            backends_with_store(Box::new(recording.clone())),
            Args {
                session: session.into(),
                sessions: sessions.iter().map(|s| (*s).to_string()).collect(),
                port: 0,
                bind: Ipv4Addr::LOCALHOST.into(),
                auth_token: None,
                allowed_hosts: Vec::new(),
                credentials: Vec::new(),
                web: WebConfig::default(),
            },
        )
        .await
        .expect_err("refused");
        assert_eq!(err.exit_code(), 2, "{session} {sessions:?}: {err}");
        assert!(recording.calls().is_empty(), "{:?}", recording.calls());
    }
}

// ---- the uniform 404 --------------------------------------------------

/// Design 3.3 and the PR 2 acceptance: an unknown, unconfigured, malformed,
/// percent-encoded, oversized or empty id answers the same bytes (status,
/// headers, body) as a path nothing routes, on every method and under every
/// suffix, and none of it reaches the store.
#[tokio::test]
async fn a_refused_session_is_byte_identical_to_an_unrouted_path_with_no_store_call() {
    let recording = Recording::new(two_sessions().await);
    let state = portal(
        backends_with_store(Box::new(recording.clone())),
        &["t4-a", "t4-b"],
        None,
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    let before = recording.calls();

    for method in METHODS {
        let unrouted = request(addr, method, "/no/such/path").await;
        assert_eq!(unrouted.status, 404, "{method}");
        assert!(unrouted.body.is_empty(), "{method}");
        let expected = wire(&unrouted);
        for id in refused_ids() {
            for rest in [
                "",
                "/",
                "/api/pulse",
                "/api/session?x=1",
                "/api/recall?q=user",
                "/app.js",
            ] {
                let path = format!("/s/{id}{rest}");
                let r = request(addr, method, &path).await;
                assert_eq!(wire(&r), expected, "{method} {path}");
            }
        }
    }
    assert_eq!(
        recording.calls(),
        before,
        "a refused request must make zero store calls"
    );
    handle.abort();
}

/// Under a configured token, the 401 comes first and is the same for every
/// id, served or not; nothing about sessions is evaluated before it. It is
/// the unrouted path's 401, byte for byte. The unscoped routes keep the 401
/// they always had (the gate over the routes; `axum` orders two headers
/// differently for a routed response, which says only that a route exists,
/// and the routes are public), with the same status and body.
#[tokio::test]
async fn the_401_comes_before_any_session_and_is_the_same_for_every_id() {
    let recording = Recording::new(two_sessions().await);
    let token = AuthToken::new(["t4", "-", "portal", "-", "key"].concat()).expect("token");
    let state = portal(
        backends_with_store(Box::new(recording.clone())),
        &["t4-a", "t4-b"],
        Some(token),
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    for method in METHODS {
        let expected = wire(&request(addr, method, "/no/such/path").await);
        assert!(expected.0.starts_with("HTTP/1.1 401"), "{}", expected.0);
        for id in refused_ids()
            .into_iter()
            .chain(["t4-a".to_string(), "t4-b".to_string()])
        {
            for rest in ["", "/", "/api/pulse", "/nope"] {
                let path = format!("/s/{id}{rest}");
                let r = request(addr, method, &path).await;
                assert_eq!(wire(&r), expected, "{method} {path}");
            }
        }
        let alias = request(addr, method, "/api/pulse").await;
        assert_eq!(alias.status, 401, "{method}");
        assert_eq!(alias.body, expected.1, "{method}");
    }
    assert!(recording.calls().is_empty(), "{:?}", recording.calls());
    handle.abort();
}

/// Review L3: a request carrying two `Authorization` headers is refused
/// like a wrong token, in either order (good then bad, bad then good), on
/// the unscoped and the scoped routes, with the 401 each path gives a
/// request with no header, byte for byte. One good header still passes.
#[tokio::test]
async fn two_authorization_headers_are_401_in_either_order() {
    let secret = ["t4", "-", "l3", "-", "key"].concat();
    let token = AuthToken::new(secret.as_str()).expect("token");
    let state = portal(
        backends_on(two_sessions().await),
        &["t4-a", "t4-b"],
        Some(token),
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    let good = format!("Authorization: Bearer {secret}\r\n");
    let bad = format!("Authorization: Bearer {secret}x\r\n");
    let send = |path: &str, auth: String| {
        let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Connection: close\r\n\r\n");
        async move { send_raw(addr, &req).await }
    };
    for path in ["/api/stats", "/s/t4-b/api/stats", "/s/t4-a/"] {
        let none = send(path, String::new()).await;
        assert_eq!(none.status, 401, "{path}");
        let one = send(path, good.clone()).await;
        assert_eq!(one.status, 200, "{path}: {}", one.body);
        for (first, second) in [(&good, &bad), (&bad, &good), (&good, &good)] {
            let r = send(path, format!("{first}{second}")).await;
            assert_eq!(wire(&r), wire(&none), "{path}");
            assert!(!r.body.contains(&secret), "never echoed");
        }
    }
    handle.abort();
}

// ---- in scope -----------------------------------------------------------

/// Q11 and the read-only sweep under a scope: a served session's routes
/// answer `GET` only (405 otherwise, as the aliases do); a path that is not
/// a route under a session is the same 404 as an unrouted path.
#[tokio::test]
async fn in_scope_mutating_methods_are_405_and_non_routes_are_the_uniform_404() {
    let store = two_sessions().await;
    let state = portal(
        backends_on(store),
        &["t4-a", "t4-b"],
        None,
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    let scoped_routes: Vec<&str> = ROUTES
        .iter()
        .copied()
        // The listing is unscoped (#4 PR 3): never under a session.
        .filter(|r| *r == "/" || (r.starts_with("/api/") && *r != "/api/sessions"))
        .collect();
    assert_eq!(scoped_routes.len(), 8, "the page and seven data routes");
    for id in ["t4-a", "t4-b"] {
        for route in &scoped_routes {
            let path = format!("/s/{id}{route}");
            for method in ["POST", "PUT", "PATCH", "DELETE"] {
                let r = request(addr, method, &path).await;
                let alias = request(addr, method, route).await;
                assert_eq!(r.status, 405, "{method} {path}: {}", r.body);
                assert_eq!(wire(&r), wire(&alias), "{method} {path} vs {route}");
            }
        }
        let unrouted = wire(&request(addr, "GET", "/no/such/path").await);
        for rest in [
            "/app.css",
            "/app.js",
            "/healthz",
            "/nope",
            "/api",
            "/api/nope",
            "//api/pulse",
            "/api/sessions",
        ] {
            let path = format!("/s/{id}{rest}");
            let r = request(addr, "GET", &path).await;
            assert_eq!(wire(&r), unrouted, "GET {path}");
        }
    }
    handle.abort();
}

/// The JSON of a response with its per-request fields removed: how long
/// since the counts moved, and how long the recall took.
fn stable_json(body: &str) -> serde_json::Value {
    let mut v: serde_json::Value = serde_json::from_str(body).expect("json");
    fn strip(v: &mut serde_json::Value) {
        if let Some(map) = v.as_object_mut() {
            map.remove("durable_change_age_ms");
            map.remove("elapsed_ms");
            for child in map.values_mut() {
                strip(child);
            }
        }
    }
    strip(&mut v);
    v
}

/// Q10: the unscoped routes are aliases for the default session. Every data
/// route answers the same as its `/s/{default}/` form (headers and body; the
/// only per-request fields are removed), and the page differs only by the
/// scoped page's `no-store` and `Referrer-Policy`.
#[tokio::test]
async fn the_aliases_equal_the_default_sessions_scoped_routes() {
    let store = two_sessions().await;
    let state = portal(
        backends_on(store),
        &["t4-a", "t4-b"],
        None,
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    for route in [
        "/api/session",
        "/api/inspect?focus=user%20schema",
        "/api/graph",
        "/api/recall?q=user%20schema",
        "/api/events",
        "/api/stats",
        "/api/pulse?since=0",
    ] {
        let alias = request(addr, "GET", route).await;
        let scoped = request(addr, "GET", &format!("/s/t4-a{route}")).await;
        assert_eq!(alias.status, 200, "{route}: {}", alias.body);
        assert_eq!(scoped.status, alias.status, "{route}");
        let headers = |r: &HttpResponse| {
            head_without_date(r)
                .lines()
                .filter(|l| !l.to_ascii_lowercase().starts_with("content-length:"))
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(headers(&scoped), headers(&alias), "{route}");
        assert_eq!(
            stable_json(&scoped.body),
            stable_json(&alias.body),
            "{route}"
        );
    }

    let alias = request(addr, "GET", "/").await;
    for path in ["/s/t4-a/"] {
        let page = request(addr, "GET", path).await;
        assert_eq!(page.status, 200, "{path}");
        assert_eq!(page.body, alias.body, "{path}: the same page");
        let extra: Vec<String> = head_without_date(&page)
            .lines()
            .filter(|l| !head_without_date(&alias).lines().any(|a| a == *l))
            .map(|l| l.to_ascii_lowercase())
            .collect();
        assert_eq!(
            extra,
            ["cache-control: no-store", "referrer-policy: same-origin"],
            "{path}"
        );
    }
    assert!(
        !head_without_date(&alias)
            .to_ascii_lowercase()
            .contains("referrer-policy"),
        "the unscoped page is unchanged"
    );
    handle.abort();
}

/// Review M1: the page's script fetches relative `api/...` URLs, so the
/// page at `/s/{b}/` reads session `b` and the page at `/` the default. No
/// fetch in the script names an absolute `/api` path, and every one it
/// makes, resolved against each page's URL, answers for that page's session.
#[tokio::test]
async fn the_scoped_page_reads_its_own_session() {
    assert!(
        !APP_JS.contains("\"/api") && !APP_JS.contains("'/api"),
        "the script must not fetch an absolute /api URL"
    );
    assert_eq!(
        APP_JS.matches("fetch(").count(),
        1,
        "one fetch, inside get(), so every request goes through the relative paths below"
    );
    // The script's own `get(path)`, not a method such as
    // `URLSearchParams.get("focus")`.
    let fetched: Vec<&str> = APP_JS
        .match_indices("get(\"")
        .filter(|(i, _)| !APP_JS[..*i].ends_with('.'))
        .filter_map(|(i, m)| APP_JS[i + m.len()..].split('"').next())
        .collect();
    assert!(fetched.len() >= 5, "{fetched:?}");
    for path in &fetched {
        assert!(path.starts_with("api/"), "relative API path: {path}");
    }

    let store = two_sessions().await;
    let state = portal(
        backends_on(store),
        &["t4-a", "t4-b"],
        None,
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    for (page, session) in [("/", "t4-a"), ("/s/t4-a/", "t4-a"), ("/s/t4-b/", "t4-b")] {
        assert_eq!(request(addr, "GET", page).await.status, 200, "{page}");
        // A page URL ends in `/`, so a relative path resolves by appending.
        for path in &fetched {
            let url = if path.ends_with('=') {
                format!("{page}{path}1")
            } else {
                format!("{page}{path}")
            };
            let r = request(addr, "GET", &url).await;
            assert_eq!(r.status, 200, "{page} fetches {url}: {}", r.body);
        }
        let info = get_json(addr, &format!("{page}api/session")).await;
        assert_eq!(info["session"], session, "{page}: {info}");
        let stats = get_json(addr, &format!("{page}api/stats")).await;
        assert_eq!(stats["session"], session, "{page}: {stats}");
    }
    handle.abort();
}

/// Review M1: `/s/{id}` without its slash would resolve the page's relative
/// URLs against `/s/`, so a `GET` or `HEAD` of it is a `308` to `/s/{id}/`
/// (query kept, with the page's `no-store` and `Referrer-Policy`); another
/// method is the page route's 405, as for `/s/{id}/`. A refused id is still
/// the uniform 404, never a redirect.
#[tokio::test]
async fn the_bare_scoped_page_redirects_to_its_slash() {
    let store = two_sessions().await;
    let state = portal(
        backends_on(store),
        &["t4-a", "t4-b"],
        None,
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    let location = |r: &HttpResponse| {
        r.headers
            .lines()
            .find_map(|l| {
                l.split_once(':')
                    .filter(|(k, _)| k.eq_ignore_ascii_case("location"))
                    .map(|(_, v)| v.trim().to_string())
            })
            .unwrap_or_default()
    };
    for method in ["GET", "HEAD"] {
        for (path, to) in [
            ("/s/t4-b", "/s/t4-b/"),
            ("/s/t4-a", "/s/t4-a/"),
            (
                "/s/t4-b?focus=billing%20ledger",
                "/s/t4-b/?focus=billing%20ledger",
            ),
        ] {
            let r = request(addr, method, path).await;
            assert_eq!(r.status, 308, "{method} {path}");
            assert_eq!(location(&r), to, "{method} {path}");
            let head = r.headers.to_ascii_lowercase();
            assert!(head.contains("cache-control: no-store"), "{head}");
            assert!(head.contains("referrer-policy: same-origin"), "{head}");
        }
    }
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let r = request(addr, method, "/s/t4-b").await;
        let slash = request(addr, method, "/s/t4-b/").await;
        assert_eq!(r.status, 405, "{method}");
        assert_eq!(wire(&r), wire(&slash), "{method}");
    }
    let refused = request(addr, "GET", "/s/t4-c").await;
    assert_eq!(refused.status, 404);
    assert!(location(&refused).is_empty());
    handle.abort();
}

/// A query string survives the rewrite: `?since=` pages the feed, `?q=`
/// reaches recall, `?focus=` reaches inspect.
#[tokio::test]
async fn the_query_string_reaches_the_scoped_route() {
    let store = two_sessions().await;
    let state = portal(
        backends_on(store),
        &["t4-a", "t4-b"],
        None,
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    let all = get_json(addr, "/s/t4-a/api/events?since=0").await;
    let total = all["total"].as_u64().expect("total");
    assert!(total > 0, "t4-a has a promotion: {all}");
    let tail = get_json(addr, &format!("/s/t4-a/api/events?since={total}")).await;
    assert_eq!(tail["events"].as_array().map(Vec::len), Some(0), "{tail}");
    let recall = get_json(addr, "/s/t4-b/api/recall?q=billing%20ledger").await;
    assert_eq!(recall["query"], "billing ledger");
    let inspect = get_json(addr, "/s/t4-b/api/inspect?focus=billing%20ledger").await;
    assert_eq!(inspect["found"], true, "{inspect}");
    handle.abort();
}

// ---- isolation ---------------------------------------------------------

/// What one session's routes must say, and must never say.
struct Expect {
    id: &'static str,
    concepts: u64,
    own: &'static str,
    other: &'static str,
}

/// Two sessions with different counts, events, concepts and embedding
/// contracts (`t4-b` written under another model), read by concurrent
/// clients alternating between them: every response names and reflects only
/// its own session; the mismatched one is recall-refused while the other
/// recalls; a third, unserved id is refused throughout.
#[tokio::test]
async fn two_sessions_stay_apart_under_concurrent_clients() {
    let store = seed("t4-a").await;
    let mut other_model = backends_on(store.clone());
    other_model.embedding.model = Some("t4-other-model".into());
    other_model.embedder_cfg.llama_model = other_model.embedding.model.clone();
    crate::cli::derive::run(
        other_model,
        crate::cli::derive::Args {
            session: "t4-b".into(),
            agent: "agent-b".into(),
            content: "billing ledger".into(),
            kind: ConceptKind::Entity,
            parent_of: vec![],
            concept: vec![],
        },
    )
    .await
    .expect("derive b");

    let state = portal(
        backends_on(store.clone()),
        &["t4-a", "t4-b"],
        None,
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    let a_concepts = get_json(addr, "/s/t4-a/api/stats").await["concepts"]
        .as_u64()
        .unwrap();
    let b_concepts = get_json(addr, "/s/t4-b/api/stats").await["concepts"]
        .as_u64()
        .unwrap();
    assert_ne!(a_concepts, b_concepts, "the fixture must tell them apart");
    let sessions = [
        Expect {
            id: "t4-a",
            concepts: a_concepts,
            own: "user schema",
            other: "billing ledger",
        },
        Expect {
            id: "t4-b",
            concepts: b_concepts,
            own: "billing ledger",
            other: "user schema",
        },
    ];

    let mut clients = Vec::new();
    for round in 0..8 {
        for expect in &sessions {
            let (id, concepts, own, other) = (expect.id, expect.concepts, expect.own, expect.other);
            clients.push(tokio::spawn(async move {
                let stats = get_json(addr, &format!("/s/{id}/api/stats")).await;
                assert_eq!(stats["session"], id, "round {round}");
                assert_eq!(stats["concepts"].as_u64(), Some(concepts), "{id}");
                let pulse = get_json(addr, &format!("/s/{id}/api/pulse?since=0")).await;
                assert_eq!(pulse["stats"]["session"], id);
                let graph = request(addr, "GET", &format!("/s/{id}/api/graph")).await;
                assert_eq!(graph.status, 200);
                assert!(
                    graph.body.contains(own) && !graph.body.contains(other),
                    "{id}"
                );
                let events = request(addr, "GET", &format!("/s/{id}/api/events")).await;
                assert!(!events.body.contains(other), "{id}: {}", events.body);
                let found = get_json(
                    addr,
                    &format!("/s/{id}/api/inspect?focus={}", own.replace(' ', "%20")),
                )
                .await;
                assert_eq!(found["found"], true, "{id}");
                let missing = get_json(
                    addr,
                    &format!("/s/{id}/api/inspect?focus={}", other.replace(' ', "%20")),
                )
                .await;
                assert_eq!(missing["found"], false, "{id} must not see {other}");
                let session = get_json(addr, &format!("/s/{id}/api/session")).await;
                assert_eq!(session["session"], id);
                let status = session["embedding_contract"]["status"].clone();
                let recall = request(
                    addr,
                    "GET",
                    &format!("/s/{id}/api/recall?q={}", own.replace(' ', "%20")),
                )
                .await;
                (id, status, recall)
            }));
        }
    }
    for client in clients {
        let (id, status, recall) = client.await.expect("client");
        if id == "t4-a" {
            assert_eq!(status, "compatible", "{id}");
            assert_eq!(recall.status, 200, "{id}: {}", recall.body);
            assert!(recall.body.contains("user schema"), "{}", recall.body);
            assert!(!recall.body.contains("billing ledger"), "{}", recall.body);
        } else {
            assert_eq!(status, "mismatch", "{id}");
            assert_eq!(recall.status, 502, "{id}: mismatched recall is refused");
            assert!(recall.body.contains("t4-other-model"), "{}", recall.body);
        }
    }
    let unserved = request(addr, "GET", "/s/t4-c/api/stats").await;
    assert_eq!(unserved.status, 404);
    handle.abort();
}

/// Each session keeps its own freshness: a write to one moves only its
/// `durable_change_age_ms` clock back, and only its counts.
#[tokio::test]
async fn freshness_and_counts_are_per_session() {
    let store = two_sessions().await;
    let state = portal(
        backends_on(store.clone()),
        &["t4-a", "t4-b"],
        None,
        &web_ttl_zero(),
    );
    let (addr, handle) = spawn(state).await;
    let a0 = get_json(addr, "/s/t4-a/api/stats").await;
    let b0 = get_json(addr, "/s/t4-b/api/stats").await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    crate::cli::derive::run(
        backends_on(store.clone()),
        crate::cli::derive::Args {
            session: "t4-b".into(),
            agent: "agent-b".into(),
            content: "invoice table".into(),
            kind: ConceptKind::Entity,
            parent_of: vec![],
            concept: vec![],
        },
    )
    .await
    .expect("derive b again");
    let a1 = get_json(addr, "/s/t4-a/api/stats").await;
    let b1 = get_json(addr, "/s/t4-b/api/stats").await;
    assert_eq!(a1["concepts"], a0["concepts"], "a did not change");
    assert!(
        b1["concepts"].as_u64() > b0["concepts"].as_u64(),
        "b grew: {b0} -> {b1}"
    );
    let age = |v: &serde_json::Value| v["durable_change_age_ms"].as_u64().unwrap();
    assert!(age(&a1) >= 30, "a keeps ageing: {a1}");
    assert!(age(&b1) < age(&a1), "b's clock reset alone: {a1} vs {b1}");
    handle.abort();
}

/// LRU at capacity over HTTP: with one view held, alternating sessions
/// reload each time, and a reloaded session answers exactly as before.
#[tokio::test]
async fn an_evicted_session_reloads_with_the_same_answers() {
    let recording = Recording::new(two_sessions().await);
    let web = WebConfig {
        max_loaded_sessions: Some(1),
        ..Default::default()
    };
    let state = portal(
        backends_with_store(Box::new(recording.clone())),
        &["t4-a", "t4-b"],
        None,
        &web,
    );
    let (addr, handle) = spawn(state.clone()).await;
    let a = stable_json(&request(addr, "GET", "/s/t4-a/api/graph").await.body);
    assert_eq!(recording.loads(), 1);
    let b = stable_json(&request(addr, "GET", "/s/t4-b/api/graph").await.body);
    assert_eq!(recording.loads(), 2);
    assert!(
        !state.views.is_loaded(&SessionId::new("t4-a")),
        "a was evicted"
    );
    let a_again = stable_json(&request(addr, "GET", "/s/t4-a/api/graph").await.body);
    assert_eq!(recording.loads(), 3, "a reloads");
    assert_eq!(a_again, a);
    assert_ne!(a, b);
    handle.abort();
}

/// An allowlisted session that was never written is an empty page (200),
/// as the default's alias always was (design 3.4, Q9), and is no oracle:
/// the same id outside the allowlist is the uniform 404.
#[tokio::test]
async fn an_allowlisted_empty_session_is_an_empty_200() {
    let store = two_sessions().await;
    let state = portal(
        backends_on(store),
        &["t4-a", "t4-empty"],
        None,
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    let stats = get_json(addr, "/s/t4-empty/api/stats").await;
    assert_eq!(stats["concepts"], 0);
    assert_eq!(stats["session"], "t4-empty");
    assert_eq!(request(addr, "GET", "/s/t4-b/api/stats").await.status, 404);
    handle.abort();
}

/// One loose-named session is served at the aliases, as before PR 2; no
/// scoped path can name it.
#[tokio::test]
async fn a_single_loose_session_is_served_at_the_aliases_only() {
    let store = seed("team notes/v1").await;
    let state = portal(
        backends_on(store),
        &["team notes/v1"],
        None,
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    let stats = get_json(addr, "/api/stats").await;
    assert_eq!(stats["session"], "team notes/v1");
    for path in [
        "/s/team%20notes%2Fv1/api/stats",
        "/s/team%20notes/v1/api/stats",
    ] {
        assert_eq!(request(addr, "GET", path).await.status, 404, "{path}");
    }
    handle.abort();
}

// ---- source pins -----------------------------------------------------------

/// The session is resolved before routing by exactly one construction: the
/// routes (under their gate) are the fallback service of an outer router
/// with the session resolution, the Host guard, and the security-header
/// layer outside both. No `any` route exists (it would answer every method
/// on a route the read-only sweep does not see), and the resolution is in
/// `scope.rs`.
#[test]
fn the_session_is_resolved_by_one_layer_before_routing() {
    let prod = production_source();
    assert_eq!(
        prod.matches(".fallback_service(").count(),
        1,
        "exactly one fallback service: the GET-only routes"
    );
    for line in prod.lines().filter(|l| l.contains("any(")) {
        assert!(
            line.contains(".any("),
            "no route answers every method (only iterator .any( calls): {line}"
        );
    }
    assert!(
        !prod.contains("routing::any"),
        "no route answers every method"
    );
    assert!(
        !prod.contains("route_service("),
        "no route bypasses the GET sweep"
    );
    assert!(
        !prod.contains("Path<"),
        "no extractor percent-decodes an id"
    );
    let router = router_source()
        .split("fn router(")
        .nth(1)
        .expect("fn router");
    let layers: Vec<_> = router.match_indices(".layer(").collect();
    assert_eq!(
        layers.len(),
        4,
        "the gate over the routes, resolve_session, the Host guard, and security headers"
    );
    let gate = router.find("state.clone(), gate)").expect("gate layer");
    let fallback = router.find(".fallback_service(").expect("fallback");
    let resolve = router
        .find("resolve_session")
        .expect("resolve_session layer");
    let host = router.find("state, host_guard)").expect("Host guard layer");
    let secure = router
        .find("from_fn(security_headers)")
        .expect("security headers layer");
    assert!(gate < fallback, "the gate is over the routes");
    assert!(
        fallback < resolve && resolve < host && host < secure,
        "source order is fallback, then resolve_session, then the Host guard, \
         then security headers (outermost, so they stamp the Host guard's early 403)"
    );
}

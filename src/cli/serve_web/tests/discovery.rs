//! Opt-in store-wide session discovery (#276): one test per constraint the
//! issue sets. Default off exposes nothing new; on, a session created after
//! startup is listed and served without a restart; erased sessions are
//! hidden and 404 like unknown ones; scoped credentials read no more than
//! without it; nothing loads at startup and the loaded-view cap holds; a
//! name outside the addressed-id charset is skipped.

use super::routing::{portal, two_sessions, wire, Recording, METHODS};
use super::*;
use crate::config::WebConfig;
use crate::surface::session::{
    parse_addressed, HostedSessions, SessionCapabilities, SessionGrant, SessionPrefix, SessionScope,
};

use super::super::discovery::{addressable, startup_notes, Discovery, DiscoveryFailure};

/// A bearer for credential `name`, built at runtime.
fn tok(name: &str) -> String {
    ["t276", "-", name, "-", "k", "e", "y"].concat()
}

/// `[web]` with discovery `on` and the listing switch `list`.
fn web(on: bool, list: bool) -> WebConfig {
    WebConfig {
        discover_sessions: on,
        list_sessions: list,
        // Every request sees the store as it is now.
        view_ttl_ms: Some(0),
        ..Default::default()
    }
}

/// One concept holding `content` in `session`, written as the CLI writes.
async fn derive_text(store: &Arc<MemoryStore>, session: &str, content: &str) {
    crate::cli::derive::run(
        backends_on(store.clone()),
        crate::cli::derive::Args {
            name: None,
            repo_scope: None,
            session: session.into(),
            agent: "agent-276".into(),
            content: content.into(),
            kind: ConceptKind::Entity,
            parent_of: vec![],
            concept: vec![],
            depends_on: Vec::new(),
            against: Vec::new(),
        },
    )
    .await
    .expect("derive");
}

/// Erase `session` as `lambo erase-session` does: a `lambo:erased` tombstone.
async fn erase(store: &dyn GraphStore, session: &str) {
    crate::cli::erase_session::run(
        store,
        crate::cli::erase_session::Args {
            session: session.into(),
            confirm: session.into(),
        },
    )
    .await
    .expect("erase");
}

/// `method path` presenting `token` (none: no header), `Host` loopback.
async fn as_caller(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> HttpResponse {
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    send_raw(
        addr,
        &format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Accept: application/json\r\n\
             Connection: close\r\n\r\n"
        ),
    )
    .await
}

/// `/api/sessions` as `token`: the names, after checking 200 and `no-store`.
async fn listing(addr: SocketAddr, token: Option<&str>) -> Vec<String> {
    let r = as_caller(addr, "GET", "/api/sessions", token).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        r.headers
            .to_ascii_lowercase()
            .contains("cache-control: no-store"),
        "{}",
        r.headers
    );
    let v: serde_json::Value = serde_json::from_str(&r.body).expect("json");
    v["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|s| s.as_str().expect("name").to_string())
        .collect()
}

/// `/api/session`'s `switchable` at `path` as `token`.
async fn switchable_at(addr: SocketAddr, path: &str, token: Option<&str>) -> bool {
    let r = as_caller(addr, "GET", path, token).await;
    assert_eq!(r.status, 200, "{path}: {}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).expect("json");
    v.get("switchable").is_some_and(|s| s == true)
}

/// A read credential: exact `names`, `"*"` when `star`, and an optional
/// `prefix`.
fn credential(name: &str, names: &[&str], star: bool, prefix: Option<&str>) -> WebCredential {
    credential_with(
        name,
        SessionScope::new(
            names.iter().map(|n| parse_addressed(n).expect("addressed")),
            star,
            prefix.map(|p| SessionPrefix::new(p).expect("prefix")),
        ),
    )
}

fn credential_with(name: &str, scope: SessionScope) -> WebCredential {
    WebCredential {
        grant: SessionGrant::new(name, scope, SessionCapabilities::default()),
        token: AuthToken::new(tok(name)).expect("token"),
    }
}

/// A portal over `served` (the first the default) on `recording`, with
/// `credentials` and `[web]` `web`. `exposed` stands for a non-loopback bind.
fn portal_with(
    recording: &Recording,
    served: &[&str],
    credentials: Vec<WebCredential>,
    exposed: bool,
    web: &WebConfig,
) -> Arc<AppState> {
    let ids: Vec<SessionId> = served.iter().map(|s| SessionId::new(*s)).collect();
    Arc::new(AppState::new(
        ids[0].clone(),
        ids,
        backends_with_store(Box::new(recording.clone())),
        exposed,
        None,
        credentials,
        &[],
        web,
    ))
}

// ---- 1. default off: nothing new --------------------------------------

/// Acceptance: with `discover_sessions` absent, behaviour is the strict
/// allowlist. A session in the store but not served, and one created after
/// startup, are the uniform 404 with no store call; the listing names the
/// allowlist only; the listing is not registered on loopback unless
/// `list_sessions` is; the page shows no picker.
#[tokio::test]
async fn discovery_off_by_default_exposes_nothing_new() {
    let store = two_sessions().await;
    assert!(!WebConfig::default().discover_sessions);

    // Off and no listing: the route does not exist, on loopback too.
    let recording = Recording::new(store.clone());
    let state = portal(
        backends_with_store(Box::new(recording.clone())),
        &["t4-a"],
        None,
        &WebConfig::default(),
    );
    assert!(!state.discovery.enabled() && !state.list_sessions);
    let (addr, handle) = spawn(state).await;
    let unrouted = wire(&request(addr, "GET", "/no/such/path").await);
    assert_eq!(wire(&request(addr, "GET", "/api/sessions").await), unrouted);
    handle.abort();

    // Off with the listing on: the allowlist only, before and after a new
    // session is written.
    let recording = Recording::new(store.clone());
    let state = portal(
        backends_with_store(Box::new(recording.clone())),
        &["t4-a"],
        None,
        &web(false, true),
    );
    let (addr, handle) = spawn(state).await;
    assert_eq!(listing(addr, None).await, ["t4-a"]);
    derive_text(&store, "t4-late", "late memory").await;
    assert_eq!(listing(addr, None).await, ["t4-a"]);
    assert!(!switchable_at(addr, "/api/session", None).await);
    let before = recording.calls();
    for method in METHODS {
        let unrouted = wire(&as_caller(addr, method, "/no/such/path", None).await);
        for id in ["t4-b", "t4-late"] {
            for rest in ["", "/", "/api/graph", "/api/pulse"] {
                let path = format!("/s/{id}{rest}");
                let r = as_caller(addr, method, &path, None).await;
                assert_eq!(wire(&r), unrouted, "{method} {path}");
            }
        }
    }
    assert_eq!(recording.calls(), before, "no store call off the allowlist");
    handle.abort();
}

// ---- 2. on: new sessions without a restart ------------------------------

/// Acceptance: with discovery on, a session written after `serve-web`
/// started is listed by `/api/sessions` and browsable at `/s/<session>/`
/// without a restart; the listing is automatic on loopback; the page offers
/// the picker.
#[tokio::test]
async fn discovery_lists_and_serves_a_session_created_after_startup() {
    let store = two_sessions().await;
    let recording = Recording::new(store.clone());
    // Loopback, no `list_sessions`: discovery registers the listing.
    let state = portal_with(&recording, &["t4-a"], vec![], false, &web(true, false));
    assert!(state.list_sessions);
    let (addr, handle) = spawn(state).await;

    assert_eq!(listing(addr, None).await, ["t4-a", "t4-b"]);
    assert_eq!(
        wire(&request(addr, "GET", "/s/t4-late/api/graph").await),
        wire(&request(addr, "GET", "/no/such/path").await),
        "not yet written: the uniform 404"
    );

    derive_text(&store, "t4-late", "late memory").await;
    assert_eq!(listing(addr, None).await, ["t4-a", "t4-b", "t4-late"]);
    let page = request(addr, "GET", "/s/t4-late").await;
    assert_eq!(page.status, 308, "{}", page.headers);
    assert_eq!(request(addr, "GET", "/s/t4-late/").await.status, 200);
    let graph = request(addr, "GET", "/s/t4-late/api/graph").await;
    assert_eq!(graph.status, 200, "{}", graph.body);
    assert!(graph.body.contains("late memory"), "{}", graph.body);
    // Served sessions are unchanged, and the alias still serves the default.
    let alias = request(addr, "GET", "/api/graph").await;
    assert!(alias.body.contains("user schema"), "{}", alias.body);
    assert!(switchable_at(addr, "/api/session", None).await);
    assert!(switchable_at(addr, "/s/t4-late/api/session", None).await);
    handle.abort();

    // Beyond loopback (a credential is then required) the listing still
    // needs `list_sessions`; the legacy-scoped `"*"` reads new sessions.
    let star = credential("star", &[], true, None);
    let recording = Recording::new(store.clone());
    let state = portal_with(
        &recording,
        &["t4-a"],
        vec![star.clone()],
        true,
        &web(true, false),
    );
    assert!(!state.list_sessions);
    let (addr, handle) = spawn(state).await;
    let token = tok("star");
    let unrouted = wire(&as_caller(addr, "GET", "/no/such/path", Some(&token)).await);
    assert_eq!(
        wire(&as_caller(addr, "GET", "/api/sessions", Some(&token)).await),
        unrouted
    );
    let graph = as_caller(addr, "GET", "/s/t4-late/api/graph", Some(&token)).await;
    assert_eq!(graph.status, 200, "{}", graph.body);
    assert_eq!(
        as_caller(addr, "GET", "/s/t4-late/api/graph", None)
            .await
            .status,
        401
    );
    handle.abort();

    let recording = Recording::new(store.clone());
    let state = portal_with(&recording, &["t4-a"], vec![star], true, &web(true, true));
    let (addr, handle) = spawn(state).await;
    assert_eq!(
        listing(addr, Some(&token)).await,
        ["t4-a", "t4-b", "t4-late"]
    );
    handle.abort();
}

// ---- 3. erased: hidden, and the same 404 ---------------------------------

/// Acceptance: an erased session (`lambo:erased`) is never listed, and every
/// path under it answers the bytes an unknown id and an unrouted path do,
/// on every method, even after its view was warm. The reader never creates
/// a session it probed.
#[tokio::test]
async fn erased_sessions_are_hidden_and_404_like_unknown_ones() {
    let store = two_sessions().await;
    derive_text(&store, "t4-gone", "doomed memory").await;
    let recording = Recording::new(store.clone());
    let state = portal_with(&recording, &["t4-a"], vec![], false, &web(true, false));
    let (addr, handle) = spawn(state).await;

    assert!(listing(addr, None).await.contains(&"t4-gone".to_string()));
    let warm = request(addr, "GET", "/s/t4-gone/api/graph").await;
    assert!(warm.body.contains("doomed memory"), "{}", warm.body);

    erase(store.as_ref(), "t4-gone").await;
    assert_eq!(
        listing(addr, None).await,
        ["t4-a", "t4-b"],
        "the next listing after an erase omits it"
    );
    for method in METHODS {
        let baseline = wire(&as_caller(addr, method, "/no/such/path", None).await);
        for path in [
            "/s/t4-gone",
            "/s/t4-gone/",
            "/s/t4-gone/api/graph",
            "/s/t4-gone/api/pulse",
            "/s/t4-never",
            "/s/t4-never/",
            "/s/t4-never/api/graph",
            "/s/t4%2Dgone/api/graph",
        ] {
            assert_eq!(
                wire(&as_caller(addr, method, path, None).await),
                baseline,
                "{method} {path}"
            );
        }
    }
    assert!(
        matches!(
            store.load_session(&SessionId::new("t4-never")).await,
            Err(StoreError::SessionNotFound(_))
        ),
        "the reader never creates a missing session"
    );
    handle.abort();
}

// ---- 4. credential boundaries ----------------------------------------------

/// Acceptance: discovery never widens a scoped credential. For an exact
/// name, a `session_prefix` and an inherited `[[serve.credential]]` `"*"`,
/// every probe and the listing answer exactly what they answer with
/// discovery off, and out-of-scope probes make no store call. A
/// `[[web.credential]]` `"*"` (the positive control) reads more.
#[tokio::test]
async fn scoped_credentials_read_no_more_with_discovery_on() {
    let store = two_sessions().await;
    derive_text(&store, "t4c-p-1", "prefix pantry").await;
    derive_text(&store, "secret-x", "secret orchard").await;
    let credentials = || {
        vec![
            credential("viewers", &["t4-a"], false, None),
            credential("pre", &[], false, Some("t4c-p-")),
            credential("star", &[], true, None),
            // What `[web] inherit_serve_credentials` builds from a
            // `[[serve.credential]]` `"*"` over `[serve] sessions = ["t4-b"]`.
            credential_with(
                "agents",
                SessionScope::new([], true, None).star_within(HostedSessions::new(
                    [parse_addressed("t4-b").expect("addressed")],
                    [],
                )),
            ),
        ]
    };
    let off_rec = Recording::new(store.clone());
    let on_rec = Recording::new(store.clone());
    let (off, off_handle) = spawn(portal_with(
        &off_rec,
        &["t4-a"],
        credentials(),
        true,
        &web(false, true),
    ))
    .await;
    let (on, on_handle) = spawn(portal_with(
        &on_rec,
        &["t4-a"],
        credentials(),
        true,
        &web(true, true),
    ))
    .await;
    // Written after both started.
    derive_text(&store, "t4-late", "late memory").await;
    derive_text(&store, "t4c-p-late", "late pantry").await;

    let ids = [
        "t4-a",
        "t4-b",
        "t4c-p-1",
        "t4c-p-late",
        "secret-x",
        "t4-late",
    ];
    for cred in ["viewers", "pre", "agents"] {
        let token = tok(cred);
        assert_eq!(
            listing(on, Some(&token)).await,
            listing(off, Some(&token)).await,
            "{cred}: the listing"
        );
        for id in ids {
            for rest in ["/", "/api/graph", "/api/session"] {
                let path = format!("/s/{id}{rest}");
                let a = as_caller(off, "GET", &path, Some(&token)).await;
                let b = as_caller(on, "GET", &path, Some(&token)).await;
                assert_eq!(a.status, b.status, "{cred} {path}");
                if a.status == 404 {
                    assert_eq!(wire(&a), wire(&b), "{cred} {path}");
                } else {
                    assert_eq!(a.body, b.body, "{cred} {path}");
                }
            }
        }
        // Out of scope with discovery on: refused in memory.
        let before = on_rec.calls();
        for id in ["secret-x", "t4-late", "t4-b"] {
            let r = as_caller(on, "GET", &format!("/s/{id}/api/graph"), Some(&token)).await;
            assert_eq!(r.status, 404, "{cred} {id}");
        }
        assert_eq!(on_rec.calls(), before, "{cred}: no store call out of scope");
    }

    // The positive control: `"*"` reads discovered sessions only with it on.
    let token = tok("star");
    for id in ["secret-x", "t4-late"] {
        let path = format!("/s/{id}/api/graph");
        assert_eq!(as_caller(off, "GET", &path, Some(&token)).await.status, 404);
        assert_eq!(as_caller(on, "GET", &path, Some(&token)).await.status, 200);
    }
    let listed = listing(on, Some(&token)).await;
    assert_eq!(listed.first().map(String::as_str), Some("t4-a"));
    for id in ids {
        assert!(listed.contains(&id.to_string()), "{id} in {listed:?}");
    }
    off_handle.abort();
    on_handle.abort();
}

/// Who discovery reaches, at the type: only a scope over every served
/// session. An inherited `"*"` (pinned to serve's hosted set) is not one.
#[test]
fn discovery_reaches_only_scopes_over_every_served_session() {
    let on = Discovery::new(true);
    let off = Discovery::new(false);
    let inherited = SessionScope::new([], true, None).star_within(HostedSessions::default());
    for (grant, reached) in [
        (SessionGrant::implicit_local(), true),
        (SessionGrant::legacy_default(), true),
        (credential("star", &["t4-a"], true, None).grant, true),
        (credential("viewers", &["t4-a"], false, None).grant, false),
        (credential("pre", &[], false, Some("t4-")).grant, false),
        (credential_with("agents", inherited).grant, false),
    ] {
        assert_eq!(on.reaches(&grant), reached, "{}", grant.name());
        assert!(!off.reaches(&grant), "{}: off reaches nobody", grant.name());
    }
}

// ---- 5. lazy and bounded --------------------------------------------------

/// Acceptance: discovery loads nothing at startup (no graph, no store-wide
/// read), a listing reads no graph, the view bounds are those without it,
/// and opening discovered sessions stays within `max_loaded_sessions`: the
/// least recently used view is dropped, and a full set of active dynamic
/// slots answers 503 rather than growing.
#[tokio::test]
async fn discovery_loads_nothing_at_startup_and_keeps_the_view_cap() {
    let store = two_sessions().await;
    for s in ["t4-c", "t4-d", "t4-e"] {
        derive_text(&store, s, "more memory").await;
    }
    let mut config = web(true, false);
    config.max_loaded_sessions = Some(1);
    let recording = Recording::new(store.clone());
    let state = portal_with(&recording, &["t4-a"], vec![], false, &config);
    let without = portal_with(
        &Recording::new(store.clone()),
        &["t4-a"],
        vec![],
        false,
        &WebConfig {
            discover_sessions: false,
            ..config.clone()
        },
    );
    assert_eq!(state.views.bounds(), without.views.bounds());
    let (addr, handle) = spawn(state.clone()).await;
    assert!(
        recording.calls().is_empty(),
        "startup reads nothing: {:?}",
        recording.calls()
    );

    let listed = listing(addr, None).await;
    assert_eq!(listed, ["t4-a", "t4-b", "t4-c", "t4-d", "t4-e"]);
    assert_eq!(
        recording.calls(),
        ["store_health"],
        "a listing loads no graph"
    );

    let all: Vec<SessionId> = listed.iter().map(|s| SessionId::new(s.as_str())).collect();
    for id in ["t4-b", "t4-c", "t4-d", "t4-e"] {
        let r = request(addr, "GET", &format!("/s/{id}/api/graph")).await;
        assert_eq!(r.status, 200, "{id}: {}", r.body);
        let loaded = all.iter().filter(|s| state.views.is_loaded(s)).count();
        assert!(loaded <= 1, "{id}: {loaded} views held, cap 1");
    }
    assert_eq!(recording.loads(), 4, "one load per session opened");

    // Every dynamic slot active: the next discovered session is refused
    // with 503, not admitted past the cap.
    let active = state
        .views
        .admit_dynamic(&SessionId::new("t4-e"))
        .expect("join the active slot");
    let busy = request(addr, "GET", "/s/t4-c/api/graph").await;
    assert_eq!(busy.status, 503);
    assert!(busy.headers.to_ascii_lowercase().contains("retry-after: 1"));
    drop(active);
    assert_eq!(request(addr, "GET", "/s/t4-c/api/graph").await.status, 200);
    handle.abort();
}

/// Each listing reads the store again (an erase must not linger in a
/// cache), and a store without `store_health` lists the served sessions
/// only, never failing the route and never listing more.
#[tokio::test]
async fn each_listing_reads_the_store_and_a_failed_read_lists_the_served_only() {
    let store = two_sessions().await;
    let recording = Recording::new(store.clone());
    let (addr, handle) = spawn(portal_with(
        &recording,
        &["t4-a"],
        vec![],
        false,
        &web(true, false),
    ))
    .await;
    listing(addr, None).await;
    listing(addr, None).await;
    let reads = recording
        .calls()
        .iter()
        .filter(|m| **m == "store_health")
        .count();
    assert_eq!(reads, 2);
    handle.abort();

    // `VectorSearch` does not forward `store_health`: the trait default,
    // a capability refusal.
    let unsupported = Discovery::new(true);
    let bare = VectorSearch(Shared(store.clone()));
    assert_eq!(
        unsupported.sessions(&bare).await.map(|s| s.len()),
        Err(DiscoveryFailure::Unsupported)
    );
    let state = Arc::new(AppState::new(
        SessionId::new("t4-a"),
        [],
        backends_with_store(Box::new(bare)),
        false,
        None,
        Vec::new(),
        &[],
        &web(true, false),
    ));
    let (addr, handle) = spawn(state).await;
    assert_eq!(listing(addr, None).await, ["t4-a"]);
    handle.abort();
}

// ---- 6. names outside the charset ---------------------------------------

/// Acceptance: a store session whose name fails `parse_addressed` is
/// skipped: not listed, not served, and no startup failure.
#[tokio::test]
async fn names_outside_the_addressed_charset_are_skipped() {
    let store = two_sessions().await;
    for loose in ["team notes", ".hidden", "a/b"] {
        derive_text(&store, loose, "loose memory").await;
    }
    let recording = Recording::new(store.clone());
    let (addr, handle) = spawn(portal_with(
        &recording,
        &["t4-a"],
        vec![],
        false,
        &web(true, false),
    ))
    .await;
    assert_eq!(listing(addr, None).await, ["t4-a", "t4-b"]);
    let unrouted = wire(&request(addr, "GET", "/no/such/path").await);
    let before = recording.calls();
    for path in [
        "/s/team%20notes/api/graph",
        "/s/.hidden/api/graph",
        "/s/a%2Fb/api/graph",
        "/s/a/b/api/graph",
    ] {
        assert_eq!(wire(&request(addr, "GET", path).await), unrouted, "{path}");
    }
    assert_eq!(recording.calls(), before, "malformed ids reach no store");
    handle.abort();

    let kept = addressable(
        ["ok-1", "", ".x", "a%2Fb", "has space", "ok:2"]
            .into_iter()
            .map(SessionId::new)
            .chain([SessionId::new("x".repeat(129))]),
    );
    let kept: Vec<&str> = kept.iter().map(|id| id.as_str()).collect();
    assert_eq!(kept, ["ok-1", "ok:2"]);
}

// ---- startup lines ----------------------------------------------------------

/// The startup notes name credentials, never tokens; say when discovery
/// reaches nobody; and warn on a non-loopback bind.
#[test]
fn startup_notes_name_who_discovery_reaches() {
    let notes = startup_notes(&["local"], false, true);
    assert!(notes[0].contains("'local'"), "{notes:?}");
    assert!(
        notes.iter().any(|n| n.contains("/api/sessions")),
        "{notes:?}"
    );
    assert!(
        !notes.iter().any(|n| n.contains("non-loopback")),
        "{notes:?}"
    );

    let notes = startup_notes(&[], true, false);
    assert!(notes[0].contains("widens nothing"), "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|n| n.contains("non-loopback") && n.contains("list_sessions")),
        "{notes:?}"
    );
    for note in startup_notes(&["star", "default"], true, false) {
        assert!(!note.contains(&tok("star")), "{note}");
    }
}

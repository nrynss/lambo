//! #32 PR 5 on the wire: the serve's router (`transport::http_app`, its
//! guard layer included) with configured credentials, over a registry whose
//! hosted sessions are in every slot state.
//!
//! * An out-of-scope caller gets a response byte-identical to a path the
//!   server does not route at all, for a live, detaching, held-elsewhere
//!   and failed session alike, and for unknown, malformed, percent-encoded
//!   and oversized ids, on every method (PR 4 review L6) — and the store
//!   sees **no call** while it does (design §6.2, a recording store).
//! * A caller in scope gets the 503s, so the states are real.
//! * A credential without `create` is refused on an absent session in its
//!   scope (the same 404) and served on an existing one.
//! * A wrong or missing token is the 401, identical for every id.

use super::pinned_serve::Shared;
use super::*;
use crate::config::ServeCredential;
use crate::mcp::serve::registry::ForcedState;
use crate::surface::session::{
    parse_addressed, SessionCapabilities, SessionGrant, SessionPrefix, SessionScope,
};
use crate::test_util::{on_the_wire_as, on_the_wire_with};

const LIVE: &str = "auth-live";
const DETACHING: &str = "auth-detaching";
const HELD: &str = "auth-held";
const FAILED: &str = "auth-failed";
const HOSTED: [&str; 4] = [LIVE, DETACHING, HELD, FAILED];

/// A fake token, built at runtime so no token-shaped literal sits in the
/// source.
fn token(label: &str) -> String {
    ["fake", label, "wire", "value"].join("-")
}

fn bearer(label: &str) -> String {
    format!("Bearer {}", token(label))
}

fn credential(
    name: &str,
    sessions: &[&str],
    prefix: Option<&str>,
    create: bool,
) -> ServeCredential {
    ServeCredential {
        grant: SessionGrant::new(
            name,
            SessionScope::new(
                sessions
                    .iter()
                    .map(|s| parse_addressed(s).expect("addressable")),
                false,
                prefix.map(|p| SessionPrefix::new(p).expect("a prefix")),
            ),
            SessionCapabilities {
                create,
                ..SessionCapabilities::default()
            },
        ),
        token: SecretToken::new(token(name)).expect("non-empty"),
    }
}

/// The router over a registry hosting [`HOSTED`]: `auth-live` attached,
/// the others forced into their states. Credentials:
///
/// | name | scope | create |
/// |---|---|---|
/// | `scoped` | `auth-live` | no |
/// | `ops` | every hosted session | no |
/// | `app` | prefix `auth-u-` | no |
/// | `maker` | prefix `auth-m-` | yes |
struct Wire {
    addr: SocketAddr,
    calls: Arc<super::pinned_serve::StoreCalls>,
    _registry: Arc<SessionRegistry>,
}

async fn wire() -> Wire {
    let mut opts = ServeOptions::new(LIVE, "agent-a");
    opts.sessions = HOSTED.iter().map(|s| s.to_string()).collect();
    opts.transport = Transport::Http;
    opts.credentials = vec![
        credential("scoped", &[LIVE], None, false),
        credential("ops", &HOSTED, None, false),
        credential("app", &[], Some("auth-u-"), false),
        credential("maker", &[], Some("auth-m-"), true),
    ];
    let authority = authority_for(&opts);

    let store = Arc::new(MemoryStore::new());
    let (recorded, calls) = Shared::recording(&store);
    // The `Host` check `serve_pinned` gives a credentialed serve.
    let registry = new_registry_with(
        &HOSTED,
        backends_over(recorded, fast_config(1_000)),
        8,
        HostCheck::for_authority(Some(&authority)),
    );
    attach_or_hold(&registry, LIVE).await;
    registry.force_state(DETACHING, ForcedState::Detaching);
    registry.force_state(HELD, ForcedState::HeldElsewhere);
    registry.force_state(FAILED, ForcedState::Failed);
    registry.mark_started();

    let addr = serve_app(guarded_app(Arc::clone(&registry), authority, 8)).await;
    Wire {
        addr,
        calls,
        _registry: registry,
    }
}

/// Every path a caller outside its scope may probe, with the unrouted
/// reference first.
fn probes() -> Vec<String> {
    let mut paths: Vec<String> = [
        "/not/routed",
        "/mcp/s/auth-detaching",
        "/mcp/s/auth-held",
        "/mcp/s/auth-failed",
        "/mcp/s/auth-unknown",
        // Inside another credential's prefix, so hosted for `"*"`.
        "/mcp/s/auth-u-someone",
        "/mcp/s/auth-m-someone",
        "/mcp/s/.x",
        "/mcp/s/a%2Fb",
        "/mcp/s/auth%2Dlive",
        "/mcp/s/",
        "/mcp/s/auth-live/",
    ]
    .map(str::to_string)
    .to_vec();
    paths.push(format!(
        "/mcp/s/{}",
        "a".repeat(crate::surface::session::MAX_ADDRESSED_LEN + 1)
    ));
    paths
}

/// PR 4 review L6 and design §6.2: a caller whose scope is `auth-live`
/// only cannot tell a detaching, held or failed session from one this
/// serve does not host, nor from a malformed or oversized id: every answer
/// is the unrouted 404, byte for byte, on every method, and the store sees
/// no call while those requests are answered. The `ops` credential, in
/// scope for all four, gets 503 for the three that are not serving, which
/// is what makes the 404s above a refusal and not an artefact of the
/// states.
///
/// Mutation: look the session up before authorizing (PR 4's order) and the
/// three states answer 503 here.
#[tokio::test]
async fn an_out_of_scope_caller_cannot_tell_any_slot_state_and_costs_no_store_call() {
    let wire = wire().await;
    let addr = wire.addr;
    assert!(
        wire.calls.len() > 0,
        "the recorder sees the startup attach, so a zero below is a real zero"
    );

    let scoped = bearer("scoped");
    let reference = on_the_wire_as(addr, "GET", "/not/routed", Some(&scoped)).await;
    assert!(
        reference.starts_with("HTTP/1.1 404 Not Found\r\n") && reference.ends_with("\r\n\r\n"),
        "{reference:?}"
    );

    // A live MCP session on `auth-live`, opened by `ops` before the window,
    // for the probes that name one.
    let (mcp_id, _) = initialize_as(addr, "/mcp/s/auth-live", Some(&bearer("ops"))).await;
    let live_mcp = [
        MCP_POST[0],
        MCP_POST[1],
        ("Mcp-Session-Id", mcp_id.as_str()),
    ];

    let before = wire.calls.len();
    for method in ["GET", "POST", "DELETE", "PUT"] {
        for path in probes() {
            assert_eq!(
                on_the_wire_as(addr, method, &path, Some(&scoped)).await,
                reference,
                "{method} {path} must be the unrouted 404 for a caller out of scope"
            );
        }
        // The other prefix credentials, for the pinned sessions outside
        // their scope, and for `/mcp` (the default session, `auth-live`,
        // is outside both prefixes).
        for other in ["app", "maker"] {
            for path in HOSTED
                .iter()
                .map(|id| format!("/mcp/s/{id}"))
                .chain(["/mcp".to_string()])
            {
                assert_eq!(
                    on_the_wire_as(addr, method, &path, Some(&bearer(other))).await,
                    reference,
                    "{other}: {method} {path}"
                );
            }
        }
    }
    // The shapes most likely to reach rmcp or the store if the order
    // regressed (#32 PR 5 review L5): an `initialize` POST with a real
    // JSON-RPC body, and requests naming a live MCP session, each against
    // its own unrouted reference with the same headers and body.
    for (who, path, headers, body) in [
        ("scoped", "/mcp/s/auth-held", &MCP_POST[..], INITIALIZE),
        ("app", "/mcp", &MCP_POST[..], INITIALIZE),
        ("app", "/mcp/s/auth-live", &live_mcp[..], STATS_CALL),
        ("scoped", "/mcp/s/auth-detaching", &live_mcp[..], STATS_CALL),
        ("maker", "/mcp/s/auth-live", &live_mcp[..], ""),
    ] {
        for method in ["POST", "GET", "DELETE"] {
            let auth = bearer(who);
            let unrouted = on_the_wire_with(
                addr,
                method,
                "/not/routed",
                "localhost",
                Some(&auth),
                headers,
                body,
            )
            .await;
            assert!(
                unrouted.starts_with("HTTP/1.1 404 Not Found\r\n"),
                "{unrouted}"
            );
            assert_eq!(
                on_the_wire_with(addr, method, path, "localhost", Some(&auth), headers, body).await,
                unrouted,
                "{who}: {method} {path} with {headers:?}"
            );
        }
    }
    // A live session's own background (its lease heartbeat) may tick in
    // the window; nothing a refused request does may reach the store, so
    // only that session's own `refresh_lease` is exempt.
    let during: Vec<_> = wire
        .calls
        .since(before)
        .into_iter()
        .filter(|(method, session)| !(*method == "refresh_lease" && session == LIVE))
        .collect();
    assert!(
        during.is_empty(),
        "refused requests reached the store: {during:?}"
    );

    // In scope, the states answer for themselves.
    let ops = bearer("ops");
    for id in [DETACHING, HELD, FAILED] {
        let reply = on_the_wire_as(addr, "GET", &format!("/mcp/s/{id}"), Some(&ops)).await;
        assert!(
            reply.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
            "ops on {id}: {reply}"
        );
    }
}

/// The 401 comes first and alone (§6.2 step 1): a wrong or missing token
/// gets the same bytes whatever the id on the session routes, hosted or
/// not, well formed or not, and the same status and body on an unrouted
/// path. (On an unrouted path the server orders `content-length` and
/// `connection` differently, as it did before PR 5: that says a path is
/// routed, which the routes are public anyway, never whether a session
/// exists.)
#[tokio::test]
async fn a_wrong_or_missing_token_is_the_same_401_for_every_id() {
    let wire = wire().await;
    let addr = wire.addr;
    let wrong = bearer("nobody");
    let reference = on_the_wire_as(addr, "GET", "/mcp/s/auth-live", Some(&wrong)).await;
    assert!(
        reference.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{reference}"
    );
    let unrouted = ["/not/routed", "/mcp/s/", "/mcp/s/auth-live/"];
    let status_and_body = |raw: &str| {
        let (head, body) = raw.split_once("\r\n\r\n").expect("a head");
        (head.lines().next().map(str::to_string), body.to_string())
    };
    for auth in [Some(wrong.as_str()), None, Some("Basic abc")] {
        for path in probes()
            .into_iter()
            .chain(["/mcp".to_string(), "/mcp/s/auth-live".to_string()])
        {
            let reply = on_the_wire_as(addr, "GET", &path, auth).await;
            if unrouted.contains(&path.as_str()) {
                assert_eq!(
                    status_and_body(&reply),
                    status_and_body(&reference),
                    "{auth:?} {path}"
                );
            } else {
                assert_eq!(reply, reference, "{auth:?} {path}");
            }
        }
    }
}

/// Design §8 PR 5: a credential without `create` is refused on an absent
/// session inside its scope with the same 404, and served on an existing
/// one. This registry attaches nothing on demand (PR 4's pinned-only
/// bounds), so a credential *with* `create` gets the 404 for an absent id
/// too, with no store call; the on-demand answers are in
/// `registry::on_demand` (#32 PR 6).
#[tokio::test]
async fn a_create_less_credential_is_refused_on_an_absent_session_and_served_on_an_existing_one() {
    let wire = wire().await;
    let addr = wire.addr;
    let reference = on_the_wire_as(addr, "GET", "/not/routed", Some(&bearer("app"))).await;
    for (who, absent) in [("app", "auth-u-absent"), ("maker", "auth-m-absent")] {
        let before = wire.calls.len();
        assert_eq!(
            on_the_wire_as(
                addr,
                "POST",
                &format!("/mcp/s/{absent}"),
                Some(&bearer(who))
            )
            .await,
            reference,
            "{who}: an absent session in scope"
        );
        let during: Vec<_> = wire
            .calls
            .since(before)
            .into_iter()
            .filter(|(method, session)| !(*method == "refresh_lease" && session == LIVE))
            .collect();
        assert!(during.is_empty(), "{who}: {during:?}");
    }

    for who in ["scoped", "ops"] {
        let (_, info) = initialize_as(addr, "/mcp/s/auth-live", Some(&bearer(who))).await;
        assert!(
            info["instructions"]
                .as_str()
                .is_some_and(|i| i.contains(LIVE)),
            "{who} reaches {LIVE}: {info}"
        );
    }
    // `/mcp` is the default session, authorized like its addressed route.
    initialize_as(addr, "/mcp", Some(&bearer("scoped"))).await;
    assert_eq!(
        on_the_wire_as(addr, "POST", "/mcp", Some(&bearer("app"))).await,
        reference,
        "the default session is outside app's prefix"
    );
}

/// The `initialize` body the `Host` tests send.
const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"host-test","version":"1"}}}"#;

/// The headers a streamable-HTTP `initialize` POST carries.
const MCP_POST: [(&str, &str); 2] = [
    ("Accept", "application/json, text/event-stream"),
    ("Content-Type", "application/json"),
];

/// #32 PR 5 review M1: a serve whose every request presents a bearer token
/// answers a request whose `Host` is not loopback, which is how a client
/// reaches a serve bound beyond loopback (`Host: 10.0.0.5:7700`). Before
/// the fix rmcp's default allow-list answered 403 after auth and scope had
/// both passed. Scope still decides first: `app` is refused with the
/// uniform 404 whatever the `Host`.
#[tokio::test]
async fn a_credentialed_serve_answers_a_non_loopback_host() {
    let wire = wire().await;
    let addr = wire.addr;
    // Two credentials, so no one of them reaches its share of the cap.
    for (host, who) in [
        ("10.0.0.5:7700", "scoped"),
        ("lambo.internal", "ops"),
        ("localhost", "scoped"),
    ] {
        let reply = on_the_wire_with(
            addr,
            "POST",
            "/mcp/s/auth-live",
            host,
            Some(&bearer(who)),
            &MCP_POST,
            INITIALIZE,
        )
        .await;
        assert!(
            reply.starts_with("HTTP/1.1 200 OK\r\n"),
            "Host {host}: {reply}"
        );
        assert!(
            reply.to_ascii_lowercase().contains("mcp-session-id:"),
            "Host {host} opens an MCP session: {reply}"
        );
    }
    let reference = on_the_wire_as(addr, "GET", "/not/routed", Some(&bearer("app"))).await;
    assert_eq!(
        on_the_wire_with(
            addr,
            "GET",
            "/mcp/s/auth-live",
            "10.0.0.5:7700",
            Some(&bearer("app")),
            &[],
            ""
        )
        .await,
        reference
    );
}

/// The other half of M1: a serve with no credential (the implicit `local`
/// grant on loopback, where no request authenticates) keeps rmcp's
/// loopback `Host` allow-list, so a page that re-points its own name at
/// the serve (DNS rebinding) is still refused, while `localhost` and the
/// loopback addresses are served.
#[tokio::test]
async fn the_implicit_local_serve_still_refuses_a_foreign_host() {
    let registry = pinned_registry(
        &["host-local"],
        backends_over(Box::new(MemoryStore::new()), fast_config(1_000)),
        8,
    )
    .await;
    let addr = serve_router(&registry, 8).await;
    for host in ["evil.example:7700", "10.0.0.5:7700"] {
        let reply = on_the_wire_with(addr, "POST", "/mcp", host, None, &MCP_POST, INITIALIZE).await;
        assert!(
            reply.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "Host {host}: {reply}"
        );
    }
    for host in ["localhost", "127.0.0.1:7700", "[::1]:7700"] {
        let reply = on_the_wire_with(addr, "POST", "/mcp", host, None, &MCP_POST, INITIALIZE).await;
        assert!(
            reply.starts_with("HTTP/1.1 200 OK\r\n"),
            "Host {host}: {reply}"
        );
    }
}

/// One raw exchange on `path` presenting `authorization` and naming the MCP
/// session `mcp_id`.
async fn naming_mcp_session(
    addr: SocketAddr,
    method: &str,
    path: &str,
    authorization: &str,
    mcp_id: &str,
    body: &str,
) -> String {
    on_the_wire_with(
        addr,
        method,
        path,
        "localhost",
        Some(authorization),
        &[MCP_POST[0], MCP_POST[1], ("Mcp-Session-Id", mcp_id)],
        body,
    )
    .await
}

/// A `tools/call` the MCP-session tests send.
const STATS_CALL: &str = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"lambo_stats","arguments":{"agent_id":"auth-test"}}}"#;

/// #32 PR 5 review L1: an MCP session is bound to the credential that
/// opened it. `ops` is in scope for `auth-live` too, but naming the MCP
/// session `scoped` opened gets exactly what naming an id rmcp never minted
/// gets, on every method, so a foreign MCP session is indistinguishable
/// from an expired one, and the `DELETE` closes nothing: `scoped` keeps
/// calling on it afterwards.
///
/// Mutation: hand the request to rmcp with the foreign id unchanged and
/// `ops`'s `POST` is a 200 with a result.
#[tokio::test]
async fn an_mcp_session_answers_only_the_credential_that_opened_it() {
    let wire = wire().await;
    let addr = wire.addr;
    let path = "/mcp/s/auth-live";
    let (mine, _) = initialize_as(addr, path, Some(&bearer("scoped"))).await;
    // Never minted: rmcp's ids are UUIDv4 strings, this is version 4 too.
    let unknown = "00000000-0000-4000-8000-000000000000";
    let ops = bearer("ops");
    for (method, body) in [("POST", STATS_CALL), ("GET", ""), ("DELETE", "")] {
        let foreign = naming_mcp_session(addr, method, path, &ops, &mine, body).await;
        let expired = naming_mcp_session(addr, method, path, &ops, unknown, body).await;
        assert_eq!(foreign, expired, "{method}: a foreign MCP session id");
        if method != "DELETE" {
            assert!(
                foreign.starts_with("HTTP/1.1 404 Not Found\r\n"),
                "{method}: {foreign}"
            );
        }
    }
    // The opener still holds its MCP session: the foreign DELETE closed
    // nothing.
    let reply = http_as(
        addr,
        "POST",
        path,
        Some(&bearer("scoped")),
        Some(&mine),
        STATS_CALL,
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.message().get("result").is_some(), "{}", reply.body);
}

/// #32 PR 5 review M2 through the real registry: four credentials share a
/// cap of 8, so each may hold 2 MCP sessions, counted across every attached
/// session by the credential that opened them. `scoped` at its share is
/// refused while `ops` still opens one, and closing one of `scoped`'s gives
/// it room again.
#[tokio::test]
async fn a_credential_at_its_share_does_not_lock_out_another() {
    let wire = wire().await;
    let addr = wire.addr;
    let path = "/mcp/s/auth-live";
    let scoped = bearer("scoped");
    let (first, _) = initialize_as(addr, path, Some(&scoped)).await;
    // Through `/mcp`, the same Lambo session: still `scoped`'s count.
    initialize_as(addr, "/mcp", Some(&scoped)).await;
    let refused = http_as(addr, "POST", path, Some(&scoped), None, INITIALIZE).await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert!(refused.body.contains("2/2 of 8"), "{}", refused.body);

    initialize_as(addr, path, Some(&bearer("ops"))).await;

    let closed = http_as(addr, "DELETE", path, Some(&scoped), Some(&first), "").await;
    assert!(closed.status < 300, "{} {}", closed.status, closed.body);
    initialize_as(addr, path, Some(&scoped)).await;
}

/// One raw exchange whose head is given as bytes (a header value that is
/// not UTF-8), the date line dropped.
async fn raw_exchange(addr: SocketAddr, request: &[u8]) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream.write_all(request).await.expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    String::from_utf8_lossy(&raw).into_owned()
}

/// An `initialize` POST on `path` as `authorization`, with `extra` header
/// bytes (each line ending in CRLF).
fn initialize_head(path: &str, authorization: &str, extra: &[u8]) -> Vec<u8> {
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Authorization: {authorization}\r\nAccept: application/json, text/event-stream\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n",
        INITIALIZE.len()
    )
    .into_bytes();
    head.extend_from_slice(extra);
    head.extend_from_slice(b"\r\n");
    head.extend_from_slice(INITIALIZE.as_bytes());
    head
}

/// #32 PR 5 review S1 through the real registry and rmcp: an `initialize`
/// whose `Mcp-Session-Id` rmcp cannot read (byte 0xFF) makes rmcp mint an
/// MCP session, and that session is now the caller's: it counts toward
/// `scoped`'s share of 2, so after one more ordinary `initialize` the next
/// is refused. At the share, an `initialize` carrying `Last-Event-ID` (which
/// rmcp ignores on a POST) is refused too.
///
/// Mutation: test the header for presence in the guard or in
/// `serve_live` (the old reading) and the third `initialize` is a 200.
#[tokio::test]
async fn an_initialize_with_an_id_rmcp_cannot_read_is_counted_and_attributed() {
    let wire = wire().await;
    let addr = wire.addr;
    let path = "/mcp/s/auth-live";
    let scoped = bearer("scoped");
    let reply = raw_exchange(
        addr,
        &initialize_head(path, &scoped, b"Mcp-Session-Id: \xff\r\n"),
    )
    .await;
    assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply}");
    assert!(
        reply.to_ascii_lowercase().contains("mcp-session-id: "),
        "rmcp minted an MCP session: {reply}"
    );

    initialize_as(addr, path, Some(&scoped)).await;
    let refused = http_as(addr, "POST", path, Some(&scoped), None, INITIALIZE).await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert!(refused.body.contains("2/2 of 8"), "{}", refused.body);

    for extra in [
        &b"Last-Event-ID: 1\r\n"[..],
        &b"Mcp-Session-Id: \xff\r\n"[..],
    ] {
        let reply = raw_exchange(addr, &initialize_head(path, &scoped, extra)).await;
        assert!(
            reply.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
            "{reply}"
        );
        assert!(reply.contains("2/2 of 8"), "{reply}");
    }
    // Another credential still opens one.
    initialize_as(addr, path, Some(&bearer("ops"))).await;
}

/// #32 PR 5 review S1: the opener is recorded even when the request that
/// minted the MCP session is dropped (a client disconnect drops axum's
/// handler future) after rmcp created the session and before the response
/// was written. `serve_live` is polled once, which on this current-thread
/// runtime gets rmcp as far as minting the session and waiting on its
/// worker, and then dropped.
///
/// Mutation: record the opener after `handle` returns (not at the mint, in
/// `openers::AttributingSessions::create_session`) and the session stays
/// live but owned by no one.
#[tokio::test]
async fn an_mcp_session_whose_initialize_was_dropped_is_still_attributed() {
    use std::future::Future;
    let wire = wire().await;
    let crate::mcp::serve::registry::Lookup::Live(session) = wire._registry.lookup(LIVE) else {
        panic!("{LIVE} is attached");
    };
    let grant = credential("scoped", &[LIVE], None, false).grant;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp/s/auth-live")
        .header("Host", "localhost")
        .header(MCP_POST[0].0, MCP_POST[0].1)
        .header(MCP_POST[1].0, MCP_POST[1].1)
        .body(axum::body::Body::from(INITIALIZE))
        .expect("request");
    {
        let mut call = Box::pin(crate::mcp::serve::transport::serve_live(
            &session, &grant, req,
        ));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            call.as_mut().poll(&mut cx).is_pending(),
            "the initialize must still be in flight when the client goes"
        );
        // The client disconnects: axum drops the handler's future.
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let live = session.live_mcp_sessions().await;
        let owned = session.live_mcp_sessions_opened_by("scoped").await;
        if live == 1 && owned == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the minted MCP session must be attributed: {live} live, {owned} owned by scoped"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// #32 PR 5 review I6: a ledgered call line names the configured
/// credential the call arrived as (`credential`, its name, never a token),
/// and a call as the legacy `default` writes the line it always wrote, with
/// no such field.
#[tokio::test]
async fn a_call_line_names_its_configured_credential_and_default_none() {
    let dir = crate::test_util::ScratchDir::new("lambo-pr5-ledger");
    let path = dir.join("calls.jsonl");
    let ledger = crate::ledger::Ledger::open(path.clone());

    let mut opts = ServeOptions::new(LIVE, "agent-a");
    opts.sessions = vec![LIVE.to_string()];
    opts.transport = Transport::Http;
    opts.auth_token = Some(SecretToken::new(token("legacy")).expect("non-empty"));
    opts.credentials = vec![credential("scoped", &[LIVE], None, false)];
    let authority = authority_for(&opts);
    let registry = new_registry_ledgered(
        &[LIVE],
        backends_over(Box::new(MemoryStore::new()), fast_config(1_000)),
        8,
        HostCheck::for_authority(Some(&authority)),
        Some(Arc::clone(&ledger)),
    );
    attach_or_hold(&registry, LIVE).await;
    registry.mark_started();
    let addr = serve_app(guarded_app(Arc::clone(&registry), authority, 8)).await;

    for who in ["scoped", "legacy"] {
        let auth = bearer(who);
        let (mcp_id, _) = initialize_as(addr, "/mcp", Some(&auth)).await;
        let reply = http_as(addr, "POST", "/mcp", Some(&auth), Some(&mcp_id), STATS_CALL).await;
        assert_eq!(reply.status, 200, "{who}: {}", reply.body);
    }
    ledger.shutdown();

    let text = std::fs::read_to_string(&path).expect("the ledger");
    let calls: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("a JSON line"))
        .filter(|l: &serde_json::Value| l["kind"] == "call")
        .collect();
    assert_eq!(calls.len(), 2, "{text}");
    assert_eq!(calls[0]["credential"], "scoped", "{text}");
    assert!(calls[1].get("credential").is_none(), "{text}");
    assert!(
        !text.contains("fake-"),
        "a token reached the ledger: {text}"
    );
}

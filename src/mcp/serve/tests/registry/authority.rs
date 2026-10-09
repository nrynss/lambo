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
use crate::test_util::on_the_wire_as;

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
    let store = Arc::new(MemoryStore::new());
    let (recorded, calls) = Shared::recording(&store);
    let registry = new_registry(&HOSTED, backends_over(recorded, fast_config(1_000)), 8);
    attach_or_hold(&registry, LIVE).await;
    registry.force_state(DETACHING, ForcedState::Detaching);
    registry.force_state(HELD, ForcedState::HeldElsewhere);
    registry.force_state(FAILED, ForcedState::Failed);
    registry.mark_started();

    let mut opts = ServeOptions::new(LIVE, "agent-a");
    opts.sessions = HOSTED.iter().map(|s| s.to_string()).collect();
    opts.transport = Transport::Http;
    opts.credentials = vec![
        credential("scoped", &[LIVE], None, false),
        credential("ops", &HOSTED, None, false),
        credential("app", &[], Some("auth-u-"), false),
        credential("maker", &[], Some("auth-m-"), true),
    ];
    let addr = serve_app(guarded_app(Arc::clone(&registry), authority_for(&opts), 8)).await;
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
        // their scope.
        for other in ["app", "maker"] {
            for id in HOSTED {
                assert_eq!(
                    on_the_wire_as(addr, method, &format!("/mcp/s/{id}"), Some(&bearer(other)))
                        .await,
                    reference,
                    "{other}: {method} {id}"
                );
            }
        }
    }
    // A live session's own background (its lease heartbeat) may tick in
    // the window; nothing a refused request does may reach the store.
    let during: Vec<_> = wire
        .calls
        .since(before)
        .into_iter()
        .filter(|(method, _)| *method != "refresh_lease")
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
/// one. Until PR 6 nothing attaches on demand, so a credential *with*
/// `create` gets the 404 for an absent id too (pinned sessions only).
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
            .filter(|(method, _)| *method != "refresh_lease")
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

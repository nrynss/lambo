//! #32 PR 7: the admin surface and the in-serve erase (design §6.3), on the
//! wire, through the serve's own router and guards over a registry of two
//! pinned sessions on one recording store.
//!
//! * Erasing an attached session: the post-erase census is zero rows of
//!   every kind except the tombstone; a flush parked in flight when the
//!   erase began never lands; the lease is never released (no gap another
//!   writer could take); the slot is `Erased`; a later request is refused
//!   (410) and nothing recreates the session; the other session keeps
//!   serving; the erased handle is dropped; a repeat is `already_absent`.
//! * Erasing a session this process does not hold (every table kind
//!   planted, an image intent included): census zero but the tombstone.
//! * A live holder elsewhere: 409, nothing touched. A confirm that does not
//!   repeat the id, or a malformed body: 400, nothing touched.
//! * A credential without `erase`, or with the session out of its scope,
//!   gets the unrouted 404 byte for byte with no store call; so does
//!   `/admin/sessions` without `admin`, which lists only the caller's scope.
//! * The MCP tool list is the same for every credential and has no erase.

use super::pinned_serve::{Shared, StoreCalls};
use super::*;
use crate::config::ServeCredential;
use crate::mcp::serve::registry::ForcedState;
use crate::store::erase::testkit::planted_batch;
use crate::store::lease::{LeaseHolder, LeaseOutcome};
use crate::surface::session::{
    parse_addressed, SessionCapabilities, SessionGrant, SessionPrefix, SessionScope,
};
use crate::test_util::{on_the_wire_as, on_the_wire_with};
use crate::types::SessionId;

const A: &str = "er-a";
const B: &str = "er-b";
const HELD: &str = "er-held";
const HOSTED: [&str; 3] = [A, B, HELD];
/// The MCP-session cap: room for each of the six credentials' share to
/// hold a few MCP sessions (#32 PR 5 review M2).
const CAP: usize = 36;
/// The prefix the `app` credential erases users under.
const USERS: &str = "er-u-";

fn token(label: &str) -> String {
    ["fake", label, "erase", "value"].join("-")
}

fn bearer(label: &str) -> String {
    format!("Bearer {}", token(label))
}

/// A credential over exact `sessions` (or every hosted one with `"*"`),
/// and/or a prefix, with the flags given.
fn credential(
    name: &str,
    sessions: &[&str],
    prefix: Option<&str>,
    caps: SessionCapabilities,
) -> ServeCredential {
    let every = sessions.contains(&"*");
    ServeCredential {
        grant: SessionGrant::new(
            name,
            SessionScope::new(
                sessions
                    .iter()
                    .filter(|s| **s != "*")
                    .map(|s| parse_addressed(s).expect("addressable")),
                every,
                prefix.map(|p| SessionPrefix::new(p).expect("a prefix")),
            ),
            caps,
        ),
        token: SecretToken::new(token(name)).expect("non-empty"),
    }
}

fn erase_cap() -> SessionCapabilities {
    SessionCapabilities {
        erase: true,
        ..SessionCapabilities::default()
    }
}

fn admin_cap() -> SessionCapabilities {
    SessionCapabilities {
        admin: true,
        ..SessionCapabilities::default()
    }
}

/// A config whose write-behind flush runs every 50 ms, so a test can hold
/// one in flight with the store's flush gate.
fn flushing_config() -> crate::Config {
    crate::Config {
        backend_flush_interval: Duration::from_millis(50),
        ..fast_config(1_000)
    }
}

/// The router over a registry hosting [`HOSTED`]: `er-a` and `er-b`
/// attached, `er-held` held by another writer. Credentials:
///
/// | name | scope | flags |
/// |---|---|---|
/// | `agent` | `er-a`, `er-b` | none |
/// | `ops` | `er-a`, `er-b`, `er-held` | `erase` |
/// | `other` | `er-b` | `erase` |
/// | `app` | prefix `er-u-` | `erase` |
/// | `root` | `"*"` | `admin` |
/// | `boss-b` | `er-b` | `admin` |
struct Wire {
    addr: SocketAddr,
    store: Arc<MemoryStore>,
    calls: Arc<StoreCalls>,
    registry: Arc<SessionRegistry>,
}

async fn wire() -> Wire {
    wire_stalling(Default::default()).await
}

/// [`wire`], whose store parks every `load_session` while `stall` is set
/// (an attach that has taken its lease and is loading).
async fn wire_stalling(stall: Arc<std::sync::atomic::AtomicBool>) -> Wire {
    let mut opts = ServeOptions::new(A, "agent-a");
    opts.sessions = HOSTED.iter().map(|s| s.to_string()).collect();
    opts.transport = Transport::Http;
    opts.credentials = vec![
        credential("agent", &[A, B], None, SessionCapabilities::default()),
        credential("ops", &HOSTED, None, erase_cap()),
        credential("other", &[B], None, erase_cap()),
        credential("app", &[], Some(USERS), erase_cap()),
        credential("root", &["*"], None, admin_cap()),
        credential("boss-b", &[B], None, admin_cap()),
    ];
    let authority = authority_for(&opts);

    let store = Arc::new(MemoryStore::new());
    // `er-held` is another process's: a live lease before the serve starts.
    let LeaseOutcome::Acquired(_) = store
        .acquire_lease(
            &SessionId::new(HELD),
            &elsewhere(),
            crate::store::lease::LEASE_TTL,
        )
        .await
        .expect("acquire")
    else {
        panic!("the other writer takes er-held");
    };
    let (recorded, calls) = Shared::recording_with_stall(&store, stall);
    let registry = new_registry_with(
        &HOSTED,
        backends_over(recorded, flushing_config()),
        CAP,
        HostCheck::for_authority(Some(&authority)),
    );
    for id in HOSTED {
        attach_or_hold(&registry, id).await;
    }
    registry.mark_started();
    let addr = serve_app(guarded_app(Arc::clone(&registry), authority, CAP)).await;
    Wire {
        addr,
        store,
        calls,
        registry,
    }
}

/// Another process's lease holder.
fn elsewhere() -> LeaseHolder {
    LeaseHolder {
        agent: crate::types::AgentId::new("someone-else"),
        pid: 1,
        host: "elsewhere".into(),
        endpoint: None,
    }
}

/// `POST /admin/s/{id}/erase` as `cred` with `body`.
async fn erase_as(addr: SocketAddr, cred: &str, id: &str, body: &str) -> Reply {
    http_as(
        addr,
        "POST",
        &format!("/admin/s/{id}/erase"),
        Some(&bearer(cred)),
        None,
        body,
    )
    .await
}

fn confirm(id: &str) -> String {
    serde_json::json!({ "confirm": id }).to_string()
}

/// Rows of every kind the store keeps for `id`, without the lease row.
fn census_but_lease(store: &MemoryStore, id: &str) -> Vec<(&'static str, usize)> {
    store
        .erase_census(&SessionId::new(id))
        .into_iter()
        .filter(|(kind, _)| *kind != "session_leases")
        .collect()
}

/// Assert `id` is erased: no row of any kind but the lease row, which is
/// the tombstone.
async fn assert_only_the_tombstone(store: &MemoryStore, id: &str) {
    for (kind, n) in census_but_lease(store, id) {
        assert_eq!(n, 0, "{id}: {kind} rows remain after the erase");
    }
    let lease = store
        .read_lease(&SessionId::new(id))
        .await
        .expect("read_lease")
        .expect("the tombstone row stays");
    assert!(
        crate::store::erase::is_tombstone(&lease),
        "{id}: the lease row is the tombstone: {lease:?}"
    );
}

/// Assert `reply` is the #23 erased error for request `id`: MCP error
/// -32003, the frame the proxy answers a call to an erased session with.
fn assert_erased_error(reply: &Reply, id: u64) {
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        reply.header("content-type").as_deref(),
        Some("application/json")
    );
    let message = reply.message();
    assert_eq!(message["id"], id, "{message}");
    assert_eq!(message["error"]["code"], -32003, "{message}");
    assert!(
        message["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("erased")),
        "{message}"
    );
}

/// The registry's row for `id` in its admin view.
fn state_of(registry: &SessionRegistry, id: &str) -> &'static str {
    registry
        .slot_views()
        .into_iter()
        .find(|v| v.session == id)
        .map_or("absent", |v| v.state)
}

/// Wait until `cond` holds, up to 10 s.
async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The acceptance row's first item, with the ordering hazards in play: a
/// write already flushed, a write applied but not yet durable whose flush
/// is parked **inside the store** when the erase starts, and an MCP session
/// still open. After the erase the census is the tombstone alone, and stays
/// so once the parked flush is let go: the fenced close joined the flush
/// task, so that flush never lands. The lease is never released on the way
/// (a release would open the gap another writer could take, design Q7),
/// the slot is `Erased`, requests are refused with 410, nothing recreates
/// the session, the handle is dropped, and the other session serves on.
///
/// Mutations: erase without the fence-and-close (straight to the store)
/// and the parked flush lands after it, recreating rows; close the handle
/// unfenced first (flush and release, then erase) and `release_lease`
/// shows up; leave the slot `Live` and the request after is served.
#[tokio::test]
async fn erasing_an_attached_session_leaves_only_the_tombstone_and_nothing_recreates_it() {
    let w = wire().await;
    let addr = w.addr;
    let agent = bearer("agent");

    // A durable write: wait until the flush has carried it to the store.
    let (mcp_a, _) = initialize_as(addr, "/mcp/s/er-a", Some(&agent)).await;
    derive_as(addr, &agent, "/mcp/s/er-a", &mcp_a, &["erase me first"]).await;
    eventually("the first write is durable", || {
        census_but_lease(&w.store, A)
            .iter()
            .any(|(kind, n)| *kind == "concepts" && *n > 0)
    })
    .await;
    // A second write, applied in RAM, whose flush is parked in the store.
    w.calls.park_flushes_of(A);
    derive_as(addr, &agent, "/mcp/s/er-a", &mcp_a, &["erase me too"]).await;
    eventually("a flush is parked in flight", || {
        w.calls.parked_flushes() > 0
    })
    .await;

    // B has data too, and must not lose any of it.
    let (mcp_b, _) = initialize_as(addr, "/mcp/s/er-b", Some(&agent)).await;
    let handle_a = Arc::downgrade(&w.registry.attached()[0].mem);
    assert_eq!(w.registry.attached()[0].id().as_str(), A);

    let before = w.calls.len();
    let reply = erase_as(addr, "ops", A, &confirm(A)).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        reply.header("content-type").as_deref(),
        Some("application/json")
    );
    let report: serde_json::Value =
        serde_json::from_str(reply.body.trim_end()).expect("the report is JSON");
    assert!(
        reply.body.ends_with("}\n"),
        "one line, as the CLI prints it"
    );
    assert_eq!(report["session"], A);
    assert_eq!(report["already_absent"], false);
    assert!(
        report["removed"]["concepts"].as_u64().unwrap() >= 1,
        "{report}"
    );
    assert_eq!(report["removed"]["leases"], 1, "{report}");
    assert_only_the_tombstone(&w.store, A).await;
    assert_eq!(state_of(&w.registry, A), "erased");

    // The parked flush is let go: the fenced close dropped it, so nothing
    // lands, now or later.
    w.calls.release_flushes();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_only_the_tombstone(&w.store, A).await;
    let during = w.calls.since(before);
    for (method, session) in &during {
        assert!(
            !(*method == "release_lease" && session == A),
            "the erase released the lease before erasing (a gap another writer could take): \
             {during:?}"
        );
        assert!(
            !matches!(*method, "acquire_lease" | "load_session" if session == A),
            "nothing re-attaches the erased session: {during:?}"
        );
    }
    let erase_at = during
        .iter()
        .position(|(m, s)| *m == "erase_session" && s == A)
        .expect("the store erase ran");
    // #32 PR 7 review M2: the fence and the close come first, so the
    // parked flush was aborted and joined before the store erase was
    // entered, and no flush got past the store after it (where a recall
    // index mirror would run).
    assert_eq!(
        w.calls.parked_at_erase(),
        vec![0],
        "a flush was still in flight when the erase reached the store"
    );
    assert!(
        !during[erase_at..]
            .iter()
            .any(|(m, s)| *m == "flushed" && s == A),
        "a flush completed after the erase: {:?}",
        &during[erase_at..]
    );
    assert!(
        !during[erase_at..]
            .iter()
            .any(|(m, s)| *m == "record_canonization" && s == A),
        "no canonization of the erased session reached the store after the erase: {:?}",
        &during[erase_at..]
    );

    // Refused from now on, in scope, with the #23 erased error (MCP error
    // -32003, design §6.2) for a new client and for the MCP session opened
    // before the erase.
    let refused = http_as(
        addr,
        "POST",
        "/mcp/s/er-a",
        Some(&agent),
        None,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"x","version":"1"}}}"#,
    )
    .await;
    assert_erased_error(&refused, 1);
    let stale = http_as(
        addr,
        "POST",
        "/mcp/s/er-a",
        Some(&agent),
        Some(&mcp_a),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    )
    .await;
    assert_erased_error(&stale, 2);
    // A notification has no id to answer, and a GET opens no call: 410.
    let note = http_as(
        addr,
        "POST",
        "/mcp/s/er-a",
        Some(&agent),
        Some(&mcp_a),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert_eq!(note.status, 410, "{}", note.body);
    // `/mcp` is er-a too (the default).
    assert_eq!(
        http_as(addr, "GET", "/mcp", Some(&agent), None, "")
            .await
            .status,
        410
    );

    // Nothing in this process can attach it again: the tombstone refuses.
    assert!(w.registry.acquire(A).await.is_err());
    assert_only_the_tombstone(&w.store, A).await;

    // The erased handle (its graph, recall cache, vectors) is gone.
    eventually("the erased Memory is dropped", || {
        handle_a.strong_count() == 0
    })
    .await;

    // The other session keeps serving.
    stats_as(addr, &agent, "/mcp/s/er-b", &mcp_b).await;
    assert_eq!(state_of(&w.registry, B), "live");

    // A repeat is `already_absent`, 200, and leaves the slot `Erased`.
    let again = erase_as(addr, "ops", A, &confirm(A)).await;
    assert_eq!(again.status, 200, "{}", again.body);
    let again: serde_json::Value = serde_json::from_str(again.body.trim_end()).unwrap();
    assert_eq!(again["already_absent"], true, "{again}");
    assert_eq!(again["fence_token"], report["fence_token"]);
    assert_eq!(state_of(&w.registry, A), "erased");
}

/// The acceptance row's second item: an id this process does not hold,
/// with a row planted in every kind the store keeps (an unconsumed #22
/// image intent with its vector included), erased by a prefix credential.
/// Only the tombstone remains, the id holds no slot afterwards (the
/// negative cache is bounded by the hosted set), and a repeat is
/// `already_absent`.
#[tokio::test]
async fn erasing_a_session_this_process_does_not_hold_leaves_only_the_tombstone() {
    let w = wire().await;
    let user = format!("{USERS}alice");
    let sid = SessionId::new(&user);
    w.store
        .flush(&planted_batch(&sid, 8), None)
        .await
        .expect("plant");
    assert!(
        census_but_lease(&w.store, &user)
            .iter()
            .filter(|(kind, _)| matches!(
                *kind,
                "sessions" | "interactions" | "concepts" | "edges" | "write_intents"
            ))
            .all(|(_, n)| *n > 0),
        "planted"
    );

    let reply = erase_as(w.addr, "app", &user, &confirm(&user)).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let report: serde_json::Value = serde_json::from_str(reply.body.trim_end()).unwrap();
    assert_eq!(report["already_absent"], false);
    assert_eq!(report["removed"]["write_intents"], 2, "{report}");
    assert_only_the_tombstone(&w.store, &user).await;
    assert_eq!(state_of(&w.registry, &user), "absent");

    let again = erase_as(w.addr, "app", &user, &confirm(&user)).await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert!(
        again.body.contains(r#""already_absent":true"#),
        "{}",
        again.body
    );
}

/// The acceptance row's third and fourth items: a session another process
/// holds is a 409 naming nothing but the holder, and is untouched (its
/// lease stays that writer's, its slot `held_elsewhere`); a confirm that
/// does not repeat the id, a malformed body and a wrong method are refused
/// before the registry is asked (400, 400, 405), touching nothing.
#[tokio::test]
async fn a_live_holder_elsewhere_is_409_and_a_bad_confirm_is_400() {
    let w = wire().await;
    let reply = erase_as(w.addr, "ops", HELD, &confirm(HELD)).await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert!(reply.body.contains("nothing was erased"), "{}", reply.body);
    assert!(reply.body.contains(&elsewhere().token()), "{}", reply.body);
    let lease = w
        .store
        .read_lease(&SessionId::new(HELD))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lease.holder, elsewhere().token());
    assert_eq!(state_of(&w.registry, HELD), "held_elsewhere");

    let before = w.calls.len();
    // A confirm naming another session, one differing only in case, and
    // bodies that are not `{"confirm": ...}` at all (an unknown field
    // included: nothing may ride along with the typo guard).
    for body in [
        confirm(B),
        confirm("ER-A"),
        String::new(),
        "{}".into(),
        "nope".into(),
        r#"{"confirm":"er-a","force":true}"#.into(),
    ] {
        let reply = erase_as(w.addr, "ops", A, &body).await;
        assert_eq!(reply.status, 400, "{body:?}: {}", reply.body);
        assert!(reply.body.contains("nothing was erased"), "{}", reply.body);
    }
    let get = http_as(
        w.addr,
        "GET",
        "/admin/s/er-a/erase",
        Some(&bearer("ops")),
        None,
        "",
    )
    .await;
    assert_eq!(get.status, 405, "{}", get.body);
    assert_eq!(get.header("allow").as_deref(), Some("POST"));
    assert!(
        w.calls.since(before).iter().all(|(m, _)| matches!(
            *m,
            "refresh_lease" | "flush" | "flushed" | "write_flush_stats"
        )),
        "a refused erase makes no store call of its own: {:?}",
        w.calls.since(before)
    );
    assert_eq!(state_of(&w.registry, A), "live");
}

/// The acceptance row's fifth and sixth items, byte for byte: a credential
/// without `erase` (`agent`), and one whose scope does not cover the id
/// (`other`, `app`), get exactly the unrouted 404 for the erase route on
/// every method, for live, held, erased, unknown, malformed and
/// percent-encoded ids; a credential without `admin` gets it for
/// `/admin/sessions`. The store sees no call while they are answered (the
/// live sessions' own lease heartbeat and flushes aside). The erase is
/// still possible afterwards, so the refusals were refusals.
///
/// Mutation: authorize with `SessionNeed::Use` instead of `Erase` and the
/// `agent` credential's erase is served.
#[tokio::test]
async fn an_erase_or_admin_request_without_the_capability_or_scope_is_the_uniform_404() {
    let w = wire().await;
    w.registry.force_state("er-b", ForcedState::Erased);
    let addr = w.addr;
    let erase_paths = [
        "/admin/s/er-a/erase",
        "/admin/s/er-b/erase",
        "/admin/s/er-held/erase",
        "/admin/s/er-unknown/erase",
        "/admin/s/er-u-bob/erase",
        "/admin/s/.x/erase",
        "/admin/s/er%2Da/erase",
        "/admin/s//erase",
    ];
    let before = w.calls.len();
    for cred in ["agent", "other", "app", "root"] {
        let auth = bearer(cred);
        let reference = on_the_wire_as(addr, "GET", "/not/routed", Some(&auth)).await;
        assert!(
            reference.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{reference}"
        );
        for path in erase_paths {
            // In scope with `erase`, these are served below, not refused.
            let served = match cred {
                "other" => path == "/admin/s/er-b/erase",
                "app" => path == "/admin/s/er-u-bob/erase",
                _ => false,
            };
            if served {
                continue;
            }
            for method in ["POST", "GET", "DELETE"] {
                let body = confirm("er-a");
                assert_eq!(
                    on_the_wire_with(addr, method, path, "localhost", Some(&auth), &[], &body)
                        .await,
                    reference,
                    "{cred}: {method} {path} must be the unrouted 404"
                );
            }
        }
        if !matches!(cred, "root") {
            for method in ["GET", "POST"] {
                assert_eq!(
                    on_the_wire_as(addr, method, "/admin/sessions", Some(&auth)).await,
                    reference,
                    "{cred}: {method} /admin/sessions without admin"
                );
            }
        }
    }
    let window = w.calls.since(before);
    assert!(
        window.iter().all(|(m, _)| matches!(
            *m,
            "refresh_lease" | "flush" | "flushed" | "write_flush_stats"
        )),
        "a refused admin request makes no store call: {window:?}"
    );
    assert_eq!(state_of(&w.registry, A), "live");

    // The same requests in scope are real answers, not the 404: `app`
    // erases an id under its prefix; `other` reaches er-b, whose slot was
    // forced to `Erased` while its lease is still this process's agent's,
    // so the store refuses the CLI-identity erase as held (409).
    let ok = erase_as(addr, "app", "er-u-bob", &confirm("er-u-bob")).await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    let held = erase_as(addr, "other", B, &confirm(B)).await;
    assert_eq!(held.status, 409, "{}", held.body);
}

/// `GET /admin/sessions` needs `admin` and lists only the caller's scope:
/// `root` (`"*"`) sees every hosted session with its state, `boss-b` only
/// `er-b`. Attached sessions carry their size.
#[tokio::test]
async fn admin_sessions_lists_the_slots_in_the_credentials_scope() {
    let w = wire().await;
    let all = http_as(
        w.addr,
        "GET",
        "/admin/sessions",
        Some(&bearer("root")),
        None,
        "",
    )
    .await;
    assert_eq!(all.status, 200, "{}", all.body);
    let all: serde_json::Value = serde_json::from_str(all.body.trim_end()).unwrap();
    let rows = all["sessions"].as_array().expect("a list");
    let states: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| (r["session"].as_str().unwrap(), r["state"].as_str().unwrap()))
        .collect();
    assert_eq!(states, [(A, "live"), (B, "live"), (HELD, "held_elsewhere")]);
    assert_eq!(rows[0]["default"], true);
    assert_eq!(rows[0]["pinned"], true);
    assert!(rows[0]["attached"]["nodes"].is_u64(), "{}", rows[0]);
    assert!(rows[2].get("attached").is_none(), "{}", rows[2]);

    let mine = http_as(
        w.addr,
        "GET",
        "/admin/sessions",
        Some(&bearer("boss-b")),
        None,
        "",
    )
    .await;
    assert_eq!(mine.status, 200, "{}", mine.body);
    let mine: serde_json::Value = serde_json::from_str(mine.body.trim_end()).unwrap();
    let sessions: Vec<&str> = mine["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["session"].as_str().unwrap())
        .collect();
    assert_eq!(sessions, [B]);

    let post = http_as(
        w.addr,
        "POST",
        "/admin/sessions",
        Some(&bearer("root")),
        None,
        "",
    )
    .await;
    assert_eq!(post.status, 405, "{}", post.body);
}

/// Erase is never on the MCP surface: an erase-capable credential and a
/// plain one see the same tool list, and no tool in it erases (the
/// published schemas themselves are pinned by the server's golden test).
#[tokio::test]
async fn the_mcp_tool_list_is_the_same_for_every_credential_and_has_no_erase() {
    let w = wire().await;
    let mut lists = Vec::new();
    for cred in ["agent", "ops"] {
        let auth = bearer(cred);
        let (mcp, _) = initialize_as(w.addr, "/mcp/s/er-a", Some(&auth)).await;
        let reply = http_as(
            w.addr,
            "POST",
            "/mcp/s/er-a",
            Some(&auth),
            Some(&mcp),
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
        )
        .await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        lists.push(reply.message()["result"]["tools"].clone());
    }
    assert_eq!(lists[0], lists[1]);
    for tool in lists[0].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        assert!(!name.contains("erase"), "{name} on the MCP surface");
    }
}

/// An erase while the session is detaching, or a second erase while the
/// first runs, is a 503 with `Retry-After`; nothing is touched.
#[tokio::test]
async fn an_erase_of_a_detaching_session_is_503() {
    let w = wire().await;
    w.registry.force_state(B, ForcedState::Detaching);
    let reply = erase_as(w.addr, "ops", B, &confirm(B)).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert!(reply.header("retry-after").is_some());
    assert!(w
        .store
        .read_lease(&SessionId::new(B))
        .await
        .unwrap()
        .is_some_and(|l| !crate::store::erase::is_tombstone(&l)));
}

/// #32 PR 7 review M1: the erase's fenced close is abandoned before its
/// fenced branch (a write holds the writers gate, and a second shutdown
/// signal cuts the close short), so the close joined nothing. The erase
/// stops and joins the session's tasks itself before it reaches the
/// store: the flush parked in the store is gone by then, and none
/// completes after the erase. The lease is not released on the way.
///
/// Mutation: ignore the abandoned close (go straight to the erase) and the
/// parked flush is still in flight when `erase_session` is entered.
#[tokio::test]
async fn an_abandoned_close_still_stops_the_flush_before_the_erase() {
    let w = wire().await;
    let addr = w.addr;
    let agent = bearer("agent");
    let (mcp_a, _) = initialize_as(addr, "/mcp/s/er-a", Some(&agent)).await;
    w.calls.park_flushes_of(A);
    derive_as(
        addr,
        &agent,
        "/mcp/s/er-a",
        &mcp_a,
        &["parked in the store"],
    )
    .await;
    eventually("a flush is parked in flight", || {
        w.calls.parked_flushes() > 0
    })
    .await;
    let mem = Arc::clone(&w.registry.attached()[0].mem);
    assert_eq!(mem.session().as_str(), A);
    // A write in progress holds the gate, so the close stalls at its
    // `writers_gate` step; two signals make it give up at once.
    let gate = mem.hold_writers_gate().await;
    w.registry.early().simulate_signal();
    w.registry.early().simulate_signal();

    let before = w.calls.len();
    let reply = erase_as(addr, "ops", A, &confirm(A)).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    drop(gate);
    w.calls.release_flushes();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        w.calls.parked_at_erase(),
        vec![0],
        "a flush was still in flight when the erase reached the store"
    );
    let during = w.calls.since(before);
    let erase_at = during
        .iter()
        .position(|(m, s)| *m == "erase_session" && s == A)
        .expect("the store erase ran");
    assert!(
        !during[erase_at..]
            .iter()
            .any(|(m, s)| *m == "flushed" && s == A),
        "a flush completed after the erase: {during:?}"
    );
    assert!(
        !during.iter().any(|(m, s)| *m == "release_lease" && s == A),
        "the lease is never released on the way: {during:?}"
    );
    assert_only_the_tombstone(&w.store, A).await;
    drop(mem);
}

/// #32 PR 7 review L2: an erase whose store call fails after it committed,
/// while the lease row cannot be read back either, says its outcome is
/// unknown (never "nothing was erased"), and is safe to repeat: the repeat
/// finds the session erased. The same store failure with the row readable
/// says it is durably erased.
///
/// Mutation: classify a failed read-back as "not erased" and the body says
/// nothing was erased.
#[tokio::test]
async fn an_erase_whose_outcome_cannot_be_read_back_says_it_is_unknown() {
    use super::pinned_serve::EraseFault;
    let w = wire().await;
    w.calls.fail_erase(EraseFault::AfterCommit);
    w.calls.fail_read_lease(true);
    let reply = erase_as(w.addr, "ops", A, &confirm(A)).await;
    w.calls.fail_read_lease(false);
    assert_eq!(reply.status, 500, "{}", reply.body);
    assert!(reply.body.contains("outcome is unknown"), "{}", reply.body);
    assert!(!reply.body.contains("nothing was erased"), "{}", reply.body);
    // It did commit; a repeat says so, and finishes.
    w.calls.fail_erase(EraseFault::None);
    let again = erase_as(w.addr, "ops", A, &confirm(A)).await;
    assert_eq!(again.status, 200, "{}", again.body);
    let again: serde_json::Value = serde_json::from_str(again.body.trim_end()).unwrap();
    assert_eq!(again["already_absent"], true, "{again}");
    assert_only_the_tombstone(&w.store, A).await;

    // Readable: the same failure is reported as durably erased.
    w.calls.fail_erase(EraseFault::AfterCommit);
    let reply = erase_as(w.addr, "ops", B, &confirm(B)).await;
    assert_eq!(reply.status, 500, "{}", reply.body);
    assert!(
        reply.body.contains("erased from the durable store"),
        "{}",
        reply.body
    );
    assert_eq!(state_of(&w.registry, B), "erased");
}

/// #32 PR 7 review L1: an erase asked for once the shutdown has taken the
/// attached set is 503 and starts nothing (it is checked and recorded under
/// the task list's lock, so none is spawned that the shutdown would not
/// join).
#[tokio::test]
async fn an_erase_after_the_shutdown_began_is_503_and_touches_nothing() {
    let w = wire().await;
    let closed = w.registry.close_set().await;
    let before = w.calls.len();
    let reply = erase_as(w.addr, "ops", B, &confirm(B)).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert!(
        !w.calls
            .since(before)
            .iter()
            .any(|(m, _)| *m == "erase_session"),
        "nothing was erased"
    );
    for session in closed {
        let _ = session.mem.close().await;
    }
}

/// #32 PR 7 review L1: an erase whose store call hangs when the shutdown
/// takes the attached set is cut short `ERASE_SHUTDOWN_GRACE` later with an
/// unknown outcome, so the shutdown's `CLOSE_GRACE` wait for it ends with
/// it rather than giving up on it. Paused clock.
///
/// Mutation: drop the cut-off and the wait times out (the erase is still
/// hanging at `CLOSE_GRACE`).
#[tokio::test]
async fn an_erase_hanging_at_the_shutdown_is_cut_short_inside_the_close_grace() {
    use super::pinned_serve::EraseFault;
    use crate::mcp::serve::registry::EraseAnswer;
    use crate::mcp::serve::shutdown::CLOSE_GRACE;
    let w = wire().await;
    w.calls.fail_erase(EraseFault::Hang);
    let registry = Arc::clone(&w.registry);
    let erase = tokio::spawn(async move { registry.erase(HELD).await });
    eventually("the erase reached the store", || {
        w.calls
            .since(0)
            .iter()
            .any(|(m, s)| *m == "erase_session" && s == HELD)
    })
    .await;

    tokio::time::pause();
    let closed = w.registry.close_set().await;
    tokio::time::timeout(CLOSE_GRACE, w.registry.join_detaches())
        .await
        .expect("the erase ends inside the shutdown's wait");
    match erase.await.unwrap() {
        EraseAnswer::Failed { erased, .. } => {
            assert_eq!(erased, crate::store::erase::Tombstone::Unknown);
        }
        other => panic!("the erase answered {other:?}"),
    }
    tokio::time::resume();
    for session in closed {
        let _ = session.mem.close().await;
    }
}

/// #32 PR 7 review L4: a registry with no attacher (a one-session serve's)
/// has the shared store from construction, so it can erase a session it
/// never admitted, as the CLI would, instead of answering that it has no
/// store.
///
/// Mutation: seed the store from the first admitted session only and the
/// erase fails with a capability error.
#[tokio::test]
async fn a_registry_that_admitted_nothing_still_erases_as_the_cli_would() {
    use crate::mcp::serve::registry::{EraseAnswer, RegistryBounds};
    let store = Arc::new(MemoryStore::new());
    let registry = SessionRegistry::new(
        vec!["er-solo".to_string()],
        Some("er-solo".to_string()),
        LeaseLossPolicy::ExitProcess,
        None,
        EarlyShutdown::unarmed(),
        RegistryBounds::pinned_only(),
        Some(Arc::clone(&store) as Arc<dyn GraphStore>),
    );
    match registry.erase("er-u-never").await {
        EraseAnswer::Erased(report) => assert!(report.already_absent, "{report:?}"),
        other => panic!("the erase answered {other:?}"),
    }
    assert_only_the_tombstone(&store, "er-u-never").await;
}

/// #32 PR 7 review H1: an erase that arrives while the pinned retry of the
/// same session is between its acquire and its admission (parked in its
/// load, the lease already taken). The erase waits for the retry's attach
/// permit before it claims the slot, so the retry admits the session over
/// the `HeldElsewhere` it started from, and the erase then finds it live
/// and erases it as its holder: 200, only the tombstone, the slot
/// `Erased`, and nothing attached.
///
/// Mutation: claim the slot before taking the permits and the retry
/// overwrites `Erasing` with `Live` (or, admitting conditionally, gives
/// its lease back late), while the erase meets the retry's lease as
/// another holder's: 409, the session served on.
#[tokio::test]
async fn an_erase_racing_the_pinned_retry_waits_for_it_then_erases_what_it_attached() {
    let stall = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let w = wire_stalling(Arc::clone(&stall)).await;
    // The other writer goes away; the retry will take the session.
    w.store
        .release_lease(&SessionId::new(HELD), &elsewhere())
        .await
        .unwrap();
    stall.store(true, std::sync::atomic::Ordering::SeqCst);
    let before = w.calls.len();
    w.registry.spawn_retry_loop();
    eventually("the retry took the lease and parked in its load", || {
        w.calls
            .since(before)
            .iter()
            .any(|(m, s)| *m == "load_session" && s == HELD)
    })
    .await;

    let addr = w.addr;
    let erase = tokio::spawn(async move { erase_as(addr, "ops", HELD, &confirm(HELD)).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!erase.is_finished(), "the erase waits for the attach");
    assert_eq!(
        state_of(&w.registry, HELD),
        "held_elsewhere",
        "the slot is not claimed while an attach is in flight"
    );

    stall.store(false, std::sync::atomic::Ordering::SeqCst);
    let reply = erase.await.unwrap();
    assert_eq!(reply.status, 200, "{}", reply.body);
    let report: serde_json::Value = serde_json::from_str(reply.body.trim_end()).unwrap();
    assert_eq!(report["already_absent"], false, "{report}");
    assert_eq!(report["removed"]["leases"], 1, "{report}");
    assert_only_the_tombstone(&w.store, HELD).await;
    assert_eq!(state_of(&w.registry, HELD), "erased");
    assert!(
        !w.registry
            .attached()
            .iter()
            .any(|s| s.id().as_str() == HELD),
        "nothing of the erased session is attached"
    );
}

/// #32 PR 7 review H1, belt and braces: a pinned retry whose slot changed
/// while it acquired (forced here, as nothing in the serve now changes it
/// then) does not overwrite it. The session it attached is taken down and
/// its lease released, so nothing of it is served and no lease is left
/// behind.
///
/// Mutation: admit unconditionally and the slot becomes `Live` again.
#[tokio::test]
async fn a_retry_that_finds_its_slot_changed_releases_what_it_took() {
    let stall = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let w = wire_stalling(Arc::clone(&stall)).await;
    w.store
        .release_lease(&SessionId::new(HELD), &elsewhere())
        .await
        .unwrap();
    stall.store(true, std::sync::atomic::Ordering::SeqCst);
    let before = w.calls.len();
    w.registry.spawn_retry_loop();
    eventually("the retry took the lease and parked in its load", || {
        w.calls
            .since(before)
            .iter()
            .any(|(m, s)| *m == "load_session" && s == HELD)
    })
    .await;
    w.registry.force_state(HELD, ForcedState::Failed);
    stall.store(false, std::sync::atomic::Ordering::SeqCst);
    eventually("the retry gave its lease back", || {
        w.calls
            .since(before)
            .iter()
            .any(|(m, s)| *m == "release_lease" && s == HELD)
    })
    .await;
    assert_eq!(state_of(&w.registry, HELD), "failed");
    assert!(!w
        .registry
        .attached()
        .iter()
        .any(|s| s.id().as_str() == HELD));
    let lease = w
        .store
        .read_lease(&SessionId::new(HELD))
        .await
        .unwrap()
        .expect("a lease row");
    assert_eq!(
        lease.holder,
        crate::store::lease::RELEASED_HOLDER,
        "{lease:?}"
    );
}

/// #32 PR 7 review L3: a session still being erased answers 503 with
/// `Retry-After: 1` (the erase may yet end in 409 or fail before its
/// commit, and serve it again); only an erased one answers the permanent
/// 410.
///
/// Mutation: answer `Erasing` with 410 again.
#[tokio::test]
async fn an_erasing_session_is_503_and_only_an_erased_one_is_410() {
    let w = wire().await;
    let ops = bearer("ops");
    w.registry.force_state(B, ForcedState::Erasing);
    let erasing = http_as(w.addr, "GET", "/mcp/s/er-b", Some(&ops), None, "").await;
    assert_eq!(erasing.status, 503, "{}", erasing.body);
    assert_eq!(erasing.header("retry-after").as_deref(), Some("1"));
    w.registry.force_state(B, ForcedState::Erased);
    let erased = http_as(w.addr, "GET", "/mcp/s/er-b", Some(&ops), None, "").await;
    assert_eq!(erased.status, 410, "{}", erased.body);
    assert!(erased.header("retry-after").is_none());
}

/// #32 PR 7 review, PR 6 reconciliation 2: an erase of a session whose
/// on-demand attach is in flight is 503 with `Retry-After`; the slot is
/// left to the attach, and nothing is touched.
#[tokio::test]
async fn an_erase_of_an_attaching_session_is_503() {
    let w = wire().await;
    w.registry.force_state(B, ForcedState::Attaching);
    let before = w.calls.len();
    let reply = erase_as(w.addr, "ops", B, &confirm(B)).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert!(reply.header("retry-after").is_some());
    assert_eq!(state_of(&w.registry, B), "attaching");
    assert!(
        !w.calls
            .since(before)
            .iter()
            .any(|(m, _)| *m == "erase_session"),
        "nothing was erased"
    );
}

/// A pinned session that meets the tombstone when its background retry
/// attaches it is `Erased`, not `Failed` (#32 PR 4 note for PR 7), so its
/// requests get 410 rather than a 503 telling an operator to act, and it
/// is not retried again.
#[tokio::test]
async fn a_held_session_erased_meanwhile_becomes_erased_not_failed() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::INFO);
    let w = wire().await;
    // The other writer goes away and the operator erases the session from
    // the CLI's side.
    w.store
        .release_lease(&SessionId::new(HELD), &elsewhere())
        .await
        .unwrap();
    let eraser = LeaseHolder::for_this_process(&crate::types::AgentId::new("lambo-erase-session"));
    w.store
        .erase_session(&SessionId::new(HELD), &eraser)
        .await
        .unwrap();
    w.registry.spawn_retry_loop();
    let deadline = Instant::now() + PINNED_RETRY * 3;
    while state_of(&w.registry, HELD) != "erased" {
        assert!(
            Instant::now() < deadline,
            "the retry never classified the session: {}",
            state_of(&w.registry, HELD)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let reply = http_as(
        w.addr,
        "GET",
        "/mcp/s/er-held",
        Some(&bearer("ops")),
        None,
        "",
    )
    .await;
    assert_eq!(reply.status, 410, "{}", reply.body);
    let given_up = |logs: &crate::test_util::CapturedLogs| {
        logs.lines()
            .iter()
            .filter(|l| l.contains("was erased; it is not retried") && l.contains(HELD))
            .count()
    };
    assert_eq!(given_up(&logs), 1, "{:?}", logs.lines());
    assert!(
        !logs
            .lines()
            .iter()
            .any(|l| l.contains("will not be retried")),
        "an erased session is not reported as failed"
    );
    assert!(crate::store::erase::is_tombstone(
        &w.store
            .read_lease(&SessionId::new(HELD))
            .await
            .unwrap()
            .unwrap()
    ));
}

/// `lambo_derive` as `auth` on an open MCP session, waiting for the write
/// to apply.
async fn derive_as(addr: SocketAddr, auth: &str, path: &str, mcp: &str, contents: &[&str]) {
    let concepts: Vec<_> = contents
        .iter()
        .map(|c| serde_json::json!({"content": c, "concept_type": "entity"}))
        .collect();
    let out = call_as(
        addr,
        auth,
        path,
        mcp,
        "lambo_derive",
        serde_json::json!({"agent_id": "agent-a", "concepts": concepts}),
    )
    .await;
    let receipt = out["structuredContent"]["receipt"]
        .as_str()
        .expect("a receipt")
        .to_string();
    call_as(
        addr,
        auth,
        path,
        mcp,
        "lambo_stats",
        serde_json::json!({"agent_id": "agent-a", "receipt": receipt, "wait_ms": 10_000}),
    )
    .await;
}

async fn stats_as(addr: SocketAddr, auth: &str, path: &str, mcp: &str) -> serde_json::Value {
    call_as(
        addr,
        auth,
        path,
        mcp,
        "lambo_stats",
        serde_json::json!({"agent_id": "agent-a"}),
    )
    .await
}

async fn call_as(
    addr: SocketAddr,
    auth: &str,
    path: &str,
    mcp: &str,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {"name": name, "arguments": args},
    })
    .to_string();
    let reply = http_as(addr, "POST", path, Some(auth), Some(mcp), &body).await;
    assert_eq!(reply.status, 200, "{name} at {path}: {}", reply.body);
    let message = reply.message();
    assert!(message.get("error").is_none(), "{name}: {message}");
    message["result"].clone()
}

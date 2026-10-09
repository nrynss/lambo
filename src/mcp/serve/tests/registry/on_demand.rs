//! #32 PR 6: the on-demand scope, through the serve's router and guards
//! (`transport::http_app`) over a registry that attaches on demand.
//!
//! The PR 6 acceptance row, one test each: N concurrent first requests
//! cause one `build_attach`; an eviction at `max_attached` releases the
//! evicted lease, and attaching it again mints the next token and recalls
//! the same; an idle session is detached after `idle_detach` (paused
//! clock); a pinned session is never evicted or idle-detached; 503 with
//! `Retry-After` when nothing can be evicted and when the session is held
//! elsewhere; one session's flood spends only its own rate. Plus: a
//! credential without `create` attaches only an existing session, an erased
//! one is 410, and the shutdown's close set takes the on-demand sessions
//! too, releasing their leases.

use super::pinned_serve::{Shared, StoreCalls};
use super::*;
use crate::config::ServeCredential;
use crate::mcp::serve::registry::OnDemandBounds;
use crate::store::lease::{LeaseHolder, LEASE_TTL, RELEASED_HOLDER};
use crate::surface::session::{
    parse_addressed, SessionCapabilities, SessionGrant, SessionPrefix, SessionScope,
};
use crate::types::{AgentId, SessionId};

/// The pinned session of every registry here.
const PINNED: &str = "od-pinned";
/// What the `maker` and `reader` credentials address.
const PREFIX: &str = "od-u-";

/// A fake token, built at runtime so no token-shaped literal sits in the
/// source.
fn token(label: &str) -> String {
    ["fake", label, "ondemand", "value"].join("-")
}

fn bearer(label: &str) -> String {
    format!("Bearer {}", token(label))
}

/// A credential over [`PINNED`] and every id under [`PREFIX`].
fn credential(name: &str, create: bool) -> ServeCredential {
    ServeCredential {
        grant: SessionGrant::new(
            name,
            SessionScope::new(
                [parse_addressed(PINNED).expect("addressable")],
                false,
                Some(SessionPrefix::new(PREFIX).expect("a prefix")),
            ),
            SessionCapabilities {
                create,
                ..SessionCapabilities::default()
            },
        ),
        token: SecretToken::new(token(name)).expect("non-empty"),
    }
}

/// A registry pinning [`PINNED`] that attaches on demand within
/// `max_attached` and `idle_detach`, each session at `session_rps`, served
/// behind the real guards with two credentials: `maker` (with `create`)
/// and `reader` (without).
struct OnDemand {
    addr: SocketAddr,
    registry: Arc<SessionRegistry>,
    store: Arc<MemoryStore>,
    calls: Arc<StoreCalls>,
}

impl OnDemand {
    async fn start(max_attached: usize, idle_detach: Duration, session_rps: u32) -> Self {
        let mut opts = ServeOptions::new(PINNED, "agent-a");
        opts.transport = Transport::Http;
        opts.credentials = vec![credential("maker", true), credential("reader", false)];
        assert!(
            crate::mcp::serve::authority::reaches_past_pinned(&opts),
            "a prefix reaches past the pinned session"
        );
        let authority = authority_for(&opts);
        let store = Arc::new(MemoryStore::new());
        let (recorded, calls) = Shared::recording(&store);
        let backends = backends_over(recorded, fast_config(1_000));
        let early = EarlyShutdown::unarmed();
        let store_cfg = backends.store_cfg.clone();
        let template = super::super::builder::serve_builder(
            &opts,
            backends,
            None,
            None,
            early.clone(),
            Some(crate::writeq::EmbedderCalibration::new()),
        );
        let registry = SessionRegistry::new(
            vec![PINNED.to_string()],
            Some(PINNED.to_string()),
            LeaseLossPolicy::for_scope(1, true),
            Some(SessionAttacher {
                template,
                store_cfg,
                ledger: None,
                max_sessions: 64,
                host_check: HostCheck::for_authority(Some(&authority)),
                agent: "agent-a".into(),
                session_rps,
            }),
            early,
            RegistryBounds {
                attach_permits: 2,
                on_demand: Some(OnDemandBounds {
                    max_attached,
                    idle_detach,
                }),
            },
        );
        attach_or_hold(&registry, PINNED).await;
        registry.mark_started();
        registry.spawn_idle_sweeper();
        let addr = serve_app(guarded_app(Arc::clone(&registry), authority, 64)).await;
        Self {
            addr,
            registry,
            store,
            calls,
        }
    }

    fn attached(&self, id: &str) -> Option<Arc<AttachedSession>> {
        self.registry
            .attached()
            .into_iter()
            .find(|s| s.id().as_str() == id)
    }

    /// How many times the store was asked `method` for session `id`.
    fn count(&self, method: &str, id: &str) -> usize {
        self.calls
            .since(0)
            .into_iter()
            .filter(|(m, s)| *m == method && s == id)
            .count()
    }

    async fn lease(&self, id: &str) -> crate::store::LeaseInfo {
        self.store
            .read_lease(&SessionId::new(id))
            .await
            .expect("read")
            .expect("a lease row")
    }

    /// Close every attached session, as the shutdown's stage 3 does.
    async fn close(self) {
        for session in self.registry.close_set().await {
            session.mem.close().await.expect("close");
        }
    }
}

/// `tools/call` as `who`, returning the `result`.
async fn call_as(
    addr: SocketAddr,
    path: &str,
    who: &str,
    mcp_session: &str,
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
    let reply = http_as(
        addr,
        "POST",
        path,
        Some(&bearer(who)),
        Some(mcp_session),
        &body,
    )
    .await;
    assert_eq!(reply.status, 200, "{name} at {path}: {}", reply.body);
    let message = reply.message();
    assert!(message.get("error").is_none(), "{name}: {message}");
    message["result"].clone()
}

/// Derive `content` into the session at `path` and wait until it applies.
async fn derive_as(addr: SocketAddr, path: &str, sid: &str, content: &str) {
    let out = call_as(
        addr,
        path,
        "maker",
        sid,
        "lambo_derive",
        serde_json::json!({
            "agent_id": "agent-a",
            "concepts": [{"content": content, "concept_type": "entity"}],
        }),
    )
    .await;
    let receipt = out["structuredContent"]["receipt"]
        .as_str()
        .expect("a receipt")
        .to_string();
    call_as(
        addr,
        path,
        "maker",
        sid,
        "lambo_stats",
        serde_json::json!({"agent_id": "agent-a", "receipt": receipt, "wait_ms": 10_000}),
    )
    .await;
}

/// The contents `lambo_recall` returns for `query` at `path`.
async fn recalled(addr: SocketAddr, path: &str, sid: &str, query: &str) -> Vec<String> {
    let out = call_as(
        addr,
        path,
        "maker",
        sid,
        "lambo_recall",
        serde_json::json!({"agent_id": "agent-a", "query": query}),
    )
    .await;
    let mut contents: Vec<String> = out["structuredContent"]["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("hits in {out}"))
        .iter()
        .filter_map(|hit| hit["content"].as_str().map(str::to_string))
        .collect();
    contents.sort();
    contents
}

/// Wait up to `budget` for `done`.
async fn until(budget: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + budget;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Acceptance: N concurrent first requests for one session cause one
/// `build_attach` (one lease acquire, one existence probe), and every one
/// of them is served by the session it attached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_requests_attach_once() {
    let od = OnDemand::start(16, Duration::from_secs(900), 0).await;
    let id = "od-u-flight";
    let path = format!("/mcp/s/{id}");
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let path = path.clone();
        let addr = od.addr;
        requests.spawn(async move { initialize_as(addr, &path, Some(&bearer("maker"))).await });
    }
    let mut served = 0;
    while let Some(done) = requests.join_next().await {
        let (_, info) = done.expect("a request task");
        assert!(
            info["instructions"]
                .as_str()
                .is_some_and(|i| i.contains(id)),
            "{info}"
        );
        served += 1;
    }
    assert_eq!(served, 8);
    assert_eq!(od.count("acquire_lease", id), 1, "one build_attach");
    assert_eq!(od.count("read_lease", id), 1, "one existence probe");
    assert!(od.attached(id).is_some());
    od.close().await;
}

/// Acceptance: at `max_attached` an attach evicts the least recently used
/// idle on-demand session, whose lease is released (token kept); attaching
/// it again mints the next token and recalls what it held. The pinned
/// session is never the one evicted.
#[tokio::test]
async fn eviction_releases_the_lease_and_a_reattach_mints_the_next_token() {
    // One pinned session and one on-demand place.
    let od = OnDemand::start(2, Duration::from_secs(900), 0).await;
    let (first, second) = ("od-u-first", "od-u-second");
    let first_path = format!("/mcp/s/{first}");
    let (sid, _) = initialize_as(od.addr, &first_path, Some(&bearer("maker"))).await;
    let marker = "on-demand eviction marker omega";
    derive_as(od.addr, &first_path, &sid, marker).await;
    let before = recalled(od.addr, &first_path, &sid, marker).await;
    assert!(before.iter().any(|c| c == marker), "{before:?}");
    let first_token = od.lease(first).await.token;

    // The second session takes the one place: the first is evicted.
    initialize_as(od.addr, &format!("/mcp/s/{second}"), Some(&bearer("maker"))).await;
    assert!(od.attached(first).is_none(), "the first was evicted");
    assert!(od.attached(PINNED).is_some(), "the pinned one never is");
    let released = od.lease(first).await;
    assert_eq!(
        released.holder, RELEASED_HOLDER,
        "the evicted lease is released"
    );
    assert_eq!(released.token, first_token, "and keeps its token");

    // The first again: the second is evicted in its turn, and the first
    // comes back under the next token with what it held.
    let (sid, _) = initialize_as(od.addr, &first_path, Some(&bearer("maker"))).await;
    assert!(od.attached(second).is_none());
    assert_eq!(od.lease(first).await.token, first_token + 1);
    assert_eq!(od.lease(second).await.holder, RELEASED_HOLDER);
    let after = recalled(od.addr, &first_path, &sid, marker).await;
    assert_eq!(after, before, "the same recall after the reattach");
    od.close().await;
}

/// Acceptance: an on-demand session unused for `idle_detach` is detached by
/// the sweeper (its lease released), on a paused clock; the pinned session,
/// idle just as long, is not.
#[tokio::test]
async fn an_idle_on_demand_session_is_detached_and_a_pinned_one_never() {
    let idle = Duration::from_secs(60);
    let od = OnDemand::start(16, idle, 0).await;
    let id = "od-u-idle";
    initialize_as(od.addr, &format!("/mcp/s/{id}"), Some(&bearer("maker"))).await;
    assert!(od.attached(id).is_some());

    tokio::time::pause();
    // Not yet: one sweep in, short of the idle time.
    tokio::time::advance(idle / 2 + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(od.attached(id).is_some(), "not idle long enough yet");
    // Past it: the next sweep takes it out of service.
    tokio::time::advance(idle).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::resume();
    until(Duration::from_secs(10), "the idle detach", || {
        matches!(
            od.registry.lookup(id),
            crate::mcp::serve::registry::Lookup::NotHosted
        )
    })
    .await;
    assert_eq!(od.lease(id).await.holder, RELEASED_HOLDER);
    assert!(
        od.attached(PINNED).is_some(),
        "pinned is never idle-detached"
    );
    od.close().await;
}

/// Acceptance: with every on-demand place taken by a session that has a
/// call in flight, an attach is refused with 503 and `Retry-After` (the
/// idle pinned session is not evicted instead), and served once one is
/// idle again. Held elsewhere is 503 with `Retry-After` too, and no
/// background retry: the next request tries again.
#[tokio::test]
async fn nothing_to_evict_or_held_elsewhere_is_503_with_retry_after() {
    let od = OnDemand::start(2, Duration::from_secs(900), 0).await;
    let busy = "od-u-busy";
    initialize_as(od.addr, &format!("/mcp/s/{busy}"), Some(&bearer("maker"))).await;
    let in_flight = od.attached(busy).expect("attached").activity.enter();

    let full = http_as(
        od.addr,
        "POST",
        "/mcp/s/od-u-next",
        Some(&bearer("maker")),
        None,
        "{}",
    )
    .await;
    assert_eq!(full.status, 503, "{}", full.body);
    assert!(full.header("retry-after").is_some(), "{}", full.head);
    assert!(od.attached(PINNED).is_some(), "pinned is never evicted");
    assert!(
        od.attached(busy).is_some(),
        "a session in use is never evicted"
    );
    assert_eq!(
        od.count("read_lease", "od-u-next"),
        0,
        "refused before any store call"
    );

    drop(in_flight);
    initialize_as(od.addr, "/mcp/s/od-u-next", Some(&bearer("maker"))).await;
    assert!(od.attached(busy).is_none(), "evicted once idle");

    // Another writer holds a session: 503 with the time its lease could
    // lapse, and nothing kept in a slot.
    let held = "od-u-held";
    let other = LeaseHolder::for_this_process(&AgentId::new("another-writer"));
    od.store
        .acquire_lease(&SessionId::new(held), &other, LEASE_TTL)
        .await
        .expect("the other writer takes it");
    let reply = http_as(
        od.addr,
        "POST",
        &format!("/mcp/s/{held}"),
        Some(&bearer("maker")),
        None,
        "{}",
    )
    .await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    let retry: u64 = reply
        .header("retry-after")
        .expect("a Retry-After")
        .parse()
        .expect("seconds");
    assert!((1..=LEASE_TTL.as_secs()).contains(&retry), "{retry}");
    assert!(od.attached(held).is_none());
    od.store
        .release_lease(&SessionId::new(held), &other)
        .await
        .expect("the other writer lets go");
    initialize_as(od.addr, &format!("/mcp/s/{held}"), Some(&bearer("maker"))).await;
    od.close().await;
}

/// Acceptance: each session draws its own bucket at `per_session_rps`, so a
/// flood on one session is refused (429) while another session, from the
/// same credential, is still served.
#[tokio::test]
async fn a_flood_on_one_session_spends_only_its_own_rate() {
    // 1 rps, burst 2.
    let od = OnDemand::start(16, Duration::from_secs(900), 1).await;
    let (flooded, quiet) = ("od-u-flood", "od-u-quiet");
    // Both attached before the flood, each by one request (one token).
    for id in [flooded, quiet] {
        let reply = http_as(
            od.addr,
            "POST",
            &format!("/mcp/s/{id}"),
            Some(&bearer("maker")),
            None,
            "{}",
        )
        .await;
        assert_ne!(reply.status, 429, "{}", reply.body);
    }
    let mut refused = 0;
    for _ in 0..6 {
        let reply = http_as(
            od.addr,
            "POST",
            &format!("/mcp/s/{flooded}"),
            Some(&bearer("maker")),
            None,
            "{}",
        )
        .await;
        if reply.status == 429 {
            assert_eq!(reply.header("retry-after").as_deref(), Some("1"));
            refused += 1;
        }
    }
    assert!(refused >= 3, "the flood is limited: {refused} refused");
    // The quiet session spent only its own one token: it is still served.
    let reply = http_as(
        od.addr,
        "POST",
        &format!("/mcp/s/{quiet}"),
        Some(&bearer("maker")),
        None,
        "{}",
    )
    .await;
    assert_ne!(reply.status, 429, "{}", reply.body);
    od.close().await;
}

/// Design decision 3: a credential without `create` attaches an on-demand
/// session only when it exists (a lease row), else gets the uniform 404
/// after one probe and no acquire; an erased session is 410 to either
/// credential and is never recreated.
#[tokio::test]
async fn without_create_only_an_existing_session_attaches_and_an_erased_one_is_gone() {
    let od = OnDemand::start(16, Duration::from_secs(900), 0).await;
    let absent = "od-u-absent";
    let reference =
        crate::test_util::on_the_wire_as(od.addr, "GET", "/not/routed", Some(&bearer("reader")))
            .await;
    assert_eq!(
        crate::test_util::on_the_wire_as(
            od.addr,
            "POST",
            &format!("/mcp/s/{absent}"),
            Some(&bearer("reader"))
        )
        .await,
        reference,
        "absent, no create: the uniform 404"
    );
    assert_eq!(od.count("read_lease", absent), 1);
    assert_eq!(od.count("acquire_lease", absent), 0, "nothing created");

    // A session a writer has used (its lease row exists, released).
    let existing = "od-u-existing";
    let writer = LeaseHolder::for_this_process(&AgentId::new("another-writer"));
    od.store
        .acquire_lease(&SessionId::new(existing), &writer, LEASE_TTL)
        .await
        .expect("acquire");
    od.store
        .release_lease(&SessionId::new(existing), &writer)
        .await
        .expect("release");
    let (_, info) = initialize_as(
        od.addr,
        &format!("/mcp/s/{existing}"),
        Some(&bearer("reader")),
    )
    .await;
    assert!(info["instructions"]
        .as_str()
        .is_some_and(|i| i.contains(existing)));

    // Erased: 410 for both, never acquired.
    let erased = "od-u-erased";
    let session = SessionId::new(erased);
    od.store
        .acquire_lease(&session, &writer, LEASE_TTL)
        .await
        .expect("acquire");
    od.store
        .erase_session(&session, &writer)
        .await
        .expect("erase");
    let acquires = od.count("acquire_lease", erased);
    for who in ["reader", "maker"] {
        let reply = http_as(
            od.addr,
            "POST",
            &format!("/mcp/s/{erased}"),
            Some(&bearer(who)),
            None,
            "{}",
        )
        .await;
        assert_eq!(reply.status, 410, "{who}: {}", reply.body);
    }
    assert_eq!(
        od.count("acquire_lease", erased),
        acquires,
        "never recreated"
    );
    od.close().await;
}

/// The process shutdown's close set takes the on-demand sessions with the
/// pinned ones, so their leases are released at exit too.
#[tokio::test]
async fn the_shutdown_closes_the_on_demand_sessions_too() {
    let od = OnDemand::start(16, Duration::from_secs(900), 0).await;
    for id in ["od-u-b", "od-u-a"] {
        initialize_as(od.addr, &format!("/mcp/s/{id}"), Some(&bearer("maker"))).await;
    }
    let order: Vec<String> = od
        .registry
        .attached()
        .iter()
        .map(|s| s.id().to_string())
        .collect();
    assert_eq!(
        order,
        [PINNED, "od-u-a", "od-u-b"],
        "pinned first, then by id"
    );
    let store = Arc::clone(&od.store);
    od.close().await;
    for id in [PINNED, "od-u-a", "od-u-b"] {
        let row = store
            .read_lease(&SessionId::new(id))
            .await
            .expect("read")
            .expect("a row");
        assert_eq!(row.holder, RELEASED_HOLDER, "{id}");
    }
}

/// `attach_concurrency` is the number of attach permits, never zero, and
/// one on SQLite (design §3.6, R1).
#[test]
fn attach_concurrency_is_one_on_sqlite() {
    use crate::store::{StoreConfig, StoreKind};
    let bounds = crate::mcp::serve::SessionBounds {
        attach_concurrency: 4,
        ..Default::default()
    };
    let memory = StoreConfig::default();
    let sqlite = StoreConfig {
        kind: StoreKind::Sqlite,
        ..StoreConfig::default()
    };
    assert_eq!(bounds.attach_permits(&memory), 4);
    assert_eq!(bounds.attach_permits(&sqlite), 1);
    let zero = crate::mcp::serve::SessionBounds {
        attach_concurrency: 0,
        ..Default::default()
    };
    assert_eq!(zero.attach_permits(&memory), 1);
    assert_eq!(bounds.session_rps(50), 50, "defaults to the global rate");
    let own = crate::mcp::serve::SessionBounds {
        per_session_rps: Some(5),
        ..Default::default()
    };
    assert_eq!(own.session_rps(50), 5);
}

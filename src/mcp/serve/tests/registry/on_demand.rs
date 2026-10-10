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
use crate::mcp::serve::registry::{Lookup, OnDemandBounds, Requester};
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

/// A credential over [`PINNED`] only: it reaches no on-demand session.
fn pinned_only(name: &str) -> ServeCredential {
    ServeCredential {
        grant: SessionGrant::new(
            name,
            SessionScope::new([parse_addressed(PINNED).expect("addressable")], false, None),
            SessionCapabilities {
                create: true,
                ..SessionCapabilities::default()
            },
        ),
        token: SecretToken::new(token(name)).expect("non-empty"),
    }
}

/// `name` asking, with or without `create`.
fn asking(name: &str, create: bool) -> Requester<'_> {
    Requester {
        credential: name,
        create,
    }
}

/// A registry pinning [`PINNED`] that attaches on demand within
/// `max_attached` and `idle_detach`, each session at `session_rps`, served
/// behind the real guards with three credentials: `maker` (with `create`)
/// and `reader` (without) over [`PREFIX`], and `pinned` over [`PINNED`]
/// only. The on-demand places are shared between `maker` and `reader`.
struct OnDemand {
    addr: SocketAddr,
    registry: Arc<SessionRegistry>,
    store: Arc<MemoryStore>,
    calls: Arc<StoreCalls>,
    /// While set, every session load stalls (`Shared`).
    stall: Arc<std::sync::atomic::AtomicBool>,
}

impl OnDemand {
    /// No eviction floor, so a test can evict a session it just used.
    async fn start(max_attached: usize, idle_detach: Duration, session_rps: u32) -> Self {
        Self::start_with(max_attached, idle_detach, session_rps, Duration::ZERO).await
    }

    async fn start_with(
        max_attached: usize,
        idle_detach: Duration,
        session_rps: u32,
        min_idle_to_evict: Duration,
    ) -> Self {
        let mut opts = ServeOptions::new(PINNED, "agent-a");
        opts.transport = Transport::Http;
        opts.credentials = vec![
            credential("maker", true),
            credential("reader", false),
            pinned_only("pinned"),
        ];
        assert!(
            crate::mcp::serve::authority::reaches_past_pinned(&opts),
            "a prefix reaches past the pinned session"
        );
        let share_among = crate::mcp::serve::authority::on_demand_credentials(&opts);
        assert_eq!(share_among, 2, "maker and reader, not pinned");
        let authority = authority_for(&opts);
        let store = Arc::new(MemoryStore::new());
        let stall = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (recorded, calls) = Shared::recording_with_stall(&store, Arc::clone(&stall));
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
                    share_among,
                    min_idle_to_evict,
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
            stall,
        }
    }

    fn stall_loads(&self, on: bool) {
        self.stall.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// Give `id` a lease row, released: a session a writer has used.
    async fn make_existing(&self, id: &str) {
        let writer = LeaseHolder::for_this_process(&AgentId::new("another-writer"));
        let session = SessionId::new(id);
        self.store
            .acquire_lease(&session, &writer, LEASE_TTL)
            .await
            .expect("acquire");
        self.store
            .release_lease(&session, &writer)
            .await
            .expect("release");
    }

    /// Wait until the store has been asked `method` for `id` `n` times.
    async fn until_called(&self, method: &str, id: &str, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.count(method, id) < n {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {method} {id}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
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
/// of them is served by the session it attached. The load is held until
/// all eight have arrived, so they overlap by construction (review L7).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_requests_attach_once() {
    let od = OnDemand::start(16, Duration::from_secs(900), 0).await;
    let id = "od-u-flight";
    let path = format!("/mcp/s/{id}");
    od.stall_loads(true);
    let mut requests = tokio::task::JoinSet::new();
    for i in 0..8 {
        let path = path.clone();
        let addr = od.addr;
        requests.spawn(async move { initialize_as(addr, &path, Some(&bearer("maker"))).await });
        if i == 0 {
            // The first one's attach is in its (stalled) load: every later
            // request finds the flight in its slot.
            od.until_called("load_session", id, 1).await;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    od.stall_loads(false);
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
    // The probe runs before any place is reserved (review H1): one read,
    // no acquire, nothing evicted.
    assert_eq!(od.count("read_lease", "od-u-next"), 1, "probed once");
    assert_eq!(od.count("acquire_lease", "od-u-next"), 0, "never acquired");

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

/// Review H1: at the cap, requests from a credential without `create` for
/// ids that do not exist (and for an erased one, and one another writer
/// holds) are answered after the probe and evict nothing: no live session
/// is detached and no lease released. An existing id from the same
/// credential then does evict, so eviction was possible all along.
#[tokio::test]
async fn requests_that_will_not_attach_never_evict() {
    // Two on-demand places, a share of one each for maker and reader.
    let od = OnDemand::start(3, Duration::from_secs(900), 0).await;
    let (m1, m2) = ("od-u-m1", "od-u-m2");
    for id in [m1, m2] {
        initialize_as(od.addr, &format!("/mcp/s/{id}"), Some(&bearer("maker"))).await;
    }
    let reference =
        crate::test_util::on_the_wire_as(od.addr, "GET", "/not/routed", Some(&bearer("reader")))
            .await;
    for i in 0..4 {
        let path = format!("/mcp/s/od-u-nope-{i}");
        assert_eq!(
            crate::test_util::on_the_wire_as(od.addr, "POST", &path, Some(&bearer("reader"))).await,
            reference,
            "absent, no create, at the cap: still the uniform 404"
        );
    }
    // Erased, and held by another live writer: answered, nothing evicted.
    let writer = LeaseHolder::for_this_process(&AgentId::new("another-writer"));
    let erased = SessionId::new("od-u-gone");
    od.store
        .acquire_lease(&erased, &writer, LEASE_TTL)
        .await
        .expect("acquire");
    od.store
        .erase_session(&erased, &writer)
        .await
        .expect("erase");
    let gone = http_as(
        od.addr,
        "POST",
        "/mcp/s/od-u-gone",
        Some(&bearer("reader")),
        None,
        "{}",
    )
    .await;
    assert_eq!(gone.status, 410, "{}", gone.body);
    od.store
        .acquire_lease(&SessionId::new("od-u-taken"), &writer, LEASE_TTL)
        .await
        .expect("the other writer holds it");
    let taken = http_as(
        od.addr,
        "POST",
        "/mcp/s/od-u-taken",
        Some(&bearer("maker")),
        None,
        "{}",
    )
    .await;
    assert_eq!(taken.status, 503, "{}", taken.body);
    assert_eq!(
        od.count("acquire_lease", "od-u-taken"),
        0,
        "the probe saw the live holder: no acquire"
    );
    for id in [m1, m2] {
        assert!(od.attached(id).is_some(), "{id} was not evicted");
        assert_eq!(od.count("release_lease", id), 0, "{id} kept its lease");
    }

    // An existing session for the reader, which is under its share while
    // maker is over its own: maker's least recently used session goes.
    od.make_existing("od-u-real").await;
    initialize_as(od.addr, "/mcp/s/od-u-real", Some(&bearer("reader"))).await;
    assert!(od.attached("od-u-real").is_some());
    assert!(
        od.attached(m1).is_none(),
        "the least recently used was evicted"
    );
    assert!(od.attached(m2).is_some());
    od.close().await;
}

/// Review M1: a request routed to a live session holds it in flight from
/// the routing, so an attach at the cap cannot evict it before its call
/// starts; once the request is done the session can be evicted.
#[tokio::test]
async fn a_routed_request_holds_its_session_against_eviction() {
    let od = OnDemand::start(2, Duration::from_secs(900), 0).await;
    let held = "od-u-held";
    initialize_as(od.addr, &format!("/mcp/s/{held}"), Some(&bearer("maker"))).await;
    let routed = od.registry.get_or_attach(held, asking("maker", true)).await;
    assert!(matches!(routed.lookup, Lookup::Live(_)));
    assert!(routed.in_flight.is_some());

    let refused = http_as(
        od.addr,
        "POST",
        "/mcp/s/od-u-next",
        Some(&bearer("maker")),
        None,
        "{}",
    )
    .await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert_eq!(refused.header("retry-after").as_deref(), Some("5"));
    assert!(od.attached(held).is_some(), "the routed session stays");

    drop(routed);
    initialize_as(od.addr, "/mcp/s/od-u-next", Some(&bearer("maker"))).await;
    assert!(
        od.attached(held).is_none(),
        "evicted once its request is done"
    );
    od.close().await;
}

/// Review M1 and M4: a session used within the eviction floor is not
/// evicted; past it, it is.
#[tokio::test]
async fn a_session_used_just_now_is_not_evicted() {
    let floor = Duration::from_millis(500);
    let od = OnDemand::start_with(2, Duration::from_secs(900), 0, floor).await;
    initialize_as(od.addr, "/mcp/s/od-u-fresh", Some(&bearer("maker"))).await;
    let refused = http_as(
        od.addr,
        "POST",
        "/mcp/s/od-u-other",
        Some(&bearer("maker")),
        None,
        "{}",
    )
    .await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert!(od.attached("od-u-fresh").is_some());
    tokio::time::sleep(floor + Duration::from_millis(100)).await;
    initialize_as(od.addr, "/mcp/s/od-u-other", Some(&bearer("maker"))).await;
    assert!(od.attached("od-u-fresh").is_none());
    od.close().await;
}

/// Review M4: at the cap, a credential at its share evicts only its own
/// sessions; with none of its own idle it gets 503, and another
/// credential's session within its share is never taken.
#[tokio::test]
async fn a_credential_at_its_share_evicts_only_its_own_sessions() {
    // Two places, a share of one each.
    let od = OnDemand::start(3, Duration::from_secs(900), 0).await;
    od.make_existing("od-u-r1").await;
    initialize_as(od.addr, "/mcp/s/od-u-r1", Some(&bearer("reader"))).await;
    initialize_as(od.addr, "/mcp/s/od-u-m1", Some(&bearer("maker"))).await;
    // The reader's session is the least recently used, but not maker's to
    // take: maker's own goes.
    initialize_as(od.addr, "/mcp/s/od-u-m2", Some(&bearer("maker"))).await;
    assert!(od.attached("od-u-m1").is_none(), "maker's own was evicted");
    assert!(
        od.attached("od-u-r1").is_some(),
        "the reader's share is kept"
    );

    // Maker's only session busy: 503, and still not the reader's.
    let busy = od.attached("od-u-m2").expect("attached").activity.enter();
    let refused = http_as(
        od.addr,
        "POST",
        "/mcp/s/od-u-m3",
        Some(&bearer("maker")),
        None,
        "{}",
    )
    .await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert!(od.attached("od-u-r1").is_some());
    assert!(od.attached("od-u-m2").is_some());
    drop(busy);
    od.close().await;
}

/// Review M3: absent (to a credential without `create`) and erased are
/// answered from memory for `NEGATIVE_TTL`; a credential with `create` is
/// not stopped by an absent entry; past the TTL the store is asked again.
#[tokio::test]
async fn negative_outcomes_are_cached_and_create_is_not_blocked() {
    let od = OnDemand::start(16, Duration::from_secs(900), 0).await;
    let absent = "od-u-later";
    for _ in 0..3 {
        let reply = http_as(
            od.addr,
            "POST",
            &format!("/mcp/s/{absent}"),
            Some(&bearer("reader")),
            None,
            "{}",
        )
        .await;
        assert_eq!(reply.status, 404, "{}", reply.body);
    }
    assert_eq!(
        od.count("read_lease", absent),
        1,
        "probed once, then cached"
    );
    // Maker may create it, cached absent or not.
    initialize_as(od.addr, &format!("/mcp/s/{absent}"), Some(&bearer("maker"))).await;
    assert!(od.attached(absent).is_some());
    assert_eq!(od.count("acquire_lease", absent), 1);

    let writer = LeaseHolder::for_this_process(&AgentId::new("another-writer"));
    let erased = "od-u-erased-c";
    od.store
        .acquire_lease(&SessionId::new(erased), &writer, LEASE_TTL)
        .await
        .expect("acquire");
    od.store
        .erase_session(&SessionId::new(erased), &writer)
        .await
        .expect("erase");
    for who in ["maker", "reader", "maker"] {
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
    assert_eq!(od.count("read_lease", erased), 1, "cached after one probe");

    tokio::time::pause();
    tokio::time::advance(crate::mcp::serve::registry::NEGATIVE_TTL).await;
    tokio::time::resume();
    let reply = http_as(
        od.addr,
        "POST",
        &format!("/mcp/s/{erased}"),
        Some(&bearer("reader")),
        None,
        "{}",
    )
    .await;
    assert_eq!(reply.status, 410);
    assert_eq!(
        od.count("read_lease", erased),
        2,
        "asked again past the TTL"
    );
    od.close().await;
}

/// Review M2: an attach whose store hangs gives its waiters 503 after
/// `ATTACH_WAIT`, and is abandoned after `ATTACH_TIMEOUT`: its slot is
/// removed, the lease it took is released, and the permit is free for the
/// next attach (paused clock).
#[tokio::test]
async fn an_attach_that_hangs_times_out_and_does_not_stick() {
    let od = OnDemand::start(16, Duration::from_secs(900), 0).await;
    let id = "od-u-hang";
    od.stall_loads(true);
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let routed = od.registry.get_or_attach(id, asking("maker", true)).await;
    match routed.lookup {
        Lookup::Unavailable { retry_after } => {
            assert_eq!(retry_after, crate::mcp::serve::registry::ATTACH_BUSY_RETRY)
        }
        _ => panic!("expected 503 while the attach hangs"),
    }
    assert!(started.elapsed() >= crate::mcp::serve::registry::ATTACH_WAIT);
    assert!(
        matches!(od.registry.lookup(id), Lookup::Unavailable { .. }),
        "still attaching"
    );
    // Past the attach's own bound: abandoned and cleared.
    tokio::time::sleep(crate::mcp::serve::registry::ATTACH_TIMEOUT).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::resume();
    until(
        Duration::from_secs(10),
        "the abandoned attach to clear",
        || matches!(od.registry.lookup(id), Lookup::NotHosted),
    )
    .await;
    od.registry.join_detaches().await;
    assert_eq!(od.lease(id).await.holder, RELEASED_HOLDER, "lease released");
    // A busy outcome is not cached: the next request attaches.
    od.stall_loads(false);
    initialize_as(od.addr, &format!("/mcp/s/{id}"), Some(&bearer("maker"))).await;
    assert!(od.attached(id).is_some());
    od.close().await;
}

/// Review L1: an attach that dies without an outcome (a panic, here an
/// abort) wakes its waiters with an error, clears its `Attaching` slot so
/// it holds no place, and releases the lease it took.
#[tokio::test]
async fn an_attach_that_dies_answers_its_waiters_and_frees_its_place() {
    // One on-demand place.
    let od = OnDemand::start(2, Duration::from_secs(900), 0).await;
    let id = "od-u-dies";
    od.stall_loads(true);
    let registry = Arc::clone(&od.registry);
    let waiter = tokio::spawn(async move {
        registry
            .get_or_attach(id, asking("maker", true))
            .await
            .lookup
    });
    od.until_called("load_session", id, 1).await;
    od.registry.abort_tasks();
    let answer = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the waiter is woken")
        .expect("the waiter task");
    assert!(matches!(answer, Lookup::Failed), "an error, not a hang");
    assert!(
        matches!(od.registry.lookup(id), Lookup::NotHosted),
        "slot cleared"
    );
    od.stall_loads(false);
    od.registry.join_detaches().await;
    assert_eq!(od.lease(id).await.holder, RELEASED_HOLDER, "lease released");
    // The place is free: another session attaches without evicting.
    initialize_as(od.addr, "/mcp/s/od-u-after", Some(&bearer("maker"))).await;
    assert!(od.attached("od-u-after").is_some());
    od.close().await;
}

/// Review L7 (design R5): a shutdown during an on-demand attach abandons
/// it promptly and releases the lease it took.
#[tokio::test]
async fn the_shutdown_during_an_on_demand_attach_releases_its_lease() {
    let od = OnDemand::start(16, Duration::from_secs(900), 0).await;
    let id = "od-u-sigterm";
    od.stall_loads(true);
    let registry = Arc::clone(&od.registry);
    let waiter = tokio::spawn(async move {
        registry
            .get_or_attach(id, asking("maker", true))
            .await
            .lookup
    });
    od.until_called("load_session", id, 1).await;
    let set = tokio::time::timeout(Duration::from_secs(5), od.registry.close_set())
        .await
        .expect("the shutdown does not wait on the stalled attach");
    assert!(set.iter().all(|s| s.id().as_str() != id));
    tokio::time::timeout(Duration::from_secs(5), od.registry.join_detaches())
        .await
        .expect("the attach task ends");
    let answer = waiter.await.expect("the waiter task");
    assert!(matches!(answer, Lookup::Unavailable { .. }));
    assert_eq!(od.lease(id).await.holder, RELEASED_HOLDER);
    for session in set {
        session.mem.close().await.expect("close");
    }
}

/// Review L7: out of scope, against a registry that attaches on demand,
/// is the byte-identical 404 with no store call: the reader asking outside
/// its prefix, and a pinned-only credential asking inside it.
#[tokio::test]
async fn out_of_scope_requests_make_no_store_call_on_demand() {
    let od = OnDemand::start(16, Duration::from_secs(900), 0).await;
    let reference =
        crate::test_util::on_the_wire_as(od.addr, "GET", "/not/routed", Some(&bearer("reader")))
            .await;
    let before = od.calls.len();
    for (who, path) in [
        ("reader", "/mcp/s/other-x"),
        ("pinned", "/mcp/s/od-u-x"),
        ("pinned", "/mcp/s/od-u-y"),
    ] {
        assert_eq!(
            crate::test_util::on_the_wire_as(od.addr, "POST", path, Some(&bearer(who))).await,
            reference,
            "{who} {path}"
        );
    }
    assert!(
        od.calls.since(before).is_empty(),
        "{:?}",
        od.calls.since(before)
    );
    od.close().await;
}

/// Review L7: the idle sweeper skips a session with a request in flight,
/// however long it has been idle, and takes it once the request is done.
#[tokio::test]
async fn the_idle_sweep_skips_a_session_in_use() {
    let idle = Duration::from_secs(60);
    let od = OnDemand::start(16, idle, 0).await;
    let id = "od-u-busy-idle";
    initialize_as(od.addr, &format!("/mcp/s/{id}"), Some(&bearer("maker"))).await;
    let routed = od.registry.get_or_attach(id, asking("maker", true)).await;
    tokio::time::pause();
    tokio::time::advance(idle * 3).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(od.attached(id).is_some(), "in use: never idle");
    drop(routed);
    tokio::time::advance(idle * 2).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::resume();
    until(Duration::from_secs(10), "the idle detach", || {
        matches!(od.registry.lookup(id), Lookup::NotHosted)
    })
    .await;
    od.close().await;
}

/// Sonnet review L-a: an attach that panics after it admitted its session
/// leaves the session live, tells its waiters so, and caches no `Failed`
/// outcome. Once the session detaches, the next request attaches it again
/// at once rather than getting 503 for `NEGATIVE_TTL` (30 s).
#[tokio::test]
async fn a_panic_after_admission_does_not_block_the_session() {
    let od = OnDemand::start(16, Duration::from_secs(900), 0).await;
    let id = "od-u-panics-late";
    od.registry.panic_after_next_admit();
    let routed = od.registry.get_or_attach(id, asking("maker", true)).await;
    assert!(
        matches!(routed.lookup, Lookup::Live(_)),
        "the waiter is served by the session the attach admitted"
    );
    drop(routed);
    od.registry.join_detaches().await;
    assert!(od.attached(id).is_some(), "still live after the panic");
    assert_ne!(
        od.lease(id).await.holder,
        RELEASED_HOLDER,
        "the live session keeps its lease"
    );

    od.registry
        .detach(id, crate::mcp::serve::registry::DetachReason::Idle)
        .await;
    assert!(matches!(od.registry.lookup(id), Lookup::NotHosted));
    until(Duration::from_secs(10), "the detached handle to go", || {
        od.attached(id).is_none()
    })
    .await;
    // No clock advance: a cached `Failed` would answer this for 30 s.
    let mut lookup = od.registry.get_or_attach(id, asking("maker", true)).await;
    for _ in 0..50 {
        match lookup.lookup {
            // The detached handle may still be going (`PreviousHandle`).
            Lookup::Unavailable { .. } => {
                tokio::time::sleep(Duration::from_millis(20)).await;
                lookup = od.registry.get_or_attach(id, asking("maker", true)).await;
            }
            _ => break,
        }
    }
    assert!(
        matches!(lookup.lookup, Lookup::Live(_)),
        "attached again, not refused from the negative cache"
    );
    drop(lookup);
    assert_eq!(od.count("acquire_lease", id), 2);
    od.close().await;
}

/// Sonnet review L-b: a flood of distinct absent ids stays bounded. An
/// attach that has not probed yet takes no place, so the ones waiting for
/// their probe are capped at `2 × max_attached`; past that a request gets
/// 503 with `Retry-After` at once, adds no slot and starts no attach. Here
/// both attach permits are held by two stalled loads, so nothing can probe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flood_of_absent_ids_stays_bounded() {
    let max_attached = 4;
    let cap = 2 * max_attached;
    let od = OnDemand::start(max_attached, Duration::from_secs(900), 0).await;
    od.stall_loads(true);
    let mut holders = Vec::new();
    for id in ["od-u-hold-1", "od-u-hold-2"] {
        let registry = Arc::clone(&od.registry);
        holders.push(tokio::spawn(async move {
            registry
                .get_or_attach(id, asking("maker", true))
                .await
                .lookup
        }));
        od.until_called("load_session", id, 1).await;
    }

    let flood: Vec<String> = (0..30).map(|i| format!("od-u-flood-{i}")).collect();
    let mut waiting = Vec::new();
    let mut refused = 0;
    for id in &flood {
        let registry = Arc::clone(&od.registry);
        let asked = id.clone();
        let task = tokio::spawn(async move {
            registry
                .get_or_attach(&asked, asking("reader", false))
                .await
        });
        // Under the cap the request waits on its attach's slot; past it
        // the request is answered at once.
        until(Duration::from_secs(10), "a slot or an answer", || {
            task.is_finished() || matches!(od.registry.lookup(id), Lookup::Unavailable { .. })
        })
        .await;
        if task.is_finished() {
            match task.await.expect("the request task").lookup {
                Lookup::Unavailable { retry_after } => {
                    assert_eq!(retry_after, crate::mcp::serve::registry::ATTACH_BUSY_RETRY);
                    refused += 1;
                }
                _ => panic!("past the cap: 503 with Retry-After"),
            }
        } else {
            waiting.push(task);
        }
    }
    assert_eq!(waiting.len(), cap, "at most 2 x max_attached attaches wait");
    assert_eq!(refused, flood.len() - cap);
    let attaching = flood
        .iter()
        .filter(|id| matches!(od.registry.lookup(id), Lookup::Unavailable { .. }))
        .count();
    assert_eq!(attaching, cap, "no slot past the cap");
    assert!(
        flood.iter().all(|id| od.count("read_lease", id) == 0),
        "nothing probed while the permits are held"
    );
    // The same over HTTP: 503 with Retry-After, and no slot.
    let reply = http_as(
        od.addr,
        "POST",
        "/mcp/s/od-u-flood-http",
        Some(&bearer("reader")),
        None,
        "{}",
    )
    .await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert_eq!(reply.header("retry-after").as_deref(), Some("5"));
    assert!(matches!(
        od.registry.lookup("od-u-flood-http"),
        Lookup::NotHosted
    ));

    // Once the permits are free the waiting attaches probe and end, and the
    // cap no longer refuses.
    od.stall_loads(false);
    for task in waiting {
        let routed = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the waiting attach ends")
            .expect("the request task");
        assert!(matches!(routed.lookup, Lookup::NotHosted));
    }
    for holder in holders {
        let lookup = holder.await.expect("the holder task");
        assert!(matches!(lookup, Lookup::Live(_)));
    }
    let reply = http_as(
        od.addr,
        "POST",
        "/mcp/s/od-u-flood-after",
        Some(&bearer("reader")),
        None,
        "{}",
    )
    .await;
    assert_eq!(reply.status, 404, "{}", reply.body);
    od.registry.join_detaches().await;
    od.close().await;
}

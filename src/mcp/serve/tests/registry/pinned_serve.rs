//! #32 PR 4 review M1/M2: the real multi-session serve (`serve_pinned`),
//! in-process, through its test seams (`PinnedSeams`): the registry it
//! builds and the pre-arm it shuts down on.
//!
//! The store is one `MemoryStore` shared by identity between the serve and
//! the test, which plays the other writer: it expires a session's lease,
//! takes it with a token one higher, latches the serve's fence as the
//! heartbeat would, and later releases the session so the serve's
//! background retry wins it back.

use super::*;
use crate::mcp::serve::{serve_pinned_with, PinnedSeams};
use crate::store::lease::{LeaseHolder, LEASE_TTL};
use crate::store::{
    Capabilities, EraseOutcome, LeaseInfo, LeaseOutcome, RecallBackfillReport, SessionFlushStats,
};
use crate::types::{
    AgentId, CanonizationEvent, GraphSnapshot, MutationBatch, NodeId, Scored, SessionId, StoreError,
};
use chrono::{DateTime, Utc};

/// `Arc<MemoryStore>` as a `GraphStore`, so the serve under test and the
/// test's other writer share one store, as two processes share a database.
/// While the flag is set, every `load_session` parks forever: a store that
/// stalls under an attach that has already taken its lease.
struct Shared(Arc<MemoryStore>, Arc<std::sync::atomic::AtomicBool>);

impl Shared {
    fn over(store: &Arc<MemoryStore>) -> Box<dyn GraphStore> {
        Box::new(Self(Arc::clone(store), Default::default()))
    }
}

#[async_trait::async_trait]
impl GraphStore for Shared {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.0.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    async fn preflight_schema(&self) -> Result<(), StoreError> {
        self.0.preflight_schema().await
    }
    fn vector_dimensions(&self) -> Option<usize> {
        self.0.vector_dimensions()
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.0.flush(batch, token).await
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        if self.1.load(std::sync::atomic::Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
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
    fn exact_vector_scan(&self) -> bool {
        self.0.exact_vector_scan()
    }
    fn holder_derives_from_graph(&self) -> bool {
        self.0.holder_derives_from_graph()
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
    async fn erase_session(
        &self,
        session: &SessionId,
        eraser: &LeaseHolder,
    ) -> Result<EraseOutcome, StoreError> {
        self.0.erase_session(session, eraser).await
    }
    async fn backfill_recall_index(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<Option<RecallBackfillReport>, StoreError> {
        self.0.backfill_recall_index(session, holder).await
    }
    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.0.acquire_lease(session, holder, ttl).await
    }
    async fn read_lease(&self, session: &SessionId) -> Result<Option<LeaseInfo>, StoreError> {
        self.0.read_lease(session).await
    }
    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.0.refresh_lease(session, holder, ttl).await
    }
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        self.0.release_lease(session, holder).await
    }
    async fn record_lease_refusal(
        &self,
        session: &SessionId,
        refused_by: &str,
        current_holder: &str,
    ) -> Result<(), StoreError> {
        self.0
            .record_lease_refusal(session, refused_by, current_holder)
            .await
    }
    async fn pending_lease_refusals(
        &self,
        session: &SessionId,
        since: DateTime<Utc>,
    ) -> Result<Vec<crate::store::lease::LeaseRefusal>, StoreError> {
        self.0.pending_lease_refusals(session, since).await
    }
    async fn write_flush_stats(
        &self,
        session: &SessionId,
        stats: &SessionFlushStats,
    ) -> Result<(), StoreError> {
        self.0.write_flush_stats(session, stats).await
    }
    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionFlushStats>, StoreError> {
        self.0.read_flush_stats(session).await
    }
}

/// The agent the serve under test writes as.
const AGENT: &str = "agent-a";

/// The serve under test, running in the background.
struct PinnedServe {
    registry: Arc<SessionRegistry>,
    early: EarlyShutdown,
    task: tokio::task::JoinHandle<Result<(), LamboError>>,
}

impl PinnedServe {
    /// Start `serve_pinned` over `store` for `sessions` on a loopback port
    /// the kernel picks, with `opts` adjusted by `tweak`.
    async fn start(
        store: &Arc<MemoryStore>,
        sessions: &[&str],
        tweak: impl FnOnce(&mut ServeOptions),
    ) -> Self {
        let early = EarlyShutdown::unarmed();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let seams = PinnedSeams {
            early: Some(early.clone()),
            registry: Some(tx),
        };
        let opts = pinned_opts(sessions, tweak);
        let backends = backends_over(Shared::over(store), fast_config(1_000));
        let task = tokio::spawn(serve_pinned_with(opts, backends, seams));
        let registry = tokio::time::timeout(Duration::from_secs(20), rx)
            .await
            .expect("the serve starts")
            .expect("the serve hands out its registry");
        Self {
            registry,
            early,
            task,
        }
    }

    /// Shut the serve down as a SIGTERM would, and return its outcome.
    async fn stop(self) -> Result<(), LamboError> {
        self.early.simulate_signal();
        tokio::time::timeout(Duration::from_secs(30), self.task)
            .await
            .expect("the serve shuts down")
            .expect("the serve task does not panic")
    }

    fn attached(&self, id: &str) -> Option<Arc<AttachedSession>> {
        self.registry
            .attached()
            .into_iter()
            .find(|s| s.id().as_str() == id)
    }
}

/// `ServeOptions` for an HTTP serve pinning `sessions`, the first the
/// default, on a port the kernel picks.
fn pinned_opts(sessions: &[&str], tweak: impl FnOnce(&mut ServeOptions)) -> ServeOptions {
    let mut opts = ServeOptions::new(sessions[0], AGENT);
    opts.sessions = sessions.iter().map(|s| s.to_string()).collect();
    opts.transport = Transport::Http;
    opts.port = 0;
    tweak(&mut opts);
    opts
}

/// The other writer: a different agent in this process, so its holder
/// token differs from the serve's.
fn other_writer() -> LeaseHolder {
    LeaseHolder::for_this_process(&AgentId::new("another-writer"))
}

/// The serve's own holder token.
fn serve_token() -> String {
    LeaseHolder::for_this_process(&AgentId::new(AGENT)).token()
}

async fn lease(store: &MemoryStore, id: &str) -> LeaseInfo {
    store
        .read_lease(&SessionId::new(id))
        .await
        .expect("read")
        .expect("a lease row")
}

/// The other writer takes session `id` from the serve: its lease is
/// expired, the other writer acquires it (a token one higher), and the
/// serve's fence is latched exactly as its heartbeat would latch it on the
/// next refresh. Returns the other writer's token.
async fn take_over(serve: &PinnedServe, store: &MemoryStore, id: &str) -> u64 {
    let session = SessionId::new(id);
    store.force_expire_lease(&session);
    let other = other_writer();
    let token = match store
        .acquire_lease(&session, &other, LEASE_TTL)
        .await
        .expect("acquire")
    {
        LeaseOutcome::Acquired(info) => info.token,
        LeaseOutcome::Held { current, .. } => panic!("still held by {}", current.holder),
    };
    let held = serve.attached(id).expect("attached before the takeover");
    held.mem.simulate_lease_loss_to(&other.token());
    token
}

/// Wait up to `budget` for `done`.
async fn until(budget: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + budget;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// M1: a detach drops the registry's handle on the session, so the fenced
/// `Memory` goes (its graph and its second-writer registration with it)
/// before the background retry attaches the session again. Before the fix
/// `serve_pinned` held every startup handle until it returned: the old
/// handle stayed resident and the re-attach logged a false
/// `SecondSessionWriter` ERROR.
#[tokio::test]
async fn a_detached_session_s_handle_is_gone_before_it_is_attached_again() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::INFO);
    let store = Arc::new(MemoryStore::new());
    let serve = PinnedServe::start(&store, &["m1-a", "m1-b"], |_| {}).await;

    let old = Arc::downgrade(&serve.attached("m1-a").expect("a attached").mem);
    let first = lease(&store, "m1-a").await.token;
    let theirs = take_over(&serve, &store, "m1-a").await;
    assert_eq!(theirs, first + 1);
    until(Duration::from_secs(10), "a's detach", || {
        logs.lines()
            .iter()
            .any(|l| l.contains("session detach finished") && l.contains("m1-a"))
    })
    .await;
    until(Duration::from_secs(5), "a's old handle to drop", || {
        old.strong_count() == 0
    })
    .await;

    store
        .release_lease(&SessionId::new("m1-a"), &other_writer())
        .await
        .expect("the other writer releases a");
    until(PINNED_RETRY * 3, "a's re-election", || {
        serve.attached("m1-a").is_some()
    })
    .await;
    let ours = lease(&store, "m1-a").await;
    assert_eq!(ours.holder, serve_token(), "the serve holds a again");
    assert_eq!(ours.token, theirs + 1, "a fresh fencing token");
    assert!(
        !logs
            .lines()
            .iter()
            .any(|l| l.contains("SecondSessionWriter")),
        "a false second-writer report: {:?}",
        logs.lines()
    );

    serve.stop().await.expect("a clean shutdown");
}

/// The HTTP address the serve bound, from its listening line.
async fn bound_addr(logs: &crate::test_util::CapturedLogs) -> SocketAddr {
    let mut found = None;
    until(Duration::from_secs(10), "the listening line", || {
        found = logs
            .lines()
            .iter()
            .filter(|l| l.contains("listening on /mcp"))
            .find_map(|l| {
                let at = l.find("127.0.0.1:")?;
                let rest = &l[at..];
                let end = rest
                    .char_indices()
                    .skip("127.0.0.1:".len())
                    .find(|(_, c)| !c.is_ascii_digit())
                    .map_or(rest.len(), |(i, _)| i);
                rest[..end].parse().ok()
            });
        found.is_some()
    })
    .await;
    found.expect("a bound address")
}

/// M2: design §4.2's whole cycle through the real multi-session serve and
/// its HTTP router. A session loses its lease to another writer, is
/// detached (503 with `Retry-After`, its MCP sessions gone) while the other
/// session serves on through the same MCP session it already had; the
/// other writer releases; the background retry takes the session back with
/// a new fencing token and it serves again. No false `SecondSessionWriter`
/// line, and the shutdown releases both leases, keeping their tokens.
#[tokio::test]
async fn a_lost_lease_is_detached_re_elected_and_served_again_end_to_end() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::INFO);
    let store = Arc::new(MemoryStore::new());
    let serve = PinnedServe::start(&store, &["e2e-a", "e2e-b"], |_| {}).await;
    let addr = bound_addr(&logs).await;

    let (a, _) = initialize(addr, "/mcp/s/e2e-a").await;
    let (b, _) = initialize(addr, "/mcp/s/e2e-b").await;
    let first = lease(&store, "e2e-a").await.token;
    let b_token = lease(&store, "e2e-b").await.token;

    let theirs = take_over(&serve, &store, "e2e-a").await;
    assert_eq!(theirs, first + 1);
    until(Duration::from_secs(10), "a's detach", || {
        logs.lines()
            .iter()
            .any(|l| l.contains("session detach finished") && l.contains("e2e-a"))
    })
    .await;

    // a: 503 with Retry-After, for a new client and for its old MCP session.
    for sid in [None, Some(a.as_str())] {
        let refused = http(
            addr,
            "POST",
            "/mcp/s/e2e-a",
            sid,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
        )
        .await;
        assert_eq!(refused.status, 503, "{}", refused.body);
        assert!(refused.header("retry-after").is_some(), "{}", refused.head);
    }
    // b: unaffected, on the MCP session it opened before the loss.
    derive(addr, "/mcp/s/e2e-b", &b, &[MARKER_B]).await;
    assert_eq!(stats(addr, "/mcp/s/e2e-b", &b).await["concept_count"], 1);

    store
        .release_lease(&SessionId::new("e2e-a"), &other_writer())
        .await
        .expect("the other writer releases a");
    until(PINNED_RETRY * 3, "a's re-election", || {
        serve.attached("e2e-a").is_some()
    })
    .await;
    let ours = lease(&store, "e2e-a").await;
    assert_eq!(ours.holder, serve_token());
    assert_eq!(ours.token, theirs + 1, "a fresh fencing token");

    // a serves again, through a new MCP session.
    let (a2, init) = initialize(addr, "/mcp/s/e2e-a").await;
    assert!(
        init["instructions"]
            .as_str()
            .unwrap()
            .contains("session 'e2e-a'"),
        "{init}"
    );
    derive(addr, "/mcp/s/e2e-a", &a2, &[MARKER_A]).await;
    assert_eq!(stats(addr, "/mcp/s/e2e-a", &a2).await["session"], "e2e-a");
    assert_eq!(
        lease(&store, "e2e-b").await.token,
        b_token,
        "b was never re-elected"
    );
    assert!(
        !logs
            .lines()
            .iter()
            .any(|l| l.contains("SecondSessionWriter")),
        "a false second-writer report: {:?}",
        logs.lines()
    );

    serve.stop().await.expect("a clean shutdown");
    for (id, token) in [("e2e-a", ours.token), ("e2e-b", b_token)] {
        let row = lease(&store, id).await;
        assert_eq!(row.holder, crate::store::lease::RELEASED_HOLDER, "{id}");
        assert_eq!(row.token, token, "{id}: the token is kept");
    }
}

/// M2: `serve_pinned`'s startup failure branch. A pinned session that
/// cannot be attached (here: erased, a tombstone no acquire takes) refuses
/// the start, after closing the sessions already acquired, so their leases
/// are released rather than left to lapse.
#[tokio::test]
async fn a_pinned_session_that_cannot_attach_refuses_the_start_and_releases_the_rest() {
    let store = Arc::new(MemoryStore::new());
    let operator = LeaseHolder::for_this_process(&AgentId::new("operator"));
    store
        .erase_session(&SessionId::new("fail-b"), &operator)
        .await
        .expect("erase b");

    let opts = pinned_opts(&["fail-a", "fail-b", "fail-c"], |_| {});
    let backends = backends_over(Shared::over(&store), fast_config(1_000));
    let err = tokio::time::timeout(
        Duration::from_secs(20),
        serve_pinned_with(opts, backends, PinnedSeams::default()),
    )
    .await
    .expect("the refusal is prompt")
    .expect_err("an erased pinned session refuses the start")
    .to_string();
    assert!(err.contains("erased"), "{err}");

    let a = lease(&store, "fail-a").await;
    assert_eq!(
        a.holder,
        crate::store::lease::RELEASED_HOLDER,
        "the session acquired first is released"
    );
    assert!(
        store
            .read_lease(&SessionId::new("fail-c"))
            .await
            .expect("read")
            .is_none(),
        "nothing after the failure is attempted"
    );
}

/// #32 review L2: the refusal poller keeps a session's cursor across a
/// detach and re-attach. A refusal booked before the lease loss is not
/// booked again after the re-election, which a fresh cursor (reaching back
/// one `LEASE_TTL`, filtering by this process's unchanged token) would do.
#[tokio::test]
async fn a_re_attached_session_does_not_book_a_refusal_twice() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::INFO);
    let dir = crate::test_util::ScratchDir::new("lambo-l2");
    let path = dir.join("ledger.jsonl");
    let store = Arc::new(MemoryStore::new());
    let ledger_path = path.clone();
    let serve = PinnedServe::start(&store, &["l2-a", "l2-b"], move |opts| {
        opts.ledger = Some(ledger_path);
    })
    .await;
    let booked = |path: &std::path::Path| {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("refused_takeover") && l.contains("loser-l2@"))
            .count()
    };

    store
        .record_lease_refusal(&SessionId::new("l2-a"), "loser-l2@host#1", &serve_token())
        .await
        .expect("a refusal against the serve");
    until(Duration::from_secs(10), "the refusal booked", || {
        booked(&path) == 1
    })
    .await;

    take_over(&serve, &store, "l2-a").await;
    until(Duration::from_secs(10), "a's detach", || {
        logs.lines()
            .iter()
            .any(|l| l.contains("session detach finished") && l.contains("l2-a"))
    })
    .await;
    store
        .release_lease(&SessionId::new("l2-a"), &other_writer())
        .await
        .expect("the other writer releases a");
    until(PINNED_RETRY * 3, "a's re-election", || {
        serve.attached("l2-a").is_some()
    })
    .await;
    // Several poll rounds over the re-attached session.
    tokio::time::sleep(Duration::from_millis(2_000)).await;

    serve.stop().await.expect("a clean shutdown");
    assert_eq!(booked(&path), 1, "the refusal was booked again");
}

/// #32 review L7: on the real serve's router (`serve_http`, its guard
/// layer included), every refused or unrouted path under `/mcp/` is
/// byte-identical on the wire to a path the server does not route at all:
/// status line, every header and the empty body, on every method. PR 1's
/// comparison ran on its own router only, so a fallback, a header or a
/// `.route_layer` added to the serve's would have passed.
#[tokio::test]
async fn the_serve_s_404_is_byte_identical_on_the_wire() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::INFO);
    let store = Arc::new(MemoryStore::new());
    let serve = PinnedServe::start(&store, &["wire-a", "wire-b"], |_| {}).await;
    let addr = bound_addr(&logs).await;

    let reference = crate::test_util::on_the_wire(addr, "GET", "/not/routed").await;
    assert!(
        reference.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "{reference}"
    );
    assert!(reference.ends_with("\r\n\r\n"), "empty body: {reference:?}");
    for method in ["GET", "POST", "DELETE", "PUT"] {
        for path in [
            "/not/routed",
            "/mcp/s/wire-unknown",
            "/mcp/s/.x",
            "/mcp/s/a%2Fb",
            "/mcp/s/wire%2Da",
            "/mcp/s/",
            "/mcp/s/wire-a/",
            "/mcp/s",
            "/mcp/",
        ] {
            assert_eq!(
                crate::test_util::on_the_wire(addr, method, path).await,
                reference,
                "{method} {path} must be the unrouted 404"
            );
        }
    }

    serve.stop().await.expect("a clean shutdown");
}

/// #32 review L8: the shutdown does not wait on a background attach. The
/// retry of a session held elsewhere takes the lease and then stalls in its
/// store load. J6's pre-arm, which that load is raced against, records no
/// signal here, as when every session was held at startup (nothing armed
/// it) or the shutdown did not start with a signal. Taking the close set
/// abandons the attach at once and releases the lease it took, rather than
/// waiting for the load while stage 3 holds the shutdown, then letting the
/// lease lapse.
#[tokio::test]
async fn the_shutdown_abandons_a_background_attach_and_releases_its_lease() {
    let store = Arc::new(MemoryStore::new());
    let stall = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let registry = new_registry(
        &["l8-a", "l8-b"],
        backends_over(
            Box::new(Shared(Arc::clone(&store), Arc::clone(&stall))),
            fast_config(1_000),
        ),
        32,
    );
    attach_or_hold(&registry, "l8-a").await;
    let b = SessionId::new("l8-b");
    store
        .acquire_lease(&b, &other_writer(), LEASE_TTL)
        .await
        .expect("the other writer takes b");
    attach_or_hold(&registry, "l8-b").await;
    registry.mark_started();
    registry.spawn_retry_loop();

    // b's next attach takes the lease and stalls in its load.
    stall.store(true, std::sync::atomic::Ordering::SeqCst);
    store
        .release_lease(&b, &other_writer())
        .await
        .expect("the other writer releases b");
    let deadline = Instant::now() + PINNED_RETRY * 3;
    while lease(&store, "l8-b").await.holder != serve_token() {
        assert!(Instant::now() < deadline, "the retry never took b's lease");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let set = tokio::time::timeout(Duration::from_secs(5), registry.close_set())
        .await
        .expect("the shutdown does not wait on a stalled background attach");
    assert_eq!(set.len(), 1, "only a was attached");
    assert_eq!(
        lease(&store, "l8-b").await.holder,
        crate::store::lease::RELEASED_HOLDER,
        "the abandoned attach's lease is released, not left to lapse"
    );
    for session in set {
        session.mem.close().await.expect("close a");
    }
}

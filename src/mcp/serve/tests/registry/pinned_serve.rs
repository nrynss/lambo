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
/// While the flag is set, every `load_session` parks until it is cleared: a
/// store that stalls under an attach that has already taken its lease.
///
/// Every call is also appended to the third field, `(method, session)`,
/// the recording wrapper #32 PR 5's "zero store calls" claim is measured
/// with (`registry::authority`).
pub(super) struct Shared(
    Arc<MemoryStore>,
    Arc<std::sync::atomic::AtomicBool>,
    Arc<StoreCalls>,
);

/// The calls a [`Shared`] store has seen, in order: the method and the
/// session it named (empty when it named none).
///
/// It also carries a gate for `flush` (#32 PR 7): while it is closed, every
/// flush parks inside the store, so a test can hold one in flight.
///
/// And (#32 PR 7 review) the faults an erase test injects, and the parked
/// flushes `erase_session` found in flight when it was entered.
#[derive(Default)]
pub(super) struct StoreCalls(
    parking_lot::Mutex<Vec<(&'static str, String)>>,
    FlushGate,
    Faults,
);

/// What a [`Shared`] store does wrong on purpose (#32 PR 7 review).
#[derive(Default)]
pub(super) struct Faults {
    /// [`EraseFault`] as a number, read at each `erase_session`.
    erase: std::sync::atomic::AtomicU8,
    /// While set, every `read_lease` fails.
    read_lease: std::sync::atomic::AtomicBool,
    /// [`StoreCalls::parked_flushes`] as each `erase_session` found it on
    /// entry.
    parked_at_erase: parking_lot::Mutex<Vec<usize>>,
}

/// How `erase_session` fails, when it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum EraseFault {
    /// It does not.
    None = 0,
    /// It fails before anything is erased (the store refused).
    BeforeCommit = 1,
    /// It erases, then fails (an index sweep, a lost commit reply).
    AfterCommit = 2,
}

/// Parks every `flush` while closed ([`StoreCalls::park_flushes`]).
#[derive(Default)]
pub(super) struct FlushGate {
    closed: std::sync::atomic::AtomicBool,
    parked: std::sync::atomic::AtomicUsize,
    opened: tokio::sync::Notify,
}

impl StoreCalls {
    /// From now on every `flush` parks inside the store until
    /// [`StoreCalls::release_flushes`].
    pub(super) fn park_flushes(&self) {
        self.1
            .closed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Let every parked `flush` (and every later one) through.
    pub(super) fn release_flushes(&self) {
        self.1
            .closed
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.1.opened.notify_waiters();
    }

    /// How many flushes are parked right now.
    pub(super) fn parked_flushes(&self) -> usize {
        self.1.parked.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Park while the gate is closed. The count goes down however the wait
    /// ends, a drop of the flush included.
    async fn pass_flush_gate(&self) {
        use std::sync::atomic::Ordering;
        struct Parked<'a>(&'a std::sync::atomic::AtomicUsize);
        impl Drop for Parked<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        if !self.1.closed.load(Ordering::SeqCst) {
            return;
        }
        self.1.parked.fetch_add(1, Ordering::SeqCst);
        let _parked = Parked(&self.1.parked);
        loop {
            let opened = self.1.opened.notified();
            tokio::pin!(opened);
            opened.as_mut().enable();
            if !self.1.closed.load(Ordering::SeqCst) {
                return;
            }
            opened.await;
        }
    }

    /// Make every later `erase_session` fail as `fault` says.
    pub(super) fn fail_erase(&self, fault: EraseFault) {
        self.2
            .erase
            .store(fault as u8, std::sync::atomic::Ordering::SeqCst);
    }

    /// Make every later `read_lease` fail (`fail`) or not.
    pub(super) fn fail_read_lease(&self, fail: bool) {
        self.2
            .read_lease
            .store(fail, std::sync::atomic::Ordering::SeqCst);
    }

    /// How many flushes were parked in flight at each `erase_session`'s
    /// entry, in order.
    pub(super) fn parked_at_erase(&self) -> Vec<usize> {
        self.2.parked_at_erase.lock().clone()
    }

    fn note(&self, method: &'static str, session: &str) {
        self.0.lock().push((method, session.to_string()));
    }

    /// How many calls so far.
    pub(super) fn len(&self) -> usize {
        self.0.lock().len()
    }

    /// The calls after the first `from`.
    pub(super) fn since(&self, from: usize) -> Vec<(&'static str, String)> {
        self.0.lock().get(from..).unwrap_or_default().to_vec()
    }
}

impl Shared {
    fn over(store: &Arc<MemoryStore>) -> Box<dyn GraphStore> {
        Box::new(Self(
            Arc::clone(store),
            Default::default(),
            Default::default(),
        ))
    }

    /// [`Shared::over`], handing back the call record too.
    pub(super) fn recording(store: &Arc<MemoryStore>) -> (Box<dyn GraphStore>, Arc<StoreCalls>) {
        Self::recording_with_stall(store, Default::default())
    }

    /// [`Shared::recording`], whose loads stall while `stall` is set.
    pub(super) fn recording_with_stall(
        store: &Arc<MemoryStore>,
        stall: Arc<std::sync::atomic::AtomicBool>,
    ) -> (Box<dyn GraphStore>, Arc<StoreCalls>) {
        let calls = Arc::new(StoreCalls::default());
        (
            Box::new(Self(Arc::clone(store), stall, Arc::clone(&calls))),
            calls,
        )
    }
}

#[async_trait::async_trait]
impl GraphStore for Shared {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.2.note("init_schema", "");
        self.0.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.2.note("capabilities", "");
        self.0.capabilities()
    }
    async fn preflight_schema(&self) -> Result<(), StoreError> {
        self.2.note("preflight_schema", "");
        self.0.preflight_schema().await
    }
    fn vector_dimensions(&self) -> Option<usize> {
        self.2.note("vector_dimensions", "");
        self.0.vector_dimensions()
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.2.note("flush", "");
        self.2.pass_flush_gate().await;
        let flushed = self.0.flush(batch, token).await;
        // Past the store: where a recall-tier mirror would run (#18), so a
        // test can tell one that lands after an erase.
        self.2.note("flushed", "");
        flushed
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.2.note("load_session", session.as_str());
        while self.1.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.0.load_session(session).await
    }
    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.2.note("keyword_candidates", session.as_str());
        self.0.keyword_candidates(session, tokens, limit).await
    }
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.2.note("vector_candidates", session.as_str());
        self.0.vector_candidates(session, embedding, limit).await
    }
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.2.note("vector_candidates_checked", session.as_str());
        self.0
            .vector_candidates_checked(session, embedding, expected_contract, limit)
            .await
    }
    fn exact_vector_scan(&self) -> bool {
        self.2.note("exact_vector_scan", "");
        self.0.exact_vector_scan()
    }
    fn holder_derives_from_graph(&self) -> bool {
        self.2.note("holder_derives_from_graph", "");
        self.0.holder_derives_from_graph()
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.2.note("blast_radius", session.as_str());
        self.0.blast_radius(session, node, min_edge_age, now).await
    }
    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        self.2.note("interaction_span", session.as_str());
        self.0.interaction_span(session, node, min_age, now).await
    }
    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.2
            .note("record_canonization", event.session_id.as_str());
        self.0.record_canonization(event, token).await
    }
    async fn erase_session(
        &self,
        session: &SessionId,
        eraser: &LeaseHolder,
    ) -> Result<EraseOutcome, StoreError> {
        self.2.note("erase_session", session.as_str());
        let parked = self.2.parked_flushes();
        self.2 .2.parked_at_erase.lock().push(parked);
        let fault = self.2 .2.erase.load(std::sync::atomic::Ordering::SeqCst);
        if fault == EraseFault::BeforeCommit as u8 {
            return Err(StoreError::Backend("test: the erase was refused".into()));
        }
        let erased = self.0.erase_session(session, eraser).await;
        if fault == EraseFault::AfterCommit as u8 && erased.is_ok() {
            return Err(StoreError::Backend(
                "test: the erase committed, then failed".into(),
            ));
        }
        erased
    }
    async fn backfill_recall_index(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<Option<RecallBackfillReport>, StoreError> {
        self.2.note("backfill_recall_index", session.as_str());
        self.0.backfill_recall_index(session, holder).await
    }
    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.2.note("acquire_lease", session.as_str());
        self.0.acquire_lease(session, holder, ttl).await
    }
    async fn read_lease(&self, session: &SessionId) -> Result<Option<LeaseInfo>, StoreError> {
        self.2.note("read_lease", session.as_str());
        if self
            .2
             .2
            .read_lease
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Backend("test: read_lease failed".into()));
        }
        self.0.read_lease(session).await
    }
    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.2.note("refresh_lease", session.as_str());
        self.0.refresh_lease(session, holder, ttl).await
    }
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        self.2.note("release_lease", session.as_str());
        self.0.release_lease(session, holder).await
    }
    async fn record_lease_refusal(
        &self,
        session: &SessionId,
        refused_by: &str,
        current_holder: &str,
    ) -> Result<(), StoreError> {
        self.2.note("record_lease_refusal", session.as_str());
        self.0
            .record_lease_refusal(session, refused_by, current_holder)
            .await
    }
    async fn pending_lease_refusals(
        &self,
        session: &SessionId,
        since: DateTime<Utc>,
    ) -> Result<Vec<crate::store::lease::LeaseRefusal>, StoreError> {
        self.2.note("pending_lease_refusals", session.as_str());
        self.0.pending_lease_refusals(session, since).await
    }
    async fn write_flush_stats(
        &self,
        session: &SessionId,
        stats: &SessionFlushStats,
    ) -> Result<(), StoreError> {
        self.2.note("write_flush_stats", session.as_str());
        self.0.write_flush_stats(session, stats).await
    }
    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionFlushStats>, StoreError> {
        self.2.note("read_flush_stats", session.as_str());
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
        let opts = pinned_opts(sessions, tweak);
        let seams = PinnedSeams {
            early: Some(early.clone()),
            registry: Some(tx),
            on_demand: crate::mcp::serve::authority::reaches_past_pinned(&opts),
        };
        let backends = backends_over(Shared::over(store), fast_config(1_000));
        let authority = authority_for(&opts);
        let task = tokio::spawn(serve_pinned_with(opts, backends, authority, seams));
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
/// cannot be attached (here: stored under another embedding contract, an
/// error no retry clears) refuses the start, after closing the sessions
/// already acquired, so their leases are released rather than left to
/// lapse. (An erased session, PR 4's example, no longer refuses the start
/// since #32 PR 7: see the next test.)
#[tokio::test]
async fn a_pinned_session_that_cannot_attach_refuses_the_start_and_releases_the_rest() {
    let store = Arc::new(MemoryStore::new());
    super::plant_foreign_contract(store.as_ref(), "fail-b", None).await;

    let opts = pinned_opts(&["fail-a", "fail-b", "fail-c"], |_| {});
    let backends = backends_over(Shared::over(&store), fast_config(1_000));
    let err = tokio::time::timeout(
        Duration::from_secs(20),
        serve_pinned_with(
            opts.clone(),
            backends,
            authority_for(&opts),
            PinnedSeams::default(),
        ),
    )
    .await
    .expect("the refusal is prompt")
    .expect_err("a pinned session under another contract refuses the start");
    assert!(
        !matches!(err, LamboError::Store(StoreError::StaleWrite(_))),
        "the refusal is the contract mismatch, not an erased session: {err}"
    );

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

/// #32 PR 7 (PR 4's note): an erased pinned session does not refuse the
/// start. It is served as erased (410, decided on the tombstone, not the
/// error text), the other sessions attach and serve, and nothing attaches
/// or recreates the erased one.
#[tokio::test]
async fn an_erased_pinned_session_is_served_as_erased_and_the_rest_start() {
    let store = Arc::new(MemoryStore::new());
    let operator = LeaseHolder::for_this_process(&AgentId::new("operator"));
    store
        .erase_session(&SessionId::new("gone-b"), &operator)
        .await
        .expect("erase b");

    let serve = PinnedServe::start(&store, &["gone-a", "gone-b", "gone-c"], |_| {}).await;
    assert!(serve.attached("gone-a").is_some());
    assert!(serve.attached("gone-c").is_some());
    assert!(serve.attached("gone-b").is_none());
    assert!(matches!(
        serve.registry.lookup("gone-b"),
        crate::mcp::serve::registry::Lookup::Erased
    ));
    serve.stop().await.expect("a clean shutdown");
    let b = lease(&store, "gone-b").await;
    assert!(crate::store::erase::is_tombstone(&b), "{b:?}");
    assert!(store.load_session(&SessionId::new("gone-b")).await.is_err());
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

/// #32 PR 5 (PR 4 review L6), on the real multi-session serve: with
/// credentials configured, a caller whose scope leaves out a session held
/// elsewhere gets the unrouted 404 for it, byte for byte, exactly as for a
/// name the serve does not host; only a caller in scope sees its 503.
#[tokio::test]
async fn an_out_of_scope_caller_gets_the_unrouted_404_for_a_held_session() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::INFO);
    let store = Arc::new(MemoryStore::new());
    store
        .acquire_lease(&SessionId::new("held-b"), &other_writer(), LEASE_TTL)
        .await
        .expect("the other writer holds b");
    let fake = |label: &str| ["fake", label, "held", "value"].join("-");
    let grant = |name: &str, sessions: &[&str]| {
        crate::surface::session::SessionGrant::new(
            name,
            crate::surface::session::SessionScope::new(
                sessions
                    .iter()
                    .map(|s| crate::surface::session::parse_addressed(s).expect("addressable")),
                false,
                None,
            ),
            Default::default(),
        )
    };
    let credentials = vec![
        crate::config::ServeCredential {
            grant: grant("only-a", &["held-a"]),
            token: SecretToken::new(fake("only-a")).expect("non-empty"),
        },
        crate::config::ServeCredential {
            grant: grant("both", &["held-a", "held-b"]),
            token: SecretToken::new(fake("both")).expect("non-empty"),
        },
    ];
    let serve = PinnedServe::start(&store, &["held-a", "held-b"], |opts| {
        opts.credentials = credentials;
    })
    .await;
    let addr = bound_addr(&logs).await;

    let only_a = format!("Bearer {}", fake("only-a"));
    let reference =
        crate::test_util::on_the_wire_as(addr, "GET", "/not/routed", Some(&only_a)).await;
    assert!(
        reference.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "{reference}"
    );
    for method in ["GET", "POST", "DELETE"] {
        for path in ["/mcp/s/held-b", "/mcp/s/held-unhosted"] {
            assert_eq!(
                crate::test_util::on_the_wire_as(addr, method, path, Some(&only_a)).await,
                reference,
                "{method} {path}"
            );
        }
    }
    let both = format!("Bearer {}", fake("both"));
    let in_scope =
        crate::test_util::on_the_wire_as(addr, "GET", "/mcp/s/held-b", Some(&both)).await;
    assert!(
        in_scope.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{in_scope}"
    );
    let no_token = crate::test_util::on_the_wire_as(addr, "GET", "/mcp/s/held-a", None).await;
    assert!(
        no_token.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{no_token}"
    );

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
            Box::new(Shared(
                Arc::clone(&store),
                Arc::clone(&stall),
                Default::default(),
            )),
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

/// #32 PR 6 through the real serve: one pinned session and a credential
/// whose prefix reaches past it make a registry that attaches on demand
/// (`DetachSession`, no election), and the shutdown releases the on-demand
/// session's lease with the pinned one's.
#[tokio::test]
async fn one_pinned_session_with_a_prefix_credential_attaches_on_demand() {
    let store = Arc::new(MemoryStore::new());
    let serve = PinnedServe::start(&store, &["od-e2e-pin"], |opts| {
        opts.credentials = vec![crate::config::ServeCredential {
            grant: crate::surface::session::SessionGrant::new(
                "app",
                crate::surface::session::SessionScope::new(
                    std::iter::empty(),
                    false,
                    Some(crate::surface::session::SessionPrefix::new("od-e2e-u-").expect("prefix")),
                ),
                crate::surface::session::SessionCapabilities {
                    create: true,
                    ..Default::default()
                },
            ),
            // Built at runtime, so no token-shaped literal sits here.
            token: SecretToken::new(["fake", "e2e", "ondemand", "value"].join("-"))
                .expect("non-empty"),
        }];
    })
    .await;
    assert!(serve.registry.attaches_on_demand());
    let id = "od-e2e-u-1";
    let routed = serve
        .registry
        .get_or_attach(
            id,
            crate::mcp::serve::registry::Requester {
                credential: "app",
                create: true,
            },
        )
        .await;
    assert!(matches!(
        routed.lookup,
        crate::mcp::serve::registry::Lookup::Live(_)
    ));
    assert!(
        routed.in_flight.is_some(),
        "a live answer holds the session"
    );
    drop(routed);
    assert_eq!(lease(&store, id).await.holder, serve_token());
    serve.stop().await.expect("a clean shutdown");
    for id in ["od-e2e-pin", id] {
        assert_eq!(
            lease(&store, id).await.holder,
            crate::store::lease::RELEASED_HOLDER,
            "{id}"
        );
    }
}

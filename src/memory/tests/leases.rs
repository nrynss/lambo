//! The single-writer lease: holders, refusals, fencing and release.

use super::*;

/// **L82-1, the second failure.** `serve` bounds `close()` and *drops* it on
/// timeout, so the release on close's success path never runs — the live run
/// exited with a stale lease row and wedged the session for the whole
/// `LEASE_TTL` on top of losing the tail.
///
/// Releasing is honest here: it asserts this process is gone, which it is.
#[tokio::test(start_paused = true)]
async fn an_abandoned_close_still_releases_the_lease() {
    let store = Arc::new(RoundTripStore::new(
        Arc::new(MemoryStore::new()),
        CostModel::PerMutation,
        Duration::from_millis(30),
    ));
    let mem = memory_on(store.clone(), "l82-1-stale-lease").await;
    at_cap_burst(&mem).await;

    assert!(
        tokio::time::timeout(crate::mcp::serve::CLOSE_FLUSH_GRACE, mem.close())
            .await
            .is_err(),
        "this test needs the close to be abandoned"
    );
    assert_eq!(
        store.releases(),
        0,
        "an abandoned close cannot have released on its own — that is the bug"
    );

    mem.release_lease_after_abandoned_close().await;
    assert_eq!(
        store.releases(),
        1,
        "the lease must be released on the way out even though the tail was lost"
    );

    // Idempotent: `serve` may reach this after a close that already
    // released, and a second holder-scoped DELETE is a wasted round-trip at
    // best and a race at worst.
    mem.release_lease_after_abandoned_close().await;
    assert_eq!(store.releases(), 1, "the release must happen at most once");
}

/// **L82-1, the wiring.** The two halves above are the pieces; this is
/// `serve`'s actual shutdown path putting them together.
///
/// It drives `close_bounded_until` — the real body of the bounded close,
/// with only the re-armed signal substituted (`shutdown_signal()` installs
/// process-wide SIGINT/SIGTERM handlers, which a test must not do to the
/// whole binary). A slow store makes the close blow its window, and the
/// lease must be gone anyway.
#[tokio::test(start_paused = true)]
async fn an_abandoned_close_releases_the_lease_through_serve() {
    let store = Arc::new(RoundTripStore::new(
        Arc::new(MemoryStore::new()),
        CostModel::PerMutation,
        Duration::from_millis(30),
    ));
    let mem = memory_on(store.clone(), "l82-1-serve-wiring").await;
    at_cap_burst(&mem).await;

    let err = crate::mcp::serve::close_bounded_until(&mem, std::future::pending())
        .await
        .expect_err("the close must be abandoned for this test to mean anything");
    assert!(
        err.to_string().contains("not durable"),
        "the error must still say the tail was lost: {err}"
    );
    assert_eq!(
        store.releases(),
        1,
        "serve must release the lease on the abandoned-close path — leaving it stale wedges \
             the session for the whole LEASE_TTL (L82-1)"
    );
}

/// The happy path must not gain a second release: `close()` already handed
/// the lease off, and a redundant holder-scoped DELETE is a wasted
/// round-trip on the way out.
#[tokio::test(start_paused = true)]
async fn a_close_that_finishes_releases_exactly_once_through_serve() {
    let store = Arc::new(RoundTripStore::new(
        Arc::new(MemoryStore::new()),
        CostModel::PerPlannedStatement,
        Duration::from_millis(30),
    ));
    let mem = memory_on(store.clone(), "l82-1-serve-happy").await;
    at_cap_burst(&mem).await;

    crate::mcp::serve::close_bounded_until(&mem, std::future::pending())
        .await
        .expect("the burst must drain inside the window now");
    assert_eq!(store.releases(), 1, "released once, by close() itself");
}

/// A fenced handle must NOT release: the lease belongs to whoever took the
/// session over, and `close()`'s own fenced branch has the same rule.
#[tokio::test(start_paused = true)]
async fn a_fenced_handle_does_not_release_on_an_abandoned_close() {
    let store = Arc::new(RoundTripStore::new(
        Arc::new(MemoryStore::new()),
        CostModel::PerPlannedStatement,
        Duration::from_millis(1),
    ));
    let mem = memory_on(store.clone(), "l82-1-fenced").await;
    mem.simulate_lease_loss();

    mem.release_lease_after_abandoned_close().await;
    assert_eq!(
        store.releases(),
        0,
        "a fenced handle must not evict the writer that took the session over"
    );
}

fn live_handles(session: &str) -> usize {
    ACTIVE_SESSIONS
        .lock()
        .get(&SessionId::new(session))
        .map(|agents| agents.len())
        .unwrap_or(0)
}

/// T81-8, retargeted for T8.6: the in-process `ACTIVE_SESSIONS` advisory
/// still reports a second same-process handle loudly, with both agent ids,
/// and releases the registration on drop — including a handle that was never
/// closed.
///
/// **Two separate stores on one logical session, on purpose.** Post-T8.6 the
/// store lease *refuses* a second writer that shares a store (see
/// `a_second_writer_sharing_a_store_is_refused_by_the_lease`). The advisory
/// log's remaining domain is the collision the per-store lease cannot see:
/// two writers that opened *different* store handles onto the same session
/// (different `MemoryStore` instances here; different processes/hosts in
/// production). Each acquires its own store's free lease, so both builds
/// succeed — and the process-global registry is the only thing that catches
/// them. That is exactly why the advisory log is kept, not replaced.
#[tokio::test]
async fn a_second_handle_on_one_session_is_reported_loudly() {
    let (logs, _guard) = capture_logs(tracing::Level::ERROR);

    // Distinct stores: the per-store lease cannot see across them, so this
    // isolates the process-global advisory (which can).
    let store_a: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let store_b: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let first = memory_on(store_a, "one-writer").await;
    assert_eq!(live_handles("one-writer"), 1);
    assert!(
        !logs.contains("SecondSessionWriter"),
        "the first handle is not a collision"
    );

    let second = Memory::builder()
        .session("one-writer")
        .agent("agent-b")
        .flush_interval(Duration::from_secs(3_600))
        .store(store_b)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .unwrap();
    assert_eq!(live_handles("one-writer"), 2, "reported, not refused");

    let logged = logs.contents();
    assert!(logged.contains("SecondSessionWriter"), "{logged}");
    assert!(logged.contains("one-writer"), "{logged}");
    assert!(
        logged.contains("agent-a") && logged.contains("agent-b"),
        "{logged}"
    );

    first.close().await.unwrap();
    assert_eq!(
        live_handles("one-writer"),
        1,
        "a successful close releases its slot without waiting for Drop (R2-4)"
    );
    drop(first);
    assert_eq!(live_handles("one-writer"), 1);
    // Dropped without close(): the registration is still released.
    drop(second);
    assert_eq!(live_handles("one-writer"), 0);
}

/// T8.6: a second writer that **shares a store** with the first is now
/// refused by the store-enforced single-writer lease — the promotion from
/// advisory to enforced. The refusal names the current holder and its age,
/// and points at the operator override.
#[tokio::test]
async fn a_second_writer_sharing_a_store_is_refused_by_the_lease() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let first = memory_on(store.clone(), "leased").await;

    // Same store, same session, different agent → distinct holder token →
    // the live lease is held by `first`, so this build fails closed.
    let err = Memory::builder()
        .session("leased")
        .agent("agent-b")
        .flush_interval(Duration::from_secs(3_600))
        .store(store.clone())
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .expect_err("a second writer on a shared store must be refused by the lease");
    // Fail closed: refused, and it stays a LamboError::Conflict.
    let LamboError::Conflict(msg) = err else {
        panic!("a shared-store second writer must fail closed as a Conflict, got: {err:?}");
    };
    assert!(msg.contains("single-writer"), "lease still enforced: {msg}");
    // Names the current holder and its age, so an operator can tell who to evict.
    assert!(msg.contains("agent-a"), "names the current holder: {msg}");
    assert!(msg.contains("s ago"), "names the holder's age: {msg}");
    // Surfaces the operator-takeover pointer — deliberately NOT the raw
    // `session_leases` SQL constant, which is no longer part of the
    // user-facing message (that string is intentionally not emitted).
    assert!(
        msg.contains("operator can force a takeover"),
        "surfaces the operator-takeover path: {msg}"
    );
    assert!(
        msg.contains("docs/reference/cli.mdx"),
        "points at the single-writer lease note: {msg}"
    );

    // After a clean close the lease is released, so a new writer attaches.
    first.close().await.unwrap();
    let second = memory_on(store, "leased").await;
    second.close().await.unwrap();
}

/// J2: `MemoryBuilder::endpoint` reaches the lease row, and its absence is
/// the default. This is the holder half of the proxy path — the loser's read
/// is only useful if the winner actually published where it listens.
#[tokio::test]
async fn the_builders_endpoint_is_published_into_the_lease_row() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let sid = crate::types::SessionId::from("published");

    // A CLI-shaped writer sets no endpoint: the row says "no hub here".
    let cli = memory_on(store.clone(), "published").await;
    assert_eq!(
        store.read_lease(&sid).await.unwrap().unwrap().endpoint,
        None,
        "a writer that is not a serve process must publish no endpoint"
    );
    cli.close().await.unwrap();

    // A serve-shaped writer does, and the row carries it verbatim.
    let hub = Memory::builder()
        .session("published")
        .agent("agent-hub")
        .flush_interval(Duration::from_secs(3_600))
        .store(store.clone())
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .endpoint("/run/lambo/published.sock")
        .build()
        .await
        .expect("the hub must attach");
    assert_eq!(
        store
            .read_lease(&sid)
            .await
            .unwrap()
            .unwrap()
            .endpoint
            .as_deref(),
        Some("/run/lambo/published.sock")
    );
    hub.close().await.unwrap();
}

/// T86-2: a holder that LOSES its lease is FENCED. After the store starves
/// its heartbeat past the TTL and another writer takes over, this handle must
/// refuse every further write and must NOT flush or release — otherwise the
/// two writers flush divergent graphs into one session (the exact split-brain
/// the lease prevents). Simulates the outage-plus-takeover the heartbeat sees.
#[tokio::test]
async fn a_lost_lease_fences_the_writer_and_stops_the_flush() {
    let store = Arc::new(MemoryStore::new());
    let session = SessionId::new("fenced");
    let first = Memory::builder()
        .session("fenced")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(store.clone() as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .expect("build");

    // A tail exists but nothing is durable yet (hour-long flush interval).
    first
        .derive(&[("before loss", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    assert!(
        store.load_session(&session).await.is_err(),
        "nothing flushed yet"
    );

    // Store outage past the TTL, then a DIFFERENT holder takes the session
    // over — exactly what the heartbeat's next refresh observes as `Held`.
    store.force_expire_lease(&session);
    let taker = LeaseHolder::for_this_process(&AgentId::new("agent-b"));
    let outcome = store
        .acquire_lease(&session, &taker, LEASE_TTL)
        .await
        .unwrap();
    assert!(
        outcome.is_acquired(),
        "agent-b takes over the expired lease"
    );

    // The heartbeat would latch the fence on its next tick; drive it directly.
    first.simulate_lease_loss();

    // 1. Every further write is refused with the honest lease-lost message.
    let derive_err = first
        .derive(&[("after loss", ConceptType::Entity)], &ParentOf::none())
        .await
        .expect_err("a fenced handle must refuse derive");
    let msg = derive_err.to_string();
    assert!(msg.contains("lost its single-writer lease"), "{msg}");
    assert!(msg.contains("no longer the writer"), "{msg}");

    let action_err = first
        .record_action(&Action {
            event_time: None,
            action: "write after loss",
            produces: &["x"],
            modifies: &[],
            depends_on: &[],
        })
        .expect_err("a fenced handle must refuse record_action");
    assert!(action_err
        .to_string()
        .contains("lost its single-writer lease"));

    let reserve_err = first
        .reserve(NodeId::new(), Duration::from_secs(60))
        .expect_err("a fenced handle must refuse reserve");
    assert!(reserve_err
        .to_string()
        .contains("lost its single-writer lease"));

    // 2. close() refuses to flush (no overwrite) and does not release.
    let close_err = first
        .close()
        .await
        .expect_err("a fenced close must not flush or release");
    assert!(close_err
        .to_string()
        .contains("lost its single-writer lease"));

    // No flush ever landed: the store has no concepts from `first`.
    assert!(
        store.load_session(&session).await.is_err(),
        "a fenced handle must never persist its tail — the new holder owns the session"
    );

    // The takeover holder still owns the lease: the fenced close did NOT
    // release it (a stale release would evict the new writer). A third,
    // distinct holder is therefore refused.
    let third = store
        .acquire_lease(
            &session,
            &LeaseHolder::for_this_process(&AgentId::new("agent-c")),
            LEASE_TTL,
        )
        .await
        .unwrap();
    assert!(
        !third.is_acquired(),
        "agent-b's lease is intact — the fenced handle must not release it"
    );

    drop(first);
}

/// T8.6: exactly one holder and one honest refusal, in-process — the memory
/// backend's cross-"process" analogue done as two `build`s on one store.
#[tokio::test]
async fn one_store_two_builds_yield_one_holder_and_one_refusal() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());

    let a = Memory::builder()
        .session("dup")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(store.clone())
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await;
    let b = Memory::builder()
        .session("dup")
        .agent("agent-b")
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await;

    // Exactly one wins.
    assert!(
        a.is_ok() ^ b.is_ok(),
        "exactly one of the two builds must acquire the lease"
    );
    if let Ok(m) = a {
        m.close().await.unwrap();
    }
    if let Ok(m) = b {
        m.close().await.unwrap();
    }
}

/// R2-4: close-then-reattach in one process — the MCP server's ordinary
/// shape, and this crate's own reload test — must be silent.
///
/// `close()` used to leave the registration in place until `Drop`, so
/// rebuilding the session while the closed handle was still in scope fired
/// the ops-level `SecondSessionWriter` ERROR against a handle that had
/// already flushed its tail and stopped every task. A detector that cries
/// wolf on the one sequence that is certainly safe is a detector that gets
/// filtered out.
#[tokio::test]
async fn a_closed_handle_does_not_collide_with_a_reattach() {
    let (logs, _guard) = capture_logs(tracing::Level::ERROR);

    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store.clone(), "reattach").await;
    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    mem.close().await.unwrap();
    assert_eq!(live_handles("reattach"), 0);

    // The closed handle is deliberately still in scope: the owner holds it
    // (for `stats`, for a retry) while re-attaching.
    let reattached = memory_on(store, "reattach").await;
    let logged = logs.contents();
    assert!(
        !logged.contains("SecondSessionWriter"),
        "a re-attach after a successful close is not a second writer: {logged}"
    );
    assert_eq!(live_handles("reattach"), 1);

    // ...and the closed handle's `Drop` must not release the *new* one's
    // slot: the registry keys on session + agent id, which the re-attach
    // reuses.
    drop(mem);
    assert_eq!(
        live_handles("reattach"),
        1,
        "Drop after an already-released close must not evict the live handle"
    );

    reattached.close().await.unwrap();
    assert_eq!(live_handles("reattach"), 0);
}

/// R2-2: dropping a handle whose `close()` **failed** must not be silent.
///
/// The leak guard keyed on task handles still being `Some`; a failed close
/// has already taken all three, so `leaked` was false and `Drop` said
/// nothing — while the tail that same close deliberately kept in the log
/// (T81-5, for the retry it documents) went out with the handle. The exact
/// case an owner most needs told, lost most quietly.
///
/// The failed close also keeps its registration, which is the other half of
/// the R2-4 policy: this handle still holds an undurable tail.
#[tokio::test]
async fn dropping_a_handle_whose_close_failed_warns_about_the_kept_tail() {
    let (logs, _guard) = capture_logs(tracing::Level::WARN);

    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let store: Arc<dyn GraphStore> = Arc::new(FlakyStore::new(inner, usize::MAX));
    let mem = memory_on(store, "drop-after-failed-close").await;
    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();

    let err = mem.close().await.unwrap_err();
    assert!(err.to_string().contains("simulated outage"), "{err}");
    let kept = mem.graph().read().log_len();
    assert!(kept > 0, "the failed close kept the tail for a retry");
    assert_eq!(
        live_handles("drop-after-failed-close"),
        1,
        "a failed close keeps its registration — the tail is still undurable"
    );
    assert!(!logs.contains("dropped"), "nothing dropped yet");

    drop(mem);
    assert_eq!(live_handles("drop-after-failed-close"), 0);

    let logged = logs.contents();
    assert!(
        logged.contains("un-flushed"),
        "dropping a handle that still holds an un-flushed tail must warn: {logged}"
    );
    assert!(
        logged.contains("close() that did not finish"),
        "the warning must name the cause — a failed or cancelled close, not a \
             forgotten one: {logged}"
    );
    assert!(logged.contains(&kept.to_string()), "{logged}");
}

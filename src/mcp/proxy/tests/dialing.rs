//! Dialing the holder: dial budgets and shutdown during the dial.

use super::*;

/// `GraphStore` whose lease-row read **never returns**, and whose every other
/// surface is `unreachable!` — the injection seam for J2-R2-1.
///
/// A hung `read_lease` is precisely the shape the finding is about: nothing
/// in this module bounded it, and what *did* bound it was the store adapter's
/// own tuning — sqlite's `busy_timeout` behind a `max_connections(1)` pool,
/// cockroach's `statement_timeout` behind the same 30s sqlx default acquire
/// (the arithmetic is at [`DIAL_BUDGET`]). Injecting the pathology at the
/// trait the production type already takes, rather than provoking a real
/// store into it, is the [`crate::ledger`] `BatchSink` precedent: neither the
/// timing nor the store's configuration has to be reproduced for the bound to
/// be falsifiable.
struct HungLeaseStore;

#[async_trait::async_trait]
impl crate::store::GraphStore for HungLeaseStore {
    async fn read_lease(
        &self,
        _session: &crate::types::SessionId,
    ) -> Result<Option<LeaseInfo>, crate::types::StoreError> {
        // Never resolves. The dial's own bound must be what ends this.
        std::future::pending().await
    }
    async fn init_schema(&self) -> Result<(), crate::types::StoreError> {
        unreachable!("a proxy never initializes schema")
    }
    fn capabilities(&self) -> crate::store::Capabilities {
        unreachable!("a proxy never asks a store what it can do")
    }
    async fn flush(
        &self,
        _batch: &crate::types::MutationBatch,
        _token: Option<u64>,
    ) -> Result<(), crate::types::StoreError> {
        unreachable!("a proxy holds no tail and never flushes")
    }
    async fn load_session(
        &self,
        _session: &crate::types::SessionId,
    ) -> Result<crate::types::GraphSnapshot, crate::types::StoreError> {
        unreachable!("a proxy holds no graph and never loads")
    }
    async fn keyword_candidates(
        &self,
        _session: &crate::types::SessionId,
        _tokens: &[String],
        _limit: usize,
    ) -> Result<Vec<crate::types::Scored<crate::types::NodeId>>, crate::types::StoreError> {
        unreachable!("a proxy never queries")
    }
    async fn vector_candidates(
        &self,
        _session: &crate::types::SessionId,
        _embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<crate::types::Scored<crate::types::NodeId>>, crate::types::StoreError> {
        unreachable!("a proxy never queries")
    }
    async fn blast_radius(
        &self,
        _session: &crate::types::SessionId,
        _node: crate::types::NodeId,
        _min_edge_age: std::time::Duration,
        _now: chrono::DateTime<Utc>,
    ) -> Result<u64, crate::types::StoreError> {
        unreachable!("a proxy never queries")
    }
    async fn interaction_span(
        &self,
        _session: &crate::types::SessionId,
        _node: crate::types::NodeId,
        _min_age: std::time::Duration,
        _now: chrono::DateTime<Utc>,
    ) -> Result<crate::types::InteractionSpan, crate::types::StoreError> {
        unreachable!("a proxy never queries")
    }
    async fn record_canonization(
        &self,
        _event: &crate::types::CanonizationEvent,
        _token: Option<u64>,
    ) -> Result<(), crate::types::StoreError> {
        unreachable!("a proxy never canonizes")
    }
}

/// A proxy whose every dial parks forever in the lease-row read.
fn proxy_onto_a_hung_store() -> HubProxy {
    HubProxy::new(
        crate::types::SessionId::new("j2-r2-1"),
        ours(),
        Arc::new(HungLeaseStore),
        "this-host".to_string(),
        "this-host".to_string(),
        None,
    )
}

/// J2-R2-1: a SIGTERM arriving during a dial is honoured at the next poll,
/// whatever the store is doing.
///
/// This is the property the old `2 × CONNECT_BUDGET` sentence claimed and the
/// code did not have. Under a hung `read_lease` the pre-fix arm body waited
/// for the store — 38s on sqlite, 50s on cockroach, computed at
/// [`DIAL_BUDGET`] — and this test would have measured `DIAL_BUDGET` at best
/// and never returned at worst.
///
/// **Its own negative control.** Remove the `biased` shutdown arm from
/// `dial_bounded` and the call still returns, at `DIAL_BUDGET`, with
/// `Dialled::Failed` — so the mutation lands as a failed assertion on the
/// variant *and* on the elapsed time, not as a hung suite. Remove the
/// `tokio::time::timeout` as well and it hangs, which is the honest signal for
/// that mutation.
///
/// Time is paused, so the test costs no wall clock: the runtime auto-advances
/// to the shutdown timer the instant the hung read is the only pending work —
/// exactly the state a store wedged at its connection pool produces.
#[tokio::test(start_paused = true)]
async fn a_shutdown_during_the_dial_is_honoured_and_not_left_to_the_store() {
    let proxy = proxy_onto_a_hung_store();
    let handshake = Handshake::default();
    let shutdown = tokio::time::sleep(std::time::Duration::from_millis(50));
    tokio::pin!(shutdown);
    let start = tokio::time::Instant::now();
    let outcome = proxy.dial_bounded(&handshake, shutdown.as_mut()).await;
    let waited = start.elapsed();
    assert!(
        matches!(outcome, Dialled::ShutdownRequested),
        "a shutdown against a hung lease-row read must end the dial at once; \
             `Dialled::Failed` here means the race is gone and the budget cap answered instead"
    );
    assert!(
        waited < DIAL_BUDGET,
        "the signal, not the budget, must decide this one: waited {waited:?}"
    );
}

/// J2-R2-1: with no shutdown in sight, a hung lease-row read is cut off at
/// the **chosen** bound rather than at the store's.
///
/// The client on the other side of this dial is blocked on the call that
/// triggered it, so the second half of the fix is that `DIAL_BUDGET` — a
/// number chosen here — decides, and not `busy_timeout` / `statement_timeout`
/// / sqlx's default pool acquire, which are numbers chosen for a flush.
#[tokio::test(start_paused = true)]
async fn a_hung_lease_read_is_cut_off_at_the_chosen_dial_budget() {
    // The half of the argument that is arithmetic, asserted rather than
    // asserted-in-prose: the chosen bound has to sit under the smallest
    // store-emergent one, or the store is still what governs. 8s is sqlite's
    // `busy_timeout`, set in `SqliteStore::connect` and pinned there by
    // `file_backed_wal_and_busy_timeout_applied`.
    assert!(
        DIAL_BUDGET < std::time::Duration::from_secs(8),
        "DIAL_BUDGET must stay below sqlite's 8s busy_timeout, or the number an operator \
             reads at the constant is not the number that decides"
    );
    let proxy = proxy_onto_a_hung_store();
    let handshake = Handshake::default();
    let shutdown = std::future::pending::<()>();
    tokio::pin!(shutdown);
    let start = tokio::time::Instant::now();
    let outcome = proxy.dial_bounded(&handshake, shutdown.as_mut()).await;
    let waited = start.elapsed();
    assert!(
        waited >= DIAL_BUDGET && waited < DIAL_BUDGET + std::time::Duration::from_secs(1),
        "the dial must end at the budget, not at the store: waited {waited:?}"
    );
    let Dialled::Failed(e) = outcome else {
        panic!("a hung store read must fail the dial, not produce a connection")
    };
    let text = e.to_string();
    assert!(
        text.contains(&format!("within {}s", DIAL_BUDGET.as_secs())),
        "the operator needs the bound that fired named in the line: {text}"
    );
    assert!(
        text.contains("connection pool"),
        "and the likeliest cause, since 'the holder is not answering' would send them to \
             the wrong process: {text}"
    );
}

/// J2-R2-1 at the entry point: the **first** dial is inside the raced region
/// too, which it was not before — `run` used to await it above the
/// `tokio::pin!`, so a proxy was deaf for a whole store timeout before its
/// loop ever started.
///
/// `run` returns `Ok(())`, the clean exit `serve`'s proxy branch expects, and
/// it does so before the stdin task is spawned — which is why this test can
/// drive `run` at all without touching the suite's real stdio.
#[tokio::test(start_paused = true)]
async fn a_shutdown_during_the_proxys_first_dial_exits_cleanly() {
    let proxy = proxy_onto_a_hung_store();
    let start = tokio::time::Instant::now();
    proxy
        .run(tokio::time::sleep(std::time::Duration::from_millis(50)))
        .await
        .expect("a shutdown before the first connection is a clean exit, not a failure");
    let waited = start.elapsed();
    assert!(
        waited < DIAL_BUDGET,
        "the first dial must be raced against the signal, not merely capped: {waited:?}"
    );
}

use super::*;
use crate::embed::{Embedder, FixtureEmbedder};
use crate::graph::action::Action;
use crate::memory::Memory;
use crate::store::{GraphStore, MemoryStore};
use crate::types::EmbeddingContract;

async fn mem(session: &str) -> Arc<Memory> {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let m = Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        })
        .build()
        .await
        .expect("build");
    Arc::new(m)
}

fn assert_closed(m: &Memory, ctx: &str) {
    let action = Action {
        event_time: None,
        action: "post-close write",
        produces: &[],
        modifies: &[],
        depends_on: &[],
    };
    assert!(
        m.record_action(&action).is_err(),
        "{ctx}: the session must be closed to writers after run_and_close"
    );
}

/// Issue #13 review: the keep-warm stops when the transport returns,
/// not after the close. `run_and_close` owns that now (serve passes the
/// keep-warm's abort handle), so a task handed to it must come back
/// cancelled even though nobody else aborts it. The ordering against
/// the close needs a close that takes time and is pinned in
/// `memory::tests::the_keep_warm_is_stopped_before_the_close_starts`;
/// this one pins the abort itself.
#[tokio::test]
async fn run_and_close_stops_the_tasks_it_is_handed() {
    let m = mem("serve-close-stops-keep-warm").await;
    let pump = tokio::spawn(async {});
    let keep_warm = tokio::spawn(std::future::pending::<()>());
    let out = run_and_close(
        m.clone(),
        async { Ok(()) },
        pump,
        &[keep_warm.abort_handle()],
        &EarlyShutdown::unarmed(),
    )
    .await;
    assert!(out.is_ok(), "{out:?}");
    let joined = tokio::time::timeout(Duration::from_secs(5), keep_warm)
        .await
        .expect("the handed task must end once run_and_close returns");
    assert!(
        joined
            .expect_err("it never completes on its own")
            .is_cancelled(),
        "run_and_close must abort what it is handed before the close"
    );
    assert_closed(&m, "stop-before-close path");
}

/// **The first signal must never abandon a close; the second must.**
///
/// `close_bounded`'s escape hatch is the operator who watches a close
/// stall and presses Ctrl-C again. It used to detect that by arming a
/// *fresh* `signal()` registration at the top of the close and treating
/// whatever that caught as the second signal, on the reasoning that a
/// registration created after a signal was delivered will not replay
/// it.
///
/// Delivery is not when the record is written. Tokio's unix handler
/// sets a flag and writes one byte to a self-pipe; the watch that wakes
/// registrations is not sent until the signal driver task is scheduled
/// to drain it. On a loaded machine those are milliseconds apart, so a
/// registration created in the gap catches the FIRST signal and reads
/// it as a second — abandoning a close nobody asked to abandon and
/// losing the tail with it.
///
/// The pre-handshake durability test hit exactly this under CPU
/// contention, and it is not a test-only shape: closing stdin and then
/// sending `SIGTERM` is the shutdown sequence the MCP spec prescribes
/// for clients, so a real client could lose a holder's tail the same
/// way. Counting arrivals instead of dating them is what fixes it — one
/// `kill` is one increment no matter when the driver records it.
///
/// `simulate_signal` bumps the record directly, so this drives the
/// observer side without installing process-wide handlers on the whole
/// test binary or sending a real signal to it.
#[tokio::test]
async fn one_signal_does_not_abandon_the_close_but_two_do() {
    // One signal: the close must complete and the tail must be durable.
    let one = EarlyShutdown::unarmed();
    one.simulate_signal();
    let m = mem("serve-close-one-signal").await;
    let out = close_bounded_until(&m, one.second_signal()).await;
    assert!(
        out.is_ok(),
        "a single shutdown signal is the one that STARTED the shutdown — it must never \
                 be mistaken for the operator's give-up second press, whenever it happens to be \
                 recorded: {out:?}"
    );
    assert_closed(&m, "one-signal path");

    // Two signals: the escape hatch is still there, and still says so.
    let two = EarlyShutdown::unarmed();
    two.simulate_signal();
    two.simulate_signal();
    let m2 = mem("serve-close-two-signals").await;
    let out2 = close_bounded_until(&m2, two.second_signal()).await;
    let err = out2.expect_err(
        "a genuine second signal must still abandon the close — removing the escape \
                 hatch would turn a stalled flush into an unkillable process",
    );
    assert!(
        err.to_string().contains("second shutdown signal"),
        "the abandon must be reported as what it is: {err}"
    );
}

/// The record `close_bounded` reads is a **count**, and an unarmed one
/// never claims anything.
///
/// Kept separate from the close above so a regression in the counting
/// itself is named as such rather than surfacing as a mysterious close
/// result. `now_or_never`-style polling is spelled with a zero timeout
/// so a future that is genuinely pending is not silently treated as
/// ready.
#[tokio::test]
async fn the_signal_record_counts_arrivals_rather_than_latching() {
    let e = EarlyShutdown::unarmed();
    async fn ready(f: impl Future<Output = ()>) -> bool {
        tokio::time::timeout(Duration::from_millis(50), f)
            .await
            .is_ok()
    }

    assert!(
        !ready(e.fired()).await,
        "an unarmed record has seen nothing and must park"
    );
    assert!(
        !ready(e.second_signal()).await,
        "an unarmed record certainly has not seen two"
    );

    e.simulate_signal();
    assert!(
        ready(e.fired()).await,
        "one arrival is a shutdown — this is J6's pre-arm and must still fire"
    );
    assert!(
        !ready(e.second_signal()).await,
        "one arrival is NOT two: this is the assertion the old boolean record could \
                 not make, and the whole of the fix"
    );

    e.simulate_signal();
    assert!(ready(e.second_signal()).await, "two arrivals are two");
}

#[tokio::test]
async fn close_runs_when_the_transport_returns_ok() {
    let m = mem("serve-close-ok").await;
    let pump = tokio::spawn(std::future::pending::<()>());
    let out = run_and_close(
        m.clone(),
        async { Ok(()) },
        pump,
        &[],
        &EarlyShutdown::unarmed(),
    )
    .await;
    assert!(out.is_ok(), "clean transport exit closes cleanly: {out:?}");
    assert_closed(&m, "ok path");
}

#[tokio::test]
async fn close_runs_even_when_the_transport_errors() {
    let m = mem("serve-close-err").await;
    let pump = tokio::spawn(std::future::pending::<()>());
    let out = run_and_close(
        m.clone(),
        async { Err(LamboError::Config("transport blew up".into())) },
        pump,
        &[],
        &EarlyShutdown::unarmed(),
    )
    .await;
    assert!(out.is_err(), "the transport error is surfaced: {out:?}");
    // The whole point: the error path still closed the session.
    assert_closed(&m, "err path");
}

/// **T8.6 release-on-close, tied into the lifecycle seam.** A serve
/// process acquires the single-writer lease on start; a clean exit
/// through `run_and_close` must **release** it (hand off), not leave it
/// to expire at the TTL. Proven by a fresh writer — a *different* holder
/// on the same store — attaching immediately after the close.
#[tokio::test]
async fn a_clean_close_releases_the_lease() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let contract = EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: 1024,
    };
    let first = Arc::new(
        Memory::builder()
            .session("serve-lease-release")
            .agent("agent-a")
            .flush_interval(Duration::from_secs(3_600))
            .store(store.clone())
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract.clone())
            .build()
            .await
            .expect("build A"),
    );

    let pump = tokio::spawn(std::future::pending::<()>());
    let out = run_and_close(
        first.clone(),
        async { Ok(()) },
        pump,
        &[],
        &EarlyShutdown::unarmed(),
    )
    .await;
    assert!(out.is_ok(), "clean close: {out:?}");
    assert_closed(&first, "release path");

    // The lease was released, so a *different* writer attaches at once
    // (a still-held lease would refuse this with a Conflict).
    let second = Memory::builder()
        .session("serve-lease-release")
        .agent("agent-b")
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract)
        .build()
        .await
        .expect("a clean close must release the lease so a new writer can attach");
    second.close().await.expect("close B");
}

/// **J6** — the observer half of the pre-arm, pinned without a signal.
///
/// The window: `holder_shutdown` arms at the first statement after
/// `resolve_role` returns, but the lease is taken *inside* it, so a
/// SIGTERM between the two hit the default disposition and killed the
/// process with `close()` un-run (CI run 32710994512,
/// `unix_wait_status(15)`). [`EarlyShutdown`] records such a signal at
/// the acquire; this pins that [`wind_down`] then completes on its
/// **first poll** rather than waiting for a second signal that will
/// never come.
///
/// The mutation it catches is the one measured on the branch: with the
/// `early.fired()` arm deleted, the pre-handshake test does not go back
/// to `unix_wait_status(15)` — it fails with *"did not exit within
/// 15s"*, because the registration now catches the signal and nothing
/// acts on it. That is J2-R1-7's SIGTERM immunity, arrived at from the
/// other side, and it is why arming and observing have to land
/// together.
///
/// `std::future::pending` stands in for the fresh `shutdown_signal()`,
/// so the pre-arm is the only thing that can complete this — and
/// `simulate_signal` sets the record directly, so the test binary's own
/// signal disposition is untouched.
#[tokio::test]
async fn a_signal_recorded_before_the_arming_winds_the_serve_down_at_once() {
    let m = mem("serve-j5-prearm-observed").await;
    let early = EarlyShutdown::unarmed();

    // Nothing recorded yet: a holder that owns its lease and has seen
    // no signal must not wind down. Without this half, an
    // always-ready arm would pass the assertion below while exiting
    // every healthy serve the instant it started.
    let healthy = tokio::time::timeout(
        Duration::from_millis(50),
        wind_down(std::future::pending::<()>(), early.clone(), m.clone(), None),
    )
    .await;
    assert!(
        healthy.is_err(),
        "a holder with no recorded signal must not wind down"
    );

    // A signal that landed in the window, recorded at the acquire and
    // asked about for the first time here.
    early.simulate_signal();
    tokio::time::timeout(
        Duration::from_secs(5),
        wind_down(std::future::pending::<()>(), early, m, None),
    )
    .await
    .expect(
        "a signal recorded before the arming must complete the wind-down immediately — \
                 otherwise the transport serves on and the pre-handshake SIGTERM is swallowed",
    );
}

/// **J6** — the arming half: `build_attach` arms on the winning branch,
/// and **only** there.
///
/// Two builds against one store. The first takes the single-writer
/// lease and must arm; the second is refused and must not, because a
/// serve that loses becomes a proxy and a proxy holds no lease, no tail
/// and no graph — there is nothing a handler could save, and arming
/// over an election that may legitimately run for `ELECTION_BUDGET`
/// (20s) is the immunity J2-R1-7 rejected.
///
/// So this is the wedge invariant read through the signal disposition:
/// the arm sits behind the acquire, which makes "a proxy never arms"
/// the same statement as "a proxy never takes the lease" rather than a
/// second thing to keep true.
#[tokio::test]
async fn only_the_attach_that_takes_the_lease_arms_the_pre_arm() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let contract = EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: 1024,
    };
    // Distinct agents: the lease token is `agent@host#pid`, so a second
    // attach under the SAME agent in this one process is a refresh of
    // its own lease, not the refusal this test is about.
    let builder = |agent: &str, early: EarlyShutdown| {
        Memory::builder()
            .session("serve-j5-arm-on-the-winner")
            .agent(agent)
            .flush_interval(Duration::from_secs(3_600))
            .store(Arc::clone(&store))
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract.clone())
            .early_shutdown(early)
    };

    let winner_arm = EarlyShutdown::unarmed();
    assert!(
        !winner_arm.is_armed(),
        "constructing the handle must install nothing — `serve` hands it to the builder \
                 BEFORE the election, and an election that armed on construction would be deaf \
                 for the whole of ELECTION_BUDGET"
    );
    let held = builder("agent-a", winner_arm.clone())
        .build_attach()
        .await
        .expect("the first attach takes the lease");
    assert!(
        matches!(held, crate::memory::Attach::Attached(_)),
        "the first attach must win the lease"
    );
    assert!(
        winner_arm.is_armed(),
        "the attach that TOOK the lease must arm at the acquire — this is the window CI \
                 run 32710994512 died in"
    );

    let loser_arm = EarlyShutdown::unarmed();
    let refused = builder("agent-b", loser_arm.clone())
        .build_attach()
        .await
        .expect("a refusal is reported as data, not as an error (J2)");
    assert!(
        matches!(refused, crate::memory::Attach::Held(_)),
        "the second attach must be refused the lease"
    );
    assert!(
        !loser_arm.is_armed(),
        "an attach that did NOT take the lease must leave the process's signal \
                 disposition alone: it becomes a proxy, holds nothing a handler could save, and \
                 arming over the election is what J2-R1-7 rejected"
    );

    let crate::memory::Attach::Attached(mem) = held else {
        unreachable!("asserted above");
    };
    mem.close().await.expect("close the holder");
}

/// **JE2E-4** (operator ruling, 2026-08-22). Losing the lease must end
/// the transport by the same route SIGTERM does, so the client respawns
/// the serve and it comes back as a proxy to the real holder.
///
/// Before this, the fence gated writes and nothing else: an ex-holder
/// kept its listener, its attached proxies and its own client, serving
/// **silently stale reads** for as long as the process lived.
///
/// Two halves, and both matter:
///
/// 1. a healthy holder's wind-down does **not** fire (or every serve
///    would exit at once, which is the failure a naive `select!` on a
///    ready future produces);
/// 2. the fence latching resolves it, and names the winner so the exit
///    line can say who took the session.
#[tokio::test]
async fn losing_the_lease_winds_the_serve_down_like_a_sigterm() {
    let m = mem("serve-fence-winddown").await;

    // A healthy holder waits. `std::future::pending` stands in for the
    // signal, so the ONLY thing that can complete this is the fence.
    let healthy = tokio::time::timeout(
        Duration::from_millis(50),
        wind_down(
            std::future::pending::<()>(),
            EarlyShutdown::unarmed(),
            m.clone(),
            None,
        ),
    )
    .await;
    assert!(
        healthy.is_err(),
        "a holder that still owns its lease must not wind down"
    );

    // Now the heartbeat's transition, driven directly (the real one
    // fires on its 15s interval).
    let waiting = tokio::spawn({
        let m = m.clone();
        async move {
            wind_down(
                std::future::pending::<()>(),
                EarlyShutdown::unarmed(),
                m,
                None,
            )
            .await
        }
    });
    tokio::task::yield_now().await;
    m.simulate_lease_loss_to("agent-b@host#7");
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the fence must wake the wind-down, not leave it parked")
        .expect("the wind-down task must not panic");

    // And the identity the exit line names is the writer that won.
    assert_eq!(
        m.lease_lost_latched().await,
        "agent-b@host#7",
        "the exit line must name who took the session, not 'someone'"
    );

    // The close that follows is the fenced one: it refuses rather than
    // flushing a tail another writer now owns, which is why the exit
    // code is non-zero. `serve`'s tail — the ledger drain and the
    // identity-licensed unlink — runs on this path exactly as it does
    // on the clean one.
    let pump = tokio::spawn(std::future::pending::<()>());
    let out = run_and_close(
        m.clone(),
        async { Ok(()) },
        pump,
        &[],
        &EarlyShutdown::unarmed(),
    )
    .await;
    assert!(
        out.is_err(),
        "a fenced close must not claim success: {out:?}"
    );
}

/// **JE2E-R2-2 — the composition, which the two tests above leave
/// untested.** They pin `wind_down` and the fenced `run_and_close` in
/// isolation; round 2 showed that severing the one expression which
/// *joins* them — `serve`'s `holder_shutdown(...)` handed to the
/// transport — left the whole 1016-test suite green while every fenced
/// holder went back to living forever.
///
/// Two devices close that, and this test is the second of them:
///
/// 1. **The type.** [`HolderShutdown`] has one constructor and the
///    transports take `Pin<&mut HolderShutdown>`, so passing the bare
///    signal no longer compiles. That is what makes the *wiring*
///    unseverable; a test cannot, because a test pins one spelling of a
///    line a refactor may re-spell.
/// 2. **This test**, which pins the other half: that the future the
///    constructor builds actually *cancels a running transport* when the
///    fence latches. The type guarantees `serve` hands over a
///    `HolderShutdown`; this guarantees a `HolderShutdown` is worth
///    handing over.
///
/// The transport is real — `serve_http_bounded` on an ephemeral port,
/// the same function `serve_http` ends in — with a handler that never
/// returns, so nothing but the shutdown future can end it. It also
/// asserts JE2E-R2-4's artifact, because this is the one test that has a
/// ledger, a fence and the exit path in the same place.
///
/// **Declined, with the gap stated:** a binary-level self-heal test —
/// two real serves, the holder wedged until its lease lapses, the
/// successor winning it, the ex-holder exiting and its client's respawn
/// arriving as a proxy. It needs a real `LEASE_TTL` expiry (45 s) plus an
/// election, cannot use a paused clock (the processes have their own),
/// and would be the slowest test in the tree by an order of magnitude.
/// What stays unproven without it: that a *real client* respawns a
/// serve that exits non-zero. That is client behaviour, not lambo's, and
/// it is the same assumption the J2 outage story already rests on.
#[tokio::test]
async fn a_fenced_holder_shutdown_cancels_a_running_transport_and_books_the_loss() {
    let dir = crate::test_util::ScratchDir::new("lambo-r2-compose");
    let path = dir.join("calls.jsonl");
    let ledger = Ledger::open(&path);

    let m = mem("serve-fence-composition").await;

    // A transport that never finishes on its own: if it returns, the
    // shutdown future is the only thing that can have ended it.
    let app = axum::Router::new().route(
        "/never",
        axum::routing::get(|| async {
            tokio::time::sleep(Duration::from_secs(3_600)).await;
            "unreachable"
        }),
    );
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind ephemeral port");

    // Built exactly as `serve` builds it, through the one constructor.
    let shutdown = holder_shutdown(
        m.clone(),
        Some(Arc::clone(&ledger)),
        EarlyShutdown::unarmed(),
    );
    let server = tokio::spawn(serve_http_bounded(
        listener,
        app,
        shutdown,
        Duration::from_millis(200),
    ));

    // It must NOT end on its own — otherwise the assertion below would
    // pass against a transport that was never cancelled at all.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !server.is_finished(),
        "the transport must still be running while this holder owns its lease"
    );

    m.simulate_lease_loss_to("agent-b@host#9");

    let out = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect(
            "losing the lease must cancel the transport — this is the composition \
                     JE2E-R2-2 found severable with every gate green",
        )
        .expect("server task");
    assert!(
        out.is_ok(),
        "a wound-down transport is not an error: {out:?}"
    );

    // **JE2E-R2-4.** The largest lease event a holder can suffer must
    // leave an artifact, not just a stderr line. Written by the fence
    // arm before it cancelled the transport, so it is on the ledger by
    // the time the drain runs.
    ledger.shutdown();
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .expect("ledger file")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("one JSON object per line"))
        .collect();
    let lost = lines
        .iter()
        .find(|l| l["kind"] == "lease" && l["event"] == "lost")
        .unwrap_or_else(|| {
            panic!("a lease loss must reach the ledger, not only stderr: {lines:?}")
        });
    assert_eq!(
        lost["side"], "holder",
        "the loser here is the holder itself"
    );
    assert_eq!(
        lost["counterparty"], "agent-b@host#9",
        "and it names who took the session: {lost}"
    );
    assert!(
        lost.get("dialled").is_none(),
        "the winner is a lease token, not a socket path (JE2E-11): {lost}"
    );
}

//! Shutdown coordination for a holder: the grace budgets and the build-time
//! relations between them, the one shutdown future the transports accept
//! ([`HolderShutdown`], always [`wind_down`]), and the close that runs on
//! every exit path ([`run_and_close_sessions`], [`close_bounded`]).
//!
//! # The holder's shutdown, stage by stage
//!
//! [`serve`](super::serve) runs these in this order on every exit path of
//! the holder branch (signal, lease loss, client disconnect, transport
//! error). Each stage is one named step below, so stage logging (#40)
//! attaches at one call each.
//!
//! | # | stage | step | bound |
//! |---|---|---|---|
//! | 1 | transport drain (HTTP graceful drain, stdio cancel) | the transport future inside [`run_and_close_sessions`], ended by [`wind_down`] | [`SHUTDOWN_GRACE`] |
//! | 2 | keep-warm abort | `stop_before_close` in [`run_and_close_sessions`], from [`ProcessTasks::stop_before_close`](super::process::ProcessTasks::stop_before_close): the keep-warm and the process's write-queue calibration probe ([`EmbedderCalibration`](crate::writeq::EmbedderCalibration), #32 PR 3) | instant |
//! | 3 | session close | [`close_sessions`], each through [`close_bounded`]: [`Memory::close`] and its own ten logged steps (`serialize`, `replay_stop`, `queue_quiesce`, `writers_gate`, `heartbeat_abort`, `producer_joins`, `flush_join`, `final_drain`, `final_flush`, `lease_release`; `src/memory/shutdown.rs`), or on abandonment the bounded lease release | [`CLOSE_GRACE`] |
//! | 4 | event pump abort | after the close, in [`close_sessions`], so final-drain events still reach the log | instant |
//! | 5 | background tasks | [`ProcessTasks::stop`](super::process::ProcessTasks::stop): ledger heartbeat, keep-warm (again), refusal poller; the calibration probe (again) | instant |
//! | 6 | endpoint release | `hub::Hub::release` per session (`session::AttachedSession::release_endpoint`): stop accepting, end every endpoint session (each cancels its rmcp service and waits for it), then the socket file if still ours | `hub::ENDPOINT_RELEASE_GRACE`, then the stragglers are aborted and joined (unbounded, but milliseconds in practice; the watchdog's 1 s overrun allowance covers it) |
//! | 7 | ledger close | [`close_ledger`] | the ledger's own shutdown bound |
//!
//! Stages 1 to 4 are [`run_and_close_sessions`] (its one-session form,
//! `run_and_close`, is the seam the "close always runs" tests drive), whose
//! per-session half, stages 3 and 4, is [`close_sessions`]. Stages 3, 4 and
//! 6 run for every attached session, concurrently for 3 and 6, so one bound
//! covers the set (#32 design §3.5); a single-session serve's set has one
//! member and logs exactly the lines it always has. Every stage logs a
//! `started` and a `finished in N ms` line through [`ShutdownProgress`]
//! (#40; the line format is in [`super::stages`]), so a shutdown that stalls
//! names its stage. The order is load-bearing:
//!
//! * the tail is durable (or honestly lost) before any proxy connection is
//!   cut, in stage 6. Until then an endpoint session stays connected; a call
//!   it makes after stage 3 reaches a closed `Memory` and is refused;
//! * nothing that can write to the ledger is running when stage 7 drains it.
//!   Stages 4 and 5 abort the event pump and the background tasks; stage 6
//!   ends the endpoint sessions and waits for them. One residue remains:
//!   rmcp runs each tool call in a task of its own, which a cancelled
//!   session waits on for up to 2 s (rmcp's drain) but does not join. A call
//!   still running past that drain, against a closed `Memory`, can still
//!   append its line; once stage 7 has begun, the ledger counts it as
//!   `write_failed`. Bounded and counted, never silent.
//!
//! Not watched: everything after `serve` returns. The watchdog is disarmed
//! when `serve` ends, before the `Memory`, store and embedder are dropped and
//! before the runtime shuts down, so a hang in those drops or in process exit
//! is still unlogged and is left to the supervisor's kill.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use super::signals::{shutdown_signal, EarlyShutdown};
use super::stages::{ShutdownProgress, Stage};
use crate::ledger::Ledger;
use crate::memory::Memory;
use crate::store::flush::CatchUnwindPoll;
use crate::store::lease;
use crate::types::{LamboError, SessionId};

/// How long a transport gets to wind itself down after the shutdown signal
/// before it is dropped and `close()` runs anyway.
///
/// This bound is the whole point (R1/T82-2). `axum::serve(..).with_graceful_shutdown`
/// waits for **every in-flight connection to finish**, and a streamable-HTTP MCP
/// client holds its server→client SSE channel open for the life of the session
/// (kept alive by `sse_keep_alive`, so it never idles out). Without a deadline,
/// graceful shutdown never returns, `Memory::close` never runs, and the tail is
/// lost — the exact durability failure the signal handling exists to prevent.
/// A dropped connection is recoverable; a dropped write-behind tail is not.
pub(super) const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Hard bound on the final [`Memory::close`] (R2-b).
///
/// `close()` is otherwise unbounded: a store that hangs on the final flush would
/// hang the process with the shutdown path already spent, which is exactly the
/// durability-vs-liveness trade the grace windows above exist to resolve.
/// Bounding it is safe *for liveness* — dropping the `close()` future returns
/// the drained tail to the front of the graph log (see `Memory::close`'s
/// *Cancellation* section), leaves the session closed to writers, and latches no
/// success, so the process can exit instead of wedging. It is **not** safe for
/// durability: that "returned to the log" is the *in-memory* write-behind log
/// (`src/graph/mod.rs`), and there is **no on-disk WAL**. On the serve path an
/// abandoned close is immediately followed by process exit, so the un-flushed
/// tail dies with the process — it is LOST, not recoverable on restart. (The
/// within-process retry semantics `Memory::close` documents apply only to a
/// caller that stays alive and calls `close()` again; `serve` does not.) Larger
/// than [`SHUTDOWN_GRACE`] because by the time close runs the transport is
/// already down and this is the last thing standing between the process and exit.
///
/// This is the budget for the **whole** close phase, split between the flush
/// attempt ([`CLOSE_FLUSH_GRACE`]) and the lease release that follows an
/// abandoned one ([`LEASE_RELEASE_GRACE`]).
pub(super) const CLOSE_GRACE: Duration = Duration::from_secs(10);

/// What [`Memory::close`] itself gets — [`CLOSE_GRACE`] minus the slice reserved
/// for [`LEASE_RELEASE_GRACE`].
///
/// The live review (L82-1) found that a close which blows its deadline is
/// *dropped*, so the lease release inside it never runs and the session stays
/// wedged for the full `LEASE_TTL`. Fixing that needs a window to release in,
/// and that window is carved **out of** `CLOSE_GRACE` rather than added on top:
/// [`SHUTDOWN_BUDGET`] is a number operators have already sized their
/// supervisor's SIGKILL timeout against, and a durability bug is not a reason to
/// quietly move it.
///
/// The two seconds this costs the flush are affordable because the same finding
/// was fixed at the root: [`crate::store::batch`] turned a flush from one
/// network round-trip per mutation into one per few hundred rows, so the
/// 784-mutation tail that could not drain in 10 s now plans into single-digit
/// statements.
///
/// `pub(crate)` so the burst-drain regression test in `memory` can assert
/// against the real budget instead of a copy of the number.
pub(crate) const CLOSE_FLUSH_GRACE: Duration = Duration::from_secs(8);

/// Build-time invariant: the write queue's close quiesce cannot become the
/// reason a `close()` blows the deadline `serve` gives it.
///
/// Asserted here, on the serving side, because the dependency runs this way:
/// the write queue is core and knows nothing about transports, while `serve`
/// is the consumer that sizes `close()`'s budget around it (#27).
const _: () = assert!(
    crate::writeq::WRITE_QUEUE_DRAIN_BUDGET.as_secs() * 4 <= CLOSE_FLUSH_GRACE.as_secs(),
    "WRITE_QUEUE_DRAIN_BUDGET must stay at or under a quarter of CLOSE_FLUSH_GRACE — the write \
     queue quiesce runs in series BEFORE the final flush, so it is carved out of close()'s \
     budget, not added to it",
);

/// Bound on the best-effort lease release that follows an abandoned `close()`
/// (L82-1).
///
/// One `UPDATE ... WHERE session_id = $1 AND holder = $2` against a cluster the
/// flush was just talking to. Two seconds is several round-trips' worth; if it
/// does not land in that, the row lapses at TTL exactly as it did before this
/// existed, and the process still exits.
pub(super) const LEASE_RELEASE_GRACE: Duration = Duration::from_secs(2);

/// Build-time invariant: the close phase's two halves add up to its budget.
///
/// An edit that grows either without shrinking the other — or that grows
/// [`CLOSE_GRACE`] expecting the flush to receive it — fails the build.
const _: () = assert!(
    CLOSE_FLUSH_GRACE.as_secs() + LEASE_RELEASE_GRACE.as_secs() == CLOSE_GRACE.as_secs(),
    "CLOSE_FLUSH_GRACE + LEASE_RELEASE_GRACE must be exactly CLOSE_GRACE — the close phase is \
     those two steps in series and nothing else, and SHUTDOWN_BUDGET is sized on CLOSE_GRACE",
);

/// Worst-case wall-clock from the shutdown signal to a durable (or honestly
/// lost) tail and a released lease (R4): stages 1 to 4.
///
/// Those stages are two bounded phases in series: the transport winds down
/// within [`SHUTDOWN_GRACE`] (rmcp's own graceful drain happens *inside* that
/// window — `run_until_shutdown` gives the whole transport, drain included,
/// exactly `SHUTDOWN_GRACE` after cancel), then the final flush runs within
/// [`CLOSE_GRACE`]; the keep-warm and event-pump aborts are instant. The
/// compile-time guard just below (and `the_grace_windows_are_sane`) pins the
/// sum to this budget so a later bump to either window cannot silently push
/// it past what a supervisor allows.
///
/// **It is not the time to process exit.** This said "the true aggregate cap",
/// with only `event_pump.abort()` and process teardown outside it; since #28
/// stage 6 waits up to `hub::ENDPOINT_RELEASE_GRACE` (3 s) and stage 7 up to
/// the ledger's `SHUTDOWN_DRAIN` (0.5 s), both after the lease release. The
/// exit bound is `watchdog::EXIT_BUDGET` (18.5 s), and the hard stop that holds
/// even when these timers cannot fire is `watchdog::SHUTDOWN_WATCHDOG` (20 s,
/// #40). A supervisor's SIGKILL escalation (launchd `ExitTimeOut`, systemd
/// `TimeoutStopSec`, Kubernetes `terminationGracePeriodSeconds`) must exceed
/// the watchdog, or the final flush can be cut off and the stall goes
/// unnamed: 30 s is the recommendation.
pub(super) const SHUTDOWN_BUDGET: Duration = Duration::from_secs(15);

/// Build-time invariant: the end-to-end shutdown cost fits [`SHUTDOWN_BUDGET`].
/// A future edit that pushes `SHUTDOWN_GRACE + CLOSE_GRACE` over the budget fails
/// the build, not just the test (`Duration::as_secs` is `const`).
const _: () = assert!(
    SHUTDOWN_GRACE.as_secs() + CLOSE_GRACE.as_secs() <= SHUTDOWN_BUDGET.as_secs(),
    "SHUTDOWN_GRACE + CLOSE_GRACE exceeds SHUTDOWN_BUDGET — a supervisor's SIGKILL timeout \
     is sized against the budget; lower a window or justify raising the budget",
);

/// Build-time invariant: the single-writer lease TTL comfortably outlasts the
/// whole shutdown budget (T8.6).
///
/// `Memory::close` releases the lease on a graceful shutdown, but the release
/// only lands after the transport has wound down and the final flush has run —
/// up to [`SHUTDOWN_BUDGET`] later. If the TTL were not larger than that budget,
/// a slow-but-graceful close could let the lease **expire mid-shutdown**, briefly
/// admitting a second writer while the first is still flushing its tail — the
/// exact hazard the lease exists to prevent. `LEASE_TTL` (45s) is 3× the budget;
/// this pins the relationship so a later bump to either window cannot silently
/// invert it.
const _: () = assert!(
    lease::LEASE_TTL.as_secs() > SHUTDOWN_BUDGET.as_secs(),
    "LEASE_TTL must exceed SHUTDOWN_BUDGET so a slow-but-graceful close releases the lease \
     rather than letting it expire mid-shutdown (T8.6)",
);

/// The only shutdown future [`serve`](super::serve)'s transports will accept (JE2E-R2-2).
///
/// # Why a newtype instead of `impl Future`
///
/// The ruling's entire behaviour lives in one expression — which future `serve`
/// hands to the transport — and round 2 demonstrated that **severing it passed
/// the whole suite**: replacing [`wind_down`]'s result with a bare
/// `shutdown_signal()` left 1016 tests green while every fenced holder went back
/// to living forever. Two tests pinned `wind_down` and the fenced close in
/// isolation; nothing pinned that `serve` composes them.
///
/// A test is the weaker answer to that, because it pins one spelling of a line
/// that a refactor is free to re-spell. So the transports take
/// `Pin<&mut HolderShutdown>` rather than a generic, and this type has exactly
/// one constructor — [`holder_shutdown`], which always wraps `wind_down`. The
/// severing mutation is now a **type error**, and any future re-plumbing of
/// `serve`'s shutdown still has to produce one of these, which still runs the
/// fence race. Closed by construction rather than by vigilance.
///
/// **Two constructors since #32 PR 4**, one per lease-loss policy: a
/// one-session serve gets [`holder_shutdown`] (the fence race, unchanged), a
/// serve holding several pinned sessions gets [`registry_shutdown`], whose
/// fence handling is per session (a lost lease detaches that session rather
/// than ending the process for all of them). `serve` chooses by
/// `LeaseLossPolicy`, derived in one place; the severing mutation on the
/// one-session path is still a type error, and pinned by
/// `serve_feeds_the_fence_into_the_transports_shutdown`.
///
/// The box costs one allocation per serve process, at startup, for a future
/// that is polled until the process ends. `wind_down` is an `async fn` and so
/// has no nameable type; boxing is what lets the *type* be the guarantee.
pub(crate) struct HolderShutdown(Pin<Box<dyn Future<Output = ()> + Send>>);

impl Future for HolderShutdown {
    type Output = ();

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.0.as_mut().poll(cx)
    }
}

/// Build this holder's wind-down future — **the only way to get a
/// [`HolderShutdown`]** (JE2E-R2-2).
///
/// `shutdown_signal()` is evaluated here, as an argument, so its eager handler
/// registration happens at this call and not at the first poll; see that
/// function for why that matters and what a lazier spelling would re-open.
///
/// `early` is J6's pre-arm — the registration installed back at the acquire —
/// and it is passed in **alongside** the fresh `shutdown_signal()`, not in
/// place of it. A signal that landed in the window between the two arming
/// points is recorded only by `early`; one that lands after is seen by both.
/// See [`EarlyShutdown`].
///
/// When the wind-down resolves, the holder's shutdown has begun: stage 1
/// (the transport drain) is started on `progress` before the transport sees
/// the future ready (#40).
pub(super) fn holder_shutdown(
    mem: Arc<Memory>,
    ledger: Option<Arc<Ledger>>,
    early: EarlyShutdown,
    progress: ShutdownProgress,
) -> HolderShutdown {
    // Evaluated here, outside the `async` block, so the registration stays
    // eager (see `shutdown_signal`).
    let signal = shutdown_signal();
    HolderShutdown(Box::pin(async move {
        wind_down(signal, early, mem, ledger).await;
        progress.begin(Stage::TransportDrain);
    }))
}

/// The wind-down of a serve holding several pinned sessions (#32 PR 4,
/// [`LeaseLossPolicy::DetachSession`](super::registry::LeaseLossPolicy)) —
/// **the only other way to get a [`HolderShutdown`]**.
///
/// The same two signal arms as [`wind_down`] (the fresh registration, made
/// eagerly here, and J6's pre-arm), and **no fence arm**: under
/// `DetachSession` a session that loses its lease is detached by its own
/// watcher (`registry::watch_lease`), and the process and its other sessions
/// keep serving (design §4.2). `serve` picks between this and
/// [`holder_shutdown`] by the registry's policy, which is derived in one
/// place (`LeaseLossPolicy::for_scope`); a one-session serve always gets
/// [`holder_shutdown`], so JE2E-4's exit on a lost lease is unchanged.
pub(super) fn registry_shutdown(
    early: EarlyShutdown,
    progress: ShutdownProgress,
) -> HolderShutdown {
    // Evaluated here, outside the `async` block, so the registration stays
    // eager (see `shutdown_signal`).
    let signal = shutdown_signal();
    HolderShutdown(Box::pin(async move {
        tokio::select! {
            () = signal => {}
            () = early.fired() => {}
        }
        progress.begin(Stage::TransportDrain);
    }))
}

/// What ends a holder's transport: a signal, **or** losing the single-writer
/// lease (JE2E-4; operator ruling, 2026-08-22).
///
/// The `signal` half is [`shutdown_signal`]'s, and the **eager registration**
/// that makes it work is a property of *that* function and of its call site —
/// `shutdown_signal()` is evaluated as this function's argument, so wrapping it
/// here defers only the polling. Nothing about the arming point moves; see
/// there.
///
/// # A fenced ex-holder winds down instead of living forever
///
/// The fence is an `AtomicBool` that gates *writes*. Before this, nothing tore
/// the serve down when it latched: an ex-holder kept its endpoint listener, its
/// established proxy connections and its own client, answering honest write
/// refusals and — the part that is not honest — silently **stale reads**, with
/// no staleness label, for as long as the process lived. That was the same trade
/// pre-J2, so nobody had re-made it. J2 changed the alternatives: a fenced serve
/// that *exits* is respawned by the client that spawned it, and the respawn
/// finds a live holder and comes back as a **proxy** to it. So the wind-down is
/// a self-heal, and staying up is now the strictly worse option.
///
/// # It is the SIGTERM path, deliberately, and not a faster one
///
/// The fence trip enters exactly the future the signal enters, so everything
/// downstream is unchanged and unduplicated: the transport is cancelled with its
/// grace window, `Memory::close` runs (its fenced branch drops the tail and does
/// **not** release a lease that is no longer ours — there is nothing to flush
/// past the fence, because writes have been refused since it latched), the
/// refusal poller and the heartbeat are aborted, the endpoint is unlinked *only
/// if it is still ours* (JE2E-2 — a fenced holder's successor is listening at
/// the same address, and these two findings compose here), and the ledger is
/// drained last with its `startup` / `lease` / `completion` lines intact. The
/// ordering at `serve`'s tail is what makes that true and it is not changed by
/// this: the drain runs after `run_and_close_sessions` returns, on the error path as much
/// as the success one.
///
/// The exit is therefore non-zero — `close`'s fenced branch returns its refusal
/// — which is the honest code: this process's tail was discarded. Both facts a
/// reader needs are in one line here, before any of it runs.
///
/// # It leaves an artifact, not just a stderr line (JE2E-R2-4)
///
/// §J4's bar is that **lease conflicts leave an artifact**, and a lease *loss*
/// is the largest lease event a holder can suffer. It was stderr-only: this
/// arm's `tracing::warn!` and `close()`'s fenced `tracing::error!`, neither of
/// which reaches the shared ledger. `completion` lines cover it only when
/// writes were in flight at the fence — and the commonest fence, like the
/// commonest holder death (JE2E-3's own argument), is **idle**, so an idle
/// fenced holder's ledger simply stopped mid-air. An operator asking "why did
/// this holder exit at T" could reconstruct it from the respawn's `startup` and
/// `proxying` lines, but only inferentially, which is the exact state JE2E-3
/// was filed against.
///
/// So the arm appends `kind:"lease", event:"lost", side:"holder"` naming the
/// winner, **before** it returns and thereby cancels the transport. It survives
/// by the existing ordering rather than by a new guarantee: the ledger is
/// drained at the very end of [`serve`](super::serve), after `run_and_close_sessions`, on the error
/// path as much as the success one. [`crate::ledger::party_key`]'s fallback
/// already files an unlisted event's other party under `counterparty`, which is
/// what this is — a lease token, not a socket path.
pub(super) async fn wind_down(
    signal: impl std::future::Future<Output = ()>,
    early: EarlyShutdown,
    mem: Arc<Memory>,
    ledger: Option<Arc<Ledger>>,
) {
    tokio::select! {
        () = signal => {}
        // J6. Ready on the FIRST poll when a signal already arrived in the
        // pre-handshake window, so the transport is cancelled before it serves
        // a byte and `close()` still flushes the tail. Nothing downstream
        // distinguishes the two signal arms — this is the same exit, learned
        // through the earlier registration.
        () = early.fired() => {}
        winner = mem.lease_lost_latched() => {
            book_lease_loss(&mem, ledger.as_ref(), &winner);
            if crate::store::erase::is_erased_holder(&winner) {
                // #32 PR 7: the session was erased (in this process through
                // the admin route, or by an erase that met a lapsed lease).
                // There is nothing left to serve or to proxy to.
                tracing::warn!(
                    session = %mem.session(),
                    "lambo serve: this session was erased; exiting, since it was the only \
                     session this process serves. Its in-memory tail is discarded and nothing \
                     recreates the session"
                );
                return;
            }
            tracing::warn!(
                session = %mem.session(),
                holder = %winner,
                "lambo serve: lease lost to {winner}, exiting so the client can respawn into a \
                 proxy — this process's writes have been refused since the fence latched and its \
                 reads would go on silently serving a graph another writer now owns. The tail it \
                 could not flush is discarded, exactly as a crash would discard it",
            );
        }
    }
}

/// The `kind:"lease", event:"lost", side:"holder"` line a holder books when
/// its lease fence latches, naming the `winner` (JE2E-R2-4). Shared by
/// [`wind_down`] and the per-session lease-loss watcher of a multi-session
/// serve, so both leave the same artifact.
pub(super) fn book_lease_loss(mem: &Memory, ledger: Option<&Arc<Ledger>>, winner: &str) {
    if let Some(ledger) = ledger {
        ledger.append(&crate::ledger::lease_line(
            "lost",
            "holder",
            &mem.session().to_string(),
            &mem.agent().to_string(),
            winner,
            None,
        ));
    }
}

/// Run the transport future, then close the session — **on every exit path**.
///
/// Split out from [`serve`](super::serve) so the "close always runs" guarantee is testable
/// without a real socket or handshake: the guarantee lives here, not tangled
/// with transport construction. Whatever the transport returns — clean
/// disconnect, forced-close `Ok`, or a transport `Err` — [`Memory::close`] runs
/// afterward, bounded by [`close_bounded`].
///
/// The event pump is aborted *after* `close()` (R1/T82-17): canonization and
/// conflict events emitted during the final drain are exactly what an operator
/// debugging a failed close wants on stderr, and aborting first threw them away.
///
/// `stop_before_close` is the opposite case: tasks nothing needs during the
/// close, aborted the moment the transport returns and before `close()`
/// starts. [`run_and_close_sessions`] takes it as a hook run at that moment
/// rather than a list taken before the transport, so a task the transport
/// started (a probe spawned by an attach while serving) is stopped too. Today that is the issue-13 embedder keep-warm and the process's
/// write-queue calibration probe (#32 PR 3): once no client can call, a touch
/// or a probe embed only competes with the final drain (and on a slow remote
/// embedder could keep a request in flight across it). Aborting is idempotent,
/// so `serve` still aborts the same tasks after close on its usual path.
///
/// Each stage is logged on `progress` (#40). Stage 1 was started by the
/// shutdown future when it resolved; a transport that ended on its own
/// (client hangup, transport error) gets both of its lines here.
///
/// One session: this is [`run_and_close_sessions`] over a set of one, which
/// is exactly what a single-session `serve` runs (#32 PR 2). Since that
/// split `serve` calls the set form itself, so this is the tests' seam,
/// gated like its readers (`serve`'s close and stage tests and
/// `memory::tests::shutdown`).
#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
pub(crate) async fn run_and_close(
    mem: Arc<Memory>,
    transport: impl Future<Output = Result<(), LamboError>>,
    event_pump: tokio::task::JoinHandle<()>,
    stop_before_close: &[tokio::task::AbortHandle],
    early: &EarlyShutdown,
    progress: &ShutdownProgress,
) -> Result<(), LamboError> {
    let session = SessionClose {
        mem: &mem,
        event_pump: &event_pump,
    };
    let stop_before_close = || {
        for task in stop_before_close {
            task.abort();
        }
    };
    run_and_close_sessions(&[session], transport, stop_before_close, early, progress).await
}

/// What stages 3 and 4 need of one attached session: its memory, to close,
/// and its event pump, to abort after the close.
pub(super) struct SessionClose<'a> {
    pub(super) mem: &'a Memory,
    pub(super) event_pump: &'a tokio::task::JoinHandle<()>,
}

/// `run_and_close` over every attached session (#32 design §3.5): stages
/// 1 and 2 are process-wide and run once, then [`close_sessions`] runs
/// stages 3 and 4 over the set.
///
/// The result is the transport's error if it failed (the closes still ran,
/// and their outcomes are not logged, as before the set), else the first
/// session's close error, else `Ok`; see [`SessionCloses::report`].
#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
pub(super) async fn run_and_close_sessions(
    sessions: &[SessionClose<'_>],
    transport: impl Future<Output = Result<(), LamboError>>,
    stop_before_close: impl FnOnce(),
    early: &EarlyShutdown,
    progress: &ShutdownProgress,
) -> Result<(), LamboError> {
    // Stages 1 and 2.
    let outcome = stop_transport(transport, stop_before_close, progress).await;
    // Stages 3 and 4.
    let closed = close_sessions(sessions, early, progress).await;

    outcome?;
    closed.report()
}

/// Stages 1 and 2, the process-wide half of the close: the transport winds
/// down (bounded inside the transport), then the tasks nothing needs during
/// the close are stopped. Returns the transport's outcome.
pub(super) async fn stop_transport(
    transport: impl Future<Output = Result<(), LamboError>>,
    stop_before_close: impl FnOnce(),
    progress: &ShutdownProgress,
) -> Result<(), LamboError> {
    // Stage 1: the transport winds down (bounded inside the transport).
    let outcome = transport.await;
    progress.end(Stage::TransportDrain);
    // Stage 2: tasks nothing needs during the close.
    progress.run(Stage::KeepWarmAbort, stop_before_close);
    outcome
}

/// Stages 3 and 4 over a set of sessions: close every session concurrently,
/// so one [`CLOSE_GRACE`] covers them all, then abort every event pump.
///
/// The per-session half of the shutdown, with nothing process-wide in it: no
/// transport and no keep-warm. [`run_and_close_sessions`] calls it after
/// stages 1 and 2; #32 PR 4's detach (design §3.4) calls it on its own, for a
/// set of one, under [`ShutdownProgress::for_session`].
///
/// The outcomes come back unlogged, in set order. The caller logs them with
/// [`SessionCloses::report`], or does not when something else already
/// decided the result (a transport error, in [`run_and_close_sessions`]).
pub(super) async fn close_sessions(
    sessions: &[SessionClose<'_>],
    early: &EarlyShutdown,
    progress: &ShutdownProgress,
) -> SessionCloses {
    // Stage 3: the session closes, concurrently.
    progress.begin(Stage::SessionClose);
    let results = join_all_catching(
        sessions
            .iter()
            .map(|session| close_bounded(session.mem, early))
            .collect(),
    )
    .await;
    // A member that panicked has no outcome to report. Its siblings' do: log
    // them, each naming its session, before the panic resumes (#32 review
    // L2), or a sibling's lost tail would go unlogged with it.
    let mut panicked = None;
    let mut closed = Vec::with_capacity(results.len());
    for (session, result) in sessions.iter().zip(results) {
        match result {
            Ok(outcome) => closed.push((session.mem.session().clone(), outcome)),
            Err(payload) => {
                panicked.get_or_insert(payload);
            }
        }
    }
    if let Some(payload) = panicked {
        let _ = SessionCloses {
            outcomes: closed,
            named: true,
        }
        .report();
        std::panic::resume_unwind(payload);
    }
    progress.end(Stage::SessionClose);
    // Stage 4: the event pumps, after the close.
    progress.run(Stage::EventPumpAbort, || {
        for session in sessions {
            session.event_pump.abort();
        }
    });
    SessionCloses {
        named: closed.len() > 1,
        outcomes: closed,
    }
}

/// The line an operator acts on when a session's tail did not reach the
/// store.
const TAIL_LOST: &str =
    "lambo serve: final flush failed — tail lost on exit, not durable (no on-disk WAL)";

/// Each session's close outcome from [`close_sessions`], with the session
/// it is about, in set order, not yet logged.
#[must_use = "an unreported close outcome is a tail loss nobody logged"]
pub(super) struct SessionCloses {
    outcomes: Vec<(SessionId, Result<(), LamboError>)>,
    /// Whether each line names its session: the set had more than one
    /// member.
    named: bool,
}

impl SessionCloses {
    /// Name the session on every outcome line, even in a set of one: a
    /// registry detach (#32 PR 4) runs in a process serving other
    /// sessions, where an unattributed "tail lost" would not say whose.
    pub(super) fn named(mut self) -> Self {
        self.named = true;
        self
    }

    /// Log each session's outcome, in set order, and fold them: the first
    /// close error, else `Ok`.
    ///
    /// For a set of one this is exactly the line a single-session serve has
    /// always logged, with no `session` field. With more than one, each line
    /// names its `session`: "tail lost" is the line an operator acts on, and
    /// N unattributed copies of it would not say whose tail.
    pub(super) fn report(self) -> Result<(), LamboError> {
        let named = self.named;
        let mut failed = None;
        for (session, closed) in self.outcomes {
            match closed {
                Err(e) => {
                    if named {
                        tracing::error!(session = %session, error = %e, "{TAIL_LOST}");
                    } else {
                        tracing::error!(error = %e, "{TAIL_LOST}");
                    }
                    failed.get_or_insert(e);
                }
                Ok(()) if named => tracing::info!(
                    session = %session,
                    "lambo serve: session closed, tail durable"
                ),
                Ok(()) => tracing::info!("lambo serve: session closed, tail durable"),
            }
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// Drive `futures` to completion concurrently and return their outputs in
/// order, like `futures::future::join_all` (not a dependency here).
///
/// On the calling task, not spawned: nothing has to be `'static`, and every
/// line the futures log reaches the caller's subscriber, in the caller's
/// span. A set of one is polled exactly as an `.await` on it would be.
///
/// **A member's panic does not cancel the others** (#32 review L3). The
/// panic is caught at that member's poll, the member is dropped (as the
/// unwind would have dropped it), and the rest run to completion. Only then
/// is the first panic resumed, so it still reaches the caller. Without this,
/// one session whose close panicked would drop every sibling's close
/// mid-flight: their tails lost and their leases held until `LEASE_TTL`.
/// For a set of one nothing changes: the panic propagates from the same
/// poll.
pub(super) async fn join_all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut outputs = Vec::with_capacity(futures.len());
    let mut panicked = None;
    for result in join_all_catching(futures).await {
        match result {
            Ok(output) => outputs.push(output),
            Err(payload) => {
                panicked.get_or_insert(payload);
            }
        }
    }
    if let Some(payload) = panicked {
        std::panic::resume_unwind(payload);
    }
    outputs
}

/// A panic payload caught at a member's poll.
pub(super) type Panic = Box<dyn std::any::Any + Send>;

/// [`join_all`] without the resume: each member's output, or the panic it
/// raised, in input order, once every member is done. For a caller that has
/// something to do with the siblings' outputs before the panic resumes
/// ([`close_sessions`] logs their close outcomes).
pub(super) async fn join_all_catching<F: Future>(futures: Vec<F>) -> Vec<Result<F::Output, Panic>> {
    let mut futures: Vec<Option<Pin<Box<CatchUnwindPoll<F>>>>> = futures
        .into_iter()
        .map(|future| Some(Box::pin(CatchUnwindPoll(future))))
        .collect();
    let mut outputs: Vec<Option<Result<F::Output, Panic>>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (slot, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            let Some(future) = slot.as_mut() else {
                continue;
            };
            match future.as_mut().poll(cx) {
                // A panicked member is dropped at once, as the unwind would
                // have dropped it, and never polled again.
                std::task::Poll::Ready(result) => {
                    *output = Some(result);
                    *slot = None;
                }
                std::task::Poll::Pending => pending = true,
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    outputs
        .into_iter()
        .map(|output| output.expect("join_all returns only once every future is ready"))
        .collect()
}

/// [`Memory::close`], bounded two ways (R2-b).
///
/// A [`CLOSE_GRACE`] deadline caps a store that hangs on the final flush, and a
/// **re-armed** shutdown signal lets an operator who sees the close stall press
/// Ctrl-C a second time to force the exit rather than have it swallowed. Either
/// path abandons the `close()` future, which is safe *for liveness*: the drained
/// tail returns to the front of the (in-memory) graph log, the session stays
/// closed to writers, and no success is latched — so the returned `Err` is honest.
///
/// It is **not** durable. Because `serve` exits right after abandoning close and
/// there is no on-disk WAL, the abandoned tail is LOST — the in-memory log dies
/// with the process. The messages below say exactly that; they must not promise a
/// restart will recover it (R4/P2 — a prior wording did, and it was false).
///
/// ## The lease is released even when the tail is lost (L82-1)
///
/// Abandoning `close()` drops it mid-flight, so the release on its own success
/// path never runs. The live review watched exactly that: SIGTERM under an
/// at-cap burst timed the close out and left a **stale lease row**, wedging the
/// session for the whole `LEASE_TTL` on top of losing the tail. Two failures for
/// one cause, and the second one is not inherent — the release is a statement
/// about this process being gone, not a claim that anything was written.
///
/// So both abandon paths below run [`Memory::release_lease_after_abandoned_close`]
/// under [`LEASE_RELEASE_GRACE`] before returning. It cannot rescue the tail and
/// does not pretend to: the returned error is unchanged, and a release that
/// itself times out leaves the row to lapse at TTL, which is where this started.
pub(super) async fn close_bounded(mem: &Memory, early: &EarlyShutdown) -> Result<(), LamboError> {
    close_bounded_until(mem, early.second_signal()).await
}

/// [`close_bounded`] with the re-armed signal passed in.
///
/// `shutdown_signal()` registers process-wide SIGINT/SIGTERM handlers, which a
/// unit test must not do to the whole test binary. Taking the future as an
/// argument lets `memory`'s tests drive the real body with
/// `std::future::pending()` — see `an_abandoned_close_releases_the_lease_through_serve`.
///
/// # The second signal is a count on J6's pre-arm
///
/// This section said J6's pre-arm ([`EarlyShutdown`]) was "deliberately NOT
/// wired in here" and that [`close_bounded`] "keeps building a **fresh**
/// `shutdown_signal()`". Neither is true any more: [`close_bounded`] passes
/// `early.second_signal()`, which resolves once the pre-arm has *counted* two
/// signals. The fresh registration read the first signal as a second under
/// CPU contention (its delivery reaches registrations only when the signal
/// driver runs, so one created in that gap catches the first signal) and
/// abandoned a close nobody asked to abandon; the count cannot be fooled by
/// when the record is written. The full argument is on
/// [`EarlyShutdown::second_signal`], and
/// `one_signal_does_not_abandon_the_close_but_two_do` pins it. A latched
/// record would still be wrong here, for the reason this section gave: it
/// would make the escape hatch ready on the first poll of every
/// signal-initiated close.
pub(crate) async fn close_bounded_until(
    mem: &Memory,
    shutdown: impl Future<Output = ()>,
) -> Result<(), LamboError> {
    // Scoped so the abandoned `close()` future is *dropped* before the release
    // below: it holds `close_state` and the writers' write guard, and its
    // documented cancellation behaviour (returning the drained tail to the front
    // of the log, latching no success) should run before anything else does.
    let outcome = {
        let close = mem.close();
        tokio::pin!(close);
        tokio::select! {
        // Bias toward the close itself: if it is already done, take that answer
        // rather than a signal delivered in the same poll.
        biased;
        r = tokio::time::timeout(CLOSE_FLUSH_GRACE, &mut close) => match r {
            Ok(r) => r,
            Err(_) => {
                tracing::error!(
                    grace_secs = CLOSE_FLUSH_GRACE.as_secs(),
                    "lambo serve: close() did not finish within the grace window — abandoning \
                     it and exiting; the un-flushed tail is LOST (the write-behind log is \
                     in-memory only, there is no on-disk WAL, and a restart will NOT recover it)"
                );
                Err(LamboError::Config(format!(
                    "close timed out after {}s; tail lost on exit, not durable",
                    CLOSE_FLUSH_GRACE.as_secs()
                )))
            }
        },
        () = shutdown => {
            tracing::warn!(
                "lambo serve: a second shutdown signal arrived during close — abandoning it \
                 and exiting; the un-flushed tail is LOST (in-memory write-behind log, no \
                 on-disk WAL, not recoverable on restart)"
            );
            Err(LamboError::Config(
                "close interrupted by a second shutdown signal; tail lost on exit, not durable"
                    .into(),
            ))
        }
        }
    };

    if outcome.is_err() {
        release_lease_bounded(mem).await;
    }
    outcome
}

/// Best-effort lease release on the way out of an abandoned close (L82-1).
///
/// Bounded so a store that is *why* the close hung cannot hang the exit too. A
/// timeout is logged, not returned: the caller's error is already the honest
/// account of what went wrong, and "we also could not tidy the lease" does not
/// change what an operator must do.
pub(super) async fn release_lease_bounded(mem: &Memory) {
    if tokio::time::timeout(
        LEASE_RELEASE_GRACE,
        mem.release_lease_after_abandoned_close(),
    )
    .await
    .is_err()
    {
        tracing::warn!(
            grace_secs = LEASE_RELEASE_GRACE.as_secs(),
            "lambo serve: could not release the single-writer lease within its window after an \
             abandoned close; the row will lapse at LEASE_TTL instead, and until then this \
             session refuses new writers"
        );
    }
}

/// Stage 7: drain the call ledger and report what it wrote, last of all.
///
/// Every task that appends to it has been stopped (stages 4 to 6: the event
/// pump, the background tasks, the endpoint sessions), and the `lease:lost`
/// line a fenced holder books in [`wind_down`] is already queued, so this
/// drain carries the session's last lines. The one exception is the residue
/// the stage table names: a tool call rmcp is still running past its 2 s
/// cancel drain may append after this, and is counted, not lost silently.
pub(super) fn close_ledger(ledger: Option<Arc<Ledger>>) {
    if let Some(ledger) = ledger {
        ledger.shutdown();
        tracing::info!(
            written = ledger.counters().written(),
            dropped = ledger.counters().dropped(),
            path = %ledger.path().display(),
            "lambo serve: call ledger closed"
        );
    }
}

//! How a serve process hears SIGINT / SIGTERM: the eager registration
//! ([`shutdown_signal`]), the counting one behind it
//! ([`shutdown_signal_counter`]), and J6's pre-arm ([`EarlyShutdown`]),
//! armed at the lease acquire through [`crate::memory::AttachShutdown`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// The signal registration armed the instant the single-writer lease is taken
/// (J6), which **only records** that a signal arrived.
///
/// # The window this closes
///
/// [`shutdown_signal`] is armed at [`holder_shutdown`](super::shutdown::holder_shutdown), the first statement
/// after [`resolve_role`](super::roles::resolve_role) returns. The lease, though, is taken *inside*
/// `resolve_role` — inside the `build_attach` call at the top of its election
/// loop — and everything between the two ran under the **default disposition**:
/// a SIGTERM landing there killed the process outright, so `Memory::close`
/// never ran and the write-behind tail (a clean run already has `mutations=1`,
/// the session-attach record) died with it. That is not a hypothetical: it is
/// CI run 32710994512 failing
/// `a_pre_handshake_sigterm_still_flushes_the_session_row` with
/// `unix_wait_status(15)` — killed by the signal, not exited on it.
///
/// The window is the reason that test's `"session attached"` matcher is loose:
/// it fires on the **memory-level** line, logged from inside the `Memory` build
/// right after the lease is acquired, precisely so the signal lands here rather
/// than in the guarded region below the arming (I-R2-2).
///
/// # Why the arming could not simply move up
///
/// J2-R1-7 rejected arming above `resolve_role`, and that ruling stands: the
/// election loop is allowed to run for the whole of [`ELECTION_BUDGET`](super::roles::ELECTION_BUDGET) — 20
/// seconds — by design, and a registration nothing polls makes the process
/// **SIGTERM-immune** for exactly as long as nothing polls it. Twenty seconds
/// of unkillable wait, bought to protect a process that holds no lease and no
/// tail, is the worse trade; see the arming comment in [`serve`](super::serve).
///
/// This type is armed on the **winning** branch only — from the
/// `LeaseOutcome::Acquired` arm of `MemoryBuilder::build_attach`, and from
/// nowhere else. A serve that is still electing has not armed, so the election
/// stays killable; a serve that loses and becomes a proxy never arms at all,
/// which is also how the wedge invariant survives: the hook sits behind the
/// acquire, so "a proxy never arms" is the same statement as "a proxy never
/// takes the lease".
///
/// # Why it does not re-create the immunity one `await` down
///
/// Arming at the acquire puts one genuinely unbounded `await` under the guard:
/// the startup load, which reads the whole durable session back. A passive flag
/// there would be J2-R1-7's trade again with a worse bound — *unbounded*
/// deafness instead of 20 seconds. So `build_attach` **races** that load against
/// [`EarlyShutdown::fired`]: a signal during the load abandons it, releases the
/// freshly-taken lease through the startup-error path that was already there,
/// and returns. Every remaining step under the guard — the daemon / flush /
/// canonization spawns, the attach log, the write pipeline, the `Memory`
/// construction, the return through `resolve_role` and the match in [`serve`](super::serve) —
/// is synchronous, so there is no second place a signal can be parked across.
/// The guard therefore covers no unbounded wait at all, which is the property
/// that makes it a durability fix rather than an availability regression.
///
/// # What "observe it" means
///
/// The record is a `watch<bool>`, set by a task that awaits one
/// [`shutdown_signal`] and does nothing else — it never blocks, holds no lock
/// and touches no store. Two places read it, which is why it is a watch rather
/// than the signal future itself: the startup-load race above, and
/// [`wind_down`](super::shutdown::wind_down), which selects it alongside the fresh `shutdown_signal()` that
/// [`holder_shutdown`](super::shutdown::holder_shutdown) still arms exactly as it did before. `wait_for` checks
/// the current value first, so if the signal already landed in the window,
/// `wind_down` completes on its **first poll** — the transport is cancelled
/// before it serves a byte, `close()` runs, and the process exits 0 with the
/// tail durable.
///
/// # It cannot swallow a second signal
///
/// `tokio::signal::unix` delivers to *every* live registration for a kind, not
/// to the first one to ask, so this one consuming a SIGTERM does not consume it
/// for the others. In particular [`close_bounded`](super::shutdown::close_bounded)'s re-arm — the operator's
/// "press Ctrl-C again to give up on a stalled close" escape — still works, and
/// still is not tripped by the signal that started the shutdown: a `watch`
/// receiver created after a value was sent does replay it, but a *fresh*
/// `signal()` registration does not replay a signal delivered before it
/// existed, and `close_bounded` builds a fresh one.
#[derive(Clone)]
pub(crate) struct EarlyShutdown {
    /// How many shutdown signals this process has been sent, not merely
    /// *whether* it has been sent one. The count is what makes
    /// [`EarlyShutdown::second_signal`] correct — see it for the CI failure
    /// that a boolean could not tell apart.
    pub(super) signals: tokio::sync::watch::Receiver<u64>,
    /// The sender, kept beside the receiver so [`EarlyShutdown::arm`] can be a
    /// `&self` method on the same handle the builder carries.
    pub(super) tx: Arc<tokio::sync::watch::Sender<u64>>,
    /// Latches on the first [`EarlyShutdown::arm`] so a second call cannot
    /// install a second registration. `build_attach` calls it exactly once per
    /// acquire and `MemoryBuilder` is `Clone`, so this is belt-and-braces
    /// rather than load-bearing — but a duplicate registration would be a real
    /// leak, and the check is one atomic.
    pub(super) armed: Arc<std::sync::atomic::AtomicBool>,
}

impl EarlyShutdown {
    /// A handle that is **not yet armed** — no signal handler is installed
    /// until [`EarlyShutdown::arm`] is called.
    ///
    /// Constructing one is free and installs nothing, which is what lets
    /// [`serve`](super::serve) hand it into the builder *before* the election runs while
    /// still arming only on the winning branch.
    pub(crate) fn unarmed() -> Self {
        let (tx, signals) = tokio::sync::watch::channel(0u64);
        Self {
            signals,
            tx: Arc::new(tx),
            armed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Install the registration. Called from the `LeaseOutcome::Acquired` arm
    /// of `MemoryBuilder::build_attach` and from nowhere else.
    ///
    /// Synchronous and non-blocking on purpose: it must not add an `await` to
    /// the acquire it follows. [`shutdown_signal`]'s registration is eager, so
    /// the handlers are installed by the time this returns — a signal that
    /// arrives one instruction later is already buffered by the registration
    /// rather than killing the process.
    pub(crate) fn arm(&self) {
        use std::sync::atomic::Ordering;
        if self.armed.swap(true, Ordering::SeqCst) {
            return;
        }
        // EAGER: `shutdown_signal()` is called HERE, not inside the spawned
        // task, so the handlers exist before this function returns rather than
        // whenever the scheduler first polls the task. Moving the call into the
        // `async move` below would re-open the very window this type closes,
        // with every gate green — the same trap `shutdown_signal`'s own
        // docstring records for `wind_down`.
        //
        // It counts rather than latches, and keeps counting after the first:
        // the close-phase escape hatch reads this record to tell an operator's
        // *second* Ctrl-C from the first, and it cannot do that from a boolean.
        let counter = shutdown_signal_counter();
        let tx = Arc::clone(&self.tx);
        tokio::spawn(counter(tx));
    }

    /// Whether [`EarlyShutdown::arm`] has run on this handle.
    ///
    /// The J6 pin that `build_attach` arms on the winning branch **and only**
    /// there: a losing attach must leave this `false`, which is the wedge
    /// invariant read through the signal disposition — a process that never
    /// took the lease never changed how it dies.
    // Gated exactly as their only consumers are: the J6 disposition tests live
    // in a `store-memory` + `embed-fixture` module, so a bare `#[cfg(test)]`
    // leaves these dead under `--no-default-features --features store-sqlite`
    // and CI's RUSTFLAGS `-D warnings` turns that into a build failure.
    #[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
    pub(crate) fn is_armed(&self) -> bool {
        self.armed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Record a signal without sending one.
    ///
    /// [`EarlyShutdown::arm`] installs process-wide SIGINT/SIGTERM handlers,
    /// which a unit test must not do to the whole test binary just to assert
    /// what happens *after* the record exists — the same reason
    /// [`close_bounded_until`](super::shutdown::close_bounded_until) takes its re-armed signal as an argument. This
    /// sets the watch directly, so the observer side can be driven with no
    /// handler and no real signal.
    // Gated exactly as their only consumers are: the J6 disposition tests live
    // in a `store-memory` + `embed-fixture` module, so a bare `#[cfg(test)]`
    // leaves these dead under `--no-default-features --features store-sqlite`
    // and CI's RUSTFLAGS `-D warnings` turns that into a build failure.
    #[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
    pub(crate) fn simulate_signal(&self) {
        self.tx.send_modify(|n| *n += 1);
    }

    /// Resolve once a signal has been recorded — **immediately** if one already
    /// was.
    ///
    /// `wait_for` inspects the current value before it waits, which is the
    /// whole point: the interesting case is a signal that landed while the
    /// session was still attaching, long before anything asked.
    pub(crate) async fn fired(&self) {
        self.at_least(1).await;
    }

    /// Resolve once a **second** shutdown signal has been recorded.
    ///
    /// This is [`close_bounded`](super::shutdown::close_bounded)'s escape hatch — the operator who watches a
    /// close stall and presses Ctrl-C again — and it is expressed as "the
    /// count reached two" rather than as a fresh `signal()` registration
    /// because the fresh registration got it wrong, and lost data doing so.
    ///
    /// The old spelling reasoned that a registration created after a signal was
    /// *delivered* does not replay it, so anything it caught had to be a second
    /// signal. Delivery is not the event that matters, though: tokio's unix
    /// handler only sets a flag and writes a byte to its self-pipe, and the
    /// watch that wakes registrations is not sent until the signal driver task
    /// gets scheduled to drain that pipe. Under CPU contention those are
    /// milliseconds apart, and any registration created in the gap sees the
    /// *first* signal and calls it the second.
    ///
    /// That is exactly what the pre-handshake durability test hit once the
    /// stdio hangup above was classified correctly: the holder closed on the
    /// stdin EOF that `Child::wait()` causes, `close_bounded` armed a fresh
    /// registration, and the single `SIGTERM` the test had already sent then
    /// landed on it — so the close was abandoned, `final flush failed — tail
    /// lost on exit` was logged, and the holder exited 1 with the session row
    /// gone. One signal, read as two. And it is not a test-only shape: closing
    /// stdin and then sending `SIGTERM` is the shutdown sequence the MCP spec
    /// prescribes for clients, so a real client shutting a holder down could
    /// lose the tail the same way.
    ///
    /// A count cannot be fooled by when the record is written: one `kill` is
    /// one increment whenever the driver gets round to it. Two signals close
    /// enough together to coalesce inside one `watch` value would count as one,
    /// which errs toward finishing the close — the safe direction, and not the
    /// shape of the impatient-operator case this serves anyway.
    ///
    /// Why an absolute two rather than "one more than the count when the close
    /// began": reading a baseline at the top of the close would reintroduce the
    /// very race this fixes, because the whole problem is that the first
    /// signal's increment may not have been written *yet* when the close
    /// starts. A baseline of zero read in that window makes the first signal
    /// look like the increment, and the close is abandoned again. Two is
    /// immune precisely because it does not depend on reading anything at a
    /// particular moment.
    ///
    /// The one behaviour this trades away, stated plainly: on a close reached
    /// *without* any signal — a client hangup, the `ConnectionClosed` path
    /// above — an operator's first Ctrl-C no longer abandons the close; it
    /// takes two. That is a deliberate trade and a small one, because the
    /// close is already bounded by [`CLOSE_GRACE`](super::shutdown::CLOSE_GRACE), so the cost is a bounded
    /// wait rather than a hang, and the thing bought with it is that a tail is
    /// never thrown away on a signal the operator only sent once.
    pub(crate) async fn second_signal(&self) {
        self.at_least(2).await;
    }

    /// Resolve once at least `n` shutdown signals have been recorded —
    /// **immediately** if that many already were.
    ///
    /// Parks forever on an unarmed handle: nothing is counting, so no claim
    /// about signals can honestly be made. On the serve path that cannot
    /// happen for anything that closes a session — [`EarlyShutdown::arm`] runs
    /// in the acquire, and only a process that acquired has a `Memory` to
    /// close — and on the library path ([`build_memory`](super::build_memory)) an unarmed handle is
    /// the whole point.
    pub(super) async fn at_least(&self, n: u64) {
        let mut rx = self.signals.clone();
        // `Err` means every sender is gone, which cannot happen while `self`
        // holds one; treat it as "no signal" and park rather than reporting a
        // shutdown nobody asked for.
        if rx.wait_for(|seen| *seen >= n).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// The builder's view of the pre-arm (#27): `memory` names this trait, not
/// the type, so the core does not depend on the serving layer. Both methods
/// forward to the inherent ones above.
impl crate::memory::AttachShutdown for EarlyShutdown {
    fn arm(&self) {
        EarlyShutdown::arm(self);
    }

    fn fired(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(EarlyShutdown::fired(self))
    }
}

/// Ctrl-C (and SIGTERM on unix), so `close()` still runs.
///
/// Registration is EAGER: the handlers are installed when this function is
/// *called*, not when the returned future is first polled. An `async fn` body
/// runs on first poll, which left a window between the "session attached" log
/// and the transport's first poll of this future where a signal still had the
/// default disposition — a SIGTERM in that window killed the process outright
/// (R2-a; observed as a CI-only failure of the pre-handshake durability test
/// on a loaded runner). `tokio::signal::unix::signal()` registers with the
/// runtime immediately and buffers a signal that arrives before `recv()` is
/// polled, so calling this before the attach log closes that window. Eagerness
/// only makes the arming *point* effective; it does not move it. The call site
/// in [`serve`](super::serve) sits as early as it does for that reason (I-R2-1), and it
/// cannot move above `resolve_role`: that loop is allowed to run for the whole
/// of [`ELECTION_BUDGET`](super::roles::ELECTION_BUDGET), **20 seconds**, by design, and arming over it would
/// make that wait unkillable (J2-R1-7). The figure was written here as 50
/// seconds — the pre-J2-L2 budget — until JE2E-7; the argument holds at 20s.
/// See the arming comment in [`serve`](super::serve) for the trade written out.
///
/// # It is not the *only* registration any more (J6)
///
/// This paragraph said "everything before the call site in [`serve`](super::serve) — the
/// pre-lease group (the endpoint derivation, `Ledger::open` and its startup
/// line, J4) and `resolve_role`, which takes the lease — is still unguarded".
/// Half of that is now false, and the false half is the half that lost data:
/// CI run 32710994512 killed a serve with `unix_wait_status(15)` inside
/// `resolve_role`, after the lease was taken, with `close()` un-run.
///
/// [`EarlyShutdown`] arms a second registration at the acquire — inside
/// `build_attach`, in the `LeaseOutcome::Acquired` arm — for the same eager
/// reason this function documents, and *only* records the arrival for
/// [`wind_down`](super::shutdown::wind_down) to read. So the accurate statement is now:
///
/// * the **pre-lease group** and the **election** above the acquire are
///   unguarded, deliberately, and stay killable — that is J2-R1-7's ruling and
///   J6 does not touch it;
/// * from the **acquire** onward the process is covered, first by the pre-arm
///   and then by this call site, with no gap between them.
///
/// The pre-arm calls *this* function, so its eagerness is this contract, used
/// twice. A future edit that made `EarlyShutdown::arm` construct the future
/// lazily instead would re-open the window with every gate green, exactly as
/// the `wind_down` trap below would.
///
/// **The eagerness survives [`wind_down`](super::shutdown::wind_down)** (JE2E-4), and the reason is which
/// expression runs when: `serve` writes `wind_down(shutdown_signal(), …)`, so
/// this function is *called* — and its handlers installed — while the argument
/// is evaluated, before `wind_down`'s body has been polled at all. A future
/// edit that moved the call inside `wind_down`'s body, or replaced the argument
/// with a lazily-constructed future, would silently re-open the R2-a window with
/// every gate green. That is the whole reason this contract is documented here,
/// on the function it is a property of, rather than beside the wrapper.
/// (JE2E-R2-1: it briefly *was* beside the wrapper — a `wind_down` inserted
/// between this docstring and this signature took the block with it, leaving
/// `shutdown_signal` undocumented and the eager-registration contract attached
/// to an `async fn` that installs nothing at call time.)
pub(super) fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // Register both handlers NOW. Errors (exotic platforms, exhausted
        // signal slots) degrade to the lazy ctrl_c path rather than failing.
        let int = signal(SignalKind::interrupt());
        let term = signal(SignalKind::terminate());
        async move {
            match (int, term) {
                (Ok(mut int), Ok(mut term)) => {
                    tokio::select! {
                        _ = int.recv() => {}
                        _ = term.recv() => {}
                    }
                }
                (Ok(mut int), Err(_)) => {
                    let _ = int.recv().await;
                }
                (Err(_), Ok(mut term)) => {
                    let _ = term.recv().await;
                }
                (Err(_), Err(_)) => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        async {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// [`shutdown_signal`], but it never stops listening — it counts.
///
/// Returns a *builder* of the counting future rather than the future itself so
/// the registration keeps [`shutdown_signal`]'s eager contract: the `signal()`
/// calls run when this function is called, inside [`EarlyShutdown::arm`], not
/// when the spawned task is first polled. A signal arriving one instruction
/// after `arm` returns is therefore already buffered by a live registration
/// rather than hitting the default disposition, which is the property R2-a and
/// J6 both turn on.
///
/// One registration, polled in a loop, is deliberate and is the part that makes
/// the count trustworthy. Creating a *new* registration per signal — the
/// obvious alternative — leaves a window between one signal being recorded and
/// the next registration existing, and a signal in that window is not counted
/// at all. That is the same class of mistake as the one
/// [`EarlyShutdown::second_signal`] documents, only in the other direction: it
/// would make the operator's second Ctrl-C occasionally do nothing.
pub(super) fn shutdown_signal_counter(
) -> impl FnOnce(Arc<tokio::sync::watch::Sender<u64>>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // Register both handlers NOW, exactly as `shutdown_signal` does and for
        // exactly the same reason. Errors (exotic platforms, exhausted signal
        // slots) degrade to the lazy ctrl_c path rather than failing.
        let int = signal(SignalKind::interrupt());
        let term = signal(SignalKind::terminate());
        move |tx| {
            Box::pin(async move {
                match (int, term) {
                    (Ok(mut int), Ok(mut term)) => loop {
                        // The `Option` is load-bearing and must not be dropped
                        // on the floor. `recv()` yields `None` when the
                        // registration behind it is gone, and `None` is
                        // *immediately* ready forever after — so a loop that
                        // treated it as an arrival would spin a core at full
                        // tilt and, far worse, drive this counter up until it
                        // tripped `second_signal` and abandoned a close that
                        // nobody had asked to abandon. That is the tail-loss
                        // failure this whole type exists to prevent, delivered
                        // by the fix for it. Tokio's global signal registry
                        // never drops its sender, so this is a guard rather
                        // than an expected path — which is exactly why it has
                        // to be written down rather than assumed.
                        let arrived = tokio::select! {
                            v = int.recv() => v.is_some(),
                            v = term.recv() => v.is_some(),
                        };
                        if !arrived {
                            return;
                        }
                        // A receiver is always alive (the `EarlyShutdown` this
                        // was spawned from holds one), and a failed send would
                        // mean the shutdown path is already gone.
                        tx.send_modify(|n| *n += 1);
                    },
                    (Ok(mut int), Err(_)) => loop {
                        if int.recv().await.is_none() {
                            return;
                        }
                        tx.send_modify(|n| *n += 1);
                    },
                    (Err(_), Ok(mut term)) => loop {
                        if term.recv().await.is_none() {
                            return;
                        }
                        tx.send_modify(|n| *n += 1);
                    },
                    // No registration at all: `ctrl_c()` resolves once, so this
                    // records the first signal and nothing after it. Degraded,
                    // and honest about being degraded — the escape hatch simply
                    // never trips, which loses no data.
                    (Err(_), Err(_)) => {
                        if tokio::signal::ctrl_c().await.is_ok() {
                            tx.send_modify(|n| *n += 1);
                        }
                    }
                }
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        }
    }
    #[cfg(not(unix))]
    {
        move |tx: Arc<tokio::sync::watch::Sender<u64>>| {
            Box::pin(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    tx.send_modify(|n| *n += 1);
                }
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        }
    }
}

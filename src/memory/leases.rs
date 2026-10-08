//! Single-writer custody of a session: the store lease, its heartbeat and
//! fence, and in-process second-writer detection.
//!
//! * **The lease** (T8.6) is taken by `MemoryBuilder::build_attach` before the
//!   startup load, refreshed by [`spawn_lease_heartbeat`], released only by a
//!   *successful* `close()` (or [`Memory::release_lease_after_abandoned_close`]),
//!   and left to lapse at TTL on every crash-shaped path, `Drop` included.
//! * **The fence** (T86-2) is the `lease_lost` flag the heartbeat latches when a
//!   refresh finds another holder. Every write gate and the flush loop obey it;
//!   [`LeaseLostSignal`] is the wake-up and winner id `serve` acts on (JE2E-4).
//! * **The registry** ([`ACTIVE_SESSIONS`]) reports, never refuses, a second
//!   `Memory` on one session in one process (T81-8). Keyed by session, so many
//!   sessions in one process (#32) are each tracked independently.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex as PlMutex;
use tokio::task::JoinHandle;

use super::Memory;
use crate::store::lease::{LeaseHolder, LeaseOutcome, LEASE_HEARTBEAT_INTERVAL, LEASE_TTL};
use crate::store::GraphStore;
use crate::types::{AgentId, LamboError, SessionId};

/// Live [`Memory`] handles per session, process-wide.
///
/// Two `Memory`s on one session each spawn a full task trio over **divergent
/// in-RAM copies** and flush both copies into the same rows: the later flush
/// wins, the other handle's writes are overwritten, one side's GC deletes nodes
/// the other still holds — and neither side looks wrong from the inside. Cheap
/// to detect, so it is detected.
///
/// **Reported, not refused.** Spec §2.2 assigns single-writer enforcement to
/// deployment, and a process-global refusal would be both too strong and too
/// weak: too strong because `build()` would gain a new failure mode for
/// legitimate re-attaches (a leaked handle whose owner dropped the reference,
/// a tool that opens a read-mostly second view), and far too weak because the
/// collisions that actually corrupt a session come from *other processes and
/// hosts*, which no in-process registry can see. Inventing a policy here would
/// buy a false sense of protection; an ERROR line naming both agents buys T8.2
/// the diagnostic it will actually want.
pub(super) static ACTIVE_SESSIONS: LazyLock<PlMutex<HashMap<SessionId, Vec<AgentId>>>> =
    LazyLock::new(|| PlMutex::new(HashMap::new()));

/// Record a handle; log loudly if the session already had one.
pub(super) fn register_session(session: &SessionId, agent: &AgentId) {
    let mut active = ACTIVE_SESSIONS.lock();
    let agents = active.entry(session.clone()).or_default();
    if !agents.is_empty() {
        tracing::error!(
            session = %session,
            agent = %agent,
            existing = ?agents,
            handles = agents.len() + 1,
            "SecondSessionWriter: this process already holds a Memory handle for session \
             {session} (agents {agents:?}) and is opening another for {agent}. Spec §2.2 is one \
             writer per session: the two handles keep divergent in-RAM graphs and flush them into \
             the same rows, so the later flush silently overwrites the other's writes. Close one."
        );
    }
    agents.push(agent.clone());
}

/// Release a handle's registration.
///
/// Reached through [`Memory::unregister_once`], from whichever comes first: a
/// **successful** [`Memory::close`], or [`Drop`] (so a handle that is never
/// closed still releases when it goes).
///
/// **A successful close releases; a failed one does not** (R2-4). Close then
/// re-attach in the same process is the MCP server's ordinary shape — and this
/// crate's own reload test — so holding the slot until `Drop` fired a
/// `SecondSessionWriter` ERROR against a handle that had already made its tail
/// durable and stopped every task: a false alarm on the one path that is
/// certainly safe, which is how ops-level detectors get ignored. A **failed**
/// close is the opposite case and keeps its slot: that handle still holds an
/// undurable tail in its in-RAM graph and is documented as retryable, so a
/// second writer arriving on the session is still the divergence the detector
/// is for.
pub(super) fn unregister_session(session: &SessionId, agent: &AgentId) {
    let mut active = ACTIVE_SESSIONS.lock();
    if let Some(agents) = active.get_mut(session) {
        if let Some(position) = agents.iter().position(|a| a == agent) {
            agents.remove(position);
        }
        if agents.is_empty() {
            active.remove(session);
        }
    }
}

/// The serve-facing half of the single-writer fence (JE2E-4).
///
/// The fence itself is the `AtomicBool` that [`FlushTask::with_fence`](crate::store::flush::FlushTask::with_fence) and every
/// [`Memory`] write path read. It is the **safety** mechanism, it is unchanged,
/// and it remains the authority on whether this handle still owns the session.
/// This type carries the two things a *serve* additionally needs in order to
/// **act** on the latch rather than merely obey it:
///
/// * a wake-up, so a wind-down does not have to poll for a transition that
///   happens at most once in a process's life; and
/// * the id of the writer that took the session, captured at the latch, so the
///   exit line can name it instead of saying "someone".
///
/// Deliberately not folded into the `AtomicBool`: `FlushTask::with_fence`'s
/// contract is a plain flag shared with the write gate, and widening it would
/// put a notifier on the flush hot path to serve one waiter that only ever wakes
/// once.
#[derive(Debug, Default)]
pub(crate) struct LeaseLostSignal {
    /// The new holder's token, from the refresh that found the lease gone.
    pub(super) winner: parking_lot::Mutex<Option<String>>,
    pub(super) woken: tokio::sync::Notify,
}

impl LeaseLostSignal {
    /// Record the winner and wake every waiter. Idempotent, like the fence
    /// beside it — the heartbeat keeps beating after a loss and may call this
    /// again; the first winner recorded is kept, because it is the one that was
    /// true at the transition.
    pub(super) fn latch(&self, winner: &str) {
        let mut slot = self.winner.lock();
        if slot.is_none() {
            *slot = Some(winner.to_string());
        }
        drop(slot);
        self.woken.notify_waiters();
    }

    /// Who took the session, if the fence has latched.
    pub(super) fn winner(&self) -> Option<String> {
        self.winner.lock().clone()
    }
}

/// Spawn the single-writer lease heartbeat (T8.6).
///
/// Refreshes the lease every [`LEASE_HEARTBEAT_INTERVAL`] (a third of the TTL),
/// so a live holder keeps the session and a crashed one's lease lapses within
/// one full TTL. The first tick is consumed immediately, so the first *refresh*
/// lands one interval after acquisition, not at once.
///
/// A refresh that comes back [`LeaseOutcome::Held`] means this handle **lost**
/// the session — its lease expired (a store outage starved the heartbeat past
/// the TTL) and another writer took over. On that transition the heartbeat
/// latches the shared `fence` (T86-2): the owning [`Memory`] then refuses every
/// further write and its write-behind flush loop stops and drops its pending
/// tail, so the two writers can no longer flush divergent graphs into one
/// session. The loss is still logged loudly, and the fence is idempotent, so the
/// heartbeat may keep beating after it without changing anything.
///
/// **What the process does about it is `serve`'s decision, and since JE2E-4 it
/// winds down** (operator ruling, 2026-08-22). The fence is a *safety*
/// mechanism and stays exactly what it was; `signal` is how a serve learns of
/// the latch promptly enough to act on it, and carries the id of the writer that
/// took the session so the exit line can name it. A library caller that wants
/// the old "stop safely and stay up" behaviour still gets it: nothing here ends
/// anything.
pub(super) fn spawn_lease_heartbeat(
    store: Arc<dyn GraphStore>,
    session: SessionId,
    holder: LeaseHolder,
    fence: Arc<AtomicBool>,
    signal: Arc<LeaseLostSignal>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(LEASE_HEARTBEAT_INTERVAL);
        // Skip the immediate first tick: refresh after one interval, not now.
        interval.tick().await;
        loop {
            interval.tick().await;
            match store.refresh_lease(&session, &holder, LEASE_TTL).await {
                Ok(LeaseOutcome::Acquired(_)) => {}
                Ok(LeaseOutcome::Held { current, .. }) => {
                    // T86-2: fence the writer. From here every write is refused
                    // and the flush loop stops overwriting the new holder's rows.
                    fence.store(true, Ordering::Release);
                    // JE2E-4: and wake whoever is waiting to act on it. The
                    // fence is latched FIRST, so a waiter that wakes always
                    // finds the flag already set.
                    signal.latch(&current.holder);
                    tracing::error!(
                        session = %session,
                        holder = %holder,
                        new_holder = %current.holder,
                        "single-writer lease LOST: this handle's lease expired (heartbeat starved \
                         past the TTL) and {} took the session. This handle is now FENCED — further \
                         writes are refused and its tail will NOT be flushed; an operator must \
                         reconcile.",
                        current.holder
                    );
                }
                Err(err) => {
                    // A transient store blip: log and keep beating. The lease has
                    // TTL slack for two missed refreshes before it can lapse.
                    tracing::warn!(
                        session = %session,
                        holder = %holder,
                        error = %err,
                        "single-writer lease heartbeat refresh failed; will retry next interval"
                    );
                }
            }
        }
    })
}

impl Memory {
    /// Give up this handle's [`ACTIVE_SESSIONS`] slot, at most once (R2-4).
    ///
    /// Called by a successful `close()` and by [`Drop`], whichever comes first.
    /// The flag is what makes it safe for both to call it: the registry keys on
    /// session + agent id, so a second release would evict whichever *other*
    /// handle had re-attached under the same ids in between — turning the
    /// second-writer detector into a source of the very blind spot it exists to
    /// remove.
    pub(super) fn unregister_once(&self) {
        if self.registered.swap(false, Ordering::AcqRel) {
            unregister_session(&self.session, &self.agent);
        }
    }

    /// Abort the lease heartbeat, if it is still running (T8.6). Synchronous, so
    /// both `close()` and `Drop` can call it. Idempotent — the second caller
    /// finds the slot already `None`.
    pub(super) fn abort_heartbeat(&self) {
        if let Some(handle) = self.heartbeat_handle.lock().take() {
            handle.abort();
        }
    }

    /// Release this handle's single-writer lease, at most once (T8.6).
    ///
    /// Called only from the **success** paths of `close()`: a graceful close
    /// hands the session off immediately rather than waiting out the TTL. Guarded
    /// by `lease_released` so a retried `close()` does not release twice — the
    /// release is holder-scoped in the store anyway, but the flag also skips a
    /// redundant round-trip. `Drop` deliberately does not call this (see the
    /// field docs): a handle abandoned without a clean close lets its lease lapse
    /// at TTL, the crash-shaped path.
    pub(super) async fn release_lease_once(&self) {
        if self.lease_released.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Err(err) = self
            .store
            .release_lease(&self.session, &self.lease_holder)
            .await
        {
            // Non-fatal: the lease lapses at TTL even if the explicit release
            // could not reach the store. A failed release must not turn a
            // durable close into an error.
            tracing::warn!(
                session = %self.session,
                holder = %self.lease_holder,
                error = %err,
                "could not release the single-writer lease on close; it will lapse at TTL instead"
            );
        }
    }

    /// Release the single-writer lease after a `close()` that was **abandoned**
    /// (L82-1).
    ///
    /// `close()` releases on its own success paths, and deliberately does not on
    /// a failed final flush — that failure keeps the lease for a retried close
    /// and lets it lapse at TTL if none comes. Neither covers the case the live
    /// review hit: `serve` bounds `close()` with a deadline, and a close that
    /// blows the deadline is *dropped*, so it never reaches either path. The
    /// process then exits with the lease row still present, and the session is
    /// wedged for the rest of the TTL — a second, avoidable failure stacked on
    /// top of the lost tail.
    ///
    /// Releasing here is sound because the release is not a durability claim.
    /// It says "this process is gone", which is true — `serve` calls this
    /// immediately before exiting. The tail is lost either way; keeping the
    /// lease does not make it less lost, it only makes the next writer wait 45 s
    /// to find that out.
    ///
    /// **Except when this handle was fenced.** A lost lease belongs to another
    /// writer, and `release_lease` is holder-scoped precisely so a straggler
    /// cannot evict it — but skipping the call entirely also skips the log line
    /// that would confuse an operator reading it. `close()`'s fenced branch has
    /// the same rule and the same reason.
    ///
    /// Best-effort by construction: `release_lease_once` already downgrades a
    /// store error to a warning, and the caller bounds this with its own
    /// deadline.
    pub async fn release_lease_after_abandoned_close(&self) {
        if self.lease_lost() {
            tracing::debug!(
                session = %self.session,
                "not releasing the single-writer lease after an abandoned close: this handle was \
                 fenced, so the lease belongs to another writer"
            );
            return;
        }
        // The heartbeat must not outlive the release and re-acquire what we just
        // gave up. `close()` aborts it first thing, but an abandoned close may
        // have been dropped before reaching that line.
        self.abort_heartbeat();
        self.release_lease_once().await;
    }

    /// Test hook (T86-2): latch the lease-lost fence exactly as
    /// [`spawn_lease_heartbeat`] does when a refresh comes back
    /// [`LeaseOutcome::Held`]. The real heartbeat only fires on its
    /// [`LEASE_HEARTBEAT_INTERVAL`] (15s), so a test drives the fence directly
    /// after arranging a real store-level takeover.
    ///
    /// Gate matches both callers' tests modules (not bare `test`): under
    /// `--no-default-features` feature combos those modules are compiled out and
    /// a bare `#[cfg(test)]` method becomes a dead-code error under
    /// `-D warnings` (CI feature-matrix).
    ///
    /// `pub(crate)` since J1-R2-2, so `mcp::server`'s tests can latch the fence
    /// too. Before that they could not, and the reserve path's lease-lost arm
    /// had no MCP-level test at all — which is how a `Conflict`-variant match
    /// came to render `lease_lost_error`'s operator SQL to a model without a
    /// single test going red. Widening a test hook by one crate is the cheaper
    /// half of that trade: the alternative (driving a real store-level takeover
    /// from `mcp::server::tests`, as `memory.rs`'s own fence test does) needs a
    /// second `Memory` on a shared store plus a lease acquisition, none of which
    /// the MCP assertion is about. The hook stays `#[cfg(test)]`, so it does not
    /// exist in a shipped binary.
    #[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
    pub(crate) fn simulate_lease_loss(&self) {
        self.simulate_lease_loss_to("another-writer@host#1");
    }

    /// [`Memory::simulate_lease_loss`] naming the writer that took the session,
    /// for the JE2E-4 wind-down tests that assert the exit line names it.
    #[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
    pub(crate) fn simulate_lease_loss_to(&self, winner: &str) {
        // Same order as the heartbeat: fence first, then wake, so a waiter that
        // wakes always finds the flag already set.
        self.lease_lost.store(true, Ordering::Release);
        self.lease_lost_signal.latch(winner);
    }

    /// `true` once the heartbeat latched a lost lease (T86-2).
    ///
    /// `pub(crate)` since JE2E-4, so `mcp::serve` can tell a fenced exit from an
    /// ordinary one — it is the difference between "we released the lease" and
    /// "another writer owns it", and it decides what the exit says.
    pub(crate) fn lease_lost(&self) -> bool {
        self.lease_lost.load(Ordering::Acquire)
    }

    /// Resolve once this handle's single-writer lease has been lost, with the
    /// id of the writer that took it (JE2E-4).
    ///
    /// **Waits forever on a healthy holder**, which is the point: it is one arm
    /// of `serve`'s wind-down `select!`, beside SIGTERM. The fence latches at
    /// most once in a process's life, so this is a wake-up rather than a poll.
    ///
    /// Race-free by construction, and the ordering is the whole of it: the
    /// waiter is registered (`enable`) *before* the flag is re-read, so a latch
    /// landing between the read and the await is still delivered. Both latch
    /// sites set the flag before they notify, so a wake always finds it set —
    /// but the flag, never the notification, is the authority, and the loop is
    /// what makes a spurious wake harmless.
    pub(crate) async fn lease_lost_latched(&self) -> String {
        loop {
            let notified = self.lease_lost_signal.woken.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.lease_lost() {
                return self
                    .lease_lost_signal
                    .winner()
                    .unwrap_or_else(|| "another writer".to_string());
            }
            notified.await;
        }
    }

    /// The honest refusal a fenced handle returns (T86-2): another writer owns
    /// the session now, so this process is no longer the writer and refuses to
    /// touch the graph. A `Conflict`, the same class as the build-time
    /// single-writer refusal.
    pub(super) fn lease_lost_error(&self) -> LamboError {
        LamboError::Conflict(format!(
            "session {} lost its single-writer lease: this process's lease expired (the store was \
             unreachable past the {}s TTL) and another writer took the session. This handle is no \
             longer the writer and refuses further writes — its tail will not be flushed. Spec \
             §2.2 is one writer per session; an operator must reconcile and, if needed, force a \
             takeover: {}",
            self.session,
            LEASE_TTL.as_secs(),
            crate::store::lease::OPERATOR_OVERRIDE,
        ))
    }
}

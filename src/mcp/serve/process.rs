//! The holder's process-wide part (#32 PR 2): the background tasks one
//! serve process runs whatever number of sessions it holds. The per-session
//! part is in `session`.

/// The holder's process-wide background tasks: spawned beside the transport
/// on the holder path, stopped after the close (stage 5), except the
/// keep-warm, which is also stopped before it (stage 2).
///
/// Process-wide, not per session (#32 design §3.1, which splits it from the
/// per-session tasks): the keep-warm touches the shared embedder, and the
/// heartbeat and the refusal poller are one task each for the whole process
/// once it serves many sessions. A single-session serve spawns them for its
/// one session, exactly as before.
pub(super) struct ProcessTasks {
    /// I2 heartbeat, when `--ledger-heartbeat` is set.
    pub(super) heartbeat: Option<tokio::task::JoinHandle<()>>,
    /// Issue #13 embedder keep-warm, when the backends ask for one.
    pub(super) keep_warm: Option<tokio::task::JoinHandle<()>>,
    /// J4 holder-side refusal poller, when a ledger is attached.
    pub(super) refusal_poller: Option<tokio::task::JoinHandle<()>>,
}

impl ProcessTasks {
    /// Stage 2's handles: the keep-warm, which stops when the transport does,
    /// before the close and its final drain (issue #13); see
    /// [`run_and_close`](super::shutdown::run_and_close).
    pub(super) fn stop_before_close(&self) -> Vec<tokio::task::AbortHandle> {
        self.keep_warm
            .iter()
            .map(tokio::task::JoinHandle::abort_handle)
            .collect()
    }

    /// Stage 5: stop every background task, after `close()`.
    ///
    /// After the close, deliberately: the tail's durability is the
    /// load-bearing guarantee and the ledger is not allowed to be in front of
    /// it. The heartbeat is stopped first so it cannot enqueue a line into a
    /// ledger that is draining (stage 7, which is bounded: a writer stuck on a
    /// hung filesystem is abandoned, never allowed to hold process exit).
    pub(super) fn stop(self) {
        if let Some(heartbeat) = self.heartbeat {
            heartbeat.abort();
        }
        // Issue #13. Already aborted inside `run_and_close`, before the close;
        // repeated here (idempotent) so this exit path aborts it without
        // relying on that. Nothing to drain: a touch writes nothing.
        if let Some(task) = self.keep_warm {
            task.abort();
        }
        // J4. The refusal-recorder task is stopped before the ledger drains, so
        // it cannot enqueue a line into a closing ledger.
        if let Some(poller) = self.refusal_poller {
            poller.abort();
        }
    }
}

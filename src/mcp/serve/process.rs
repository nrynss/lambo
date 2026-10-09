//! The holder's process-wide part (#32 PR 2): the background tasks one
//! serve process runs whatever number of sessions it holds. The per-session
//! part is in `session`.

use std::sync::Arc;
use std::time::Duration;

use super::heartbeat::{heartbeat_loop, record_refused_takeovers};
use crate::ledger::Ledger;
use crate::mcp::server::LamboServer;
use crate::memory::Memory;
use crate::writeq::EmbedderCalibration;

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
    /// #32 PR 3: the process's write-queue calibration, whose probe is
    /// aborted beside the keep-warm at stage 2 and again at stage 5. Held
    /// here so both stages' abort lists are built in one place that `serve`
    /// and its tests share (review P3-1).
    pub(super) calibration: EmbedderCalibration,
}

impl ProcessTasks {
    /// Spawn the holder's process-wide tasks, below the arming (see
    /// [`serve`](super::serve)): the I2 ledger heartbeat over `server`, the #13
    /// keep-warm over the session's embedder, and the J4 refusal poller.
    pub(super) fn spawn(
        server: &LamboServer,
        mem: &Arc<Memory>,
        ledger: &Option<Arc<Ledger>>,
        heartbeat_every: Option<Duration>,
        keep_warm: Option<Duration>,
        calibration: &EmbedderCalibration,
    ) -> Self {
        let heartbeat = match (ledger, heartbeat_every) {
            (Some(ledger), Some(every)) => {
                tracing::info!(
                    path = %ledger.path().display(),
                    interval_secs = every.as_secs(),
                    version = crate::ledger::VERSION,
                    git_sha = crate::ledger::GIT_SHA,
                    "lambo serve: call ledger open, heartbeat armed"
                );
                Some(tokio::spawn(heartbeat_loop(
                    server.clone(),
                    Arc::clone(ledger),
                    every,
                )))
            }
            (Some(ledger), None) => {
                tracing::info!(
                    path = %ledger.path().display(),
                    "lambo serve: call ledger open (no heartbeat)"
                );
                None
            }
            _ => None,
        };
        // Issue #13 — embedder keep-warm. Spawned here, below the arming and
        // beside the heartbeat, for the same reasons: spawning awaits nothing, so
        // the pre-handshake window is not widened, and the loop's first touch is
        // one full interval out, so startup gains no forward. Holder path only: a
        // proxy holds no embedder (it is released when `resolve_role` returns).
        // Aborted when the transport returns, before the close (see
        // `run_and_close_sessions`), and again beside the heartbeat after it.
        let keep_warm_task = keep_warm.map(|every| {
            tracing::info!(
                interval_secs = every.as_secs(),
                "lambo serve: embedder keep-warm armed"
            );
            tokio::spawn(crate::embed::keep_warm::keep_warm_loop(
                Arc::clone(mem.embedder()),
                every,
            ))
        });
        // J4 — the holder side of a refused takeover: record the incumbent's
        // line when the store reports a refusal this process turned away. Spawned
        // only when a ledger is attached, and only on the holder path (the proxy
        // branch returned above). Aborted at close like the heartbeat.
        let refusal_poller = match ledger {
            Some(ledger) => {
                let holder_token =
                    crate::store::lease::LeaseHolder::for_this_process(mem.agent()).token();
                Some(tokio::spawn(record_refused_takeovers(
                    mem.store().clone(),
                    mem.session().clone(),
                    mem.agent().clone(),
                    holder_token,
                    Arc::clone(ledger),
                )))
            }
            None => None,
        };
        Self {
            heartbeat,
            keep_warm: keep_warm_task,
            refusal_poller,
            calibration: calibration.clone(),
        }
    }

    /// Stage 2: abort the keep-warm, which stops when the transport does,
    /// before the close and its final drain (issue #13), and the write-queue
    /// calibration probe (#32 PR 3), which no session's close aborts and
    /// whose embeds would only compete with the final drains; see
    /// [`run_and_close_sessions`](super::shutdown::run_and_close_sessions).
    ///
    /// Called **at** stage 2, not before the transport runs: the calibration
    /// is asked for its probes then (`EmbedderCalibration::abort`), so a
    /// probe spawned by an attach during the transport (#32 PR 4's lazy
    /// attaches) is stopped here too, not only at stage 5 (review P3-2).
    pub(super) fn stop_before_close(&self) {
        if let Some(task) = &self.keep_warm {
            task.abort();
        }
        self.calibration.abort();
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
        // Issue #13. Already aborted inside `run_and_close_sessions`, before the close;
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
        // #32 PR 3. Already aborted at stage 2; repeated here (idempotent),
        // like the keep-warm. Dropping `self` drops this clone; the probe's
        // last holders are the calibration in `serve` and the sessions.
        self.calibration.abort();
    }
}

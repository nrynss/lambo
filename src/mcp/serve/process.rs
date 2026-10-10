//! The holder's process-wide part (#32 PR 2): the background tasks one
//! serve process runs whatever number of sessions it holds. The per-session
//! part is in `session`.

use std::sync::Arc;
use std::time::Duration;

use super::heartbeat::{
    heartbeat_ticks, poll_refused_takeovers, RefusalCursor, REFUSAL_POLL_INTERVAL,
};
use super::registry::SessionRegistry;
use crate::embed::Embedder;
use crate::ledger::Ledger;
use crate::types::AgentId;
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
    /// [`serve`](super::serve)): the I2 ledger heartbeat and the J4 refusal
    /// poller, each one task over every session in `registry` (#32 PR 4),
    /// and the #13 keep-warm over the process's one `embedder`, taken from
    /// the resolved backends rather than from a session, so it does not
    /// depend on which session attached first or outlive none of them.
    ///
    /// Spawned before the sessions' own parts, which keeps the startup's log
    /// order; the heartbeat and the poller wait for
    /// [`SessionRegistry::mark_started`] before their first round, so the
    /// heartbeat's immediate first line still covers every startup session.
    pub(super) fn spawn(
        registry: &Arc<SessionRegistry>,
        ledger: &Option<Arc<Ledger>>,
        heartbeat_every: Option<Duration>,
        keep_warm: Option<Duration>,
        embedder: &Arc<dyn Embedder>,
        agent: &AgentId,
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
                Some(tokio::spawn(registry_heartbeat(
                    Arc::clone(registry),
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
                Arc::clone(embedder),
                every,
            ))
        });
        // J4 — the holder side of a refused takeover: record the incumbent's
        // line when the store reports a refusal this process turned away. Spawned
        // only when a ledger is attached, and only on the holder path (the proxy
        // branch returned above). Aborted at close like the heartbeat.
        let refusal_poller = ledger.as_ref().map(|_| {
            let holder_token = crate::store::lease::LeaseHolder::for_this_process(agent).token();
            tokio::spawn(registry_refusal_poller(
                Arc::clone(registry),
                agent.clone(),
                holder_token,
            ))
        });
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
    /// is shut down then (`EmbedderCalibration::shutdown`), so a probe
    /// spawned by an attach during the transport (#32 PR 4's lazy attaches)
    /// is stopped here too, not only at stage 5 (review P3-2). The shutdown
    /// is final: an attach still in flight afterwards starts no probe.
    pub(super) fn stop_before_close(&self) {
        if let Some(task) = &self.keep_warm {
            task.abort();
        }
        self.calibration.shutdown();
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
        self.calibration.shutdown();
    }
}

/// The I2 heartbeat over every attached session: each beat appends one
/// `stats` line per session, in pinned order, to that session's own ledger
/// handle (so each line names its session). A one-session serve writes
/// exactly the line it always has.
async fn registry_heartbeat(registry: Arc<SessionRegistry>, every: Duration) {
    registry.started().await;
    heartbeat_ticks(every, || {
        for session in registry.attached() {
            if let Some(ledger) = session.server.ledger() {
                ledger.append(&session.server.heartbeat_line());
            }
        }
    })
    .await;
}

/// The refusal poller's cursors, one per session (#32 PR 6 review L2).
///
/// A cursor is kept while its session is pinned or attached, and for
/// [`LEASE_TTL`](crate::store::lease::LEASE_TTL) after an on-demand session
/// is detached: a fresh cursor reaches back one `LEASE_TTL`, and the holder
/// token it filters by is this process's, unchanged across a reattach, so a
/// session reattached inside that window with a fresh cursor would book
/// every refusal already booked in it again. Past the window a fresh cursor
/// sees only refusals made while the session was detached, never booked.
/// At most [`DETACHED_CURSORS_MAX`] detached cursors are kept (the oldest
/// detach goes first), so sessions coming and going cannot grow the map.
#[derive(Default)]
pub(super) struct RefusalCursors {
    cursors: std::collections::HashMap<String, RefusalCursor>,
    /// When each kept cursor's session was first seen detached.
    detached: std::collections::HashMap<String, std::time::Instant>,
}

/// The most cursors [`RefusalCursors`] keeps for detached sessions.
pub(super) const DETACHED_CURSORS_MAX: usize = 1024;

impl RefusalCursors {
    /// Keep the cursors of sessions `live` says are pinned or attached;
    /// keep a detached one's for `LEASE_TTL` from `now`, its first round
    /// detached.
    pub(super) fn prune(&mut self, live: impl Fn(&str) -> bool, now: std::time::Instant) {
        let ttl = crate::store::lease::LEASE_TTL;
        for id in self.cursors.keys() {
            if live(id) {
                self.detached.remove(id);
            } else {
                self.detached.entry(id.clone()).or_insert(now);
            }
        }
        let expired: Vec<String> = self
            .detached
            .iter()
            .filter(|(_, at)| now.saturating_duration_since(**at) >= ttl)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.detached.remove(&id);
            self.cursors.remove(&id);
        }
        while self.detached.len() > DETACHED_CURSORS_MAX {
            let Some(oldest) = self
                .detached
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.detached.remove(&oldest);
            self.cursors.remove(&oldest);
        }
    }

    /// Session `id`'s cursor, kept or new.
    pub(super) fn cursor(&mut self, id: &str) -> &mut RefusalCursor {
        self.detached.remove(id);
        self.cursors
            .entry(id.to_string())
            .or_insert_with(RefusalCursor::starting_now)
    }
}

/// How long the refusal poller sleeps between rounds over `attached`
/// sessions (design §3.6): `max(REFUSAL_POLL_INTERVAL, 100 ms × attached)`,
/// so the store load stays flat as the set grows. One session polls every
/// 500 ms, as before.
pub(super) fn refusal_poll_interval(attached: usize) -> Duration {
    let per_session = Duration::from_millis(100).saturating_mul(attached as u32);
    REFUSAL_POLL_INTERVAL.max(per_session)
}

/// The J4 holder-side refusal poller over every attached session: one task,
/// one cursor per session, each round polling each attached session once.
///
/// Every session's cursor outlives a detach, so a session that is detached
/// and attached again resumes where it stopped (see [`RefusalCursors`]).
async fn registry_refusal_poller(registry: Arc<SessionRegistry>, agent: AgentId, my_token: String) {
    registry.started().await;
    let mut cursors = RefusalCursors::default();
    loop {
        tokio::time::sleep(refusal_poll_interval(registry.attached().len())).await;
        let attached = registry.attached();
        cursors.prune(
            |id| registry.is_pinned(id) || attached.iter().any(|s| s.id().as_str() == id),
            std::time::Instant::now(),
        );
        for session in attached {
            let Some(ledger) = session.server.ledger() else {
                continue;
            };
            let cursor = cursors.cursor(session.id().as_str());
            poll_refused_takeovers(
                session.mem.store(),
                session.id(),
                &agent,
                &my_token,
                ledger,
                cursor,
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #32 PR 6 review L2: a detached session's cursor survives a reattach
    /// inside `LEASE_TTL`, and is dropped after it.
    #[test]
    fn a_detached_sessions_cursor_survives_a_reattach_within_the_lease_ttl() {
        let mut cursors = RefusalCursors::default();
        let t0 = std::time::Instant::now();
        let marker = chrono::DateTime::<chrono::Utc>::from_timestamp(1_000, 0).expect("a time");
        cursors.cursor("od").cursor = marker;
        // Detached for a while, then attached again: the same cursor.
        cursors.prune(|_| false, t0);
        cursors.prune(|_| false, t0 + crate::store::lease::LEASE_TTL / 2);
        assert_eq!(
            cursors.cursor("od").cursor,
            marker,
            "kept across the reattach"
        );
        // Detached again, past the TTL: dropped, and a new one starts now.
        let t1 = t0 + crate::store::lease::LEASE_TTL;
        cursors.prune(|_| false, t1);
        cursors.prune(|_| false, t1 + crate::store::lease::LEASE_TTL);
        assert_ne!(cursors.cursor("od").cursor, marker, "dropped after the TTL");
        // A pinned or attached session's is never dropped.
        cursors.cursor("pin").cursor = marker;
        cursors.prune(|id| id == "pin", t1 + crate::store::lease::LEASE_TTL * 10);
        assert_eq!(cursors.cursor("pin").cursor, marker);
    }

    #[test]
    fn detached_cursors_are_bounded() {
        let mut cursors = RefusalCursors::default();
        let t0 = std::time::Instant::now();
        for i in 0..DETACHED_CURSORS_MAX + 5 {
            cursors.cursor(&format!("s{i}"));
            cursors.prune(|_| false, t0 + Duration::from_millis(i as u64));
        }
        assert_eq!(cursors.cursors.len(), DETACHED_CURSORS_MAX);
        assert!(!cursors.cursors.contains_key("s0"), "the oldest went first");
    }
}

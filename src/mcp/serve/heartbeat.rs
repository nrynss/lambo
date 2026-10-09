//! The holder's ledger and event wiring (I1, I2, J4): the ledger
//! configuration refusal, the `stats` heartbeat, the pre-lease `startup`
//! line, the holder-side refusal poller and the daemon event pump. Each task
//! is spawned by [`serve`](super::serve) on the holder path and aborted there.

use std::sync::Arc;
use std::time::Duration;

use super::{ServeOptions, Transport};
use crate::ledger::Ledger;
use crate::mcp::server::LamboServer;
use crate::store::lease;
use crate::types::{DaemonEvent, LamboError};

/// Both ledger configuration errors, refused in one place.
///
/// **`--ledger-heartbeat` without `--ledger`.** An operator who asked for
/// heartbeats and got a server with no ledger at all would find out a day later,
/// from an absent file. Refusing at startup costs them one flag; the alternative
/// costs them the run.
///
/// **A zero heartbeat interval.** `tokio::time::interval` panics on a zero
/// period, and a heartbeat that fired as fast as the executor allows would be a
/// flood, not a heartbeat. The guard used to live only in `main.rs`, which left
/// two holes: a `serve()` caller that is not the CLI got a silently-panicked
/// heartbeat task, and the two configuration errors exited with two different
/// codes (1 and 2) for the same class of mistake. Both are refused here now, so
/// both take the same path out — and the CLI's wording is kept verbatim, since it
/// is the message an operator has already learned to read.
///
/// Split out from [`serve`](super::serve) so it is testable without a store, a transport, or a
/// lease, and called before any lease is taken.
pub fn authorize_ledger(opts: &ServeOptions) -> Result<(), LamboError> {
    match (&opts.ledger, opts.ledger_heartbeat) {
        (None, Some(secs)) => Err(LamboError::Config(format!(
            "--ledger-heartbeat {}s was given without --ledger: heartbeat lines are written TO \
             the call ledger, so there is nowhere to put them. Pass --ledger <path> as well, or \
             drop --ledger-heartbeat.",
            secs.as_secs()
        ))),
        (_, Some(every)) if every.is_zero() => Err(LamboError::Config(
            "--ledger-heartbeat must be at least 1 second (0 given); omit the flag to disable \
             heartbeats"
                .to_string(),
        )),
        _ => Ok(()),
    }
}

/// Append a `stats` heartbeat line every `every` (I2).
///
/// The first line lands immediately rather than one interval in: it stamps the
/// binary's version and sha at the moment the session attached, which is the
/// "which pinned binary produced this stretch of ledger" question the heartbeat
/// exists to answer. Waiting an interval would leave the first stretch
/// unattributed.
///
/// Runs until aborted. `Memory::stats()` is synchronous and holds no lock
/// across an await (spec §6.4) — it takes the graph read lock, counts, and
/// releases before this function's next `tick()`.
pub(super) async fn heartbeat_loop(server: LamboServer, ledger: Arc<Ledger>, every: Duration) {
    heartbeat_ticks(every, || ledger.append(&server.heartbeat_line())).await;
}

/// The heartbeat's cadence: run `beat` at once, then every `every`, until
/// aborted. [`heartbeat_loop`] beats one session.
pub(super) async fn heartbeat_ticks(every: Duration, mut beat: impl FnMut()) {
    let mut ticker = tokio::time::interval(every);
    // Skip missed ticks rather than firing a burst to catch up: a heartbeat
    // backlog after a stall would be a pile of near-identical lines stamped
    // microseconds apart, which is noise, not history.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        beat();
    }
}

/// The J4 pre-lease startup line: this serve's intent to acquire the
/// single-writer lease, written to the ledger before `resolve_role` makes its
/// first acquire attempt. See [`crate::ledger::startup_line`].
pub(super) fn serve_startup_line(opts: &ServeOptions) -> serde_json::Value {
    crate::ledger::startup_line(
        &opts.session,
        &opts.agent,
        match opts.transport {
            Transport::Stdio => "stdio",
            Transport::Http => "http",
        },
    )
}

/// How often the holder's refusal-recorder task re-checks the store.
pub(super) const REFUSAL_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// J4 — the holder side of a refused acquisition. Spawned only in the holder
/// branch of [`serve`](super::serve): it polls the store for lease refusals this process
/// turned away and appends a `lease:refused_takeover` line for each it has not
/// yet recorded. This and [`record_refused_loser`](super::roles::record_refused_loser) together make "a refused
/// lease acquisition appears in the ledger from both sides" true.
///
/// Refusals recorded against a *previous* holder are filtered out by matching
/// `current_holder` against this process's own lease token, and each refusal is
/// deduped by (refused_by, at) so a repeated poll never double-logs. The read
/// window starts a little before the poller's own start so no refusal at the
/// acquire boundary is missed; the dedup set is what keeps it exact.
///
/// # The cursor moves (JE2E-1)
///
/// `since` used to be computed **once**, at task start, and never advanced:
/// every 500 ms poll therefore re-read and re-allocated the whole accumulated
/// history from `start − LEASE_TTL` to now, and `seen` grew monotonically
/// beside it. On a long-lived `--ledger` holder facing the workstream's own
/// founding scenario — a client that auto-respawns a losing serve — that is
/// quadratic work over the holder's uptime and an unbounded set.
///
/// The cursor is now advanced to the **maximum `at` this poll saw**, which is
/// sound because [`crate::store::GraphStore::pending_lease_refusals`] is
/// inclusive at its lower bound (`refused_at >= since`): the next poll re-reads
/// exactly the newest instant, and the dedup set retires the duplicate. The
/// overlap is deliberate — a cursor advanced *past* the newest row would drop a
/// second refusal stamped at the same store instant.
///
/// `seen` is bounded by the same move: it only ever needs to hold the rows the
/// next poll can re-deliver, which is the rows at the cursor instant, so it is
/// rebuilt per poll from that instant rather than accumulated. Rows *older*
/// than the cursor are unreachable by construction and cannot be re-logged.
/// The bookkeeping is [`RefusalCursor`], extracted for the same reason
/// [`waiting_fits`](super::roles::waiting_fits) was: the *claim about* it is what a review can check.
pub(super) async fn record_refused_takeovers(
    store: Arc<dyn crate::store::GraphStore>,
    session: crate::types::SessionId,
    agent: crate::types::AgentId,
    my_token: String,
    ledger: Arc<Ledger>,
) {
    let mut cursor = RefusalCursor::starting_now();
    loop {
        tokio::time::sleep(REFUSAL_POLL_INTERVAL).await;
        poll_refused_takeovers(&store, &session, &agent, &my_token, &ledger, &mut cursor).await;
    }
}

/// One poll of [`record_refused_takeovers`]: append a `refused_takeover`
/// line for each refusal this holder turned away that `cursor` has not yet
/// seen. A store error drops this poll; the next one retries.
pub(super) async fn poll_refused_takeovers(
    store: &Arc<dyn crate::store::GraphStore>,
    session: &crate::types::SessionId,
    agent: &crate::types::AgentId,
    my_token: &str,
    ledger: &Ledger,
    cursor: &mut RefusalCursor,
) {
    match store.pending_lease_refusals(session, cursor.since()).await {
        Ok(refusals) => {
            for r in cursor.take_new(refusals, my_token) {
                ledger.append(&crate::ledger::lease_line(
                    "refused_takeover",
                    "holder",
                    &session.to_string(),
                    &agent.to_string(),
                    &r.refused_by,
                    Some(serde_json::json!({ "at": r.at.to_rfc3339() })),
                ));
            }
        }
        Err(_) => {
            // A seed / store blip; the next poll retries.
        }
    }
}

/// How far **below** the cursor each poll re-reads (JE2E-R2-5).
///
/// A refusal's `refused_at` and the moment its row becomes *visible* are not the
/// same instant. On Cockroach `now()` is the transaction's read timestamp while
/// visibility is commit-ordered, so a slow-committing INSERT can surface a row
/// stamped *earlier* than one that committed before it — and a cursor that had
/// already advanced past that stamp would exclude it with `refused_at >= since`
/// forever. A cursor that only moves forward trades unbounded re-reads for that
/// window; re-reading a fixed slice below it trades the window back for a
/// bounded, constant overlap.
///
/// One second is chosen against the thing being absorbed — commit latency plus
/// store-clock offset between two processes — the same quantity
/// [`lease::LEASE_TTL`]'s slack covers at a much larger scale, and two orders
/// above the measured refusal path (a refused start's INSERT is a single
/// statement). It costs one extra second of rows per poll, deduped, and the
/// dedup set is bounded by the same second rather than by uptime.
pub(super) const REFUSAL_OVERLAP_SECS: i64 = 1;

/// The holder-side refusal poller's read window and its dedup set (JE2E-1,
/// widened by JE2E-R2-5).
///
/// Three invariants:
///
/// * **No refusal is logged twice.** The read is inclusive at its lower bound
///   *and* deliberately overlaps the previous one, so rows come back; `seen` is
///   what retires them.
/// * **No refusal is skipped because it arrived late.** The read starts
///   [`REFUSAL_OVERLAP_SECS`] below the cursor, so a row whose stamp lands under
///   an already-advanced cursor — commit-order versus stamp-order, see the
///   constant — is still delivered and still logged.
/// * **Neither the window nor the set grows with uptime.** The cursor moves
///   forward with the newest row seen, and `seen` is pruned to the overlap
///   window on every poll, so both are bounded by a second of traffic rather
///   than by how long this holder has been up.
///
/// The cursor lands **on** the newest row seen, never past it: a store stamp has
/// finite resolution, so two refusals can share one instant and advancing past
/// it would drop the second. The overlap subsumes that, but the property is
/// kept because it is the cheaper of the two guarantees and does not depend on
/// the constant being right.
///
/// **The residual, stated rather than implied.** A row that becomes visible more
/// than [`REFUSAL_OVERLAP_SECS`] below the cursor is still never logged. The
/// window is not "rows older than the cursor cannot be *re*-logged" — it is that
/// they can never be logged at all — and the constant is what bounds how late a
/// row may be. What survives regardless: the loser's own `refused` line (written
/// by the loser, on its own ledger) and the store row itself, retained for
/// [`lease::LEASE_REFUSAL_RETENTION`]. Only the holder-side `refused_takeover`
/// line is lost, and the recorder is best-effort by construction — a store error
/// already drops a poll.
pub(super) struct RefusalCursor {
    pub(super) cursor: chrono::DateTime<chrono::Utc>,
    /// `(refused_by, at)` already logged, for rows inside the overlap window.
    /// Pruned to that window on every poll, which is what bounds it.
    pub(super) seen: std::collections::HashSet<(String, chrono::DateTime<chrono::Utc>)>,
}

impl RefusalCursor {
    pub(super) fn starting_at(cursor: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            cursor,
            seen: Default::default(),
        }
    }

    /// A cursor for a poller starting now: its first read reaches back one
    /// [`lease::LEASE_TTL`], so no refusal at the acquire boundary is missed.
    pub(super) fn starting_now() -> Self {
        Self::starting_at(
            chrono::Utc::now()
                - chrono::Duration::from_std(lease::LEASE_TTL)
                    .unwrap_or_else(|_| chrono::Duration::seconds(0)),
        )
    }

    pub(super) fn overlap() -> chrono::Duration {
        chrono::Duration::seconds(REFUSAL_OVERLAP_SECS)
    }

    /// The lower bound to read from on the next poll: the cursor, less the
    /// overlap.
    pub(super) fn since(&self) -> chrono::DateTime<chrono::Utc> {
        self.cursor - Self::overlap()
    }

    /// Consume one poll's rows: return the ones that are new to this holder,
    /// and advance the window over them.
    ///
    /// Rows whose `current_holder` is not `my_token` were refused by a
    /// *previous* holder of this session and are none of this process's
    /// business — they are skipped and, deliberately, do **not** move the
    /// cursor: moving it over another holder's row could carry the window past
    /// one of ours stamped at the same instant.
    pub(super) fn take_new(
        &mut self,
        refusals: Vec<crate::store::lease::LeaseRefusal>,
        my_token: &str,
    ) -> Vec<crate::store::lease::LeaseRefusal> {
        let mut new = Vec::new();
        let mut newest = self.cursor;
        for r in refusals {
            if r.current_holder != my_token {
                continue;
            }
            // One key, one question: has this exact refusal been logged? The
            // overlap makes re-delivery the normal case rather than an edge, so
            // the dedup carries the stamp as well as the token.
            if self.seen.insert((r.refused_by.clone(), r.at)) {
                new.push(r.clone());
            }
            if r.at > newest {
                newest = r.at;
            }
        }
        self.cursor = newest;
        // Everything the next read can re-deliver, and nothing else. This is
        // the line that keeps the set bounded by a second of traffic instead of
        // by this holder's uptime (JE2E-1's other half).
        let floor = self.since();
        self.seen.retain(|(_, at)| *at >= floor);
        new
    }
}

/// Drain the daemon's event stream into the log.
///
/// A dropped or lagging receiver is not an error (spec §6.1); a lagging one
/// re-syncs.
pub(super) async fn log_events(mut rx: tokio::sync::broadcast::Receiver<DaemonEvent>) {
    loop {
        match rx.recv().await {
            Ok(ev) => tracing::info!(event = ?ev, "daemon event"),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "daemon event stream lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

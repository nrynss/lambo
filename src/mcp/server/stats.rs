//! The `lambo_stats` payload, shared with the I2 heartbeat so the two can
//! never report different numbers, and the GC summary both render.

use serde_json::json;

use super::LamboServer;

/// The text half of the `gc` object, one line on the `lambo_stats` summary.
pub(super) fn gc_summary_line(g: &crate::memory::GcStats) -> String {
    let at = g
        .last_gc_at
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "never".into());
    match &g.last_sweep {
        None => format!(
            "gc: last_gc_at={at} last_gc_epoch={} last_sweep=none (this process)",
            g.last_gc_epoch
        ),
        Some(s) => format!(
            "gc: last_gc_at={at} last_gc_epoch={} last_sweep trigger={} collected={} \
             deferred={} cap={} cap_bound={}",
            g.last_gc_epoch,
            s.trigger.map(|t| t.as_str()).unwrap_or("direct"),
            s.collected,
            s.deferred,
            s.collection_cap,
            s.cap_bound
        ),
    }
}

/// `lambo_stats`' `gc` object (issue #29) — one builder for the tool and the
/// I2 heartbeat. `last_gc_at` is RFC 3339 or null; `last_sweep` is null until
/// this process has swept.
pub(super) fn gc_stats_json(g: &crate::memory::GcStats) -> serde_json::Value {
    json!({
        "last_gc_at": g.last_gc_at.map(|t| t.to_rfc3339()),
        "last_gc_epoch": g.last_gc_epoch,
        "last_sweep": g.last_sweep.as_ref().map(|s| json!({
            "trigger": s.trigger.map(|t| t.as_str()),
            "collected": s.collected,
            "deferred": s.deferred,
            "collection_cap": s.collection_cap,
            "cap_bound": s.cap_bound,
            "resources_spared_by_dependents": s.resources_spared_by_dependents,
            "survivors_deferred": s.survivors_deferred,
        })),
    })
}

impl LamboServer {
    /// The `lambo_stats` numbers, as the JSON both `lambo_stats` and the I2
    /// heartbeat report.
    ///
    /// One builder so the two can never drift: a heartbeat that disagreed with
    /// the tool would make the whole time axis in `scripts/observability`
    /// unreadable.
    pub(super) fn stats_json(&self) -> serde_json::Value {
        self.stats_json_with_gc(&self.mem.gc_stats())
    }

    /// [`Self::stats_json`] over a `gc` reading the caller already took, so
    /// `lambo_stats` can render its structured `gc` object and its text `gc:`
    /// line from one value (two reads straddling the daemon's first anchor or a
    /// sweep could otherwise disagree within one answer).
    pub(super) fn stats_json_with_gc(&self, gc: &crate::memory::GcStats) -> serde_json::Value {
        let s = self.mem.stats();
        let mut payload = json!({
            "session": s.session.0,
            "agent": s.agent.0,
            "flush_lag_ms": s.flush_lag.as_millis() as u64,
            "log_depth": s.log_depth,
            "flush_depth": s.flush_depth,
            "dead_lettered": s.dead_lettered,
            "degraded": s.degraded,
            "node_count": s.node_count,
            "edge_count": s.edge_count,
            "concept_count": s.concept_count,
            "total_concepts": s.concept_count,
            "embedded_concepts": s.embedded_concepts,
            "canonical_count": s.canonical_count,
            "epoch": s.epoch,
            "daemon_cycles": s.daemon_cycles,
            "canonization_cycles": s.canonization_cycles,
            "canonization_failures": s.canonization_failures,
            // Which promotion policy the cycles above are actually running.
            // The counters cannot answer it: `canonization_cycles` climbs
            // identically under both policies and `canonical_count` staying 0
            // is the normal reading for a young swarm session AND the whole
            // symptom of a `Solo` selection that did not take. Read from the
            // live `Config`, so it reports the value that WON — file, env, or
            // default — rather than any one of the three inputs.
            "promotion_policy": self.mem.config().promotion_policy.as_str(),
            // Issue #29: GC's sweep accounting, read-side only. An additive
            // key: `last_gc_at`/`last_gc_epoch` are the durable mark,
            // `last_sweep` the last sweep THIS process ran (null after a
            // restart until the next one).
            "gc": gc_stats_json(gc),
        });
        // I1: dropped lines are reported next to written ones so a gap in the
        // ledger is never mistaken for a gap in the traffic. Emitted ONLY when
        // a ledger exists — with `--ledger` off the payload is byte-identical
        // to what it was before I1, which is what "off by default means no
        // behaviour change" has to mean for a payload.
        //
        // `ledger_dropped_lines` stays the headline total ("is this ledger
        // complete?"); the two `_channel_full` / `_write_failed` keys beside it
        // answer "why", which the total cannot: backpressure means the writer is
        // behind, a failed write means the path is broken, and an operator
        // reading one number cannot tell those apart. Additive keys on a payload
        // that only exists when the ledger is on.
        if let Some(ledger) = &self.ledger {
            let obj = payload.as_object_mut().expect("json! built an object");
            obj.insert(
                "ledger_path".into(),
                json!(ledger.path().display().to_string()),
            );
            obj.insert(
                "ledger_written_lines".into(),
                json!(ledger.counters().written()),
            );
            obj.insert(
                "ledger_dropped_lines".into(),
                json!(ledger.counters().dropped()),
            );
            obj.insert(
                "ledger_dropped_channel_full".into(),
                json!(ledger.counters().dropped_channel_full()),
            );
            obj.insert(
                "ledger_dropped_write_failed".into(),
                json!(ledger.counters().dropped_write_failed()),
            );
            // I-R2-3. Queue depth, because the drop counters have a blind spot
            // about themselves: on a path whose `open` blocks (reader-less FIFO,
            // hung mount) the writer parks before its first write, so `written`
            // and both drop counters read `0` — indistinguishable from an idle
            // server — until CHANNEL_CAPACITY lines have piled up. This key moves
            // on the first call, so "writer parked" is visible immediately.
            obj.insert(
                "ledger_queued_lines".into(),
                json!(ledger.counters().queued()),
            );
        }
        // J3. Unconditional, unlike the `ledger_*` keys above, and the
        // difference is not an inconsistency: the ledger is an optional
        // subsystem, so "off by default means no behaviour change" is a promise
        // that can be kept for it byte-for-byte. The write queue has no off
        // switch — every `lambo_derive` goes through it — so there is no
        // baseline payload left to preserve, and hiding the keys behind a
        // condition that is always true would only make them look optional.
        //
        // `write_queue_bound` / `write_queue_lane_bound` are the static
        // fairness/memory caps in force (the J3 redesign — no rate sizes a
        // bound any more); `write_queue_measured` and the rate keys are the
        // embedder telemetry that used to size them and now only describes
        // them. `write_queue_accepted` is here so the gauge is re-derivable
        // from the payload — `outstanding = accepted − applied − failed −
        // deferred` — which is the property I-R2-3 asked `ledger_queued_lines`
        // for and the reason `dropped` sits beside them rather than inside
        // them.
        {
            let queue = self.mem.pipeline();
            let c = queue.counters();
            let calibration = queue.calibration();
            let obj = payload.as_object_mut().expect("json! built an object");
            obj.insert(
                "write_queue_bound".into(),
                json!(calibration.map_or(crate::writeq::WRITE_QUEUE_MAX, |c| c.bound)),
            );
            // The bound that actually refuses one agent's burst, reported
            // beside the aggregate one (J3-R1-1): a lane drains 1-wide however
            // wide the deployment's embedder is, so this is the number that
            // explains a drop a single-agent session sees. Since the J3
            // redesign it is the per-agent fair share, not a measurement.
            obj.insert(
                "write_queue_lane_bound".into(),
                json!(calibration.map_or(crate::writeq::WRITE_QUEUE_LANE_MAX, |c| c.lane_bound)),
            );
            obj.insert(
                "write_queue_measured".into(),
                json!(calibration.is_some_and(|c| c.measured())),
            );
            // `probe`, `observed` or `unmeasured`. The probe fires at the
            // coldest moment of the process's life and measured a 7x spread
            // across repeats on one host (J3-R1-2), so "measured" is not enough
            // on its own: an operator needs to know whether the number is a
            // startup estimate or this deployment's own observed writes.
            obj.insert(
                "write_queue_bound_source".into(),
                json!(calibration.map_or("unmeasured", |c| c.source.tag())),
            );
            obj.insert(
                "write_queue_items_per_sec".into(),
                json!(calibration.and_then(|c| c.items_per_sec)),
            );
            obj.insert(
                "write_queue_serial_items_per_sec".into(),
                json!(calibration.and_then(|c| c.serial_items_per_sec)),
            );
            // The probe's own serial figure, kept beside whichever rate is in
            // force (J3-R2-4). Replacing a number is not a reason to destroy
            // it: `serial_items_per_sec` alone tells an operator what the
            // deployment retires at now, and the GAP between the pair is the
            // self-diagnosing comparison — how far the startup estimate sat
            // from the work the agents actually send, the fact two review
            // rounds had to measure at a release binary because nothing
            // published it. Equal to `write_queue_serial_items_per_sec` while
            // `bound_source` is `probe`, and frozen at the probe's reading
            // after that.
            obj.insert(
                "write_queue_probe_serial_items_per_sec".into(),
                json!(calibration.and_then(|c| c.probe_serial_items_per_sec)),
            );
            // #11. The ratio the server already logs once at the takeover,
            // published so a rig can watch it without the log: how many times
            // faster the probe read than the rate now in force. `null` until
            // observation has taken over from a probe that landed.
            obj.insert(
                "write_queue_probe_optimism".into(),
                json!(calibration.and_then(|c| c.probe_optimism())),
            );
            // #11. Admission-to-settle latency of the last
            // APPLY_LATENCY_WINDOW applied writes, in whole milliseconds:
            // what a caller waiting on a receipt experiences, so a rig can
            // check its derive latency against the published wait_ms maximum
            // without joining the ledger. `null` before the first applied
            // write.
            let latency = queue.apply_latency();
            obj.insert(
                "write_queue_apply_samples".into(),
                json!(latency.map_or(0, |l| l.samples)),
            );
            let ms = |pick: fn(&crate::writeq::ApplyLatencySummary) -> std::time::Duration| {
                json!(latency.as_ref().map(|l| pick(l).as_millis() as u64))
            };
            obj.insert("write_queue_apply_ms_p50".into(), ms(|l| l.p50));
            obj.insert("write_queue_apply_ms_p90".into(), ms(|l| l.p90));
            obj.insert("write_queue_apply_ms_max".into(), ms(|l| l.max));
            obj.insert("write_queue_outstanding".into(), json!(c.outstanding()));
            obj.insert("write_queue_accepted".into(), json!(c.accepted()));
            obj.insert("write_queue_applied".into(), json!(c.applied()));
            obj.insert("write_queue_failed".into(), json!(c.failed()));
            obj.insert("write_queue_abandoned".into(), json!(c.abandoned()));
            obj.insert("write_queue_dropped".into(), json!(c.dropped()));
            // Split out of `write_queue_dropped`'s total, not subtracted from
            // it (J3-R1-8): "the embedder is the bottleneck" and "the session
            // is shutting down and refused a tail" are the same count but
            // opposite diagnoses, and `dropped` remains their sum so no count
            // vanishes.
            obj.insert(
                "write_queue_dropped_closed".into(),
                json!(c.dropped_closed()),
            );
            // J3 durable intents: `deferred` counts this session's acked
            // writes a clean close handed to the NEXT serve as durable
            // intents (a fourth settle class — neither applied nor failed);
            // `replayed` counts a PREVIOUS process's intents this session
            // applied at attach (not summed into `applied`, which counts only
            // this session's own accepted jobs, so `outstanding` stays exact).
            obj.insert("write_queue_deferred".into(), json!(c.deferred()));
            obj.insert("write_queue_replayed".into(), json!(c.replayed()));
            // J3 round-1 N1: the replay DEBT, not a total — durable intents
            // this session found owed and has not yet paid. Non-zero with
            // `replayed` not advancing is the visible form of "the embedder was
            // not answering at attach, so nothing was consumed".
            obj.insert("write_queue_replay_owed".into(), json!(c.replay_owed()));
            // J3 round-2 R-8: a *level* (`replay_owed`) cannot tell "draining"
            // from "wedged"; this names the class of the error that ended the
            // last replay. `null` = draining/idle, "embedder" = sick/wedged,
            // "other" = store/lease/config.
            obj.insert(
                "write_queue_replay_blocked".into(),
                match c.replay_blocked() {
                    crate::writeq::ReplayBlockReason::None => json!(null),
                    crate::writeq::ReplayBlockReason::Embedder => json!("embedder"),
                    crate::writeq::ReplayBlockReason::Other => json!("other"),
                },
            );
            obj.insert("receipts_retained".into(), json!(queue.receipts_retained()));
        }
        payload
    }

    /// Build one I2 heartbeat line: the `lambo_stats` payload, this process's
    /// uptime, and the binary's version + git sha.
    pub fn heartbeat_line(&self) -> serde_json::Value {
        crate::ledger::stats_line(self.stats_json(), self.started_at.elapsed())
    }
}

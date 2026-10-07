//! The lambo_stats payload: GC accounting, promotion policy, bounds,
//! waits and config clamping.

use super::*;

/// Issues #29 and #30 coexist in `lambo_stats`: recalling and inspecting
/// (accesses noted, then applied) leaves the `gc` object present, the
/// epoch and the durable sweep mark where they were, and the payload's
/// key set exactly as before, so no access accounting leaked into it.
#[tokio::test]
async fn stats_gc_block_is_unmoved_by_recall_and_inspect_accesses() {
    let s = server_with_config(
        "mcp-gc-stats-access",
        Config {
            daemon_tick_interval: Duration::from_secs(3_600),
            ..Config::default()
        },
    )
    .await;
    call(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "auth middleware", "concept_type": "entity"}]
        }),
    )
    .await;
    s.mem.settle_daemon().await;
    let stats_call = || call(&s, "lambo_stats", json!({"agent_id": "agent-a"}));
    let before = stats_call().await.structured_content.expect("payload");

    let inspect = call(
        &s,
        "lambo_inspect",
        json!({"agent_id": "agent-a", "focus": "auth middleware"}),
    )
    .await;
    assert_eq!(inspect.is_error, Some(false), "{inspect:?}");
    let recall = call(
        &s,
        "lambo_recall",
        json!({"agent_id": "agent-a", "query": "auth middleware"}),
    )
    .await;
    assert_eq!(recall.is_error, Some(false), "{recall:?}");
    assert!(
        s.mem.unapplied_accesses() > 0,
        "premise: accesses are noted"
    );
    s.mem.settle_daemon().await;

    let after = stats_call().await.structured_content.expect("payload");
    assert!(after["gc"].is_object(), "{after}");
    assert_eq!(after["epoch"], before["epoch"], "accesses never move it");
    assert_eq!(
        after["gc"]["last_gc_epoch"], before["gc"]["last_gc_epoch"],
        "nor the durable sweep mark"
    );
    let keys =
        |p: &serde_json::Value| -> Vec<String> { p.as_object().unwrap().keys().cloned().collect() };
    assert_eq!(keys(&after), keys(&before));
    s.mem.close().await.expect("close");
}

/// Issue #29: `lambo_stats` carries a `gc` object — the durable sweep mark
/// and the last sweep this process ran (null before one) — and a matching
/// summary line. Read-side only: asking twice changes nothing.
#[tokio::test]
async fn stats_reports_gc_sweep_accounting() {
    let s = server("mcp-gc-stats").await;
    let epoch_before = s.mem.stats().epoch;
    // The daemon's first cycle may anchor the clock concurrently, so the
    // payload must match the mark read just before or just after it.
    let before = s.mem.gc_stats();
    let stats = call(&s, "lambo_stats", json!({"agent_id": "agent-a"})).await;
    let after = s.mem.gc_stats();
    let payload = stats.structured_content.expect("stats payload");
    let gc = payload["gc"].as_object().expect("gc object");
    let matches = |m: &crate::memory::GcStats| {
        gc["last_gc_epoch"] == json!(m.last_gc_epoch)
            && gc["last_gc_at"] == json!(m.last_gc_at.map(|t| t.to_rfc3339()))
    };
    assert!(matches(&before) || matches(&after), "{payload}");
    assert!(gc["last_sweep"].is_null(), "no sweep yet: {payload}");
    let summary = payload["summary"].as_str().unwrap();
    assert!(
        summary.contains("gc: last_gc_at=") && summary.contains("last_sweep=none"),
        "{summary}"
    );
    let again = call(&s, "lambo_stats", json!({"agent_id": "agent-a"})).await;
    assert_eq!(
        again.structured_content.unwrap()["epoch"],
        json!(epoch_before),
        "reading GC stats writes nothing"
    );
    s.mem.close().await.expect("close");
}

/// `lambo_stats` renders its structured `gc` object from the reading it is
/// handed, not from a second read: a fabricated reading that no daemon
/// state could produce must come back verbatim.
#[tokio::test]
async fn stats_json_renders_the_gc_reading_it_is_given() {
    use chrono::TimeZone;
    let s = server("mcp-gc-one-read").await;
    let fabricated = crate::memory::GcStats {
        last_gc_at: Some(
            chrono::Utc
                .with_ymd_and_hms(1999, 12, 31, 23, 0, 0)
                .unwrap(),
        ),
        last_gc_epoch: 987_654_321,
        last_sweep: None,
    };
    let payload = s.stats_json_with_gc(&fabricated);
    assert_eq!(payload["gc"], gc_stats_json(&fabricated));
    s.mem.close().await.expect("close");
}

/// The `gc` object and line render a completed sweep's numbers.
#[test]
fn gc_stats_json_and_line_render_a_sweep() {
    use chrono::TimeZone;
    let at = chrono::Utc.with_ymd_and_hms(2026, 10, 7, 9, 0, 0).unwrap();
    let g = crate::memory::GcStats {
        last_gc_at: Some(at),
        last_gc_epoch: 1234,
        last_sweep: Some(crate::memory::GcSweepSummary {
            trigger: Some(crate::daemon::gc::GcTrigger::Elapsed),
            collected: 32,
            deferred: 7,
            collection_cap: 32,
            cap_bound: true,
            resources_spared_by_dependents: 5,
            survivors_deferred: 2_700,
        }),
    };
    assert_eq!(
        gc_stats_json(&g),
        json!({
            "last_gc_at": "2026-10-07T09:00:00+00:00",
            "last_gc_epoch": 1234,
            "last_sweep": {
                "trigger": "elapsed",
                "collected": 32,
                "deferred": 7,
                "collection_cap": 32,
                "cap_bound": true,
                "resources_spared_by_dependents": 5,
                "survivors_deferred": 2700,
            },
        })
    );
    assert_eq!(
        gc_summary_line(&g),
        "gc: last_gc_at=2026-10-07T09:00:00+00:00 last_gc_epoch=1234 last_sweep \
             trigger=elapsed collected=32 deferred=7 cap=32 cap_bound=true"
    );
    let none = crate::memory::GcStats {
        last_gc_at: None,
        last_gc_epoch: 0,
        last_sweep: None,
    };
    assert_eq!(
        gc_stats_json(&none),
        json!({"last_gc_at": null, "last_gc_epoch": 0, "last_sweep": null})
    );
    assert!(gc_summary_line(&none).contains("last_gc_at=never"));
}

/// P2-b: `lambo_stats` names the live promotion policy, in both halves of
/// its answer.
///
/// The failure this closes is not a bad config — it is a *correct* one an
/// operator cannot read back. `lambo.toml` saying `Solo` under a stale
/// exported `LAMBO_PROMOTION_POLICY=Swarm` runs Swarm, legitimately, and
/// before this key the only remaining evidence was days of absent
/// canonization events. Asserted on both arms, because a field hardcoded to
/// one policy would pass a single-arm test.
#[tokio::test]
async fn stats_reports_which_promotion_policy_is_live() {
    for policy in PromotionPolicy::ALL {
        let s = server_with_config(
            &format!("stats-policy-{}", policy.as_str().to_ascii_lowercase()),
            Config {
                promotion_policy: policy,
                ..Config::default()
            },
        )
        .await;
        let out = call(&s, "lambo_stats", json!({"agent_id": "agent-a"})).await;
        let payload = out.structured_content.as_ref().expect("stats payload");
        assert_eq!(
            payload["promotion_policy"].as_str(),
            Some(policy.as_str()),
            "the structured payload must name the live policy: {payload}"
        );
        let text = format!("{:?}", out.content);
        assert!(
            text.contains(&format!("promotion_policy={}", policy.as_str())),
            "the text summary must name the live policy too: {text}"
        );
        s.mem.close().await.expect("close");
    }
}

/// **Done-when: the queue bound comes from a ceiling measured on the
/// deployment's own embedder, and drops are counted in `lambo_stats`.**
#[tokio::test]
async fn the_stats_payload_reports_the_measured_bound_and_the_drop_count() {
    let s = server("mcp-j3-stats").await;
    // Force the probe to land before reading the payload.
    call(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "make the probe land", "concept_type": "logic"}],
        }),
    )
    .await;
    let out = call_raw(&s, "lambo_stats", json!({"agent_id": "agent-a"})).await;
    let p = out.structured_content.expect("payload");
    for key in [
        "write_queue_bound",
        "write_queue_lane_bound",
        "write_queue_measured",
        "write_queue_bound_source",
        "write_queue_items_per_sec",
        "write_queue_serial_items_per_sec",
        "write_queue_probe_serial_items_per_sec",
        "write_queue_outstanding",
        "write_queue_accepted",
        "write_queue_applied",
        "write_queue_failed",
        "write_queue_abandoned",
        "write_queue_dropped",
        "write_queue_dropped_closed",
        "write_queue_deferred",
        "write_queue_replayed",
        "write_queue_replay_owed",
        "receipts_retained",
    ] {
        assert!(p.get(key).is_some(), "{key} missing from {p}");
    }
    assert_eq!(
        p["write_queue_measured"],
        json!(true),
        "the fixture embedder IS a measurement of this deployment's embedder: {p}"
    );
    // One real write has been applied, which is under OBSERVED_MIN_SAMPLES,
    // so the probe's figure is still the one in force.
    assert_eq!(p["write_queue_bound_source"], json!("probe"), "{p}");
    // **And the bounds are the static fairness/memory caps, however fast
    // the probe read** (J3 redesign). The FixtureEmbedder is instant
    // (~98 000 items/s measured) and the bounds must not move for it —
    // rate-derived bounds are the retired estimator role whose five
    // falsified axes this workstream spent three rounds on.
    assert_eq!(
        p["write_queue_lane_bound"],
        json!(crate::writeq::WRITE_QUEUE_LANE_MAX),
        "{p}"
    );
    assert_eq!(
        p["write_queue_bound"],
        json!(crate::writeq::WRITE_QUEUE_MAX),
        "{p}"
    );
    assert!(
        p["write_queue_serial_items_per_sec"].as_f64().unwrap() > 0.0,
        "the serial leg is the load-bearing measurement: {p}"
    );
    // Both rates are published, and while the source is `probe` they are
    // the same number — the pair only diverges once observation takes over,
    // and that divergence is the diagnosis J3-R2-4 asked for.
    assert_eq!(
        p["write_queue_probe_serial_items_per_sec"], p["write_queue_serial_items_per_sec"],
        "{p}"
    );
    assert_eq!(p["write_queue_dropped_closed"], json!(0), "{p}");
    assert_eq!(p["write_queue_accepted"], json!(1), "{p}");
    assert_eq!(p["write_queue_applied"], json!(1), "{p}");
    assert_eq!(p["write_queue_dropped"], json!(0), "{p}");
    assert_eq!(p["write_queue_outstanding"], json!(0), "{p}");
    // The gauge must be re-derivable from the payload — I-R2-3's property.
    let derived = p["write_queue_accepted"].as_u64().unwrap()
        - p["write_queue_applied"].as_u64().unwrap()
        - p["write_queue_failed"].as_u64().unwrap();
    assert_eq!(
        derived,
        p["write_queue_outstanding"].as_u64().unwrap(),
        "{p}"
    );
    s.mem.close().await.expect("close");
}

/// A `lambo_stats` that WAITS must report the session **after** the wait.
///
/// Found by measuring the shipped binary rather than by a test: the payload
/// used to be snapshotted before the wait, so a call that blocked for a
/// write and then reported `write_queue_applied: 0` and `concept_count: 0`
/// — beside a receipt in the same payload saying `applied` — contradicted
/// itself. A payload that disagrees with itself is worse than a slow one.
#[tokio::test]
async fn a_waiting_stats_call_reports_the_session_after_the_wait() {
    let s = server("mcp-j3-stats-order").await;
    let ack = call_raw(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "counted after the wait", "concept_type": "logic"}],
        }),
    )
    .await;
    let receipt = ack.structured_content.as_ref().expect("payload")["receipt"]
        .as_str()
        .expect("receipt")
        .to_string();
    let out = call_raw(
        &s,
        "lambo_stats",
        json!({
            "agent_id": "agent-a",
            "receipt": receipt,
            "wait_ms": crate::writeq::RECEIPT_WAIT_MAX.as_millis() as u64,
        }),
    )
    .await;
    let p = out.structured_content.expect("payload");
    assert_eq!(p["receipt"]["state"], json!("applied"), "{p}");
    assert_eq!(
        p["write_queue_applied"],
        json!(1),
        "the counters must be read after the wait, not before it: {p}"
    );
    assert_eq!(p["write_queue_outstanding"], json!(0), "{p}");
    assert_eq!(
        p["concept_count"],
        json!(1),
        "the graph counts must be read after the wait too: {p}"
    );
    // The text block is built from the same snapshot, so it must agree.
    let text = match &out.content[0] {
        rmcp::model::ContentBlock::Text(t) => t.text.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    assert!(text.contains("concepts=1"), "{text}");
    s.mem.close().await.expect("close");
}

/// **N6 pinned.** A config default outside the MCP bound is clamped into
/// range rather than making the tool refuse a request that named nothing —
/// while an explicit out-of-range value from the client is still rejected.
#[test]
fn a_config_default_over_the_mcp_bound_is_clamped_not_fatal() {
    assert_eq!(
        clamp_cfg_default("default_top_k", MAX_TOP_K + 500, 1, MAX_TOP_K),
        MAX_TOP_K,
        "a config default above the cap is clamped to the cap"
    );
    assert_eq!(
        clamp_cfg_default("default_top_k", 0, 1, MAX_TOP_K),
        1,
        "a zero config default is clamped up to the floor"
    );
    assert_eq!(
        clamp_cfg_default("default_top_k", 7, 1, MAX_TOP_K),
        7,
        "an in-range config default is left untouched"
    );
}

/// **K2 acceptance.** The `lambo_stats` payload answers "how many of this
/// session's concepts actually carry a vector" — the counter that would
/// have made the 92/100 dogfood damage visible on day one. The default
/// server runs `MatchStrategy::Canonical`, where a derive deliberately does
/// NOT embed, so the honest reading here is 0/2: two concepts APPLIED, zero
/// EMBEDDED — precisely the applied-vs-embedded gap the field exists to
/// make visible (`lambo re-embed` is what closes it).
#[tokio::test]
async fn k2_stats_payload_reports_embedding_coverage() {
    let s = server("k2-coverage").await;
    let ack = call(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "concepts": [
                {"content": "user schema", "concept_type": "entity"},
                {"content": "auth middleware", "concept_type": "entity"}
            ]
        }),
    )
    .await;
    assert!(!ack.is_error.unwrap_or(false), "derive failed: {ack:?}");

    // `call` settles the J3 receipt, so the write IS applied.
    let stats = call(&s, "lambo_stats", json!({"agent_id": "agent-a"})).await;
    let payload = stats.structured_content.expect("stats payload");
    assert_eq!(
        payload["total_concepts"], payload["concept_count"],
        "the alias must track concept_count exactly: {payload}"
    );
    assert_eq!(payload["total_concepts"], 2, "{payload}");
    assert_eq!(
        payload["embedded_concepts"].as_u64(),
        Some(0),
        "canonical derives do not embed; the counter must SAY so, not hide it: {payload}"
    );
    s.mem.close().await.expect("close");
}

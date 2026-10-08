//! Condition detection and the event channel: conflict, drift, stale and
//! high-risk conditions, receivers, the ring and severity order.

use super::*;

#[cfg(feature = "fixtures")]
#[tokio::test(start_paused = true)]
async fn loop_emits_planted_conflict_from_rest_api_fixture() {
    use crate::fixtures;
    use crate::store::GraphStore;

    // Rebase the fixture onto wall clock so the caching layer's write
    // lands 5s before `anchor` — inside the loop's 30s conflict window.
    let anchor = Utc::now();
    let store =
        fixtures::load_store_relative("session-rest-api", anchor, Duration::from_secs(5)).unwrap();
    let snap = store
        .load_session(&SessionId::from("session-rest-api"))
        .await
        .unwrap();
    let g = Graph::from_snapshot(snap).unwrap();

    let daemon = Daemon::new(
        Arc::new(RwLock::new(g)),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    )
    // Pin detection time to `anchor` so the planted 5s-ago write renders
    // exactly "5s" — the wall clock would make this flaky on slow CI.
    .with_clock(Arc::new(move || anchor));
    let mut rx = daemon.events();
    let handle = daemon.spawn();

    // Warm-up cycle: the planted conflict is the only event in the
    // session (no root goal → no drift; all writes < 1h old → no stale;
    // no fresh high-value writes → no high-risk).
    let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("planted conflict within 2s")
        .unwrap();
    match evt {
        DaemonEvent::Conflict {
            node_id,
            agents,
            detail,
        } => {
            let caching = NodeId("f0000000-0000-4000-8000-000000001010".parse().unwrap());
            assert_eq!(
                node_id, caching,
                "the caching layer is the planted conflict"
            );
            assert_eq!(
                agents,
                vec![AgentId::from("agent-a"), AgentId::from("agent-b")]
            );
            assert!(
                detail.contains("agent-a") && detail.contains("agent-b") && detail.contains("5s"),
                "renderable detail: {detail}"
            );
        }
        other => panic!("first warm-up event must be the planted Conflict, got {other:?}"),
    }
    handle.abort();
}

#[cfg(feature = "fixtures")]
#[tokio::test(start_paused = true)]
async fn loop_emits_planted_drift_from_session_drift_fixture() {
    use crate::fixtures;
    use crate::store::GraphStore;

    let anchor = Utc::now();
    let store =
        fixtures::load_store_relative("session-drift", anchor, Duration::from_secs(5)).unwrap();
    let snap = store
        .load_session(&SessionId::from("session-drift"))
        .await
        .unwrap();
    let g = Graph::from_snapshot(snap).unwrap();
    let planted = g
        .concepts()
        .find(|c| c.content == "far budget concept")
        .expect("fixture must contain the planted drifted concept")
        .id;

    let daemon = Daemon::new(
        Arc::new(RwLock::new(g)),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    );
    let mut rx = daemon.events();
    let handle = daemon.spawn();

    // Warm-up: single-agent fixture → no conflict; drift is the only event
    // kind (drift detection is clock-free, the chain is 6 hops). The planted
    // node sorts first by id; the fixture's isolated pair follows with the
    // no-path warning spec §9 requires (ALGO-5) — they are GC's step-3 food,
    // and warning once before GC's interval collects them is correct.
    let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("planted drift within 2s")
        .unwrap();
    match evt {
        DaemonEvent::Drift {
            node_id,
            hops,
            detail,
        } => {
            assert_eq!(node_id, planted, "the planted drifted node");
            assert_eq!(hops, 6);
            assert!(detail.contains("6 hops"), "renderable detail: {detail}");
        }
        other => panic!("first warm-up event must be the planted Drift, got {other:?}"),
    }
    let mut no_path = 0;
    while let Ok(evt) = rx.try_recv() {
        match evt {
            DaemonEvent::Drift { hops, detail, .. } => {
                assert_eq!(
                    hops,
                    drift::DRIFT_HOPS_NO_PATH_EVENT,
                    "unreachable sentinel: {detail}"
                );
                assert!(detail.contains("no path"), "renderable detail: {detail}");
                no_path += 1;
            }
            other => panic!("the drift fixture must emit only Drift, got {other:?}"),
        }
    }
    assert_eq!(no_path, 2, "the fixture's isolated pair");
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn lagged_receiver_does_not_block_the_loop() {
    let (graph, c1_id, _, i2_id) = conflicted_graph();
    // Capacity 2 with a receiver that never drains: the daemon must stay
    // unblocked (scores keep advancing) and the consumer must see
    // `Lagged`, never a hang.
    let params = CycleParams {
        event_capacity: 2,
        ..Default::default()
    };
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
        params,
    );
    let mut rx = daemon.events();
    let handle = daemon.spawn();
    wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;

    // Each mutation cycle introduces a NEW contested node — an agent-b
    // concept with a fresh Dependency edge from the agent-a concept c1 —
    // so emit-on-transition publishes exactly one distinct event per
    // cycle (a persisting conflict is emitted once, on entry, never
    // re-emitted). ~10 distinct events through a capacity-2 channel.
    for n in 2..=10u64 {
        let c = concept_at(n, i2_id, "agent-b", &format!("extra {n}"), wall_ts(3));
        let cid = c.id;
        graph.write().insert_concept(c, i2_id).unwrap();
        graph
            .write()
            .upsert_edge(dep_edge_at(100 + n, c1_id, cid, wall_ts(3)))
            .unwrap();
        daemon.wake();
        wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;
    }

    // ~10 events through a capacity-2 channel → the loop never blocked
    // (proven above by scores advancing) and the consumer is lagged.
    match rx.recv().await {
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
        other => panic!("expected Lagged, got {other:?}"),
    }
    // Re-synced to the newest retained window: the tail is Conflicts.
    match rx.recv().await {
        Ok(DaemonEvent::Conflict { .. }) => {}
        other => panic!("expected a Conflict in the retained window, got {other:?}"),
    }
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn dropped_receiver_does_not_break_the_loop() {
    let (graph, c1_id, _, i2_id) = conflicted_graph();
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    );
    let rx = daemon.events();
    drop(rx); // zero receivers — every publish is discarded (spec §6.1)
    let handle = daemon.spawn();
    wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;

    for n in 2..=6u64 {
        let c = concept_at(n, i2_id, "agent-b", &format!("extra {n}"), wall_ts(3));
        graph.write().insert_concept(c, i2_id).unwrap();
        daemon.wake();
        wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;
    }

    // The loop survived every discarded send — and still maintained the
    // hot list (the detection side is independent of the consumer side).
    assert!(
        daemon.hot_list().read().contains(c1_id),
        "conflict entry must be on the hot list despite zero receivers"
    );
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn stale_fires_for_idle_session_after_window_elapses() {
    // T4.6 finding-1 regression: detection must run on EVERY tick, not
    // only on epoch change. A session with NO mutations ages a concept
    // past the stale window; pure time passing must fire
    // `DaemonEvent::Stale` (spec §9 background-daemon semantics, §6.1
    // Stale).
    use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};

    let t0 = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let mut g = Graph::new(sid());
    let i1 = interaction_at(1, None, "agent-a", t0);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    let c = concept_at(1, iid, "agent-a", "idle concept", t0);
    let cid = c.id;
    g.insert_concept(c, iid).unwrap();
    let epoch0 = g.epoch();
    let graph = Arc::new(RwLock::new(g));

    // Controllable clock: the loop reads `now` from this cell; the test
    // advances it. Stale window shortened to 60s; the tick is short
    // because an idle session has no wake source — time alone must
    // drive the loop.
    let now_secs = Arc::new(AtomicI64::new(1_700_000_000));
    let clock: Clock = {
        let now_secs = now_secs.clone();
        Arc::new(move || {
            Utc.timestamp_opt(now_secs.load(AtomicOrdering::SeqCst), 0)
                .unwrap()
        })
    };
    let params = CycleParams {
        stale_window: Duration::from_secs(60),
        ..Default::default()
    };
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_millis(10),
        params,
    )
    .with_clock(clock);
    let mut rx = daemon.events();
    let handle = daemon.spawn();

    // Warm-up at t0: the concept's activity is fresh — and no other
    // detector fires — so nothing is published.
    wait_until(|| daemon.scores().epoch == epoch0).await;
    match rx.try_recv() {
        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
        other => panic!("no event before the window elapses, got {other:?}"),
    }

    // No mutation — only time passes. The next tick ages the concept
    // past the window: Stale ENTERS the detected set and is emitted once.
    now_secs.fetch_add(61, AtomicOrdering::SeqCst);
    let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("idle staleness within 2s")
        .unwrap();
    match evt {
        DaemonEvent::Stale { node_id, detail } => {
            assert_eq!(node_id, cid);
            assert!(detail.contains("61s"), "renderable detail: {detail}");
        }
        other => panic!("expected Stale for the idle concept, got {other:?}"),
    }
    // The hot list mirrors the fresh hit.
    assert!(
        daemon.hot_list().read().contains(cid),
        "stale node must be on the hot list"
    );

    // Time keeps passing but the condition persists: no re-emission.
    now_secs.fetch_add(3600, AtomicOrdering::SeqCst);
    wake_and_settle(&daemon).await;
    match rx.try_recv() {
        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
        other => panic!("persisting stale must not re-emit, got {other:?}"),
    }
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn loop_emits_high_risk_for_fresh_write_to_canonical_node() {
    // Final-review finding 1a: the entered-gated HighRisk emit path (the
    // `high_risk_hits` loop in `run_loop`) + `events::high_risk_event`
    // mapper had no loop-level test. A Canonical node with a fresh
    // in-window write is a high-risk modification (spec §9 "high-risk
    // modification" hot-list condition; events.rs v0.1 rule): exactly one
    // `DaemonEvent::HighRisk` on transition, a HighRiskModification
    // hot-list entry with a renderable reason, and NO re-emit while the
    // condition persists (emit-on-transition, finding 3).
    use crate::daemon::hotlist::{Condition, HotListPayload};

    let t0 = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let mut g = Graph::new(sid());
    let i1 = interaction_at(1, None, "agent-a", t0);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    // The high-value node: Canonical (spec §10 Stage 3), fresh at t0 —
    // its Derives edge from `i1` is dated t0 too (graph.rs
    // insert_concept dates the edge at the concept's created_at).
    let c1 = Concept {
        canonization_status: CanonizationStatus::Canonical,
        blast_radius: Some(8),
        ..concept_at(1, iid, "agent-a", "canonical concept", t0)
    };
    let c1_id = c1.id;
    g.insert_concept(c1, iid).unwrap();
    // The modifying writer: a fresh in-window Dependency edge onto c1.
    let c2 = concept_at(2, iid, "agent-a", "modifier", t0);
    let c2_id = c2.id;
    g.insert_concept(c2, iid).unwrap();
    g.upsert_edge(dep_edge_at(1, c2_id, c1_id, t0)).unwrap();

    let graph = Arc::new(RwLock::new(g));
    // Clock pinned to t0: the t0 writes stay inside the 30s high-risk
    // window for the test's whole lifetime — the condition persists.
    let clock: Clock = Arc::new(move || t0);
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    )
    .with_clock(clock);
    let mut rx = daemon.events();
    let handle = daemon.spawn();

    // Warm-up: the only transition is (HighRiskModification, c1) —
    // single agent (no conflict), no root goal (no drift), all activity
    // fresh (no stale).
    let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("high-risk event within 2s")
        .unwrap();
    match evt {
        DaemonEvent::HighRisk { node_id, detail } => {
            assert_eq!(node_id, c1_id, "the fresh-written canonical node");
            assert!(
                detail.contains("Canonical") && detail.contains("modified within 30s"),
                "renderable detail: {detail}"
            );
        }
        other => panic!("first warm-up event must be the HighRisk, got {other:?}"),
    }

    // The hot list mirrors the fresh hit: a HighRiskModification entry
    // with the renderable reason.
    {
        let h = daemon.hot_list();
        let guard = h.read();
        let entry = guard
            .iter()
            .find(|e| e.node == c1_id)
            .expect("warm-up must put the canonical node on the hot list");
        assert_eq!(entry.condition, Condition::HighRiskModification);
        match &entry.payload {
            HotListPayload::HighRisk { reason } => {
                assert!(reason.contains("Canonical"), "renderable reason: {reason}")
            }
            other => panic!("expected HighRisk payload, got {other:?}"),
        }
    }

    // The condition persists (the t0 write never leaves the 30s window):
    // wakes re-run detection but emit-on-transition publishes nothing —
    // the event fired once, on entry.
    for _ in 0..3 {
        wake_and_settle(&daemon).await;
    }
    match rx.try_recv() {
        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
        other => panic!("persisting high-risk must not re-emit, got {other:?}"),
    }
    handle.abort();
}

/// Parse the seconds out of a `stale_event` detail
/// ("... untouched for <N>s").
///
/// Gated with its only caller (NEW-1): CI's feature matrix runs
/// `--no-default-features --features store-sqlite|store-cockroach` under
/// `RUSTFLAGS="-D warnings"`, where an ungated helper whose sole use sits
/// behind `fixtures` is a hard dead-code error, not a warning.
#[cfg(feature = "fixtures")]
fn stale_seconds(detail: &str) -> u64 {
    detail
        .trim_end_matches('s')
        .rsplit(' ')
        .next()
        .unwrap_or_default()
        .parse()
        .unwrap_or_else(|_| panic!("detail must render seconds-inactive: {detail:?}"))
}

#[cfg(feature = "fixtures")]
#[tokio::test(start_paused = true)]
async fn loop_emits_one_session_stale_from_rest_api_fixture_after_writes_age_out() {
    // Final-review finding 1a: staleness had only a synthetic-clock loop
    // test. This one is fixture-driven: rebase session-rest-api so its
    // newest write lands 2h before `anchor` — past the 1h STALE_WINDOW
    // and far outside the 30s conflict/high-risk windows. The session as a
    // whole is stale, so the warm-up cycle must emit exactly ONE Stale
    // (CONC-2: per session, not per concept — this asserted 22 before) and
    // nothing else: no Conflict (all writes are 2h old, outside
    // conflict_recency_window), no Drift (the session has no root goal —
    // drift.rs: no goal nodes → no hits), no HighRisk (no fresh in-window
    // writes — user schema is Canonical/blast-radius 8 but its write is 2h
    // old).
    use crate::fixtures;
    use crate::store::GraphStore;

    let anchor = Utc::now();
    let store =
        fixtures::load_store_relative("session-rest-api", anchor, Duration::from_secs(7200))
            .unwrap();
    let snap = store
        .load_session(&SessionId::from("session-rest-api"))
        .await
        .unwrap();
    let g = Graph::from_snapshot(snap).unwrap();
    assert_eq!(
        g.concepts().count(),
        22,
        "fixture must keep its 22 concepts"
    );

    let daemon = Daemon::new(
        Arc::new(RwLock::new(g)),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    )
    // Pin detection time to `anchor` so the 2h-rebased writes render
    // stable seconds-inactive (the wall clock would make this flaky).
    .with_clock(Arc::new(move || anchor));
    let mut rx = daemon.events();
    let handle = daemon.spawn();

    let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("session stale within 2s")
        .unwrap();
    let anchor_node = match evt {
        DaemonEvent::Stale { node_id, detail } => {
            assert!(
                detail.contains("untouched for"),
                "renderable detail: {detail}"
            );
            assert!(
                stale_seconds(&detail) >= 7200,
                "2h rebase ⇒ the newest write is ≥ 2h old, got {detail}"
            );
            node_id
        }
        other => panic!("aged-out fixture must emit one session Stale, got {other:?}"),
    };
    // One event for the whole session — 22 concepts, one Stale.
    match rx.try_recv() {
        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
        other => panic!("staleness is per session: expected exactly one, got {other:?}"),
    }
    // The hot list mirrors it: one entry, on the anchor node.
    {
        let h = daemon.hot_list();
        let guard = h.read();
        assert_eq!(guard.len(), 1, "one hot-list entry for the stale session");
        assert!(guard.contains(anchor_node));
    }
    handle.abort();
}

// ------------------------------------------------------------------
// XP-4 / CONC-2 / CONC-3 — the event seam
// ------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn non_daemon_caller_can_emit_canonized_on_the_daemon_channel() {
    // XP-4: P6's documented seam (`events::emit_canonized`) needs the
    // broadcast Sender. Before `Daemon::event_sender()` no public path to it
    // existed anywhere in the crate, so P6 could not reach the channel
    // without owning the daemon's private field. This test is the seam: a
    // caller that is *not* the daemon loop emits, and a `Daemon::events()`
    // subscriber receives.
    use crate::types::CanonizationEvent;

    let (graph, cid) = locked_graph_with_one_concept();
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    );
    let mut rx = daemon.events();

    // The "P6 evaluator": holds only the sender, never the daemon.
    let sender = daemon.event_sender();
    let event = CanonizationEvent {
        id: NodeId(Uuid::from_u64_pair(9, 1)),
        session_id: sid(),
        node_id: cid,
        from_status: CanonizationStatus::None,
        to_status: CanonizationStatus::Candidate,
        blast_radius: Some(3),
        occurred_at: ts(0),
        last_demotion_time: None,
    };
    assert_eq!(sender.emitted_total(), 0, "nothing published yet");
    events::emit_canonized(&sender, event.clone());

    let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("Canonized within 2s")
        .unwrap();
    match got {
        DaemonEvent::Canonized { event: e } => assert_eq!(e, event),
        other => panic!("expected Canonized, got {other:?}"),
    }
    // NEW-3: the seam is an `events::EventSender`, not a raw broadcast
    // Sender — P6's send advanced the channel's SHARED publication counter,
    // which is what the loop's re-arm measures ring eviction against. A
    // clone taken later reads the same count.
    assert_eq!(sender.emitted_total(), 1);
    assert_eq!(daemon.event_sender().emitted_total(), 1);
    // The daemon's own handle keeps the channel open after the clone drops.
    drop(sender);
    assert_eq!(daemon.event_sender().receiver_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn external_publisher_flood_cannot_permanently_evict_a_held_conflict() {
    // NEW-3: `event_sender()` used to hand out a raw `broadcast::Sender`
    // clone. Its sends advanced the ring but not the loop's emission count,
    // so re-arm (CONC-2) could not see that the held Conflict's event had
    // been pushed out — leaving CONC-2 only partially closed. Probe on the
    // pre-fix code: 300 external `Canonized` sends + a continuously-held
    // Conflict + 601 daemon cycles delivered **0** Conflict events.
    //
    // Capacity 4, flood 10 (> capacity, so the warm-up Conflict is certainly
    // gone from the retained window), and the subscriber drains LATE — only
    // after the flood — so the only way it can ever see the Conflict is the
    // re-arm path counting the external sends.
    use crate::types::CanonizationEvent;

    let (graph, c1_id, _, _) = conflicted_graph();
    let params = CycleParams {
        event_capacity: 4,
        ..Default::default()
    };
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
        params,
    );
    let mut rx = daemon.events();
    let handle = daemon.spawn();

    // Warm-up publishes the Conflict (stamp 1). Wait on the hot list, not on
    // `rx` — draining would defeat the point.
    wait_until(|| daemon.hot_list().read().contains(c1_id)).await;

    // The "P6 evaluator" floods the channel it shares with the daemon.
    let sender = daemon.event_sender();
    for n in 0..10u64 {
        events::emit_canonized(
            &sender,
            CanonizationEvent {
                id: NodeId(Uuid::from_u64_pair(9, n)),
                session_id: sid(),
                node_id: c1_id,
                from_status: CanonizationStatus::None,
                to_status: CanonizationStatus::Candidate,
                blast_radius: Some(3),
                occurred_at: ts(0),
                last_demotion_time: None,
            },
        );
    }
    assert!(
        sender.emitted_total() >= 11,
        "the warm-up Conflict plus 10 external sends must all be counted, \
             got {}",
        sender.emitted_total()
    );

    // Cycles with no graph change: nothing ENTERS the condition set, so the
    // only possible publication is the re-arm of the still-held Conflict.
    let mut saw_conflict = false;
    for _ in 0..8 {
        wake_and_settle(&daemon).await;
        while let Ok(evt) = rx.try_recv() {
            if let DaemonEvent::Conflict { node_id, .. } = evt
                && node_id == c1_id
            {
                saw_conflict = true;
            }
        }
        if saw_conflict {
            break;
        }
    }
    assert!(
        saw_conflict,
        "an external publisher's flood must not permanently evict a held \
             Conflict — every publisher counts, so re-arm still fires"
    );
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn late_subscriber_misses_the_warm_up_condition_set() {
    // CONC-3: pins the documented P8 ordering obligation. The warm-up cycle
    // publishes the whole restored condition set — including the demo's
    // planted Conflict — and `broadcast` delivers only what is sent after a
    // receiver exists. Emission is on transition, so nothing re-publishes
    // for a late subscriber's benefit. P8 must subscribe BEFORE spawn; see
    // `Daemon::events`.
    let (graph, c1_id, _, _) = conflicted_graph();
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    );
    let handle = daemon.spawn();
    // The warm-up cycle has run once the hot list carries its conflict.
    wait_until(|| daemon.hot_list().read().contains(c1_id)).await;

    // Subscribe *after* the warm-up, then drive several more cycles: the
    // condition still holds, so no transition fires and nothing arrives.
    let mut late = daemon.events();
    for _ in 0..3 {
        wake_and_settle(&daemon).await;
    }
    match late.try_recv() {
        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
        other => panic!("a late subscriber must not see the warm-up set, got {other:?}"),
    }
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn held_condition_is_re_emitted_once_the_ring_wraps_past_it() {
    // CONC-2: emit-on-transition alone loses an event permanently — the
    // transition is recorded whether or not any consumer got it, so an
    // event evicted from the ring while its condition still holds is never
    // re-published. The demo's Conflict is exactly such an event.
    //
    // Capacity 2, and the consumer drains fully after every cycle so no
    // event is ever lost to lag — the only way c1's Conflict can reappear
    // is the re-arm path. Two later events push its emission out of the
    // 2-slot retained window; the next cycle must re-publish it.
    let (graph, c1_id, _, i2_id) = conflicted_graph();
    let params = CycleParams {
        event_capacity: 2,
        ..Default::default()
    };
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
        params,
    );
    let mut rx = daemon.events();
    let handle = daemon.spawn();

    /// Everything currently queued, as `(is_conflict_on_target, node)`.
    fn drain(rx: &mut broadcast::Receiver<DaemonEvent>) -> Vec<NodeId> {
        let mut out = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            if let DaemonEvent::Conflict { node_id, .. } = evt {
                out.push(node_id);
            }
        }
        out
    }

    // Warm-up: the c1 conflict, emitted on entry.
    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("warm-up conflict within 2s")
        .unwrap();
    assert!(matches!(
        first,
        DaemonEvent::Conflict { node_id, .. } if node_id == c1_id
    ));
    assert!(drain(&mut rx).is_empty(), "warm-up emits exactly one event");

    // Two more cycles, each introducing one new contested node: one
    // entering event per cycle, drained immediately. After the second, two
    // events have been published since c1's, so its slot is gone.
    let mut seen_c1_again = false;
    for n in 2..=3u64 {
        let c = concept_at(n, i2_id, "agent-b", &format!("extra {n}"), wall_ts(3));
        let cid = c.id;
        graph.write().insert_concept(c, i2_id).unwrap();
        graph
            .write()
            .upsert_edge(dep_edge_at(100 + n, c1_id, cid, wall_ts(3)))
            .unwrap();
        wake_and_settle(&daemon).await;
        if drain(&mut rx).contains(&c1_id) {
            seen_c1_again = true;
        }
    }

    assert!(
        seen_c1_again,
        "a still-held Conflict whose event left the retained window must be \
             re-published (CONC-2 re-arm), not lost forever"
    );
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn entering_conditions_publish_highest_severity_first() {
    // CONC-2: a burst must put the most actionable event first for a
    // consumer draining in order — Conflict, HighRisk, Drift, Stale
    // (`Condition::severity`). Pre-fix the order was Conflict, Drift,
    // Stale, HighRisk: the hazard came last.
    //
    // The graph plants a Conflict and a HighRisk that enter together: a
    // Canonical, high-blast-radius node written by a second agent. The two
    // interactions carry distinct timestamps so the edge attributes cleanly
    // to agent-b (ALGO-3) and only `contested` is contested.
    let t0 = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let t1 = t0 + chrono::Duration::seconds(5);
    let now = t0 + chrono::Duration::seconds(10);
    let mut g = Graph::new(sid());
    let i1 = interaction_at(1, None, "agent-a", t0);
    let i1_id = i1.id;
    g.insert_interaction(i1).unwrap();
    let i2 = interaction_at(2, Some(1), "agent-b", t1);
    let i2_id = i2.id;
    g.insert_interaction(i2).unwrap();
    let contested = Concept {
        canonization_status: CanonizationStatus::Canonical,
        blast_radius: Some(8),
        ..concept_at(1, i1_id, "agent-a", "user schema", t0)
    };
    let contested_id = contested.id;
    g.insert_concept(contested, i1_id).unwrap();
    let writer = concept_at(2, i2_id, "agent-b", "cache layer", t1);
    let writer_id = writer.id;
    g.insert_concept(writer, i2_id).unwrap();
    g.upsert_edge(dep_edge_at(1, writer_id, contested_id, t1))
        .unwrap();

    let daemon = Daemon::new(
        Arc::new(RwLock::new(g)),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    )
    .with_clock(Arc::new(move || now));
    let mut rx = daemon.events();
    let handle = daemon.spawn();

    // Both conditions enter on the warm-up cycle; Conflict must be first.
    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("warm-up burst within 2s")
        .unwrap();
    assert!(
        matches!(first, DaemonEvent::Conflict { node_id, .. } if node_id == contested_id),
        "Conflict outranks HighRisk in the burst, got {first:?}"
    );
    let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("second event within 2s")
        .unwrap();
    assert!(
        matches!(second, DaemonEvent::HighRisk { node_id, .. } if node_id == contested_id),
        "HighRisk follows Conflict, got {second:?}"
    );
    handle.abort();
}

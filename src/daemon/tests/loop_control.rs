//! The loop itself: wake and rescore, cycles, abort, panics and config.

use super::*;

#[tokio::test(start_paused = true)]
async fn epoch_change_triggers_rescore_via_wake() {
    // Tick of 1h so only the explicit wake drives cycles.
    let (graph, cid) = locked_graph_with_one_concept();
    let epoch0 = graph.read().epoch();
    assert_eq!(epoch0, 3);

    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    );
    let handle = daemon.spawn();

    // Warm-up rescore on the first cycle.
    wait_until(|| daemon.scores().epoch == epoch0).await;
    let warm = daemon.scores();
    assert_eq!(warm.ranked.len(), 1);
    assert_eq!(warm.ranked[0].item, cid);

    // Mutate the graph: add a second concept → epoch bumps.
    let c2 = {
        let iid = match graph.read().node(cid).unwrap() {
            crate::types::Node::Concept(c) => c.origin_interaction,
            _ => unreachable!(),
        };
        let c = concept(2, iid, "auth middleware");
        let id = c.id;
        graph.write().insert_concept(c, iid).unwrap();
        id
    };
    let epoch1 = graph.read().epoch();
    assert!(epoch1 > epoch0);

    // Explicit wake must trigger a rescore without waiting for the tick.
    daemon.wake();
    wait_until(|| daemon.scores().epoch == epoch1).await;
    let after = daemon.scores();
    assert_eq!(after.epoch, epoch1);
    assert_eq!(after.ranked.len(), 2);
    assert!(after.ranked.iter().any(|s| s.item == c2));

    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn no_epoch_change_does_not_rescore() {
    let (graph, _) = locked_graph_with_one_concept();
    let epoch0 = graph.read().epoch();

    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    );
    let handle = daemon.spawn();
    wait_until(|| daemon.scores().epoch == epoch0).await;
    let before = daemon.scores();

    // Wake with no mutation: only the RESCORE is epoch-gated (finding 1)
    // — detection still runs, but with no condition transitions nothing
    // is published and the score table stays byte-identical.
    wake_and_settle(&daemon).await;
    let after = daemon.scores();
    assert_eq!(before, after, "no epoch change must not rescore");

    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn cycle_completes_without_deadlock() {
    // Lock-discipline smoke: a cycle that takes the read lock, rescorees,
    // releases, then awaits must complete — never hold the lock across
    // .await (a violation would deadlock the write side below).
    let (graph, cid) = locked_graph_with_one_concept();
    let epoch0 = graph.read().epoch();

    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_millis(10),
    );
    let handle = daemon.spawn();
    wait_until(|| daemon.scores().epoch == epoch0).await;

    // Writer: grab the write lock, mutate, release — repeatedly while the
    // daemon runs. If the daemon held the read lock across .await, the
    // writer would starve and the timeout below would fire.
    let iid = match graph.read().node(cid).unwrap() {
        crate::types::Node::Concept(c) => c.origin_interaction,
        _ => unreachable!(),
    };
    for n in 2..=10u64 {
        let c = concept(n, iid, &format!("concept {n}"));
        graph.write().insert_concept(c, iid).unwrap();
        daemon.wake();
        wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;
    }
    let final_table = daemon.scores();
    assert_eq!(final_table.ranked.len(), 10);
    for w in final_table.ranked.windows(2) {
        assert!(w[0].score >= w[1].score, "ranked list must stay sorted");
    }

    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn abort_stops_the_loop() {
    let (graph, _) = locked_graph_with_one_concept();
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_millis(10),
    );
    let handle = daemon.spawn();
    wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;
    handle.abort();
    // Abort is our stop mechanism at this stage (graceful stop = P8).
    assert!(handle.await.is_err(), "aborted task must not complete Ok");
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "spawn called twice")]
async fn spawn_twice_panics() {
    let (graph, _) = locked_graph_with_one_concept();
    let daemon = Daemon::new(graph, ScoringWeights::default(), Duration::from_secs(3600));
    let first = daemon.spawn();
    std::mem::drop(first);
    // Second spawn must panic (single-loop guard), before any future exists.
    daemon.spawn();
}

#[tokio::test(start_paused = true)]
async fn loop_maintains_hot_list_from_fresh_hits() {
    use crate::daemon::hotlist::{Condition, HotListPayload};

    let (graph, c1_id, dep_eid, _) = conflicted_graph();
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    );
    let handle = daemon.spawn();
    wait_until(|| !daemon.hot_list().read().is_empty()).await;

    {
        let h = daemon.hot_list();
        let guard = h.read();
        let entry = guard
            .iter()
            .find(|e| e.node == c1_id)
            .expect("warm-up must put the conflicted node on the hot list");
        assert_eq!(entry.condition, Condition::Conflict);
        match &entry.payload {
            HotListPayload::Conflict { agents, .. } => assert_eq!(agents.len(), 2),
            other => panic!("expected Conflict payload, got {other:?}"),
        }
    }

    // Resolve the conflict (drop agent-b's fresh edge) → `(Conflict,
    // c1)` leaves the detected set → the next cycle's fresh-set sync
    // (retain_conditions) evicts the entry — no predicate involved.
    graph.write().remove_edge(dep_eid).unwrap();
    daemon.wake();
    wait_until(|| !daemon.hot_list().read().contains(c1_id)).await;
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn a_panicking_cycle_does_not_kill_the_loop() {
    // CONC-4: a panic inside the cycle used to kill the task silently —
    // scoring, events and GC stopped for the whole process lifetime with no
    // signal. The cycle body is contained, so the loop survives and the next
    // tick works normally.
    //
    // The panic is injected through the one seam the loop calls on every
    // cycle: the clock.
    use std::sync::atomic::AtomicUsize;

    let (graph, _) = locked_graph_with_one_concept();
    let calls = Arc::new(AtomicUsize::new(0));
    let clock: Clock = {
        let calls = calls.clone();
        Arc::new(move || {
            // Panic on the 1st cycle only; every later cycle is normal.
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("injected daemon cycle panic");
            }
            Utc::now()
        })
    };
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_millis(10),
    )
    .with_clock(clock);
    let handle = daemon.spawn();

    // The loop must reach a later cycle and publish a score table — proof
    // it survived the panic rather than dying with the task.
    wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;
    assert!(
        calls.load(Ordering::SeqCst) > 1,
        "the loop must run cycles after the panicking one"
    );
    assert!(!handle.is_finished(), "the task must still be alive");
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn a_failed_rescore_is_retried_on_the_next_cycle() {
    // #79 review L1: the epoch was marked done BEFORE the rescore, so a
    // rescore that panicked (contained by CONC-4) never ran again until the
    // next write. In an idle session the table stayed stale, and every recall
    // that would show a fresh concept missing from it stayed cold.
    let _quiet = crate::test_util::quiet_logs();
    let (graph, cid) = locked_graph_with_one_concept();
    let epoch = graph.read().epoch();
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    );
    daemon.fail_next_rescores(1);
    let handle = daemon.spawn();
    // The first cycle's rescore panics: nothing is published, but the rest
    // of the cycle still runs (#79 review F1).
    wait_until(|| daemon.cycles() == 1).await;
    assert_eq!(daemon.rescore_faults.load(Ordering::Acquire), 0);
    assert!(daemon.scores().ranked.is_empty());

    // A wake with NO mutation must still retry the failed rescore.
    wake_and_settle(&daemon).await;
    let table = daemon.scores();
    assert_eq!(table.epoch, epoch, "the retried rescore published");
    assert!(table.ranked.iter().any(|s| s.item == cid));
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn a_rescore_that_always_panics_does_not_starve_detection_or_gc() {
    // #79 review F1: the rescore ran inside the cycle-wide catch_unwind, so
    // a rescore that panicked on every attempt unwound past detection, the
    // hot list and GC on every cycle, for as long as the epoch stood still.
    use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};

    let _quiet = crate::test_util::quiet_logs();
    let t0 = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let mut g = Graph::new(sid());
    let i1 = interaction_at(1, None, "agent-a", t0);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    // `kept` ages into Stale (detection, hot list, event); `orphan` loses
    // its only Derives edge, so GC collects it.
    let kept = concept_at(1, iid, "agent-a", "idle concept", t0);
    let kept_id = kept.id;
    g.insert_concept(kept, iid).unwrap();
    let orphan = concept_at(2, iid, "agent-a", "orphaned concept", t0);
    let orphan_id = orphan.id;
    g.insert_concept(orphan, iid).unwrap();
    let derives = g
        .edge_between(iid, orphan_id, EdgeType::Derives)
        .unwrap()
        .id;
    g.remove_edge(derives).unwrap();
    let graph = Arc::new(RwLock::new(g));

    // The clock is already past the stale window on the first cycle.
    let now_secs = Arc::new(AtomicI64::new(1_700_000_061));
    let clock: Clock = {
        let now_secs = now_secs.clone();
        Arc::new(move || {
            Utc.timestamp_opt(now_secs.load(AtomicOrdering::SeqCst), 0)
                .unwrap()
        })
    };
    let params = CycleParams {
        stale_window: Duration::from_secs(60),
        gc_interval: 3,
        ..Default::default()
    };
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
        params,
    )
    .with_clock(clock);
    daemon.fail_next_rescores(usize::MAX);
    let mut rx = daemon.events();
    let handle = daemon.spawn();

    // Every rescore panics, yet the cycle completes: detection published
    // Stale and the hot list holds it, and GC swept the orphan.
    wait_until(|| daemon.cycles() >= 1).await;
    let mut stale = false;
    while let Ok(evt) = rx.try_recv() {
        if let DaemonEvent::Stale { node_id, .. } = evt
            && node_id == kept_id
        {
            stale = true;
        }
    }
    assert!(stale, "detection published Stale despite the rescore panic");
    assert!(daemon.hot_list().read().contains(kept_id));
    assert!(
        daemon.last_gc().is_some(),
        "GC ran despite the rescore panic"
    );
    assert!(graph.read().node(orphan_id).is_none());
    assert!(daemon.scores().ranked.is_empty(), "no table was published");

    // Let GC's own writes settle so the epoch stands still while the
    // rescore keeps failing.
    let mut epoch = graph.read().epoch();
    for _ in 0..20 {
        wake_and_settle(&daemon).await;
        let now = graph.read().epoch();
        if now == epoch {
            break;
        }
        epoch = now;
    }
    wake_and_settle(&daemon).await;
    assert_eq!(graph.read().epoch(), epoch, "the epoch settled");
    assert!(daemon.scores().ranked.is_empty());
    assert!(
        daemon.rescore_faults.load(Ordering::Acquire) > 0,
        "the rescore was still failing"
    );

    // The failed epoch was never marked: a healthy rescore on a wake with
    // no mutation publishes it.
    daemon.fail_next_rescores(0);
    wake_and_settle(&daemon).await;
    let table = daemon.scores();
    assert_eq!(table.epoch, epoch, "the healthy rescore published");
    assert!(table.ranked.iter().any(|s| s.item == kept_id));
    handle.abort();
}

#[test]
fn cycle_params_come_from_config_with_no_duplicated_defaults() {
    // XP-7: `CycleParams::default()` duplicated Config's spec constants as
    // literals, so the two could drift silently. It is now derived.
    use crate::config::Config;

    assert_eq!(
        CycleParams::default(),
        CycleParams::from(&Config::default())
    );
    assert_eq!(Config::default().daemon_tick_interval, DAEMON_TICK_INTERVAL);

    // A non-default Config must reach the loop's params — including
    // `drift_threshold`, whose u32/usize split forced a cast at every use.
    let config = Config {
        hot_list_max: 7,
        conflict_recency_window: Duration::from_secs(11),
        drift_threshold: 9,
        gc_interval: 13,
        max_canonical_nodes: 17,
        daemon_tick_interval: Duration::from_millis(250),
        ..Default::default()
    };
    let params = CycleParams::from(&config);
    assert_eq!(params.hot_list_max, 7);
    assert_eq!(params.conflict_window, Duration::from_secs(11));
    assert_eq!(params.drift_threshold, 9);
    assert_eq!(params.gc_interval, 13);
    assert_eq!(params.max_canonical_nodes, 17);

    let (graph, _) = locked_graph_with_one_concept();
    let daemon = Daemon::from_config(graph, &config);
    assert_eq!(daemon.tick, Duration::from_millis(250));
    assert_eq!(daemon.params, params);
    assert_eq!(daemon.hot_list().read().max(), 7);
}

#[tokio::test(start_paused = true)]
async fn wake_without_mutation_publishes_nothing() {
    let (graph, _, _, _) = conflicted_graph();
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    );
    let mut rx = daemon.events();
    let handle = daemon.spawn();

    // Warm-up publishes exactly the one Conflict (consumed below).
    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(first, DaemonEvent::Conflict { .. }));

    // A wake with no mutation: detection runs but the condition set is
    // unchanged → no transitions → nothing published (emit-on-transition).
    wake_and_settle(&daemon).await;
    match rx.try_recv() {
        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
        other => panic!("wake without a mutation must not publish, got {other:?}"),
    }
    handle.abort();
}

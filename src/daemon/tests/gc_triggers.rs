//! GC scheduling: mutation and timed triggers, restart accounting,
//! survivor bumps and the published outcome.

use super::*;

#[tokio::test(start_paused = true)]
async fn loop_runs_gc_every_gc_interval_mutations() {
    let (graph, cid) = locked_graph_with_one_concept();
    // Orphan the concept (drop its only Derives edge) so GC step 2
    // collects it — a deterministic GC-able node.
    let iid = match graph.read().node(cid).unwrap() {
        crate::types::Node::Concept(c) => c.origin_interaction,
        _ => unreachable!(),
    };
    let derives = {
        let g = graph.read();
        g.edge_between(iid, cid, EdgeType::Derives).unwrap().id
    };
    graph.write().remove_edge(derives).unwrap();

    // Interval 3: the warm-up epoch (4 mutations) already crosses it.
    let params = CycleParams {
        gc_interval: 3,
        ..Default::default()
    };
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
        params,
    );
    let handle = daemon.spawn();

    wait_until(|| graph.read().node(cid).is_none()).await;
    handle.abort();
}

/// Issue #30: the cycle applies noted read accesses, and they do not
/// advance the GC mutation trigger. With `gc_interval` one mutation past
/// the warm-up epoch, any number of applied accesses leaves GC unswept and
/// the epoch (hence the score table and the recall-cache key) untouched;
/// the next real write then fires the sweep exactly as before.
#[tokio::test(start_paused = true)]
async fn applied_accesses_do_not_advance_the_gc_trigger() {
    let (graph, cid) = locked_graph_with_one_concept();
    let epoch0 = graph.read().epoch();
    assert_eq!(epoch0, 3);
    let params = CycleParams {
        gc_interval: epoch0 + 1,
        ..Default::default()
    };
    let ledger = Arc::new(access::AccessLedger::new());
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
        params,
    )
    .with_access_ledger(ledger.clone());
    let handle = daemon.spawn();
    wait_until(|| daemon.cycles() >= 1).await;

    for round in 0..5 {
        for _ in 0..20 {
            ledger.record([cid], ts(round));
        }
        wake_and_settle(&daemon).await;
        assert_eq!(ledger.pending(), 0, "the cycle applied the ledger");
    }
    let (count, last) = match graph.read().node(cid) {
        Some(crate::types::Node::Concept(c)) => (c.access_count, c.last_accessed),
        _ => unreachable!(),
    };
    assert_eq!(count, 100);
    assert_eq!(last, Some(ts(4)));
    assert_eq!(
        graph.read().epoch(),
        epoch0,
        "accesses never bump the epoch"
    );
    assert_eq!(daemon.scores().epoch, epoch0);
    assert!(
        daemon.last_gc().is_none(),
        "100 accesses must not fund a sweep that needs one mutation"
    );
    // Five applies → nothing in the write-behind log, one access-dirty
    // concept for the flush to take (issue #30: reads never grow the log).
    assert_eq!(graph.read().log_len(), 3);
    assert_eq!(graph.read().pending_accesses(), 1);

    // One real mutation crosses the interval: the trigger is intact.
    let mut i2 = interaction(2);
    i2.previous_id = Some(interaction(1).id);
    graph.write().insert_interaction(i2).unwrap();
    wake_and_settle(&daemon).await;
    assert!(daemon.last_gc().is_some(), "a real write still sweeps");
    handle.abort();
}

/// Issue #30 x #29: accesses are not mutations for the timed trigger
/// either. A previously swept session past `gc_max_interval` with exactly
/// `gc_idle_floor` accesses applied since the sweep stays unswept (the
/// watermark never moves, so "mutations since the last sweep" stays 0);
/// the same session with real mutations at the floor sweeps on time.
#[tokio::test(start_paused = true)]
async fn applied_accesses_do_not_satisfy_the_idle_floor_of_the_timed_trigger() {
    let t0 = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
    let floor = 100u64;
    let params = CycleParams {
        gc_interval: 1_000_000,
        gc_max_interval: Duration::from_secs(3600),
        gc_idle_floor: floor,
        ..Default::default()
    };
    let (graph, cid) = locked_graph_with_one_concept();
    let epoch0 = graph.read().epoch();
    // As loaded: swept two days ago, nothing since.
    graph
        .write()
        .record_gc_sweep(epoch0, t0 - chrono::Duration::days(2));
    let ledger = Arc::new(access::AccessLedger::new());
    let (_cell, clock) = settable_clock(t0);
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
        params,
    )
    .with_clock(clock)
    .with_access_ledger(ledger.clone());
    let handle = daemon.spawn();
    wait_until(|| daemon.cycles() >= 1).await;

    for round in 0..5 {
        for _ in 0..(floor / 5) {
            ledger.record([cid], ts(round));
        }
        wake_and_settle(&daemon).await;
        assert_eq!(ledger.pending(), 0, "the cycle applied the ledger");
    }
    let applied = match graph.read().node(cid) {
        Some(crate::types::Node::Concept(c)) => c.access_count,
        _ => unreachable!(),
    };
    assert_eq!(u64::try_from(applied).unwrap(), floor, "all applied");
    assert_eq!(graph.read().epoch(), epoch0, "the epoch did not move");
    assert_eq!(graph.read().gc_mark().last_gc_epoch, epoch0);
    assert!(
        daemon.last_gc().is_none(),
        "{floor} accesses must not satisfy gc_idle_floor for the timed trigger"
    );
    handle.abort();

    // Control: the same floor met by real mutations does sweep on time.
    let (graph, _) = locked_graph_with_one_concept();
    let epoch = graph.read().epoch();
    graph
        .write()
        .record_gc_sweep(epoch - 3, t0 - chrono::Duration::days(2));
    let (_cell, clock) = settable_clock(t0);
    let control = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
        CycleParams {
            gc_idle_floor: 3,
            ..params
        },
    )
    .with_clock(clock);
    let handle = control.spawn();
    wake_and_settle(&control).await;
    assert_eq!(
        control.last_gc().expect("real mutations sweep").trigger,
        Some(gc::GcTrigger::Elapsed)
    );
    handle.abort();
}

/// Issue #17: the GC interval measures deployment-lifetime mutations. The
/// epoch a writer resumes from the durable snapshot counts every writer
/// before it, so a restart does not reset the sweep clock: once the
/// cumulative counter crosses `gc_interval` the sweep fires — and with it
/// `gc_survived` starts moving, Stage 1's input. Pre-fix this sweep never
/// fired: the restarted epoch began at 0 and the restarted writer's three
/// mutations could not cross the interval alone.
#[tokio::test(start_paused = true)]
async fn restart_resumes_gc_accounting_and_the_sweep_fires_cumulatively() {
    // "Process 1": a session with several mutations (both concepts keep
    // their Derives edges — a loaded graph must satisfy §5.7, so the
    // orphaning happens after the restart).
    let (graph, _cid) = locked_graph_with_one_concept();
    let iid = match graph.read().node(_cid).unwrap() {
        crate::types::Node::Concept(c) => c.origin_interaction,
        _ => unreachable!(),
    };
    let second = concept(2, iid, "second concept");
    let second_id = second.id;
    graph.write().insert_concept(second, iid).unwrap();
    let lifetime = graph.read().epoch();

    // "Restart": a writer attaches to the durable session. The interval
    // sits TWO mutations past the lifetime counter, so only a resumed
    // counter can cross it: the restarted writer's edge removal stays one
    // short, and the concept insert that follows (node + Derives, two
    // mutations) crosses it.
    let resumed = Graph::from_snapshot(graph.read().snapshot()).unwrap();
    assert_eq!(
        resumed.epoch(),
        lifetime,
        "the restart must resume the counter, not reset it"
    );
    let graph2 = Arc::new(RwLock::new(resumed));
    let params = CycleParams {
        gc_interval: lifetime + 2,
        ..Default::default()
    };
    let daemon = Daemon::with_params(
        graph2.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
        params,
    );
    let handle = daemon.spawn();

    // Warm-up cycle: the resumed counter is still two below the interval.
    wait_until(|| daemon.scores().epoch == graph2.read().epoch()).await;
    assert!(
        daemon.last_gc().is_none(),
        "no sweep before the interval is crossed"
    );

    // First mutation of the restarted writer: orphan `second`
    // (deterministic GC-able work). Still one short of the interval.
    let derives = {
        let g = graph2.read();
        g.edge_between(iid, second_id, EdgeType::Derives)
            .unwrap()
            .id
    };
    graph2.write().remove_edge(derives).unwrap();
    daemon.wake();
    wait_until(|| daemon.scores().epoch == graph2.read().epoch()).await;
    assert!(
        daemon.last_gc().is_none(),
        "one mutation short of the interval must not sweep"
    );

    // The insert crosses the interval CUMULATIVELY — process 1's mutations
    // count — and the sweep fires.
    let post = concept(3, iid, "post-restart concept");
    graph2.write().insert_concept(post, iid).unwrap();
    daemon.wake();
    wait_until(|| daemon.last_gc().is_some()).await;
    let outcome = daemon.last_gc().unwrap();
    assert!(
        outcome.concepts_collected.contains(&second_id),
        "the orphaned concept is collected: {outcome:?}"
    );
    // Every surviving concept takes its step-5 bump — Stage 1's input.
    let g = graph2.read();
    for id in &outcome.survivors {
        let c = match g.node(*id).unwrap() {
            crate::types::Node::Concept(c) => c,
            _ => unreachable!(),
        };
        assert_eq!(c.gc_survived, 1, "survivor bumped exactly once");
    }
    handle.abort();
}

/// `gc_survived` of every concept in `ids`, in order.
fn survived(graph: &Arc<RwLock<Graph>>, ids: &[NodeId]) -> Vec<i32> {
    let g = graph.read();
    ids.iter()
        .map(|id| match g.node(*id) {
            Some(crate::types::Node::Concept(c)) => c.gc_survived,
            _ => panic!("{id} missing"),
        })
        .collect()
}

/// A controllable cycle clock: the returned cell sets the daemon's `now`.
fn settable_clock(
    start: chrono::DateTime<Utc>,
) -> (Arc<std::sync::Mutex<chrono::DateTime<Utc>>>, Clock) {
    let cell = Arc::new(std::sync::Mutex::new(start));
    let clock: Clock = {
        let cell = cell.clone();
        Arc::new(move || *cell.lock().unwrap())
    };
    (cell, clock)
}

/// Issue #29 (a defect in #17's landed behaviour): the GC watermark is
/// durable. Pre-fix it was per-process state starting at 0, so once a
/// session's lifetime count passed `gc_interval` every writer restart swept
/// once and bumped every `gc_survived` — three restarts alone reached
/// Stage 1's `>= 3`. Now the restarted writer resumes the mark with the
/// epoch and a restart with no new writes sweeps nothing.
#[tokio::test(start_paused = true)]
async fn restart_past_gc_interval_does_not_sweep_or_bump_again() {
    let (graph, ids) = locked_graph_with_canonical_concepts(3);
    let params = CycleParams {
        gc_interval: 3,
        ..Default::default()
    };
    // Process 1: the lifetime count crosses the interval; one sweep.
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
        params,
    );
    let handle = daemon.spawn();
    wait_until(|| daemon.last_gc().is_some()).await;
    handle.abort();
    assert_eq!(survived(&graph, &ids), vec![1, 1, 1]);
    let mark = graph.read().gc_mark();
    assert!(mark.last_gc_epoch >= 3 && mark.last_gc_at.is_some());

    // Three restarts, no writes in between.
    let mut current = graph;
    for restart in 1..=3 {
        let resumed = Graph::from_snapshot(current.read().snapshot()).unwrap();
        assert_eq!(
            resumed.gc_mark(),
            mark,
            "restart {restart} resumes the mark"
        );
        current = Arc::new(RwLock::new(resumed));
        let daemon = Daemon::with_params(
            current.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
            params,
        );
        let handle = daemon.spawn();
        for _ in 0..3 {
            wake_and_settle(&daemon).await;
        }
        handle.abort();
        assert!(
            daemon.last_gc().is_none(),
            "restart {restart} must not sweep a session nobody wrote to"
        );
    }
    assert_eq!(
        survived(&current, &ids),
        vec![1, 1, 1],
        "restarts alone must never move gc_survived toward Stage 1"
    );
}

/// Issue #29: a previously swept session whose writer was down past
/// `gc_max_interval` with at least `gc_idle_floor` unswept mutations finds
/// a sweep already due and sweeps once on its first cycle back — the sweep
/// the downtime delayed, intended (and the case "restarts never sweep"
/// wording got wrong). Below the floor it does not.
#[tokio::test(start_paused = true)]
async fn a_previously_swept_session_sweeps_once_on_attach_when_already_due() {
    let t0 = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
    let params = CycleParams {
        gc_interval: 1_000_000,
        gc_max_interval: Duration::from_secs(3600),
        gc_idle_floor: 3,
        ..Default::default()
    };
    for (unswept, expect_sweep) in [(3u64, true), (2, false)] {
        let (graph, ids) = locked_graph_with_canonical_concepts(3);
        let epoch = graph.read().epoch();
        // As loaded: swept a week ago, `unswept` mutations since.
        graph
            .write()
            .record_gc_sweep(epoch - unswept, t0 - chrono::Duration::days(7));
        let (_cell, clock) = settable_clock(t0);
        let daemon = Daemon::with_params(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
            params,
        )
        .with_clock(clock);
        let handle = daemon.spawn();
        wake_and_settle(&daemon).await;
        wake_and_settle(&daemon).await;
        match daemon.last_gc() {
            Some(o) if expect_sweep => {
                assert_eq!(o.trigger, Some(gc::GcTrigger::Elapsed));
                assert_eq!(graph.read().gc_mark().last_gc_at, Some(t0));
                assert_eq!(survived(&graph, &ids), vec![1, 1, 1], "once");
            }
            None if !expect_sweep => {
                assert_eq!(survived(&graph, &ids), vec![0, 0, 0]);
            }
            other => panic!("unswept {unswept}: expected sweep {expect_sweep}, got {other:?}"),
        }
        handle.abort();
    }
}

/// Issue #29 item 3: a sweep time left in the future by a forward clock
/// jump (and kept by the monotonic merge) is re-anchored at `now` on the
/// first cycle, so the time trigger fires one interval later instead of
/// waiting for real time to catch up; a stamp within the tolerance is left
/// alone; the epoch watermark never moves.
#[tokio::test(start_paused = true)]
async fn a_future_sweep_time_is_reanchored_and_the_time_trigger_recovers() {
    let (graph, ids) = locked_graph_with_canonical_concepts(3);
    let iid = match graph.read().node(ids[0]).unwrap() {
        crate::types::Node::Concept(c) => c.origin_interaction,
        _ => unreachable!(),
    };
    let t0 = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
    let epoch = graph.read().epoch();
    // As loaded after a jump to next year: swept "then", nothing since.
    graph
        .write()
        .record_gc_sweep(epoch, t0 + chrono::Duration::days(365));
    let params = CycleParams {
        gc_interval: 1_000_000,
        gc_max_interval: Duration::from_secs(3600),
        gc_idle_floor: 1,
        ..Default::default()
    };
    let (clock_cell, clock) = settable_clock(t0);
    let set_clock = |t| *clock_cell.lock().unwrap() = t;
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
        params,
    )
    .with_clock(clock);
    let handle = daemon.spawn();

    wake_and_settle(&daemon).await;
    let mark = graph.read().gc_mark();
    assert_eq!(mark.last_gc_at, Some(t0), "re-anchored at now");
    assert!(mark.last_gc_at_reset, "the regression must persist");
    assert_eq!(mark.last_gc_epoch, epoch, "the epoch watermark never moves");
    assert!(daemon.last_gc().is_none(), "re-anchoring is not a sweep");

    graph
        .write()
        .insert_concept(concept(60, iid, "after the jump"), iid)
        .unwrap();
    set_clock(t0 + chrono::Duration::seconds(3600));
    wake_and_settle(&daemon).await;
    let swept = daemon.last_gc().expect("the time trigger recovered");
    assert_eq!(swept.trigger, Some(gc::GcTrigger::Elapsed));
    handle.abort();

    // Within the tolerance: left alone (and so not yet elapsed).
    let (graph, _) = locked_graph_with_canonical_concepts(3);
    let near = t0 + gc::GC_CLOCK_SKEW_TOLERANCE - chrono::Duration::seconds(1);
    let epoch = graph.read().epoch();
    graph.write().record_gc_sweep(epoch, near);
    let (_cell, clock) = settable_clock(t0);
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
        params,
    )
    .with_clock(clock);
    let handle = daemon.spawn();
    wake_and_settle(&daemon).await;
    let mark = graph.read().gc_mark();
    assert_eq!(mark.last_gc_at, Some(near));
    assert!(!mark.last_gc_at_reset);
    handle.abort();
}

/// Issue #29: the time bound. A session far below `gc_interval` sweeps once
/// `gc_max_interval` has passed since its last sweep **and** at least
/// `gc_idle_floor` mutations happened since; never on attach (the clock
/// is anchored, not overdue), never when idle, and once — not N times —
/// after a long gap.
#[tokio::test(start_paused = true)]
async fn timed_sweep_needs_the_interval_and_the_floor_and_has_no_backlog() {
    let (graph, ids) = locked_graph_with_canonical_concepts(3);
    let iid = match graph.read().node(ids[0]).unwrap() {
        crate::types::Node::Concept(c) => c.origin_interaction,
        _ => unreachable!(),
    };
    let floor = 5;
    let params = CycleParams {
        gc_interval: 1_000_000,
        gc_max_interval: Duration::from_secs(3600),
        gc_idle_floor: floor,
        ..Default::default()
    };
    let t0 = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
    let (clock_cell, clock) = settable_clock(t0);
    let set_clock = |t| *clock_cell.lock().unwrap() = t;
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
        params,
    )
    .with_clock(clock);
    let handle = daemon.spawn();

    // Attach: the warm-up epoch (7) is over the floor, but the session
    // never swept, so its clock is anchored at t0 — not overdue.
    wake_and_settle(&daemon).await;
    assert_eq!(graph.read().gc_mark().last_gc_at, Some(t0));
    assert!(daemon.last_gc().is_none(), "no sweep on attach");

    // One second short of the interval: nothing.
    set_clock(t0 + chrono::Duration::seconds(3599));
    wake_and_settle(&daemon).await;
    assert!(daemon.last_gc().is_none());

    // Interval elapsed and the floor met: the timed sweep fires.
    let t1 = t0 + chrono::Duration::seconds(3600);
    set_clock(t1);
    wake_and_settle(&daemon).await;
    let first = daemon.last_gc().expect("timed sweep");
    assert_eq!(first.trigger, Some(gc::GcTrigger::Elapsed));
    assert_eq!(survived(&graph, &ids), vec![1, 1, 1]);
    assert_eq!(graph.read().gc_mark().last_gc_at, Some(t1));

    // Idle for a day: elapsed, but zero mutations since — no sweep.
    set_clock(t1 + chrono::Duration::days(1));
    wake_and_settle(&daemon).await;
    wake_and_settle(&daemon).await;
    assert_eq!(survived(&graph, &ids), vec![1, 1, 1], "idle never sweeps");

    // Below the floor: two concept inserts are 4 mutations (node +
    // Derives each), one short of 5.
    {
        let mut g = graph.write();
        g.insert_concept(concept(50, iid, "w1"), iid).unwrap();
        g.insert_concept(concept(52, iid, "w3"), iid).unwrap();
    }
    wake_and_settle(&daemon).await;
    assert_eq!(survived(&graph, &ids), vec![1, 1, 1], "below the floor");

    // One more write reaches the floor; a month has passed since the last
    // sweep — exactly one sweep, no backlog.
    graph
        .write()
        .insert_concept(concept(51, iid, "w2"), iid)
        .unwrap();
    set_clock(t1 + chrono::Duration::days(30));
    for _ in 0..4 {
        wake_and_settle(&daemon).await;
    }
    assert_eq!(
        survived(&graph, &ids),
        vec![2, 2, 2],
        "thirty missed intervals are one sweep"
    );
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn loop_drains_deferred_survivor_bumps_to_convergence() {
    // CONC-6/XP-10: GC hands back the survivor bumps it deferred, and the
    // loop drains them a chunk per cycle. Every survivor must still end up
    // with exactly one bump for that run — chunking changes when, not which.
    let (graph, ids) = locked_graph_with_canonical_concepts(6);

    // gc_interval 3 → the warm-up epoch already crosses it; chunk 2 → the
    // run bumps 2 and defers 4, drained over the next two cycles.
    let params = CycleParams {
        gc_interval: 3,
        gc_survivor_bump_chunk: 2,
        ..Default::default()
    };
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_millis(10),
        params,
    );
    let handle = daemon.spawn();

    let all_bumped = |want: i32| {
        let g = graph.read();
        ids.iter().all(|id| match g.node(*id) {
            Some(crate::types::Node::Concept(c)) => c.gc_survived >= want,
            _ => false,
        })
    };
    wait_until(|| all_bumped(1)).await;
    // And no survivor is double-counted: GC does not re-run while a drain
    // is outstanding, so every counter is exactly 1 — one run, one bump
    // each. (This asserted only `max - min <= 1` while GC still funded its
    // own next sweep off the drains; that is NEW-2's fixed point now, and
    // `idle_session_reaches_a_gc_fixed_point_after_the_bumps_drain` pins it
    // directly.)
    {
        let g = graph.read();
        let counts: Vec<i32> = ids
            .iter()
            .map(|id| match g.node(*id) {
                Some(crate::types::Node::Concept(c)) => c.gc_survived,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            counts,
            vec![1; 6],
            "chunking changes when a bump lands, never how many"
        );
    }
    handle.abort();
}

/// A locked graph with `n` Canonical concepts off one interaction. Canonical
/// is protected, so every GC step spares them and the survivor set is
/// exactly these `n` — the shape both survivor-bump tests need.
fn locked_graph_with_canonical_concepts(n: u64) -> (Arc<RwLock<Graph>>, Vec<NodeId>) {
    let mut g = Graph::new(sid());
    let i = interaction(1);
    let iid = i.id;
    g.insert_interaction(i).unwrap();
    let mut ids: Vec<NodeId> = Vec::new();
    for k in 1..=n {
        let c = Concept {
            canonization_status: CanonizationStatus::Canonical,
            ..concept(k, iid, &format!("canonical {k}"))
        };
        ids.push(c.id);
        g.insert_concept(c, iid).unwrap();
    }
    (Arc::new(RwLock::new(g)), ids)
}

#[tokio::test(start_paused = true)]
async fn idle_session_reaches_a_gc_fixed_point_after_the_bumps_drain() {
    // NEW-2: the chunked survivor bumps re-triggered GC. Bumps are
    // mutations, `epoch_after` covers only the in-`run` chunk, and the
    // deferred tail drained on later cycles — where each `UpsertNode` was
    // credited as a *session* mutation toward the next `gc_interval`. With
    // `survivors >= gc_interval + chunk` GC became fully self-sustaining on
    // an idle session: `gc_survived` climbed past canonization Stage 1's
    // `>= 3` gate with zero writes, and the epoch ran away from T5.4's
    // recall cache.
    //
    // Six survivors, interval 3, chunk 2 — pre-fix this loops forever
    // (sweep, drain, drain, sweep, …). Post-fix the drains cancel out and
    // an idle session has exactly one sweep.
    let (graph, ids) = locked_graph_with_canonical_concepts(6);
    let params = CycleParams {
        gc_interval: 3,
        gc_survivor_bump_chunk: 2,
        ..Default::default()
    };
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_millis(10),
        params,
    );
    let handle = daemon.spawn();

    let survived = || -> Vec<i32> {
        let g = graph.read();
        ids.iter()
            .map(|id| match g.node(*id) {
                Some(crate::types::Node::Concept(c)) => c.gc_survived,
                _ => unreachable!("Canonical concepts are protected"),
            })
            .collect()
    };

    // The one sweep's bumps drain over the following cycles.
    wait_until(|| survived().iter().all(|&n| n == 1)).await;
    let settled_epoch = graph.read().epoch();
    let settled_cycles = daemon.cycles();

    // Now idle for many more cycles with ZERO session writes. Nothing may
    // move: no second sweep (`gc_survived` stays 1), and no mutation at all
    // (the epoch is the witness — a sweep's bumps would advance it).
    wait_until(|| daemon.cycles() >= settled_cycles + 40).await;
    assert_eq!(
        survived(),
        vec![1; 6],
        "an idle session must not sweep again — GC's own bumps must not \
             fund the next gc_interval"
    );
    assert_eq!(
        graph.read().epoch(),
        settled_epoch,
        "epoch must stabilize on an idle session (T5.4's recall cache keys \
             on it)"
    );
    handle.abort();
}

// ------------------------------------------------------------------
// XP-5 / XP-7 / CONC-4 — observability, config plumbing, containment
// ------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn last_gc_exposes_the_outcome_and_syncs_the_owners_index() {
    // XP-5: `GcOutcome` was dropped except `epoch_after`, so T6.4's
    // canonical-budget signal was unreachable and `sync_index` — spec §9
    // step 4 — had no production caller at all: a collected concept stayed
    // searchable in the owner's index indefinitely.
    let (graph, cid) = locked_graph_with_one_concept();
    // Orphan the concept so GC step 2 collects it deterministically.
    let iid = match graph.read().node(cid).unwrap() {
        crate::types::Node::Concept(c) => c.origin_interaction,
        _ => unreachable!(),
    };
    let derives = {
        let g = graph.read();
        g.edge_between(iid, cid, EdgeType::Derives).unwrap().id
    };
    graph.write().remove_edge(derives).unwrap();

    // The owner's index, pre-populated the way the P3 contract requires.
    let index = Arc::new(RwLock::new(InvertedIndex::new()));
    {
        let g = graph.read();
        let mut idx = index.write();
        for c in g.concepts() {
            idx.add(c);
        }
    }
    assert!(
        !index.read().search("user schema", 5).is_empty(),
        "the concept must start out searchable"
    );

    let params = CycleParams {
        gc_interval: 3,
        ..Default::default()
    };
    let daemon = Daemon::with_params(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_millis(10),
        params,
    )
    .with_index(index.clone());
    assert!(
        daemon.last_gc().is_none(),
        "no outcome before the first run"
    );
    let handle = daemon.spawn();

    wait_until(|| daemon.last_gc().is_some()).await;
    let outcome = daemon.last_gc().unwrap();
    assert!(
        outcome.concepts_collected.contains(&cid),
        "the orphan was collected: {outcome:?}"
    );
    assert_eq!(outcome.max_canonical_nodes, 1000, "the ceiling T6.4 reads");
    assert!(!outcome.canonical_over_budget);
    assert_eq!(outcome.epoch_after, graph.read().epoch());

    // Step 4: the index no longer serves the collected concept.
    wait_until(|| index.read().search("user schema", 5).is_empty()).await;
    handle.abort();
}

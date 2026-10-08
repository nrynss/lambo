//! Events, stats, pulse and freshness.

use super::*;

/// Remediation round 3: same-instant canonization events are routine (one
/// eval cycle stamps the cycle's `now` on every event it emits), and the
/// feed's `seq` — which the page uses as a cursor — must not depend on
/// run-minted event ids. The two events here share one `occurred_at` and
/// their nodes' keys order OPPOSITE to the event ids, so the old
/// `(occurred_at, event id)` order emitted `beta` first; the stable chain
/// emits `alpha` first.
#[test]
fn canon_event_feed_ties_break_on_node_canonical_key_ahead_of_id() {
    let sid = SessionId::from("web-events");
    let node_alpha = NodeId(uuid::Uuid::from_u64_pair(5, 2)); // key "alpha api"
    let node_beta = NodeId(uuid::Uuid::from_u64_pair(5, 1)); // key "beta api"
                                                             // One shared instant: the cycle's `now` that every event in the batch
                                                             // carries. The actual value is irrelevant; only its equality is.
    let at = Utc::now();
    let snap = GraphSnapshot {
        session_id: sid.clone(),
        concepts: vec![
            concept(sid.clone(), node_alpha, NodeId::nil(), "alpha api", at),
            concept(sid.clone(), node_beta, NodeId::nil(), "beta api", at),
        ],
        canonization_events: vec![
            // Smaller event id on the LARGER-key node.
            CanonizationEvent {
                id: NodeId(uuid::Uuid::from_u64_pair(9, 1)),
                session_id: sid.clone(),
                node_id: node_beta,
                from_status: CanonizationStatus::Venerable,
                to_status: CanonizationStatus::Canonical,
                blast_radius: None,
                occurred_at: at,
                last_demotion_time: None,
            },
            // Larger event id on the SMALLER-key node.
            CanonizationEvent {
                id: NodeId(uuid::Uuid::from_u64_pair(9, 2)),
                session_id: sid.clone(),
                node_id: node_alpha,
                from_status: CanonizationStatus::Candidate,
                to_status: CanonizationStatus::Venerable,
                blast_radius: None,
                occurred_at: at,
                last_demotion_time: None,
            },
        ],
        ..GraphSnapshot::default()
    };

    let feed = events_from(&snap, 0);
    assert_eq!(feed.total, 2);
    assert_eq!(
        feed.events[0].node_id,
        node_alpha.0.to_string(),
        "same-instant events follow the moved concept's canonical key, not the event id"
    );
    assert_eq!(feed.events[1].node_id, node_beta.0.to_string());
}

#[tokio::test]
async fn events_endpoint_tails_the_canonization_feed() {
    let store = seed("t85-events").await;
    let (addr, handle) = spawn(state_on(store.clone(), "t85-events")).await;

    let all = get_json(addr, "/api/events").await;
    assert_eq!(all["total"], 3, "three audited hops were seeded: {all}");
    let events = all["events"].as_array().expect("events array");
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["from_status"], "None");
    assert_eq!(events[0]["to_status"], "Candidate");
    assert_eq!(events[2]["to_status"], "Canonical");
    assert_eq!(
        events[2]["content"], "user schema",
        "the feed must name the concept, not just its uuid: {all}"
    );
    assert_eq!(events[0]["seq"], 0);
    assert_eq!(events[2]["seq"], 2);

    // The cursor the page polls with: nothing new since the last read.
    let caught_up = get_json(addr, "/api/events?since=3").await;
    assert_eq!(caught_up["total"], 3);
    assert!(caught_up["events"].as_array().expect("array").is_empty());

    // A new transition appends, and only the new one comes back.
    promote(&store, "t85-events", "auth middleware").await;
    let tail = get_json(addr, "/api/events?since=3").await;
    assert_eq!(tail["total"], 6, "{tail}");
    let tail_events = tail["events"].as_array().expect("array");
    assert_eq!(tail_events.len(), 3);
    assert_eq!(tail_events[0]["content"], "auth middleware");
    assert_eq!(tail_events[0]["seq"], 3);

    handle.abort();
}

#[tokio::test]
async fn stats_endpoint_counts_the_session_and_refuses_to_fake_writer_fields() {
    let store = seed("t85-stats").await;
    let (addr, handle) = spawn(state_on(store, "t85-stats")).await;

    let stats = get_json(addr, "/api/stats").await;
    assert!(stats["nodes"].as_u64().expect("nodes") >= 3, "{stats}");
    assert!(stats["edges"].as_u64().expect("edges") >= 1, "{stats}");
    assert_eq!(stats["concepts"], 3, "{stats}"); // user schema, auth middleware, create user
    assert_eq!(stats["canonical"], 1, "{stats}");
    assert_eq!(stats["canonization_events"], 3, "{stats}");
    assert_eq!(stats["mode"], "reader");

    // The load-bearing honesty: a reader must say n/a, never 0.
    assert!(
        stats["flush_lag_ms"].is_null(),
        "a reader cannot observe flush lag; reporting a number would be a lie: {stats}"
    );
    assert!(stats["log_depth"].is_null(), "{stats}");
    assert!(
        stats["writer_only"]
            .as_str()
            .expect("writer_only")
            .contains("reader"),
        "{stats}"
    );

    handle.abort();
}

#[tokio::test]
async fn stats_endpoint_renders_writer_published_flush_stats() {
    // T85-3: once a writer's FlushTask has published flush stats into the
    // shared store, a reader must render the real numbers (not n/a).
    let store = seed("t85-stats-live").await;
    let sid = SessionId::from("t85-stats-live");
    store
        .write_flush_stats(
            &sid,
            &crate::store::SessionFlushStats {
                flush_lag_ms: 12,
                log_depth: 3,
            },
        )
        .await
        .unwrap();
    let (addr, handle) = spawn(state_on(store, "t85-stats-live")).await;

    let stats = get_json(addr, "/api/stats").await;
    assert_eq!(stats["flush_lag_ms"], 12, "{stats}");
    assert_eq!(stats["log_depth"], 3, "{stats}");
    assert_eq!(stats["mode"], "reader");

    handle.abort();
}

#[tokio::test]
async fn pulse_returns_stats_and_events_in_one_round_trip() {
    let store = seed("t85-pulse").await;
    let (addr, handle) = spawn(state_on(store, "t85-pulse")).await;

    let pulse = get_json(addr, "/api/pulse?since=0").await;
    assert_eq!(pulse["events"]["total"], 3, "{pulse}");
    assert_eq!(pulse["stats"]["canonical"], 1, "{pulse}");
    assert_eq!(
        pulse["stats"]["canonization_events"], pulse["events"]["total"],
        "stats and the feed must agree within one response: {pulse}"
    );

    handle.abort();
}

// ---- freshness ------------------------------------------------------

#[test]
fn durable_change_age_resets_only_when_the_counts_move() {
    let state = state_on(Arc::new(MemoryStore::new()), "t85-freshness");
    let first = state.observe(7);
    std::thread::sleep(Duration::from_millis(12));
    let same = state.observe(7);
    assert!(
        same > first,
        "an unchanged snapshot must keep ageing: {first:?} -> {same:?}"
    );
    let moved = state.observe(8);
    assert!(
        moved < same,
        "a changed snapshot must reset the age: {same:?} -> {moved:?}"
    );
}

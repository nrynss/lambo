//! Daemon event transport — spec §6.1, P4 T4.6.
//!
//! A `tokio::sync::broadcast` channel carrying [`DaemonEvent`]. The sender
//! lives on the daemon ([`crate::daemon::Daemon::events`] subscribes);
//! consumers decide when to read. A **dropped or lagging receiver is not an
//! error**: [`emit`] is fire-and-forget — a dropped receiver discards the
//! event (`SendError` is swallowed) and a slow receiver simply misses
//! messages (`RecvError::Lagged`) once the channel's capacity is exceeded.
//! The daemon never blocks on consumers (spec §6.1: "A dropped receiver is
//! not an error").
//!
//! ## Bounded channel vs. "no queue bound"
//!
//! Spec §6.1 deletes v0.6.0's callback pool — "no callback thread pool, no
//! re-entrancy guard, no execution timeout, no queue bound". `tokio::sync::broadcast`
//! still needs a finite capacity; [`EVENT_CAPACITY`] is that bound. The "no
//! queue bound" promise is kept in *behavior*, not in memory: a consumer that
//! falls behind is dropped from the live window (`Lagged`), never
//! back-pressured, so the daemon never blocks and never accumulates
//! unboundedly.
//!
//! ## Event sources
//!
//! * **Conflict / Drift / Stale / HighRisk** — the daemon loop runs the four
//!   detectors each cycle, publishes `DaemonEvent`s on condition
//!   **transition** (enter the detected set; exit just stops emitting — the
//!   §6.1 enum has no resolved variant), and keeps the hot list equal to the
//!   cycle's fresh hits ([`crate::daemon::hotlist::HotList::retain_conditions`]).
//! * **Stale / HighRisk** — spec §6.1 names the five kinds and §9 names the
//!   hot-list conditions ("conflict detected, high-risk modification, drift
//!   detected, stale session") but defines **no quantitative triggers** for
//!   the last two. v0.1 interpretation (documented seam — T5.x may refine):
//!   * **Stale** — a concept whose most recent activity (creation, access,
//!     or any incident-edge write) is older than [`STALE_WINDOW`]. The window
//!     matches GC's `GC_EDGE_TTL` (1h) so "untouched for this long" means the
//!     same thing to GC and to staleness. The T2/T3 writers never set
//!     `last_accessed`, so activity is currently `max(created_at, edge
//!     writes)`; once recall (T5.x) stamps `last_accessed`, accesses join the
//!     max.
//!   * **HighRisk** — a fresh write to a **high-value** node: Canonical,
//!     Venerable, or `blast_radius >= HIGH_RISK_BLAST_RADIUS` (spec §10
//!     Stage-3 threshold: hypothetical removal would orphan > 5 nodes).
//!     Modifying such a node is the "high-risk modification" the hot list
//!     names. A write is fresh when the node itself or any incident edge was
//!     written inside [`HIGH_RISK_WRITE_WINDOW`] (the same 30s recency story
//!     as conflict).
//! * **Canonized** — the emit site is P6 (canonization transitions);
//!   [`emit_canonized`] is the seam. The daemon loop does **not** fabricate
//!   canonization events.
//!
//! ## P8 seam
//!
//! Spec §6.1's `mem.events() -> Receiver<DaemonEvent>` delegates to
//! [`crate::daemon::Daemon::events`] — one channel per daemon, same sender.

use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::daemon::conflict::ConflictHit;
use crate::daemon::drift::DriftHit;
use crate::daemon::hotlist::{Condition, HotList, HotListEntry, HotListPayload};
use crate::graph::Graph;
use crate::types::{CanonizationEvent, CanonizationStatus, DaemonEvent, NodeId};
use tokio::sync::broadcast;

/// Broadcast capacity. Consumers slower than the daemon's peak emission rate
/// fall behind and get `RecvError::Lagged` — the daemon never waits for them.
pub const EVENT_CAPACITY: usize = 256;

/// A concept untouched for this long is *stale* (spec §6.1 `Stale` event;
/// §9 "stale session" hot-list condition). v0.1 decision — the spec defines
/// no bound. Aligned with GC's `GC_EDGE_TTL` (1h): "untouched for this long"
/// is one story across the daemon.
pub const STALE_WINDOW: Duration = Duration::from_secs(3600);

/// A write inside this window counts as a *current* modification (HighRisk).
/// Same recency story as conflict's `CONFLICT_RECENCY_WINDOW`.
pub const HIGH_RISK_WRITE_WINDOW: Duration = Duration::from_secs(30);

/// A node whose hypothetical removal would orphan at least this many nodes
/// (spec §10 Stage-3 threshold: "orphan > 5 nodes") is high-value; modifying
/// it is high-risk.
pub const HIGH_RISK_BLAST_RADIUS: u64 = 5;

/// A fresh event channel at [`EVENT_CAPACITY`].
pub fn event_channel() -> (
    broadcast::Sender<DaemonEvent>,
    broadcast::Receiver<DaemonEvent>,
) {
    event_channel_with_capacity(EVENT_CAPACITY)
}

/// An event channel with an explicit capacity (tests; the slow-consumer
/// contract lives at small capacities).
fn event_channel_with_capacity(
    capacity: usize,
) -> (
    broadcast::Sender<DaemonEvent>,
    broadcast::Receiver<DaemonEvent>,
) {
    assert!(capacity > 0, "broadcast capacity must be > 0");
    broadcast::channel(capacity)
}

/// Publish one event, fire-and-forget (spec §6.1).
///
/// `Sender::send` returns `Err(SendError)` only when **zero** receivers
/// exist — the dropped-receiver case, which is not an error. Lagged receivers
/// never make `send` fail (they just skip the value). This never blocks and
/// never panics, so any caller may publish without coordinating with
/// consumers.
pub fn emit(sender: &broadcast::Sender<DaemonEvent>, event: DaemonEvent) {
    let _ = sender.send(event);
}

/// The P6 seam: publish a canonization transition. Canonization (P6) calls
/// this at every transition; the daemon loop does not invent `Canonized`
/// events.
///
/// `#[allow(dead_code)]`: P6 is the only caller; until then the helper is a
/// deliberate, documented seam (spec §6.1 requires the `Canonized` kind on
/// the channel from day one).
#[allow(dead_code)]
pub(crate) fn emit_canonized(sender: &broadcast::Sender<DaemonEvent>, event: CanonizationEvent) {
    emit(sender, DaemonEvent::Canonized { event });
}

/// Map a conflict hit to its broadcast event. T5.3 renders the payload, so
/// `detail` carries the renderable sentence ("agents [a, b] wrote within 5s").
pub fn conflict_event(hit: &ConflictHit) -> DaemonEvent {
    DaemonEvent::Conflict {
        node_id: hit.node,
        agents: hit.agents.clone(),
        detail: format!("agents {:?} wrote within {}s", hit.agents, hit.seconds_ago),
    }
}

/// Map a drift hit to its broadcast event. [`DriftHit::detail`] already
/// renders "concept X is N hops from root goal Y (threshold T)".
pub fn drift_event(hit: &DriftHit) -> DaemonEvent {
    DaemonEvent::Drift {
        node_id: hit.node,
        hops: hit.hops as u32,
        detail: hit.detail.clone(),
    }
}

/// Build a `Stale` event from a detector hit.
pub fn stale_event(node_id: NodeId, seconds_inactive: u64) -> DaemonEvent {
    DaemonEvent::Stale {
        node_id,
        detail: format!("node {node_id} untouched for {seconds_inactive}s"),
    }
}

/// Build a `HighRisk` event from a detector hit. `reason` is the renderable
/// detail ("high-value node X (Canonical, blast radius 8) modified within
/// 30s").
pub fn high_risk_event(node_id: NodeId, reason: String) -> DaemonEvent {
    DaemonEvent::HighRisk {
        node_id,
        detail: reason,
    }
}

/// One stale node (spec §6.1 `Stale`). Pure data; the loop turns it into a
/// `DaemonEvent::Stale` and a hot-list entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleHit {
    /// The stale concept.
    pub node: NodeId,
    /// How long its most recent activity is in the past, in seconds.
    pub seconds_inactive: u64,
}

/// Most recent activity of a concept: its own creation/access or any incident
/// edge write. `None` for a node missing from the graph (defensive).
fn last_activity(graph: &Graph, node: NodeId) -> Option<DateTime<Utc>> {
    let own = match graph.node(node) {
        Some(crate::types::Node::Concept(c)) => c.last_accessed.unwrap_or(c.created_at),
        _ => return None,
    };
    let incident = graph
        .incident_edges(node)
        .iter()
        .map(|e| e.last_reinforced.max(e.created_at))
        .max()
        .unwrap_or(own);
    Some(own.max(incident))
}

/// Detect every stale concept: activity older than `window` at `now`
/// (see the module docs for the v0.1 rule). Pure; deterministic
/// (id-ascending). Edges dated after `now` (mocked clocks) are not activity.
pub fn detect_stale(graph: &Graph, window: Duration, now: DateTime<Utc>) -> Vec<StaleHit> {
    let window_start = now
        - ChronoDuration::from_std(window).expect("stale window fits in chrono's duration range");
    let mut hits: Vec<StaleHit> = Vec::new();
    for c in graph.concepts() {
        let Some(last) = last_activity(graph, c.id) else {
            continue;
        };
        if last > now || last >= window_start {
            continue;
        }
        hits.push(StaleHit {
            node: c.id,
            seconds_inactive: (now - last).num_seconds().max(0) as u64,
        });
    }
    hits.sort_by_key(|h| h.node.0);
    hits
}

/// One high-risk modification (spec §6.1 `HighRisk`). Pure data; the loop
/// turns it into a `DaemonEvent::HighRisk` and a hot-list entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighRiskHit {
    /// The modified high-value node.
    pub node: NodeId,
    /// Renderable reason ("high-value node X (Canonical) modified within 30s").
    pub reason: String,
}

/// Is `c` a high-value node — modifying it is a high-risk modification?
fn is_high_value(c: &crate::types::Concept) -> bool {
    matches!(
        c.canonization_status,
        CanonizationStatus::Canonical | CanonizationStatus::Venerable
    ) || c
        .blast_radius
        .is_some_and(|b| b >= HIGH_RISK_BLAST_RADIUS as i32)
}

/// Detect every high-risk modification: a fresh write to a high-value node
/// (see the module docs for the v0.1 rule). Pure; deterministic
/// (id-ascending). Future-dated writes do not count.
pub fn detect_high_risk(graph: &Graph, window: Duration, now: DateTime<Utc>) -> Vec<HighRiskHit> {
    let window_start = now
        - ChronoDuration::from_std(window)
            .expect("high-risk window fits in chrono's duration range");
    let mut hits: Vec<HighRiskHit> = Vec::new();
    for c in graph.concepts() {
        if !is_high_value(c) {
            continue;
        }
        let own_write = c.created_at >= window_start && c.created_at <= now;
        let edge_write = graph.incident_edges(c.id).iter().any(|e| {
            let w = e.last_reinforced.max(e.created_at);
            w >= window_start && w <= now
        });
        if !(own_write || edge_write) {
            continue;
        }
        let status = format!("{:?}", c.canonization_status);
        let radius = c
            .blast_radius
            .map(|b| format!(", blast radius {b}"))
            .unwrap_or_default();
        hits.push(HighRiskHit {
            node: c.id,
            reason: format!(
                "high-value node {} ({status}{radius}) modified within {}s",
                c.id,
                window.as_secs()
            ),
        });
    }
    hits.sort_by_key(|h| h.node.0);
    hits
}

/// Detect stale nodes and refresh the hot list with one `StaleSession` entry
/// per hit (mirrors `conflict::insert_conflicts`). Returns the hits: the
/// daemon loop publishes them on transition and syncs the hot list against
/// them.
///
/// Re-insertion refreshes the payload + re-validation predicate; the T4.6
/// loop additionally drops entries whose `(node, condition)` is no longer
/// detected ([`crate::daemon::hotlist::HotList::retain_conditions`]), so a
/// node touched again (activity re-enters the window) is evicted on the next
/// cycle — no captured-`now` predicate involved.
pub fn insert_stale(
    hot: &mut HotList,
    graph: &Graph,
    window: Duration,
    now: DateTime<Utc>,
) -> Vec<StaleHit> {
    let hits = detect_stale(graph, window, now);
    for hit in &hits {
        let node = hit.node;
        let payload = HotListPayload::Stale {
            seconds_inactive: hit.seconds_inactive,
        };
        let holds = move |g: &Graph| detect_stale(g, window, now).iter().any(|h| h.node == node);
        let _ = hot.insert(HotListEntry::new(
            node,
            Condition::StaleSession,
            payload,
            holds,
        ));
    }
    hits
}

/// Detect high-risk modifications and refresh the hot list with one
/// `HighRiskModification` entry per hit (mirrors `conflict::insert_conflicts`).
/// Returns the hits: the daemon loop publishes them on transition and syncs
/// the hot list against them.
///
/// Re-insertion refreshes the payload + re-validation predicate; the T4.6
/// loop drops entries whose `(node, condition)` is no longer detected each
/// cycle, so a HighRisk entry whose 30s write window elapsed is evicted on
/// the next cycle — it cannot linger as a ghost (finding 2).
pub fn insert_high_risk(
    hot: &mut HotList,
    graph: &Graph,
    window: Duration,
    now: DateTime<Utc>,
) -> Vec<HighRiskHit> {
    let hits = detect_high_risk(graph, window, now);
    for hit in &hits {
        let node = hit.node;
        let payload = HotListPayload::HighRisk {
            reason: hit.reason.clone(),
        };
        let holds = move |g: &Graph| {
            detect_high_risk(g, window, now)
                .iter()
                .any(|h| h.node == node)
        };
        let _ = hot.insert(HotListEntry::new(
            node,
            Condition::HighRiskModification,
            payload,
            holds,
        ));
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use crate::types::{AgentId, Concept, ConceptType, Edge, EdgeType, Interaction, SessionId};
    use chrono::TimeZone;
    use tokio::sync::broadcast::error::RecvError;
    use uuid::Uuid;

    fn ts(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + s, 0).unwrap()
    }

    fn sid() -> SessionId {
        SessionId::from("t4.6-events")
    }

    fn nid(kind: u64, id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(kind, id))
    }

    fn interaction(id: u64, prev: Option<u64>, agent: &str, at: i64) -> Interaction {
        Interaction {
            id: nid(0, id),
            session_id: sid(),
            agent_id: AgentId::from(agent),
            prompt_text: Some("p".into()),
            previous_id: prev.map(|p| nid(0, p)),
            created_at: ts(at),
        }
    }

    fn concept(
        id: u64,
        origin: NodeId,
        agent: &str,
        content: &str,
        at: i64,
        status: CanonizationStatus,
        blast_radius: Option<i32>,
    ) -> Concept {
        Concept {
            id: nid(1, id),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.to_string(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from(agent),
            created_at: ts(at),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: status,
            blast_radius,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        }
    }

    fn edge(id: u64, source: NodeId, target: NodeId, at: i64) -> Edge {
        Edge {
            id: nid(3, id),
            session_id: sid(),
            source,
            target,
            edge_type: EdgeType::Dependency,
            weight: 1.0,
            reinforcements: 1,
            created_at: ts(at),
            last_reinforced: ts(at),
        }
    }

    /// One interaction (agent-a) + a second interaction (agent-b).
    fn base_graph() -> (Graph, NodeId, NodeId) {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, "agent-a", 0);
        let i1_id = i1.id;
        g.insert_interaction(i1).unwrap();
        let i2 = interaction(2, Some(1), "agent-b", 10);
        let i2_id = i2.id;
        g.insert_interaction(i2).unwrap();
        (g, i1_id, i2_id)
    }

    #[tokio::test]
    async fn all_five_kinds_round_trip_through_emit() {
        let (tx, mut rx) = event_channel_with_capacity(16);
        emit(
            &tx,
            DaemonEvent::Conflict {
                node_id: nid(1, 1),
                agents: vec![AgentId::from("agent-a")],
                detail: "d".into(),
            },
        );
        emit(
            &tx,
            DaemonEvent::Drift {
                node_id: nid(1, 2),
                hops: 6,
                detail: "d".into(),
            },
        );
        emit(
            &tx,
            DaemonEvent::Stale {
                node_id: nid(1, 3),
                detail: "d".into(),
            },
        );
        emit(
            &tx,
            DaemonEvent::HighRisk {
                node_id: nid(1, 4),
                detail: "d".into(),
            },
        );
        let canon = CanonizationEvent {
            id: nid(9, 1),
            session_id: sid(),
            node_id: nid(1, 4),
            from_status: CanonizationStatus::None,
            to_status: CanonizationStatus::Canonical,
            blast_radius: Some(3),
            last_demotion_time: None,
            occurred_at: ts(0),
        };
        emit_canonized(&tx, canon.clone());

        let got: Vec<DaemonEvent> = vec![
            rx.recv().await.unwrap(),
            rx.recv().await.unwrap(),
            rx.recv().await.unwrap(),
            rx.recv().await.unwrap(),
            rx.recv().await.unwrap(),
        ];
        assert_eq!(
            got,
            vec![
                DaemonEvent::Conflict {
                    node_id: nid(1, 1),
                    agents: vec![AgentId::from("agent-a")],
                    detail: "d".into(),
                },
                DaemonEvent::Drift {
                    node_id: nid(1, 2),
                    hops: 6,
                    detail: "d".into(),
                },
                DaemonEvent::Stale {
                    node_id: nid(1, 3),
                    detail: "d".into(),
                },
                DaemonEvent::HighRisk {
                    node_id: nid(1, 4),
                    detail: "d".into(),
                },
                DaemonEvent::Canonized { event: canon },
            ],
            "all five kinds arrive, in emission order"
        );
    }

    #[tokio::test]
    async fn slow_receiver_gets_lagged_not_a_hang() {
        // Capacity 2; the receiver never drains. Every send succeeds (never
        // blocks, never errors while a receiver exists); the oldest values
        // are dropped and the receiver eventually observes Lagged — the
        // spec §6.1 contract for a consumer that cannot keep up.
        let (tx, mut rx) = event_channel_with_capacity(2);
        for i in 0..4 {
            let sent = tx.send(DaemonEvent::Stale {
                node_id: nid(1, i),
                detail: format!("{i}"),
            });
            assert!(sent.is_ok(), "send must never fail while a receiver exists");
        }

        match rx.recv().await {
            Err(RecvError::Lagged(skipped)) => {
                assert!(skipped >= 1, "missed messages must be reported");
            }
            other => panic!("expected Lagged, got {other:?}"),
        }

        // The receiver is re-synced to the newest retained window; it still
        // sees the tail of the stream.
        let evt = rx.recv().await.unwrap();
        match evt {
            DaemonEvent::Stale { detail, .. } => assert_eq!(detail, "2"),
            other => panic!("expected the newest retained Stale, got {other:?}"),
        }
    }

    #[test]
    fn dropped_receiver_discards_events_silently() {
        let (tx, rx) = event_channel_with_capacity(4);
        drop(rx);
        // Zero receivers -> SendError; emit swallows it (spec §6.1: a dropped
        // receiver is not an error) — no panic, no block.
        emit(
            &tx,
            DaemonEvent::Stale {
                node_id: nid(1, 1),
                detail: "d".into(),
            },
        );
    }

    #[test]
    fn conflict_and_drift_event_mappers_carry_renderable_details() {
        let c = conflict_event(&ConflictHit {
            node: nid(1, 1),
            agents: vec![AgentId::from("agent-a"), AgentId::from("agent-b")],
            seconds_ago: 5,
        });
        match c {
            DaemonEvent::Conflict {
                node_id,
                agents,
                detail,
            } => {
                assert_eq!(node_id, nid(1, 1));
                assert_eq!(
                    agents,
                    vec![AgentId::from("agent-a"), AgentId::from("agent-b")]
                );
                assert!(
                    detail.contains("agent-a")
                        && detail.contains("agent-b")
                        && detail.contains("5s"),
                    "detail must be renderable: {detail}"
                );
            }
            other => panic!("expected Conflict, got {other:?}"),
        }

        let d = drift_event(&DriftHit {
            node: nid(1, 2),
            goal: nid(1, 3),
            hops: 6,
            detail: "concept X is 6 hops from root goal Y (threshold 5)".into(),
        });
        match d {
            DaemonEvent::Drift {
                node_id,
                hops,
                detail,
            } => {
                assert_eq!(node_id, nid(1, 2));
                assert_eq!(hops, 6);
                assert!(
                    detail.contains("6 hops"),
                    "detail must be renderable: {detail}"
                );
            }
            other => panic!("expected Drift, got {other:?}"),
        }
    }

    #[test]
    fn detect_stale_flags_only_nodes_untouched_beyond_the_window() {
        let now = ts(7200);
        let (mut g, i1_id, i2_id) = base_graph();
        // c1: created at 0, accessed at 400 -> stale (6800s inactive; the
        // access is far older than the 1h window starting at 3600).
        let mut c1 = concept(
            1,
            i1_id,
            "agent-a",
            "stale one",
            0,
            CanonizationStatus::None,
            None,
        );
        c1.last_accessed = Some(ts(400));
        let c1_id = c1.id;
        g.insert_concept(c1, i1_id).unwrap();
        // c2: created at 7000 (200s ago) -> fresh, never stale.
        let c2 = concept(
            2,
            i2_id,
            "agent-b",
            "fresh one",
            7000,
            CanonizationStatus::None,
            None,
        );
        let c2_id = c2.id;
        g.insert_concept(c2, i2_id).unwrap();
        // c3: created at 0 but with an edge written at 7100 (100s ago) -> the
        // edge write refreshes its activity; not stale.
        let c3 = concept(
            3,
            i1_id,
            "agent-a",
            "refreshed one",
            0,
            CanonizationStatus::None,
            None,
        );
        let c3_id = c3.id;
        g.insert_concept(c3, i1_id).unwrap();
        g.upsert_edge(edge(1, c2_id, c3_id, 7100)).unwrap();

        let hits = detect_stale(&g, STALE_WINDOW, now);
        assert_eq!(hits.len(), 1, "only c1 is stale: {hits:?}");
        assert_eq!(hits[0].node, c1_id);
        assert_eq!(hits[0].seconds_inactive, 6800);
    }

    #[test]
    fn detect_high_risk_requires_value_and_fresh_write() {
        let now = ts(100);
        let (mut g, i1_id, i2_id) = base_graph();
        // c1: Canonical, Dependency write at 90 (10s ago) -> hit.
        let c1 = concept(
            1,
            i1_id,
            "agent-a",
            "canonical",
            0,
            CanonizationStatus::Canonical,
            None,
        );
        let c1_id = c1.id;
        g.insert_concept(c1, i1_id).unwrap();
        // c2: blast radius 8, write at 95 (5s ago) -> hit.
        let c2 = concept(
            2,
            i1_id,
            "agent-a",
            "big radius",
            0,
            CanonizationStatus::None,
            Some(8),
        );
        let c2_id = c2.id;
        g.insert_concept(c2, i1_id).unwrap();
        // c3: Venerable but write at 50 is outside the 30s window -> no hit.
        let c3 = concept(
            3,
            i2_id,
            "agent-b",
            "venerable old write",
            0,
            CanonizationStatus::Venerable,
            None,
        );
        let c3_id = c3.id;
        g.insert_concept(c3, i2_id).unwrap();
        // c4: ordinary node with a fresh write -> not high-value -> no hit.
        let c4 = concept(
            4,
            i2_id,
            "agent-b",
            "plain fresh",
            0,
            CanonizationStatus::None,
            None,
        );
        let c4_id = c4.id;
        g.insert_concept(c4, i2_id).unwrap();
        // Writer concept for the Dependency edges (never a hit itself).
        let cw = concept(
            5,
            i2_id,
            "agent-b",
            "writer",
            0,
            CanonizationStatus::None,
            None,
        );
        let cw_id = cw.id;
        g.insert_concept(cw, i2_id).unwrap();
        g.upsert_edge(edge(1, cw_id, c1_id, 90)).unwrap();
        g.upsert_edge(edge(2, cw_id, c2_id, 95)).unwrap();
        g.upsert_edge(edge(3, cw_id, c3_id, 50)).unwrap();
        g.upsert_edge(edge(4, cw_id, c4_id, 95)).unwrap();

        let hits = detect_high_risk(&g, HIGH_RISK_WRITE_WINDOW, now);
        assert_eq!(hits.len(), 2, "only c1 and c2 fire: {hits:?}");
        assert_eq!(hits[0].node, c1_id);
        assert!(
            hits[0].reason.contains("Canonical"),
            "reason: {}",
            hits[0].reason
        );
        assert_eq!(hits[1].node, c2_id);
        assert!(
            hits[1].reason.contains("blast radius 8"),
            "reason: {}",
            hits[1].reason
        );
    }

    #[test]
    fn hot_list_entries_refresh_and_revalidate() {
        let now = ts(7200);
        let (mut g, i1_id, _) = base_graph();
        // One stale node (c1 @ 0) and one high-risk node (Canonical c2 with a
        // fresh write at 7190, 10s before now -> inside the 30s window).
        let c1 = concept(
            1,
            i1_id,
            "agent-a",
            "stale",
            0,
            CanonizationStatus::None,
            None,
        );
        let c1_id = c1.id;
        g.insert_concept(c1, i1_id).unwrap();
        let c2 = concept(
            2,
            i1_id,
            "agent-a",
            "canonical",
            0,
            CanonizationStatus::Canonical,
            None,
        );
        let c2_id = c2.id;
        g.insert_concept(c2, i1_id).unwrap();
        let cw = concept(
            3,
            i1_id,
            "agent-a",
            "writer",
            0,
            CanonizationStatus::None,
            None,
        );
        let cw_id = cw.id;
        g.insert_concept(cw, i1_id).unwrap();
        g.upsert_edge(edge(1, cw_id, c2_id, 7190)).unwrap();

        let mut hot = HotList::new();
        insert_stale(&mut hot, &g, STALE_WINDOW, now);
        insert_high_risk(&mut hot, &g, HIGH_RISK_WRITE_WINDOW, now);

        let stale_entry = hot
            .iter()
            .find(|e| e.node == c1_id)
            .expect("stale node must be on the hot list");
        assert_eq!(stale_entry.condition, Condition::StaleSession);
        match &stale_entry.payload {
            HotListPayload::Stale { seconds_inactive } => assert_eq!(*seconds_inactive, 7200),
            other => panic!("expected Stale payload, got {other:?}"),
        }

        let hr_entry = hot
            .iter()
            .find(|e| e.node == c2_id)
            .expect("high-risk node must be on the hot list");
        assert_eq!(hr_entry.condition, Condition::HighRiskModification);
        match &hr_entry.payload {
            HotListPayload::HighRisk { reason } => assert!(reason.contains("Canonical")),
            other => panic!("expected HighRisk payload, got {other:?}"),
        }

        // Re-running with the same graph refreshes, never duplicates.
        insert_stale(&mut hot, &g, STALE_WINDOW, now);
        insert_high_risk(&mut hot, &g, HIGH_RISK_WRITE_WINDOW, now);
        assert_eq!(hot.len(), 2, "refresh must not duplicate entries");

        // Re-validation: touch c1 (fresh edge write) and age out c2's write
        // (remove the fresh edge) -> both predicates stop holding.
        g.upsert_edge(edge(2, cw_id, c1_id, 7200)).unwrap(); // activity = now
        g.remove_edge(edge(1, cw_id, c2_id, 7190).id).unwrap();
        assert!(
            !hot.revalidate(&g, c1_id),
            "touched node is no longer stale"
        );
        assert!(
            !hot.revalidate(&g, c2_id),
            "write aged out -> no longer high-risk"
        );
        assert!(!hot.contains(c1_id));
        assert!(!hot.contains(c2_id));
    }
}

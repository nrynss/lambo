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
//!   * **Stale** — the **session** whose most recent activity (any concept's
//!     creation or access, any edge write) is older than [`STALE_WINDOW`]. The
//!     window matches GC's `GC_EDGE_TTL` (1h) so "untouched for this long"
//!     means the same thing to GC and to staleness. The T2/T3 writers never set
//!     `last_accessed`, so activity is currently `max(created_at, edge
//!     writes)`; once recall (T5.x) stamps `last_accessed`, accesses join the
//!     max. Scope is **per session, not per concept** (CONC-2/ALGO-8): spec §9
//!     names the condition "stale session", one event fires per transition into
//!     staleness, and the event's `node_id` is the session's activity anchor
//!     ([`session_stale_at`]). Per-concept staleness emitted one event per
//!     concept in a single warm-up burst — 22 on the shipped fixture, thousands
//!     at scale — wrapping the same cycle's `Conflict` out of the ring.
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
//! ## Every publisher counts ([`EventSender`], NEW-3)
//!
//! The channel handle is [`EventSender`], not a bare `broadcast::Sender`: it
//! pairs the sender with a **shared publication counter**. The loop's re-arm
//! detector (CONC-2) decides an event has been evicted from the ring by
//! comparing that counter against the stamp it recorded at the event's
//! publication, so a publisher that advanced the ring **without** advancing the
//! counter would make a held condition invisible to re-arm — permanently, which
//! is the exact failure CONC-2 exists to rule out. `event_sender()` used to hand
//! out a raw `Sender` clone, so 300 external `Canonized` sends could evict a
//! held `Conflict` for good (probe: 601 cycles, 0 `Conflict` deliveries).
//! [`EventSender::send`] increments then sends, so the counter can only ever run
//! *ahead* of the ring — re-arm may fire one event early (a harmless duplicate),
//! never too late.
//!
//! ## P8 seam
//!
//! Spec §6.1's `mem.events() -> Receiver<DaemonEvent>` delegates to
//! [`crate::daemon::Daemon::events`] — one channel per daemon, same sender.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::daemon::conflict::ConflictHit;
use crate::daemon::drift::{DriftHit, DRIFT_HOPS_NO_PATH_EVENT};
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

/// The §6.1 channel's send half: the broadcast sender plus the **publication
/// counter every publisher shares** (NEW-3 — see the module docs for why a bare
/// `Sender` clone breaks CONC-2's re-arm).
///
/// Cheap to clone (two `Arc`s) and the supported multi-producer pattern: every
/// clone feeds the same ring *and* the same counter, so `Canonized` events from
/// P6 reach the same [`crate::daemon::Daemon::events`] subscribers as the
/// daemon's own detector events and are equally visible to re-arm. The daemon
/// retains its own handle, so a dropped clone never closes the channel.
#[derive(Clone, Debug)]
pub struct EventSender {
    tx: Arc<broadcast::Sender<DaemonEvent>>,
    emitted: Arc<AtomicU64>,
}

impl EventSender {
    /// Publish one event, fire-and-forget (spec §6.1), and return its **1-based
    /// publication index** on the channel — CONC-2's re-arm stamp.
    ///
    /// Infallible by construction: `broadcast::Sender::send` errors only when
    /// **zero** receivers exist — the dropped-receiver case, which §6.1 says is
    /// not an error — and lagged receivers never make it fail (they skip the
    /// value). So this never blocks, never panics, and never reports failure;
    /// any caller may publish without coordinating with consumers.
    ///
    /// The counter is incremented **before** the send: the count may briefly run
    /// ahead of the ring, never behind it, so re-arm can only be early.
    pub fn send(&self, event: DaemonEvent) -> u64 {
        let stamp = self.emitted.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.tx.send(event);
        stamp
    }

    /// Events published on this channel by **every** publisher (NEW-3). The
    /// loop's re-arm detector measures ring eviction against this.
    pub fn emitted_total(&self) -> u64 {
        self.emitted.load(Ordering::Acquire)
    }

    /// A receiver seeing everything published after this call (spec §6.1).
    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.tx.subscribe()
    }

    /// Live receiver count (tests; P8 diagnostics).
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// A fresh event channel at [`EVENT_CAPACITY`].
pub fn event_channel() -> (EventSender, broadcast::Receiver<DaemonEvent>) {
    event_channel_with_capacity(EVENT_CAPACITY)
}

/// An event channel with an explicit capacity (tests; the slow-consumer
/// contract lives at small capacities).
pub(crate) fn event_channel_with_capacity(
    capacity: usize,
) -> (EventSender, broadcast::Receiver<DaemonEvent>) {
    assert!(capacity > 0, "broadcast capacity must be > 0");
    let (tx, rx) = broadcast::channel(capacity);
    (
        EventSender {
            tx: Arc::new(tx),
            emitted: Arc::new(AtomicU64::new(0)),
        },
        rx,
    )
}

/// Publish one event, fire-and-forget (spec §6.1); returns the re-arm stamp.
///
/// Free-function form of [`EventSender::send`], kept because the loop and the
/// mappers below read as `events::emit(sender, events::conflict_event(hit))`.
pub fn emit(sender: &EventSender, event: DaemonEvent) -> u64 {
    sender.send(event)
}

/// The P6 seam: publish a canonization transition. Canonization (P6) calls
/// this at every transition; the daemon loop does not invent `Canonized`
/// events.
///
/// P6 gets the [`EventSender`] from [`crate::daemon::Daemon::event_sender`]
/// (XP-4) — before that accessor existed this helper had no reachable caller,
/// which is what the `#[allow(dead_code)]` it used to carry was really saying.
/// Taking the wrapper rather than a raw `broadcast::Sender` is NEW-3: P6's sends
/// advance the same ring the daemon's re-arm reasons about, so they must advance
/// the same counter.
pub fn emit_canonized(sender: &EventSender, event: CanonizationEvent) {
    emit(sender, DaemonEvent::Canonized { event });
}

/// Map a conflict hit to its broadcast event.
///
/// `detail` names the **writer** of the newest qualifying write, which is the
/// subject of spec §13's sentence, and then the full contesting set (ALGO-2):
/// the writer cannot be recovered from `agents`, and picking one of them is
/// wrong on the shipped fixture.
pub fn conflict_event(hit: &ConflictHit) -> DaemonEvent {
    DaemonEvent::Conflict {
        node_id: hit.node,
        agents: hit.agents.clone(),
        detail: format!(
            "{} wrote to it {}s ago; agents {:?} hold edges",
            hit.writer, hit.seconds_ago, hit.agents
        ),
    }
}

/// Map a drift hit to its broadcast event. [`DriftHit::detail`] already
/// renders "concept X is N hops from root goal Y (threshold T)", or the no-path
/// sentence.
///
/// §6.1's `hops: u32` is frozen and has no "unreachable" encoding, so a no-path
/// hit (ALGO-5) reports [`DRIFT_HOPS_NO_PATH_EVENT`] — `4294967295`, which is
/// also what a `Serialize`d event puts on the wire. The `u64`-shaped hot-list
/// payload carries the same number as
/// [`crate::daemon::drift::DRIFT_HOPS_NO_PATH`] (NEW-5: the two used to
/// disagree — this site hardcoded `u32::MAX` while the constant it pointed at was
/// `u64::MAX`). Consumers render `detail`, which says so in words.
pub fn drift_event(hit: &DriftHit) -> DaemonEvent {
    DaemonEvent::Drift {
        node_id: hit.node,
        hops: hit.hops.map_or(DRIFT_HOPS_NO_PATH_EVENT, |h| h as u32),
        detail: hit.detail.clone(),
    }
}

/// Build a `Stale` event from a detector hit. `node_id` is the session's
/// activity anchor — the concept whose write is the session's most recent
/// (spec §9 "stale session"; CONC-2).
pub fn stale_event(node_id: NodeId, seconds_inactive: u64) -> DaemonEvent {
    DaemonEvent::Stale {
        node_id,
        detail: format!(
            "session idle — newest activity at node {node_id}, untouched for {seconds_inactive}s"
        ),
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

/// The session's staleness (spec §6.1 `Stale`, §9 "stale **session**"). Pure
/// data; the loop turns it into a `DaemonEvent::Stale` and a hot-list entry.
///
/// Session-scoped, not concept-scoped (CONC-2/ALGO-8): `node` is the *anchor* —
/// the concept carrying the session's most recent activity, i.e. the one whose
/// write is the reason the session is only this stale. See
/// [`session_stale_at`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleHit {
    /// The concept carrying the session's most recent activity.
    pub node: NodeId,
    /// How long the session's most recent activity is in the past, in seconds.
    pub seconds_inactive: u64,
}

/// `window` as a `chrono` duration, saturating instead of panicking on a
/// window outside chrono's range (config-reachable; CONC-4).
fn chrono_window(window: Duration) -> ChronoDuration {
    ChronoDuration::from_std(window).unwrap_or(ChronoDuration::MAX)
}

/// The session's most recent activity at or before `now`, as
/// `(anchor concept, instant)`.
///
/// One linear pass over concepts and edges — `O(nodes + edges)`, not
/// `O(nodes × degree)`: an edge write counts as activity for both endpoints, so
/// folding the edge set once is equivalent to per-node `last_activity` folds and
/// strictly cheaper. Future-dated writes (mocked clocks) are ignored, matching
/// every other detector. The anchor breaks ties by smallest id, so the result is
/// deterministic.
///
/// `None` for a session with no concept whose activity is at or before `now`.
fn session_last_activity(graph: &Graph, now: DateTime<Utc>) -> Option<(NodeId, DateTime<Utc>)> {
    let mut best: Option<(NodeId, DateTime<Utc>)> = None;
    let mut consider = |node: NodeId, at: DateTime<Utc>| {
        if at > now {
            return;
        }
        let better = match &best {
            None => true,
            Some((id, t)) => at > *t || (at == *t && node.0 < id.0),
        };
        if better {
            best = Some((node, at));
        }
    };
    for c in graph.concepts() {
        consider(c.id, c.last_accessed.unwrap_or(c.created_at));
    }
    for e in graph.edges() {
        let w = e.last_reinforced.max(e.created_at);
        // Only concept endpoints anchor a *concept* activity claim; an edge from
        // an interaction still refreshes the concept it lands on.
        for endpoint in [e.source, e.target] {
            if matches!(graph.node(endpoint), Some(crate::types::Node::Concept(_))) {
                consider(endpoint, w);
            }
        }
    }
    best
}

/// Is the **session** stale at `now` (spec §9 "stale session")?
///
/// The session is stale when *nothing in it* has been touched for longer than
/// `window` — i.e. its most recent activity ([`session_last_activity`]) is
/// outside the window. One hit for the whole session, anchored on the concept
/// carrying that most recent activity.
///
/// Session scope is CONC-2/ALGO-8's fix. Per-concept staleness made warm-up on
/// a resumed session (spec §2.5 — *every* restart) emit one `Stale` per concept
/// in a single synchronous burst: 22 on the shipped fixture, ~4,000 at scale,
/// into a 256-slot ring, wrapping the same cycle's `Conflict` out before any
/// consumer could drain it. It also read the condition wrong: a session with one
/// fresh write and a hundred old concepts is *not* a stale session, yet it fired
/// a hundred Stale events.
pub fn session_stale_at(graph: &Graph, window: Duration, now: DateTime<Utc>) -> Option<StaleHit> {
    let window_start = now - chrono_window(window);
    let (node, last) = session_last_activity(graph, now)?;
    if last >= window_start {
        return None;
    }
    Some(StaleHit {
        node,
        seconds_inactive: (now - last).num_seconds().max(0) as u64,
    })
}

/// The session's staleness as a hit list: at most **one** hit
/// ([`session_stale_at`]). Kept vector-shaped so the loop treats all four
/// detectors uniformly.
pub fn detect_stale(graph: &Graph, window: Duration, now: DateTime<Utc>) -> Vec<StaleHit> {
    session_stale_at(graph, window, now).into_iter().collect()
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

/// Is a fresh write to this one high-value node in flight at `now`? The
/// per-node primitive (CONC-5), O(degree).
pub fn high_risk_at(
    graph: &Graph,
    node: NodeId,
    window: Duration,
    now: DateTime<Utc>,
) -> Option<HighRiskHit> {
    let window_start = now - chrono_window(window);
    let c = match graph.node(node) {
        Some(crate::types::Node::Concept(c)) => c,
        _ => return None,
    };
    if !is_high_value(c) {
        return None;
    }
    let own_write = c.created_at >= window_start && c.created_at <= now;
    let edge_write = graph.incident_edges(node).iter().any(|e| {
        let w = e.last_reinforced.max(e.created_at);
        w >= window_start && w <= now
    });
    if !(own_write || edge_write) {
        return None;
    }
    let status = format!("{:?}", c.canonization_status);
    let radius = c
        .blast_radius
        .map(|b| format!(", blast radius {b}"))
        .unwrap_or_default();
    Some(HighRiskHit {
        node,
        reason: format!(
            "high-value node {} ({status}{radius}) modified within {}s",
            c.id,
            window.as_secs()
        ),
    })
}

/// Detect every high-risk modification: a fresh write to a high-value node
/// (see the module docs for the v0.1 rule). Pure; deterministic
/// (id-ascending). Future-dated writes do not count.
pub fn detect_high_risk(graph: &Graph, window: Duration, now: DateTime<Utc>) -> Vec<HighRiskHit> {
    let mut hits: Vec<HighRiskHit> = graph
        .concepts()
        .filter_map(|c| high_risk_at(graph, c.id, window, now))
        .collect();
    hits.sort_by_key(|h| h.node.0);
    hits
}

/// Detect session staleness and refresh the hot list with **one**
/// `StaleSession` entry (CONC-2: one per session, not one per concept).
/// Returns the hit list: the daemon loop publishes it on transition and syncs
/// the hot list against it.
///
/// Re-insertion refreshes the payload + re-validation predicate; the T4.6 loop
/// additionally drops entries whose `(node, condition)` is no longer detected
/// ([`crate::daemon::hotlist::HotList::retain_conditions`]), so a session
/// touched again is evicted on the next cycle. A write that lands *and* moves
/// the anchor evicts the old entry the same way — the pair changed.
pub fn insert_stale(
    hot: &mut HotList,
    graph: &Graph,
    window: Duration,
    now: DateTime<Utc>,
) -> Vec<StaleHit> {
    let hits = detect_stale(graph, window, now);
    for hit in &hits {
        // Session-level re-check against the caller's `now`, returning the
        // refreshed payload (XP-3). The entry's node is the anchor; a
        // re-validation that finds a *different* anchor means a write landed,
        // which is exactly when the session stopped being stale.
        let anchor = hit.node;
        let holds = move |g: &Graph, at: DateTime<Utc>| {
            session_stale_at(g, window, at)
                .filter(|h| h.node == anchor)
                .map(|h| HotListPayload::Stale {
                    seconds_inactive: h.seconds_inactive,
                })
        };
        let _ = hot.insert(HotListEntry::new(
            anchor,
            Condition::StaleSession,
            HotListPayload::Stale {
                seconds_inactive: hit.seconds_inactive,
            },
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
        // Per-node re-check against the caller's `now` (CONC-5 / XP-3): the 30s
        // window is evaluated at read time, so an elapsed HighRisk drops out of
        // a recall instead of re-validating true against a frozen instant.
        let holds = move |g: &Graph, at: DateTime<Utc>| {
            high_risk_at(g, node, window, at).map(|h| HotListPayload::HighRisk { reason: h.reason })
        };
        let _ = hot.insert(HotListEntry::new(
            node,
            Condition::HighRiskModification,
            HotListPayload::HighRisk {
                reason: hit.reason.clone(),
            },
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

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
    async fn slow_receiver_gets_lagged_not_a_hang() {
        // Capacity 2; the receiver never drains. Every send succeeds (never
        // blocks, never errors while a receiver exists); the oldest values
        // are dropped and the receiver eventually observes Lagged — the
        // spec §6.1 contract for a consumer that cannot keep up.
        //
        // `EventSender::send` has no failure mode at all (NEW-3 made the §6.1
        // "a dropped receiver is not an error" contract structural), so what is
        // asserted here is that each send *happened* and was counted: the
        // returned stamps are 1..=4 in order.
        let (tx, mut rx) = event_channel_with_capacity(2);
        for i in 0..4 {
            let stamp = tx.send(DaemonEvent::Stale {
                node_id: nid(1, i),
                detail: format!("{i}"),
            });
            assert_eq!(stamp, i + 1, "every send is counted, in order");
        }
        assert_eq!(tx.emitted_total(), 4);

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
            writer: AgentId::from("agent-b"),
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
            goal: Some(nid(1, 3)),
            hops: Some(6),
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

    /// CONC-2/ALGO-8: staleness is a property of the **session**, not of each
    /// concept. One fresh write anywhere keeps the session live, no matter how
    /// many old concepts it carries; when the session does go stale, exactly
    /// one hit fires, anchored on the newest activity.
    ///
    /// Pre-fix this graph produced two `Stale` hits (c1 and c3) while the
    /// session had been written to 100s ago — the wrong condition, and the
    /// per-concept burst that wrapped the demo `Conflict` out of the ring.
    #[test]
    fn detect_stale_is_a_session_property_not_a_per_concept_one() {
        let now = ts(7200);
        let (mut g, i1_id, i2_id) = base_graph();
        // c1: created at 0, accessed at 400 — old, but not the session's story.
        let mut c1 = concept(
            1,
            i1_id,
            "agent-a",
            "old one",
            0,
            CanonizationStatus::None,
            None,
        );
        c1.last_accessed = Some(ts(400));
        g.insert_concept(c1, i1_id).unwrap();
        // c2: created at 7000 (200s ago) — the session is alive.
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
        // c3: created at 0, edge written at 7100 (100s ago) — the newest
        // activity in the session, so the anchor if it ever goes stale.
        let c3 = concept(
            3,
            i1_id,
            "agent-a",
            "edge-refreshed one",
            0,
            CanonizationStatus::None,
            None,
        );
        let c3_id = c3.id;
        g.insert_concept(c3, i1_id).unwrap();
        g.upsert_edge(edge(1, c2_id, c3_id, 7100)).unwrap();

        assert!(
            detect_stale(&g, STALE_WINDOW, now).is_empty(),
            "a session written to 100s ago is not stale, whatever its oldest concept says"
        );

        // Two hours after that last write the whole session is stale: exactly
        // one hit, anchored on the newest activity — the `c2 -> c3` edge at
        // 7100 refreshes both endpoints, so the tie-break (smallest id) picks
        // c2 over c3.
        let much_later = ts(7100 + 7200);
        let hits = detect_stale(&g, STALE_WINDOW, much_later);
        assert_eq!(
            hits.len(),
            1,
            "one hit per session, not per concept: {hits:?}"
        );
        assert_eq!(hits[0].node, c2_id, "anchor = newest activity, ties by id");
        assert_eq!(hits[0].seconds_inactive, 7200);
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

    /// A graph with a Canonical `c2`, a writer `cw`, and one `cw -> c2`
    /// Dependency edge at `t=7190` — the session's newest activity. Returns
    /// `(graph, c2, cw)`.
    ///
    /// Session-stale and high-risk are mutually exclusive by construction here
    /// (CONC-2): a write fresh enough to be high-risk (30s) is far too fresh to
    /// leave the session stale (1h), so the two conditions are exercised at
    /// different `now`s rather than side by side.
    fn canonical_write_graph() -> (Graph, NodeId, NodeId) {
        let (mut g, i1_id, _) = base_graph();
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
        (g, c2_id, cw_id)
    }

    #[test]
    fn hot_list_entries_refresh_and_revalidate() {
        let now = ts(7200);
        let (mut g, c2_id, cw_id) = canonical_write_graph();

        let mut hot = HotList::new();
        insert_high_risk(&mut hot, &g, HIGH_RISK_WRITE_WINDOW, now);

        let hr_entry = hot
            .iter()
            .find(|e| e.node == c2_id)
            .expect("high-risk node must be on the hot list");
        assert_eq!(hr_entry.condition, Condition::HighRiskModification);
        match &hr_entry.payload {
            HotListPayload::HighRisk { reason } => assert!(reason.contains("Canonical")),
            other => panic!("expected HighRisk payload, got {other:?}"),
        }

        // Two hours on, the session is stale: exactly one entry, anchored on
        // the newest activity (c2 and cw tie at 7190; smallest id wins).
        let stale_now = ts(7190 + 7200);
        assert_eq!(insert_stale(&mut hot, &g, STALE_WINDOW, stale_now).len(), 1);
        let stale_entry = hot
            .iter()
            .find(|e| e.condition == Condition::StaleSession)
            .expect("the stale session must be on the hot list");
        assert_eq!(stale_entry.node, c2_id, "anchor = newest activity");
        match &stale_entry.payload {
            HotListPayload::Stale { seconds_inactive } => assert_eq!(*seconds_inactive, 7200),
            other => panic!("expected Stale payload, got {other:?}"),
        }

        // Re-running with the same graph refreshes, never duplicates.
        insert_stale(&mut hot, &g, STALE_WINDOW, stale_now);
        insert_high_risk(&mut hot, &g, HIGH_RISK_WRITE_WINDOW, now);
        assert_eq!(hot.len(), 2, "refresh must not duplicate entries");

        // Re-validation: a fresh write revives the session and ages c2's
        // high-risk window out, so both predicates stop holding.
        g.upsert_edge(edge(2, cw_id, c2_id, 7190 + 7200)).unwrap();
        assert!(
            !hot.revalidate(&g, c2_id, ts(7190 + 7200 + 60)),
            "a fresh write ends both the stale session and the old high-risk window"
        );
        assert!(hot.is_empty());
    }

    // ------------------------------------------------------------------
    // XP-3 / CONC-5 — read-time re-validation of the clock-bound conditions
    // ------------------------------------------------------------------

    /// XP-3: a `HighRisk` entry must age out on the **clock alone**, and a
    /// `Stale` entry's `seconds_inactive` must track read time.
    ///
    /// Pre-fix both predicates captured the detection `now` by move and re-ran
    /// `detect_high_risk`/`detect_stale` against it, so the 30s window was
    /// re-derived from a frozen instant: the entry re-validated `true` forever
    /// against an unchanged graph and served the detection-time payload.
    #[test]
    fn revalidate_ages_clock_bound_conditions_out_without_touching_the_graph() {
        let detected_at = ts(7200);
        let (g, c2_id, _) = canonical_write_graph();

        // c2's only fresh write is at 7190 — 10s before detection, inside the
        // 30s high-risk window.
        let mut hot = HotList::new();
        insert_high_risk(&mut hot, &g, HIGH_RISK_WRITE_WINDOW, detected_at);
        assert!(hot.contains(c2_id));

        // 100s later, graph untouched: the write left the 30s window.
        assert!(
            !hot.revalidate(&g, c2_id, ts(7300)),
            "the clock alone must age a high-risk write out"
        );
        assert!(!hot.contains(c2_id), "no ghost HighRisk entry");

        // Same for the stale session, in the other direction: once stale it
        // stays stale, but `seconds_inactive` tracks read time.
        let stale_now = ts(7190 + 7200);
        insert_stale(&mut hot, &g, STALE_WINDOW, stale_now);
        assert!(hot.revalidate(&g, c2_id, ts(7190 + 7300)));
        let refreshed = hot.iter().find(|e| e.node == c2_id).unwrap();
        match &refreshed.payload {
            HotListPayload::Stale { seconds_inactive } => assert_eq!(
                *seconds_inactive, 7300,
                "seconds_inactive must be recomputed at read time, not frozen at 7200"
            ),
            other => panic!("expected Stale payload, got {other:?}"),
        }
    }

    /// CONC-5: the per-node primitives are the single source of truth — they
    /// must agree with the whole-graph passes on every concept, so swapping the
    /// predicates' `detect_*` calls for them cannot change what recall sees.
    #[test]
    fn per_node_primitives_agree_with_the_whole_graph_passes() {
        let now = ts(7200);
        let (mut g, i1_id, i2_id) = base_graph();
        let mut c1 = concept(
            1,
            i1_id,
            "agent-a",
            "old one",
            0,
            CanonizationStatus::None,
            None,
        );
        c1.last_accessed = Some(ts(400));
        g.insert_concept(c1, i1_id).unwrap();
        let c2 = concept(
            2,
            i2_id,
            "agent-b",
            "fresh canonical",
            7000,
            CanonizationStatus::Canonical,
            Some(8),
        );
        let c2_id = c2.id;
        g.insert_concept(c2, i2_id).unwrap();
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
        g.upsert_edge(edge(1, c2_id, c3_id, 7190)).unwrap();

        let high_risk: Vec<HighRiskHit> = detect_high_risk(&g, HIGH_RISK_WRITE_WINDOW, now);
        for c in g.concepts() {
            assert_eq!(
                high_risk_at(&g, c.id, HIGH_RISK_WRITE_WINDOW, now).as_ref(),
                high_risk.iter().find(|h| h.node == c.id),
                "high_risk_at must agree with detect_high_risk for {}",
                c.id
            );
        }
    }
}

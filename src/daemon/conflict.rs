//! Conflict detection — spec §9, P4 T4.3 (the demo trigger).
//!
//! A conflict is: **two or more active agents with edges to the same node, at
//! least one `Causal`/`Dependency` edge with write activity inside the
//! `conflict_recency_window`** (30s by default; passed in — the function takes
//! `now`, so tests and fixtures mock the clock by simply passing a `now` of
//! their choosing).
//!
//! ## Agent attribution ("who wrote this edge")
//!
//! Edges carry no agent id (the §5 `Edge` shape is frozen), so the writer is
//! resolved — see `edge_writers`: an interaction-sourced edge belongs to that
//! interaction's agent; a concept→concept edge belongs to the agent **acting at
//! the edge's write timestamp**, falling back to the source concept's
//! `origin_agent` when no interaction was written at that instant.
//!
//! Resolving by write time rather than by the source concept's origin is
//! ALGO-3's fix: `record_action` reuses an already-canonical concept as the
//! source of a new edge, so origin-based attribution credits the concept's
//! original author instead of the acting agent — collapsing the demo's
//! two-agent set to one and silently suppressing the conflict.
//!
//! "Agent X has an edge to node N" == X is the resolved writer of at least one
//! edge incident to N (N as source or as target).
//!
//! ### Same-instant collisions (NEW-4)
//!
//! When several interactions share one instant, "the agent acting then" is not a
//! single answer, so **every** candidate joins the contested node's agent set and
//! [`ConflictHit::writer`] names one of them deterministically (smallest
//! interaction id at that instant). Keeping only a tie-break winner — the earlier
//! rule — reproduced exactly the bug ALGO-3 fixed: the agent set collapsed to one
//! agent and the conflict silently did not fire, while an unrelated node could
//! pick up a spurious conflict naming the wrong writer. Erring toward detection
//! is deliberate; see `ConflictHit::writer` for the residual ambiguity.
//!
//! **J1 made this path non-degenerate, and it is the one place in this module
//! whose *accuracy* changed** (J1-R1-8). Before per-call `agent_id`, every
//! interaction in a `serve` carried the process's own `--agent`, so a same-instant
//! tie collapsed to one agent whichever rule was applied and the residual
//! ambiguity could not be observed. Two clients writing through one `serve` in the
//! same instant are now genuinely different agents, so this rule starts producing
//! real multi-agent sets, and `writer` — "smallest interaction id at that instant"
//! — becomes a deterministic choice between two *live* agents rather than a
//! formality. The behaviour is unchanged and still correct for its purpose
//! (detection over precision, since the §13 sentence's job is to make the reader
//! look): what changed is that the sentence can now name the wrong one of two real
//! agents. J2 and J3 interleave far harder than J1 does, so J3's Done-when carries
//! this as something to measure rather than assume.
//!
//! ## "Active agent"
//!
//! Spec §9 requires "two or more active agents with edges to the same node" but
//! gives no quantitative bound on "active". Interpreted minimally: an agent is
//! **active** when it has an edge to the node (any age) — the recency
//! dimension of a conflict lives in the *write-activity* clause below, not in
//! the agent set. This is the only reading consistent with the planted conflict
//! in `fixtures/session-rest-api.json` (the caching layer: agent-a's `Derives`
//! edge is old, agent-b's `Dependency` edges are fresh — the conflict must
//! still fire) and with [`crate::fixtures::load_store_relative`]'s documented
//! contract ("Makes the P4 conflict / recency window runnable").
//!
//! ## Write activity and the window
//!
//! An edge's latest write is `max(created_at, last_reinforced)` (creation or a
//! duplicate natural-key reinforcement). Write activity falls **inside** the
//! window iff that time is in `[now - window, now]` — inclusive: a write
//! exactly `window` ago or exactly at `now` counts.
//!
//! ## Future-dated edges (mocked `now`)
//!
//! Because `detect` takes `now` explicitly, a fixture or mock may run `now`
//! *earlier* than some edge timestamps. Edges written after `now` are treated
//! as **outside the window**: they have not happened yet at the instant being
//! examined, so they never count as write activity and never move
//! `seconds_ago`. They still count for agent attribution (the agent
//! demonstrably holds an edge to the node).
//!
//! ## `seconds_ago` and `writer`
//!
//! The age, in whole seconds (truncated), of the most recent qualifying write
//! — the latest `Causal`/`Dependency` write inside the window — together with
//! the agent that made *that* write ([`ConflictHit::writer`], ALGO-2). T5.3
//! renders "Agent A wrote to it eleven seconds ago" from `writer` +
//! `seconds_ago`; `agents` is the full contesting set. Deriving the subject
//! from `agents` alone is not possible and guessing is wrong on the shipped
//! fixture, where the newest write is agent-b's and the naive first-listed
//! guess names agent-a.
//!
//! ## Hot list
//!
//! [`insert_conflicts`] refreshes one entry per hit. The entry's re-validation
//! predicate re-runs [`conflict_at`] against the **caller's** `now` and returns
//! the payload it computed, so a recall five minutes later either drops the
//! entry (the write left the window) or renders the true age — never the frozen
//! detection-time value (XP-3). [`HotList`] dedups by `(node, condition)`, so
//! re-running the detector refreshes an existing conflict entry instead of
//! duplicating it. The hits are returned: the daemon loop (T4.6) publishes them
//! on transition and syncs the hot list against them with
//! [`HotList::retain_conditions`], so a conflict that ages out of the recency
//! window is dropped on the next cycle too.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::daemon::hotlist::{Condition, HotList, HotListEntry, HotListPayload};
use crate::graph::Graph;
use crate::types::{AgentId, Edge, EdgeType, Node, NodeId};

/// Spec §9 `conflict_recency_window`; mirrors [`crate::config::Config`]'s
/// default (30s). `Config` drives the daemon's construction; this const is the
/// module-level default for callers that do not read config.
pub const CONFLICT_RECENCY_WINDOW: Duration = Duration::from_secs(30);

/// One detected conflict (spec §9): a node that ≥2 active agents have edges
/// to, with a recent `Causal`/`Dependency` write.
///
/// This is the daemon detector's own type (pure data); T4.6 maps it to
/// `DaemonEvent::Conflict` and recall (T5.3) renders the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictHit {
    /// The contested node.
    pub node: NodeId,
    /// Agents with edges to the node, sorted by id (deterministic).
    pub agents: Vec<AgentId>,
    /// The agent that made the most recent qualifying write (ALGO-2) — the
    /// subject of spec §13's "Agent A wrote to it eleven seconds ago".
    ///
    /// **Residual ambiguity (NEW-4).** When two or more interactions from
    /// different agents share the newest qualifying write's instant, the edge
    /// carries no evidence of which one wrote it (edges have no agent id — the
    /// §5 shape is frozen). This field is then *one of* the candidates, picked
    /// deterministically: newest qualifying write, then smallest interaction id
    /// at that instant. Every candidate is in [`ConflictHit::agents`], so the
    /// contested set is complete even when the named writer is a coin toss;
    /// `agents.len()` exceeding the number of agents that truly touched the node
    /// is the deliberate direction of the error (a spurious advisory warning
    /// beats a silently missed conflict). Same-instant cross-agent interactions
    /// do not occur on the shipped fixture (15-minute granularity).
    pub writer: AgentId,
    /// Age of `writer`'s write, in seconds, at the `now` this was computed for.
    pub seconds_ago: u64,
}

/// The session's interactions indexed by write timestamp, for resolving which
/// **agent was acting** when an edge was written (ALGO-3).
///
/// One `O(interactions)` build per pass, `O(1)` per edge: a whole-graph
/// [`detect`] builds it once and shares it across every node, and a per-node
/// [`conflict_at`] (recall's path, CONC-5) builds it once for that node.
struct WriterTimeline {
    /// `created_at -> every (interaction id, agent) written at that instant`,
    /// id-ascending.
    ///
    /// **All** candidates, not the smallest id (NEW-4). Collapsing an instant to
    /// one interaction discards the only evidence that the attribution is
    /// ambiguous, and under same-instant cross-agent interactions that produced
    /// both failure modes: a false negative on the genuinely contested node (its
    /// agent set collapsed to one agent, so no conflict fired) and a spurious
    /// conflict naming the wrong writer.
    by_time: HashMap<DateTime<Utc>, Vec<(NodeId, AgentId)>>,
}

impl WriterTimeline {
    fn of(graph: &Graph) -> Self {
        let mut by_time: HashMap<DateTime<Utc>, Vec<(NodeId, AgentId)>> = HashMap::new();
        for i in graph.interactions() {
            by_time
                .entry(i.created_at)
                .or_default()
                .push((i.id, i.agent_id.clone()));
        }
        // Id-ascending, so `EdgeWriters::named` is deterministic.
        for candidates in by_time.values_mut() {
            candidates.sort_by_key(|(id, _)| id.0);
        }
        Self { by_time }
    }

    /// Every interaction written at exactly `at`, id-ascending. Empty when none
    /// was.
    ///
    /// Exact-match only, deliberately. Every production write path stamps an
    /// edge with the timestamp of the interaction performing the write:
    /// `record_action` copies the interaction's `created_at` onto each
    /// `Causal`/`Dependency` edge verbatim, and `insert_concept` dates its
    /// `Derives` edge from the concept it is deriving. So an exact hit *is* the
    /// acting interaction, while a near-miss carries no attribution evidence at
    /// all — a hand-built or time-rebased edge could sit anywhere between two
    /// interactions, and guessing "the latest interaction at or before" would
    /// invent an author. Callers fall back to structural attribution instead.
    fn acting(&self, at: DateTime<Utc>) -> &[(NodeId, AgentId)] {
        self.by_time.get(&at).map_or(&[], Vec::as_slice)
    }
}

/// Who wrote an edge — one agent, or every same-instant candidate when the
/// timeline cannot separate them (NEW-4).
enum EdgeWriters<'a> {
    /// Exactly attributed: an interaction-sourced edge, or the structural
    /// fallback to the source concept's `origin_agent`.
    Certain(AgentId),
    /// The interactions acting at the edge's write instant, id-ascending and
    /// non-empty. One element is the ordinary case; more means two or more
    /// interactions share the instant and no evidence separates them.
    Acting(&'a [(NodeId, AgentId)]),
}

impl EdgeWriters<'_> {
    /// Every candidate writer. All of them join the contested node's agent set:
    /// "agent X has an edge to node N" is true of each, and dropping the ones a
    /// tie-break did not pick is what made a real conflict invisible (NEW-4).
    fn all(&self) -> impl Iterator<Item = &AgentId> {
        let (certain, acting): (&[AgentId], &[(NodeId, AgentId)]) = match self {
            Self::Certain(agent) => (std::slice::from_ref(agent), &[]),
            Self::Acting(candidates) => (&[], candidates),
        };
        certain.iter().chain(acting.iter().map(|(_, agent)| agent))
    }

    /// The single agent to *name* as the writer: the only candidate, or the
    /// smallest interaction id at a collided instant (the slice is id-ascending).
    fn named(&self) -> &AgentId {
        match self {
            Self::Certain(agent) => agent,
            // Non-empty by construction (`Acting` is only built from a hit).
            Self::Acting(candidates) => &candidates[0].1,
        }
    }
}

/// The agents that may have written `edge`.
///
/// Resolution order (ALGO-3):
///
/// 1. An edge whose **source is an `Interaction`** (`Derives`/`Temporal`) is
///    that interaction's — exact by construction, no inference needed.
/// 2. Otherwise the interactions **acting** at the edge's write timestamp
///    ([`WriterTimeline::acting`]) — usually exactly one.
/// 3. Otherwise the source concept's `origin_agent`.
///
/// Step 2 is the fix. Attributing a concept→concept edge to its source
/// concept's `origin_agent` — the old rule — credits whoever *first created
/// that concept*, not whoever is writing now. `record_action` resolves an
/// existing canonical concept as the source of a new `Causal`/`Dependency`
/// edge (spec §7), which is exactly the demo's shape: agent B records an
/// action against agent A's `user schema`, and the resulting edge was
/// attributed to agent A. Both edges then read as agent A's, the agent set
/// collapses to one, and the conflict silently does not fire.
///
/// `None` only for a source node missing from the graph (defensive;
/// `assert_invariants` guarantees every edge endpoint exists).
fn edge_writers<'a>(
    graph: &'a Graph,
    edge: &Edge,
    timeline: &'a WriterTimeline,
) -> Option<EdgeWriters<'a>> {
    match graph.node(edge.source) {
        Some(Node::Interaction(i)) => Some(EdgeWriters::Certain(i.agent_id.clone())),
        Some(Node::Concept(c)) => {
            let acting = timeline.acting(edge.last_reinforced.max(edge.created_at));
            Some(if acting.is_empty() {
                EdgeWriters::Certain(c.origin_agent.clone())
            } else {
                EdgeWriters::Acting(acting)
            })
        }
        _ => None,
    }
}

/// Pure conflict check for one node — the per-node primitive (CONC-5): shared
/// by [`detect`] and the hot-list re-validation predicates so both use
/// identical logic, and cheap enough for recall to call under the graph lock
/// (one neighborhood walk, not a whole-graph pass).
pub fn conflict_at(
    graph: &Graph,
    node: NodeId,
    window: Duration,
    now: DateTime<Utc>,
) -> Option<ConflictHit> {
    conflict_at_with(graph, node, window, now, &WriterTimeline::of(graph))
}

/// [`conflict_at`] against a pre-built timeline (whole-graph passes build one).
fn conflict_at_with(
    graph: &Graph,
    node: NodeId,
    window: Duration,
    now: DateTime<Utc>,
    timeline: &WriterTimeline,
) -> Option<ConflictHit> {
    let window_start = now - chrono_window(window);

    let mut agents: Vec<AgentId> = Vec::new();
    // The newest qualifying write and *who made it* travel together (ALGO-2).
    let mut newest: Option<(DateTime<Utc>, AgentId)> = None;

    for edge in graph.incident_edges(node) {
        let Some(writers) = edge_writers(graph, edge, timeline) else {
            continue;
        };
        // Every candidate joins the set (NEW-4) — see `EdgeWriters::all`.
        for agent in writers.all() {
            if !agents.contains(agent) {
                agents.push(agent.clone());
            }
        }

        // Write activity inside [now - window, now]; future-dated edges (the
        // edge's last write is after `now`) are outside the window.
        let last_write = edge.last_reinforced.max(edge.created_at);
        let qualifying = matches!(edge.edge_type, EdgeType::Causal | EdgeType::Dependency)
            && last_write <= now
            && last_write >= window_start;
        if qualifying && newest.as_ref().is_none_or(|(t, _)| last_write > *t) {
            newest = Some((last_write, writers.named().clone()));
        }
    }

    if agents.len() < 2 {
        return None;
    }
    let (last_write, writer) = newest?;

    agents.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let seconds_ago = (now - last_write).num_seconds().max(0) as u64;
    Some(ConflictHit {
        node,
        agents,
        writer,
        seconds_ago,
    })
}

/// `window` as a `chrono` duration. `Duration::MAX` seconds exceeds chrono's
/// range, so an unrepresentable window saturates rather than panicking
/// (config-reachable — see CONC-4's `from_std` sweep).
fn chrono_window(window: Duration) -> ChronoDuration {
    ChronoDuration::from_std(window).unwrap_or(ChronoDuration::MAX)
}

/// Detect every conflict in the graph (spec §9).
///
/// Pure: no locks, no I/O, no hot-list mutation. `now` is passed in so callers
/// (and tests) control the clock. Hits are returned in deterministic
/// node-id order.
pub fn detect(graph: &Graph, window: Duration, now: DateTime<Utc>) -> Vec<ConflictHit> {
    let timeline = WriterTimeline::of(graph);
    let mut hits: Vec<ConflictHit> = Vec::new();
    for node in graph
        .temporal_chain()
        .iter()
        .copied()
        .chain(graph.concepts().map(|c| c.id))
    {
        if let Some(hit) = conflict_at_with(graph, node, window, now, &timeline) {
            hits.push(hit);
        }
    }
    hits.sort_by_key(|h| h.node.0);
    hits
}

/// Detect conflicts and refresh the hot list with one entry per hit.
///
/// [`HotList::insert`] dedups by `(node, condition)`, so re-running this with a
/// fresh `now` refreshes the payload (`seconds_ago`) and the re-validation
/// predicate instead of duplicating (T4.2's contract). Each entry's predicate
/// re-checks the same conflict logic against the `now` **its caller** passes to
/// [`HotList::revalidate`] — never this cycle's frozen instant (XP-3).
///
/// Returns the hits — the daemon loop emits them on condition transition and
/// uses them as the fresh set it syncs the hot list against
/// ([`HotList::retain_conditions`]; T4.6 finding 2).
pub fn insert_conflicts(
    hot: &mut HotList,
    graph: &Graph,
    window: Duration,
    now: DateTime<Utc>,
) -> Vec<ConflictHit> {
    let hits = detect(graph, window, now);
    for hit in &hits {
        let node = hit.node;
        // The predicate re-runs the same per-node check against the *caller's*
        // `now` and hands back the payload it computed, so recall renders a
        // read-time `seconds_ago` and writer (XP-3 / ALGO-2). Only `node` and
        // `window` are captured — both `Copy` (spec §6.4: no lock re-entry).
        let holds =
            move |g: &Graph, at: DateTime<Utc>| conflict_at(g, node, window, at).map(payload_of);
        let _ = hot.insert(HotListEntry::new(
            node,
            Condition::Conflict,
            payload_of(hit.clone()),
            holds,
        ));
    }
    hits
}

/// The hot-list payload for a hit (one conversion, used at insert and at every
/// re-validation).
fn payload_of(hit: ConflictHit) -> HotListPayload {
    HotListPayload::Conflict {
        agents: hit.agents,
        writer: hit.writer,
        seconds_ago: hit.seconds_ago,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CanonizationStatus, Concept, ConceptType, Edge, Interaction, SessionId};
    use chrono::TimeZone;
    use uuid::Uuid;

    /// Whole-second timestamp helper (seconds since a fixed epoch).
    fn t(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + s, 0).unwrap()
    }

    fn sid() -> SessionId {
        SessionId::from("t4.3-conflict")
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
            created_at: t(at),
        }
    }

    fn concept(id: u64, origin: NodeId, agent: &str, content: &str, at: i64) -> Concept {
        Concept {
            id: nid(1, id),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.to_string(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from(agent),
            created_at: t(at),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        }
    }

    fn dep_edge(id: u64, source: NodeId, target: NodeId, at: i64) -> Edge {
        Edge {
            id: nid(3, id),
            session_id: sid(),
            source,
            target,
            edge_type: EdgeType::Dependency,
            weight: 0.8,
            reinforcements: 1,
            created_at: t(at),
            last_reinforced: t(at),
        }
    }

    /// A valid (invariant-clean) two-agent session:
    /// - i1 (agent-a) derives c1 "shared node"; c2/c3 (origin agent-b) derive
    ///   from i2 (agent-b).
    /// - Two `Dependency` edges `c2 -> c1` and `c3 -> c1`, both written at
    ///   `dep_at` — so c1 is the contested node and the *only* conflicted one
    ///   (c2/c3 each have edges from a single agent).
    ///
    /// Returns `(graph, c1 id, dependency-edge ids)`.
    fn two_agent_graph(c1_at: i64, dep_at: i64) -> (Graph, NodeId, Vec<NodeId>) {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, "agent-a", c1_at - 20);
        let i2 = interaction(2, Some(1), "agent-b", c1_at - 10);
        g.insert_interaction(i1.clone()).unwrap();
        g.insert_interaction(i2.clone()).unwrap();

        let c1 = concept(1, i1.id, "agent-a", "shared node", c1_at);
        let c1_id = c1.id;
        g.insert_concept(c1, i1.id).unwrap();
        let c2 = concept(2, i2.id, "agent-b", "writer b one", c1_at - 10);
        let c2_id = c2.id;
        g.insert_concept(c2, i2.id).unwrap();
        let c3 = concept(3, i2.id, "agent-b", "writer b two", c1_at - 10);
        let c3_id = c3.id;
        g.insert_concept(c3, i2.id).unwrap();

        let e1 = dep_edge(1, c2_id, c1_id, dep_at);
        let e2 = dep_edge(2, c3_id, c1_id, dep_at);
        let dep_ids = vec![e1.id, e2.id];
        g.upsert_edge(e1).unwrap();
        g.upsert_edge(e2).unwrap();
        (g, c1_id, dep_ids)
    }

    // ------------------------------------------------------------------
    // Planted conflict in the session-rest-api fixture (mocked now)
    // ------------------------------------------------------------------

    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn rest_api_planted_conflict_fires() {
        use crate::fixtures;
        use crate::store::GraphStore;

        // Rebase the fixture so the most recent concept write (the caching
        // layer, authored by agent-a) lands 5s before the anchor, then detect
        // with `now == anchor` and the spec's 30s window.
        let anchor = Utc::now();
        let store =
            fixtures::load_store_relative("session-rest-api", anchor, Duration::from_secs(5))
                .unwrap();
        let snap = store
            .load_session(&SessionId::from("session-rest-api"))
            .await
            .unwrap();
        let g = Graph::from_snapshot(snap).unwrap();

        let hits = detect(&g, CONFLICT_RECENCY_WINDOW, anchor);
        assert!(!hits.is_empty(), "planted conflict must fire");

        // The planted conflict: the caching layer. agent-b's fresh Dependency
        // edges (api layer, load testing) touch it; agent-a wrote it (Derives).
        let caching = NodeId("f0000000-0000-4000-8000-000000001010".parse().unwrap());
        let hit = hits
            .iter()
            .find(|h| h.node == caching)
            .expect("caching layer is the planted conflict");
        assert_eq!(
            hit.agents,
            vec![AgentId::from("agent-a"), AgentId::from("agent-b")]
        );
        assert_eq!(
            hit.seconds_ago, 5,
            "most recent qualifying write lands 5s before anchor"
        );

        // The demo pillar (user schema) also has two agents with edges — but
        // its most recent Dependency write is 20+ minutes before `now` after
        // this rebase, so it must NOT fire: only write activity inside the
        // window counts. The caching layer is the fixture's only in-window
        // conflict, matching fixtures.rs' `load_store_relative` contract.
        assert_eq!(
            hits.len(),
            1,
            "only the planted conflict may fire at this rebase: {hits:?}"
        );
        let schema = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
        assert!(
            hits.iter().all(|h| h.node != schema),
            "stale multi-agent writes are not a conflict"
        );
        // Every hit is a genuine conflict: ≥2 distinct agents each time.
        assert!(hits.iter().all(|h| h.agents.len() >= 2));

        // The user-id node is single-agent (only agent-a edges) and stale —
        // the fixture's own negative control.
        let user_id = NodeId("f0000000-0000-4000-8000-000000001013".parse().unwrap());
        assert!(
            hits.iter().all(|h| h.node != user_id),
            "single-agent node must not fire"
        );
    }

    // ------------------------------------------------------------------
    // Single-agent and stale-window negatives (synthetic, mocked now)
    // ------------------------------------------------------------------

    #[test]
    fn single_agent_does_not_fire() {
        let now = t(3600);
        // Everything that touches c1 is agent-a's: c1 and c2 both derive from
        // i1, so the Derives edges (i1 -> c1, i1 -> c2) and the Dependency
        // c2 -> c1 are all attributed to agent-a. i2 (agent-b) exists in the
        // session but has no edges to c1 — despite the fresh in-window writes
        // there is only one agent, so no conflict.
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, "agent-a", 3600 - 60);
        let i2 = interaction(2, Some(1), "agent-b", 3600 - 50);
        g.insert_interaction(i1.clone()).unwrap();
        g.insert_interaction(i2.clone()).unwrap();
        let c1 = concept(1, i1.id, "agent-a", "shared node", 3600 - 40);
        let c1_id = c1.id;
        g.insert_concept(c1, i1.id).unwrap();
        let c2 = concept(2, i1.id, "agent-a", "same agent", 3600 - 50);
        let c2_id = c2.id;
        g.insert_concept(c2, i1.id).unwrap();
        g.upsert_edge(dep_edge(1, c2_id, c1_id, 3600 - 11)).unwrap();

        assert!(
            g.assert_invariants().is_ok(),
            "test graph must be invariant-clean"
        );
        assert!(
            detect(&g, CONFLICT_RECENCY_WINDOW, now).is_empty(),
            "one agent with fresh writes is not a conflict"
        );
    }

    #[test]
    fn stale_window_does_not_fire() {
        let now = t(3600);
        // Two agents, but the only Causal/Dependency write is 31s ago —
        // outside the 30s window. The old Derives edge alone (40s ago) is not
        // a qualifying write either.
        let (g, _, _) = two_agent_graph(3600 - 40, 3600 - 31);
        assert!(
            detect(&g, CONFLICT_RECENCY_WINDOW, now).is_empty(),
            "write outside the window is not a conflict"
        );
    }

    #[test]
    fn window_boundaries_are_inclusive() {
        let now = t(3600);
        // A write exactly `window` ago still counts (inclusive lower bound).
        let (g, c1, _) = two_agent_graph(3600 - 40, 3600 - 30);
        let hits = detect(&g, CONFLICT_RECENCY_WINDOW, now);
        assert_eq!(hits.len(), 1, "exactly one contested node");
        assert_eq!(hits[0].node, c1);
        assert_eq!(hits[0].seconds_ago, 30);

        // A write exactly at `now` counts too (inclusive upper bound).
        let (g, c1, _) = two_agent_graph(3600 - 40, 3600);
        let hits = detect(&g, CONFLICT_RECENCY_WINDOW, now);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node, c1);
        assert_eq!(hits[0].seconds_ago, 0);
    }

    #[test]
    fn future_dated_edges_are_outside_the_window() {
        let now = t(100);
        // Both agents' edges exist, but every write is dated after `now` —
        // none has "happened yet", so nothing counts as write activity.
        let (g, _, _) = two_agent_graph(110, 105);
        assert!(
            detect(&g, CONFLICT_RECENCY_WINDOW, now).is_empty(),
            "future-dated writes are outside the window"
        );

        // Mixed: agent-a's write is old (outside the window), agent-b's
        // Dependency write is future-dated. Two agents, but no qualifying
        // write inside [now - window, now].
        let (g, _, _) = two_agent_graph(60, 105);
        assert!(
            detect(&g, CONFLICT_RECENCY_WINDOW, now).is_empty(),
            "future-dated edges must not count as write activity"
        );
    }

    #[test]
    fn empty_graph_has_no_conflicts() {
        let g = Graph::new(sid());
        assert!(detect(&g, CONFLICT_RECENCY_WINDOW, t(3600)).is_empty());
    }

    // ------------------------------------------------------------------
    // Payload + hot-list integration
    // ------------------------------------------------------------------

    #[test]
    fn payload_carries_agents_and_seconds_ago() {
        let now = t(3600);
        // Dependency write 11s before now: the "eleven seconds ago" sentence
        // data T5.3 renders.
        let (g, c1, _) = two_agent_graph(3600 - 40, 3600 - 11);
        let hits = detect(&g, CONFLICT_RECENCY_WINDOW, now);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node, c1);
        assert_eq!(
            hits[0].agents,
            vec![AgentId::from("agent-a"), AgentId::from("agent-b")]
        );
        assert_eq!(hits[0].seconds_ago, 11);
    }

    #[test]
    fn detect_is_deterministic_and_sorted() {
        let now = t(3600);
        let (g, _, _) = two_agent_graph(3600 - 40, 3600 - 11);
        let a = detect(&g, CONFLICT_RECENCY_WINDOW, now);
        let b = detect(&g, CONFLICT_RECENCY_WINDOW, now);
        assert_eq!(a, b);
        assert!(
            a.windows(2).all(|w| w[0].node.0 <= w[1].node.0),
            "sorted by node id"
        );
    }

    #[test]
    fn insert_conflicts_refreshes_instead_of_duplicating() {
        let now = t(3600);
        let (mut g, c1, dep_ids) = two_agent_graph(3600 - 40, 3600 - 11);
        let mut hot = HotList::new();

        insert_conflicts(&mut hot, &g, CONFLICT_RECENCY_WINDOW, now);
        assert_eq!(hot.len(), 1);
        let entry = hot.peek().unwrap();
        assert_eq!(entry.node, c1);
        assert_eq!(entry.condition, Condition::Conflict);
        match &entry.payload {
            HotListPayload::Conflict {
                agents,
                writer,
                seconds_ago,
            } => {
                assert_eq!(
                    *agents,
                    vec![AgentId::from("agent-a"), AgentId::from("agent-b")]
                );
                assert_eq!(*writer, AgentId::from("agent-b"));
                assert_eq!(*seconds_ago, 11);
            }
            other => panic!("unexpected payload {other:?}"),
        }

        // Re-run the detector with a later `now`: the (node, condition) pair
        // is already present, so the entry refreshes — payload and predicate
        // update, the list does not grow.
        let later = t(3600 + 5);
        insert_conflicts(&mut hot, &g, CONFLICT_RECENCY_WINDOW, later);
        assert_eq!(hot.len(), 1, "refresh must not duplicate entries");
        match &hot.peek().unwrap().payload {
            HotListPayload::Conflict { seconds_ago, .. } => {
                assert_eq!(*seconds_ago, 16, "payload refreshed with the new now");
            }
            other => panic!("unexpected payload {other:?}"),
        }

        // Re-validation: the holds predicate re-checks the conflict against the
        // graph at the `now` it is handed. Remove the recent Dependency edges —
        // the condition stops holding and the node drops off the list.
        for id in dep_ids {
            g.remove_edge(id).unwrap();
        }
        assert!(
            !hot.revalidate(&g, c1, later),
            "conflict gone → entry evicted on revalidation"
        );
        assert!(hot.is_empty());
    }

    // ------------------------------------------------------------------
    // XP-3 — re-validation consults live time
    // ------------------------------------------------------------------

    /// XP-3: advancing the clock past the recency window must age the entry
    /// out, with **no graph mutation at all**.
    ///
    /// Pre-fix the predicate captured `now` by move and re-derived its window
    /// from that frozen instant, so this returned `true` forever — and recall
    /// (T5.3), the documented consumer, would render the frozen `seconds_ago`:
    /// "wrote to it eleven seconds ago", five minutes later, on camera.
    #[test]
    fn revalidate_ages_a_conflict_out_when_only_the_clock_advances() {
        let detected_at = t(3600);
        let (g, c1, _) = two_agent_graph(3600 - 40, 3600 - 11);
        let mut hot = HotList::new();
        insert_conflicts(&mut hot, &g, CONFLICT_RECENCY_WINDOW, detected_at);
        assert!(hot.contains(c1), "detected at t=3600");

        // Still inside the 30s window: holds, and the age has moved on.
        let inside = t(3600 + 10);
        assert!(hot.revalidate(&g, c1, inside), "21s ago is still in-window");
        match &hot.peek().unwrap().payload {
            HotListPayload::Conflict { seconds_ago, .. } => assert_eq!(
                *seconds_ago, 21,
                "seconds_ago must be recomputed at read time, not frozen at 11"
            ),
            other => panic!("unexpected payload {other:?}"),
        }

        // Past the window, graph untouched: the entry must go.
        let outside = t(3600 + 20);
        assert!(
            !hot.revalidate(&g, c1, outside),
            "31s ago is outside the 30s window — the clock alone must age it out"
        );
        assert!(hot.is_empty(), "no ghost entry survives a clock advance");
    }

    /// ALGO-3: the writer of a concept→concept edge is the **acting** agent,
    /// not the source concept's original author.
    ///
    /// The demo shape: agent-a creates `user schema`; agent-b then records an
    /// action whose edge *originates* at a concept agent-a created. Pre-fix
    /// both edges resolved to agent-a, the agent set collapsed to one, and the
    /// conflict silently did not fire.
    #[test]
    fn resolved_concept_write_is_attributed_to_the_acting_agent() {
        let now = t(3600);
        let mut g = Graph::new(sid());
        // agent-a's interaction creates both concepts.
        let i1 = interaction(1, None, "agent-a", 3600 - 120);
        g.insert_interaction(i1.clone()).unwrap();
        // agent-b's interaction: the acting one for the edge below.
        let i2 = interaction(2, Some(1), "agent-b", 3600 - 11);
        g.insert_interaction(i2.clone()).unwrap();

        let pillar = concept(1, i1.id, "agent-a", "user schema", 3600 - 120);
        let pillar_id = pillar.id;
        g.insert_concept(pillar, i1.id).unwrap();
        // A concept agent-a authored — the source of agent-b's new edge, which
        // is exactly what `record_action` produces when it resolves an existing
        // canonical concept.
        let source = concept(2, i1.id, "agent-a", "session store", 3600 - 120);
        let source_id = source.id;
        g.insert_concept(source, i1.id).unwrap();

        // The edge is stamped with agent-b's interaction timestamp, which is
        // what `record_action` does (it copies the interaction's created_at).
        g.upsert_edge(dep_edge(1, source_id, pillar_id, 3600 - 11))
            .unwrap();
        assert!(g.assert_invariants().is_ok());

        let hits = detect(&g, CONFLICT_RECENCY_WINDOW, now);
        let hit = hits
            .iter()
            .find(|h| h.node == pillar_id)
            .expect("cross-agent write to the pillar is a conflict");
        assert_eq!(
            hit.agents,
            vec![AgentId::from("agent-a"), AgentId::from("agent-b")],
            "the acting agent must join the contesting set"
        );
        assert_eq!(
            hit.writer,
            AgentId::from("agent-b"),
            "the newest qualifying write is agent-b's"
        );
        assert_eq!(hit.seconds_ago, 11);
    }

    /// ALGO-2: the payload names the writer of the **newest** qualifying write,
    /// which is not recoverable from the sorted `agents` list — on this graph
    /// (and on the shipped fixture) the naive first-listed guess is wrong.
    #[test]
    fn writer_is_the_newest_qualifying_writer_not_the_first_agent() {
        let now = t(3600);
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, "agent-a", 3600 - 60);
        let i2 = interaction(2, Some(1), "agent-b", 3600 - 5);
        g.insert_interaction(i1.clone()).unwrap();
        g.insert_interaction(i2.clone()).unwrap();
        let target = concept(1, i1.id, "agent-a", "shared node", 3600 - 60);
        let target_id = target.id;
        g.insert_concept(target, i1.id).unwrap();
        let from_a = concept(2, i1.id, "agent-a", "writer a", 3600 - 60);
        let from_a_id = from_a.id;
        g.insert_concept(from_a, i1.id).unwrap();
        let from_b = concept(3, i2.id, "agent-b", "writer b", 3600 - 5);
        let from_b_id = from_b.id;
        g.insert_concept(from_b, i2.id).unwrap();

        // agent-a wrote 25s ago, agent-b 5s ago — both in the 30s window.
        g.upsert_edge(dep_edge(1, from_a_id, target_id, 3600 - 25))
            .unwrap();
        g.upsert_edge(dep_edge(2, from_b_id, target_id, 3600 - 5))
            .unwrap();

        let hits = detect(&g, CONFLICT_RECENCY_WINDOW, now);
        let hit = hits.iter().find(|h| h.node == target_id).unwrap();
        assert_eq!(
            hit.agents[0],
            AgentId::from("agent-a"),
            "agent-a sorts first — the naive rendering blames it"
        );
        assert_eq!(
            hit.writer,
            AgentId::from("agent-b"),
            "but the newest write is agent-b's"
        );
        assert_eq!(hit.seconds_ago, 5);
    }

    /// NEW-4: two agents' interactions at the **same instant**, and a cross-agent
    /// write stamped with it. The timeline used to keep only the smallest
    /// interaction id per instant, which re-created the very failure ALGO-3
    /// fixed — the contested node's agent set collapsed to one agent and the
    /// conflict silently did not fire.
    #[test]
    fn same_instant_cross_agent_write_still_conflicts() {
        let now = t(3600);
        let mut g = Graph::new(sid());
        // Both interactions at 3600-10: the collided instant. i1 (agent-a) has
        // the smaller id, so the old tie-break resolved every edge written then
        // to agent-a and agent-b vanished from attribution entirely.
        let i1 = interaction(1, None, "agent-a", 3600 - 10);
        let i2 = interaction(2, Some(1), "agent-b", 3600 - 10);
        g.insert_interaction(i1.clone()).unwrap();
        g.insert_interaction(i2.clone()).unwrap();

        let contested = concept(1, i1.id, "agent-a", "user schema", 3600 - 10);
        let contested_id = contested.id;
        g.insert_concept(contested, i1.id).unwrap();
        let source = concept(2, i2.id, "agent-b", "cache layer", 3600 - 10);
        let source_id = source.id;
        g.insert_concept(source, i2.id).unwrap();

        // The cross-agent write: a concept→concept Dependency stamped with the
        // collided instant, so attribution has to fall to the timeline.
        g.upsert_edge(dep_edge(1, source_id, contested_id, 3600 - 10))
            .unwrap();
        assert!(g.assert_invariants().is_ok());

        let hits = detect(&g, CONFLICT_RECENCY_WINDOW, now);
        let hit = hits
            .iter()
            .find(|h| h.node == contested_id)
            .expect("a same-instant cross-agent write is still a conflict");
        assert_eq!(
            hit.agents,
            vec![AgentId::from("agent-a"), AgentId::from("agent-b")],
            "every same-instant candidate joins the contested set"
        );
        // The named writer is one of the candidates, chosen deterministically:
        // smallest interaction id at the instant (documented ambiguity).
        assert_eq!(hit.writer, AgentId::from("agent-a"));
        assert_eq!(hit.seconds_ago, 10);
    }
}

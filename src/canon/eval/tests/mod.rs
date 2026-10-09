use super::*;
#[cfg(feature = "store-memory")]
use crate::types::{
    AgentId, Concept, ConceptType, DaemonEvent, Edge, EdgeType, Interaction, Scored, SessionId,
};
#[cfg(feature = "store-memory")]
use chrono::TimeZone;
use std::collections::HashSet;
use uuid::Uuid;

#[cfg(feature = "store-memory")]
fn ts() -> DateTime<Utc> {
    Utc.timestamp_opt(1_752_000_000, 0).unwrap()
}

#[cfg(feature = "store-memory")]
fn sid() -> SessionId {
    SessionId::from("test-session")
}

fn nid(id: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(2, id))
}

#[cfg(feature = "store-memory")]
fn iid(id: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(1, id))
}

#[cfg(feature = "store-memory")]
fn eid(id: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(3, id))
}

#[cfg(feature = "store-memory")]
fn interaction(id: u64, prev: Option<u64>, at: DateTime<Utc>) -> Interaction {
    Interaction {
        event_time: None,
        id: iid(id),
        session_id: sid(),
        agent_id: AgentId::from("agent-a"),
        prompt_text: Some(format!("i{id}")),
        previous_id: prev.map(iid),
        created_at: at,
    }
}

#[cfg(feature = "store-memory")]
fn concept(id: u64, origin: u64, gc: i32, status: CanonizationStatus) -> Concept {
    Concept {
        id: nid(id),
        session_id: sid(),
        content: format!("c{id}"),
        canonical_key: format!("c{id}"),
        concept_type: ConceptType::Entity,
        origin_interaction: iid(origin),
        origin_agent: AgentId::from("agent-a"),
        created_at: ts(),
        access_count: 0,
        last_accessed: None,
        gc_survived: gc,
        canonization_status: status,
        blast_radius: None,
        last_demotion_time: None,
        embedding: None,
        human_confirmed: 0,
        embedding_source: None,
        chunk_group_id: None,
    }
}

#[cfg(feature = "store-memory")]
fn table(pairs: &[(u64, f64)]) -> ScoreTable {
    ScoreTable {
        epoch: 0,
        ranked: pairs
            .iter()
            .map(|&(id, score)| Scored::new(nid(id), score))
            .collect(),
    }
}

#[cfg(feature = "store-memory")]
fn params() -> EvalParams {
    EvalParams {
        min_age: Duration::ZERO,
        min_edge_age: Duration::ZERO,
        ..EvalParams::default()
    }
}

#[cfg(feature = "store-memory")]
fn status_of(graph: &Graph, id: NodeId) -> CanonizationStatus {
    concept_status(graph, id).expect("concept")
}

/// Twenty still-`None` peers off one interaction — enough to open the
/// Stage-1 session gate, which is all these two tests need.
#[cfg(feature = "store-memory")]
fn twenty_peer_graph() -> (Graph, ScoreTable) {
    let mut g = Graph::new(sid());
    g.insert_interaction(interaction(1, None, ts())).unwrap();
    for id in 1..=20u64 {
        g.insert_concept(concept(id, 1, 5, CanonizationStatus::None), iid(1))
            .unwrap();
    }
    let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
    pairs.push((20, 1.0));
    (g, table(&pairs))
}

/// **The seam is load-bearing in the pipeline, not merely present.**
///
/// `canon::policy`'s own tests prove the scorers compute different
/// verdicts on the same corpus. They do *not* prove `gather` ever asks.
/// Reverting `gather` to call `stage1_candidates` directly leaves swarm
/// behaviour identical, so every other test in this crate stays green
/// while `promotion_policy` silently becomes decorative — the exact
/// "config selection falls back to swarm" defect C1 had to rule out, and
/// C2 must still rule out now that solo resolves to a real scorer.
///
/// The corpus is one a solo session legitimately promotes and swarm never
/// could: four event-timed turns ≥24h apart (flushed inside one minute —
/// the bulk bootstrap), one Entity re-derived by each. Solo's recurrence
/// term counts four sessions → 4.8 ≥ 3.0; Stage 1's swarm gate needs
/// twenty peers and `gc_survived >= 3`, neither of which exists here.
///
/// Mutation: replace the dispatch in `gather` with the original
/// `stage1_candidates(graph, scores, params.min_peer_count)` → the solo
/// half goes empty and the assertion fails. Verified red.
#[cfg(feature = "store-memory")]
#[test]
fn gather_dispatches_on_the_configured_promotion_policy() {
    let mut g = Graph::new(sid());
    let mut prev = None;
    for n in 1..=4u64 {
        let mut turn = interaction(n, prev, ts());
        turn.event_time = Some(ts() - chrono::Duration::hours(48 * (4 - n) as i64));
        g.insert_interaction(turn).unwrap();
        prev = Some(n);
    }
    let hub = concept(10, 1, 0, CanonizationStatus::None);
    let hub_id = hub.id;
    g.insert_concept(hub, iid(1)).unwrap();
    for n in 2..=4u64 {
        g.upsert_edge(Edge {
            id: eid(n),
            session_id: sid(),
            source: iid(n),
            target: hub_id,
            edge_type: EdgeType::Derives,
            weight: 0.9,
            reinforcements: 1,
            created_at: ts(),
            last_reinforced: ts(),
            event_time: Some(ts() - chrono::Duration::hours(48 * (4 - n) as i64)),
        })
        .unwrap();
    }
    let scores = table(&[]);

    // Solo: the recurrence term admits the hub at the Candidate bar.
    let solo_params = EvalParams {
        promotion_policy: PromotionPolicy::Solo,
        ..params()
    };
    let plan = Evaluator::new().gather(&g, &scores, &solo_params, ts());
    assert_eq!(plan.stage1, vec![hub_id]);

    // Swarm: the same rows clear nothing — no peers, no gc survival.
    let plan = Evaluator::new().gather(&g, &scores, &params(), ts());
    assert!(plan.stage1.is_empty(), "swarm must refuse this corpus");
}

/// The complement: under the **default** policy the same fixture goes
/// through the seam and produces the Stage-1 set the welded pipeline
/// always produced — nearest-rank P90 over twenty peers leaves exactly
/// the one concept scoring strictly above it.
///
/// Without this, `gather_dispatches_on_the_configured_promotion_policy`
/// would still pass if the swarm arm were broken to refuse as well.
///
/// Mutation: make `SwarmScorer::candidates` return `Vec::new()` → red.
#[cfg(feature = "store-memory")]
#[test]
fn the_default_policy_still_gathers_the_welded_stage1_set() {
    let (g, scores) = twenty_peer_graph();
    let params = params();
    assert_eq!(params.promotion_policy, PromotionPolicy::Swarm);
    let plan = Evaluator::new().gather(&g, &scores, &params, ts());
    assert_eq!(plan.stage1, vec![nid(20)]);
}

/// Ring arithmetic, degenerate inputs included: empty ring, zero window,
/// a cursor past the end (wrap), and a cursor whose node has **left** the
/// ring — the last is the churn case (F1), where the resume point must
/// still be "the first id strictly greater", not an index.
#[test]
fn ring_window_is_identity_anchored_and_wraps() {
    let ring: Vec<NodeId> = (1..=5).map(nid).collect();
    assert!(ring_window(&[], None, 3).is_empty(), "empty ring");
    assert!(ring_window(&ring, None, 0).is_empty(), "zero window");
    assert_eq!(ring_window(&ring, None, 2), vec![nid(1), nid(2)]);
    assert_eq!(ring_window(&ring, Some(nid(2)), 2), vec![nid(3), nid(4)]);
    assert_eq!(
        ring_window(&ring, Some(nid(5)), 2),
        vec![nid(1), nid(2)],
        "a cursor at the end wraps to the head"
    );
    assert_eq!(
        ring_window(&ring, Some(nid(9)), 2),
        vec![nid(1), nid(2)],
        "a cursor past every member wraps too"
    );
    // Churn: 3 and 4 promoted out since the cursor was set to 3.
    let shrunk = vec![nid(1), nid(2), nid(5)];
    assert_eq!(
        ring_window(&shrunk, Some(nid(3)), 2),
        vec![nid(5), nid(1)],
        "a departed cursor resumes at the first surviving id above it"
    );
    // A window wider than the ring never repeats an id.
    let full = ring_window(&ring, Some(nid(3)), 99);
    assert_eq!(full.len(), ring.len());
    assert_eq!(
        full.iter().copied().collect::<HashSet<_>>().len(),
        ring.len(),
        "no duplicates: {full:?}"
    );
}

/// CON-6: `as i32` would wrap `i32::MAX + 1` to `i32::MIN`.
#[test]
fn blast_radius_narrow_rejects_unrepresentable_u64() {
    assert_eq!(narrow_blast_radius(0).unwrap(), 0);
    assert_eq!(narrow_blast_radius(i32::MAX as u64).unwrap(), i32::MAX);
    let err = narrow_blast_radius(i32::MAX as u64 + 1).unwrap_err();
    match err {
        StoreError::Invariant(msg) => {
            assert!(msg.contains("i32"), "{msg}");
            assert!(msg.contains("CON-6"), "{msg}");
        }
        other => panic!("expected Invariant, got {other:?}"),
    }
}

#[cfg(feature = "store-memory")]
mod with_store;

#[cfg(all(feature = "store-memory", feature = "fixtures"))]
mod fixture;

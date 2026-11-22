//! #325: a session the write gates accept must load.
//!
//! The write gates check `Causal ∪ Dependency` (`closing_edge`) and
//! `Hierarchical` (`reject_closing_edge`) separately, so a cycle that mixes
//! the kinds is written. A child citing its own parent is the simplest:
//! `P -Hierarchical-> C` and `C -Dependency-> P`. The load check now checks
//! the same two conditions separately, so the session loads; a pure cycle of
//! either condition is still refused.
//!
//! Mirrors `formal/LamboProofs/Cite.lean`: `written_loadOk` (every written
//! graph passes the two-condition check), `child_cites_parent_written` and
//! `child_cites_parent_written'` (the scenario in either order),
//! `union_check_rejects_written` (the old union check refused it), and
//! `hierarchical_cycle_not_loadOk` / `dependency_cycle_not_loadOk`.

use chrono::{TimeZone, Utc};

use super::*;
use crate::graph::action::{record_action, Action};
use crate::graph::cite::{validate_scoped, ConceptCitations};
use crate::graph::derive::{derive_cited, ParentOf};
use crate::store::memory::MemoryStore;
use crate::types::{AgentId, ConceptType, Edge, EdgeType, Interaction, NodeId};

const PARENT: &str = "the writer lease protocol";
const CHILD: &str = "flush retries keep the writer lease";

fn agent() -> AgentId {
    AgentId::from("agent-a")
}

/// A graph with one interaction.
fn graph(sid: &str) -> (Graph, NodeId) {
    let mut g = Graph::new(SessionId::from(sid));
    let i = Interaction {
        event_time: None,
        id: NodeId::new(),
        session_id: SessionId::from(sid),
        agent_id: agent(),
        prompt_text: Some("mixed cycle".into()),
        previous_id: None,
        created_at: Utc.timestamp_opt(1_752_000_000, 0).unwrap(),
    };
    let id = i.id;
    g.insert_interaction(i).unwrap();
    (g, id)
}

/// One derive call through the write path `Memory`'s synchronous derive
/// takes: the citation pre-pass under the call's `parent_of`, then the
/// derive with the pinned citations, both gates included.
fn derive(
    g: &mut Graph,
    i: NodeId,
    concepts: &[(&str, ConceptType)],
    pairs: &[(&str, &str)],
    citations: &[(&str, &[&str])],
) {
    let parent_of = ParentOf::from_pairs(pairs);
    let citations: Vec<ConceptCitations> = citations
        .iter()
        .map(|(content, deps)| ConceptCitations {
            name: None,
            content: content.to_string(),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            against: Vec::new(),
        })
        .collect();
    let pinned = validate_scoped(g, concepts, &citations, &parent_of).unwrap();
    derive_cited(g, i, &agent(), concepts, &parent_of, 10, &pinned).unwrap();
}

fn node_of(g: &Graph, content: &str) -> NodeId {
    g.concepts()
        .find(|c| c.content == content)
        .map(|c| c.id)
        .unwrap_or_else(|| panic!("no concept {content:?}"))
}

/// Flush everything the graph wrote to a store, load it back through
/// `materialize`, and return the loaded graph. Also materializes the
/// in-RAM snapshot directly.
async fn flush_and_load(mut g: Graph) -> Graph {
    let sid = g.session_id().clone();
    materialize(g.snapshot()).expect("the in-RAM snapshot materializes");
    let batch = g.drain_log();
    let store = MemoryStore::new();
    store.flush(&batch, None).await.unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    let loaded = materialize(snap).expect("the flushed session loads");
    loaded.graph.assert_invariants().unwrap();
    loaded.graph
}

fn has(g: &Graph, src: NodeId, tgt: NodeId, ty: EdgeType) -> bool {
    g.edge_between(src, tgt, ty).is_some()
}

/// The issue's scenario in one call: `C` is derived with `depends_on: [P]`
/// and `parent_of: [(P, C)]`. Lean: `child_cites_parent_written`,
/// `written_loadOk`.
#[tokio::test]
async fn a_child_citing_its_parent_in_one_call_loads() {
    let (mut g, i) = graph("mixed-cycle-one-call");
    derive(&mut g, i, &[(PARENT, ConceptType::Logic)], &[], &[]);
    derive(
        &mut g,
        i,
        &[(CHILD, ConceptType::Logic)],
        &[(PARENT, CHILD)],
        &[(CHILD, &[PARENT])],
    );
    let (p, c) = (node_of(&g, PARENT), node_of(&g, CHILD));
    assert!(has(&g, p, c, EdgeType::Hierarchical));
    assert!(has(&g, c, p, EdgeType::Dependency));
    let loaded = flush_and_load(g).await;
    assert!(has(&loaded, p, c, EdgeType::Hierarchical));
    assert!(has(&loaded, c, p, EdgeType::Dependency));
}

/// The two-call forms: the citation first and `parent_of` later, and the
/// other way round. Lean: `child_cites_parent_written'`.
#[tokio::test]
async fn a_child_citing_its_parent_in_two_calls_loads() {
    // The citation first.
    let (mut g, i) = graph("mixed-cycle-cite-first");
    derive(&mut g, i, &[(PARENT, ConceptType::Logic)], &[], &[]);
    derive(
        &mut g,
        i,
        &[(CHILD, ConceptType::Logic)],
        &[],
        &[(CHILD, &[PARENT])],
    );
    derive(
        &mut g,
        i,
        &[(PARENT, ConceptType::Logic), (CHILD, ConceptType::Logic)],
        &[(PARENT, CHILD)],
        &[],
    );
    let (p, c) = (node_of(&g, PARENT), node_of(&g, CHILD));
    assert!(has(&g, p, c, EdgeType::Hierarchical));
    assert!(has(&g, c, p, EdgeType::Dependency));
    flush_and_load(g).await;

    // The parent_of pair first.
    let (mut g, i) = graph("mixed-cycle-parent-first");
    derive(
        &mut g,
        i,
        &[(PARENT, ConceptType::Logic), (CHILD, ConceptType::Logic)],
        &[(PARENT, CHILD)],
        &[],
    );
    derive(
        &mut g,
        i,
        &[(CHILD, ConceptType::Logic)],
        &[],
        &[(CHILD, &[PARENT])],
    );
    let (p, c) = (node_of(&g, PARENT), node_of(&g, CHILD));
    assert!(has(&g, p, c, EdgeType::Hierarchical));
    assert!(has(&g, c, p, EdgeType::Dependency));
    flush_and_load(g).await;
}

/// The issue's two `record_action` forms: an action that depends on `X`,
/// then `parent_of (X, action)`; an action that produces `R`, then
/// `parent_of (R, action)`.
#[tokio::test]
async fn an_action_made_the_child_of_what_it_cites_or_produces_loads() {
    let (mut g, i) = graph("mixed-cycle-action");
    derive(&mut g, i, &[("schema v1", ConceptType::Entity)], &[], &[]);
    let x = node_of(&g, "schema v1");
    let a = record_action(
        &mut g,
        i,
        &agent(),
        &Action {
            repo_scope: None,
            action: "migrated the users table",
            produces: &[],
            modifies: &[],
            depends_on: &["schema v1"],
            event_time: None,
        },
    )
    .unwrap()
    .action_node;
    derive(
        &mut g,
        i,
        &[("schema v1", ConceptType::Entity)],
        &[("schema v1", "migrated the users table")],
        &[],
    );
    assert!(has(&g, a, x, EdgeType::Dependency));
    assert!(has(&g, x, a, EdgeType::Hierarchical));
    // A child has one parent, so the produced resource's form gets its own
    // action.
    let b = record_action(
        &mut g,
        i,
        &agent(),
        &Action {
            repo_scope: None,
            action: "generated the client",
            produces: &["client v2"],
            modifies: &[],
            depends_on: &[],
            event_time: None,
        },
    )
    .unwrap()
    .action_node;
    let out = node_of(&g, "client v2");
    derive(
        &mut g,
        i,
        &[("client v2", ConceptType::Resource)],
        &[("client v2", "generated the client")],
        &[],
    );
    assert!(has(&g, b, out, EdgeType::Causal));
    assert!(has(&g, out, b, EdgeType::Hierarchical));
    flush_and_load(g).await;
}

/// The load check still refuses a pure cycle of either condition, when one
/// is forced into a snapshot past the write gates. Lean:
/// `hierarchical_cycle_not_loadOk`, `dependency_cycle_not_loadOk`.
#[test]
fn a_pure_cycle_forced_into_a_snapshot_is_still_refused() {
    let (mut g, i) = graph("mixed-cycle-pure");
    derive(&mut g, i, &[(PARENT, ConceptType::Logic)], &[], &[]);
    derive(
        &mut g,
        i,
        &[(CHILD, ConceptType::Logic)],
        &[(PARENT, CHILD)],
        &[(CHILD, &[PARENT])],
    );
    let (p, c) = (node_of(&g, PARENT), node_of(&g, CHILD));
    let base = g.snapshot();
    let template = base
        .edges
        .iter()
        .find(|e| e.edge_type == EdgeType::Dependency)
        .expect("the citation edge")
        .clone();
    for (ty, back, label) in [
        // P -Hierarchical-> C already; C -Hierarchical-> P closes it.
        (EdgeType::Hierarchical, (c, p), "Hierarchical cycle"),
        // C -Dependency-> P already; P -Dependency-> C closes it.
        (EdgeType::Dependency, (p, c), "Causal/Dependency cycle"),
        // C -Dependency-> P already; P -Causal-> C closes it.
        (EdgeType::Causal, (p, c), "Causal/Dependency cycle"),
    ] {
        let mut snap = base.clone();
        snap.edges.push(Edge {
            id: NodeId::new(),
            source: back.0,
            target: back.1,
            edge_type: ty,
            ..template.clone()
        });
        let err = materialize(snap).expect_err("a pure cycle must not load");
        assert!(
            matches!(&err, StoreError::Invariant(m) if m.contains(label)),
            "{ty:?}: {err:?}"
        );
    }
}

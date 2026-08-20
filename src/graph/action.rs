//! `record_action()` (T2.4) — spec §7 write-path entry point.
//!
//! Records one agent action as a `Resource` concept plus its structural edges:
//! `Causal` to each produce/modify, `Dependency` to each dependency. Implicit
//! concept creation runs the full canonicalization pipeline (T2.2) — every
//! content string resolves through [`canonicalize`], so a repeated phrase is
//! one concept, never a duplicate. The graph is never left half-written:
//! resolution, planning, and the cycle check all run read-only, and mutation
//! happens only after every check passes (validate-then-mutate).
//!
//! ## Edge direction (pinned by the fixture convention)
//!
//! `Dependency` edges point from the *dependent* to the *dependency*
//! (`fixtures/session-rest-api.json`: `"create user" -> "user schema"`); the
//! action node is the dependent, its `depends_on` are the dependencies. By the
//! same rule the action node **causes** its produces/modifies, so every edge
//! this module creates has the action node as its source.
//!
//! ## Cycle rejection (spec §5.7 — "enforced at write time by BFS")
//!
//! Adding a `Causal`/`Dependency` edge that would close a cycle is rejected
//! with `LamboError::Store(StoreError::Invariant)` **before anything is
//! written**. The check is a BFS over `Causal`/`Dependency` out-neighbors of
//! the graph plus the call's planned edges: for every planned edge `a -> b`,
//! if `b` can reach `a` (through existing or planned edges), the write would
//! close a cycle. [`Graph::upsert_edge`] deliberately stores what it is given
//! (T2.1); this module is the spec-mandated write-time gate.
//!
//! ## Determinism
//!
//! The pinned signature has no clock, so created concepts and edges are
//! timestamped with the **interaction's** `created_at` (the action is part of
//! that interaction). No `Utc::now()` — tests stay deterministic and snapshot
//! round-trips are stable.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::graph::canonical::{canonicalize, CanonicalizeResult};
use crate::graph::Graph;
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, LamboError, Node, NodeId,
    StoreError,
};

/// Initial weight of `Causal` edges created by [`record_action`] — the
/// module-owned structural default (0.5, same initial value as the other
/// module-created structural edges; the Graph-owned `Derives`/`Temporal`
/// defaults are 0.9/1.0, T2.1). Fixture `Dependency` weights are
/// story-specific hand-set values (`scripts/gen-fixtures.py`), not a
/// convention this module mirrors.
const CAUSAL_WEIGHT: f64 = 0.5;

/// Initial weight of `Dependency` edges created by [`record_action`] — see
/// [`CAUSAL_WEIGHT`].
const DEPENDENCY_WEIGHT: f64 = 0.5;

/// One agent action to record (pinned T2.4 contract, spec §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action<'a> {
    /// The action itself — becomes a `Resource` concept.
    pub action: &'a str,
    /// Resources this action creates; `Causal` edges from the action node.
    pub produces: &'a [&'a str],
    /// Resources this action mutates; `Causal` edges from the action node.
    pub modifies: &'a [&'a str],
    /// Things this action depends on; `Dependency` edges from the action node.
    pub depends_on: &'a [&'a str],
}

/// Result of a successful [`record_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionOutcome {
    /// The `Resource` concept for the action (existing or newly created).
    pub action_node: NodeId,
    /// Concepts newly created by this call (implicit creation), in encounter
    /// order (action, produces, modifies, depends_on), deduplicated by
    /// canonical key. Matched contents are reused, never listed here.
    pub created: Vec<NodeId>,
    /// Number of edges newly inserted by this call — the deduplicated planned
    /// edges whose natural key did not already exist. Re-recorded edges
    /// reinforce instead (see [`Graph::upsert_edge`]) and are not counted.
    pub edges: usize,
}

/// Record an agent action (spec §7).
///
/// # Flow
///
/// 1. `interaction` must name an existing `Interaction` node — else
///    `StoreError::NotFound`.
/// 2. Resolve every content string through [`canonicalize`] (read-only): the
///    action string, each produce/modify -> `Resource`, each dependency ->
///    `Entity`. Unmatched strings become *planned* concepts — `NodeId`s are
///    allocated during planning, but nothing is written yet.
/// 3. Plan edges, source = the action node: `Causal` to each produce/modify,
///    `Dependency` to each dependency. Deduplicated by natural key.
/// 4. Cycle check — BFS over `Causal`/`Dependency` out-neighbors (graph ∪
///    planned edges): if any planned edge `a -> b` has `b` reaching `a`, the
///    write would close a cycle -> `Invariant` error, graph untouched.
/// 5. Mutate: `insert_concept` for each planned concept (`origin_interaction`
///    = `interaction` — this also creates the structural `Derives` edge), then
///    `upsert_edge` for each planned edge.
///
/// Validate-then-mutate is the contract: every failure path leaves the graph
/// byte-identical — `snapshot()` unchanged, no log entries appended, `epoch`
/// and `log_len` untouched.
pub fn record_action(
    graph: &mut Graph,
    interaction: NodeId,
    agent: &AgentId,
    action: &Action,
) -> Result<ActionOutcome, LamboError> {
    // Step 1 — the interaction must exist (a concept id is equally rejected:
    // `record_action` attaches the action to a specific interaction).
    let created_at = match graph.node(interaction) {
        Some(Node::Interaction(i)) => i.created_at,
        _ => {
            return Err(LamboError::Store(StoreError::NotFound(format!(
                "interaction {interaction} not found"
            ))))
        }
    };
    let session_id = graph.session_id().clone();

    let Plan {
        action_node,
        planned,
        planned_edges,
    } = plan(graph, action)?;

    // Step 5 — every check passed; mutate. Concepts first (their Derives edges
    // are emitted by insert_concept), then the planned edges, so the mutation
    // log never references an endpoint that was not upserted earlier.
    let mut created: Vec<NodeId> = Vec::with_capacity(planned.len());
    for c in &planned {
        let concept = Concept {
            id: c.id,
            session_id: session_id.clone(),
            content: c.content.clone(),
            canonical_key: c.key.clone(),
            concept_type: c.concept_type,
            origin_interaction: interaction,
            origin_agent: agent.clone(),
            created_at,
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        };
        graph.insert_concept(concept, interaction)?;
        created.push(c.id);
    }

    let mut edges = 0usize;
    for &(src, tgt, ty) in &planned_edges {
        let weight = match ty {
            EdgeType::Causal => CAUSAL_WEIGHT,
            EdgeType::Dependency => DEPENDENCY_WEIGHT,
            _ => unreachable!("record_action only plans Causal/Dependency edges"),
        };
        let edge = Edge {
            id: NodeId::new(),
            session_id: session_id.clone(),
            source: src,
            target: tgt,
            edge_type: ty,
            weight,
            reinforcements: 1,
            created_at,
            last_reinforced: created_at,
        };
        let is_new = graph.edge_between(src, tgt, ty).is_none();
        graph.upsert_edge(edge)?;
        if is_new {
            edges += 1;
        }
    }

    Ok(ActionOutcome {
        action_node,
        created,
        edges,
    })
}

/// [`record_action`]'s planned write: what it would create, before it creates
/// anything.
struct Plan {
    action_node: NodeId,
    planned: Vec<PlannedConcept>,
    planned_edges: Vec<(NodeId, NodeId, EdgeType)>,
}

/// [`record_action`]'s read-only steps 2 to 4 — resolve, plan, cycle-check —
/// with the plan handed back instead of applied.
///
/// One function rather than two copies of the same three steps, because J3's
/// asynchronous ack path has to run exactly these checks on the call path (see
/// [`validate`]) while `record_action` needs the plan they produce. The cycle
/// check is inseparable from the planning — it reasons about edges landing on
/// not-yet-created nodes — so a "cheap validation" that skipped it would not be
/// the same check.
fn plan(graph: &Graph, action: &Action) -> Result<Plan, LamboError> {
    // Step 2 — resolve + plan concepts (read-only; nothing touches the graph).
    let mut resolved: HashMap<String, NodeId> = HashMap::new();
    let mut planned: Vec<PlannedConcept> = Vec::new();
    let action_node = resolve(
        graph,
        &mut resolved,
        &mut planned,
        action.action,
        ConceptType::Resource,
    )?;

    // Step 3 — plan edges, deduplicated by natural key.
    let mut planned_edges: Vec<(NodeId, NodeId, EdgeType)> = Vec::new();
    let mut edge_keys: HashSet<(NodeId, NodeId, EdgeType)> = HashSet::new();
    for p in action.produces {
        let tgt = resolve(graph, &mut resolved, &mut planned, p, ConceptType::Resource)?;
        plan_edge(
            &mut planned_edges,
            &mut edge_keys,
            action_node,
            tgt,
            EdgeType::Causal,
        );
    }
    for m in action.modifies {
        let tgt = resolve(graph, &mut resolved, &mut planned, m, ConceptType::Resource)?;
        plan_edge(
            &mut planned_edges,
            &mut edge_keys,
            action_node,
            tgt,
            EdgeType::Causal,
        );
    }
    for d in action.depends_on {
        let tgt = resolve(graph, &mut resolved, &mut planned, d, ConceptType::Entity)?;
        plan_edge(
            &mut planned_edges,
            &mut edge_keys,
            action_node,
            tgt,
            EdgeType::Dependency,
        );
    }

    // Step 4 — cycle check (read-only). Rejection leaves the graph byte-identical.
    if let Some((src, tgt, ty)) = closing_edge(graph, &planned_edges) {
        return Err(LamboError::Store(StoreError::Invariant(format!(
            "{ty:?} edge {src} -> {tgt} would create a cycle"
        ))));
    }

    Ok(Plan {
        action_node,
        planned,
        planned_edges,
    })
}

/// [`record_action`]'s read-only pre-pass, for a caller that must validate
/// **before** deciding to write — J3's asynchronous ack path
/// ([`crate::Memory::record_action_async_as`]).
///
/// Runs the real steps 2 to 4 through [`plan`] and discards the plan, so the
/// errors an agent can fix (an unresolvable content, an empty canonical key, an
/// edge that would close a cycle) surface at call time instead of on a receipt.
/// Nothing is written whether it passes or fails.
pub fn validate(graph: &Graph, action: &Action) -> Result<(), LamboError> {
    plan(graph, action).map(|_| ())
}

/// A concept planned for creation by [`record_action`] — resolved but not yet
/// written. Ids are allocated during planning so the cycle check can reason
/// about edges that would land on not-yet-created nodes.
struct PlannedConcept {
    id: NodeId,
    content: String,
    key: String,
    concept_type: ConceptType,
}

/// Resolve one content string to a concept id: [`canonicalize`] against the
/// graph, reusing a matched concept or a concept already planned in this call
/// (dedup by canonical key). Unmatched strings become planned concepts; the
/// first encounter wins on duplicate keys with conflicting requested types
/// (e.g. the same phrase in both `produces` and `depends_on`).
fn resolve(
    graph: &Graph,
    resolved: &mut HashMap<String, NodeId>,
    planned: &mut Vec<PlannedConcept>,
    content: &str,
    concept_type: ConceptType,
) -> Result<NodeId, LamboError> {
    match canonicalize(content, graph)? {
        CanonicalizeResult::Matched { key, node } => {
            // GRAPH-8: a concept with an empty canonical key is junk regardless
            // of how it got in — reject at the entry point.
            reject_empty_key(content, &key)?;
            Ok(node)
        }
        CanonicalizeResult::Unmatched { key } => {
            reject_empty_key(content, &key)?;
            if let Some(&id) = resolved.get(&key) {
                return Ok(id);
            }
            let id = NodeId::new();
            resolved.insert(key.clone(), id);
            planned.push(PlannedConcept {
                id,
                content: content.to_string(),
                key,
                concept_type,
            });
            Ok(id)
        }
    }
}

/// Register one planned edge unless its natural key is already planned.
fn plan_edge(
    planned: &mut Vec<(NodeId, NodeId, EdgeType)>,
    seen: &mut HashSet<(NodeId, NodeId, EdgeType)>,
    src: NodeId,
    tgt: NodeId,
    ty: EdgeType,
) {
    if seen.insert((src, tgt, ty)) {
        planned.push((src, tgt, ty));
    }
}

/// Returns the first planned edge `a -> b` whose insertion would close a cycle
/// — `b` can reach `a` through `Causal`/`Dependency` out-neighbors of the
/// graph plus the other planned edges — or `None` when the write is acyclic.
///
/// The planned edge under test is never traversed by the BFS (it is an
/// *incoming* edge of `b`, and reaching `a` is the termination condition), so
/// the search over the combined graph is exact. A self-loop (`a == b`) is a
/// cycle by definition — an action node in its own produces/depends_on (e.g.
/// action `"x"` producing `["x"]`) would create one. `Hierarchical` is
/// deliberately excluded: write-time acyclicity of that edge type is not part
/// of the pinned §5.7 contract (see `graph.rs` module docs).
fn closing_edge(
    graph: &Graph,
    planned: &[(NodeId, NodeId, EdgeType)],
) -> Option<(NodeId, NodeId, EdgeType)> {
    let mut planned_out: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for &(src, tgt, _) in planned {
        planned_out.entry(src).or_default().push(tgt);
    }
    for &(src, tgt, ty) in planned {
        if src == tgt {
            return Some((src, tgt, ty));
        }
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut queue: VecDeque<NodeId> = VecDeque::new();
        seen.insert(tgt);
        queue.push_back(tgt);
        while let Some(cur) = queue.pop_front() {
            let next = graph
                .out_neighbors_typed(cur, EdgeType::Causal)
                .into_iter()
                .chain(graph.out_neighbors_typed(cur, EdgeType::Dependency))
                .chain(planned_out.get(&cur).into_iter().flatten().copied());
            for n in next {
                if n == src {
                    return Some((src, tgt, ty));
                }
                if seen.insert(n) {
                    queue.push_back(n);
                }
            }
        }
    }
    None
}

/// GRAPH-8 guard: reject content whose canonical key is empty — empty,
/// whitespace-only, or stopword-only input would all collapse onto one key-""
/// concept with a frozen arbitrary type. Typed `Invariant` error, raised during
/// the read-only planning phase (validate-then-mutate: nothing is written).
fn reject_empty_key(content: &str, key: &str) -> Result<(), LamboError> {
    if key.is_empty() {
        return Err(LamboError::Store(StoreError::Invariant(format!(
            "record_action: content {:?} canonicalizes to an empty key (empty, \
             whitespace-only, or stopword-only content is rejected)",
            content
        ))));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Interaction, Mutation, SessionId};
    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    fn ts(minutes: i64) -> DateTime<Utc> {
        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        base + chrono::Duration::minutes(minutes)
    }

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn agent() -> AgentId {
        AgentId::from("agent-a")
    }

    fn uid(u: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(0, u))
    }

    fn interaction(id: u64, prev: Option<NodeId>, at_min: i64) -> Interaction {
        Interaction {
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid(),
            agent_id: agent(),
            prompt_text: Some(format!("prompt {id}")),
            previous_id: prev,
            created_at: ts(at_min),
        }
    }

    /// Test concept helper (chunk_group_id: None, per repo convention). The
    /// canonical key is explicit — `canonicalize` matches on the canonical
    /// form, which is sorted + stemmed, not the raw content.
    fn concept(id: u64, origin: NodeId, content: &str, key: &str) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(2, id)),
            session_id: sid(),
            content: content.into(),
            canonical_key: key.into(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: agent(),
            created_at: ts(0),
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

    fn edge(id: u64, src: NodeId, tgt: NodeId, ty: EdgeType, w: f64) -> Edge {
        Edge {
            id: NodeId(Uuid::from_u64_pair(3, id)),
            session_id: sid(),
            source: src,
            target: tgt,
            edge_type: ty,
            weight: w,
            reinforcements: 1,
            created_at: ts(0),
            last_reinforced: ts(0),
        }
    }

    fn graph_with_interaction() -> (Graph, NodeId) {
        let mut g = Graph::new(sid());
        let i = interaction(1, None, 0);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        (g, iid)
    }

    fn action<'a>(
        action: &'a str,
        produces: &'a [&'a str],
        modifies: &'a [&'a str],
        depends_on: &'a [&'a str],
    ) -> Action<'a> {
        Action {
            action,
            produces,
            modifies,
            depends_on,
        }
    }

    #[test]
    fn record_action_creates_resource_concept_and_structural_edges() {
        let (mut g, iid) = graph_with_interaction();
        let act = action(
            "created migrations/003.sql",
            &["migrations/003.sql"],
            &["auth middleware"],
            &["user schema"],
        );
        let out = record_action(&mut g, iid, &agent(), &act).unwrap();

        // Action node is a Resource concept with a Derives edge from the
        // interaction (auto via insert_concept).
        let action_concept = match g.node(out.action_node).unwrap() {
            Node::Concept(c) => c,
            _ => panic!("action node is not a concept"),
        };
        assert_eq!(action_concept.concept_type, ConceptType::Resource);
        assert_eq!(action_concept.content, "created migrations/003.sql");
        assert_eq!(action_concept.origin_interaction, iid);
        assert_eq!(action_concept.origin_agent.as_str(), "agent-a");
        let derives = g
            .edge_between(iid, out.action_node, EdgeType::Derives)
            .expect("action concept derives from the interaction");
        assert_eq!(derives.weight, 0.9);

        // Encounter order: action, produces, modifies, depends_on.
        assert_eq!(out.created.len(), 4);
        assert_eq!(out.created[0], out.action_node);
        let prod = out.created[1];
        let modi = out.created[2];
        let dep = out.created[3];
        let prod_type = match g.node(prod).unwrap() {
            Node::Concept(c) => c.concept_type,
            _ => panic!(),
        };
        let dep_type = match g.node(dep).unwrap() {
            Node::Concept(c) => c.concept_type,
            _ => panic!(),
        };
        assert_eq!(prod_type, ConceptType::Resource);
        assert_eq!(dep_type, ConceptType::Entity);

        // Causal to produces/modifies, Dependency to depends_on; direction is
        // action -> target, never the reverse.
        let causal_prod = g
            .edge_between(out.action_node, prod, EdgeType::Causal)
            .expect("causal to produce");
        assert_eq!(causal_prod.weight, CAUSAL_WEIGHT);
        assert!(g
            .edge_between(prod, out.action_node, EdgeType::Causal)
            .is_none());
        assert!(g
            .edge_between(out.action_node, modi, EdgeType::Causal)
            .is_some());
        let dep_edge = g
            .edge_between(out.action_node, dep, EdgeType::Dependency)
            .expect("dependency to depends_on");
        assert_eq!(dep_edge.weight, DEPENDENCY_WEIGHT);
        assert!(g
            .edge_between(dep, out.action_node, EdgeType::Dependency)
            .is_none());

        // Outcome counts: 3 planned edges, all new.
        assert_eq!(out.edges, 3);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn record_action_resolves_existing_concepts_and_creates_missing() {
        let (mut g, iid) = graph_with_interaction();
        // Pre-existing concept; canonicalize("user schema") -> key "schema user".
        let us = concept(1, iid, "user schema", "schema user");
        let us_id = us.id;
        g.insert_concept(us, iid).unwrap();
        let nodes_before = g.node_count();

        let act = action(
            "created migrations/003.sql",
            &["migrations/003.sql"],
            &[],
            &["user schema"],
        );
        let out = record_action(&mut g, iid, &agent(), &act).unwrap();

        // depends_on resolved to the EXISTING concept; only the action + the
        // produce are created.
        assert_eq!(out.created.len(), 2);
        assert!(!out.created.contains(&us_id));
        assert!(g
            .edge_between(out.action_node, us_id, EdgeType::Dependency)
            .is_some());
        assert_eq!(g.node_count(), nodes_before + 2);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn record_action_rejects_missing_or_non_interaction() {
        let (mut g, iid) = graph_with_interaction();
        let act = action("x", &[], &[], &[]);

        // Unknown node id.
        let err = record_action(&mut g, uid(999), &agent(), &act).unwrap_err();
        assert!(matches!(err, LamboError::Store(StoreError::NotFound(_))));

        // A concept id is not an interaction.
        let c = concept(1, iid, "c", "c");
        let cid = c.id;
        g.insert_concept(c, iid).unwrap();
        let log_before = g.log_len();
        let err = record_action(&mut g, cid, &agent(), &act).unwrap_err();
        assert!(matches!(err, LamboError::Store(StoreError::NotFound(_))));

        // Rejections leave the graph untouched (no log entries appended).
        assert_eq!(g.log_len(), log_before);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn record_action_rejects_dependency_cycle_and_leaves_graph_unchanged() {
        let (mut g, iid) = graph_with_interaction();
        // X -> Y (action "x" depends on "y").
        // Labels are single letters but NOT stopwords: "a"/"an" canonicalize
        // to an empty key and are rejected by the GRAPH-8 content guard.
        let out_a = record_action(&mut g, iid, &agent(), &action("x", &[], &[], &["y"])).unwrap();
        assert_eq!(out_a.created.len(), 2);
        let a_id = out_a.action_node;
        let b_id = out_a.created[1];
        assert!(g.edge_between(a_id, b_id, EdgeType::Dependency).is_some());

        // B -> A would close the cycle. Must be rejected BEFORE any mutation.
        let before = g.snapshot();
        let log_before = g.log_len();
        let epoch_before = g.epoch();
        let err = record_action(&mut g, iid, &agent(), &action("y", &[], &[], &["x"])).unwrap_err();
        assert!(matches!(err, LamboError::Store(StoreError::Invariant(_))));
        assert!(err.to_string().contains("would create a cycle"));

        // Byte-identical: snapshot, mutation log, and epoch all unchanged.
        assert_eq!(g.snapshot(), before);
        assert_eq!(g.log_len(), log_before);
        assert_eq!(g.epoch(), epoch_before);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn record_action_rejects_three_hop_chain_cycle() {
        let (mut g, iid) = graph_with_interaction();
        record_action(&mut g, iid, &agent(), &action("x", &[], &[], &["y"])).unwrap();
        record_action(&mut g, iid, &agent(), &action("y", &[], &[], &["z"])).unwrap();
        let before = g.snapshot();
        let log_before = g.log_len();

        // c -> a closes a -> b -> c.
        let err = record_action(&mut g, iid, &agent(), &action("z", &[], &[], &["x"])).unwrap_err();
        assert!(matches!(err, LamboError::Store(StoreError::Invariant(_))));
        assert!(err.to_string().contains("would create a cycle"));
        assert_eq!(g.snapshot(), before);
        assert_eq!(g.log_len(), log_before);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn record_action_rejects_self_referential_planned_edges() {
        let (mut g, iid) = graph_with_interaction();
        let before = g.snapshot();
        // Baseline includes the interaction's own UpsertNode log entry.
        let log_before = g.log_len();

        // Action node in its own produces: planned X -> X Causal self-loop.
        let err = record_action(&mut g, iid, &agent(), &action("x", &["x"], &[], &[])).unwrap_err();
        assert!(matches!(err, LamboError::Store(StoreError::Invariant(_))));
        assert_eq!(g.snapshot(), before);
        assert_eq!(g.log_len(), log_before);

        // Same via depends_on: planned Y -> Y Dependency self-loop.
        let err = record_action(&mut g, iid, &agent(), &action("y", &[], &[], &["y"])).unwrap_err();
        assert!(matches!(err, LamboError::Store(StoreError::Invariant(_))));
        assert_eq!(g.snapshot(), before);
        assert_eq!(g.log_len(), log_before);
    }

    #[test]
    fn record_action_rejects_cycle_closing_existing_edges() {
        let (mut g, iid) = graph_with_interaction();
        // Seed an existing structural edge b -> c (upsert_edge stores what it
        // is given — write-time rejection is this module's job, T2.1). Note
        // "a" is a stopword (canonical key ""), so the seeds use "b"/"c".
        let b = concept(1, iid, "b", "b");
        let c = concept(2, iid, "c", "c");
        let b_id = b.id;
        let c_id = c.id;
        g.insert_concept(b, iid).unwrap();
        g.insert_concept(c, iid).unwrap();
        g.upsert_edge(edge(1, b_id, c_id, EdgeType::Dependency, 0.7))
            .unwrap();

        let before = g.snapshot();
        let log_before = g.log_len();
        let epoch_before = g.epoch();

        // Action "c" (matches the existing concept) depends on "b": planned
        // c -> b closes the existing b -> c.
        let err = record_action(&mut g, iid, &agent(), &action("c", &[], &[], &["b"])).unwrap_err();
        assert!(matches!(err, LamboError::Store(StoreError::Invariant(_))));
        assert!(err.to_string().contains("would create a cycle"));
        assert_eq!(g.snapshot(), before);
        assert_eq!(g.log_len(), log_before);
        assert_eq!(g.epoch(), epoch_before);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn record_action_orders_nodes_before_edges_in_log() {
        let (mut g, iid) = graph_with_interaction();
        record_action(
            &mut g,
            iid,
            &agent(),
            &action("act", &["prod"], &[], &["dep"]),
        )
        .unwrap();

        let batch = g.drain_log();
        let mut seen_nodes: HashSet<NodeId> = HashSet::new();
        let mut edge_writes = 0usize;
        for m in &batch.mutations {
            match m {
                Mutation::UpsertNode { node } => {
                    seen_nodes.insert(node.id());
                }
                Mutation::UpsertEdge { edge } => {
                    assert!(
                        seen_nodes.contains(&edge.source),
                        "edge {edge:?}: source not upserted earlier in the batch"
                    );
                    assert!(
                        seen_nodes.contains(&edge.target),
                        "edge {edge:?}: target not upserted earlier in the batch"
                    );
                    edge_writes += 1;
                }
                other => panic!("unexpected mutation in batch: {other:?}"),
            }
        }
        // 1 interaction + 3 concepts = 4 node upserts; 3 Derives + Causal +
        // Dependency = 5 edge upserts, all after their endpoints.
        assert_eq!(batch.mutations.len(), 9);
        assert_eq!(edge_writes, 5);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn record_action_dedups_within_call() {
        let (mut g, iid) = graph_with_interaction();
        // "x" appears twice in produces and once in depends_on.
        let act = action("act", &["x", "x"], &[], &["x"]);
        let out = record_action(&mut g, iid, &agent(), &act).unwrap();

        // One concept for "x" (first encounter wins: Resource from produces),
        // one Causal + one Dependency edge.
        assert_eq!(out.created.len(), 2);
        assert_eq!(out.edges, 2);
        let x = out.created[1];
        assert_eq!(
            g.out_neighbors_typed(out.action_node, EdgeType::Causal),
            vec![x]
        );
        assert_eq!(
            g.out_neighbors_typed(out.action_node, EdgeType::Dependency),
            vec![x]
        );
        g.assert_invariants().unwrap();
    }

    #[test]
    fn record_action_repeat_is_reinforcement_not_duplication() {
        let (mut g, iid) = graph_with_interaction();
        let act = action("deploy", &["app"], &[], &["db"]);
        let first = record_action(&mut g, iid, &agent(), &act).unwrap();
        let nodes_before = g.node_count();
        let edges_before = g.edge_count();

        // Re-recording the same action: same action node, nothing created,
        // nothing inserted — existing edges reinforce.
        let second = record_action(&mut g, iid, &agent(), &act).unwrap();
        assert_eq!(second.action_node, first.action_node);
        assert!(second.created.is_empty());
        assert_eq!(second.edges, 0);
        assert_eq!(g.node_count(), nodes_before);
        assert_eq!(g.edge_count(), edges_before);

        let dep = g
            .edge_between(first.action_node, first.created[2], EdgeType::Dependency)
            .unwrap();
        assert_eq!(dep.reinforcements, 2);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn record_action_rejects_empty_and_stopword_only_content() {
        // GRAPH-8: empty/whitespace-only/stopword-only content would collapse
        // onto one key-"" concept — rejected at the entry point during the
        // read-only planning phase, before anything is written.
        let (mut g, iid) = graph_with_interaction();
        // Blank action string.
        let err = record_action(&mut g, iid, &agent(), &action("", &[], &[], &[])).unwrap_err();
        assert!(
            matches!(&err, LamboError::Store(StoreError::Invariant(_))),
            "{err}"
        );
        assert!(err.to_string().contains("empty key"), "{err}");
        // Stopword-only action.
        let err =
            record_action(&mut g, iid, &agent(), &action("the and of", &[], &[], &[])).unwrap_err();
        assert!(matches!(&err, LamboError::Store(StoreError::Invariant(_))));
        // Empty produce.
        let err = record_action(&mut g, iid, &agent(), &action("deploy", &["   "], &[], &[]))
            .unwrap_err();
        assert!(matches!(&err, LamboError::Store(StoreError::Invariant(_))));
        // Stopword-only dependency.
        let err = record_action(
            &mut g,
            iid,
            &agent(),
            &action("deploy", &[], &[], &["the a an"]),
        )
        .unwrap_err();
        assert!(matches!(&err, LamboError::Store(StoreError::Invariant(_))));
        // Nothing was written by any rejected call.
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.log_len(), 1);
        g.assert_invariants().unwrap();
    }
}

//! `derive()` — the primary write path (T2.3), spec §7.
//!
//! Flow, exactly per spec §7: per concept — canonicalize (T2.2) → within-call
//! dedup → match or create → `Derives` edge from the current interaction →
//! pairwise `CoOccurrence` capped at `max_cooccurrence_per_derive` →
//! `Hierarchical` edges from `ParentOf` → mutation batch (every write appends to
//! `Graph`'s ordered log; [`derive`] emits no mutations itself) → daemon notify
//! (see the seam below).
//!
//! ## Derives edges — why `insert_concept`, not `upsert_edge`
//!
//! The pinned flow says step 4 "ensures a Derives edge from `interaction` for
//! every concept". That step is realized by [`Graph::insert_concept`]'s
//! structural edge for **both** outcomes:
//!
//! * **Unmatched → created:** `insert_concept` writes the node *and* the
//!   `Derives` edge (weight 0.9, `reinforcements = 1`, per the fixture
//!   convention). A second `upsert_edge` for the same natural key would
//!   immediately reinforce it (0.9 → 1.9 on first derive) — that is why created
//!   concepts are not re-written.
//! * **Matched → reused:** the node is **re-upserted** with `insert_concept`.
//!   This is the only public write path that emits the required `UpsertNode`,
//!   keeping the §2.4 mutation-ordering contract ("every `UpsertEdge`'s
//!   endpoints were `UpsertNode`'d earlier in the same batch") when derive
//!   writes edges to pre-existing concepts. The re-upsert is idempotent (same
//!   id, stored `Concept` cloned as-is — `gc_survived`/status/etc. preserved),
//!   and its structural `Derives` write *creates* the edge when absent (new
//!   interaction re-deriving an existing concept) or *reinforces* it when
//!   present (same interaction re-deriving — "re-derives reinforce", T2.1
//!   natural-key semantics).
//!
//! Reinforcement counting (step 7) uses [`Graph::edge_between`] *before* each
//! write: if the natural key already exists, the write is a duplicate and bumps
//! the edge (counted once per duplicate write, across `Derives` /
//! `CoOccurrence` / `Hierarchical`).
//!
//! ## Within-call dedup
//!
//! Duplicate *contents* in `concepts` are processed once (first occurrence
//! wins, including its `ConceptType`). Two different contents that canonicalize
//! to the same key are collapsed by the matcher: every colliding content
//! resolves `Matched` to the same node and is recorded in `outcome.matched`.
//! The within-call guard tracks every node **written** this call — created, or
//! matched and re-upserted — so a later resolution to a node already written
//! earlier in the call is skipped entirely (no re-upsert, no reinforcement
//! bump): a node is never written twice within one call, and one call can never
//! self-reinforce. `ParentOf` pairs are deduped on the **resolved**
//! `(parent_node, child_node)` pair, so two pairs whose ends canonicalize to
//! the same node pair write exactly one `Hierarchical` edge (re-resolving the
//! duplicate pair's ends is a no-op thanks to the guard).
//!
//! ## Timestamps
//!
//! [`derive`] is synchronous and takes no clock, so every timestamp derives
//! from the interaction node's `created_at` (deterministic, rebuild-test
//! friendly): new concepts are stamped with it, and derive-created
//! `CoOccurrence`/`Hierarchical` edges carry it as `created_at` /
//! `last_reinforced`. `Derives` edges follow [`Graph::insert_concept`]'s
//! convention (edge stamped with the concept's `created_at`).
//!
//! ## `ParentOf` contents
//!
//! Both ends of every pair resolve through the same canonicalize/create path as
//! the `concepts` argument (they may be new or existing). A brand-new content
//! appearing only in `ParentOf` is created as a concept too, with
//! [`PARENT_OF_CONCEPT_TYPE`] (`Entity`) and the same `Derives` edge from the
//! interaction. `ParentOf` contents do **not** join the pairwise `CoOccurrence`
//! step (that is scoped to the call's `concepts` argument). A pair whose ends
//! resolve to the same node is rejected up front — a `Hierarchical` self-loop
//! is a cycle (§5.7 / adve-review T2.1 M1) and must never be written.
//!
//! ## Validate-then-mutate (no partial writes)
//!
//! [`derive`] validates every fallible input up front in a read-only pre-pass
//! (interaction exists; raw- and resolved-reflexive `parent_of` pairs), so the
//! write loop that follows cannot fail mid-call — an error never leaves a
//! partial batch in the graph or the mutation log (muse-spark M4). The loop's
//! reflexive checks remain as unreachable defense-in-depth.
//!
//! ## Daemon notify seam (T4.x — do NOT build the channel here)
//!
//! [`derive`] is the write path the daemon will subscribe to: T4.x's daemon
//! consumes the session's mutation stream and reacts to derive-created
//! conflicts / drift / staleness. The notify hook (a channel send, spec §6.1
//! `DaemonEvent`) is **deferred to T4** — there is nothing to send to yet, so
//! this module deliberately contains no stub functions and no channel types.
//! When T4 lands, the send belongs *after* the successful graph writes here
//! (or, more likely, inside the flush path that drains `MutationBatch`), and
//! the receiver-side contract is `DaemonEvent` (already in `crate::types`).

use std::collections::HashSet;

use chrono::{DateTime, Utc};

use crate::graph::canonical::{canonicalize, CanonicalizeResult};
use crate::graph::Graph;
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, LamboError, Node, NodeId,
    SessionId, StoreError,
};

/// Initial weight of `CoOccurrence` edges written by [`derive`] (module
/// constant per task contract; `Derives`/`Temporal` are Graph-owned at 0.9/1.0).
pub const COOCCURRENCE_WEIGHT: f64 = 0.5;

/// Initial weight of `Hierarchical` edges written by [`derive`] (module
/// constant per task contract).
pub const HIERARCHICAL_WEIGHT: f64 = 0.5;

/// `ConceptType` assigned to contents that appear only in [`ParentOf`] pairs.
/// `derive`'s caller supplies types only for the `concepts` argument; a
/// parent/child that does not already exist must still become a concept, and
/// `Entity` is the generic default (documented decision — T2.3 Handoff Log).
pub const PARENT_OF_CONCEPT_TYPE: ConceptType = ConceptType::Entity;

/// Declaration of `Hierarchical` parent→child relationships for one [`derive`]
/// call. Contents are resolved through the same canonicalize/create path as the
/// `concepts` argument.
#[derive(Clone, Copy, Debug)]
pub struct ParentOf<'a> {
    pairs: &'a [(&'a str, &'a str)],
}

impl<'a> ParentOf<'a> {
    /// No hierarchical relationships.
    pub fn none() -> Self {
        Self { pairs: &[] }
    }

    /// Hierarchical pairs `(parent, child)` in declaration order. Both ends of
    /// each pair resolve (and may be created) as concepts.
    pub fn from_pairs(pairs: &'a [(&'a str, &'a str)]) -> Self {
        Self { pairs }
    }

    /// Raw `(parent, child)` pairs in declaration order (read-only accessor so
    /// the async hybrid twin in [`crate::graph::hybrid`] can mirror derive's
    /// validation + Hierarchical edge steps without owning the field).
    pub fn pairs(&self) -> &'a [(&'a str, &'a str)] {
        self.pairs
    }
}
/// Summary of one `derive` call — returned by both the sync
/// [`derive`] and its async hybrid twin ([`crate::graph::hybrid::derive`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeriveOutcome {
    /// Concepts created by this call (fresh nodes), in resolution order:
    /// `concepts` argument first, then `ParentOf` contents.
    pub created: Vec<NodeId>,
    /// Concepts matched to pre-existing nodes (or to a node created earlier in
    /// this same call), in the same resolution order. `matched` is faithful to
    /// the `derive` contract: every entry here was re-upserted (or, within the
    /// same call, written) and carries/reinforces a `Derives` edge from the
    /// interaction. Hybrid semantic merges are NOT recorded here — see
    /// [`Self::semantic_merged`].
    pub matched: Vec<NodeId>,
    /// Targets of hybrid semantic merges (only populated by
    /// [`crate::graph::hybrid::derive`]): pre-existing concepts a hybrid write
    /// merged against via a decaying `Semantic` edge. Kept separate from
    /// [`Self::matched`] because a merge does NOT re-upsert the target nor add
    /// a `Derives` edge to it — `matched` must retain its "re-derived /
    /// Derives-reinforced" meaning for sync `derive` consumers (PHASE-7 T7.2
    /// remediation, MINOR-3).
    pub semantic_merged: Vec<NodeId>,
    /// Number of duplicate natural-key writes in this call that bumped an
    /// existing edge (`Derives` / `CoOccurrence` / `Hierarchical`).
    pub reinforced: usize,
}

/// The primary write path (spec §7): derive `concepts` from `interaction`,
/// declared `ParentOf` hierarchies, and pairwise `CoOccurrence` among the
/// call's concepts.
///
/// * `interaction` must name an existing `Interaction` node — otherwise
///   `NotFound`.
/// * `concepts` are deduplicated by content (first occurrence wins), then each
///   is canonicalized (T2.2): an unmatched key creates a concept (with an
///   automatic `Derives` edge from `interaction`); a matched key reuses the
///   existing node (re-upserted so the mutation batch stays well-ordered).
/// * Every concept in the call is guaranteed a `Derives` edge from
///   `interaction` (created or reinforced — see module docs).
/// * Pairwise `CoOccurrence` edges in call order, at most
///   `max_cooccurrence_per_derive` per call, initial weight
///   [`COOCCURRENCE_WEIGHT`].
/// * `parent_of` pairs become `Hierarchical` edges, initial weight
///   [`HIERARCHICAL_WEIGHT`].
///
/// Returns the [`DeriveOutcome`] (created / matched / reinforced counts). All
/// mutations flow through `Graph`'s write APIs into its ordered log; [`derive`]
/// is synchronous and pure of I/O (the daemon notify hook is T4.x's, see module
/// docs).
pub fn derive(
    graph: &mut Graph,
    interaction: NodeId,
    agent: &AgentId,
    concepts: &[(&str, ConceptType)],
    parent_of: &ParentOf,
    max_cooccurrence_per_derive: usize,
) -> Result<DeriveOutcome, LamboError> {
    // Step 1 — validate the interaction node. Both a missing node and a node
    // that is not an Interaction are NotFound per the pinned contract.
    let interaction_created_at = match graph.node(interaction) {
        Some(Node::Interaction(i)) => i.created_at,
        Some(_) => {
            return Err(LamboError::Store(StoreError::NotFound(format!(
                "derive: node {interaction} exists but is not an Interaction"
            ))))
        }
        None => {
            return Err(LamboError::Store(StoreError::NotFound(format!(
                "derive: interaction node {interaction} not found in graph"
            ))))
        }
    };
    let session_id = graph.session_id().clone();

    // Pre-pass (read-only): hoist every fallible input check here so the write
    // loop below cannot fail mid-call — derive is validate-then-mutate, never
    // partially applied (muse-spark M4). canonicalize is read-only, so nothing
    // is written by this pass. Raw-reflexive AND resolved-reflexive parent_of
    // pairs (both ends canonicalize to the same key -> same node) are rejected
    // before any concept or edge exists.
    // GRAPH-8: content that canonicalizes to an empty key (empty, whitespace-
    // only, or stopword-only input) would collapse onto one key-"" concept with
    // a frozen arbitrary type — rejected up front, before any write.
    for &(content, _) in concepts {
        let key = match canonicalize(content, graph)? {
            CanonicalizeResult::Matched { key, .. } | CanonicalizeResult::Unmatched { key } => key,
        };
        reject_empty_key(content, &key)?;
    }
    for &(parent, child) in parent_of.pairs {
        if parent == child {
            return Err(LamboError::Store(StoreError::Invariant(format!(
                "derive: parent_of pair ({parent}, {child}) is reflexive — a Hierarchical \
                 self-loop is a cycle (spec §5.7)"
            ))));
        }
        let parent_key = match canonicalize(parent, graph)? {
            CanonicalizeResult::Matched { key, .. } | CanonicalizeResult::Unmatched { key } => key,
        };
        let child_key = match canonicalize(child, graph)? {
            CanonicalizeResult::Matched { key, .. } | CanonicalizeResult::Unmatched { key } => key,
        };
        reject_empty_key(parent, &parent_key)?;
        reject_empty_key(child, &child_key)?;
        if parent_key == child_key {
            return Err(LamboError::Store(StoreError::Invariant(format!(
                "derive: parent_of pair ({parent}, {child}) resolves to the same canonical \
                 key ({parent_key:?}) — a Hierarchical self-loop is a cycle (spec §5.7)"
            ))));
        }
    }

    let mut outcome = DeriveOutcome::default();
    // Nodes written earlier in THIS call (created, or matched and re-upserted).
    // Their node and Derives edge are already written; a later content that
    // canonicalizes to the same node (key collision) must not re-upsert and
    // accidentally reinforce the edge just written — one call never
    // self-reinforces.
    let mut written_this_call: HashSet<NodeId> = HashSet::new();

    // Steps 2–4 — dedup by content, canonicalize, match-or-create, and ensure
    // the Derives edge (via insert_concept's structural edge; see module docs).
    let mut seen_contents: HashSet<&str> = HashSet::with_capacity(concepts.len());
    let mut call_nodes: Vec<NodeId> = Vec::with_capacity(concepts.len());
    for &(content, concept_type) in concepts {
        if !seen_contents.insert(content) {
            continue;
        }
        let node = resolve_concept(
            graph,
            content,
            concept_type,
            interaction,
            agent,
            interaction_created_at,
            &session_id,
            &mut written_this_call,
            &mut outcome,
        )?;
        // Two different contents can canonicalize to the same node (key
        // collision); dedup by node id so the CoOccurrence step never writes a
        // self-loop for them.
        if !call_nodes.contains(&node) {
            call_nodes.push(node);
        }
    }

    // Step 5 — pairwise CoOccurrence among the call's concepts (created +
    // matched, deduped), in call order, capped at max_cooccurrence_per_derive
    // edges per call. Direction: earlier-in-call -> later-in-call (CoOccurrence
    // is symmetric; this convention keeps writes deterministic). Policy note
    // (muse-spark S4): the cap biases connectivity toward early concepts in a
    // large call (first 10 pairs = pairs among roughly the first 5 concepts of
    // 8) — spec-§7-allowed, flag for P5 if recall balancing ever cares.
    let mut written = 0usize;
    'pairs: for i in 0..call_nodes.len() {
        for j in (i + 1)..call_nodes.len() {
            if written >= max_cooccurrence_per_derive {
                break 'pairs;
            }
            let (source, target) = pair_direction(graph, call_nodes[i], call_nodes[j]);
            if graph
                .edge_between(source, target, EdgeType::CoOccurrence)
                .is_some()
            {
                outcome.reinforced += 1;
            }
            graph.upsert_edge(Edge {
                id: NodeId::new(),
                session_id: session_id.clone(),
                source,
                target,
                edge_type: EdgeType::CoOccurrence,
                weight: COOCCURRENCE_WEIGHT,
                reinforcements: 1,
                created_at: interaction_created_at,
                last_reinforced: interaction_created_at,
            })?;
            written += 1;
        }
    }

    // Step 6 — Hierarchical edges from parent_of. Reflexive pairs (raw-content
    // equal, or canonicalizing to the same node) are rejected up front: a
    // Hierarchical self-loop is a cycle and would trip assert_invariants.
    // Pairs are deduped on the RESOLVED (parent_node, child_node) pair: two
    // pairs whose ends canonicalize to the same node pair (key collision) must
    // write exactly one Hierarchical edge. Re-resolving the duplicate pair's
    // ends is a write no-op — the within-call guard skips already-written
    // nodes (only recording them in outcome.matched).
    let mut seen_pairs: HashSet<(NodeId, NodeId)> = HashSet::with_capacity(parent_of.pairs.len());
    for &(parent, child) in parent_of.pairs {
        // Defense-in-depth: both reflexive checks were hoisted to the pre-pass
        // (validate-then-mutate, muse-spark M4) and are unreachable here — kept
        // so a future edit cannot silently reintroduce a reflexive write.
        if parent == child {
            return Err(LamboError::Store(StoreError::Invariant(format!(
                "derive: parent_of pair ({parent}, {child}) is reflexive — a Hierarchical \
                 self-loop is a cycle (spec §5.7)"
            ))));
        }
        let parent_node = resolve_concept(
            graph,
            parent,
            PARENT_OF_CONCEPT_TYPE,
            interaction,
            agent,
            interaction_created_at,
            &session_id,
            &mut written_this_call,
            &mut outcome,
        )?;
        let child_node = resolve_concept(
            graph,
            child,
            PARENT_OF_CONCEPT_TYPE,
            interaction,
            agent,
            interaction_created_at,
            &session_id,
            &mut written_this_call,
            &mut outcome,
        )?;
        if parent_node == child_node {
            return Err(LamboError::Store(StoreError::Invariant(format!(
                "derive: parent_of pair ({parent}, {child}) resolves to the same concept \
                 {parent_node} — a Hierarchical self-loop is a cycle (spec §5.7)"
            ))));
        }
        if !seen_pairs.insert((parent_node, child_node)) {
            // Same resolved node pair as an earlier pair in this call — the
            // Hierarchical edge was already written; skip the duplicate.
            continue;
        }
        if graph
            .edge_between(parent_node, child_node, EdgeType::Hierarchical)
            .is_some()
        {
            outcome.reinforced += 1;
        }
        graph.upsert_edge(Edge {
            id: NodeId::new(),
            session_id: session_id.clone(),
            source: parent_node,
            target: child_node,
            edge_type: EdgeType::Hierarchical,
            weight: HIERARCHICAL_WEIGHT,
            reinforcements: 1,
            created_at: interaction_created_at,
            last_reinforced: interaction_created_at,
        })?;
    }

    Ok(outcome)
}

/// Canonicalize `content` and either create the concept (unmatched key) or
/// reuse the existing node (matched key), guaranteeing a `Derives` edge from
/// `interaction` in both cases (created by [`Graph::insert_concept`]'s
/// structural edge; reinforced when the natural key already exists). Records
/// the node in `outcome.created` / `outcome.matched` and counts Derives
/// reinforcements in `outcome.reinforced`. Every node written this call
/// (created, or matched and re-upserted) is tracked in `written_this_call`; a
/// later resolution to the same node (canonical-key collision) skips the write
/// and the reinforcement entirely — one call never self-reinforces — but still
/// records the match.
// Private helper: every parameter is a distinct input with no natural grouping
// (a params struct would obscure the call sites); kept under review by tests.
#[allow(clippy::too_many_arguments)]
fn resolve_concept(
    graph: &mut Graph,
    content: &str,
    concept_type: ConceptType,
    interaction: NodeId,
    agent: &AgentId,
    created_at: DateTime<Utc>,
    session_id: &SessionId,
    written_this_call: &mut HashSet<NodeId>,
    outcome: &mut DeriveOutcome,
) -> Result<NodeId, LamboError> {
    match canonicalize(content, graph)? {
        CanonicalizeResult::Unmatched { key } => {
            let concept = Concept {
                id: NodeId::new(),
                session_id: session_id.clone(),
                content: content.to_string(),
                canonical_key: key,
                concept_type,
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
            let id = concept.id;
            // Writes UpsertNode + the structural Derives edge (weight 0.9).
            graph.insert_concept(concept, interaction)?;
            written_this_call.insert(id);
            outcome.created.push(id);
            Ok(id)
        }
        CanonicalizeResult::Matched { node, .. } => {
            if written_this_call.contains(&node) {
                // Node + Derives edge already written earlier in this call
                // (created, or matched and re-upserted). A canonical-key
                // collision resolving to it again must not re-upsert — that
                // would self-reinforce the just-written Derives edge. Record
                // the match and return.
                outcome.matched.push(node);
                return Ok(node);
            }
            let existing = match graph.node(node) {
                Some(Node::Concept(c)) => c.clone(),
                _ => {
                    return Err(LamboError::Store(StoreError::Invariant(format!(
                        "derive: canonicalize matched {node} but the stored node is not a Concept"
                    ))))
                }
            };
            // Re-upsert the node (keeps the drained batch well-ordered: every
            // edge endpoint was node-upserted earlier in the batch) and ensure
            // the Derives edge from `interaction`. A duplicate natural key
            // (re-derive from the same interaction) reinforces it — counted.
            if graph
                .edge_between(interaction, node, EdgeType::Derives)
                .is_some()
            {
                outcome.reinforced += 1;
            }
            graph.insert_concept(existing, interaction)?;
            written_this_call.insert(node);
            outcome.matched.push(node);
            Ok(node)
        }
    }
}

/// CoOccurrence is symmetric (module doc), so a pair may already exist in
/// either direction: a swapped-order re-derive (`derive([x,y])` then
/// `derive([y,x])`) must reinforce the existing edge, not create a reverse
/// duplicate (adve-review GRAPH-5 — which would violate the T2.3 exit criterion
/// and double-count the pair in P4's density dimension). Fresh edges keep the
/// deterministic earlier-in-call -> later-in-call direction; when only the
/// reverse edge exists, its direction is adopted so the natural-key dedup
/// reinforces instead of inserting a second edge.
fn pair_direction(graph: &Graph, a: NodeId, b: NodeId) -> (NodeId, NodeId) {
    if graph.edge_between(a, b, EdgeType::CoOccurrence).is_some() {
        (a, b)
    } else if graph.edge_between(b, a, EdgeType::CoOccurrence).is_some() {
        (b, a)
    } else {
        (a, b)
    }
}

/// GRAPH-8 guard: reject content whose canonical key is empty — empty,
/// whitespace-only, or stopword-only input would all collapse onto one key-""
/// concept with a frozen arbitrary type. Typed `Invariant` error, raised at the
/// entry point before anything is written.
fn reject_empty_key(content: &str, key: &str) -> Result<(), LamboError> {
    if key.is_empty() {
        return Err(LamboError::Store(StoreError::Invariant(format!(
            "derive: content {:?} canonicalizes to an empty key (empty, whitespace-only, \
             or stopword-only content is rejected)",
            content
        ))));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    use crate::graph::canonical::canonical_key;
    use crate::graph::demote::demote;
    use crate::types::{Interaction, Mutation};

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

    /// Same shape as graph.rs's `concept()` helper (chunk_group_id: None);
    /// `canonical_key` is computed the way derive's matcher would, so a
    /// pre-seeded concept is actually matchable by `derive`.
    fn concept(id: u64, origin: NodeId, content: &str) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(2, id)),
            session_id: sid(),
            content: content.into(),
            canonical_key: canonical_key(content, |_| None),
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

    /// Fresh graph with one interaction (chain head), returning (graph, iid).
    fn graph_with_interaction(id: u64, at_min: i64) -> (Graph, NodeId) {
        let mut g = Graph::new(sid());
        let i = interaction(id, None, at_min);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        (g, iid)
    }

    fn derives_of(g: &Graph, concept: NodeId) -> Vec<NodeId> {
        g.in_neighbors_typed(concept, EdgeType::Derives)
    }

    fn concept_of(g: &Graph, id: NodeId) -> &Concept {
        match g.node(id) {
            Some(Node::Concept(c)) => c,
            other => panic!("{id} is not a concept node: {other:?}"),
        }
    }

    #[test]
    fn derive_creates_concepts_derives_and_cooccurrence() {
        let (mut g, iid) = graph_with_interaction(1, 0);
        let out = derive(
            &mut g,
            iid,
            &agent(),
            &[
                ("user schema", ConceptType::Entity),
                ("auth middleware", ConceptType::Logic),
            ],
            &ParentOf::none(),
            10,
        )
        .unwrap();

        assert_eq!(out.created.len(), 2);
        assert!(out.matched.is_empty());
        assert_eq!(out.reinforced, 0);

        let a = g.node(out.created[0]).unwrap();
        let b = g.node(out.created[1]).unwrap();
        assert!(matches!(a, Node::Concept(_)));
        assert!(matches!(b, Node::Concept(_)));

        // Two Derives edges, weight 0.9 / reinforcements 1 (fresh, NOT bumped).
        for &cid in &out.created {
            let d = g
                .edge_between(iid, cid, EdgeType::Derives)
                .expect("derives");
            assert_eq!(d.weight, 0.9);
            assert_eq!(d.reinforcements, 1);
        }
        // One CoOccurrence edge, weight 0.5.
        let c = g
            .edge_between(out.created[0], out.created[1], EdgeType::CoOccurrence)
            .expect("cooccurrence");
        assert_eq!(c.weight, 0.5);
        assert_eq!(g.edge_count(), 3); // derives x2 + cooccurrence
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_dedupes_within_call() {
        let (mut g, iid) = graph_with_interaction(1, 0);
        let out = derive(
            &mut g,
            iid,
            &agent(),
            &[
                ("user schema", ConceptType::Entity),
                ("user schema", ConceptType::Logic), // duplicate content — first wins
            ],
            &ParentOf::none(),
            10,
        )
        .unwrap();

        assert_eq!(out.created.len(), 1);
        assert!(out.matched.is_empty());
        assert_eq!(g.node_count(), 2); // interaction + one concept
        assert_eq!(g.edge_count(), 1); // single Derives
        assert_eq!(derives_of(&g, out.created[0]), vec![iid]);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_matches_existing_concept_from_new_interaction() {
        let (mut g, i1) = graph_with_interaction(1, 0);
        let i2 = interaction(2, Some(i1), 5);
        let i2id = i2.id;
        g.insert_interaction(i2).unwrap();
        // Pre-seed a concept via the Graph API (canonical_key computed the same
        // way derive's matcher does).
        let c = concept(1, i1, "user schema");
        let cid = c.id;
        g.insert_concept(c, i1).unwrap();

        let out = derive(
            &mut g,
            i2id,
            &agent(),
            &[("user schema", ConceptType::Entity)],
            &ParentOf::none(),
            10,
        )
        .unwrap();

        assert!(out.created.is_empty(), "matched concepts are NOT recreated");
        assert_eq!(out.matched, vec![cid]);
        assert_eq!(out.reinforced, 0);
        assert_eq!(g.node_count(), 3); // i1, i2, c — nothing new

        // New interaction -> exactly one new Derives edge (i2 -> c).
        let mut derives = derives_of(&g, cid);
        derives.sort_by_key(|id| id.0);
        assert_eq!(derives, {
            let mut v = vec![i1, i2id];
            v.sort_by_key(|id| id.0);
            v
        });
        let d = g.edge_between(i2id, cid, EdgeType::Derives).unwrap();
        assert_eq!(d.reinforcements, 1, "fresh edge, not reinforced");
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_caps_cooccurrence_edges() {
        let (mut g, iid) = graph_with_interaction(1, 0);
        let out = derive(
            &mut g,
            iid,
            &agent(),
            &[
                // Single-letter labels, but not stopwords — GRAPH-8 rejects
                // content that canonicalizes to an empty key ("a"/"an" are
                // stopwords).
                ("x", ConceptType::Entity),
                ("y", ConceptType::Entity),
                ("z", ConceptType::Entity),
                ("w", ConceptType::Entity),
            ],
            &ParentOf::none(),
            2, // cap: exactly 2 CoOccurrence edges for 4 concepts
        )
        .unwrap();

        assert_eq!(out.created.len(), 4);
        let [a, b, c, d] = [
            out.created[0],
            out.created[1],
            out.created[2],
            out.created[3],
        ];
        // Deterministic call-order pairs (0,1) and (0,2).
        assert!(g.edge_between(a, b, EdgeType::CoOccurrence).is_some());
        assert!(g.edge_between(a, c, EdgeType::CoOccurrence).is_some());
        assert!(g.edge_between(a, d, EdgeType::CoOccurrence).is_none());
        assert!(g.edge_between(b, c, EdgeType::CoOccurrence).is_none());
        assert!(g.edge_between(c, d, EdgeType::CoOccurrence).is_none());
        assert_eq!(out.reinforced, 0);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_creates_hierarchical_edges_and_parent_of_concepts() {
        let (mut g, i1) = graph_with_interaction(1, 0);
        let i2 = interaction(2, Some(i1), 5);
        let i2id = i2.id;
        g.insert_interaction(i2).unwrap();
        // "user schema" is a real concept (derived from i1); "api layer" is
        // brand-new and must be created by the parent_of resolution (as Entity).
        let pre = concept(1, i1, "user schema");
        let pre_id = pre.id;
        g.insert_concept(pre, i1).unwrap();

        let out = derive(
            &mut g,
            i2id,
            &agent(),
            &[],
            &ParentOf::from_pairs(&[("user schema", "api layer")]),
            10,
        )
        .unwrap();

        assert_eq!(out.created.len(), 1, "parent_of brand-new content created");
        assert_eq!(out.matched, vec![pre_id]);
        let api = out.created[0];
        match g.node(api).unwrap() {
            Node::Concept(c) => {
                assert_eq!(c.concept_type, ConceptType::Entity);
                assert_eq!(c.origin_interaction, i2id);
            }
            _ => panic!("concept"),
        }
        let h = g
            .edge_between(pre_id, api, EdgeType::Hierarchical)
            .expect("hierarchical");
        assert_eq!(h.weight, 0.5);
        assert_eq!(h.reinforcements, 1, "fresh hierarchical edge");
        // The parent_of-created concept also carries a Derives edge (§5.7).
        assert_eq!(derives_of(&g, api), vec![i2id]);
        // The matched parent gained one Derives edge from the new interaction.
        assert_eq!(derives_of(&g, pre_id).len(), 2);
        assert_eq!(out.reinforced, 0);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_rederive_reinforces_edges() {
        let (mut g, iid) = graph_with_interaction(1, 0);
        let first = derive(
            &mut g,
            iid,
            &agent(),
            &[
                ("user schema", ConceptType::Entity),
                ("auth middleware", ConceptType::Logic),
            ],
            &ParentOf::none(),
            10,
        )
        .unwrap();
        let second = derive(
            &mut g,
            iid,
            &agent(),
            &[
                ("user schema", ConceptType::Entity),
                ("auth middleware", ConceptType::Logic),
            ],
            &ParentOf::none(),
            10,
        )
        .unwrap();

        // No duplicate nodes: same ids across calls.
        assert_eq!(second.created, Vec::<NodeId>::new());
        assert_eq!(second.matched, first.created);
        // 2 Derives duplicates + 1 CoOccurrence duplicate.
        assert_eq!(second.reinforced, 3);
        assert_eq!(g.node_count(), 3); // interaction + 2 concepts, unchanged

        let co = g
            .edge_between(first.created[0], first.created[1], EdgeType::CoOccurrence)
            .unwrap();
        assert_eq!(co.reinforcements, 2);
        assert_eq!(co.weight, 0.5 + 1.0);
        for &cid in &first.created {
            let d = g.edge_between(iid, cid, EdgeType::Derives).unwrap();
            assert_eq!(d.reinforcements, 2);
            assert_eq!(d.weight, 0.9 + 1.0);
        }
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_parent_of_rederive_reinforces_hierarchical() {
        let (mut g, iid) = graph_with_interaction(1, 0);
        let pairs = ParentOf::from_pairs(&[("user schema", "api layer")]);
        let first = derive(&mut g, iid, &agent(), &[], &pairs, 10).unwrap();
        let second = derive(&mut g, iid, &agent(), &[], &pairs, 10).unwrap();

        assert_eq!(first.reinforced, 0);
        // 2 Derives duplicates (both ends) + 1 Hierarchical duplicate.
        assert_eq!(second.reinforced, 3);
        let (p, c) = (first.created[0], first.created[1]);
        let h = g.edge_between(p, c, EdgeType::Hierarchical).unwrap();
        assert_eq!(h.reinforcements, 2);
        assert_eq!(h.weight, 0.5 + 1.0);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_missing_interaction_returns_not_found() {
        let mut g = Graph::new(sid());
        let missing = NodeId(Uuid::from_u64_pair(9, 9));
        let err = derive(
            &mut g,
            missing,
            &agent(),
            &[("x", ConceptType::Entity)],
            &ParentOf::none(),
            10,
        )
        .unwrap_err();
        assert!(
            matches!(&err, LamboError::Store(StoreError::NotFound(_))),
            "{err}"
        );
        // Nothing was written.
        assert_eq!(g.log_len(), 0);
        assert_eq!(g.node_count(), 0);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_non_interaction_node_returns_not_found() {
        let (mut g, iid) = graph_with_interaction(1, 0);
        let c = concept(1, iid, "user schema");
        let cid = c.id;
        g.insert_concept(c, iid).unwrap();
        let err = derive(
            &mut g,
            cid,
            &agent(),
            &[("x", ConceptType::Entity)],
            &ParentOf::none(),
            10,
        )
        .unwrap_err();
        assert!(
            matches!(&err, LamboError::Store(StoreError::NotFound(_))),
            "{err}"
        );
        // Only the seed writes remain: interaction (1) + concept (2: node + Derives).
        assert_eq!(g.log_len(), 3);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_reflexive_parent_of_is_rejected() {
        let (mut g, iid) = graph_with_interaction(1, 0);
        let err = derive(
            &mut g,
            iid,
            &agent(),
            &[],
            &ParentOf::from_pairs(&[("user schema", "user schema")]),
            10,
        )
        .unwrap_err();
        assert!(
            matches!(&err, LamboError::Store(StoreError::Invariant(_))),
            "{err}"
        );
        // No concepts were created by the rejected call.
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.edge_count(), 0);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_collapses_contents_sharing_a_canonical_key() {
        let (mut g, iid) = graph_with_interaction(1, 0);
        // "user schema" and "schema user" canonicalize to the same key
        // ("schema user"): one node is created, the second content matches it,
        // and no CoOccurrence self-loop is written for the collision.
        let out = derive(
            &mut g,
            iid,
            &agent(),
            &[
                ("user schema", ConceptType::Entity),
                ("schema user", ConceptType::Entity),
                ("auth middleware", ConceptType::Logic),
            ],
            &ParentOf::none(),
            10,
        )
        .unwrap();

        assert_eq!(out.created.len(), 2); // "user schema" + "auth middleware"
        assert_eq!(out.matched.len(), 1); // "schema user" -> the first node
        assert_eq!(out.matched[0], out.created[0]);
        assert_eq!(g.node_count(), 3); // interaction + 2 concepts
        assert!(
            g.edge_between(out.created[0], out.created[0], EdgeType::CoOccurrence)
                .is_none(),
            "no self-loop from a key collision"
        );
        assert!(g
            .edge_between(out.created[0], out.created[1], EdgeType::CoOccurrence)
            .is_some());
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_key_collision_on_preexisting_concept_does_not_self_reinforce() {
        // F1 regression: two distinct contents in ONE call that canonicalize
        // to the same PRE-EXISTING concept ("user schema" and "schema user"
        // both canonicalize to "schema user") must both resolve to that node,
        // but only the first may write. The second must not re-enter the write
        // path and bump the just-created Derives edge — one call never
        // self-reinforces.
        let (mut g, i1) = graph_with_interaction(1, 0);
        let i2 = interaction(2, Some(i1), 5);
        let i2id = i2.id;
        g.insert_interaction(i2).unwrap();
        // Pre-seed the concept (canonical key "schema user") derived from i1.
        let c = concept(1, i1, "user schema");
        let cid = c.id;
        g.insert_concept(c, i1).unwrap();

        let out = derive(
            &mut g,
            i2id,
            &agent(),
            &[
                ("user schema", ConceptType::Entity),
                ("schema user", ConceptType::Entity),
            ],
            &ParentOf::none(),
            10,
        )
        .unwrap();

        assert!(
            out.created.is_empty(),
            "both contents matched the pre-existing node"
        );
        // Every content that resolved Matched is recorded: one entry per
        // colliding content, both resolving to the same node.
        assert_eq!(out.matched, vec![cid, cid]);
        assert_eq!(
            out.reinforced, 0,
            "a collision within one call must not self-reinforce"
        );

        // Exactly ONE Derives edge from the new interaction, written fresh
        // (weight 0.9, reinforcements 1) — not bumped to 1.9 by the collision.
        let mut derives = derives_of(&g, cid);
        derives.sort_by_key(|id| id.0);
        assert_eq!(derives, {
            let mut v = vec![i1, i2id];
            v.sort_by_key(|id| id.0);
            v
        });
        let d = g.edge_between(i2id, cid, EdgeType::Derives).unwrap();
        assert_eq!(
            d.reinforcements, 1,
            "single write, never reinforced within the call"
        );
        assert_eq!(d.weight, 0.9);
        assert_eq!(g.node_count(), 3); // i1, i2, c — nothing created
        assert_eq!(g.edge_count(), 3); // Temporal (i2->i1) + seed Derives (i1->c) + derive Derives (i2->c)
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_parent_of_colliding_pairs_write_one_hierarchical_edge() {
        // F2 regression: two parent_of pairs whose ends canonicalize to the
        // SAME resolved node pair ("user schema" and "schema user" both
        // resolve to the pre-existing concept, child "api layer" is shared)
        // must write exactly ONE Hierarchical edge — the second pair must not
        // reinforce the fresh edge (0.5 -> 1.5) — and the shared parent must
        // not be double-written (the F1 guard covers parent_of resolution too).
        let (mut g, i1) = graph_with_interaction(1, 0);
        let i2 = interaction(2, Some(i1), 5);
        let i2id = i2.id;
        g.insert_interaction(i2).unwrap();
        let pre = concept(1, i1, "user schema"); // canonical key "schema user"
        let pre_id = pre.id;
        g.insert_concept(pre, i1).unwrap();

        let out = derive(
            &mut g,
            i2id,
            &agent(),
            &[],
            &ParentOf::from_pairs(&[("user schema", "api layer"), ("schema user", "api layer")]),
            10,
        )
        .unwrap();

        assert_eq!(out.created.len(), 1, "api layer created exactly once");
        let api = out.created[0];
        // Parent matched by both pairs; child matched again by the second
        // pair's re-resolution (a write no-op thanks to the within-call guard).
        assert_eq!(out.matched, vec![pre_id, pre_id, api]);
        assert_eq!(out.reinforced, 0, "no within-call reinforcement anywhere");

        // Exactly ONE Hierarchical edge, fresh (weight 0.5, reinforcements 1).
        let h = g
            .edge_between(pre_id, api, EdgeType::Hierarchical)
            .expect("hierarchical");
        assert_eq!(
            h.reinforcements, 1,
            "single write, never bumped within the call"
        );
        assert_eq!(h.weight, 0.5);
        // The shared parent gained exactly one Derives edge — no double write.
        let mut derives = derives_of(&g, pre_id);
        derives.sort_by_key(|id| id.0);
        assert_eq!(derives, {
            let mut v = vec![i1, i2id];
            v.sort_by_key(|id| id.0);
            v
        });
        let d = g.edge_between(i2id, pre_id, EdgeType::Derives).unwrap();
        assert_eq!(d.reinforcements, 1);
        assert_eq!(d.weight, 0.9);
        assert_eq!(derives_of(&g, api), vec![i2id]);
        assert_eq!(g.node_count(), 4); // i1, i2, pre, api
        assert_eq!(g.edge_count(), 5); // Temporal (i2->i1) + seed Derives + derive Derives x2 + Hierarchical
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_empty_call_is_noop() {
        let (mut g, iid) = graph_with_interaction(1, 0);
        let out = derive(&mut g, iid, &agent(), &[], &ParentOf::none(), 10).unwrap();
        assert_eq!(out, DeriveOutcome::default());
        assert_eq!(g.log_len(), 1); // the interaction seed only
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_after_demote_creates_new_concept_not_observation_match() {
        // GRAPH-1 regression: demote creates an Observation keyed "schema user";
        // a later derive of an Entity with that key must create a NEW concept
        // (Unmatched) — Observations skip the step-5 matcher per spec §7 and
        // must never capture agent-declared content.
        let (mut g, iid) = graph_with_interaction(1, 0);
        let obs = demote(&mut g, iid, &agent(), "user schema", "chunk-1").unwrap();
        assert_eq!(obs.len(), 1);
        assert_eq!(
            concept_of(&g, obs[0]).canonical_key,
            "schema user",
            "the Observation carries the same key derive will produce"
        );

        let out = derive(
            &mut g,
            iid,
            &agent(),
            &[("UserSchema", ConceptType::Entity)],
            &ParentOf::none(),
            10,
        )
        .unwrap();
        assert_eq!(
            out.created.len(),
            1,
            "derive must create a NEW concept, not match the Observation"
        );
        assert!(out.matched.is_empty());
        let entity = concept_of(&g, out.created[0]);
        assert_eq!(entity.concept_type, ConceptType::Entity);
        assert_ne!(entity.id, obs[0]);
        // Observation + Entity coexist under one canonical key (partial UNIQUE
        // errata — non-Observations stay unique).
        assert_eq!(g.node_count(), 3); // interaction + observation + entity
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_with_duplicate_observation_keys_is_deterministic() {
        // GRAPH-1: duplicate Observation keys are legal (partial-UNIQUE errata);
        // the matcher must skip ALL of them deterministically — the outcome
        // must never depend on HashMap iteration order.
        for _ in 0..5 {
            let (mut g, iid) = graph_with_interaction(1, 0);
            // Two Observations sharing the key "schema user" (different chunks).
            demote(&mut g, iid, &agent(), "user schema", "chunk-1").unwrap();
            demote(&mut g, iid, &agent(), "user schema", "chunk-2").unwrap();
            let out = derive(
                &mut g,
                iid,
                &agent(),
                &[("UserSchema", ConceptType::Entity)],
                &ParentOf::none(),
                10,
            )
            .unwrap();
            assert_eq!(out.created.len(), 1, "always creates, never matches");
            assert!(out.matched.is_empty());
            assert_eq!(g.node_count(), 4); // interaction + 2 observations + entity
            g.assert_invariants().unwrap();
        }
    }

    #[test]
    fn derive_swapped_order_reinforces_single_cooccurrence_edge() {
        // GRAPH-5 regression: derive([x,y]) then derive([y,x]) must reinforce
        // the existing CoOccurrence edge (one 1.5-weight edge), not create a
        // reverse duplicate (T2.3 exit criterion; P4 density double-count).
        let (mut g, iid) = graph_with_interaction(1, 0);
        let first = derive(
            &mut g,
            iid,
            &agent(),
            &[("x", ConceptType::Entity), ("y", ConceptType::Entity)],
            &ParentOf::none(),
            10,
        )
        .unwrap();
        let second = derive(
            &mut g,
            iid,
            &agent(),
            &[("y", ConceptType::Entity), ("x", ConceptType::Entity)],
            &ParentOf::none(),
            10,
        )
        .unwrap();

        // Exactly ONE CoOccurrence edge, reinforced to 0.5 + 1.0.
        assert_eq!(g.edge_count(), 3); // derives x2 + cooccurrence
        let co = g
            .edge_between(first.created[0], first.created[1], EdgeType::CoOccurrence)
            .expect("the original-direction edge");
        assert_eq!(co.reinforcements, 2);
        assert_eq!(co.weight, 1.5);
        assert!(
            g.edge_between(first.created[1], first.created[0], EdgeType::CoOccurrence)
                .is_none(),
            "no reverse duplicate"
        );
        // Both calls counted the CoOccurrence duplicate reinforcement.
        assert_eq!(second.reinforced, 3); // 2 Derives + 1 CoOccurrence
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_rejects_empty_and_stopword_only_content() {
        // GRAPH-8: empty/whitespace-only/stopword-only content would collapse
        // onto one key-"" concept — rejected at the entry point, before any
        // write.
        let (mut g, iid) = graph_with_interaction(1, 0);
        for bad in ["", "   ", "the and of", "-_"] {
            let err = derive(
                &mut g,
                iid,
                &agent(),
                &[(bad, ConceptType::Entity)],
                &ParentOf::none(),
                10,
            )
            .unwrap_err();
            assert!(
                matches!(&err, LamboError::Store(StoreError::Invariant(_))),
                "{bad:?}: {err}"
            );
            assert!(err.to_string().contains("empty key"), "{bad:?}: {err}");
        }
        // parent_of ends are guarded too.
        let err = derive(
            &mut g,
            iid,
            &agent(),
            &[],
            &ParentOf::from_pairs(&[("user schema", "the and of")]),
            10,
        )
        .unwrap_err();
        assert!(matches!(&err, LamboError::Store(StoreError::Invariant(_))));
        // Nothing was written by any rejected call.
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.log_len(), 1);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn derive_mutation_log_is_well_ordered() {
        // One accumulated batch: interaction seed + a full derive (fresh
        // concepts + parent_of-created content). Every UpsertEdge's endpoints
        // must have been UpsertNode'd earlier in the same batch, and no
        // deletion may precede them.
        let (mut g, iid) = graph_with_interaction(1, 0);
        derive(
            &mut g,
            iid,
            &agent(),
            &[
                ("user schema", ConceptType::Entity),
                ("auth middleware", ConceptType::Logic),
            ],
            &ParentOf::from_pairs(&[("user schema", "api layer")]),
            10,
        )
        .unwrap();

        let batch = g.drain_log();
        let mut seen_nodes: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        let mut edge_upserts = 0usize;
        for m in &batch.mutations {
            match m {
                Mutation::UpsertNode { node } => {
                    seen_nodes.insert(node.id());
                }
                Mutation::UpsertEdge { edge } => {
                    edge_upserts += 1;
                    assert!(seen_nodes.contains(&edge.source), "{m:?}");
                    assert!(seen_nodes.contains(&edge.target), "{m:?}");
                }
                Mutation::SetRootGoal { .. } => {}
                Mutation::DeleteNode { .. } | Mutation::DeleteEdge { .. } => {
                    panic!("derive must not delete: {m:?}");
                }
                Mutation::CanonizationTransition { .. } => {}
            }
        }
        // interaction + 3 concepts node-upserted; edges: 3 Derives + 1
        // CoOccurrence + 1 Hierarchical.
        assert_eq!(seen_nodes.len(), 4);
        assert_eq!(edge_upserts, 5);
        assert!(g.drain_log().is_empty());
        g.assert_invariants().unwrap();
    }
}

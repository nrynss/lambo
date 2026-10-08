//! [`Graph`] — the in-RAM bipartite graph (T2.1).
//!
//! Structure: `HashMap<NodeId, Node>` for nodes, `HashMap<NodeId, Edge>` keyed by
//! edge id (schema PK) with a natural-key index `(source, target, edge_type) -> id`
//! (schema `UNIQUE`), plus per-node out/in adjacency grouped by [`EdgeType`] so
//! recall BFS (P5) and canonization queries (P6) never scan the edge table.
//!
//! Invariants (spec §5.7) are enforced at write time:
//! * every non-first interaction has exactly one `Temporal` predecessor —
//!   [`Graph::insert_interaction`] builds the chain by construction;
//! * every concept has at least one `Derives` edge from its origin interaction —
//!   [`Graph::insert_concept`] creates it by construction;
//! * no duplicate `(source, target, edge_type)` — the natural-key index is the
//!   authority; a duplicate write reinforces instead of inserting;
//! * weights ≥ 0 and finite — NaN/±Inf clamp to 0.0, negatives are rejected;
//! * no cycles in `Causal`/`Dependency`/`Hierarchical` — write-time rejection of
//!   `Causal`/`Dependency` cycles is `record_action`'s BFS (T2.4);
//!   [`Graph::assert_invariants`] detects cycles in all three as a safety net
//!   (`Hierarchical` is a DAG constraint by definition, see adve-review T2.1 M1);
//!
//! Load path: [`Graph::from_snapshot`] seeds state without touching the mutation
//! log (a loaded session's history is already durable) and runs
//! `assert_invariants` before returning.

use std::collections::{BTreeMap, HashMap, HashSet};

#[allow(unused_imports)] // rustdoc links in the struct docs; the tests' `super::*`
use crate::types::GraphSnapshot;
use crate::types::{
    CanonizationEvent, Concept, ConceptType, Edge, EdgeType, GcMark, Interaction, LamboError,
    Mutation, Node, NodeId, Reservation, SessionId, StoreError,
};

mod accesses;
mod embeddings;
mod invariants;
mod mutation_log;
mod root_goal;
mod snapshot;
mod transitions;

pub use root_goal::root_goal_texts;

/// Edge-weight bump per reinforcement (v0.6.0 §5.4 semantics; see module docs).
pub const REINFORCE_BUMP: f64 = 1.0;
/// Cap on reinforced edge weight so decay thresholds stay meaningful and weights
/// stay finite by construction.
pub const MAX_EDGE_WEIGHT: f64 = 10.0;
/// Initial weight of the structural `Temporal` edge (matches fixture convention).
const TEMPORAL_WEIGHT: f64 = 1.0;
/// Initial weight of the structural `Derives` edge (matches fixture convention).
const DERIVES_WEIGHT: f64 = 0.9;

type EdgeKey = (NodeId, NodeId, EdgeType);

/// In-RAM session graph. Owns no lock (see `src/graph/mod.rs`).
#[derive(Clone, Debug)]
pub struct Graph {
    session_id: SessionId,
    nodes: HashMap<NodeId, Node>,
    edges: HashMap<NodeId, Edge>,
    edge_keys: HashMap<EdgeKey, NodeId>,
    out: HashMap<NodeId, HashMap<EdgeType, HashSet<NodeId>>>,
    incoming: HashMap<NodeId, HashMap<EdgeType, HashSet<NodeId>>>,
    /// Interactions in temporal chain order (`chain[i].previous_id == chain[i-1]`).
    temporal_chain: Vec<NodeId>,
    /// source_key -> canonical_key (direct lookup only, no transitivity).
    synonyms: BTreeMap<String, String>,
    /// Advisory soft locks (spec §11). RAM-local: no `Mutation` kind exists, so
    /// these round-trip through [`GraphSnapshot`] but never enter the write-behind log.
    reservations: Vec<Reservation>,
    canonization_events: Vec<CanonizationEvent>,
    root_goal: Option<serde_json::Value>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    closed_at: Option<chrono::DateTime<chrono::Utc>>,
    embedding: Option<crate::types::EmbeddingContract>,
    /// Ordered write-behind log; drained by the flush task (T3.4). Append-only
    /// here; [`Graph::drain_log`] is the only way out.
    mutation_log: Vec<Mutation>,
    /// `MutationEpoch` — bumps once per appended mutation. Recall-cache invalidation
    /// key (spec §8); GC's step 7 is redundant but harmless (any mutation already
    /// bumps the epoch).
    ///
    /// The counter is **deployment-lifetime, not process-lifetime** (issue #17):
    /// [`Graph::from_snapshot`] resumes it from [`GraphSnapshot::mutation_epoch`]
    /// and [`Graph::drain_log`] stamps it back onto every flushed batch, so a
    /// writer restart neither resets GC's `gc_interval` measure nor rewinds the
    /// epoch scale the recall cache keys on. RAM-local bumps (reservations,
    /// synonyms) are counted while they last and ride along with the next
    /// flushed stamp — a crash before that flush sheds their contribution, one
    /// after it does not. The durable watermark is therefore never behind the
    /// count of durable mutations; it may run ahead of it by RAM-local bumps.
    epoch: u64,
    /// GC's sweep accounting (issue #29): the epoch the next sweep interval is
    /// measured from and the time of the last sweep. Graph state for the same
    /// reason `epoch` is — [`Graph::from_snapshot`] resumes it and
    /// [`Graph::drain_log`] stamps it onto every flushed batch, so a writer
    /// restart neither re-runs a sweep nobody owed nor resets the
    /// `gc_max_interval` clock. Setting it is **not** a mutation: it never
    /// bumps `epoch` and never enters the log (it rides the next batch's
    /// stamp, the way `epoch` does).
    gc_mark: GcMark,
    /// Concepts whose access columns changed since they were last handed to
    /// the flush ([`Graph::record_accesses`] / [`Graph::drain_accesses`],
    /// issue #30). A set, not a log: however many reads land while the store
    /// is unreachable, it holds at most one entry per concept, and the values
    /// are read from the node when drained. RAM-only, like the log.
    access_dirty: HashSet<NodeId>,
}

impl Graph {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            nodes: HashMap::new(),
            edges: HashMap::new(),
            edge_keys: HashMap::new(),
            out: HashMap::new(),
            incoming: HashMap::new(),
            temporal_chain: Vec::new(),
            synonyms: BTreeMap::new(),
            reservations: Vec::new(),
            canonization_events: Vec::new(),
            root_goal: None,
            created_at: None,
            closed_at: None,
            embedding: None,
            mutation_log: Vec::new(),
            epoch: 0,
            gc_mark: GcMark::default(),
            access_dirty: HashSet::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Write path — node entry points
    // -----------------------------------------------------------------------

    /// Insert an interaction, extending the temporal chain by construction.
    ///
    /// * First interaction: `previous_id` must be `None`.
    /// * Subsequent: `previous_id` must be the current chain tail.
    /// * The structural `Temporal` edge (new -> previous) is created automatically,
    ///   so the §5.7 predecessor invariant holds by construction.
    ///
    /// Re-upserting an existing interaction is idempotent (no duplicate chain
    /// entry) but must keep its chain position (`previous_id` unchanged).
    pub fn insert_interaction(&mut self, i: Interaction) -> Result<(), LamboError> {
        if i.session_id != self.session_id {
            return Err(invariant(format!(
                "interaction {} session {} != graph {}",
                i.id, i.session_id, self.session_id
            )));
        }
        let known = self.nodes.contains_key(&i.id);
        let pos = self.temporal_chain.iter().position(|&x| x == i.id);
        match (known, pos) {
            (false, None) => {
                // Fresh interaction: validate chain position.
                let tail = self.temporal_chain.last().copied();
                match (tail, i.previous_id) {
                    (None, None) => {}
                    (None, Some(_)) => {
                        return Err(invariant(format!(
                            "first interaction {} must have previous_id = None",
                            i.id
                        )));
                    }
                    (Some(_), None) => {
                        return Err(invariant(format!(
                            "non-first interaction {} needs previous_id = current tail",
                            i.id
                        )));
                    }
                    (Some(tail), Some(prev)) if prev == tail => {}
                    (Some(tail), Some(prev)) => {
                        return Err(invariant(format!(
                            "interaction {} previous {prev} != chain tail {tail}",
                            i.id
                        )));
                    }
                }
            }
            (true, Some(pos)) => {
                // Re-upsert: chain position is fixed.
                let expected = if pos == 0 {
                    None
                } else {
                    Some(self.temporal_chain[pos - 1])
                };
                if i.previous_id != expected {
                    return Err(invariant(format!(
                        "re-upsert of interaction {} would move it within the chain",
                        i.id
                    )));
                }
            }
            (true, None) => {
                return Err(invariant(format!(
                    "interaction {} exists but is missing from the temporal chain",
                    i.id
                )));
            }
            // Chain references a node that is not in `nodes` — internal corruption.
            (false, Some(_)) => {
                return Err(invariant(format!(
                    "temporal chain references interaction {} which is not stored",
                    i.id
                )));
            }
        }

        let node = Node::Interaction(i.clone());
        self.nodes.insert(i.id, node.clone());
        if !known {
            self.temporal_chain.push(i.id);
        }
        self.append_mutation(Mutation::UpsertNode { node });

        if let Some(prev) = i.previous_id {
            let edge = Edge {
                event_time: i.event_time,
                id: NodeId::new(),
                session_id: self.session_id.clone(),
                source: i.id,
                target: prev,
                edge_type: EdgeType::Temporal,
                weight: TEMPORAL_WEIGHT,
                reinforcements: 1,
                created_at: i.created_at,
                last_reinforced: i.created_at,
            };
            // Endpoints exist by construction (prev is on the chain, i just stored).
            let final_edge = self.record_edge(edge)?;
            self.append_mutation(Mutation::UpsertEdge { edge: final_edge });
        }
        Ok(())
    }

    /// Insert a concept, creating its structural `Derives` edge from
    /// `derives_from` (the interaction that produced it) by construction, so the
    /// §5.7 invariant "every concept has ≥ 1 Derives edge" holds at write time.
    ///
    /// `derives_from` must name an existing interaction node. Re-upserting an
    /// existing concept is idempotent; its `Derives` edge reinforces if already
    /// present (duplicate natural-key write).
    pub fn insert_concept(&mut self, c: Concept, derives_from: NodeId) -> Result<(), LamboError> {
        if c.session_id != self.session_id {
            return Err(invariant(format!(
                "concept {} session {} != graph {}",
                c.id, c.session_id, self.session_id
            )));
        }
        let interaction_event_time = match self.nodes.get(&derives_from) {
            Some(Node::Interaction(i)) => i.event_time,
            _ => {
                return Err(invariant(format!(
                    "concept {} derives from {derives_from}, which is not an interaction in this graph",
                    c.id
                )));
            }
        };
        if let Some(vector) = &c.embedding {
            let contract = self.embedding.as_ref().ok_or_else(|| {
                invariant(format!(
                    "concept {} carries a vector without a session embedding contract",
                    c.id
                ))
            })?;
            if vector.len() != contract.dim || vector.iter().any(|x| !x.is_finite()) {
                return Err(invariant(format!(
                    "concept {} vector is non-finite or has width {} != contract {}",
                    c.id,
                    vector.len(),
                    contract.dim
                )));
            }
        }
        // Schema §4 `UNIQUE (session_id, canonical_key)`, partial for
        // Observations (spec errata 2026-08-11 / muse-spark M1-M2): two
        // non-Observation concepts must never share a canonical key — a
        // collision fragments the graph and would fail the store's upsert at
        // flush time (P3). Demoted Observations skip the match step by design
        // (spec §7) and may legitimately share keys, so they are exempt;
        // Observation keys may shadow entity keys (grok G7 — P5 recall must
        // disambiguate by concept_type, not key uniqueness).
        // Scaling note (grok G4): this is an O(N) scan per insert — no
        // canonical_key index in v0.1 (deliberate cut); P4 GC should not
        // benchmark a long-session derive against this without an index.
        if c.concept_type != ConceptType::Observation {
            let collision = self.nodes.iter().find_map(|(id, n)| match n {
                Node::Concept(x)
                    if *id != c.id
                        && x.canonical_key == c.canonical_key
                        && x.concept_type != ConceptType::Observation =>
                {
                    Some(*id)
                }
                _ => None,
            });
            if let Some(other) = collision {
                return Err(invariant(format!(
                    "concept {} canonical_key {:?} collides with concept {other} \
                     (UNIQUE (session_id, canonical_key), spec §4; Observations exempt)",
                    c.id, c.canonical_key
                )));
            }
        }
        let node = Node::Concept(c.clone());
        self.nodes.insert(c.id, node.clone());
        self.append_mutation(Mutation::UpsertNode { node });

        let edge = Edge {
            id: NodeId::new(),
            session_id: self.session_id.clone(),
            source: derives_from,
            target: c.id,
            edge_type: EdgeType::Derives,
            weight: DERIVES_WEIGHT,
            reinforcements: 1,
            created_at: c.created_at,
            last_reinforced: c.created_at,
            event_time: interaction_event_time,
        };
        let final_edge = self.record_edge(edge)?;
        self.append_mutation(Mutation::UpsertEdge { edge: final_edge });
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Write path — edge entry point
    // -----------------------------------------------------------------------

    /// Upsert an edge. Enforces session match, existing endpoints, and weight
    /// sanity (NaN/±Inf clamp to 0.0, negatives rejected). A duplicate natural key
    /// `(source, target, edge_type)` reinforces the existing edge instead of
    /// inserting a second one: weight bumps by [`REINFORCE_BUMP`] capped at
    /// [`MAX_EDGE_WEIGHT`], `reinforcements += 1`, `last_reinforced` moves to the
    /// write time; the original id and `created_at` are preserved.
    ///
    /// On reinforcement the incoming edge's `weight` is **intentionally ignored** —
    /// a duplicate write is a reinforcement (fixed bump, v0.6.0 §5.4), not a
    /// re-weight. Callers that want a different weight must delete the edge first
    /// (adve-review T2.1 S1).
    ///
    /// `Causal`/`Dependency` cycle rejection is `record_action`'s BFS (T2.4) —
    /// this primitive stores what it is given; `assert_invariants` detects cycles
    /// in `Causal`/`Dependency`/`Hierarchical` as a safety net.
    pub fn upsert_edge(&mut self, edge: Edge) -> Result<(), LamboError> {
        let final_edge = self.record_edge(edge)?;
        self.append_mutation(Mutation::UpsertEdge { edge: final_edge });
        Ok(())
    }

    /// Remove a concept node and every incident edge. Emits `DeleteEdge` for each
    /// incident edge (before the `DeleteNode`, per §2.4 deletion ordering), then
    /// the `DeleteNode` itself. Missing node -> `NotFound`.
    ///
    /// Interactions are **append-only** in v0.1 (interaction compaction is cut,
    /// spec §9) — removing one is rejected as an invariant violation, so the
    /// temporal chain can never be left with a dangling `previous_id`
    /// (adve-review T2.1 S2).
    pub fn remove_node(&mut self, id: NodeId) -> Result<(), LamboError> {
        if !self.nodes.contains_key(&id) {
            return Err(not_found(format!("node {id}")));
        }
        if matches!(self.nodes.get(&id), Some(Node::Interaction(_))) {
            return Err(invariant(format!(
                "interaction {id} is append-only; node removal is not supported for \
                 interactions in v0.1 (interaction compaction is cut, spec §9)"
            )));
        }
        // Incident edges come from the adjacency index (O(degree)), not a full
        // edge scan (adve-review T2.1 S3). A self-loop appears in both out and in
        // maps, so dedup before removing.
        let mut incident: Vec<NodeId> = Vec::new();
        let mut seen: HashSet<NodeId> = HashSet::new();
        if let Some(by_type) = self.out.get(&id) {
            for (ty, targets) in by_type {
                for &tgt in targets {
                    if let Some(&eid) = self.edge_keys.get(&(id, tgt, *ty)) {
                        if seen.insert(eid) {
                            incident.push(eid);
                        }
                    }
                }
            }
        }
        if let Some(by_type) = self.incoming.get(&id) {
            for (ty, sources) in by_type {
                for &src in sources {
                    if let Some(&eid) = self.edge_keys.get(&(src, id, *ty)) {
                        if seen.insert(eid) {
                            incident.push(eid);
                        }
                    }
                }
            }
        }
        for eid in incident {
            self.remove_edge(eid)?;
        }
        self.nodes.remove(&id);
        // Issue #30: nothing left to count against; keep the dirty set bounded
        // by the live concepts even while the flush is not draining it.
        self.access_dirty.remove(&id);
        self.temporal_chain.retain(|&x| x != id);
        self.reservations.retain(|r| r.node_id != id);
        self.append_mutation(Mutation::DeleteNode { id });
        Ok(())
    }

    /// Remove an edge by id. Missing edge -> `NotFound`.
    pub fn remove_edge(&mut self, id: NodeId) -> Result<(), LamboError> {
        let edge = self
            .edges
            .get(&id)
            .cloned()
            .ok_or_else(|| not_found(format!("edge {id}")))?;
        let key = (edge.source, edge.target, edge.edge_type);
        self.edges.remove(&id);
        self.edge_keys.remove(&key);
        self.remove_adjacency(edge.source, edge.target, edge.edge_type);
        self.append_mutation(Mutation::DeleteEdge { id });
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Read path
    // -----------------------------------------------------------------------

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    pub fn edge(&self, id: NodeId) -> Option<&Edge> {
        self.edges.get(&id)
    }

    pub fn edge_between(&self, source: NodeId, target: NodeId, ty: EdgeType) -> Option<&Edge> {
        self.edge_keys
            .get(&(source, target, ty))
            .and_then(|id| self.edges.get(id))
    }

    /// Every edge, in unspecified order. For whole-graph folds that would
    /// otherwise pay `incident_edges` per node (session-level staleness, T4.6);
    /// callers needing determinism sort by `id`.
    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.edges.values()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }

    pub fn interactions(&self) -> impl Iterator<Item = &Interaction> {
        self.nodes.values().filter_map(|n| match n {
            Node::Interaction(i) => Some(i),
            _ => None,
        })
    }

    pub fn concepts(&self) -> impl Iterator<Item = &Concept> {
        self.nodes.values().filter_map(|n| match n {
            Node::Concept(c) => Some(c),
            _ => None,
        })
    }

    pub fn temporal_chain(&self) -> &[NodeId] {
        &self.temporal_chain
    }

    /// Out-neighbors (all edge types). Deduplicated; returned in deterministic
    /// (id-ascending) order so callers never see HashMap iteration order.
    pub fn out_neighbors(&self, src: NodeId) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = self
            .out
            .get(&src)
            .map(|by_type| {
                by_type
                    .values()
                    .flatten()
                    .copied()
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by_key(|id| id.0);
        v
    }

    pub fn out_neighbors_typed(&self, src: NodeId, ty: EdgeType) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = self
            .out
            .get(&src)
            .and_then(|by_type| by_type.get(&ty))
            .map(|targets| targets.iter().copied().collect())
            .unwrap_or_default();
        v.sort_by_key(|id| id.0);
        v
    }

    /// In-neighbors (all edge types). Deduplicated; deterministic id-ascending order.
    pub fn in_neighbors(&self, tgt: NodeId) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = self
            .incoming
            .get(&tgt)
            .map(|by_type| {
                by_type
                    .values()
                    .flatten()
                    .copied()
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by_key(|id| id.0);
        v
    }

    pub fn in_neighbors_typed(&self, tgt: NodeId, ty: EdgeType) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = self
            .incoming
            .get(&tgt)
            .and_then(|by_type| by_type.get(&ty))
            .map(|sources| sources.iter().copied().collect())
            .unwrap_or_default();
        v.sort_by_key(|id| id.0);
        v
    }

    /// All edges incident to `node` (out or in), in id-ascending order.
    ///
    /// Routed through the out/in adjacency index — `O(degree log degree)` for
    /// the sort, never a scan of the edge set (adve-review CONC-1; `remove_node`
    /// already used the index for the same reason). The daemon calls this per
    /// concept in every detector and in `rescore`, so a full `edges.values()`
    /// filter made each pass `O(nodes × edges)` and held the graph lock for
    /// 186–272ms per cycle at 4k concepts. A self-loop appears in both maps, so
    /// edge ids are deduplicated.
    pub fn incident_edges(&self, node: NodeId) -> Vec<&Edge> {
        let mut ids: Vec<NodeId> = Vec::new();
        if let Some(by_type) = self.out.get(&node) {
            for (ty, targets) in by_type {
                for &tgt in targets {
                    if let Some(&eid) = self.edge_keys.get(&(node, tgt, *ty)) {
                        ids.push(eid);
                    }
                }
            }
        }
        if let Some(by_type) = self.incoming.get(&node) {
            for (ty, sources) in by_type {
                for &src in sources {
                    if let Some(&eid) = self.edge_keys.get(&(src, node, *ty)) {
                        ids.push(eid);
                    }
                }
            }
        }
        ids.sort_by_key(|id| id.0);
        ids.dedup();
        ids.iter().filter_map(|id| self.edges.get(id)).collect()
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    fn append_mutation(&mut self, m: Mutation) {
        self.mutation_log.push(m);
        self.epoch += 1;
    }

    /// Validate and store an edge: session match, endpoints exist, weight sanity,
    /// natural-key dedup with reinforcement. Returns the final stored edge.
    /// Pure state mutation — the caller decides log emission.
    fn record_edge(&mut self, edge: Edge) -> Result<Edge, LamboError> {
        if edge.session_id != self.session_id {
            return Err(invariant(format!(
                "edge {} session {} != graph {}",
                edge.id, edge.session_id, self.session_id
            )));
        }
        if !self.nodes.contains_key(&edge.source) {
            return Err(not_found(format!(
                "edge {} source {}",
                edge.id, edge.source
            )));
        }
        if !self.nodes.contains_key(&edge.target) {
            return Err(not_found(format!(
                "edge {} target {}",
                edge.id, edge.target
            )));
        }
        // Spec §5 edge-type endpoint matrix (adve-review GRAPH-2): the schema
        // deliberately carries no FK on edge endpoints (spec §4 "the writer
        // enforces it") — this is that write gate. A type-invalid edge (e.g.
        // `Semantic` from an interaction) would pollute recall BFS permanently,
        // so it is rejected here, not merely flagged by assert_invariants.
        let src_node = self.nodes.get(&edge.source).expect("source checked above");
        let tgt_node = self.nodes.get(&edge.target).expect("target checked above");
        if let Some(msg) = edge_endpoint_error(edge.edge_type, src_node, tgt_node) {
            return Err(invariant(format!("edge {} {msg}", edge.id)));
        }
        if let Some(other) = self.edges.get(&edge.id) {
            let key = (edge.source, edge.target, edge.edge_type);
            let other_key = (other.source, other.target, other.edge_type);
            if key != other_key {
                return Err(invariant(format!(
                    "edge id {} reused for a different natural key",
                    edge.id
                )));
            }
        }
        let weight = normalize_weight(edge.weight)?;
        let key = (edge.source, edge.target, edge.edge_type);
        if let Some(existing_id) = self.edge_keys.get(&key).copied() {
            // Reinforcement on duplicate natural key (v0.6.0 §5.4 semantics).
            let existing = self
                .edges
                .get_mut(&existing_id)
                .expect("edge_keys consistent");
            existing.weight = (existing.weight + REINFORCE_BUMP).min(MAX_EDGE_WEIGHT);
            existing.reinforcements += 1;
            existing.last_reinforced = edge.last_reinforced;
            // Original id and created_at preserved.
            return Ok(existing.clone());
        }
        let mut edge = edge;
        edge.weight = weight;
        self.edge_keys.insert(key, edge.id);
        self.edges.insert(edge.id, edge.clone());
        self.add_adjacency(edge.source, edge.target, edge.edge_type);
        Ok(edge)
    }

    fn add_adjacency(&mut self, src: NodeId, tgt: NodeId, ty: EdgeType) {
        self.out
            .entry(src)
            .or_default()
            .entry(ty)
            .or_default()
            .insert(tgt);
        self.incoming
            .entry(tgt)
            .or_default()
            .entry(ty)
            .or_default()
            .insert(src);
    }

    fn remove_adjacency(&mut self, src: NodeId, tgt: NodeId, ty: EdgeType) {
        if let Some(by_type) = self.out.get_mut(&src) {
            if let Some(targets) = by_type.get_mut(&ty) {
                targets.remove(&tgt);
                if targets.is_empty() {
                    by_type.remove(&ty);
                }
            }
            if by_type.is_empty() {
                self.out.remove(&src);
            }
        }
        if let Some(by_type) = self.incoming.get_mut(&tgt) {
            if let Some(sources) = by_type.get_mut(&ty) {
                sources.remove(&src);
                if sources.is_empty() {
                    by_type.remove(&ty);
                }
            }
            if by_type.is_empty() {
                self.incoming.remove(&tgt);
            }
        }
    }

    /// DFS over `Causal`/`Dependency`/`Hierarchical` out-edges; returns a node on a
    /// back edge. `Hierarchical` is included because it is a DAG constraint by
    /// definition (A parent of B parent of A is nonsense) — spec §5.7 names only
    /// `Causal`/`Dependency`, so write-time rejection stays per spec (T2.4); the
    /// safety net here is broader than the write-time contract.
    ///
    /// Iterative (adve-review GRAPH-3): an explicit stack replaces recursion, so
    /// a deep chain (~10k+ nodes, plausible for a long record_action-heavy
    /// session) cannot overflow the ~2 MiB worker-thread stack that
    /// `load_session` materializes on. The recursive version SIGABRT'd on load
    /// and left the session permanently unloadable. Same three-color semantics
    /// (1 = on the current DFS path, 2 = fully explored); the dead `path` vec is
    /// gone with the recursion.
    fn dfs_cycle(&self, start: NodeId, color: &mut HashMap<NodeId, u8>) -> Option<NodeId> {
        // Stack frames: (node, unexplored out-neighbors, next index to visit).
        let mut stack: Vec<(NodeId, Vec<NodeId>, usize)> = Vec::new();
        color.insert(start, 1); // gray
        stack.push((start, self.cycle_neighbors(start), 0));
        while let Some(top) = stack.last() {
            let node = top.0;
            let next = top.2;
            if next >= top.1.len() {
                color.insert(node, 2); // black
                stack.pop();
                continue;
            }
            let tgt = top.1[next];
            stack.last_mut().expect("stack non-empty in loop").2 = next + 1;
            match color.get(&tgt).copied().unwrap_or(0) {
                1 => return Some(tgt), // back edge
                2 => continue,
                _ => {
                    color.insert(tgt, 1);
                    stack.push((tgt, self.cycle_neighbors(tgt), 0));
                }
            }
        }
        None
    }

    /// `Causal` + `Dependency` + `Hierarchical` out-neighbors of `node`, in that
    /// type-priority order — the same iteration the recursive DFS used.
    fn cycle_neighbors(&self, node: NodeId) -> Vec<NodeId> {
        let causal = self.out_neighbors_typed(node, EdgeType::Causal);
        let dependency = self.out_neighbors_typed(node, EdgeType::Dependency);
        let hierarchical = self.out_neighbors_typed(node, EdgeType::Hierarchical);
        causal
            .into_iter()
            .chain(dependency)
            .chain(hierarchical)
            .collect()
    }
}

/// Weights must be ≥ 0 and finite (spec §5.7): NaN/±Inf clamp to 0.0, negatives
/// are rejected as invariant violations.
fn normalize_weight(w: f64) -> Result<f64, LamboError> {
    if w < 0.0 {
        return Err(invariant(format!("negative edge weight {w}")));
    }
    Ok(if w.is_finite() { w } else { 0.0 })
}

/// Spec §5 edge-type endpoint matrix (adve-review GRAPH-2): `Temporal` connects
/// interactions, `Derives` connects an interaction to a concept, and the
/// remaining five types (`CoOccurrence`/`Causal`/`Dependency`/`Hierarchical`/
/// `Semantic`) connect concepts to concepts. Returns a violation message when
/// the endpoints violate the matrix, `None` when legal. The schema carries no FK
/// on endpoints (spec §4: "the writer enforces it") — `record_edge` is that
/// gate and `assert_invariants` the safety net.
fn edge_endpoint_error(edge_type: EdgeType, source: &Node, target: &Node) -> Option<String> {
    let (ok, want) = match edge_type {
        EdgeType::Temporal => (
            matches!(source, Node::Interaction(_)) && matches!(target, Node::Interaction(_)),
            "Interaction -> Interaction",
        ),
        EdgeType::Derives => (
            matches!(source, Node::Interaction(_)) && matches!(target, Node::Concept(_)),
            "Interaction -> Concept",
        ),
        _ => (
            matches!(source, Node::Concept(_)) && matches!(target, Node::Concept(_)),
            "Concept -> Concept",
        ),
    };
    if ok {
        None
    } else {
        Some(format!(
            "{edge_type:?} edge must connect {want} (spec §5) — got source {} / target {}",
            source.id(),
            target.id()
        ))
    }
}

fn invariant(msg: impl Into<String>) -> LamboError {
    LamboError::Store(StoreError::Invariant(msg.into()))
}

fn not_found(msg: String) -> LamboError {
    LamboError::Store(StoreError::NotFound(msg))
}

#[cfg(test)]
mod tests;

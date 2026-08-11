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
//! * no cycles in `Causal`/`Dependency` — write-time rejection is `record_action`'s
//!   BFS (T2.4); [`Graph::assert_invariants`] still detects cycles as a safety net.
//!
//! Load path: [`Graph::from_snapshot`] seeds state without touching the mutation
//! log (a loaded session's history is already durable) and runs
//! `assert_invariants` before returning.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::types::{
    CanonizationEvent, Concept, Edge, EdgeType, GraphSnapshot, Interaction, LamboError, Mutation,
    MutationBatch, Node, NodeId, Reservation, SessionId, StoreError, Synonym,
};

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
#[derive(Debug)]
pub struct Graph {
    session_id: SessionId,
    nodes: HashMap<NodeId, Node>,
    edges: HashMap<NodeId, Edge>,
    edge_keys: HashMap<EdgeKey, NodeId>,
    out: HashMap<NodeId, HashMap<EdgeType, HashSet<NodeId>>>,
    incoming: HashMap<NodeId, HashMap<EdgeType, HashSet<NodeId>>>,
    /// Interactions in temporal chain order (chain[i].previous_id == chain[i-1]).
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
    epoch: u64,
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
        }
    }

    // -----------------------------------------------------------------------
    // Construction / materialization
    // -----------------------------------------------------------------------

    /// Materialize a session snapshot into RAM. Seeds state without emitting
    /// mutations (the history is already durable) and verifies every §5.7
    /// invariant before returning.
    pub fn from_snapshot(snap: GraphSnapshot) -> Result<Self, LamboError> {
        let sid = snap.session_id.clone();
        let mut g = Self::new(sid.clone());

        g.root_goal = snap.root_goal;
        g.created_at = snap.created_at;
        g.closed_at = snap.closed_at;
        g.embedding = snap.embedding;

        for i in &snap.interactions {
            if i.session_id != sid {
                return Err(invariant(format!(
                    "interaction {} session {} != snapshot {}",
                    i.id, i.session_id, sid
                )));
            }
            g.nodes.insert(i.id, Node::Interaction(i.clone()));
        }
        for c in &snap.concepts {
            if c.session_id != sid {
                return Err(invariant(format!(
                    "concept {} session {} != snapshot {}",
                    c.id, c.session_id, sid
                )));
            }
            g.nodes.insert(c.id, Node::Concept(c.clone()));
        }

        // Rebuild the temporal chain by walking `previous_id` links.
        let mut next_of: HashMap<NodeId, NodeId> = HashMap::new();
        let mut heads: Vec<NodeId> = Vec::new();
        for i in &snap.interactions {
            match i.previous_id {
                None => heads.push(i.id),
                Some(prev) => {
                    if !g.nodes.contains_key(&prev) {
                        return Err(invariant(format!(
                            "interaction {} previous {} missing",
                            i.id, prev
                        )));
                    }
                    if next_of.insert(prev, i.id).is_some() {
                        return Err(invariant(format!(
                            "interaction {} has two successors (fork in temporal chain)",
                            prev
                        )));
                    }
                }
            }
        }
        if heads.len() != 1 {
            return Err(invariant(format!(
                "expected exactly one chain head, found {}",
                heads.len()
            )));
        }
        let mut chain = Vec::with_capacity(snap.interactions.len());
        let mut cur = heads[0];
        loop {
            chain.push(cur);
            match next_of.get(&cur) {
                Some(&next) => {
                    if chain.contains(&next) {
                        return Err(invariant("cycle in temporal chain"));
                    }
                    cur = next;
                }
                None => break,
            }
        }
        if chain.len() != snap.interactions.len() {
            return Err(invariant(format!(
                "temporal chain covers {} of {} interactions",
                chain.len(),
                snap.interactions.len()
            )));
        }
        g.temporal_chain = chain;

        for e in &snap.edges {
            let weight = normalize_weight(e.weight)?;
            let mut e = e.clone();
            e.weight = weight;
            g.record_edge(e)?;
        }
        for s in &snap.synonyms {
            if s.session_id != sid {
                return Err(invariant(format!(
                    "synonym session {} != snapshot {}",
                    s.session_id, sid
                )));
            }
            g.synonyms.insert(s.source_key.clone(), s.canonical_key.clone());
        }
        for r in &snap.reservations {
            if r.session_id != sid {
                return Err(invariant(format!(
                    "reservation session {} != snapshot {}",
                    r.session_id, sid
                )));
            }
            g.reservations.push(r.clone());
        }
        for ev in &snap.canonization_events {
            if ev.session_id != sid {
                return Err(invariant(format!(
                    "canonization event session {} != snapshot {}",
                    ev.session_id, sid
                )));
            }
            g.canonization_events.push(ev.clone());
        }

        g.assert_invariants()?;
        Ok(g)
    }

    /// Full session materialization for `load_session` / store round-trips.
    ///
    /// Deterministic ordering: interactions in temporal chain order, concepts and
    /// edges sorted by id, synonyms sorted by `source_key`.
    pub fn snapshot(&self) -> GraphSnapshot {
        let mut interactions: Vec<Interaction> = self
            .temporal_chain
            .iter()
            .filter_map(|id| match self.nodes.get(id) {
                Some(Node::Interaction(i)) => Some(i.clone()),
                _ => None,
            })
            .collect();
        let mut concepts: Vec<Concept> = self
            .nodes
            .values()
            .filter_map(|n| match n {
                Node::Concept(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        let mut edges: Vec<Edge> = self.edges.values().cloned().collect();
        let mut synonyms: Vec<Synonym> = self
            .synonyms
            .iter()
            .map(|(src, canon)| Synonym {
                session_id: self.session_id.clone(),
                source_key: src.clone(),
                canonical_key: canon.clone(),
            })
            .collect();
        interactions.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        concepts.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        edges.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        synonyms.sort_by(|a, b| a.source_key.cmp(&b.source_key));

        GraphSnapshot {
            session_id: self.session_id.clone(),
            root_goal: self.root_goal.clone(),
            created_at: self.created_at,
            closed_at: self.closed_at,
            interactions,
            concepts,
            edges,
            synonyms,
            reservations: self.reservations.clone(),
            canonization_events: self.canonization_events.clone(),
            embedding: self.embedding.clone(),
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
        if !matches!(self.nodes.get(&derives_from), Some(Node::Interaction(_))) {
            return Err(invariant(format!(
                "concept {} derives from {derives_from}, which is not an interaction in this graph",
                c.id
            )));
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
    /// `Causal`/`Dependency` cycle rejection is `record_action`'s BFS (T2.4) —
    /// this primitive stores what it is given; `assert_invariants` detects cycles.
    pub fn upsert_edge(&mut self, edge: Edge) -> Result<(), LamboError> {
        let final_edge = self.record_edge(edge)?;
        self.append_mutation(Mutation::UpsertEdge { edge: final_edge });
        Ok(())
    }

    /// Remove a node and every incident edge. Emits `DeleteEdge` for each incident
    /// edge (before the `DeleteNode`, per §2.4 deletion ordering), then the
    /// `DeleteNode` itself. Missing node -> `NotFound`.
    pub fn remove_node(&mut self, id: NodeId) -> Result<(), LamboError> {
        if !self.nodes.contains_key(&id) {
            return Err(not_found(format!("node {id}")));
        }
        let incident: Vec<NodeId> = self
            .edges
            .iter()
            .filter(|(_, e)| e.source == id || e.target == id)
            .map(|(eid, _)| *eid)
            .collect();
        for eid in incident {
            self.remove_edge(eid)?;
        }
        self.nodes.remove(&id);
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

    /// Apply a canonization transition to a concept and record it. Concept must
    /// exist; status and blast radius are set from the event. The event is
    /// appended to the session's audit trail and emitted as a mutation.
    pub fn apply_canonization_transition(
        &mut self,
        event: CanonizationEvent,
    ) -> Result<(), LamboError> {
        if event.session_id != self.session_id {
            return Err(invariant(format!(
                "canonization event session {} != graph {}",
                event.session_id, self.session_id
            )));
        }
        let concept = match self.nodes.get_mut(&event.node_id) {
            Some(Node::Concept(c)) => c,
            _ => {
                return Err(not_found(format!(
                    "concept {} for canonization",
                    event.node_id
                )))
            }
        };
        concept.canonization_status = event.to_status;
        concept.blast_radius = event.blast_radius;
        self.canonization_events.push(event.clone());
        self.append_mutation(Mutation::CanonizationTransition { event });
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Write path — session metadata, synonyms, reservations
    // -----------------------------------------------------------------------

    /// Declare (or replace) a direct synonym mapping. RAM-local: synonyms have no
    /// `Mutation` kind and round-trip through the snapshot only.
    pub fn declare_synonym(&mut self, source_key: &str, canonical_key: &str) {
        self.synonyms
            .insert(source_key.to_string(), canonical_key.to_string());
    }

    pub fn set_root_goal(&mut self, goal: Option<serde_json::Value>) {
        self.root_goal = goal;
    }

    pub fn set_embedding(&mut self, contract: Option<crate::types::EmbeddingContract>) {
        self.embedding = contract;
    }

    /// Advisory soft lock (spec §11). Same-agent re-reservation extends; cross-agent
    /// denial is T2.7's policy — this stores what it is given.
    pub fn set_reservation(&mut self, r: Reservation) {
        if let Some(existing) = self.reservations.iter_mut().find(|x| x.node_id == r.node_id) {
            *existing = r;
        } else {
            self.reservations.push(r);
        }
    }

    pub fn clear_reservation(&mut self, node_id: NodeId) {
        self.reservations.retain(|r| r.node_id != node_id);
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

    /// Out-neighbors (all edge types).
    pub fn out_neighbors(&self, src: NodeId) -> Vec<NodeId> {
        self.out
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
            .unwrap_or_default()
    }

    pub fn out_neighbors_typed(&self, src: NodeId, ty: EdgeType) -> Vec<NodeId> {
        self.out
            .get(&src)
            .and_then(|by_type| by_type.get(&ty))
            .map(|targets| targets.iter().copied().collect())
            .unwrap_or_default()
    }

    /// In-neighbors (all edge types).
    pub fn in_neighbors(&self, tgt: NodeId) -> Vec<NodeId> {
        self.incoming
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
            .unwrap_or_default()
    }

    pub fn in_neighbors_typed(&self, tgt: NodeId, ty: EdgeType) -> Vec<NodeId> {
        self.incoming
            .get(&tgt)
            .and_then(|by_type| by_type.get(&ty))
            .map(|sources| sources.iter().copied().collect())
            .unwrap_or_default()
    }

    /// All edges incident to `node` (out or in).
    pub fn incident_edges(&self, node: NodeId) -> Vec<&Edge> {
        self.edges
            .values()
            .filter(|e| e.source == node || e.target == node)
            .collect()
    }

    pub fn synonyms(&self) -> impl Iterator<Item = (&str, &str)> {
        self.synonyms
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn synonym(&self, source_key: &str) -> Option<&str> {
        self.synonyms.get(source_key).map(|s| s.as_str())
    }

    pub fn reservation(&self, node_id: NodeId) -> Option<&Reservation> {
        self.reservations.iter().find(|r| r.node_id == node_id)
    }

    pub fn reservations(&self) -> &[Reservation] {
        &self.reservations
    }

    pub fn canonization_events(&self) -> &[CanonizationEvent] {
        &self.canonization_events
    }

    pub fn root_goal(&self) -> Option<&serde_json::Value> {
        self.root_goal.as_ref()
    }

    pub fn embedding(&self) -> Option<&crate::types::EmbeddingContract> {
        self.embedding.as_ref()
    }

    // -----------------------------------------------------------------------
    // Mutation log / epoch
    // -----------------------------------------------------------------------

    /// Number of mutations currently awaiting flush.
    pub fn log_len(&self) -> usize {
        self.mutation_log.len()
    }

    /// `MutationEpoch` — bumps once per appended mutation; unchanged by reads and
    /// by draining. Recall caches key on this (spec §8).
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Drain the ordered mutation log into a batch (T3.4's flush input).
    pub fn drain_log(&mut self) -> MutationBatch {
        MutationBatch {
            mutations: std::mem::take(&mut self.mutation_log),
        }
    }

    // -----------------------------------------------------------------------
    // Invariants
    // -----------------------------------------------------------------------

    /// Verify every §5.7 invariant, collecting all violations into one error:
    /// session consistency, edge endpoints, natural-key uniqueness, finite
    /// non-negative weights, the temporal chain, Derives coverage, and
    /// Causal/Dependency acyclicity. `Ok(())` when the graph is well-formed.
    pub fn assert_invariants(&self) -> Result<(), LamboError> {
        let mut v: Vec<String> = Vec::new();

        for n in self.nodes.values() {
            if n.session_id() != &self.session_id {
                v.push(format!(
                    "node {} session {} != graph {}",
                    n.id(),
                    n.session_id(),
                    self.session_id
                ));
            }
        }

        for e in self.edges.values() {
            if e.session_id != self.session_id {
                v.push(format!(
                    "edge {} session {} != graph {}",
                    e.id, e.session_id, self.session_id
                ));
            }
            if !self.nodes.contains_key(&e.source) {
                v.push(format!("edge {} source {} missing", e.id, e.source));
            }
            if !self.nodes.contains_key(&e.target) {
                v.push(format!("edge {} target {} missing", e.id, e.target));
            }
            let w = e.weight;
            if !w.is_finite() || w < 0.0 {
                v.push(format!("edge {} weight {w} not finite and >= 0", e.id));
            }
        }

        if self.edge_keys.len() != self.edges.len() {
            v.push(format!(
                "natural-key index has {} entries for {} edges",
                self.edge_keys.len(),
                self.edges.len()
            ));
        }
        for ((s, t, ty), id) in &self.edge_keys {
            match self.edges.get(id) {
                Some(e)
                    if e.source == *s && e.target == *t && e.edge_type == *ty => {}
                _ => v.push(format!("natural-key index entry {s}->{t} {ty:?} inconsistent")),
            }
        }

        // Temporal chain: set equality with interaction nodes, link consistency,
        // exactly one Temporal predecessor per non-first interaction.
        // Convention: Temporal edges point back in time (source = newer, target =
        // previous — matches `scripts/gen-fixtures.py`), so each non-first
        // interaction carries exactly one outbound Temporal edge to its predecessor.
        let chain_set: HashSet<NodeId> = self.temporal_chain.iter().copied().collect();
        let interaction_ids: HashSet<NodeId> = self
            .nodes
            .iter()
            .filter_map(|(id, n)| match n {
                Node::Interaction(_) => Some(*id),
                _ => None,
            })
            .collect();
        if chain_set != interaction_ids {
            v.push("temporal chain does not cover exactly the interaction nodes".into());
        }
        for (pos, &id) in self.temporal_chain.iter().enumerate() {
            let inter = match self.nodes.get(&id) {
                Some(Node::Interaction(i)) => i,
                _ => {
                    v.push(format!("chain entry {id} is not an interaction"));
                    continue;
                }
            };
            let expected_prev = if pos == 0 {
                None
            } else {
                Some(self.temporal_chain[pos - 1])
            };
            if inter.previous_id != expected_prev {
                v.push(format!(
                    "interaction {} previous {:?} != chain position {}",
                    id, inter.previous_id, pos
                ));
            }
            let temporal_out = self.out_neighbors_typed(id, EdgeType::Temporal);
            let want = if pos == 0 { 0 } else { 1 };
            if temporal_out.len() != want {
                v.push(format!(
                    "interaction {} has {} Temporal out-edges (want {want})",
                    id,
                    temporal_out.len()
                ));
            }
            if pos > 0 && !temporal_out.contains(&self.temporal_chain[pos - 1]) {
                v.push(format!(
                    "interaction {} Temporal out-edge does not target the chain predecessor",
                    id
                ));
            }
        }

        // Derives: every concept has >= 1 inbound Derives from an interaction.
        for c in self.concepts() {
            let derives = self.in_neighbors_typed(c.id, EdgeType::Derives);
            if derives.is_empty() {
                v.push(format!("concept {} has no Derives edge", c.id));
            }
            for src in derives {
                if !matches!(self.nodes.get(&src), Some(Node::Interaction(_))) {
                    v.push(format!(
                        "concept {} Derives source {src} is not an interaction",
                        c.id
                    ));
                }
            }
        }

        // Causal/Dependency acyclicity (safety net; write-time rejection is T2.4).
        let mut color: HashMap<NodeId, u8> = HashMap::new();
        for n in self.nodes.keys() {
            if color.get(n).copied().unwrap_or(0) == 0 {
                let mut path = Vec::new();
                if let Some(back) = self.dfs_cycle(*n, &mut color, &mut path) {
                    v.push(format!("Causal/Dependency cycle detected through {back}"));
                    break;
                }
            }
        }

        if v.is_empty() {
            Ok(())
        } else {
            Err(invariant(v.join("; ")))
        }
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
            let existing = self.edges.get_mut(&existing_id).expect("edge_keys consistent");
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

    /// DFS over `Causal`/`Dependency` out-edges; returns a node on a back edge.
    fn dfs_cycle(
        &self,
        node: NodeId,
        color: &mut HashMap<NodeId, u8>,
        path: &mut Vec<NodeId>,
    ) -> Option<NodeId> {
        color.insert(node, 1); // gray
        path.push(node);
        let causal = self.out_neighbors_typed(node, EdgeType::Causal);
        let dependency = self.out_neighbors_typed(node, EdgeType::Dependency);
        for tgt in causal.into_iter().chain(dependency) {
            match color.get(&tgt).copied().unwrap_or(0) {
                1 => return Some(tgt), // back edge
                2 => continue,
                _ => {
                    if let Some(back) = self.dfs_cycle(tgt, color, path) {
                        return Some(back);
                    }
                }
            }
        }
        color.insert(node, 2); // black
        path.pop();
        None
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

fn invariant(msg: impl Into<String>) -> LamboError {
    LamboError::Store(StoreError::Invariant(msg.into()))
}

fn not_found(msg: String) -> LamboError {
    LamboError::Store(StoreError::NotFound(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    fn ts(minutes: i64) -> DateTime<Utc> {
        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        base + chrono::Duration::minutes(minutes)
    }

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn interaction(id: u64, prev: Option<NodeId>, at_min: i64) -> Interaction {
        Interaction {
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid(),
            agent_id: crate::types::AgentId::from("agent-a"),
            prompt_text: Some(format!("prompt {id}")),
            previous_id: prev,
            created_at: ts(at_min),
        }
    }

    fn concept(id: u64, origin: NodeId, content: &str) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(2, id)),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.into(),
            concept_type: crate::types::ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: crate::types::AgentId::from("agent-a"),
            created_at: ts(0),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: crate::types::CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
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

    fn uid(u: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(0, u))
    }

    /// Helper: fresh graph with one interaction + one derived concept.
    fn small_graph() -> (Graph, NodeId, NodeId) {
        let mut g = Graph::new(sid());
        let i = interaction(1, None, 0);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        let c = concept(1, iid, "user schema");
        let cid = c.id;
        g.insert_concept(c, iid).unwrap();
        (g, iid, cid)
    }

    #[test]
    fn empty_graph_is_consistent() {
        let g = Graph::new(sid());
        assert!(g.is_empty());
        assert_eq!(g.epoch(), 0);
        assert_eq!(g.log_len(), 0);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn insert_interaction_builds_chain_and_temporal_edge() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let i2 = interaction(2, Some(i1.id), 5);
        let i1_id = i1.id;
        let i2_id = i2.id;
        g.insert_interaction(i1).unwrap();
        assert_eq!(g.temporal_chain(), &[i1_id]);
        g.insert_interaction(i2).unwrap();
        assert_eq!(g.temporal_chain(), &[i1_id, i2_id]);
        // Structural Temporal edge exists: i2 -> i1.
        let e = g
            .edge_between(i2_id, i1_id, EdgeType::Temporal)
            .expect("temporal edge");
        assert_eq!(e.weight, 1.0);
        assert_eq!(g.edge_count(), 1);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn insert_interaction_rejects_bad_chain_positions() {
        let mut g = Graph::new(sid());
        // First interaction must have previous_id None.
        let i1 = interaction(1, Some(uid(99)), 0);
        assert!(g.insert_interaction(i1).is_err());
        g.assert_invariants().unwrap();

        let i1 = interaction(1, None, 0);
        let i1_id = i1.id;
        g.insert_interaction(i1).unwrap();

        // Non-first without previous_id.
        let bad = interaction(2, None, 5);
        assert!(g.insert_interaction(bad).is_err());
        // Non-first with a previous that is not the tail.
        let bad = interaction(2, Some(uid(999)), 5);
        assert!(g.insert_interaction(bad).is_err());
        // Chain unchanged after rejections.
        assert_eq!(g.temporal_chain(), &[i1_id]);
        assert_eq!(g.edge_count(), 0);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn reupsert_interaction_is_idempotent() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let i2 = interaction(2, Some(i1.id), 5);
        g.insert_interaction(i1.clone()).unwrap();
        g.insert_interaction(i2.clone()).unwrap();

        // Same position re-upsert: ok, single chain entry.
        g.insert_interaction(i2.clone()).unwrap();
        assert_eq!(g.temporal_chain(), &[i1.id, i2.id]);
        assert_eq!(g.node_count(), 2);
        g.assert_invariants().unwrap();

        // Changing position is rejected.
        let moved = interaction(2, None, 5);
        assert!(g.insert_interaction(moved).is_err());
    }

    #[test]
    fn insert_concept_creates_derives_edge() {
        let (g, iid, cid) = small_graph();
        let d = g.edge_between(iid, cid, EdgeType::Derives).expect("derives");
        assert_eq!(d.weight, 0.9);
        assert_eq!(g.edge_count(), 1);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn insert_concept_rejects_missing_or_non_interaction_origin() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();

        // Missing origin.
        let c = concept(1, uid(555), "orphan");
        assert!(g.insert_concept(c, uid(555)).is_err());
        // Origin that exists but is a concept, not an interaction.
        let c1 = concept(1, iid, "first");
        let c1_id = c1.id;
        g.insert_concept(c1, iid).unwrap();
        let c2 = concept(2, c1_id, "second");
        assert!(g.insert_concept(c2, c1_id).is_err());
        g.assert_invariants().unwrap();
    }

    #[test]
    fn reupsert_concept_reinforces_derives() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        let c = concept(1, iid, "user schema");
        let cid = c.id;
        g.insert_concept(c.clone(), iid).unwrap();
        g.insert_concept(c, iid).unwrap();

        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
        let d = g.edge_between(iid, cid, EdgeType::Derives).unwrap();
        assert_eq!(d.reinforcements, 2);
        assert_eq!(d.weight, (0.9 + REINFORCE_BUMP).min(MAX_EDGE_WEIGHT));
        g.assert_invariants().unwrap();
    }

    #[test]
    fn duplicate_edge_reinforces_in_place() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        let c1 = concept(1, iid, "user schema");
        let cid = c1.id;
        g.insert_concept(c1, iid).unwrap();
        let c2 = concept(2, iid, "auth middleware");
        let c2id = c2.id;
        g.insert_concept(c2, iid).unwrap();

        let e1 = edge(1, cid, c2id, EdgeType::CoOccurrence, 0.5);
        let e1_created = e1.created_at;
        g.upsert_edge(e1.clone()).unwrap();
        let mut e2 = edge(2, cid, c2id, EdgeType::CoOccurrence, 0.5);
        e2.last_reinforced = ts(99);
        g.upsert_edge(e2).unwrap();

        // Single edge, reinforced in place.
        assert_eq!(g.edge_count(), 3); // derives x2 + cooccurrence
        let e = g.edge_between(cid, c2id, EdgeType::CoOccurrence).unwrap();
        assert_eq!(e.reinforcements, 2);
        assert_eq!(e.weight, 0.5 + REINFORCE_BUMP);
        assert_eq!(e.created_at, e1_created);
        assert_eq!(e.last_reinforced, ts(99));
        // Original id wins.
        assert_eq!(e.id, e1.id);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn edge_weight_normalization() {
        let (mut g, iid, cid) = small_graph();
        let c2 = concept(2, iid, "auth middleware");
        let c2id = c2.id;
        g.insert_concept(c2, iid).unwrap();

        // NaN and +Inf clamp to 0.0.
        let nan = edge(1, cid, c2id, EdgeType::Semantic, f64::NAN);
        g.upsert_edge(nan).unwrap();
        let e = g.edge_between(cid, c2id, EdgeType::Semantic).unwrap();
        assert_eq!(e.weight, 0.0);

        let inf = edge(2, cid, c2id, EdgeType::Hierarchical, f64::INFINITY);
        g.upsert_edge(inf).unwrap();
        let e = g.edge_between(cid, c2id, EdgeType::Hierarchical).unwrap();
        assert_eq!(e.weight, 0.0);

        // Negative (and -Inf) rejected.
        let neg = edge(3, cid, c2id, EdgeType::Causal, -1.0);
        assert!(g.upsert_edge(neg).is_err());
        let neg_inf = edge(4, cid, c2id, EdgeType::Dependency, f64::NEG_INFINITY);
        assert!(g.upsert_edge(neg_inf).is_err());
        g.assert_invariants().unwrap();
    }

    #[test]
    fn upsert_edge_validates_endpoints_and_session() {
        let (mut g, iid, cid) = small_graph();
        let c2 = concept(2, iid, "auth middleware");
        let c2id = c2.id;
        g.insert_concept(c2, iid).unwrap();

        // Missing source / target.
        assert!(g
            .upsert_edge(edge(1, uid(900), c2id, EdgeType::Causal, 0.5))
            .is_err());
        assert!(g
            .upsert_edge(edge(2, cid, uid(900), EdgeType::Causal, 0.5))
            .is_err());

        // Session mismatch.
        let mut bad = edge(3, cid, c2id, EdgeType::Causal, 0.5);
        bad.session_id = SessionId::from("other");
        assert!(g.upsert_edge(bad).is_err());
        g.assert_invariants().unwrap();
    }

    #[test]
    fn remove_node_cleans_incident_edges() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let i2 = interaction(2, Some(i1.id), 5);
        let i2id = i2.id;
        g.insert_interaction(i1).unwrap();
        g.insert_interaction(i2).unwrap();
        let c = concept(1, i2id, "user schema");
        let cid = c.id;
        g.insert_concept(c, i2id).unwrap();

        // Extra concept-to-concept edge.
        let c2 = concept(2, i2id, "api layer");
        let c2id = c2.id;
        g.insert_concept(c2, i2id).unwrap();
        g.upsert_edge(edge(1, cid, c2id, EdgeType::Dependency, 0.7))
            .unwrap();
        g.upsert_edge(edge(2, c2id, cid, EdgeType::Dependency, 0.7))
            .unwrap();

        // Remove c2: both dependency edges must go; Derives (i2 -> c2) must go too.
        let edges_before = g.edge_count();
        g.remove_node(c2id).unwrap();
        assert_eq!(g.node_count(), 3); // i1, i2, c
        assert!(g.node(c2id).is_none());
        assert_eq!(g.edge_count(), edges_before - 3);
        assert!(g.edge_between(i2id, c2id, EdgeType::Derives).is_none());
        assert!(g.edge_between(cid, c2id, EdgeType::Dependency).is_none());
        assert!(g.out_neighbors(cid).is_empty());
        assert!(g.in_neighbors(c2id).is_empty());
        g.assert_invariants().unwrap();
    }

    #[test]
    fn remove_missing_node_or_edge_errors() {
        let (mut g, _, _) = small_graph();
        assert!(g.remove_node(uid(999)).is_err());
        assert!(g.remove_edge(uid(999)).is_err());
        g.assert_invariants().unwrap();
    }

    #[test]
    fn cycle_is_detected_by_assert_invariants() {
        let (mut g, iid, cid) = small_graph();
        let c2 = concept(2, iid, "auth middleware");
        let c2id = c2.id;
        g.insert_concept(c2, iid).unwrap();

        // A -> B -> A dependency cycle. upsert_edge stores it; write-time rejection
        // is record_action's (T2.4); assert_invariants must flag it.
        g.upsert_edge(edge(1, cid, c2id, EdgeType::Dependency, 0.7))
            .unwrap();
        g.upsert_edge(edge(2, c2id, cid, EdgeType::Dependency, 0.7))
            .unwrap();
        let err = g.assert_invariants().unwrap_err().to_string();
        assert!(err.contains("cycle"), "{err}");
    }

    #[test]
    fn transition_applies_status_and_appends_event() {
        let (mut g, _, cid) = small_graph();
        let ev = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid(),
            node_id: cid,
            from_status: crate::types::CanonizationStatus::None,
            to_status: crate::types::CanonizationStatus::Candidate,
            blast_radius: Some(3),
            occurred_at: ts(10),
        };
        g.apply_canonization_transition(ev.clone()).unwrap();
        let c = match g.node(cid).unwrap() {
            Node::Concept(c) => c,
            _ => panic!("concept"),
        };
        assert_eq!(
            c.canonization_status,
            crate::types::CanonizationStatus::Candidate
        );
        assert_eq!(c.blast_radius, Some(3));
        assert_eq!(g.canonization_events(), &[ev]);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn synonyms_declare_lookup_and_snapshot() {
        let mut g = Graph::new(sid());
        g.declare_synonym("register_user", "create_user");
        g.declare_synonym("delete_user", "remove_user");
        assert_eq!(g.synonym("register_user"), Some("create_user"));
        assert_eq!(g.synonym("delete_user"), Some("remove_user"));
        assert_eq!(g.synonym("unknown"), None);
        // Replace wins.
        g.declare_synonym("register_user", "signup_user");
        assert_eq!(g.synonym("register_user"), Some("signup_user"));

        let snap = g.snapshot();
        let keys: Vec<&str> = snap.synonyms.iter().map(|s| s.source_key.as_str()).collect();
        assert_eq!(keys, vec!["delete_user", "register_user"]); // sorted
        assert_eq!(snap.synonyms.len(), 2);
        assert!(g.mutation_log.is_empty(), "synonyms are RAM-local");
    }

    #[test]
    fn reservations_round_trip_through_snapshot() {
        let mut g = Graph::new(sid());
        let r = Reservation {
            session_id: sid(),
            node_id: uid(42),
            agent_id: crate::types::AgentId::from("agent-a"),
            expires_at: ts(5),
        };
        g.set_reservation(r.clone());
        assert_eq!(g.reservation(uid(42)), Some(&r));
        g.clear_reservation(uid(42));
        assert_eq!(g.reservation(uid(42)), None);
        g.set_reservation(r.clone());
        let snap = g.snapshot();
        assert_eq!(snap.reservations, vec![r]);
    }

    #[test]
    fn epoch_bumps_per_mutation_not_per_read() {
        let (mut g, iid, cid) = small_graph();
        let e0 = g.epoch();
        assert!(e0 > 0);
        // Reads do not bump.
        let _ = g.node(cid);
        let _ = g.out_neighbors(iid);
        assert_eq!(g.epoch(), e0);
        // drain does not reset.
        let _ = g.drain_log();
        assert_eq!(g.epoch(), e0);
        // Next write bumps again.
        g.upsert_edge(edge(1, iid, cid, EdgeType::Semantic, 0.5))
            .unwrap();
        assert!(g.epoch() > e0);
    }

    #[test]
    fn drain_log_clears_and_orders_writes() {
        let (mut g, iid, cid) = small_graph();
        let c2 = concept(2, iid, "auth middleware");
        let c2id = c2.id;
        g.insert_concept(c2, iid).unwrap();
        g.upsert_edge(edge(1, cid, c2id, EdgeType::CoOccurrence, 0.5))
            .unwrap();
        g.remove_node(c2id).unwrap();

        let batch = g.drain_log();
        assert_eq!(g.log_len(), 0);
        assert!(g.drain_log().is_empty());

        // Ordering contract: every edge's endpoints were upserted earlier in the
        // same batch; deletions follow upserts; node deletions follow their
        // incident edge deletions.
        let mut seen_nodes: HashSet<NodeId> = HashSet::new();
        let mut seen_edges: HashSet<NodeId> = HashSet::new();
        let mut saw_delete = false;
        for m in &batch.mutations {
            match m {
                Mutation::UpsertNode { node } => {
                    assert!(!saw_delete, "node upsert after deletion: {m:?}");
                    seen_nodes.insert(node.id());
                }
                Mutation::UpsertEdge { edge } => {
                    assert!(!saw_delete, "edge upsert after deletion: {m:?}");
                    assert!(seen_nodes.contains(&edge.source), "{m:?}");
                    assert!(seen_nodes.contains(&edge.target), "{m:?}");
                    seen_edges.insert(edge.id);
                }
                Mutation::DeleteEdge { id } => {
                    saw_delete = true;
                    assert!(seen_edges.contains(id) || seen_nodes.contains(id), "{m:?}");
                }
                Mutation::DeleteNode { id } => {
                    assert!(saw_delete, "DeleteNode must follow incident DeleteEdges");
                    assert!(seen_nodes.contains(id), "{m:?}");
                }
                Mutation::CanonizationTransition { .. } => {}
            }
        }
        assert!(saw_delete);
    }

    #[test]
    fn fixture_rest_api_loads_and_passes_invariants() {
        let snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
        let g = Graph::from_snapshot(snap).unwrap();
        g.assert_invariants().unwrap();
        assert_eq!(g.node_count(), 12 + 22);
        assert_eq!(g.temporal_chain().len(), 12);
        // Every concept has a Derives edge.
        for c in g.concepts() {
            assert!(!g.in_neighbors_typed(c.id, EdgeType::Derives).is_empty());
        }
        // Snapshot round-trips exactly (fixture order == snapshot order).
        let snap2 = crate::fixtures::load_snapshot("session-rest-api").unwrap();
        assert_eq!(g.snapshot(), snap2);
    }

    #[test]
    fn fixture_drift_loads_and_passes_invariants() {
        let snap = crate::fixtures::load_snapshot("session-drift").unwrap();
        let g = Graph::from_snapshot(snap).unwrap();
        g.assert_invariants().unwrap();
        assert_eq!(g.temporal_chain().len(), 2);
        assert_eq!(g.edge_count(), 17);
        assert_eq!(g.node_count(), 9 + 2);
    }

    #[test]
    fn from_snapshot_rejects_violating_graphs() {
        // Edge referencing a missing node.
        let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
        let mut bad_edge = snap.edges[0].clone();
        bad_edge.source = uid(999);
        snap.edges.push(bad_edge);
        assert!(Graph::from_snapshot(snap).is_err());

        // Forked temporal chain: two interactions claiming the same predecessor.
        let mut snap = crate::fixtures::load_snapshot("session-drift").unwrap();
        let mut fork = snap.interactions[1].clone();
        fork.previous_id = snap.interactions[0].previous_id; // both now head-adjacent
        fork.id = uid(700);
        fork.session_id = snap.session_id.clone();
        snap.interactions.push(fork);
        // Its own Derives/Temporal edges are missing, but the chain fork fires first.
        assert!(Graph::from_snapshot(snap).is_err());

        // Negative edge weight rejected.
        let mut snap = crate::fixtures::load_snapshot("session-drift").unwrap();
        let mut bad = snap.edges[0].clone();
        bad.weight = -0.5;
        snap.edges[0] = bad;
        assert!(Graph::from_snapshot(snap).is_err());
    }

    #[test]
    fn invariant_report_lists_every_violation() {
        let mut g = Graph::new(sid());
        // No interactions yet; add one concept whose origin does not exist.
        let c = concept(1, uid(999), "orphan");
        let err = g.insert_concept(c, uid(999)).unwrap_err().to_string();
        assert!(err.contains("not an interaction"), "{err}");
    }
}

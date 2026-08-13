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

use crate::types::{
    CanonizationEvent, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, GraphSnapshot,
    Interaction, LamboError, Mutation, MutationBatch, Node, NodeId, Reservation, SessionId,
    StoreError, Synonym,
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
#[derive(Clone, Debug)]
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
    ///
    /// A zero-interaction snapshot is a valid **empty** graph (adve-review
    /// GRAPH-6 — "expected exactly one chain head, found 0" was rejecting
    /// fresh sessions), and duplicate natural-key edges are rejected rather than
    /// silently merged via reinforcement (GRAPH-7 — the loaded graph must equal
    /// the stored snapshot; reinforcement is a write-path semantic, not a load
    /// one).
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
        if snap.interactions.is_empty() {
            // GRAPH-6: a zero-interaction snapshot is a valid empty graph, not a
            // malformed chain. A non-empty snapshot with a forked/absent chain
            // still fails below; concepts without Derives edges still fail
            // assert_invariants.
            g.temporal_chain = Vec::new();
        } else {
            if heads.len() != 1 {
                return Err(invariant(format!(
                    "expected exactly one chain head, found {}",
                    heads.len()
                )));
            }
            let mut chain = Vec::with_capacity(snap.interactions.len());
            let mut visited: HashSet<NodeId> = HashSet::with_capacity(snap.interactions.len());
            let mut cur = heads[0];
            loop {
                if !visited.insert(cur) {
                    return Err(invariant("cycle in temporal chain"));
                }
                chain.push(cur);
                match next_of.get(&cur) {
                    Some(&next) => cur = next,
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
        }

        // GRAPH-7: duplicate (source, target, edge_type) in one snapshot must be
        // rejected up front — record_edge would silently reinforce, leaving a
        // loaded graph that disagrees with the stored snapshot.
        let mut seen_edge_keys: HashSet<EdgeKey> = HashSet::with_capacity(snap.edges.len());
        for e in &snap.edges {
            let key = (e.source, e.target, e.edge_type);
            if !seen_edge_keys.insert(key) {
                return Err(invariant(format!(
                    "duplicate natural-key edge ({}, {}, {:?}) in snapshot",
                    key.0, key.1, key.2
                )));
            }
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
            g.synonyms
                .insert(s.source_key.clone(), s.canonical_key.clone());
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
        let interactions: Vec<Interaction> = self
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
        // Interactions were collected in temporal chain order above — do NOT sort
        // them by id (adve-review T2.1 S4): the chain order is the documented
        // contract, and random v4 UUIDs would silently destroy it.
        concepts.sort_by_key(|c| c.id.0);
        edges.sort_by_key(|e| e.id.0);
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
    ///
    /// Write-gate validation (adve-review GRAPH-4): `from_status` must equal the
    /// concept's **current** status — a fabricated audit row is rejected with a
    /// typed invariant error — and the pair must be an edge of the spec §10
    /// state machine ([`legal_canonization_transition`]; stage skips, downgrades
    /// and self-loops are rejected). A demotion event additionally carries the
    /// concept's new `last_demotion_time` (COH-3, spec §10 "Demotion sets
    /// `last_demotion_time`"); non-demotion events leave that field untouched.
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
        if concept.canonization_status != event.from_status {
            return Err(invariant(format!(
                "canonization transition for {} claims {:?} -> {:?} but the concept's \
                 current status is {:?} (fabricated transition rejected)",
                event.node_id, event.from_status, event.to_status, concept.canonization_status
            )));
        }
        if !legal_canonization_transition(event.from_status, event.to_status) {
            return Err(invariant(format!(
                "illegal canonization transition {:?} -> {:?} for concept {} \
                 (spec §10 state machine)",
                event.from_status, event.to_status, event.node_id
            )));
        }
        concept.canonization_status = event.to_status;
        concept.blast_radius = event.blast_radius;
        // COH-3: a demotion event always carries Some (the concept's new
        // last_demotion_time); a non-demotion event's None must not clobber a
        // previously demoted concept's value.
        if let Some(t) = event.last_demotion_time {
            concept.last_demotion_time = Some(t);
        }
        self.canonization_events.push(event.clone());
        self.append_mutation(Mutation::CanonizationTransition { event });
        Ok(())
    }

    /// GC survivor bookkeeping (T4.5, spec §9 step 5): increment `gc_survived`
    /// on every surviving concept — canonization Stage 1's input.
    ///
    /// Missing ids are skipped; the count is **saturating** (`i32` is the
    /// schema column type — 2^31 GC cycles would otherwise overflow). Each
    /// bump is emitted as an `UpsertNode` mutation so the durable store
    /// mirrors the counter (spec §2.4 log contract; the store's upsert
    /// replaces the row in place).
    pub fn bump_gc_survived(&mut self, concept_ids: &[NodeId]) -> usize {
        let mut bumped = 0;
        let mut updates: Vec<Concept> = Vec::new();
        for &id in concept_ids {
            let Some(Node::Concept(c)) = self.nodes.get_mut(&id) else {
                continue;
            };
            c.gc_survived = c.gc_survived.saturating_add(1);
            bumped += 1;
            updates.push(c.clone());
        }
        for c in updates {
            self.append_mutation(Mutation::UpsertNode {
                node: Node::Concept(c),
            });
        }
        bumped
    }

    // -----------------------------------------------------------------------
    // Write path — session metadata, synonyms, reservations
    // -----------------------------------------------------------------------

    /// Declare (or replace) a direct synonym mapping. RAM-local: synonyms have no
    /// `Mutation` kind and round-trip through the snapshot only. A changed
    /// mapping still bumps the epoch because it changes canonicalization results
    /// observed by hybrid planning and recall.
    pub fn declare_synonym(&mut self, source_key: &str, canonical_key: &str) {
        let changed = self.synonyms.get(source_key).map(String::as_str) != Some(canonical_key);
        if changed {
            self.synonyms
                .insert(source_key.to_string(), canonical_key.to_string());
            self.epoch += 1;
        }
    }

    /// Declare the session's root goal (spec §9 drift anchor).
    ///
    /// Spec §9: "Root goal nodes are automatically `Venerable`" — **every**
    /// concept the goal names (matched by `content` or `canonical_key`) is
    /// promoted to `Venerable` through the T2.1 mutation path
    /// ([`Graph::apply_canonization_transition`]: audit row +
    /// `Mutation::CanonizationTransition`), so the promotion is durable and
    /// visible to the §10 state machine — not a bare field flip. The goal itself
    /// is recorded as `Mutation::SetRootGoal` (XP-8), so it survives a reload.
    ///
    /// ## Accepted goal shapes ([`root_goal_texts`], ALGO-6)
    ///
    /// A bare string, **an array of strings** (spec §6.1's own example is a
    /// list), or the `{content, key}` object form. Anything else is stored but
    /// names no concept. A multi-goal session promotes all matches
    /// **id-ascending** — the previous code took the first `HashMap` match,
    /// which is iteration-order dependent and therefore nondeterministic under
    /// multiple matches (ALGO-12).
    ///
    /// The §10 state machine has no `Venerable -> Venerable` or
    /// `Canonical -> Venerable` edge, so a goal concept that is already
    /// `Venerable` or `Canonical` is left untouched (a `Canonical` root goal
    /// is strictly stronger protection); clearing the goal (`None`) stores
    /// the clear and never demotes.
    ///
    /// `occurred_at` is **logical time** — the session's newest interaction
    /// timestamp ([`Graph::logical_now`]) — not `Utc::now()`: this is otherwise
    /// a wholly logical-time write path (`record_action` takes no clock), and a
    /// wall-clock stamp made the audit trail non-monotonic against the rows
    /// around it (ALGO-12).
    pub fn set_root_goal(&mut self, goal: Option<serde_json::Value>) {
        let texts = root_goal_texts(goal.as_ref());
        if !texts.is_empty() {
            let occurred_at = self.logical_now();
            let mut matches: Vec<NodeId> = self
                .nodes
                .iter()
                .filter_map(|(id, n)| match n {
                    Node::Concept(c)
                        if texts
                            .iter()
                            .any(|t| c.content == *t || c.canonical_key == *t) =>
                    {
                        Some(*id)
                    }
                    _ => None,
                })
                .collect();
            matches.sort_by_key(|id| id.0);
            for cid in matches {
                let status = match self.nodes.get(&cid) {
                    Some(Node::Concept(c)) => c.canonization_status,
                    _ => continue,
                };
                if matches!(
                    status,
                    CanonizationStatus::None | CanonizationStatus::Candidate
                ) {
                    let event = CanonizationEvent {
                        id: NodeId::new(),
                        session_id: self.session_id.clone(),
                        node_id: cid,
                        from_status: status,
                        to_status: CanonizationStatus::Venerable,
                        blast_radius: None,
                        last_demotion_time: None,
                        occurred_at,
                    };
                    // The only rejection modes are invariant violations that
                    // cannot occur here (the concept exists; the pair is a
                    // legal §10 edge), so the promotion is best-effort.
                    let _ = self.apply_canonization_transition(event);
                }
            }
        }
        // XP-8: the goal is durable. A reload without this replayed an empty
        // goal, which silently disabled drift detection and emptied GC's
        // root-goal exclusion. The mutation also bumps the epoch, so T5.4's
        // recall cache cannot serve results computed against the old goal.
        if self.root_goal != goal {
            self.root_goal = goal.clone();
            self.append_mutation(Mutation::SetRootGoal {
                session_id: self.session_id.clone(),
                goal,
            });
        }
    }

    /// The session's logical "now": its newest interaction timestamp, falling
    /// back to the newest concept's (a session with concepts but no interactions
    /// is not constructible through the write API) and finally to the epoch
    /// origin. Write paths that need a timestamp but take no clock use this so
    /// the audit trail stays monotonic (ALGO-12).
    pub fn logical_now(&self) -> chrono::DateTime<chrono::Utc> {
        self.interactions()
            .map(|i| i.created_at)
            .chain(self.concepts().map(|c| c.created_at))
            .max()
            .unwrap_or_else(|| chrono::DateTime::from_timestamp_nanos(0))
    }

    /// Stamp the session's embedding space on first vector work, or verify an
    /// existing stamp. Ordinary callers cannot clear or replace the contract.
    pub fn stamp_embedding(
        &mut self,
        contract: crate::types::EmbeddingContract,
    ) -> Result<(), LamboError> {
        if let Some(existing) = &self.embedding {
            existing.ensure_compatible(&contract)?;
            return Ok(());
        }
        self.embedding = Some(contract.clone());
        self.append_mutation(Mutation::SetEmbedding {
            session_id: self.session_id.clone(),
            embedding: Some(contract),
        });
        Ok(())
    }

    /// Explicit contract replacement/clear gate for an atomic re-embedding
    /// workflow. It is safe only after every vector-bearing concept has been
    /// removed or rewritten in the same staged graph transaction.
    pub fn replace_embedding_without_vectors(
        &mut self,
        contract: Option<crate::types::EmbeddingContract>,
    ) -> Result<(), LamboError> {
        if self.concepts().any(|c| c.embedding.is_some()) {
            return Err(invariant(
                "cannot clear or replace embedding contract while concept vectors remain",
            ));
        }
        if self.embedding != contract {
            self.embedding = contract.clone();
            self.append_mutation(Mutation::SetEmbedding {
                session_id: self.session_id.clone(),
                embedding: contract,
            });
        }
        Ok(())
    }
    /// Advisory soft lock (spec §11). Same-agent re-reservation extends; cross-agent
    /// denial is T2.7's policy — this stores what it is given.
    pub fn set_reservation(&mut self, r: Reservation) {
        if let Some(existing) = self
            .reservations
            .iter_mut()
            .find(|x| x.node_id == r.node_id)
        {
            *existing = r;
        } else {
            self.reservations.push(r);
        }
        // Reservations render into recall context (T2.7 soft-lock line), so a
        // transition must invalidate the epoch-keyed recall cache. Reservations
        // are RAM-local (no Mutation kind), so bump the epoch directly (P5
        // phase-close finding).
        self.epoch += 1;
    }

    pub fn clear_reservation(&mut self, node_id: NodeId) {
        self.reservations.retain(|r| r.node_id != node_id);
        self.epoch += 1;
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

    pub fn synonyms(&self) -> impl Iterator<Item = (&str, &str)> {
        self.synonyms.iter().map(|(k, v)| (k.as_str(), v.as_str()))
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

    /// The concept-naming strings in the session's root goal — see
    /// [`root_goal_texts`]. The single reading of the goal shape, shared by
    /// [`Graph::set_root_goal`], drift detection and GC's exclusion list, so the
    /// three cannot disagree about what "the root goal" names (ALGO-6).
    pub fn root_goal_texts(&self) -> Vec<String> {
        root_goal_texts(self.root_goal.as_ref())
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
    ///
    /// The batch is in **chronological** write order. §2.4's phase grouping
    /// (nodes -> edges -> deletions -> transitions) holds within a single logical
    /// write, not across the batch. Replay in order — never re-sort.
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
            if let Node::Concept(c) = n {
                if let Some(vector) = &c.embedding {
                    match &self.embedding {
                        Some(contract)
                            if vector.len() == contract.dim
                                && vector.iter().all(|x| x.is_finite()) => {}
                        Some(contract) => v.push(format!(
                            "concept {} vector invalid for embedding contract width {}",
                            c.id, contract.dim
                        )),
                        None => v.push(format!(
                            "concept {} carries a vector without an embedding contract",
                            c.id
                        )),
                    }
                }
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
            // GRAPH-2: endpoint-type matrix (spec §5) — assert_invariants is the
            // safety net; record_edge rejects the class at the write gate, this
            // arm catches any graph that got into that state another way.
            if let (Some(s), Some(t)) = (self.nodes.get(&e.source), self.nodes.get(&e.target)) {
                if let Some(msg) = edge_endpoint_error(e.edge_type, s, t) {
                    v.push(format!("edge {} {msg}", e.id));
                }
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
                Some(e) if e.source == *s && e.target == *t && e.edge_type == *ty => {}
                _ => v.push(format!(
                    "natural-key index entry {s}->{t} {ty:?} inconsistent"
                )),
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

        // Canonical-key uniqueness (schema §4 UNIQUE, partial: Observations
        // exempt — demote creates context-overflow duplicates by design,
        // spec errata 2026-08-11 / muse-spark M1-M2).
        let mut keys: HashMap<&str, NodeId> = HashMap::new();
        for c in self
            .concepts()
            .filter(|c| c.concept_type != ConceptType::Observation)
        {
            if let Some(prev) = keys.insert(c.canonical_key.as_str(), c.id) {
                v.push(format!(
                    "concepts {prev} and {} share canonical_key {:?} (UNIQUE; Observations exempt)",
                    c.id, c.canonical_key
                ));
            }
        }

        // Causal/Dependency/Hierarchical acyclicity (safety net; write-time
        // rejection of Causal/Dependency cycles is T2.4's BFS; Hierarchical is a
        // DAG constraint by definition).
        let mut color: HashMap<NodeId, u8> = HashMap::new();
        for n in self.nodes.keys() {
            if color.get(n).copied().unwrap_or(0) == 0 {
                if let Some(back) = self.dfs_cycle(*n, &mut color) {
                    v.push(format!(
                        "Causal/Dependency/Hierarchical cycle detected through {back}"
                    ));
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

/// The concept-naming strings in a `root_goal` value (ALGO-6).
///
/// Accepted shapes, deduplicated and sorted so every consumer sees the same list
/// in the same order:
///
/// * `"launch the product"` — a single goal.
/// * `["launch the product", "ship the API"]` — spec §6.1's own `root_goal`
///   example is a **list**, and the string-only reading silently disabled drift
///   detection, auto-`Venerable` promotion and GC's root-goal exclusion for
///   every array goal. Non-string elements are ignored rather than rejected: the
///   goal is stored either way, so a partially structured goal still anchors the
///   names it does carry.
/// * `{"content": …, "key": …}` — the object form GC already accepted; kept so
///   an existing session's exclusion list does not change meaning.
///
/// Any other shape names no concept (it is still stored — spec §6.1 types
/// `root_goal` as free-form JSON).
pub fn root_goal_texts(goal: Option<&serde_json::Value>) -> Vec<String> {
    let Some(goal) = goal else {
        return Vec::new();
    };
    let mut texts: Vec<String> = match goal {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        serde_json::Value::Object(map) => ["content", "key"]
            .iter()
            .filter_map(|k| map.get(*k).and_then(|v| v.as_str()).map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    texts.sort();
    texts.dedup();
    texts
}

/// Legal spec §10 state-machine edges (adve-review GRAPH-4): Stage 1 promotes
/// `None -> Candidate`, Stage 2 promotes to `Venerable` (from `None` or
/// `Candidate` — the two stages evaluate independent evidence), Stage 3 promotes
/// `Venerable -> Canonical`, and demotion returns `Canonical -> None` (spec §10:
/// demotion nulls `blast_radius` and sets `last_demotion_time`). Everything else
/// — stage skips, downgrades, and self-loops — is rejected at the
/// [`Graph::apply_canonization_transition`] write gate.
fn legal_canonization_transition(from: CanonizationStatus, to: CanonizationStatus) -> bool {
    matches!(
        (from, to),
        (CanonizationStatus::None, CanonizationStatus::Candidate)
            | (CanonizationStatus::None, CanonizationStatus::Venerable)
            | (CanonizationStatus::Candidate, CanonizationStatus::Venerable)
            | (CanonizationStatus::Venerable, CanonizationStatus::Canonical)
            | (CanonizationStatus::Canonical, CanonizationStatus::None)
    )
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
        let d = g
            .edge_between(iid, cid, EdgeType::Derives)
            .expect("derives");
        assert_eq!(d.weight, 0.9);
        assert_eq!(g.edge_count(), 1);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn insert_concept_enforces_partial_canonical_key_uniqueness() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();

        // Two non-Observation concepts with the same canonical key -> rejected
        // (schema §4 UNIQUE, spec errata 2026-08-11 / muse-spark M1).
        let mut c1 = concept(1, iid, "user schema");
        c1.canonical_key = "schema user".into();
        g.insert_concept(c1, iid).unwrap();
        let mut c2 = concept(2, iid, "schema user");
        c2.canonical_key = "schema user".into();
        let err = g.insert_concept(c2, iid).unwrap_err().to_string();
        assert!(err.contains("collides"), "{err}");
        // Rejection leaves the graph unchanged.
        assert_eq!(g.node_count(), 2);
        // Same-id re-upsert is idempotent and allowed (not a collision).
        let mut c1b = concept(1, iid, "user schema");
        c1b.canonical_key = "schema user".into();
        g.insert_concept(c1b, iid).unwrap();
        assert_eq!(g.node_count(), 2);

        // Observations are exempt (demote creates context-overflow duplicates
        // by design — muse-spark M2): two Observations sharing a key are fine.
        let mut o1 = concept(3, iid, "drift note");
        o1.concept_type = ConceptType::Observation;
        o1.canonical_key = "note".into();
        g.insert_concept(o1, iid).unwrap();
        let mut o2 = concept(4, iid, "drift note");
        o2.concept_type = ConceptType::Observation;
        o2.canonical_key = "note".into();
        g.insert_concept(o2, iid).unwrap();
        // Observation + Entity sharing a key: only one non-Observation row.
        let mut o3 = concept(5, iid, "schema user");
        o3.concept_type = ConceptType::Observation;
        o3.canonical_key = "schema user".into();
        g.insert_concept(o3, iid).unwrap();
        g.assert_invariants().unwrap();
    }

    #[cfg(feature = "fixtures")]
    #[test]
    fn assert_invariants_rejects_duplicate_canonical_keys_on_load() {
        // A loaded snapshot with two non-Observation concepts sharing a key
        // must fail assert_invariants (from_snapshot -> invariant check).
        let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
        let mut clone = snap.concepts[0].clone();
        clone.id = NodeId::new();
        clone.content = "colliding clone".into();
        snap.concepts.push(clone);
        let err = Graph::from_snapshot(snap).unwrap_err().to_string();
        assert!(err.contains("canonical_key"), "{err}");
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
    fn record_edge_rejects_type_invalid_endpoints() {
        // GRAPH-2: spec §5 pins what each edge type connects; the write gate
        // must reject type-invalid endpoint pairs instead of storing them (they
        // would pollute recall BFS permanently).
        let (mut g, iid, cid) = small_graph();
        let c2 = concept(2, iid, "auth middleware");
        let c2id = c2.id;
        g.insert_concept(c2, iid).unwrap();

        // Concept-only edge types from/to an interaction.
        let err = g
            .upsert_edge(edge(1, iid, c2id, EdgeType::Semantic, 0.5))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Concept -> Concept"), "{err}");
        let err = g
            .upsert_edge(edge(2, c2id, iid, EdgeType::Causal, 0.5))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Concept -> Concept"), "{err}");
        // Temporal must connect interactions.
        let err = g
            .upsert_edge(edge(3, iid, cid, EdgeType::Temporal, 0.5))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Interaction -> Interaction"), "{err}");
        // Derives must connect interaction -> concept.
        let err = g
            .upsert_edge(edge(4, cid, c2id, EdgeType::Derives, 0.5))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Interaction -> Concept"), "{err}");
        // A legal Concept -> Concept edge still writes.
        g.upsert_edge(edge(5, cid, c2id, EdgeType::Semantic, 0.5))
            .unwrap();
        // Nothing was written by the rejected attempts (derives x2 + semantic).
        assert_eq!(g.edge_count(), 3);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn assert_invariants_flags_type_invalid_edges() {
        // GRAPH-2: assert_invariants must catch the class even if an edge got
        // into the graph another way (record_edge rejects it, so inject into the
        // private indexes directly — defense for future load-path bugs).
        let (mut g, iid, _) = small_graph();
        let c2 = concept(2, iid, "auth middleware");
        let c2id = c2.id;
        g.insert_concept(c2, iid).unwrap();
        let bad = edge(9, iid, c2id, EdgeType::Semantic, 0.5);
        g.edges.insert(bad.id, bad.clone());
        g.edge_keys
            .insert((bad.source, bad.target, bad.edge_type), bad.id);
        g.add_adjacency(bad.source, bad.target, bad.edge_type);
        let err = g.assert_invariants().unwrap_err().to_string();
        assert!(err.contains("Semantic edge must connect"), "{err}");
    }

    /// CONC-1: `incident_edges` must read the **adjacency index**, not scan the
    /// edge set. Structural, not timing-based: an edge present in `edges` +
    /// `edge_keys` but *absent* from the adjacency maps is invisible to an
    /// index-backed lookup and visible to a `edges.values()` filter — so this
    /// fails on the pre-fix implementation and passes on the index-backed one.
    ///
    /// The daemon calls this per concept in every detector and in `rescore`, so
    /// the scan made each pass `O(nodes × edges)` and held the graph lock for
    /// hundreds of milliseconds per cycle at 4k concepts (§6.4's second clause).
    #[test]
    fn incident_edges_reads_the_adjacency_index_not_the_edge_map() {
        let (mut g, iid, cid) = small_graph();
        let c2 = concept(2, iid, "api layer");
        let c2id = c2.id;
        g.insert_concept(c2, iid).unwrap();
        g.upsert_edge(edge(1, cid, c2id, EdgeType::Dependency, 0.7))
            .unwrap();

        let via_index: Vec<NodeId> = g.incident_edges(cid).iter().map(|e| e.id).collect();
        // Ground truth for this graph: the Derives provenance edge + the
        // Dependency edge, id-ascending.
        let mut expected: Vec<NodeId> = g
            .edges
            .values()
            .filter(|e| e.source == cid || e.target == cid)
            .map(|e| e.id)
            .collect();
        expected.sort_by_key(|id| id.0);
        assert_eq!(via_index, expected, "index-backed lookup must be complete");

        // Now desynchronize: an edge in `edges`/`edge_keys` but not in the
        // adjacency maps. An index-backed reader cannot see it; a full scan can.
        let ghost = edge(99, c2id, cid, EdgeType::Causal, 0.6);
        let ghost_id = ghost.id;
        g.edges.insert(ghost_id, ghost.clone());
        g.edge_keys
            .insert((ghost.source, ghost.target, ghost.edge_type), ghost_id);

        assert!(
            !g.incident_edges(cid).iter().any(|e| e.id == ghost_id),
            "incident_edges must be sourced from the adjacency index — a scan of \
             the edge map would surface the un-indexed edge"
        );
        assert_eq!(
            g.incident_edges(cid).len(),
            expected.len(),
            "and the indexed set is unchanged"
        );
    }

    /// CONC-1: routing through the index must not change the id-ascending
    /// contract existing callers rely on, including with a self-loop (which
    /// appears in both the out and in maps and must appear once).
    #[test]
    fn incident_edges_stay_id_ascending_and_deduplicated() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        let hub = concept(1, iid, "hub");
        let hub_id = hub.id;
        g.insert_concept(hub, iid).unwrap();
        // Spokes attached in descending id order so insertion order and id
        // order disagree.
        for n in (2..=6u64).rev() {
            let c = concept(n, iid, &format!("spoke {n}"));
            let cid = c.id;
            g.insert_concept(c, iid).unwrap();
            g.upsert_edge(edge(100 + n, hub_id, cid, EdgeType::Dependency, 0.7))
                .unwrap();
        }
        // A self-loop: present in both adjacency directions.
        g.edges.insert(
            uid(777),
            Edge {
                id: uid(777),
                ..edge(777, hub_id, hub_id, EdgeType::CoOccurrence, 0.6)
            },
        );
        g.edge_keys
            .insert((hub_id, hub_id, EdgeType::CoOccurrence), uid(777));
        g.add_adjacency(hub_id, hub_id, EdgeType::CoOccurrence);

        let ids: Vec<NodeId> = g.incident_edges(hub_id).iter().map(|e| e.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_by_key(|id| id.0);
        assert_eq!(ids, sorted, "id-ascending order is part of the contract");
        assert_eq!(
            ids.iter().filter(|id| **id == uid(777)).count(),
            1,
            "a self-loop must appear exactly once"
        );
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
    fn remove_node_rejects_interactions() {
        let (mut g, iid, _) = small_graph();
        // Interactions are append-only in v0.1 (spec §9: compaction is cut).
        // Removing one would leave a dangling previous_id in the chain — rejected
        // at write time, not detected lazily (adve-review T2.1 S2).
        let err = g.remove_node(iid).unwrap_err().to_string();
        assert!(err.contains("append-only"), "{err}");
        // Nothing changed.
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
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
    fn hierarchical_cycle_is_detected_by_assert_invariants() {
        let (mut g, iid, cid) = small_graph();
        let c2 = concept(2, iid, "auth middleware");
        let c2id = c2.id;
        g.insert_concept(c2, iid).unwrap();

        // A parent-of B parent-of A is semantically nonsensical. upsert_edge stores
        // it; assert_invariants must flag it (adve-review T2.1 M1).
        g.upsert_edge(edge(1, cid, c2id, EdgeType::Hierarchical, 0.7))
            .unwrap();
        g.upsert_edge(edge(2, c2id, cid, EdgeType::Hierarchical, 0.7))
            .unwrap();
        let err = g.assert_invariants().unwrap_err().to_string();
        assert!(err.contains("cycle"), "{err}");
    }

    #[test]
    fn self_loop_structural_edge_is_a_cycle() {
        let (mut g, _, cid) = small_graph();
        // Non-structural self-loop is legal.
        g.upsert_edge(edge(1, cid, cid, EdgeType::CoOccurrence, 0.3))
            .unwrap();
        g.assert_invariants().unwrap();
        // Structural self-loop (A -> A) is a cycle by definition.
        g.upsert_edge(edge(2, cid, cid, EdgeType::Dependency, 0.3))
            .unwrap();
        let err = g.assert_invariants().unwrap_err().to_string();
        assert!(err.contains("cycle"), "{err}");
    }

    #[test]
    fn out_neighbors_dedup_across_edge_types() {
        let (mut g, iid, cid) = small_graph();
        let c2 = concept(2, iid, "auth middleware");
        let c2id = c2.id;
        g.insert_concept(c2, iid).unwrap();
        g.upsert_edge(edge(1, cid, c2id, EdgeType::CoOccurrence, 0.5))
            .unwrap();
        g.upsert_edge(edge(2, cid, c2id, EdgeType::Semantic, 0.5))
            .unwrap();
        // Two edge types to the same target -> one neighbor.
        assert_eq!(g.out_neighbors(cid), vec![c2id]);
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
            last_demotion_time: None,
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
    fn transition_from_status_mismatch_is_rejected() {
        // GRAPH-4: the audit trail must match reality — an event whose
        // from_status does not equal the concept's current status is fabricated
        // and must be rejected before anything is written.
        let (mut g, _, cid) = small_graph(); // concept status: None
        let ev = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid(),
            node_id: cid,
            from_status: crate::types::CanonizationStatus::Venerable,
            to_status: crate::types::CanonizationStatus::Canonical,
            blast_radius: Some(5),
            last_demotion_time: None,
            occurred_at: ts(10),
        };
        let err = g.apply_canonization_transition(ev).unwrap_err().to_string();
        assert!(err.contains("current status"), "{err}");
        // Nothing changed: no status, no blast radius, no audit row, no mutation.
        match g.node(cid).unwrap() {
            Node::Concept(c) => {
                assert_eq!(
                    c.canonization_status,
                    crate::types::CanonizationStatus::None
                );
                assert_eq!(c.blast_radius, None);
            }
            _ => panic!("concept"),
        }
        assert!(g.canonization_events().is_empty());
        assert_eq!(g.log_len(), 3, "only the seed writes");
        g.assert_invariants().unwrap();
    }

    #[test]
    fn illegal_transition_pairs_are_rejected() {
        // GRAPH-4: spec §10 state machine — stage skips, downgrades and
        // self-loops are not edges of the machine and must be rejected at the
        // write gate (the demo's canonization_events table only ever shows
        // legal transitions).
        let (mut g, _, cid) = small_graph();
        // Stage skip: None -> Canonical requires passing through the stages.
        let skip = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid(),
            node_id: cid,
            from_status: crate::types::CanonizationStatus::None,
            to_status: crate::types::CanonizationStatus::Canonical,
            blast_radius: Some(5),
            last_demotion_time: None,
            occurred_at: ts(10),
        };
        let err = g
            .apply_canonization_transition(skip)
            .unwrap_err()
            .to_string();
        assert!(err.contains("illegal"), "{err}");
        // Self-loop: a transition must change status.
        let self_loop = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid(),
            node_id: cid,
            from_status: crate::types::CanonizationStatus::None,
            to_status: crate::types::CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            occurred_at: ts(11),
        };
        assert!(g.apply_canonization_transition(self_loop).is_err());
        // Downgrade: demotion is only Canonical -> None.
        let promote = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid(),
            node_id: cid,
            from_status: crate::types::CanonizationStatus::None,
            to_status: crate::types::CanonizationStatus::Venerable,
            blast_radius: None,
            last_demotion_time: None,
            occurred_at: ts(12),
        };
        g.apply_canonization_transition(promote).unwrap();
        let downgrade = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid(),
            node_id: cid,
            from_status: crate::types::CanonizationStatus::Venerable,
            to_status: crate::types::CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: Some(ts(13)),
            occurred_at: ts(13),
        };
        let err = g
            .apply_canonization_transition(downgrade)
            .unwrap_err()
            .to_string();
        assert!(err.contains("illegal"), "{err}");
        // Only the single legal promotion was recorded.
        assert_eq!(g.canonization_events().len(), 1);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn legal_transitions_apply_and_demotion_carries_last_demotion_time() {
        // GRAPH-4 + COH-3: walk the full §10 path None -> Candidate ->
        // Venerable -> Canonical -> None. Demotion nulls blast_radius and
        // stamps last_demotion_time (spec §10); non-demotion events leave
        // last_demotion_time untouched; the carry survives a snapshot round-trip.
        let (mut g, _, cid) = small_graph();
        let promote = |from, to, blast, last_demotion, at| CanonizationEvent {
            id: NodeId::new(),
            session_id: sid(),
            node_id: cid,
            from_status: from,
            to_status: to,
            blast_radius: blast,
            last_demotion_time: last_demotion,
            occurred_at: ts(at),
        };
        g.apply_canonization_transition(promote(
            crate::types::CanonizationStatus::None,
            crate::types::CanonizationStatus::Candidate,
            Some(3),
            None,
            1,
        ))
        .unwrap();
        g.apply_canonization_transition(promote(
            crate::types::CanonizationStatus::Candidate,
            crate::types::CanonizationStatus::Venerable,
            Some(3),
            None,
            2,
        ))
        .unwrap();
        g.apply_canonization_transition(promote(
            crate::types::CanonizationStatus::Venerable,
            crate::types::CanonizationStatus::Canonical,
            Some(7),
            None,
            3,
        ))
        .unwrap();
        // Demotion: blast_radius nulled, last_demotion_time stamped.
        g.apply_canonization_transition(promote(
            crate::types::CanonizationStatus::Canonical,
            crate::types::CanonizationStatus::None,
            None,
            Some(ts(4)),
            4,
        ))
        .unwrap();
        match g.node(cid).unwrap() {
            Node::Concept(c) => {
                assert_eq!(
                    c.canonization_status,
                    crate::types::CanonizationStatus::None
                );
                assert_eq!(c.blast_radius, None);
                assert_eq!(c.last_demotion_time, Some(ts(4)));
            }
            _ => panic!("concept"),
        }
        // A later non-demotion promotion must NOT clobber the carry.
        g.apply_canonization_transition(promote(
            crate::types::CanonizationStatus::None,
            crate::types::CanonizationStatus::Candidate,
            Some(2),
            None,
            5,
        ))
        .unwrap();
        match g.node(cid).unwrap() {
            Node::Concept(c) => assert_eq!(c.last_demotion_time, Some(ts(4))),
            _ => panic!("concept"),
        }
        // The carry survives a snapshot round-trip (from_snapshot -> to_snapshot).
        let h = Graph::from_snapshot(g.snapshot()).unwrap();
        match h.node(cid).unwrap() {
            Node::Concept(c) => assert_eq!(c.last_demotion_time, Some(ts(4))),
            _ => panic!("concept"),
        }
        let demote_ev = h
            .canonization_events()
            .iter()
            .find(|e| e.to_status == crate::types::CanonizationStatus::None)
            .expect("demotion event recorded");
        assert_eq!(demote_ev.last_demotion_time, Some(ts(4)));
        assert_eq!(h.canonization_events().len(), 5);
        g.assert_invariants().unwrap();
    }

    // ------------------------------------------------------------------
    // set_root_goal — spec §9: root goal nodes are automatically Venerable
    // ------------------------------------------------------------------

    /// Fresh graph with a goal concept ("launch the product") + an unrelated
    /// concept, both derived from one interaction.
    fn goal_graph() -> (Graph, NodeId, NodeId) {
        let mut g = Graph::new(sid());
        let i = interaction(1, None, 0);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        let goal = concept(1, iid, "launch the product");
        let goal_id = goal.id;
        g.insert_concept(goal, iid).unwrap();
        let other = concept(2, iid, "unrelated concept");
        let other_id = other.id;
        g.insert_concept(other, iid).unwrap();
        (g, goal_id, other_id)
    }

    fn status_of(g: &Graph, id: NodeId) -> crate::types::CanonizationStatus {
        match g.node(id).unwrap() {
            Node::Concept(c) => c.canonization_status,
            _ => panic!("concept"),
        }
    }

    #[test]
    fn set_root_goal_promotes_matching_concept_to_venerable() {
        let (mut g, goal_id, other_id) = goal_graph();

        g.set_root_goal(Some(serde_json::json!("launch the product")));

        assert_eq!(
            g.root_goal(),
            Some(&serde_json::json!("launch the product"))
        );
        assert_eq!(
            status_of(&g, goal_id),
            crate::types::CanonizationStatus::Venerable,
            "goal concept auto-promoted to Venerable"
        );
        assert_eq!(
            status_of(&g, other_id),
            crate::types::CanonizationStatus::None,
            "non-goal concept untouched"
        );
        // Audited through the T2.1 mutation path: one transition event and one
        // `Mutation::CanonizationTransition` in the write-behind log.
        assert_eq!(g.canonization_events().len(), 1);
        let ev = &g.canonization_events()[0];
        assert_eq!(ev.node_id, goal_id);
        assert_eq!(ev.from_status, crate::types::CanonizationStatus::None);
        assert_eq!(ev.to_status, crate::types::CanonizationStatus::Venerable);
        let batch = g.drain_log();
        assert_eq!(
            batch
                .mutations
                .iter()
                .filter(|m| matches!(m, Mutation::CanonizationTransition { .. }))
                .count(),
            1
        );
    }

    /// ALGO-6: spec §6.1's own `root_goal` example is a **list**. A string-only
    /// reading stored the array but named no concept, silently disabling drift
    /// detection, auto-`Venerable` promotion and GC's root-goal exclusion.
    ///
    /// ALGO-12: **every** match is promoted, id-ascending, so the outcome does
    /// not depend on `HashMap` iteration order.
    #[test]
    fn set_root_goal_accepts_an_array_and_promotes_every_match() {
        let (mut g, goal_id, other_id) = goal_graph();
        g.set_root_goal(Some(serde_json::json!([
            "launch the product",
            "unrelated concept"
        ])));

        assert_eq!(
            g.root_goal_texts(),
            vec![
                "launch the product".to_string(),
                "unrelated concept".to_string()
            ],
            "both names are read out of the array, sorted"
        );
        for id in [goal_id, other_id] {
            assert_eq!(
                status_of(&g, id),
                crate::types::CanonizationStatus::Venerable,
                "every named concept is auto-promoted, not just the first match"
            );
        }
        // Audited id-ascending, so the event order is deterministic under
        // multiple matches.
        let events = g.canonization_events();
        assert_eq!(events.len(), 2);
        assert!(events[0].node_id.0 < events[1].node_id.0);

        // ALGO-12: `occurred_at` is logical time — the session's newest
        // interaction stamp — not `Utc::now()`, so the audit trail stays
        // monotonic against the rows around it.
        let logical = g.logical_now();
        assert_eq!(logical, ts(0));
        assert!(
            events.iter().all(|e| e.occurred_at == logical),
            "occurred_at must come from logical time: {events:?}"
        );
    }

    /// ALGO-6: an object goal keeps working, and an unrecognised shape is
    /// stored but names nothing (spec §6.1 types `root_goal` as free-form JSON).
    #[test]
    fn root_goal_texts_reads_every_supported_shape() {
        assert!(root_goal_texts(None).is_empty());
        assert_eq!(
            root_goal_texts(Some(&serde_json::json!("one"))),
            vec!["one".to_string()]
        );
        assert_eq!(
            root_goal_texts(Some(&serde_json::json!(["b", "a", "a"]))),
            vec!["a".to_string(), "b".to_string()],
            "sorted and deduplicated for determinism"
        );
        assert_eq!(
            root_goal_texts(Some(&serde_json::json!({"content": "c", "key": "k"}))),
            vec!["c".to_string(), "k".to_string()]
        );
        assert_eq!(
            root_goal_texts(Some(&serde_json::json!(["ok", 7, null]))),
            vec!["ok".to_string()],
            "non-string elements are ignored, not fatal"
        );
        assert!(root_goal_texts(Some(&serde_json::json!(42))).is_empty());
    }

    /// XP-8: the root goal is durable. Before `Mutation::SetRootGoal` a reload
    /// replayed an empty goal, so drift detection silently stopped and GC's
    /// root-goal exclusion emptied. The mutation also bumps the epoch, so T5.4's
    /// recall cache cannot serve results computed against the old goal.
    #[test]
    fn set_root_goal_emits_a_mutation_and_bumps_the_epoch() {
        let (mut g, _, _) = goal_graph();
        g.drain_log();
        let epoch_before = g.epoch();

        g.set_root_goal(Some(serde_json::json!("launch the product")));
        assert!(g.epoch() > epoch_before, "a goal change bumps the epoch");
        let batch = g.drain_log();
        let goals: Vec<&Mutation> = batch
            .mutations
            .iter()
            .filter(|m| matches!(m, Mutation::SetRootGoal { .. }))
            .collect();
        assert_eq!(goals.len(), 1, "one SetRootGoal: {:?}", batch.mutations);
        match goals[0] {
            Mutation::SetRootGoal { session_id, goal } => {
                assert_eq!(*session_id, sid());
                assert_eq!(
                    goal.as_ref(),
                    Some(&serde_json::json!("launch the product"))
                );
            }
            other => panic!("unexpected {other:?}"),
        }

        // Re-setting the same goal is a no-op: no mutation, no epoch bump.
        let epoch_after = g.epoch();
        g.set_root_goal(Some(serde_json::json!("launch the product")));
        assert_eq!(g.epoch(), epoch_after, "an unchanged goal writes nothing");
        assert!(g.drain_log().is_empty());

        // Clearing emits the clear so a reload does not resurrect the old goal.
        g.set_root_goal(None);
        let batch = g.drain_log();
        assert!(batch
            .mutations
            .iter()
            .any(|m| matches!(m, Mutation::SetRootGoal { goal: None, .. })));
    }

    #[test]
    fn set_root_goal_is_idempotent_for_venerable_goal() {
        let (mut g, goal_id, _) = goal_graph();
        g.set_root_goal(Some(serde_json::json!("launch the product")));
        g.set_root_goal(Some(serde_json::json!("launch the product")));

        assert_eq!(
            status_of(&g, goal_id),
            crate::types::CanonizationStatus::Venerable
        );
        // The §10 state machine has no Venerable -> Venerable edge: the second
        // call must not attempt a self-loop transition.
        assert_eq!(g.canonization_events().len(), 1);
    }

    #[test]
    fn set_embedding_is_ordered_durable_metadata() {
        let mut g = Graph::new(sid());
        let embedding = crate::types::EmbeddingContract {
            kind: "fixture".into(),
            model: Some("v1".into()),
            dim: 1024,
        };
        let epoch = g.epoch();
        g.stamp_embedding(embedding.clone()).unwrap();
        assert!(g.epoch() > epoch);
        let batch = g.drain_log();
        assert_eq!(
            batch.mutations,
            vec![Mutation::SetEmbedding {
                session_id: sid(),
                embedding: Some(embedding.clone()),
            }]
        );

        let epoch = g.epoch();
        g.stamp_embedding(embedding).unwrap();
        assert_eq!(g.epoch(), epoch, "an identical contract is a no-op");
        assert!(g.drain_log().is_empty());
    }

    #[test]
    fn embedding_contract_cannot_change_while_vectors_remain() {
        let (mut g, interaction, _) = small_graph();
        let fixture = crate::types::EmbeddingContract {
            kind: "fixture".into(),
            model: Some("v1".into()),
            dim: 1024,
        };
        g.stamp_embedding(fixture.clone()).unwrap();
        let mut vector_concept = concept(99, interaction, "vector-bearing");
        vector_concept.embedding = Some(vec![0.0; 1024]);
        g.insert_concept(vector_concept, interaction).unwrap();
        g.drain_log();
        let before = g.snapshot();
        let mut corrupt_reload = before.clone();
        corrupt_reload.embedding = None;
        assert!(
            Graph::from_snapshot(corrupt_reload).is_err(),
            "load rejects vectors whose contract was lost"
        );

        let other = crate::types::EmbeddingContract {
            kind: "bedrock".into(),
            model: Some("titan-v2".into()),
            dim: 1024,
        };
        assert!(g.stamp_embedding(other.clone()).is_err());
        assert!(g.replace_embedding_without_vectors(Some(other)).is_err());
        assert!(g.replace_embedding_without_vectors(None).is_err());
        assert_eq!(g.snapshot(), before);
        assert!(g.drain_log().is_empty());
    }

    #[test]
    fn set_root_goal_leaves_canonical_goal_untouched() {
        let (mut g, goal_id, _) = goal_graph();
        g.set_root_goal(Some(serde_json::json!("launch the product")));
        // Earn Canonical via the mutation path (Venerable -> Canonical).
        let promote = |to, at| CanonizationEvent {
            id: NodeId::new(),
            session_id: sid(),
            node_id: goal_id,
            from_status: crate::types::CanonizationStatus::Venerable,
            to_status: to,
            blast_radius: Some(3),
            last_demotion_time: None,
            occurred_at: ts(at),
        };
        g.apply_canonization_transition(promote(crate::types::CanonizationStatus::Canonical, 1))
            .unwrap();
        let events_before = g.canonization_events().len();

        g.set_root_goal(Some(serde_json::json!("launch the product")));

        assert_eq!(
            status_of(&g, goal_id),
            crate::types::CanonizationStatus::Canonical,
            "a Canonical root goal must not be downgraded (no Canonical -> Venerable edge)"
        );
        assert_eq!(g.canonization_events().len(), events_before);
    }

    #[test]
    fn clearing_root_goal_never_demotes() {
        let (mut g, goal_id, _) = goal_graph();
        g.set_root_goal(Some(serde_json::json!("launch the product")));

        g.set_root_goal(None);

        assert_eq!(g.root_goal(), None);
        assert_eq!(
            status_of(&g, goal_id),
            crate::types::CanonizationStatus::Venerable,
            "clearing the goal stores the clear but never demotes (demotion is T6.4's)"
        );
        assert_eq!(g.canonization_events().len(), 1);
    }

    #[test]
    fn set_root_goal_matches_canonical_key_when_content_differs() {
        let mut g = Graph::new(sid());
        let i = interaction(1, None, 0);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        let mut c = concept(1, iid, "the product launch");
        c.canonical_key = "launch product".into();
        let goal_id = c.id;
        g.insert_concept(c, iid).unwrap();

        g.set_root_goal(Some(serde_json::json!("launch product")));

        assert_eq!(
            status_of(&g, goal_id),
            crate::types::CanonizationStatus::Venerable,
            "goal matching by canonical_key promotes"
        );
    }

    #[test]
    fn set_root_goal_promotes_candidate_goal_to_venerable() {
        let (mut g, goal_id, _) = goal_graph();
        let promote = |from, to, at| CanonizationEvent {
            id: NodeId::new(),
            session_id: sid(),
            node_id: goal_id,
            from_status: from,
            to_status: to,
            blast_radius: Some(2),
            last_demotion_time: None,
            occurred_at: ts(at),
        };
        g.apply_canonization_transition(promote(
            crate::types::CanonizationStatus::None,
            crate::types::CanonizationStatus::Candidate,
            1,
        ))
        .unwrap();

        g.set_root_goal(Some(serde_json::json!("launch the product")));

        assert_eq!(
            status_of(&g, goal_id),
            crate::types::CanonizationStatus::Venerable,
            "Candidate -> Venerable is a legal §10 edge (Stage 2 promotion)"
        );
        assert_eq!(g.canonization_events().len(), 2);
    }

    #[test]
    fn structured_root_goal_is_stored_without_promotion() {
        let (mut g, goal_id, _) = goal_graph();
        let structured = serde_json::json!({ "goal": "launch the product" });

        g.set_root_goal(Some(structured.clone()));

        assert_eq!(g.root_goal(), Some(&structured), "structured goal stored");
        assert_eq!(
            status_of(&g, goal_id),
            crate::types::CanonizationStatus::None,
            "non-string goal names no concept -> no promotion"
        );
        assert!(g.canonization_events().is_empty());
    }

    #[test]
    fn synonyms_declare_lookup_and_snapshot() {
        let mut g = Graph::new(sid());
        let epoch = g.epoch();
        g.declare_synonym("register_user", "create_user");
        assert_eq!(g.epoch(), epoch + 1);
        let unchanged = g.epoch();
        g.declare_synonym("register_user", "create_user");
        assert_eq!(g.epoch(), unchanged, "identical synonym is a no-op");
        g.declare_synonym("delete_user", "remove_user");
        assert_eq!(g.synonym("register_user"), Some("create_user"));
        assert_eq!(g.synonym("delete_user"), Some("remove_user"));
        assert_eq!(g.synonym("unknown"), None);
        // Replace wins.
        let before_replace = g.epoch();
        g.declare_synonym("register_user", "signup_user");
        assert_eq!(g.epoch(), before_replace + 1);
        assert_eq!(g.synonym("register_user"), Some("signup_user"));

        let snap = g.snapshot();
        let keys: Vec<&str> = snap
            .synonyms
            .iter()
            .map(|s| s.source_key.as_str())
            .collect();
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
        // Seed the edge's target concept *before* the drain so the post-drain
        // section contains exactly the one edge write. The edge must be
        // type-legal (GRAPH-2 rejects type-invalid endpoints at the write gate —
        // a `Semantic` edge from an interaction was never legal per spec §5).
        let c2 = concept(2, iid, "auth middleware");
        let c2id = c2.id;
        g.insert_concept(c2, iid).unwrap();
        // drain does not reset; anchor on the post-drain epoch so the next
        // write is isolated from insert_concept's own bumps.
        let e_before = g.epoch();
        let _ = g.drain_log();
        assert_eq!(g.epoch(), e_before);
        // Next write bumps again.
        g.upsert_edge(edge(1, cid, c2id, EdgeType::Semantic, 0.5))
            .unwrap();
        assert!(g.epoch() > e_before);
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
                // Neither carries graph topology, so neither participates in
                // the endpoint-ordering contract.
                Mutation::CanonizationTransition { .. }
                | Mutation::SetRootGoal { .. }
                | Mutation::SetEmbedding { .. } => {}
            }
        }
        assert!(saw_delete);
    }

    #[test]
    fn mutation_log_is_chronological_across_interleaved_writes() {
        // Adve-review T2.1 M2: §2.4's phase grouping holds *within* a logical
        // write, not across the batch. A node upsert may legally follow a
        // DeleteNode in the same drained batch (create -> delete -> create within
        // one flush interval); adapters replay in order and never re-sort.
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let i1_id = i1.id;
        g.insert_interaction(i1).unwrap();
        let c1 = concept(1, i1_id, "first");
        let c1_id = c1.id;
        g.insert_concept(c1, i1_id).unwrap(); // UpsertNode(c1), UpsertEdge(derives)
        g.remove_node(c1_id).unwrap(); // DeleteEdge(derives), DeleteNode(c1)
        let c2 = concept(2, i1_id, "second");
        g.insert_concept(c2, i1_id).unwrap(); // UpsertNode(c2), UpsertEdge(derives2)

        let batch = g.drain_log();
        let kinds: Vec<&str> = batch
            .mutations
            .iter()
            .map(|m| match m {
                Mutation::UpsertNode { .. } => "upsert_node",
                Mutation::UpsertEdge { .. } => "upsert_edge",
                Mutation::DeleteNode { .. } => "delete_node",
                Mutation::DeleteEdge { .. } => "delete_edge",
                Mutation::CanonizationTransition { .. } => "transition",
                Mutation::SetRootGoal { .. } => "set_root_goal",
                Mutation::SetEmbedding { .. } => "set_embedding",
            })
            .collect();
        let expected = [
            "upsert_node", // i1
            "upsert_node", // c1
            "upsert_edge", // derives c1
            "delete_edge", // derives c1
            "delete_node", // c1
            "upsert_node", // c2 — legally AFTER a DeleteNode
            "upsert_edge", // derives c2
        ];
        assert_eq!(kinds, expected);

        // Chronological replay is always safe: every edge references a node
        // upserted earlier in the same batch.
        let mut nodes: HashSet<NodeId> = HashSet::new();
        for m in &batch.mutations {
            match m {
                Mutation::UpsertNode { node } => {
                    nodes.insert(node.id());
                }
                Mutation::UpsertEdge { edge } => {
                    assert!(nodes.contains(&edge.source), "{m:?}");
                    assert!(nodes.contains(&edge.target), "{m:?}");
                }
                _ => {}
            }
        }
    }

    #[cfg(feature = "fixtures")]
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
    fn snapshot_roundtrip_preserves_structure() {
        // Adve-review T2.1 S5: snapshot equality is necessary but not sufficient —
        // the adjacency index and natural-key map must survive a round-trip too.
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let i1_id = i1.id;
        let i2 = interaction(2, Some(i1_id), 5);
        let i2_id = i2.id;
        g.insert_interaction(i1).unwrap();
        g.insert_interaction(i2).unwrap();
        let c = concept(1, i2_id, "user schema");
        let cid = c.id;
        g.insert_concept(c, i2_id).unwrap();
        let c2 = concept(2, i2_id, "auth middleware");
        let c2id = c2.id;
        g.insert_concept(c2, i2_id).unwrap();
        g.upsert_edge(edge(1, cid, c2id, EdgeType::CoOccurrence, 0.5))
            .unwrap();
        g.upsert_edge(edge(2, cid, c2id, EdgeType::Dependency, 0.7))
            .unwrap();

        let h = Graph::from_snapshot(g.snapshot()).unwrap();

        // Structural queries agree across the round-trip.
        assert_eq!(
            h.edge_between(cid, c2id, EdgeType::CoOccurrence),
            g.edge_between(cid, c2id, EdgeType::CoOccurrence)
        );
        assert_eq!(
            h.edge_between(cid, c2id, EdgeType::Dependency),
            g.edge_between(cid, c2id, EdgeType::Dependency)
        );
        assert_eq!(
            h.out_neighbors_typed(cid, EdgeType::CoOccurrence),
            g.out_neighbors_typed(cid, EdgeType::CoOccurrence)
        );
        assert_eq!(
            h.in_neighbors_typed(c2id, EdgeType::Dependency),
            g.in_neighbors_typed(c2id, EdgeType::Dependency)
        );
        assert_eq!(h.out_neighbors(cid), g.out_neighbors(cid));
        assert_eq!(h.in_neighbors(c2id), g.in_neighbors(c2id));
        assert_eq!(h.temporal_chain(), g.temporal_chain());
        assert_eq!(h.node_count(), g.node_count());
        assert_eq!(h.edge_count(), g.edge_count());
        h.assert_invariants().unwrap();
    }

    #[test]
    fn empty_snapshot_roundtrips() {
        // GRAPH-6: a zero-interaction snapshot is a valid empty graph, not a
        // malformed chain ("expected exactly one chain head, found 0").
        let g = Graph::new(sid());
        let snap = g.snapshot();
        let h = Graph::from_snapshot(snap.clone()).unwrap();
        assert!(h.is_empty());
        assert_eq!(h.snapshot(), snap);
        h.assert_invariants().unwrap();
    }

    #[test]
    fn from_snapshot_rejects_duplicate_natural_key_edges() {
        // GRAPH-7: two edges with the same (source, target, edge_type) in one
        // snapshot must be rejected — record_edge would silently merge them via
        // reinforcement, leaving a loaded graph that disagrees with the stored
        // snapshot.
        let (mut g, iid, cid) = small_graph();
        let c2 = concept(2, iid, "auth middleware");
        let c2id = c2.id;
        g.insert_concept(c2, iid).unwrap();
        g.upsert_edge(edge(1, cid, c2id, EdgeType::CoOccurrence, 0.5))
            .unwrap();
        let mut snap = g.snapshot();
        // A second edge with the same natural key (fresh id) — must be rejected.
        let mut dup = edge(2, cid, c2id, EdgeType::CoOccurrence, 0.5);
        dup.id = NodeId::new();
        snap.edges.push(dup);
        let err = Graph::from_snapshot(snap).unwrap_err().to_string();
        assert!(err.contains("duplicate natural-key edge"), "{err}");
    }

    #[test]
    fn deep_chain_cycle_check_does_not_overflow_stack() {
        // GRAPH-3 regression: dfs_cycle was recursive — a ~10k-deep
        // Causal/Dependency chain overflowed the ~2 MiB worker-thread stack that
        // load_session materializes on (SIGABRT -> session permanently
        // unloadable). The check must be iterative. Exercise the full load path
        // (from_snapshot -> assert_invariants) on a small-stack thread, exactly
        // like load.rs does.
        const N: usize = 20_000;
        let i = interaction(1, None, 0);
        let iid = i.id;
        let mut snap = GraphSnapshot {
            session_id: sid(),
            root_goal: None,
            created_at: None,
            closed_at: None,
            interactions: vec![i],
            concepts: (0..N)
                .map(|k| {
                    let mut c = concept(k as u64 + 1, iid, "chain");
                    c.content = format!("chain {k}");
                    c.canonical_key = format!("chain{k}");
                    c
                })
                .collect(),
            edges: Vec::with_capacity(2 * N),
            synonyms: vec![],
            reservations: vec![],
            canonization_events: vec![],
            embedding: None,
        };
        // Every concept derives from the interaction (assert_invariants
        // requires it) plus a single Causal chain c0 -> c1 -> ... -> c(N-1).
        for k in 0..N {
            snap.edges.push(edge(
                k as u64 + 1,
                iid,
                NodeId(Uuid::from_u64_pair(2, k as u64 + 1)),
                EdgeType::Derives,
                0.9,
            ));
        }
        for k in 0..N - 1 {
            snap.edges.push(edge(
                N as u64 + k as u64 + 1,
                NodeId(Uuid::from_u64_pair(2, k as u64 + 1)),
                NodeId(Uuid::from_u64_pair(2, k as u64 + 2)),
                EdgeType::Causal,
                0.5,
            ));
        }
        let handle = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || Graph::from_snapshot(snap))
            .expect("spawn small-stack thread");
        let g = handle.join().expect("no panic on the loader thread");
        g.expect("load + invariants pass")
            .assert_invariants()
            .unwrap();
    }

    #[cfg(feature = "fixtures")]
    #[test]
    fn fixture_drift_loads_and_passes_invariants() {
        let snap = crate::fixtures::load_snapshot("session-drift").unwrap();
        let g = Graph::from_snapshot(snap).unwrap();
        g.assert_invariants().unwrap();
        assert_eq!(g.temporal_chain().len(), 2);
        assert_eq!(g.edge_count(), 17);
        assert_eq!(g.node_count(), 9 + 2);
    }

    #[cfg(feature = "fixtures")]
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

    #[test]
    fn bump_gc_survived_increments_and_emits_upserts() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        let c1 = concept(1, iid, "survivor one");
        let c1id = c1.id;
        let c2 = concept(2, iid, "survivor two");
        let c2id = c2.id;
        g.insert_concept(c1, iid).unwrap();
        g.insert_concept(c2, iid).unwrap();
        // Discard the insert mutations so the count below isolates the bumps.
        g.drain_log();
        let epoch_before = g.epoch();
        let bumped = g.bump_gc_survived(&[c1id, c2id]);

        assert_eq!(bumped, 2);
        let c1 = match g.node(c1id).unwrap() {
            Node::Concept(c) => c,
            _ => unreachable!(),
        };
        assert_eq!(c1.gc_survived, 1);
        // Every bump emits an UpsertNode so the durable store mirrors it.
        assert!(g.epoch() > epoch_before);
        let batch = g.drain_log();
        let upserts = batch
            .mutations
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    Mutation::UpsertNode {
                        node: Node::Concept(_)
                    }
                )
            })
            .count();
        assert_eq!(upserts, 2);
        // Missing ids are skipped, not fatal.
        assert_eq!(g.bump_gc_survived(&[NodeId::nil()]), 0);
    }

    // Adve-review T2.1 I4: the owner (T2.3+ `Memory`) wraps Graph in
    // `Arc<RwLock<Graph>>` (spec §6.4). Compile-time proof it can.
    const _: () = {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        let _ = [assert_send::<Graph>, assert_sync::<Graph>];
    };
}

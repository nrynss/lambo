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
    CanonizationEvent, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, GcMark,
    GraphSnapshot, Interaction, LamboError, Mutation, MutationBatch, Node, NodeId, Reservation,
    SessionId, StoreError, Synonym, WriteIntent, WriteIntentOutcome,
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
    ///
    /// The mutation epoch **resumes** from [`GraphSnapshot::mutation_epoch`]
    /// (issue #17): the counter is durable accounting, not process state.
    /// Restarting it at 0 on every writer start is what kept GC's
    /// `gc_interval` sweep — and through `gc_survived` every Swarm Stage 1
    /// promotion — permanently out of reach in a low-write deployment. The
    /// log stays empty either way: loading counts nothing new.
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
        // Issue #17: resume the durable mutation accounting AFTER the
        // invariant pass so `epoch` is the last thing a caller could observe
        // mid-construction. `Graph::new` started it at 0; the snapshot's value
        // is the deployment's count through its last durable mutation.
        g.epoch = snap.mutation_epoch;
        // Issue #29: GC's sweep accounting resumes with it, so a restart does
        // not measure the next sweep from 0 (which swept once per restart and
        // bumped every `gc_survived` once the lifetime count passed the
        // interval).
        // The reset flag is writer-side only (stored marks are always
        // `false`): a snapshot that somehow carries it must not make this
        // fresh writer replace the stored time on its first flush.
        g.gc_mark = GcMark {
            last_gc_at_reset: false,
            ..snap.gc_mark
        };
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
            // Write intents are not graph state: they pass through this
            // graph's mutation log (`record_write_intent` /
            // `consume_write_intent`) into the store, and only
            // `load_session` materializes them. A RAM snapshot therefore has
            // none to offer — the store is their single home.
            write_intents: Vec::new(),
            // The mutation accounting is graph state (issue #17): a seeded or
            // otherwise re-materialized session must resume the epoch, not
            // restart it, or GC's `gc_interval` would measure per-process
            // mutations again.
            mutation_epoch: self.epoch,
            // A snapshot is a stored view: the writer-side re-anchor flag
            // never leaves the graph this way (only `drain_log` carries it).
            gc_mark: GcMark {
                last_gc_at_reset: false,
                ..self.gc_mark
            },
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

    /// Apply a canonization transition to a concept and record it. Concept must
    /// exist; status and blast radius are set from the event. The event is
    /// appended to the session's audit trail and emitted as a mutation.
    ///
    /// Write-gate validation (adve-review GRAPH-4): `from_status` must equal the
    /// concept's **current** status — a fabricated audit row is rejected with a
    /// typed invariant error — and the pair must be an edge of the spec §10
    /// state machine (`legal_canonization_transition`; stage skips, downgrades
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

    /// Record one explicit human confirmation of a concept (C2, spec §3.2's
    /// "Human Confirmed" term).
    ///
    /// The counter is **saturating** (`i32` is the schema column type) and the
    /// bump is emitted as an `UpsertNode` mutation so the durable store mirrors
    /// it, exactly like [`Self::bump_gc_survived`]. Returns the new count; a
    /// missing id or a non-concept node is [`StoreError::NotFound`] — there is
    /// no silent skip here, because a confirmation the graph cannot apply must
    /// fail loudly rather than be lost.
    ///
    /// No agent write path reaches this method: it is called only through
    /// [`crate::Memory::confirm_human`], the human-in-the-loop surface the
    /// solo score's second term reads.
    pub fn confirm_human(&mut self, node: NodeId) -> Result<i32, LamboError> {
        let Some(Node::Concept(c)) = self.nodes.get_mut(&node) else {
            return Err(LamboError::Store(StoreError::NotFound(format!(
                "confirm_human: no concept {node}"
            ))));
        };
        c.human_confirmed = c.human_confirmed.saturating_add(1);
        let confirmed = c.human_confirmed;
        let updated = c.clone();
        self.append_mutation(Mutation::UpsertNode {
            node: Node::Concept(updated),
        });
        Ok(confirmed)
    }

    /// Apply coalesced read accesses (issue #30): for each `(id, count, at)`,
    /// `access_count += count` (saturating, `i32` is the column type) and
    /// `last_accessed = max(last_accessed, at)`. Returns how many concepts were
    /// updated.
    ///
    /// The update is in RAM at once (GC and scoring see it); durability is
    /// **deferred**: the concept is marked access-dirty, and the write-behind
    /// flush turns the dirty set into one [`Mutation::RecordAccess`] per
    /// concept, with its absolute values at that moment, via
    /// [`Self::drain_accesses`] — a narrow, monotonic update of the two access
    /// columns (no full-row rewrite, no embedding, no vector-index touch).
    /// Nothing is appended to the mutation log here, so reads can never grow
    /// the log: during a store outage the flush stops draining accesses while
    /// it holds unflushed ones, and the dirty set stays bounded by the concept
    /// count. **The epoch is not bumped** either.
    /// An access is bookkeeping about a read, not a change to what the graph
    /// says: no node, edge, key or status that recall, canonicalization or a
    /// hybrid plan reads changes. Bumping would (a) count reads toward GC's
    /// `gc_interval` trigger, so a read-heavy session would sweep on reads
    /// alone, (b) invalidate every recall-cache entry on every recall, turning
    /// the cache off for the read-mostly case it exists for, and (c) force a
    /// concurrent hybrid commit to replan against a graph nothing changed. The
    /// same reasoning already keeps [`Self::record_write_intent`] off the epoch.
    ///
    /// The durable mutation watermark is unaffected: accesses are never
    /// counted, and the adapters persist `MAX` of whatever stamp a flush
    /// carries.
    ///
    /// Ids that are missing, or that are not concepts (an interaction hit), are
    /// skipped: a concept collected or retracted between the read and this
    /// apply has nothing left to count against. `count == 0` is a no-op for
    /// that id.
    pub fn record_accesses(
        &mut self,
        accesses: &[(NodeId, u32, chrono::DateTime<chrono::Utc>)],
    ) -> usize {
        let mut ordered: Vec<&(NodeId, u32, chrono::DateTime<chrono::Utc>)> =
            accesses.iter().filter(|(_, n, _)| *n > 0).collect();
        ordered.sort_by_key(|(id, _, _)| id.0);
        let mut updated = 0;
        for &(id, count, at) in ordered {
            let Some(Node::Concept(c)) = self.nodes.get_mut(&id) else {
                continue;
            };
            let delta = i32::try_from(count).unwrap_or(i32::MAX);
            c.access_count = c.access_count.saturating_add(delta);
            c.last_accessed = Some(c.last_accessed.map_or(at, |prev| prev.max(at)));
            // Deliberately NOT the log (nor `append_mutation`): see the doc
            // comment. The flush asks for it through `drain_accesses`.
            self.access_dirty.insert(id);
            updated += 1;
        }
        updated
    }

    /// Hand at most `limit` access-dirty concepts to the flush as
    /// [`Mutation::RecordAccess`] (issue #30), each with its **current**
    /// absolute values, in id order (deterministic), and clear them from the
    /// dirty set. Ids that are no longer concepts are dropped from the set
    /// without a mutation. The rest stay dirty for a later call.
    ///
    /// Not part of [`Self::drain_log`] on purpose: the log is replayed in
    /// order and grows with every drain the flush retains, while access
    /// bookkeeping is a per-concept *state* that only needs its latest value
    /// to be durable. The flush loop decides when to take it (never while it
    /// already holds undelivered accesses — see `store::flush`), and
    /// `Memory::close` takes all of it after its final `drain_log`. Values only
    /// ever rise, so a `RecordAccess` taken after a concept upsert in the log
    /// carries values at least as high — the ordering `store::batch` relies on.
    pub fn drain_accesses(&mut self, limit: usize) -> Vec<Mutation> {
        if limit == 0 || self.access_dirty.is_empty() {
            return Vec::new();
        }
        let mut ids: Vec<NodeId> = self.access_dirty.iter().copied().collect();
        ids.sort_unstable_by_key(|id| id.0);
        let mut out = Vec::with_capacity(limit.min(ids.len()));
        for id in ids {
            if out.len() == limit {
                break;
            }
            self.access_dirty.remove(&id);
            if let Some(Node::Concept(c)) = self.nodes.get(&id) {
                if let Some(last_accessed) = c.last_accessed {
                    out.push(Mutation::RecordAccess {
                        session_id: c.session_id.clone(),
                        id,
                        access_count: c.access_count,
                        last_accessed,
                    });
                }
            }
        }
        out
    }

    /// Concepts whose applied accesses have not yet been handed to the flush
    /// ([`Self::drain_accesses`]). At most the session's concept count.
    pub fn pending_accesses(&self) -> usize {
        self.access_dirty.len()
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

    /// Replace a same-width contract after an operator explicitly declares
    /// the stored vectors compatible with a renamed model identifier, or
    /// after a migration has already removed every old vector.
    ///
    /// This is crate-private because ordinary graph callers must never relabel
    /// an existing vector space. The only production caller is the
    /// `--allow-embedding-mismatch` writer attach path, which checks equal
    /// dimensions, permits vectors only for a same-kind identifier rename,
    /// emits a warning, and records the replacement durably. A cross-kind
    /// migration must clear/rewrite its vectors before this point.
    pub(crate) fn replace_embedding_with_operator_override(
        &mut self,
        contract: crate::types::EmbeddingContract,
    ) -> Result<(), LamboError> {
        if let Some(existing) = &self.embedding {
            if existing.dim != contract.dim {
                return Err(invariant(format!(
                    "cannot override embedding contract width {} with width {}; \
                     --allow-embedding-mismatch is only for same-width migrations",
                    existing.dim, contract.dim
                )));
            }
            if self.concepts().any(|concept| concept.embedding.is_some())
                && existing.kind != contract.kind
            {
                return Err(invariant(format!(
                    "cannot relabel {} vectors as {} while stored concept vectors remain; \
                     atomically clear/re-embed the vectors before changing embedder kind",
                    existing.kind, contract.kind
                )));
            }
        }
        if self.embedding.as_ref() != Some(&contract) {
            self.embedding = Some(contract.clone());
            self.append_mutation(Mutation::SetEmbedding {
                session_id: self.session_id.clone(),
                embedding: Some(contract),
            });
        }
        Ok(())
    }

    /// Atomic full re-embed: rewrite **every** concept vector into a new space
    /// and swap the session contract in one staged graph transaction (K2).
    ///
    /// This is the operation `lambo re-embed` runs when a session migrates to a
    /// different embedder (e.g. bge_m3 → candle): the `EmbeddingContract`
    /// forbids two model spaces in one session, so the old vectors must be
    /// replaced — not relabelled — in the same batch as the contract change.
    /// `replace_embedding_with_operator_override` refuses that (it only lets a
    /// same-kind identifier rename relabel existing vectors); this method is
    /// the sanctioned path that replaces vectors first.
    ///
    /// * `updates` maps every concept id to its freshly embedded vector in the
    ///   target space. **Every** concept must appear: a concept left out keeps
    ///   a vector from the old space, which is exactly the mixed-space
    ///   violation this operation exists to end. An id that is not a concept,
    ///   a vector of the wrong width, or a non-finite vector is a hard error
    ///   and the graph is left untouched.
    /// * The contract swap allows the same width only (a re-embed never
    ///   changes dimensionality; a width change is a fresh session, not a
    ///   migration) and, like the RAM invariants everywhere, refuses a
    ///   non-different contract as a no-op.
    ///
    /// Mutations are appended in order, so the drained batch carries every
    /// `UpsertNode` (new vectors) **before** the trailing `SetEmbedding` (new
    /// contract). The store applies one flushed batch transactionally, so the
    /// durable session never holds the new contract beside old-space vectors —
    /// and on a crash mid-flush the old contract and old vectors survive
    /// together, which is consistent. Callers MUST flush the drained batch to
    /// the store; until then the RAM graph is ahead of the durable state.
    pub fn reembed_all(
        &mut self,
        updates: Vec<(NodeId, Vec<f32>)>,
        contract: crate::types::EmbeddingContract,
    ) -> Result<(), LamboError> {
        if let Some(existing) = &self.embedding {
            if existing.dim != contract.dim {
                return Err(invariant(format!(
                    "cannot re-embed session from width {} to width {}; a re-embed never \
                     changes dimensionality (start a fresh session for a different width)",
                    existing.dim, contract.dim
                )));
            }
            if *existing == contract {
                return Err(invariant(
                    "re-embed requested but the session already carries exactly this contract",
                ));
            }
        }

        let concept_ids: std::collections::HashSet<NodeId> =
            self.concepts().map(|c| c.id).collect();
        // Duplicate ids are as fatal as missing ones: a list [a, a] over
        // concepts {a, b} has the right length but leaves `b` carrying an
        // old-space vector — exactly the mixed-space state this method exists
        // to end. Count distinct coverage, not list length.
        let mut covered: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        for (id, _) in &updates {
            if !covered.insert(*id) {
                return Err(invariant(format!(
                    "re-embed updates list concept {id} twice; each concept appears exactly once"
                )));
            }
        }
        if covered.len() != concept_ids.len() {
            return Err(invariant(format!(
                "re-embed requires every concept: updates cover {} of {} concepts",
                covered.len(),
                concept_ids.len()
            )));
        }
        for (id, vector) in &updates {
            if !concept_ids.contains(id) {
                return Err(invariant(format!(
                    "re-embed update targets {id}, which is not a concept in this session"
                )));
            }
            if vector.len() != contract.dim || vector.iter().any(|x| !x.is_finite()) {
                return Err(invariant(format!(
                    "re-embed vector for {id} is non-finite or has width {} != contract {}",
                    vector.len(),
                    contract.dim
                )));
            }
        }

        // Rewrite each concept's vector in the node map, then emit its upsert
        // mutation (the batch orders every UpsertNode before the SetEmbedding).
        for (id, vector) in updates {
            // Write through the node map, then emit the upsert from a CLONE:
            // `append_mutation` takes `&mut self`, which cannot overlap the
            // `self.nodes` borrow.
            let node = match self.nodes.get_mut(&id) {
                Some(Node::Concept(c)) => {
                    c.embedding = Some(vector);
                    Node::Concept(c.clone())
                }
                Some(Node::Interaction(_)) => {
                    return Err(invariant(format!(
                        "re-embed update targets {id}, which is an interaction, not a concept"
                    )));
                }
                None => {
                    return Err(invariant(format!(
                        "re-embed update targets {id}, which is not a node"
                    )));
                }
            };
            self.append_mutation(Mutation::UpsertNode { node });
        }
        self.embedding = Some(contract.clone());
        self.append_mutation(Mutation::SetEmbedding {
            session_id: self.session_id.clone(),
            embedding: Some(contract),
        });
        Ok(())
    }

    /// Fill in vectors for concepts that have **none**, without touching the
    /// session contract or any vector already stored.
    ///
    /// This is the backfill twin of [`Graph::reembed_all`], and the two are
    /// mutually exclusive by design: `reembed_all` migrates *between* spaces
    /// and therefore refuses to run when the live contract is already the
    /// stored one, which is exactly the state a backfill runs in. Without this
    /// method a session that accumulated NULL vectors inside its own current
    /// space had no repair path at all — the 2026-09-01 dogfood finding, where
    /// `lambo re-embed` correctly refused with "already carries exactly this
    /// contract" and left 555 unembedded concepts in place.
    ///
    /// * The contract must already be stamped and identical to `contract`. A
    ///   session with no contract is refused rather than stamped here: a
    ///   backfill is repair, and stamping a space from a repair path is how a
    ///   session acquires a contract nobody chose.
    /// * Every id must name a concept whose `embedding` is `None`. Overwriting
    ///   an existing vector is refused — that is a migration, and migrations go
    ///   through `reembed_all` so the contract moves with them.
    /// * Width and finiteness are checked exactly as in `reembed_all`.
    /// * Partial coverage is fine and expected: this is the one vector
    ///   operation that does not require every concept, because the concepts it
    ///   skips are already correct.
    ///
    /// Returns how many concepts were given a vector. Callers MUST flush the
    /// drained batch; until then the RAM graph is ahead of the durable state.
    pub fn embed_missing(
        &mut self,
        updates: Vec<(NodeId, Vec<f32>)>,
        contract: &crate::types::EmbeddingContract,
    ) -> Result<usize, LamboError> {
        match &self.embedding {
            None => {
                return Err(invariant(
                    "cannot backfill embeddings in a session with no embedding contract; \
                     a contract is stamped by the first real write, never by a repair",
                ))
            }
            Some(existing) if existing != contract => {
                return Err(invariant(format!(
                    "cannot backfill embeddings from a different space: session carries \
                     kind={} dim={} but the live embedder is kind={} dim={}; that is a \
                     migration, which is `re-embed`, not a backfill",
                    existing.kind, existing.dim, contract.kind, contract.dim
                )))
            }
            Some(_) => {}
        }

        let mut covered: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        for (id, vector) in &updates {
            if !covered.insert(*id) {
                return Err(invariant(format!(
                    "backfill updates list concept {id} twice; each concept appears at most once"
                )));
            }
            match self.nodes.get(id) {
                Some(Node::Concept(c)) => {
                    if c.embedding.is_some() {
                        return Err(invariant(format!(
                            "backfill targets {id}, which already carries a vector; \
                             replacing a vector is a migration (`re-embed`), not a backfill"
                        )));
                    }
                }
                Some(Node::Interaction(_)) => {
                    return Err(invariant(format!(
                        "backfill targets {id}, which is an interaction, not a concept"
                    )))
                }
                None => {
                    return Err(invariant(format!(
                        "backfill targets {id}, which is not a node"
                    )))
                }
            }
            if vector.len() != contract.dim || vector.iter().any(|x| !x.is_finite()) {
                return Err(invariant(format!(
                    "backfill vector for {id} is non-finite or has width {} != contract {}",
                    vector.len(),
                    contract.dim
                )));
            }
        }

        // No `SetEmbedding` tail here: the contract is unchanged, so emitting
        // one would append a mutation that says nothing and make the batch look
        // like a migration to anything reading the log.
        let filled = updates.len();
        for (id, vector) in updates {
            let node = match self.nodes.get_mut(&id) {
                Some(Node::Concept(c)) => {
                    c.embedding = Some(vector);
                    Node::Concept(c.clone())
                }
                // Unreachable: validated above, and `self` is not shared across
                // the two loops.
                _ => unreachable!("backfill target validated as a concept above"),
            };
            self.append_mutation(Mutation::UpsertNode { node });
        }
        Ok(filled)
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
    /// by draining. Recall caches key on this (spec §8). Two kinds of logged
    /// mutation are deliberately not counted, because neither changes what the
    /// graph says: write intents ([`Graph::record_write_intent`]) and read-access
    /// bookkeeping ([`Graph::record_accesses`], issue #30).
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Drain the ordered mutation log into a batch (T3.4's flush input).
    ///
    /// The batch is in **chronological** write order. §2.4's phase grouping
    /// (nodes -> edges -> deletions -> transitions) holds within a single logical
    /// write, not across the batch. Replay in order — never re-sort.
    ///
    /// The batch is stamped with [`Graph::epoch`] at drain time
    /// ([`MutationBatch::mutation_epoch`]): an absolute watermark the store
    /// persists in the same transaction as the batch, so the counter and the
    /// content it counts land atomically — the durable watermark is never
    /// behind the count of durable mutations, though it may run ahead of it by
    /// the RAM-local epoch bumps (reservations, synonyms) the stamp carries
    /// (issue #17).
    pub fn drain_log(&mut self) -> MutationBatch {
        MutationBatch {
            mutations: std::mem::take(&mut self.mutation_log),
            mutation_epoch: self.epoch,
            gc_mark: self.gc_mark,
        }
    }

    /// GC's durable sweep accounting (issue #29); see [`GcMark`].
    pub fn gc_mark(&self) -> GcMark {
        self.gc_mark
    }

    /// Record a completed GC sweep: the next interval is measured from
    /// `epoch_after` and the `gc_max_interval` clock restarts at `at`.
    ///
    /// Not a mutation (no epoch bump, no log entry): the mark rides the next
    /// drained batch's stamp. The caller (the daemon) calls this inside the
    /// same write guard as the sweep, so the mark and the sweep's mutations
    /// drain — and so persist — together.
    pub fn record_gc_sweep(&mut self, epoch_after: u64, at: chrono::DateTime<chrono::Utc>) {
        self.gc_mark = self.gc_mark.merge(GcMark {
            last_gc_epoch: epoch_after,
            last_gc_at: Some(at),
            last_gc_at_reset: false,
        });
    }

    /// Exclude `n` epoch bumps from GC's session-mutation measure by advancing
    /// [`GcMark::last_gc_epoch`] by `n` (NEW-2), never past [`Graph::epoch`].
    ///
    /// GC's deferred survivor-bump drains use this: their own `UpsertNode`s
    /// advance the epoch, and crediting them as session writes made GC
    /// self-sustaining on an idle session. It is only for writes that **did**
    /// advance the epoch, called under the same write guard that appended
    /// them, with `n` equal to the bumps they caused.
    ///
    /// A write that does not advance the epoch needs no exemption and must not
    /// call this: the measure is `epoch - last_gc_epoch`, so such a write is
    /// already invisible to it, and "exempting" it would cancel real session
    /// writes out of `gc_interval` and the idle floor and suppress sweeps.
    /// (Issue #30 records accesses without advancing the epoch, so it is in
    /// this category.)
    ///
    /// The watermark is clamped to the current epoch: a watermark ahead of the
    /// epoch would hide the next writes from the measure until the epoch caught
    /// up. An `n` that would overshoot is a caller bug (debug-asserted).
    pub fn exempt_from_gc_measure(&mut self, n: u64) {
        let advanced = self.gc_mark.last_gc_epoch.saturating_add(n);
        debug_assert!(
            advanced <= self.epoch,
            "exempt_from_gc_measure({n}) would move last_gc_epoch {} past epoch {}: \
             only exempt bumps that were actually appended",
            self.gc_mark.last_gc_epoch,
            self.epoch
        );
        self.gc_mark.last_gc_epoch = advanced.min(self.epoch).max(self.gc_mark.last_gc_epoch);
    }

    /// Re-anchor the `gc_max_interval` clock at `at` after the stored
    /// `last_gc_at` was found in the future (issue #29: a forward wall-clock
    /// jump persisted a future sweep time, and the max-merge would otherwise
    /// keep it — disabling the time trigger until real time caught up).
    ///
    /// Not a mutation; rides the next drained batch like every mark change.
    /// Sets [`GcMark::last_gc_at_reset`] so the flush carry and the store
    /// accept this one regression of `last_gc_at` instead of max-merging it
    /// away. `last_gc_epoch` is untouched. The caller (the daemon) decides when
    /// a stored time is "in the future" — see
    /// [`crate::daemon::gc::gc_clock_ahead`].
    pub fn reanchor_gc_clock(&mut self, at: chrono::DateTime<chrono::Utc>) {
        self.gc_mark.last_gc_at = Some(at);
        self.gc_mark.last_gc_at_reset = true;
    }

    /// Start the `gc_max_interval` clock for a session that has never swept
    /// (issue #29): a no-op once [`GcMark::last_gc_at`] is set. The first timed
    /// sweep is then due a full interval after a writer first observed the
    /// session, never immediately on attach.
    pub fn anchor_gc_clock(&mut self, at: chrono::DateTime<chrono::Utc>) {
        if self.gc_mark.last_gc_at.is_none() {
            self.gc_mark.last_gc_at = Some(at);
        }
    }

    /// Re-append already-drained mutations to the **front** of the log,
    /// preserving chronological order (T8.1 shutdown drain).
    ///
    /// The write-behind flush task owns its `pending` buffer, so a batch it
    /// drained but has not yet made durable — most importantly one RETAINED
    /// after exhausted retries — is invisible to [`Graph::drain_log`]. On
    /// shutdown the task hands that buffer back here so `Memory::close`'s
    /// final `drain_log` can see it and flush it (COH-6). A hard
    /// `JoinHandle::abort()` would drop it with the task.
    ///
    /// Front, not back: everything in `mutations` was appended to the log
    /// **before** anything still in it, so prepending is what restores
    /// chronological order — the `src/graph/mod.rs` "replay in order, never
    /// re-sort" contract.
    ///
    /// The epoch is **not** bumped: these mutations were counted when they
    /// were first appended, the graph state they describe is already applied,
    /// and re-counting them would needlessly invalidate every recall cache
    /// entry at shutdown.
    ///
    /// This is the only re-entry point into the log and it is deliberately
    /// narrow: it takes mutations that this graph already produced. Feeding it
    /// anything else would put mutations in the log that the in-RAM graph does
    /// not reflect.
    pub fn push_front_log(&mut self, mutations: Vec<Mutation>) {
        if mutations.is_empty() {
            return;
        }
        self.mutation_log.splice(0..0, mutations);
    }

    /// Append a durable write intent to the log (J3 — appended at ack, so the
    /// write-behind drain and the close-time final flush carry it exactly as
    /// they carry every other mutation).
    ///
    /// The epoch is deliberately **not** bumped: an intent is not graph state —
    /// no node, edge, or recall-visible fact changes — and bumping would force
    /// a concurrent hybrid commit to replan against a graph that has not
    /// changed, and invalidate every recall cache entry for a record recall
    /// cannot see. The mutation log is the only thing touched.
    pub fn record_write_intent(&mut self, intent: WriteIntent) {
        debug_assert_eq!(intent.session_id, self.session_id);
        self.mutation_log.push(Mutation::PutWriteIntent { intent });
    }

    /// Append the consumption of a write intent (J3).
    ///
    /// **Must be called in the same write-lock critical section as the commit
    /// of the mutations the intent produced.** The flush loop's drain takes
    /// this same lock, so mutations appended under one hold always travel in
    /// one batch — and a batch is one store transaction, which is what makes
    /// "the applied write is durable" and "the intent is consumed" a single
    /// fact. Consuming under a *separate* hold opens the window this design
    /// exists to close: a flush between the two commits the applied mutations,
    /// the process dies, and the next serve replays an intent whose write is
    /// already durable — the double-apply.
    ///
    /// Epoch not bumped, for [`Graph::record_write_intent`]'s reason.
    pub fn consume_write_intent(&mut self, receipt: String, outcome: WriteIntentOutcome) {
        self.mutation_log.push(Mutation::ConsumeWriteIntent {
            session_id: self.session_id.clone(),
            receipt,
            outcome,
        });
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
mod tests;

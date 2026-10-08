//! [`Graph::assert_invariants`]: the single spec §5.7 checker every load,
//! test and debug path calls.
//!
//! The write paths enforce the invariants at write time (the module docs in
//! the root list how); this is the safety net that collects every violation
//! into one error. It reuses the root's private helpers (`edge_endpoint_error`,
//! the cycle DFS), so the write gate and the checker cannot drift apart.

use std::collections::{HashMap, HashSet};

use super::{edge_endpoint_error, invariant, Graph};
use crate::types::{ConceptType, EdgeType, LamboError, Node, NodeId};

impl Graph {
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
            if let Node::Concept(c) = n
                && let Some(vector) = &c.embedding
            {
                match &self.embedding {
                    Some(contract)
                        if vector.len() == contract.dim && vector.iter().all(|x| x.is_finite()) => {
                    }
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
            if let (Some(s), Some(t)) = (self.nodes.get(&e.source), self.nodes.get(&e.target))
                && let Some(msg) = edge_endpoint_error(e.edge_type, s, t)
            {
                v.push(format!("edge {} {msg}", e.id));
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
            if color.get(n).copied().unwrap_or(0) == 0
                && let Some(back) = self.dfs_cycle(*n, &mut color)
            {
                v.push(format!(
                    "Causal/Dependency/Hierarchical cycle detected through {back}"
                ));
                break;
            }
        }

        if v.is_empty() {
            Ok(())
        } else {
            Err(invariant(v.join("; ")))
        }
    }
}

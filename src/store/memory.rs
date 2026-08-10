//! In-RAM GraphStore for tests and fixture-ok parallel tracks.

use async_trait::async_trait;
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;
use std::time::Duration;

use super::{Capabilities, GraphStore};
use crate::types::{
    CanonizationEvent, Edge, EdgeType, GraphSnapshot, InteractionSpan, Mutation, MutationBatch,
    Node, NodeId, Scored, SessionId, StoreError,
};

#[derive(Default)]
struct SessionData {
    snapshot: GraphSnapshot,
}

/// Complete in-memory store. Structural queries computed naively (correct, not fast).
#[derive(Default)]
pub struct MemoryStore {
    inner: RwLock<HashMap<String, SessionData>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_session_mut<R>(
        &self,
        session: &SessionId,
        f: impl FnOnce(&mut SessionData) -> R,
    ) -> Result<R, StoreError> {
        let mut g = self
            .inner
            .write()
            .map_err(|_| StoreError::Backend("lock poisoned".into()))?;
        let entry = g.entry(session.0.clone()).or_insert_with(|| SessionData {
            snapshot: GraphSnapshot {
                session_id: session.clone(),
                ..Default::default()
            },
        });
        Ok(f(entry))
    }

    fn with_session<R>(
        &self,
        session: &SessionId,
        f: impl FnOnce(&SessionData) -> R,
    ) -> Result<R, StoreError> {
        let g = self
            .inner
            .read()
            .map_err(|_| StoreError::Backend("lock poisoned".into()))?;
        let data = g
            .get(&session.0)
            .ok_or_else(|| StoreError::SessionNotFound(session.0.clone()))?;
        Ok(f(data))
    }

    fn apply_mutation(snap: &mut GraphSnapshot, m: &Mutation) -> Result<(), StoreError> {
        match m {
            Mutation::UpsertNode { node } => match node {
                Node::Interaction(i) => {
                    if let Some(pos) = snap.interactions.iter().position(|x| x.id == i.id) {
                        snap.interactions[pos] = i.clone();
                    } else {
                        snap.interactions.push(i.clone());
                    }
                }
                Node::Concept(c) => {
                    if let Some(pos) = snap.concepts.iter().position(|x| x.id == c.id) {
                        snap.concepts[pos] = c.clone();
                    } else {
                        snap.concepts.push(c.clone());
                    }
                }
            },
            Mutation::UpsertEdge { edge } => {
                if let Some(pos) = snap.edges.iter().position(|x| x.id == edge.id) {
                    snap.edges[pos] = edge.clone();
                } else if let Some(pos) = snap.edges.iter().position(|x| {
                    x.source == edge.source
                        && x.target == edge.target
                        && x.edge_type == edge.edge_type
                }) {
                    snap.edges[pos] = edge.clone();
                } else {
                    snap.edges.push(edge.clone());
                }
            }
            Mutation::DeleteNode { id } => {
                snap.interactions.retain(|i| i.id != *id);
                snap.concepts.retain(|c| c.id != *id);
                snap.edges
                    .retain(|e| e.source != *id && e.target != *id && e.id != *id);
            }
            Mutation::DeleteEdge { id } => {
                snap.edges.retain(|e| e.id != *id);
            }
            Mutation::CanonizationTransition { event } => {
                if let Some(c) = snap.concepts.iter_mut().find(|c| c.id == event.node_id) {
                    c.canonization_status = event.to_status;
                    c.blast_radius = event.blast_radius;
                }
                snap.canonization_events.push(event.clone());
            }
        }
        Ok(())
    }
}

#[async_trait]
impl GraphStore for MemoryStore {
    async fn init_schema(&self) -> Result<(), StoreError> {
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::empty()
    }

    async fn flush(&self, batch: &MutationBatch) -> Result<(), StoreError> {
        // Group by session from first node/edge we see; mutations may span one session.
        let mut by_session: HashMap<String, Vec<&Mutation>> = HashMap::new();
        for m in &batch.mutations {
            let sid = match m {
                Mutation::UpsertNode { node } => node.session_id().0.clone(),
                Mutation::UpsertEdge { edge } => edge.session_id.0.clone(),
                Mutation::CanonizationTransition { event } => event.session_id.0.clone(),
                Mutation::DeleteNode { .. } | Mutation::DeleteEdge { .. } => {
                    // Apply to all sessions that contain the id (rare in practice).
                    String::new()
                }
            };
            by_session.entry(sid).or_default().push(m);
        }

        for (sid, muts) in by_session {
            if sid.is_empty() {
                // Deletions without session: scan all
                let mut g = self
                    .inner
                    .write()
                    .map_err(|_| StoreError::Backend("lock poisoned".into()))?;
                for data in g.values_mut() {
                    for m in &muts {
                        Self::apply_mutation(&mut data.snapshot, m)?;
                    }
                }
                continue;
            }
            self.with_session_mut(&SessionId(sid), |data| {
                for m in muts {
                    Self::apply_mutation(&mut data.snapshot, m)?;
                }
                Ok::<(), StoreError>(())
            })??;
        }
        Ok(())
    }

    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.with_session(session, |d| d.snapshot.clone())
    }

    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        let tokens_l: Vec<String> = tokens.iter().map(|t| t.to_lowercase()).collect();
        self.with_session(session, |d| {
            let mut scored: Vec<Scored<NodeId>> = d
                .snapshot
                .concepts
                .iter()
                .filter_map(|c| {
                    let content = c.content.to_lowercase();
                    let key = c.canonical_key.to_lowercase();
                    let hits = tokens_l
                        .iter()
                        .filter(|t| content.contains(t.as_str()) || key.contains(t.as_str()))
                        .count();
                    if hits == 0 {
                        return None;
                    }
                    Some(Scored::new(c.id, hits as f64))
                })
                .collect();
            scored.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            scored.truncate(limit);
            scored
        })
    }

    async fn vector_candidates(
        &self,
        _session: &SessionId,
        _embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        Err(StoreError::Capability(
            "MemoryStore has no VECTOR_SEARCH".into(),
        ))
    }

    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
    ) -> Result<u64, StoreError> {
        // Spec §4.1: count concepts that would be orphaned (only inbound edge is from `node`,
        // edge old enough). 1-hop definition.
        let now = Utc::now();
        self.with_session(session, |d| {
            let min_created = now - chrono::Duration::from_std(min_edge_age).unwrap_or_default();
            let mut count = 0u64;
            for c in &d.snapshot.concepts {
                if c.id == node {
                    continue;
                }
                let inbound: Vec<&Edge> = d
                    .snapshot
                    .edges
                    .iter()
                    .filter(|e| e.target == c.id && e.created_at <= min_created)
                    .collect();
                if inbound.is_empty() {
                    continue;
                }
                let only_from_node = inbound.iter().all(|e| e.source == node)
                    && inbound.iter().any(|e| e.source == node);
                if only_from_node && inbound.iter().any(|e| e.source == node) {
                    // Exactly: has edge from node, and no other sources
                    let sources: HashSet<NodeId> = inbound.iter().map(|e| e.source).collect();
                    if sources.len() == 1 && sources.contains(&node) {
                        count += 1;
                    }
                }
            }
            count
        })
    }

    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
    ) -> Result<InteractionSpan, StoreError> {
        // Spec §4.1: inbound Dependency/Causal/Hierarchical from concepts whose
        // origin_interaction is old enough; distinct interaction count + temporal coverage.
        let now = Utc::now();
        self.with_session(session, |d| {
            let min_created = now - chrono::Duration::from_std(min_age).unwrap_or_default();
            let structural = [
                EdgeType::Dependency,
                EdgeType::Causal,
                EdgeType::Hierarchical,
            ];
            let mut interaction_ids: HashSet<NodeId> = HashSet::new();
            let mut times = Vec::new();
            for e in &d.snapshot.edges {
                if e.target != node || !structural.contains(&e.edge_type) {
                    continue;
                }
                if e.created_at > min_created {
                    continue;
                }
                let Some(src) = d.snapshot.concepts.iter().find(|c| c.id == e.source) else {
                    continue;
                };
                let Some(ix) = d
                    .snapshot
                    .interactions
                    .iter()
                    .find(|i| i.id == src.origin_interaction)
                else {
                    continue;
                };
                if ix.created_at > min_created {
                    continue;
                }
                interaction_ids.insert(ix.id);
                times.push(ix.created_at);
            }
            let distinct = interaction_ids.len() as u64;
            let coverage = if times.is_empty() {
                0.0
            } else {
                let lo = times.iter().min().copied().unwrap();
                let hi = times.iter().max().copied().unwrap();
                let all: Vec<_> = d
                    .snapshot
                    .interactions
                    .iter()
                    .map(|i| i.created_at)
                    .collect();
                let sess_lo = all.iter().min().copied().unwrap_or(lo);
                let sess_hi = all.iter().max().copied().unwrap_or(hi);
                let sess_span = (sess_hi - sess_lo).num_milliseconds().max(0) as f64;
                if sess_span <= 0.0 {
                    0.0
                } else {
                    (hi - lo).num_milliseconds().max(0) as f64 / sess_span
                }
            };
            InteractionSpan { distinct, coverage }
        })
    }

    async fn record_canonization(&self, event: &CanonizationEvent) -> Result<(), StoreError> {
        self.with_session_mut(&event.session_id, |data| {
            Self::apply_mutation(
                &mut data.snapshot,
                &Mutation::CanonizationTransition {
                    event: event.clone(),
                },
            )
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentId, CanonizationStatus, Concept, ConceptType, Interaction};
    use chrono::TimeZone;

    fn sample_session() -> (SessionId, NodeId, NodeId, NodeId) {
        let sid = SessionId::from("test-sess");
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        let c2 = NodeId::new();
        (sid, i1, c1, c2)
    }

    #[tokio::test]
    async fn flush_and_load_roundtrip() {
        let store = MemoryStore::new();
        let (sid, i1, c1, _) = sample_session();
        let ts = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let batch = MutationBatch {
            mutations: vec![
                Mutation::UpsertNode {
                    node: Node::Interaction(Interaction {
                        id: i1,
                        session_id: sid.clone(),
                        agent_id: AgentId::from("a"),
                        prompt_text: Some("hi".into()),
                        previous_id: None,
                        created_at: ts,
                    }),
                },
                Mutation::UpsertNode {
                    node: Node::Concept(Concept {
                        id: c1,
                        session_id: sid.clone(),
                        content: "user schema".into(),
                        canonical_key: "schema user".into(),
                        concept_type: ConceptType::Entity,
                        origin_interaction: i1,
                        origin_agent: AgentId::from("a"),
                        created_at: ts,
                        access_count: 0,
                        last_accessed: None,
                        gc_survived: 0,
                        canonization_status: CanonizationStatus::None,
                        blast_radius: None,
                        last_demotion_time: None,
                        embedding: None,
                    }),
                },
            ],
        };
        store.flush(&batch).await.unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.interactions.len(), 1);
        assert_eq!(snap.concepts.len(), 1);
        assert_eq!(snap.concepts[0].content, "user schema");
    }

    #[tokio::test]
    async fn keyword_candidates_match() {
        let store = MemoryStore::new();
        let (sid, i1, c1, _) = sample_session();
        let ts = Utc::now();
        store
            .flush(&MutationBatch {
                mutations: vec![
                    Mutation::UpsertNode {
                        node: Node::Interaction(Interaction {
                            id: i1,
                            session_id: sid.clone(),
                            agent_id: AgentId::from("a"),
                            prompt_text: None,
                            previous_id: None,
                            created_at: ts,
                        }),
                    },
                    Mutation::UpsertNode {
                        node: Node::Concept(Concept {
                            id: c1,
                            session_id: sid.clone(),
                            content: "user schema".into(),
                            canonical_key: "schema user".into(),
                            concept_type: ConceptType::Entity,
                            origin_interaction: i1,
                            origin_agent: AgentId::from("a"),
                            created_at: ts,
                            access_count: 0,
                            last_accessed: None,
                            gc_survived: 0,
                            canonization_status: CanonizationStatus::None,
                            blast_radius: None,
                            last_demotion_time: None,
                            embedding: None,
                        }),
                    },
                ],
            })
            .await
            .unwrap();
        let hits = store
            .keyword_candidates(&sid, &["schema".into()], 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item, c1);
    }

    #[tokio::test]
    async fn vector_candidates_capability_error() {
        let store = MemoryStore::new();
        let err = store
            .vector_candidates(&SessionId::from("x"), &[0.0; 1024], 5)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Capability(_)));
    }
}

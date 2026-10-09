//! Pure projection: durable mutations to recall-index writes, the version
//! arithmetic, and contract-keyed index naming. No I/O; every rule the
//! mirror follows is decided here and unit-tested here.

use sha2::{Digest, Sha256};

use super::index::{DocOp, IndexDoc};
use crate::types::{Concept, EmbeddingContract, Mutation, Node, SessionId, StoreError};

/// Short, stable hash of an embedding contract: the index-name half of the
/// contract guarantee. Every document in `{prefix}-v-{hash}` was embedded
/// under exactly this `{kind, model, dim}`, so a query that names the hash of
/// the contract it was embedded with can only rank vectors from its own space.
pub(crate) fn contract_hash(contract: &EmbeddingContract) -> String {
    let mut h = Sha256::new();
    h.update(contract.kind.as_bytes());
    h.update([0]);
    match &contract.model {
        Some(m) => {
            h.update([1]);
            h.update(m.as_bytes());
        }
        None => h.update([0]),
    }
    h.update([0]);
    h.update(contract.dim.to_string().as_bytes());
    let digest = h.finalize();
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Validate an operator-supplied index prefix against Elasticsearch's index
/// naming rules, leaving room for the `-v-{hash}` / `-meta` suffixes.
///
/// Deletes and counts address every data index through the wildcard
/// `{prefix}-v-*`. A prefix that contains `-v-` or ends in `-v` would let
/// that pattern match another deployment's indices on a shared cluster
/// (#18 review L2: prefix `lambo` reaches `lambo-v-prod-v-<hash>`), so both
/// are refused. With them refused, `{q}-v-{hash}` starts with `{p}-v-` only
/// when `p == q`.
pub(crate) fn validate_index_prefix(prefix: &str) -> Result<(), StoreError> {
    if prefix.contains("-v-") || prefix.ends_with("-v") {
        return Err(StoreError::Backend(format!(
            "recall.index_prefix {prefix:?} must not contain \"-v-\" or end in \"-v\": data \
             indices are named {{prefix}}-v-{{hash}} and addressed as {{prefix}}-v-*, so such a \
             prefix would reach another deployment's indices on the same cluster"
        )));
    }
    let ok_chars = prefix
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
    let bad_start = prefix.starts_with(['-', '_', '+', '.']);
    if prefix.is_empty() || prefix.len() > 200 || !ok_chars || bad_start {
        return Err(StoreError::Backend(format!(
            "recall.index_prefix {prefix:?} is not a valid index name prefix: use 1-200 \
             lowercase letters, digits, '-', '_' or '.', not starting with '-', '_', '+' or '.'"
        )));
    }
    Ok(())
}

/// The largest fencing token whose shifted value still fits an Elasticsearch
/// external version (a positive signed 64-bit integer).
const MAX_VERSIONED_TOKEN: u64 = (i64::MAX as u64) >> 32;

/// The external version a mirror write is made at:
/// `(fencing_token << 32) | flush_counter`.
///
/// A new holder's token is strictly greater than any earlier holder's, so its
/// writes outrank every write the old one could still land late; within one
/// holder the counter orders its flushes. `None` when the token is too large
/// to shift into a signed 64-bit version (refuse the mirror rather than wrap).
pub(crate) fn mirror_version(token: u64, counter: u32) -> Option<u64> {
    (token <= MAX_VERSIONED_TOKEN).then(|| (token << 32) | u64::from(counter))
}

/// The projection of one batch for one session.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Projection {
    /// Index writes, at most one per node id (the last mutation for an id in
    /// the batch wins, because the whole batch is written at one version and
    /// an equal version would be refused as a conflict).
    pub ops: Vec<DocOp>,
    /// Every contract a write targets, first-seen order.
    pub contracts: Vec<EmbeddingContract>,
    /// The session's contract after the batch.
    pub contract_after: Option<EmbeddingContract>,
}

/// Project `mutations` for `session`, starting from the session's durable
/// contract `contract_before`.
///
/// | Mutation | Index write |
/// |---|---|
/// | `UpsertNode(Concept)` with a vector of the contract's width | index the document |
/// | `UpsertNode(Concept)` without one | delete it (the vector was cleared) |
/// | `DeleteNode` | delete it |
/// | `SetEmbedding` | switch the contract for what follows |
/// | everything else | nothing (graph structure and canonization stay durable-only) |
///
/// With no contract in force nothing is written: there is no index to write
/// to, and a vector without a contract is not interpretable (the same rule
/// `load_session`'s legacy-vector quarantine applies). `DeleteNode` names no
/// session, so a delete is written for the session the batch was attributed
/// to; an id that was never indexed there is a no-op delete.
pub(crate) fn project(
    session: &SessionId,
    mutations: &[Mutation],
    contract_before: Option<EmbeddingContract>,
    version: Option<u64>,
) -> Projection {
    let mut current = contract_before;
    let mut ops: Vec<DocOp> = Vec::new();
    let mut contracts: Vec<EmbeddingContract> = Vec::new();
    let mut push = |op: DocOp, ops: &mut Vec<DocOp>| {
        if !contracts.contains(op.contract()) {
            contracts.push(op.contract().clone());
        }
        ops.retain(|o| o.id() != op.id());
        ops.push(op);
    };
    for m in mutations {
        match m {
            Mutation::SetEmbedding {
                session_id,
                embedding,
            } if session_id == session => current.clone_from(embedding),
            Mutation::UpsertNode {
                node: Node::Concept(c),
            } if &c.session_id == session => {
                let Some(contract) = current.clone() else {
                    continue;
                };
                let op = match index_doc(c, &contract, version) {
                    Some(doc) => DocOp::Index {
                        contract,
                        id: c.id,
                        version,
                        doc,
                    },
                    None => DocOp::Delete {
                        contract,
                        id: c.id,
                        version,
                    },
                };
                push(op, &mut ops);
            }
            Mutation::DeleteNode { id } => {
                if let Some(contract) = current.clone() {
                    push(
                        DocOp::Delete {
                            contract,
                            id: *id,
                            version,
                        },
                        &mut ops,
                    );
                }
            }
            _ => {}
        }
    }
    Projection {
        ops,
        contracts,
        contract_after: current,
    }
}

/// The document for `concept` under `contract`, or `None` when the concept
/// carries no vector of the contract's width.
pub(crate) fn index_doc(
    concept: &Concept,
    contract: &EmbeddingContract,
    version: Option<u64>,
) -> Option<IndexDoc> {
    let embedding = concept.embedding.as_ref()?;
    if embedding.len() != contract.dim {
        return None;
    }
    Some(IndexDoc {
        session_id: concept.session_id.0.clone(),
        node_id: concept.id.0.to_string(),
        canonical_key: concept.canonical_key.clone(),
        content: concept.content.clone(),
        concept_type: serde_json::to_value(concept.concept_type)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        created_at: concept.created_at,
        embedding: embedding.clone(),
        v: version.unwrap_or(0),
    })
}

/// The session a batch writes, when it names exactly one.
pub(crate) fn sole_session(mutations: &[Mutation]) -> Result<Option<SessionId>, usize> {
    let ids = crate::store::batch::batch_session_ids(mutations);
    match ids.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(SessionId::new(*one))),
        many => Err(many.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentId, CanonizationStatus, ConceptType, NodeId};

    fn contract(model: Option<&str>) -> EmbeddingContract {
        EmbeddingContract {
            kind: "fixture".into(),
            model: model.map(str::to_owned),
            dim: 2,
        }
    }

    fn concept(sid: &SessionId, id: NodeId, embedding: Option<Vec<f32>>) -> Mutation {
        Mutation::UpsertNode {
            node: Node::Concept(Concept {
                id,
                session_id: sid.clone(),
                content: "c".into(),
                canonical_key: "c".into(),
                concept_type: ConceptType::Logic,
                origin_interaction: NodeId::new(),
                origin_agent: AgentId::new("a"),
                created_at: chrono::Utc::now(),
                access_count: 0,
                last_accessed: None,
                gc_survived: 0,
                canonization_status: CanonizationStatus::None,
                blast_radius: None,
                last_demotion_time: None,
                embedding,
                human_confirmed: 0,
                chunk_group_id: None,
                embedding_source: None,
            }),
        }
    }

    #[test]
    fn the_contract_hash_is_stable_and_separates_every_field() {
        let base = contract(None);
        assert_eq!(contract_hash(&base), contract_hash(&contract(None)));
        assert_eq!(contract_hash(&base).len(), 16);
        let variants = [
            contract(Some("m")),
            contract(Some("")),
            EmbeddingContract {
                kind: "bge_m3".into(),
                ..base.clone()
            },
            EmbeddingContract {
                dim: 3,
                ..base.clone()
            },
        ];
        for v in &variants {
            assert_ne!(contract_hash(&base), contract_hash(v), "{v:?}");
        }
        assert_ne!(contract_hash(&variants[0]), contract_hash(&variants[1]));
    }

    #[test]
    fn the_version_orders_holders_before_counters_and_refuses_overflow() {
        let old = mirror_version(1, u32::MAX).unwrap();
        let new = mirror_version(2, 1).unwrap();
        assert!(new > old, "a new holder outranks every counter of the old");
        assert!(mirror_version(2, 2).unwrap() > new);
        assert!(mirror_version(MAX_VERSIONED_TOKEN, u32::MAX).unwrap() <= i64::MAX as u64);
        assert_eq!(mirror_version(MAX_VERSIONED_TOKEN + 1, 1), None);
    }

    #[test]
    fn the_projection_follows_the_mutation_table() {
        let sid = SessionId::new("s");
        let other = SessionId::new("t");
        let (a, b, c, d) = (NodeId::new(), NodeId::new(), NodeId::new(), NodeId::new());
        let k1 = contract(None);
        let k2 = contract(Some("m2"));
        let muts = vec![
            // No contract yet: nothing to write.
            concept(&sid, a, Some(vec![1.0, 0.0])),
            Mutation::SetEmbedding {
                session_id: sid.clone(),
                embedding: Some(k1.clone()),
            },
            concept(&sid, a, Some(vec![1.0, 0.0])),
            // Wrong width and no vector both delete.
            concept(&sid, b, Some(vec![1.0, 0.0, 0.0])),
            concept(&sid, c, None),
            // Another session's concept is not this session's write.
            concept(&other, d, Some(vec![0.0, 1.0])),
            Mutation::DeleteEdge { id: NodeId::new() },
            Mutation::SetEmbedding {
                session_id: sid.clone(),
                embedding: Some(k2.clone()),
            },
            // The last write for an id wins.
            concept(&sid, a, Some(vec![0.0, 1.0])),
            Mutation::DeleteNode { id: c },
        ];
        let p = project(&sid, &muts, None, Some(7));
        assert_eq!(p.contract_after, Some(k2.clone()));
        assert_eq!(p.contracts, vec![k1.clone(), k2.clone()]);
        let summary: Vec<(NodeId, &str, &EmbeddingContract)> = p
            .ops
            .iter()
            .map(|op| match op {
                DocOp::Index {
                    id,
                    contract,
                    version,
                    doc,
                } => {
                    assert_eq!(*version, Some(7));
                    assert_eq!(doc.v, 7);
                    (*id, "index", contract)
                }
                DocOp::Delete {
                    id,
                    contract,
                    version,
                } => {
                    assert_eq!(*version, Some(7));
                    (*id, "delete", contract)
                }
            })
            .collect();
        assert_eq!(
            summary,
            vec![(b, "delete", &k1), (a, "index", &k2), (c, "delete", &k2)]
        );
    }

    #[test]
    fn a_cleared_contract_stops_writes() {
        let sid = SessionId::new("s");
        let muts = vec![
            Mutation::SetEmbedding {
                session_id: sid.clone(),
                embedding: None,
            },
            concept(&sid, NodeId::new(), Some(vec![1.0, 0.0])),
            Mutation::DeleteNode { id: NodeId::new() },
        ];
        let p = project(&sid, &muts, Some(contract(None)), None);
        assert!(p.ops.is_empty());
        assert_eq!(p.contract_after, None);
    }

    #[test]
    fn index_prefixes_follow_the_engine_naming_rules() {
        for good in ["lambo", "lambo-prod", "a.b_c-1"] {
            validate_index_prefix(good).unwrap();
        }
        for bad in ["", "Lambo", "-x", "_x", "+x", ".x", "a b", "a*", "a/b"] {
            assert!(validate_index_prefix(bad).is_err(), "{bad:?}");
        }
    }

    /// L2: deletes reach `{prefix}-v-*`. A prefix that holds `-v-`, or ends
    /// in `-v`, would make another deployment's data indices match that
    /// pattern (`lambo-v-*` matches `lambo-v-prod-v-<hash>` and
    /// `lambo-v-v-<hash>`), so such prefixes are refused: then
    /// `{p}-v-{hash}` matches `{q}-v-*` only when `p == q`.
    #[test]
    fn no_prefix_can_reach_another_deployments_indices() {
        for bad in ["lambo-v-prod", "lambo-v", "a-v-b", "x-v-"] {
            let err = validate_index_prefix(bad).unwrap_err();
            assert!(err.to_string().contains("-v-"), "{bad:?}: {err}");
        }
        for good in [
            "lambo",
            "lambo-prod",
            "lambo-vx",
            "lambo-dev2",
            "v",
            "v-lambo",
        ] {
            validate_index_prefix(good).unwrap();
        }
        // Exhaustive over short prefixes: no two accepted prefixes p != q
        // let q's data index match p's delete pattern.
        let alphabet = ['a', 'v', '-'];
        let mut prefixes = vec![String::new()];
        for _ in 0..4 {
            let next: Vec<String> = prefixes
                .iter()
                .flat_map(|p| alphabet.iter().map(move |c| format!("{p}{c}")))
                .collect();
            prefixes.extend(next);
        }
        prefixes.sort();
        prefixes.dedup();
        let ok: Vec<&String> = prefixes
            .iter()
            .filter(|p| validate_index_prefix(p).is_ok())
            .collect();
        for p in &ok {
            for q in &ok {
                let index = format!("{q}-v-0123456789abcdef");
                if p != q {
                    assert!(
                        !index.starts_with(&format!("{p}-v-")),
                        "{p:?}'s pattern reaches {index:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_batch_is_attributed_only_to_a_single_named_session() {
        let (s, t) = (SessionId::new("s"), SessionId::new("t"));
        assert_eq!(sole_session(&[]), Ok(None));
        assert_eq!(
            sole_session(&[Mutation::DeleteNode { id: NodeId::new() }]),
            Ok(None)
        );
        assert_eq!(
            sole_session(&[concept(&s, NodeId::new(), None)]),
            Ok(Some(s.clone()))
        );
        assert_eq!(
            sole_session(&[
                concept(&s, NodeId::new(), None),
                concept(&t, NodeId::new(), None)
            ]),
            Err(2)
        );
    }
}

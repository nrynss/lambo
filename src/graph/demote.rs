//! Context-overflow demotion (T2.5) — spec §7 `demote()`.
//!
//! A context-overflow chunk (the caller detects the overflow) is split into
//! sentences per UAX #29 and each non-empty sentence becomes a fresh
//! [`ConceptType::Observation`] concept. Demoted observations are **new
//! information by construction**:
//!
//! * they do **not** go through the match step of the canonicalization pipeline
//!   (spec §7.1 step 5) — there is no merge with an existing concept carrying
//!   the same canonical key;
//! * they are **never deduplicated**, even when two sentences are identical —
//!   the chunk already overflowed the context, so its content is new to the
//!   graph;
//! * every observation from one chunk shares `chunk_group_id` — the T5.2
//!   sibling co-retrieval contract: recall phase 2 force-includes
//!   `chunk_group_id` siblings (spec §8).
//!
//! ## Segmentation
//!
//! Sentences are split with [`UnicodeSegmentation::split_sentence_bounds`]
//! (`unicode-segmentation`, the spec §6.3 pinned crate). A custom
//! `chunk_split_fn` is cut per spec §7 — this module adds no segmentation logic
//! of its own; whatever UAX #29 does (acronyms, abbreviations, numeric
//! expressions) is the contract. Each segment is trimmed and empty /
//! whitespace-only segments are skipped, so an empty or whitespace-only chunk
//! returns `Ok(vec![])` with zero graph changes.
//!
//! ## Writes
//!
//! All mutations flow through [`Graph::insert_concept`] (T2.1): each observation
//! gets its structural `Derives` edge from `interaction` by construction (the
//! §5.7 "every concept has ≥ 1 Derives edge" invariant holds at write time; the
//! initial weight is Graph-owned at 0.9 — this module creates no edges itself),
//! and the write-behind log stays ordered node-before-its-edge per sentence.
//! Synchronous and pure like every P2 module: the graph owns no lock and nothing
//! here holds one across an await (spec §6.4).

use chrono::Utc;
use unicode_segmentation::UnicodeSegmentation;

use crate::graph::{canonical, Graph};
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, LamboError, Node, NodeId, StoreError,
};

/// Context-overflow demotion: one fresh `Observation` concept per non-empty
/// sentence of `chunk`, all sharing `chunk_group_id`; returns the created
/// concept ids in sentence order.
///
/// # Errors
///
/// [`StoreError::NotFound`] when `interaction` is not an `Interaction` node in
/// `graph` — missing entirely, or present but a concept (both are the pinned
/// "validate the interaction" contract; `insert_concept` would reject a
/// non-interaction origin with an `Invariant`, so we validate up front for the
/// typed error). Nothing is written on error.
///
/// # Behavior
///
/// 1. Validate `interaction` — must be an existing `Interaction` node.
/// 2. Segment `chunk` with UAX #29 ([`split_sentence_bounds`]) — the one and
///    only segmentation (custom `chunk_split_fn` is cut, spec §7).
/// 3. Per sentence: trim, skip empty/whitespace-only, then create exactly one
///    `Observation` concept:
///    * `content` = the trimmed sentence;
///    * `canonical_key` = the T2.2 derivation with the graph's synonym table
///      ([`canonical_key_for`]) — the key is derived, never matched (demoted
///      observations skip spec §7.1 step 5);
///    * `origin_interaction` = `interaction`, `origin_agent` = `agent`;
///    * `chunk_group_id` = `Some(chunk_group_id)` (T5.2 co-retrieval contract).
///    No dedup across sentences — duplicates are new by construction.
/// 4. Insert via [`Graph::insert_concept`], which creates the structural
///    `Derives` edge from `interaction` by construction.
///
/// An empty or whitespace-only `chunk` yields `Ok(vec![])` with no mutations.
pub fn demote(
    graph: &mut Graph,
    interaction: NodeId,
    agent: &AgentId,
    chunk: &str,
    chunk_group_id: &str,
) -> Result<Vec<NodeId>, LamboError> {
    // Step 1 — validate the interaction up front. A missing node and a node that
    // exists but is not an interaction are both `NotFound`.
    match graph.node(interaction) {
        Some(Node::Interaction(_)) => {}
        Some(_) => {
            return Err(LamboError::Store(StoreError::NotFound(format!(
                "interaction {interaction} not found (node exists but is not an interaction)"
            ))));
        }
        None => {
            return Err(LamboError::Store(StoreError::NotFound(format!(
                "interaction {interaction} not found"
            ))));
        }
    }

    // Steps 2–4 — one Observation per non-empty sentence, in order.
    let mut created: Vec<NodeId> = Vec::new();
    for sentence in chunk.split_sentence_bounds() {
        let sentence = sentence.trim();
        if sentence.is_empty() {
            continue;
        }
        let concept = Concept {
            id: NodeId::new(),
            session_id: graph.session_id().clone(),
            content: sentence.to_string(),
            canonical_key: canonical_key_for(graph, sentence),
            concept_type: ConceptType::Observation,
            origin_interaction: interaction,
            origin_agent: agent.clone(),
            created_at: Utc::now(),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: Some(chunk_group_id.to_string()),
        };
        let id = concept.id;
        graph.insert_concept(concept, interaction)?;
        created.push(id);
    }
    Ok(created)
}

/// Canonical-key derivation for a demoted observation — the T2.2 pipeline with
/// the graph's synonym table, raw-input lookup first (spec §7.1 steps 1–4).
///
/// [`canonical::canonical_key`] pins `impl Fn(&str) -> Option<&str>`, whose
/// elided lifetimes make it `for<'a> Fn(&'a str) -> Option<&'a str>`; a closure
/// borrowing the graph cannot satisfy that bound (the mapped value borrows from
/// the graph, not from the input — the T2.2 constraint that pushed
/// `canonicalize` to do its own raw lookup). The raw lookup is therefore done
/// here and `canonical_key` is applied to the effective string with a
/// never-matching fn — semantically identical: direct lookup only (no chains),
/// and any whitespace difference is irrelevant because `normalize_tokens` splits
/// on all whitespace.
fn canonical_key_for(graph: &Graph, sentence: &str) -> String {
    let raw = sentence.trim();
    let effective = graph.synonym(raw).unwrap_or(raw);
    canonical::canonical_key(effective, no_synonym)
}

/// Never-matching synonym lookup — a `fn` item satisfies `canonical_key`'s
/// higher-ranked bound; the caller has already resolved synonyms.
fn no_synonym(_: &str) -> Option<&str> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    use crate::types::{EdgeType, Interaction, Mutation, SessionId};

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

    /// `chunk_group_id: None` per the T2.1 test-helper convention.
    fn concept(id: u64, origin: NodeId, content: &str) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(2, id)),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.into(),
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

    fn uid(u: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(0, u))
    }

    /// Fresh graph with one interaction; returns (graph, interaction id).
    fn fresh_graph() -> (Graph, NodeId) {
        let mut g = Graph::new(sid());
        let i = interaction(1, None, 0);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        (g, iid)
    }

    fn concept_of<'a>(g: &'a Graph, id: NodeId) -> &'a Concept {
        match g.node(id) {
            Some(Node::Concept(c)) => c,
            other => panic!("{id} is not a concept node: {other:?}"),
        }
    }

    fn contents(g: &Graph, ids: &[NodeId]) -> Vec<String> {
        ids.iter().map(|&id| concept_of(g, id).content.clone()).collect()
    }

    #[test]
    fn multi_sentence_chunk_yields_one_observation_per_sentence() {
        let (mut g, iid) = fresh_graph();
        let ids = demote(
            &mut g,
            iid,
            &agent(),
            "First sentence. Second sentence! Third?",
            "chunk-1",
        )
        .unwrap();

        assert_eq!(ids.len(), 3);
        assert_eq!(contents(&g, &ids), ["First sentence.", "Second sentence!", "Third?"]);
        for (i, c) in ids.iter().map(|&id| concept_of(&g, id)).enumerate() {
            assert_eq!(c.concept_type, ConceptType::Observation, "sentence {i}");
            assert!(!c.canonical_key.is_empty(), "sentence {i}: empty canonical key");
            assert_eq!(c.origin_interaction, iid, "sentence {i}");
            assert_eq!(c.origin_agent, agent(), "sentence {i}");
            // T5.2 sibling co-retrieval contract: all share the chunk group id.
            assert_eq!(c.chunk_group_id.as_deref(), Some("chunk-1"), "sentence {i}");
            // §5.7 invariant, by construction via insert_concept.
            assert!(
                g.edge_between(iid, c.id, EdgeType::Derives).is_some(),
                "sentence {i}: missing Derives edge"
            );
        }
        g.assert_invariants().unwrap();
    }

    #[test]
    fn single_sentence_and_no_terminator_chunk() {
        let (mut g, iid) = fresh_graph();
        // Contraction apostrophe is not a sentence boundary (UAX #29).
        let ids = demote(&mut g, iid, &agent(), "It's one sentence.", "g1").unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(contents(&g, &ids), ["It's one sentence."]);
        assert!(!concept_of(&g, ids[0]).canonical_key.is_empty());
        assert_eq!(concept_of(&g, ids[0]).chunk_group_id.as_deref(), Some("g1"));

        // No sentence terminator at all: the whole chunk is one sentence.
        let ids = demote(&mut g, iid, &agent(), "no terminator here", "g2").unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(contents(&g, &ids), ["no terminator here"]);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn empty_and_whitespace_only_chunks_are_noops() {
        let (mut g, iid) = fresh_graph();
        let before_nodes = g.node_count();
        let before_log = g.log_len();

        assert!(demote(&mut g, iid, &agent(), "", "g").unwrap().is_empty());
        assert!(demote(&mut g, iid, &agent(), "  \n\t  ", "g").unwrap().is_empty());
        // No graph changes, no mutations.
        assert_eq!(g.node_count(), before_nodes);
        assert_eq!(g.log_len(), before_log);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn missing_interaction_returns_not_found() {
        let (mut g, _) = fresh_graph();
        let err = demote(&mut g, uid(999), &agent(), "Hello.", "g").unwrap_err();
        assert!(matches!(err, LamboError::Store(StoreError::NotFound(_))), "{err}");
        assert!(err.to_string().contains("not found"), "{err}");
        assert_eq!(g.log_len(), 1); // first interaction: node upsert only, unchanged
        g.assert_invariants().unwrap();
    }

    #[test]
    fn non_interaction_node_returns_not_found() {
        let (mut g, iid) = fresh_graph();
        let c = concept(1, iid, "user schema");
        let cid = c.id;
        g.insert_concept(c, iid).unwrap();

        let err = demote(&mut g, cid, &agent(), "Hello.", "g").unwrap_err();
        assert!(matches!(err, LamboError::Store(StoreError::NotFound(_))), "{err}");
        // Nothing written by the failed demote (interaction + seeded concept).
        assert_eq!(g.node_count(), 2);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn duplicate_sentences_are_not_deduped() {
        let (mut g, iid) = fresh_graph();
        let ids = demote(&mut g, iid, &agent(), "Same. Same.", "g").unwrap();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        let c0 = concept_of(&g, ids[0]);
        let c1 = concept_of(&g, ids[1]);
        assert_eq!(c0.content, "Same.");
        assert_eq!(c1.content, "Same.");
        // Same key, but two fresh concepts — observations never merge (spec §7).
        assert_eq!(c0.canonical_key, c1.canonical_key);
        assert_eq!(c0.chunk_group_id, c1.chunk_group_id);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn trailing_punctuation_and_whitespace() {
        let (mut g, iid) = fresh_graph();
        let ids = demote(&mut g, iid, &agent(), "Lead in.  Second part!\n", "g").unwrap();
        // UAX #29 segments may carry trailing whitespace (and a leading empty
        // segment) — trim + skip empties must yield exactly the two sentences.
        assert_eq!(contents(&g, &ids), ["Lead in.", "Second part!"]);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn uax29_numeric_acronym_and_abbreviation_rules() {
        let (mut g, iid) = fresh_graph();
        // SB6: no boundary between a terminator and a following digit, so "3.14"
        // stays inside one sentence; the real boundary comes before "Next.".
        let ids = demote(&mut g, iid, &agent(), "Pi is 3.14. Next.", "g").unwrap();
        assert_eq!(contents(&g, &ids), ["Pi is 3.14.", "Next."]);
        // SB7 keeps "U.S.A." whole (UpperLower ATerm × Upper), and SB8 places no
        // boundary before a Lowercase start — the chunk is one sentence.
        // (unicode-segmentation implements the default UAX #29 rules without the
        // locale abbreviation lists of SB6, so "Dr." would terminate; the rules
        // above are the ones that fire on these inputs.)
        let ids = demote(&mut g, iid, &agent(), "U.S.A. is big.", "g").unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(contents(&g, &ids), ["U.S.A. is big."]);
        g.assert_invariants().unwrap();
    }

    #[test]
    fn mutation_log_is_ordered_node_then_edge_per_sentence() {
        let (mut g, iid) = fresh_graph();
        let _initial = g.drain_log(); // interaction node + Temporal edge
        let ids = demote(&mut g, iid, &agent(), "One. Two.", "g").unwrap();
        assert_eq!(ids.len(), 2);

        let batch = g.drain_log();
        // Per sentence: one UpsertNode, then the Derives edge referencing it.
        assert_eq!(batch.mutations.len(), 2 * ids.len());
        let mut seen_nodes: Vec<NodeId> = Vec::new();
        for m in &batch.mutations {
            match m {
                Mutation::UpsertNode { node } => seen_nodes.push(node.id()),
                Mutation::UpsertEdge { edge } => {
                    assert_eq!(edge.edge_type, EdgeType::Derives);
                    assert_eq!(edge.source, iid);
                    assert!(
                        seen_nodes.contains(&edge.target),
                        "Derives edge target {} not yet upserted",
                        edge.target
                    );
                }
                other => panic!("unexpected mutation {other:?}"),
            }
        }
        // Returned ids == upsert order of the observation nodes.
        let obs_in_log: Vec<NodeId> = batch
            .mutations
            .iter()
            .filter_map(|m| match m {
                Mutation::UpsertNode { node } => match node {
                    Node::Concept(c) if c.concept_type == ConceptType::Observation => Some(c.id),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(obs_in_log, ids);
        g.assert_invariants().unwrap();
    }
}

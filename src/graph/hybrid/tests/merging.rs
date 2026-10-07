//! Semantic merges: thresholds, decaying edges, canonical-key collapse
//! and ties.

use super::*;

#[tokio::test]
async fn near_pair_merges_with_decaying_semantic_edge() {
    let sess = "hybrid-near";
    // Interaction 1 derives "register user" -> fresh concept C1.
    let (graph, i1) = graph_with_interaction(sess, 1, 0, "user signs up for the platform");
    // Interaction 2 (origin context for the near content).
    let mut second = interaction(2, Some(i1), 60, "an admin creates an account for a user");
    second.session_id = sid(sess);
    let i2 = second.id;
    graph.write().insert_interaction(second).unwrap();

    // Interaction 1: derive "register user" (canonical miss -> fresh concept).
    let out1 = derive(
        graph.clone(),
        &SpyStore::with_vector(Vec::new()),
        &RecordingEmbedder::new(),
        &contract("fixture", 1024),
        i1,
        &agent(),
        &[(NEAR_A, ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();
    assert_eq!(out1.created.len(), 1);
    let c1 = out1.created[0];

    // Interaction 2: "create account" — store returns C1 at 0.9 (>= 0.85).
    let recorder = RecordingEmbedder::new();
    let out2 = derive(
        graph.clone(),
        &SpyStore::with_vector(vec![hit(c1, 0.9)]),
        &recorder,
        &contract("fixture", 1024),
        i2,
        &agent(),
        &[(NEAR_B, ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();

    // Calibration rule: the embed was called WITH context (name + origin),
    // never the bare label.
    let texts = recorder.embedded_texts();
    assert_eq!(
        texts.len(),
        1,
        "exactly one embed for the single unmatched concept"
    );
    let ctx = &texts[0];
    assert!(
        ctx.contains(NEAR_B) && ctx.contains("creates an account"),
        "embed must carry the concept name AND origin interaction text, got {ctx:?}"
    );
    assert!(
        !ctx.contains("Concept:"),
        "origin text present so the context must not fall back to the bare framing"
    );

    // The near pair merged into a SECOND concept + a decaying Semantic edge
    // (a merge cannot be a bare reuse: a `Semantic` edge legally connects two
    // concepts, and the new content has a distinct canonical key).
    assert_eq!(out2.created.len(), 1, "new concept for the near surface");
    let c2 = out2.created[0];
    // MINOR-3: the merge target reports in `semantic_merged`, NOT `matched`.
    // A merge does not re-upsert the target nor Derives-reinforce it, so
    // `matched` must not over-count it as "re-derived" (derive contract).
    assert!(
        out2.matched.is_empty(),
        "merge target must not pollute matched"
    );
    assert_eq!(out2.semantic_merged, vec![c1]);
    let sem = {
        let g = graph.read();
        g.edge_between(c1, c2, EdgeType::Semantic)
            .or_else(|| g.edge_between(c2, c1, EdgeType::Semantic))
            .cloned()
    }
    .expect("Semantic merge edge exists");
    assert!(
        sem.weight >= 0.85,
        "edge weight reflects the accepted score"
    );
    // No canonical duplicate: the two concepts carry distinct canonical keys.
    let (k1, k2) = {
        let g = graph.read();
        let a = match g.node(c1) {
            Some(Node::Concept(c)) => c.canonical_key.clone(),
            _ => unreachable!(),
        };
        let b = match g.node(c2) {
            Some(Node::Concept(c)) => c.canonical_key.clone(),
            _ => unreachable!(),
        };
        (a, b)
    };
    assert_ne!(k1, k2, "hybrid never creates a canonical-key duplicate");
    // Contract stamped on first embed.
    assert_eq!(graph.read().embedding().unwrap().kind, "fixture");
    graph.read().assert_invariants().unwrap();
}

/// Below-threshold ("far") content creates a fresh concept that **keeps its
/// vector** (L82-4, product decision 2026-08-14) but is **not merged**: no
/// `Semantic` edge is written. This test was
/// `far_text_creates_fresh_keyword_concept` and asserted `embedding.is_none()`
/// under the pre-L82-4 rule; the vector assertion is inverted, every other
/// assertion (no Semantic edge, Derives present, invariants) is unchanged —
/// they are the properties that carry the precision bias forward.
#[tokio::test]
async fn far_text_creates_fresh_concept_with_vector_but_no_merge() {
    let sess = "hybrid-far";
    let (graph, i1) = graph_with_interaction(sess, 1, 0, "user signs up for the platform");
    let c1 = {
        let out = derive(
            graph.clone(),
            &SpyStore::with_vector(Vec::new()),
            &RecordingEmbedder::new(),
            &contract("fixture", 1024),
            i1,
            &agent(),
            &[(NEAR_A, ConceptType::Entity)],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
            None,
        )
        .await
        .unwrap();
        out.created[0]
    };

    let mut second = interaction(2, Some(i1), 60, "physics notes on gauge theories");
    second.session_id = sid(sess);
    let i2 = second.id;
    graph.write().insert_interaction(second).unwrap();

    // FAR is well below threshold: 0.2 < 0.85 -> fresh concept, keyword-only.
    let out2 = derive(
        graph.clone(),
        &SpyStore::with_vector(vec![hit(c1, 0.2)]),
        &RecordingEmbedder::new(),
        &contract("fixture", 1024),
        i2,
        &agent(),
        &[(FAR, ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();

    assert_eq!(out2.created.len(), 1);
    assert!(out2.matched.is_empty());
    let c2 = out2.created[0];
    let g = graph.read();
    // L82-4: the below-threshold fresh concept persists the vector it
    // computed, so recall's vector leg can reach organically-derived data.
    match g.node(c2) {
        Some(Node::Concept(con)) => assert_eq!(
            con.embedding.as_ref().map(Vec::len),
            Some(1024),
            "below-threshold fresh concept persists its computed vector (L82-4)"
        ),
        _ => unreachable!(),
    }
    // ...and MAJOR-1 still holds where it counts: the refused merge writes NO
    // Semantic edge, so C2 is never pulled into C1's recall neighbourhood
    // (nor P6's physical fold) on the strength of a 0.2 similarity.
    assert!(g.edge_between(c1, c2, EdgeType::Semantic).is_none());
    assert!(g.edge_between(c2, c1, EdgeType::Semantic).is_none());
    assert!(
        g.edge_between(i2, c2, EdgeType::Derives).is_some(),
        "derives edge present"
    );
    g.assert_invariants().unwrap();
}

/// L82-4 exclusion semantic, pinned: **threshold-preserving**. Persisting a
/// fresh concept's vector must not change *when* a merge happens — the bar is
/// still `score >= semantic_match_threshold` and nothing else. Same target,
/// same session, three derives: the only variable is the candidate score.
#[tokio::test]
async fn persisted_fresh_vector_does_not_lower_the_merge_bar() {
    let sess = "hybrid-bar";
    let (graph, i1) = graph_with_interaction(sess, 1, 0, "billing retries a failed charge");

    // Derive 1 — nothing to compare against: fresh concept, vector persisted.
    let out1 = derive(
        graph.clone(),
        &SpyStore::with_vector(Vec::new()),
        &RecordingEmbedder::new(),
        &contract("fixture", 1024),
        i1,
        &agent(),
        &[("billing retry policy", ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();
    let c1 = out1.created[0];
    let has_vector = |node: NodeId| match graph.read().node(node) {
        Some(Node::Concept(c)) => c.embedding.is_some(),
        _ => unreachable!("expected a concept"),
    };
    assert!(has_vector(c1), "fresh concept persists its vector (L82-4)");

    // Derive 2 — the SAME persisted vector is now a candidate, but at 0.84,
    // one hundredth under the bar: still no merge, and the new concept keeps
    // its own vector. A stored vector is not an invitation to merge.
    let mut i2n = interaction(2, Some(i1), 60, "the ledger reconciles a payment");
    i2n.session_id = sid(sess);
    let i2 = i2n.id;
    graph.write().insert_interaction(i2n).unwrap();
    let out2 = derive(
        graph.clone(),
        &SpyStore::with_vector(vec![hit(c1, 0.84)]),
        &RecordingEmbedder::new(),
        &contract("fixture", 1024),
        i2,
        &agent(),
        &[("payment ledger", ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();
    assert!(
        out2.semantic_merged.is_empty(),
        "0.84 < 0.85 — the bar did not move because c1 has a vector"
    );
    assert!(has_vector(out2.created[0]));
    assert_eq!(
        graph
            .read()
            .edges()
            .filter(|e| e.edge_type == EdgeType::Semantic)
            .count(),
        0
    );

    // Derive 3 — same candidate, now exactly at the bar: the merge fires.
    // The threshold is the whole decision procedure, before and after L82-4.
    let mut i3n = interaction(3, Some(i2), 120, "an operator retries the charge by hand");
    i3n.session_id = sid(sess);
    let i3 = i3n.id;
    graph.write().insert_interaction(i3n).unwrap();
    let out3 = derive(
        graph.clone(),
        &SpyStore::with_vector(vec![hit(c1, SEMANTIC_MATCH_THRESHOLD_DEFAULT)]),
        &RecordingEmbedder::new(),
        &contract("fixture", 1024),
        i3,
        &agent(),
        &[("manual charge retry", ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();
    assert_eq!(out3.semantic_merged, vec![c1]);
    let g = graph.read();
    let c3 = out3.created[0];
    assert!(g
        .edge_between(c1, c3, EdgeType::Semantic)
        .or_else(|| g.edge_between(c3, c1, EdgeType::Semantic))
        .is_some());
    g.assert_invariants().unwrap();
}

/// L82-4 property 3: a vector minted during a call can never drive a merge
/// *inside* that call. Every checked vector-candidate query is issued in the
/// gather phase, before a single node is written, so a sibling concept of
/// the same derive is structurally invisible as a candidate — no
/// self-referential merging on the strength of a just-minted vector.
#[tokio::test]
async fn vectors_minted_in_this_call_cannot_merge_within_it() {
    let sess = "hybrid-same-call";
    let (graph, i1) = graph_with_interaction(sess, 1, 0, "two new ideas in one turn");
    let store = SpyStore::with_vector(Vec::new());
    let out = derive(
        graph.clone(),
        &store,
        &RecordingEmbedder::new(),
        &contract("fixture", 1024),
        i1,
        &agent(),
        &[
            (NEAR_A, ConceptType::Entity),
            (NEAR_B, ConceptType::Entity), // near A, and both are brand new
        ],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();

    assert_eq!(out.created.len(), 2);
    assert!(out.semantic_merged.is_empty());
    assert_eq!(
        store.vector_calls(),
        2,
        "one query per unmatched concept, all in the gather phase"
    );
    let g = graph.read();
    for id in &out.created {
        match g.node(*id) {
            Some(Node::Concept(c)) => {
                assert!(c.embedding.is_some(), "both fresh concepts persist vectors")
            }
            _ => unreachable!(),
        }
    }
    assert_eq!(
        g.edges()
            .filter(|e| e.edge_type == EdgeType::Semantic)
            .count(),
        0,
        "siblings of one call never merge into each other"
    );
    g.assert_invariants().unwrap();
}

#[test]
fn semantic_edge_decays() {
    assert!(EdgeType::Semantic.decays());
    // And it is Concept->Concept (the reason a merge needs a second concept).
    let mut g = Graph::new(sid("matrix"));
    let i = NodeId(Uuid::from_u64_pair(1, 99));
    let a = NodeId(Uuid::from_u64_pair(2, 1));
    let b = NodeId(Uuid::from_u64_pair(2, 2));
    let at = ts(0);
    g.insert_interaction(Interaction {
        event_time: None,
        id: i,
        session_id: sid("matrix"),
        agent_id: agent(),
        prompt_text: Some("go".into()),
        previous_id: None,
        created_at: at,
    })
    .unwrap();
    g.insert_concept(
        Concept {
            id: a,
            session_id: sid("matrix"),
            content: "x".into(),
            canonical_key: "x".into(),
            concept_type: ConceptType::Entity,
            origin_interaction: i,
            origin_agent: agent(),
            created_at: at,
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            chunk_group_id: None,
        },
        i,
    )
    .unwrap();
    g.insert_concept(
        Concept {
            id: b,
            session_id: sid("matrix"),
            content: "y".into(),
            canonical_key: "y".into(),
            concept_type: ConceptType::Entity,
            origin_interaction: i,
            origin_agent: agent(),
            created_at: at,
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            chunk_group_id: None,
        },
        i,
    )
    .unwrap();
    g.upsert_edge(Edge {
        event_time: None,
        id: NodeId::new(),
        session_id: sid("matrix"),
        source: a,
        target: b,
        edge_type: EdgeType::Semantic,
        weight: 0.9,
        reinforcements: 1,
        created_at: at,
        last_reinforced: at,
    })
    .unwrap();
    g.assert_invariants().unwrap();
}

/// P7 MAJOR-1 regression — mirrors sync
/// `derive::derive_collapses_contents_sharing_a_canonical_key` through
/// `hybrid::derive`. Without a VECTOR_SEARCH capability, every unmatched
/// concept is byte-identical to canonical derive: two distinct contents
/// that collapse onto one canonical key must yield ONE node — the second
/// content resolves `Matched` to the first's just-created node — never an
/// `insert_concept` UNIQUE (session_id, canonical_key) hard error + partial
/// write.
#[tokio::test]
async fn hybrid_collapses_contents_sharing_a_canonical_key() {
    let sess = "hybrid-collapse";
    let (graph, iid) = graph_with_interaction(sess, 1, 0, "user signs up");
    let out = derive(
        graph.clone(),
        &SpyStore::without_vector(),
        &RecordingEmbedder::new(),
        &contract("fixture", 1024),
        iid,
        &agent(),
        &[
            ("user schema", ConceptType::Entity),
            ("schema user", ConceptType::Entity),
            ("auth middleware", ConceptType::Logic),
        ],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();

    assert_eq!(out.created.len(), 2); // "user schema" + "auth middleware"
    assert_eq!(out.matched.len(), 1); // "schema user" -> the first node
    assert_eq!(out.matched[0], out.created[0]);
    let g = graph.read();
    assert_eq!(g.node_count(), 3); // interaction + 2 concepts
                                   // No self-loop from the key collision: the collapsed node is the SAME
                                   // node as the first concept, so `call_nodes` held it once.
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

/// P7 MAJOR-1 regression (merge path): two distinct contents that collapse
/// onto one canonical key BOTH hybrid-merge against the same target. The
/// first creates its node + Semantic edge; the second must collapse to that
/// node (recorded in `matched`) rather than creating a canonical-key
/// duplicate or erroring.
#[tokio::test]
async fn hybrid_collapses_shared_key_under_merge() {
    let sess = "hybrid-collapse-merge";
    // Interaction 1 seeds a pre-existing concept C ("billing") with a key
    // distinct from "schema user" so the colliding pair stays Unmatched.
    let (graph, i1) = graph_with_interaction(sess, 1, 0, "invoicing a customer");
    let c1 = {
        let out = derive(
            graph.clone(),
            &SpyStore::with_vector(Vec::new()),
            &RecordingEmbedder::new(),
            &contract("fixture", 1024),
            i1,
            &agent(),
            &[("billing flow", ConceptType::Entity)],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
            None,
        )
        .await
        .unwrap();
        out.created[0]
    };
    let mut second = interaction(2, Some(i1), 60, "designing the data model");
    second.session_id = sid(sess);
    let i2 = second.id;
    graph.write().insert_interaction(second).unwrap();

    // Both colliding contents embed, and the store returns C1 at 0.9 for
    // each — so both resolve HybridMerge { targets: vec![c1] }.
    let out = derive(
        graph.clone(),
        &SpyStore::with_vector(vec![hit(c1, 0.9)]),
        &RecordingEmbedder::new(),
        &contract("fixture", 1024),
        i2,
        &agent(),
        &[
            ("user schema", ConceptType::Entity),
            ("schema user", ConceptType::Entity),
        ],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();

    // One created node; the second content collapses to it (matched),
    // rather than a canonical-key duplicate or an insert error.
    assert_eq!(out.created.len(), 1);
    assert_eq!(out.matched, vec![out.created[0]]);
    assert_eq!(out.semantic_merged, vec![c1]);
    let g = graph.read();
    let n1 = out.created[0];
    assert_eq!(g.node_count(), 4); // i1, i2, c1, n1
                                   // Exactly one Semantic merge edge (c1 <-> n1, direction by UUID order)
                                   // — the collapse wrote no second edge and no duplicate node.
    assert!(g
        .edge_between(c1, n1, EdgeType::Semantic)
        .or_else(|| g.edge_between(n1, c1, EdgeType::Semantic))
        .is_some());
    assert_eq!(
        g.edges()
            .filter(|e| e.edge_type == EdgeType::Semantic)
            .count(),
        1
    );
    g.assert_invariants().unwrap();
}

/// P7 MINOR-2: when the store returns a candidate that is NOT a Concept
/// (a bogus/refused merge target), the concept must degrade to a TRUE
/// keyword-only node — `embedding: None`, no Semantic edge — never persist
/// a vector for a merge it refused to make.
#[tokio::test]
async fn hybrid_refused_merge_target_is_keyword_only() {
    let sess = "hybrid-refused";
    let (graph, iid) = graph_with_interaction(sess, 1, 0, "auth flow");
    // The store hands back a hit pointing at the INTERACTION node (not a
    // Concept) at/above threshold — the merge must be refused.
    let out = derive(
        graph.clone(),
        &SpyStore::with_vector(vec![hit(iid, 0.9)]),
        &RecordingEmbedder::new(),
        &contract("fixture", 1024),
        iid,
        &agent(),
        &[("create account", ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();

    assert_eq!(out.created.len(), 1);
    assert!(out.matched.is_empty());
    assert!(out.semantic_merged.is_empty());
    let c = out.created[0];
    let g = graph.read();
    match g.node(c) {
        Some(Node::Concept(con)) => assert!(
            con.embedding.is_none(),
            "refused-merge concept must be keyword-only (embedding: None)"
        ),
        _ => unreachable!(),
    }
    assert!(g.edge_between(iid, c, EdgeType::Semantic).is_none());
    assert!(g.edge_between(c, iid, EdgeType::Semantic).is_none());
    assert!(g.edge_between(iid, c, EdgeType::Derives).is_some());
    g.assert_invariants().unwrap();
}

/// A concept with a FIXED id and key, inserted straight into the graph so a
/// test can pit id order against key order for the merge pick.
fn preseeded_concept(sess: &str, id: Uuid, key: &str, owner: NodeId) -> Concept {
    Concept {
        id: NodeId(id),
        session_id: sid(sess),
        content: key.to_string(),
        canonical_key: key.to_string(),
        concept_type: ConceptType::Entity,
        origin_interaction: owner,
        origin_agent: agent(),
        created_at: ts(0),
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        canonization_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding: None,
        human_confirmed: 0,
        chunk_group_id: None,
    }
}

/// Issue-2 remediation round 1: candidates tied at the top score must
/// resolve the merge target stably — smallest canonical key, then id — not
/// by the run-minted UUID the gather phase's pick used to fall back to.
/// The ids here order OPPOSITE to the keys, so the old reversed-UUID pick
/// merges into `beta`; the stable pick must land on `alpha`.
#[tokio::test]
async fn tied_merge_candidates_pick_smallest_canonical_key() {
    let sess = "hybrid-tie-key";
    let (graph, iid) = graph_with_interaction(sess, 1, 0, "billing questions");
    let alpha = preseeded_concept(sess, Uuid::from_u64_pair(9, 2), "alpha api", iid);
    let beta = preseeded_concept(sess, Uuid::from_u64_pair(9, 1), "beta api", iid);
    let (alpha_id, beta_id) = (alpha.id, beta.id);
    {
        let mut g = graph.write();
        g.insert_concept(alpha, iid).unwrap();
        g.insert_concept(beta, iid).unwrap();
    }

    let out = derive(
        graph.clone(),
        // Both candidates tied at 0.9, in non-key order — the tier, not
        // the store's ordering, decides.
        &SpyStore::with_vector(vec![hit(beta_id, 0.9), hit(alpha_id, 0.9)]),
        &RecordingEmbedder::new(),
        &contract("fixture", 1024),
        iid,
        &agent(),
        &[("gamma queries", ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();

    assert_eq!(out.created.len(), 1);
    assert_eq!(
        out.semantic_merged,
        vec![alpha_id],
        "an exact score tie must resolve on the canonical key, not the UUID"
    );
    let g = graph.read();
    let n1 = out.created[0];
    assert!(
        g.edge_between(alpha_id, n1, EdgeType::Semantic).is_some()
            || g.edge_between(n1, alpha_id, EdgeType::Semantic).is_some()
    );
    g.assert_invariants().unwrap();
}

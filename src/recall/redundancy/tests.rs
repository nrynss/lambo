//! #181's predicates: the normalization, the text guard, the vetoes and
//! each redundancy signal on its own.

use chrono::{TimeZone, Utc};
use uuid::Uuid;

use super::*;
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Edge, EmbeddingContract, Interaction,
    SessionId,
};

fn sid() -> SessionId {
    SessionId::from("redundancy")
}

fn id(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(5, n))
}

fn origin() -> NodeId {
    NodeId(Uuid::from_u64_pair(4, 1))
}

fn concept(n: u64, content: &str, ty: ConceptType, embedding: Option<Vec<f32>>) -> Concept {
    Concept {
        id: id(n),
        session_id: sid(),
        content: content.into(),
        canonical_key: format!("key {n}"),
        concept_type: ty,
        origin_interaction: origin(),
        origin_agent: AgentId::from("agent-a"),
        created_at: Utc.timestamp_opt(1_760_000_000, 0).unwrap(),
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        canonization_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding,
        human_confirmed: 0,
        embedding_source: None,
        chunk_group_id: None,
    }
}

/// A graph with a 2-d contract and the given concepts.
fn graph(concepts: &[Concept]) -> Graph {
    let mut g = Graph::new(sid());
    g.stamp_embedding(EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: 2,
    })
    .unwrap();
    g.insert_interaction(Interaction {
        event_time: None,
        id: origin(),
        session_id: sid(),
        agent_id: AgentId::from("agent-a"),
        prompt_text: None,
        previous_id: None,
        created_at: Utc.timestamp_opt(1_760_000_000, 0).unwrap(),
    })
    .unwrap();
    for c in concepts {
        g.insert_concept(c.clone(), origin()).unwrap();
    }
    g
}

fn edge(n: u64, source: NodeId, target: NodeId, edge_type: EdgeType) -> Edge {
    let at = Utc.timestamp_opt(1_760_000_100, 0).unwrap();
    Edge {
        event_time: None,
        id: NodeId(Uuid::from_u64_pair(6, n)),
        session_id: sid(),
        source,
        target,
        edge_type,
        weight: 1.0,
        reinforcements: 1,
        created_at: at,
        last_reinforced: at,
    }
}

/// The signal under `[selection] skip_redundant = true, similar_signals =
/// true` at `threshold`: every signal counts.
fn signal(g: &Graph, threshold: f64, a: u64, b: u64) -> Option<RedundancySignal> {
    signal_under(
        g,
        Redundancy {
            threshold,
            similar: true,
        },
        a,
        b,
    )
}

/// The signal under the shipped default (`skip_redundant = true` alone):
/// identical content only, whatever the threshold.
fn default_signal(g: &Graph, threshold: f64, a: u64, b: u64) -> Option<RedundancySignal> {
    let selection = RecallSelection {
        skip_redundant: true,
        ..RecallSelection::default()
    };
    signal_under(g, Redundancy::of(&selection, threshold).unwrap(), a, b)
}

fn signal_under(g: &Graph, r: Redundancy, a: u64, b: u64) -> Option<RedundancySignal> {
    let concept = |n: u64| match g.node(id(n)) {
        Some(crate::types::Node::Concept(c)) => c,
        _ => panic!("concept {n}"),
    };
    let (a, b) = (concept(a), concept(b));
    let (an, bn) = (
        normalized_content(&a.content),
        normalized_content(&b.content),
    );
    r.signal(g, (a, &an), (b, &bn))
}

#[test]
fn the_selection_defaults_to_identical_content_only() {
    assert_eq!(
        RecallSelection::default(),
        RecallSelection {
            skip_redundant: false,
            similar_signals: false
        }
    );
    assert_eq!(Redundancy::of(&RecallSelection::default(), 0.85), None);
    let on = RecallSelection {
        skip_redundant: true,
        ..RecallSelection::default()
    };
    assert_eq!(
        Redundancy::of(&on, 0.85),
        Some(Redundancy {
            threshold: 0.85,
            similar: false
        })
    );
    assert!(on.validate().is_ok());
    assert!(RecallSelection {
        skip_redundant: false,
        similar_signals: true
    }
    .validate()
    .is_err());
}

/// The shipped default does not skip a pair joined only by a `Semantic`
/// edge, or only by cosine, however low the threshold; identical content
/// still is.
#[test]
fn the_default_ignores_a_semantic_edge_only_or_cosine_only_pair() {
    let mut g = graph(&[
        concept(
            1,
            "lease ttl is long",
            ConceptType::Logic,
            Some(vec![1.0, 0.0]),
        ),
        concept(2, "the lease lasts long", ConceptType::Logic, None),
        concept(
            3,
            "leases last long",
            ConceptType::Logic,
            Some(vec![1.0, 0.0]),
        ),
        concept(4, "lease ttl is long", ConceptType::Entity, None),
    ]);
    g.upsert_edge(edge(1, id(2), id(1), EdgeType::Semantic))
        .unwrap();
    assert_eq!(signal(&g, 0.85, 1, 2), Some(RedundancySignal::SemanticEdge));
    assert_eq!(default_signal(&g, 0.85, 1, 2), None, "edge only");
    assert_eq!(signal(&g, -1.0, 1, 3), Some(RedundancySignal::Cosine));
    assert_eq!(default_signal(&g, -1.0, 1, 3), None, "cosine only");
    assert_eq!(
        default_signal(&g, 0.85, 1, 4),
        Some(RedundancySignal::SameContent)
    );
}

#[test]
fn the_window_is_four_per_slot_up_to_twenty() {
    assert_eq!(window(0), 0);
    assert_eq!(window(1), 4);
    assert_eq!(window(3), 12);
    assert_eq!(window(5), 20);
    assert_eq!(window(6), 20);
    assert_eq!(window(usize::MAX), WINDOW_MAX);
}

#[test]
fn normalization_folds_whitespace_composition_and_invisibles_only() {
    assert_eq!(
        normalized_content("  The  Dogfood\tstore\n holds 92 of 100 "),
        "The Dogfood store holds 92 of 100"
    );
    // Case is kept: a path or a variable that differs only in case is
    // another one.
    for (a, b) in [
        ("see src/Foo.rs", "see src/foo.rs"),
        ("PATH is read at startup", "path is read at startup"),
    ] {
        assert_ne!(
            normalized_content(a),
            normalized_content(b),
            "{a:?} / {b:?}"
        );
    }
    // NFC: a decomposed accent equals the composed one.
    assert_eq!(
        normalized_content("cafe\u{301}"),
        normalized_content("café")
    );
    // An invisible codepoint cannot make two identical renderings differ.
    assert_eq!(
        normalized_content("billing\u{200B} retries"),
        normalized_content("billing retries")
    );
    // Nothing that carries meaning is erased.
    for (a, b) in [
        ("release v0.5.0 is current", "release v0.6.0 is current"),
        ("the cap is 280", "the cap is 281"),
        ("see src/main.rs", "see src/mein.rs"),
        ("use the candle embedder", "do not use the candle embedder"),
        ("X replaces Y", "Y replaces X"),
    ] {
        assert_ne!(
            normalized_content(a),
            normalized_content(b),
            "{a:?} / {b:?}"
        );
    }
    // Strict by design: punctuation is kept too, so a near-copy is left to
    // the edge and cosine signals rather than the same-content one.
    assert_ne!(
        normalized_content("the leases expire."),
        normalized_content("the leases expire")
    );
}

#[test]
fn negations_are_counted_by_word_and_contraction() {
    assert_eq!(negation_count("use the candle embedder"), 0);
    assert_eq!(negation_count("do not use the candle embedder"), 1);
    assert_eq!(negation_count("Don't use it; never retry, no cap"), 3);
    assert_eq!(negation_count("it isn\u{2019}t cached"), 1);
    assert_eq!(negation_count("nothing is cached without a lease"), 2);
    // Words that merely start like one do not count.
    assert_eq!(negation_count("note the notation: nodes are nominal"), 0);
    // U+02BC, `avoid`, `non-` and `unless`.
    assert_eq!(negation_count("it isn\u{2bc}t cached"), 1);
    assert_eq!(
        negation_count("Avoid retries; use non-blocking reads unless told"),
        3
    );
    assert_eq!(negation_count("nonce and union are words"), 0);
    // An invisible codepoint inside a negator does not hide it.
    assert_eq!(negation_count("do n\u{200B}ot retry"), 1);
}

#[test]
fn distinct_claims_are_different_identifiers_digits_or_negation() {
    assert!(distinct_claims(
        "release v0.5.0 is current",
        "release v0.6.0 is current"
    ));
    assert!(distinct_claims("the cap is 280", "the cap is 281"));
    assert!(distinct_claims("see src/main.rs", "see src/mein.rs"));
    assert!(distinct_claims(
        "use the candle embedder",
        "do not use the candle embedder"
    ));
    // A paraphrase with the same identifiers, polarity and order is not.
    assert!(!distinct_claims(
        "the dogfood embedder is BAAI/bge-m3 on Metal",
        "the dogfood embedder runs BAAI/bge-m3 on Metal now"
    ));
    assert!(!distinct_claims("never retry", "do not retry"));
    // Conservative by design: a paraphrase that moves a phrase reads as a
    // different claim, so both are kept.
    assert!(distinct_claims(
        "the dogfood embedder is BAAI/bge-m3 on Metal",
        "on Metal, the dogfood embedder runs BAAI/bge-m3"
    ));
    // Identical texts never are, whatever they hold.
    for text in [
        "UserSchema replaces OrderSchema in store_sqlite, not deadbeef",
        "enable the unsafe path unless PATH is set: port \u{ff18}\u{ff10}",
        "",
    ] {
        assert!(!distinct_claims(text, text), "{text:?}");
    }
}

/// The #181 review's probes: each pair is a different claim that the
/// write path's `same_identifiers` and a plain negation count passed.
#[test]
fn the_review_probes_are_distinct_claims() {
    for (a, b) in [
        // snake_case and CamelCase identifiers.
        (
            "store_sqlite owns the vector leg",
            "store_postgres owns the vector leg",
        ),
        (
            "UserSchema validates the request body",
            "OrderSchema validates the request body",
        ),
        // Hashes without a digit.
        ("the fix landed in deadbeef", "the fix landed in cafebabe"),
        // Swapped roles.
        ("warn replaces verify", "verify replaces warn"),
        // A negation that moved.
        ("Use A, not B", "Do not use A"),
        ("use candle, not llama", "do not use candle"),
        // Antonyms, listed and by prefix.
        ("enable the canary", "disable the canary"),
        ("the cache is safe", "the cache is unsafe"),
        ("allow writes from layers", "deny writes from layers"),
        ("the input is valid", "the input is invalid"),
        // Full-width digits.
        (
            "the port is \u{ff18}\u{ff10}",
            "the port is \u{ff18}\u{ff11}",
        ),
        // Negators the old count missed: U+02BC, avoid, non-, unless.
        ("it isn\u{2bc}t cached", "it is cached"),
        ("avoid caching the index", "caching the index"),
        ("non-blocking reads", "blocking reads"),
        (
            "retry unless the lease expired",
            "retry if the lease expired",
        ),
        // Case in an identifier.
        ("see src/Foo.rs", "see src/foo.rs"),
        ("PATH is read at startup", "path is read at startup"),
    ] {
        assert!(
            crate::graph::canonical::same_identifiers(a, b),
            "premise: the write-path guard does not tell {a:?} from {b:?}"
        );
        assert!(distinct_claims(a, b), "{a:?} / {b:?}");
        assert!(distinct_claims(b, a), "{b:?} / {a:?}");
    }
}

#[test]
fn claim_identifiers_cover_snake_camel_hex_caps_and_unicode_digits() {
    assert_eq!(
        claim_identifiers(
            "(UserSchema) uses store_sqlite, deadbeef, PATH, src/Foo.rs and port \u{ff18}\u{ff10}."
        ),
        [
            "UserSchema",
            "store_sqlite",
            "deadbeef",
            "PATH",
            "src/Foo.rs",
            "\u{ff18}\u{ff10}"
        ]
    );
    // Plain words, a capitalised word and a short hex-letter word are not.
    assert!(claim_identifiers("Use the decaf cache, then retry").is_empty());
    // Invisible codepoints are ignored.
    assert_eq!(
        claim_identifiers("store\u{200B}_sqlite"),
        claim_identifiers("store_sqlite")
    );
}

#[test]
fn identical_content_across_types_is_the_same_content_signal() {
    let text = "Dogfood finding: 92 of 100 concepts carry no durable embedding";
    let g = graph(&[
        concept(1, text, ConceptType::Entity, None),
        concept(
            2,
            &format!("  {}\n", text.replace(' ', " \t")),
            ConceptType::Observation,
            None,
        ),
    ]);
    assert_eq!(signal(&g, 0.85, 1, 2), Some(RedundancySignal::SameContent));
    assert_eq!(signal(&g, 0.85, 2, 1), Some(RedundancySignal::SameContent));
    assert_eq!(
        signal(&g, 0.85, 1, 1),
        None,
        "a member is not its own duplicate"
    );
}

/// Texts that differ only in case are not the same content, under either
/// configuration; byte-identical copies (the 2026-09-07 efficacy run's
/// Entity and Observation pairs) still are.
#[test]
fn texts_differing_only_in_case_are_not_the_same_content() {
    let g = graph(&[
        concept(1, "see src/Foo.rs", ConceptType::Logic, None),
        concept(2, "see src/foo.rs", ConceptType::Observation, None),
        concept(3, "PATH is read at startup", ConceptType::Logic, None),
        concept(4, "path is read at startup", ConceptType::Entity, None),
        concept(5, EFFICACY_COPY, ConceptType::Entity, None),
        concept(6, EFFICACY_COPY, ConceptType::Observation, None),
    ]);
    for (a, b) in [(1, 2), (3, 4)] {
        assert_eq!(default_signal(&g, 0.85, a, b), None);
        assert_eq!(signal(&g, 0.85, a, b), None);
    }
    assert_eq!(EFFICACY_COPY.len(), 374, "the published length");
    assert_eq!(
        default_signal(&g, 0.85, 5, 6),
        Some(RedundancySignal::SameContent)
    );
}

/// The 2026-09-07 `missing-downloads` duplicate (ranks 2 and 3, an Entity
/// and an Observation), as both copies stored it (lambo-paper,
/// `evaluation/results/2026-09-07/vimanam-2026-09-07/exports/lambo/
/// missing-downloads.full.json`).
const EFFICACY_COPY: &str = "Event-time MCP round-1 review verdict: REQUEST CHANGES. Optional \
    RFC3339 event_time is correctly deserialized into DateTime<Utc>, threaded through \
    derive_async_as and Action.event_time, preserved through the J2 byte proxy, stamped onto \
    interactions and action edges, and restores Solo separated-session evidence; focused tests \
    plus cargo test --all --features fixtures pass.";

/// Under `similar_signals = true`.
#[test]
fn a_semantic_edge_either_way_is_the_edge_signal() {
    let mut g = graph(&[
        concept(1, "lease ttl is 45 seconds", ConceptType::Logic, None),
        concept(2, "the lease lasts 45 seconds", ConceptType::Logic, None),
        concept(
            3,
            "the lease lasts 45 seconds, renewed",
            ConceptType::Logic,
            None,
        ),
    ]);
    assert_eq!(signal(&g, 0.85, 1, 2), None);
    g.upsert_edge(edge(1, id(2), id(1), EdgeType::Semantic))
        .unwrap();
    assert_eq!(signal(&g, 0.85, 1, 2), Some(RedundancySignal::SemanticEdge));
    assert_eq!(signal(&g, 0.85, 2, 1), Some(RedundancySignal::SemanticEdge));
    assert_eq!(signal(&g, 0.85, 1, 3), None, "no edge, no vector");
}

/// Under `similar_signals = true`.
#[test]
fn cosine_needs_two_contract_vectors_and_the_threshold() {
    let g = graph(&[
        concept(1, "alpha one", ConceptType::Logic, Some(vec![1.0, 0.0])),
        concept(2, "alpha two", ConceptType::Logic, Some(vec![0.99, 0.141])),
        concept(3, "alpha three", ConceptType::Logic, None),
        concept(4, "alpha four", ConceptType::Logic, Some(vec![0.0, 1.0])),
    ]);
    // cos(1, 2) is about 0.990.
    assert_eq!(signal(&g, 0.95, 1, 2), Some(RedundancySignal::Cosine));
    assert_eq!(signal(&g, 0.995, 1, 2), None, "below the threshold");
    assert_eq!(
        signal(&g, 0.0, 1, 3),
        None,
        "a member without a vector gives no evidence"
    );
    assert_eq!(signal(&g, 0.0, 3, 1), None);
    assert_eq!(signal(&g, 0.5, 1, 4), None);
}

/// Under `similar_signals = true`.
#[test]
fn a_session_without_a_contract_gives_no_cosine_evidence() {
    let mut g = Graph::new(sid());
    g.insert_interaction(Interaction {
        event_time: None,
        id: origin(),
        session_id: sid(),
        agent_id: AgentId::from("agent-a"),
        prompt_text: None,
        previous_id: None,
        created_at: Utc.timestamp_opt(1_760_000_000, 0).unwrap(),
    })
    .unwrap();
    g.insert_concept(concept(1, "alpha one", ConceptType::Logic, None), origin())
        .unwrap();
    g.insert_concept(concept(2, "alpha two", ConceptType::Logic, None), origin())
        .unwrap();
    assert_eq!(signal(&g, 0.0, 1, 2), None);
}

/// Under both configurations: the default (identical content only) and
/// `similar_signals = true` (every signal).
#[test]
fn a_contradicts_edge_either_way_vetoes_every_signal() {
    let text = "the dogfood embedder is candle BGE-M3";
    for (source, target) in [(1, 2), (2, 1)] {
        let mut g = graph(&[
            concept(1, text, ConceptType::Logic, Some(vec![1.0, 0.0])),
            concept(2, text, ConceptType::Observation, Some(vec![1.0, 0.0])),
        ]);
        g.upsert_edge(edge(1, id(1), id(2), EdgeType::Semantic))
            .unwrap();
        assert_eq!(signal(&g, 0.5, 1, 2), Some(RedundancySignal::SameContent));
        assert_eq!(
            default_signal(&g, 0.5, 1, 2),
            Some(RedundancySignal::SameContent)
        );
        g.upsert_edge(edge(2, id(source), id(target), EdgeType::Contradicts))
            .unwrap();
        assert!(never_redundant(&g, id(1), id(2)));
        assert!(never_redundant(&g, id(2), id(1)));
        assert_eq!(signal(&g, 0.5, 1, 2), None);
        assert_eq!(signal(&g, 0.5, 2, 1), None);
        assert_eq!(default_signal(&g, 0.5, 1, 2), None);
    }
}

/// #198: a `SuspectedSupersedes` edge either way vetoes every signal,
/// under both configurations, like `Contradicts`.
#[test]
fn a_suspected_supersedes_edge_either_way_vetoes_every_signal() {
    let text = "the default recall fusion is max";
    for (source, target) in [(1, 2), (2, 1)] {
        let mut g = graph(&[
            concept(1, text, ConceptType::Logic, Some(vec![1.0, 0.0])),
            concept(2, text, ConceptType::Logic, Some(vec![1.0, 0.0])),
        ]);
        g.upsert_edge(edge(1, id(1), id(2), EdgeType::Semantic))
            .unwrap();
        assert_eq!(
            default_signal(&g, 0.5, 1, 2),
            Some(RedundancySignal::SameContent)
        );
        g.upsert_edge(edge(
            2,
            id(source),
            id(target),
            EdgeType::SuspectedSupersedes,
        ))
        .unwrap();
        assert!(never_redundant(&g, id(1), id(2)));
        assert!(never_redundant(&g, id(2), id(1)));
        assert_eq!(signal(&g, 0.5, 1, 2), None);
        assert_eq!(signal(&g, 0.5, 2, 1), None);
        assert_eq!(default_signal(&g, 0.5, 1, 2), None);
    }
}

/// Under `similar_signals = true`, the only configuration in which the
/// edge and cosine signals fire.
#[test]
fn different_identifiers_digits_or_negation_veto_the_edge_and_cosine_signals() {
    let v = Some(vec![1.0, 0.0]);
    for (n, (a, b)) in [
        ("release v0.5.0 is current", "release v0.6.0 is current"),
        ("the gc cap is 280 concepts", "the gc cap is 281 concepts"),
        (
            "the config is lambo.toml",
            "the config is lambo.example.toml",
        ),
        ("use the candle embedder", "do not use the candle embedder"),
    ]
    .into_iter()
    .enumerate()
    {
        let n = n as u64 * 2;
        let mut g = graph(&[
            concept(n + 1, a, ConceptType::Logic, v.clone()),
            concept(n + 2, b, ConceptType::Logic, v.clone()),
        ]);
        assert_eq!(signal(&g, 0.5, n + 1, n + 2), None, "cosine: {a:?} / {b:?}");
        g.upsert_edge(edge(n, id(n + 1), id(n + 2), EdgeType::Semantic))
            .unwrap();
        assert_eq!(signal(&g, 0.5, n + 1, n + 2), None, "edge: {a:?} / {b:?}");
    }
}

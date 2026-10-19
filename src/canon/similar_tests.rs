//! Tests for the near-duplicate suggestions (#178).

use std::collections::{BTreeSet, HashSet};

use super::similar::*;
use crate::graph::canonical::canonical_key;
use crate::graph::Graph;
use crate::test_util::fair_test as fair;
use crate::types::{CanonizationStatus, Concept, ConceptType, Edge, EdgeType, GraphSnapshot, NodeId};

/// One concept to put in a test graph.
struct Spec<'a> {
    name: &'a str,
    t: ConceptType,
    content: &'a str,
    status: CanonizationStatus,
    embedding: Option<Vec<f32>>,
}

fn spec<'a>(name: &'a str, t: ConceptType, content: &'a str) -> Spec<'a> {
    Spec {
        name,
        t,
        content,
        status: CanonizationStatus::None,
        embedding: None,
    }
}

fn nid(name: &str) -> NodeId {
    fair::hashed_id(&format!("similar/{name}"))
}

/// The fair-test fixture's first interaction and nothing else, plus `specs`
/// (each derived from that interaction) and `extra` edges.
fn snapshot(specs: &[Spec<'_>], extra: &[(&str, &str, EdgeType)]) -> GraphSnapshot {
    let mut snap = fair::snapshot(false);
    let template = snap.concepts[0].clone();
    let edge = snap.edges[0].clone();
    let origin = snap.interactions[0].clone();
    snap.interactions = vec![origin.clone()];
    snap.concepts.clear();
    snap.edges.clear();
    snap.synonyms.clear();
    snap.reservations.clear();
    snap.canonization_events.clear();
    for (i, s) in specs.iter().enumerate() {
        let mut c: Concept = template.clone();
        c.id = nid(s.name);
        c.content = s.content.into();
        c.canonical_key = canonical_key(s.content, |_| None);
        c.concept_type = s.t;
        c.canonization_status = s.status;
        c.embedding = s.embedding.clone();
        c.origin_interaction = origin.id;
        c.created_at = template.created_at + chrono::Duration::seconds(i as i64);
        let mut d: Edge = edge.clone();
        d.id = fair::hashed_id(&format!("similar-derives/{}", s.name));
        d.source = origin.id;
        d.target = c.id;
        d.edge_type = EdgeType::Derives;
        snap.edges.push(d);
        snap.concepts.push(c);
    }
    if let Some(dim) = specs.iter().find_map(|s| s.embedding.as_ref().map(Vec::len)) {
        snap.embedding = Some(crate::types::EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim,
        });
    }
    for (i, (a, b, t)) in extra.iter().enumerate() {
        let mut e: Edge = edge.clone();
        e.id = fair::hashed_id(&format!("similar-edge/{i}"));
        e.source = nid(a);
        e.target = nid(b);
        e.edge_type = *t;
        snap.edges.push(e);
    }
    snap
}

fn graph(specs: &[Spec<'_>], extra: &[(&str, &str, EdgeType)]) -> Graph {
    Graph::from_snapshot(snapshot(specs, extra)).expect("valid snapshot")
}

fn text_only() -> SimilarParams {
    SimilarParams {
        cosine: None,
        ..SimilarParams::default()
    }
}

/// The suggested pairs, ignoring direction.
fn unordered(r: &SimilarReport) -> BTreeSet<(uuid::Uuid, uuid::Uuid)> {
    r.pairs
        .iter()
        .map(|p| (p.alias.0.min(p.ruling.0), p.alias.0.max(p.ruling.0)))
        .collect()
}

fn both(a: &str, b: &str) -> (uuid::Uuid, uuid::Uuid) {
    let (a, b) = (nid(a).0, nid(b).0);
    (a.min(b), a.max(b))
}

fn pairs(r: &SimilarReport) -> HashSet<(NodeId, NodeId)> {
    r.pairs.iter().map(|p| (p.alias, p.ruling)).collect()
}

const E: ConceptType = ConceptType::Entity;

#[test]
fn fold_lowercases_and_collapses_separators() {
    assert_eq!(fold("Write_Queue-receipt"), "write queue receipt");
    assert_eq!(fold("  write \t queue__receipt  "), "write queue receipt");
    assert_eq!(fold("v0.5.0"), "v0.5.0");
    assert_eq!(fold(""), "");
}

#[test]
fn spelling_variants_are_suggested() {
    let g = graph(
        &[
            spec("a", E, "the writer lease renews every tick"),
            spec("b", E, "the writer lease renews evry tick"),
            spec("c", E, "write_queue_receipt is durable"),
            spec("d", E, "write queue reciept is durable"),
            spec("far", E, "an unrelated sentence about recall"),
        ],
        &[],
    );
    let r = detect_similar(&g, &text_only());
    let want: BTreeSet<_> = [both("a", "b"), both("c", "d")].into_iter().collect();
    assert_eq!(unordered(&r), want, "{r:#?}");
    for p in &r.pairs {
        assert!(p.jaccard >= DEFAULT_JACCARD);
        assert_eq!(p.source, SimilarSource::Lexical);
    }
}

/// Every #148 / #178 identifier family, as near-identical texts and with
/// identical vectors: never suggested by either source.
pub(crate) const IDENTIFIER_PAIRS: &[(&str, &str)] = &[
    ("cut the release v0.5.0 today", "cut the release v0.6.0 today"),
    ("the server follows rfc 7231", "the server follows rfc 7230"),
    ("checksums are sha256 digests", "checksums are sha512 digests"),
    ("route traffic to host1.svc first", "route traffic to host2.svc first"),
    ("tracked as issue #148 upstream", "tracked as issue #146 upstream"),
    ("the daemon listens on port 7700", "the daemon listens on port 7701"),
    ("served by embedding model v1", "served by embedding model v2"),
    ("the build needs node 18 installed", "the build needs node 20 installed"),
    ("commit 2122de39e47e fixed the bug", "commit 2122de39e48e fixed the bug"),
    ("the entry point is src/main.rs", "the entry point is src/mein.rs"),
];

#[test]
fn identifier_pairs_are_never_suggested_by_text_or_vector() {
    let names: Vec<(String, String)> = (0..IDENTIFIER_PAIRS.len())
        .map(|i| (format!("x{i}"), format!("y{i}")))
        .collect();
    let mut specs = Vec::new();
    for (i, (a, b)) in IDENTIFIER_PAIRS.iter().enumerate() {
        // The same vector for both: cosine 1.
        let v: Vec<f32> = (0..16).map(|k| if k == i { 1.0 } else { 0.1 }).collect();
        for (n, t) in [(&names[i].0, a), (&names[i].1, b)] {
            let mut s = spec(n, E, t);
            s.embedding = Some(v.clone());
            specs.push(s);
        }
    }
    let g = graph(&specs, &[]);
    let r = detect_similar(
        &g,
        &SimilarParams {
            jaccard: 0.5,
            ..SimilarParams::default()
        },
    );
    // Pairs within a family are all guarded. (Different families have
    // different vectors and little text in common.)
    assert!(r.pairs.is_empty(), "{:#?}", r.pairs);
    assert!(r.guarded.digits_differ + r.guarded.identifiers_differ >= IDENTIFIER_PAIRS.len());
}

#[test]
fn types_must_match_unless_the_text_is_the_same() {
    let g = graph(
        &[
            spec("ent", E, "the writer lease renews every tick"),
            spec("logic", ConceptType::Logic, "the writer lease renews evry tick"),
            spec("obs", ConceptType::Observation, "The writer lease renews every tick"),
        ],
        &[],
    );
    let r = detect_similar(&g, &text_only());
    let want: BTreeSet<_> = [both("ent", "obs")].into_iter().collect();
    assert_eq!(unordered(&r), want, "{r:#?}");
    assert_eq!(r.guarded.type_differs, 2);
}

#[test]
fn a_contradicts_edge_keeps_a_pair_apart() {
    let specs = [
        spec("a", E, "the writer lease renews every tick"),
        spec("b", E, "the writer lease renews evry tick"),
    ];
    for (s, t) in [("a", "b"), ("b", "a")] {
        let g = graph(&specs, &[(s, t, EdgeType::Contradicts)]);
        let r = detect_similar(&g, &text_only());
        assert!(r.pairs.is_empty());
        assert_eq!(r.guarded.contradicts, 1);
    }
}

#[test]
fn canonical_concepts_need_the_operator_flag() {
    let mut a = spec("a", E, "the writer lease renews every tick");
    a.status = CanonizationStatus::Canonical;
    let g = graph(&[a, spec("b", E, "the writer lease renews evry tick")], &[]);
    let r = detect_similar(&g, &text_only());
    assert!(r.pairs.is_empty());
    assert_eq!(r.guarded.canonical, 1);
    let r = detect_similar(
        &g,
        &SimilarParams {
            include_canonical: true,
            ..text_only()
        },
    );
    // The Canonical concept rules (higher status, no citations either way).
    assert_eq!(pairs(&r), [(nid("b"), nid("a"))].into_iter().collect());
}

#[test]
fn short_texts_are_left_to_the_key() {
    let g = graph(&[spec("a", E, "lease tick"), spec("b", E, "lease tikc")], &[]);
    let r = detect_similar(&g, &text_only());
    assert!(r.pairs.is_empty());
    assert_eq!(r.guarded.too_short, 2);
}

#[test]
fn the_more_cited_concept_rules() {
    let specs = [
        spec("a", E, "the writer lease renews every tick"),
        spec("b", E, "the writer lease renews evry tick"),
        spec("c1", E, "a citer of the newer one"),
        spec("c2", E, "another citer of the newer one"),
    ];
    let g = graph(
        &specs,
        &[
            ("c1", "b", EdgeType::Dependency),
            ("c2", "b", EdgeType::Dependency),
        ],
    );
    let r = detect_similar(&g, &text_only());
    let p = &r.pairs[0];
    assert_eq!((p.alias, p.ruling), (nid("a"), nid("b")));
    assert_eq!((p.alias_citations, p.ruling_citations), (0, 2));
    // Uncited: the older one rules.
    let g = graph(&specs, &[]);
    let p = &detect_similar(&g, &text_only()).pairs[0];
    assert_eq!((p.alias, p.ruling), (nid("b"), nid("a")));
}

#[test]
fn three_variants_make_a_star_not_a_chain() {
    let g = graph(
        &[
            spec("a", E, "the writer lease renews every tick"),
            spec("b", E, "the writer lease renews evry tick"),
            spec("c", E, "the writer laese renews every tick"),
        ],
        &[],
    );
    let r = detect_similar(&g, &text_only());
    let aliases: HashSet<NodeId> = r.pairs.iter().map(|p| p.alias).collect();
    let rulings: HashSet<NodeId> = r.pairs.iter().map(|p| p.ruling).collect();
    assert_eq!(aliases.len(), r.pairs.len(), "an alias twice: {r:#?}");
    assert!(aliases.is_disjoint(&rulings), "a chain: {r:#?}");
    assert!(!r.pairs.is_empty());
}

#[test]
fn overrides_are_not_suggested_again() {
    let specs = [
        spec("a", E, "the writer lease renews every tick"),
        spec("b", E, "the writer lease renews evry tick"),
    ];
    let g = graph(&specs, &[]);
    let rejected = SimilarParams {
        rejected: [(nid("a"), nid("b"))].into_iter().collect(),
        ..text_only()
    };
    let r = detect_similar(&g, &rejected);
    assert!(r.pairs.is_empty());
    assert_eq!(r.guarded.rejected, 1);
    let accepted = SimilarParams {
        accepted: vec![(nid("a"), nid("b"))],
        ..text_only()
    };
    let r = detect_similar(&g, &accepted);
    assert!(r.pairs.is_empty());
    assert_eq!(r.guarded.accepted, 1);
}

#[test]
fn split_overrides_keeps_line_numbers_and_refuses_other_verbs() {
    let (a, r) = split_overrides("# decisions\naccept aaaa bbbb\nreject cccc dddd # no\n\n").unwrap();
    assert_eq!(a, "\naaaa bbbb\n\n\n");
    assert_eq!(r, "\n\ncccc dddd\n\n");
    let e = split_overrides("accept a b\nmerge a b\n").unwrap_err();
    assert!(e.starts_with("line 2:"), "{e}");
}

#[test]
fn vector_pairs_are_a_second_source_with_the_same_guards() {
    let mut a = spec("a", E, "lease renewal is driven by the tick");
    let mut b = spec("b", E, "every tick extends the writer's hold");
    let mut c = spec("c", E, "every tick extends the hold for 30 seconds");
    let v = vec![1.0, 0.0, 0.0, 0.0];
    a.embedding = Some(v.clone());
    b.embedding = Some(vec![0.99, 0.05, 0.0, 0.0]);
    c.embedding = Some(v);
    let g = graph(&[a, b, c], &[]);
    let r = detect_similar(&g, &SimilarParams::default());
    // a~b by vector; c differs in its digits from both.
    assert_eq!(r.pairs.len(), 1, "{r:#?}");
    assert_eq!(r.pairs[0].source, SimilarSource::Vector);
    assert!(r.pairs[0].cosine.unwrap() >= 0.99);
    assert!(r.pairs[0].jaccard < DEFAULT_JACCARD);
    assert_eq!(r.guarded.digits_differ, 2);
    assert!(detect_similar(&g, &text_only()).pairs.is_empty());
}

#[test]
fn detection_is_deterministic() {
    let specs: Vec<Spec<'_>> = [
        ("a", "the writer lease renews every tick"),
        ("b", "the writer lease renews evry tick"),
        ("c", "the writer laese renews every tick"),
        ("d", "write_queue_receipt is durable"),
        ("e", "write queue reciept is durable"),
    ]
    .iter()
    .map(|(n, t)| spec(n, E, t))
    .collect();
    let g = graph(&specs, &[]);
    let first = detect_similar(&g, &text_only());
    for _ in 0..3 {
        assert_eq!(detect_similar(&graph(&specs, &[]), &text_only()), first);
    }
}

/// The negative rows of `fixtures/canonicalization-cases.json`: each row with
/// a `not_similar_to` partner must never be suggested with it.
#[cfg(feature = "fixtures")]
#[test]
fn fixture_negative_pairs_are_never_suggested() {
    let cases = crate::fixtures::load_canonicalization_cases().unwrap();
    let rows: Vec<(String, String)> = cases
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| {
            Some((
                c["input"].as_str()?.to_string(),
                c["not_similar_to"].as_str()?.to_string(),
            ))
        })
        .collect();
    assert!(rows.len() >= 8, "the negative rows are part of the contract");
    let names: Vec<(String, String)> = (0..rows.len())
        .map(|i| (format!("n{i}"), format!("m{i}")))
        .collect();
    let mut specs = Vec::new();
    for (i, (a, b)) in rows.iter().enumerate() {
        specs.push(spec(&names[i].0, E, a));
        specs.push(spec(&names[i].1, E, b));
    }
    let r = detect_similar(
        &graph(&specs, &[]),
        &SimilarParams {
            jaccard: 0.5,
            min_chars: 1,
            ..text_only()
        },
    );
    assert!(r.pairs.is_empty(), "{:#?}", r.pairs);
}

fn mix_for_test(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z ^ (z >> 31)
}

/// #178's runtime target: 10k concepts in under 2 s (text source). Run in
/// release: `cargo test --release similar_timing -- --ignored --nocapture`.
#[test]
#[ignore = "timing; run in release"]
fn similar_timing() {
    // A 4096-word vocabulary of random 3-to-8-letter words, eight words a
    // concept, and every third concept a one-letter typo of the one before:
    // a spread of 3-grams like a store's, not one where every text shares
    // most of them.
    let mut v: u64 = 0x9E37_79B9_7F4A_7C15;
    let vocab: Vec<String> = (0..4096)
        .map(|_| {
            v = mix_for_test(v);
            let len = 3 + (v % 6) as usize;
            (0..len)
                .map(|k| (b'a' + ((v >> (5 * k + 3)) % 26) as u8) as char)
                .collect()
        })
        .collect();
    let word = |n: u64| vocab[n as usize].clone();
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut texts: Vec<String> = Vec::with_capacity(10_000);
    for i in 0..10_000 {
        let t = if i % 3 == 2 {
            let prev: &String = &texts[i - 1];
            // Words are 3+ letters, so index 1 is a letter of the first.
            let mut c: Vec<char> = prev.chars().collect();
            c[1] = if c[1] == 'z' { 'y' } else { 'z' };
            c.into_iter().collect()
        } else {
            (0..8).map(|_| word(next() % 4096)).collect::<Vec<_>>().join(" ")
        };
        texts.push(t);
    }
    let names: Vec<String> = (0..texts.len()).map(|i| format!("t{i}")).collect();
    let specs: Vec<Spec<'_>> = names
        .iter()
        .zip(&texts)
        .map(|(n, t)| spec(n, E, t))
        .collect();
    let g = graph(&specs, &[]);
    let t0 = std::time::Instant::now();
    let r = detect_similar(&g, &text_only());
    let took = t0.elapsed();
    println!(
        "similar_timing: {} concepts, {} lexical candidates, {} pairs, {:?}",
        texts.len(),
        r.lexical_candidates,
        r.pairs.len(),
        took
    );
    assert!(took < std::time::Duration::from_secs(2), "{took:?}");

    // The vector source over the same 10k, 1024 wide.
    let mut v: u64 = 7;
    let specs: Vec<Spec<'_>> = names
        .iter()
        .zip(&texts)
        .map(|(n, t)| {
            let mut s = spec(n, E, t);
            s.embedding = Some(
                (0..1024)
                    .map(|_| {
                        v = mix_for_test(v);
                        (v >> 40) as f32 / (1u64 << 24) as f32 - 0.5
                    })
                    .collect(),
            );
            s
        })
        .collect();
    let g = graph(&specs, &[]);
    let t0 = std::time::Instant::now();
    let r = detect_similar(&g, &SimilarParams::default());
    println!(
        "similar_timing (with vectors, 1024 wide): {} vector candidates, {} pairs, {:?}",
        r.vector_candidates,
        r.pairs.len(),
        t0.elapsed()
    );
}


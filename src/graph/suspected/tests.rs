//! #198: the trigger's text rule (one span and value slot, the cue
//! lexicon, the `difflib` port) and its pair predicate, without the write
//! path (the hybrid tests drive that end to end).

use chrono::TimeZone;
use uuid::Uuid;

use super::slot::{differing_spans, first_cue, judge, normalize, tokens, Span};
use super::*;
use crate::types::{AgentId, CanonizationStatus, Interaction};

fn span(a_start: usize, a_end: usize, b_start: usize, b_end: usize) -> Span {
    Span {
        a_start,
        a_end,
        b_start,
        b_end,
    }
}

fn spans_of(a: &str, b: &str) -> Vec<(usize, usize, usize, usize)> {
    differing_spans(&tokens(a), &tokens(b))
        .into_iter()
        .map(|s| (s.a_start, s.a_end, s.b_start, s.b_end))
        .collect()
}

/// The port reproduces `difflib.SequenceMatcher(autojunk=False)`'s
/// non-equal opcodes. Expected values were printed by Python 3 on the same
/// inputs (whitespace tokens, `.,;()'"` stripped, lowercased).
#[test]
fn the_alignment_matches_difflib() {
    type Case = (
        &'static str,
        &'static str,
        Vec<(usize, usize, usize, usize)>,
    );
    let cases: [Case; 9] = [
        ("a b c d", "a x c d", vec![(1, 2, 1, 2)]),
        ("a b c d", "a b c d e", vec![(4, 4, 4, 5)]),
        ("x a b", "a b", vec![(0, 1, 0, 0)]),
        ("a b a b c", "b a b c c", vec![(0, 1, 0, 0), (5, 5, 4, 5)]),
        (
            "the cat sat on the mat",
            "the dog sat on a mat",
            vec![(1, 2, 1, 2), (4, 5, 4, 5)],
        ),
        (
            "one two three",
            "three two one",
            vec![(0, 0, 0, 2), (1, 3, 3, 3)],
        ),
        ("a b c a b c", "a b c", vec![(3, 6, 3, 3)]),
        ("A, b. (c)", "a b c", vec![]),
        (
            "p q r s t u",
            "p q x s y u",
            vec![(2, 3, 2, 3), (4, 5, 4, 5)],
        ),
    ];
    for (a, b, want) in cases {
        assert_eq!(spans_of(a, b), want, "{a:?} / {b:?}");
    }
    assert_eq!(
        differing_spans(&tokens("a b c d"), &tokens("a x c d")),
        vec![span(1, 2, 1, 2)]
    );
}

#[test]
fn tokens_and_normalization() {
    assert_eq!(tokens("  a\tb \n c  "), vec!["a", "b", "c"]);
    assert_eq!(normalize("(Hello),"), "hello");
    assert_eq!(normalize("\"x.y.\""), "x.y");
    assert_eq!(normalize("v0.5.0."), "v0.5.0");
    // Only the listed characters are stripped, and only at the ends.
    assert_eq!(normalize("port:"), "port:");
    assert_eq!(normalize("a,b"), "a,b");
}

/// The lexicon is a documented, lowercase, duplicate-free set that holds
/// the copulas, the separators and `reports` (added on the dev split).
#[test]
fn the_cue_lexicon() {
    let mut seen = std::collections::HashSet::new();
    for cue in PREDICATE_CUES {
        assert_eq!(*cue, cue.to_lowercase(), "{cue} is lowercase");
        assert_eq!(normalize(cue), *cue, "{cue} survives normalization");
        assert!(seen.insert(*cue), "{cue} is listed once");
    }
    for cue in [
        "is", "are", "was", "=", ":", "runs", "uses", "pins", "reports", "at",
    ] {
        assert!(PREDICATE_CUES.contains(&cue), "{cue}");
    }
    // The reference script's list is 58 entries; `reports` makes 59.
    assert_eq!(PREDICATE_CUES.len(), 59);
}

#[test]
fn the_first_cue() {
    assert_eq!(
        first_cue(&tokens("the embedder listens on port 8080")),
        Some(2)
    );
    assert_eq!(first_cue(&tokens("J1 round-1 verdict: approve")), Some(2));
    assert_eq!(first_cue(&tokens("timeout = 30s")), Some(1));
    assert_eq!(
        first_cue(&tokens("The port Is 8080.")),
        Some(2),
        "case-folded"
    );
    assert_eq!(first_cue(&tokens("(Uses) candle")), Some(0), "stripped");
    assert_eq!(first_cue(&tokens("release v0.5.0 notes")), None);
    assert_eq!(first_cue(&tokens("")), None);
}

/// The rule on the shapes the owner decision names.
#[test]
fn one_span_and_value_slot() {
    // A new value after the predicate.
    assert!(one_span_value_slot(
        "dogfood runs the pinned lambo 0.4.0",
        "dogfood runs the pinned lambo 0.5.0"
    ));
    assert!(one_span_value_slot(
        "The demo's user schema reports 15 dependents.",
        "The demo's user schema reports 9 dependents."
    ));
    // A changed subject: two different things.
    let v = judge("J1 round-1 verdict: approve", "J2 round-1 verdict: approve");
    assert_eq!((v.spans, v.value_slot), (1, false));
    // Two changed values: two spans.
    let v = judge(
        "The #190 queries split 239 dev / 129 held-out.",
        "The #190 queries split 199 dev / 111 held-out.",
    );
    assert_eq!(v.spans, 2);
    assert!(!v.suspected());
    // No cue in one text.
    assert!(!one_span_value_slot(
        "release v0.5.0 notes",
        "release v0.6.0 notes"
    ));
    // Identical texts differ nowhere.
    let v = judge("x is 1", "x is 1");
    assert_eq!((v.spans, v.value_slot), (0, false));
    // The span must start strictly after the cue: a changed cue word is
    // not a new value.
    assert!(!one_span_value_slot(
        "the port is 8080",
        "the port was 8080"
    ));
}

fn t(minutes: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 12, 0, 0).unwrap() + chrono::Duration::minutes(minutes)
}

fn sid() -> SessionId {
    SessionId::from("suspected-unit")
}

fn interaction(n: u64, at: i64, event: Option<i64>) -> Interaction {
    Interaction {
        id: NodeId(Uuid::from_u64_pair(0x198, n)),
        session_id: sid(),
        agent_id: AgentId::from("agent"),
        prompt_text: None,
        previous_id: None,
        created_at: t(at),
        event_time: event.map(t),
    }
}

fn concept(n: u64, origin: &Interaction, content: &str, ty: ConceptType) -> Concept {
    Concept {
        id: NodeId(Uuid::from_u64_pair(0x199, n)),
        session_id: sid(),
        content: content.into(),
        canonical_key: content.to_lowercase(),
        concept_type: ty,
        origin_interaction: origin.id,
        origin_agent: AgentId::from("agent"),
        created_at: origin.created_at,
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        human_confirmed: 0,
        canonization_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding: None,
        chunk_group_id: None,
        embedding_source: None,
    }
}

/// A graph with two interactions (minutes 0 and 10, or as given) and one
/// concept from each.
fn pair_graph(
    old: (&str, ConceptType, Option<i64>),
    new: (&str, ConceptType, Option<i64>),
) -> (Graph, Concept, Concept) {
    let mut g = Graph::new(sid());
    let i0 = interaction(1, 0, old.2);
    let mut i1 = interaction(2, 10, new.2);
    i1.previous_id = Some(i0.id);
    g.insert_interaction(i0.clone()).unwrap();
    g.insert_interaction(i1.clone()).unwrap();
    let a = concept(1, &i0, old.0, old.1);
    let b = concept(2, &i1, new.0, new.1);
    g.insert_concept(a.clone(), i0.id).unwrap();
    g.insert_concept(b.clone(), i1.id).unwrap();
    (g, a, b)
}

const OLD: &str = "the embedder listens on port 8080";
const NEW: &str = "the embedder listens on port 8081";

#[test]
fn the_pair_predicate() {
    use ConceptType::*;
    let (g, a, b) = pair_graph((OLD, Logic, None), (NEW, Logic, None));
    let want = Some(SuspectedPair {
        newer: b.id,
        older: a.id,
    });
    assert_eq!(candidate(&g, &a, &b), want);
    assert_eq!(
        candidate(&g, &b, &a),
        want,
        "argument order does not matter"
    );
    assert_eq!(candidate(&g, &a, &a), None, "not with itself");
    // Type conditions.
    for (ta, tb) in [(Logic, Constraint), (Resource, Resource), (Entity, Entity)] {
        let (g, a, b) = pair_graph((OLD, ta, None), (NEW, tb, None));
        assert_eq!(candidate(&g, &a, &b), None, "{ta:?}/{tb:?}");
    }
    // The identifier guard would merge these: never suspected.
    let (g, a, b) = pair_graph(
        (OLD, Logic, None),
        ("port 8080 is where the embedder listens", Logic, None),
    );
    assert_eq!(candidate(&g, &a, &b), None);
}

#[test]
fn the_type_and_scope_conditions() {
    assert!(eligible_type(ConceptType::Logic));
    assert!(eligible_type(ConceptType::Constraint));
    assert!(eligible_type(ConceptType::Observation));
    assert!(!eligible_type(ConceptType::Resource));
    assert!(!eligible_type(ConceptType::Entity));
    // A different recorded repository scope is a different repository.
    let (g, a, mut b) = pair_graph(
        (OLD, ConceptType::Logic, None),
        (NEW, ConceptType::Logic, None),
    );
    b.canonical_key = crate::graph::resource::scoped_key_unchecked(NEW, "host/owner/repo");
    assert_eq!(repo_scope(&a), None);
    assert_eq!(repo_scope(&b).as_deref(), Some("host/owner/repo"));
    assert_eq!(candidate(&g, &a, &b), None);
}

/// Dating: the origin interaction's about-time clamped to its write time.
#[test]
fn direction_and_ties() {
    use ConceptType::Logic;
    // A tie records nothing: both writes are about minute -5.
    let (g, a, b) = pair_graph((OLD, Logic, Some(-5)), (NEW, Logic, Some(-5)));
    assert_eq!(candidate(&g, &a, &b), None);
    // Backfilled: the second write is about minute -30, before the first.
    let (g, a, b) = pair_graph((OLD, Logic, None), (NEW, Logic, Some(-30)));
    assert_eq!(dated_at(&g, &b), t(-30));
    assert_eq!(
        candidate(&g, &a, &b),
        Some(SuspectedPair {
            newer: a.id,
            older: b.id
        })
    );
    // A future event time is clamped to the write time.
    let (g, a, b) = pair_graph((OLD, Logic, Some(500)), (NEW, Logic, None));
    assert_eq!(dated_at(&g, &a), t(0));
    assert_eq!(
        candidate(&g, &a, &b).map(|p| p.newer),
        Some(b.id),
        "a future date does not make the first write the newer"
    );
}

/// Recording, the pair predicate for #181, settling and listing.
#[test]
fn record_settle_and_pairs() {
    use ConceptType::Logic;
    let (mut g, a, b) = pair_graph((OLD, Logic, None), (NEW, Logic, None));
    let stamp = Stamp {
        created_at: t(10),
        event_time: None,
    };
    assert_eq!(
        record_refused_hits(&mut g, b.id, &[a.id, a.id], stamp).unwrap(),
        1
    );
    assert_eq!(
        record_refused_hits(&mut g, b.id, &[a.id], stamp).unwrap(),
        0
    );
    assert!(is_suspected_pair(&g, a.id, b.id));
    assert!(is_suspected_pair(&g, b.id, a.id));
    assert_eq!(
        pairs(&g),
        vec![SuspectedPair {
            newer: b.id,
            older: a.id
        }]
    );
    g.assert_invariants().unwrap();
    assert_eq!(settle(&mut g, a.id, b.id).unwrap(), 1);
    assert!(!is_suspected_pair(&g, a.id, b.id));
    assert_eq!(settle(&mut g, a.id, b.id).unwrap(), 0);
    // A hit that is not a concept is skipped.
    assert_eq!(
        record_refused_hits(&mut g, b.id, &[NodeId::new()], stamp).unwrap(),
        0
    );
}

/// The edge is informational: it confers no dependence, so blast radii,
/// #207's stale-by-dependency citations and the dependents relation never
/// see it.
#[test]
fn the_edge_confers_nothing() {
    use ConceptType::Logic;
    let (mut g, a, b) = pair_graph((OLD, Logic, None), (NEW, Logic, None));
    let stamp = Stamp {
        created_at: t(10),
        event_time: None,
    };
    record_refused_hits(&mut g, b.id, &[a.id], stamp).unwrap();
    let edge = g
        .edge_between(b.id, a.id, EdgeType::SuspectedSupersedes)
        .unwrap()
        .clone();
    assert_eq!(edge.dependence(), None);
    let radii = crate::recall::format::blast_radii(&g);
    assert_eq!(radii.get(&a.id).copied().unwrap_or(0), 0);
    assert_eq!(radii.get(&b.id).copied().unwrap_or(0), 0);
    // #207 reads only `Dependency` citations: b cites nothing.
    assert!(crate::daemon::contradiction::stale_citations(
        &g,
        b.id,
        std::time::Duration::ZERO,
        t(60)
    )
    .is_empty());
    // Recall expansion follows none of it.
    assert!(!crate::recall::expand::TRAVERSAL_ORDER.contains(&EdgeType::SuspectedSupersedes));
}

use crate::graph::index::InvertedIndex;
use crate::graph::Graph;
use crate::recall::{candidates, detail::DetailedHit, dispatch, format};
use crate::test_util::citations::{concept, graph};
use crate::types::{ConceptType, RecallHit};

#[test]
fn a_name_finds_an_old_target_without_keyword_recent_or_vector_evidence() {
    let (mut g, i) = graph("name-recall");
    let target = concept(
        &mut g,
        i,
        "the writer keeps exclusive ownership until release",
        ConceptType::Logic,
    );
    g.declare_name("Caf\u{e9} invariant", target).unwrap();
    let g = Graph::from_snapshot(g.snapshot()).unwrap();
    let (hits, _) = candidates::candidates_with_legs_as(
        &g,
        &InvertedIndex::new(),
        candidates::Phase1Input::default(),
        "INVARIANT cafe\u{301}",
        5,
        candidates::RecentLeg::Skip,
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item, target);
    assert_eq!(
        dispatch::resolve_anchor(&g, "what depends on caf\u{e9} invariant?"),
        Some(target)
    );
    let hit = RecallHit {
        node_id: target,
        content: "the ruling".into(),
        concept_type: Some(ConceptType::Logic),
        score: 1.0,
        is_canonical: false,
        blast_radius: None,
        repo_scope: None,
    };
    let mut detailed = DetailedHit::new(&hit, None);
    detailed.names = g.display_names(target);
    assert!(format::render_detailed_block(&detailed).contains("names [\"Caf\u{e9} invariant\"]"));
    assert_eq!(
        serde_json::to_value(&detailed).unwrap()["names"][0],
        "Caf\u{e9} invariant"
    );
}

//! #198: the suspected-supersession surface. The older hit of a
//! `SuspectedSupersedes` pair gets one `possibly_superseded` line naming the
//! newer one, only when both are shown, never forcing anything; the
//! structured `possibly_superseded_by` field is set whether or not the line
//! is enabled; a recall with no shown pair is byte-identical.

use super::*;

fn suspected_edge(newer: u64, older: u64) -> crate::types::Edge {
    crate::types::Edge {
        edge_type: crate::types::EdgeType::SuspectedSupersedes,
        ..dep_edge(newer, older)
    }
}

fn line(by: u64) -> String {
    format::possibly_superseded_warning(uid(by), &format!("concept {by}"))
}

fn field(result: &DetailedRecall, id: u64) -> Option<NodeId> {
    result
        .detailed
        .iter()
        .find(|h| h.node_id == uid(id))
        .and_then(|h| h.possibly_superseded_by)
}

/// [`assemble_contradicted`]'s ranking (higher id ranks higher) with the
/// line switch given.
fn assemble_gated(g: &Graph, in_expanded: &[u64], q: RecallQuery, line: bool) -> DetailedRecall {
    let scores = ScoreTable {
        epoch: 0,
        ranked: in_expanded
            .iter()
            .map(|&id| Scored::new(uid(id), id as f64))
            .collect(),
    };
    let expanded = ExpandedSet {
        required: in_expanded
            .iter()
            .map(|&id| Scored::new(uid(id), 0.0))
            .collect(),
        siblings: Vec::new(),
    };
    assemble_with_legs(
        g,
        &expanded,
        &[],
        None,
        &scores,
        &HashMap::new(),
        &q,
        RecallWeights::default(),
        crate::Config::default().canonization_edge_min_age,
        line,
        ts(60),
        default_token_count,
    )
}

fn scores(r: &DetailedRecall) -> Vec<f64> {
    r.hits.iter().map(|h| h.score).collect()
}

/// Both shown: the older hit carries one line and the field, the newer
/// nothing, and hits, order and scores are those of the same recall
/// without the edge.
#[test]
fn the_older_hit_is_warned_when_both_are_shown() {
    let clean = graph_with(6);
    let mut g = graph_with(6);
    g.upsert_edge(suspected_edge(5, 2)).unwrap();
    let before = assemble_gated(&clean, &[2, 3, 5], query(3, 10_000), true);
    let after = assemble_gated(&g, &[2, 3, 5], query(3, 10_000), true);
    assert_eq!(ids_of(&after), vec![uid(5), uid(3), uid(2)]);
    assert_eq!(ids_of(&after), ids_of(&before));
    assert_eq!(scores(&after), scores(&before));
    assert_eq!(kinds(&after, 2), vec![AnnotationKind::PossiblySuperseded]);
    assert!(kinds(&after, 5).is_empty());
    assert_eq!(field(&after, 2), Some(uid(5)));
    assert_eq!(field(&after, 5), None);
    assert_eq!(after.warnings, vec![line(5)]);
    assert!(after.context.contains(&line(5)), "{}", after.context);
    // The wire carries the kind in snake_case and the field by id.
    let json = serde_json::to_value(&after).unwrap();
    let older = &json["hits"][2];
    assert_eq!(older["annotations"][0]["kind"], "possibly_superseded");
    assert_eq!(older["possibly_superseded_by"], uid(5).0.to_string());
    assert!(json["hits"][0].get("possibly_superseded_by").is_none());
}

/// Only one shown: no line, no field, and the result is byte-identical to
/// the same recall without the edge, whichever end is missing. The line
/// never forces the other concept in past `top_k`.
#[test]
fn one_end_shown_is_byte_identical() {
    let clean = graph_with(6);
    let mut g = graph_with(6);
    g.upsert_edge(suspected_edge(5, 2)).unwrap();
    for (members, top_k) in [
        (&[2u64, 3, 5][..], 2), // the newer c5 shown, the older c2 cut
        (&[2, 3][..], 2),       // the older c2 shown, the newer not a member
        (&[2, 3, 5][..], 1),
    ] {
        let before = assemble_gated(&clean, members, query(top_k, 10_000), true);
        let after = assemble_gated(&g, members, query(top_k, 10_000), true);
        assert_eq!(ids_of(&after), ids_of(&before), "{members:?}/{top_k}");
        assert_eq!(after.context, before.context, "{members:?}/{top_k}");
        assert_eq!(after.warnings, before.warnings, "{members:?}/{top_k}");
        assert_eq!(
            serde_json::to_string(&after).unwrap(),
            serde_json::to_string(&before).unwrap(),
            "{members:?}/{top_k}"
        );
        assert!(after
            .detailed
            .iter()
            .all(|h| h.possibly_superseded_by.is_none()));
    }
    // And no pair at all: nothing on the wire names the feature.
    let plain = assemble_gated(&clean, &[2, 3, 5], query(3, 10_000), true);
    let text = serde_json::to_string(&plain).unwrap();
    assert!(!text.contains("possibly_superseded"), "{text}");
}

/// The gate off (an embedder below the precision bar): the structured
/// field only. No line in the annotations, warnings or context, which are
/// exactly those of the recall without the edge.
#[test]
fn with_the_line_off_only_the_field_is_set() {
    let clean = graph_with(6);
    let mut g = graph_with(6);
    g.upsert_edge(suspected_edge(5, 2)).unwrap();
    let before = assemble_gated(&clean, &[2, 3, 5], query(3, 10_000), false);
    let after = assemble_gated(&g, &[2, 3, 5], query(3, 10_000), false);
    assert_eq!(field(&after, 2), Some(uid(5)));
    assert!(kinds(&after, 2).is_empty());
    assert_eq!(after.warnings, before.warnings);
    assert_eq!(after.context, before.context);
    assert_eq!(ids_of(&after), ids_of(&before));
}

/// Several shown replacements: one line per hit, naming the newest by the
/// trigger's clock (all tie here, so the smallest id).
#[test]
fn at_most_one_line_per_hit() {
    let mut g = graph_with(6);
    g.upsert_edge(suspected_edge(5, 2)).unwrap();
    g.upsert_edge(suspected_edge(4, 2)).unwrap();
    let result = assemble_gated(&g, &[2, 4, 5], query(3, 10_000), true);
    assert_eq!(kinds(&result, 2), vec![AnnotationKind::PossiblySuperseded]);
    assert_eq!(field(&result, 2), Some(uid(4)));
    assert_eq!(result.warnings, vec![line(4)]);
}

/// A later-dated replacement wins over an earlier one.
#[test]
fn the_newest_replacement_is_named() {
    let mut g = graph_with(6);
    let mut later = interaction(2);
    later.created_at = ts(5);
    later.previous_id = Some(iid(1));
    g.insert_interaction(later).unwrap();
    g.insert_concept(concept(7, iid(2), "concept 7"), iid(2))
        .unwrap();
    g.upsert_edge(suspected_edge(5, 2)).unwrap();
    g.upsert_edge(suspected_edge(7, 2)).unwrap();
    let result = assemble_gated(&g, &[2, 5, 7], query(3, 10_000), true);
    assert_eq!(field(&result, 2), Some(uid(7)));
    assert_eq!(result.warnings, vec![line(7)]);
}

#[test]
fn the_kind_is_snake_case_on_the_wire() {
    assert_eq!(
        serde_json::to_string(&AnnotationKind::PossiblySuperseded).unwrap(),
        "\"possibly_superseded\""
    );
}

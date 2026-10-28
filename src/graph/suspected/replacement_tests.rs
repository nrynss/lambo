//! #198 part c: the replacement-candidate predicate, ranking, cap and
//! receipt form, without the write path (the hybrid tests drive that end
//! to end).

use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

use super::*;
use crate::types::{
    AgentId, CanonizationStatus, ConceptType, Edge, EdgeType, EmbeddingSource, Interaction,
    SessionId, SourceModality, VectorOrigin,
};

fn t(minutes: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 12, 0, 0).unwrap() + chrono::Duration::minutes(minutes)
}

fn sid() -> SessionId {
    SessionId::from("replacement-unit")
}

fn id(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(0x198c, n))
}

fn interaction() -> Interaction {
    Interaction {
        id: NodeId(Uuid::from_u64_pair(0x198d, 1)),
        session_id: sid(),
        agent_id: AgentId::from("agent"),
        prompt_text: None,
        previous_id: None,
        created_at: t(0),
        event_time: None,
    }
}

fn concept(n: u64, content: &str, ty: ConceptType) -> Concept {
    Concept {
        id: id(n),
        session_id: sid(),
        content: content.into(),
        canonical_key: content.to_lowercase(),
        concept_type: ty,
        origin_interaction: interaction().id,
        origin_agent: AgentId::from("agent"),
        created_at: t(0),
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

/// A graph holding the new concept (id 0) and `olds`.
fn graph(new: Concept, olds: Vec<Concept>) -> Graph {
    let mut g = Graph::new(sid());
    let i = interaction();
    g.insert_interaction(i.clone()).unwrap();
    g.insert_concept(new, i.id).unwrap();
    for c in olds {
        g.insert_concept(c, i.id).unwrap();
    }
    g
}

fn edge(g: &mut Graph, source: NodeId, target: NodeId, ty: EdgeType) {
    g.upsert_edge(Edge {
        event_time: None,
        id: NodeId::new(),
        session_id: sid(),
        source,
        target,
        edge_type: ty,
        weight: 1.0,
        reinforcements: 1,
        created_at: t(0),
        last_reinforced: t(0),
    })
    .unwrap();
}

fn hit(n: u64, score: f64) -> Scored<NodeId> {
    Scored::new(id(n), score)
}

fn listed(g: &Graph, hits: &[Scored<NodeId>], skip: &[NodeId]) -> Vec<(NodeId, f64)> {
    select(g, id(0), hits, 0.85, skip)
        .into_iter()
        .map(|c| {
            assert_eq!(c.new, id(0));
            (c.id, c.cosine)
        })
        .collect()
}

const NEW: &str = "lane C batches embeds at the forward width of 8";
const OLD: &str = "lane C prefetches across jobs with two limits";

#[test]
fn a_close_same_type_hit_is_listed_with_its_excerpt() {
    use ConceptType::Logic;
    let g = graph(concept(0, NEW, Logic), vec![concept(1, OLD, Logic)]);
    let got = select(&g, id(0), &[hit(1, 0.9)], 0.85, &[]);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, id(1));
    assert_eq!(got[0].excerpt, crate::recall::format::excerpt(OLD));
    assert_eq!(got[0].cosine, 0.9);
}

/// Each eligible type lists; `Resource` and `Entity` never do, and the two
/// must share the type.
#[test]
fn type_and_eligibility() {
    use ConceptType::*;
    for ty in [Logic, Constraint, Observation] {
        let g = graph(concept(0, NEW, ty), vec![concept(1, OLD, ty)]);
        assert_eq!(listed(&g, &[hit(1, 0.9)], &[]).len(), 1, "{ty:?}");
    }
    for ty in [Resource, Entity] {
        let g = graph(concept(0, NEW, ty), vec![concept(1, OLD, ty)]);
        assert!(listed(&g, &[hit(1, 0.9)], &[]).is_empty(), "{ty:?}");
    }
    for (a, b) in [
        (Logic, Constraint),
        (Constraint, Observation),
        (Observation, Logic),
    ] {
        let g = graph(concept(0, NEW, a), vec![concept(1, OLD, b)]);
        assert!(listed(&g, &[hit(1, 0.9)], &[]).is_empty(), "{a:?}/{b:?}");
    }
}

#[test]
fn repository_scope_must_match() {
    use ConceptType::Logic;
    let mut scoped = concept(1, OLD, Logic);
    scoped.canonical_key = crate::graph::resource::scoped_key_unchecked(OLD, "host/owner/repo");
    let g = graph(concept(0, NEW, Logic), vec![scoped]);
    assert!(listed(&g, &[hit(1, 0.9)], &[]).is_empty());
    // Both in one scope: listed.
    let mut new = concept(0, NEW, Logic);
    new.canonical_key = crate::graph::resource::scoped_key_unchecked(NEW, "host/owner/repo");
    let mut old = concept(1, OLD, Logic);
    old.canonical_key = crate::graph::resource::scoped_key_unchecked(OLD, "host/owner/repo");
    let g = graph(new, vec![old]);
    assert_eq!(listed(&g, &[hit(1, 0.9)], &[]).len(), 1);
}

/// At the floor is listed; below it, or a non-finite score, is not. A
/// finite score is clamped into [0, 1] first: a cosine just above 1.0 from
/// float error is listed at 1.0, and a negative one is below any floor.
#[test]
fn the_floor() {
    use ConceptType::Logic;
    let g = graph(concept(0, NEW, Logic), vec![concept(1, OLD, Logic)]);
    assert_eq!(listed(&g, &[hit(1, 0.85)], &[]).len(), 1, "at the floor");
    assert!(listed(&g, &[hit(1, 0.8499)], &[]).is_empty(), "below");
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.9] {
        assert!(listed(&g, &[hit(1, bad)], &[]).is_empty(), "{bad}");
    }
    for above in [1.0 + 1e-12, 1.000_000_1, 1.5] {
        assert_eq!(
            listed(&g, &[hit(1, above)], &[]),
            vec![(id(1), 1.0)],
            "{above}"
        );
    }
}

/// Ranked by cosine, then id; capped; a repeated hit counted once.
#[test]
fn ordering_cap_and_dedup() {
    use ConceptType::Logic;
    let olds = (1..=5)
        .map(|n| concept(n, &format!("{OLD} {n}"), Logic))
        .collect();
    let g = graph(concept(0, NEW, Logic), olds);
    let hits = [
        hit(4, 0.88),
        hit(2, 0.91),
        hit(5, 0.86),
        hit(3, 0.91),
        hit(1, 0.87),
        hit(2, 0.91),
    ];
    assert_eq!(
        listed(&g, &hits, &[]),
        vec![(id(2), 0.91), (id(3), 0.91), (id(4), 0.88)]
    );
    assert_eq!(REPLACEMENT_CANDIDATE_LIMIT, 3);
}

/// A pair already joined by `against` or `depends_on`, either way, is
/// settled: never listed.
#[test]
fn an_already_cited_concept_is_not_listed() {
    use ConceptType::Logic;
    for ty in [EdgeType::Contradicts, EdgeType::Dependency] {
        for forward in [true, false] {
            let mut g = graph(concept(0, NEW, Logic), vec![concept(1, OLD, Logic)]);
            let (s, t) = if forward {
                (id(0), id(1))
            } else {
                (id(1), id(0))
            };
            edge(&mut g, s, t, ty);
            assert!(
                listed(&g, &[hit(1, 0.9)], &[]).is_empty(),
                "{ty:?} forward={forward}"
            );
        }
    }
    // Another relation does not settle it.
    let mut g = graph(concept(0, NEW, Logic), vec![concept(1, OLD, Logic)]);
    edge(&mut g, id(0), id(1), EdgeType::CoOccurrence);
    assert_eq!(listed(&g, &[hit(1, 0.9)], &[]).len(), 1);
}

/// The merged-into concept (and anything else the caller skips) is never
/// listed; the next hit takes its place.
#[test]
fn the_merged_into_hit_is_skipped() {
    use ConceptType::Logic;
    let g = graph(
        concept(0, NEW, Logic),
        vec![concept(1, OLD, Logic), concept(2, "another rule", Logic)],
    );
    let hits = [hit(1, 0.95), hit(2, 0.9)];
    assert_eq!(listed(&g, &hits, &[id(1)]), vec![(id(2), 0.9)]);
    assert_eq!(listed(&g, &hits, &[]).len(), 2);
}

/// Itself, another session's concept, an image concept, and an id that is
/// not a concept are never listed; a new id that is not a concept lists
/// nothing.
#[test]
fn never_itself_another_session_an_image_or_a_non_concept() {
    use ConceptType::Logic;
    let mut image = concept(2, OLD, Logic);
    image.embedding_source = Some(EmbeddingSource {
        modality: SourceModality::Image,
        origin: VectorOrigin::Client,
        sha256: None,
        mime: None,
    });
    let g = graph(concept(0, NEW, Logic), vec![image]);
    assert!(listed(&g, &[hit(0, 1.0), hit(2, 0.9), hit(9, 0.9)], &[]).is_empty());
    assert!(select(&g, id(7), &[hit(0, 0.9)], 0.85, &[]).is_empty());
    // Another session's concept, judged by the predicate.
    let new = concept(0, NEW, Logic);
    let mut other = concept(1, OLD, Logic);
    other.session_id = SessionId::from("elsewhere");
    let g = graph(new.clone(), vec![]);
    assert!(!eligible_pair(&g, &new, &other));
}

/// The receipt form: `None` when empty, so nothing changes; the wire
/// names; the byte bound keeps the true total.
#[test]
fn the_receipt_report() {
    assert_eq!(ReplacementReport::for_receipt(&[]), None);
    let c = ReplacementCandidate {
        new: id(0),
        id: NodeId(Uuid::from_u64_pair(0xabcd_ef01_0000_0000, 1)),
        excerpt: "an older rule".into(),
        cosine: 0.867_891,
    };
    let report = ReplacementReport::for_receipt(std::slice::from_ref(&c)).unwrap();
    assert_eq!(report.total, 1);
    let wire = serde_json::to_value(&report).unwrap();
    assert_eq!(
        wire,
        serde_json::json!({
            "candidates": [{
                "id": "abcdef01",
                "node_id": c.id.0.to_string(),
                "excerpt": "an older rule",
                "cosine": 0.8679,
                "new_id": crate::recall::format::short_id(id(0)),
            }],
            "total": 1,
        })
    );
    assert_eq!(report.cosines(), vec![0.8679]);
    let text = report.describe();
    assert!(
        text.starts_with("; 1 replacement candidate(s): existing concepts this write may replace"),
        "{text}"
    );
    assert!(
        text.contains("\n  - abcdef01 \"an older rule\" (cosine 0.8679, for "),
        "{text}"
    );
    assert!(!text.contains("more)"), "{text}");

    let long = ReplacementCandidate {
        excerpt: "é".repeat(80),
        ..c.clone()
    };
    let many = vec![long; 40];
    let report = ReplacementReport::for_receipt(&many).unwrap();
    assert_eq!(report.total, 40);
    let kept = report.candidates.len();
    assert!(kept > 0 && kept < 40, "{kept}");
    // The bound is the serialized rows' exact size: what is kept fits, and
    // one more row would not.
    let rows: Vec<serde_json::Value> = serde_json::to_value(&report).unwrap()["candidates"]
        .as_array()
        .unwrap()
        .clone();
    let bytes: usize = rows.iter().map(|r| r.to_string().len()).sum();
    assert_eq!(
        bytes,
        report.candidates.iter().map(row_bytes).sum::<usize>()
    );
    assert!(bytes <= REPLACEMENT_RECEIPT_BYTES, "{bytes}");
    assert!(
        bytes + row_bytes(&many[0]) > REPLACEMENT_RECEIPT_BYTES,
        "{bytes}"
    );
    assert!(report
        .describe()
        .ends_with(&format!("\n  ({} more)", 40 - kept)));
}

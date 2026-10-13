//! #19(c) on the `Memory` surface: an `against` write on a promoted belief
//! warns on the next recall and pulls the contradicting concept in; the
//! warning lapses at read time once the belief is demoted; and a layered
//! recall never leaks a concept across sessions, nor lifts a layer floor
//! for a companion.

use super::*;
use crate::graph::cite::ConceptCitations;
use crate::memory::LayerRef;
use crate::recall::detail::AnnotationKind;
use crate::recall::format;
use crate::writeq::RECEIPT_WAIT_MAX;

const BELIEF: &str = "brand y chargers are safe to leave plugged in";
/// Shares no keyword with the queries below, so only the contradiction
/// brings it into a result.
const NEWER: &str = "units overheated overnight and were recalled";

async fn session(name: &str) -> Memory {
    Memory::builder()
        .session(name)
        .agent("agent-a")
        .config(Config {
            daemon_tick_interval: Duration::from_secs(3_600),
            ..Config::default()
        })
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(MemoryStore::new()) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Canonical)
        .build()
        .await
        .expect("build")
}

fn node_of(mem: &Memory, content: &str) -> NodeId {
    mem.graph
        .read()
        .concepts()
        .find(|c| c.content == content)
        .map(|c| c.id)
        .unwrap_or_else(|| panic!("no concept {content:?}"))
}

fn transition(mem: &Memory, id: NodeId, from: CanonizationStatus, to: CanonizationStatus) {
    let now = Utc::now();
    mem.graph
        .write()
        .apply_canonization_transition(CanonizationEvent {
            id: NodeId::new(),
            session_id: mem.session().clone(),
            node_id: id,
            from_status: from,
            to_status: to,
            blast_radius: None,
            last_demotion_time: (to == CanonizationStatus::None).then_some(now),
            occurred_at: now,
            reason: None,
        })
        .unwrap();
}

/// A Canonical belief in `mem`, contradicted by agent-b's `NEWER`.
async fn contradicted(mem: &Memory) -> (NodeId, NodeId) {
    mem.derive_as(
        &AgentId::new("agent-a"),
        &[(BELIEF, ConceptType::Logic)],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    let belief = node_of(mem, BELIEF);
    transition(
        mem,
        belief,
        CanonizationStatus::None,
        CanonizationStatus::Venerable,
    );
    transition(
        mem,
        belief,
        CanonizationStatus::Venerable,
        CanonizationStatus::Canonical,
    );
    let agent_b = AgentId::new("agent-b");
    let submitted = mem
        .derive_cited_async_as(
            &agent_b,
            &[(NEWER, ConceptType::Observation)],
            &ParentOf::none(),
            &[ConceptCitations {
                content: NEWER.to_string(),
                depends_on: Vec::new(),
                against: vec![format::short_id(belief)],
            }],
            None,
        )
        .await
        .unwrap();
    let answer = mem
        .pipeline()
        .wait(&agent_b, submitted.receipt, RECEIPT_WAIT_MAX)
        .await;
    assert!(answer.is_settled(), "{}", answer.tag());
    mem.settle_daemon().await;
    (belief, node_of(mem, NEWER))
}

fn kinds(r: &crate::recall::detail::DetailedRecall, id: NodeId) -> Vec<AnnotationKind> {
    r.detailed
        .iter()
        .find(|h| h.node_id == id)
        .map(|h| h.annotations.iter().map(|a| a.kind).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn an_against_write_on_a_canonical_belief_warns_until_it_is_demoted() {
    let mem = session("contradiction-e2e").await;
    let (belief, newer) = contradicted(&mem).await;

    let r = mem
        .recall_detailed(query("brand y chargers"))
        .await
        .unwrap();
    assert!(
        kinds(&r, belief).contains(&AnnotationKind::Contradiction),
        "{}",
        r.context
    );
    assert_eq!(kinds(&r, newer), vec![AnnotationKind::Contradicts]);
    let line = r
        .warnings
        .iter()
        .find(|w| w.starts_with("⚠ Contradicted at"))
        .expect("a contradiction line");
    assert!(line.contains("by agent-b"), "{line}");
    assert!(line.contains(&format::short_id(newer)), "{line}");
    assert!(r.context.contains(NEWER), "the companion is rendered");

    // Demoted: recall's read-time revalidation drops the warning, with no
    // daemon cycle in between (the tick is an hour and nothing woke it).
    transition(
        &mem,
        belief,
        CanonizationStatus::Canonical,
        CanonizationStatus::None,
    );
    let r = mem
        .recall_detailed(query("brand y chargers"))
        .await
        .unwrap();
    assert!(!kinds(&r, belief).contains(&AnnotationKind::Contradiction));
    // NEWER may still be a hit (the recent leg finds fresh writes), but not
    // as a companion.
    assert!(!kinds(&r, newer).contains(&AnnotationKind::Contradicts));
    mem.close().await.unwrap();
}

/// Layered scope (#33): the contradiction lives in the `general` layer; a
/// project recall shows the layer's contradicted ruling (keyword evidence
/// admits it) with its prefixed line naming the contradicting concept, but
/// the companion has no evidence of its own, so the floor keeps it out.
/// Nothing from one session is annotated or shown as the other's.
#[tokio::test]
async fn a_layered_recall_keeps_contradictions_inside_their_session() {
    let project = session("contradiction-project").await;
    let general = session("contradiction-general").await;
    project
        .derive(
            &[("brand y chargers ship in march", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap();
    project.settle_daemon().await;
    let (belief, newer) = contradicted(&general).await;

    let r = project
        .recall_layered(&[LayerRef::new(&general)], query("brand y chargers"))
        .await
        .unwrap();
    let shown: Vec<(String, NodeId)> = r
        .hits
        .iter()
        .map(|h| (h.session.as_str().to_string(), h.hit.node_id))
        .collect();
    assert!(
        shown.contains(&("contradiction-general".to_string(), belief)),
        "{shown:?}"
    );
    assert!(
        !shown.iter().any(|(_, id)| *id == newer),
        "a floored companion is not shown: {shown:?}"
    );
    let line = r
        .warnings
        .iter()
        .find(|w| w.contains("Contradicted"))
        .expect("the layer's contradiction line");
    assert!(
        line.starts_with("[contradiction-general] ⚠ Contradicted at"),
        "{line}"
    );
    assert!(line.contains(&format::short_id(newer)), "{line}");
    // The project's own hit carries no contradiction annotation.
    for (h, d) in r.hits.iter().zip(&r.detail.detailed) {
        if h.session.as_str() == "contradiction-project" {
            assert!(
                !d.annotations.iter().any(|a| matches!(
                    a.kind,
                    AnnotationKind::Contradiction | AnnotationKind::Contradicts
                )),
                "{d:?}"
            );
        }
    }
    project.close().await.unwrap();
    general.close().await.unwrap();
}

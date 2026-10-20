//! #19(c) on the `Memory` surface: an `against` write on a promoted belief
//! warns on the next recall and pulls the contradicting concept in; since
//! #179 it keeps warning once the belief is demoted, and an ordinary stale
//! fact is never shown without its correction; and a layered
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
    session_on(name, Arc::new(MemoryStore::new())).await
}

async fn session_on(name: &str, store: Arc<dyn GraphStore>) -> Memory {
    Memory::builder()
        .session(name)
        .agent("agent-a")
        .config(Config {
            daemon_tick_interval: Duration::from_secs(3_600),
            ..Config::default()
        })
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
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
async fn an_against_write_on_a_canonical_belief_warns_and_still_warns_once_demoted() {
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

    // Demoted (#179): the warning stays, and the contradicting concept is
    // still shown beside the belief, now as an ordinary contradicted hit
    // that earned its own slot. No daemon cycle in between (the tick is an
    // hour and nothing woke it): read-time revalidation keeps the entry.
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
    assert!(
        kinds(&r, belief).contains(&AnnotationKind::Contradiction),
        "{}",
        r.context
    );
    assert_eq!(kinds(&r, newer), vec![AnnotationKind::Contradicts]);
    mem.close().await.unwrap();
}

/// The paper's "stale twice" case (#179, efficacy run 2026-09-07): the
/// old embedder fact has six dependents, so it outranks the newer fact that
/// was written with `against = [old]`. Before #179 a recall returned the
/// stale fact first with no warning, and at `top_k = 1` alone. Now it
/// carries the contradiction line and the newer fact comes with it.
#[tokio::test]
async fn the_stale_fact_is_never_returned_without_its_correction() {
    const OLD: &str = "the dogfood embedder is llama.cpp bge-m3 q8_0 on port 8080";
    const NEW: &str = "the dogfood embedder moved to candle bge-m3 on metal";
    let mem = session("contradiction-stale-twice").await;
    mem.derive(&[(OLD, ConceptType::Logic)], &ParentOf::none())
        .await
        .unwrap();
    let old = node_of(&mem, OLD);
    for n in 0..6 {
        let dependent = format!("nightly probe {n} passes on the rig");
        let submitted = mem
            .derive_cited_async_as(
                &AgentId::new("agent-a"),
                &[(dependent.as_str(), ConceptType::Observation)],
                &ParentOf::none(),
                &[ConceptCitations {
                    content: dependent.clone(),
                    depends_on: vec![format::short_id(old)],
                    against: Vec::new(),
                }],
                None,
            )
            .await
            .unwrap();
        let answer = mem
            .pipeline()
            .wait(
                &AgentId::new("agent-a"),
                submitted.receipt,
                RECEIPT_WAIT_MAX,
            )
            .await;
        assert!(answer.is_settled(), "{}", answer.tag());
    }
    let agent_b = AgentId::new("agent-b");
    let submitted = mem
        .derive_cited_async_as(
            &agent_b,
            &[(NEW, ConceptType::Logic)],
            &ParentOf::none(),
            &[ConceptCitations {
                content: NEW.to_string(),
                depends_on: Vec::new(),
                against: vec![format::short_id(old)],
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
    let new = node_of(&mem, NEW);

    for top_k in [1, 2, 5] {
        let r = mem
            .recall_detailed(RecallQuery {
                top_k,
                ..query("what is the dogfood embedder")
            })
            .await
            .unwrap();
        let ids: Vec<NodeId> = r.hits.iter().map(|h| h.node_id).collect();
        assert!(
            ids.contains(&old),
            "top_k {top_k}: the old fact ranks in: {ids:?}\n{}",
            r.context
        );
        assert!(
            ids.contains(&new),
            "top_k {top_k}: the stale fact came back without its correction:\n{}",
            r.context
        );
        assert!(kinds(&r, old).contains(&AnnotationKind::Contradiction));
        assert_eq!(kinds(&r, new), vec![AnnotationKind::Contradicts]);
        let line = r
            .warnings
            .iter()
            .find(|w| w.starts_with("⚠ Contradicted at"))
            .expect("a contradiction line");
        assert!(line.contains(&format::short_id(new)), "{line}");
        assert!(
            ids.len() <= top_k + 1,
            "top_k {top_k}: at most one extra: {ids:?}"
        );
    }
    // At top_k 1 the stale fact ranks first, as in the paper's run. Its
    // fresh dependents also make it a high-risk hot member (forced, no
    // slot), so the correction ranks into the free slot here rather than
    // as a slotless companion; either way it is shown, exactly once.
    let r = mem
        .recall_detailed(RecallQuery {
            top_k: 1,
            ..query("what is the dogfood embedder")
        })
        .await
        .unwrap();
    assert_eq!(r.hits[0].node_id, old, "{}", r.context);
    assert_eq!(r.hits.iter().filter(|h| h.node_id == new).count(), 1);
    let companion = r.detailed.iter().find(|h| h.node_id == new).unwrap();
    let old_forced = kinds(&r, old).contains(&AnnotationKind::Hot);
    assert_eq!(
        companion.companion_of,
        if old_forced { Vec::new() } else { vec![old] },
        "{}",
        r.context
    );
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
    let general = session_on(
        "contradiction-general",
        Arc::new(crate::test_util::GraphRanked(
            crate::test_util::VectorSearchable(Arc::new(MemoryStore::new())),
        )),
    )
    .await;
    project
        .derive(
            &[("brand y chargers ship in march", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap();
    project.settle_daemon().await;
    let (belief, newer) = contradicted(&general).await;
    crate::test_util::seed_layer_match(&general, BELIEF, "brand y chargers").await;

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

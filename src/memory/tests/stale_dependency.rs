//! #207 on the `Memory` surface: a memory whose own `depends_on` cites a
//! concept that a newer concept contradicts is recalled with a warning
//! naming both, and nothing else about the recall changes; also through a
//! layered recall.

use super::contradiction::{kinds, node_of, session, session_on};
use super::*;
use crate::graph::cite::ConceptCitations;
use crate::memory::LayerRef;
use crate::recall::detail::AnnotationKind;
use crate::recall::format;
use crate::writeq::RECEIPT_WAIT_MAX;

/// The API element (E).
const ELEMENT: &str = "GET /api/v1/memo (list all)";
/// The memory that rests on it (M).
const MEMORY: &str = "list-all returns every memo";
/// The newer concept contradicting E (C).
const CHANGE: &str = "list-all removed in v0.22; the replacement pages at 10";
/// Matches M's keywords only.
const QUERY: &str = "returns every";

/// Write one concept as `agent`, citing and contradicting by short id,
/// and wait until it is applied.
async fn write(
    mem: &Memory,
    agent: &str,
    content: &str,
    depends_on: &[NodeId],
    against: &[NodeId],
) -> NodeId {
    let agent = AgentId::new(agent);
    let submitted = mem
        .derive_cited_async_as(
            &agent,
            &[(content, ConceptType::Logic)],
            &ParentOf::none(),
            &[ConceptCitations {
                content: content.to_string(),
                depends_on: depends_on.iter().map(|id| format::short_id(*id)).collect(),
                against: against.iter().map(|id| format::short_id(*id)).collect(),
            }],
            None,
        )
        .await
        .unwrap();
    let answer = mem
        .pipeline()
        .wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX)
        .await;
    assert!(answer.is_settled(), "{}", answer.tag());
    node_of(mem, content)
}

/// Phase 1 only: without traversal, E (which M cites) is not expanded in,
/// so the result is exactly what ranks for the query.
fn flat(text: &str) -> RecallQuery {
    RecallQuery {
        traversal_depth: 0,
        ..query(text)
    }
}

fn ids(r: &crate::recall::detail::DetailedRecall) -> Vec<NodeId> {
    r.hits.iter().map(|h| h.node_id).collect()
}

/// The issue's example end to end: before C, M is recalled clean; after
/// C is written `against: [E]`, M carries one line naming E and C, and
/// nothing is added on M's account. (That membership is unchanged is
/// pinned exactly in `recall::assemble`'s tests.)
#[tokio::test]
async fn a_memory_citing_a_contradicted_element_is_warned_once_it_is_contradicted() {
    let mem = session("stale-by-dependency").await;
    let element = write(&mem, "agent-a", ELEMENT, &[], &[]).await;
    let memory = write(&mem, "agent-a", MEMORY, &[element], &[]).await;
    mem.settle_daemon().await;

    let before = mem.recall_detailed(flat(QUERY)).await.unwrap();
    assert!(ids(&before).contains(&memory), "{}", before.context);
    assert!(kinds(&before, memory).is_empty(), "{}", before.context);
    assert!(!before.context.contains("Rests on"), "{}", before.context);

    let change = write(&mem, "agent-b", CHANGE, &[], &[element]).await;
    mem.settle_daemon().await;

    let after = mem.recall_detailed(flat(QUERY)).await.unwrap();
    // Every hit from before is still there. The recent leg ranks E in on
    // both sides; once E is contradicted, #179 shows C beside E. Nothing
    // is added for M: C is there for E alone.
    let (was, now) = (ids(&before), ids(&after));
    assert!(was.iter().all(|id| now.contains(id)), "{}", after.context);
    assert!(
        now.iter().all(|id| was.contains(id) || *id == change),
        "{}",
        after.context
    );
    if let Some(c) = after.detailed.iter().find(|h| h.node_id == change) {
        assert!(!c.companion_of.contains(&memory));
        assert!(!c.contradicts.contains(&memory));
    }
    assert_eq!(
        kinds(&after, memory),
        vec![AnnotationKind::RestsOnContradicted]
    );
    let line = format::rests_on_contradicted_warning(element, ELEMENT, change, CHANGE);
    assert_eq!(
        after
            .warnings
            .iter()
            .filter(|w| w.starts_with("⚠ Rests on"))
            .collect::<Vec<_>>(),
        vec![&line]
    );
    assert!(after.context.contains(&line), "{}", after.context);
    assert!(line.contains(&format::short_id(element)), "{line}");
    assert!(line.contains(&format::short_id(change)), "{line}");
    let hit = after.detailed.iter().find(|h| h.node_id == memory).unwrap();
    assert!(hit.companion_of.is_empty());
    // The structured detail carries it as its own kind.
    let json = serde_json::to_value(hit).unwrap();
    assert_eq!(json["annotations"][0]["kind"], "rests_on_contradicted");
    assert_eq!(json["annotations"][0]["text"], line.as_str());

    let expanded = mem.recall_detailed(query(QUERY)).await.unwrap();
    assert_eq!(
        kinds(&expanded, memory),
        vec![AnnotationKind::RestsOnContradicted]
    );
    assert!(expanded.warnings.contains(&line), "{}", expanded.context);
    mem.close().await.unwrap();
}

/// A layered recall carries a layer hit's stale-citation line, prefixed
/// with its session, and adds neither the cited nor the contradicting
/// concept.
#[tokio::test]
async fn a_layered_recall_carries_a_layer_hits_stale_citation() {
    let project = session("stale-dep-project").await;
    let general = session_on(
        "stale-dep-general",
        Arc::new(crate::test_util::GraphRanked(
            crate::test_util::VectorSearchable(Arc::new(MemoryStore::new())),
        )),
    )
    .await;
    project
        .derive(
            &[("the memo service ships in march", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap();
    project.settle_daemon().await;
    let element = write(&general, "agent-a", ELEMENT, &[], &[]).await;
    let memory = write(&general, "agent-a", MEMORY, &[element], &[]).await;
    let change = write(&general, "agent-b", CHANGE, &[], &[element]).await;
    crate::test_util::seed_layer_match(&general, MEMORY, QUERY).await;

    let r = project
        .recall_layered(&[LayerRef::new(&general)], query(QUERY))
        .await
        .unwrap();
    let shown: Vec<NodeId> = r.hits.iter().map(|h| h.hit.node_id).collect();
    assert!(shown.contains(&memory), "{shown:?}");
    assert!(!shown.contains(&element), "{shown:?}");
    assert!(!shown.contains(&change), "{shown:?}");
    let detail = r
        .hits
        .iter()
        .zip(&r.detail.detailed)
        .find(|(h, _)| h.hit.node_id == memory)
        .map(|(_, d)| d)
        .unwrap();
    assert_eq!(
        detail
            .annotations
            .iter()
            .map(|a| a.kind)
            .collect::<Vec<_>>(),
        vec![AnnotationKind::RestsOnContradicted]
    );
    let line = format::rests_on_contradicted_warning(element, ELEMENT, change, CHANGE);
    assert!(
        r.warnings.contains(&format!("[stale-dep-general] {line}")),
        "{:?}",
        r.warnings
    );
    project.close().await.unwrap();
    general.close().await.unwrap();
}

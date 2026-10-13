//! #20 on the `Memory` surface: the historical `superseded` line on a
//! recalled demoted hit. It names the superseding concept (with a quote
//! while it is in the graph, by id alone once collected), it never forces a
//! hit into the result, and it leaves once the concept is Canonical again
//! or its latest demotion is a budget one.

use super::*;
use crate::recall::detail::AnnotationKind;
use crate::recall::format;
use crate::types::DemotionReason;

const BELIEF: &str = "a failed flush retries the same batch until it lands";
const NEWER: &str = "flushes give up after three attempts and surface the error";
const OTHER: &str = "the queue drains before shutdown";

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
        .match_strategy(MatchStrategy::Hybrid)
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

fn transition(
    mem: &Memory,
    id: NodeId,
    from: CanonizationStatus,
    to: CanonizationStatus,
    reason: Option<DemotionReason>,
) {
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
            reason,
        })
        .unwrap();
}

fn kinds(r: &crate::recall::detail::DetailedRecall, id: NodeId) -> Vec<AnnotationKind> {
    r.detailed
        .iter()
        .find(|h| h.node_id == id)
        .map(|h| h.annotations.iter().map(|a| a.kind).collect())
        .unwrap_or_default()
}

fn line(r: &crate::recall::detail::DetailedRecall) -> Option<&String> {
    r.warnings.iter().find(|w| w.starts_with("⤓ Superseded at"))
}

#[tokio::test]
async fn a_superseded_hit_carries_its_history_without_being_forced() {
    use CanonizationStatus::{Canonical, Venerable};
    let mem = session("superseded-e2e").await;
    mem.derive_as(
        &AgentId::new("agent-a"),
        &[
            (BELIEF, ConceptType::Logic),
            (NEWER, ConceptType::Logic),
            (OTHER, ConceptType::Logic),
        ],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    let (belief, newer) = (node_of(&mem, BELIEF), node_of(&mem, NEWER));
    transition(&mem, belief, CanonizationStatus::None, Venerable, None);
    transition(&mem, belief, Venerable, Canonical, None);
    transition(
        &mem,
        belief,
        Canonical,
        CanonizationStatus::None,
        Some(DemotionReason::Superseded { by: newer }),
    );

    let r = mem
        .recall_detailed(query("failed flush batch"))
        .await
        .unwrap();
    assert_eq!(
        kinds(&r, belief),
        vec![AnnotationKind::Superseded],
        "{}",
        r.context
    );
    let text = line(&r).expect("the superseded line");
    assert!(text.contains(&format!("\"{NEWER}\"")), "{text}");
    assert!(text.contains(&format::short_id(newer)), "{text}");
    assert!(r.context.contains(text.as_str()), "rendered in the block");

    // History, not a hot condition: a recall that does not match the
    // belief does not pull it in.
    let r = mem
        .recall_detailed(RecallQuery {
            top_k: 1,
            ..query("queue drains shutdown")
        })
        .await
        .unwrap();
    assert!(
        r.detailed.iter().all(|h| h.node_id != belief),
        "{}",
        r.context
    );

    // The superseding concept collected: the line names it by id alone.
    mem.graph.write().remove_node(newer).unwrap();
    let r = mem
        .recall_detailed(query("failed flush batch"))
        .await
        .unwrap();
    let text = line(&r).expect("still there");
    assert!(!text.contains('"'), "{text}");
    assert!(text.contains(&format::short_id(newer)), "{text}");

    // Canonical again: gone.
    transition(&mem, belief, CanonizationStatus::None, Venerable, None);
    transition(&mem, belief, Venerable, Canonical, None);
    let r = mem
        .recall_detailed(query("failed flush batch"))
        .await
        .unwrap();
    assert!(line(&r).is_none(), "{}", r.context);
    assert!(!kinds(&r, belief).contains(&AnnotationKind::Superseded));

    // A later budget demotion: the line names why it is down now, so none.
    transition(&mem, belief, Canonical, CanonizationStatus::None, None);
    let r = mem
        .recall_detailed(query("failed flush batch"))
        .await
        .unwrap();
    assert!(line(&r).is_none(), "{}", r.context);
}

/// Inspect's canonization section names the latest demotion's reason and
/// source (#20): a supersession quotes its source while it is in the graph
/// and names it by id once collected; a budget demotion says so.
#[tokio::test]
async fn inspect_names_the_latest_demotion_and_its_source() {
    use CanonizationStatus::{Canonical, Venerable};
    let mem = session("superseded-inspect").await;
    mem.derive_as(
        &AgentId::new("agent-a"),
        &[(BELIEF, ConceptType::Logic), (NEWER, ConceptType::Logic)],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    let (belief, newer) = (node_of(&mem, BELIEF), node_of(&mem, NEWER));
    let section = |mem: &Memory| crate::canon::with_last_demotion(None, &mem.graph.read(), belief);
    assert_eq!(section(&mem), None, "never demoted: nothing to say");
    transition(&mem, belief, CanonizationStatus::None, Venerable, None);
    transition(&mem, belief, Venerable, Canonical, None);
    transition(
        &mem,
        belief,
        Canonical,
        CanonizationStatus::None,
        Some(DemotionReason::Superseded { by: newer }),
    );
    let text = section(&mem).unwrap();
    assert!(
        text.contains(&format!(
            "superseded by \"{NEWER}\" (id {})",
            format::short_id(newer)
        )),
        "{text}"
    );
    let with_admission = crate::canon::with_last_demotion(
        Some("canonization (swarm): None\n".into()),
        &mem.graph.read(),
        belief,
    )
    .unwrap();
    assert!(
        with_admission.starts_with("canonization (swarm): None\nlast demotion "),
        "{with_admission}"
    );
    mem.graph.write().remove_node(newer).unwrap();
    assert!(section(&mem).unwrap().contains("no longer in memory"));
    transition(&mem, belief, CanonizationStatus::None, Venerable, None);
    transition(&mem, belief, Venerable, Canonical, None);
    transition(&mem, belief, Canonical, CanonizationStatus::None, None);
    assert!(section(&mem).unwrap().ends_with(": budget\n"));
}

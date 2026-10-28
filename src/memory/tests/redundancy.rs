//! #181 end to end: `Config::recall_selection` reaches assembly through the
//! session's daemon, for a plain recall and for each tier of a layered one.

use super::*;
use crate::memory::LayerRef;
use crate::recall::redundancy::{RecallSelection, RedundancySignal};

/// The efficacy run's duplicate, stored as an Entity and an Observation.
const DUPLICATE: &str = "dogfood finding: most concepts in the dogfood store carry no durable \
                         embedding";
/// A distinct fact sharing one query word with it.
const OTHER: &str = "the dogfood rig serves one http writer";

/// The Observation first: the canonical-key matcher skips Observations, so
/// the Entity derived after it is a second concept (an Observation derived
/// after the Entity would match it instead).
const DUPLICATED: [(&str, ConceptType); 3] = [
    (DUPLICATE, ConceptType::Observation),
    (DUPLICATE, ConceptType::Entity),
    (OTHER, ConceptType::Logic),
];

async fn session(
    store: Arc<dyn GraphStore>,
    name: &str,
    skip_redundant: bool,
    concepts: &[(&str, ConceptType)],
) -> Memory {
    let mem = Memory::builder()
        .session(name)
        .agent("agent-a")
        .config(Config {
            daemon_tick_interval: Duration::from_secs(3_600),
            // `similar_signals` stays at its default (off): these tests
            // run the shipped configuration, identical content only.
            recall_selection: RecallSelection {
                skip_redundant,
                ..RecallSelection::default()
            },
            ..Config::default()
        })
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .expect("build");
    // One derive per concept, so each copy is its own write, as in the
    // efficacy store.
    for c in concepts {
        let out = mem
            .derive(std::slice::from_ref(c), &ParentOf::none())
            .await
            .unwrap();
        assert_eq!(out.created.len(), 1, "{c:?}: {out:?}");
    }
    mem.settle_daemon().await;
    mem
}

fn query_two() -> RecallQuery {
    RecallQuery {
        query: "dogfood durable embedding".into(),
        top_k: 2,
        max_tokens: 500,
        traversal_depth: 0,
    }
}

#[tokio::test]
async fn the_setting_reaches_assembly_through_the_daemon() {
    let off = session(
        Arc::new(MemoryStore::new()),
        "redundancy-off",
        false,
        &DUPLICATED,
    )
    .await;
    let plain = off.recall_detailed(query_two()).await.unwrap();
    assert_eq!(plain.hits.len(), 2);
    assert!(
        plain.hits.iter().all(|h| h.content == DUPLICATE),
        "premise: both copies outrank the other fact: {:#?}",
        plain.hits
    );
    assert!(plain
        .detailed
        .iter()
        .all(|d| d.skipped_redundant.is_empty()));

    let on = session(
        Arc::new(MemoryStore::new()),
        "redundancy-on",
        true,
        &DUPLICATED,
    )
    .await;
    let r = on.recall_detailed(query_two()).await.unwrap();
    let contents: Vec<&str> = r.hits.iter().map(|h| h.content.as_str()).collect();
    assert_eq!(contents, [DUPLICATE, OTHER]);
    let skipped = &r.detailed[0].skipped_redundant;
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].signal, RedundancySignal::SameContent);
    let short = crate::recall::format::short_id(skipped[0].node_id);
    assert!(
        r.context
            .lines()
            .next()
            .unwrap()
            .ends_with(&format!("also {short})")),
        "{}",
        r.context
    );
    // The public recall returns the same hits and context.
    let public = on.recall(query_two()).await.unwrap();
    assert_eq!(public.context, r.context);
    off.close().await.unwrap();
    on.close().await.unwrap();
}

/// A layer applies its own session's setting inside its own assembly, and
/// the merge keeps the `also` item on the layer's hit, after `from`.
#[tokio::test]
async fn a_layered_recall_skips_inside_each_tier() {
    let store = || -> Arc<dyn GraphStore> {
        Arc::new(crate::test_util::GraphRanked(
            crate::test_util::VectorSearchable(Arc::new(MemoryStore::new())),
        ))
    };
    let own = session(
        store(),
        "redundancy-own",
        true,
        &[("the caller keeps its own dogfood notes", ConceptType::Logic)],
    )
    .await;
    let general = session(store(), "redundancy-general", true, &DUPLICATED).await;
    let r = own
        .recall_layered(
            &[LayerRef::new(&general).with_min_similarity(0.0)],
            RecallQuery {
                top_k: 4,
                ..query_two()
            },
        )
        .await
        .unwrap();
    let dups: Vec<_> = r
        .hits
        .iter()
        .filter(|h| h.hit.content == DUPLICATE)
        .collect();
    assert_eq!(dups.len(), 1, "{:?}", r.context);
    let dup = dups[0];
    assert_eq!(dup.session, *general.session());
    assert_eq!(dup.skipped_redundant.len(), 1);
    let short = crate::recall::format::short_id(dup.skipped_redundant[0].node_id);
    assert!(
        r.context
            .contains(&format!("from redundancy-general, also {short})")),
        "{}",
        r.context
    );
    own.close().await.unwrap();
    general.close().await.unwrap();
}

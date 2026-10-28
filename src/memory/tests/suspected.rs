//! #198 on the `Memory` surface: a hybrid write that the identifier guard
//! refuses to merge, and whose text differs from the older fact in one
//! value-slot span, records a suspected supersession; recall names it on the
//! older hit as a line under a validated embedding contract (or with the
//! line forced on) and as the structured field only otherwise.

use super::*;
use crate::recall::detail::AnnotationKind;
use crate::recall::format;
use crate::recall::suspected::SuspectedLine;

const OLD: &str = "the embedder listens on port 8080";
const NEW: &str = "the embedder listens on port 8081";

/// Every text embeds to the same unit vector, so every pair is a vector
/// hit at cosine 1.0: what decides is the guard and the trigger.
struct Constant;

#[async_trait]
impl Embedder for Constant {
    fn dimensions(&self) -> usize {
        1024
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        let mut v = vec![0.0; 1024];
        v[0] = 1.0;
        Ok(v)
    }
}

async fn hybrid(name: &str, kind: &str, line: SuspectedLine) -> Memory {
    Memory::builder()
        .session(name)
        .agent("agent-a")
        .config(Config {
            daemon_tick_interval: Duration::from_secs(3_600),
            suspected_supersession_line: line,
            ..Config::default()
        })
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(crate::test_util::GraphRanked(
            crate::test_util::VectorSearchable(Arc::new(MemoryStore::new())),
        )) as Arc<dyn GraphStore>)
        .embedder(Arc::new(Constant) as Arc<dyn Embedder>)
        .embedding_contract(contract(kind, 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build")
}

fn node_of(mem: &Memory, content: &str) -> NodeId {
    super::contradiction::node_of(mem, content)
}

/// Write OLD, then NEW a moment later, and return their ids.
async fn write_pair(mem: &Memory) -> (NodeId, NodeId) {
    mem.derive(&[(OLD, ConceptType::Logic)], &ParentOf::none())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let out = mem
        .derive(&[(NEW, ConceptType::Logic)], &ParentOf::none())
        .await
        .unwrap();
    assert_eq!(out.suspected, 1, "{out:?}");
    assert!(out.semantic_merged.is_empty());
    mem.settle_daemon().await;
    (node_of(mem, OLD), node_of(mem, NEW))
}

#[tokio::test]
async fn a_validated_contract_shows_the_line() {
    let mem = hybrid("suspected-bge", "bge_m3", SuspectedLine::Validated).await;
    let (old, new) = write_pair(&mem).await;
    assert!(crate::graph::suspected::is_suspected_pair(
        &mem.graph().read(),
        old,
        new
    ));
    let r = mem
        .recall_detailed(query("embedder listens"))
        .await
        .unwrap();
    let hit = r.detailed.iter().find(|h| h.node_id == old).unwrap();
    assert_eq!(hit.possibly_superseded_by, Some(new));
    assert_eq!(
        super::contradiction::kinds(&r, old),
        vec![AnnotationKind::PossiblySuperseded]
    );
    let line = format::possibly_superseded_warning(new, NEW);
    assert!(r.warnings.contains(&line), "{:?}", r.warnings);
    assert!(r.context.contains(&line), "{}", r.context);
    assert!(super::contradiction::kinds(&r, new).is_empty());
    mem.close().await.unwrap();
}

/// An unvalidated contract (the test fixture, like EmbeddingGemma 2):
/// the structured field only; `on` shows the line anyway, `off` hides it
/// from a validated one.
#[tokio::test]
async fn the_line_follows_the_gate() {
    for (kind, mode, shown) in [
        ("fixture", SuspectedLine::Validated, false),
        ("fixture", SuspectedLine::On, true),
        ("bge_m3", SuspectedLine::Off, false),
    ] {
        let mem = hybrid(&format!("suspected-gate-{kind}-{mode}"), kind, mode).await;
        let (old, new) = write_pair(&mem).await;
        let r = mem
            .recall_detailed(query("embedder listens"))
            .await
            .unwrap();
        let hit = r.detailed.iter().find(|h| h.node_id == old).unwrap();
        assert_eq!(hit.possibly_superseded_by, Some(new), "{kind}/{mode}");
        let kinds = super::contradiction::kinds(&r, old);
        assert_eq!(
            kinds.contains(&AnnotationKind::PossiblySuperseded),
            shown,
            "{kind}/{mode}"
        );
        assert_eq!(
            r.context.contains("Possibly superseded"),
            shown,
            "{kind}/{mode}: {}",
            r.context
        );
        mem.close().await.unwrap();
    }
}

/// The edge is flushed with the write: the closed session reloads with it.
#[tokio::test]
async fn the_edge_reaches_the_store_with_the_write() {
    let mem = hybrid("suspected-flush", "bge_m3", SuspectedLine::Validated).await;
    let (old, new) = write_pair(&mem).await;
    let store = mem.store().clone();
    let session = mem.session().clone();
    mem.close().await.unwrap();
    let snap = store.load_session(&session).await.unwrap();
    assert!(snap.edges.iter().any(|e| {
        e.edge_type == crate::types::EdgeType::SuspectedSupersedes
            && e.source == new
            && e.target == old
    }));
}

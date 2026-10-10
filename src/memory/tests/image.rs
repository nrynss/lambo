//! #22 PR 3: `Memory::derive_image_as` and `derive_image_async_as` — the
//! call-path checks, the identity rules and the queued and replayed image
//! derive.

use super::replay::VectorSearchable;
use super::*;
use crate::embed::{png_with_label, ImageMime};
use crate::graph::image::{image_content, ImageDerive, ImagePayload};
use crate::types::{Concept, EmbeddingSource, ImageMimeWire, SourceModality, VectorOrigin};

const DIM: usize = 1024;

fn live() -> EmbeddingContract {
    contract("fixture", DIM)
}

async fn image_memory(store: Arc<MemoryStore>, session: &str) -> Memory {
    Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(store)) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(live())
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build")
}

fn agent() -> AgentId {
    AgentId::from("agent-a")
}

fn bytes_derive<'a>(caption: &'a str, png: &'a [u8], id: Option<&'a str>) -> ImageDerive<'a> {
    ImageDerive {
        caption,
        concept_type: ConceptType::Resource,
        image_id: id,
        payload: ImagePayload::Bytes(
            crate::surface::image::validate(png, "image/png").expect("valid png"),
        ),
        parent_of: &[],
        event_time: None,
    }
}

fn vector_derive<'a>(
    caption: &'a str,
    values: Vec<f32>,
    declared: EmbeddingContract,
    id: Option<&'a str>,
) -> ImageDerive<'a> {
    ImageDerive {
        caption,
        concept_type: ConceptType::Resource,
        image_id: id,
        payload: ImagePayload::Vector { values, declared },
        parent_of: &[],
        event_time: None,
    }
}

fn image_concepts(mem: &Memory) -> Vec<Concept> {
    let g = mem.graph().read();
    let mut out: Vec<Concept> = g
        .concepts()
        .filter(|c| c.embedding_source.is_some())
        .cloned()
        .collect();
    out.sort_by(|a, b| a.content.cmp(&b.content));
    out
}

fn sha_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    crate::graph::image::hex(&Sha256::digest(bytes))
}

#[tokio::test]
async fn a_server_embedded_image_is_one_concept_with_its_vector_and_source() {
    let store = Arc::new(MemoryStore::new());
    let mem = image_memory(store.clone(), "image-bytes").await;
    let png = png_with_label("red silk saree");
    let out = mem
        .derive_image_as(&agent(), bytes_derive("render 17", &png, None))
        .await
        .unwrap();
    assert_eq!((out.created.len(), out.embedded), (1, 1));

    let digest = sha_hex(&png);
    let [c] = image_concepts(&mem).try_into().unwrap();
    assert_eq!(c.content, image_content("render 17", &digest[..16]));
    assert_eq!(c.concept_type, ConceptType::Resource);
    let want = FixtureEmbedder::new().embed_sync("red silk saree");
    let got = c.embedding.clone().unwrap();
    assert!(
        got.iter().zip(&want).all(|(a, b)| (a - b).abs() < 1e-6),
        "the image's vector, renormalized"
    );
    assert_eq!(
        c.embedding_source,
        Some(EmbeddingSource {
            modality: SourceModality::Image,
            origin: VectorOrigin::Server,
            sha256: Some(digest),
            mime: Some(ImageMimeWire::from(ImageMime::Png)),
        })
    );
    assert_eq!(mem.graph().read().embedding(), Some(&live()));

    // Durable through the flush and a reload.
    mem.close().await.unwrap();
    let snap = store
        .load_session(&SessionId::new("image-bytes"))
        .await
        .unwrap();
    let stored = snap.concepts.iter().find(|s| s.id == c.id).unwrap();
    assert_eq!(stored.embedding, c.embedding);
    assert_eq!(stored.embedding_source, c.embedding_source);
}

#[tokio::test]
async fn a_client_vector_is_renormalized_and_labelled_client() {
    let mem = image_memory(Arc::new(MemoryStore::new()), "image-client").await;
    let unit = FixtureEmbedder::new().embed_sync("red silk saree");
    let scaled: Vec<f32> = unit.iter().map(|x| x * 3.0).collect();
    mem.derive_image_as(&agent(), vector_derive("render 17", scaled, live(), None))
        .await
        .unwrap();
    let [c] = image_concepts(&mem).try_into().unwrap();
    let got = c.embedding.unwrap();
    let norm: f32 = got.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-5, "renormalized: {norm}");
    assert_eq!(
        c.content,
        image_content("render 17", &crate::graph::image::vector_id(&got))
    );
    assert_eq!(
        c.embedding_source,
        Some(EmbeddingSource {
            modality: SourceModality::Image,
            origin: VectorOrigin::Client,
            sha256: None,
            mime: None,
        })
    );
    mem.close().await.unwrap();
}

/// AC4 core, on the call path: every mismatch is a `Config` refusal before
/// anything is written (not even an interaction), and no message quotes the
/// vector.
#[tokio::test]
async fn a_submitted_vector_that_does_not_fit_is_refused_on_the_call_path() {
    let mem = image_memory(Arc::new(MemoryStore::new()), "image-refused").await;
    let good = vec![0.25_f32; DIM];
    let mut nan = good.clone();
    nan[7] = f32::NAN;
    let mut inf = good.clone();
    inf[7] = f32::INFINITY;
    let cases: Vec<(&str, Vec<f32>, EmbeddingContract)> = vec![
        ("kind", good.clone(), contract("other", DIM)),
        (
            "model",
            good.clone(),
            EmbeddingContract {
                model: Some("v2".into()),
                ..live()
            },
        ),
        ("dim", vec![0.25; 512], contract("fixture", 512)),
        ("width", vec![0.25; 512], live()),
        ("non-finite", nan, live()),
        ("infinite", inf, live()),
        ("zero norm", vec![0.0; DIM], live()),
    ];
    for (what, values, declared) in cases {
        let err = mem
            .derive_image_as(&agent(), vector_derive("render 17", values, declared, None))
            .await
            .expect_err(what);
        assert!(matches!(err, LamboError::Config(_)), "{what}: {err:?}");
        assert!(!err.to_string().contains("0.25"), "{what}: {err}");
    }
    let async_err = mem
        .derive_image_async_as(
            &agent(),
            vector_derive("render 17", vec![0.0; DIM], live(), None),
        )
        .await
        .expect_err("the acknowledged path refuses on the call path too");
    assert!(matches!(async_err, LamboError::Config(_)), "{async_err:?}");
    {
        let g = mem.graph().read();
        assert_eq!(g.concepts().count(), 0);
        assert_eq!(g.temporal_chain().len(), 0, "not even an interaction");
    }
    mem.close().await.unwrap();
}

/// Design Q16 and the caller-fixable rules: each is a `Config` refusal with
/// nothing written.
#[tokio::test]
async fn an_image_derive_refuses_what_it_cannot_honour() {
    let png = png_with_label("red silk saree");

    // Not hybrid.
    let canonical = Memory::builder()
        .session("image-canonical")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(Arc::new(MemoryStore::new()))) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(live())
        .match_strategy(MatchStrategy::Canonical)
        .build()
        .await
        .unwrap();
    let err = canonical
        .derive_image_as(&agent(), bytes_derive("render 17", &png, None))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, LamboError::Config(m) if m.contains("hybrid")),
        "{err:?}"
    );
    canonical.close().await.unwrap();

    // No vector search: a bare MemoryStore.
    let plain = Memory::builder()
        .session("image-no-vectors")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(MemoryStore::new()) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(live())
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .unwrap();
    let err = plain
        .derive_image_as(&agent(), bytes_derive("render 17", &png, None))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, LamboError::Config(m) if m.contains("VECTOR_SEARCH")),
        "{err:?}"
    );
    plain.close().await.unwrap();

    // A text-only embedder, for bytes (a client vector still works).
    let text_only = Memory::builder()
        .session("image-text-only")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(Arc::new(MemoryStore::new()))) as Arc<dyn GraphStore>)
        .embedder(Arc::new(TextOnly(FixtureEmbedder::new())) as Arc<dyn Embedder>)
        .embedding_contract(live())
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .unwrap();
    let err = text_only
        .derive_image_as(&agent(), bytes_derive("render 17", &png, None))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, LamboError::Config(m) if m.contains("does not embed images")),
        "{err:?}"
    );
    text_only
        .derive_image_as(
            &agent(),
            vector_derive("render 17", vec![0.5; DIM], live(), Some("r17")),
        )
        .await
        .expect("a client vector needs no image embedder");
    text_only.close().await.unwrap();

    let mem = image_memory(Arc::new(MemoryStore::new()), "image-rules").await;
    let observation = ImageDerive {
        concept_type: ConceptType::Observation,
        ..bytes_derive("render 17", &png, None)
    };
    let rules: Vec<(&str, ImageDerive<'_>)> = vec![
        ("observation", observation),
        ("blank caption", bytes_derive("   ", &png, None)),
        (
            "caption with a suffix",
            bytes_derive("a [image:x] b", &png, None),
        ),
        (
            "id with a dash",
            bytes_derive("render 17", &png, Some("r-17")),
        ),
        (
            "upper-case id",
            bytes_derive("render 17", &png, Some("R17")),
        ),
        ("empty id", bytes_derive("render 17", &png, Some(""))),
        (
            "control character",
            bytes_derive("render\u{0} 17", &png, None),
        ),
    ];
    for (what, derive) in rules {
        let err = mem.derive_image_as(&agent(), derive).await.expect_err(what);
        assert!(matches!(err, LamboError::Config(_)), "{what}: {err:?}");
    }
    assert_eq!(mem.graph().read().concepts().count(), 0);
    mem.close().await.unwrap();
}

/// A wrapper that hides the fixture's image modality.
struct TextOnly(FixtureEmbedder);

#[async_trait]
impl Embedder for TextOnly {
    fn dimensions(&self) -> usize {
        self.0.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.0.embed(text).await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.0.embed_query(text).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.0.as_any()
    }
}

/// Design section 4.2: the same image id re-derives onto its concept; two
/// images sharing one caption stay two concepts.
#[tokio::test]
async fn identity_is_the_caption_and_the_image_id() {
    let mem = image_memory(Arc::new(MemoryStore::new()), "image-identity").await;
    let a = png_with_label("red silk saree");
    let b = png_with_label("blue cotton kurta");

    let first = mem
        .derive_image_as(&agent(), bytes_derive("outfit for onam", &a, Some("o1")))
        .await
        .unwrap()
        .created[0];
    let again = mem
        .derive_image_as(&agent(), bytes_derive("Outfit for Onam", &b, Some("o1")))
        .await
        .unwrap();
    assert_eq!(
        (again.created.len(), again.matched.clone()),
        (0, vec![first]),
        "same caption and id: one concept"
    );
    let second = mem
        .derive_image_as(&agent(), bytes_derive("outfit for onam", &b, Some("o2")))
        .await
        .unwrap();
    assert_eq!(
        second.created.len(),
        1,
        "same caption, other id: two concepts"
    );
    // Default ids: the same bytes land on one concept, other bytes do not.
    let d1 = mem
        .derive_image_as(&agent(), bytes_derive("outfit for onam", &a, None))
        .await
        .unwrap();
    let d2 = mem
        .derive_image_as(&agent(), bytes_derive("outfit for onam", &a, None))
        .await
        .unwrap();
    assert_eq!(d2.matched, d1.created);

    let images = image_concepts(&mem);
    assert_eq!(images.len(), 3);
    assert!(images
        .iter()
        .all(|c| c.content.starts_with("outfit for onam [image:")));
    let first_vector = images
        .iter()
        .find(|c| c.id == first)
        .and_then(|c| c.embedding.clone())
        .unwrap();
    assert_eq!(
        first_vector,
        FixtureEmbedder::new().embed_sync("red silk saree"),
        "the match kept the first image's vector"
    );
    // No Semantic edge touches an image concept.
    {
        let g = mem.graph().read();
        for c in &images {
            assert!(
                g.incident_edges(c.id)
                    .iter()
                    .all(|e| e.edge_type != crate::types::EdgeType::Semantic),
                "{}",
                c.content
            );
        }
    }
    mem.close().await.unwrap();
}

/// The acknowledged path: validated and embedded on the call path, the vector
/// queued as the intent, applied by the lane worker as an image derive.
#[tokio::test]
async fn an_acknowledged_image_derive_applies_and_says_so() {
    let mem = image_memory(Arc::new(MemoryStore::new()), "image-async").await;
    let png = png_with_label("red silk saree");
    let pairs = [("wardrobe", "render 17 [image:r17]")];
    let derive = ImageDerive {
        parent_of: &pairs,
        ..bytes_derive("render 17", &png, Some("r17"))
    };
    let submitted = mem.derive_image_async_as(&agent(), derive).await.unwrap();
    assert_eq!(submitted.kind, crate::writeq::WriteKind::DeriveImage);
    assert_eq!(submitted.kind.tool(), "lambo_derive_image");
    let answer = mem
        .pipeline()
        .wait(&agent(), submitted.receipt, crate::writeq::RECEIPT_WAIT_MAX)
        .await;
    let crate::writeq::ReceiptAnswer::Applied(summary) = answer else {
        panic!("applied: {answer:?}");
    };
    assert_eq!(summary.kind, crate::writeq::WriteKind::DeriveImage);
    assert_eq!(
        summary.summary,
        // The pair's child is the image concept itself, which the pair
        // resolves to and counts as matched, as a text derive does.
        "derived 1 image concept(s): 2 created (2 embedded), 1 matched existing"
    );
    let [c] = image_concepts(&mem).try_into().unwrap();
    assert_eq!(c.content, "render 17 [image:r17]");
    assert_eq!(
        c.embedding,
        Some(FixtureEmbedder::new().embed_sync("red silk saree"))
    );
    mem.close().await.unwrap();
}

/// #22 PR 4: a text concept that already holds the image's key refuses the
/// image derive with its own variant, on the synchronous path, and with its
/// own model-safe sentence on the receipt: it names the fix and the caller's
/// image id, never the text concept's node id or anything else.
#[tokio::test]
async fn an_image_id_held_by_a_text_concept_is_refused_with_the_fix_on_both_paths() {
    let mem = image_memory(Arc::new(MemoryStore::new()), "image-id-taken").await;
    let squat = mem
        .derive(
            &[("Render 17 [IMAGE:r17]", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap();
    let [text_node] = squat.created.try_into().unwrap();
    let png = png_with_label("red silk saree");

    let err = mem
        .derive_image_as(&agent(), bytes_derive("render 17", &png, Some("r17")))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, LamboError::ImageIdTaken(id) if id == "r17"),
        "{err:?}"
    );

    let submitted = mem
        .derive_image_async_as(&agent(), bytes_derive("render 17", &png, Some("r17")))
        .await
        .unwrap();
    let answer = mem
        .pipeline()
        .wait(&agent(), submitted.receipt, crate::writeq::RECEIPT_WAIT_MAX)
        .await;
    let crate::writeq::ReceiptAnswer::Failed(why) = &answer else {
        panic!("failed: {answer:?}");
    };
    assert!(why.contains("choose another image id"), "{why}");
    assert!(why.contains("\"r17\""), "{why}");
    assert!(!why.contains(&text_node.to_string()), "{why}");
    assert!(!why.contains("logged server-side"), "{why}");
    assert!(image_concepts(&mem).is_empty());
    mem.close().await.unwrap();
}

/// The query-embedding cache (#14) only ever holds recall's query vectors:
/// an image derive, sync or queued, puts nothing in it.
#[tokio::test]
async fn an_image_derive_never_touches_the_query_embedding_cache() {
    let mem = image_memory(Arc::new(MemoryStore::new()), "image-cache").await;
    let png = png_with_label("red silk saree");
    mem.derive_image_as(&agent(), bytes_derive("render 17", &png, None))
        .await
        .unwrap();
    let submitted = mem
        .derive_image_async_as(
            &agent(),
            vector_derive("render 18", vec![0.5; DIM], live(), None),
        )
        .await
        .unwrap();
    mem.pipeline()
        .wait(&agent(), submitted.receipt, crate::writeq::RECEIPT_WAIT_MAX)
        .await;
    assert_eq!(mem.query_embeddings.lock().len(), 0);
    mem.close().await.unwrap();
}

/// Plant one unconsumed `DeriveImage` intent, as a process that acked it and
/// died before applying it would leave it, in a session not yet stamped.
#[cfg(feature = "fixtures")]
fn plant_image_intent(store: &MemoryStore, session: &str, declared: EmbeddingContract) -> String {
    use crate::types::{WriteIntent, WriteIntentPayload};
    let sid = SessionId::new(session);
    let interaction = NodeId::new();
    let receipt = "lwr1.00000000deadbee2.18f00000000.1".to_string();
    let content = "render 17 [image:r17]".to_string();
    store
        .seed(GraphSnapshot {
            session_id: sid.clone(),
            embedding: None,
            interactions: vec![Interaction {
                event_time: None,
                id: interaction,
                session_id: sid.clone(),
                agent_id: agent(),
                prompt_text: Some(content.clone()),
                previous_id: None,
                created_at: Utc::now(),
            }],
            write_intents: vec![WriteIntent {
                session_id: sid,
                receipt: receipt.clone(),
                agent: agent(),
                interaction,
                lane_seq: 1,
                issued_ms: 1_755_000_000_000,
                payload: WriteIntentPayload::DeriveImage {
                    concepts: vec![(content.clone(), ConceptType::Resource)],
                    pairs: Vec::new(),
                    supplied: crate::types::SuppliedVector {
                        content,
                        vector: FixtureEmbedder::new().embed_sync("red silk saree"),
                        contract: declared,
                        source: EmbeddingSource {
                            modality: SourceModality::Image,
                            origin: VectorOrigin::Client,
                            sha256: None,
                            mime: None,
                        },
                    },
                },
                created_at: Utc::now(),
                outcome: None,
            }],
            ..GraphSnapshot::default()
        })
        .expect("seed");
    receipt
}

#[cfg(feature = "fixtures")]
async fn settled(mem: &Memory, receipt: &str) -> crate::writeq::ReceiptAnswer {
    use std::str::FromStr;
    let id = crate::writeq::ReceiptId::from_str(receipt).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let answer = mem.pipeline().lookup(&agent(), id);
        if !matches!(answer.tag(), "pending" | "pending_replay") {
            return answer;
        }
        assert!(std::time::Instant::now() < deadline, "replay settles");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// AC4 at replay: an image intent acked under one contract and replayed by a
/// process whose live contract differs settles `failed` (a fact about that
/// intent, not an outage), writes nothing, and does not block the replay.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn a_replayed_image_intent_under_a_changed_live_contract_settles_failed() {
    let store = Arc::new(MemoryStore::new());
    let receipt = plant_image_intent(
        &store,
        "image-replay-changed",
        EmbeddingContract {
            model: Some("acked-under-v1".into()),
            ..live()
        },
    );
    let mem = image_memory(store.clone(), "image-replay-changed").await;
    let answer = settled(&mem, &receipt).await;
    assert_eq!(answer.tag(), "failed", "{answer:?}");
    assert_eq!(mem.pipeline().counters().replay_owed(), 0);
    assert!(image_concepts(&mem).is_empty());
    // Attach stamped the live space; the intent's vector never entered it.
    assert_eq!(mem.graph().read().embedding(), Some(&live()));
    mem.close().await.unwrap();
    let snap = store
        .load_session(&SessionId::new("image-replay-changed"))
        .await
        .unwrap();
    assert_eq!(
        snap.write_intents[0]
            .outcome
            .as_ref()
            .map(|o| o.tag.as_str()),
        Some("failed"),
        "consumed as failed, durably"
    );
    assert!(snap.concepts.is_empty());
}

/// #22 PR 4: a replayed image intent whose key a text concept already holds
/// settles `failed` with the image-id sentence, not a bare class, and does not
/// block the replay.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn a_replayed_image_intent_whose_id_is_taken_settles_failed_with_the_fix() {
    let session = "image-replay-taken";
    // A text concept holding the image's key, written by an earlier writer.
    let squatter = image_memory(Arc::new(MemoryStore::new()), session).await;
    squatter
        .derive(
            &[("Render 17 [IMAGE:r17]", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap();
    let mut snap = squatter.graph().read().snapshot();
    squatter.close().await.unwrap();
    let head = snap.interactions[0].id;
    // Then an image intent acked behind it, chained after its interaction.
    let store = Arc::new(MemoryStore::new());
    let receipt = plant_image_intent(&store, session, live());
    let mut planted = store.load_session(&SessionId::new(session)).await.unwrap();
    planted.interactions[0].previous_id = Some(head);
    let now = Utc::now();
    snap.edges.push(crate::types::Edge {
        id: NodeId::new(),
        session_id: SessionId::new(session),
        source: planted.interactions[0].id,
        target: head,
        edge_type: crate::types::EdgeType::Temporal,
        weight: 1.0,
        reinforcements: 0,
        created_at: now,
        last_reinforced: now,
        event_time: None,
    });
    snap.write_intents = planted.write_intents;
    snap.interactions.extend(planted.interactions);
    store.seed(snap).expect("seed");

    let mem = image_memory(store.clone(), session).await;
    let answer = settled(&mem, &receipt).await;
    let crate::writeq::ReceiptAnswer::Failed(why) = &answer else {
        panic!("failed: {answer:?}");
    };
    assert!(why.contains("choose another image id"), "{why}");
    assert!(why.contains("\"r17\""), "{why}");
    assert_eq!(mem.pipeline().counters().replay_owed(), 0);
    assert!(image_concepts(&mem).is_empty());
    mem.close().await.unwrap();
}

/// The control: the same intent under the contract it was acked in applies
/// after the restart, vector and source intact.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn a_replayed_image_intent_under_its_own_contract_applies() {
    let store = Arc::new(MemoryStore::new());
    let receipt = plant_image_intent(&store, "image-replay-same", live());
    let mem = image_memory(store.clone(), "image-replay-same").await;
    let answer = settled(&mem, &receipt).await;
    assert_eq!(answer.tag(), "applied_after_restart", "{answer:?}");
    let [c] = image_concepts(&mem).try_into().unwrap();
    assert_eq!(
        c.embedding,
        Some(FixtureEmbedder::new().embed_sync("red silk saree"))
    );
    assert!(matches!(
        mem.graph().read().node(c.id),
        Some(crate::types::Node::Concept(_))
    ));
    mem.close().await.unwrap();
}

/// #22 review L4: an image intent at the head of the backlog in a process
/// that cannot apply one (no vector-search store, or the canonical
/// strategy) stops the replay with its own block reason, and the intent
/// stays durable and unconsumed for a process that can.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn a_replayed_image_intent_this_process_cannot_apply_names_its_block_reason() {
    let _quiet = crate::test_util::quiet_logs();
    for (session, vector_search, strategy) in [
        ("image-replay-no-vectors", false, MatchStrategy::Hybrid),
        ("image-replay-canonical", true, MatchStrategy::Canonical),
    ] {
        let store = Arc::new(MemoryStore::new());
        let receipt = plant_image_intent(&store, session, live());
        let backing: Arc<dyn GraphStore> = if vector_search {
            Arc::new(VectorSearchable(store.clone()))
        } else {
            store.clone()
        };
        let mem = Memory::builder()
            .session(session)
            .agent("agent-a")
            .flush_interval(Duration::from_secs(3_600))
            .store(backing)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(live())
            .match_strategy(strategy)
            .build()
            .await
            .expect("build");
        let counters = mem.pipeline().counters();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while counters.replay_blocked() == crate::writeq::ReplayBlockReason::None {
            assert!(
                std::time::Instant::now() < deadline,
                "{session}: replay blocks"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            counters.replay_blocked(),
            crate::writeq::ReplayBlockReason::ImageConfig,
            "{session}"
        );
        assert_eq!(counters.replay_owed(), 1, "{session}: still owed");
        assert_eq!(
            mem.pipeline()
                .lookup(&agent(), std::str::FromStr::from_str(&receipt).unwrap())
                .tag(),
            "pending_replay",
            "{session}"
        );
        assert!(image_concepts(&mem).is_empty());
        mem.close().await.unwrap();
        let snap = store.load_session(&SessionId::new(session)).await.unwrap();
        assert!(
            snap.write_intents[0].outcome.is_none(),
            "{session}: durable and unconsumed"
        );
    }
}

/// An embedder whose image path fails as Lambo's own decoder does (22g).
#[derive(Debug)]
struct UnreadableImages(FixtureEmbedder);

#[async_trait::async_trait]
impl Embedder for UnreadableImages {
    fn dimensions(&self) -> usize {
        self.0.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.0.embed(text).await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.0.embed_query(text).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.0.modalities()
    }
    async fn embed_image(
        &self,
        _image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        Err(crate::embed::EmbedError::Unreadable(
            "could not decode this image/png image: truncated".into(),
        ))
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.0.as_any()
    }
}

/// An image Lambo itself cannot read is a permanent `Embed` refusal that
/// says so, not "the embedder refused the image": no embedder was asked
/// (22g review L3).
///
/// Mutation: drop the `Unreadable` arm in `embed_image_or_refuse` -> red.
#[tokio::test]
async fn an_unreadable_image_is_named_as_lambos_own_failure() {
    let mem = Memory::builder()
        .session("image-unreadable")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(Arc::new(MemoryStore::new()))) as Arc<dyn GraphStore>)
        .embedder(Arc::new(UnreadableImages(FixtureEmbedder::new())) as Arc<dyn Embedder>)
        .embedding_contract(live())
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .unwrap();
    let png = png_with_label("red silk saree");
    let err = mem
        .derive_image_as(&agent(), bytes_derive("render 17", &png, None))
        .await
        .unwrap_err();
    let LamboError::Embed(msg) = &err else {
        panic!("{err:?}");
    };
    assert!(msg.starts_with("Lambo could not read the image"), "{msg}");
    assert!(
        msg.contains("could not decode this image/png image"),
        "{msg}"
    );
    assert!(!msg.contains("embedder refused"), "{msg}");
    assert!(image_concepts(&mem).is_empty(), "nothing was written");
    mem.close().await.unwrap();
}

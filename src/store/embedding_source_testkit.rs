//! #22 PR 2: one check, run by every adapter, that a concept's
//! `embedding_source` survives flush and load, is cleared by an upsert that
//! clears it, and is left alone by the narrow read-access update (#30) and by
//! the embedding quarantine (which nulls the vector only). #22 PR 3 adds a
//! second: a settled image write intent keeps no vector.
//!
//! The SQLite and in-memory adapters run it in CI, Postgres in a live
//! `#[ignore]` test, and Cockroach inside its live conformance suite.

// Each adapter's test calls this; a build with none of them compiles it unused.
#![cfg_attr(
    not(any(
        feature = "store-sqlite",
        feature = "store-memory",
        feature = "store-postgres",
        all(feature = "store-cockroach", feature = "fixtures")
    )),
    allow(dead_code)
)]

use chrono::Utc;

use crate::store::GraphStore;
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, EmbeddingContract, EmbeddingSource,
    ImageMimeWire, Interaction, Mutation, MutationBatch, Node, NodeId, SessionId, SourceModality,
    VectorOrigin,
};

/// A server-embedded image: every field set.
pub(crate) fn server_source() -> EmbeddingSource {
    EmbeddingSource {
        modality: SourceModality::Image,
        origin: VectorOrigin::Server,
        sha256: Some("0123456789abcdef".repeat(4)),
        mime: Some(ImageMimeWire::Jpeg),
    }
}

/// A client-submitted vector: no digest, no MIME.
pub(crate) fn client_source() -> EmbeddingSource {
    EmbeddingSource {
        modality: SourceModality::Image,
        origin: VectorOrigin::Client,
        sha256: None,
        mime: None,
    }
}

fn concept(
    sid: &SessionId,
    id: NodeId,
    origin: NodeId,
    content: &str,
    embedding: Option<Vec<f32>>,
    embedding_source: Option<EmbeddingSource>,
) -> Concept {
    Concept {
        id,
        session_id: sid.clone(),
        content: content.into(),
        canonical_key: content.to_lowercase(),
        concept_type: ConceptType::Entity,
        origin_interaction: origin,
        origin_agent: AgentId::new("embedding-source-test"),
        created_at: Utc::now(),
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        canonization_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding,
        human_confirmed: 0,
        chunk_group_id: None,
        embedding_source,
    }
}

fn upsert(c: &Concept) -> Mutation {
    Mutation::UpsertNode {
        node: Node::Concept(c.clone()),
    }
}

async fn flush(store: &dyn GraphStore, mutations: Vec<Mutation>, token: Option<u64>) {
    store
        .flush(
            &MutationBatch {
                mutations,
                ..Default::default()
            },
            token,
        )
        .await
        .expect("flush");
}

async fn loaded(store: &dyn GraphStore, sid: &SessionId, id: NodeId) -> Concept {
    store
        .load_session(sid)
        .await
        .expect("load")
        .concepts
        .into_iter()
        .find(|c| c.id == id)
        .expect("the concept was flushed")
}

/// Plant three concepts in `sid` (a fresh id): a server-embedded image, a
/// client-submitted vector, and a plain text concept. Check that each
/// `embedding_source` loads back exactly, that a `RecordAccess` leaves it
/// alone, that the embedding quarantine nulls the vectors but keeps the
/// sources, and that an upsert which nulls the vector and its source (what
/// `re-embed --drop-image-vectors` will write, #22 PR 3) clears it. `dim` is
/// the store's vector width; `token` is the flush fencing token, if the
/// store wants one.
pub(crate) async fn check_embedding_source_round_trip(
    store: &dyn GraphStore,
    sid: &SessionId,
    dim: usize,
    token: Option<u64>,
) {
    let ts = Utc::now();
    let i1 = NodeId::new();
    let (server, client, text) = (NodeId::new(), NodeId::new(), NodeId::new());
    let vector =
        |seed: usize| -> Vec<f32> { (0..dim).map(|i| ((i + seed) % 5) as f32 + 1.0).collect() };
    let server_c = concept(
        sid,
        server,
        i1,
        &format!("outfit for onam [image:{server}]"),
        Some(vector(1)),
        Some(server_source()),
    );
    let client_c = concept(
        sid,
        client,
        i1,
        &format!("outfit for diwali [image:{client}]"),
        Some(vector(2)),
        Some(client_source()),
    );
    let text_c = concept(
        sid,
        text,
        i1,
        &format!("prefers linen {text}"),
        Some(vector(3)),
        None,
    );
    flush(
        store,
        vec![
            Mutation::SetEmbedding {
                session_id: sid.clone(),
                embedding: Some(EmbeddingContract {
                    kind: "fixture".into(),
                    model: Some("embedding-source-test".into()),
                    dim,
                }),
            },
            Mutation::UpsertNode {
                node: Node::Interaction(Interaction {
                    id: i1,
                    session_id: sid.clone(),
                    agent_id: AgentId::new("embedding-source-test"),
                    prompt_text: None,
                    previous_id: None,
                    created_at: ts,
                    event_time: None,
                }),
            },
            upsert(&server_c),
            upsert(&client_c),
            upsert(&text_c),
        ],
        token,
    )
    .await;

    assert_eq!(
        loaded(store, sid, server).await.embedding_source,
        Some(server_source()),
        "a server-embedded source round-trips with its digest and MIME"
    );
    assert_eq!(
        loaded(store, sid, client).await.embedding_source,
        Some(client_source()),
        "a client source round-trips without a digest or MIME"
    );
    let text_loaded = loaded(store, sid, text).await;
    assert_eq!(text_loaded.embedding_source, None, "NULL stays None");
    assert!(
        text_loaded.embedding.is_some(),
        "a text vector is unaffected"
    );

    // #30's narrow access update touches two columns only.
    flush(
        store,
        vec![Mutation::RecordAccess {
            session_id: sid.clone(),
            id: client,
            access_count: 3,
            last_accessed: ts,
        }],
        token,
    )
    .await;
    let accessed = loaded(store, sid, client).await;
    assert_eq!(accessed.access_count, 3, "the access landed");
    assert_eq!(
        accessed.embedding_source,
        Some(client_source()),
        "a read access must not touch the source"
    );

    // #22 decision (review L1): the embedding quarantine nulls vectors but
    // KEEPS their source. A concept whose image vector was quarantined is
    // still an image concept: a later `re-embed --missing-only` (PR 3) must
    // not hand it a vector of its caption. Clearing the contract and
    // restamping it is the quarantine every adapter runs (the first-stamp
    // legacy upgrade); SQLite also runs it on a width restamp, checked in
    // its own tests.
    let contract = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("embedding-source-test".into()),
        dim,
    };
    for embedding in [None, Some(contract)] {
        flush(
            store,
            vec![Mutation::SetEmbedding {
                session_id: sid.clone(),
                embedding,
            }],
            token,
        )
        .await;
    }
    for (id, source) in [
        (server, Some(server_source())),
        (client, Some(client_source())),
        (text, None),
    ] {
        let quarantined = loaded(store, sid, id).await;
        assert_eq!(quarantined.embedding, None, "the restamp quarantined {id}");
        assert_eq!(
            quarantined.embedding_source, source,
            "the quarantine keeps the source of {id}"
        );
    }

    // A whole-record upsert carries the column in its conflict update.
    let mut dropped = server_c.clone();
    dropped.embedding = None;
    dropped.embedding_source = None;
    flush(store, vec![upsert(&dropped)], token).await;
    let dropped = loaded(store, sid, server).await;
    assert_eq!(dropped.embedding, None);
    assert_eq!(
        dropped.embedding_source, None,
        "an upsert that clears the source clears the column"
    );
    assert_eq!(
        loaded(store, sid, client).await.embedding_source,
        Some(client_source()),
        "only the upserted row changed"
    );
}

/// A source whose sha256 is not 64 lowercase hex characters is refused at
/// the flush, so the store never holds a value its own load would refuse
/// (#22 review round 2, L1). Nothing of the batch lands.
pub(crate) async fn check_a_malformed_digest_is_refused_on_write(
    store: &dyn GraphStore,
    sid: &SessionId,
    dim: usize,
    token: Option<u64>,
) {
    let i1 = NodeId::new();
    let id = NodeId::new();
    let mut bad = server_source();
    bad.sha256 = Some("AB".repeat(32));
    let c = concept(
        sid,
        id,
        i1,
        &format!("outfit [image:{id}]"),
        Some((0..dim).map(|i| i as f32 + 1.0).collect()),
        Some(bad),
    );
    let err = store
        .flush(
            &MutationBatch {
                mutations: vec![
                    Mutation::UpsertNode {
                        node: Node::Interaction(Interaction {
                            id: i1,
                            session_id: sid.clone(),
                            agent_id: AgentId::new("embedding-source-test"),
                            prompt_text: None,
                            previous_id: None,
                            created_at: Utc::now(),
                            event_time: None,
                        }),
                    },
                    upsert(&c),
                ],
                ..Default::default()
            },
            token,
        )
        .await
        .expect_err("a malformed digest must not be written");
    assert!(
        matches!(err, crate::store::StoreError::Invariant(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains("sha256"), "{err}");
    let loaded = store.load_session(sid).await;
    assert!(
        loaded.map_or(true, |s| s.concepts.iter().all(|c| c.id != id)),
        "the refused concept was written"
    );
}

/// #22 review L1: a **settled** image intent keeps no vector in the store.
///
/// In `sid` (any session this check may write to), put an unconsumed
/// `DeriveImage` intent and an unconsumed text `Derive` intent, check both
/// load back exactly (the image's vector is owed a replay), then consume
/// both and check the image intent loads with
/// [`WriteIntentPayload::settled`]'s payload and its outcome while the text
/// intent's payload is untouched. A settled image intent put directly (a
/// snapshot save or a replayed put of a settled row) is stored settled too.
pub(crate) async fn check_a_settled_image_intent_keeps_no_vector(
    store: &dyn GraphStore,
    sid: &SessionId,
    dim: usize,
    token: Option<u64>,
) {
    use crate::types::{SuppliedVector, WriteIntent, WriteIntentOutcome, WriteIntentPayload};

    let ts = Utc::now();
    let interaction = NodeId::new();
    let contract = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("embedding-source-test".into()),
        dim,
    };
    let content = format!("outfit for pongal [image:{}]", interaction.0.simple());
    let image_payload = WriteIntentPayload::DeriveImage {
        concepts: vec![(content.clone(), ConceptType::Resource)],
        pairs: vec![],
        supplied: SuppliedVector {
            content,
            vector: (0..dim).map(|i| (i % 3) as f32 + 0.5).collect(),
            contract: contract.clone(),
            source: server_source(),
        },
    };
    let text_payload = WriteIntentPayload::Derive {
        concepts: vec![("prefers silk".into(), ConceptType::Entity)],
        pairs: vec![],
    };
    let intent = |receipt: &str, payload: &WriteIntentPayload, outcome| WriteIntent {
        session_id: sid.clone(),
        receipt: format!("{receipt}-{}", interaction.0.simple()),
        agent: AgentId::new("embedding-source-test"),
        interaction,
        lane_seq: 1,
        issued_ms: ts.timestamp_millis(),
        payload: payload.clone(),
        created_at: ts,
        outcome,
    };
    let outcome = |tag: &str| WriteIntentOutcome {
        tag: tag.into(),
        summary: format!("{tag} for the settled-intent check"),
        consumed_at: Utc::now(),
    };
    let image = intent("settle-image", &image_payload, None);
    let text = intent("settle-text", &text_payload, None);
    let failed = intent("settle-failed", &image_payload, Some(outcome("failed")));
    let stored = |receipt: &str| {
        let receipt = receipt.to_string();
        async move {
            store
                .load_session(sid)
                .await
                .expect("load")
                .write_intents
                .into_iter()
                .find(|i| i.receipt == receipt)
                .unwrap_or_else(|| panic!("intent {receipt} is stored"))
        }
    };

    flush(
        store,
        vec![
            Mutation::SetEmbedding {
                session_id: sid.clone(),
                embedding: Some(contract.clone()),
            },
            Mutation::UpsertNode {
                node: Node::Interaction(Interaction {
                    id: interaction,
                    session_id: sid.clone(),
                    agent_id: AgentId::new("embedding-source-test"),
                    prompt_text: None,
                    previous_id: None,
                    created_at: ts,
                    event_time: None,
                }),
            },
            Mutation::PutWriteIntent {
                intent: image.clone(),
            },
            Mutation::PutWriteIntent {
                intent: text.clone(),
            },
            Mutation::PutWriteIntent {
                intent: failed.clone(),
            },
        ],
        token,
    )
    .await;
    assert_eq!(
        stored(&image.receipt).await.payload,
        image_payload,
        "an unconsumed image intent keeps its vector: the replay owes it"
    );
    let settled_failed = stored(&failed.receipt).await;
    assert_eq!(
        settled_failed.payload,
        image_payload.settled().expect("an image payload settles"),
        "a settled image intent put directly is stored without its vector"
    );
    assert_eq!(settled_failed.outcome.map(|o| o.tag), Some("failed".into()));

    let applied = outcome("applied");
    flush(
        store,
        vec![
            Mutation::ConsumeWriteIntent {
                session_id: sid.clone(),
                receipt: image.receipt.clone(),
                outcome: applied.clone(),
            },
            Mutation::ConsumeWriteIntent {
                session_id: sid.clone(),
                receipt: text.receipt.clone(),
                outcome: applied.clone(),
            },
        ],
        token,
    )
    .await;
    let consumed = stored(&image.receipt).await;
    assert_eq!(
        consumed.payload,
        WriteIntentPayload::Derive {
            concepts: vec![],
            pairs: vec![],
        },
        "a consumed image intent drops its vector (and loads in a build without DeriveImage)"
    );
    assert_eq!(
        consumed.outcome.as_ref().map(|o| o.summary.as_str()),
        Some(applied.summary.as_str()),
        "the receipt still answers from the outcome"
    );
    let consumed_text = stored(&text.receipt).await;
    assert_eq!(
        consumed_text.payload, text_payload,
        "text payloads are kept"
    );
    assert!(consumed_text.outcome.is_some());
}

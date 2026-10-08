//! J3 durable intents: defer at close, replay at the next attach.

use super::*;

// -- J3 durable intents: defer at close, replay at the next attach ------

/// `MemoryStore` behind a `VECTOR_SEARCH` face whose candidate reads
/// succeed (empty), so hybrid's below-threshold arm actually persists its
/// vectors — the *embedding column* is what the J3 assertions read.
struct VectorSearchable(Arc<MemoryStore>);

#[async_trait]
impl GraphStore for VectorSearchable {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities() | Capabilities::VECTOR_SEARCH
    }
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.0.init_schema().await
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.0.flush(batch, token).await
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.0.load_session(session).await
    }
    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0.keyword_candidates(session, tokens, limit).await
    }
    async fn vector_candidates(
        &self,
        _session: &SessionId,
        _embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        Ok(Vec::new())
    }
    async fn vector_candidates_checked(
        &self,
        _session: &SessionId,
        _embedding: &[f32],
        _expected_contract: &EmbeddingContract,
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        Ok(Vec::new())
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.0.blast_radius(session, node, min_edge_age, now).await
    }
    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        self.0.interaction_span(session, node, min_age, now).await
    }
    async fn record_canonization(
        &self,
        event: &crate::types::CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.0.record_canonization(event, token).await
    }
    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::LeaseOutcome, StoreError> {
        self.0.acquire_lease(session, holder, ttl).await
    }
    async fn read_lease(
        &self,
        session: &SessionId,
    ) -> Result<Option<crate::store::LeaseInfo>, StoreError> {
        self.0.read_lease(session).await
    }
    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::LeaseOutcome, StoreError> {
        self.0.refresh_lease(session, holder, ttl).await
    }
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::LeaseHolder,
    ) -> Result<(), StoreError> {
        self.0.release_lease(session, holder).await
    }
}

/// Answers the calibration probe's own texts instantly and hangs forever
/// on real content — the shape that leaves an ADMITTED write undrainable
/// at close, which is the case J3's durable intents exist for.
struct HangingEmbedder {
    inner: FixtureEmbedder,
}

#[async_trait]
impl Embedder for HangingEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        if text.contains(crate::writeq::PROBE_TEXT) {
            return self.inner.embed(text).await;
        }
        std::future::pending().await
    }
}

/// **J3's founding invariant, end to end at `Memory` level**: acked ⇒
/// (applied ∨ durable intent) at a clean close — and the durable half is a
/// real write, not a tombstone: the next attach replays it, the concept
/// lands **with its embedding** (the J3-R3-1 rule: durability is judged at
/// the store's embedding column, never at `applied` counts), and the
/// original receipt id answers `applied_after_restart` in the new process.
#[tokio::test]
async fn an_acked_write_survives_a_clean_close_as_a_durable_intent_and_replays() {
    let inner = Arc::new(MemoryStore::new());
    let sid = SessionId::new("intent-replay");
    let agent = AgentId::new("agent-a");

    // Session 1: the embedder hangs on real content, so the acked write
    // cannot drain inside close()'s budget.
    let mem1 = Memory::builder()
        .session("intent-replay")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(HangingEmbedder {
            inner: FixtureEmbedder::new(),
        }) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build session 1");
    let submitted = mem1
        .derive_async_as(
            &agent,
            &[("user schema", ConceptType::Entity)],
            &ParentOf::none(),
            None,
        )
        .await
        .expect("ack");
    assert!(!submitted.dropped(), "the write must be ADMITTED");

    mem1.close().await.expect("clean close");
    assert_eq!(
        mem1.pipeline().lookup(&agent, submitted.receipt).tag(),
        "intent_durable",
        "an undrained acked write settles intent_durable at a clean close"
    );
    assert_eq!(mem1.pipeline().counters().deferred(), 1);
    assert_eq!(mem1.pipeline().counters().abandoned(), 0);

    // Durable: the intent survived the close; the concept did not apply.
    let snap = inner.load_session(&sid).await.expect("session durable");
    assert!(
        snap.concepts.is_empty(),
        "nothing applied — the embed never returned"
    );
    assert_eq!(snap.write_intents.len(), 1, "the acked write IS durable");
    assert!(snap.write_intents[0].outcome.is_none(), "unconsumed");
    assert_eq!(snap.write_intents[0].receipt, submitted.receipt.to_string());

    // Session 2: a working embedder. The attach replays the intent.
    let mem2 = Memory::builder()
        .session("intent-replay")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build session 2");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while mem2.pipeline().counters().replayed() < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "the replay task must apply the intent"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let answer = mem2.pipeline().lookup(&agent, submitted.receipt);
    assert_eq!(
        answer.tag(),
        "applied_after_restart",
        "the ORIGINAL receipt id answers its truth in the new process: {answer:?}"
    );
    // Agent-scoped across restart, like everything else about receipts.
    assert_eq!(
        mem2.pipeline()
            .lookup(&AgentId::new("agent-b"), submitted.receipt)
            .tag(),
        "forbidden"
    );

    mem2.close().await.expect("clean close 2");
    let snap = inner.load_session(&sid).await.expect("reload");
    let concept = snap
        .concepts
        .iter()
        .find(|c| c.content == "user schema")
        .expect("the replayed write landed");
    assert!(
        concept.embedding.is_some(),
        "durability is judged at the EMBEDDING column, never at applied counts (J3-R3-1)"
    );
    let intent = &snap.write_intents[0];
    let outcome = intent.outcome.as_ref().expect("consumed by the replay");
    assert_eq!(outcome.tag, "applied_after_restart");
}

/// Unreachable for everything, including the calibration probe's own text —
/// a llama.cpp that is down or restarting, as the shipped adapter reports it
/// (`EmbedError::Unavailable`, "llama.cpp unreachable").
struct DeadEmbedder;

#[async_trait]
impl Embedder for DeadEmbedder {
    fn dimensions(&self) -> usize {
        1024
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        Err(crate::embed::EmbedError::Unavailable(
            "llama.cpp unreachable: connection refused".into(),
        ))
    }
}

/// Answers the probe's text, then **refuses real content the way a serving
/// llama.cpp refuses it** — a non-success HTTP status, which the adapter
/// reports as `EmbedError::Backend`. The liveness gate therefore passes and
/// the refusal reaches the classifier, which is the case that must still
/// consume.
struct RefusingEmbedder {
    inner: FixtureEmbedder,
}

#[async_trait]
impl Embedder for RefusingEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        if text.contains(crate::writeq::PROBE_TEXT) {
            return self.inner.embed(text).await;
        }
        Err(crate::embed::EmbedError::Backend(
            "llama.cpp returned 500 Internal Server Error for model \"bge-m3\": \
                 input is too long"
                .into(),
        ))
    }
}

/// Session 1 defers an acked write as a durable intent, then close.
async fn defer_one_intent(
    inner: &Arc<MemoryStore>,
    session: &str,
    agent: &AgentId,
) -> crate::writeq::ReceiptId {
    let mem = Memory::builder()
        .session(session)
        .agent(agent.as_str())
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(HangingEmbedder {
            inner: FixtureEmbedder::new(),
        }) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build the deferring session");
    let submitted = mem
        .derive_async_as(
            agent,
            &[("user schema", ConceptType::Entity)],
            &ParentOf::none(),
            None,
        )
        .await
        .expect("ack");
    assert!(!submitted.dropped(), "the write must be ADMITTED");
    mem.close().await.expect("clean close");
    assert_eq!(mem.pipeline().counters().deferred(), 1);
    submitted.receipt
}

/// **J3 round-1 N1, the P1**: a transient embedder outage at attach must not
/// destroy the durable-intent backlog.
///
/// Red before the fix: `spawn_replay` ran unconditionally and its failure arm
/// consumed every intent as `failed` — so this test's session 2 settled the
/// acked write `failed` with nothing written, permanently, because a
/// dependency the write does not need in order to be *recorded* was down for
/// one attach. Green: the liveness embed fails, the loop never starts, the
/// intent is still unconsumed, and session 3 applies it.
#[tokio::test]
async fn a_dead_embedder_at_attach_leaves_the_backlog_durable_and_a_later_serve_applies_it() {
    let inner = Arc::new(MemoryStore::new());
    let sid = SessionId::new("intent-outage");
    let agent = AgentId::new("agent-a");
    let receipt = defer_one_intent(&inner, "intent-outage", &agent).await;

    // Session 2: the embedder is down for the whole attach.
    let mem2 = Memory::builder()
        .session("intent-outage")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(DeadEmbedder) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("a dead embedder must not refuse the attach");
    // Give the replay task every chance to misbehave.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(
        mem2.pipeline().counters().replayed(),
        0,
        "nothing can be applied with no embedder"
    );
    assert_eq!(
        mem2.pipeline().counters().replay_owed(),
        1,
        "the debt must be visible on the stats surface, not silently discharged"
    );
    let answer = mem2.pipeline().lookup(&agent, receipt);
    assert_eq!(
        answer.tag(),
        "pending_replay",
        "an outage at attach must not settle an acked write failed: {answer:?}"
    );
    mem2.close().await.expect("clean close 2");
    let snap = inner.load_session(&sid).await.expect("reload");
    assert_eq!(snap.write_intents.len(), 1);
    assert!(
        snap.write_intents[0].outcome.is_none(),
        "the intent must survive the outage UNCONSUMED: {:?}",
        snap.write_intents[0].outcome
    );

    // Session 3: the embedder is back. The write the outage would have
    // destroyed is applied, with its embedding.
    let mem3 = Memory::builder()
        .session("intent-outage")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build session 3");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while mem3.pipeline().counters().replayed() < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "the intent must still be replayable after the outage"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        mem3.pipeline().lookup(&agent, receipt).tag(),
        "applied_after_restart"
    );
    assert_eq!(mem3.pipeline().counters().replay_owed(), 0, "debt paid");
    mem3.close().await.expect("clean close 3");
    let snap = inner.load_session(&sid).await.expect("reload");
    let concept = snap
        .concepts
        .iter()
        .find(|c| c.content == "user schema")
        .expect("the write survived a whole outage and then landed");
    assert!(
        concept.embedding.is_some(),
        "judged at the EMBEDDING column (J3-R3-1)"
    );
}

/// **J3-R2R-2 (in-session symmetry).** A write the worker reaches DURING an
/// embedder outage is not destroyed as `failed`: its durable intent is left
/// unconsumed so the next serve can re-attempt it, even though the live
/// caller's receipt says `failed` (nothing was written by this process).
/// Red before the fix: the in-session arm consumed *any* error class as
/// `failed`, so this intent's row was settled and the write could never
/// come back — the same transient condition N1 was raised about, on the
/// path that handles almost every write.
#[tokio::test]
async fn a_write_reached_during_an_in_session_embedder_outage_is_not_consumed() {
    let inner = Arc::new(MemoryStore::new());
    let sid = SessionId::new("in-session-outage");
    let agent = AgentId::new("agent-a");
    let mem = Memory::builder()
        .session("in-session-outage")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(DeadEmbedder) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build a session whose embedder is dead");
    let submitted = mem
        .derive_async_as(
            &agent,
            &[("user schema", ConceptType::Entity)],
            &ParentOf::none(),
            None,
        )
        .await
        .expect("ack");
    assert!(
        !submitted.dropped(),
        "admitted — an embedder outage must not drop the write at the door"
    );
    // Wait for the worker to reach it (a dead embedder fails fast).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let a = mem.pipeline().lookup(&agent, submitted.receipt);
        if a.is_settled() {
            assert_eq!(
                a.tag(),
                "failed",
                "the in-session caller learns its write failed: {a:?}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the worker never settled the receipt"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    mem.close().await.expect("clean close");
    let snap = inner.load_session(&sid).await.expect("reload");
    assert_eq!(snap.write_intents.len(), 1);
    assert!(
        snap.write_intents[0].outcome.is_none(),
        "an EmbedUnavailable must leave the intent UNCONSUMED for replay: {:?}",
        snap.write_intents[0].outcome
    );
}

/// **J3 round-1 F2**: a consumed intent row older than the retention window
/// must not answer better for having survived a restart than the same id
/// would in a process that never restarted.
///
/// `WRITE_INTENT_RETENTION` claimed "expired rows are skipped at load" and
/// no such filter existed anywhere, so a stale consumed row answered
/// `applied_after_restart` where a live process would have swept the receipt
/// to `expired` — the exact asymmetry the
/// `RECEIPT_RETENTION == WRITE_INTENT_RETENTION` assert forbids, pointing the
/// other way. The skip lives at the replay's seeding step now, and this pins
/// both sides of the boundary in one attach.
// `MemoryStore::seed` is fixtures-only; this is the only test here that
// needs to fabricate a previous process's consumed row.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn a_consumed_intent_past_its_retention_window_is_not_answered_from() {
    use crate::types::{WriteIntent, WriteIntentOutcome, WriteIntentPayload};
    use std::str::FromStr;
    let inner = Arc::new(MemoryStore::new());
    let sid = SessionId::new("intent-stale");
    let agent = AgentId::new("agent-a");
    let interaction = NodeId::new();
    let now = Utc::now();
    let stale_receipt = "lwr1.00000000deadbee1.18f00000000.1";
    let fresh_receipt = "lwr1.00000000deadbee1.18f00000000.2";
    let consumed = |receipt: &str, seq: u64, consumed_at| WriteIntent {
        session_id: sid.clone(),
        receipt: receipt.to_string(),
        agent: agent.clone(),
        interaction,
        lane_seq: seq,
        issued_ms: 1_755_000_000_000,
        payload: WriteIntentPayload::Derive {
            concepts: vec![("stale probe".to_string(), ConceptType::Entity)],
            pairs: Vec::new(),
        },
        created_at: now,
        outcome: Some(WriteIntentOutcome {
            tag: "applied_after_restart".into(),
            summary: "derived 1 concept(s)".into(),
            consumed_at,
        }),
    };
    inner
        .seed(GraphSnapshot {
            session_id: sid.clone(),
            embedding: Some(contract("fixture", 1024)),
            interactions: vec![Interaction {
                event_time: None,
                id: interaction,
                session_id: sid.clone(),
                agent_id: agent.clone(),
                prompt_text: None,
                previous_id: None,
                created_at: now,
            }],
            write_intents: vec![
                // One window plus a minute in the past: outside.
                consumed(
                    stale_receipt,
                    1,
                    now - chrono::Duration::from_std(crate::writeq::RECEIPT_RETENTION).unwrap()
                        - chrono::Duration::seconds(60),
                ),
                // Just now: inside.
                consumed(fresh_receipt, 2, now),
            ],
            ..GraphSnapshot::default()
        })
        .expect("seed");

    let mem = Memory::builder()
        .session("intent-stale")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build");
    let stale = crate::writeq::ReceiptId::from_str(stale_receipt).unwrap();
    let fresh = crate::writeq::ReceiptId::from_str(fresh_receipt).unwrap();
    assert_eq!(
        mem.pipeline().lookup(&agent, fresh).tag(),
        "applied_after_restart",
        "a row inside the window is still the durable carrier of the answer"
    );
    assert_eq!(
        mem.pipeline().lookup(&agent, stale).tag(),
        "restart_lost",
        "a row past the window must not answer better than the same id would \
             have in a process that never restarted"
    );
    mem.close().await.expect("clean close");
}

/// **The other half of N1's classification**: a content-level refusal still
/// consumes. Without this, "leave it for the next process" would become
/// retry-forever for a record no embedder will ever accept — the objection
/// the original consume-always arm was built to answer, which the fix must
/// not trade away.
#[tokio::test]
async fn a_content_refusal_at_replay_still_settles_the_intent_failed() {
    let inner = Arc::new(MemoryStore::new());
    let sid = SessionId::new("intent-poison");
    let agent = AgentId::new("agent-a");
    let receipt = defer_one_intent(&inner, "intent-poison", &agent).await;

    let mem2 = Memory::builder()
        .session("intent-poison")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(RefusingEmbedder {
            inner: FixtureEmbedder::new(),
        }) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build session 2");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let answer = mem2.pipeline().lookup(&agent, receipt);
        if answer.tag() == "failed" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a refused replay must settle, not hang: {answer:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(mem2.pipeline().counters().replay_owed(), 0);
    mem2.close().await.expect("clean close 2");
    let snap = inner.load_session(&sid).await.expect("reload");
    let outcome = snap.write_intents[0]
        .outcome
        .as_ref()
        .expect("a poison record IS consumed — otherwise it retries forever");
    assert_eq!(outcome.tag, "failed");
    assert!(
        snap.concepts.is_empty(),
        "nothing was written for a refused replay"
    );
}

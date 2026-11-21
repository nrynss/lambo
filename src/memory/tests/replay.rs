//! J3 durable intents: defer at close, replay at the next attach.

use super::*;

// -- J3 durable intents: defer at close, replay at the next attach ------

/// Moved to `crate::test_util` (#22 PR 4) so the MCP tests can serve an
/// image derive over it too.
pub(super) use crate::test_util::VectorSearchable;

/// Answers the calibration probe's own texts instantly and hangs forever
/// on real content — the shape that leaves an ADMITTED write undrainable
/// at close, which is the case J3's durable intents exist for.
pub(super) struct HangingEmbedder {
    inner: FixtureEmbedder,
    /// Real-content calls that have entered the hang, so a test can wait for
    /// a job to be inside the embedder rather than sleep for it.
    entered: Arc<AtomicUsize>,
}

impl HangingEmbedder {
    pub(super) fn new() -> Self {
        Self {
            inner: FixtureEmbedder::new(),
            entered: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Embedder for HangingEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Document)
            .await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Query).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.inner.embed_image(image).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

impl HangingEmbedder {
    /// The `embed` behaviour above, in either text role (#22: a wrapper
    /// forwards the role, so its inner embedder sees what the caller asked).
    async fn embed_as(
        &self,
        text: &str,
        role: crate::test_util::TextRole,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        if text.contains(crate::writeq::PROBE_TEXT) {
            return role.embed(&self.inner, text).await;
        }
        self.entered.fetch_add(1, Ordering::SeqCst);
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
        .embedder(Arc::new(HangingEmbedder::new()) as Arc<dyn Embedder>)
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
        self.embed_as(text, crate::test_util::TextRole::Document)
            .await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Query).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.inner.embed_image(image).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

impl RefusingEmbedder {
    /// The `embed` behaviour above, in either text role (#22: a wrapper
    /// forwards the role, so its inner embedder sees what the caller asked).
    async fn embed_as(
        &self,
        text: &str,
        role: crate::test_util::TextRole,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        if text.contains(crate::writeq::PROBE_TEXT) {
            return role.embed(&self.inner, text).await;
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
        .embedder(Arc::new(HangingEmbedder::new()) as Arc<dyn Embedder>)
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
/// unconsumed so the next serve can re-attempt it. Since #295 the live
/// caller's receipt says `pending_replay` (the write is held, and may still
/// apply in this serve), and a clean close answers `intent_durable`.
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
        if a.tag() != "pending" {
            assert_eq!(
                a.tag(),
                "pending_replay",
                "the in-session caller learns its write is owed, not applied: {a:?}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the worker never reached the write"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    mem.close().await.expect("clean close");
    assert_eq!(
        mem.pipeline().lookup(&agent, submitted.receipt).tag(),
        "intent_durable"
    );
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
            receipt_destination: None,
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

/// **#116 review M1**: a replayed intent refused because it would close a
/// cycle settles `failed` on its own and the replay goes on.
///
/// Two derives whose `parent_of` pairs close a Hierarchical 2-cycle, and two
/// actions whose `produces` close a Causal one, are all acked while nothing
/// has applied (each ack sees no edge yet), then deferred by a clean close.
/// At the next attach the first of each pair applies, the second is refused,
/// and the plain derive queued behind them still applies. Before the fix the
/// first refusal stopped the replay as session-wide, leaving it and every
/// later intent `pending_replay` for good.
#[tokio::test]
async fn a_cycle_refused_at_replay_settles_failed_and_the_replay_goes_on() {
    let inner = Arc::new(MemoryStore::new());
    let agent = AgentId::new("agent-a");
    let mem = Memory::builder()
        .session("intent-cycle")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(HangingEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build the deferring session");
    let derive_pairs = |pairs: &'static [(&'static str, &'static str)]| {
        let mem = &mem;
        let agent = &agent;
        async move {
            mem.derive_async_as(agent, &[], &ParentOf::from_pairs(pairs), None)
                .await
                .expect("ack")
                .receipt
        }
    };
    let act = |action: &'static str, produces: &'static [&'static str]| {
        let mem = &mem;
        let agent = &agent;
        async move {
            mem.record_action_async_as(
                agent,
                &Action {
                    repo_scope: None,
                    event_time: None,
                    action,
                    produces,
                    modifies: &[],
                    depends_on: &[],
                },
            )
            .await
            .expect("ack")
            .receipt
        }
    };
    let first_pair = derive_pairs(&[("cyc alpha", "cyc beta")]).await;
    let closing_pair = derive_pairs(&[("cyc beta", "cyc alpha")]).await;
    let first_action = act("ran step one", &["step two output"]).await;
    let closing_action = act("step two output", &["ran step one"]).await;
    let later = mem
        .derive_async_as(
            &agent,
            &[("a later fact", ConceptType::Entity)],
            &ParentOf::none(),
            None,
        )
        .await
        .expect("ack")
        .receipt;
    mem.close().await.expect("clean close");
    assert_eq!(mem.pipeline().counters().deferred(), 5);

    let mem2 = Memory::builder()
        .session("intent-cycle")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build session 2");
    let receipts = [
        first_pair,
        closing_pair,
        first_action,
        closing_action,
        later,
    ];
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let tags = loop {
        let tags: Vec<&str> = receipts
            .iter()
            .map(|r| mem2.pipeline().lookup(&agent, *r).tag())
            .collect();
        if tags
            .iter()
            .all(|t| *t != "pending_replay" && *t != "pending")
        {
            break tags;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the replay must settle every intent, not stop: {tags:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(
        tags,
        [
            "applied_after_restart",
            "failed",
            "applied_after_restart",
            "failed",
            "applied_after_restart"
        ]
    );
    for (receipt, said) in [
        (
            closing_pair,
            crate::surface::error::HIERARCHICAL_CYCLE_REFUSAL,
        ),
        (closing_action, crate::surface::error::CAUSAL_CYCLE_REFUSAL),
    ] {
        let answer = format!("{:?}", mem2.pipeline().lookup(&agent, receipt));
        assert!(
            answer.contains(said),
            "the receipt says why, model-safe: {answer}"
        );
    }
    assert_eq!(mem2.pipeline().counters().replay_owed(), 0);
    assert!(matches!(
        mem2.pipeline().counters().replay_blocked(),
        crate::writeq::ReplayBlockReason::None
    ));
    mem2.graph.read().assert_invariants().unwrap();
    mem2.close().await.expect("clean close 2");
}

/// Issue #16 §2 made `parent_of` ends embed, so a call can now ask for more
/// embeds than fit one `HYBRID_IO_TIMEOUT`. Under hybrid the J3 ack refuses
/// a call over `MAX_HYBRID_EMBEDS` at call time with a `Config` error, rather
/// than acking it and letting it time out at apply (and, as a kept intent,
/// time out again at replay). Exactly the budget is admitted.
#[tokio::test]
async fn an_over_budget_derive_is_refused_at_the_ack() {
    use crate::graph::hybrid::MAX_HYBRID_EMBEDS;
    let inner = Arc::new(MemoryStore::new());
    let mem = Memory::builder()
        .session("embed-budget-ack")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner)) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build");
    let agent = AgentId::new("agent-a");
    let pairs: Vec<(String, String)> = (0..MAX_HYBRID_EMBEDS / 2)
        .map(|i| (format!("parent end {i}"), format!("child end {i}")))
        .collect();
    let pair_refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();

    let err = mem
        .derive_async_as(
            &agent,
            &[("one more", ConceptType::Entity)],
            &ParentOf::from_pairs(&pair_refs),
            None,
        )
        .await
        .expect_err("over budget is refused at the ack");
    assert!(
        matches!(&err, LamboError::Config(m) if m.contains("would embed")),
        "unexpected error: {err:?}"
    );

    let submitted = mem
        .derive_async_as(&agent, &[], &ParentOf::from_pairs(&pair_refs), None)
        .await
        .expect("exactly the budget is admitted");
    assert!(!submitted.dropped());
    mem.close().await.expect("close");
}

/// **A wait on a replay-owed receipt ends when the session closes** (#11
/// review P3-2).
///
/// A close settles every receipt this process holds (`intent_durable`) and
/// wakes the waiters, so those waits end with it. A `pending_replay` id is not
/// one of them: it is owed to a replay the close stops, so nothing settles it,
/// and its wait kept looping against a closed session until its own budget
/// ran out, up to 34 s after the close since #11. The wait now answers once
/// the pipeline is sealed and its workers are gone, because nothing in this
/// process can settle the id after that.
#[tokio::test]
async fn a_wait_on_a_replay_owed_receipt_ends_when_the_session_closes() {
    let inner = Arc::new(MemoryStore::new());
    let agent = AgentId::new("agent-a");
    let receipt = defer_one_intent(&inner, "intent-wait-close", &agent).await;

    // Session 2 cannot replay (the embedder is down), so the id stays owed.
    let mem2 = Memory::builder()
        .session("intent-wait-close")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(DeadEmbedder) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build session 2");
    assert_eq!(
        mem2.pipeline().lookup(&agent, receipt).tag(),
        "pending_replay"
    );

    let wait = mem2
        .pipeline()
        .wait(&agent, receipt, crate::writeq::RECEIPT_WAIT_MAX);
    let close = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        mem2.close().await.expect("clean close");
    };
    let (answer, ()) =
        tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(wait, close) })
            .await
            .expect("the wait must end with the close, not run out its RECEIPT_WAIT_MAX");
    assert_eq!(
        answer.tag(),
        "pending_replay",
        "the honest answer: the next serve owes the replay"
    );

    // And a wait that starts after the close answers at once.
    let started = std::time::Instant::now();
    let after = mem2
        .pipeline()
        .wait(&agent, receipt, crate::writeq::RECEIPT_WAIT_MAX)
        .await;
    assert_eq!(after.tag(), "pending_replay");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

/// A session whose acked write cannot drain (the embedder hangs on real
/// content), over `store`, with that write admitted. For the #117 review M1
/// tests: the write's receipt is what the close has to answer.
async fn undrainable_write(
    session: &str,
    store: Arc<dyn GraphStore>,
) -> (Memory, AgentId, crate::writeq::ReceiptId, Arc<AtomicUsize>) {
    let agent = AgentId::new("agent-a");
    let embedder = HangingEmbedder::new();
    let entered = embedder.entered.clone();
    let mem = Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(Arc::new(embedder) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build");
    let submitted = mem
        .derive_async_as(
            &agent,
            &[("user schema", ConceptType::Entity)],
            &ParentOf::none(),
            None,
        )
        .await
        .expect("ack");
    assert!(!submitted.dropped(), "the write must be ADMITTED");
    (mem, agent, submitted.receipt, entered)
}

/// #117 review M1: a close whose final flush FAILS must not answer
/// `intent_durable` — the intent never reached the store. It answers
/// `close_unflushed`; the retried close that does flush upgrades it, and the
/// intent is then really in the store.
#[tokio::test]
async fn a_failed_final_flush_answers_close_unflushed_until_a_retry_lands_it() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn GraphStore> = Arc::new(FlakyStore::new(
        Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>,
        1,
    ));
    let (mem, agent, receipt, _) = undrainable_write("close-unflushed", store).await;

    mem.close()
        .await
        .expect_err("the one failing flush is close's");
    assert_eq!(
        mem.pipeline().lookup(&agent, receipt).tag(),
        "close_unflushed",
        "a close whose flush failed cannot vouch for the intent"
    );
    assert_eq!(mem.pipeline().counters().deferred(), 0);
    assert!(
        inner
            .load_session(&SessionId::new("close-unflushed"))
            .await
            .is_err(),
        "and indeed nothing reached the store"
    );

    mem.close().await.expect("the store recovered");
    assert_eq!(
        mem.pipeline().lookup(&agent, receipt).tag(),
        "intent_durable",
        "the retry made the intent durable, so the answer is upgraded"
    );
    assert_eq!(mem.pipeline().counters().deferred(), 1);
    let snap = inner
        .load_session(&SessionId::new("close-unflushed"))
        .await
        .expect("durable now");
    assert_eq!(snap.write_intents.len(), 1);
}

/// #117 review M1: a close on a handle that LOST its lease drops the tail,
/// so the set-aside receipt answers `close_unflushed`, never
/// `intent_durable`.
#[tokio::test]
async fn a_lease_lost_close_answers_close_unflushed() {
    let inner = Arc::new(MemoryStore::new());
    let session = SessionId::new("close-fenced");
    let (mem, agent, receipt, entered) = undrainable_write(
        "close-fenced",
        Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>,
    )
    .await;

    // Wait until the lane worker has taken the job into the (hanging)
    // embedder before the fence: a job still queued at the fence is refused
    // by the worker's own per-job check (`failed`, its own honest message)
    // and never reaches the close's set-aside.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while entered.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the job never reached the embedder"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    inner.force_expire_lease(&session);
    let taker = LeaseHolder::for_this_process(&AgentId::new("agent-b"));
    assert!(inner
        .acquire_lease(&session, &taker, LEASE_TTL)
        .await
        .unwrap()
        .is_acquired());
    mem.simulate_lease_loss();

    let err = mem.close().await.expect_err("a fenced close refuses");
    assert!(
        err.to_string().contains("lost its single-writer lease"),
        "{err}"
    );
    assert_eq!(
        mem.pipeline().lookup(&agent, receipt).tag(),
        "close_unflushed",
        "a fenced close dropped the tail and cannot vouch for the intent"
    );
    assert_eq!(mem.pipeline().counters().deferred(), 0);
}

/// [`VectorSearchable`] whose `flush` parks, once armed, until released:
/// the flush loop's in-flight flush the #293 review's close test needs.
struct GatedFlush {
    inner: VectorSearchable,
    armed: AtomicBool,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl GraphStore for GatedFlush {
    fn capabilities(&self) -> crate::store::Capabilities {
        self.inner.capabilities()
    }
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.inner.init_schema().await
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.flush(batch, token).await
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.inner.load_session(session).await
    }
    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner.keyword_candidates(session, tokens, limit).await
    }
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner
            .vector_candidates(session, embedding, limit)
            .await
    }
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner
            .vector_candidates_checked(session, embedding, expected_contract, limit)
            .await
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.inner
            .blast_radius(session, node, min_edge_age, now)
            .await
    }
    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        self.inner
            .interaction_span(session, node, min_age, now)
            .await
    }
    async fn interaction_span_with_window(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
        window: Duration,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        self.inner
            .interaction_span_with_window(session, node, min_age, now, window)
            .await
    }
    async fn record_canonization(
        &self,
        event: &crate::types::CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.inner.record_canonization(event, token).await
    }
    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.inner.acquire_lease(session, holder, ttl).await
    }
    async fn read_lease(
        &self,
        session: &SessionId,
    ) -> Result<Option<crate::store::LeaseInfo>, StoreError> {
        self.inner.read_lease(session).await
    }
    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.inner.refresh_lease(session, holder, ttl).await
    }
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        self.inner.release_lease(session, holder).await
    }
}

/// #293 review: the fence can latch while `close()` stops the flush loop.
/// A flush in flight when close starts is refused as stale (the lease moved
/// on and this handle's heartbeat has not beaten yet), the loop latches the
/// fence and drops `pending`, and close, which checked the fence only before
/// it stopped the loop, used to find an empty log and return `Ok`, answering
/// the set-aside receipt `intent_durable` over a batch that never landed.
/// Close re-checks the fence after joining the loop. Mirrors
/// `Flush.close_ok_no_drop` (the bug: `Flush.close_ok_over_drop`).
#[tokio::test]
async fn a_stale_flush_during_close_answers_close_unflushed() {
    let (logs, _guard) = capture_logs(tracing::Level::INFO);
    let inner = Arc::new(MemoryStore::new());
    let session = SessionId::new("close-stale");
    let gate = Arc::new(GatedFlush {
        inner: VectorSearchable(inner.clone()),
        armed: AtomicBool::new(false),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let agent = AgentId::new("agent-a");
    let embedder = HangingEmbedder::new();
    let entered = embedder.entered.clone();
    let mem = Memory::builder()
        .session("close-stale")
        .agent("agent-a")
        .flush_interval(Duration::from_millis(50))
        .store(gate.clone() as Arc<dyn GraphStore>)
        .embedder(Arc::new(embedder) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build");
    let submitted = mem
        .derive_async_as(
            &agent,
            &[("user schema", ConceptType::Entity)],
            &ParentOf::none(),
            None,
        )
        .await
        .expect("ack");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while entered.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the job never reached the embedder"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // A mutation for the flush loop to send, and the flush it sends parks.
    gate.armed.store(true, Ordering::SeqCst);
    {
        let mut graph = mem.graph.write();
        let previous_id = graph.temporal_chain().last().copied();
        graph
            .insert_interaction(crate::Interaction {
                event_time: None,
                id: NodeId(uuid::Uuid::from_u64_pair(293, 1)),
                session_id: session.clone(),
                agent_id: agent.clone(),
                prompt_text: Some("in flight at close".into()),
                previous_id,
                created_at: Utc::now(),
            })
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .expect("the flush loop sent the batch");

    // The lease moves on before this handle's heartbeat (15 s) notices.
    inner.force_expire_lease(&session);
    let taker = LeaseHolder::for_this_process(&AgentId::new("agent-b"));
    assert!(inner
        .acquire_lease(&session, &taker, LEASE_TTL)
        .await
        .unwrap()
        .is_acquired());
    assert!(!mem.lease_lost(), "nothing has noticed yet");

    // Close runs past its fence check and waits on the loop; only then does
    // the parked flush return, refused as stale.
    let release = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !logs.contains("flush_join started") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "close never reached the flush join"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // If the heartbeat had latched first, close would take its first
        // fence check and this test would no longer cover the re-check.
        assert!(
            !logs.contains("single-writer lease LOST"),
            "the heartbeat latched before close's first check; rerun"
        );
        gate.release.notify_one();
    };
    let (closed, ()) = tokio::join!(mem.close(), release);

    let err = closed.expect_err("close must not report Ok over a batch it did not write");
    assert!(matches!(err, LamboError::Conflict(_)), "{err:?}");
    assert!(mem.lease_lost(), "the stale flush latched the fence");
    assert_eq!(
        mem.pipeline().lookup(&agent, submitted.receipt).tag(),
        "close_unflushed",
        "a close that lost the tail cannot vouch for the set-aside intent"
    );
    assert_eq!(mem.pipeline().counters().deferred(), 0);
    let row = inner.read_lease(&session).await.unwrap().unwrap();
    assert_eq!(
        row.holder,
        taker.token(),
        "the lease is not ours to release"
    );
}

/// [`VectorSearchable`] whose checked vector lookup is slower than a write's
/// whole I/O budget, every time (#125 Sonnet review).
struct SlowLookup(VectorSearchable);

#[async_trait::async_trait]
impl GraphStore for SlowLookup {
    fn capabilities(&self) -> crate::store::Capabilities {
        self.0.capabilities() | crate::store::Capabilities::VECTOR_SEARCH
    }
    async fn init_schema(&self) -> Result<(), crate::types::StoreError> {
        self.0.init_schema().await
    }
    async fn flush(
        &self,
        batch: &crate::types::MutationBatch,
        token: Option<u64>,
    ) -> Result<(), crate::types::StoreError> {
        self.0.flush(batch, token).await
    }
    async fn load_session(
        &self,
        session: &crate::types::SessionId,
    ) -> Result<crate::types::GraphSnapshot, crate::types::StoreError> {
        self.0.load_session(session).await
    }
    async fn keyword_candidates(
        &self,
        session: &crate::types::SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<crate::types::Scored<NodeId>>, crate::types::StoreError> {
        self.0.keyword_candidates(session, tokens, limit).await
    }
    async fn vector_candidates(
        &self,
        _session: &crate::types::SessionId,
        _embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<crate::types::Scored<NodeId>>, crate::types::StoreError> {
        Ok(Vec::new())
    }
    async fn vector_candidates_checked(
        &self,
        _session: &crate::types::SessionId,
        _embedding: &[f32],
        _expected_contract: &crate::types::EmbeddingContract,
        _limit: usize,
    ) -> Result<Vec<crate::types::Scored<NodeId>>, crate::types::StoreError> {
        // Slower than any write's whole budget, every time.
        tokio::time::sleep(crate::graph::hybrid::HYBRID_IO_TIMEOUT * 2).await;
        Ok(Vec::new())
    }
    async fn blast_radius(
        &self,
        session: &crate::types::SessionId,
        node: crate::types::NodeId,
        min_edge_age: Duration,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, crate::types::StoreError> {
        self.0.blast_radius(session, node, min_edge_age, now).await
    }
    async fn interaction_span(
        &self,
        session: &crate::types::SessionId,
        node: crate::types::NodeId,
        min_age: Duration,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<crate::types::InteractionSpan, crate::types::StoreError> {
        self.0.interaction_span(session, node, min_age, now).await
    }
    async fn interaction_span_with_window(
        &self,
        session: &crate::types::SessionId,
        node: crate::types::NodeId,
        min_age: Duration,
        now: chrono::DateTime<chrono::Utc>,
        window: Duration,
    ) -> Result<crate::types::InteractionSpan, crate::types::StoreError> {
        self.0
            .interaction_span_with_window(session, node, min_age, now, window)
            .await
    }
    async fn record_canonization(
        &self,
        event: &crate::types::CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), crate::types::StoreError> {
        self.0.record_canonization(event, token).await
    }
    async fn acquire_lease(
        &self,
        session: &crate::types::SessionId,
        holder: &crate::store::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::LeaseOutcome, crate::types::StoreError> {
        self.0.acquire_lease(session, holder, ttl).await
    }
    async fn read_lease(
        &self,
        session: &crate::types::SessionId,
    ) -> Result<Option<crate::store::LeaseInfo>, crate::types::StoreError> {
        self.0.read_lease(session).await
    }
    async fn refresh_lease(
        &self,
        session: &crate::types::SessionId,
        holder: &crate::store::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::LeaseOutcome, crate::types::StoreError> {
        self.0.refresh_lease(session, holder, ttl).await
    }
    async fn release_lease(
        &self,
        session: &crate::types::SessionId,
        holder: &crate::store::LeaseHolder,
    ) -> Result<(), crate::types::StoreError> {
        self.0.release_lease(session, holder).await
    }
}

/// **#125 (Sonnet review)**: a lookup timeout at replay is settled `failed`
/// instead of being left durable as an embedder outage, so a write whose
/// lookup is slow every time cannot hold the backlog on every restart; and a
/// run of [`REPLAY_LOOKUP_TIMEOUT_LIMIT`] of them stops the replay with the
/// rest durable, because then the store, not one write, is slow.
///
/// Red before: each timeout was `EmbedUnavailable`, left durable, and counted
/// toward embedder sickness, so three stopped the replay with none settled,
/// on every restart.
#[tokio::test(start_paused = true)]
async fn a_lookup_timeout_at_replay_settles_failed_and_a_run_of_them_stops_it() {
    use crate::writeq::{ReceiptAnswer, ReplayBlockReason, REPLAY_LOOKUP_TIMEOUT_LIMIT};
    let inner = Arc::new(MemoryStore::new());
    let agent = AgentId::new("agent-a");
    let mem = Memory::builder()
        .session("intent-slow-lookup")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(HangingEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build the deferring session");
    let mut receipts = Vec::new();
    for i in 0..REPLAY_LOOKUP_TIMEOUT_LIMIT + 1 {
        let content = format!("slow lookup concept {i}");
        let submitted = mem
            .derive_async_as(
                &agent,
                &[(content.as_str(), ConceptType::Entity)],
                &ParentOf::none(),
                None,
            )
            .await
            .expect("ack");
        assert!(!submitted.dropped());
        receipts.push(submitted.receipt);
    }
    mem.close().await.expect("clean close");

    let mem2 = Memory::builder()
        .session("intent-slow-lookup")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(SlowLookup(VectorSearchable(inner.clone()))) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build session 2");
    let settled = REPLAY_LOOKUP_TIMEOUT_LIMIT - 1;
    for _ in 0..10_000 {
        if mem2.pipeline().counters().replay_blocked() != ReplayBlockReason::None {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(
        mem2.pipeline().counters().replay_blocked(),
        ReplayBlockReason::Other,
        "a run of lookup timeouts stops the replay as a store fault, not an embedder one"
    );
    for (i, receipt) in receipts.iter().enumerate() {
        let answer = mem2.pipeline().lookup(&agent, *receipt);
        if i < settled {
            match answer {
                ReceiptAnswer::Failed(why) => {
                    assert!(why.contains("candidate lookup"), "{why}")
                }
                other => panic!("intent {i} must settle failed: {other:?}"),
            }
        } else {
            assert_eq!(answer.tag(), "pending_replay", "intent {i} stays durable");
        }
    }
    assert_eq!(
        mem2.pipeline().counters().replay_owed(),
        (receipts.len() - settled) as u64
    );
    mem2.close().await.expect("clean close 2");

    // The next serve, with a healthy store, applies what was left durable.
    let mem3 = Memory::builder()
        .session("intent-slow-lookup")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build session 3");
    for _ in 0..10_000 {
        if mem3.pipeline().counters().replay_owed() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for receipt in &receipts[settled..] {
        assert_eq!(
            mem3.pipeline().lookup(&agent, *receipt).tag(),
            "applied_after_restart"
        );
    }
    mem3.close().await.expect("clean close 3");
}

/// One unconsumed derive intent a previous process left, for
/// `MemoryStore::seed`: receipt `lwr1.{epoch}.{issued_ms}.{seq}`, as
/// `ReceiptId`'s `Display` writes it.
#[cfg(feature = "fixtures")]
#[allow(clippy::too_many_arguments)]
fn owed_derive(
    sid: &SessionId,
    agent: &AgentId,
    interaction: NodeId,
    epoch: u64,
    issued_ms: i64,
    seq: u64,
    concepts: &[(&str, ConceptType)],
    pairs: &[(&str, &str)],
) -> crate::types::WriteIntent {
    crate::types::WriteIntent {
        session_id: sid.clone(),
        receipt: format!("lwr1.{epoch:016x}.{issued_ms:x}.{seq:x}"),
        agent: agent.clone(),
        interaction,
        lane_seq: seq,
        issued_ms,
        payload: crate::types::WriteIntentPayload::Derive {
            receipt_destination: None,
            concepts: concepts.iter().map(|(c, t)| (c.to_string(), *t)).collect(),
            pairs: pairs
                .iter()
                .map(|(p, c)| (p.to_string(), c.to_string()))
                .collect(),
        },
        created_at: Utc::now(),
        outcome: None,
    }
}

/// Seed `inner` with a session whose only content is one interaction and the
/// given intents, as `load_session` returns them (`intents` is the load
/// order), then attach `strategy` over it and wait until the replay has
/// paid its debt or named a block reason (it stopped, or ended with intents
/// owed: since #295 a finished replay that leaves a debt always names one).
#[cfg(feature = "fixtures")]
async fn replay_planted(
    inner: &Arc<MemoryStore>,
    session: &str,
    strategy: MatchStrategy,
    intents: impl FnOnce(&SessionId, NodeId) -> Vec<crate::types::WriteIntent>,
) -> Memory {
    let mem = attach_planted(
        inner,
        session,
        strategy,
        Arc::new(FixtureEmbedder::new()),
        intents,
    )
    .await;
    wait_for(&mem, |mem| {
        mem.pipeline().counters().replay_owed() == 0
            || mem.pipeline().counters().replay_blocked() != crate::writeq::ReplayBlockReason::None
    })
    .await;
    mem
}

/// Wait (bounded) until `done` holds, then a moment more: the replay
/// publishes a settle just after it moves its counters.
#[cfg(feature = "fixtures")]
async fn wait_for(mem: &Memory, done: impl Fn(&Memory) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !done(mem) {
        assert!(
            std::time::Instant::now() < deadline,
            "the replay never reached the awaited state"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// [`replay_planted`]'s seed and attach, with the embedder given and no
/// wait.
#[cfg(feature = "fixtures")]
async fn attach_planted(
    inner: &Arc<MemoryStore>,
    session: &str,
    strategy: MatchStrategy,
    embedder: Arc<dyn Embedder>,
    intents: impl FnOnce(&SessionId, NodeId) -> Vec<crate::types::WriteIntent>,
) -> Memory {
    let sid = SessionId::new(session);
    let interaction = NodeId::new();
    let now = Utc::now();
    inner
        .seed(GraphSnapshot {
            session_id: sid.clone(),
            embedding: Some(contract("fixture", 1024)),
            interactions: vec![Interaction {
                event_time: None,
                id: interaction,
                session_id: sid.clone(),
                agent_id: AgentId::new("agent-a"),
                prompt_text: None,
                previous_id: None,
                created_at: now,
            }],
            write_intents: intents(&sid, interaction),
            ..GraphSnapshot::default()
        })
        .expect("seed");
    Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(embedder)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(strategy)
        .build()
        .await
        .expect("build")
}

/// **#294**: a replayed intent refused because its input no longer fits the
/// graph it meets (an Observation derived twice, a child given a second
/// Hierarchical parent) settles `failed`, and the replay goes on, as it
/// does for a cycle refusal and as the in-session worker does for the same
/// error.
///
/// The issue's scenario: a `Canonical` session acked each pair of writes
/// against a graph that held neither, and both flushed before the process
/// died. Before the fix the second of the first pair took the stop arm
/// (block reason `Other`): it and everything behind it stayed
/// `pending_replay`, on every serve.
///
/// Mirrors the Lean theorems `Drain.fixed_input_goes_on` (the replay settles
/// an input refusal `failed` and goes on) and `Drain.fixed_sessionWide_stops`
/// (any other non-transient error still stops it); the code before the fix
/// is `Drain.store_error_blocks_backlog` and
/// `Drain.replayStep_inputRefusal_stops`.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn a_graph_dependent_input_refusal_at_replay_settles_failed_and_the_replay_goes_on() {
    let inner = Arc::new(MemoryStore::new());
    let agent = AgentId::new("agent-a");
    let epoch = 0x00ab_cdef_0123_4567;
    let mem = replay_planted(
        &inner,
        "intent-input-refusal",
        MatchStrategy::Canonical,
        |sid, ix| {
            vec![
                owed_derive(
                    sid,
                    &agent,
                    ix,
                    epoch,
                    1_000,
                    1,
                    &[("tide gauge reading", ConceptType::Observation)],
                    &[],
                ),
                owed_derive(
                    sid,
                    &agent,
                    ix,
                    epoch,
                    1_001,
                    2,
                    &[("tide gauge reading", ConceptType::Observation)],
                    &[],
                ),
                owed_derive(sid, &agent, ix, epoch, 1_002, 3, &[], &[("harbor", "pier")]),
                owed_derive(sid, &agent, ix, epoch, 1_003, 4, &[], &[("marina", "pier")]),
                owed_derive(
                    sid,
                    &agent,
                    ix,
                    epoch,
                    1_004,
                    5,
                    &[("a later fact", ConceptType::Entity)],
                    &[],
                ),
            ]
        },
    )
    .await;
    let tags: Vec<&str> = (1..=5)
        .map(|seq| {
            let r: crate::writeq::ReceiptId = format!("lwr1.{epoch:016x}.{:x}.{seq:x}", 999 + seq)
                .parse()
                .unwrap();
            mem.pipeline().lookup(&agent, r).tag()
        })
        .collect();
    assert_eq!(
        tags,
        [
            "applied_after_restart",
            "failed",
            "applied_after_restart",
            "failed",
            "applied_after_restart"
        ],
        "the input refusals settle failed and the replay applies what is behind them"
    );
    assert_eq!(mem.pipeline().counters().replay_owed(), 0);
    assert_eq!(
        mem.pipeline().counters().replay_blocked(),
        crate::writeq::ReplayBlockReason::None
    );
    mem.graph.read().assert_invariants().unwrap();
    mem.close().await.expect("clean close");
    let snap = inner
        .load_session(&SessionId::new("intent-input-refusal"))
        .await
        .expect("reload");
    assert!(
        snap.write_intents.iter().all(|i| i.outcome.is_some()),
        "every intent is consumed: nothing is owed to the next serve"
    );
}

/// Answers like [`FixtureEmbedder`], except that the first `trips` embeds
/// of a text containing `trip` get a transient answer
/// (`EmbedError::Unavailable`, an embedder that blinked), as at a replay
/// meeting a brief outage.
#[cfg(feature = "fixtures")]
struct Blink {
    inner: FixtureEmbedder,
    trip: &'static str,
    trips: std::sync::Mutex<usize>,
}

#[cfg(feature = "fixtures")]
impl Blink {
    fn arc(trip: &'static str, trips: usize) -> Arc<dyn Embedder> {
        Arc::new(Blink {
            inner: FixtureEmbedder::new(),
            trip,
            trips: std::sync::Mutex::new(trips),
        })
    }
}

#[cfg(feature = "fixtures")]
#[async_trait]
impl Embedder for Blink {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Document)
            .await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Query).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.inner.embed_image(image).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

#[cfg(feature = "fixtures")]
impl Blink {
    async fn embed_as(
        &self,
        text: &str,
        role: crate::test_util::TextRole,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        let blink = text.contains(self.trip) && {
            let mut left = self.trips.lock().unwrap();
            let blink = *left > 0;
            *left = left.saturating_sub(1);
            blink
        };
        if blink {
            return Err(crate::embed::EmbedError::Unavailable(
                "llama.cpp unreachable: connection refused".into(),
            ));
        }
        role.embed(&self.inner, text).await
    }
}

/// The receipt id [`owed_derive`] gives a row.
#[cfg(feature = "fixtures")]
fn owed_receipt(epoch: u64, issued_ms: i64, seq: u64) -> crate::writeq::ReceiptId {
    format!("lwr1.{epoch:016x}.{issued_ms:x}.{seq:x}")
        .parse()
        .unwrap()
}

/// **#295 (1)**: a transient embedder answer at replay must not let the same
/// agent's later intent apply first.
///
/// Agent A owes two writes, agent B one, in that load order. The replay's
/// embed of A's first write blinks at both attempts the serve makes. Before
/// the fix the loop left it durable and applied A's second write, so the next
/// serve applied A's writes in the order 2, 1. Now A is deferred: its second
/// write waits too, B's write still applies, the serve says why its debt is
/// left (`embedder`), and the next serve applies A's two writes in order.
///
/// Mirrors the Lean theorems `Drain.loopD_no_overtake` and
/// `Drain.fixed_agent_order` (per-agent order across serves, whatever the
/// embedder answers); the code before the fix is
/// `Drain.transient_skip_reorders`.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn a_transient_answer_at_replay_defers_the_agent_instead_of_reordering_it() {
    let inner = Arc::new(MemoryStore::new());
    let a = AgentId::new("agent-a");
    let b = AgentId::new("agent-b");
    let epoch = 0x0011_2233_4455_6677;
    let plant = |sid: &SessionId, ix: NodeId| {
        vec![
            owed_derive(
                sid,
                &a,
                ix,
                epoch,
                1_000,
                1,
                &[("first fact of a", ConceptType::Entity)],
                &[],
            ),
            owed_derive(
                sid,
                &a,
                ix,
                epoch,
                1_001,
                2,
                &[("second fact of a", ConceptType::Entity)],
                &[],
            ),
            owed_derive(
                sid,
                &b,
                ix,
                epoch,
                1_002,
                3,
                &[("only fact of b", ConceptType::Entity)],
                &[],
            ),
        ]
    };
    let mem = attach_planted(
        &inner,
        "intent-transient-order",
        MatchStrategy::Hybrid,
        Blink::arc("first fact of a", 2),
        plant,
    )
    .await;
    let (a1, a2, b1) = (
        owed_receipt(epoch, 1_000, 1),
        owed_receipt(epoch, 1_001, 2),
        owed_receipt(epoch, 1_002, 3),
    );
    // The serve names a block reason once it ends with writes owed.
    wait_for(&mem, |mem| {
        mem.pipeline().counters().replay_blocked() != crate::writeq::ReplayBlockReason::None
    })
    .await;
    assert_eq!(
        [
            mem.pipeline().lookup(&a, a1).tag(),
            mem.pipeline().lookup(&a, a2).tag(),
            mem.pipeline().lookup(&b, b1).tag(),
        ],
        ["pending_replay", "pending_replay", "applied_after_restart"],
        "A's second write must not apply while its first is left for the next serve"
    );
    assert_eq!(mem.pipeline().counters().replay_owed(), 2);
    assert_eq!(
        mem.pipeline().counters().replay_blocked(),
        crate::writeq::ReplayBlockReason::Embedder,
        "writes are owed because the embedder did not answer them: the serve says so"
    );
    mem.close().await.expect("clean close");

    // The next serve applies A's writes, in order.
    let mem2 = Memory::builder()
        .session("intent-transient-order")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build session 2");
    wait_for(&mem2, |mem| mem.pipeline().counters().replay_owed() == 0).await;
    assert_eq!(
        mem2.pipeline().lookup(&a, a1).tag(),
        "applied_after_restart"
    );
    assert_eq!(
        mem2.pipeline().lookup(&a, a2).tag(),
        "applied_after_restart"
    );
    mem2.close().await.expect("clean close 2");
}

/// **#295 (2)**: one process's intents replay in admission (`lane_seq`)
/// order even when its clock stepped back between two admissions.
///
/// The adapters load by (`issued_ms`, `lane_seq`), so A's second write,
/// stamped a millisecond before its first, loads first; both derive one
/// Observation, so whichever applies first wins and the other is refused
/// (#294). Before the fix the second write won.
///
/// Mirrors the Lean theorems `Drain.fixedLoad_sorted` and
/// `Drain.fixed_epoch_seq_order` (per-process sequence order with no clock
/// assumption); the code before the fix is `Drain.clock_step_reorders`.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn a_clock_step_between_two_admissions_does_not_reorder_their_replay() {
    let inner = Arc::new(MemoryStore::new());
    let a = AgentId::new("agent-a");
    let epoch = 0x0099_8877_6655_4433;
    let mem = replay_planted(
        &inner,
        "intent-clock-step",
        MatchStrategy::Canonical,
        |sid, ix| {
            // The adapters' (`issued_ms`, `lane_seq`) order.
            vec![
                owed_derive(
                    sid,
                    &a,
                    ix,
                    epoch,
                    999,
                    2,
                    &[("harbor tide note", ConceptType::Observation)],
                    &[],
                ),
                owed_derive(
                    sid,
                    &a,
                    ix,
                    epoch,
                    1_000,
                    1,
                    &[("harbor tide note", ConceptType::Observation)],
                    &[],
                ),
            ]
        },
    )
    .await;
    assert_eq!(
        [
            mem.pipeline()
                .lookup(&a, owed_receipt(epoch, 1_000, 1))
                .tag(),
            mem.pipeline().lookup(&a, owed_receipt(epoch, 999, 2)).tag(),
        ],
        ["applied_after_restart", "failed"],
        "the first admitted write applies first, whatever its clock said"
    );
    mem.close().await.expect("clean close");
}

/// **#295 (3)**: an intent whose receipt id does not parse is settled
/// `failed` (it can be neither applied nor looked up by its id), not skipped
/// on every serve and counted owed forever.
///
/// Mirrors the Lean theorem `Drain.fixed_badId_settled`; the code before the
/// fix is `Drain.bad_id_never_settled`.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn an_intent_with_an_unparseable_receipt_id_is_settled_failed() {
    let inner = Arc::new(MemoryStore::new());
    let a = AgentId::new("agent-a");
    let epoch = 0x0055_5555_5555_5555;
    let mem = attach_planted(
        &inner,
        "intent-bad-id",
        MatchStrategy::Canonical,
        Arc::new(FixtureEmbedder::new()),
        |sid, ix| {
            let mut bad = owed_derive(
                sid,
                &a,
                ix,
                epoch,
                1_000,
                1,
                &[("lost fact", ConceptType::Entity)],
                &[],
            );
            bad.receipt = "lwr1.not-a-receipt".into();
            vec![
                bad,
                owed_derive(
                    sid,
                    &a,
                    ix,
                    epoch,
                    1_001,
                    2,
                    &[("kept fact", ConceptType::Entity)],
                    &[],
                ),
            ]
        },
    )
    .await;
    let good = owed_receipt(epoch, 1_001, 2);
    wait_for(&mem, |mem| {
        mem.pipeline().lookup(&a, good).tag() != "pending_replay"
    })
    .await;
    assert_eq!(
        mem.pipeline().lookup(&a, good).tag(),
        "applied_after_restart"
    );
    assert_eq!(
        mem.pipeline().counters().replay_owed(),
        0,
        "the unparseable row is no longer owed"
    );
    mem.close().await.expect("clean close");
    let snap = inner
        .load_session(&SessionId::new("intent-bad-id"))
        .await
        .expect("reload");
    let bad = snap
        .write_intents
        .iter()
        .find(|i| i.receipt == "lwr1.not-a-receipt")
        .expect("the row is kept, settled");
    assert_eq!(
        bad.outcome.as_ref().map(|o| o.tag.as_str()),
        Some("failed"),
        "settled, so no later serve skips it again"
    );
}

/// An embedder that is down (`EmbedError::Unavailable`) while its switch is
/// on, for real content; the calibration probe's own texts always answer.
struct Switchable {
    inner: FixtureEmbedder,
    down: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl Embedder for Switchable {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Document)
            .await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Query).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.inner.embed_image(image).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

impl Switchable {
    async fn embed_as(
        &self,
        text: &str,
        role: crate::test_util::TextRole,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        if self.down.load(Ordering::SeqCst) && !text.contains(crate::writeq::PROBE_TEXT) {
            return Err(crate::embed::EmbedError::Unavailable(
                "llama.cpp unreachable: connection refused".into(),
            ));
        }
        role.embed(&self.inner, text).await
    }
}

/// A session over `inner` whose embedder is down while `down` is set.
async fn switchable_session(
    inner: &Arc<MemoryStore>,
    session: &str,
    down: &Arc<std::sync::atomic::AtomicBool>,
) -> Memory {
    Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(Arc::new(Switchable {
            inner: FixtureEmbedder::new(),
            down: down.clone(),
        }) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build")
}

/// Derive one concept as `agent` and wait until the worker has taken the
/// write (its receipt is no longer `pending`).
async fn derive_and_wait(mem: &Memory, agent: &AgentId, content: &str) -> crate::writeq::ReceiptId {
    let submitted = mem
        .derive_async_as(
            agent,
            &[(content, ConceptType::Entity)],
            &ParentOf::none(),
            None,
        )
        .await
        .expect("ack");
    assert!(!submitted.dropped());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while mem.pipeline().lookup(agent, submitted.receipt).tag() == "pending" {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker never took the write"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    submitted.receipt
}

/// Derive one `parent_of` pair as `agent` and wait until the worker has
/// taken the write. Two pairs in opposite directions close a cycle, so of the
/// two writes whichever applies second is refused: the answers show the
/// order they applied in.
async fn pair_and_wait(
    mem: &Memory,
    agent: &AgentId,
    pair: &'static [(&'static str, &'static str)],
) -> crate::writeq::ReceiptId {
    let submitted = mem
        .derive_async_as(agent, &[], &ParentOf::from_pairs(pair), None)
        .await
        .expect("ack");
    assert!(!submitted.dropped());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while mem.pipeline().lookup(agent, submitted.receipt).tag() == "pending" {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker never took the write"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    submitted.receipt
}

/// **#295 (in session)**: after an embedder outage leaves one of an agent's
/// writes owed, the agent's later write in the same server must not apply
/// ahead of it.
///
/// A's two writes are opposite `parent_of` pairs, so whichever applies
/// second is refused as a cycle. Before the fix the first answered `failed`
/// with its intent kept and the second applied; the next serve then refused
/// the first. Now the second is held behind the first: both answer
/// `pending_replay`, a clean close keeps both as durable intents, and the
/// next serve applies the first and refuses the second.
///
/// Mirrors the Lean theorems `Drain.stepW_ord` and
/// `Drain.sessionW_then_serve_order`; the code before the fix is
/// `Drain.outage_then_replay_reorders`.
#[tokio::test]
async fn an_outage_in_session_holds_the_agents_later_writes_behind_the_owed_one() {
    let inner = Arc::new(MemoryStore::new());
    let a = AgentId::new("agent-a");
    let down = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mem = switchable_session(&inner, "in-session-hold", &down).await;
    let a1 = pair_and_wait(&mem, &a, &[("hold alpha", "hold beta")]).await;
    let a2 = pair_and_wait(&mem, &a, &[("hold beta", "hold alpha")]).await;
    assert_eq!(
        [
            mem.pipeline().lookup(&a, a1).tag(),
            mem.pipeline().lookup(&a, a2).tag()
        ],
        ["pending_replay", "pending_replay"],
        "a's second write must wait behind its first, not apply ahead of it"
    );
    mem.close().await.expect("clean close");
    assert_eq!(mem.pipeline().lookup(&a, a1).tag(), "intent_durable");
    assert_eq!(mem.pipeline().lookup(&a, a2).tag(), "intent_durable");

    down.store(false, Ordering::SeqCst);
    let mem2 = switchable_session(&inner, "in-session-hold", &down).await;
    wait_for_owed(&mem2).await;
    assert_eq!(
        [
            mem2.pipeline().lookup(&a, a1).tag(),
            mem2.pipeline().lookup(&a, a2).tag()
        ],
        ["applied_after_restart", "failed"],
        "the first applied first; the second, closing the cycle, was refused"
    );
    mem2.close().await.expect("clean close 2");
}

/// **#295 (in session)**: once the embedder is back, the agent's next write
/// first applies the held one, then itself, in this server; another agent's
/// write is not held.
///
/// As above, the two writes are opposite pairs: the held first applies and
/// the second is refused. Before the fix the first answered `failed` and the
/// second applied ahead of it.
///
/// Mirrors `Drain.retry_settles_head` and `Drain.stepW_ord`.
#[tokio::test]
async fn a_held_write_applies_before_the_agents_next_write_once_the_embedder_is_back() {
    let inner = Arc::new(MemoryStore::new());
    let a = AgentId::new("agent-a");
    let b = AgentId::new("agent-b");
    let down = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mem = switchable_session(&inner, "in-session-recover", &down).await;
    let a1 = pair_and_wait(&mem, &a, &[("recover alpha", "recover beta")]).await;
    assert_eq!(mem.pipeline().lookup(&a, a1).tag(), "pending_replay");
    down.store(false, Ordering::SeqCst);
    let b1 = derive_and_wait(&mem, &b, "only fact of b").await;
    assert_eq!(
        mem.pipeline().lookup(&b, b1).tag(),
        "applied",
        "b is not held"
    );
    let a2 = pair_and_wait(&mem, &a, &[("recover beta", "recover alpha")]).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while mem.pipeline().lookup(&a, a2).tag() == "pending_replay" {
        assert!(std::time::Instant::now() < deadline, "a2 never settled");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        [
            mem.pipeline().lookup(&a, a1).tag(),
            mem.pipeline().lookup(&a, a2).tag()
        ],
        ["applied", "failed"],
        "the held write applied first; the later one, closing the cycle, was refused"
    );
    mem.close().await.expect("clean close");
    let snap = inner
        .load_session(&SessionId::new("in-session-recover"))
        .await
        .expect("reload");
    assert!(snap.write_intents.iter().all(|i| i.outcome.is_some()));
}

/// **#295 review (S4)**: a lookup timeout in session follows the replay's
/// rule: one is a fact about that write and settles `failed`; it does not
/// hold the agent's lane as an embedder outage does.
#[tokio::test(start_paused = true)]
async fn a_lookup_timeout_in_session_settles_failed_and_holds_nothing() {
    let inner = Arc::new(MemoryStore::new());
    let a = AgentId::new("agent-a");
    let mem = Memory::builder()
        .session("in-session-slow-lookup")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(SlowLookup(VectorSearchable(inner.clone()))) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build");
    let submitted = mem
        .derive_async_as(
            &a,
            &[("slow lookup fact", ConceptType::Entity)],
            &ParentOf::none(),
            None,
        )
        .await
        .expect("ack");
    for _ in 0..10_000 {
        if mem.pipeline().lookup(&a, submitted.receipt).tag() != "pending" {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    match mem.pipeline().lookup(&a, submitted.receipt) {
        crate::writeq::ReceiptAnswer::Failed(why) => {
            assert!(why.contains("candidate lookup"), "{why}")
        }
        other => panic!("a lookup timeout settles failed, not held: {other:?}"),
    }
    mem.close().await.expect("clean close");
}

/// Wait (bounded) until a replay has nothing owed.
async fn wait_for_owed(mem: &Memory) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while mem.pipeline().counters().replay_owed() != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the replay never finished"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// The three intents of [`a_transient_answer_at_replay_defers_the_agent_instead_of_reordering_it`]:
/// two of agent A, then one of agent B.
#[cfg(feature = "fixtures")]
fn two_of_a_one_of_b(
    sid: &SessionId,
    ix: NodeId,
    epoch: u64,
    first: (&str, ConceptType),
) -> Vec<crate::types::WriteIntent> {
    let (a, b) = (AgentId::new("agent-a"), AgentId::new("agent-b"));
    vec![
        owed_derive(sid, &a, ix, epoch, 1_000, 1, &[first], &[]),
        owed_derive(
            sid,
            &a,
            ix,
            epoch,
            1_001,
            2,
            &[("second fact of a", ConceptType::Entity)],
            &[],
        ),
        owed_derive(
            sid,
            &b,
            ix,
            epoch,
            1_002,
            3,
            &[("only fact of b", ConceptType::Entity)],
            &[],
        ),
    ]
}

/// **#295 review (starvation)**: an agent deferred at a transient answer gets
/// one more attempt at the end of the pass, so a write that blinked once does
/// not wait for the next serve. Here the retry applies A's two writes, in
/// order, after B's; nothing is owed and nothing is blocked.
///
/// Mirrors `Drain.replayP_progress` and `Drain.replayP_preserves`.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn a_deferred_agent_is_retried_once_at_the_end_of_the_pass() {
    let inner = Arc::new(MemoryStore::new());
    let (a, b) = (AgentId::new("agent-a"), AgentId::new("agent-b"));
    let epoch = 0x0022_3344_5566_7788;
    let mem = attach_planted(
        &inner,
        "intent-retry-pass",
        MatchStrategy::Hybrid,
        Blink::arc("first fact of a", 1),
        |sid, ix| two_of_a_one_of_b(sid, ix, epoch, ("first fact of a", ConceptType::Entity)),
    )
    .await;
    wait_for(&mem, |mem| mem.pipeline().counters().replay_owed() == 0).await;
    assert_eq!(
        [
            mem.pipeline()
                .lookup(&a, owed_receipt(epoch, 1_000, 1))
                .tag(),
            mem.pipeline()
                .lookup(&a, owed_receipt(epoch, 1_001, 2))
                .tag(),
            mem.pipeline()
                .lookup(&b, owed_receipt(epoch, 1_002, 3))
                .tag(),
        ],
        ["applied_after_restart"; 3]
    );
    assert_eq!(
        mem.pipeline().counters().replay_blocked(),
        crate::writeq::ReplayBlockReason::None
    );
    mem.close().await.expect("clean close");
}

/// **#295 review (quarantine)**: an intent whose receipt id is in another
/// format (`lwr2`, as a newer build might write) is not settled `failed`,
/// which cannot be undone. It is left owed, and its agent is deferred so the
/// agent's later write does not overtake it; another agent's write still
/// applies, and the serve names the block reason `other`. A malformed `lwr1`
/// id is still settled `failed`
/// (`an_intent_with_an_unparseable_receipt_id_is_settled_failed`).
///
/// Mirrors `Drain.stepQ_unknownFormat` and `Drain.serveQ_block`; the fix as
/// first committed is `Drain.fixed_badId_settled`.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn an_intent_in_another_receipt_format_is_quarantined_not_settled() {
    let inner = Arc::new(MemoryStore::new());
    let (a, b) = (AgentId::new("agent-a"), AgentId::new("agent-b"));
    let epoch = 0x0033_4455_6677_8899;
    let mem = attach_planted(
        &inner,
        "intent-quarantine",
        MatchStrategy::Canonical,
        Arc::new(FixtureEmbedder::new()),
        |sid, ix| {
            let mut rows = two_of_a_one_of_b(sid, ix, epoch, ("future fact", ConceptType::Entity));
            rows[0].receipt = "lwr2.0000000000000001.3e8.1.extra".into();
            rows
        },
    )
    .await;
    wait_for(&mem, |mem| {
        mem.pipeline().counters().replay_blocked() != crate::writeq::ReplayBlockReason::None
    })
    .await;
    assert_eq!(
        mem.pipeline()
            .lookup(&b, owed_receipt(epoch, 1_002, 3))
            .tag(),
        "applied_after_restart"
    );
    assert_eq!(
        mem.pipeline()
            .lookup(&a, owed_receipt(epoch, 1_001, 2))
            .tag(),
        "pending_replay",
        "A's later write waits behind the quarantined one"
    );
    assert_eq!(mem.pipeline().counters().replay_owed(), 2);
    assert_eq!(
        mem.pipeline().counters().replay_blocked(),
        crate::writeq::ReplayBlockReason::Other
    );
    mem.close().await.expect("clean close");
    let snap = inner
        .load_session(&SessionId::new("intent-quarantine"))
        .await
        .expect("reload");
    let quarantined = snap
        .write_intents
        .iter()
        .find(|i| i.receipt.starts_with("lwr2."))
        .expect("kept");
    assert!(quarantined.outcome.is_none(), "left owed, not settled");
}

/// Answers the probe's texts; real content waits for a permit on `gate`.
#[cfg(feature = "fixtures")]
struct Gated {
    inner: FixtureEmbedder,
    gate: Arc<tokio::sync::Semaphore>,
    entered: Arc<AtomicUsize>,
}

#[cfg(feature = "fixtures")]
#[async_trait]
impl Embedder for Gated {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        if !text.contains(crate::writeq::PROBE_TEXT) {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.gate.acquire().await.expect("gate").forget();
        }
        self.inner.embed(text).await
    }
}

/// **#295 review (S5)**: a replay that halts because the lease is lost
/// leaves the intent it stopped at, and every later one, owed; the block
/// reason says `Other` (the reason a lost lease has), not `null`.
///
/// Mirrors `Drain.serveR_reported`; the code before the fix is the hidden
/// halt in `Drain.serveQ_block`.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn a_replay_halted_by_a_lost_lease_names_a_block_reason() {
    let inner = Arc::new(MemoryStore::new());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let entered = Arc::new(AtomicUsize::new(0));
    let epoch = 0x0044_5566_7788_99aa;
    let mem = attach_planted(
        &inner,
        "intent-halt",
        MatchStrategy::Hybrid,
        Arc::new(Gated {
            inner: FixtureEmbedder::new(),
            gate: gate.clone(),
            entered: entered.clone(),
        }),
        |sid, ix| two_of_a_one_of_b(sid, ix, epoch, ("halt fact", ConceptType::Entity)),
    )
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while entered.load(Ordering::SeqCst) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the replay never embedded"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    mem.simulate_lease_loss();
    gate.add_permits(64);
    wait_for(&mem, |mem| {
        mem.pipeline().counters().replay_blocked() != crate::writeq::ReplayBlockReason::None
    })
    .await;
    assert_eq!(
        mem.pipeline().counters().replay_blocked(),
        crate::writeq::ReplayBlockReason::Other
    );
    assert!(mem.pipeline().counters().replay_owed() > 0);
}

/// **#295 review (S6)**: only an id in another version of the receipt
/// format (`lwr<digits>.`) is quarantined; any other unparseable id is
/// malformed and settled `failed`, so it holds nothing back.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn an_unparseable_id_outside_the_format_family_is_settled_failed() {
    let inner = Arc::new(MemoryStore::new());
    let a = AgentId::new("agent-a");
    let epoch = 0x0055_6677_8899_aabb;
    let mem = replay_planted(
        &inner,
        "intent-garbled",
        MatchStrategy::Canonical,
        |sid, ix| {
            let mut rows = two_of_a_one_of_b(sid, ix, epoch, ("garbled fact", ConceptType::Entity));
            rows[0].receipt = "lwrx.not-a-version".into();
            rows
        },
    )
    .await;
    assert_eq!(mem.pipeline().counters().replay_owed(), 0);
    assert_eq!(
        mem.pipeline()
            .lookup(&a, owed_receipt(epoch, 1_001, 2))
            .tag(),
        "applied_after_restart"
    );
    mem.close().await.expect("clean close");
    let snap = inner
        .load_session(&SessionId::new("intent-garbled"))
        .await
        .expect("reload");
    let garbled = snap
        .write_intents
        .iter()
        .find(|i| i.receipt == "lwrx.not-a-version")
        .expect("kept");
    assert_eq!(
        garbled.outcome.as_ref().map(|o| o.tag.as_str()),
        Some("failed")
    );
}

/// **#295 review #3, SF1 and N3**: in session, three lookup timeouts in a
/// row with nothing applied between hold the third write (the store is slow
/// for everyone), and its retry, which the run does not count again, settles
/// it `failed` if the lookup times out once more. One input slow every time
/// therefore cannot hold the lane for the serve. Before, every retry counted
/// toward the run, so the third write stayed held.
#[tokio::test(start_paused = true)]
async fn a_lookup_timeout_run_holds_the_third_write_and_its_retry_settles_it() {
    let inner = Arc::new(MemoryStore::new());
    let a = AgentId::new("agent-a");
    let mem = Memory::builder()
        .session("in-session-lookup-run")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(SlowLookup(VectorSearchable(inner.clone()))) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build");
    let mut ids = Vec::new();
    for i in 0..crate::writeq::REPLAY_LOOKUP_TIMEOUT_LIMIT {
        let content = format!("slow lookup run fact {i}");
        let submitted = mem
            .derive_async_as(
                &a,
                &[(content.as_str(), ConceptType::Entity)],
                &ParentOf::none(),
                None,
            )
            .await
            .expect("ack");
        for _ in 0..10_000 {
            if mem.pipeline().lookup(&a, submitted.receipt).tag() != "pending" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        ids.push(submitted.receipt);
    }
    let last = *ids.last().unwrap();
    for id in &ids[..ids.len() - 1] {
        assert_eq!(
            mem.pipeline().lookup(&a, *id).tag(),
            "failed",
            "below the run: settled"
        );
    }
    assert_eq!(
        mem.pipeline().lookup(&a, last).tag(),
        "pending_replay",
        "at the run's limit the store is slow for everyone: held"
    );
    for _ in 0..10_000 {
        if mem.pipeline().lookup(&a, last).tag() != "pending_replay" {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    match mem.pipeline().lookup(&a, last) {
        crate::writeq::ReceiptAnswer::Failed(why) => {
            assert!(why.contains("candidate lookup"), "{why}")
        }
        other => panic!("the retry of the held write settles it: {other:?}"),
    }
    mem.close().await.expect("clean close");
}

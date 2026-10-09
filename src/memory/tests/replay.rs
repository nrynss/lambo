//! J3 durable intents: defer at close, replay at the next attach.

use super::*;

// -- J3 durable intents: defer at close, replay at the next attach ------

/// Moved to `crate::test_util` (#22 PR 4) so the MCP tests can serve an
/// image derive over it too.
pub(super) use crate::test_util::VectorSearchable;

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

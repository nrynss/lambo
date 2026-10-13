//! #36 PR 4 on the `Memory` surface: the call-path pre-pass, the background
//! commit, durable-intent replay, the foreign-scope probe and the lock
//! discipline around them.

use super::*;
use crate::graph::cite::ConceptCitations;
use crate::recall::format;
use crate::surface::resolve::ForeignScopes;
use crate::test_util::VectorSearchable;
use crate::types::{EdgeType, ReferenceRefusalKind};
use crate::writeq::{ReceiptAnswer, ReceiptId, RECEIPT_WAIT_MAX};
use std::sync::atomic::{AtomicBool, Ordering};

const RULING: &str = "the writer lease stays with its holder until the holder releases it";
const OTHER_RULING: &str = "flush batches commit in one store transaction";

fn cite(content: &str, depends_on: &[&str]) -> ConceptCitations {
    ConceptCitations {
        content: content.to_string(),
        depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
        against: Vec::new(),
    }
}

async fn memory_with(
    store: Arc<dyn GraphStore>,
    session: &str,
    strategy: MatchStrategy,
    embedder: Arc<dyn Embedder>,
) -> Memory {
    Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(embedder)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(strategy)
        .build()
        .await
        .expect("build")
}

async fn hybrid(session: &str) -> (Memory, Arc<MemoryStore>) {
    let inner = Arc::new(MemoryStore::new());
    let mem = memory_with(
        Arc::new(VectorSearchable(inner.clone())),
        session,
        MatchStrategy::Hybrid,
        Arc::new(FixtureEmbedder::new()),
    )
    .await;
    (mem, inner)
}

async fn canonical(session: &str) -> Memory {
    memory_with(
        Arc::new(MemoryStore::new()),
        session,
        MatchStrategy::Canonical,
        Arc::new(FixtureEmbedder::new()),
    )
    .await
}

fn node_of(mem: &Memory, content: &str) -> NodeId {
    mem.graph
        .read()
        .concepts()
        .find(|c| c.content == content)
        .map(|c| c.id)
        .unwrap_or_else(|| panic!("no concept {content:?}"))
}

async fn settle(mem: &Memory, receipt: ReceiptId) -> ReceiptAnswer {
    let answer = mem
        .pipeline()
        .wait(&AgentId::new("agent-a"), receipt, RECEIPT_WAIT_MAX)
        .await;
    assert!(answer.is_settled(), "{}", answer.tag());
    answer
}

fn kind(err: &LamboError) -> ReferenceRefusalKind {
    match err {
        LamboError::Reference(r) => r.kind.clone(),
        other => panic!("expected a reference refusal, got {other:?}"),
    }
}

/// A refused reference is an `Err` before any receipt exists: no
/// interaction is opened, nothing is queued, the graph is untouched.
#[tokio::test]
async fn a_refused_reference_is_an_error_before_the_ack_and_writes_nothing() {
    let (mem, _) = hybrid("cite-mem-prepass").await;
    let agent = AgentId::new("agent-a");
    mem.derive_as(&agent, &[(RULING, ConceptType::Logic)], &ParentOf::none())
        .await
        .unwrap();
    let before = mem.graph.read().snapshot();
    let accepted = mem.pipeline().counters().accepted();
    let err = mem
        .derive_cited_async_as(
            &agent,
            &[("a decision", ConceptType::Logic)],
            &ParentOf::none(),
            &[cite("a decision", &["no such ruling"])],
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&err), ReferenceRefusalKind::Missing);
    assert_eq!(mem.graph.read().snapshot(), before);
    assert_eq!(mem.pipeline().counters().accepted(), accepted);
    mem.close().await.unwrap();
}

/// What the pre-pass accepts, the background applies, under both
/// strategies: a citation by short id, then an action citing the decision
/// by its full node id, which lands on it and mints no Entity named by the
/// id.
#[tokio::test]
async fn an_accepted_citation_applies_in_the_background_under_both_strategies() {
    for strategy in [MatchStrategy::Canonical, MatchStrategy::Hybrid] {
        let mem = match strategy {
            MatchStrategy::Canonical => canonical("cite-mem-apply-c").await,
            MatchStrategy::Hybrid => hybrid("cite-mem-apply-h").await.0,
        };
        let agent = AgentId::new("agent-a");
        mem.derive_as(&agent, &[(RULING, ConceptType::Logic)], &ParentOf::none())
            .await
            .unwrap();
        let ruling = node_of(&mem, RULING);
        let short = format::short_id(ruling);
        let decision = "flush retries keep the lease";
        let submitted = mem
            .derive_cited_async_as(
                &agent,
                &[(decision, ConceptType::Logic)],
                &ParentOf::none(),
                &[cite(decision, &[&short])],
                None,
            )
            .await
            .unwrap();
        let answer = settle(&mem, submitted.receipt).await;
        let ReceiptAnswer::Applied(summary) = &answer else {
            panic!("{strategy:?}: {}", answer.describe())
        };
        assert!(
            summary.summary.ends_with("; 1 citation(s) recorded"),
            "{}",
            summary.summary
        );
        let d = node_of(&mem, decision);
        assert!(mem
            .graph
            .read()
            .edge_between(d, ruling, EdgeType::Dependency)
            .is_some());

        let concepts = mem.graph.read().concepts().count();
        let full = d.0.to_string();
        let acted = mem
            .record_action_async_as(
                &agent,
                &Action {
                    repo_scope: None,
                    event_time: None,
                    action: "retried a flush under the lease",
                    produces: &[],
                    modifies: &[],
                    depends_on: &[full.as_str()],
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            settle(&mem, acted.receipt).await,
            ReceiptAnswer::Applied(_)
        ));
        let action = node_of(&mem, "retried a flush under the lease");
        assert!(mem
            .graph
            .read()
            .edge_between(action, d, EdgeType::Dependency)
            .is_some());
        assert_eq!(mem.graph.read().concepts().count(), concepts + 1);
        mem.graph.read().assert_invariants().unwrap();
        mem.close().await.unwrap();
    }
}

/// The synchronous path and the acknowledged one refuse the same
/// references with the same kinds: one pre-pass.
#[tokio::test]
async fn the_sync_and_async_paths_refuse_the_same_references() {
    let (mem, _) = hybrid("cite-mem-agree").await;
    let agent = AgentId::new("agent-a");
    // A call cannot cite a concept it creates itself: the target must exist.
    let own = mem
        .derive_cited_as(
            &agent,
            &[
                (RULING, ConceptType::Logic),
                (OTHER_RULING, ConceptType::Logic),
            ],
            &ParentOf::none(),
            &[cite(OTHER_RULING, &[RULING])],
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&own), ReferenceRefusalKind::Missing);
    mem.derive_as(&agent, &[(RULING, ConceptType::Logic)], &ParentOf::none())
        .await
        .unwrap();
    mem.derive_cited_as(
        &agent,
        &[(OTHER_RULING, ConceptType::Logic)],
        &ParentOf::none(),
        &[cite(OTHER_RULING, &[RULING])],
    )
    .await
    .unwrap();
    let cases = [
        (RULING, cite(RULING, &["nothing like it"])),
        (RULING, cite(RULING, &[RULING])),
        (RULING, cite(RULING, &[OTHER_RULING])),
        ("fresh", cite("fresh", &["deadbeef"])),
    ];
    for (content, c) in cases {
        let concepts = [(content, ConceptType::Logic)];
        let sync = mem
            .derive_cited_as(
                &agent,
                &concepts,
                &ParentOf::none(),
                std::slice::from_ref(&c),
            )
            .await
            .unwrap_err();
        let ack = mem
            .derive_cited_async_as(&agent, &concepts, &ParentOf::none(), &[c], None)
            .await
            .unwrap_err();
        assert_eq!(kind(&sync), kind(&ack), "{sync}");
    }
    mem.close().await.unwrap();
}

/// Holds every real-content embed until released; the calibration probe
/// passes through. Shared with `against` (#19(b)).
pub(super) struct GatedEmbedder {
    pub(super) inner: FixtureEmbedder,
    pub(super) hold: tokio::sync::watch::Receiver<bool>,
    pub(super) entered: Arc<AtomicUsize>,
}

#[async_trait]
impl Embedder for GatedEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        if !text.contains(crate::writeq::PROBE_TEXT) {
            let mut hold = self.hold.clone();
            if *hold.borrow() {
                self.entered.fetch_add(1, Ordering::SeqCst);
            }
            while *hold.borrow() {
                if hold.changed().await.is_err() {
                    break;
                }
            }
        }
        self.inner.embed(text).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

/// The `plan_still_holds` question: a cited concept removed between the ack
/// and the commit of a hybrid derive (while its embed is in flight) fails
/// the receipt with the typed `Changed` refusal; nothing of the derive is
/// written, and no edge dangles.
#[tokio::test]
async fn a_cited_concept_removed_between_the_ack_and_the_commit_fails_the_receipt() {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let mem = memory_with(
        Arc::new(VectorSearchable(Arc::new(MemoryStore::new()))),
        "cite-mem-race",
        MatchStrategy::Hybrid,
        Arc::new(GatedEmbedder {
            inner: FixtureEmbedder::new(),
            hold: rx,
            entered: entered.clone(),
        }),
    )
    .await;
    let agent = AgentId::new("agent-a");
    mem.derive_as(&agent, &[(RULING, ConceptType::Logic)], &ParentOf::none())
        .await
        .unwrap();
    let ruling = node_of(&mem, RULING);
    tx.send(true).unwrap();
    let decision = "a decision resting on the ruling";
    let submitted = mem
        .derive_cited_async_as(
            &agent,
            &[(decision, ConceptType::Logic)],
            &ParentOf::none(),
            &[cite(decision, &[&format::short_id(ruling)])],
            None,
        )
        .await
        .expect("the pre-pass accepts: the ruling exists now");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while entered.load(Ordering::SeqCst) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the job never embedded"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    mem.retract(RULING, DryRun::No).await.unwrap();
    tx.send(false).unwrap();
    let answer = settle(&mem, submitted.receipt).await;
    let ReceiptAnswer::Failed(why) = &answer else {
        panic!("{}", answer.describe())
    };
    assert!(
        why.contains("reference refused") && why.contains("names nothing now"),
        "{why}"
    );
    {
        let g = mem.graph.read();
        assert!(g.concepts().all(|c| c.content != decision));
        assert!(g.edges().all(|e| e.target != ruling && e.source != ruling));
    }
    mem.close().await.unwrap();
}

/// Durable intents re-resolve at replay: after a restart, a citation whose
/// target is still there applies (by short id and by full id); one whose
/// target was removed settles `failed` with the typed refusal. Nothing
/// dangles.
#[tokio::test]
async fn a_replayed_citation_re_resolves_and_settles_applied_or_failed() {
    let inner = Arc::new(MemoryStore::new());
    let agent = AgentId::new("agent-a");
    // Session 0 writes the two rulings.
    let mem0 = memory_with(
        Arc::new(VectorSearchable(inner.clone())),
        "cite-replay",
        MatchStrategy::Hybrid,
        Arc::new(FixtureEmbedder::new()),
    )
    .await;
    mem0.derive_as(
        &agent,
        &[
            (RULING, ConceptType::Logic),
            (OTHER_RULING, ConceptType::Logic),
        ],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    let kept = node_of(&mem0, RULING);
    let removed = node_of(&mem0, OTHER_RULING);
    mem0.close().await.unwrap();

    // Session 1 acks three cited derives that cannot apply (the embedder
    // hangs), removes one target, and closes: three durable intents.
    let (tx, rx) = tokio::sync::watch::channel(true);
    let mem1 = memory_with(
        Arc::new(VectorSearchable(inner.clone())),
        "cite-replay",
        MatchStrategy::Hybrid,
        Arc::new(GatedEmbedder {
            inner: FixtureEmbedder::new(),
            hold: rx,
            entered: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .await;
    let mut receipts = Vec::new();
    for (content, reference) in [
        ("cites the kept ruling", format::short_id(kept)),
        ("cites the removed ruling", format::short_id(removed)),
        ("cites by the full id", kept.0.to_string()),
    ] {
        let s = mem1
            .derive_cited_async_as(
                &agent,
                &[(content, ConceptType::Logic)],
                &ParentOf::none(),
                &[cite(content, &[&reference])],
                None,
            )
            .await
            .expect("ack");
        receipts.push(s.receipt);
    }
    mem1.retract(OTHER_RULING, DryRun::No).await.unwrap();
    mem1.close().await.unwrap();
    assert_eq!(mem1.pipeline().counters().deferred(), 3);
    drop(tx);

    // Session 2 replays them.
    let mem2 = memory_with(
        Arc::new(VectorSearchable(inner.clone())),
        "cite-replay",
        MatchStrategy::Hybrid,
        Arc::new(FixtureEmbedder::new()),
    )
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let answers = loop {
        let answers: Vec<ReceiptAnswer> = receipts
            .iter()
            .map(|r| mem2.pipeline().lookup(&agent, *r))
            .collect();
        if answers
            .iter()
            .all(|a| a.tag() != "pending_replay" && a.tag() != "pending")
        {
            break answers;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the replay must settle"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let tags: Vec<&str> = answers.iter().map(ReceiptAnswer::tag).collect();
    assert_eq!(
        tags,
        ["applied_after_restart", "failed", "applied_after_restart"]
    );
    let shown = answers[1].describe();
    assert!(shown.contains("reference refused"), "{shown}");
    {
        let g = mem2.graph.read();
        let citing = g
            .concepts()
            .find(|c| c.content == "cites the kept ruling")
            .expect("applied")
            .id;
        assert!(g.edge_between(citing, kept, EdgeType::Dependency).is_some());
        assert!(g
            .concepts()
            .all(|c| c.content != "cites the removed ruling"));
        let by_full = g
            .concepts()
            .find(|c| c.content == "cites by the full id")
            .expect("applied")
            .id;
        assert!(g
            .edge_between(by_full, kept, EdgeType::Dependency)
            .is_some());
        g.assert_invariants().unwrap();
    }
    mem2.close().await.unwrap();
}

/// A foreign-scope probe that records whether this session's graph lock was
/// free when it ran.
struct LockProbe<'a> {
    mem: &'a Memory,
    ran: AtomicBool,
    lock_was_free: AtomicBool,
}

impl ForeignScopes for LockProbe<'_> {
    fn holder_of(&self, _reference: &str) -> Option<String> {
        self.ran.store(true, Ordering::SeqCst);
        self.lock_was_free
            .store(self.mem.graph.try_write().is_some(), Ordering::SeqCst);
        Some("general".to_string())
    }
}

/// The foreign-scope lookup reads another session only after this
/// session's lock is released (one session's lock at a time), and its
/// answer becomes the refusal.
#[tokio::test]
async fn the_foreign_probe_runs_with_this_sessions_lock_released() {
    let (mem, _) = hybrid("cite-mem-probe").await;
    let agent = AgentId::new("agent-a");
    let probe = LockProbe {
        mem: &mem,
        ran: AtomicBool::new(false),
        lock_was_free: AtomicBool::new(false),
    };
    let err = mem
        .derive_cited_async_scoped(
            &agent,
            &[("a decision", ConceptType::Logic)],
            &ParentOf::none(),
            &[cite("a decision", &["8baad4e0"])],
            None,
            &probe,
            None,
        )
        .await
        .unwrap_err();
    assert!(probe.ran.load(Ordering::SeqCst));
    assert!(probe.lock_was_free.load(Ordering::SeqCst));
    assert_eq!(
        kind(&err),
        ReferenceRefusalKind::ForeignScope {
            scope: "general".into()
        }
    );
    let probe = LockProbe {
        mem: &mem,
        ran: AtomicBool::new(false),
        lock_was_free: AtomicBool::new(false),
    };
    let missing = NodeId::new().0.to_string();
    let err = mem
        .record_action_async_scoped(
            &agent,
            &Action {
                repo_scope: None,
                event_time: None,
                action: "ran it",
                produces: &[],
                modifies: &[],
                depends_on: &[missing.as_str()],
            },
            &[],
            &probe,
            None,
        )
        .await
        .unwrap_err();
    assert!(probe.lock_was_free.load(Ordering::SeqCst));
    assert!(err.to_string().contains("is in scope \"general\""), "{err}");
    mem.close().await.unwrap();
}

/// Lock order smoke test: cited writes (graph write, then index), recalls
/// (graph, index, hot list) and the daemon (graph read, then hot list) run
/// together and finish; the write path never takes the hot list, so the
/// graph-before-hot-list order has no second ordering to fight.
#[tokio::test]
async fn cited_writes_recalls_and_the_daemon_run_together_without_deadlock() {
    let (mem, _) = hybrid("cite-mem-locks").await;
    let mem = Arc::new(mem);
    let agent = AgentId::new("agent-a");
    mem.derive_as(&agent, &[(RULING, ConceptType::Logic)], &ParentOf::none())
        .await
        .unwrap();
    let short = format::short_id(node_of(&mem, RULING));
    let work = async {
        let mut tasks = Vec::new();
        for n in 0..12 {
            let mem = mem.clone();
            let short = short.clone();
            tasks.push(tokio::spawn(async move {
                let agent = AgentId::new("agent-a");
                let content = format!("decision {n} rests on the lease ruling");
                let s = mem
                    .derive_cited_async_as(
                        &agent,
                        &[(content.as_str(), ConceptType::Logic)],
                        &ParentOf::none(),
                        &[cite(&content, &[&short])],
                        None,
                    )
                    .await
                    .unwrap();
                mem.daemon.wake();
                mem.recall(RecallQuery {
                    query: "lease ruling".into(),
                    top_k: 5,
                    max_tokens: 500,
                    traversal_depth: 2,
                })
                .await
                .unwrap();
                s.receipt
            }));
        }
        let mut receipts = Vec::new();
        for t in tasks {
            receipts.push(t.await.unwrap());
        }
        for r in receipts {
            assert!(matches!(settle(&mem, r).await, ReceiptAnswer::Applied(_)));
        }
    };
    tokio::time::timeout(Duration::from_secs(60), work)
        .await
        .expect("no deadlock");
    let ruling = node_of(&mem, RULING);
    assert_eq!(format::blast_radius(&mem.graph.read(), ruling), 12);
    mem.close().await.unwrap();
}

/// A derive with no citations is exactly the call it was: no sentence
/// suffix, no names, no extra edges.
#[tokio::test]
async fn a_derive_without_citations_is_unchanged() {
    let (mem, _) = hybrid("cite-mem-legacy").await;
    let agent = AgentId::new("agent-a");
    let s = mem
        .derive_async_as(
            &agent,
            &[(RULING, ConceptType::Logic)],
            &ParentOf::none(),
            None,
        )
        .await
        .unwrap();
    let ReceiptAnswer::Applied(summary) = settle(&mem, s.receipt).await else {
        panic!("not applied")
    };
    assert_eq!(
        summary.summary,
        "derived 1 concept(s): 1 created (1 embedded), 0 matched existing"
    );
    assert_eq!(
        mem.graph
            .read()
            .edges()
            .filter(|e| e.edge_type == EdgeType::Dependency)
            .count(),
        0
    );
    assert_eq!(mem.graph.read().synonyms().count(), 0);
    mem.close().await.unwrap();
}

/// Review M2: a hybrid similarity merge that makes the citing concept the
/// same as its cited target (cosine at or over the 0.85 threshold, with the
/// fixture embedder's near pair) drops that citation and counts it as a
/// reinforcement; the call applies, with the merge's `Semantic` edge and no
/// `Dependency` from the merged concept to its own twin. Other citations of
/// the same call are still written.
#[tokio::test]
async fn a_similarity_merge_into_the_cited_target_drops_that_citation() {
    use crate::embed::{NEAR_A, NEAR_B};
    let mem = memory_with(
        // Ranked in the holder's graph, so the merge sees unflushed vectors.
        Arc::new(super::writes::VectorSearchStore::graph_ranked(Arc::new(
            MemoryStore::new(),
        ))),
        "cite-mem-selfmerge",
        MatchStrategy::Hybrid,
        Arc::new(super::writes::ContextTolerantEmbedder(
            FixtureEmbedder::new(),
        )),
    )
    .await;
    let agent = AgentId::new("agent-a");
    for content in [NEAR_A, RULING] {
        mem.derive_as(&agent, &[(content, ConceptType::Logic)], &ParentOf::none())
            .await
            .unwrap();
    }
    let target = node_of(&mem, NEAR_A);
    let ruling = node_of(&mem, RULING);
    let out = mem
        .derive_cited_as(
            &agent,
            &[(NEAR_B, ConceptType::Logic)],
            &ParentOf::none(),
            &[cite(
                NEAR_B,
                &[&format::short_id(target), &format::short_id(ruling)],
            )],
        )
        .await
        .expect("the merge drops the self-citation instead of refusing");
    assert_eq!(out.semantic_merged, vec![target], "the merge happened");
    assert_eq!(out.cited, 1, "the other citation is written");
    assert!(
        out.reinforced >= 1,
        "the dropped one counts as a reinforcement"
    );
    {
        let g = mem.graph.read();
        let merged = node_of(&mem, NEAR_B);
        assert!(g
            .edge_between(merged, target, EdgeType::Dependency)
            .is_none());
        assert!(g
            .edge_between(merged, ruling, EdgeType::Dependency)
            .is_some());
        g.assert_invariants().unwrap();
    }
    mem.close().await.unwrap();
}

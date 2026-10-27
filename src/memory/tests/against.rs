//! #19(b) on the `Memory` surface: `against` on a derive and on
//! `record_action`, through the call-path pre-pass, the background commit,
//! durable-intent replay and the foreign-scope probe. The graph-level
//! rules are in `graph::cite::against_tests`.

use super::citations::GatedEmbedder;
use super::*;
use crate::graph::cite::ConceptCitations;
use crate::recall::format;
use crate::surface::resolve::ForeignScopes;
use crate::test_util::VectorSearchable;
use crate::types::{EdgeType, ReferenceField, ReferenceRefusalKind};
use crate::writeq::{ReceiptAnswer, ReceiptId, RECEIPT_WAIT_MAX};
use std::sync::atomic::Ordering;

const BELIEF: &str = "brand y chargers are safe to leave plugged in";
const OTHER_BELIEF: &str = "brand x runs true to size";

fn against(content: &str, refs: &[&str]) -> ConceptCitations {
    ConceptCitations {
        content: content.to_string(),
        depends_on: Vec::new(),
        against: refs.iter().map(|s| s.to_string()).collect(),
    }
}

fn action<'a>(text: &'a str) -> Action<'a> {
    Action {
        repo_scope: None,
        event_time: None,
        action: text,
        produces: &[],
        modifies: &[],
        depends_on: &[],
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

async fn hybrid(session: &str) -> Memory {
    memory_with(
        Arc::new(VectorSearchable(Arc::new(MemoryStore::new()))),
        session,
        MatchStrategy::Hybrid,
        Arc::new(FixtureEmbedder::new()),
    )
    .await
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

fn refusal(err: &LamboError) -> (ReferenceField, ReferenceRefusalKind) {
    match err {
        LamboError::Reference(r) => (r.field, r.kind.clone()),
        other => panic!("expected a reference refusal, got {other:?}"),
    }
}

async fn belief(mem: &Memory, content: &str) -> NodeId {
    mem.derive_as(
        &AgentId::new("agent-a"),
        &[(content, ConceptType::Logic)],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    node_of(mem, content)
}

/// A scope that holds every reference (`general`), standing in for a
/// layered server's probe.
struct General;

impl ForeignScopes for General {
    fn holder_of(&self, _reference: &str) -> Option<String> {
        Some("general".to_string())
    }
}

/// Every refusal is an `Err` before any receipt exists, on a derive and on
/// an action: missing, self, and cross-session (a reference only another
/// scope holds); no interaction is opened, nothing is queued, the graph is
/// untouched.
#[tokio::test]
async fn a_refused_against_is_an_error_before_the_ack_and_writes_nothing() {
    let mem = hybrid("against-mem-prepass").await;
    let agent = AgentId::new("agent-a");
    let b = belief(&mem, BELIEF).await;
    let before = mem.graph.read().snapshot();
    let accepted = mem.pipeline().counters().accepted();
    let short = format::short_id(b);
    for (content, reference, want) in [
        (
            "a newer claim",
            "no such belief",
            ReferenceRefusalKind::Missing,
        ),
        (BELIEF, short.as_str(), ReferenceRefusalKind::SelfReference),
    ] {
        let err = mem
            .derive_cited_async_as(
                &agent,
                &[(content, ConceptType::Logic)],
                &ParentOf::none(),
                &[against(content, &[reference])],
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(refusal(&err), (ReferenceField::Against, want));
    }
    let err = mem
        .derive_cited_async_scoped(
            &agent,
            &[("a newer claim", ConceptType::Logic)],
            &ParentOf::none(),
            &[against("a newer claim", &["a general ruling"])],
            None,
            &General,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        refusal(&err),
        (
            ReferenceField::Against,
            ReferenceRefusalKind::ForeignScope {
                scope: "general".into()
            }
        )
    );
    assert!(
        err.to_string()
            .contains("cross-session contradictions are not recorded"),
        "{err}"
    );
    let act = action("returned the charger");
    for (refs, foreign, want) in [
        (
            vec!["no such belief".to_string()],
            false,
            ReferenceRefusalKind::Missing,
        ),
        (
            vec!["a general ruling".to_string()],
            true,
            ReferenceRefusalKind::ForeignScope {
                scope: "general".into(),
            },
        ),
    ] {
        let err = if foreign {
            mem.record_action_async_scoped(&agent, &act, &refs, &General, None)
                .await
        } else {
            mem.record_action_against_async_as(&agent, &act, &refs)
                .await
        }
        .unwrap_err();
        assert_eq!(refusal(&err), (ReferenceField::ActionAgainst, want));
    }
    assert_eq!(mem.graph.read().snapshot(), before);
    assert_eq!(mem.pipeline().counters().accepted(), accepted);
    mem.close().await.unwrap();
}

/// What the pre-pass accepts, the background applies, under both
/// strategies: a derive contradicting a belief by short id, and an action
/// contradicting another by full id. The receipt says what was recorded.
#[tokio::test]
async fn an_accepted_against_applies_in_the_background_under_both_strategies() {
    for strategy in [MatchStrategy::Canonical, MatchStrategy::Hybrid] {
        let mem = match strategy {
            MatchStrategy::Canonical => canonical("against-mem-apply-c").await,
            MatchStrategy::Hybrid => hybrid("against-mem-apply-h").await,
        };
        let agent = AgentId::new("agent-a");
        let b = belief(&mem, BELIEF).await;
        let other = belief(&mem, OTHER_BELIEF).await;
        let newer = "brand y chargers overheat when left plugged in";
        let submitted = mem
            .derive_cited_async_as(
                &agent,
                &[(newer, ConceptType::Logic)],
                &ParentOf::none(),
                &[against(newer, &[&format::short_id(b)])],
                None,
            )
            .await
            .unwrap();
        let answer = settle(&mem, submitted.receipt).await;
        let ReceiptAnswer::Applied(summary) = &answer else {
            panic!("{strategy:?}: {}", answer.describe())
        };
        assert!(
            summary.summary.ends_with("; 1 contradiction(s) recorded"),
            "{}",
            summary.summary
        );
        let n = node_of(&mem, newer);
        assert!(mem
            .graph
            .read()
            .edge_between(n, b, EdgeType::Contradicts)
            .is_some());

        let concepts = mem.graph.read().concepts().count();
        let acted = mem
            .record_action_against_async_as(
                &agent,
                &action("returned the shirt: it ran a size small"),
                &[other.0.to_string()],
            )
            .await
            .unwrap();
        assert!(matches!(
            settle(&mem, acted.receipt).await,
            ReceiptAnswer::Applied(_)
        ));
        let a = node_of(&mem, "returned the shirt: it ran a size small");
        assert!(mem
            .graph
            .read()
            .edge_between(a, other, EdgeType::Contradicts)
            .is_some());
        // Only the action's own concept is new: a contradiction never
        // creates its target.
        assert_eq!(mem.graph.read().concepts().count(), concepts + 1);
        mem.graph.read().assert_invariants().unwrap();
        mem.close().await.unwrap();
    }
}

/// The synchronous paths (the CLI's) take `against` too, under both
/// strategies, and refuse what the acknowledged ones refuse.
#[tokio::test]
async fn the_synchronous_paths_record_and_refuse_the_same() {
    for strategy in [MatchStrategy::Canonical, MatchStrategy::Hybrid] {
        let mem = match strategy {
            MatchStrategy::Canonical => canonical("against-mem-sync-c").await,
            MatchStrategy::Hybrid => hybrid("against-mem-sync-h").await,
        };
        let agent = AgentId::new("agent-a");
        let b = belief(&mem, BELIEF).await;
        let newer = "a newer claim";
        let out = mem
            .derive_cited_as(
                &agent,
                &[(newer, ConceptType::Logic)],
                &ParentOf::none(),
                &[against(newer, &[BELIEF])],
            )
            .await
            .unwrap();
        assert_eq!(out.contradicted, 1, "{strategy:?}");
        let out = mem
            .record_action_against_embedded_as(
                &agent,
                &action("unplugged every charger"),
                &[format::short_id(b)],
            )
            .await
            .unwrap();
        assert!(mem
            .graph
            .read()
            .edge_between(out.action_node, b, EdgeType::Contradicts)
            .is_some());
        let before = mem.graph.read().snapshot();
        let err = mem
            .record_action_against_embedded_as(
                &agent,
                &action("unplugged every charger again"),
                &["no such belief".to_string()],
            )
            .await
            .unwrap_err();
        assert_eq!(
            refusal(&err),
            (ReferenceField::ActionAgainst, ReferenceRefusalKind::Missing)
        );
        assert_eq!(mem.graph.read().snapshot(), before, "{strategy:?}");
        mem.close().await.unwrap();
    }
}

/// Owner decision, option A: a contradicted concept removed between the
/// ack and the commit of a hybrid derive (while its embed is in flight)
/// fails the receipt with the typed `Changed` refusal, as a citation does;
/// nothing of the derive is written, and no edge dangles. The same for an
/// action, whose apply also waits on the embedder.
#[tokio::test]
async fn a_contradicted_concept_removed_while_the_write_is_in_flight_fails_the_receipt() {
    for on in ["derive", "action"] {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let entered = Arc::new(AtomicUsize::new(0));
        let mem = memory_with(
            Arc::new(VectorSearchable(Arc::new(MemoryStore::new()))),
            "against-mem-race",
            MatchStrategy::Hybrid,
            Arc::new(GatedEmbedder {
                inner: FixtureEmbedder::new(),
                hold: rx,
                entered: entered.clone(),
            }),
        )
        .await;
        let agent = AgentId::new("agent-a");
        let b = belief(&mem, BELIEF).await;
        tx.send(true).unwrap();
        let newer = "a newer claim against the belief";
        let submitted = match on {
            "derive" => {
                mem.derive_cited_async_as(
                    &agent,
                    &[(newer, ConceptType::Logic)],
                    &ParentOf::none(),
                    &[against(newer, &[&format::short_id(b)])],
                    None,
                )
                .await
            }
            _ => {
                mem.record_action_against_async_as(&agent, &action(newer), &[format::short_id(b)])
                    .await
            }
        }
        .expect("the pre-pass accepts: the belief exists now");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while entered.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "{on}: the job never embedded"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        mem.retract(BELIEF, DryRun::No).await.unwrap();
        tx.send(false).unwrap();
        let answer = settle(&mem, submitted.receipt).await;
        let ReceiptAnswer::Failed(why) = &answer else {
            panic!("{on}: {}", answer.describe())
        };
        assert!(
            why.contains("reference refused: against") && why.contains("names nothing now"),
            "{on}: {why}"
        );
        {
            let g = mem.graph.read();
            assert!(g.concepts().all(|c| c.content != newer), "{on}");
            assert!(
                g.edges().all(|e| e.edge_type != EdgeType::Contradicts),
                "{on}"
            );
            g.assert_invariants().unwrap();
        }
        mem.close().await.unwrap();
    }
}

/// Durable intents re-resolve at replay: after a restart, a derive and an
/// action whose contradicted concept is still there apply; ones whose
/// target was removed settle `failed` with the typed refusal.
#[tokio::test]
async fn a_replayed_contradiction_re_resolves_and_settles_applied_or_failed() {
    let inner = Arc::new(MemoryStore::new());
    let agent = AgentId::new("agent-a");
    let mem0 = memory_with(
        Arc::new(VectorSearchable(inner.clone())),
        "against-replay",
        MatchStrategy::Hybrid,
        Arc::new(FixtureEmbedder::new()),
    )
    .await;
    let kept = belief(&mem0, BELIEF).await;
    let removed = belief(&mem0, OTHER_BELIEF).await;
    mem0.close().await.unwrap();

    // Session 1 acks four writes that cannot apply (the embedder hangs),
    // removes one target, and closes: four durable intents.
    let (tx, rx) = tokio::sync::watch::channel(true);
    let mem1 = memory_with(
        Arc::new(VectorSearchable(inner.clone())),
        "against-replay",
        MatchStrategy::Hybrid,
        Arc::new(GatedEmbedder {
            inner: FixtureEmbedder::new(),
            hold: rx,
            entered: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .await;
    let mut receipts = Vec::new();
    for (content, target) in [
        ("contradicts the kept belief", kept),
        ("contradicts the removed belief", removed),
    ] {
        let s = mem1
            .derive_cited_async_as(
                &agent,
                &[(content, ConceptType::Logic)],
                &ParentOf::none(),
                &[against(content, &[&format::short_id(target)])],
                None,
            )
            .await
            .expect("ack");
        receipts.push(s.receipt);
    }
    for (text, target) in [
        ("an action against the kept belief", kept),
        ("an action against the removed belief", removed),
    ] {
        let s = mem1
            .record_action_against_async_as(&agent, &action(text), &[target.0.to_string()])
            .await
            .expect("ack");
        receipts.push(s.receipt);
    }
    mem1.retract(OTHER_BELIEF, DryRun::No).await.unwrap();
    mem1.close().await.unwrap();
    assert_eq!(mem1.pipeline().counters().deferred(), 4);
    drop(tx);

    let mem2 = memory_with(
        Arc::new(VectorSearchable(inner.clone())),
        "against-replay",
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
        [
            "applied_after_restart",
            "failed",
            "applied_after_restart",
            "failed"
        ]
    );
    for failed in [&answers[1], &answers[3]] {
        let shown = failed.describe();
        assert!(shown.contains("reference refused: against"), "{shown}");
    }
    {
        let g = mem2.graph.read();
        for source in [
            "contradicts the kept belief",
            "an action against the kept belief",
        ] {
            let s = g
                .concepts()
                .find(|c| c.content == source)
                .expect("applied")
                .id;
            assert!(g.edge_between(s, kept, EdgeType::Contradicts).is_some());
        }
        assert!(g
            .concepts()
            .all(|c| c.content != "contradicts the removed belief"
                && c.content != "an action against the removed belief"));
        assert_eq!(
            g.edges()
                .filter(|e| e.edge_type == EdgeType::Contradicts)
                .count(),
            2
        );
        g.assert_invariants().unwrap();
    }
    mem2.close().await.unwrap();
}

/// #193: a concept is never merged by similarity into what it is declared
/// `against`. Through 0.7.0 the near twin merged (a `Semantic` edge) and
/// contradicted it too (#36 PR 4 review M2); now the veto refuses the
/// merge, the contradiction is written, and the receipt counts the veto.
#[tokio::test]
async fn a_near_twin_declared_against_is_not_merged_into() {
    use crate::embed::{NEAR_A, NEAR_B};
    let mem = memory_with(
        Arc::new(super::writes::VectorSearchStore::graph_ranked(Arc::new(
            MemoryStore::new(),
        ))),
        "against-mem-merge",
        MatchStrategy::Hybrid,
        Arc::new(super::writes::ContextTolerantEmbedder(
            FixtureEmbedder::new(),
        )),
    )
    .await;
    let agent = AgentId::new("agent-a");
    let target = belief(&mem, NEAR_A).await;
    let out = mem
        .derive_cited_as(
            &agent,
            &[(NEAR_B, ConceptType::Logic)],
            &ParentOf::none(),
            &[against(NEAR_B, &[&format::short_id(target)])],
        )
        .await
        .expect("the veto does not refuse the write");
    assert!(out.semantic_merged.is_empty(), "vetoed: {out:?}");
    assert_eq!(out.merge_guarded.contradicts, 1);
    assert_eq!(out.contradicted, 1);
    {
        let g = mem.graph.read();
        let merged = node_of(&mem, NEAR_B);
        assert_ne!(merged, target);
        assert!(g
            .edge_between(merged, target, EdgeType::Contradicts)
            .is_some());
        assert!(g.edge_between(merged, target, EdgeType::Semantic).is_none());
        assert!(g.edge_between(target, merged, EdgeType::Semantic).is_none());
        g.assert_invariants().unwrap();
    }
    mem.close().await.unwrap();
}

/// #19(c) owner decision 2, option B, applied with #19(b): an `against`
/// write by a second agent on a concept another agent just depended on is
/// no conflict, through the real write path. A second agent's own
/// `Dependency` write on the same concept is (the control).
#[tokio::test]
async fn an_against_write_by_another_agent_raises_no_conflict() {
    use crate::daemon::conflict::{detect, CONFLICT_RECENCY_WINDOW};
    let mem = canonical("against-mem-conflict").await;
    let a = AgentId::new("agent-a");
    let b = AgentId::new("agent-b");
    let target = belief(&mem, BELIEF).await;
    mem.record_action_as(
        &a,
        &Action {
            repo_scope: None,
            event_time: None,
            action: "left the charger plugged in",
            produces: &[],
            modifies: &[],
            depends_on: &[BELIEF],
        },
    )
    .unwrap();
    mem.derive_cited_as(
        &b,
        &[("the charger overheated overnight", ConceptType::Observation)],
        &ParentOf::none(),
        &[against(
            "the charger overheated overnight",
            &[&format::short_id(target)],
        )],
    )
    .await
    .unwrap();
    {
        let g = mem.graph.read();
        assert!(g
            .edges()
            .any(|e| e.edge_type == EdgeType::Contradicts && e.target == target));
        assert!(
            detect(&g, CONFLICT_RECENCY_WINDOW, chrono::Utc::now()).is_empty(),
            "a contradiction is not a concurrent edit"
        );
    }
    mem.record_action_as(
        &b,
        &Action {
            repo_scope: None,
            event_time: None,
            action: "unplugged the charger",
            produces: &[],
            modifies: &[],
            depends_on: &[BELIEF],
        },
    )
    .unwrap();
    let hits = detect(
        &mem.graph.read(),
        CONFLICT_RECENCY_WINDOW,
        chrono::Utc::now(),
    );
    assert!(
        hits.iter().any(|h| h.node == target),
        "the control conflicts: {hits:?}"
    );
    mem.close().await.unwrap();
}

/// The graph persists stale dependencies; durable receipts retain their summary,
/// not the in-process report payload.
#[cfg(feature = "store-sqlite")]
#[tokio::test]
async fn stale_dependencies_survive_restart_but_receipt_payload_does_not() {
    let dir = crate::test_util::ScratchDir::new("stale-restart");
    let path = dir.join("memory.db");
    let open = |path: std::path::PathBuf| async move {
        let store = crate::store::SqliteStore::connect(path.to_str().unwrap()).unwrap();
        store.init_schema().await.unwrap();
        Arc::new(store) as Arc<dyn GraphStore>
    };
    let store = open(path.clone()).await;
    let mem = memory_with(
        store.clone(),
        "stale-restart",
        MatchStrategy::Canonical,
        Arc::new(FixtureEmbedder::new()),
    )
    .await;
    let agent = AgentId::new("agent-a");
    let target = belief(&mem, BELIEF).await;
    mem.record_action_as(
        &agent,
        &Action {
            repo_scope: None,
            event_time: None,
            action: "continue using charger",
            produces: &[],
            modifies: &[],
            depends_on: &[BELIEF],
        },
    )
    .unwrap();
    let newer = "the charger overheated";
    let submitted = mem
        .derive_cited_async_as(
            &agent,
            &[(newer, ConceptType::Logic)],
            &ParentOf::none(),
            &[against(newer, &[&target.0.to_string()])],
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        settle(&mem, submitted.receipt).await,
        ReceiptAnswer::Applied(_)
    ));
    mem.close().await.unwrap();
    drop(mem);
    let reopened = memory_with(
        open(path).await,
        "stale-restart",
        MatchStrategy::Canonical,
        Arc::new(FixtureEmbedder::new()),
    )
    .await;
    let report = crate::daemon::contradiction::stale_report(
        &reopened.graph.read(),
        [target],
        Duration::ZERO,
        chrono::Utc::now(),
    );
    assert_eq!(report.total, 1);
    assert_eq!(report.dependents[0].content, "continue using charger");
    let recovered = reopened.pipeline().lookup(&agent, submitted.receipt);
    assert!(
        matches!(recovered, ReceiptAnswer::AppliedAfterRestart(_)),
        "{recovered:?}"
    );
    reopened.close().await.unwrap();
}

/// Pause report construction at the commit boundary and race a writer for
/// the graph lock. The writer cannot alter the graph before capture.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_receipt_capture_excludes_interleaving_writers() {
    for (strategy, is_action) in [
        (MatchStrategy::Canonical, false),
        (MatchStrategy::Hybrid, false),
        (MatchStrategy::Canonical, true),
        (MatchStrategy::Hybrid, true),
    ] {
        let mem = if strategy == MatchStrategy::Hybrid {
            hybrid("stale-lock-h").await
        } else {
            canonical("stale-lock-c").await
        };
        let agent = AgentId::new("agent-c");
        let target = belief(&mem, BELIEF).await;
        mem.record_action_as(
            &AgentId::new("agent-b"),
            &Action {
                repo_scope: None,
                event_time: None,
                action: "dependent charger advice",
                produces: &[],
                modifies: &[],
                depends_on: &[BELIEF],
            },
        )
        .unwrap();
        let dependent = node_of(&mem, "dependent charger advice");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *crate::writeq::REPORT_GATE.lock().unwrap() = Some((target, entered_tx, release_rx));
        let newer = "charger is unsafe";
        let submitted = if is_action {
            mem.record_action_against_async_as(&agent, &action(newer), &[target.0.to_string()])
                .await
                .unwrap()
        } else {
            mem.derive_cited_async_as(
                &agent,
                &[(newer, ConceptType::Logic)],
                &ParentOf::none(),
                &[against(newer, &[&target.0.to_string()])],
                None,
            )
            .await
            .unwrap()
        };
        tokio::task::spawn_blocking(move || {
            entered_rx.recv_timeout(Duration::from_secs(10)).unwrap()
        })
        .await
        .unwrap();
        assert!(
            mem.graph.try_write().is_none(),
            "{strategy:?}, action={is_action}: report capture must hold the commit lock"
        );
        let graph = mem.graph.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            graph.write().remove_node(dependent).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        let answer = mem
            .pipeline()
            .wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX)
            .await;
        let ReceiptAnswer::Applied(summary) = answer else {
            panic!("{answer:?}")
        };
        assert_eq!(
            summary.stale_report.unwrap().total,
            1,
            "receipt must describe the commit before the dependent was removed"
        );
        mem.close().await.unwrap();
    }
}

//! #125: the lane prefetch. Queued jobs of one lane share embedder rounds,
//! while each job still commits in lane order and settles its own receipt.

use super::*;
use crate::test_util::BatchingEmbedder;
use std::sync::atomic::AtomicBool;

/// A hybrid rig over a gated [`BatchingEmbedder`], calibrated, with the
/// embedder's counters and gate handed back.
struct Batched {
    rig: Rig,
    gate: Arc<tokio::sync::Semaphore>,
    singles: Arc<AtomicUsize>,
    rounds: Arc<AtomicUsize>,
    round_sizes: Arc<PlMutex<Vec<usize>>>,
    fail_rounds: Arc<AtomicBool>,
}

impl Batched {
    async fn new(session: &str) -> Self {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut e = BatchingEmbedder::new(32);
        e.gate = Some(gate.clone());
        let (singles, rounds, round_sizes, fail_rounds) = (
            e.singles.clone(),
            e.rounds.clone(),
            e.round_sizes.clone(),
            e.fail_rounds.clone(),
        );
        let rig = Rig::hybrid_persisting(session, Arc::new(e));
        calibrate_through_gate(&rig, &gate).await;
        Self {
            rig,
            gate,
            singles,
            rounds,
            round_sizes,
            fail_rounds,
        }
    }

    fn counts(&self) -> (usize, usize) {
        (
            self.rounds.load(Ordering::SeqCst),
            self.singles.load(Ordering::SeqCst),
        )
    }

    async fn derive(&self, agent: &AgentId, contents: &[&str]) -> Submitted {
        let interaction = self.rig.interaction(agent);
        self.rig
            .pipeline
            .submit_derive(
                agent.clone(),
                interaction,
                contents
                    .iter()
                    .map(|c| (c.to_string(), ConceptType::Entity))
                    .collect(),
                Vec::new(),
            )
            .await
    }

    async fn settle(&self, agent: &AgentId, s: &Submitted) -> ReceiptAnswer {
        self.rig
            .pipeline
            .wait(agent, s.receipt, RECEIPT_WAIT_MAX)
            .await
    }
}

fn applied(answer: &ReceiptAnswer) -> &AppliedSummary {
    match answer {
        ReceiptAnswer::Applied(s) => s,
        other => panic!("expected applied, got {other:?}"),
    }
}

/// N queued derives cost two rounds (the running job's, then one for the
/// whole backlog), not 2N single embeds; the lane still applies in order.
#[tokio::test]
async fn queued_derives_share_embedder_rounds_and_keep_lane_order() {
    let b = Batched::new("wq-125-rounds").await;
    let a = AgentId::new("agent-a");
    let (rounds0, singles0) = b.counts();

    // The first job's round parks in the embedder, so the rest queue
    // behind it in the lane.
    let first = b.derive(&a, &["lane alpha", "lane beta"]).await;
    until(|| b.counts().0 == rounds0 + 1, "the first job's round").await;
    let mut rest = Vec::new();
    for i in 0..4 {
        // Jobs 1 and 2 derive the SAME content: lane order decides which
        // one creates it and which matches it.
        let shared = format!("lane shared {}", i / 2 + 1);
        let own = format!("lane own {i}");
        rest.push(b.derive(&a, &[shared.as_str(), own.as_str()]).await);
    }
    b.gate.add_permits(64);

    assert_eq!(applied(&b.settle(&a, &first).await).embedded, Some(2));
    let answers: Vec<ReceiptAnswer> = {
        let mut v = Vec::new();
        for s in &rest {
            v.push(b.settle(&a, s).await);
        }
        v
    };
    let (rounds, singles) = b.counts();
    assert_eq!(
        (rounds - rounds0, singles - singles0),
        (2, 0),
        "five two-concept derives: one round for the first, one for the four queued"
    );
    assert_eq!(
        b.round_sizes.lock()[rounds0..].to_vec(),
        vec![2, 6],
        "the backlog round carries each distinct predicted text once"
    );
    // Lane order: job 0 creates "lane shared 1", job 1 matches it.
    assert_eq!(applied(&answers[0]).created_count, 2);
    assert_eq!(
        (
            applied(&answers[1]).created_count,
            applied(&answers[1]).matched_count
        ),
        (1, 1)
    );
    assert_eq!(applied(&answers[2]).created_count, 2);
    assert_eq!(
        (
            applied(&answers[3]).created_count,
            applied(&answers[3]).matched_count
        ),
        (1, 1)
    );
    // Every created concept carries the vector an unbatched write stores.
    let fixture = FixtureEmbedder::new();
    let stored: Vec<(String, Option<Vec<f32>>)> = b
        .rig
        .graph
        .read()
        .concepts()
        .map(|c| (c.content.clone(), c.embedding.clone()))
        .collect();
    assert_eq!(stored.len(), 2 + 6);
    for (content, embedding) in stored {
        let want = fixture
            .embed(&crate::graph::hybrid::context_text(&content, None))
            .await
            .unwrap();
        assert_eq!(embedding, Some(want), "{content}");
    }
}

/// A text the embedder refuses fails only its own write: the round's other
/// slots seed the other jobs, and the refused job, embedding alone, settles
/// `failed` with the permanent class.
#[tokio::test]
async fn a_refused_text_in_a_round_fails_only_its_own_write() {
    let b = Batched::new("wq-125-refused").await;
    let a = AgentId::new("agent-a");
    let first = b.derive(&a, &["refuse lead one", "refuse lead two"]).await;
    let (rounds0, _) = b.counts();
    until(|| b.counts().0 >= rounds0, "the first round").await;
    let bad = format!("refuse {}", BatchingEmbedder::REFUSE);
    let ok_before = b.derive(&a, &["refuse ok one", "refuse ok two"]).await;
    let refused = b.derive(&a, &["refuse neighbour", bad.as_str()]).await;
    let ok_after = b.derive(&a, &["refuse ok three", "refuse ok four"]).await;
    b.gate.add_permits(64);

    applied(&b.settle(&a, &first).await);
    applied(&b.settle(&a, &ok_before).await);
    match b.settle(&a, &refused).await {
        ReceiptAnswer::Failed(why) => assert!(why.contains("embedding error"), "{why}"),
        other => panic!("expected failed, got {other:?}"),
    }
    let after = b.settle(&a, &ok_after).await;
    assert_eq!(applied(&after).embedded, Some(2));
    // Nothing of the refused write landed, its neighbour included.
    let g = b.rig.graph.read();
    assert!(!g
        .concepts()
        .any(|c| c.content.starts_with("refuse neighbour")));
}

/// A round that fails as a whole embeds nothing ahead; each job then
/// embeds its own texts alone and applies.
#[tokio::test]
async fn a_whole_round_outage_leaves_each_write_to_embed_alone() {
    let b = Batched::new("wq-125-outage").await;
    b.fail_rounds.store(true, Ordering::SeqCst);
    let a = AgentId::new("agent-a");
    let (_, singles0) = b.counts();
    let mut subs = Vec::new();
    for i in 0..3 {
        let x = format!("outage x{i}");
        let y = format!("outage y{i}");
        subs.push(b.derive(&a, &[x.as_str(), y.as_str()]).await);
    }
    b.gate.add_permits(64);
    for s in &subs {
        assert_eq!(applied(&b.settle(&a, s).await).embedded, Some(2));
    }
    assert_eq!(b.counts().1 - singles0, 6, "every text embedded alone");
}

/// Two agents' lanes prefetch independently: each lane's writes apply in
/// that lane's order, and neither lane's round carries the other's texts.
#[tokio::test]
async fn each_lane_prefetches_only_its_own_jobs() {
    let b = Batched::new("wq-125-lanes").await;
    let x = AgentId::new("agent-x");
    let y = AgentId::new("agent-y");
    let mut subs = Vec::new();
    for i in 0..3 {
        let cx = format!("lanes x {}", i / 2);
        let cy = format!("lanes y {}", i / 2);
        subs.push((&x, b.derive(&x, &[cx.as_str()]).await));
        subs.push((&y, b.derive(&y, &[cy.as_str()]).await));
    }
    b.gate.add_permits(64);
    let mut answers = Vec::new();
    for (agent, s) in &subs {
        answers.push(b.settle(agent, s).await);
    }
    // Per lane: create, match (same content), create.
    for lane in [0, 1] {
        let mine: Vec<&ReceiptAnswer> = answers.iter().skip(lane).step_by(2).collect();
        assert_eq!(applied(mine[0]).created_count, 1);
        assert_eq!(applied(mine[1]).matched_count, 1);
        assert_eq!(applied(mine[2]).created_count, 1);
    }
    for size in b.round_sizes.lock().iter() {
        assert!(*size <= 3, "a round holds one lane's texts only: {size}");
    }
}

//! J3 write acknowledgements and receipts: acks before the embedder,
//! receipt lookups and piggybacking, wire event time.

use super::*;

// -----------------------------------------------------------------------
// J3 — writes acknowledged before the embedder
// -----------------------------------------------------------------------

/// **Done-when: every write ack carries a receipt.** Both write tools, and
/// the ack must NOT pretend to know what the write did.
#[tokio::test]
async fn every_write_ack_carries_a_receipt_and_claims_nothing_about_the_write() {
    let s = server("mcp-j3-ack").await;
    for (tool, args) in [
        (
            "lambo_derive",
            json!({
                "agent_id": "agent-a",
                "concepts": [{"content": "async ack", "concept_type": "logic"}],
            }),
        ),
        (
            "lambo_record_action",
            json!({"agent_id": "agent-a", "action": "wrote the pipeline"}),
        ),
    ] {
        let ack = call_raw(&s, tool, args).await;
        assert_eq!(ack.is_error, Some(false), "{ack:?}");
        let payload = ack.structured_content.as_ref().expect("payload");
        let id = payload["receipt"].as_str().expect("a receipt id");
        assert!(
            id.parse::<crate::writeq::ReceiptId>().is_ok(),
            "{tool}'s receipt must be parseable: {id}"
        );
        assert_eq!(payload["receipt_state"], json!("pending"), "{tool}");
        // The ack cannot know these, so it must not carry them at all.
        for absent in ["created", "matched", "action_node", "edges"] {
            assert!(
                payload.get(absent).is_none(),
                "{tool}'s ack must not carry {absent} — an empty value is a claim: {payload}"
            );
        }
        // And the receipt id is in the text too, because that is what the
        // model reads.
        let text = match &ack.content[0] {
            rmcp::model::ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        assert!(
            text.contains(id),
            "{tool}'s text must name the receipt: {text}"
        );
    }
    s.mem.close().await.expect("close");
}

/// **Done-when: waiting on a receipt restores read-your-writes for a
/// caller that asks.** Through the shipped surface, and the test the
/// harness comment on `call` points at.
#[tokio::test]
async fn waiting_on_a_receipt_through_lambo_stats_restores_read_your_writes() {
    let s = server("mcp-j3-ryw").await;
    let ack = call_raw(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "read your writes", "concept_type": "logic"}],
        }),
    )
    .await;
    let receipt = ack.structured_content.as_ref().expect("payload")["receipt"]
        .as_str()
        .expect("receipt")
        .to_string();

    let waited = call_raw(
        &s,
        "lambo_stats",
        json!({
            "agent_id": "agent-a",
            "receipt": receipt,
            "wait_ms": crate::writeq::RECEIPT_WAIT_MAX.as_millis() as u64,
        }),
    )
    .await;
    let payload = waited.structured_content.expect("stats payload");
    assert_eq!(payload["receipt"]["state"], json!("applied"), "{payload}");
    assert_eq!(payload["receipt"]["id"], json!(receipt));
    assert_eq!(payload["receipt"]["created_count"], json!(1), "{payload}");

    // The write is now visible to a read that follows the wait — which is
    // the whole claim.
    let seen = call_raw(
        &s,
        "lambo_inspect",
        json!({"agent_id": "agent-a", "focus": "read your writes"}),
    )
    .await;
    assert_eq!(seen.is_error, Some(false), "{seen:?}");
    s.mem.close().await.expect("close");
}

/// Historical evidence supplied on the wire must enter the existing async
/// Memory path unchanged. Replacing either handler's `p.event_time` with
/// `None` makes this red: the interaction and the action edge fall back to
/// their flush stamps instead of the caller's about-time.
#[tokio::test]
async fn wire_event_time_stamps_derive_and_record_action_interactions_and_edges() {
    let s = server("mcp-event-time-write").await;
    let derive_time: DateTime<Utc> = "2018-04-05T06:07:08Z".parse().expect("RFC3339");
    let action_time: DateTime<Utc> = "2019-05-06T07:08:09Z".parse().expect("RFC3339");

    let derived = call(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "historical wire derive", "concept_type": "entity"}],
            "event_time": derive_time.to_rfc3339(),
        }),
    )
    .await;
    assert_eq!(derived.is_error, Some(false), "{derived:?}");

    let actioned = call(
        &s,
        "lambo_record_action",
        json!({
            "agent_id": "agent-a",
            "action": "historical wire action",
            "produces": ["historical wire artifact"],
            "event_time": action_time.to_rfc3339(),
        }),
    )
    .await;
    assert_eq!(actioned.is_error, Some(false), "{actioned:?}");

    {
        let g = s.mem.graph().read();
        let derived_interaction = g
            .interactions()
            .find(|i| i.prompt_text.as_deref() == Some("historical wire derive"))
            .expect("derive interaction");
        assert_eq!(derived_interaction.event_time, Some(derive_time));
        assert_eq!(derived_interaction.about_time(), derive_time);

        let action_interaction = g
            .interactions()
            .find(|i| i.prompt_text.as_deref() == Some("historical wire action"))
            .expect("action interaction");
        assert_eq!(action_interaction.event_time, Some(action_time));
        assert_eq!(action_interaction.about_time(), action_time);
        assert!(
            g.edges().any(|edge| {
                edge.edge_type == crate::types::EdgeType::Causal
                    && edge.event_time == Some(action_time)
                    && edge.about_time() == action_time
            }),
            "record_action's structural edge must inherit its interaction event time"
        );
    }

    // Omission is byte-for-byte the old live-fact behaviour: no stored
    // event time and `about_time()` falls back to server-stamped created_at.
    let live = call(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "live wire derive", "concept_type": "entity"}],
        }),
    )
    .await;
    assert_eq!(live.is_error, Some(false), "{live:?}");
    {
        let g = s.mem.graph().read();
        let live_interaction = g
            .interactions()
            .find(|i| i.prompt_text.as_deref() == Some("live wire derive"))
            .expect("live derive interaction");
        assert_eq!(live_interaction.event_time, None);
        assert_eq!(live_interaction.about_time(), live_interaction.created_at);
    }
    s.mem.close().await.expect("close");
}

/// A bulk historical corpus supplied over MCP must retain its separated
/// sessions. If `derive_impl` regresses to `event_time: None`, all three
/// submissions have near-identical flush stamps and this becomes one
/// session, so Solo can no longer admit the derived concept.
#[tokio::test]
async fn wire_derived_history_recurs_under_solo_event_time() {
    use crate::canon::{separated_session_count, EvalParams, PromotionPolicy};
    use crate::daemon::ScoreTable;

    let s = server("mcp-event-time-solo").await;
    let content = "wire-derived recurring historical fact";
    for event_time in [
        "2018-01-01T00:00:00Z",
        "2018-01-03T00:00:00Z",
        "2018-01-05T00:00:00Z",
    ] {
        let out = call(
            &s,
            "lambo_derive",
            json!({
                "agent_id": "agent-a",
                "concepts": [{"content": content, "concept_type": "entity"}],
                "event_time": event_time,
            }),
        )
        .await;
        assert_eq!(out.is_error, Some(false), "{out:?}");
    }

    {
        let g = s.mem.graph().read();
        let concept = g
            .concepts()
            .find(|concept| concept.content == content)
            .expect("the wire-derived concept exists");
        let support_times: Vec<_> = g
            .edges()
            .filter(|edge| {
                edge.edge_type == crate::types::EdgeType::Derives && edge.target == concept.id
            })
            .filter_map(|edge| g.node(edge.source))
            .filter_map(|node| match node {
                crate::types::Node::Interaction(interaction) => Some(interaction.about_time()),
                crate::types::Node::Concept(_) => None,
            })
            .collect();
        assert_eq!(
            separated_session_count(&support_times, Duration::from_secs(24 * 60 * 60)),
            3,
            "three two-day-separated wire event times are three Solo sessions"
        );
        assert!(
            PromotionPolicy::Solo
                .scorer()
                .candidates(
                    &g,
                    &ScoreTable::default(),
                    &EvalParams::default(),
                    "2020-01-01T00:00:00Z"
                        .parse()
                        .expect("injected evaluation time"),
                )
                .contains(&concept.id),
            "three event-timed entity sessions clear Solo's Candidate bar"
        );
    }
    s.mem.close().await.expect("close");
}

/// The process-selected policy must reach the live canonization task, not
/// merely the scorer unit tests. The same seven historical Constraint
/// derives are seven >=24h-separated sessions: Solo's 1.5 eviction
/// resistance makes the score 10.5, past `CANONICAL_BAR`, so it climbs
/// None → Candidate → Venerable → Canonical at one hop per cycle.
///
/// Swarm refuses the identical corpus, and the operative reason is the
/// **peer-count floor**, not an absent P90 convergence: `stage1_candidates`
/// returns empty as soon as `peers.len() < canonization_min_peer_count`
/// (default 20) and never reaches the score distribution at all. Two other
/// swarm gates would each independently give the same answer here —
/// `gc_survived >= 3` cannot be met with `gc_interval` at its 10 000
/// default, and one concept has no peer distribution to cut at P90 — so the
/// arm is over-determined. That is fine for a negative control, but the
/// reason it fires must be stated correctly: this arm exercises the floor.
#[tokio::test]
async fn a_single_writer_historical_constraint_canonizes_only_under_solo() {
    use crate::types::CanonizationStatus;

    /// Cycles the arm is given **after the corpus is complete**, counted
    /// from a baseline rather than from process start.
    ///
    /// Solo needs exactly three: one hop per cycle is structural
    /// (`canon::eval`). The budget is not three, and it is not absolute.
    /// An absolute `canonization_cycles >= 4` left one cycle of slack for
    /// the whole run, and the 10 ms eval interval means cycles fire *while*
    /// the seven derives are still being submitted — a cycle that sees one
    /// recurrence (1.5, under `CANDIDATE_BAR`) or six (9.0, under
    /// `CANONICAL_BAR`) is a real cycle that promotes nothing and spends
    /// budget. Baselining makes those unspendable, so the pass condition is
    /// "three hops happened" instead of "three hops happened inside four
    /// ticks"; the headroom on top costs ~90 ms.
    const CYCLE_BUDGET: u64 = 12;

    async fn run(policy: PromotionPolicy, session: &str) -> CanonizationStatus {
        let config = Config {
            promotion_policy: policy,
            canonization_eval_interval: Duration::from_millis(10),
            ..Config::default()
        };
        config.validate().expect("selected policy validates");
        let s = server_with_config(session, config).await;
        let content = "single-writer recurring constraint";
        for event_time in [
            "2018-01-01T00:00:00Z",
            "2018-01-03T00:00:00Z",
            "2018-01-05T00:00:00Z",
            "2018-01-07T00:00:00Z",
            "2018-01-09T00:00:00Z",
            "2018-01-11T00:00:00Z",
            "2018-01-13T00:00:00Z",
        ] {
            // `call` (not `call_raw`) awaits the ack's receipt to a settled
            // state, which is load-bearing here and not incidental: J3
            // makes `lambo_derive` return once the write is *ordered*, so
            // `is_error == Some(false)` alone would mean queued, not
            // applied, and the loop below could open on a graph with no
            // such concept in it.
            let out = call(
                &s,
                "lambo_derive",
                json!({
                    "agent_id": "agent-a",
                    "concepts": [{"content": content, "concept_type": "constraint"}],
                    "event_time": event_time,
                }),
            )
            .await;
            assert_eq!(out.is_error, Some(false), "derive failed: {out:?}");
        }

        // Every one of the seven is applied by now, so from here on every
        // cycle scores the complete corpus. Anything the eval spent while
        // the corpus was partial is behind this line.
        let baseline = s.mem.stats().canonization_cycles;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = s
                .mem
                .graph()
                .read()
                .concepts()
                .find(|concept| concept.content == content)
                .expect("the applied derives left a constraint in the graph")
                .canonization_status;
            let spent = s.mem.stats().canonization_cycles.saturating_sub(baseline);
            if status == CanonizationStatus::Canonical
                || spent >= CYCLE_BUDGET
                || tokio::time::Instant::now() >= deadline
            {
                // The Swarm arm is a LIVE negative control: it must have
                // burned its whole budget of real cycles on the finished
                // corpus and still promoted nothing. Without this the arm
                // could pass by never having evaluated anything.
                assert!(
                    status == CanonizationStatus::Canonical || spent >= CYCLE_BUDGET,
                    "{policy:?} neither promoted nor got its {CYCLE_BUDGET} cycles \
                         (spent {spent}) — the arm timed out instead of deciding"
                );
                s.mem.close().await.expect("close");
                return status;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    assert_eq!(
        run(PromotionPolicy::Solo, "mcp-solo-promotes").await,
        CanonizationStatus::Canonical,
        "seven historical recurrences must clear Solo's Canonical band"
    );
    assert_eq!(
        run(PromotionPolicy::Swarm, "mcp-swarm-does-not-promote").await,
        CanonizationStatus::None,
        "the same lone-writer corpus must not silently change Swarm"
    );
}

/// A receipt with **no** `wait_ms` is a fetch, not a wait: it answers with
/// whatever the state is, which for a fresh ack is `pending`. The
/// asynchrony is real and this is what proves it — without this, every
/// other J3 test could be passing over a secretly synchronous path.
#[tokio::test]
async fn a_fresh_receipt_fetched_without_waiting_answers_pending() {
    let s = server("mcp-j3-pending").await;
    // A held embedder would make this deterministic; the fixture embedder
    // is fast, so accept either answer and assert only that BOTH are
    // reachable states of a real queue — never an error, never "unknown".
    let ack = call_raw(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "not yet applied", "concept_type": "logic"}],
        }),
    )
    .await;
    let receipt = ack.structured_content.as_ref().expect("payload")["receipt"]
        .as_str()
        .expect("receipt")
        .to_string();
    let fetched = call_raw(
        &s,
        "lambo_stats",
        json!({"agent_id": "agent-a", "receipt": receipt}),
    )
    .await;
    let state = fetched.structured_content.expect("payload")["receipt"]["state"]
        .as_str()
        .expect("a state")
        .to_string();
    assert!(
        state == "pending" || state == "applied",
        "a fetch must answer the real state, got {state}"
    );
    s.mem.close().await.expect("close");
}

/// **Done-when: outcomes are retrievable by receipt, and expired /
/// restart-lost answer distinctly, never "unknown".** Through the shipped
/// surface this time — the taxonomy itself is pinned in `writeq`.
#[tokio::test]
async fn a_foreign_receipt_is_named_restart_lost_not_unknown() {
    let s = server("mcp-j3-foreign").await;
    // A well-formed id from a different process epoch.
    let foreign = "lwr1.dead0000beef0000.1a00000000.1";
    let out = call_raw(
        &s,
        "lambo_stats",
        json!({"agent_id": "agent-a", "receipt": foreign}),
    )
    .await;
    let payload = out.structured_content.expect("payload");
    assert_eq!(
        payload["receipt"]["state"],
        json!("restart_lost"),
        "{payload}"
    );
    let detail = payload["receipt"]["detail"].as_str().expect("detail");
    assert!(detail.contains("UNKNOWN"), "{detail}");
    assert!(detail.contains("Recall before re-deriving"), "{detail}");
    assert!(
        !detail.contains("\"unknown\""),
        "the STATE must never be the word unknown: {detail}"
    );

    // A malformed id is a parameter error, not a receipt state — a client
    // typo must not read as a lost write.
    let bad = call_raw(
        &s,
        "lambo_stats",
        json!({"agent_id": "agent-a", "receipt": "not-a-receipt"}),
    )
    .await;
    assert_eq!(bad.is_error, Some(true), "{bad:?}");
    s.mem.close().await.expect("close");
}

/// Receipts are per-agent scoped (J1 is why): one agent must not be able
/// to read another's write outcome, which can carry concept ids and error
/// text.
#[tokio::test]
async fn another_agents_receipt_is_refused_through_lambo_stats() {
    let s = server("mcp-j3-scope").await;
    let ack = call_raw(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "a's private write", "concept_type": "logic"}],
        }),
    )
    .await;
    let receipt = ack.structured_content.as_ref().expect("payload")["receipt"]
        .as_str()
        .expect("receipt")
        .to_string();
    let peek = call_raw(
        &s,
        "lambo_stats",
        json!({"agent_id": "agent-b", "receipt": receipt.clone()}),
    )
    .await;
    let payload = peek.structured_content.expect("payload");
    assert_eq!(payload["receipt"]["state"], json!("forbidden"), "{payload}");
    assert!(
        payload["receipt"].get("created").is_none(),
        "a refused lookup must leak no outcome: {payload}"
    );
    // The owner still gets it.
    let own = call_raw(
        &s,
        "lambo_stats",
        json!({
            "agent_id": "agent-a",
            "receipt": receipt,
            "wait_ms": crate::writeq::RECEIPT_WAIT_MAX.as_millis() as u64,
        }),
    )
    .await;
    assert_eq!(
        own.structured_content.expect("payload")["receipt"]["state"],
        json!("applied")
    );
    s.mem.close().await.expect("close");
}

/// **Shape part 3: piggybacked on that agent's next tool response, tagged
/// and self-identifying.** And scoped — the other agent's response must not
/// carry it.
#[tokio::test]
async fn a_settled_receipt_is_piggybacked_on_that_agents_next_response() {
    let s = server("mcp-j3-piggyback").await;
    let ack = call_raw(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "piggyback me", "concept_type": "logic"}],
        }),
    )
    .await;
    let receipt = ack.structured_content.as_ref().expect("payload")["receipt"]
        .as_str()
        .expect("receipt")
        .to_string();
    // Let it settle without consuming the piggyback.
    let id: crate::writeq::ReceiptId = receipt.parse().expect("id");
    s.mem
        .pipeline()
        .wait(
            &AgentId::new("agent-a"),
            id,
            crate::writeq::RECEIPT_WAIT_MAX,
        )
        .await;

    // A DIFFERENT agent's next call must not carry it.
    let other = call_raw(&s, "lambo_saints", json!({"agent_id": "agent-b"})).await;
    let other_text = other
        .content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !other_text.contains(&receipt),
        "agent-b must not be handed agent-a's receipt: {other_text}"
    );

    // agent-a's next call must.
    let mine = call_raw(&s, "lambo_saints", json!({"agent_id": "agent-a"})).await;
    let text = mine
        .content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("write receipts"),
        "untagged piggyback: {text}"
    );
    assert!(text.contains(&receipt), "{text}");
    assert!(text.contains("applied"), "{text}");

    // Take-once: the call after that must not repeat it.
    let again = call_raw(&s, "lambo_saints", json!({"agent_id": "agent-a"})).await;
    let again_text = again
        .content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !again_text.contains(&receipt),
        "a delivered receipt must not be re-announced forever: {again_text}"
    );
    s.mem.close().await.expect("close");
}

/// **Done-when: one agent's writes apply in submission order, pinning the
/// `Temporal` chain — with two agents interleaving through one process.**
///
/// Since J1 the chain is SESSION-wide, so the per-agent claim is read by
/// filtering the chain on `agent_id`. That is not a workaround: the chain
/// records arrival order across a shared session, and one agent's slice of
/// it is that agent's submission order **for calls it sends one after
/// another** — which is what this test sends, and the only scope the
/// property holds in (J3-R1-10; two calls one agent has in flight
/// simultaneously can be chained in one order and drained in the other).
#[tokio::test]
async fn interleaved_agents_each_keep_their_own_order_on_the_temporal_chain() {
    let s = server("mcp-j3-chain").await;
    let mut expected_a = Vec::new();
    let mut expected_b = Vec::new();
    for i in 0..4 {
        for (agent, expected) in [("agent-a", &mut expected_a), ("agent-b", &mut expected_b)] {
            let content = format!("{agent}-step-{i}");
            expected.push(content.clone());
            call(
                &s,
                "lambo_derive",
                json!({
                    "agent_id": agent,
                    "concepts": [{"content": content, "concept_type": "logic"}],
                }),
            )
            .await;
        }
    }

    let (chain_a, chain_b) = {
        let g = s.mem.graph().read();
        let prompts_for = |want: &str| -> Vec<String> {
            g.temporal_chain()
                .iter()
                .filter_map(|id| match g.node(*id) {
                    Some(crate::types::Node::Interaction(i)) if i.agent_id.0 == want => {
                        i.prompt_text.clone()
                    }
                    _ => None,
                })
                .collect()
        };
        (prompts_for("agent-a"), prompts_for("agent-b"))
    };
    assert_eq!(chain_a, expected_a, "agent-a's slice of the chain");
    assert_eq!(chain_b, expected_b, "agent-b's slice of the chain");

    // The session-wide chain interleaves, which is what a shared session
    // means — and is the thing the per-agent filter exists to see past.
    let interleaved = {
        let g = s.mem.graph().read();
        g.temporal_chain()
            .iter()
            .filter_map(|id| match g.node(*id) {
                Some(crate::types::Node::Interaction(i)) => Some(i.agent_id.0.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert!(
        interleaved.windows(2).any(|w| w[0] != w[1]),
        "the chain must actually interleave, or this test proves nothing: {interleaved:?}"
    );
    s.mem.close().await.expect("close");
}

/// **Done-when: `lambo_derive` returns after validation without waiting on
/// the embedder.** Pinned by construction rather than by a stopwatch: a
/// timing assertion would be flaky, so this asserts the *property* — the
/// ack lands with the embedder untouched.
#[tokio::test]
async fn the_ack_lands_before_the_embedder_is_called() {
    use std::sync::atomic::{AtomicUsize, Ordering as O};

    struct CountingEmbedder {
        inner: FixtureEmbedder,
        calls: Arc<AtomicUsize>,
        gate: Arc<tokio::sync::Semaphore>,
    }
    #[async_trait::async_trait]
    impl Embedder for CountingEmbedder {
        fn dimensions(&self) -> usize {
            self.inner.dimensions()
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::EmbedError> {
            self.embed_as(text, crate::test_util::TextRole::Document)
                .await
        }
        async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::EmbedError> {
            self.embed_as(text, crate::test_util::TextRole::Query).await
        }
        fn modalities(&self) -> crate::embed::Modalities {
            self.inner.modalities()
        }
        async fn embed_image(
            &self,
            image: crate::embed::ImageInput<'_>,
        ) -> Result<Vec<f32>, crate::EmbedError> {
            self.inner.embed_image(image).await
        }
    }

    impl CountingEmbedder {
        /// The `embed` behaviour above, in either text role (#22: a wrapper
        /// forwards the role, so its inner embedder sees what the caller asked).
        async fn embed_as(
            &self,
            text: &str,
            role: crate::test_util::TextRole,
        ) -> Result<Vec<f32>, crate::EmbedError> {
            self.calls.fetch_add(1, O::Relaxed);
            let p = self.gate.acquire().await.expect("gate");
            p.forget();
            role.embed(&self.inner, text).await
        }
    }

    // A store that advertises VECTOR_SEARCH, because hybrid `derive` skips
    // the embedder entirely when the store has none — and a test that
    // proves "the ack does not wait for the embedder" against a store that
    // never embeds proves nothing at all. That is exactly how this test
    // first passed-by-accident, so the wrapper is load-bearing.
    struct VectorCapable(MemoryStore);
    #[async_trait::async_trait]
    impl GraphStore for VectorCapable {
        fn capabilities(&self) -> crate::store::Capabilities {
            crate::store::Capabilities::VECTOR_SEARCH
        }
        async fn init_schema(&self) -> Result<(), crate::types::StoreError> {
            self.0.init_schema().await
        }
        fn vector_dimensions(&self) -> Option<usize> {
            self.0.vector_dimensions()
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
            session: &crate::types::SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<crate::types::Scored<NodeId>>, crate::types::StoreError> {
            self.0.vector_candidates(session, embedding, limit).await
        }
        async fn blast_radius(
            &self,
            session: &crate::types::SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: chrono::DateTime<chrono::Utc>,
        ) -> Result<u64, crate::types::StoreError> {
            self.0.blast_radius(session, node, min_edge_age, now).await
        }
        async fn interaction_span(
            &self,
            session: &crate::types::SessionId,
            node: NodeId,
            min_age: Duration,
            now: chrono::DateTime<chrono::Utc>,
        ) -> Result<crate::types::InteractionSpan, crate::types::StoreError> {
            self.0.interaction_span(session, node, min_age, now).await
        }
        async fn record_canonization(
            &self,
            event: &crate::types::CanonizationEvent,
            token: Option<u64>,
        ) -> Result<(), crate::types::StoreError> {
            self.0.record_canonization(event, token).await
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(
        crate::writeq::PROBE_CONCURRENCY,
    ));
    let store: Arc<dyn GraphStore> = Arc::new(VectorCapable(MemoryStore::new()));
    let mem = Memory::builder()
        .session("mcp-j3-latency")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .match_strategy(crate::MatchStrategy::Hybrid)
        .store(store)
        .embedder(Arc::new(CountingEmbedder {
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
            gate: gate.clone(),
        }) as Arc<dyn Embedder>)
        .embedding_contract(EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        })
        .build()
        .await
        .expect("build");
    let s = LamboServer::new(Arc::new(mem));

    // Drain the probe's permits so the NEXT embed parks.
    let ack = call_raw(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "acked before the embedder", "concept_type": "logic"}],
        }),
    )
    .await;
    assert_eq!(ack.is_error, Some(false), "{ack:?}");
    assert_eq!(
        ack.structured_content.as_ref().expect("payload")["receipt_state"],
        json!("pending"),
        "the ack must return with the write still outstanding"
    );
    // The write is parked in the embedder — the ack did not wait for it.
    let receipt: crate::writeq::ReceiptId = ack.structured_content.as_ref().expect("payload")
        ["receipt"]
        .as_str()
        .expect("receipt")
        .parse()
        .expect("id");
    assert_eq!(
        s.mem
            .pipeline()
            .lookup(&AgentId::new("agent-a"), receipt)
            .tag(),
        "pending",
        "an ack that had waited for the embedder would already be settled"
    );
    // Release it and confirm it lands.
    gate.add_permits(8);
    let answer = s
        .mem
        .pipeline()
        .wait(
            &AgentId::new("agent-a"),
            receipt,
            crate::writeq::RECEIPT_WAIT_MAX,
        )
        .await;
    assert_eq!(answer.tag(), "applied", "{answer:?}");
    let total = calls.load(O::Relaxed);
    assert!(
        total > crate::writeq::PROBE_CONCURRENCY,
        "the background write must have embedded after the ack (calls={total}, probe={})",
        crate::writeq::PROBE_CONCURRENCY
    );
    s.mem.close().await.expect("close");
}

/// **Done-when (close-drain durability).** `close()` quiesces the queue
/// before it drains the log, so a write acked just before shutdown is
/// durable rather than lost.
#[tokio::test]
async fn close_makes_a_write_acked_just_before_it_durable() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = Memory::builder()
        .session("mcp-j3-close")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(store.clone())
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        })
        .build()
        .await
        .expect("build");
    let s = LamboServer::new(Arc::new(mem));
    // No wait, no piggyback: ack and immediately close.
    let ack = call_raw(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "durable across close", "concept_type": "logic"}],
        }),
    )
    .await;
    assert_eq!(ack.is_error, Some(false), "{ack:?}");
    s.mem.close().await.expect("close");

    let snap = store
        .load_session(&crate::types::SessionId::new("mcp-j3-close"))
        .await
        .expect("load");
    assert!(
        snap.concepts
            .iter()
            .any(|c| c.content == "durable across close"),
        "close() must drain the write queue before it drains the log — otherwise an ack \
             just before shutdown is a lie"
    );
}

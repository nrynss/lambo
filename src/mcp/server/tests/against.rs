//! #19(b) over MCP: `against` on a `lambo_derive` concept and on
//! `lambo_record_action`. The cross-session refusal is in `layered`.

use super::*;
use crate::recall::format;
use crate::types::EdgeType;

const BELIEF: &str = "brand y chargers are safe to leave plugged in";

#[tokio::test]
async fn stale_report_is_delivered_on_receipt_and_session_read() {
    let s = server("mcp-stale-report").await;
    let belief = belief(&s).await;
    let dependent = derive_created(
        &s,
        "agent-b",
        json!([{
            "content": "keep using this charger", "concept_type": "logic",
            "depends_on": [belief.0.to_string()]
        }]),
    )
    .await
    .remove(0);
    for (tool, args) in [
        (
            "lambo_derive",
            json!({"agent_id":"agent-c", "concepts":[{"content":"charger is unsafe", "concept_type":"logic", "against":[belief.0.to_string()]}]}),
        ),
        (
            "lambo_record_action",
            json!({"agent_id":"agent-d", "action":"recall the charger", "against":[belief.0.to_string()]}),
        ),
    ] {
        let ack = call_raw(&s, tool, args.clone()).await;
        let receipt = ack.structured_content.unwrap()["receipt"]
            .as_str()
            .unwrap()
            .to_owned();
        let out = call_raw(
            &s,
            "lambo_stats",
            json!({"agent_id":args["agent_id"], "receipt":receipt, "wait_ms":34000}),
        )
        .await;
        let report = &out.structured_content.as_ref().unwrap()["receipt"]["stale_report"];
        assert_eq!(report["total"], 1, "{report}");
        assert_eq!(report["dependents"][0]["id"], dependent);
        assert_eq!(report["dependents"][0]["cited_id"], belief.0.to_string());
    }
    let read = call_raw(&s, "lambo_stale", json!({"agent_id":"agent-b"})).await;
    let report = &read.structured_content.as_ref().unwrap()["stale_report"];
    assert_eq!(report["total"], 1);
    assert_eq!(report["dependents"][0]["id"], dependent);
    s.mem.close().await.expect("close");
}

async fn belief(s: &LamboServer) -> NodeId {
    let ids = derive_created(
        s,
        "agent-a",
        json!([{"content": BELIEF, "concept_type": "logic"}]),
    )
    .await;
    NodeId(ids[0].parse().expect("uuid"))
}

fn node(s: &LamboServer, content: &str) -> NodeId {
    s.mem
        .graph()
        .read()
        .concepts()
        .find(|c| c.content == content)
        .unwrap_or_else(|| panic!("no concept {content:?}"))
        .id
}

/// A derive's `against` and an action's land as `Contradicts` edges, newer
/// to older; the target gains no dependent (inspect's list is unchanged)
/// and nothing is created for a reference.
#[tokio::test]
async fn a_contradicting_derive_and_action_land_on_their_targets() {
    let s = server("mcp-against").await;
    let belief = belief(&s).await;
    let newer = "brand y chargers overheat when left plugged in";
    let out = call(
        &s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": [{
            "content": newer,
            "concept_type": "observation",
            "against": [format::short_id(belief)],
        }]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    let concepts = s.mem.graph().read().concepts().count();
    let out = call(
        &s,
        "lambo_record_action",
        json!({"agent_id": "agent-a", "action": "refunded: the charger overheated",
               "against": [belief.0.to_string()]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    {
        let g = s.mem.graph().read();
        let n = g.concepts().find(|c| c.content == newer).unwrap().id;
        let a = g
            .concepts()
            .find(|c| c.content == "refunded: the charger overheated")
            .unwrap()
            .id;
        assert!(g.edge_between(n, belief, EdgeType::Contradicts).is_some());
        assert!(g.edge_between(a, belief, EdgeType::Contradicts).is_some());
        assert!(g
            .edges()
            .all(|e| e.target != belief || e.edge_type != EdgeType::Dependency));
        assert_eq!(g.concepts().count(), concepts + 1, "only the action");
        // Opposition is not dependence: no blast radius.
        assert_eq!(format::blast_radius(&g, belief), 0);
    }
    s.mem.close().await.expect("close");
}

/// Every refusal arrives before the ack, as an error naming `against`,
/// and writes nothing: missing, ambiguous by content shared with a short
/// id, an interaction's id, the concept itself, and on an action a
/// missing target (never created, unlike `depends_on` text).
#[tokio::test]
async fn a_bad_against_is_refused_before_the_ack_with_its_reason() {
    let s = server("mcp-against-refuse").await;
    let belief = belief(&s).await;
    let before = s.mem.graph().read().snapshot();
    let interaction = s
        .mem
        .graph()
        .read()
        .interactions()
        .next()
        .expect("an interaction")
        .id;
    for (content, reference, says) in [
        (
            "a newer claim",
            "chargers are fine".to_string(),
            "names no concept in this session",
        ),
        (
            "a newer claim",
            interaction.0.to_string(),
            "is the id of an interaction",
        ),
        (
            BELIEF,
            format::short_id(belief),
            "names the contradicting concept itself",
        ),
    ] {
        let out = call_raw(
            &s,
            "lambo_derive",
            json!({"agent_id": "agent-a", "concepts": [{
                "content": content, "concept_type": "logic", "against": [reference],
            }]}),
        )
        .await;
        assert_eq!(out.is_error, Some(true));
        let text = text_of(&out);
        assert!(
            text.starts_with("lambo_derive: reference refused: against \""),
            "{text}"
        );
        assert!(text.contains(says), "{text}");
        assert!(text.ends_with("nothing was written"), "{text}");
    }
    let out = call_raw(
        &s,
        "lambo_record_action",
        json!({"agent_id": "agent-a", "action": "ran it", "against": ["chargers are fine"]}),
    )
    .await;
    assert_eq!(out.is_error, Some(true));
    assert!(
        text_of(&out).starts_with("lambo_record_action: reference refused: against \"chargers are fine\" on \"ran it\" names no concept"),
        "{}",
        text_of(&out)
    );
    assert_eq!(s.mem.graph().read().snapshot(), before);
    s.mem.close().await.expect("close");
}

/// The surface limits: 8 per concept (or action), 32 across a derive,
/// counted apart from `depends_on`; empty entries are bad parameters.
#[tokio::test]
async fn the_against_limits_are_bad_params() {
    let s = server("mcp-against-limits").await;
    let nine: Vec<String> = (0..9).map(|n| format!("r{n}")).collect();
    for (tool, args, says) in [
        (
            "lambo_derive",
            json!({"agent_id": "agent-a", "concepts": [
                {"content": "a", "concept_type": "logic", "against": nine}]}),
            "a concept's against may list at most 8",
        ),
        (
            "lambo_derive",
            json!({"agent_id": "agent-a", "concepts": [
                {"content": "a", "concept_type": "logic", "against": [""]}]}),
            "concept.against entry",
        ),
        (
            "lambo_record_action",
            json!({"agent_id": "agent-a", "action": "ran it", "against": nine}),
            "an action's against may list at most 8",
        ),
        (
            "lambo_record_action",
            json!({"agent_id": "agent-a", "action": "ran it", "against": [" "]}),
            "against entry",
        ),
    ] {
        let out = call_raw(&s, tool, args).await;
        assert_eq!(out.is_error, Some(true));
        assert!(text_of(&out).contains(says), "{}", text_of(&out));
    }
    let five: Vec<serde_json::Value> = (0..5)
        .map(|n| {
            json!({"content": format!("c{n}"), "concept_type": "logic",
                   "depends_on": ["a","b","c","d","e","f","g"],
                   "against": ["a","b","c","d","e","f","g"]})
        })
        .collect();
    let out = call_raw(
        &s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": five}),
    )
    .await;
    // 35 citations is over first; then 35 contradictions on their own.
    assert!(
        text_of(&out).contains("depends_on may list at most 32"),
        "{}",
        text_of(&out)
    );
    let five: Vec<serde_json::Value> = (0..5)
        .map(|n| {
            json!({"content": format!("c{n}"), "concept_type": "logic",
                   "against": ["a","b","c","d","e","f","g"]})
        })
        .collect();
    let out = call_raw(
        &s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": five}),
    )
    .await;
    assert!(
        text_of(&out).contains("against may list at most 32"),
        "{}",
        text_of(&out)
    );
    s.mem.close().await.expect("close");
}

/// Two concepts may contradict each other over the wire (no acyclicity),
/// and re-sending a contradiction reinforces its one edge.
#[tokio::test]
async fn mutual_and_resent_contradictions_over_the_wire() {
    let s = server("mcp-against-mutual").await;
    let belief = belief(&s).await;
    let newer = "brand y chargers overheat when left plugged in";
    for _ in 0..2 {
        let out = call(
            &s,
            "lambo_derive",
            json!({"agent_id": "agent-a", "concepts": [{
                "content": newer, "concept_type": "logic",
                "against": [format::short_id(belief)],
            }]}),
        )
        .await;
        assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    }
    let n = node(&s, newer);
    let out = call(
        &s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": [{
            "content": BELIEF, "concept_type": "logic",
            "against": [format::short_id(n)],
        }]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    {
        let g = s.mem.graph().read();
        assert_eq!(
            g.edge_between(n, belief, EdgeType::Contradicts)
                .expect("newer -> older")
                .reinforcements,
            2
        );
        assert!(g.edge_between(belief, n, EdgeType::Contradicts).is_some());
        assert_eq!(
            g.edges()
                .filter(|e| e.edge_type == EdgeType::Contradicts)
                .count(),
            2
        );
    }
    s.mem.close().await.expect("close");
}

/// #19(b) review M2 over the wire: one concept, or one action, that both
/// cites and contradicts the same concept is refused before the ack, and
/// nothing is written.
#[tokio::test]
async fn citing_and_contradicting_one_concept_in_one_call_is_refused() {
    let s = server("mcp-against-both").await;
    let belief = belief(&s).await;
    let before = s.mem.graph().read().snapshot();
    let short = format::short_id(belief);
    for (tool, args) in [
        (
            "lambo_derive",
            json!({"agent_id": "agent-a", "concepts": [{
                "content": "a newer claim", "concept_type": "logic",
                "depends_on": [short.clone()], "against": [BELIEF],
            }]}),
        ),
        (
            "lambo_record_action",
            json!({"agent_id": "agent-a", "action": "acted on it",
                   "depends_on": [BELIEF], "against": [short.clone()]}),
        ),
    ] {
        let out = call_raw(&s, tool, args).await;
        assert_eq!(out.is_error, Some(true), "{tool}");
        let text = text_of(&out);
        assert!(
            text.contains("reference refused: against")
                && text.contains("also cites in depends_on"),
            "{tool}: {text}"
        );
    }
    assert_eq!(s.mem.graph().read().snapshot(), before);
    s.mem.close().await.expect("close");
}

/// #19(b) review L6: the surface counts the per-concept limit per content,
/// as the core does, so one content sent as two wire concepts with 5
/// references each is over 8 here too (for `depends_on` and `against`),
/// and is a bad parameter rather than a core refusal.
#[tokio::test]
async fn the_per_concept_limit_counts_per_content() {
    let s = server("mcp-against-per-content").await;
    let five = |p: &str| -> Vec<String> { (0..5).map(|n| format!("{p}{n}")).collect() };
    for field in ["depends_on", "against"] {
        let out = call_raw(
            &s,
            "lambo_derive",
            json!({"agent_id": "agent-a", "concepts": [
                {"content": "one claim", "concept_type": "logic", field: five("a")},
                {"content": "one claim", "concept_type": "logic", field: five("b")},
            ]}),
        )
        .await;
        assert_eq!(out.is_error, Some(true), "{field}");
        assert!(
            text_of(&out).contains(&format!("a concept's {field} may list at most 8")),
            "{field}: {}",
            text_of(&out)
        );
    }
    s.mem.close().await.expect("close");
}

/// A deferred contradiction replayed at attach publishes the same typed report
/// on the piggyback and explicit lookup; a later restart has only the durable summary.
#[tokio::test]
async fn stale_replay_report_is_structured_on_piggyback_and_lookup() {
    use std::sync::atomic::{AtomicI64, Ordering};
    struct Gate(FixtureEmbedder, tokio::sync::watch::Receiver<bool>);
    #[async_trait::async_trait]
    impl Embedder for Gate {
        fn dimensions(&self) -> usize {
            self.0.dimensions()
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
            if !text.contains(crate::writeq::PROBE_TEXT) {
                let mut hold = self.1.clone();
                while *hold.borrow() {
                    if hold.changed().await.is_err() {
                        break;
                    }
                }
            }
            self.0.embed(text).await
        }
    }
    let inner = Arc::new(MemoryStore::new());
    let store =
        || Arc::new(crate::test_util::VectorSearchable(inner.clone())) as Arc<dyn GraphStore>;
    let seconds = Arc::new(AtomicI64::new(Utc::now().timestamp()));
    let clock: crate::daemon::Clock = {
        let s = seconds.clone();
        Arc::new(move || DateTime::from_timestamp(s.load(Ordering::SeqCst), 0).unwrap())
    };
    let build = |embedder: Arc<dyn Embedder>, strategy| {
        let clock = clock.clone();
        let store = store();
        async move {
            let config = Config {
                canonization_edge_min_age: Duration::ZERO,
                ..Config::default()
            };
            let mem = Memory::builder()
                .session("stale-replay-mcp")
                .agent("agent-a")
                .config(config)
                .flush_interval(Duration::from_secs(3600))
                .store(store)
                .embedder(embedder)
                .embedding_contract(fixture_contract())
                .match_strategy(strategy)
                .clock(clock)
                .build()
                .await
                .unwrap();
            LamboServer::new(Arc::new(mem))
        }
    };
    let s0 = build(
        Arc::new(FixtureEmbedder::new()),
        crate::types::MatchStrategy::Canonical,
    )
    .await;
    let target = belief(&s0).await;
    seconds.fetch_add(2, Ordering::SeqCst);
    let dependent = derive_created(&s0, "agent-b", json!([{"content":"replay dependent", "concept_type":"logic", "depends_on":[target.0.to_string()]}])).await.remove(0);
    s0.mem.close().await.unwrap();
    drop(s0);
    seconds.fetch_add(2, Ordering::SeqCst);
    let (_first_tx, first_rx) = tokio::sync::watch::channel(true);
    let s1 = build(
        Arc::new(Gate(FixtureEmbedder::new(), first_rx)),
        crate::types::MatchStrategy::Hybrid,
    )
    .await;
    let ack = call_raw(&s1, "lambo_derive", json!({"agent_id":"agent-c", "concepts":[{"content":"replay contradiction", "concept_type":"logic", "against":[target.0.to_string()]}]})).await;
    let receipt = ack.structured_content.unwrap()["receipt"]
        .as_str()
        .unwrap()
        .to_owned();
    let destination = crate::types::SessionId::new("project-caller");
    let scoped_agent = AgentId::new("agent-d");
    let scoped = s1
        .mem
        .record_action_async_scoped(
            &scoped_agent,
            &Action {
                repo_scope: Some("host/owner/repo"),
                event_time: None,
                action: "scoped replay contradiction",
                produces: &[],
                modifies: &[],
                depends_on: &[],
            },
            &[target.0.to_string()],
            &crate::surface::resolve::NoForeignScopes,
            Some(&destination),
        )
        .await
        .unwrap();
    s1.mem.close().await.unwrap();
    drop(s1);
    let (replay_tx, replay_rx) = tokio::sync::watch::channel(true);
    let s2 = build(
        Arc::new(Gate(FixtureEmbedder::new(), replay_rx)),
        crate::types::MatchStrategy::Hybrid,
    )
    .await;
    assert_eq!(
        s2.mem
            .pipeline()
            .lookup_for(&scoped_agent, Some(&destination), scoped.receipt)
            .tag(),
        "pending_replay"
    );
    assert_eq!(
        s2.mem
            .pipeline()
            .lookup_for(&scoped_agent, None, scoped.receipt)
            .tag(),
        "forbidden"
    );
    assert_eq!(
        s2.mem
            .pipeline()
            .wait_for(&scoped_agent, None, scoped.receipt, Duration::from_secs(1))
            .await
            .tag(),
        "forbidden"
    );
    let pipeline = s2.mem.pipeline().clone();
    let wait_agent = scoped_agent.clone();
    let wait_destination = destination.clone();
    let scoped_id = scoped.receipt;
    let rightful_wait = tokio::spawn(async move {
        pipeline
            .wait_for(
                &wait_agent,
                Some(&wait_destination),
                scoped_id,
                Duration::from_secs(10),
            )
            .await
    });
    tokio::task::yield_now().await;
    replay_tx.send(false).unwrap();
    assert_eq!(rightful_wait.await.unwrap().tag(), "applied_after_restart");
    let id = receipt.parse().unwrap();
    let answer = s2
        .mem
        .pipeline()
        .wait(
            &AgentId::new("agent-c"),
            id,
            crate::writeq::RECEIPT_WAIT_MAX,
        )
        .await;
    assert_eq!(answer.tag(), "applied_after_restart", "{answer:?}");
    let scoped_answer = s2
        .mem
        .pipeline()
        .wait_for(
            &scoped_agent,
            Some(&destination),
            scoped.receipt,
            crate::writeq::RECEIPT_WAIT_MAX,
        )
        .await;
    assert_eq!(scoped_answer.tag(), "applied_after_restart");
    assert_eq!(
        s2.mem
            .pipeline()
            .lookup_for(&scoped_agent, None, scoped.receipt)
            .tag(),
        "forbidden"
    );
    let forbidden = call_raw(
        &s2,
        "lambo_stats",
        json!({"agent_id":"agent-d", "receipt":scoped.receipt.to_string()}),
    )
    .await;
    assert_eq!(
        forbidden.structured_content.unwrap()["receipt"]["state"],
        "forbidden"
    );
    assert!(s2
        .mem
        .pipeline()
        .take_piggyback_at_most(&scoped_agent, None, 10)
        .0
        .is_empty());
    let project = server("project-caller").await;
    let (rightful_server, _source) = super::layered::layered(&project, &s2);
    let rightful = call_raw(
        &rightful_server,
        "lambo_saints",
        json!({"agent_id":"agent-d"}),
    )
    .await;
    let payload = rightful.structured_content.unwrap();
    let scoped_piggy = &payload["write_receipts"][0];
    assert_eq!(scoped_piggy["id"], scoped.receipt.to_string());
    assert_eq!(scoped_piggy["state"], "applied_after_restart");
    assert_eq!(scoped_piggy["stale_report"]["total"], 1);
    project.mem.close().await.unwrap();
    s2.mem
        .pipeline()
        .forget_live_receipt_for_test(scoped.receipt);
    assert_eq!(
        s2.mem
            .pipeline()
            .lookup_for(&scoped_agent, None, scoped.receipt)
            .tag(),
        "forbidden"
    );
    assert_eq!(
        s2.mem
            .pipeline()
            .lookup_for(&scoped_agent, Some(&destination), scoped.receipt)
            .tag(),
        "applied_after_restart"
    );
    let other = call_raw(&s2, "lambo_saints", json!({"agent_id":"agent-b"})).await;
    assert!(other
        .structured_content
        .unwrap()
        .get("write_receipts")
        .is_none());
    let piggy = call_raw(&s2, "lambo_saints", json!({"agent_id":"agent-c"})).await;
    let p = &piggy.structured_content.unwrap()["write_receipts"][0];
    assert_eq!(p["id"], receipt);
    assert_eq!(p["state"], "applied_after_restart");
    assert_eq!(p["stale_report"]["total"], 1);
    assert_eq!(p["stale_report"]["dependents"][0]["id"], dependent);
    let fetched = call_raw(
        &s2,
        "lambo_stats",
        json!({"agent_id":"agent-c", "receipt":receipt}),
    )
    .await;
    let r = &fetched.structured_content.unwrap()["receipt"];
    assert_eq!(r["state"], "applied_after_restart");
    assert_eq!(r["stale_report"], p["stale_report"]);
    s2.mem.close().await.unwrap();
    drop(s2);
    let s3 = build(
        Arc::new(FixtureEmbedder::new()),
        crate::types::MatchStrategy::Hybrid,
    )
    .await;
    assert_eq!(
        s3.mem
            .pipeline()
            .lookup_for(&scoped_agent, None, scoped.receipt)
            .tag(),
        "forbidden"
    );
    assert_eq!(
        s3.mem
            .pipeline()
            .lookup_for(&scoped_agent, Some(&destination), scoped.receipt)
            .tag(),
        "applied_after_restart"
    );
    assert_eq!(
        s3.mem
            .pipeline()
            .wait_for(&scoped_agent, None, scoped.receipt, Duration::from_secs(1))
            .await
            .tag(),
        "forbidden"
    );
    let fetched = call_raw(
        &s3,
        "lambo_stats",
        json!({"agent_id":"agent-c", "receipt":receipt}),
    )
    .await;
    let payload = fetched.structured_content.unwrap();
    assert_eq!(payload["receipt"]["state"], "applied_after_restart");
    assert!(payload["receipt"].get("stale_report").is_none());
    s3.mem.close().await.unwrap();
}

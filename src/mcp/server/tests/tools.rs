//! Tool behaviour through the router: recall, record_action, saints,
//! reserve and closed-session handling.

use super::*;

#[tokio::test]
async fn recall_through_the_router_returns_the_context_block() {
    let s = server("mcp-recall").await;
    // Write something to recall.
    let derived = call(
        &s,
        "lambo_derive",
        serde_json::json!({
            "agent_id": "agent-a",
            "concepts": [
                {"content": "user schema", "concept_type": "entity"},
                {"content": "must stay backward compatible", "concept_type": "constraint"}
            ]
        }),
    )
    .await;
    assert_eq!(derived.is_error, Some(false), "{derived:?}");

    let out = call(
        &s,
        "lambo_recall",
        serde_json::json!({"agent_id": "agent-a", "query": "update user schema"}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");

    // The text content is the T5.3 context block verbatim — that is the
    // artifact the calling agent reads.
    let text = match &out.content[0] {
        ContentBlock::Text(t) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(
        text.contains("user schema"),
        "context block should name the recalled concept, got:\n{text}"
    );
    let structured = out.structured_content.expect("structured content");
    assert_eq!(structured["context"], serde_json::Value::String(text));
    assert!(structured["hits"].as_array().is_some_and(|h| !h.is_empty()));
    s.mem.close().await.expect("close");
}

#[tokio::test]
async fn record_action_and_saints_and_stats_round_trip() {
    let s = server("mcp-roundtrip").await;
    let acted = call(
        &s,
        "lambo_record_action",
        serde_json::json!({
            "agent_id": "agent-a",
            "action": "created migrations/003.sql",
            "produces": ["migrations/003.sql"],
            "depends_on": ["user schema"]
        }),
    )
    .await;
    assert_eq!(acted.is_error, Some(false), "{acted:?}");

    // Nothing is canonical yet, but the tool must answer cleanly.
    let saints = call(
        &s,
        "lambo_saints",
        serde_json::json!({"agent_id": "agent-a"}),
    )
    .await;
    assert_eq!(saints.is_error, Some(false));
    assert_eq!(
        saints.structured_content.unwrap()["saints"],
        serde_json::json!([])
    );

    let stats = call(
        &s,
        "lambo_stats",
        serde_json::json!({"agent_id": "agent-a"}),
    )
    .await;
    assert_eq!(stats.is_error, Some(false));
    let st = stats.structured_content.unwrap();
    assert_eq!(st["session"], "mcp-roundtrip");
    assert!(st["node_count"].as_u64().unwrap() > 0);
    s.mem.close().await.expect("close");
}

/// **N1 pinned.** `lambo_record_action` refuses a target list whose combined
/// `produces` + `modifies` + `depends_on` count exceeds `MAX_ACTION_TARGETS`,
/// and accepts one exactly at the cap — so the bound is a real cap, not an
/// off-by-one that never trips.
#[tokio::test]
async fn the_image_suffix_is_refused_in_concept_text_but_not_in_references() {
    let s = server("mcp-image-suffix").await;
    for (tool, args) in [
        (
            "lambo_derive",
            json!({"agent_id": "agent-a",
                   "concepts": [{"content": "render 17 [image:r17]", "concept_type": "entity"}]}),
        ),
        (
            "lambo_derive",
            json!({"agent_id": "agent-a",
                   "concepts": [{"content": "fine", "concept_type": "entity"},
                                {"content": "[IMAGE:R17] Render 17", "concept_type": "logic"}]}),
        ),
        (
            "lambo_record_action",
            json!({"agent_id": "agent-a", "action": "dismissed [image:r17]"}),
        ),
    ] {
        let out = call_raw(&s, tool, args).await;
        assert_eq!(out.is_error, Some(true), "{tool}: {out:?}");
        let text = text_of(&out);
        assert!(text.contains("only lambo_derive_image creates"), "{text}");
        assert!(!text.contains("r17") && !text.contains("R17"), "{text}");
        assert!(
            out.structured_content.is_none(),
            "no receipt was issued: {out:?}"
        );
    }
    assert_eq!(s.mem.stats().concept_count, 0, "nothing was written");

    // Naming an image concept from a reference is how a text write links to
    // it (design 5.2), so references are not refused.
    let ok = call(
        &s,
        "lambo_record_action",
        json!({"agent_id": "agent-a", "action": "dismissed the red saree",
               "depends_on": ["red saree [image:r17]"]}),
    )
    .await;
    assert_eq!(ok.is_error, Some(false), "{ok:?}");
    s.mem.close().await.expect("close");
}

#[tokio::test]
async fn record_action_caps_the_combined_target_count() {
    let s = server("mcp-action-cap").await;

    // One over the cap, split across all three lists, must be refused.
    let produces: Vec<String> = (0..MAX_ACTION_TARGETS).map(|i| format!("p{i}")).collect();
    let over = call(
        &s,
        "lambo_record_action",
        serde_json::json!({
            "agent_id": "agent-a",
            "action": "touch everything",
            "produces": produces,
            "modifies": ["m0"],
        }),
    )
    .await;
    assert_eq!(
        over.is_error,
        Some(true),
        "a target list over the cap must be refused: {over:?}"
    );
    let text = match &over.content[0] {
        ContentBlock::Text(t) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(
        text.contains(&MAX_ACTION_TARGETS.to_string()),
        "the refusal must name the cap, got: {text}"
    );

    // Exactly at the cap is accepted.
    let at_cap: Vec<String> = (0..MAX_ACTION_TARGETS).map(|i| format!("q{i}")).collect();
    let ok = call(
        &s,
        "lambo_record_action",
        serde_json::json!({
            "agent_id": "agent-a",
            "action": "touch exactly the cap",
            "produces": at_cap,
        }),
    )
    .await;
    assert_eq!(
        ok.is_error,
        Some(false),
        "a target list exactly at the cap must be accepted: {ok:?}"
    );
    s.mem.close().await.expect("close");
}

#[tokio::test]
async fn reserve_takes_and_releases_a_soft_lock() {
    let s = server("mcp-reserve").await;
    let created = derive_created(
        &s,
        "agent-a",
        serde_json::json!([{"content": "session store", "concept_type": "entity"}]),
    )
    .await
    .remove(0);

    let held = call(
        &s,
        "lambo_reserve",
        serde_json::json!({"agent_id": "agent-a", "node_id": created, "ttl_seconds": 30}),
    )
    .await;
    assert_eq!(held.is_error, Some(false), "{held:?}");

    let freed = call(
        &s,
        "lambo_reserve",
        serde_json::json!({"agent_id": "agent-a", "node_id": created, "release": true}),
    )
    .await;
    assert_eq!(freed.is_error, Some(false), "{freed:?}");
    s.mem.close().await.expect("close");
}

/// A closed session must refuse writes through the MCP surface too, as a
/// readable tool error rather than a panic or a silent success.
#[tokio::test]
async fn a_closed_session_refuses_writes_through_the_tools() {
    let s = server("mcp-closed").await;
    s.mem.close().await.expect("close");
    let out = call(
        &s,
        "lambo_derive",
        serde_json::json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "too late", "concept_type": "entity"}]
        }),
    )
    .await;
    assert_eq!(out.is_error, Some(true), "{out:?}");
}

/// **J1 acceptance, replacing R1/T82-3's blanket refusal.** The same
/// three-agent reproduction, now with real mutual exclusion instead of a
/// refusal: `agent-b` must not be able to reserve a node `agent-a` holds
/// (a *conflict*, not a refusal-to-try), `agent-c` must not be able to
/// release it, and `agent-a` must still be able to.
///
/// R1/T82-3's reasoning stands — mutual exclusion that reports success
/// without providing exclusion is worse than none — but the exclusion is
/// now genuine, so refusing is no longer how it is honoured. What must NOT
/// regress is the second half: a non-holder still cannot release.
#[tokio::test]
async fn two_agents_through_one_server_hold_distinct_locks() {
    let s = server("mcp-reserve-foreign").await;
    let node = derive_created(
        &s,
        "agent-a",
        serde_json::json!([{"content": "shared config", "concept_type": "entity"}]),
    )
    .await
    .remove(0);

    let a = call(
        &s,
        "lambo_reserve",
        serde_json::json!({"agent_id": "agent-a", "node_id": node, "ttl_seconds": 60}),
    )
    .await;
    assert_eq!(a.is_error, Some(false), "{a:?}");
    assert_eq!(
        a.structured_content.unwrap()["agent_id"],
        serde_json::json!("agent-a"),
        "the lock is held under the caller's id"
    );

    // Contention, not refusal: `agent-b` loses the race for a node
    // `agent-a` holds — the §11 conflict that could never fire before J1
    // because there was only ever one agent.
    let b = call(
        &s,
        "lambo_reserve",
        serde_json::json!({"agent_id": "agent-b", "node_id": node, "ttl_seconds": 60}),
    )
    .await;
    assert_eq!(
        b.is_error,
        Some(true),
        "a second agent must not be told it took a lock the first holds: {b:?}"
    );
    // J1-R1-2: the refusal must be *usable*, not just correctly classed.
    // "coordinate by ids" needs the holder and the expiry, so the loser can
    // tell a lock worth waiting for from one to work around.
    let expiry = s
        .mem
        .graph()
        .read()
        .reservation(NodeId(node.parse().unwrap()))
        .expect("agent-a's reservation")
        .expires_at
        .to_string();
    for expected in ["agent-a", "until", expiry.as_str(), "nothing was reserved"] {
        assert!(
            text_of(&b).contains(expected),
            "the loss must name {expected:?} — the holder, the expiry and \
                 what happened, not just the class: {}",
            text_of(&b)
        );
    }

    // A non-holder still cannot release — the half of R1/T82-3 that must
    // never regress, now enforced by the graph rather than by a guard in
    // this file.
    for other in ["agent-b", "agent-c"] {
        let r = call(
            &s,
            "lambo_reserve",
            serde_json::json!({"agent_id": other, "node_id": node, "release": true}),
        )
        .await;
        assert_eq!(
            r.is_error,
            Some(true),
            "{other} must not be able to release agent-a's lock: {r:?}"
        );
        assert!(
            text_of(&r).contains("agent-a") && text_of(&r).contains("nothing was released"),
            "and must be told who does hold it, and that its own call \
                 changed nothing: {}",
            text_of(&r)
        );
    }
    assert!(
        s.mem
            .graph()
            .read()
            .reservation(NodeId(node.parse().unwrap()))
            .is_some(),
        "agent-a's reservation must have survived every foreign attempt"
    );

    // Distinct locks: `agent-b` holds its own on a different node while
    // `agent-a` holds this one. Two clients, one serve, two locks.
    let other_node = derive_created(
        &s,
        "agent-b",
        serde_json::json!([{"content": "b's own node", "concept_type": "entity"}]),
    )
    .await
    .remove(0);
    let b_own = call(
        &s,
        "lambo_reserve",
        serde_json::json!({"agent_id": "agent-b", "node_id": other_node, "ttl_seconds": 60}),
    )
    .await;
    assert_eq!(
        b_own.is_error,
        Some(false),
        "a foreign agent_id must be able to take a lock of its own — the \
             pre-J1 blanket refusal is gone: {b_own:?}"
    );
    assert_eq!(
        b_own.structured_content.unwrap()["agent_id"],
        serde_json::json!("agent-b")
    );

    // And the holder can still let go.
    let freed = call(
        &s,
        "lambo_reserve",
        serde_json::json!({"agent_id": "agent-a", "node_id": node, "release": true}),
    )
    .await;
    assert_eq!(freed.is_error, Some(false), "{freed:?}");
    s.mem.close().await.expect("close");
}

/// **R1/T82-14 pinned.** The read tools answer from the RAM graph after
/// `close()` — documented on the impl block, and now asserted, so a future
/// change to that behaviour is a deliberate one.
#[tokio::test]
async fn read_tools_still_answer_after_close() {
    let s = server("mcp-closed-reads").await;
    call(
        &s,
        "lambo_derive",
        serde_json::json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "before the close", "concept_type": "entity"}]
        }),
    )
    .await;
    s.mem.close().await.expect("close");

    for (tool, args) in [
        ("lambo_stats", serde_json::json!({"agent_id": "agent-a"})),
        ("lambo_saints", serde_json::json!({"agent_id": "agent-a"})),
        (
            "lambo_inspect",
            serde_json::json!({"agent_id": "agent-a", "focus": "before the close"}),
        ),
    ] {
        let out = call(&s, tool, args).await;
        assert_eq!(
            out.is_error,
            Some(false),
            "{tool} reads a closed session's RAM graph, which does not change: {out:?}"
        );
    }
    // Recall, which needs the embedder and the store, still refuses.
    let recall = call(
        &s,
        "lambo_recall",
        serde_json::json!({"agent_id": "agent-a", "query": "before the close"}),
    )
    .await;
    assert_eq!(recall.is_error, Some(true), "{recall:?}");
}

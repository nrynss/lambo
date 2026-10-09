//! I1 / I2: the serve call ledger and heartbeat lines.

use super::*;

/// Derive, wait, and return the `receipt` object from the `lambo_stats`
/// payload — the whole relocated fact set, as a client sees it.
async fn receipt_payload(
    s: &LamboServer,
    agent_id: &str,
    concepts: serde_json::Value,
) -> serde_json::Value {
    let ack = call_raw(
        s,
        "lambo_derive",
        serde_json::json!({"agent_id": agent_id, "concepts": concepts}),
    )
    .await;
    let receipt = ack.structured_content.as_ref().expect("ack payload")["receipt"]
        .as_str()
        .expect("ack carries a receipt id")
        .to_string();
    let waited = call_raw(
        s,
        "lambo_stats",
        serde_json::json!({
            "agent_id": agent_id,
            "receipt": receipt,
            "wait_ms": crate::writeq::RECEIPT_WAIT_MAX.as_millis() as u64,
        }),
    )
    .await;
    waited.structured_content.expect("stats payload")["receipt"].clone()
}

/// **I1 acceptance.** With `--ledger` on, EVERY published tool appends
/// exactly one line, and every line parses as one JSON object carrying the
/// common head. Driven through the `#[tool]` wrappers (the only thing the
/// router can reach), so a tool whose wrapper forgot the ledger fails here.
#[tokio::test]
async fn i1_every_tool_call_appends_exactly_one_parseable_ledger_line() {
    let dir = ledger_dir("every-tool");
    let path = dir.join("calls.jsonl");
    let s = server_with_ledger("i1-every-tool", &path).await;
    let ledger = Arc::clone(s.ledger().expect("ledger attached"));

    // One call per published tool, in a realistic order.
    call(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "the ledger is not the store", "concept_type": "logic"}],
        }),
    )
    .await;
    call(
        &s,
        "lambo_record_action",
        json!({
            "agent_id": "agent-a",
            "action": "wrote src/ledger.rs",
            "produces": ["src/ledger.rs"],
            "depends_on": ["the ledger is not the store"],
        }),
    )
    .await;
    call(
        &s,
        "lambo_recall",
        json!({"agent_id": "agent-a", "query": "ledger"}),
    )
    .await;
    call(
        &s,
        "lambo_inspect",
        json!({"agent_id": "agent-a", "focus": "ledger"}),
    )
    .await;
    call(&s, "lambo_saints", json!({"agent_id": "agent-a"})).await;
    call(&s, "lambo_stats", json!({"agent_id": "agent-a"})).await;
    call(
        &s,
        "lambo_reserve",
        json!({
            "agent_id": "agent-a",
            "node_id": uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    // #22: listed here because the fixture embeds images. This store has no
    // vector search, so it is refused, which still writes its one line.
    call(
        &s,
        "lambo_derive_image",
        json!({
            "agent_id": "agent-a",
            "caption": "red silk saree",
            "concept_type": "resource",
            "image": {"mime": "image/png", "data": png_b64("red silk saree")},
        }),
    )
    .await;

    let published: Vec<String> = tools(&s).iter().map(|t| t.name.to_string()).collect();
    let lines = read_ledger(&ledger, published.len() as u64);

    let mut seen: Vec<String> = Vec::new();
    for line in &lines {
        assert_eq!(line["v"], json!(crate::ledger::LINE_VERSION), "{line}");
        assert_eq!(line["kind"], json!("call"), "{line}");
        assert_eq!(line["agent_id"], json!("agent-a"), "{line}");
        assert!(
            chrono::DateTime::parse_from_rfc3339(line["ts"].as_str().expect("ts is a string"))
                .is_ok(),
            "the server timestamp is RFC3339: {line}"
        );
        assert!(line["duration_us"].is_u64(), "{line}");
        assert!(
            matches!(line["outcome"].as_str(), Some("ok" | "error" | "panic")),
            "outcome is one of the three classes: {line}"
        );
        seen.push(line["tool"].as_str().expect("tool name").to_string());
    }
    seen.sort();
    let mut expected = published;
    expected.sort();
    assert_eq!(
        seen, expected,
        "every published tool contributed exactly one line"
    );

    ledger.shutdown();
    s.mem.close().await.expect("close");
}

/// **I1 acceptance (DOGFOOD metrics 4 and 5).** A recall line carries the
/// final score AND the per-leg provenance the max-merge would otherwise
/// destroy, plus the typed warning flags — including
/// `blast_radius_warning`, which is metric 5 in one field.
///
/// The `⚑` line is provoked the way it fires in production: a Canonical
/// concept with dependents. The assertion is on the FLAG, never on the
/// rendered text — the whole point of reading H3's typed annotation kinds
/// is that "a warning fired" stops being a grep.
#[tokio::test]
async fn i1_recall_lines_carry_per_leg_scores_and_the_warning_flags() {
    use crate::types::CanonizationStatus;

    let dir = ledger_dir("recall-legs");
    let path = dir.join("calls.jsonl");
    let s = server_with_ledger("i1-recall-legs", &path).await;
    let ledger = Arc::clone(s.ledger().expect("ledger attached"));

    // An action gives the target a dependent, so its blast radius is > 0.
    call(
        &s,
        "lambo_record_action",
        json!({
            "agent_id": "agent-a",
            "action": "rebuild the pagination index",
            "modifies": ["pagination contract"],
        }),
    )
    .await;
    // Promote the target: `⚑` renders for Canonical hits only. Through the
    // audited transition path (None -> Venerable -> Canonical), because
    // that is the only way a status can legally change — there is no
    // back-door setter, by design (GRAPH-4).
    {
        let mut g = s.mem.graph().write();
        let target = g
            .concepts()
            .find(|c| c.content.contains("pagination contract"))
            .map(|c| c.id)
            .expect("the action created its target concept");
        let now = chrono::Utc::now();
        for (from, to) in [
            (CanonizationStatus::None, CanonizationStatus::Venerable),
            (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
        ] {
            g.apply_canonization_transition(crate::types::CanonizationEvent {
                id: NodeId::new(),
                session_id: s.mem.session().clone(),
                node_id: target,
                from_status: from,
                to_status: to,
                blast_radius: Some(1),
                last_demotion_time: None,
                occurred_at: now,
            })
            .expect("audited promotion");
        }
    }

    call(
        &s,
        "lambo_recall",
        json!({"agent_id": "agent-a", "query": "pagination contract"}),
    )
    .await;

    let lines = read_ledger(&ledger, 2);
    let recall = lines
        .iter()
        .find(|l| l["tool"] == json!("lambo_recall"))
        .expect("a recall line");

    assert_eq!(recall["outcome"], json!("ok"), "{recall}");
    assert_eq!(recall["query"], json!("pagination contract"), "{recall}");
    assert!(recall["top_k"].is_u64(), "{recall}");
    let hits = recall["hits"].as_array().expect("hits array");
    assert!(!hits.is_empty(), "the query must actually hit: {recall}");

    // Per-leg provenance: at least one hit reports a named leg with a
    // number, and every reported leg name is one of the three phase-1 legs.
    let mut legged = 0usize;
    for hit in hits {
        assert!(
            hit["score"].is_f64() || hit["score"].is_i64(),
            "final score: {hit}"
        );
        assert!(hit["node_id"].as_str().is_some(), "{hit}");
        assert!(hit["included_in_context"].is_boolean(), "{hit}");
        let legs = hit["legs"].as_object().expect("legs is an object");
        for (name, value) in legs {
            assert!(
                matches!(name.as_str(), "bm25" | "recent" | "vector_cosine"),
                "unexpected leg name {name}: {hit}"
            );
            assert!(
                value.is_f64() || value.is_i64(),
                "leg {name} carries a score: {hit}"
            );
        }
        if !legs.is_empty() {
            legged += 1;
        }
    }
    assert!(
        legged > 0,
        "at least one hit must report its phase-1 legs, else the provenance was dropped \
             on the way out: {recall}"
    );

    // The warning flags, from typed producers.
    assert_eq!(
        recall["canonical_marker"],
        json!(true),
        "a Canonical hit was returned, so the canonical marker rendered: {recall}"
    );
    assert_eq!(
        recall["blast_radius_warning"],
        json!(true),
        "DOGFOOD metric 5: the blast-radius warning fired and the ledger says so: {recall}"
    );
    for flag in ["conflict_line", "hot_warning", "reservation_warning"] {
        assert!(
            recall[flag].is_boolean(),
            "{flag} is always present as a boolean: {recall}"
        );
    }
    assert!(recall["warning_count"].is_u64(), "{recall}");
    // `canonical_marker` above is only claimable because the block that
    // carries `[canonical]` actually rendered. Pinned so the two halves of
    // the flag's definition are asserted together, not separately.
    assert!(
        hits.iter()
            .any(|h| h["is_canonical"] == json!(true) && h["included_in_context"] == json!(true)),
        "the Canonical hit must be IN the context for canonical_marker to be true: {recall}"
    );

    ledger.shutdown();
    s.mem.close().await.expect("close");
}

/// **I-R1-1.** The five set-level flags are not all budget-blind, because
/// the two rendering paths are not alike.
///
/// Recall the same Canonical, load-bearing concept with `max_tokens: 1`, so
/// no hit block fits and the rendered context is empty. The reviewer's probe,
/// promoted to a test:
///
/// * `canonical_marker` must be **false** — `[canonical]` lives only inside a
///   hit's block, and no block rendered. Reporting `true` here was the
///   finding: the ledger claimed a marker the agent never received.
/// * the four warning flags must still be **true** — their lines go into the
///   flat `warnings` vector for every returned hit whatever the budget did,
///   and reach the agent as a second text block.
/// * per-hit `is_canonical` must still be true, so "was a Canonical concept
///   *returned*" stays answerable from the hits.
#[tokio::test]
async fn i1_the_canonical_marker_flag_is_false_when_the_budget_rendered_nothing() {
    use crate::types::CanonizationStatus;

    let dir = ledger_dir("canonical-budget");
    let path = dir.join("calls.jsonl");
    let s = server_with_ledger("i1-canonical-budget", &path).await;
    let ledger = Arc::clone(s.ledger().expect("ledger attached"));

    call(
        &s,
        "lambo_record_action",
        json!({
            "agent_id": "agent-a",
            "action": "rebuild the pagination index",
            "modifies": ["pagination contract"],
        }),
    )
    .await;
    {
        let mut g = s.mem.graph().write();
        let target = g
            .concepts()
            .find(|c| c.content.contains("pagination contract"))
            .map(|c| c.id)
            .expect("the action created its target concept");
        let now = chrono::Utc::now();
        for (from, to) in [
            (CanonizationStatus::None, CanonizationStatus::Venerable),
            (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
        ] {
            g.apply_canonization_transition(crate::types::CanonizationEvent {
                id: NodeId::new(),
                session_id: s.mem.session().clone(),
                node_id: target,
                from_status: from,
                to_status: to,
                blast_radius: Some(1),
                last_demotion_time: None,
                occurred_at: now,
            })
            .expect("audited promotion");
        }
    }

    let out = call(
        &s,
        "lambo_recall",
        json!({
            "agent_id": "agent-a",
            "query": "pagination contract",
            "max_tokens": 1,
        }),
    )
    .await;

    // The response really did carry no marker: assert against the artifact
    // the agent received, not only against the ledger's opinion of it.
    let rendered = out
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !rendered.contains("[canonical]"),
        "a 1-token budget renders no hit block, so no canonical marker: {rendered}"
    );

    let lines = read_ledger(&ledger, 2);
    let recall = lines
        .iter()
        .find(|l| l["tool"] == json!("lambo_recall"))
        .expect("a recall line");

    let hits = recall["hits"].as_array().expect("hits array");
    assert!(!hits.is_empty(), "the query must still hit: {recall}");
    assert!(
        hits.iter()
            .all(|h| h["included_in_context"] == json!(false)),
        "no hit fits a 1-token budget: {recall}"
    );
    assert!(
        hits.iter().any(|h| h["is_canonical"] == json!(true)),
        "the Canonical hit was still RETURNED, and the hit says so: {recall}"
    );

    assert_eq!(
        recall["canonical_marker"],
        json!(false),
        "the marker renders inside the block, and no block rendered: {recall}"
    );
    assert_eq!(
        recall["blast_radius_warning"],
        json!(true),
        "the warning line reaches the agent through `warnings` whatever the budget \
             did to the block: {recall}"
    );

    ledger.shutdown();
    s.mem.close().await.expect("close");
}

/// **I1 acceptance, relocated by J3.** The metric-2 counts moved from the
/// ledger call line to the **receipt**, because an ack issued before the
/// write has no counts to report. This test pins both halves: what the line
/// carries now, and that the distinction metric 2 turns on
/// (`created` against `matched` on a re-derive) is still recoverable.
#[tokio::test]
async fn i1_derive_lines_carry_the_admission_and_the_receipt_carries_the_counts() {
    let dir = ledger_dir("derive-counts");
    let path = dir.join("calls.jsonl");
    let s = server_with_ledger("i1-derive-counts", &path).await;
    let ledger = Arc::clone(s.ledger().expect("ledger attached"));

    let concepts = json!([{"content": "recall before you derive", "concept_type": "logic"}]);
    let first = receipt_payload(&s, "agent-a", concepts.clone()).await;
    let second = receipt_payload(&s, "agent-a", concepts).await;

    // The receipt: metric 2, unchanged in meaning.
    assert_eq!(
        first["created_count"],
        json!(1),
        "first derive creates: {first}"
    );
    assert_eq!(first["matched_count"], json!(0), "{first}");
    assert_eq!(
        second["created_count"],
        json!(0),
        "re-deriving the same content creates nothing: {second}"
    );
    assert_eq!(
        second["matched_count"],
        json!(1),
        "re-deriving the same content MATCHES — this is metric 2: {second}"
    );
    for r in [&first, &second] {
        assert!(r["semantic_merged"].is_u64(), "{r}");
        assert!(r["reinforced"].is_u64(), "{r}");
        // `edges` belongs to record_action; a zero here would be a claim.
        assert!(r.get("edges").is_none(), "{r}");
    }

    // The line: what the ack knew when it was written. Two derive calls
    // and two stats calls, in that interleaved order.
    let lines = read_ledger(&ledger, 4);
    for i in [0usize, 2] {
        let line = &lines[i];
        assert_eq!(line["tool"], json!("lambo_derive"), "{line}");
        assert_eq!(line["concepts_requested"], json!(1), "{line}");
        assert_eq!(line["admitted"], json!(true), "{line}");
        assert!(
            line["receipt"].is_string(),
            "the line must name the receipt its counts moved to: {line}"
        );
        assert!(
            line["created"].is_null() && line["matched"].is_null(),
            "created/matched must be ABSENT, not zero — the ack does not know them: {line}"
        );
    }
    // The join works: the line's receipt is the receipt the counts are on.
    assert_eq!(lines[0]["receipt"], first["id"], "{}", lines[0]);
    assert_eq!(lines[2]["receipt"], second["id"], "{}", lines[2]);

    ledger.shutdown();
    s.mem.close().await.expect("close");
}

/// **I1 acceptance.** `record_action` reports its edge count and `reserve`
/// reports grant/refusal — including the refusal, which must never be
/// recorded as a grant.
#[tokio::test]
async fn i1_record_action_reports_edges_and_reserve_reports_grant_or_refusal() {
    let dir = ledger_dir("edges-grants");
    let path = dir.join("calls.jsonl");
    let s = server_with_ledger("i1-edges-grants", &path).await;
    let ledger = Arc::clone(s.ledger().expect("ledger attached"));

    call(
        &s,
        "lambo_record_action",
        json!({
            "agent_id": "agent-a",
            "action": "provision the store",
            "produces": ["migrations/sqlite/001_init.sql"],
            "modifies": ["schema"],
        }),
    )
    .await;
    let node_id = {
        let g = s.mem.graph().read();
        let id = g.concepts().next().map(|c| c.id).expect("a concept");
        id.0.to_string()
    };
    // Granted.
    call(
        &s,
        "lambo_reserve",
        json!({"agent_id": "agent-a", "node_id": node_id}),
    )
    .await;
    // Granted to a DIFFERENT agent on a different node (J1): a foreign id
    // now succeeds, and the grant must be booked as a grant under that id.
    let other_node = {
        let g = s.mem.graph().read();
        let mut ids = g.concepts().map(|c| c.id);
        let first = ids.next().expect("a concept");
        ids.find(|id| *id != first).unwrap_or(first).0.to_string()
    };
    call(
        &s,
        "lambo_reserve",
        json!({"agent_id": "someone-else", "node_id": other_node}),
    )
    .await;
    // Refused: `someone-else` loses a race for the node `agent-a` holds.
    // Post-J1 the only reserve refusal is a real §11 conflict, so this is
    // the line that pins `granted: false` against a path that could report
    // a grant.
    call(
        &s,
        "lambo_reserve",
        json!({"agent_id": "someone-else", "node_id": node_id}),
    )
    .await;

    let lines = read_ledger(&ledger, 4);
    let action = &lines[0];
    assert_eq!(action["tool"], json!("lambo_record_action"));
    // J3: `edges` and `created` moved to the receipt — an ack issued before
    // the write cannot count either. The line names the receipt so the two
    // can be joined; `i1_derive_lines_carry_the_admission_and_the_receipt_carries_the_counts`
    // pins the counts themselves on the derive side of the same change.
    assert_eq!(action["admitted"], json!(true), "{action}");
    assert!(action["receipt"].is_string(), "{action}");
    assert!(
        action["edges"].is_null() && action["created"].is_null(),
        "edges/created must be ABSENT, not zero: {action}"
    );

    let granted = &lines[1];
    assert_eq!(granted["op"], json!("reserve"), "{granted}");
    assert_eq!(granted["granted"], json!(true), "{granted}");
    assert_eq!(granted["outcome"], json!("ok"), "{granted}");

    let foreign_grant = &lines[2];
    assert_eq!(foreign_grant["op"], json!("reserve"), "{foreign_grant}");
    assert_eq!(
        foreign_grant["granted"],
        json!(true),
        "a foreign id's successful reserve is a grant: {foreign_grant}"
    );
    assert_eq!(foreign_grant["outcome"], json!("ok"), "{foreign_grant}");
    assert_eq!(
        foreign_grant["agent_id"],
        json!("someone-else"),
        "the line attributes to the CALLER, not the process agent: {foreign_grant}"
    );

    let refused = &lines[3];
    assert_eq!(refused["op"], json!("reserve"), "{refused}");
    assert_eq!(
        refused["granted"],
        json!(false),
        "a refusal must never be logged as a grant: {refused}"
    );
    assert_eq!(refused["outcome"], json!("error"), "{refused}");
    assert_eq!(
        refused["error_kind"],
        json!("conflict"),
        "post-J1 the reserve refusal is a real §11 conflict: {refused}"
    );
    assert_eq!(refused["agent_id"], json!("someone-else"), "{refused}");

    ledger.shutdown();
    s.mem.close().await.expect("close");
}

/// **I1 acceptance — the failure mode that matters.** The ledger path goes
/// away *mid-run*. Every subsequent tool call must still succeed, the lines
/// must be counted as dropped, and `lambo_stats` must report the count so
/// the silence in the file is visible.
#[tokio::test]
async fn i1_an_unwritable_path_mid_run_drops_lines_and_never_fails_a_tool_call() {
    let dir = ledger_dir("unwritable");
    let path = dir.join("calls.jsonl");
    let s = server_with_ledger("i1-unwritable", &path).await;
    let ledger = Arc::clone(s.ledger().expect("ledger attached"));

    // One good call first, so the "before" state is real.
    let ok = call(&s, "lambo_stats", json!({"agent_id": "agent-a"})).await;
    assert_ne!(ok.is_error, Some(true), "the first call succeeds");
    let deadline = Instant::now() + Duration::from_secs(10);
    while ledger.counters().written() < 1 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(ledger.counters().written(), 1);

    // Pull the ground out. The writer reopens per batch, so the next batch
    // cannot open its path. Removing the directory (rather than `chmod`)
    // fails the same way for root, which some CI containers are.
    std::fs::remove_dir_all(&dir).expect("remove the ledger directory");

    // Every tool, after the failure. All must succeed.
    let calls: Vec<(&str, serde_json::Value)> = vec![
        (
            "lambo_derive",
            json!({
                "agent_id": "agent-a",
                "concepts": [{"content": "memory outlives its ledger", "concept_type": "logic"}],
            }),
        ),
        (
            "lambo_record_action",
            json!({
                "agent_id": "agent-a", "action": "kept serving", "produces": ["a line that is gone"],
            }),
        ),
        (
            "lambo_recall",
            json!({"agent_id": "agent-a", "query": "memory"}),
        ),
        ("lambo_saints", json!({"agent_id": "agent-a"})),
        ("lambo_stats", json!({"agent_id": "agent-a"})),
    ];
    let n = calls.len() as u64;
    for (tool, args) in calls {
        let out = call(&s, tool, args).await;
        assert_ne!(
            out.is_error,
            Some(true),
            "{tool} must still succeed with a dead ledger — observability never takes \
                 down memory"
        );
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    while ledger.counters().dropped() < n && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        ledger.counters().dropped(),
        n,
        "every post-failure line is accounted for as a drop"
    );
    assert_eq!(
        ledger.counters().written(),
        1,
        "the one pre-failure line stays written"
    );

    // The counter must be reachable from `lambo_stats` — otherwise the
    // silence is invisible, which is the whole failure this guards against.
    let stats = call(&s, "lambo_stats", json!({"agent_id": "agent-a"})).await;
    let payload = stats.structured_content.expect("stats payload");
    assert_eq!(
        payload["ledger_dropped_lines"]
            .as_u64()
            .expect("drop count in the stats payload"),
        n,
        "lambo_stats reports the dropped-line count: {payload}"
    );
    assert_eq!(payload["ledger_written_lines"], json!(1), "{payload}");
    assert!(
        payload["ledger_path"].as_str().is_some(),
        "the payload names the path the drops were destined for: {payload}"
    );

    ledger.shutdown();
    s.mem.close().await.expect("close");
}

/// **Off means off.** With no ledger the `lambo_stats` payload carries no
/// `ledger_*` key at all — the payload is what it was before I1 existed.
#[tokio::test]
async fn i1_with_the_ledger_off_the_stats_payload_is_unchanged() {
    let s = server("i1-off").await;
    assert!(s.ledger().is_none(), "the ledger is off by default");
    let stats = call(&s, "lambo_stats", json!({"agent_id": "agent-a"})).await;
    let payload = stats.structured_content.expect("stats payload");
    let obj = payload.as_object().expect("object");
    for key in obj.keys() {
        assert!(
            !key.starts_with("ledger_"),
            "with --ledger off the payload must not grow a {key} field: {payload}"
        );
    }
    // …and the fields callers already depend on are all still there.
    for key in [
        "summary",
        "session",
        "agent",
        "flush_lag_ms",
        "log_depth",
        "flush_depth",
        "dead_lettered",
        "degraded",
        "node_count",
        "edge_count",
        "concept_count",
        "total_concepts",
        "embedded_concepts",
        "canonical_count",
        "epoch",
        "daemon_cycles",
        "canonization_cycles",
        "canonization_failures",
        // P2-b: which policy the cycle counters belong to. A caller that
        // has to infer it from a flat `canonical_count` is back to the
        // dead end the selector exists to end.
        "promotion_policy",
        // Issue #29: GC sweep accounting (an additive key).
        "gc",
        "warnings",
    ] {
        assert!(
            obj.contains_key(key),
            "{key} is missing from the payload: {payload}"
        );
    }
    s.mem.close().await.expect("close");
}

/// **I1 acceptance.** A full dogfood day's worth of lines round-trips
/// through a JSON parser — the `duckdb`-end-to-end criterion, asserted on
/// the property duckdb's `read_json` actually needs (every line an
/// independent JSON object, no partial writes, no interleaving) rather than
/// by shelling out to duckdb from a unit test.
///
/// Concurrency is the part worth testing: the HTTP transport clones the
/// server handle per request, so many tasks append at once and a torn line
/// would be invisible in a serial test.
#[tokio::test]
async fn i1_a_days_worth_of_concurrent_lines_all_parse() {
    let dir = ledger_dir("a-day");
    let path = dir.join("calls.jsonl");
    let s = server_with_ledger("i1-a-day", &path).await;
    let ledger = Arc::clone(s.ledger().expect("ledger attached"));

    // 480 calls is a heavy dogfood day (a call every ~3 minutes over 24h),
    // driven 8-wide to mix the writers.
    const AGENTS: usize = 8;
    const PER_AGENT: usize = 60;

    // **A warm-up kept for its second job** (J3-R2-1 originally; J3
    // redesign since). It was a precondition when a probe-era lane was
    // capped at four and an 8-wide burst would be refused until observation
    // took over; under the static fair-share bound (64) nothing refuses
    // this day either way. It stays because it also pins the flip: the
    // assertion below requires `bound_source == "observed"` before the
    // burst, which is the one-way probe→observed transition doing its
    // telemetry job.
    const WARM: usize = crate::writeq::OBSERVED_MIN_SAMPLES as usize;
    for i in 0..WARM {
        let ack = call(
            &s,
            "lambo_derive",
            json!({
                "agent_id": "agent-a",
                "concepts": [{
                    "content": format!("warm the observed rate {i}"),
                    "concept_type": "observation",
                }],
            }),
        )
        .await;
        let receipt: crate::writeq::ReceiptId = ack.structured_content.as_ref().expect("payload")
            ["receipt"]
            .as_str()
            .expect("an admitted derive carries a receipt")
            .parse()
            .expect("id");
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
    }
    assert_eq!(
        s.mem
            .pipeline()
            .calibration()
            .expect("the probe has landed")
            .source
            .tag(),
        "observed",
        "the day below is admitted against an OBSERVED bound, not the probe's estimate"
    );

    let mut handles = Vec::new();
    for a in 0..AGENTS {
        let s = s.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..PER_AGENT {
                call(
                    &s,
                    "lambo_derive",
                    json!({
                        "agent_id": "agent-a",
                        "concepts": [{
                            "content": format!("day concept {a}-{i}"),
                            "concept_type": "observation",
                        }],
                    }),
                )
                .await;
            }
        }));
    }
    for h in handles {
        h.await.expect("agent task");
    }

    let total = (AGENTS * PER_AGENT + WARM) as u64;
    let lines = read_ledger(&ledger, total);
    assert_eq!(lines.len() as u64, total, "one line per call, none torn");
    for line in &lines {
        assert_eq!(line["kind"], json!("call"));
        assert_eq!(line["tool"], json!("lambo_derive"));
        // J3: the facts a derive line can carry at ack time. `created` /
        // `matched` moved to the receipt (see `derive_impl`'s I1 note), so
        // asserting them here would assert the pre-J3 shape.
        assert!(
            line["concepts_requested"].is_u64() && line["admitted"] == json!(true),
            "{line}"
        );
        assert!(line["receipt"].is_string(), "{line}");
    }
    assert_eq!(ledger.counters().dropped(), 0, "no drops at this rate");

    ledger.shutdown();
    s.mem.close().await.expect("close");
}

/// **I1.** Ledger concept text is bounded, and cut on a char boundary —
/// a byte slice through a multi-byte codepoint would panic, and the ledger
/// is never allowed to panic a tool call.
#[test]
fn i1_ledger_content_is_truncated_on_a_char_boundary() {
    let short = "a canonical decision";
    assert_eq!(truncate_for_ledger(short), short, "short text is untouched");

    // Exactly at the boundary: still untouched.
    let exact: String = "x".repeat(LEDGER_CONTENT_PREFIX);
    assert_eq!(truncate_for_ledger(&exact), exact);

    // One over: cut, and the cut is announced.
    let over: String = "x".repeat(LEDGER_CONTENT_PREFIX + 1);
    let cut = truncate_for_ledger(&over);
    assert!(cut.ends_with("…[truncated]"), "{cut}");
    assert_eq!(
        cut.chars().count(),
        LEDGER_CONTENT_PREFIX + "…[truncated]".chars().count()
    );

    // Multi-byte all the way through: no panic, and the result is valid
    // UTF-8 by construction (it is a `String`).
    let multibyte: String = "é".repeat(LEDGER_CONTENT_PREFIX * 2);
    let cut = truncate_for_ledger(&multibyte);
    assert!(cut.starts_with('é'));
    assert!(cut.ends_with("…[truncated]"));
    // And it survives a JSON round-trip, which is the only thing the ledger
    // actually does with it.
    let v = json!({"content": cut});
    let back: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&v).expect("encode")).expect("decode");
    assert_eq!(back["content"], v["content"]);
}

/// **I-R1-12.** The recall `query` is bounded too.
///
/// It was the one client string on a recall line that went in whole: bounded
/// only by `check_size`'s 16 KiB, so a real 15.4 KiB query produced a
/// 15,752-byte line — ten times what the hit budget allows for all
/// `MAX_TOP_K` hits together. Cut at a wider cap than concept text, because a
/// query is the input under study and the reports print it verbatim.
#[test]
fn i1_the_recall_query_is_truncated_at_its_own_wider_cap() {
    // The two caps' relative order is pinned at compile time beside the
    // constants themselves, not here — a runtime assert on two consts is one
    // clippy refuses, and rightly.
    let short = "how do we paginate list endpoints";
    assert_eq!(truncate_to(short, LEDGER_QUERY_PREFIX), short);

    let exact: String = "q".repeat(LEDGER_QUERY_PREFIX);
    assert_eq!(truncate_to(&exact, LEDGER_QUERY_PREFIX), exact);

    // A query at `check_size`'s ceiling: cut, announced, and bounded.
    let huge: String = "q".repeat(16 * 1024);
    let cut = truncate_to(&huge, LEDGER_QUERY_PREFIX);
    assert!(cut.ends_with("…[truncated]"), "the cut is announced");
    assert_eq!(
        cut.chars().count(),
        LEDGER_QUERY_PREFIX + "…[truncated]".chars().count()
    );

    // Multi-byte: a char boundary, never a byte one.
    let multibyte: String = "é".repeat(LEDGER_QUERY_PREFIX * 2);
    let cut = truncate_to(&multibyte, LEDGER_QUERY_PREFIX);
    assert!(cut.starts_with('é') && cut.ends_with("…[truncated]"));

    // And it is the truncated form that reaches the line.
    let facts = recall_facts(
        &huge,
        8,
        &crate::recall::detail::DetailedRecall::warn_only(String::new()),
    );
    let query = facts["query"].as_str().expect("query is a string");
    assert!(query.ends_with("…[truncated]"), "{}", &query[..40]);
    assert_eq!(query.chars().count(), cut.chars().count());
}

/// **I2 acceptance.** A heartbeat line carries the stats payload, uptime,
/// the crate version and a `git_sha` field.
///
/// The interval itself is `crate::mcp::serve`'s (tested there); this pins
/// the line's contents, which is what the analysis kit's time axis reads.
#[tokio::test]
async fn i2_heartbeat_lines_carry_the_stats_payload_the_version_and_the_sha() {
    let dir = ledger_dir("heartbeat");
    let path = dir.join("calls.jsonl");
    let s = server_with_ledger("i2-heartbeat", &path).await;
    let ledger = Arc::clone(s.ledger().expect("ledger attached"));

    for _ in 0..3 {
        ledger.append(&s.heartbeat_line());
    }
    let lines = read_ledger(&ledger, 3);
    for line in &lines {
        assert_eq!(line["kind"], json!("stats"), "{line}");
        assert_eq!(line["v"], json!(crate::ledger::LINE_VERSION), "{line}");
        assert!(line["uptime_secs"].is_u64(), "{line}");
        assert_eq!(
            line["version"],
            json!(env!("CARGO_PKG_VERSION")),
            "the heartbeat names the crate version: {line}"
        );
        let sha = line["git_sha"].as_str().expect("git_sha is a string");
        assert!(
            !sha.is_empty(),
            "git_sha is always present — 'unknown' when LAMBO_GIT_SHA was unset at build \
                 time, never absent: {line}"
        );
        // The stats payload, and the ledger counters with it.
        assert_eq!(line["stats"]["session"], json!("i2-heartbeat"), "{line}");
        assert!(line["stats"]["node_count"].is_u64(), "{line}");
        assert!(line["stats"]["ledger_dropped_lines"].is_u64(), "{line}");
    }

    ledger.shutdown();
    s.mem.close().await.expect("close");
}

/// #32 decision 15: a server's call lines name its session, so one ledger
/// file can carry several sessions once a serve hosts more than one.
///
/// Mutation: drop the session scoping in `LamboServer::with_ledger` → red.
#[tokio::test]
async fn i32_every_call_line_names_the_servers_session() {
    let dir = ledger_dir("i32-session");
    let path = dir.join("calls.jsonl");
    let s = server_with_ledger("i32-ledger-session", &path).await;
    let ledger = Arc::clone(s.ledger().expect("ledger attached"));
    call(&s, "lambo_stats", json!({"agent_id": "agent-a"})).await;
    call(
        &s,
        "lambo_recall",
        json!({"agent_id": "agent-a", "query": "session"}),
    )
    .await;
    let lines = read_ledger(&ledger, 2);
    assert_eq!(lines.len(), 2);
    for line in &lines {
        assert_eq!(line["kind"], json!("call"), "{line}");
        assert_eq!(line["session"], json!("i32-ledger-session"), "{line}");
    }
    ledger.shutdown();
    s.mem.close().await.expect("close");
}

/// #32 decision 15, the write side: a `Memory` built with a ledger books its
/// `completion` lines under its own session, even when the handle it was given
/// is unscoped (a library caller that is not `serve`).
///
/// Mutation: pass `self.ledger` to the write pipeline unscoped in
/// `MemoryBuilder::build` → red.
#[tokio::test]
async fn i32_completion_lines_name_the_memorys_session() {
    let dir = ledger_dir("i32-completion");
    let path = dir.join("calls.jsonl");
    let ledger = Ledger::open(&path);
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = Memory::builder()
        .session("i32-completion-session")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        })
        .ledger(Some(Arc::clone(&ledger)))
        .build()
        .await
        .expect("build");
    // No `with_ledger`: only the write pipeline appends to this file.
    let s = LamboServer::new(Arc::new(mem));
    let receipt = receipt_payload(
        &s,
        "agent-a",
        json!([{"content": "completion lines name their session", "concept_type": "logic"}]),
    )
    .await;
    assert_eq!(receipt["state"], json!("applied"), "{receipt}");
    let lines = read_ledger(&ledger, 1);
    let completion = lines
        .iter()
        .find(|l| l["kind"] == json!("completion"))
        .unwrap_or_else(|| panic!("a completion line: {lines:?}"));
    assert_eq!(
        completion["session"],
        json!("i32-completion-session"),
        "{completion}"
    );
    ledger.shutdown();
    s.mem.close().await.expect("close");
}

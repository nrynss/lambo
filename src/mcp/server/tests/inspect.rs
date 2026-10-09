//! lambo_inspect: focus resolution, bounded scans, near matches and
//! failure recording.

use super::*;
use crate::surface::focus::{MAX_INSPECT_BOUNDED_SCAN, MAX_INSPECT_SCAN_CONCEPTS};

#[tokio::test]
async fn inspect_finds_a_concept_and_reports_a_miss() {
    let s = server("mcp-inspect").await;
    call(
        &s,
        "lambo_derive",
        serde_json::json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "auth middleware", "concept_type": "entity"}]
        }),
    )
    .await;

    let hit = call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "auth middleware"}),
    )
    .await;
    assert_eq!(hit.is_error, Some(false), "{hit:?}");
    let view = hit.structured_content.unwrap()["view"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(view.contains("auth middleware"), "{view}");

    let miss = call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "no such concept at all"}),
    )
    .await;
    assert_eq!(
        miss.is_error,
        Some(true),
        "a miss is a tool error the caller can read"
    );
    s.mem.close().await.expect("close");
}

/// **R1/T82-7 pinned.** An ambiguous focus is refused with the candidates
/// named, rather than resolved to an arbitrary one of them whose `node_id`
/// then flows into `lambo_reserve` and into edits.
#[tokio::test]
async fn inspect_refuses_an_ambiguous_focus_and_names_the_candidates() {
    let s = server("mcp-inspect-ambiguous").await;
    call(
        &s,
        "lambo_derive",
        serde_json::json!({
            "agent_id": "agent-a",
            "concepts": [
                {"content": "auth middleware", "concept_type": "entity"},
                {"content": "auth middleware rewrite", "concept_type": "entity"},
                {"content": "legacy auth middleware shim", "concept_type": "entity"}
            ]
        }),
    )
    .await;

    let out = call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "auth"}),
    )
    .await;
    assert_eq!(out.is_error, Some(true), "{out:?}");
    let text = text_of(&out);
    for expected in [
        "auth middleware",
        "auth middleware rewrite",
        "legacy auth middleware shim",
    ] {
        assert!(
            text.contains(expected),
            "candidate {expected} missing: {text}"
        );
    }

    // An exact name still resolves, and says nothing about resolution.
    let exact = call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "auth middleware"}),
    )
    .await;
    assert_eq!(exact.is_error, Some(false), "{exact:?}");
    assert!(
        !text_of(&exact).contains("resolved '"),
        "{}",
        text_of(&exact)
    );
    s.mem.close().await.expect("close");
}

/// Issue #30 over the tool surface: a resolved inspect counts its focus
/// (not the neighbourhood), a refused one counts nothing, and a recall
/// counts exactly the hits its structured payload returned.
///
/// Deterministic about who applies the counts: a one-hour daemon tick and
/// a settled daemon after the derive mean no cycle runs during the reads
/// (reads never wake it), the premise is asserted, and `close` is what
/// applies them. It used to race the 1 s default tick, so it caught a
/// broken close only when the tick happened to lose.
#[tokio::test]
async fn inspect_focus_and_recall_hits_are_counted_as_accesses() {
    let s = server_with_config(
        "mcp-issue-30",
        Config {
            daemon_tick_interval: Duration::from_secs(3_600),
            ..Config::default()
        },
    )
    .await;
    call(
        &s,
        "lambo_derive",
        serde_json::json!({
            "agent_id": "agent-a",
            "concepts": [
                {"content": "auth middleware", "concept_type": "entity"},
                {"content": "auth middleware rewrite", "concept_type": "entity"},
                {"content": "session token store", "concept_type": "entity"}
            ],
            "parent_of": [{"parent": "auth middleware", "child": "session token store"}]
        }),
    )
    .await;
    s.mem.settle_daemon().await;

    // Exact focus, depth 2: the child is in the neighbourhood.
    let exact = call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "auth middleware"}),
    )
    .await;
    assert_eq!(exact.is_error, Some(false), "{exact:?}");
    assert!(text_of(&exact).contains("session token store"));
    // Ambiguous: refused, nothing returned, nothing counted.
    let refused = call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "auth"}),
    )
    .await;
    assert_eq!(refused.is_error, Some(true), "{refused:?}");

    let recall = call(
        &s,
        "lambo_recall",
        serde_json::json!({"agent_id": "agent-a", "query": "auth middleware rewrite"}),
    )
    .await;
    assert_eq!(recall.is_error, Some(false), "{recall:?}");
    let returned: std::collections::HashSet<String> = recall.structured_content.as_ref().unwrap()
        ["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["content"].as_str().unwrap().to_string())
        .collect();
    assert!(!returned.is_empty());

    // Premise: every count is still in the ledger, so `close` applies it.
    assert!(s.mem.unapplied_accesses() > 0);
    assert!(s.mem.graph().read().concepts().all(|c| c.access_count == 0));
    s.mem.close().await.expect("close");
    let g = s.mem.graph().read();
    for c in g.concepts() {
        let expected =
            i32::from(c.content == "auth middleware") + i32::from(returned.contains(&c.content));
        assert_eq!(
            c.access_count, expected,
            "{}: inspect focus + recall hits only",
            c.content
        );
        assert_eq!(c.last_accessed.is_some(), expected > 0, "{}", c.content);
    }
}

/// A single substring match is usable — but the caller is told, in the
/// text, that it was not what they literally asked for.
#[tokio::test]
async fn inspect_reports_a_fuzzy_resolution_in_the_text() {
    let s = server("mcp-inspect-fuzzy").await;
    call(
        &s,
        "lambo_derive",
        serde_json::json!({
            "agent_id": "agent-a",
            "concepts": [{"content": "postgres connection pool", "concept_type": "entity"}]
        }),
    )
    .await;
    let out = call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "connection"}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    let text = text_of(&out);
    assert!(
        text.contains("resolved 'connection' → 'postgres connection pool'"),
        "a fuzzy match must announce itself in the text: {text}"
    );
    s.mem.close().await.expect("close");
}

/// Resolution must be a function of the graph's contents, not of hash
/// iteration order: same graph, same answer, every time.
#[tokio::test]
async fn focus_resolution_is_deterministic() {
    let s = server("mcp-inspect-determinism").await;
    call(
        &s,
        "lambo_derive",
        serde_json::json!({
            "agent_id": "agent-a",
            "concepts": [
                {"content": "queue worker", "concept_type": "entity"},
                {"content": "queue worker retry", "concept_type": "entity"},
                {"content": "dead letter queue worker", "concept_type": "entity"}
            ]
        }),
    )
    .await;
    let first = {
        let g = s.mem.graph().read();
        format!("{:?}", resolve_focus(&g, "queue"))
    };
    for _ in 0..20 {
        let g = s.mem.graph().read();
        assert_eq!(
            format!("{:?}", resolve_focus(&g, "queue")),
            first,
            "focus resolution must not depend on HashMap iteration order"
        );
    }
    s.mem.close().await.expect("close");
}

/// Issue #9: a Missing focus does what Ambiguous does: refuse, explain,
/// and offer the closest concepts WITH their node ids, announced as
/// suggestions, never a silent match.
#[tokio::test]
async fn inspect_missing_suggests_near_matches_with_their_node_ids() {
    let s = server("mcp-inspect-near-matches").await;
    call(
        &s,
        "lambo_derive",
        serde_json::json!({
            "agent_id": "agent-a",
            "concepts": [
                {"content": "vector storage codec", "concept_type": "entity"},
                {"content": "vector clock ordering", "concept_type": "entity"}
            ]
        }),
    )
    .await;
    let codec_id = {
        let g = s.mem.graph().read();
        g.concepts()
            .find(|c| c.content == "vector storage codec")
            .map(|c| c.id.0.to_string())
            .expect("the derived concept")
    };

    let out = call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "vector codec"}),
    )
    .await;
    assert_eq!(out.is_error, Some(true), "{out:?}");
    let text = text_of(&out);
    assert!(
        text.contains("no concept matching 'vector codec'"),
        "the refusal must still say no: {text}"
    );
    assert!(
        text.contains("suggestions"),
        "the suggestions must announce themselves as suggestions: {text}"
    );
    assert!(
        text.contains("vector storage codec"),
        "the closest concept by token overlap must be offered: {text}"
    );
    assert!(
        text.contains(&codec_id),
        "the suggestion must carry its node id ({codec_id}): {text}"
    );

    // A focus sharing no token with any concept offers nothing: the
    // refusal stays bare rather than inventing a suggestion.
    let bare = call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "qqq zzz"}),
    )
    .await;
    assert_eq!(bare.is_error, Some(true), "{bare:?}");
    assert!(
        !text_of(&bare).contains("suggestions"),
        "{}",
        text_of(&bare)
    );
    s.mem.close().await.expect("close");
}

/// Pad the graph past [`MAX_INSPECT_SCAN_CONCEPTS`] with staggered
/// creation times: the recency side of the bounded subset is then exactly
/// the last concepts created. `pad concept {k}` foci are unique per k, so
/// a fuzzy focus names exactly one pad.
async fn pad_graph_past_inspect_cap(s: &LamboServer, count: usize) {
    use crate::types::{Concept, Interaction};
    let mut g = s.mem.graph().write();
    let origin = NodeId::new();
    g.insert_interaction(Interaction {
        event_time: None,
        id: origin,
        session_id: s.mem.session().clone(),
        agent_id: AgentId::from("agent-a"),
        prompt_text: None,
        previous_id: None,
        created_at: chrono::Utc::now(),
    })
    .unwrap();
    let base = chrono::TimeZone::timestamp_opt(&chrono::Utc, 1_752_000_000, 0).unwrap();
    for k in 0..count {
        let c = Concept {
            id: NodeId::new(),
            session_id: s.mem.session().clone(),
            content: format!("pad concept {k}"),
            canonical_key: format!("pad concept {k}"),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from("agent-a"),
            created_at: base + chrono::Duration::minutes(k as i64),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: crate::types::CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        };
        g.insert_concept(c, origin).unwrap();
    }
}

/// Issue #9: past the scan cap the fuzzy leg scans the bounded subset and
/// SAYS so in the text: a match inside the subset announces the bound; a
/// match outside it fails honestly instead of pretending no concept
/// exists. The graph is 2,001 concepts: the cap the dogfood rig crosses.
#[tokio::test]
async fn past_the_cap_inspect_says_what_the_bounded_scan_scanned() {
    let s = server("mcp-inspect-bounded").await;
    pad_graph_past_inspect_cap(&s, MAX_INSPECT_SCAN_CONCEPTS + 1).await;

    // Inside the recency side of the subset: resolves fuzzily and
    // announces the bounded scan.
    let inside = call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "ad concept 2000"}),
    )
    .await;
    assert_eq!(inside.is_error, Some(false), "{inside:?}");
    let text = text_of(&inside);
    assert!(
        text.contains("bounded scan of"),
        "a bounded resolution must say so: {text}"
    );
    assert!(
        text.contains(&format!(
            "past the {MAX_INSPECT_SCAN_CONCEPTS}-concept full-scan cap"
        )),
        "the bounded note must name the cap: {text}"
    );
    assert!(
        text.contains("pad concept 2000"),
        "the matched concept must render: {text}"
    );

    // Outside both sides (old, no blast radius): an honest failure that
    // still names the bound. "ad concept 5" substring-matches exactly one
    // pad, so a full scan would have resolved it. The miss still carries
    // the near-match remediation, subset-scoped and announced.
    let outside = call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "ad concept 5"}),
    )
    .await;
    assert_eq!(outside.is_error, Some(true), "{outside:?}");
    let text = text_of(&outside);
    assert!(
        text.contains("bounded subset"),
        "the refusal must say the scan was bounded: {text}"
    );
    assert!(
        text.contains(&format!("{MAX_INSPECT_BOUNDED_SCAN} most recently created")),
        "the refusal must name the subset composition: {text}"
    );
    assert!(
        text.contains("nearest within the bounded subset"),
        "a past-cap miss must offer subset-scoped suggestions: {text}"
    );
    assert!(
        text.contains("pad concept 2000 ["),
        "the newest subset concept must be suggested with its node id: {text}"
    );
    s.mem.close().await.expect("close");
}

/// Issue #9: every inspect failure records its failure mode and its focus
/// in the ledger facts before the error returns, so the rig's telemetry
/// can classify Missing / Ambiguous / Oversized without reprobing. The
/// focus is truncated with an explicit marker. Past the cap a no-match
/// focus is Oversized by design (the bounded scan ran and found nothing),
/// so the Oversized case runs on its own padded graph.
#[tokio::test]
async fn failed_inspects_record_their_failure_mode_and_focus_in_the_ledger() {
    // Within the cap: Missing and Ambiguous are reachable.
    let dir = ledger_dir("inspect-failures");
    let path = dir.join("calls.jsonl");
    let s = server_with_ledger("i1-inspect-failures", &path).await;
    let ledger = Arc::clone(s.ledger().expect("ledger attached"));
    call(
        &s,
        "lambo_derive",
        serde_json::json!({
            "agent_id": "agent-a",
            "concepts": [
                {"content": "auth middleware", "concept_type": "entity"},
                {"content": "auth middleware rewrite", "concept_type": "entity"}
            ]
        }),
    )
    .await;

    // Missing: shares no token with any concept.
    call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "qqq zzz"}),
    )
    .await;
    // Ambiguous: "middleware" substring-matches both concepts exactly.
    call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "middleware"}),
    )
    .await;
    // The focus is truncated to LEDGER_FOCUS_PREFIX characters with an
    // explicit marker: 250 chars in, 200 + marker out.
    call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "x".repeat(250)}),
    )
    .await;

    // Four lines: the derive that seeded the concepts, then the three
    // failed inspects.
    let lines = read_ledger(&ledger, 4);
    let by_focus = |needle: &str| {
        lines
            .iter()
            .find(|l| l["tool"] == json!("lambo_inspect") && l["focus"] == json!(needle))
            .unwrap_or_else(|| panic!("no ledger line with focus {needle}: {lines:?}"))
    };
    let missing = by_focus("qqq zzz");
    assert_eq!(missing["outcome"], json!("error"));
    assert_eq!(missing["failure"], json!("missing"));

    let ambiguous = by_focus("middleware");
    assert_eq!(ambiguous["failure"], json!("ambiguous"));

    let truncated = lines
        .iter()
        .find(|l| {
            l["tool"] == json!("lambo_inspect")
                && l["focus"]
                    .as_str()
                    .is_some_and(|f| f.ends_with("[truncated]"))
        })
        .and_then(|l| l["focus"].as_str())
        .expect("the 250-char focus must be truncated with the explicit marker");
    assert!(
        truncated.starts_with(&"x".repeat(LEDGER_FOCUS_PREFIX)),
        "the cut keeps the first {LEDGER_FOCUS_PREFIX} chars: {truncated}"
    );
    s.mem.close().await.expect("close");

    // Past the cap: the no-match focus books as Oversized, because the
    // bounded subset scan is what ran and found nothing.
    let dir = ledger_dir("inspect-failures-oversized");
    let path = dir.join("calls.jsonl");
    let s = server_with_ledger("i1-inspect-failures-oversized", &path).await;
    let ledger = Arc::clone(s.ledger().expect("ledger attached"));
    pad_graph_past_inspect_cap(&s, MAX_INSPECT_SCAN_CONCEPTS + 1).await;
    call(
        &s,
        "lambo_inspect",
        serde_json::json!({"agent_id": "agent-a", "focus": "ad concept 5"}),
    )
    .await;
    let lines = read_ledger(&ledger, 1);
    assert_eq!(lines[0]["failure"], json!("oversized"));
    assert_eq!(lines[0]["focus"], json!("ad concept 5"));
    s.mem.close().await.expect("close");
}

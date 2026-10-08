//! /api/inspect: focus, gates, cooldowns and bounds.

use super::*;

/// A `GraphStore` that counts the two gate-only reads while delegating
/// everything else. H2's query-count regression wraps a seeded store in
/// this to prove a Canonical hit never reaches `blast_radius` /
/// `interaction_span` — JSON omission alone would be vacuous.
struct Counting {
    inner: Shared,
    blast_radius_calls: Arc<AtomicUsize>,
    interaction_span_calls: Arc<AtomicUsize>,
    /// When set, the two gate-only queries fail. There is no other way to
    /// drive `/api/inspect`'s `Err` arm from a test — `MemoryStore` cannot
    /// be made to fail either query — and that arm is now a *labelled*
    /// absence, so it needs a pin of its own.
    fail_gate_queries: bool,
}

#[async_trait]
impl GraphStore for Counting {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.inner.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn vector_dimensions(&self) -> Option<usize> {
        self.inner.vector_dimensions()
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
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
        self.blast_radius_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_gate_queries {
            return Err(StoreError::Backend("injected gate-query failure".into()));
        }
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
        self.interaction_span_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_gate_queries {
            return Err(StoreError::Backend("injected gate-query failure".into()));
        }
        self.inner
            .interaction_span(session, node, min_age, now)
            .await
    }
    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.inner.record_canonization(event, token).await
    }
    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::lease::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
        self.inner.acquire_lease(session, holder, ttl).await
    }
    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::lease::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
        self.inner.refresh_lease(session, holder, ttl).await
    }
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::lease::LeaseHolder,
    ) -> Result<(), StoreError> {
        self.inner.release_lease(session, holder).await
    }
    async fn write_flush_stats(
        &self,
        session: &SessionId,
        stats: &crate::store::SessionFlushStats,
    ) -> Result<(), StoreError> {
        self.inner.write_flush_stats(session, stats).await
    }
    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<crate::store::SessionFlushStats>, StoreError> {
        self.inner.read_flush_stats(session).await
    }
}

// ---- /api/inspect ---------------------------------------------------

/// Focus on a real concept: the page gets its status, its blast radius
/// and its structural dependents (never a `CoOccurrence` edge). H2: the
/// T11 gate block is NOT part of a Canonical hit — a promoted fact has no
/// promotion left to explain.
#[tokio::test]
async fn inspect_endpoint_reports_a_focus_and_its_structural_dependents() {
    let store = seed("t93-inspect").await;
    let (addr, handle) = spawn(state_on(store, "t93-inspect")).await;

    let hit = get_json(addr, "/api/inspect?focus=user%20schema").await;
    assert_eq!(hit["found"], true, "{hit}");
    assert_eq!(hit["status"], "Canonical", "{hit}");
    assert!(hit["blast_radius"].as_u64().is_some(), "{hit}");
    assert_eq!(hit["truncated"], false, "{hit}");

    // Structural edges only: the false CoOccurrence edge (T7) must never
    // appear on the page.
    let deps = hit["dependents"].as_array().expect("dependents");
    for dep in deps {
        let edge = dep["edge"].as_str().expect("edge");
        assert!(
            matches!(edge, "Dependency" | "Causal" | "Hierarchical"),
            "non-structural edge '{edge}' leaked onto the inspect page: {hit}"
        );
        assert!(dep["content"].as_str().is_some(), "{hit}");
        assert!(dep["concept_type"].as_str().is_some(), "{hit}");
    }

    // H2: the serialized Canonical object must NOT carry a gate_progress
    // key at all. `get()` proves key absence on the wire JSON — indexing
    // would not, since a `null` also reads as "absent" while a real key
    // breaks the contract that no Canonical payload pairs its status with
    // gate figures saying it does not qualify.
    assert!(hit.get("gate_progress").is_none(), "{hit}");
    // P3-7: and the absence says WHY. `gate_progress: null` used to mean
    // three unrelated things at once (Canonical, `Solo`, failed store
    // read); the `Solo` cause is gone and the other two are now named, so a
    // client never renders "no gates" for a broken query and a promoted
    // fact identically.
    assert_eq!(hit["gate_progress_omitted"], "already_canonical", "{hit}");
    assert_eq!(hit["promotion_policy"], "Swarm", "{hit}");

    handle.abort();
}

/// H2: Candidate, Venerable and status-None hits keep the T11 gate block —
/// the same shape and the same thresholds — because their promotion is
/// still in question. Only a Canonical hit drops it.
#[tokio::test]
async fn inspect_keeps_the_gate_block_for_every_non_canonical_status() {
    let store = Arc::new(MemoryStore::new());
    let sid = SessionId::new("t93-inspect-gate-statuses");
    let iid = NodeId::new();
    let now = Utc::now();
    let mut batch = MutationBatch::new();
    batch.push(Mutation::UpsertNode {
        node: Node::Interaction(Interaction {
            event_time: None,
            id: iid,
            session_id: sid.clone(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("gate fixture".to_string()),
            previous_id: None,
            created_at: now,
        }),
    });
    for (content, status) in [
        ("candidate concept", CanonizationStatus::Candidate),
        ("venerable concept", CanonizationStatus::Venerable),
        ("plain concept", CanonizationStatus::None),
    ] {
        let cid = NodeId::new();
        let mut c = concept(sid.clone(), cid, iid, content, now);
        c.canonization_status = status;
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(c),
        });
        // §5.7: every concept must have a Derives edge from an interaction.
        batch.push(Mutation::UpsertEdge {
            edge: edge(NodeId::new(), sid.clone(), iid, cid, EdgeType::Derives, now),
        });
    }
    store.flush(&batch, None).await.expect("seed gate statuses");
    let (addr, handle) = spawn(state_on(store, "t93-inspect-gate-statuses")).await;

    for (focus, status) in [
        ("candidate%20concept", "Candidate"),
        ("venerable%20concept", "Venerable"),
        ("plain%20concept", "None"),
    ] {
        let hit = get_json(addr, &format!("/api/inspect?focus={focus}")).await;
        assert_eq!(hit["status"], status, "{hit}");
        // Same gate shape and thresholds the T11 payload always shipped:
        // gc survival >= 3, blast radius strictly > 5, distinct
        // interactions >= 3, coverage >= 0.3.
        let gp = &hit["gate_progress"];
        assert_eq!(gp["gc_survived"]["bar"], 3.0, "{hit}");
        assert_eq!(gp["blast_radius"]["bar"], 5.0, "{hit}");
        assert!(gp["blast_radius"]["strictly_above"] == true, "{hit}");
        assert_eq!(gp["distinct_interactions"]["bar"], 3.0, "{hit}");
        assert_eq!(gp["coverage"]["bar"], 0.3, "{hit}");
        assert_eq!(gp["in_cooldown"], false, "{hit}");
    }
    handle.abort();
}

/// **P2-e / C2.** Under `promotion_policy = "Solo"` the four gates decide
/// nothing, so `/api/inspect` must not ship them — and must not run the two
/// store queries that exist only to build them. What it must *keep*
/// shipping is the block itself: the policy label and the cooldown.
///
/// The lie this closes: `gate_progress`'s four numbers are `gc_survived`,
/// blast radius, distinct interactions and coverage, and `SoloScorer` reads
/// none of them (`admits_hop` ignores the stage evidence argument
/// entirely). A `Solo` deployment's page therefore showed "0 of 4 gates
/// met" beside a concept that went Canonical on the next cycle.
///
/// The second lie, which whole-struct suppression introduced while closing
/// the first: `GateProgress` is also the **only** carrier of
/// `in_cooldown` / `cooldown_until`, and the re-promotion cooldown is
/// policy-independent — `canon::eval` applies it to the
/// Venerable → Canonical hop exactly when the stage-3 evidence is absent,
/// which is every score-admitted solo hop. Returning nothing therefore
/// replaced four irrelevant numbers with zero relevant ones. Pinned
/// directly below by `inspect_reports_the_repromotion_cooldown_under_solo`.
///
/// Both policies run, against the same concept and the same store: a
/// `Solo`-only assertion would pass equally well if the four gates had been
/// dropped for everyone. The query counters make the `Solo` half
/// non-vacuous the same way H2's do — an implementation that computed the
/// gates and then discarded them would satisfy key-absence alone.
#[tokio::test]
async fn inspect_drops_only_the_swarm_gates_under_solo_and_keeps_them_under_swarm() {
    use crate::canon::PromotionPolicy;

    const SWARM_GATE_KEYS: [&str; 4] = [
        "gc_survived",
        "blast_radius",
        "distinct_interactions",
        "coverage",
    ];

    for policy in PromotionPolicy::ALL {
        let session = format!("p2e-inspect-{}", policy.as_str().to_ascii_lowercase());
        let seeded = seed(&session).await;
        let blast_calls = Arc::new(AtomicUsize::new(0));
        let span_calls = Arc::new(AtomicUsize::new(0));
        let counted = Counting {
            inner: Shared(seeded),
            blast_radius_calls: blast_calls.clone(),
            interaction_span_calls: span_calls.clone(),
            fail_gate_queries: false,
        };
        let mut backends = backends_with_store(Box::new(counted));
        backends.config.promotion_policy = policy;
        let (addr, handle) = spawn(state_from_backends(backends, &session, None)).await;

        // `auth middleware` is the seed's un-promoted concept: not
        // Canonical, so H2's own omission does not fire and the only thing
        // deciding the gates is the policy.
        let hit = get_json(addr, "/api/inspect?focus=auth%20middleware").await;
        assert_eq!(hit["found"], true, "{hit}");
        assert_ne!(hit["status"], "Canonical", "{hit}");

        // P2-2 / P3-7: the payload names the policy it resolved, so a gate
        // count is attributable and an absence is never a guess. Present at
        // both levels and on every policy.
        assert_eq!(hit["promotion_policy"], policy.as_str(), "{hit}");
        let gp = &hit["gate_progress"];
        assert!(
            !gp.is_null(),
            "the block is shipped under BOTH policies — only its four \
                 swarm gates are policy-dependent: {hit}"
        );
        assert_eq!(gp["policy"], policy.as_str(), "{hit}");
        // The block is present, so nothing is omitted and nothing claims to
        // be. The reason key exists only for a genuine absence.
        assert!(hit.get("gate_progress_omitted").is_none(), "{hit}");
        // Policy-independent, so it is here on both arms.
        assert_eq!(gp["in_cooldown"], false, "{hit}");

        let queries = blast_calls.load(Ordering::SeqCst) + span_calls.load(Ordering::SeqCst);
        match policy {
            PromotionPolicy::Solo => {
                for key in SWARM_GATE_KEYS {
                    assert!(
                        gp.get(key).is_none(),
                        "Solo must not ship `{key}`, a gate it does not read: {hit}"
                    );
                }
                assert_eq!(
                    queries, 0,
                    "Solo must not run the gate-only store queries either"
                );
            }
            PromotionPolicy::Swarm => {
                // Unchanged wire paths: the four gates stay flat on
                // `gate_progress`, where every other consumer already
                // reads them.
                assert_eq!(
                    gp["gc_survived"]["bar"], 3.0,
                    "Swarm's gates are still surfaced unchanged: {hit}"
                );
                for key in SWARM_GATE_KEYS {
                    assert!(gp[key]["bar"].is_number(), "{key} missing: {hit}");
                }
                assert!(
                    queries > 0,
                    "Swarm must still reach blast_radius / interaction_span"
                );
            }
        }
        handle.abort();
    }
}

/// **P2-1.** A `Solo` concept inside the re-promotion cooldown must still
/// report it.
///
/// This is the case whole-struct suppression silently broke, and it is not
/// a corner: under `Solo` a budget-demoted concept (Canonical → `None`,
/// `last_demotion_time` stamped) climbs back to Venerable on the score band
/// alone in two cycles, and then stalls the full 300 s cooldown, because
/// `canon::eval` gates the Venerable → Canonical hop on
/// `in_repromotion_cooldown` precisely when the stage-3 evidence is absent
/// — which is every score-admitted hop. `GateProgress` is the only carrier
/// of `in_cooldown` / `cooldown_until` in the payload, so with the block
/// suppressed the operator got `status: "Venerable"`, no explanation
/// whatsoever, and a page whose `renderGates` returned early — strictly
/// worse than the pre-C2 payload, which at least carried this one true
/// fact among the four bogus gates.
///
/// Deliberately asserted with the four swarm gates absent in the same
/// breath: the point is not that the block exists but that it carries the
/// determinant that is live and drops the four that are not.
#[tokio::test]
async fn inspect_reports_the_repromotion_cooldown_under_solo() {
    use crate::canon::PromotionPolicy;

    let store = Arc::new(MemoryStore::new());
    let sid = SessionId::new("p2-1-solo-cooldown");
    let iid = NodeId::new();
    let cid = NodeId::new();
    let now = Utc::now();
    let mut batch = MutationBatch::new();
    batch.push(Mutation::UpsertNode {
        node: Node::Interaction(Interaction {
            event_time: None,
            id: iid,
            session_id: sid.clone(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("cooling".to_string()),
            previous_id: None,
            created_at: now,
        }),
    });
    // Back at Venerable on the band, demoted 5s ago: inside the default
    // 300s cooldown, and therefore stalled for another 295.
    let mut c = concept(sid.clone(), cid, iid, "cooling", now);
    c.canonization_status = CanonizationStatus::Venerable;
    c.last_demotion_time = Some(now - chrono::Duration::seconds(5));
    batch.push(Mutation::UpsertNode {
        node: Node::Concept(c),
    });
    // §5.7: every concept must have a Derives edge from an interaction.
    batch.push(Mutation::UpsertEdge {
        edge: edge(NodeId::new(), sid.clone(), iid, cid, EdgeType::Derives, now),
    });
    store.flush(&batch, None).await.expect("seed solo cooldown");

    let mut backends = backends_with_store(Box::new(Shared(store)));
    backends.config.promotion_policy = PromotionPolicy::Solo;
    let (addr, handle) = spawn(state_from_backends(backends, "p2-1-solo-cooldown", None)).await;

    let hit = get_json(addr, "/api/inspect?focus=cooling").await;
    assert_eq!(hit["found"], true, "{hit}");
    assert_eq!(hit["status"], "Venerable", "{hit}");
    assert_eq!(hit["promotion_policy"], "Solo", "{hit}");

    let gp = &hit["gate_progress"];
    assert_eq!(gp["policy"], "Solo", "{hit}");
    assert_eq!(
        gp["in_cooldown"], true,
        "the cooldown is the live determinant under Solo and must be \
             reported, not suppressed with the four gates: {hit}"
    );
    assert!(
        gp["cooldown_until"].is_string(),
        "a cooling concept must carry cooldown_until: {hit}"
    );
    // ... and none of swarm's four rode along.
    for key in [
        "gc_survived",
        "blast_radius",
        "distinct_interactions",
        "coverage",
    ] {
        assert!(gp.get(key).is_none(), "Solo must not ship `{key}`: {hit}");
    }
    handle.abort();
}

/// **T3-P3-6 / T3-P3-3.** The page half of the payload above.
///
/// `inspect_reports_the_repromotion_cooldown_under_solo` proves the wire
/// carries `policy: "Solo"`, `in_cooldown: true` and none of the four
/// gates. What it cannot see is what `web/app.js` then draws, and the
/// combination it draws was self-contradictory: the Solo line said "There
/// is nothing to tick off" and the cooldown paragraph directly under it went
/// on to talk about "the checks above: every one of them can be met".
///
/// There is no JS test harness in this repo — no `package.json`, and
/// `node --check` is syntax only — so page behaviour is pinned from here
/// against the embedded source, the way `h1_web_*` already pins the
/// embedding banner. These are properties, not identifiers: what the copy
/// must say, and what it must be *keyed on*, so that the two paragraphs
/// cannot be updated apart again.
#[test]
fn the_page_explains_a_gateless_policy_and_its_cooldown_without_contradicting_itself() {
    // Both paragraphs are decided by `rendered` — the count of gate rows
    // actually drawn — and not by the policy name. That is the fix: two
    // paragraphs about the same block, keyed on the same value.
    assert!(
        APP_JS.contains("heading.textContent = rendered > 0 ? GATES_HEADING_CHECKS"),
        "the heading over the gate group must follow whether a checklist was drawn"
    );
    assert!(
        APP_JS.contains(r#""become Canonical again. " + (rendered > 0"#),
        "the cooldown copy must branch on the same `rendered` the gate rows do, \
             or it will keep describing checks that are not on the page"
    );
    assert!(
        APP_JS.contains("This is separate from the checks above"),
        "with a checklist drawn, the cooldown must still say it is separate from it"
    );
    assert!(
        APP_JS.contains("This is separate from whatever else promotion needs"),
        "with no checklist drawn, the cooldown must not refer to checks above it"
    );

    // The gateless line names the policy rather than leaving an empty
    // checklist under a heading that reads "no checks left to pass".
    assert!(
        APP_JS.contains("Promotion here runs the Solo policy"),
        "the Solo branch must name the policy"
    );
    assert!(
        APP_JS.contains("nothing to tick off."),
        "the Solo branch must say there is no checklist, not show an empty one"
    );
    // The named-policy branch is for a policy this file has no copy for — a
    // variant added after it was written. It must not describe that as a
    // transient failure: nothing is missing and nothing is being retried. A
    // genuinely failed read is the separate `unavailable` path below.
    //
    // R4-2: the policy-less fallback under it is NOT reachable from any
    // server this repo ships — a post-C2 one always carries `policy`, and a
    // pre-C2 one always serializes the four gate keys, so `gateAbsenceCopy`
    // is never called against it. It is a defensive default for a malformed
    // payload, kept so a truncated response reads as a sentence rather than
    // "runs the undefined policy". Asserted for its wording, not as a
    // behaviour this endpoint can produce.
    assert!(
        APP_JS.contains("policy, which does not use the four "),
        "an unrecognised policy must still get an honest line, not the Solo copy"
    );
    assert!(
        !APP_JS.contains("not available right now"),
        "\"not available right now\" describes a retry that never happens"
    );

    // T3-P3-7: the wire distinction between the two absences now reaches
    // the page. `already_canonical` still draws nothing; `unavailable` says
    // the read failed, so a broken store query no longer renders
    // identically to a promoted fact.
    assert!(
        APP_JS.contains(r#"d.gate_progress_omitted === "unavailable""#),
        "the page must act on the omission reason, not just receive it"
    );
    assert!(
        APP_JS.contains("The checks could not be read just now"),
        "an unavailable gate read must be visible as a reading problem"
    );

    // The heading is set from script, so the static one in index.html can
    // only be the checklist wording — which means it needs an id.
    assert!(
        INDEX_HTML.contains(r#"id="details-gates-heading""#),
        "the gate heading must be addressable, or it stays \"Why nothing \
             relies on this yet\" over a block with no checklist"
    );
}

/// **P3-7.** The third meaning `gate_progress: null` used to carry: the
/// store read behind the gates failed.
///
/// It must stay a 200 with the rest of the payload intact — the block is
/// additive and the page loads on this endpoint — but it must no longer be
/// indistinguishable from H2's deliberate omission for a Canonical concept.
/// A client that cannot tell "there are no gates to show" from "we could
/// not read them" renders a broken store as a promoted fact.
#[tokio::test]
async fn inspect_labels_a_failed_gate_read_as_unavailable_and_still_returns_200() {
    let session = "p3-7-gate-read-failure";
    let seeded = seed(session).await;
    let counted = Counting {
        inner: Shared(seeded),
        blast_radius_calls: Arc::new(AtomicUsize::new(0)),
        interaction_span_calls: Arc::new(AtomicUsize::new(0)),
        fail_gate_queries: true,
    };
    let backends = backends_with_store(Box::new(counted));
    let (addr, handle) = spawn(state_from_backends(backends, session, None)).await;

    // Non-Canonical, so H2 does not fire and the only thing that can drop
    // the block is the injected read failure.
    let r = request(addr, "GET", "/api/inspect?focus=auth%20middleware").await;
    assert_eq!(
        r.status, 200,
        "an additive block must not fail the endpoint"
    );
    let hit: serde_json::Value = serde_json::from_str(&r.body).expect("json");
    assert_eq!(hit["found"], true, "{hit}");
    assert_ne!(hit["status"], "Canonical", "{hit}");
    // The rest of the payload is untouched — degradation, not an error.
    assert!(hit["dependents"].as_array().is_some(), "{hit}");
    assert_eq!(hit["promotion_policy"], "Swarm", "{hit}");

    assert!(hit.get("gate_progress").is_none(), "{hit}");
    assert_eq!(hit["gate_progress_omitted"], "unavailable", "{hit}");
    // ... and distinguishable from H2's, which is the whole point.
    assert_ne!(hit["gate_progress_omitted"], "already_canonical", "{hit}");
    handle.abort();
}

/// H2: a Canonical hit must not run either gate-only store query. The
/// counting wrapper proves it on the store surface — key absence in the
/// JSON alone would be vacuous, because an implementation that computed
/// the block and then dropped it would still pass a pure key-absence test.
#[tokio::test]
async fn inspect_canonical_hit_runs_neither_gate_only_store_query() {
    let seeded = seed("t93-inspect-query-count").await;
    let blast_calls = Arc::new(AtomicUsize::new(0));
    let span_calls = Arc::new(AtomicUsize::new(0));
    let counted = Counting {
        inner: Shared(seeded),
        blast_radius_calls: blast_calls.clone(),
        interaction_span_calls: span_calls.clone(),
        fail_gate_queries: false,
    };
    let state = state_from_backends(
        backends_with_store(Box::new(counted)),
        "t93-inspect-query-count",
        None,
    );
    let (addr, handle) = spawn(state).await;

    let hit = get_json(addr, "/api/inspect?focus=user%20schema").await;
    assert_eq!(hit["status"], "Canonical", "{hit}");
    assert!(hit.get("gate_progress").is_none(), "{hit}");
    assert_eq!(
        blast_calls.load(Ordering::SeqCst),
        0,
        "a Canonical hit must not query blast_radius"
    );
    assert_eq!(
        span_calls.load(Ordering::SeqCst),
        0,
        "a Canonical hit must not query interaction_span"
    );

    handle.abort();
}

/// A miss is a `200` with `found: false` — never a non-2xx — and carries
/// the empty dependents shape from the contract.
#[tokio::test]
async fn inspect_endpoint_miss_is_a_200_with_found_false() {
    let store = seed("t93-inspect-miss").await;
    let (addr, handle) = spawn(state_on(store, "t93-inspect-miss")).await;

    let r = request(addr, "GET", "/api/inspect?focus=no-such-concept").await;
    assert_eq!(
        r.status, 200,
        "a miss must be 200, never non-2xx: {}",
        r.body
    );
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["found"], false, "{v}");
    assert_eq!(v["blast_radius"], 0, "{v}");
    assert!(v["dependents"].as_array().unwrap().is_empty(), "{v}");
    // The reader's resolved policy is a property of the PROCESS, so it is
    // present even with no concept to describe — and no omission reason is
    // claimed, because `found: false` already accounts for the absence.
    assert_eq!(v["promotion_policy"], "Swarm", "{v}");
    assert!(v.get("gate_progress_omitted").is_none(), "{v}");
    // status / gate_progress are omitted on a miss (no concept to explain).
    assert!(v["status"].is_null(), "{v}");
    assert!(v["gate_progress"].is_null(), "{v}");

    // A blank focus is the same miss, not an error.
    let blank = request(addr, "GET", "/api/inspect?focus=").await;
    assert_eq!(blank.status, 200, "blank focus must be a 200 miss");

    handle.abort();
}

/// Drive `/api/inspect` past `MAX_INSPECT_NODES` structural dependents:
/// `truncated` is true and the payload pins at the cap. This also pins the
/// T3-R1-3 fix — a non-structural/duplicate incident edge alone must not
/// set `truncated` when the structural list is complete.
#[tokio::test]
async fn inspect_truncates_and_reports_at_the_dependents_bound() {
    let n = MAX_INSPECT_NODES + 1;
    let store = seed_chain_around("t93-inspect-cap", "hub", n).await;
    let (addr, handle) = spawn(state_on(store, "t93-inspect-cap")).await;

    let hit = get_json(addr, "/api/inspect?focus=hub").await;
    assert_eq!(hit["found"], true, "{hit}");
    assert_eq!(hit["truncated"], true, "{hit}");
    assert_eq!(
        hit["dependents"].as_array().expect("dependents").len(),
        MAX_INSPECT_NODES,
        "at the bound the payload must be pinned at the cap: {hit}"
    );
    handle.abort();
}

/// The `depth` query parameter is accepted for CLI parity but deliberately
/// treated as 1: any depth value returns the same hop-1 shape, never
/// rejected (T3-R1-N3).
#[tokio::test]
async fn inspect_ignores_the_depth_parameter() {
    let store = seed("t93-inspect-depth").await;
    let (addr, handle) = spawn(state_on(store, "t93-inspect-depth")).await;

    let d1 = get_json(addr, "/api/inspect?focus=user%20schema&depth=1").await;
    let d3 = get_json(addr, "/api/inspect?focus=user%20schema&depth=3").await;
    assert_eq!(d1["found"], true, "{d1}");
    assert_eq!(d3["found"], true, "{d3}");
    assert_eq!(
        d1["dependents"], d3["dependents"],
        "depth must be ignored (treated as 1), not change the shape: {d1} {d3}"
    );
    handle.abort();
}

/// A concept demoted within the re-promotion cooldown must surface
/// `in_cooldown: true` + `cooldown_until` in its gate progress — the
/// fifth (non-threshold) reason a Venerable that clears all four gates
/// still does not promote (T3-R1-4).
#[tokio::test]
async fn inspect_surfaces_a_cooling_concepts_repromotion_cooldown() {
    let store = Arc::new(MemoryStore::new());
    let sid = SessionId::new("t93-cooldown");
    let iid = NodeId::new();
    let cid = NodeId::new();
    let now = Utc::now();
    let mut batch = MutationBatch::new();
    batch.push(Mutation::UpsertNode {
        node: Node::Interaction(Interaction {
            event_time: None,
            id: iid,
            session_id: sid.clone(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("hot".to_string()),
            previous_id: None,
            created_at: now,
        }),
    });
    // Demoted 5s ago: inside the default 300s cooldown.
    let mut c = concept(sid.clone(), cid, iid, "hot", now);
    c.last_demotion_time = Some(now - chrono::Duration::seconds(5));
    batch.push(Mutation::UpsertNode {
        node: Node::Concept(c),
    });
    // §5.7: every concept must have a Derives edge from an interaction.
    batch.push(Mutation::UpsertEdge {
        edge: edge(NodeId::new(), sid.clone(), iid, cid, EdgeType::Derives, now),
    });
    store.flush(&batch, None).await.expect("seed cooldown");
    let (addr, handle) = spawn(state_on(store, "t93-cooldown")).await;

    let hit = get_json(addr, "/api/inspect?focus=hot").await;
    assert_eq!(hit["found"], true, "{hit}");
    let gp = &hit["gate_progress"];
    assert_eq!(gp["in_cooldown"], true, "{hit}");
    assert!(
        gp["cooldown_until"].is_string(),
        "a cooling concept must carry cooldown_until: {hit}"
    );
    handle.abort();
}

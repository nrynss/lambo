//! Structural and keyword queries, blast radius and interaction spans,
//! against the in-memory store.

use super::*;

#[cfg(feature = "store-memory")]
fn plant_edge(
    sid: &SessionId,
    source: NodeId,
    target: NodeId,
    edge_type: EdgeType,
    ts: DateTime<Utc>,
) -> Mutation {
    Mutation::UpsertEdge {
        edge: Edge {
            event_time: None,
            id: NodeId::new(),
            session_id: sid.clone(),
            source,
            target,
            edge_type,
            weight: 1.0,
            reinforcements: 1,
            created_at: ts,
            last_reinforced: ts,
        },
    }
}

#[cfg(feature = "store-memory")]
#[tokio::test]
async fn keyword_candidates_match_memory_and_guard_inputs() {
    let sqlite = test_store();
    sqlite.init_schema().await.unwrap();
    let memory = MemoryStore::new();

    let sid = SessionId::from("kw");
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    let c2 = NodeId::new();
    let c3 = NodeId::new();
    let ts = Utc::now();
    let batch = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&sid, i1, None, ts),
            plant_concept(&sid, c1, i1, "user schema design", ConceptType::Entity, ts),
            plant_concept(&sid, c2, i1, "API rate limits", ConceptType::Entity, ts),
            // Mixed-case row (cockroach R1 bug class): a raw `contains`
            // on row strings would score this 0.0 for "register"; the SQL
            // predicate lowercases the column, so it must score like
            // MemoryStore's Rust-side lowercase.
            plant_concept(&sid, c3, i1, "Register User", ConceptType::Entity, ts),
        ],
    };
    sqlite.flush(&batch, None).await.unwrap();
    memory.flush(&batch, None).await.unwrap();

    for tokens in [
        vec!["schema".to_string()],
        vec!["user".to_string(), "schema".to_string()],
        vec!["rate".to_string()],
        vec!["api".to_string()],
        vec!["nope".to_string()],
        vec!["  USER  ".to_string()],
        vec!["register".to_string()],
    ] {
        let got = sqlite.keyword_candidates(&sid, &tokens, 10).await.unwrap();
        let want = memory.keyword_candidates(&sid, &tokens, 10).await.unwrap();
        assert_eq!(got, want, "tokens {tokens:?}");
    }

    // Explicit mixed-case lock: "Register User" scores 1.0 for "register"
    // and ranks exactly like MemoryStore.
    let got = sqlite
        .keyword_candidates(&sid, &["register".into()], 10)
        .await
        .unwrap();
    let want = memory
        .keyword_candidates(&sid, &["register".into()], 10)
        .await
        .unwrap();
    assert_eq!(got, want);
    assert_eq!(got.len(), 1, "only the mixed-case concept matches");
    assert_eq!(got[0].item, c3);
    assert_eq!(got[0].score, 1.0, "mixed-case content must score, not 0.0");

    // Empty / whitespace tokens match nothing; limit 0 matches nothing.
    assert!(sqlite
        .keyword_candidates(&sid, &["".into(), "  ".into()], 5)
        .await
        .unwrap()
        .is_empty());
    assert!(sqlite
        .keyword_candidates(&sid, &["schema".into()], 0)
        .await
        .unwrap()
        .is_empty());

    // Missing session errors (MemoryStore parity).
    let err = sqlite
        .keyword_candidates(&SessionId::from("ghost"), &["schema".into()], 5)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::SessionNotFound(_)));
}

/// T3.6 three-way agreement matrix: EVERY node (concepts + interactions)
/// × min-age {0, 3600s} × both queries, each answer asserted EXACTLY
/// equal to MemoryStore's naive computation on the same snapshot.
/// Returns the number of equality assertions performed.
#[cfg(feature = "fixtures")]
async fn assert_structural_agreement_matrix(
    store: &SqliteStore,
    memory: &MemoryStore,
    sid: &SessionId,
    snap: &GraphSnapshot,
) -> usize {
    let node_ids: Vec<NodeId> = snap
        .concepts
        .iter()
        .map(|c| c.id)
        .chain(snap.interactions.iter().map(|i| i.id))
        .collect();
    let ages = [Duration::from_secs(0), Duration::from_secs(3600)];
    let mut assertions = 0;
    for node in &node_ids {
        for age in ages {
            let br = store
                .blast_radius(sid, *node, age, Utc::now())
                .await
                .unwrap();
            let br_want = memory
                .blast_radius(sid, *node, age, Utc::now())
                .await
                .unwrap();
            assert_eq!(br, br_want, "blast_radius {node} age {age:?}");

            let span = store
                .interaction_span(sid, *node, age, Utc::now())
                .await
                .unwrap();
            let span_want = memory
                .interaction_span(sid, *node, age, Utc::now())
                .await
                .unwrap();
            assert_eq!(span, span_want, "interaction_span {node} age {age:?}");
            assertions += 2;
        }
    }
    assertions
}

/// Acceptance (T3.6): the three-way agreement matrix on BOTH fixture
/// graphs — `session-rest-api` (the Canonical hub 1001, the Venerable
/// 1012, the D1–D8 orphans C1013–C1020, the P1/P2 peers C1021/C1022 and
/// C1002–C1007, 22 concepts) and `session-drift` (two interaction chains,
/// 9 concepts) — flushed into the store, every node × min-age {0, 3600s},
/// blast_radius + interaction_span (distinct AND coverage) exactly equal
/// to MemoryStore's answers on the same snapshot.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn structural_queries_agree_with_memory_on_both_fixtures() {
    let mut total_assertions = 0;
    for fixture in ["session-rest-api", "session-drift"] {
        let snap: GraphSnapshot = crate::fixtures::load_snapshot(fixture).unwrap();
        let sid = snap.session_id.clone();

        let batch = snapshot_to_batch(&snap);
        let sqlite = test_store();
        sqlite.init_schema().await.unwrap();
        sqlite.flush(&batch, None).await.unwrap();
        let memory = MemoryStore::new();
        memory.flush(&batch, None).await.unwrap();

        total_assertions += assert_structural_agreement_matrix(&sqlite, &memory, &sid, &snap).await;

        // Deterministic sanity anchors on rest-api (independent of the
        // oracle): eight concepts depend on the Canonical hub 1001
        // exclusively; span = 6 distinct interactions over 25 of 55
        // minutes.
        if fixture == "session-rest-api" {
            let hub: NodeId = snap
                .concepts
                .iter()
                .find(|c| c.id.0.to_string().ends_with("001001"))
                .unwrap()
                .id;
            assert_eq!(
                sqlite
                    .blast_radius(&sid, hub, Duration::from_secs(0), Utc::now())
                    .await
                    .unwrap(),
                8
            );
            let span = sqlite
                .interaction_span(&sid, hub, Duration::from_secs(0), Utc::now())
                .await
                .unwrap();
            assert_eq!(span.distinct, 6);
            assert!((span.coverage - 25.0 / 55.0).abs() < 1e-9, "{span:?}");
        }
    }
    // Matrix dimensions: rest-api 34 nodes (22 concepts + 12
    // interactions), drift 11 nodes (9 + 2); 2 ages; 2 queries each ->
    // 45 nodes × 2 × 2 = 180 equality assertions.
    assert_eq!(total_assertions, 180, "matrix dimensions drifted");
}

/// §4.1 errata probe (T3.6): mirror of MemoryStore's
/// `blast_radius_ignores_provenance_derives_edges` against the SQL
/// adapter. §5.7 requires every concept to carry a `Derives` edge
/// (interaction → concept); if blast_radius counted that inbound edge as
/// "another source", every concept would look non-orphaned and Stage-3
/// blast radius would collapse to ~0. The adapter must ignore provenance
/// (`Derives`/`Temporal`) edges exactly like MemoryStore — never
/// un-orphaning a concept through them.
#[cfg(feature = "store-memory")]
#[tokio::test]
async fn blast_radius_errata_derives_must_not_un_orphan() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("errata-derives");
    let ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
    let i1 = NodeId::new();
    let pillar = NodeId::new();
    let orphan = NodeId::new();
    let alone = NodeId::new();
    let batch = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&sid, i1, None, ts),
            plant_concept(&sid, pillar, i1, "pillar", ConceptType::Entity, ts),
            plant_concept(&sid, orphan, i1, "orphan", ConceptType::Entity, ts),
            plant_concept(&sid, alone, i1, "alone", ConceptType::Entity, ts),
            // pillar -> orphan (Dependency): the only structural inbound.
            plant_edge(&sid, pillar, orphan, EdgeType::Dependency, ts),
            // orphan ALSO has the mandatory §5.7 Derives from its origin
            // interaction — counting it would un-orphan orphan.
            plant_edge(&sid, i1, orphan, EdgeType::Derives, ts),
            // alone has ONLY the Derives provenance: never an orphan of
            // anyone (no structural inbound edge exists at all).
            plant_edge(&sid, i1, alone, EdgeType::Derives, ts),
        ],
    };
    store.flush(&batch, None).await.unwrap();
    let memory = MemoryStore::new();
    memory.flush(&batch, None).await.unwrap();

    for min_age in [Duration::from_secs(0), Duration::from_secs(3600)] {
        let want = memory
            .blast_radius(&sid, pillar, min_age, Utc::now())
            .await
            .unwrap();
        assert_eq!(want, 1, "oracle sanity: Derives must not un-orphan");
        let got = store
            .blast_radius(&sid, pillar, min_age, Utc::now())
            .await
            .unwrap();
        assert_eq!(
            got, want,
            "SQLite must ignore provenance Derives exactly like MemoryStore (min_age {min_age:?})"
        );
    }
}

/// Issue #35: a dependent is reached through a **structural** edge from
/// the focus (`Dependency`/`Causal`/`Hierarchical`), as Postgres/Cockroach
/// (`BLAST_RADIUS_SQL`), MemoryStore and the graph's `blast_radii` define
/// it. SQLite's first `EXISTS` had no `edge_type` filter, so a concept the
/// focus only co-occurs with (`CoOccurrence`), resembles (`Semantic`) or
/// precedes (`Temporal`) counted as a dependent and inflated Stage 3.
#[cfg(feature = "store-memory")]
#[tokio::test]
async fn blast_radius_counts_only_structural_edges_from_the_focus() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("structural-only");
    let ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
    let i1 = NodeId::new();
    let pillar = NodeId::new();
    let dep = NodeId::new();
    let co = NodeId::new();
    let sem = NodeId::new();
    let tmp = NodeId::new();
    let batch = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&sid, i1, None, ts),
            plant_concept(&sid, pillar, i1, "pillar", ConceptType::Entity, ts),
            plant_concept(&sid, dep, i1, "dep", ConceptType::Entity, ts),
            plant_concept(&sid, co, i1, "co", ConceptType::Entity, ts),
            plant_concept(&sid, sem, i1, "sem", ConceptType::Entity, ts),
            plant_concept(&sid, tmp, i1, "tmp", ConceptType::Entity, ts),
            // The one real dependent.
            plant_edge(&sid, pillar, dep, EdgeType::Dependency, ts),
            // Non-structural edges from the focus: none of these targets
            // has any structural inbound edge, so a missing filter on the
            // first EXISTS would count each as an exclusive dependent.
            plant_edge(&sid, pillar, co, EdgeType::CoOccurrence, ts),
            plant_edge(&sid, pillar, sem, EdgeType::Semantic, ts),
            plant_edge(&sid, pillar, tmp, EdgeType::Temporal, ts),
        ],
    };
    store.flush(&batch, None).await.unwrap();
    let memory = MemoryStore::new();
    memory.flush(&batch, None).await.unwrap();

    for min_age in [Duration::from_secs(0), Duration::from_secs(3600)] {
        let want = memory
            .blast_radius(&sid, pillar, min_age, Utc::now())
            .await
            .unwrap();
        assert_eq!(want, 1, "oracle sanity: only the Dependency target counts");
        let got = store
            .blast_radius(&sid, pillar, min_age, Utc::now())
            .await
            .unwrap();
        assert_eq!(
            got, want,
            "SQLite must count structural dependents only, like MemoryStore (min_age {min_age:?})"
        );
    }
}

/// R2-3: `blast_radius` must be session-scoped on the **source** side of
/// both structural subqueries, exactly as Cockroach's `BLAST_RADIUS_SQL`
/// is and as MemoryStore is by construction (it walks one session's
/// snapshot, and its `concept_ids` set holds that session's concepts
/// only).
///
/// `hub -> dep` in session `here` makes `dep` an exclusive dependent, so
/// blast is 1. A second structural edge into `dep` whose **source concept
/// lives in another session** must not un-orphan it: MemoryStore skips
/// that source (not in `concept_ids`), and SQLite used to join `concepts`
/// with no session predicate, satisfy the `NOT EXISTS` arm, and answer 0
/// — under-counting blast, which suppresses Stage-3 promotions and
/// mis-ranks budget demotion. The session-local answer is the contract on
/// every backend.
#[cfg(feature = "store-memory")]
#[tokio::test]
async fn blast_radius_ignores_cross_session_sources_like_memory() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let here = SessionId::from("here");
    let there = SessionId::from("there");
    let ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
    let i1 = NodeId::new();
    let i2 = NodeId::new();
    let hub = NodeId::new();
    let dep = NodeId::new();
    let foreign = NodeId::new();

    let local = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&here, i1, None, ts),
            plant_concept(&here, hub, i1, "hub", ConceptType::Entity, ts),
            plant_concept(&here, dep, i1, "dep", ConceptType::Entity, ts),
            plant_edge(&here, hub, dep, EdgeType::Dependency, ts),
            // An edge recorded in `here` whose source concept belongs to
            // `there` — the schema permits it (edges carry no FK) and
            // `GraphStore::flush` is public.
            plant_edge(&here, foreign, dep, EdgeType::Dependency, ts),
        ],
    };
    let elsewhere = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&there, i2, None, ts),
            plant_concept(&there, foreign, i2, "foreign", ConceptType::Entity, ts),
        ],
    };
    store.flush(&local, None).await.unwrap();
    store.flush(&elsewhere, None).await.unwrap();
    let memory = MemoryStore::new();
    memory.flush(&local, None).await.unwrap();
    memory.flush(&elsewhere, None).await.unwrap();

    let want = memory
        .blast_radius(&here, hub, Duration::from_secs(0), Utc::now())
        .await
        .unwrap();
    assert_eq!(
        want, 1,
        "oracle sanity: a foreign-session source cannot un-orphan `dep`"
    );
    let got = store
        .blast_radius(&here, hub, Duration::from_secs(0), Utc::now())
        .await
        .unwrap();
    assert_eq!(
        got, want,
        "SQLite must scope the structural sources to the session, like \
             MemoryStore and Cockroach"
    );
}

/// Edge-age interaction (T3.6 matrix; round-1 review F1 remediation): an
/// AGED inbound structural edge vs a freshly-created one. At min-age 0 the
/// fresh edge counts and un-orphans the target; at 3600s it is filtered
/// out and the orphan still counts. Both cutoffs must agree with
/// MemoryStore on every node.
///
/// Review F1: the span's TWO timestamp gates are discriminated
/// behaviorally, not just textually —
/// * **e-gate:** the fresh edge's source carries a DISTINCT origin
///   interaction (`i2`), so the span set genuinely shrinks at min_age =
///   3600s (aged edge included, fresh edge excluded): `span(orphan).distinct`
///   is 2 at min-age 0 and 1 at 1h. Dropping `e.created_at <= ?` would keep
///   `i2` in the span and fail the anchor (before the fix every origin was
///   `i1`, so the span was identical with or without either gate);
/// * **i-gate probe:** an AGED edge (`probe_src -> probe_victim`) whose
///   origin interaction `i3` is FRESH (created after the 1h cutoff) must be
///   excluded from the span — `span(probe_victim).distinct` is 1 at min-age
///   0 and 0 at 1h. Dropping `i.created_at <= ?` would keep `i3` in the span.
///
/// `blast_radius` is origin-agnostic by contrast: the aged probe edge counts
/// at both ages even though its origin is fresh (the i-gate is span-only,
/// matching MemoryStore).
#[cfg(feature = "store-memory")]
#[tokio::test]
async fn structural_queries_aged_vs_fresh_edge_agree_with_memory() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("aged-vs-fresh");
    let old_ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
    let now = Utc::now();
    let i1 = NodeId::new();
    let i2 = NodeId::new();
    let i3 = NodeId::new();
    let pillar = NodeId::new();
    let orphan = NodeId::new();
    let other = NodeId::new();
    let probe_src = NodeId::new();
    let probe_victim = NodeId::new();

    // Base: aged graph — pillar -> orphan (aged edge, aged origin i1) and
    // probe_src -> probe_victim (aged edge, FRESH origin i3: the i-gate
    // probe). `other`'s origin is DISTINCT i2 — the fresh edge's source.
    let base = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&sid, i1, None, old_ts),
            plant_interaction(&sid, i2, None, old_ts),
            plant_interaction(&sid, i3, None, now),
            plant_concept(&sid, pillar, i1, "pillar", ConceptType::Entity, old_ts),
            plant_concept(&sid, orphan, i1, "orphan", ConceptType::Entity, old_ts),
            plant_concept(&sid, other, i2, "other", ConceptType::Entity, old_ts),
            plant_concept(
                &sid,
                probe_src,
                i3,
                "probe-src",
                ConceptType::Entity,
                old_ts,
            ),
            plant_concept(
                &sid,
                probe_victim,
                i1,
                "probe-victim",
                ConceptType::Entity,
                old_ts,
            ),
            plant_edge(&sid, pillar, orphan, EdgeType::Dependency, old_ts),
            plant_edge(&sid, probe_src, probe_victim, EdgeType::Dependency, old_ts),
        ],
    };
    store.flush(&base, None).await.unwrap();
    let memory = MemoryStore::new();
    memory.flush(&base, None).await.unwrap();
    // Then a genuinely FRESH other -> orphan dependency (created now).
    let fresh = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![plant_edge(&sid, other, orphan, EdgeType::Dependency, now)],
    };
    store.flush(&fresh, None).await.unwrap();
    memory.flush(&fresh, None).await.unwrap();

    let one_hour = Duration::from_secs(3600);
    for node in [pillar, orphan, other, probe_src, probe_victim, i1, i2, i3] {
        for min_age in [Duration::from_secs(0), one_hour] {
            let br = store
                .blast_radius(&sid, node, min_age, Utc::now())
                .await
                .unwrap();
            let br_want = memory
                .blast_radius(&sid, node, min_age, Utc::now())
                .await
                .unwrap();
            assert_eq!(br, br_want, "blast_radius {node} age {min_age:?}");
            let span = store
                .interaction_span(&sid, node, min_age, Utc::now())
                .await
                .unwrap();
            let span_want = memory
                .interaction_span(&sid, node, min_age, Utc::now())
                .await
                .unwrap();
            assert_eq!(span, span_want, "interaction_span {node} age {min_age:?}");
        }
    }
    // e-gate discrimination on the SPAN (independent of the oracle):
    // `other`'s DISTINCT origin i2 counts at min-age 0 and must vanish at
    // 1h when the fresh edge is filtered.
    assert_eq!(
        store
            .interaction_span(&sid, orphan, Duration::from_secs(0), Utc::now())
            .await
            .unwrap()
            .distinct,
        2,
        "e-gate: fresh edge's distinct origin counts at min_age=0"
    );
    assert_eq!(
        store
            .interaction_span(&sid, orphan, one_hour, Utc::now())
            .await
            .unwrap()
            .distinct,
        1,
        "e-gate: fresh edge's distinct origin filtered at min_age=1h"
    );
    // i-gate probe: the AGED probe_src -> probe_victim edge's origin i3 is
    // FRESH, so it is in the span at min-age 0 and must be excluded at 1h.
    assert_eq!(
        store
            .interaction_span(&sid, probe_victim, Duration::from_secs(0), Utc::now())
            .await
            .unwrap()
            .distinct,
        1,
        "i-gate: fresh origin counts at min_age=0"
    );
    assert_eq!(
        store
            .interaction_span(&sid, probe_victim, one_hour, Utc::now())
            .await
            .unwrap()
            .distinct,
        0,
        "i-gate: aged edge with fresh origin excluded at min_age=1h"
    );
    // blast_radius is origin-agnostic: the aged probe edge counts at 1h
    // even though its origin is fresh (span-only i-gate, MemoryStore parity).
    assert_eq!(
        store
            .blast_radius(&sid, probe_src, one_hour, Utc::now())
            .await
            .unwrap(),
        1,
        "blast_radius ignores origin age"
    );
    // The age filter is doing real work for blast_radius (independent of
    // the oracle): with min-age 0 the fresh edge un-orphans; with min-age
    // 1h it is filtered and the orphan still counts.
    assert_eq!(
        store
            .blast_radius(&sid, pillar, Duration::from_secs(0), Utc::now())
            .await
            .unwrap(),
        0,
        "fresh edge counts at min_age=0"
    );
    assert_eq!(
        store
            .blast_radius(&sid, pillar, one_hour, Utc::now())
            .await
            .unwrap(),
        1,
        "fresh edge filtered at min_age=1h"
    );
}

/// T3.6 round-1 review F1: text-level lock that the span SQL gates on BOTH
/// timestamps — the edge's AND the origin interaction's (spec §4.1 second
/// errata). Mirror of Cockroach's
/// `structural_query_placeholder_order_and_counts`: a future narrowing that
/// drops either clause fails here even while the fixtures keep every origin
/// older than the cutoff (the behavioral discrimination is covered by
/// [`structural_queries_aged_vs_fresh_edge_agree_with_memory`]).
#[test]
fn structural_span_sql_gates_both_timestamps() {
    // D: each gate resolves the stored instant through the fallback rule
    // (COALESCE(event_time, created_at)), so the assertion tracks the
    // about-time expression, not a bare column.
    assert!(
        INTERACTION_SPAN_SQL.contains("COALESCE(e.event_time, e.created_at) <= ?"),
        "span SQL must gate the EDGE about-time"
    );
    assert!(
        INTERACTION_SPAN_SQL.contains("COALESCE(i.event_time, i.created_at) <= ?"),
        "span SQL must gate the ORIGIN-INTERACTION about-time"
    );
}

/// F5: `concepts.origin_interaction` is a **global** FK, so a concept in
/// session S may legally point at an interaction in session S′. The span
/// CTE must scope the joined interaction to S (the extent CTE always was),
/// otherwise foreign interactions inflate `distinct` and — since their
/// timestamps sit outside S's extent — the coverage ratio, on a population
/// `MemoryStore` (which resolves origins inside the session snapshot only)
/// never sees. Pre-fix this returned `distinct = 3`, coverage clamped from
/// 200.0; MemoryStore returned `distinct = 0`.
#[cfg(feature = "store-memory")]
#[tokio::test]
async fn interaction_span_ignores_cross_session_origin_interactions() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let here = SessionId::from("span-here");
    let there = SessionId::from("span-there");
    let t0 = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
    let t = |secs: i64| t0 + chrono::Duration::seconds(secs);

    // `here` extent is 100s; `there` straddles it by ±10000s, so an
    // unscoped ratio is 20000/100 = 200.0.
    let (h0, h1) = (NodeId::new(), NodeId::new());
    let (b0, b1, b2) = (NodeId::new(), NodeId::new(), NodeId::new());
    let target = NodeId::new();
    let (s0, s1, s2) = (NodeId::new(), NodeId::new(), NodeId::new());
    let mut mutations = vec![
        plant_interaction(&here, h0, None, t(0)),
        plant_interaction(&here, h1, Some(h0), t(100)),
        plant_interaction(&there, b0, None, t(-10_000)),
        plant_interaction(&there, b1, Some(b0), t(0)),
        plant_interaction(&there, b2, Some(b1), t(10_000)),
        plant_concept(&here, target, h0, "target", ConceptType::Entity, t(0)),
    ];
    // Supports live in `here` but their origins point across sessions.
    for (id, origin, name) in [(s0, b0, "s0"), (s1, b1, "s1"), (s2, b2, "s2")] {
        mutations.push(plant_concept(
            &here,
            id,
            origin,
            name,
            ConceptType::Entity,
            t(0),
        ));
        mutations.push(plant_edge(&here, id, target, EdgeType::Dependency, t(0)));
    }
    let batch = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations,
    };
    store.flush(&batch, None).await.unwrap();
    let memory = MemoryStore::new();
    memory.flush(&batch, None).await.unwrap();

    let got = store
        .interaction_span(&here, target, Duration::from_secs(0), Utc::now())
        .await
        .unwrap();
    let want = memory
        .interaction_span(&here, target, Duration::from_secs(0), Utc::now())
        .await
        .unwrap();
    assert_eq!(
        got, want,
        "cross-session origins must not diverge from MemoryStore"
    );
    assert_eq!(
        got.distinct, 0,
        "foreign-session origin interactions must not count"
    );
    assert_eq!(got.coverage, 0.0);
    assert!(
        got.coverage <= 1.0,
        "coverage is a ratio of the session's own extent"
    );
}

/// F1: a single-interaction session (temporal extent is one point) with a
/// supported inbound dependency reports coverage 1.0, not 0.0 — parity
/// with MemoryStore and Cockroach (canonization Stage 2 in short
/// sessions), and agreement with MemoryStore's naive answer.
#[cfg(feature = "store-memory")]
#[tokio::test]
async fn interaction_span_single_point_session_coverage_is_one() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("single-span");
    let ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
    let i1 = NodeId::new();
    let pillar = NodeId::new();
    let orphan = NodeId::new();
    let batch = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&sid, i1, None, ts),
            plant_concept(&sid, pillar, i1, "pillar", ConceptType::Entity, ts),
            plant_concept(&sid, orphan, i1, "orphan", ConceptType::Entity, ts),
            Mutation::UpsertEdge {
                edge: Edge {
                    event_time: None,
                    id: NodeId::new(),
                    session_id: sid.clone(),
                    source: pillar,
                    target: orphan,
                    edge_type: EdgeType::Dependency,
                    weight: 1.0,
                    reinforcements: 1,
                    created_at: ts,
                    last_reinforced: ts,
                },
            },
        ],
    };
    store.flush(&batch, None).await.unwrap();
    let memory = MemoryStore::new();
    memory.flush(&batch, None).await.unwrap();

    let span = store
        .interaction_span(&sid, orphan, Duration::from_secs(0), Utc::now())
        .await
        .unwrap();
    let span_want = memory
        .interaction_span(&sid, orphan, Duration::from_secs(0), Utc::now())
        .await
        .unwrap();
    assert_eq!(span, span_want, "MemoryStore parity");
    assert_eq!(span.distinct, 1);
    assert_eq!(span.coverage, 1.0);

    // Unsupported target: no inbound structural edges -> 0.0 on both.
    let empty = store
        .interaction_span(&sid, pillar, Duration::from_secs(0), Utc::now())
        .await
        .unwrap();
    let empty_want = memory
        .interaction_span(&sid, pillar, Duration::from_secs(0), Utc::now())
        .await
        .unwrap();
    assert_eq!(empty, empty_want);
    assert_eq!(empty.distinct, 0);
    assert_eq!(empty.coverage, 0.0);
}

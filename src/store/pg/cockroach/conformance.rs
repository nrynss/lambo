use super::*;
use crate::fixtures::{load_mutation_batch, load_snapshot};
use crate::types::{AgentId, ConceptType, Interaction, Node};
use crate::MemoryStore;
use chrono::TimeZone;
use std::env;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

fn dsn() -> Option<String> {
    env::var("LAMBO_COCKROACH_DSN")
        .ok()
        .filter(|s| !s.is_empty())
}

fn missing_dsn_is_fatal(require_live: bool, require_vector_index: bool) -> bool {
    require_live || require_vector_index
}

/// Returns the DSN when present. Without one: under `LAMBO_REQUIRE_LIVE=1` or
/// `LAMBO_REQUIRE_VECTOR_INDEX=1` this PANICS (an evidence-captured live run must
/// never silently skip); otherwise it prints a skip notice and returns None.
/// Callers are `#[ignore]`d, so a missing DSN surfaces as an ignored test in the
/// default run, not a false `ok`.
fn dsn_or_skip(test: &str) -> Option<String> {
    match dsn() {
        Some(d) => Some(d),
        None => {
            let require_live = env::var_os("LAMBO_REQUIRE_LIVE").is_some();
            let require_vector_index = env::var_os("LAMBO_REQUIRE_VECTOR_INDEX").is_some();
            if missing_dsn_is_fatal(require_live, require_vector_index) {
                panic!(
                    "{test}: LAMBO_COCKROACH_DSN is unset but a required-live flag \
                         is set (LAMBO_REQUIRE_LIVE={require_live}, \
                         LAMBO_REQUIRE_VECTOR_INDEX={require_vector_index}) — refusing \
                         to skip a live cockroach test"
                );
            }
            eprintln!("SKIP {test}: LAMBO_COCKROACH_DSN not set");
            None
        }
    }
}

/// Non-ignored honesty gate: the two live tests above are `#[ignore]`d, so the
/// default run reports them as ignored instead of passing while skipping. With
/// `LAMBO_REQUIRE_LIVE=1` a missing DSN must fail loudly — this gate fails even
/// though the ignored tests themselves did not run.
#[test]
fn live_dsn_gate_fails_loudly_when_required() {
    let require_live = env::var_os("LAMBO_REQUIRE_LIVE").is_some();
    let require_vector_index = env::var_os("LAMBO_REQUIRE_VECTOR_INDEX").is_some();
    if !missing_dsn_is_fatal(require_live, require_vector_index) {
        return;
    }
    assert!(
        dsn().is_some(),
        "LAMBO_REQUIRE_LIVE=1 or LAMBO_REQUIRE_VECTOR_INDEX=1 requires \
             LAMBO_COCKROACH_DSN: live cockroach tests must not be silently skipped \
             (run with -- --ignored and a real DSN)"
    );
}

#[test]
fn vector_index_requirement_makes_missing_dsn_fatal() {
    assert!(!missing_dsn_is_fatal(false, false));
    assert!(missing_dsn_is_fatal(true, false));
    assert!(missing_dsn_is_fatal(false, true));
    assert!(missing_dsn_is_fatal(true, true));
}

fn cfg(dsn: String) -> StoreConfig {
    StoreConfig {
        kind: crate::store::StoreKind::Cockroach,
        dsn: Some(dsn),
        path: None,
        // Cockroach parses its width out of `VECTOR(n)`; the pin is for
        // width-agnostic adapters, so the conformance suite carries none.
        vector_dim: None,
    }
}

/// Fresh store for the suite. All checks run inside ONE `#[tokio::test]`, so the
/// pool is created and every connection is used on the same (single-test) Tokio
/// runtime — connections never cross runtimes, which avoids both "pool timed out
/// while waiting for an open connection" (one pool, ≤ `MAX_POOL_CONNECTIONS` conns)
/// and "A Tokio 1.x context was found, but it is being shutdown" (a pooled
/// connection registered with a dead per-test runtime).
fn new_store(dsn: &str) -> CockroachStore {
    CockroachStore::new(cfg(dsn.to_string())).unwrap()
}

/// T8.6 (live): the store-enforced single-writer lease across two **separate
/// pools** (the cross-process shape — each pool is an independent set of
/// connections, exactly what two processes have). One acquires, the other is
/// refused fail-closed and told the holder; after a release the second wins;
/// an unreleased lease is reclaimable only after its TTL.
///
/// `#[ignore]`d like every live cockroach test, so a run without
/// `LAMBO_COCKROACH_DSN` reports it as ignored, never a skip-as-green. Run:
/// `cargo test --features store-cockroach,fixtures -- --ignored`.
#[tokio::test]
#[ignore = "live: requires LAMBO_COCKROACH_DSN"]
async fn single_writer_lease_is_enforced_across_pools() {
    let Some(dsn) = dsn_or_skip("single_writer_lease_is_enforced_across_pools") else {
        return;
    };
    use crate::store::lease::{LeaseHolder, LeaseOutcome};

    let store_a = new_store(&dsn);
    store_a.init_schema().await.expect("init_schema");
    let store_b = new_store(&dsn);

    // Unique session per run so a shared cluster never cross-contaminates.
    let sid = SessionId::from(format!("t8.6-lease-{}", Uuid::new_v4()));
    let a = LeaseHolder {
        endpoint: None,
        agent: AgentId::new("proc-a"),
        pid: 111,
        host: "host-a".into(),
    };
    let b = LeaseHolder {
        endpoint: None,
        agent: AgentId::new("proc-b"),
        pid: 222,
        host: "host-b".into(),
    };
    let ttl = Duration::from_secs(30);

    // Clean any leftover row from a previous aborted run.
    let _ = store_a.release_lease(&sid, &a).await;

    assert!(store_a
        .acquire_lease(&sid, &a, ttl)
        .await
        .expect("A acquire")
        .is_acquired());

    match store_b
        .acquire_lease(&sid, &b, ttl)
        .await
        .expect("B acquire")
    {
        LeaseOutcome::Held { current, .. } => assert_eq!(current.holder, a.token()),
        other => panic!("the second pool must be refused, got {other:?}"),
    }

    // Refresh keeps acquired_at.
    let LeaseOutcome::Acquired(first) = store_a.acquire_lease(&sid, &a, ttl).await.unwrap() else {
        panic!("A refresh");
    };
    let LeaseOutcome::Acquired(refreshed) = store_a.refresh_lease(&sid, &a, ttl).await.unwrap()
    else {
        panic!("A refresh 2");
    };
    assert_eq!(first.acquired_at, refreshed.acquired_at);

    // J2: the endpoint column, on the live cluster. A holds no endpoint
    // (it was built without one), so the row says so; republishing as a
    // reachable holder writes it, `read_lease` reads it back without
    // touching the lease, and a refresh does not blank it.
    assert_eq!(first.endpoint, None);
    assert_eq!(
        store_a
            .read_lease(&sid)
            .await
            .expect("read A")
            .unwrap()
            .endpoint,
        None
    );
    let a_hub = a.clone().reachable_at("/run/lambo/crdb.sock");
    let LeaseOutcome::Acquired(published) = store_a.acquire_lease(&sid, &a_hub, ttl).await.unwrap()
    else {
        panic!("A republish as a reachable holder");
    };
    assert_eq!(published.endpoint.as_deref(), Some("/run/lambo/crdb.sock"));
    assert_eq!(
        store_b
            .read_lease(&sid)
            .await
            .expect("B reads A's row")
            .unwrap()
            .endpoint
            .as_deref(),
        Some("/run/lambo/crdb.sock"),
        "a losing process must be able to read the holder's endpoint from \
             another pool — that read IS the proxy path"
    );
    let LeaseOutcome::Acquired(after_refresh) =
        store_a.refresh_lease(&sid, &a_hub, ttl).await.unwrap()
    else {
        panic!("A refresh 3");
    };
    assert_eq!(
        after_refresh.endpoint.as_deref(),
        Some("/run/lambo/crdb.sock")
    );

    // Release → B takes it.
    store_a.release_lease(&sid, &a).await.expect("A release");
    assert!(store_b
        .acquire_lease(&sid, &b, ttl)
        .await
        .expect("B re-acquire")
        .is_acquired());

    // Expiry-after-crash: B holds a short-TTL lease and never releases; A is
    // refused before the TTL and reclaims after it.
    let short = Duration::from_secs(2);
    store_b.release_lease(&sid, &b).await.ok();
    store_b
        .acquire_lease(&sid, &b, short)
        .await
        .expect("B short");
    assert!(matches!(
        store_a.acquire_lease(&sid, &a, ttl).await.unwrap(),
        LeaseOutcome::Held { .. }
    ));
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert!(store_a
        .acquire_lease(&sid, &a, ttl)
        .await
        .expect("A reclaim after expiry")
        .is_acquired());

    // Cleanup.
    store_a.release_lease(&sid, &a).await.ok();
}

/// T7.4: prove the ANN accuracy dial actually reaches the server **through
/// sqlx**, not merely that it parses.
///
/// This test exists because the obvious way to check it by hand is wrong:
/// `PGOPTIONS=-c vector_search_beam_size=128 psql …` is silently IGNORED on
/// this deployment (measured 2026-08-13: still reports 32, and a
/// `statement_timeout` set the same way reports 0). Only the `options`
/// **connection parameter** in the startup message is honoured — which is
/// what `PgConnectOptions::options()` sets, and what `pool()` uses for both
/// `statement_timeout` and this dial. A hand-check with `PGOPTIONS` would
/// therefore "disprove" a setting that works perfectly.
///
/// Also pins that `options()` APPENDS rather than replaces: sqlx 0.8 builds
/// one space-joined `-c k=v` string, so adding the beam size must not drop
/// the STORE-2 `statement_timeout` bound.
#[tokio::test]
#[ignore = "live: requires LAMBO_COCKROACH_DSN"]
async fn vector_beam_size_reaches_the_server_and_keeps_statement_timeout() {
    let Some(dsn) = dsn_or_skip("vector_beam_size_reaches_the_server") else {
        return;
    };
    // Build the options under the env lock and RELEASE it before any await
    // (spec §6.4 / clippy::await_holding_lock). This is exactly why
    // `connect_options` is synchronous.
    let options = {
        let env = crate::test_util::env_lock();
        env.set(VECTOR_BEAM_SIZE_ENV, "128");
        // Same normalization `CockroachStore::new` applies: sqlx + rustls
        // cannot open libpq's `sslrootcert=system`, and `connect_options`
        // is fed `self.dsn`, which is already rewritten.
        let built = CockroachStore::connect_options(&dsn_for_rustls(&dsn));
        built.unwrap()
    };

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy_with(options);
    let beam: String = sqlx::query_scalar("SHOW vector_search_beam_size")
        .fetch_one(&pool)
        .await
        .unwrap();
    let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(
        beam, "128",
        "explicit LAMBO_VECTOR_BEAM_SIZE did not reach the server"
    );
    assert_ne!(
        timeout, "0",
        "adding the beam-size option dropped the STORE-2 statement_timeout \
             — options() must append, not replace"
    );
}

/// **J3-R2R-3, live.** The Cockroach dialect half of the column preflight:
/// on a real cluster, `init_schema`, a passing preflight, then a required
/// column temporarily renamed away (the older-build shape) refusing by
/// table and column name through the `information_schema.columns` path, and
/// the rename reverted so the shared cluster is left exactly as found.
#[tokio::test]
#[ignore = "live: requires LAMBO_COCKROACH_DSN"]
async fn column_preflight_refuses_a_missing_column_live() {
    let Some(dsn) = dsn_or_skip("column_preflight_refuses_a_missing_column_live") else {
        return;
    };
    let store = new_store(&dsn);
    store
        .init_schema()
        .await
        .expect("init_schema on live cluster");
    store
        .preflight_schema()
        .await
        .expect("a provisioned live cluster must pass the column preflight");
    let pool = &store.pool().await.expect("pool");
    sqlx::query("ALTER TABLE concepts RENAME COLUMN chunk_group_id TO chunk_group_id_x")
        .execute(pool)
        .await
        .expect("rename a required column away (older-build shape)");
    let err = store
        .preflight_schema()
        .await
        .expect_err("a live cluster missing a required column must not pass")
        .to_string();
    assert!(err.contains("concepts"), "names the table: {err}");
    assert!(err.contains("chunk_group_id"), "names the column: {err}");
    assert!(err.contains("lambo provision"), "actionable: {err}");
    sqlx::query("ALTER TABLE concepts RENAME COLUMN chunk_group_id_x TO chunk_group_id")
        .execute(pool)
        .await
        .expect("rename the column back — the shared cluster is left as found");
    store
        .preflight_schema()
        .await
        .expect("passes again after the rename is reverted");
}

fn embed(seed: f32) -> Vec<f32> {
    let mut v: Vec<f32> = (0..1024)
        .map(|i| ((i as f32 + 1.0) * seed).sin() * 0.5)
        .collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    for x in &mut v {
        *x /= norm;
    }
    v
}

fn plant_interaction(sid: &SessionId, id: NodeId, ts: DateTime<Utc>) -> Mutation {
    Mutation::UpsertNode {
        node: Node::Interaction(Interaction {
            id,
            session_id: sid.clone(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("seed".into()),
            previous_id: None,
            created_at: ts,
            event_time: None,
        }),
    }
}

fn plant_concept(
    sid: &SessionId,
    id: NodeId,
    origin: NodeId,
    content: &str,
    ts: DateTime<Utc>,
    embedding: Option<Vec<f32>>,
) -> Mutation {
    plant_concept_full(
        sid,
        id,
        origin,
        content,
        &content.to_lowercase(),
        ConceptType::Entity,
        ts,
        embedding,
        None,
    )
}

/// Full-shape concept planter: verbatim canonical_key, concept type, and
/// chunk_group_id — used by the legal-demote (R2), chunk_group_id round-trip,
/// and mixed-case keyword checks.
// Test helper: every parameter is a distinct planter field (same precedent as
// derive.rs resolve_concept) — a params struct would obscure the call sites.
#[allow(clippy::too_many_arguments)]
fn plant_concept_full(
    sid: &SessionId,
    id: NodeId,
    origin: NodeId,
    content: &str,
    canonical_key: &str,
    concept_type: ConceptType,
    ts: DateTime<Utc>,
    embedding: Option<Vec<f32>>,
    chunk_group_id: Option<String>,
) -> Mutation {
    Mutation::UpsertNode {
        node: Node::Concept(Concept {
            id,
            session_id: sid.clone(),
            content: content.into(),
            canonical_key: canonical_key.into(),
            concept_type,
            origin_interaction: origin,
            origin_agent: AgentId::from("agent-a"),
            created_at: ts,
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding,
            chunk_group_id,
            human_confirmed: 0,
        }),
    }
}

fn plant_edge(
    sid: &SessionId,
    source: NodeId,
    target: NodeId,
    edge_type: EdgeType,
    ts: DateTime<Utc>,
) -> Mutation {
    Mutation::UpsertEdge {
        edge: Edge {
            id: NodeId::new(),
            session_id: sid.clone(),
            source,
            target,
            edge_type,
            weight: 1.0,
            reinforcements: 1,
            created_at: ts,
            last_reinforced: ts,
            event_time: None,
        },
    }
}

fn sorted_snap_parts(snap: &GraphSnapshot) -> (Vec<NodeId>, Vec<NodeId>, Vec<NodeId>) {
    let mut ii: Vec<NodeId> = snap.interactions.iter().map(|i| i.id).collect();
    let mut cc: Vec<NodeId> = snap.concepts.iter().map(|c| c.id).collect();
    let mut ee: Vec<NodeId> = snap.edges.iter().map(|e| e.id).collect();
    ii.sort_by_key(|n| n.0);
    cc.sort_by_key(|n| n.0);
    ee.sort_by_key(|n| n.0);
    (ii, cc, ee)
}

async fn check_init_schema_idempotent(store: &CockroachStore) {
    store.init_schema().await.unwrap();
    store.init_schema().await.unwrap();
}

/// No Tokio needed: `build_store` constructs the adapter without creating the pool
/// (lazy OnceCell), so this runs as a plain sync test. The real DSN also exercises
/// the rustls rewrite + parse validation at construction. `#[ignore]`d: without
/// `LAMBO_COCKROACH_DSN` this must report as ignored, not skip-as-green.
#[test]
#[ignore = "requires LAMBO_COCKROACH_DSN (run live via -- --ignored)"]
fn build_store_returns_working_adapter() {
    let Some(dsn) = dsn_or_skip("build_store_returns_working_adapter") else {
        return;
    };
    let s = crate::store::build_store(cfg(dsn)).unwrap();
    assert!(s.capabilities().contains(Capabilities::VECTOR_SEARCH));
    assert_eq!(s.vector_dimensions(), Some(1024));
}

async fn check_flush_mutations_batch_roundtrip(store: &CockroachStore) {
    let batch = load_mutation_batch("mutations-batch").unwrap();
    let sid = SessionId::from("session-mutations");
    store.flush(&batch, None).await.unwrap();

    // Direct snapshot read-back.
    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(snap.interactions.len(), 2, "both interactions survive");
    assert_eq!(snap.concepts.len(), 1, "deleted concept removed");
    assert_eq!(snap.concepts[0].content, "kept concept");
    assert_eq!(
        snap.concepts[0].canonization_status,
        CanonizationStatus::Candidate,
        "canonization transition applied"
    );
    assert_eq!(snap.edges.len(), 1, "delete_edge + incident-edge cleanup");
    assert_eq!(
        snap.edges[0].id,
        NodeId(Uuid::from_str("f0000000-0000-4000-8000-000000007052").unwrap()),
        "only the Derives edge survives"
    );
    assert_eq!(snap.canonization_events.len(), 1);
    assert_eq!(
        snap.canonization_events[0].node_id,
        NodeId(Uuid::from_str("f0000000-0000-4000-8000-000000007002").unwrap())
    );
    // NOTE: this fixture's final state is intentionally NOT a legal §5.7 graph — it
    // deletes the Temporal edge between the two interactions, so the loaded snapshot
    // cannot be materialized via Graph::from_snapshot (would fail the Temporal-edge
    // invariant). Snapshot-level round-trip is the correct conformance here; graph
    // materialization of legal batches is covered by load.rs tests.
}

async fn check_load_missing_session_is_session_not_found(store: &CockroachStore) {
    let err = store
        .load_session(&SessionId::from(format!("no-such-{}", Uuid::new_v4())))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::SessionNotFound(_)));
}

async fn check_vector_write_and_candidates_top1(store: &CockroachStore) {
    let sid = SessionId::from(format!("conformance-vector-{}", Uuid::new_v4()));
    let i1 = NodeId::new();
    let a = NodeId::new();
    let b = NodeId::new();
    let ts = Utc::now();
    let probe = embed(0.17);
    let contract = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("fixture-v1".into()),
        dim: store.vector_dim,
    };
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(contract.clone()),
                    },
                    plant_interaction(&sid, i1, ts),
                    plant_concept(&sid, a, i1, "alpha concept", ts, Some(probe.clone())),
                    plant_concept(&sid, b, i1, "beta concept", ts, Some(embed(0.5))),
                ],
            },
            None,
        )
        .await
        .unwrap();

    let hits = store
        .vector_candidates_checked(&sid, &probe, &contract, 3)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].item, a, "identical embedding must rank first");
    assert!(
        (hits[0].score - 1.0).abs() < 1e-4,
        "score {}",
        hits[0].score
    );
    assert!(hits[0].score > hits[1].score);

    // Round-trip the stored vector through load_session.
    let snap = store.load_session(&sid).await.unwrap();
    let back = snap
        .concepts
        .iter()
        .find(|c| c.id == a)
        .unwrap()
        .embedding
        .as_ref()
        .unwrap();
    assert_eq!(back.len(), 1024);
    let max_diff = probe
        .iter()
        .zip(back.iter())
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max);
    assert!(max_diff < 1e-4, "vector round-trip max diff {max_diff}");
}

async fn check_vector_candidates_are_session_scoped(store: &CockroachStore) {
    // DECISION D1: the SQL queries GLOBALLY, so a closer foreign-session concept is
    // in the raw top-k — the Rust session filter must drop it, never return it.
    // Two near-paraphrase concepts in session A should retrieve each other via the
    // index while staying session-scoped.
    let sid_a = SessionId::from(format!("conformance-vecscope-a-{}", Uuid::new_v4()));
    let sid_b = SessionId::from(format!("conformance-vecscope-b-{}", Uuid::new_v4()));
    let (i1, i2) = (NodeId::new(), NodeId::new());
    let (a, c, b) = (NodeId::new(), NodeId::new(), NodeId::new());
    let ts = Utc::now();
    let probe = embed(0.11);
    let contract = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("fixture-v1".into()),
        dim: store.vector_dim,
    };
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    Mutation::SetEmbedding {
                        session_id: sid_a.clone(),
                        embedding: Some(contract.clone()),
                    },
                    plant_interaction(&sid_a, i1, ts),
                    plant_concept(&sid_a, a, i1, "register user", ts, Some(probe.clone())),
                    plant_concept(&sid_a, c, i1, "create account", ts, Some(embed(0.115))),
                ],
            },
            None,
        )
        .await
        .unwrap();
    // Foreign session B holds a concept whose vector is EXACTLY the probe — the
    // globally-closest row — so it would rank first in the raw query.
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    Mutation::SetEmbedding {
                        session_id: sid_b.clone(),
                        embedding: Some(contract.clone()),
                    },
                    plant_interaction(&sid_b, i2, ts),
                    plant_concept(&sid_b, b, i2, "foreign closer", ts, Some(probe.clone())),
                ],
            },
            None,
        )
        .await
        .unwrap();

    let hits = store
        .vector_candidates_checked(&sid_a, &probe, &contract, 10)
        .await
        .unwrap();
    let items: Vec<_> = hits.iter().map(|h| h.item).collect();
    assert!(
        !items.contains(&b),
        "foreign-session node {b} leaked into results: {items:?}"
    );
    assert!(
        items.contains(&a) && items.contains(&c),
        "near paraphrases should retrieve each other; got {items:?}"
    );
    assert_eq!(
        hits[0].item, a,
        "own exact candidate should rank first in-session; got {items:?}"
    );
}

async fn check_vector_explain_is_global_topk(store: &CockroachStore) {
    // DECISION D1 shape on the live plan: the query is GLOBAL (no session predicate),
    // so the plan must NOT scan the anti-pattern `concepts_session_id_canonical_key_key`
    // (the T0.3-spike shape that bypassed the vector index). The plan is an ordered
    // top-k over the whole table; whether the optimizer accelerates that top-k with
    // the vector index (`vector search`) is asserted separately by the standalone
    // `vector_explain_camera_proof` gate.
    //
    // T7.4 correction (2026-08-13): this comment used to say that gate was
    // "PENDING where the optimizer scans a small table". That was wrong on both
    // counts — table size was never the cause. The proof failed because it asserted
    // the spaced `vector search` against `EXPLAIN (OPT, VERBOSE)`, which spells the
    // operator `vector-search`, and because a NON-partial vector index cannot imply
    // the query's `embedding IS NOT NULL` predicate, forcing a FULL SCAN. With the
    // index partial (migrations/cockroach/001_init.sql) the proof is green.
    //
    // T7.3 remediation (planner-variance hardening): EXPLAIN with a LITERAL
    // `LIMIT 5` (not a parameterized `LIMIT $2`) to match the captured T0.3
    // evidence, which reproduced a `top-k` node under the literal shape. With a
    // placeholder limit the optimizer MAY fall back to a `limit` + `sort` plan
    // instead — the same correct global-ordered semantics, different node name.
    // So the positive assertion accepts EITHER a `top-k` or a `limit` ordering
    // construct; the assertion that MUST always hold — and is the DECISION D1
    // non-negotiable — is that the plan does NOT reference the session-filtered
    // anti-pattern index. This keeps the gate green against planner variance
    // without weakening the no-anti-pattern guarantee.
    let pool = &store.pool().await.unwrap();
    let probe = encode_vector(&embed(0.5)).unwrap();
    let rows = sqlx::query(
        "EXPLAIN (OPT, VERBOSE) \
             SELECT id, session_id, embedding <-> $1::VECTOR AS dist \
             FROM concepts WHERE embedding IS NOT NULL ORDER BY dist ASC LIMIT 5",
    )
    .bind(&probe)
    .fetch_all(pool)
    .await
    .map_err(backend)
    .unwrap();
    let plan: Vec<String> = rows
        .iter()
        .map(|r| r.try_get::<String, usize>(0).map_err(backend))
        .collect::<Result<_, _>>()
        .unwrap();
    let text = plan.join("\n");
    assert!(
        text.contains("top-k") || text.contains("limit"),
        "EXPLAIN must be a global ordered top-k/limit query, got:\n{text}"
    );
    assert!(
        !text.contains("concepts_session_id_canonical_key_key"),
        "EXPLAIN must NOT scan the session-filtered index (DECISION D1 anti-pattern):\n{text}"
    );
}

async fn check_keyword_candidates_on_planted_concept(store: &CockroachStore) {
    let sid = SessionId::from(format!("conformance-kw-{}", Uuid::new_v4()));
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, Utc::now()),
                    plant_concept(&sid, c1, i1, "user schema", Utc::now(), None),
                ],
            },
            None,
        )
        .await
        .unwrap();
    let hits = store
        .keyword_candidates(&sid, &["schema".into()], 5)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item, c1);
    assert_eq!(hits[0].score, 1.0);
    // Empty tokens match nothing (MemoryStore parity).
    assert!(store
        .keyword_candidates(&sid, &["   ".into()], 5)
        .await
        .unwrap()
        .is_empty());
    // Missing session -> SessionNotFound (MemoryStore parity).
    let err = store
        .keyword_candidates(&SessionId::from("nope"), &["schema".into()], 5)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::SessionNotFound(_)));
}

async fn check_keyword_mixed_case_ranks_like_memory_store(store: &CockroachStore) {
    // Regression (P3 review R1): the SQL predicate lowercases content/key, so the
    // score must fold case too. A mixed-case concept ("Register User") matched by
    // token "register" must score > 0 and rank exactly like MemoryStore — a
    // raw-`contains` score would give it 0.0 and sink it below "user schema".
    let sid = SessionId::from(format!("conformance-kwcase-{}", Uuid::new_v4()));
    let i1 = NodeId::new();
    let mixed = NodeId::new();
    let lower = NodeId::new();
    let ts = Utc::now();
    let batch = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&sid, i1, ts),
            // Mixed-case content AND canonical_key — selected by the SQL's lower()
            // predicate, scored only if the score loop folds case.
            plant_concept_full(
                &sid,
                mixed,
                i1,
                "Register User",
                "Register User",
                ConceptType::Entity,
                ts,
                None,
                None,
            ),
            plant_concept_full(
                &sid,
                lower,
                i1,
                "user schema",
                "user schema",
                ConceptType::Entity,
                ts,
                None,
                None,
            ),
        ],
    };
    store.flush(&batch, None).await.unwrap();
    let mem = MemoryStore::new();
    mem.flush(&batch, None).await.unwrap();

    let tokens: Vec<String> = vec!["register".into(), "user".into()];
    let crdb = store.keyword_candidates(&sid, &tokens, 5).await.unwrap();
    let mem_res = mem.keyword_candidates(&sid, &tokens, 5).await.unwrap();
    assert_eq!(
        crdb, mem_res,
        "mixed-case scoring must match MemoryStore exactly"
    );
    assert_eq!(crdb.len(), 2);
    assert_eq!(
        crdb[0].item, mixed,
        "Register User (2 hits) must rank above user schema (1 hit)"
    );
    assert_eq!(crdb[0].score, 2.0);
    assert_eq!(crdb[1].item, lower);
    assert_eq!(crdb[1].score, 1.0);
}

async fn check_legal_demote_flush_partial_index(store: &CockroachStore) {
    // R2 (P3 review): the schema's canonical-key unique index is PARTIAL
    // (`WHERE concept_type <> 'Observation'`, spec §4 errata / muse-spark M1-M2).
    // A legal demote (T2.5) writes Observations that share a canonical key
    // (identical sentences from different chunks); those must flush successfully
    // against the live store instead of colliding on the index.
    let sid = SessionId::from(format!("conformance-demote-{}", Uuid::new_v4()));
    let i1 = NodeId::new();
    let o1 = NodeId::new();
    let o2 = NodeId::new();
    let ts = Utc::now();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, ts),
                    plant_concept_full(
                        &sid,
                        o1,
                        i1,
                        "identical sentence",
                        "identical sentence",
                        ConceptType::Observation,
                        ts,
                        None,
                        Some("chunk-1".into()),
                    ),
                    plant_concept_full(
                        &sid,
                        o2,
                        i1,
                        "identical sentence",
                        "identical sentence",
                        ConceptType::Observation,
                        ts,
                        None,
                        Some("chunk-1".into()),
                    ),
                ],
            },
            None,
        )
        .await
        .unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(snap.concepts.len(), 2, "both demoted Observations survive");
    assert!(snap
        .concepts
        .iter()
        .all(|c| c.concept_type == ConceptType::Observation));
    assert!(snap
        .concepts
        .iter()
        .all(|c| c.canonical_key == "identical sentence"));
    assert!(snap
        .concepts
        .iter()
        .all(|c| c.chunk_group_id.as_deref() == Some("chunk-1")));

    // Negative lock on the same index: a duplicate-key NON-Observation must be
    // rejected (the RAM graph rejects it as an invariant; the store fails loudly).
    let e1 = NodeId::new();
    let e2 = NodeId::new();
    let bad = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_concept_full(
                &sid,
                e1,
                i1,
                "dup key",
                "dup key",
                ConceptType::Entity,
                ts,
                None,
                None,
            ),
            plant_concept_full(
                &sid,
                e2,
                i1,
                "dup key",
                "dup key",
                ConceptType::Entity,
                ts,
                None,
                None,
            ),
        ],
    };
    assert!(
        store.flush(&bad, None).await.is_err(),
        "duplicate-key non-Observation must violate concepts_key_non_obs_idx"
    );
}

async fn check_chunk_group_id_survives_flush_load(store: &CockroachStore) {
    // T5.2 contract (schema persistence): flush→load must PRESERVE
    // chunk_group_id — the implementer's snapshot normalization to None is gone;
    // the round-trip now asserts survival.
    let sid = SessionId::from(format!("conformance-cgid-{}", Uuid::new_v4()));
    let i1 = NodeId::new();
    let obs = NodeId::new();
    let plain = NodeId::new();
    let ts = Utc::now();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, ts),
                    plant_concept_full(
                        &sid,
                        obs,
                        i1,
                        "overflow sentence",
                        "overflow sentence",
                        ConceptType::Observation,
                        ts,
                        None,
                        Some("chunk-42".into()),
                    ),
                    plant_concept_full(
                        &sid,
                        plain,
                        i1,
                        "plain concept",
                        "plain concept",
                        ConceptType::Entity,
                        ts,
                        None,
                        None,
                    ),
                ],
            },
            None,
        )
        .await
        .unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    let obs_row = snap.concepts.iter().find(|c| c.id == obs).unwrap();
    assert_eq!(
        obs_row.chunk_group_id.as_deref(),
        Some("chunk-42"),
        "chunk_group_id must survive flush→load (T5.2 sibling co-retrieval key)"
    );
    let plain_row = snap.concepts.iter().find(|c| c.id == plain).unwrap();
    assert_eq!(
        plain_row.chunk_group_id, None,
        "NULL chunk_group_id stays None"
    );
}

async fn check_embedding_contract_read_and_flush_immunity(store: &CockroachStore) {
    // Embedding contract (S5-class snapshot metadata): seed — the PRODUCTION
    // full-snapshot path (STORE-1 remediation) — persists embedding_kind/model/dim,
    // and load_session materializes GraphSnapshot.embedding when present. A later
    // flush (which only ensures the session row; there is no session-metadata
    // Mutation kind) must NOT clobber the stamped contract.
    let sid = SessionId::from(format!("conformance-embed-{}", Uuid::new_v4()));
    store
        .seed(&GraphSnapshot {
            session_id: sid.clone(),
            embedding: Some(EmbeddingContract {
                kind: "bge_m3".into(),
                model: Some("BAAI/bge-m3".into()),
                dim: 1024,
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(
        snap.embedding,
        Some(EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("BAAI/bge-m3".into()),
            dim: 1024,
        }),
        "load_session must materialize the seeded embedding contract"
    );
    // A subsequent flush without a metadata mutation must not clobber the
    // seeded contract.
    let i1 = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, Utc::now()),
                    plant_concept(&sid, NodeId::new(), i1, "seed concept", Utc::now(), None),
                ],
            },
            None,
        )
        .await
        .unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    let emb = snap
        .embedding
        .expect("embedding contract survives a later flush");
    assert_eq!(emb.kind, "bge_m3");
    assert_eq!(emb.model.as_deref(), Some("BAAI/bge-m3"));
    assert_eq!(emb.dim, 1024);
    assert_eq!(
        snap.concepts.len(),
        1,
        "flush content still lands alongside the contract"
    );
    // Session with no stamp reads back None.
    let plain_sid = SessionId::from(format!("conformance-embed-none-{}", Uuid::new_v4()));
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![plant_interaction(&plain_sid, NodeId::new(), Utc::now())],
            },
            None,
        )
        .await
        .unwrap();
    let snap = store.load_session(&plain_sid).await.unwrap();
    assert_eq!(snap.embedding, None, "unstamped session has no contract");
}

async fn check_seed_load_full_snapshot_roundtrip(store: &CockroachStore) {
    let snap = load_snapshot("session-rest-api").unwrap();
    store.seed(&snap).await.unwrap();
    let loaded = store.load_session(&snap.session_id).await.unwrap();

    let (li, lc, le) = sorted_snap_parts(&loaded);
    let (si, sc, se) = sorted_snap_parts(&snap);
    assert_eq!(li, si, "interactions round-trip");
    assert_eq!(lc, sc, "concepts round-trip");
    assert_eq!(le, se, "edges round-trip");

    // Field-level equality on representative rows.
    assert_eq!(loaded.session_id, snap.session_id);
    assert_eq!(loaded.root_goal, snap.root_goal);
    assert_eq!(loaded.created_at, snap.created_at);
    assert_eq!(loaded.closed_at, snap.closed_at);
    assert_eq!(
        loaded.synonyms.len(),
        snap.synonyms.len(),
        "synonym persisted via seed"
    );
    assert_eq!(loaded.synonyms[0].source_key, snap.synonyms[0].source_key);
    assert_eq!(
        loaded.synonyms[0].canonical_key,
        snap.synonyms[0].canonical_key
    );
    assert!(loaded.reservations.is_empty());
    assert!(loaded.canonization_events.is_empty());

    // Full deep-equality of the whole graph (fixture order is id-ordered).
    let mut a = loaded.concepts.clone();
    let mut b = snap.concepts.clone();
    a.sort_by_key(|c| c.id.0);
    b.sort_by_key(|c| c.id.0);
    assert_eq!(a, b, "concept rows deep-equal (incl. timestamps)");
    let mut ai = loaded.interactions.clone();
    let mut bi = snap.interactions.clone();
    ai.sort_by_key(|i| i.id.0);
    bi.sort_by_key(|i| i.id.0);
    assert_eq!(ai, bi);
    let mut ae = loaded.edges.clone();
    let mut be = snap.edges.clone();
    ae.sort_by_key(|e| e.id.0);
    be.sort_by_key(|e| e.id.0);
    assert_eq!(ae, be);
}

/// T3.6 matrix for ONE fixture: seed it into the live Cockroach store AND a
/// fresh MemoryStore, then assert EVERY node (concepts + interactions) ×
/// min-age {0, 3600s} × both queries answers EXACTLY like MemoryStore's
/// naive scan on the same snapshot. Seeding is an idempotent upsert, so
/// re-runs against a persistent cluster converge. Returns the number of
/// equality assertions performed (2 per node × age cell).
async fn check_fixture_structural_agreement(store: &CockroachStore, fixture: &str) -> usize {
    let snap = load_snapshot(fixture).unwrap();
    let sid = snap.session_id.clone();
    store.seed(&snap).await.unwrap();

    let mem = {
        let m = MemoryStore::new();
        m.seed(snap.clone()).unwrap();
        Arc::new(m)
    };

    let node_ids: Vec<NodeId> = snap
        .concepts
        .iter()
        .map(|c| c.id)
        .chain(snap.interactions.iter().map(|i| i.id))
        .collect();
    let ages = [Duration::ZERO, Duration::from_secs(3600)];
    let mut assertions = 0usize;
    for node in &node_ids {
        for age in ages {
            let mem_br = mem
                .blast_radius(&sid, *node, age, Utc::now())
                .await
                .unwrap();
            let crdb_br = store
                .blast_radius(&sid, *node, age, Utc::now())
                .await
                .unwrap();
            assert_eq!(mem_br, crdb_br, "blast_radius({fixture}, {node}, {age:?})");
            let mem_span = mem
                .interaction_span(&sid, *node, age, Utc::now())
                .await
                .unwrap();
            let crdb_span = store
                .interaction_span(&sid, *node, age, Utc::now())
                .await
                .unwrap();
            assert_eq!(
                mem_span, crdb_span,
                "interaction_span({fixture}, {node}, {age:?}): mem={mem_span:?} crdb={crdb_span:?}"
            );
            assertions += 2;
        }
    }

    // Deterministic anchors on rest-api (independent of the oracle): eight
    // concepts depend on the Canonical hub 1001 exclusively; span = 6
    // distinct interactions over 25 of 55 minutes.
    if fixture == "session-rest-api" {
        let hub: NodeId = snap
            .concepts
            .iter()
            .find(|c| c.id.0.to_string().ends_with("001001"))
            .unwrap()
            .id;
        assert_eq!(
            store
                .blast_radius(&sid, hub, Duration::ZERO, Utc::now())
                .await
                .unwrap(),
            8,
            "hub 1001 blast radius anchor"
        );
        let span = store
            .interaction_span(&sid, hub, Duration::ZERO, Utc::now())
            .await
            .unwrap();
        assert_eq!(span.distinct, 6);
        assert!(
            (span.coverage - 25.0 / 55.0).abs() < 1e-9,
            "hub 1001 span anchor: {span:?}"
        );
    }
    assertions
}

/// T3.6 acceptance: the three-way agreement matrix on BOTH fixture graphs —
/// `session-rest-api` (22 concepts incl. Canonical hub 1001, Venerable
/// 1012, D1–D8 orphans, P1/P2 peers) and `session-drift` (9 concepts) —
/// every node × min-age {0, 3600s}, blast_radius + interaction_span
/// (distinct AND coverage) exactly equal to MemoryStore.
async fn check_structural_queries_agree_with_memory_store(store: &CockroachStore) {
    let mut total_assertions = 0usize;
    for fixture in ["session-rest-api", "session-drift"] {
        total_assertions += check_fixture_structural_agreement(store, fixture).await;
    }
    // Lock the matrix dimensions like the sqlite suite (round-1 review F2):
    // rest-api 34 nodes (22 concepts + 12 interactions) + drift 11 nodes
    // (9 + 2) = 45 nodes; 2 ages; 2 queries each -> 180 equality assertions.
    // A future narrowing of either matrix loop silently shrinks coverage,
    // so the count must be verified, not just implied by the loop shape.
    assert_eq!(total_assertions, 180, "matrix dimensions drifted");
}

async fn check_structural_queries_age_filter_agrees(store: &CockroachStore) {
    // T3.6 matrix + round-1 review F1 remediation: the span's TWO timestamp
    // gates are discriminated behaviorally, not just textually —
    // * e-gate: the fresh edge's source carries a DISTINCT origin
    //   interaction (`i2`), so the span set genuinely shrinks at
    //   min_age = 3600s: `span(orphan).distinct` is 2 at min-age 0 and 1 at
    //   1h (before the fix every origin was `i1`, so the span was identical
    //   with or without either gate);
    // * i-gate probe: an AGED edge (`probe_src -> probe_victim`) whose
    //   origin interaction `i3` is FRESH (created after the 1h cutoff) must
    //   be excluded from the span — `span(probe_victim).distinct` is 1 at
    //   min-age 0 and 0 at 1h.
    // blast_radius is origin-agnostic by contrast (the i-gate is span-only,
    // matching MemoryStore).
    let sid = SessionId::from(format!("conformance-age-{}", Uuid::new_v4()));
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
            plant_interaction(&sid, i1, old_ts),
            plant_interaction(&sid, i2, old_ts),
            plant_interaction(&sid, i3, now),
            plant_concept(&sid, pillar, i1, "pillar", old_ts, None),
            plant_concept(&sid, orphan, i1, "orphan", old_ts, None),
            plant_concept(&sid, other, i2, "other", old_ts, None),
            plant_concept(&sid, probe_src, i3, "probe-src", old_ts, None),
            plant_concept(&sid, probe_victim, i1, "probe-victim", old_ts, None),
            plant_edge(&sid, pillar, orphan, EdgeType::Dependency, old_ts),
            plant_edge(&sid, probe_src, probe_victim, EdgeType::Dependency, old_ts),
        ],
    };
    store.flush(&base, None).await.unwrap();
    let mem = MemoryStore::new();
    mem.flush(&base, None).await.unwrap();
    // Then a genuinely FRESH other -> orphan dependency (created now).
    let fresh = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![plant_edge(&sid, other, orphan, EdgeType::Dependency, now)],
    };
    store.flush(&fresh, None).await.unwrap();
    mem.flush(&fresh, None).await.unwrap();

    let one_hour = Duration::from_secs(3600);
    // Aged-vs-fresh edge interaction: EVERY node × both cutoffs must agree
    // with MemoryStore exactly (the aged pillar -> orphan edge vs the
    // freshly created other -> orphan edge, plus the i-gate probe).
    for node in [pillar, orphan, other, probe_src, probe_victim, i1, i2, i3] {
        for min_age in [Duration::ZERO, one_hour] {
            let mem_br = mem
                .blast_radius(&sid, node, min_age, Utc::now())
                .await
                .unwrap();
            let crdb_br = store
                .blast_radius(&sid, node, min_age, Utc::now())
                .await
                .unwrap();
            assert_eq!(mem_br, crdb_br, "blast_radius({node}, {min_age:?})");
            let mem_span = mem
                .interaction_span(&sid, node, min_age, Utc::now())
                .await
                .unwrap();
            let crdb_span = store
                .interaction_span(&sid, node, min_age, Utc::now())
                .await
                .unwrap();
            assert_eq!(
                mem_span, crdb_span,
                "interaction_span({node}, {min_age:?}): mem={mem_span:?} crdb={crdb_span:?}"
            );
        }
    }
    // e-gate discrimination on the SPAN (independent of the oracle):
    // `other`'s DISTINCT origin i2 counts at min-age 0 and must vanish at
    // 1h when the fresh edge is filtered.
    assert_eq!(
        store
            .interaction_span(&sid, orphan, Duration::ZERO, Utc::now())
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
            .interaction_span(&sid, probe_victim, Duration::ZERO, Utc::now())
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
    // The age filter is doing real work for blast_radius: with min_age=0 the
    // fresh edge un-orphans; with min_age=1h it is filtered and the orphan
    // still counts.
    assert_eq!(
        store
            .blast_radius(&sid, pillar, Duration::ZERO, Utc::now())
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

/// §4.1 errata probe (T3.6): mirror of MemoryStore's
/// `blast_radius_ignores_provenance_derives_edges` against the live SQL
/// adapter. §5.7 requires every concept to carry a `Derives` edge
/// (interaction → concept); if blast_radius counted that inbound edge as
/// "another source", every concept would look non-orphaned and Stage-3
/// blast radius would collapse to ~0. Cockroach must ignore provenance
/// (`Derives`/`Temporal`) edges exactly like MemoryStore — never
/// un-orphaning a concept through them.
async fn check_structural_queries_errata_derives_probe(store: &CockroachStore) {
    let sid = SessionId::from(format!("conformance-errata-{}", Uuid::new_v4()));
    let old_ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
    let i1 = NodeId::new();
    let pillar = NodeId::new();
    let orphan = NodeId::new();
    let alone = NodeId::new();
    let batch = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&sid, i1, old_ts),
            plant_concept(&sid, pillar, i1, "pillar", old_ts, None),
            plant_concept(&sid, orphan, i1, "orphan", old_ts, None),
            plant_concept(&sid, alone, i1, "alone", old_ts, None),
            // pillar -> orphan (Dependency): the only structural inbound.
            plant_edge(&sid, pillar, orphan, EdgeType::Dependency, old_ts),
            // orphan ALSO has the mandatory §5.7 Derives from its origin
            // interaction — counting it would un-orphan orphan.
            plant_edge(&sid, i1, orphan, EdgeType::Derives, old_ts),
            // alone has ONLY the Derives provenance: never an orphan of
            // anyone (no structural inbound edge exists at all).
            plant_edge(&sid, i1, alone, EdgeType::Derives, old_ts),
        ],
    };
    store.flush(&batch, None).await.unwrap();
    let mem = MemoryStore::new();
    mem.flush(&batch, None).await.unwrap();

    for min_age in [Duration::ZERO, Duration::from_secs(3600)] {
        let want = mem
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
            "Cockroach must ignore provenance Derives exactly like MemoryStore (min_age {min_age:?})"
        );
    }
}

/// F1: a single-interaction session (temporal extent is one point) with a
/// supported inbound dependency reports coverage 1.0, not 0.0 — parity
/// with the MemoryStore fix (canonization Stage 2 in short sessions).
async fn check_interaction_span_single_point_session_coverage(store: &CockroachStore) {
    let sid = SessionId::from(format!("conformance-span-single-{}", Uuid::new_v4()));
    let ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
    let i1 = NodeId::new();
    let pillar = NodeId::new();
    let orphan = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, ts),
                    plant_concept(&sid, pillar, i1, "pillar", ts, None),
                    plant_concept(&sid, orphan, i1, "orphan", ts, None),
                    plant_edge(&sid, pillar, orphan, EdgeType::Dependency, ts),
                ],
            },
            None,
        )
        .await
        .unwrap();

    let mem = MemoryStore::new();
    mem.flush(
        &MutationBatch {
            mutation_epoch: 0,
            gc_mark: Default::default(),
            mutations: vec![
                plant_interaction(&sid, i1, ts),
                plant_concept(&sid, pillar, i1, "pillar", ts, None),
                plant_concept(&sid, orphan, i1, "orphan", ts, None),
                plant_edge(&sid, pillar, orphan, EdgeType::Dependency, ts),
            ],
        },
        None,
    )
    .await
    .unwrap();

    let crdb_span = store
        .interaction_span(&sid, orphan, Duration::ZERO, Utc::now())
        .await
        .unwrap();
    let mem_span = mem
        .interaction_span(&sid, orphan, Duration::ZERO, Utc::now())
        .await
        .unwrap();
    assert_eq!(
        crdb_span, mem_span,
        "three-way parity on the single-point session"
    );
    assert_eq!(crdb_span.distinct, 1);
    assert_eq!(crdb_span.coverage, 1.0, "F1: {crdb_span:?}");

    // Unsupported target: no inbound structural edges -> 0.0 on both.
    let empty_crdb = store
        .interaction_span(&sid, pillar, Duration::ZERO, Utc::now())
        .await
        .unwrap();
    let empty_mem = mem
        .interaction_span(&sid, pillar, Duration::ZERO, Utc::now())
        .await
        .unwrap();
    assert_eq!(empty_crdb, empty_mem);
    assert_eq!(empty_crdb.distinct, 0);
    assert_eq!(empty_crdb.coverage, 0.0);
}

async fn check_record_canonization_appends_and_is_idempotent(store: &CockroachStore) {
    let sid = SessionId::from(format!("conformance-canon-{}", Uuid::new_v4()));
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, Utc::now()),
                    plant_concept(&sid, c1, i1, "pillar", Utc::now(), None),
                ],
            },
            None,
        )
        .await
        .unwrap();

    let event = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid.clone(),
        node_id: c1,
        from_status: CanonizationStatus::None,
        to_status: CanonizationStatus::Venerable,
        blast_radius: Some(9),
        last_demotion_time: None,
        occurred_at: Utc::now(),
    };
    store.record_canonization(&event, None).await.unwrap();
    // Same event re-recorded (retried flush) must not duplicate the audit row.
    store.record_canonization(&event, None).await.unwrap();

    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(snap.canonization_events.len(), 1, "idempotent append");
    assert_eq!(
        snap.canonization_events[0].to_status,
        CanonizationStatus::Venerable
    );
    assert_eq!(snap.canonization_events[0].blast_radius, Some(9));
    assert_eq!(
        snap.concepts[0].canonization_status,
        CanonizationStatus::Venerable
    );
    assert_eq!(snap.concepts[0].blast_radius, Some(9));

    // Missing concept -> NotFound (MemoryStore parity).
    let ghost = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid.clone(),
        node_id: NodeId::new(),
        from_status: CanonizationStatus::None,
        to_status: CanonizationStatus::Canonical,
        blast_radius: None,
        last_demotion_time: None,
        occurred_at: Utc::now(),
    };
    let err = store.record_canonization(&ghost, None).await.unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
}

async fn check_corrupt_contract_row_load_errors(store: &CockroachStore) {
    // STORE-7: a sessions row with embedding_dim set but embedding_kind NULL
    // (manufactured here via DIRECT SQL — the store's write path can never
    // produce it) must make load_session error like sqlite, never return a
    // silent `embedding: None`. The offline unit test
    // `session_embedding_xor_corruption_errors_not_silent_none` covers the
    // same arms without a cluster; this check proves the end-to-end read path.
    let sid = SessionId::from(format!("conformance-store7-a-{}", Uuid::new_v4()));
    let pool = &store.pool().await.unwrap();
    sqlx::query("INSERT INTO sessions (session_id, embedding_dim) VALUES ($1, 1024)")
        .bind(sid.0.as_str())
        .execute(pool)
        .await
        .unwrap();
    let err = store.load_session(&sid).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("embedding_dim without embedding_kind"),
        "dim-without-kind row must error, not silently load: {err}"
    );

    // Mirror image: kind set, dim NULL.
    let sid2 = SessionId::from(format!("conformance-store7-b-{}", Uuid::new_v4()));
    sqlx::query("INSERT INTO sessions (session_id, embedding_kind) VALUES ($1, 'bge_m3')")
        .bind(sid2.0.as_str())
        .execute(pool)
        .await
        .unwrap();
    let err = store.load_session(&sid2).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("embedding_kind without embedding_dim"),
        "{err}"
    );
}

async fn check_unstamped_vector_candidates_are_empty_until_contract_commit(store: &CockroachStore) {
    let probe = embed(0.23);
    let expected = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("fixture-v1".into()),
        dim: store.vector_dim,
    };
    let missing = SessionId::from(format!("conformance-vector-fresh-{}", Uuid::new_v4()));
    assert!(store
        .vector_candidates_checked(&missing, &probe, &expected, 5)
        .await
        .unwrap()
        .is_empty());

    let legacy = SessionId::from(format!("conformance-vector-legacy-{}", Uuid::new_v4()));
    let interaction = NodeId::new();
    let concept = NodeId::new();
    let now = Utc::now();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&legacy, interaction, now),
                    plant_concept(
                        &legacy,
                        concept,
                        interaction,
                        "legacy unknown vector",
                        now,
                        Some(probe.clone()),
                    ),
                ],
            },
            None,
        )
        .await
        .unwrap();
    assert!(store
        .vector_candidates_checked(&legacy, &probe, &expected, 5)
        .await
        .unwrap()
        .is_empty());

    let contract = expected.clone();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![Mutation::SetEmbedding {
                    session_id: legacy.clone(),
                    embedding: Some(contract.clone()),
                }],
            },
            None,
        )
        .await
        .unwrap();
    let loaded = store.load_session(&legacy).await.unwrap();
    assert_eq!(loaded.embedding, Some(contract));
    assert!(loaded.concepts[0].embedding.is_none());
    assert!(store
        .vector_candidates_checked(&legacy, &probe, &expected, 5)
        .await
        .unwrap()
        .is_empty());

    let corrupt = SessionId::from(format!("conformance-vector-corrupt-{}", Uuid::new_v4()));
    sqlx::query("INSERT INTO sessions (session_id, embedding_dim) VALUES ($1, 1024)")
        .bind(corrupt.as_str())
        .execute(&store.pool().await.unwrap())
        .await
        .unwrap();
    assert!(store
        .vector_candidates_checked(&corrupt, &probe, &expected, 5)
        .await
        .is_err());
}

/// P4 residual closure: `SET_ROOT_GOAL_SQL` (the UPDATE path) was
/// compile-verified only until a live DSN was available. A `SetRootGoal`
/// mutation through `flush` must persist the JSONB goal and read back
/// identical; `None` clears it; the flush's session-row upsert creates the
/// row so a bare goal write works without a prior seed.
async fn check_set_root_goal_mutation_persists(store: &CockroachStore) {
    let sid = SessionId::from("live-set-root-goal");
    let goal = serde_json::json!({"text": "finish the demo", "n": 1});
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![Mutation::SetRootGoal {
                    session_id: sid.clone(),
                    goal: Some(goal.clone()),
                }],
            },
            None,
        )
        .await
        .expect("flush SetRootGoal");
    let snap = store.load_session(&sid).await.expect("load after set");
    assert_eq!(snap.root_goal, Some(goal), "goal read back identical");

    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![Mutation::SetRootGoal {
                    session_id: sid.clone(),
                    goal: None,
                }],
            },
            None,
        )
        .await
        .expect("flush SetRootGoal clear");
    let snap = store.load_session(&sid).await.expect("load after clear");
    assert_eq!(snap.root_goal, None, "None clears the goal");
}

async fn check_set_embedding_mutation_persists(store: &CockroachStore) {
    let sid = SessionId::from(format!("live-set-embedding-{}", Uuid::new_v4()));
    let embedding = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("fixture-v1".into()),
        dim: store
            .vector_dimensions()
            .expect("Cockroach has a vector dimension"),
    };
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![Mutation::SetEmbedding {
                    session_id: sid.clone(),
                    embedding: Some(embedding.clone()),
                }],
            },
            None,
        )
        .await
        .expect("flush SetEmbedding");
    let snap = store.load_session(&sid).await.expect("load after stamp");
    assert_eq!(snap.embedding, Some(embedding));
}

/// All live checks run inside ONE test/runtime — see [`new_store`] for why (pool
/// and connections must never cross Tokio runtimes). `#[ignore]`d: without
/// `LAMBO_COCKROACH_DSN` this must report as ignored, not skip-as-green. Each
/// check has a distinct session namespace so re-runs against a persistent cluster
/// are idempotent.
#[tokio::test]
#[ignore = "requires LAMBO_COCKROACH_DSN (run live via -- --ignored)"]
async fn conformance_suite() {
    let Some(dsn) = dsn_or_skip("conformance_suite") else {
        return;
    };
    let store = new_store(&dsn);
    check_init_schema_idempotent(&store).await;
    check_flush_mutations_batch_roundtrip(&store).await;
    check_load_missing_session_is_session_not_found(&store).await;
    check_vector_write_and_candidates_top1(&store).await;
    check_vector_candidates_are_session_scoped(&store).await;
    check_vector_explain_is_global_topk(&store).await;
    check_keyword_candidates_on_planted_concept(&store).await;
    check_keyword_mixed_case_ranks_like_memory_store(&store).await;
    check_legal_demote_flush_partial_index(&store).await;
    check_chunk_group_id_survives_flush_load(&store).await;
    check_embedding_contract_read_and_flush_immunity(&store).await;
    check_seed_load_full_snapshot_roundtrip(&store).await;
    check_structural_queries_agree_with_memory_store(&store).await;
    check_structural_queries_age_filter_agrees(&store).await;
    check_structural_queries_errata_derives_probe(&store).await;
    check_interaction_span_single_point_session_coverage(&store).await;
    check_record_canonization_appends_and_is_idempotent(&store).await;
    check_corrupt_contract_row_load_errors(&store).await;
    check_unstamped_vector_candidates_are_empty_until_contract_commit(&store).await;
    check_set_root_goal_mutation_persists(&store).await;
    check_set_embedding_mutation_persists(&store).await;
    crate::store::pg::delete_fencing::check_delete_only_batch_is_fenced(&store).await;
    crate::store::pg::release_fencing::check_release_keeps_the_token(&store).await;
    crate::store::pg::erase::check_erase_session(&store).await;
    crate::store::pg::erase::check_erase_after_release(&store).await;
}

/// DECISION D1 item 3 camera-proof: the global vector query must execute as
/// `vector search` on `concepts@concepts_embedding_idx` (spec §12.1 — "we used the
/// CockroachDB distributed vector index", on camera).
///
/// **T7.4 (2026-08-13) — this gate is no longer deployment-conditional.** The
/// historical "PENDING on an index-favorable cluster" reading was wrong on both
/// counts, and both causes are now fixed:
///
/// 1. The assertion could never match its own output. The test used
///    `EXPLAIN (OPT, VERBOSE)`, whose operator is spelled `vector-search`
///    (hyphenated), and asserted the spaced `vector search` — so it failed at the
///    first assertion on ANY cluster, behind ANY index, even with a perfect vector
///    plan. See `dev-diary/evidence/20260813-131108-…-camera-proof-diagnosis.txt`.
/// 2. `WHERE embedding IS NOT NULL` — which is load-bearing and must not be removed,
///    since NULL-`dist` rows hard-error the `f64` decode — cannot be proven implied
///    by a NON-partial vector index, so the optimizer planned a FULL SCAN. T7.4 made
///    `concepts_embedding_idx` itself PARTIAL on that same predicate in
///    `migrations/cockroach/001_init.sql`; the production query is unchanged.
///
/// **Why plain `EXPLAIN` and not `EXPLAIN (OPT, VERBOSE)`** (a deliberate choice —
/// each format spells the operator differently, and asserting the union of both
/// spellings would be an assertion that cannot fail informatively):
/// - Plain `EXPLAIN` is the format that literally emits `vector search`, the wording
///   DECISION D1 and spec §12.1 use, and it renders the proof in ~17 readable lines:
///   `• vector search / table: concepts@concepts_embedding_idx (partial index)`.
/// - `OPT, VERBOSE` inlines the full 1024-element probe vector into the plan text,
///   producing a ~52 KB blob (measured). That is unusable as an on-camera artifact
///   and would make any assertion failure message unreadable — which is the entire
///   argument that had favoured it.
///
/// The plan below was re-verified against the test's **bound** `$1`/`$2` over the
/// extended protocol, not against literals: T7.3 round R2 established that a
/// parameterized `LIMIT` can change plan shape, so a literal-`LIMIT` measurement
/// would not have proven this test green.
#[tokio::test]
#[ignore = "camera-proof: set LAMBO_REQUIRE_VECTOR_INDEX=1 (spec §12.1 vector-index proof)"]
async fn vector_explain_camera_proof() {
    // Kept behind its own env gate (not merged into `conformance_suite`) so the
    // §12.1 claim is asserted only when someone is deliberately capturing it, and
    // so a cluster provisioned from an older migration fails LOUDLY here rather
    // than reporting a green suite. `conformance_suite`'s
    // `check_vector_explain_is_global_topk` independently proves the DECISION D1
    // *shape* (global top-k, no session-filtered anti-pattern index) on every run.
    if env::var_os("LAMBO_REQUIRE_VECTOR_INDEX").is_none() {
        eprintln!(
            "vector_explain_camera_proof: skipped; set LAMBO_REQUIRE_VECTOR_INDEX=1 \
                 against a cluster provisioned from migrations/cockroach/001_init.sql"
        );
        return;
    }
    let Some(dsn) = dsn_or_skip("vector_explain_camera_proof") else {
        return;
    };
    let store = new_store(&dsn);
    let pool = &store.pool().await.unwrap();
    let probe = encode_vector(&embed(0.5)).unwrap();
    // EXPLAIN the production statement ITSELF, not a hand-copied lookalike.
    // adve-review MINOR-4 caught the previous version claiming to be
    // "byte-for-byte" the vector-candidates query while actually dropping
    // its `::STRING` output casts. The casts cannot change index selection,
    // so the finding was cosmetic — but the entire value of this proof is
    // that it explains the query production runs, so the claim has to be
    // true by construction rather than by careful copying. B0 keeps that
    // property by reading the statement off **this store's own**
    // `DialectSql` (`store.sql`), the exact string `vector_candidates`
    // issues, so neither an edit to the SQL nor a change of dialect can
    // silently desynchronize the camera proof from it.
    let vector_candidates_sql = &store.sql.vector_candidates;
    let rows = sqlx::query(&format!("EXPLAIN {vector_candidates_sql}"))
        .bind(&probe)
        .bind(5i64)
        .fetch_all(pool)
        .await
        .map_err(backend)
        .unwrap();
    let plan: Vec<String> = rows
        .iter()
        .map(|r| r.try_get::<String, usize>(0).map_err(backend))
        .collect::<Result<_, _>>()
        .unwrap();
    let text = plan.join("\n");
    // The plan IS the artifact — print it so a `--nocapture` run is the camera shot.
    eprintln!("vector_explain_camera_proof plan:\n{text}");
    assert!(
        text.contains("vector search"),
        "EXPLAIN must show `vector search` (plain-EXPLAIN spelling), got:\n{text}"
    );
    assert!(
        text.contains("concepts@concepts_embedding_idx"),
        "EXPLAIN must show the vector search on concepts@concepts_embedding_idx, got:\n{text}"
    );
    // The exact regression T7.4 fixed: with a NON-partial index the predicate forces
    // `spans: FULL SCAN` on concepts_pkey. Assert it is gone, so a cluster that
    // silently reverts to a non-partial index fails with a pointed message.
    assert!(
        !text.contains("FULL SCAN"),
        "EXPLAIN must not fall back to a full scan (non-partial index?), got:\n{text}"
    );
}

/// Deletes every row a live test's unique session left in the shared
/// cluster when dropped (best effort; never panics). Children first: the
/// foreign keys point at `sessions` and `interactions`.
struct SessionRows {
    dsn: String,
    session: String,
}

impl Drop for SessionRows {
    fn drop(&mut self) {
        const TABLES: [&str; 11] = [
            "edges",
            "synonyms",
            "write_intents",
            "concepts",
            "interactions",
            "canonization_events",
            "reservations",
            "session_leases",
            "lease_refusals",
            "session_stats",
            "sessions",
        ];
        let (dsn, session) = (self.dsn.clone(), self.session.clone());
        crate::test_util::run_blocking(async move {
            let store = new_store(&dsn);
            let pool = match store.pool().await {
                Ok(p) => p,
                Err(e) => return eprintln!("cleanup: pool for {session}: {e}"),
            };
            for table in TABLES {
                if let Err(e) = sqlx::query(&format!("DELETE FROM {table} WHERE session_id = $1"))
                    .bind(&session)
                    .execute(&pool)
                    .await
                {
                    eprintln!("cleanup: {table} for {session}: {e}");
                }
            }
            pool.close().await;
        });
    }
}

/// Issue #30, live: the shared narrow `RecordAccess` update on
/// CockroachDB — the dialect where `GREATEST`'s operand typing and
/// `UPDATE … FROM (VALUES …)` are the risk — inside the fenced flush,
/// monotonic against a replay and an older presentation, read back by a
/// second pool (a writer restart), and kept below a later full upsert in
/// the same batch.
///
/// `#[ignore]`d like every live cockroach test. **Not run in CI**: the
/// `cockroach-live` job is disabled by operator decision (2026-10-06). Run:
/// `cargo test --features store-cockroach,fixtures --lib -- --ignored
/// access_counts_round_trip`.
#[tokio::test]
#[ignore = "live: requires LAMBO_COCKROACH_DSN"]
async fn access_counts_round_trip_through_the_narrow_update_on_cockroach() {
    let Some(dsn) = dsn_or_skip("access_counts_round_trip_through_the_narrow_update_on_cockroach")
    else {
        return;
    };
    use crate::store::lease::{LeaseHolder, LeaseOutcome};
    let store = new_store(&dsn);
    store.init_schema().await.expect("init_schema");
    let sid = SessionId::from(format!("issue-30-live-{}", Uuid::new_v4()));
    // Removes this test's session rows when it ends, on a panic too. The
    // cluster is shared, so only the unique session is touched.
    let _cleanup = SessionRows {
        dsn: dsn.clone(),
        session: sid.to_string(),
    };
    let holder = LeaseHolder {
        endpoint: None,
        agent: AgentId::new("issue-30"),
        pid: 30,
        host: "test".into(),
    };
    let LeaseOutcome::Acquired(info) = store
        .acquire_lease(&sid, &holder, Duration::from_secs(45))
        .await
        .expect("acquire")
    else {
        panic!("expected Acquired");
    };
    let token = Some(info.token);
    let t0 = chrono::Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap();
    let t = |s: i64| t0 + chrono::Duration::seconds(s);
    let (origin, cid) = (NodeId::new(), NodeId::new());
    let concept = crate::types::Concept {
        id: cid,
        session_id: sid.clone(),
        content: format!("read often {cid}"),
        canonical_key: format!("read often {cid}"),
        concept_type: ConceptType::Entity,
        origin_interaction: origin,
        origin_agent: AgentId::new("issue-30"),
        created_at: t0,
        access_count: 0,
        last_accessed: None,
        gc_survived: 1,
        canonization_status: crate::types::CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding: None,
        human_confirmed: 0,
        chunk_group_id: None,
    };
    let access = |n: i32, at: i64| Mutation::RecordAccess {
        session_id: sid.clone(),
        id: cid,
        access_count: n,
        last_accessed: t(at),
    };
    let batch = |mutations: Vec<Mutation>| MutationBatch {
        mutation_epoch: 5,
        gc_mark: Default::default(),
        mutations,
    };
    store
        .flush(
            &batch(vec![
                Mutation::UpsertNode {
                    node: Node::Interaction(Interaction {
                        event_time: None,
                        id: origin,
                        session_id: sid.clone(),
                        agent_id: AgentId::new("issue-30"),
                        prompt_text: Some("p".into()),
                        previous_id: None,
                        created_at: t0,
                    }),
                },
                Mutation::UpsertNode {
                    node: Node::Concept(concept.clone()),
                },
                Mutation::UpsertEdge {
                    // Every concept carries a Derives edge from its origin
                    // interaction (spec §5.7); a session without it is
                    // refused at load.
                    edge: crate::types::Edge {
                        id: NodeId::new(),
                        session_id: sid.clone(),
                        source: origin,
                        target: concept.id,
                        edge_type: crate::types::EdgeType::Derives,
                        weight: 0.9,
                        reinforcements: 0,
                        created_at: t0,
                        last_reinforced: t0,
                        event_time: None,
                    },
                },
            ]),
            token,
        )
        .await
        .expect("seed");
    store
        .flush(&batch(vec![access(3, 30)]), token)
        .await
        .expect("access");
    store
        .flush(&batch(vec![access(3, 30)]), token)
        .await
        .expect("replay");
    store
        .flush(&batch(vec![access(1, 10)]), token)
        .await
        .expect("older");
    let stale = store
        .flush(&batch(vec![access(7, 70)]), Some(info.token - 1))
        .await;
    assert!(
        matches!(stale, Err(StoreError::StaleWrite(_))),
        "the access update is fenced like every write, got {stale:?}"
    );

    let restarted = new_store(&dsn);
    let snap = restarted.load_session(&sid).await.expect("reload");
    let c = snap.concepts.iter().find(|c| c.id == cid).expect("concept");
    assert_eq!((c.access_count, c.last_accessed), (3, Some(t(30))));
    assert_eq!(c.gc_survived, 1, "no other column moves");
    assert_eq!(snap.mutation_epoch, 5);

    let mut later = c.clone();
    later.access_count = 9;
    later.last_accessed = Some(t(90));
    restarted
        .flush(
            &batch(vec![
                access(4, 40),
                Mutation::UpsertNode {
                    node: Node::Concept(later),
                },
                access(10, 100),
            ]),
            token,
        )
        .await
        .expect("mixed batch");
    let snap = restarted.load_session(&sid).await.expect("load");
    let c = snap.concepts.iter().find(|c| c.id == cid).unwrap();
    assert_eq!((c.access_count, c.last_accessed), (10, Some(t(100))));
    let _ = restarted.release_lease(&sid, &holder).await;
}

use super::*;
use crate::fixtures::load_snapshot;
use crate::MemoryStore;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;

// ---- Report schema (v1 — twin of H1's; see that module's doc) --------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ParityReport {
    schema_version: u32,
    harness: HarnessInfo,
    adapters: Vec<AdapterRun>,
    pairs: Vec<PairResult>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HarnessInfo {
    /// Populated by whatever captures this report, never by the harness
    /// itself. `null` here, like in H1's committed report; the capture
    /// rev lives in the evidence README instead.
    git_rev: Option<String>,
    features: Vec<String>,
    fixtures: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ScanKind {
    Exact,
    Ann,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Attribution {
    ExactMustMatch,
    AnnEnvelope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdapterRun {
    name: String,
    scan: ScanKind,
    index_present: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdDisplacement {
    id: String,
    rank_a: usize,
    rank_b: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairResult {
    fixture: String,
    probe: String,
    limit: usize,
    adapter_a: String,
    adapter_b: String,
    attribution: Attribution,
    candidate_jaccard: f64,
    rank_prefix_match: usize,
    displacement: Vec<IdDisplacement>,
    max_score_diff: f64,
    exact_match: bool,
}

// ---- Live-run constants ---------------------------------------------

/// The cluster column width: `concepts.embedding VECTOR(1024)` in
/// `migrations/cockroach/001_init.sql`. The corpus is the SAME SHAPE as
/// H1's synthetic leg (both fixture graphs, `synthetic_unit_vector`, the
/// same four probe shapes × five limits) at the width the live DDL
/// admits — width is a property of the store, not of the corpus design.
const DIM: usize = 1024;

/// Float32 round-trip noise bound for score agreement on the shared
/// `1 − d²/2 ≡ cosine` scale: the oracle computes f32 cosine directly;
/// Cockroach accumulates an f32 L2 over 1024 dims and converts with
/// `distance_to_score`. Both err by a few ulps (~1e-6 worst case); any
/// conversion skew worth catching (`1 − d`, a scale factor, …) lands 3+
/// orders of magnitude above this line. The measured maximum goes into
/// the report and evidence README regardless.
const SCORE_SKEW_EPSILON: f64 = 1e-4;

/// C-SPANN published 0.99 recall@50 at beam 64. At equal-size top-k sets
/// jaccard = inter/(2k − inter) and recall = inter/k, so jaccard ≥ 0.98
/// ⟺ recall ≥ 0.99: the published envelope expressed in the measure the
/// report already carries.
const ANN_JACCARD_FLOOR: f64 = 0.98;

fn synthetic_unit_vector(i: usize, dim: usize) -> Vec<f32> {
    let mut v: Vec<f32> = (0..dim)
        .map(|j| {
            // Same construction as H1's helper: integer math, one divide,
            // deterministic across runs and platforms.
            let k = ((i + 1) * 37 + (j + 1) * 11) % 97;
            (k as f32) / 97.0 - 0.5
        })
        .collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(norm > 1e-6, "degenerate synthetic vector for i={i}");
    for x in &mut v {
        *x /= norm;
    }
    v
}

fn h2_contract(dim: usize) -> EmbeddingContract {
    EmbeddingContract {
        kind: "fixture".into(),
        model: Some("h2-live-model".into()),
        dim,
    }
}

// ---- The memory oracle twin (same precedent as H1's) ----------------

/// `MemoryStore` plus an exact-cosine vector leg — reimplemented rather
/// than reused from H1's private module, per the precedent its doc
/// comment records (which itself cites F's `cosine_oracle`).
struct MemoryOracleStore {
    inner: MemoryStore,
}

impl MemoryOracleStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
        }
    }
}

#[async_trait]
impl GraphStore for MemoryOracleStore {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.inner.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities() | Capabilities::VECTOR_SEARCH
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
        _session: &SessionId,
        _embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        panic!("MemoryOracleStore: unchecked vector lookup is unused by the H2 harness")
    }
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        validate_vector_candidate_limit(limit)?;
        let snapshot = match self.inner.load_session(session).await {
            Ok(s) => s,
            Err(StoreError::SessionNotFound(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        match &snapshot.embedding {
            None => return Ok(Vec::new()),
            Some(durable) if durable == expected_contract => {}
            Some(durable) => {
                return Err(StoreError::Invariant(format!(
                    "vector candidate lookup refused after embedding contract changed: \
                         vectors were written by kind={} model={:?} dim={}, but the caller \
                         expects kind={} model={:?} dim={}",
                    durable.kind,
                    durable.model,
                    durable.dim,
                    expected_contract.kind,
                    expected_contract.model,
                    expected_contract.dim,
                )));
            }
        }
        // Same ordering contract as the real pg-family candidate read:
        // best first by `total_cmp`, ties broken by canonical key asc then
        // the smaller `NodeId` (issue #2), then truncated to `limit`.
        let mut scored: Vec<(Scored<NodeId>, &str)> = snapshot
            .concepts
            .iter()
            .filter_map(|c| {
                let vector = c.embedding.as_ref()?;
                Some((
                    Scored::new(c.id, f64::from(crate::embed::cosine(embedding, vector))),
                    c.canonical_key.as_str(),
                ))
            })
            .collect();
        scored.sort_by(|(a, a_key), (b, b_key)| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| tie_break_by_key(Some(a_key), &a.item, Some(b_key), &b.item))
        });
        let mut scored: Vec<Scored<NodeId>> = scored.into_iter().map(|(s, _)| s).collect();
        scored.truncate(limit);
        Ok(scored)
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
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
    ) -> Result<InteractionSpan, StoreError> {
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
}

// ---- The three agreement measures (twins of H1's) -------------------

fn jaccard(a: &[Scored<NodeId>], b: &[Scored<NodeId>]) -> f64 {
    let sa: HashSet<NodeId> = a.iter().map(|s| s.item).collect();
    let sb: HashSet<NodeId> = b.iter().map(|s| s.item).collect();
    if sa.is_empty() && sb.is_empty() {
        return 1.0;
    }
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    inter / union
}

fn rank_prefix_match(a: &[Scored<NodeId>], b: &[Scored<NodeId>]) -> usize {
    a.iter()
        .zip(b.iter())
        .take_while(|(x, y)| x.item == y.item)
        .count()
}

fn displacements(a: &[Scored<NodeId>], b: &[Scored<NodeId>]) -> Vec<IdDisplacement> {
    let rank_a: HashMap<NodeId, usize> = a.iter().enumerate().map(|(i, s)| (s.item, i)).collect();
    let rank_b: HashMap<NodeId, usize> = b.iter().enumerate().map(|(i, s)| (s.item, i)).collect();
    let mut out: Vec<IdDisplacement> = rank_a
        .iter()
        .filter_map(|(id, &ra)| {
            rank_b.get(id).and_then(|&rb| {
                (ra != rb).then_some(IdDisplacement {
                    id: id.0.to_string(),
                    rank_a: ra,
                    rank_b: rb,
                })
            })
        })
        .collect();
    out.sort_by_key(|d| d.rank_a);
    out
}

fn max_score_diff(a: &[Scored<NodeId>], b: &[Scored<NodeId>]) -> f64 {
    let scores_b: HashMap<NodeId, f64> = b.iter().map(|s| (s.item, s.score)).collect();
    a.iter()
        .filter_map(|s| scores_b.get(&s.item).map(|&sb| (s.score - sb).abs()))
        .fold(0.0_f64, f64::max)
}

// ---- Probe/limit grid (same shapes as H1/F) --------------------------

fn probe_set(pool: &[(NodeId, Vec<f32>)]) -> Vec<(&'static str, Vec<f32>)> {
    let first = &pool[0].1;
    let second = &pool[1].1;
    let negated: Vec<f32> = first.iter().map(|x| -x).collect();
    let midpoint: Vec<f32> = {
        let mut m: Vec<f32> = first
            .iter()
            .zip(second.iter())
            .map(|(a, b)| (a + b) / 2.0)
            .collect();
        let n = m.iter().map(|x| x * x).sum::<f32>().sqrt();
        for x in &mut m {
            *x /= n;
        }
        m
    };
    let off_axis = synthetic_unit_vector(usize::from(u8::MAX), DIM);
    vec![
        ("stored-itself", first.clone()),
        ("negated", negated),
        ("midpoint", midpoint),
        ("off-axis", off_axis),
    ]
}

struct Adapter {
    name: &'static str,
    scan: ScanKind,
    index_present: bool,
    store: Box<dyn GraphStore>,
}

// ---- DSN gating (same convention as the `conformance` module) --------

fn dsn() -> Option<String> {
    env::var("LAMBO_COCKROACH_DSN")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Same contract as `conformance::dsn_or_skip`: without a DSN this prints
/// a skip notice (surfacing as ignored, never skip-as-green), and under
/// `LAMBO_REQUIRE_LIVE=1` a missing DSN panics. The DSN is read from the
/// environment only and never printed.
fn dsn_or_skip(test: &str) -> Option<String> {
    match dsn() {
        Some(d) => Some(d),
        None => {
            if env::var_os("LAMBO_REQUIRE_LIVE").is_some() {
                panic!(
                    "{test}: LAMBO_COCKROACH_DSN is unset but LAMBO_REQUIRE_LIVE is set — \
                         refusing to skip a live cockroach test"
                );
            }
            eprintln!("SKIP {test}: LAMBO_COCKROACH_DSN not set");
            None
        }
    }
}

fn cfg(dsn: String) -> StoreConfig {
    StoreConfig {
        kind: crate::store::StoreKind::Cockroach,
        dsn: Some(dsn),
        path: None,
        vector_dim: None,
    }
}

/// One pool per run, used on ONE test runtime — see `conformance::new_store`.
fn new_store(dsn: &str) -> CockroachStore {
    CockroachStore::new(cfg(dsn.to_string())).unwrap()
}

// ---- Cluster-shape + index capture (read-only) -----------------------

/// Camera-proofs that the production query plans as `vector search` on
/// `concepts@concepts_embedding_idx` (DECISION D1, spec §12.1), and
/// RETURNS whether it does, so the caller can stamp the report's
/// `index_present` for the cockroach adapter from the measurement itself
/// rather than a hardcoded literal (H2-R1-2). The plan is printed so the
/// `--nocapture` log carries it.
async fn assert_index_backed(store: &CockroachStore) -> bool {
    let pool = &store.pool().await.unwrap();
    let probe = encode_vector(&synthetic_unit_vector(usize::from(u8::MAX), DIM)).unwrap();
    // The store's own composed statement, for the reason
    // `vector_explain_camera_proof` spells out: the plan has to be the plan
    // of the query production runs.
    let vector_candidates_sql = &store.sql.vector_candidates;
    let rows = sqlx::query(&format!("EXPLAIN {vector_candidates_sql}"))
        .bind(&probe)
        .bind(5i64)
        .fetch_all(pool)
        .await
        .unwrap();
    let text: String = rows
        .iter()
        .map(|r| r.try_get::<String, usize>(0).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    eprintln!("H2 EXPLAIN plan:\n{text}");
    let vector_search = text.contains("vector search");
    let index_hit = text.contains("concepts@concepts_embedding_idx");
    let no_full_scan = !text.contains("FULL SCAN");
    assert!(
        vector_search,
        "H2: EXPLAIN must show `vector search`, got:\n{text}"
    );
    assert!(
        index_hit,
        "H2: EXPLAIN must show concepts@concepts_embedding_idx, got:\n{text}"
    );
    assert!(
        no_full_scan,
        "H2: EXPLAIN must not fall back to a full scan, got:\n{text}"
    );
    vector_search && index_hit && no_full_scan
}

/// Read-only cluster shape for the evidence log: version string and the
/// full `SHOW CREATE TABLE concepts` (which carries the vector index DDL
/// with its parameters). Never prints connection material.
async fn log_cluster_shape(store: &CockroachStore) {
    let pool = &store.pool().await.unwrap();
    let version: String = sqlx::query_scalar("SELECT version()")
        .fetch_one(pool)
        .await
        .unwrap();
    eprintln!("H2 cluster version(): {version}");
    let beam_env = match env::var(VECTOR_BEAM_SIZE_ENV) {
        Ok(v) => format!("={v}"),
        Err(_) => "unset".to_string(),
    };
    eprintln!(
        "H2 beam size: default {DEFAULT_VECTOR_BEAM_SIZE} (LAMBO_VECTOR_BEAM_SIZE {beam_env})"
    );
    let ddl: Vec<String> = sqlx::query("SHOW CREATE TABLE concepts")
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .filter_map(|r| r.try_get::<Option<String>, usize>(1).ok().flatten())
        .collect();
    eprintln!("H2 SHOW CREATE TABLE concepts:\n{}\n", ddl.join("\n"));
}

// ---- Corpus seeding --------------------------------------------------

/// Re-home a fixture snapshot into a session scope this run owns: fresh
/// node ids (mapped consistently across interactions, origins and edges)
/// under a unique per-run session id, so re-runs and concurrent runs on a
/// persistent shared cluster can never cross-contaminate — the same
/// convention the conformance suite's unique-session-per-run follows.
fn rebase_into_fresh_scope(snap: &mut GraphSnapshot, suffix: &str) {
    let sid = SessionId::from(format!("{}-h2-{}", snap.session_id.as_str(), suffix));
    let mut ids: HashMap<NodeId, NodeId> = HashMap::new();
    for i in &snap.interactions {
        ids.insert(i.id, NodeId::new());
    }
    for c in &snap.concepts {
        ids.insert(c.id, NodeId::new());
    }
    for i in &mut snap.interactions {
        i.id = ids[&i.id];
        i.session_id = sid.clone();
        if let Some(p) = i.previous_id {
            i.previous_id = Some(ids[&p]);
        }
    }
    for c in &mut snap.concepts {
        c.id = ids[&c.id];
        c.session_id = sid.clone();
        c.origin_interaction = ids[&c.origin_interaction];
    }
    for e in &mut snap.edges {
        e.id = NodeId::new();
        e.session_id = sid.clone();
        e.source = ids[&e.source];
        e.target = ids[&e.target];
    }
    snap.session_id = sid;
}

/// Structure first (interactions are FK targets), then the contract stamp,
/// then the vector-bearing concepts — the write-gate ordering F1 relies on.
fn seed_batch(snap: &GraphSnapshot, contract: &EmbeddingContract) -> MutationBatch {
    let mut mutations: Vec<Mutation> = snap
        .interactions
        .iter()
        .map(|i| Mutation::UpsertNode {
            node: Node::Interaction(i.clone()),
        })
        .collect();
    mutations.push(Mutation::SetEmbedding {
        session_id: snap.session_id.clone(),
        embedding: Some(contract.clone()),
    });
    for c in &snap.concepts {
        mutations.push(Mutation::UpsertNode {
            node: Node::Concept(c.clone()),
        });
    }
    for e in &snap.edges {
        mutations.push(Mutation::UpsertEdge { edge: e.clone() });
    }
    MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations,
    }
}

fn active_features() -> Vec<String> {
    let mut v = vec!["fixtures".to_string()];
    for (flag, name) in [
        (cfg!(feature = "store-sqlite"), "store-sqlite"),
        (cfg!(feature = "store-memory"), "store-memory"),
        (cfg!(feature = "store-cockroach"), "store-cockroach"),
        (cfg!(feature = "embed-fixture"), "embed-fixture"),
    ] {
        if flag {
            v.push(name.to_string());
        }
    }
    v
}

// ---- The grid --------------------------------------------------------

/// One fixture rebased into the run's scope, seeded into every adapter,
/// then the identical probe × limit grid H1 runs, pairwise across all
/// adapters. Exact-scan pairs are asserted bit-for-bit equal; every pair
/// touching the ANN adapter is asserted against the skew and jaccard
/// bounds above AND carried in the report with its actual numbers.
async fn run_fixture_grid(
    fixture_label: &str,
    snap: &GraphSnapshot,
    contract: &EmbeddingContract,
    adapters: &mut [Adapter],
    report: &mut ParityReport,
) {
    let sid = snap.session_id.clone();
    let batch = seed_batch(snap, contract);
    // Idempotent on every adapter (`IF NOT EXISTS` DDL / in-memory
    // constructors); required here because the sqlite leg's in-memory
    // database starts empty.
    for a in adapters.iter() {
        a.store.init_schema().await.unwrap();
    }
    for a in adapters.iter() {
        a.store.flush(&batch, None).await.unwrap();
    }
    for a in adapters.iter() {
        if !report.adapters.iter().any(|r| r.name == a.name) {
            report.adapters.push(AdapterRun {
                name: a.name.to_string(),
                scan: a.scan,
                index_present: a.index_present,
            });
        }
    }
    if !report.harness.fixtures.iter().any(|f| f == fixture_label) {
        report.harness.fixtures.push(fixture_label.to_string());
    }

    let pool: Vec<(NodeId, Vec<f32>)> = snap
        .concepts
        .iter()
        .map(|c| {
            (
                c.id,
                c.embedding.clone().expect("harness vectored every concept"),
            )
        })
        .collect();
    let probes = probe_set(&pool);
    let limits = [1usize, 3, 5, pool.len(), pool.len() + 7];

    for i in 0..adapters.len() {
        for j in (i + 1)..adapters.len() {
            let (a_name, a_scan, a_store) =
                (&adapters[i].name, &adapters[i].scan, &adapters[i].store);
            let (b_name, b_scan, b_store) =
                (&adapters[j].name, &adapters[j].scan, &adapters[j].store);
            let attribution = if *a_scan == ScanKind::Exact && *b_scan == ScanKind::Exact {
                Attribution::ExactMustMatch
            } else {
                Attribution::AnnEnvelope
            };
            for (probe_label, probe) in &probes {
                for &limit in &limits {
                    let got_a = a_store
                        .vector_candidates_checked(&sid, probe, contract, limit)
                        .await
                        .unwrap();
                    let got_b = b_store
                        .vector_candidates_checked(&sid, probe, contract, limit)
                        .await
                        .unwrap();
                    // H2-R1-1 hardening, schema-v1-discipline choice:
                    // `PairResult` is pinned by report schema v1 (twin of
                    // H1's committed evidence), so candidate counts are
                    // NOT added as fields; instead the harness asserts
                    // non-emptiness inline. Without this, jaccard()'s
                    // both-empty → 1.0 special case would let an all-empty
                    // regression pass vacuously with a perfect-looking
                    // report. The quarantine leg (which legitimately
                    // expects empty answers) runs separately and is
                    // intentionally not covered here.
                    assert!(
                        !got_a.is_empty() && !got_b.is_empty(),
                        "H2: empty candidate set from {a_name} or {b_name} on \
                             fixture {fixture_label:?} probe {probe_label:?} limit \
                             {limit} — refusing to score a possibly-vacuous cell"
                    );
                    let pair = PairResult {
                        fixture: fixture_label.to_string(),
                        probe: (*probe_label).to_string(),
                        limit,
                        adapter_a: (*a_name).to_string(),
                        adapter_b: (*b_name).to_string(),
                        attribution,
                        candidate_jaccard: jaccard(&got_a, &got_b),
                        rank_prefix_match: rank_prefix_match(&got_a, &got_b),
                        displacement: displacements(&got_a, &got_b),
                        max_score_diff: max_score_diff(&got_a, &got_b),
                        exact_match: got_a == got_b,
                    };
                    if attribution == Attribution::ExactMustMatch {
                        assert!(
                            pair.exact_match,
                            "H2: exact-scan adapters {a_name} and {b_name} disagree on \
                                 fixture {fixture_label:?} probe {probe_label:?} limit {limit}: \
                                 jaccard={} prefix={} score_diff={} displacement={:?}",
                            pair.candidate_jaccard,
                            pair.rank_prefix_match,
                            pair.max_score_diff,
                            pair.displacement,
                        );
                    } else {
                        assert!(
                            pair.candidate_jaccard >= ANN_JACCARD_FLOOR,
                            "H2: ANN candidate divergence beyond the C-SPANN envelope on \
                                 {fixture_label:?} probe {probe_label:?} limit {limit} \
                                 ({a_name} vs {b_name}): jaccard {} < {ANN_JACCARD_FLOOR}",
                            pair.candidate_jaccard
                        );
                        assert!(
                            pair.max_score_diff <= SCORE_SKEW_EPSILON,
                            "H2: systematic score skew on the shared 1 − d²/2 scale \
                                 ({a_name} vs {b_name}, {fixture_label:?} probe {probe_label:?} \
                                 limit {limit}): max diff {} > {SCORE_SKEW_EPSILON}",
                            pair.max_score_diff
                        );
                    }
                    report.pairs.push(pair);
                }
            }
        }
    }
}

// ---- The quarantine-history leg --------------------------------------

/// F's reasoned-but-unmeasured question, settled with a measurement: the
/// SAME history of writes — stamp contract A (width 1024), write vectors,
/// restamp contract B (width 4, different model) — must produce the same
/// OBSERVABLE recall behaviour on Cockroach and SQLite: zero answers out
/// of the abandoned space. The mechanisms differ by design (Cockroach:
/// NULL-only quarantine + `VECTOR(1024)` DDL width enforcement; SQLite:
/// width-change restamp-quarantine + its own width gate); this leg
/// measures whether the recall behaviour does.
///
/// Needs SQLite compiled in for the comparison side; skips cleanly (with
/// a message) when it isn't.
async fn quarantine_restamp_leg(adapters: &[Adapter], suffix: &str) {
    let cr = adapters
        .iter()
        .find(|a| a.name == "cockroach")
        .expect("cockroach adapter always present");
    let Some(sq) = adapters.iter().find(|a| a.name == "sqlite") else {
        eprintln!(
            "H2 quarantine leg skipped: sqlite adapter not compiled in (build with \
                 --features store-sqlite to compare quarantine behaviour)"
        );
        return;
    };

    let mut snap = load_snapshot("session-drift").unwrap();
    rebase_into_fresh_scope(&mut snap, &format!("{suffix}-q"));
    let contract_a = h2_contract(DIM);
    for (i, c) in snap.concepts.iter_mut().enumerate() {
        c.embedding = Some(synthetic_unit_vector(i, DIM));
    }
    let batch = seed_batch(&snap, &contract_a);
    cr.store.init_schema().await.unwrap();
    sq.store.init_schema().await.unwrap();
    cr.store.flush(&batch, None).await.unwrap();
    sq.store.flush(&batch, None).await.unwrap();

    // Recall under A works on both before the restamp.
    let probe_a = synthetic_unit_vector(0, DIM);
    let cr_before = cr
        .store
        .vector_candidates_checked(&snap.session_id, &probe_a, &contract_a, 5)
        .await
        .unwrap();
    let sq_before = sq
        .store
        .vector_candidates_checked(&snap.session_id, &probe_a, &contract_a, 5)
        .await
        .unwrap();

    // Restamp to a DIFFERENT width — the history that makes the two
    // quarantine designs diverge mechanically.
    let contract_b = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("h2-restamp-model".into()),
        dim: 4,
    };
    let restamp = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![Mutation::SetEmbedding {
            session_id: snap.session_id.clone(),
            embedding: Some(contract_b.clone()),
        }],
    };
    cr.store.flush(&restamp, None).await.unwrap();
    sq.store.flush(&restamp, None).await.unwrap();

    // SQLite: the restamp-quarantine physically NULLed every concept
    // vector (observable via load_session), so the width-4 recall
    // question is VALID under the new stamp but answers EMPTY — the
    // old space is gone, not merely hidden.
    let sq_snap = sq.store.load_session(&snap.session_id).await.unwrap();
    let quarantined = sq_snap
        .concepts
        .iter()
        .filter(|c| c.embedding.is_none())
        .count();
    assert_eq!(
        quarantined,
        sq_snap.concepts.len(),
        "H2: sqlite restamp-quarantine must NULL every concept vector"
    );
    assert!(
        !sq_snap.concepts.is_empty(),
        "H2: quarantine leg seeded no concepts"
    );
    let probe_b = synthetic_unit_vector(1, 4);
    let sq_after = sq
        .store
        .vector_candidates_checked(&snap.session_id, &probe_b, &contract_b, 5)
        .await;
    let sq_after = sq_after.unwrap();
    assert!(
        sq_after.is_empty(),
        "H2: sqlite must answer empty (not stale vectors) after restamp-quarantine, \
             got {} candidates",
        sq_after.len()
    );

    // Cockroach: no physical restamp-quarantine fires (its NULL-only
    // quarantine targets the unstamped→stamped transition), but the
    // VECTOR(1024) DDL width enforcement refuses the width-4 question
    // outright — again, nothing from space A can answer.
    let cr_after = cr
        .store
        .vector_candidates_checked(&snap.session_id, &probe_b, &contract_b, 5)
        .await;
    assert!(
        cr_after.is_err(),
        "H2: cockroach must refuse a width-4 recall question (DDL width {DIM}), got {:?}",
        cr_after
    );

    eprintln!(
        "H2 quarantine leg: under contract A both answered non-empty (cockroach {}, \
             sqlite {}); after restamping B(dim 4): sqlite quarantined {quarantined}/{} \
             concept vectors to NULL and answered EMPTY; cockroach refused via DDL width \
             enforcement. Cross-space recall delivered: 0 candidates on BOTH adapters.",
        cr_before.len(),
        sq_before.len(),
        sq_snap.concepts.len(),
    );
}

/// **Acceptance: H2's Done-when box** — "live Cockroach run, skew zero on
/// the score scale, ANN divergence stated with numbers against the
/// C-SPANN envelope".
///
/// `#[ignore]`d like every live cockroach test. Run:
/// `LAMBO_REQUIRE_LIVE=1 cargo test --features store-cockroach,fixtures \
///  --lib store::pg::cockroach::h2_cockroach_parity -- --ignored --nocapture`
/// Set `LAMBO_H2_EMIT_EVIDENCE=1` to also write the JSON report to
/// `evidence/mooshik-h2-cockroach-parity/report.json`.
#[tokio::test]
#[ignore = "live: requires LAMBO_COCKROACH_DSN"]
async fn h2_live_cockroach_recall_parity() {
    let Some(dsn) = dsn_or_skip("h2_live_cockroach_recall_parity") else {
        return;
    };
    let suffix = Uuid::new_v4().simple().to_string();

    let mut report = ParityReport {
        schema_version: 1,
        harness: HarnessInfo {
            git_rev: None,
            features: active_features(),
            fixtures: Vec::new(),
        },
        adapters: Vec::new(),
        pairs: Vec::new(),
    };

    let mut adapters: Vec<Adapter> = Vec::new();
    let cr = new_store(&dsn);
    cr.init_schema().await.unwrap();
    log_cluster_shape(&cr).await;
    let index_present = assert_index_backed(&cr).await;
    adapters.push(Adapter {
        name: "cockroach",
        scan: ScanKind::Ann,
        index_present,
        store: Box::new(cr),
    });
    adapters.push(Adapter {
        name: "memory-oracle",
        scan: ScanKind::Exact,
        index_present: false,
        store: Box::new(MemoryOracleStore::new()) as Box<dyn GraphStore>,
    });
    #[cfg(feature = "store-sqlite")]
    adapters.push(Adapter {
        name: "sqlite",
        scan: ScanKind::Exact,
        index_present: false,
        store: Box::new(
            crate::store::SqliteStore::connect("sqlite::memory:")
                .unwrap()
                .with_vector_dim(DIM)
                .unwrap(),
        ) as Box<dyn GraphStore>,
    });

    // Pinned exactly: 2 fixtures × 4 probes × 5 limits × C(n, 2) pairs.
    let n_pairs_per_cell = adapters.len() * (adapters.len() - 1) / 2;

    let contract = h2_contract(DIM);

    for fixture in ["session-rest-api", "session-drift"] {
        let mut snap: GraphSnapshot = load_snapshot(fixture).unwrap();
        for (i, c) in snap.concepts.iter_mut().enumerate() {
            c.embedding = Some(synthetic_unit_vector(i, DIM));
        }
        run_fixture_grid(fixture, &snap, &contract, &mut adapters, &mut report).await;
    }

    assert_eq!(
        report.pairs.len(),
        2 * 4 * 5 * n_pairs_per_cell,
        "H2: grid matrix dimensions drifted"
    );

    // Harness honesty check (mirrors H1's): every recorded ExactMustMatch
    // row was asserted, not just reported.
    assert!(
        report
            .pairs
            .iter()
            .all(|p| p.attribution != Attribution::ExactMustMatch || p.exact_match),
        "H2: an ExactMustMatch pair was recorded without being asserted — harness bug"
    );

    quarantine_restamp_leg(&adapters, &suffix).await;

    // Summary numbers for the run log and the evidence README.
    let ann_pairs: Vec<&PairResult> = report
        .pairs
        .iter()
        .filter(|p| p.attribution == Attribution::AnnEnvelope)
        .collect();
    let min_jaccard = ann_pairs
        .iter()
        .map(|p| p.candidate_jaccard)
        .fold(f64::INFINITY, f64::min);
    let max_skew = report
        .pairs
        .iter()
        .map(|p| p.max_score_diff)
        .fold(0.0_f64, f64::max);
    let total_displacement: usize = report.pairs.iter().map(|p| p.displacement.len()).sum();
    let fully_agreeing_ann_cells = ann_pairs
        .iter()
        .filter(|p| p.displacement.is_empty() && p.candidate_jaccard == 1.0)
        .count();
    println!(
        "H2: {} pairs across fixtures ({}) — adapters {:?}; ANN pairs: min jaccard \
             {min_jaccard}, max score skew {max_skew}, total displaced ranks \
             {total_displacement}; fully-agreeing ANN cells: {fully_agreeing_ann_cells}/{}",
        report.pairs.len(),
        report.harness.fixtures.join(", "),
        report
            .adapters
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        ann_pairs.len(),
    );

    if env::var_os("LAMBO_H2_EMIT_EVIDENCE").is_some() {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/evidence/mooshik-h2-cockroach-parity"
        );
        std::fs::create_dir_all(dir).unwrap();
        let json = serde_json::to_string_pretty(&report).unwrap();
        std::fs::write(format!("{dir}/report.json"), json).unwrap();
    }
}

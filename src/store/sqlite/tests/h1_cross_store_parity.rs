use super::*;
use serde::{Deserialize, Serialize};

// ---- Report schema (v1) --------------------------------------------

/// H1 cross-store recall parity report — schema v1.
///
/// Emitted so H1 (this file: SQLite + memory oracle), H2 (live
/// Cockroach) and H3 (pgvector, once B3 lands) all produce ONE shape,
/// diffable across runs and across whichever adapters happened to be
/// reachable in a given run. See `dev-diary/lambo-for-mooshik/
/// H-cross-store-parity.md`, "What parity means here, precisely", for
/// what each measure is and why exact-scan vs ANN adapters are
/// attributed differently.
///
/// **Why this shape survives a third adapter untouched.** Nothing
/// here names a specific backend in a field: `adapters` and every
/// pair's `adapter_a`/`adapter_b` are free-text names ("sqlite",
/// "memory-oracle", "cockroach", "postgres", …), so H3 adds NEW ROWS
/// (postgres-vs-sqlite, postgres-vs-memory-oracle, postgres-vs-
/// cockroach if all three are reachable in one run) rather than new
/// FIELDS. The one per-adapter fact that varies by backend — whether
/// an index actually served the answer — is already a plain `bool`,
/// not an enum tied to one backend's `EXPLAIN` spelling: Cockroach's
/// `concepts_embedding_idx` probe (H2) and pgvector's `SET LOCAL
/// enable_indexscan = off` forced-exact lane (H3) both just set
/// `index_present` the same way. And every score in `pairs` is
/// already on the shared cosine scale BEFORE it reaches this report:
/// SQLite/memory-oracle return cosine directly, Cockroach converts L2
/// with `1 - d^2/2`, Postgres converts cosine distance with `1 - d`.
/// The report never needs a per-adapter conversion field.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ParityReport {
    /// Bump only for a breaking change (a field removed, retyped, or
    /// repurposed). Adding an optional field is not breaking and does
    /// not bump this.
    schema_version: u32,
    harness: HarnessInfo,
    /// One entry per adapter that actually ran, across every fixture.
    adapters: Vec<AdapterRun>,
    /// One entry per (fixture, probe, limit, adapter pair).
    pairs: Vec<PairResult>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HarnessInfo {
    /// Populated by whatever captured this report (e.g. `git rev-parse
    /// HEAD` at capture time), never by the harness itself — shelling
    /// out from a unit test is its own source of flakiness.
    git_rev: Option<String>,
    /// Cargo features active when this report was generated.
    features: Vec<String>,
    /// Fixture / corpus labels this run actually exercised.
    fixtures: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ScanKind {
    /// Full scan, exact cosine — no approximation anywhere in the path.
    Exact,
    /// Approximate nearest-neighbour index (C-SPANN, HNSW, ...).
    Ann,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Attribution {
    /// Both sides of the pair are exact-scan: any disagreement is
    /// adapter skew, i.e. a bug. The harness asserts this, not just
    /// reports it.
    ExactMustMatch,
    /// At least one side is an ANN adapter: divergence within a
    /// stated envelope is expected and only reported.
    AnnEnvelope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdapterRun {
    name: String,
    scan: ScanKind,
    /// Whether an index served the answer for this adapter in this
    /// run (vs. a full/forced-exact scan). Always `false` for H1:
    /// SQLite and the memory oracle are both linear scans by
    /// construction (see F-sqlite-vectors.md, "Exact scan, not an
    /// index").
    index_present: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdDisplacement {
    /// The shared node id (stringified UUID — plain text so this
    /// schema never depends on `NodeId`'s own serde shape).
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
    /// Jaccard similarity of the top-k id sets.
    candidate_jaccard: f64,
    /// Length of the longest shared ordered prefix.
    rank_prefix_match: usize,
    /// Per shared id, its rank under `adapter_a` vs `adapter_b`, for
    /// every id whose rank differs. Empty when every shared id ranks
    /// identically.
    displacement: Vec<IdDisplacement>,
    /// Largest absolute score difference over ids present in both
    /// answers (0.0 when neither side answered).
    max_score_diff: f64,
    /// `true` iff the two `Vec<Scored<NodeId>>` answers are bit-for-
    /// bit equal (same ids, same order, same `f64` scores) — the
    /// strongest of the three measures, and the one `ExactMustMatch`
    /// pairs are asserted against.
    exact_match: bool,
    /// The two answers the aggregates were computed from. Not
    /// serialized: the post-hoc exact-lane assertions re-read them to
    /// apply the round-trip-noise rule to `displacement`.
    #[serde(skip)]
    got_a: Vec<Scored<NodeId>>,
    #[serde(skip)]
    got_b: Vec<Scored<NodeId>>,
}

// ---- The "memory oracle" adapter -----------------------------------

/// `MemoryStore` plus an exact-cosine vector leg — H1's second
/// exact-scan `GraphStore`, independent of SQLite.
///
/// This is deliberately the same design as `crate::memory`'s private
/// `VectorSearchStore` test wrapper, reimplemented here rather than
/// reused: that struct is private to `memory.rs`'s own test module,
/// exactly the situation this file's `cosine_oracle` doc comment
/// already explains for F ("Reimplemented here rather than reused
/// because `VectorSearchStore` is private... That is not a weakness
/// of the comparison: the oracle is deliberately the naive
/// formulation"). Same reasoning, same precedent, one adapter over.
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
        panic!("MemoryOracleStore: unchecked vector lookup is unused by the H1 harness")
    }
    /// Same ordering contract as SQLite's `rank_by_cosine` and F's
    /// `cosine_oracle`: best first by `total_cmp`, ties broken by
    /// canonical key asc then the smaller `NodeId` (issue #2),
    /// truncated to `limit`.
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

// ---- The three agreement measures ----------------------------------

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

/// True when every rank displacement sits inside the f32 round-trip
/// bound: for each displaced id, the score gap to the id occupying its
/// other rank is within [`H3_SCORE_SKEW_EPSILON`] in BOTH lanes. A
/// transposition therefore passes only when the swapped pair is
/// effectively tied in each lane's own score view; a swap across a
/// real gap above the bound returns false and the caller asserts.
fn displacement_within_noise(
    a: &[Scored<NodeId>],
    b: &[Scored<NodeId>],
    disp: &[IdDisplacement],
) -> bool {
    let score_a: HashMap<String, f64> = a.iter().map(|s| (s.item.0.to_string(), s.score)).collect();
    let score_b: HashMap<String, f64> = b.iter().map(|s| (s.item.0.to_string(), s.score)).collect();
    disp.iter().all(|d| {
        let within = |x: &f64, y: &f64| (x - y).abs() <= H3_SCORE_SKEW_EPSILON;
        match (
            score_a.get(&d.id),
            a.get(d.rank_b).map(|s| &s.score),
            score_b.get(&d.id),
            b.get(d.rank_a).map(|s| &s.score),
        ) {
            (Some(xa), Some(ya), Some(xb), Some(yb)) => within(xa, ya) && within(xb, yb),
            _ => false,
        }
    })
}

// ---- Probe/limit grid (same shape as the F matrix above) -----------

/// Same probe shapes as `vector_candidates_agree_with_an_exact_cosine_
/// oracle_on_both_fixtures` above, generalised to any pool: a stored
/// vector itself, its negation, a midpoint between two stored
/// vectors, and an off-axis probe built from `synthetic_unit_vector`
/// regardless of where the pool's own vectors came from.
fn probe_set(pool: &[(NodeId, Vec<f32>)], dim: usize) -> Vec<(&'static str, Vec<f32>)> {
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
    let off_axis = synthetic_unit_vector(usize::from(u8::MAX), dim);
    vec![
        ("stored-itself", first.clone()),
        ("negated", negated),
        ("midpoint", midpoint),
        ("off-axis", off_axis),
    ]
}

/// Float32 round-trip noise bound for H3 score agreement on the
/// shared cosine scale. Matches H2: a conversion mix-up (`1 - d`
/// vs `1 - d^2/2`) lands orders of magnitude above this.
const H3_SCORE_SKEW_EPSILON: f64 = 1e-4;

/// How an adapter's `index_present` is established.
///
/// # Why this is not a `bool` any more (E2E-F3a)
///
/// It was, and the `postgres-exact` lane's `false` was a literal in the
/// adapter tuple. Replacing `PostgresDialect::forced_exact_scan_sql()`
/// with `None` therefore left H3 **green with two identical lanes**:
/// the "exact" lane forced nothing, the harness reported zero adapter
/// skew and a zero hnsw envelope from a comparison of a lane against
/// itself, and the one field that could have noticed was a constant.
/// A claim about which plan served an answer has to be read off the
/// plan.
#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "store-postgres"), allow(dead_code))]
enum IndexEvidence {
    /// Linear scan by construction: SQLite and the memory oracle have
    /// no vector index to use, so there is no plan to read (see
    /// F-sqlite-vectors.md, "Exact scan, not an index"). This is the
    /// one honest constant here.
    LinearByConstruction,
    /// A live Postgres lane: EXPLAIN the production recall SQL, after
    /// the corpus is seeded, through the same forced-exact path
    /// production search uses, and read the plan.
    ProbeLivePlan {
        dsn: String,
        dim: usize,
        forced_exact: bool,
    },
}

/// Resolve [`IndexEvidence`] into the `bool` the report carries.
///
/// Runs **after** seeding on purpose: on an empty or tiny table the
/// planner picks a sequential scan for every lane, so a probe taken
/// before the corpus exists would report `false` for the hnsw lane and
/// call it evidence.
async fn resolve_index_present(evidence: &IndexEvidence, probe: &[f32]) -> bool {
    match evidence {
        IndexEvidence::LinearByConstruction => false,
        IndexEvidence::ProbeLivePlan {
            dsn,
            dim,
            forced_exact,
        } => {
            #[cfg(feature = "store-postgres")]
            {
                use crate::store::pg::postgres::{corpus, PostgresStore};
                use crate::store::{StoreConfig, StoreKind};
                let store = PostgresStore::new(StoreConfig {
                    kind: StoreKind::Postgres,
                    dsn: Some(dsn.clone()),
                    path: None,
                    vector_dim: Some(*dim),
                })
                .expect("H3: index probe store");
                let store = if *forced_exact {
                    store.with_forced_exact_scan()
                } else {
                    store
                };
                corpus::index_present(&store, probe, 5).await
            }
            #[cfg(not(feature = "store-postgres"))]
            {
                let _ = (dsn, dim, forced_exact, probe);
                false
            }
        }
    }
}

/// Adapters reachable from this process, at this `dim`. H1 always
/// has sqlite + memory-oracle (neither needs a DSN). H3 appends
/// postgres-hnsw and postgres-exact when `postgres_dsn` is `Some`.
fn build_adapters(
    dim: usize,
    postgres_dsn: Option<&str>,
) -> Vec<(&'static str, ScanKind, IndexEvidence, Box<dyn GraphStore>)> {
    #[cfg_attr(not(feature = "store-postgres"), allow(unused_mut))]
    let mut adapters = vec![
        (
            "sqlite",
            ScanKind::Exact,
            IndexEvidence::LinearByConstruction,
            Box::new(vec_test_store(dim)) as Box<dyn GraphStore>,
        ),
        (
            "memory-oracle",
            ScanKind::Exact,
            IndexEvidence::LinearByConstruction,
            Box::new(MemoryOracleStore::new()) as Box<dyn GraphStore>,
        ),
    ];
    #[cfg(feature = "store-postgres")]
    if let Some(dsn) = postgres_dsn {
        adapters.extend(postgres_h3_adapters(dim, dsn));
    }
    #[cfg(not(feature = "store-postgres"))]
    let _ = postgres_dsn;
    adapters
}

#[cfg(feature = "store-postgres")]
fn postgres_h3_adapters(
    dim: usize,
    dsn: &str,
) -> Vec<(&'static str, ScanKind, IndexEvidence, Box<dyn GraphStore>)> {
    use crate::store::pg::postgres::PostgresStore;
    use crate::store::{StoreConfig, StoreKind};
    let cfg = StoreConfig {
        kind: StoreKind::Postgres,
        dsn: Some(dsn.to_string()),
        path: None,
        vector_dim: Some(dim),
    };
    let hnsw = PostgresStore::new(cfg.clone())
        .unwrap_or_else(|e| panic!("H3: PostgresStore (hnsw lane) construct failed: {e}"));
    let exact = PostgresStore::new(cfg)
        .unwrap_or_else(|e| panic!("H3: PostgresStore (exact lane) construct failed: {e}"))
        .with_forced_exact_scan();
    vec![
        (
            "postgres-hnsw",
            ScanKind::Ann,
            IndexEvidence::ProbeLivePlan {
                dsn: dsn.to_string(),
                dim,
                forced_exact: false,
            },
            Box::new(hnsw) as Box<dyn GraphStore>,
        ),
        (
            "postgres-exact",
            ScanKind::Exact,
            IndexEvidence::ProbeLivePlan {
                dsn: dsn.to_string(),
                dim,
                forced_exact: true,
            },
            Box::new(exact) as Box<dyn GraphStore>,
        ),
    ]
}

/// One grid run's corpus, bundled into a single parameter.
///
/// Bundled rather than passed positionally because B3's eighth
/// parameter (`postgres_dsn`) tripped `clippy::too_many_arguments`
/// and turned two CI rows red on the merged tree (E2E-F1). A struct
/// keeps the next leg from doing it again, and names each input at
/// every call site.
struct FixtureGrid<'a> {
    fixture_label: &'a str,
    sid: &'a SessionId,
    contract: &'a EmbeddingContract,
    batch: &'a MutationBatch,
    pool: &'a [(NodeId, Vec<f32>)],
    dim: usize,
    postgres_dsn: Option<&'a str>,
}

/// Seed one fixture's batch into every reachable adapter and run the
/// full probe × limit grid pairwise across them, asserting exact
/// agreement for every `ExactMustMatch` pair (H1's whole point: any
/// disagreement between two exact-scan adapters is adapter skew, a
/// bug, not something to merely record).
async fn run_fixture_grid(grid: FixtureGrid<'_>, report: &mut ParityReport) {
    let FixtureGrid {
        fixture_label,
        sid,
        contract,
        batch,
        pool,
        dim,
        postgres_dsn,
    } = grid;
    let adapters = build_adapters(dim, postgres_dsn);
    for (_, _, _, adapter) in &adapters {
        adapter.init_schema().await.unwrap();
        adapter.flush(batch, None).await.unwrap();
    }
    let probes = probe_set(pool, dim);

    // Probed after seeding, never before: see `resolve_index_present`.
    // The probe used is the grid's first, so the plan read here is the
    // plan the grid's own queries get.
    for (name, scan, evidence, _) in &adapters {
        if !report.adapters.iter().any(|a| a.name == *name) {
            report.adapters.push(AdapterRun {
                name: (*name).to_string(),
                scan: *scan,
                index_present: resolve_index_present(evidence, &probes[0].1).await,
            });
        }
    }
    if !report.harness.fixtures.iter().any(|f| f == fixture_label) {
        report.harness.fixtures.push(fixture_label.to_string());
    }

    let limits = [1usize, 3, 5, pool.len(), pool.len() + 7];

    for i in 0..adapters.len() {
        for j in (i + 1)..adapters.len() {
            let (a_name, a_scan, _, a_store) = &adapters[i];
            let (b_name, b_scan, _, b_store) = &adapters[j];
            let attribution = if *a_scan == ScanKind::Exact && *b_scan == ScanKind::Exact {
                Attribution::ExactMustMatch
            } else {
                Attribution::AnnEnvelope
            };
            for (probe_label, probe) in &probes {
                for &limit in &limits {
                    let got_a = a_store
                        .vector_candidates_checked(sid, probe, contract, limit)
                        .await
                        .unwrap();
                    let got_b = b_store
                        .vector_candidates_checked(sid, probe, contract, limit)
                        .await
                        .unwrap();
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
                        got_a: got_a.clone(),
                        got_b: got_b.clone(),
                    };
                    if attribution == Attribution::ExactMustMatch {
                        let postgres_exact =
                            *a_name == "postgres-exact" || *b_name == "postgres-exact";
                        if postgres_exact {
                            // Same ids as the other exact adapter;
                            // scores may differ by f32 round-trip
                            // through pgvector. A copied Cockroach
                            // formula shows up as ~0.375 skew at
                            // cosine=0.5, well above the bound.
                            //
                            // Order must agree too, EXCEPT inside a
                            // pair whose scores sit within the
                            // round-trip bound in both lanes. Since
                            // issue #2 an exact f64 tie breaks on the
                            // canonical key, while pgvector's f32
                            // distance can see a strict order at
                            // ~1e-9 and keep score order: the H3
                            // midpoint probe of "user schema" and
                            // "create user" ties in f64 (key order
                            // puts "creat user" first) but is strict
                            // in f32 the other way. Both lanes are
                            // faithful to their own scores, so such
                            // a swap is round-trip noise, not skew;
                            // a swap across a real score gap above
                            // the bound still fails below.
                            assert_eq!(
                                got_a.len(),
                                got_b.len(),
                                "H3: postgres-exact candidate count drifted on \
                                         fixture {fixture_label:?} probe {probe_label:?} \
                                         limit {limit}"
                            );
                            assert!(
                                (pair.candidate_jaccard - 1.0).abs() < f64::EPSILON,
                                "H3: postgres-exact adapter skew (jaccard {}) on \
                                         fixture {fixture_label:?} probe {probe_label:?} \
                                         limit {limit}: displacement={:?}",
                                pair.candidate_jaccard,
                                pair.displacement,
                            );
                            let noise_ok = displacement_within_noise(
                                &pair.got_a,
                                &pair.got_b,
                                &pair.displacement,
                            );
                            assert!(
                                noise_ok,
                                "H3: postgres-exact rank displacement above the \
                                         round-trip bound on fixture {fixture_label:?} \
                                         probe {probe_label:?} limit {limit}: {:?}",
                                pair.displacement,
                            );
                            // No separate rank-prefix assert here: same-id
                            // sets plus the noise rule above already pin the
                            // full ordering modulo round-trip ties, and ids
                            // sliding past a displaced pair legitimately
                            // break prefix positions between the old and new
                            // ranks.
                            assert!(
                                pair.max_score_diff <= H3_SCORE_SKEW_EPSILON,
                                "H3: postgres-exact conversion skew {} > \
                                         {H3_SCORE_SKEW_EPSILON} on fixture \
                                         {fixture_label:?} probe {probe_label:?} limit \
                                         {limit}",
                                pair.max_score_diff,
                            );
                        } else {
                            assert!(
                                pair.exact_match,
                                "H1: exact-scan adapters {a_name} and {b_name} disagree on \
                                         fixture {fixture_label:?} probe {probe_label:?} limit \
                                         {limit}: jaccard={} prefix={} score_diff={} \
                                         displacement={:?}",
                                pair.candidate_jaccard,
                                pair.rank_prefix_match,
                                pair.max_score_diff,
                                pair.displacement,
                            );
                        }
                    } else {
                        assert!(
                            !got_a.is_empty() && !got_b.is_empty(),
                            "H3: empty candidate set from {a_name} or {b_name} on \
                                     fixture {fixture_label:?} probe {probe_label:?} limit \
                                     {limit}: refusing to score a possibly-vacuous cell"
                        );
                        assert!(
                            pair.max_score_diff <= H3_SCORE_SKEW_EPSILON,
                            "H3: systematic score skew on the shared cosine scale \
                                     ({a_name} vs {b_name}, {fixture_label:?} probe \
                                     {probe_label:?} limit {limit}): max diff {} > \
                                     {H3_SCORE_SKEW_EPSILON}",
                            pair.max_score_diff,
                        );
                    }
                    report.pairs.push(pair);
                }
            }
        }
    }
}

// ---- Leg 1: the two committed fixture graphs, synthetic vectors ----

/// The required half of H1: both committed fixture graphs, a stamped
/// contract, and `synthetic_unit_vector` — same construction as the F
/// matrix above, run cross-adapter instead of adapter-vs-oracle.
async fn run_synthetic_leg(report: &mut ParityReport, postgres_dsn: Option<&str>) {
    const DIM: usize = 8;
    for fixture in ["session-rest-api", "session-drift"] {
        let snap: GraphSnapshot = crate::fixtures::load_snapshot(fixture).unwrap();
        let sid = snap.session_id.clone();
        let contract = vec_contract(DIM);

        let pool: Vec<(NodeId, Vec<f32>)> = snap
            .concepts
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id, synthetic_unit_vector(i, DIM)))
            .collect();

        // Structure first (interactions are FK targets for concepts),
        // then the contract, then the vector-bearing concepts — the
        // same write-gate ordering F1 relies on.
        let mut mutations = snapshot_to_batch(&snap).mutations;
        mutations.push(Mutation::SetEmbedding {
            session_id: sid.clone(),
            embedding: Some(contract.clone()),
        });
        for (i, c) in snap.concepts.iter().enumerate() {
            let mut concept = c.clone();
            concept.embedding = Some(synthetic_unit_vector(i, DIM));
            mutations.push(Mutation::UpsertNode {
                node: NodeKind::Concept(concept),
            });
        }
        let batch = MutationBatch {
            mutation_epoch: 0,
            gc_mark: Default::default(),
            mutations,
        };

        run_fixture_grid(
            FixtureGrid {
                fixture_label: fixture,
                sid: &sid,
                contract: &contract,
                batch: &batch,
                pool: &pool,
                dim: DIM,
                postgres_dsn,
            },
            report,
        )
        .await;
    }
}

// ---- Leg 2 (optional): the real BGE-M3 evidence corpus --------------

/// Best-effort: seeds H1's grid from the committed real-embedder
/// corpus (`evidence/mooshik-f-sqlite-bge/f-bge.db`) instead of
/// synthetic vectors, so cross-store agreement is also checked on
/// vectors an actual embedder produced. Optional by design (H doc,
/// "H1 — the harness"): a missing or unreadable corpus skips this leg
/// with a message rather than failing the test — the required
/// evidence is the synthetic leg above. Returns whether it ran.
async fn run_real_embedder_leg(report: &mut ParityReport) -> bool {
    let src = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/evidence/mooshik-f-sqlite-bge/f-bge.db"
    );
    if !std::path::Path::new(src).exists() {
        eprintln!("H1: no real-embedder corpus at {src}, skipping optional leg");
        return false;
    }
    // Copy rather than open in place: a live pool may write WAL/SHM
    // sidecars next to whatever it opens, and the committed evidence
    // file must never pick up untracked write artifacts.
    let scratch = crate::test_util::ScratchDir::new("lambo-h1-bge");
    let tmp = scratch.join("corpus.db");
    if std::fs::copy(src, &tmp).is_err() {
        eprintln!("H1: could not copy the real-embedder corpus, skipping optional leg");
        return false;
    }
    let loaded: Result<GraphSnapshot, StoreError> = async {
        let source = SqliteStore::connect(tmp.to_str().unwrap())?;
        // Idempotent (`CREATE TABLE IF NOT EXISTS`): adds any table
        // this schema has grown since the corpus was captured
        // (`lease_refusals`, `write_intents`), touches no existing
        // column or row.
        source.init_schema().await?;
        source
            .load_session(&SessionId::from("f-bge-semantic"))
            .await
    }
    .await;
    let _ = std::fs::remove_file(&tmp);
    let snap = match loaded {
        Ok(s) => s,
        Err(e) => {
            eprintln!("H1: could not load the real-embedder corpus ({e}), skipping optional leg");
            return false;
        }
    };
    let Some(contract) = snap.embedding.clone() else {
        eprintln!("H1: real-embedder corpus carries no embedding contract, skipping optional leg");
        return false;
    };
    let pool: Vec<(NodeId, Vec<f32>)> = snap
        .concepts
        .iter()
        .filter_map(|c| c.embedding.clone().map(|v| (c.id, v)))
        .collect();
    if pool.len() < 2 {
        eprintln!("H1: real-embedder corpus has fewer than 2 embedded concepts, skipping");
        return false;
    }
    let dim = contract.dim;

    // Unlike the synthetic leg, these concepts already carry real
    // vectors, so the contract mutation only needs to land BEFORE
    // them — no separate raw-then-vector pass.
    let mut mutations = Vec::new();
    for i in &snap.interactions {
        mutations.push(Mutation::UpsertNode {
            node: NodeKind::Interaction(i.clone()),
        });
    }
    mutations.push(Mutation::SetEmbedding {
        session_id: snap.session_id.clone(),
        embedding: Some(contract.clone()),
    });
    for c in &snap.concepts {
        mutations.push(Mutation::UpsertNode {
            node: NodeKind::Concept(c.clone()),
        });
    }
    for e in &snap.edges {
        mutations.push(Mutation::UpsertEdge { edge: e.clone() });
    }
    let batch = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations,
    };

    run_fixture_grid(
        FixtureGrid {
            fixture_label: "mooshik-f-sqlite-bge (real bge_m3 embedder)",
            sid: &snap.session_id,
            contract: &contract,
            batch: &batch,
            pool: &pool,
            dim,
            postgres_dsn: None,
        },
        report,
    )
    .await;
    true
}

fn active_features() -> Vec<String> {
    let mut v = vec!["fixtures".to_string()];
    for (flag, name) in [
        (cfg!(feature = "store-sqlite"), "store-sqlite"),
        (cfg!(feature = "store-memory"), "store-memory"),
        (cfg!(feature = "store-cockroach"), "store-cockroach"),
        (cfg!(feature = "store-postgres"), "store-postgres"),
        (cfg!(feature = "embed-fixture"), "embed-fixture"),
        (cfg!(feature = "embed-bge"), "embed-bge"),
    ] {
        if flag {
            v.push(name.to_string());
        }
    }
    v
}

/// **Acceptance: H1's Done-when box** — "H1 harness runs the full grid
/// against SQLite + memory-oracle anywhere, with exact agreement
/// asserted (this much runs in normal CI)". Not `#[ignore]`d: unlike
/// H2/H3, nothing here needs a DSN.
///
/// Set `LAMBO_H1_EMIT_EVIDENCE=1` to also write the JSON report to
/// `evidence/mooshik-h1-cross-store-parity/report.json` (skipped by
/// default so a normal `cargo test` never touches the working tree).
#[tokio::test]
async fn h1_sqlite_and_memory_oracle_agree_exactly() {
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

    run_synthetic_leg(&mut report, None).await;
    // Pinned exactly, not `>=`: the optional leg below adds more pairs
    // when it runs, which would mask a drop in the synthetic leg's
    // own count under a loose bound. 2 fixtures × 4 probes × 5 limits
    // × 1 adapter pair (sqlite, memory-oracle).
    assert_eq!(
        report.pairs.len(),
        2 * 4 * 5,
        "H1: synthetic-leg matrix dimensions drifted"
    );
    let ran_real_leg = run_real_embedder_leg(&mut report).await;

    assert!(
        report
            .pairs
            .iter()
            .all(|p| p.attribution != Attribution::ExactMustMatch || p.exact_match),
        "H1: an ExactMustMatch pair was recorded without being asserted — harness bug"
    );
    println!(
        "H1: {} pairs checked across {} fixture(s) ({}), {} adapters, real-embedder \
                 leg {}",
        report.pairs.len(),
        report.harness.fixtures.len(),
        report.harness.fixtures.join(", "),
        report.adapters.len(),
        if ran_real_leg { "ran" } else { "skipped" },
    );

    if std::env::var_os("LAMBO_H1_EMIT_EVIDENCE").is_some() {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/evidence/mooshik-h1-cross-store-parity"
        );
        std::fs::create_dir_all(dir).unwrap();
        let json = serde_json::to_string_pretty(&report).unwrap();
        std::fs::write(format!("{dir}/report.json"), json).unwrap();
    }
}

/// Drops the throwaway database an H3 live test created, on every exit
/// path (pass, assert failure, panic): `DROP DATABASE … WITH (FORCE)`
/// also closes the connections the test's stores still hold. Best
/// effort, never panics. Declare it right after `CREATE DATABASE`, so
/// the stores declared later drop first.
#[cfg(feature = "store-postgres")]
struct H3Db {
    admin_dsn: String,
    db: String,
}

#[cfg(feature = "store-postgres")]
impl Drop for H3Db {
    fn drop(&mut self) {
        let (admin_dsn, db) = (self.admin_dsn.clone(), self.db.clone());
        crate::test_util::run_blocking(async move {
            let admin = match sqlx::PgPool::connect(&admin_dsn).await {
                Ok(p) => p,
                Err(e) => return eprintln!("cleanup: connect admin for {db}: {e}"),
            };
            if let Err(e) = sqlx::query(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"))
                .execute(&admin)
                .await
            {
                eprintln!("cleanup: drop {db}: {e}");
            }
            admin.close().await;
        });
    }
}

/// **Acceptance: H3**: the same harness against pgvector.
/// Forced-exact lane (`postgres-exact`) must show zero adapter skew
/// vs sqlite/memory-oracle (same ids and order, score within
/// [`H3_SCORE_SKEW_EPSILON`]). The hnsw lane's divergence is an
/// envelope, printed with numbers.
///
/// `#[ignore]`d: needs `LAMBO_POSTGRES_DSN` against the pinned
/// `pgvector/pgvector:pg17` digest. Run:
/// `LAMBO_REQUIRE_LIVE=1 cargo test --features store-postgres,store-sqlite,fixtures \
///  --lib h3_postgres_recall_parity -- --ignored --nocapture`
#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn h3_postgres_recall_parity() {
    use crate::store::pg::postgres::{dsn_for_database, postgres_dsn_or_skip};

    let Some(admin_dsn) = postgres_dsn_or_skip("h3_postgres_recall_parity") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&admin_dsn)
        .await
        .unwrap_or_else(|e| panic!("H3: connect admin DSN: {e}"));
    let db = format!("lambo_h3_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {db}"))
        .execute(&admin)
        .await
        .unwrap_or_else(|e| panic!("H3: create {db}: {e}"));
    let _db_guard = H3Db {
        admin_dsn: admin_dsn.clone(),
        db: db.clone(),
    };
    let dsn = dsn_for_database(&admin_dsn, &db);

    // E2E-F4: the camera-proof EXPLAIN that used to sit here has been
    // removed rather than repaired. It ran on the table `init_schema`
    // had just created (**zero rows**), with a probe of
    // `vec![0.0; 8]` (which gives `NaN` for `<=>` against every row),
    // and asserted only under `SET LOCAL enable_seqscan = off`, a GUC
    // that penalises the alternative rather than testing the planner's
    // judgement. It could not fail. The plan is now proved twice, both
    // times on a corpus above the planner's crossover with a real
    // probe: `store::pg::postgres::tests::explain_recall_uses_hnsw` for
    // the production query, and `h3_postgres_hnsw_envelope_at_scale`
    // for the two H3 lanes. The grid below reads `index_present` off
    // the live plan instead of a literal, which is what M3 (return
    // `None` from `forced_exact_scan_sql`) now dies on.

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
    run_synthetic_leg(&mut report, Some(&dsn)).await;

    // 2 fixtures × 4 probes × 5 limits × C(4,2)=6 adapter pairs.
    assert_eq!(
        report.pairs.len(),
        2 * 4 * 5 * 6,
        "H3: synthetic-leg matrix dimensions drifted"
    );
    assert!(
        report
            .adapters
            .iter()
            .any(|a| a.name == "postgres-hnsw" && a.scan == ScanKind::Ann),
        "H3: postgres-hnsw adapter missing"
    );
    // `index_present` is now read off the plan for both Postgres
    // lanes (E2E-F3a), and what it reads at this corpus size is
    // `true` for the hnsw lane and `false` for the forced-exact one.
    //
    // B-E2E-R2-2: the comment that used to sit here said `false` for
    // both, "far below the planner's crossover", and the probe printed
    // `true` two lines under it. The probe was right and the prose was
    // wrong, for a reason worth writing down: the grid seeds through
    // `flush()` and never `ANALYZE`s, so `pg_class.reltuples` is still
    // -1 and the planner costs the hnsw lane against a fabricated
    // estimate (209 rows against a real 22) rather than against the
    // corpus that is actually there. Measured on the pinned digest by
    // EXPLAINing the same lane before and after an `ANALYZE concepts`:
    // Index Scan using concepts_embedding_idx before, Seq Scan after.
    // `PLANNER_CROSSOVER_ROWS` was measured on the corpus helper,
    // which does ANALYZE, so it does not describe this path at all.
    //
    // None of that changes what this leg is for. The fixture grid's
    // job is exact cross-adapter agreement, and the envelope it prints
    // is structurally zero for a reason that has nothing to do with
    // which plan ran: `hnsw.ef_search` defaults to 40, above both
    // fixture corpora, so the index visits the whole corpus and
    // returns the exact answer whichever way it is reached. The
    // envelope that can move is `h3_postgres_hnsw_envelope_at_scale`.
    let indexed = |name: &str| {
        report
            .adapters
            .iter()
            .find(|a| a.name == name)
            .unwrap_or_else(|| panic!("H3: {name} adapter missing"))
            .index_present
    };
    assert!(
        report
            .adapters
            .iter()
            .any(|a| a.name == "postgres-exact" && a.scan == ScanKind::Exact),
        "H3: postgres-exact adapter missing"
    );
    assert!(
        !indexed("postgres-exact"),
        "H3: the forced-exact lane must never plan through concepts_embedding_idx"
    );
    // The diagnostic below states the hnsw lane's value, so the value
    // is asserted rather than narrated: a sentence and a measurement
    // that disagree is precisely what B-E2E-R2-2 filed.
    assert!(
        indexed("postgres-hnsw"),
        "H3: the hnsw lane's plan probe says it did NOT use \
                 concepts_embedding_idx. That is not a failure of hnsw: it means the \
                 fixture corpus now carries real statistics (an ANALYZE somewhere in \
                 the seed path, or an autovacuum that beat the probe), so the planner \
                 costs 22 rows honestly and picks a Seq Scan. Rewrite the comment and \
                 the diagnostic here to say so; do not delete this assertion."
    );
    eprintln!(
        "H3 fixture-grid plan probe: postgres-hnsw index_present={}, \
                 postgres-exact index_present={}. That is the plan the planner chose, \
                 not a statement about corpus size: the grid seeds through flush() and \
                 never ANALYZEs, so reltuples is -1 and the hnsw lane is costed against \
                 a fabricated estimate and takes the index over 22 real rows. The \
                 envelope below is still structurally zero, for the ef_search reason \
                 rather than the plan reason: ef_search defaults to 40, above both \
                 fixture corpora (9 and 22 rows), so the index returns the exact answer \
                 whichever way it is reached. The load-bearing assertion here is the \
                 forced-exact lane's false, which its GUC settles whatever the \
                 estimates say. The envelope that can move is \
                 h3_postgres_hnsw_envelope_at_scale.",
        indexed("postgres-hnsw"),
        indexed("postgres-exact"),
    );

    let exact_pairs: Vec<&PairResult> = report
        .pairs
        .iter()
        .filter(|p| {
            p.attribution == Attribution::ExactMustMatch
                && (p.adapter_a == "postgres-exact" || p.adapter_b == "postgres-exact")
        })
        .collect();
    assert!(
        !exact_pairs.is_empty(),
        "H3: no postgres-exact ExactMustMatch pairs"
    );
    for p in &exact_pairs {
        assert!(
            (p.candidate_jaccard - 1.0).abs() < f64::EPSILON,
            "H3 forced-exact adapter skew: {p:?}"
        );
        assert!(
            displacement_within_noise(&p.got_a, &p.got_b, &p.displacement),
            "H3 forced-exact rank displacement above the round-trip \
                     bound: {p:?}"
        );
        assert!(
            p.max_score_diff <= H3_SCORE_SKEW_EPSILON,
            "H3 forced-exact conversion skew {}: {p:?}",
            p.max_score_diff
        );
    }

    let hnsw_vs_exact: Vec<&PairResult> = report
        .pairs
        .iter()
        .filter(|p| {
            matches!(
                (p.adapter_a.as_str(), p.adapter_b.as_str()),
                ("postgres-hnsw", "postgres-exact") | ("postgres-exact", "postgres-hnsw")
            )
        })
        .collect();
    assert_eq!(
        hnsw_vs_exact.len(),
        2 * 4 * 5,
        "H3: hnsw-vs-exact envelope cells drifted"
    );
    let min_jaccard = hnsw_vs_exact
        .iter()
        .map(|p| p.candidate_jaccard)
        .fold(1.0_f64, f64::min);
    let max_score = hnsw_vs_exact
        .iter()
        .map(|p| p.max_score_diff)
        .fold(0.0_f64, f64::max);
    let max_disp = hnsw_vs_exact
        .iter()
        .map(|p| p.displacement.len())
        .max()
        .unwrap_or(0);
    eprintln!(
        "H3 hnsw envelope vs forced-exact AT THE FIXTURE CORPUS SIZE (9 and 22 \
                 vectors, dim 8): cells={}, min_jaccard={min_jaccard}, \
                 max_score_diff={max_score}, max_displacement_ids={max_disp}. This is not \
                 an hnsw measurement: at this n the planner scans sequentially in both \
                 lanes, so zero divergence is arithmetic, not evidence. See \
                 h3_postgres_hnsw_envelope_at_scale for the measured envelope.",
        hnsw_vs_exact.len()
    );

    println!(
        "H3: {} pairs across {} fixture(s) ({}), {} adapters; \
                 forced-exact skew cells={}, hnsw envelope min_jaccard={min_jaccard} \
                 max_score_diff={max_score}",
        report.pairs.len(),
        report.harness.fixtures.len(),
        report.harness.fixtures.join(", "),
        report.adapters.len(),
        exact_pairs.len(),
    );

    if std::env::var_os("LAMBO_H3_EMIT_EVIDENCE").is_some() {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/evidence/mooshik-h3-postgres-parity"
        );
        std::fs::create_dir_all(dir).unwrap();
        let json = serde_json::to_string_pretty(&report).unwrap();
        std::fs::write(format!("{dir}/report.json"), json).unwrap();
    }
}

/// Corpus size for the scale leg: ten times the measured planner
/// crossover, so the hnsw lane is genuinely served by the index and
/// the forced-exact lane is genuinely a sequential scan.
#[cfg(feature = "store-postgres")]
const H3_SCALE_ROWS: usize = 5_000;

/// Width for the scale leg. The fixture grid runs at dim 8, where
/// vectors are nearly collinear and hnsw has almost nothing to
/// approximate. 768 is a width a real deployment uses and the width
/// the envelope was independently re-measured at.
#[cfg(feature = "store-postgres")]
const H3_SCALE_DIM: usize = 768;

/// **Acceptance: H3's second half** — "the hnsw lane's divergence
/// stated as a measured envelope".
///
/// # What was wrong with measuring it in the fixture grid (E2E-F3b)
///
/// The grid's corpora are the two committed fixture graphs: **9 and 22
/// vectors at dim 8**. hnsw visits every one of them at that size, and
/// the planner does not use the index at all, so the lane labelled
/// "hnsw" returns the exact answer by construction. The envelope came
/// out zero, was published as zero, and could not have come out any
/// other way. B chose hnsw from day one on the reasoning that "if hnsw
/// disappoints, that is discovered early, on unimportant data"; the
/// only instrument that could discover it was pointed at a corpus where
/// it structurally cannot fire.
///
/// # What this measures
///
/// [`H3_SCALE_ROWS`] deterministic unit vectors at [`H3_SCALE_DIM`],
/// probes drawn from the corpus so each probe's own row is the exact
/// rank-1 answer, through the production `vector_candidates_checked`
/// entry point on both lanes.
///
/// Three things are asserted; the envelope itself is **reported**, not
/// bounded, because the box asks for a number and a bound would be an
/// invented policy:
///
/// 1. The hnsw lane's plan names `concepts_embedding_idx` and the
///    forced-exact lane's does not. Both read off `EXPLAIN`. Replace
///    `forced_exact_scan_sql()` with `None` and this fails, which is
///    the mutation H3 used to survive.
/// 2. The forced-exact lane really is exact: its score sequence matches
///    a cosine ranking computed here in Rust over the seeded vectors,
///    and its top hit is the probe's own row at score 1. Compared as a
///    score sequence rather than an id sequence so exact ties cannot
///    make it flaky.
/// 3. The envelope cells are non-vacuous (both lanes answered).
///
/// # A note on the number this prints
///
/// A uniformly random high-dimensional corpus is close to the worst
/// case for hnsw: all pairwise cosines cluster near zero, so everything
/// past the self-match is a near-tie and recall past rank 1 is
/// meaningless to the index. A clustered corpus from a real embedder
/// does considerably better. The number is not the finding. The finding
/// is that the envelope is now measured somewhere it can move.
///
/// `#[ignore]`d and DSN-gated exactly like the other live tests, and
/// wired into the `postgres-live` CI job beside them, rather than
/// shrunk to a size that fits a default `cargo test`.
#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn h3_postgres_hnsw_envelope_at_scale() {
    use crate::store::pg::postgres::corpus;
    use crate::store::pg::postgres::PostgresStore;
    use crate::store::pg::postgres::{dsn_for_database, postgres_dsn_or_skip};
    use crate::store::{StoreConfig, StoreKind};

    let Some(admin_dsn) = postgres_dsn_or_skip("h3_postgres_hnsw_envelope_at_scale") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&admin_dsn)
        .await
        .unwrap_or_else(|e| panic!("H3 scale: connect admin DSN: {e}"));
    let db = format!("lambo_h3s_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {db}"))
        .execute(&admin)
        .await
        .unwrap_or_else(|e| panic!("H3 scale: create {db}: {e}"));
    let _db_guard = H3Db {
        admin_dsn: admin_dsn.clone(),
        db: db.clone(),
    };
    let dsn = dsn_for_database(&admin_dsn, &db);

    let cfg = StoreConfig {
        kind: StoreKind::Postgres,
        dsn: Some(dsn.clone()),
        path: None,
        vector_dim: Some(H3_SCALE_DIM),
    };
    let hnsw = PostgresStore::new(cfg.clone()).expect("H3 scale: hnsw lane");
    let exact = PostgresStore::new(cfg)
        .expect("H3 scale: exact lane")
        .with_forced_exact_scan();
    hnsw.init_schema().await.expect("H3 scale: init_schema");

    let sid = SessionId::from("h3-scale");
    let contract = vec_contract(H3_SCALE_DIM);
    let seeded = corpus::seed(&hnsw, sid.as_str(), &contract, H3_SCALE_ROWS).await;
    assert_eq!(seeded.len(), H3_SCALE_ROWS);

    let probe_rows = [0usize, 1, H3_SCALE_ROWS / 2, H3_SCALE_ROWS - 1];
    let limits = [5usize, 10, 20, 40];

    // (1) The lanes really are two different plans, at this size.
    let sample = &seeded[probe_rows[0]].1;
    let hnsw_plan = corpus::plan(&hnsw, sample, 20).await;
    let exact_plan = corpus::plan(&exact, sample, 20).await;
    eprintln!(
        "H3 scale plan (hnsw lane):\n{}",
        corpus::elide_vector_literals(&hnsw_plan)
    );
    eprintln!(
        "H3 scale plan (forced-exact lane):\n{}",
        corpus::elide_vector_literals(&exact_plan)
    );
    assert!(
        hnsw_plan.contains("concepts_embedding_idx"),
        "H3 scale: the hnsw lane must be served by the index at {H3_SCALE_ROWS} rows, \
                 or this is not an hnsw measurement. Got:\n{hnsw_plan}"
    );
    assert!(
        !exact_plan.contains("concepts_embedding_idx"),
        "H3 scale: the forced-exact lane must not use the index. A lane that does \
                 is the hnsw lane wearing a different name, and every 'zero skew' number \
                 below is a comparison of a lane against itself. Got:\n{exact_plan}"
    );
    assert!(
        exact_plan.contains("Seq Scan on concepts"),
        "H3 scale: the forced-exact lane must actually scan sequentially. \
                 Got:\n{exact_plan}"
    );

    let mut report = ParityReport {
        schema_version: 1,
        harness: HarnessInfo {
            git_rev: None,
            features: active_features(),
            fixtures: vec![format!(
                "synthetic-scale ({H3_SCALE_ROWS} unit vectors, dim {H3_SCALE_DIM})"
            )],
        },
        adapters: vec![
            AdapterRun {
                name: "postgres-hnsw".into(),
                scan: ScanKind::Ann,
                index_present: corpus::index_present(&hnsw, sample, 20).await,
            },
            AdapterRun {
                name: "postgres-exact".into(),
                scan: ScanKind::Exact,
                index_present: corpus::index_present(&exact, sample, 20).await,
            },
        ],
        pairs: Vec::new(),
    };
    assert!(
        report.adapters[0].index_present && !report.adapters[1].index_present,
        "H3 scale: index_present must be probed and must separate the lanes: {:?}",
        report.adapters
    );

    for &row in &probe_rows {
        let (probe_id, probe) = &seeded[row];
        // The exact answer, computed here rather than asked of the
        // database: this is what makes "the forced-exact lane is exact"
        // a proof instead of a label.
        let mut oracle: Vec<(uuid::Uuid, f64)> = seeded
            .iter()
            .map(|(id, v)| (*id, f64::from(crate::embed::cosine(probe, v))))
            .collect();
        oracle.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        for &limit in &limits {
            let got_hnsw = hnsw
                .vector_candidates_checked(&sid, probe, &contract, limit)
                .await
                .expect("H3 scale: hnsw lane");
            let got_exact = exact
                .vector_candidates_checked(&sid, probe, &contract, limit)
                .await
                .expect("H3 scale: exact lane");
            assert_eq!(got_hnsw.len(), limit, "H3 scale: hnsw under-returned");
            assert_eq!(got_exact.len(), limit, "H3 scale: exact under-returned");

            // (2) The forced-exact lane matches the oracle. Compared as
            // a score sequence: two rows at an identical distance may
            // come back in either order without either answer being
            // wrong, but their scores are the same either way.
            assert_eq!(
                got_exact[0].item.0, *probe_id,
                "H3 scale: the exact lane's rank-1 for a probe drawn from the corpus \
                         must be the probe's own row (limit {limit})"
            );
            assert!(
                (got_exact[0].score - 1.0).abs() < H3_SCORE_SKEW_EPSILON,
                "H3 scale: a unit vector against itself must score 1, got {}",
                got_exact[0].score
            );
            for (rank, hit) in got_exact.iter().enumerate() {
                assert!(
                    (hit.score - oracle[rank].1).abs() < H3_SCORE_SKEW_EPSILON,
                    "H3 scale: the forced-exact lane is not exact at rank {rank} \
                             (limit {limit}): store {} vs oracle {}",
                    hit.score,
                    oracle[rank].1,
                );
            }

            report.pairs.push(PairResult {
                fixture: report.harness.fixtures[0].clone(),
                probe: format!("corpus-row-{row}"),
                limit,
                adapter_a: "postgres-hnsw".into(),
                adapter_b: "postgres-exact".into(),
                attribution: Attribution::AnnEnvelope,
                candidate_jaccard: jaccard(&got_hnsw, &got_exact),
                rank_prefix_match: rank_prefix_match(&got_hnsw, &got_exact),
                displacement: displacements(&got_hnsw, &got_exact),
                max_score_diff: max_score_diff(&got_hnsw, &got_exact),
                exact_match: got_hnsw == got_exact,
                got_a: got_hnsw.clone(),
                got_b: got_exact.clone(),
            });
        }
    }

    // (3) The envelope, reported per limit as a real range.
    assert_eq!(
        report.pairs.len(),
        probe_rows.len() * limits.len(),
        "H3 scale: envelope matrix dimensions drifted"
    );
    eprintln!(
        "H3 hnsw envelope at {H3_SCALE_ROWS} vectors, dim {H3_SCALE_DIM}, \
                 pgvector defaults, probes drawn from the corpus:"
    );
    for &limit in &limits {
        let cells: Vec<&PairResult> = report.pairs.iter().filter(|p| p.limit == limit).collect();
        let min_jaccard = cells
            .iter()
            .map(|p| p.candidate_jaccard)
            .fold(1.0_f64, f64::min);
        let max_jaccard = cells
            .iter()
            .map(|p| p.candidate_jaccard)
            .fold(0.0_f64, f64::max);
        let max_score = cells
            .iter()
            .map(|p| p.max_score_diff)
            .fold(0.0_f64, f64::max);
        let max_disp = cells
            .iter()
            .map(|p| p.displacement.len())
            .max()
            .unwrap_or(0);
        let self_hit = cells.iter().filter(|p| p.rank_prefix_match >= 1).count();
        eprintln!(
            "  k={limit:>3}: jaccard(hnsw, exact) in [{min_jaccard:.3}, \
                     {max_jaccard:.3}], max_score_diff={max_score:.6}, \
                     max_displaced_ids={max_disp}, probes whose rank-1 agrees: \
                     {self_hit}/{}",
            cells.len()
        );
    }

    if std::env::var_os("LAMBO_H3_EMIT_EVIDENCE").is_some() {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/evidence/mooshik-h3-postgres-parity"
        );
        std::fs::create_dir_all(dir).unwrap();
        let json = serde_json::to_string_pretty(&report).unwrap();
        std::fs::write(format!("{dir}/report-scale.json"), json).unwrap();
    }
}

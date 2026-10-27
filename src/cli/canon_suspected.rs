//! `lambo canon suspected` (#198, owner decision 6): report the pairs of an
//! existing session the suspected-supersession trigger would flag, and with
//! `--apply` record them.
//!
//! The trigger flags only new writes. This is the one-off look at what a
//! session already holds (the dogfood store has 184 guard-refused pairs at
//! cosine >= 0.85), and its run on a real store is the real-world
//! precision check for the owner's precision bar.
//!
//! * **Without `--apply`** it works like `canon dry-run`: the SQLite file
//!   is hashed and copied into a private scratch directory, only the copy is
//!   opened (read-only), the scratch directory is removed, and the input is
//!   hashed again (`source_unchanged`). Nothing is written.
//! * **With `--apply`** it opens the input itself, takes the session's
//!   single-writer lease (refused while a writer holds it), writes every
//!   pair not yet recorded as a `SuspectedSupersedes` edge in one fenced
//!   flush, and releases the lease.
//!
//! The cosine floor defaults to the merge threshold the session's stored
//! embedding contract calibrates to (#87), else
//! [`crate::graph::hybrid::SEMANTIC_MATCH_THRESHOLD_DEFAULT`]. The command
//! reads no `lambo.toml`, so a deployment with another threshold passes
//! `--cosine`.

use std::path::PathBuf;

use serde::Serialize;
use sqlx::sqlite::SqliteConnectOptions;

#[cfg(test)]
use super::imp::fingerprint;
use super::imp::{
    guarded, install_signals, load_copy, redact_store, removal_invariant, source_check, Scratch,
};
use super::{CommonArgs, Outcome, PolicyChoice};
use crate::cli::caps::{check_size_cli, require_nonempty, CliError};
use crate::graph::suspected::{scan, ScannedPair, Stamp};
use crate::graph::Graph;
use crate::store::lease::{LeaseHolder, LeaseOutcome, LEASE_TTL};
use crate::store::{GraphStore, SqliteStore};
use crate::types::{AgentId, EmbeddingContract, Node, NodeId};
use crate::types::{SessionId, StoreError};

/// The agent id the `--apply` lease holder is stamped with.
pub const SUSPECTED_AGENT: &str = "lambo-canon-suspected";

/// `lambo canon suspected`'s options.
#[derive(Clone, Debug)]
pub struct SuspectedArgs {
    /// The SQLite store file.
    pub store: PathBuf,
    /// The session.
    pub session: String,
    /// The cosine floor; `None` for the session contract's merge threshold.
    pub cosine: Option<f64>,
    /// Record the pairs (takes the session's lease).
    pub apply: bool,
    /// JSON instead of text.
    pub json: bool,
    /// Where the private copy goes (default: the system temp directory).
    pub scratch: Option<PathBuf>,
}

/// One reported pair.
#[derive(Clone, Debug, Serialize)]
pub struct PairLine {
    /// The suspected replacement.
    pub newer: NodeId,
    /// The concept it may replace.
    pub older: NodeId,
    /// Excerpts of their texts.
    pub newer_text: String,
    pub older_text: String,
    /// Their concept type.
    pub concept_type: String,
    /// The cosine of their stored vectors.
    pub cosine: f64,
    /// Whether the edge was already recorded before this run.
    pub recorded: bool,
}

/// The report, text or JSON.
#[derive(Clone, Debug, Serialize)]
pub struct SuspectedReport {
    pub session: String,
    /// The cosine floor used.
    pub cosine: f64,
    /// Where the floor came from: `flag`, `contract` or `default`.
    pub cosine_source: &'static str,
    pub pairs: Vec<PairLine>,
    /// Edges written by `--apply`; `None` on a dry run.
    pub applied: Option<usize>,
    /// What every run checks (the dry run's source and scratch invariants).
    pub invariants: Vec<crate::canon::dry_run::Invariant>,
}

/// The floor and where it came from.
fn floor(
    flag: Option<f64>,
    contract: Option<&EmbeddingContract>,
) -> Result<(f64, &'static str), CliError> {
    if let Some(f) = flag {
        if !(f.is_finite() && (0.0..=1.0).contains(&f)) {
            return Err(CliError::Usage(format!(
                "--cosine {f}: a cosine floor in [0, 1] is expected"
            )));
        }
        return Ok((f, "flag"));
    }
    Ok(match contract.and_then(crate::embed::calibration::lookup) {
        Some(row) => (row.merge_threshold, "contract"),
        None => (
            crate::graph::hybrid::SEMANTIC_MATCH_THRESHOLD_DEFAULT,
            "default",
        ),
    })
}

fn lines(g: &Graph, pairs: &[ScannedPair]) -> Vec<PairLine> {
    let text = |id: NodeId| match g.node(id) {
        Some(Node::Concept(c)) => (crate::recall::format::excerpt(&c.content), c.concept_type),
        _ => (String::new(), crate::types::ConceptType::Logic),
    };
    pairs
        .iter()
        .map(|p| {
            let (newer_text, ty) = text(p.pair.newer);
            let (older_text, _) = text(p.pair.older);
            PairLine {
                newer: p.pair.newer,
                older: p.pair.older,
                newer_text,
                older_text,
                concept_type: format!("{ty:?}"),
                cosine: p.cosine,
                recorded: p.recorded,
            }
        })
        .collect()
}

/// The text form.
pub fn render(r: &SuspectedReport) -> String {
    use crate::recall::format::short_id;
    let recorded = r.pairs.iter().filter(|p| p.recorded).count();
    let mut out = format!(
        "lambo canon suspected: session {}, cosine >= {:.4} ({})\n{} pair(s) the trigger would \
         flag, {recorded} already recorded\n",
        r.session,
        r.cosine,
        r.cosine_source,
        r.pairs.len()
    );
    for p in &r.pairs {
        out.push_str(&format!(
            "  {:.4} {} newer {} \"{}\"\n         older {} \"{}\"{}\n",
            p.cosine,
            p.concept_type,
            short_id(p.newer),
            p.newer_text,
            short_id(p.older),
            p.older_text,
            if p.recorded { " (recorded)" } else { "" }
        ));
    }
    match r.applied {
        Some(n) => out.push_str(&format!("applied: {n} edge(s) written\n")),
        None => out.push_str(&format!(
            "dry run: nothing written; --apply records the {} not yet recorded\n",
            r.pairs.len() - recorded
        )),
    }
    for i in r.invariants.iter().filter(|i| !i.held) {
        out.push_str(&format!("invariant {} failed: {}\n", i.name, i.detail));
    }
    out
}

fn output(report: &SuspectedReport, json: bool) -> Result<Outcome, CliError> {
    let ok = report.invariants.iter().all(|i| i.held);
    let output = if json {
        serde_json::to_string_pretty(report).map_err(|e| CliError::Runtime(e.to_string()))?
    } else {
        render(report)
    };
    Ok(Outcome { output, ok })
}

/// The dry run, on a private copy.
async fn dry(args: SuspectedArgs) -> Result<Outcome, CliError> {
    let common = CommonArgs {
        store: args.store.clone(),
        session: args.session.clone(),
        policy: PolicyChoice::Both,
        at: None,
        json: args.json,
        quiet: 1,
        max_cycles: 1,
        top: 0,
        assume_survived: false,
        scratch: args.scratch.clone(),
        watch: Vec::new(),
        supersede: false,
    };
    let stop = install_signals()?;
    let scratch = Scratch::new(common.scratch.as_deref())?;
    let dir = scratch.path().to_path_buf();
    let c = common.clone();
    let flag = args.cosine;
    let work = async move {
        let loaded = load_copy(&c, &dir, chrono::Utc::now(), &|| {}).await?;
        let (cosine, cosine_source) = floor(flag, loaded.snapshot.embedding.as_ref())?;
        let g = Graph::from_snapshot(loaded.snapshot)
            .map_err(|e| CliError::Runtime(format!("load the session: {e}")))?;
        let pairs = scan(&g, cosine);
        Ok((
            lines(&g, &pairs),
            cosine,
            cosine_source,
            loaded.facts,
            loaded.before,
        ))
    };
    let ((pairs, cosine, cosine_source, mut facts, before), removed) =
        guarded(scratch, work, stop).await?;
    let report = SuspectedReport {
        session: args.session,
        cosine,
        cosine_source,
        pairs,
        applied: None,
        invariants: vec![
            removal_invariant(removed),
            source_check(&common, &before, &mut facts),
        ],
    };
    output(&report, args.json)
}

/// `--apply`: on the input, under the session's lease.
async fn apply(args: SuspectedArgs) -> Result<Outcome, CliError> {
    let meta = std::fs::metadata(&args.store)
        .map_err(|e| CliError::Usage(format!("--store {}: {e}", args.store.display())))?;
    if !meta.is_file() {
        return Err(CliError::Usage(format!(
            "--store {}: not a file (a SQLite store file is expected)",
            args.store.display()
        )));
    }
    let store = SqliteStore::new(SqliteConnectOptions::new().filename(&args.store));
    store
        .preflight_schema()
        .await
        .map_err(|e| CliError::Runtime(redact_store(&e)))?;
    let session = SessionId::new(&args.session);
    let holder = LeaseHolder::for_this_process(&AgentId::new(SUSPECTED_AGENT));
    let token = match store
        .acquire_lease(&session, &holder, LEASE_TTL)
        .await
        .map_err(|e| CliError::Runtime(redact_store(&e)))?
    {
        LeaseOutcome::Acquired(info) => info.token,
        LeaseOutcome::Held { current, age } => {
            return Err(CliError::Runtime(format!(
                "session {session} is held by another writer ({}, holding the lease for {}s); \
                 nothing was written. Stop it, or run without --apply to report only.",
                current.holder,
                age.as_secs()
            )));
        }
    };
    let result = async {
        let snapshot = match store.load_session(&session).await {
            Ok(s) => s,
            Err(StoreError::SessionNotFound(_)) => {
                return Err(CliError::Usage(format!(
                    "session {:?} is not in {}",
                    args.session,
                    args.store.display()
                )))
            }
            Err(e) => return Err(CliError::Runtime(redact_store(&e))),
        };
        let (cosine, cosine_source) = floor(args.cosine, snapshot.embedding.as_ref())?;
        let mut g = Graph::from_snapshot(snapshot)
            .map_err(|e| CliError::Runtime(format!("load the session: {e}")))?;
        let _ = g.drain_log();
        let pairs = scan(&g, cosine);
        let now = chrono::Utc::now();
        let session_id = g.session_id().clone();
        let mut applied = 0;
        for p in pairs.iter().filter(|p| !p.recorded) {
            if crate::graph::suspected::record(
                &mut g,
                p.pair,
                Stamp {
                    created_at: now,
                    event_time: None,
                },
                &session_id,
            )
            .map_err(|e| CliError::Runtime(e.to_string()))?
            {
                applied += 1;
            }
        }
        let batch = g.drain_log();
        if !batch.mutations.is_empty() {
            store
                .flush(&batch, Some(token))
                .await
                .map_err(|e| CliError::Runtime(redact_store(&e)))?;
        }
        Ok(SuspectedReport {
            session: args.session.clone(),
            cosine,
            cosine_source,
            pairs: lines(&g, &pairs),
            applied: Some(applied),
            invariants: Vec::new(),
        })
    }
    .await;
    let released = store.release_lease(&session, &holder).await;
    let report = result?;
    if let Err(e) = released {
        return Err(CliError::Runtime(format!(
            "{} edge(s) written, but the lease was not released (it expires on its own): {}",
            report.applied.unwrap_or(0),
            redact_store(&e)
        )));
    }
    output(&report, args.json)
}

/// Run `lambo canon suspected`.
pub(super) async fn run(args: SuspectedArgs) -> Result<Outcome, CliError> {
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;
    if args.apply {
        apply(args).await
    } else {
        dry(args).await
    }
}

#[cfg(all(test, feature = "fixtures"))]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use chrono::TimeZone;
    use uuid::Uuid;

    use super::*;
    use crate::test_util::ScratchDir;
    use crate::types::{CanonizationStatus, Concept, ConceptType, EdgeType, Interaction};

    const SESSION: &str = "suspected-cli";

    fn t(minutes: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 10, 1, 12, 0, 0).unwrap()
            + chrono::Duration::minutes(minutes)
    }

    fn unit(axis: usize) -> Vec<f32> {
        let mut v = vec![0.0; 8];
        v[axis] = 1.0;
        v
    }

    /// A session with: a suspected pair (8080 then 8081, one vector); a
    /// subject-slot pair (J1/J2, one vector); a pair already declared
    /// (`against`); and a far concept. Contract `fixture`, 8 dims.
    fn graph() -> Graph {
        let sid = SessionId::new(SESSION);
        let mut g = Graph::new(sid.clone());
        let mut prev = None;
        for n in 0..4u64 {
            let id = NodeId(Uuid::from_u64_pair(0x198c, n));
            g.insert_interaction(Interaction {
                id,
                session_id: sid.clone(),
                agent_id: AgentId::new("agent-a"),
                prompt_text: None,
                previous_id: prev,
                created_at: t(n as i64 * 10),
                event_time: None,
            })
            .unwrap();
            prev = Some(id);
        }
        g.stamp_embedding(EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 8,
        })
        .unwrap();
        let concepts: [(u64, &str, u64, usize); 7] = [
            (1, "the embedder listens on port 8080", 0, 0),
            (2, "the embedder listens on port 8081", 1, 0),
            (3, "J1 round-1 verdict: approve", 0, 1),
            (4, "J2 round-1 verdict: approve", 1, 1),
            (5, "the cache holds 10 entries", 0, 2),
            (6, "the cache holds 12 entries", 1, 2),
            (7, "an unrelated observation", 2, 3),
        ];
        for (n, content, origin, axis) in concepts {
            let origin = NodeId(Uuid::from_u64_pair(0x198c, origin));
            g.insert_concept(
                Concept {
                    id: NodeId(Uuid::from_u64_pair(0x198d, n)),
                    session_id: sid.clone(),
                    content: content.into(),
                    canonical_key: content.to_lowercase(),
                    concept_type: ConceptType::Logic,
                    origin_interaction: origin,
                    origin_agent: AgentId::new("agent-a"),
                    created_at: t(0),
                    access_count: 0,
                    last_accessed: None,
                    gc_survived: 0,
                    human_confirmed: 0,
                    canonization_status: CanonizationStatus::None,
                    blast_radius: None,
                    last_demotion_time: None,
                    embedding: Some(unit(axis)),
                    chunk_group_id: None,
                    embedding_source: None,
                },
                origin,
            )
            .unwrap();
        }
        // 6 is declared against 5: settled, never suspected.
        g.upsert_edge(crate::types::Edge {
            event_time: None,
            id: NodeId::new(),
            session_id: sid,
            source: cid(6),
            target: cid(5),
            edge_type: EdgeType::Contradicts,
            weight: 1.0,
            reinforcements: 1,
            created_at: t(10),
            last_reinforced: t(10),
        })
        .unwrap();
        g
    }

    fn cid(n: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(0x198d, n))
    }

    async fn fixture_db(dir: &Path) -> PathBuf {
        use std::str::FromStr;
        let path = dir.join("store.db");
        let options = SqliteConnectOptions::from_str(path.to_str().unwrap())
            .unwrap()
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete);
        let store = SqliteStore::new(options);
        store.init_schema().await.unwrap();
        store.seed(&graph().snapshot()).await.unwrap();
        drop(store);
        path
    }

    fn args(store: PathBuf, scratch: &Path, apply: bool) -> SuspectedArgs {
        SuspectedArgs {
            store,
            session: SESSION.into(),
            cosine: None,
            apply,
            json: true,
            scratch: Some(scratch.to_path_buf()),
        }
    }

    async fn edges_in(path: &Path) -> Vec<(NodeId, NodeId)> {
        let store = SqliteStore::new(SqliteConnectOptions::new().filename(path).read_only(true));
        let snap = store.load_session(&SessionId::new(SESSION)).await.unwrap();
        let mut out: Vec<(NodeId, NodeId)> = snap
            .edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::SuspectedSupersedes)
            .map(|e| (e.source, e.target))
            .collect();
        out.sort_by_key(|(s, _)| s.0);
        out
    }

    #[test]
    fn the_scan_finds_the_value_change_only() {
        let g = graph();
        let pairs = scan(&g, 0.85);
        assert_eq!(pairs.len(), 1, "{pairs:?}");
        assert_eq!(pairs[0].pair.newer, cid(2));
        assert_eq!(pairs[0].pair.older, cid(1));
        assert!((pairs[0].cosine - 1.0).abs() < 1e-6);
        assert!(!pairs[0].recorded);
    }

    /// The dry run reports the pair and writes nothing: the input's hash is
    /// unchanged and it holds no suspected edge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_dry_run_reports_and_writes_nothing() {
        let input = ScratchDir::new("lambo-suspected-input");
        let scratch = ScratchDir::new("lambo-suspected-scratch");
        let path = fixture_db(&input).await;
        let before = fingerprint(&path).unwrap();
        let out = run(args(path.clone(), &scratch, false)).await.unwrap();
        assert!(out.ok, "{}", out.output);
        let report: serde_json::Value = serde_json::from_str(&out.output).unwrap();
        assert_eq!(report["applied"], serde_json::Value::Null);
        assert_eq!(report["cosine_source"], "default");
        assert_eq!(report["pairs"].as_array().unwrap().len(), 1);
        assert_eq!(report["pairs"][0]["newer"], cid(2).0.to_string());
        assert_eq!(report["pairs"][0]["older"], cid(1).0.to_string());
        assert_eq!(report["pairs"][0]["recorded"], false);
        assert_eq!(fingerprint(&path).unwrap(), before);
        assert!(edges_in(&path).await.is_empty());
        // The text form names both and says nothing was written.
        let text = run(SuspectedArgs {
            json: false,
            ..args(path.clone(), &scratch, false)
        })
        .await
        .unwrap()
        .output;
        assert!(
            text.contains(&crate::recall::format::short_id(cid(2))),
            "{text}"
        );
        assert!(text.contains("dry run: nothing written"), "{text}");
    }

    /// `--apply` writes the edge under the session's lease and releases it;
    /// a second `--apply` writes nothing new, and the report says the pair
    /// is recorded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_records_the_pairs_once() {
        let input = ScratchDir::new("lambo-suspected-input");
        let scratch = ScratchDir::new("lambo-suspected-scratch");
        let path = fixture_db(&input).await;
        let out = run(args(path.clone(), &scratch, true)).await.unwrap();
        assert!(out.ok, "{}", out.output);
        let report: serde_json::Value = serde_json::from_str(&out.output).unwrap();
        assert_eq!(report["applied"], 1);
        assert_eq!(edges_in(&path).await, vec![(cid(2), cid(1))]);
        let store = SqliteStore::new(SqliteConnectOptions::new().filename(&path).read_only(true));
        assert!(store
            .read_lease(&SessionId::new(SESSION))
            .await
            .unwrap()
            .is_none_or(|l| l.expires_at <= chrono::Utc::now()));
        drop(store);
        let again = run(args(path.clone(), &scratch, true)).await.unwrap();
        let report: serde_json::Value = serde_json::from_str(&again.output).unwrap();
        assert_eq!(report["applied"], 0);
        assert_eq!(report["pairs"][0]["recorded"], true);
        assert_eq!(edges_in(&path).await.len(), 1);
    }

    /// `--apply` is refused while a writer holds the session, and writes
    /// nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_is_refused_while_a_writer_holds_the_session() {
        let input = ScratchDir::new("lambo-suspected-input");
        let scratch = ScratchDir::new("lambo-suspected-scratch");
        let path = fixture_db(&input).await;
        {
            let store = SqliteStore::new(SqliteConnectOptions::new().filename(&path));
            store
                .acquire_lease(
                    &SessionId::new(SESSION),
                    &LeaseHolder::for_this_process(&AgentId::new("operator")),
                    Duration::from_secs(3_600),
                )
                .await
                .unwrap();
        }
        let err = run(args(path.clone(), &scratch, true)).await.unwrap_err();
        assert!(err.to_string().contains("held by another writer"), "{err}");
        assert!(edges_in(&path).await.is_empty());
    }

    #[tokio::test]
    async fn bad_inputs_are_usage_errors() {
        let scratch = ScratchDir::new("lambo-suspected-scratch");
        let missing = scratch.join("missing.db");
        for apply in [false, true] {
            let err = run(args(missing.clone(), &scratch, apply))
                .await
                .unwrap_err();
            assert_eq!(err.exit_code(), 2, "{err}");
        }
        let err = run(SuspectedArgs {
            cosine: Some(1.5),
            ..args(missing.clone(), &scratch, true)
        })
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 2, "{err}");
    }
}

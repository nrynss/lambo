//! Operator tools for durable concept names (#146).
//!
//! Every command uses the configured store and needs no embedder. Preview
//! reads a session once and changes only an in-memory graph. Apply reloads
//! under the session's writer lease, checks the preview's source epoch and
//! persists the complete graph transaction in one fenced flush.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

use serde::Serialize;

use super::caps::{check_size_cli, require_nonempty, CliError};
use crate::graph::{names, Graph};
use crate::store::lease::{LeaseHolder, LeaseOutcome, LEASE_TTL};
use crate::store::GraphStore;
use crate::types::{AgentId, CanonizationStatus, ConceptName, GraphSnapshot, NodeId, SessionId};

/// Names are operator metadata; the agent does not manufacture provenance
/// interactions or embeddings when attaching them to existing concepts.
pub const NAMES_AGENT: &str = "lambo-names";
const MAX_MAP_BYTES: u64 = 4 * 1024 * 1024;

/// An existing-memory upgrade or one explicit name declaration.
#[derive(Clone, Debug)]
pub enum Action {
    /// Import surviving legacy synonyms and repair reviewed alias pairs.
    Migrate { pairs: Vec<(NodeId, NodeId)> },
    /// Bind a name to an existing concept's full id.
    Assign { name: String, target: NodeId },
}

/// Parsed command options. `expect_epoch` is mandatory when applying.
#[derive(Clone, Debug)]
pub struct Args {
    pub session: String,
    pub action: Action,
    pub apply: bool,
    pub expect_epoch: Option<u64>,
}

/// Preview and apply produce the same change report, alongside the source
/// epoch an operator must pass to apply the reviewed state.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub session: String,
    pub operation: &'static str,
    pub source_epoch: u64,
    pub resulting_epoch: u64,
    pub applied: bool,
    pub mutations: usize,
    pub changes: serde_json::Value,
    /// Every new binding, so a preview identifies what its counts refer to.
    pub bindings: Vec<ConceptName>,
    /// Aliases removed from active memory; their original records remain archived.
    pub retirements: Vec<Retirement>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Retirement {
    pub alias: NodeId,
    pub target: NodeId,
    pub previous_status: CanonizationStatus,
    pub archived_edges: usize,
}

/// A reviewed repair map contains full ids, not concept text or a heuristic.
/// Requiring the full form also keeps a repeated repair resolvable after its
/// alias has been retired and is no longer an active concept.
pub fn parse_id(value: &str) -> Result<NodeId, CliError> {
    let id = uuid::Uuid::parse_str(value.trim())
        .map_err(|_| CliError::Usage("a concept id must be a full hyphenated UUID".into()))?;
    if !id
        .hyphenated()
        .to_string()
        .eq_ignore_ascii_case(value.trim())
    {
        return Err(CliError::Usage(
            "a concept id must be a full hyphenated UUID".into(),
        ));
    }
    Ok(NodeId(id))
}

/// Parse `alias-id target-id` lines; comments start with `#`.
pub fn parse_map(text: &str) -> Result<Vec<(NodeId, NodeId)>, CliError> {
    if text.len() as u64 > MAX_MAP_BYTES {
        return Err(CliError::Usage("name repair map exceeds 4 MiB".into()));
    }
    let mut pairs = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let fields: Vec<_> = line
            .split('#')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .collect();
        if fields.is_empty() {
            continue;
        }
        if fields.len() != 2 {
            return Err(CliError::Usage(format!(
                "repair map line {} must contain alias-id target-id",
                index + 1
            )));
        }
        let pair = (parse_id(fields[0]), parse_id(fields[1]));
        match pair {
            (Ok(alias), Ok(target)) => pairs.push((alias, target)),
            _ => {
                return Err(CliError::Usage(format!(
                    "repair map line {} must contain two full hyphenated UUIDs",
                    index + 1
                )))
            }
        }
    }
    Ok(pairs)
}

/// Read a bounded reviewed map before touching the store or its lease.
pub fn read_map(path: &Path) -> Result<Vec<(NodeId, NodeId)>, CliError> {
    let file = std::fs::File::open(path)
        .map_err(|e| CliError::Usage(format!("read repair map {}: {e}", path.display())))?;
    let mut text = String::new();
    file.take(MAX_MAP_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| CliError::Usage(format!("read repair map: {e}")))?;
    parse_map(&text)
}

fn check_snapshot(snapshot: &GraphSnapshot, expect_epoch: Option<u64>) -> Result<(), CliError> {
    if let Some(expected) = expect_epoch
        && snapshot.mutation_epoch != expected
    {
        return Err(CliError::Usage(format!(
            "the session changed since preview (expected epoch {expected}, found {}); \
                 preview and review it again; nothing was written",
            snapshot.mutation_epoch
        )));
    }
    if snapshot
        .write_intents
        .iter()
        .any(|intent| intent.outcome.is_none())
    {
        return Err(CliError::Runtime(
            "the session has unconsumed write intents; let its writer replay and settle them \
             before migrating names; nothing was written"
                .into(),
        ));
    }
    Ok(())
}

fn evaluate(snapshot: GraphSnapshot, args: &Args) -> Result<(Graph, Report), CliError> {
    check_snapshot(&snapshot, args.expect_epoch)?;
    let source_epoch = snapshot.mutation_epoch;
    let old_names: BTreeSet<_> = snapshot.names.iter().map(|n| n.name_key.clone()).collect();
    let old_retired: BTreeSet<_> = snapshot
        .retired_aliases
        .iter()
        .map(|r| r.concept.id.to_string())
        .collect();
    let mut graph = Graph::from_snapshot(snapshot).map_err(CliError::from)?;
    let _ = graph.drain_log();
    let (operation, changes) = match &args.action {
        Action::Migrate { pairs } => (
            "migrate",
            serde_json::to_value(names::upgrade(&mut graph, pairs).map_err(CliError::from)?)
                .map_err(|e| CliError::Runtime(format!("render name upgrade: {e}")))?,
        ),
        Action::Assign { name, target } => {
            let changed = graph.declare_name(name, *target).map_err(CliError::from)?;
            (
                "assign",
                serde_json::json!({"target": target, "name": name, "changed": changed}),
            )
        }
    };
    let resulting_epoch = graph.epoch();
    let mutations = resulting_epoch.saturating_sub(source_epoch) as usize;
    let bindings = graph
        .names()
        .filter(|n| !old_names.contains(&n.name_key))
        .cloned()
        .collect();
    let retirements = graph
        .retired_aliases()
        .filter(|r| !old_retired.contains(&r.concept.id.to_string()))
        .map(|r| Retirement {
            alias: r.concept.id,
            target: r.target,
            previous_status: r.concept.canonization_status,
            archived_edges: r.edges.len(),
        })
        .collect();
    Ok((
        graph,
        Report {
            session: args.session.clone(),
            operation,
            source_epoch,
            resulting_epoch,
            applied: args.apply,
            mutations,
            changes,
            bindings,
            retirements,
        },
    ))
}

/// Preview or apply against any compiled store adapter.
pub async fn run(store: &dyn GraphStore, args: Args) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;
    if let Action::Assign { name, .. } = &args.action {
        require_nonempty("name", name)?;
        check_size_cli("name", name)?;
    }
    if args.apply && args.expect_epoch.is_none() {
        return Err(CliError::Usage(
            "--apply requires --expect-epoch from the reviewed preview; nothing was written".into(),
        ));
    }
    store
        .preflight_schema()
        .await
        .map_err(|e| CliError::Runtime(format!("names schema: {e}; run lambo provision first")))?;
    let session = SessionId::new(&args.session);
    // A miss is an error: a mistyped session must not gain a lease row.
    let snapshot = store
        .load_session(&session)
        .await
        .map_err(|e| CliError::Runtime(format!("load names session: {e}")))?;
    check_snapshot(&snapshot, args.expect_epoch)?;
    if !args.apply {
        let (_, report) = evaluate(snapshot, &args)?;
        return serde_json::to_string_pretty(&report)
            .map_err(|e| CliError::Runtime(format!("render names report: {e}")));
    }

    let holder = LeaseHolder::for_this_process(&AgentId::new(NAMES_AGENT));
    let token = match store
        .acquire_lease(&session, &holder, LEASE_TTL)
        .await
        .map_err(|e| CliError::Runtime(format!("take names writer lease: {e}")))?
    {
        LeaseOutcome::Acquired(info) => info.token,
        LeaseOutcome::Held { current, age } => {
            return Err(CliError::Runtime(format!(
                "session {session} is held by another writer ({}, holding the lease for {}s); \
             stop it before applying the names migration; nothing was written",
                current.holder,
                age.as_secs()
            )))
        }
    };
    let result = async {
        let snapshot = store
            .load_session(&session)
            .await
            .map_err(|e| CliError::Runtime(format!("reload names session under lease: {e}")))?;
        let (mut graph, mut report) = evaluate(snapshot, &args)?;
        let batch = graph.drain_log();
        report.mutations = batch.mutations.len();
        if !batch.mutations.is_empty() {
            match store
                .refresh_lease(&session, &holder, LEASE_TTL)
                .await
                .map_err(|e| CliError::Runtime(format!("refresh names writer lease: {e}")))?
            {
                LeaseOutcome::Acquired(info) if info.token == token => {}
                _ => {
                    return Err(CliError::Runtime(
                        "the names writer lease changed while planning; nothing was written".into(),
                    ))
                }
            }
            store
                .flush(&batch, Some(token))
                .await
                .map_err(|e| CliError::Runtime(format!("persist names transaction: {e}")))?;
        }
        Ok(report)
    }
    .await;
    let released = store.release_lease(&session, &holder).await;
    let report = result?;
    released.map_err(|e| {
        CliError::Runtime(format!(
            "names transaction committed, but releasing its lease failed: {e}"
        ))
    })?;
    serde_json::to_string_pretty(&report)
        .map_err(|e| CliError::Runtime(format!("render names report: {e}")))
}

#[cfg(all(test, feature = "store-memory", feature = "fixtures"))]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::store::MemoryStore;
    use crate::test_util::fair_test as fair;
    use crate::types::{EdgeType, WriteIntent, WriteIntentPayload};

    fn args(action: Action, apply: bool, expect_epoch: Option<u64>) -> Args {
        Args {
            session: fair::NAME.into(),
            action,
            apply,
            expect_epoch,
        }
    }

    fn assign() -> Action {
        Action::Assign {
            name: "Writer lease rule".into(),
            target: fair::concept_id("lease_stays"),
        }
    }

    fn repair() -> Action {
        Action::Migrate {
            pairs: vec![(
                fair::concept_id("lease_alias"),
                fair::concept_id("lease_stays"),
            )],
        }
    }

    fn value(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn a_reviewed_map_requires_full_ids_and_parses_comments() {
        let alias = fair::concept_id("lease_alias");
        let target = fair::concept_id("lease_stays");
        let text = format!("# reviewed\n{alias} {target} # lease alias\n\n");
        assert_eq!(parse_map(&text).unwrap(), vec![(alias, target)]);
        assert!(parse_map("writer lease the ruling").is_err());
        assert!(parse_map(&format!("{} {target}", &alias.to_string()[..8])).is_err());
        assert!(parse_id(&alias.0.simple().to_string()).is_err());
        assert!(parse_map(&"x".repeat(MAX_MAP_BYTES as usize + 1)).is_err());
    }

    #[tokio::test]
    async fn assigning_an_existing_concept_previews_without_a_lease_or_reembedding() {
        let store = MemoryStore::new();
        let before = fair::snapshot(true);
        store.seed(before.clone()).unwrap();
        let preview = value(&run(&store, args(assign(), false, None)).await.unwrap());
        assert_eq!(preview["applied"], false);
        assert_eq!(preview["source_epoch"], before.mutation_epoch);
        assert_eq!(preview["changes"]["changed"], true);
        assert_eq!(store.load_session(&fair::session()).await.unwrap(), before);
        assert!(store.read_lease(&fair::session()).await.unwrap().is_none());

        let applied = value(
            &run(&store, args(assign(), true, Some(before.mutation_epoch)))
                .await
                .unwrap(),
        );
        assert_eq!(applied["applied"], true);
        let after = store.load_session(&fair::session()).await.unwrap();
        assert_eq!(after.concepts, before.concepts);
        assert_eq!(after.edges, before.edges);
        assert_eq!(after.embedding, before.embedding);
        assert_eq!(
            Graph::from_snapshot(after)
                .unwrap()
                .name_target("WRITER LEASE RULE"),
            Some(fair::concept_id("lease_stays"))
        );
    }

    #[tokio::test]
    async fn apply_requires_the_reviewed_epoch_and_refuses_stale_memory_without_a_lease() {
        let store = MemoryStore::new();
        let before = fair::snapshot(true);
        store.seed(before.clone()).unwrap();
        let missing = run(&store, args(assign(), true, None)).await.unwrap_err();
        assert_eq!(missing.exit_code(), 2);
        assert!(missing.to_string().contains("--expect-epoch"));
        let stale = run(
            &store,
            args(assign(), true, Some(before.mutation_epoch + 1)),
        )
        .await
        .unwrap_err();
        assert!(stale.to_string().contains("changed since preview"));
        assert_eq!(store.load_session(&fair::session()).await.unwrap(), before);
        assert!(store.read_lease(&fair::session()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn applying_refuses_a_live_writer_without_changing_memory() {
        let store = MemoryStore::new();
        let before = fair::snapshot(true);
        store.seed(before.clone()).unwrap();
        let writer = LeaseHolder::for_this_process(&AgentId::new("other-writer"));
        store
            .acquire_lease(&fair::session(), &writer, Duration::from_secs(3600))
            .await
            .unwrap();
        let lease = store.read_lease(&fair::session()).await.unwrap();
        let err = run(&store, args(repair(), true, Some(before.mutation_epoch)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("another writer"));
        assert_eq!(store.load_session(&fair::session()).await.unwrap(), before);
        assert_eq!(store.read_lease(&fair::session()).await.unwrap(), lease);
    }

    #[tokio::test]
    async fn a_refused_reviewed_mapping_keeps_the_whole_upgrade_unchanged_and_releases_its_lease() {
        let store = MemoryStore::new();
        let before = fair::snapshot(true);
        store.seed(before.clone()).unwrap();
        let invalid_map = Action::Migrate {
            pairs: vec![
                (
                    fair::concept_id("lease_alias"),
                    fair::concept_id("lease_stays"),
                ),
                (NodeId::new(), fair::concept_id("identity")),
            ],
        };
        let err = run(&store, args(invalid_map, true, Some(before.mutation_epoch)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("alias"), "{err}");
        assert_eq!(store.load_session(&fair::session()).await.unwrap(), before);
        // The failed attempt must release its lease so the corrected whole
        // map can be applied immediately, without an override or timeout.
        let corrected = value(
            &run(&store, args(repair(), true, Some(before.mutation_epoch)))
                .await
                .unwrap(),
        );
        assert_eq!(corrected["changes"]["aliases_retired"], 1);
    }

    #[tokio::test]
    async fn reviewed_alias_repair_is_durable_preserves_citation_times_and_repeats_as_a_noop() {
        let store = MemoryStore::new();
        let before = fair::snapshot(true);
        store.seed(before.clone()).unwrap();
        let alias = fair::concept_id("lease_alias");
        let target = fair::concept_id("lease_stays");
        let old_citations: Vec<_> = before
            .edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Dependency && e.target == alias)
            .cloned()
            .collect();
        assert!(!old_citations.is_empty());
        let preview = value(&run(&store, args(repair(), false, None)).await.unwrap());
        assert_eq!(preview["changes"]["aliases_retired"], 1);
        assert_eq!(store.load_session(&fair::session()).await.unwrap(), before);
        let applied = value(
            &run(&store, args(repair(), true, Some(before.mutation_epoch)))
                .await
                .unwrap(),
        );
        assert_eq!(applied["changes"], preview["changes"]);
        let after = store.load_session(&fair::session()).await.unwrap();
        let graph = Graph::from_snapshot(after.clone()).unwrap();
        assert!(graph.node(alias).is_none());
        assert!(graph.retired_alias(alias).is_some());
        assert_eq!(graph.name_target("writer lease"), Some(target));
        for old in &old_citations {
            let moved = after
                .edges
                .iter()
                .find(|e| {
                    e.source == old.source
                        && e.target == target
                        && e.edge_type == EdgeType::Dependency
                })
                .unwrap();
            assert_eq!(moved.created_at, old.created_at);
            assert_eq!(moved.event_time, old.event_time);
        }
        for original in before.concepts.iter().filter(|c| c.id != alias) {
            assert_eq!(
                after.concepts.iter().find(|c| c.id == original.id),
                Some(original)
            );
        }
        let repeat = value(
            &run(&store, args(repair(), true, Some(after.mutation_epoch)))
                .await
                .unwrap(),
        );
        assert_eq!(repeat["mutations"], 0);
        assert_eq!(store.load_session(&fair::session()).await.unwrap(), after);
    }

    #[tokio::test]
    async fn unconsumed_intents_require_replay_before_migration() {
        let store = MemoryStore::new();
        let mut before = fair::snapshot(true);
        before.write_intents.push(WriteIntent {
            session_id: fair::session(),
            receipt: "owed-replay".into(),
            agent: AgentId::new("writer"),
            interaction: before.interactions[0].id,
            lane_seq: 1,
            issued_ms: fair::now().timestamp_millis(),
            payload: WriteIntentPayload::Derive {
                concepts: vec![],
                pairs: vec![],
                receipt_destination: None,
            },
            created_at: fair::now(),
            outcome: None,
        });
        store.seed(before.clone()).unwrap();
        let err = run(&store, args(repair(), true, Some(before.mutation_epoch)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unconsumed write intents"));
        assert_eq!(store.load_session(&fair::session()).await.unwrap(), before);
        assert!(store.read_lease(&fair::session()).await.unwrap().is_none());
    }
}

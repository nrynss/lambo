//! `lambo session classify` (read-only: an explicit input, never a writer)
//! and `lambo session split` (#33 D2: writes a validated plan's targets into
//! a new offline SQLite file; the source stays read-only), and `lambo
//! session verify` (#33 D3: a read-only before/after recall comparison of
//! the source and the split output).

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::caps::{
    check_in_range_cli, require_nonempty, CliError, MAX_MAX_TOKENS, MAX_TOP_K, MAX_TRAVERSAL_DEPTH,
};
use crate::resolve::{ClassificationSource, SplitIo};
use crate::session::{
    build_split, classify, classify_reviewed, ClassificationPlan, ClassificationRules,
    OperatorOverride, ReviewedAssignment, SplitOutput,
};
use crate::types::{GraphSnapshot, SessionId};

#[derive(Debug)]
pub struct ClassifyArgs {
    pub session: String,
    pub rules: PathBuf,
    pub overrides: Option<PathBuf>,
    pub reviewed_assignments: Option<PathBuf>,
    pub output: PathBuf,
}

fn read_bounded(path: &Path, limit: u64) -> Result<String, CliError> {
    let file = std::fs::File::open(path).map_err(|e| {
        CliError::Usage(format!("read classification input {}: {e}", path.display()))
    })?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| CliError::Usage(format!("read classification input: {e}")))?;
    if bytes.len() as u64 > limit {
        return Err(CliError::Usage(
            "classification input exceeds size bound".into(),
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| CliError::Usage("classification input must be UTF-8".into()))
}

fn read_snapshot(path: &Path) -> Result<GraphSnapshot, CliError> {
    let value: serde_json::Value = serde_json::from_str(&read_bounded(path, 512 * 1024 * 1024)?)
        .map_err(|e| CliError::Usage(format!("snapshot JSON: {e}")))?;
    let object = value.as_object().ok_or_else(|| {
        CliError::Usage("snapshot must be a complete GraphSnapshot object".into())
    })?;
    for key in [
        "session_id",
        "interactions",
        "concepts",
        "edges",
        "synonyms",
        "canonization_events",
        "root_goal",
        "created_at",
        "closed_at",
    ] {
        if !object.contains_key(key) {
            return Err(CliError::Usage(format!(
                "snapshot is missing required field {key}"
            )));
        }
    }
    let allowed = [
        "session_id",
        "interactions",
        "concepts",
        "edges",
        "synonyms",
        "canonization_events",
        "root_goal",
        "created_at",
        "closed_at",
        "reservations",
        "embedding",
        "write_intents",
        "mutation_epoch",
        "gc_mark",
    ];
    if object.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(CliError::Usage(
            "snapshot contains unsupported fields".into(),
        ));
    }
    serde_json::from_value(value).map_err(|e| CliError::Usage(format!("snapshot schema: {e}")))
}

/// Publishes a complete private file at `path`, never replacing anything.
///
/// The bytes go to a `create_new` sibling first, are synced, then hard-linked
/// to `path`. `link` refuses any existing entry (file, symlink or alias), so an
/// output spelling cannot truncate an input, and a failed write never leaves a
/// partial plan at `path`. The temporary name is removed on every outcome.
fn write_new_private(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<(), CliError> {
    write_new_private_as("classification plan", path, write)
}

/// [`write_new_private`], naming the file `what` in its errors.
fn write_new_private_as(
    what: &str,
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<(), CliError> {
    let name = path
        .file_name()
        .ok_or_else(|| CliError::Usage(format!("{what} {} must name a file", path.display())))?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".{}.partial", uuid::Uuid::new_v4()));
    let temp = path.with_file_name(temp_name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| CliError::Usage(format!("create new {what} {}: {e}", path.display())))?;
    let written = write(&mut file)
        .and_then(|_| file.sync_all())
        .map_err(|e| CliError::Runtime(format!("write {what}: {e}")))
        .and_then(|_| {
            std::fs::hard_link(&temp, path)
                .map_err(|e| CliError::Usage(format!("create new {what} {}: {e}", path.display())))
        });
    drop(file);
    let removed = std::fs::remove_file(&temp);
    written?;
    removed
        .map_err(|e| CliError::Runtime(format!("remove temporary {what} {}: {e}", temp.display())))
}

fn read_rules(path: &Path) -> Result<ClassificationRules, CliError> {
    let text = read_bounded(path, 4 * 1024 * 1024)?;
    let rules: ClassificationRules = if path.extension().is_some_and(|e| e == "toml") {
        toml::from_str(&text)
            .map_err(|e| CliError::Usage(format!("classification rules: {}", e.message())))?
    } else {
        serde_json::from_str(&text)
            .map_err(|e| CliError::Usage(format!("classification rules: {e}")))?
    };
    rules
        .validate()
        .map_err(|e| CliError::Usage(e.to_string()))?;
    Ok(rules)
}

fn read_overrides(path: &Path) -> Result<Vec<OperatorOverride>, CliError> {
    let mut overrides = Vec::new();
    for (n, line) in read_bounded(path, 32 * 1024 * 1024)?.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        overrides.push(
            serde_json::from_str(line)
                .map_err(|e| CliError::Usage(format!("override line {}: {e}", n + 1)))?,
        );
    }
    Ok(overrides)
}

fn read_labels(path: &Path) -> Result<Vec<ReviewedAssignment>, CliError> {
    let mut labels = Vec::new();
    for (n, line) in read_bounded(path, 32 * 1024 * 1024)?.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        labels
            .push(serde_json::from_str(line).map_err(|e| {
                CliError::Usage(format!("reviewed assignment line {}: {e}", n + 1))
            })?);
    }
    Ok(labels)
}

/// Materialize the source through the immutable path only: a read-only
/// schema check (never `init_schema`), then one load. No lease, flush,
/// canonization or writable pool touches it.
async fn load_source(
    source: ClassificationSource,
    session: &str,
    what: &str,
) -> Result<GraphSnapshot, CliError> {
    let snapshot = match source {
        ClassificationSource::Snapshot(path) => read_snapshot(&path)?,
        ClassificationSource::Database(store) => {
            // Read-only verification, not init_schema. Old schemas must be
            // exported/upgraded on a separate copy, never repaired here.
            store
                .preflight_schema()
                .await
                .map_err(|e| CliError::Runtime(format!("{what} schema: {e}")))?;
            store
                .load_session(&SessionId::from(session))
                .await
                .map_err(|e| CliError::Runtime(format!("{what} source: {e}")))?
        }
    };
    if snapshot.session_id.0 != session {
        return Err(CliError::Usage(
            "snapshot session does not match --session".into(),
        ));
    }
    Ok(snapshot)
}

/// Produces a private plan, printing only aggregate counts and its path.
/// The output is published create-new (see [`write_new_private`]), so an
/// output spelling cannot truncate an input.
pub async fn classify_session(
    source: ClassificationSource,
    args: ClassifyArgs,
) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    let rules = read_rules(&args.rules)?;
    let overrides = match &args.overrides {
        Some(path) => read_overrides(path)?,
        None => Vec::new(),
    };
    let snapshot = load_source(source, &args.session, "classification").await?;
    let plan = if let Some(path) = &args.reviewed_assignments {
        classify_reviewed(&snapshot, &rules, &read_labels(path)?, &overrides)
    } else {
        classify(&snapshot, &rules, &overrides)
    }
    .map_err(|e| CliError::Usage(e.to_string()))?;
    let bytes = serde_json::to_vec_pretty(&plan)
        .map_err(|e| CliError::Runtime(format!("render classification plan: {e}")))?;
    write_new_private(&args.output, |output| {
        output.write_all(&bytes)?;
        output.write_all(b"\n")
    })?;
    Ok(format!("classification plan {}: {} interactions, {} concepts, {} low-confidence items, {} shared hubs; source opened read-only", args.output.display(), plan.source.interactions, plan.source.concepts, plan.low_confidence.len(), plan.assignments.iter().filter(|a| !a.copy_into.is_empty()).count()))
}

#[derive(Debug)]
pub struct SplitArgs {
    pub session: String,
    pub plan: PathBuf,
    pub rules: PathBuf,
    pub reviewed_assignments: Option<PathBuf>,
    pub overrides: Option<PathBuf>,
    pub report: PathBuf,
    pub dry_run: bool,
}

/// `(device, inode)` of an existing file, following links.
fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// The absolute spelling of `path` with its directory resolved, whether or
/// not the file itself exists.
fn spelled(path: &Path) -> Option<PathBuf> {
    if let Ok(p) = std::fs::canonicalize(path) {
        return Some(p);
    }
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    Some(std::fs::canonicalize(parent).ok()?.join(path.file_name()?))
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

const SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Refuse an output (and its SQLite sidecars) or report that names an input
/// by spelling, symlink, hardlink or directory alias. Path strings alone
/// miss hardlinks, so existing files are compared by device and inode.
fn check_outputs(
    inputs: &[&Path],
    output: &Path,
    report: &Path,
    dry_run: bool,
) -> Result<(), CliError> {
    let alias = || {
        CliError::Usage(
            "--output-database and --report must not name, link to or share a file with any \
             input, each other or a SQLite sidecar"
                .into(),
        )
    };
    let input_ids: Vec<_> = inputs.iter().filter_map(|p| identity(p)).collect();
    let mut input_names: Vec<PathBuf> = Vec::new();
    for p in inputs {
        let Some(name) = spelled(p) else { continue };
        input_names.extend(SIDECARS.iter().map(|s| with_suffix(&name, s)));
        input_names.push(name);
    }
    let output_name = spelled(output).ok_or_else(alias)?;
    let report_name = spelled(report).ok_or_else(alias)?;
    let mut output_names = vec![output_name.clone()];
    output_names.extend(SIDECARS.iter().map(|s| with_suffix(&output_name, s)));
    if output_names
        .iter()
        .any(|o| input_names.contains(o) || o == &report_name)
        || input_names.contains(&report_name)
    {
        return Err(alias());
    }
    if std::fs::symlink_metadata(report).is_ok() {
        return Err(CliError::Usage(format!(
            "split report {} already exists; reports are never overwritten",
            report.display()
        )));
    }
    if dry_run {
        return Ok(());
    }
    let output_files = std::iter::once(output.to_path_buf())
        .chain(SIDECARS.iter().map(|s| with_suffix(output, s)));
    for path in output_files {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                use std::os::unix::fs::MetadataExt;
                if !meta.file_type().is_file()
                    || meta.nlink() != 1
                    || identity(&path).is_some_and(|id| input_ids.contains(&id))
                {
                    return Err(alias());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(CliError::Usage(format!("inspect {}: {e}", path.display()))),
        }
    }
    Ok(())
}

/// Re-validate the plan by reproducing it: D1 must derive exactly this plan
/// from the source, the supplied rules and labels and the plan's own audited
/// overrides. The seal alone detects corruption, not authorization.
fn reproduce_plan(
    plan: &ClassificationPlan,
    snapshot: &GraphSnapshot,
    args: &SplitArgs,
) -> Result<(), CliError> {
    let rules = read_rules(&args.rules)?;
    if rules != plan.rules {
        return Err(CliError::Usage(
            "--rules differ from the rules the plan was classified with".into(),
        ));
    }
    // The plan's overrides never authorize themselves: the operator's
    // overrides file is an input the plan must reproduce from, exactly as
    // the reviewed labels are.
    let overrides = match &args.overrides {
        Some(path) => {
            let mut given = read_overrides(path)?;
            given.sort_by_key(|o| (o.kind, o.id.0));
            if given != plan.overrides {
                return Err(CliError::Usage(
                    "--overrides differ from the plan's audited overrides".into(),
                ));
            }
            given
        }
        None if plan.overrides.is_empty() => Vec::new(),
        None => {
            return Err(CliError::Usage(
                "the plan carries audited overrides; pass the same --overrides file".into(),
            ))
        }
    };
    let reproduced = match (
        &args.reviewed_assignments,
        &plan.reviewed_assignments_sha256,
    ) {
        (Some(path), Some(_)) => {
            classify_reviewed(snapshot, &rules, &read_labels(path)?, &overrides)
        }
        (None, None) => classify(snapshot, &rules, &overrides),
        (None, Some(_)) => {
            return Err(CliError::Usage(
                "the plan imported reviewed labels; pass the same --reviewed-assignments".into(),
            ))
        }
        (Some(_), None) => {
            return Err(CliError::Usage(
                "the plan imported no reviewed labels; drop --reviewed-assignments".into(),
            ))
        }
    }
    .map_err(|e| CliError::Usage(e.to_string()))?;
    if &reproduced != plan {
        return Err(CliError::Usage(
            "the plan does not reproduce from this source, rules, labels and overrides; \
             reclassify instead of editing a plan"
                .into(),
        ));
    }
    Ok(())
}

/// How one target ended, for the report.
#[derive(serde::Serialize)]
struct TargetReport<'a> {
    session: &'a str,
    sha256: &'a str,
    outcome: &'static str,
    rows_written: u64,
}

#[derive(serde::Serialize)]
struct SplitReport<'a> {
    report_version: u32,
    dry_run: bool,
    algorithm_version: u32,
    source_session: &'a str,
    source_sha256: &'a str,
    plan_sha256: &'a str,
    split_sha256: &'a str,
    output_database: String,
    /// Session metadata rules applied to every target.
    metadata: &'static str,
    root_goal_cleared: bool,
    low_confidence_count: usize,
    low_confidence: &'a [crate::session::LowConfidenceItem],
    accounting: &'a crate::session::SplitAccounting,
    targets: Vec<TargetReport<'a>>,
    ledger: &'a [crate::session::LedgerEntry],
    placements: &'a [crate::session::Placement],
}

const METADATA_RULES: &str = "root_goal cleared; created_at, mutation_epoch and GC watermark \
inherited from the source; closed_at cleared; transient GC reset flag false; reservations, \
leases and writer stats not migrated";

fn summary(split: &SplitOutput, output: &Path, report: &Path, outcome: &str) -> String {
    let a = &split.accounting;
    let e = &a.edges;
    let mut out = format!(
        "split {} -> {}: {} targets, {outcome}\n",
        split.source_session,
        output.display(),
        split.targets.len()
    );
    out.push_str(&format!(
        "concepts {} -> {} ({} hubs, {} placements, +{}); interactions {} -> {} (+{} origin \
         replicas: {} for assigned concepts, {} only for hub copies)\n",
        a.source_concepts,
        a.target_concepts,
        a.hubs,
        a.hub_placements,
        a.hub_extra_rows,
        a.source_interactions,
        a.target_interactions,
        a.interaction_replicas,
        a.replicas_for_primary_concepts,
        a.replicas_for_hub_copies_only
    ));
    out.push_str(&format!(
        "source edges {}: intra {}, cross-session Temporal rebuilt {}, hub reattached {}, \
         Derives reattached {}, dropped {}\n",
        e.source_edges,
        e.intra,
        e.cross_temporal_rebuilt,
        e.hub_reattached,
        e.derives_reattached,
        e.dropped
    ));
    out.push_str(&format!(
        "output edges {}: {} chain links ({} kept, {} new), {} structural (+{} shared-hub \
         copies)\n",
        e.output_edges,
        e.output_chain_links,
        e.output_preserved_links,
        e.output_new_links,
        e.output_structural,
        e.output_structural_extra
    ));
    if a.consumed_write_intents_dropped > 0 {
        out.push_str(&format!(
            "{} consumed write receipts were not migrated (receipts only)\n",
            a.consumed_write_intents_dropped
        ));
    }
    if !a.replica_matrix.is_empty() {
        let cells: Vec<String> = a
            .replica_matrix
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect();
        out.push_str(&format!("origin replicas: {}\n", cells.join(", ")));
        out.push_str(&format!(
            "replica text: {} stubs without prompt text (full text stays in the primary \
             target); {} in general, {} of them with text (listed in the report)\n",
            a.stubbed_replicas.len(),
            a.general_replicas,
            a.general_full_text_replicas.len()
        ));
    }
    if !a.general_replicas_from_excluded_projects.is_empty() {
        out.push_str(&format!(
            "GENERAL PROVENANCE: {} interactions assigned to projects excluded from general are \
             copied into general as origins of audited general rulings (stubs unless the report \
             lists them as full-text); \
             review them before any cut-over\n",
            a.general_replicas_from_excluded_projects.len()
        ));
    }
    if !split.low_confidence.is_empty() {
        out.push_str(&format!(
            "LOW CONFIDENCE: {} items are placed by the plan but unreviewed; review them \
             (listed in the report) before any cut-over\n",
            split.low_confidence.len()
        ));
    }
    out.push_str(&format!(
        "report {}; source opened read-only",
        report.display()
    ));
    out
}

/// `lambo session split` (#33 D2). Validates and reproduces the plan, builds
/// every target in memory and validates it, then (unless `dry_run`) imports
/// each target into the explicit SQLite output in its own transaction, and
/// finally publishes the private report create-new.
pub async fn split_session(io: SplitIo, args: SplitArgs) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    let output = io.destination.path().to_path_buf();
    let mut inputs: Vec<&Path> = vec![&args.plan, &args.rules];
    inputs.extend(args.reviewed_assignments.as_deref());
    inputs.extend(args.overrides.as_deref());
    inputs.push(&io.source_path);
    check_outputs(&inputs, &output, &args.report, args.dry_run)?;

    let plan: ClassificationPlan =
        serde_json::from_str(&read_bounded(&args.plan, 512 * 1024 * 1024)?)
            .map_err(|e| CliError::Usage(format!("classification plan: {e}")))?;
    if plan.source.session != args.session {
        return Err(CliError::Usage(
            "the plan was made for a different --session".into(),
        ));
    }
    let snapshot = load_source(io.source, &args.session, "split").await?;
    reproduce_plan(&plan, &snapshot, &args)?;
    let split = build_split(&plan, &snapshot).map_err(|e| CliError::Usage(e.to_string()))?;
    drop(snapshot);

    let mut targets = Vec::with_capacity(split.targets.len());
    let outcome = if args.dry_run {
        for t in &split.targets {
            targets.push(TargetReport {
                session: &t.session,
                sha256: &t.sha256,
                outcome: "dry_run",
                rows_written: 0,
            });
        }
        "dry run, nothing written".to_owned()
    } else {
        import(&split, &io.destination, &mut targets).await?
    };
    let report = SplitReport {
        report_version: 1,
        dry_run: args.dry_run,
        algorithm_version: split.algorithm_version,
        source_session: &split.source_session,
        source_sha256: &split.source_sha256,
        plan_sha256: &split.plan_sha256,
        split_sha256: &split.split_sha256,
        output_database: output.display().to_string(),
        metadata: METADATA_RULES,
        root_goal_cleared: split.root_goal_cleared,
        low_confidence_count: split.low_confidence.len(),
        low_confidence: &split.low_confidence,
        accounting: &split.accounting,
        targets,
        ledger: &split.ledger,
        placements: &split.placements,
    };
    let bytes = serde_json::to_vec_pretty(&report)
        .map_err(|e| CliError::Runtime(format!("render split report: {e}")))?;
    write_new_private_as("split report", &args.report, |f| {
        f.write_all(&bytes)?;
        f.write_all(b"\n")
    })
    .map_err(|e| {
        if args.dry_run {
            e
        } else {
            CliError::Runtime(format!(
                "{e}; the imports are committed, and a rerun with a new --report verifies \
                 them without writing"
            ))
        }
    })?;
    Ok(summary(&split, &output, &args.report, &outcome))
}

#[derive(Debug)]
pub struct VerifyArgs {
    pub session: String,
    pub plan: PathBuf,
    pub queries: PathBuf,
    pub report: PathBuf,
    pub top_k: Option<usize>,
    pub max_tokens: Option<usize>,
    pub traversal_depth: Option<usize>,
    pub at: Option<chrono::DateTime<chrono::Utc>>,
    /// Sessions that appear only as counts (`--aggregate-only`, repeatable).
    pub aggregate_only: Vec<String>,
}

fn read_queries(path: &Path) -> Result<crate::session::VerifyQueries, CliError> {
    let text = read_bounded(path, 4 * 1024 * 1024)?;
    let queries: crate::session::VerifyQueries = if path.extension().is_some_and(|e| e == "toml") {
        toml::from_str(&text).map_err(|e| CliError::Usage(format!("query set: {}", e.message())))?
    } else {
        serde_json::from_str(&text).map_err(|e| CliError::Usage(format!("query set: {e}")))?
    };
    queries
        .validate()
        .map_err(|e| CliError::Usage(e.to_string()))?;
    Ok(queries)
}

/// Every target of `split` as the output stores it, each read in one
/// read-only transaction and checked against its receipt and its digest.
#[cfg(feature = "store-sqlite")]
async fn load_split_targets(
    split: &SplitOutput,
    output: &crate::resolve::VerifySplitOutput,
) -> Result<std::collections::BTreeMap<String, GraphSnapshot>, CliError> {
    use crate::store::SplitReceipt;
    let mismatch = |e: crate::types::StoreError| {
        CliError::Usage(format!(
            "the split output does not match this source and plan: {e}"
        ))
    };
    let names: std::collections::BTreeSet<String> =
        split.targets.iter().map(|t| t.session.clone()).collect();
    let store = output.store();
    store
        .verify_split_destination(&split.split_sha256, &names)
        .await
        .map_err(mismatch)?;
    let mut out = std::collections::BTreeMap::new();
    for t in &split.targets {
        let receipt = SplitReceipt::for_target(
            &t.snapshot,
            &split.split_sha256,
            &t.sha256,
            split.algorithm_version,
            &split.source_sha256,
            &split.plan_sha256,
        );
        let stored = store
            .read_verified_split_target(&receipt, &t.snapshot, &names)
            .await
            .map_err(mismatch)?;
        out.insert(t.session.clone(), stored);
    }
    Ok(out)
}

#[cfg(not(feature = "store-sqlite"))]
async fn load_split_targets(
    _: &SplitOutput,
    _: &crate::resolve::VerifySplitOutput,
) -> Result<std::collections::BTreeMap<String, GraphSnapshot>, CliError> {
    Err(CliError::Usage(
        "session verify requires --features store-sqlite".into(),
    ))
}

fn ratio(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.3}"))
}

fn counts(map: &std::collections::BTreeMap<String, usize>) -> String {
    if map.is_empty() {
        return "none".to_owned();
    }
    map.iter()
        .map(|(k, v)| format!("{k} {v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Aggregates only: counts per project, no ids, queries or content.
fn verify_summary(report: &crate::session::VerifyReport, path: &Path) -> String {
    let p = &report.params;
    let mut out = format!(
        "verify {}: split {} (algorithm {}), {} projects, top_k {}, clock {}; pinned {} \
         (default {}); aggregate-only {}\nmeasures {}\n",
        report.source_session,
        &report.split_sha256[..report.split_sha256.len().min(12)],
        report.split_algorithm_version,
        report.projects.len(),
        p.top_k,
        p.at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        report.pinned.len(),
        report.default_session,
        if report.aggregate_only.is_empty() {
            "none".to_owned()
        } else {
            report.aggregate_only.join(", ")
        },
        report.measures,
    );
    for (project, s) in &report.projects {
        let layers = if s.layers.is_empty() {
            "unlayered".to_owned()
        } else {
            format!("layers {}", s.layers.join(", "))
        };
        out.push_str(&format!(
            "{project} ({layers}): {} queries; before {} hits ({} in scope, {} from other \
             sessions removed: {}); after {} hits ({}; {} shared-hub hits); recall@{} {} \
             (mean {}); lost {}, merged by dedupe {}, gained {}; FOREIGN-PRIMARY AFTER {} ({}); \
             {} on both sides, {} moved (mean |shift| {}, max {}); annotation kinds +{} -{}; \
             warning kinds +{} -{} ({} queries changed)\n",
            s.queries,
            s.before_hits,
            s.before_in_scope,
            s.removed_foreign,
            counts(&s.removed_foreign_by_primary),
            s.after_hits,
            counts(&s.after_by_session),
            s.after_hub_hits,
            p.top_k,
            ratio(s.recall_at_k),
            ratio(s.mean_recall_at_k),
            s.lost,
            s.deduped,
            s.gained,
            s.foreign_primary_after,
            counts(&s.foreign_primary_after_by_primary),
            s.common,
            s.moved,
            ratio(s.mean_abs_shift),
            s.max_abs_shift,
            s.annotations_added.values().sum::<usize>(),
            s.annotations_removed.values().sum::<usize>(),
            counts(&s.warnings_added),
            counts(&s.warnings_removed),
            s.queries_with_changed_warnings,
        ));
    }
    out.push_str(&format!(
        "report {}; source and split output opened read-only",
        path.display()
    ));
    out
}

/// `lambo session verify` (#33 D3). Rebuilds the split from the source and
/// plan, requires the output's receipts and target digests to match it,
/// replays the query set before (plain recall on the source) and after
/// (layered recall per project, as the serving `[serve]` table pins and
/// layers it), and
/// publishes the private report create-new. Both inputs are read-only: no
/// lease, flush or schema initialization touches either.
pub async fn verify_session(
    io: crate::resolve::VerifyIo,
    args: VerifyArgs,
) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    if std::fs::symlink_metadata(&args.report).is_ok() {
        return Err(CliError::Usage(format!(
            "verify report {} already exists; reports are never overwritten",
            args.report.display()
        )));
    }
    let cfg = &io.config;
    let top_k = args.top_k.unwrap_or(cfg.default_top_k);
    let max_tokens = args.max_tokens.unwrap_or(cfg.default_max_tokens);
    let traversal_depth = args.traversal_depth.unwrap_or(cfg.default_traversal_depth);
    check_in_range_cli("top-k", top_k, 1, MAX_TOP_K)?;
    check_in_range_cli("max-tokens", max_tokens, 1, MAX_MAX_TOKENS)?;
    check_in_range_cli("traversal-depth", traversal_depth, 0, MAX_TRAVERSAL_DEPTH)?;
    let queries = read_queries(&args.queries)?;

    let plan: ClassificationPlan =
        serde_json::from_str(&read_bounded(&args.plan, 512 * 1024 * 1024)?)
            .map_err(|e| CliError::Usage(format!("classification plan: {e}")))?;
    if plan.source.session != args.session {
        return Err(CliError::Usage(
            "the plan was made for a different --session".into(),
        ));
    }
    let snapshot = load_source(io.source, &args.session, "verify").await?;
    // The same pure build the split ran: a plan that does not seal against
    // this source is refused here, and the result's digests and placements
    // are what the output must match.
    let split = build_split(&plan, &snapshot).map_err(|e| CliError::Usage(e.to_string()))?;
    let targets = load_split_targets(&split, &io.split).await?;
    let at = match args.at {
        Some(at) => at,
        None => crate::session::newest_row(&snapshot)
            .ok_or_else(|| CliError::Usage("the source is empty; pass --at".into()))?,
    };
    let aggregate_only: std::collections::BTreeSet<String> =
        args.aggregate_only.iter().cloned().collect();
    let report = crate::session::verify_split(crate::session::VerifyInputs {
        source: snapshot,
        plan: &plan,
        split: &split,
        targets,
        serve: &io.serve,
        queries: &queries,
        aggregate_only: &aggregate_only,
        params: crate::session::VerifyParams {
            top_k,
            max_tokens,
            traversal_depth,
            at,
        },
        embedder: io.embedder.as_ref(),
        embedding: &io.embedding,
        config: cfg,
    })
    .await
    .map_err(|e| CliError::Usage(e.to_string()))?;
    let bytes = serde_json::to_vec_pretty(&report)
        .map_err(|e| CliError::Runtime(format!("render verify report: {e}")))?;
    write_new_private_as("verify report", &args.report, |f| {
        f.write_all(&bytes)?;
        f.write_all(b"\n")
    })?;
    Ok(verify_summary(&report, &args.report))
}

#[cfg(not(feature = "store-sqlite"))]
async fn import<'a>(
    _: &'a SplitOutput,
    _: &crate::resolve::SplitDestination,
    _: &mut Vec<TargetReport<'a>>,
) -> Result<String, CliError> {
    Err(CliError::Usage(
        "split import requires --features store-sqlite".into(),
    ))
}

/// Publish a provisioned, empty destination at `output`, create-new: the
/// schema goes into a private sibling first, which is linked into place only
/// when complete, so a failed provision never leaves a half-made output.
#[cfg(feature = "store-sqlite")]
async fn create_destination(output: &Path) -> Result<(), CliError> {
    use crate::resolve::SplitDestination;
    let name = output
        .file_name()
        .ok_or_else(|| CliError::Usage("--output-database must name a file".into()))?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".{}.partial", uuid::Uuid::new_v4()));
    let temp = output.with_file_name(temp_name);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| CliError::Usage(format!("create {}: {e}", temp.display())))?;
    let store = SplitDestination::open(&temp);
    let provisioned = store.provision_split_destination().await;
    let finished = store.finish_split_destination().await;
    let linked = provisioned
        .and(finished)
        .map_err(|e| CliError::Runtime(format!("provision split destination: {e}")))
        .and_then(|_| {
            std::fs::hard_link(&temp, output).map_err(|e| {
                CliError::Usage(format!(
                    "create new split destination {}: {e}",
                    output.display()
                ))
            })
        });
    let removed = std::fs::remove_file(&temp);
    for suffix in SIDECARS {
        let _ = std::fs::remove_file(with_suffix(&temp, suffix));
    }
    linked?;
    removed.map_err(|e| CliError::Runtime(format!("remove {}: {e}", temp.display())))
}

/// Import every target, one transaction each, in session order. A new
/// output is created first; an existing one is accepted only as this same
/// split's output, checked read-only before any import.
#[cfg(feature = "store-sqlite")]
async fn import<'a>(
    split: &'a SplitOutput,
    destination: &crate::resolve::SplitDestination,
    reports: &mut Vec<TargetReport<'a>>,
) -> Result<String, CliError> {
    use crate::store::{SplitImportOutcome, SplitReceipt};
    let output = destination.path();
    let created = match std::fs::symlink_metadata(output) {
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            create_destination(output).await?;
            true
        }
        Err(e) => {
            return Err(CliError::Usage(format!(
                "inspect {}: {e}",
                output.display()
            )))
        }
    };
    let names: std::collections::BTreeSet<String> =
        split.targets.iter().map(|t| t.session.clone()).collect();
    let store = destination.store();
    let result =
        async {
            store
                .verify_split_destination(&split.split_sha256, &names)
                .await
                .map_err(|e| CliError::Usage(e.to_string()))?;
            let (mut imported, mut present, mut rows) = (0usize, 0usize, 0u64);
            for t in &split.targets {
                let receipt = SplitReceipt::for_target(
                    &t.snapshot,
                    &split.split_sha256,
                    &t.sha256,
                    split.algorithm_version,
                    &split.source_sha256,
                    &split.plan_sha256,
                );
                let outcome = store
                    .import_split_target(&receipt, &t.snapshot, &names)
                    .await
                    .map_err(|e| {
                        CliError::Runtime(format!(
                            "import target {} ({imported} imported, {present} verified before \
                         it; a rerun resumes): {e}",
                            t.session
                        ))
                    })?;
                let (label, changes) = match outcome {
                    SplitImportOutcome::Imported { changes } => {
                        imported += 1;
                        ("imported", changes)
                    }
                    SplitImportOutcome::AlreadyPresent => {
                        present += 1;
                        ("already_present", 0)
                    }
                };
                rows += changes;
                reports.push(TargetReport {
                    session: &t.session,
                    sha256: &t.sha256,
                    outcome: label,
                    rows_written: changes,
                });
            }
            Ok::<_, CliError>(format!(
            "{}{imported} imported, {present} already present and verified, {rows} rows written",
            if created { "new output; " } else { "existing output; " }
        ))
        }
        .await;
    let finished = store
        .finish_split_destination()
        .await
        .map_err(|e| CliError::Runtime(format!("close split destination: {e}")));
    let summary = result?;
    finished?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::resolve_classification_source;

    fn inputs() -> (PathBuf, ClassifyArgs) {
        let dir = std::env::temp_dir().join(format!("lambo-classify-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let (source, rules) = crate::session::tests::fixture();
        let snapshot = dir.join("snapshot.json");
        std::fs::write(&snapshot, serde_json::to_vec(&source).unwrap()).unwrap();
        let rules_file = dir.join("rules.json");
        std::fs::write(&rules_file, serde_json::to_vec(&rules).unwrap()).unwrap();
        (
            snapshot,
            ClassifyArgs {
                session: "mixed".into(),
                rules: rules_file,
                overrides: None,
                reviewed_assignments: None,
                output: dir.join("plan.json"),
            },
        )
    }

    #[tokio::test]
    async fn snapshot_classification_preserves_inputs_and_refuses_output_aliases() {
        let (snapshot, mut args) = inputs();
        let before = std::fs::read(&snapshot).unwrap();
        let rules_before = std::fs::read(&args.rules).unwrap();
        args.output = snapshot.clone();
        assert!(classify_session(
            resolve_classification_source(None, Some(&snapshot)).unwrap(),
            args
        )
        .await
        .is_err());
        assert_eq!(std::fs::read(&snapshot).unwrap(), before);
        let (snapshot, mut args) = inputs();
        let alias = snapshot.parent().unwrap().join("alias.json");
        std::os::unix::fs::symlink(&snapshot, &alias).unwrap();
        args.output = alias;
        assert!(classify_session(
            resolve_classification_source(None, Some(&snapshot)).unwrap(),
            args
        )
        .await
        .is_err());
        let (snapshot, mut args) = inputs();
        let rules = args.rules.clone();
        args.output = rules.clone();
        assert!(classify_session(
            resolve_classification_source(None, Some(&snapshot)).unwrap(),
            args
        )
        .await
        .is_err());
        assert_eq!(std::fs::read(&rules).unwrap(), rules_before);
    }

    #[tokio::test]
    async fn snapshot_runs_without_a_backend_and_writes_only_a_new_private_plan() {
        let (snapshot, args) = inputs();
        let out = args.output.clone();
        let summary = classify_session(
            resolve_classification_source(None, Some(&snapshot)).unwrap(),
            args,
        )
        .await
        .unwrap();
        assert!(summary.contains("4 interactions, 6 concepts"));
        assert!(summary.ends_with("source opened read-only"));
        let plan: crate::session::ClassificationPlan =
            serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        let source: GraphSnapshot =
            serde_json::from_slice(&std::fs::read(&snapshot).unwrap()).unwrap();
        crate::session::validate_plan(&plan, &source).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(out).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn failed_or_clobbering_writes_leave_no_partial_or_replaced_plan() {
        let dir = std::env::temp_dir().join(format!("lambo-publish-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let out = dir.join("plan.json");
        let failed = write_new_private(&out, |f| {
            f.write_all(b"{\"partial\":")?;
            Err(std::io::Error::other("disk full"))
        });
        assert!(matches!(failed, Err(CliError::Runtime(_))));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::write(&out, b"existing").unwrap();
        assert!(write_new_private(&out, |f| f.write_all(b"new")).is_err());
        assert_eq!(std::fs::read(&out).unwrap(), b"existing");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let fresh = dir.join("fresh.json");
        write_new_private(&fresh, |f| f.write_all(b"new")).unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"new");
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&fresh).unwrap().nlink(), 1);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn metadata_only_wrong_session_and_unknown_rules_do_not_emit_plans() {
        let (snapshot, args) = inputs();
        let out = args.output.clone();
        std::fs::write(&snapshot, r#"{"session_id":"mixed"}"#).unwrap();
        assert!(classify_session(
            resolve_classification_source(None, Some(&snapshot)).unwrap(),
            args
        )
        .await
        .is_err());
        assert!(!out.exists());
        let (snapshot, mut args) = inputs();
        args.session = "wrong".into();
        assert!(classify_session(
            resolve_classification_source(None, Some(&snapshot)).unwrap(),
            args
        )
        .await
        .is_err());
        let (snapshot, args) = inputs();
        std::fs::write(&args.rules, r#"{"unknown":true}"#).unwrap();
        assert!(classify_session(
            resolve_classification_source(None, Some(&snapshot)).unwrap(),
            args
        )
        .await
        .is_err());
    }

    mod split {
        use super::super::*;
        use crate::resolve::resolve_split_io;
        use crate::session::split_tests::{labels, overrides, plan_for, rules, source};

        struct Inputs {
            dir: PathBuf,
            snapshot: PathBuf,
            args: SplitArgs,
        }

        fn inputs() -> Inputs {
            let dir =
                std::env::temp_dir().join(format!("lambo-split-cli-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            let source = source();
            let snapshot = dir.join("snapshot.json");
            std::fs::write(&snapshot, serde_json::to_vec(&source).unwrap()).unwrap();
            let rules_file = dir.join("rules.json");
            std::fs::write(&rules_file, serde_json::to_vec(&rules()).unwrap()).unwrap();
            let labels_file = dir.join("labels.jsonl");
            let lines: Vec<String> = labels()
                .iter()
                .map(|l| serde_json::to_string(l).unwrap())
                .collect();
            std::fs::write(&labels_file, lines.join("\n")).unwrap();
            let overrides_file = dir.join("overrides.jsonl");
            let lines: Vec<String> = overrides()
                .iter()
                .map(|o| serde_json::to_string(o).unwrap())
                .collect();
            std::fs::write(&overrides_file, lines.join("\n")).unwrap();
            let plan = dir.join("plan.json");
            std::fs::write(&plan, serde_json::to_vec(&plan_for(&source)).unwrap()).unwrap();
            Inputs {
                args: SplitArgs {
                    session: "mixed".into(),
                    plan,
                    rules: rules_file,
                    reviewed_assignments: Some(labels_file),
                    overrides: Some(overrides_file),
                    report: dir.join("report.json"),
                    dry_run: false,
                },
                snapshot,
                dir,
            }
        }

        async fn run(i: &Inputs, output: &Path, args: SplitArgs) -> Result<String, CliError> {
            let io = resolve_split_io(None, Some(&i.snapshot), output, args.dry_run)
                .map_err(|e| CliError::Usage(e.to_string()))?;
            split_session(io, args).await
        }

        fn refused(result: Result<String, CliError>, needle: &str) {
            let err = result.unwrap_err().to_string();
            assert!(err.contains(needle), "{err}");
        }

        #[cfg(feature = "store-sqlite")]
        const ALIAS: &str = "must not name, link to or share a file";

        fn again(args: &SplitArgs, report: &str) -> SplitArgs {
            SplitArgs {
                session: args.session.clone(),
                plan: args.plan.clone(),
                rules: args.rules.clone(),
                reviewed_assignments: args.reviewed_assignments.clone(),
                overrides: args.overrides.clone(),
                report: args.report.with_file_name(report),
                dry_run: args.dry_run,
            }
        }

        #[tokio::test]
        async fn dry_run_reports_without_a_destination() {
            let mut i = inputs();
            i.args.dry_run = true;
            let output = i.dir.join("split.db");
            let args = again(&i.args, "report.json");
            let summary = run(&i, &output, args).await.unwrap();
            assert!(
                summary.contains("3 targets, dry run, nothing written"),
                "{summary}"
            );
            assert!(summary.contains("source edges 26: intra 14, cross-session Temporal rebuilt 5, hub reattached 2, Derives reattached 2, dropped 3"), "{summary}");
            assert!(summary.contains("LOW CONFIDENCE: 2 items"), "{summary}");
            assert!(!output.exists());
            let report: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&i.args.report).unwrap()).unwrap();
            assert_eq!(report["dry_run"], true);
            assert_eq!(report["low_confidence_count"], 2);
            assert_eq!(report["ledger"].as_array().unwrap().len(), 26);
            // Ids and counts only: no content leaves the source.
            let text = std::fs::read_to_string(&i.args.report).unwrap();
            assert!(!text.contains("alpha cache is warm"));
            std::fs::remove_dir_all(&i.dir).unwrap();
        }

        #[cfg(feature = "store-sqlite")]
        fn db_bytes(path: &Path) -> Vec<u8> {
            let mut out = std::fs::read(path).unwrap();
            if let Ok(wal) = std::fs::read(super::super::with_suffix(path, "-wal")) {
                out.extend(wal);
            }
            out
        }

        #[cfg(feature = "store-sqlite")]
        #[tokio::test(flavor = "multi_thread")]
        async fn split_imports_once_and_a_rerun_verifies_with_zero_writes() {
            let i = inputs();
            let output = i.dir.join("split.db");
            let summary = run(&i, &output, again(&i.args, "report.json"))
                .await
                .unwrap();
            assert!(
                summary.contains("new output; 3 imported, 0 already present"),
                "{summary}"
            );
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let meta = std::fs::metadata(&output).unwrap();
            assert_eq!(meta.nlink(), 1);
            assert_eq!(
                std::fs::metadata(&i.args.report)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            // No provisioning temp file survives.
            let names: Vec<_> = std::fs::read_dir(&i.dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert!(!names.iter().any(|n| n.ends_with(".partial")), "{names:?}");
            let before = db_bytes(&output);
            let summary = run(&i, &output, again(&i.args, "report-2.json"))
                .await
                .unwrap();
            assert!(
                summary.contains(
                    "existing output; 0 imported, 3 already present and verified, 0 rows written"
                ),
                "{summary}"
            );
            assert_eq!(db_bytes(&output), before);
            let report: serde_json::Value = serde_json::from_slice(
                &std::fs::read(i.args.report.with_file_name("report-2.json")).unwrap(),
            )
            .unwrap();
            assert!(report["targets"]
                .as_array()
                .unwrap()
                .iter()
                .all(|t| t["outcome"] == "already_present" && t["rows_written"] == 0));
            std::fs::remove_dir_all(&i.dir).unwrap();
        }

        #[cfg(feature = "store-sqlite")]
        #[tokio::test]
        async fn outputs_that_alias_inputs_or_existing_reports_are_refused() {
            let i = inputs();
            let snapshot_before = std::fs::read(&i.snapshot).unwrap();
            // The output names the snapshot.
            refused(
                run(&i, &i.snapshot.clone(), again(&i.args, "r1.json")).await,
                ALIAS,
            );
            // A hardlink to the plan.
            let link = i.dir.join("link.db");
            std::fs::hard_link(&i.args.plan, &link).unwrap();
            refused(run(&i, &link, again(&i.args, "r2.json")).await, ALIAS);
            // A symlink, even to a new name.
            let sym = i.dir.join("sym.db");
            std::os::unix::fs::symlink(i.dir.join("elsewhere.db"), &sym).unwrap();
            refused(run(&i, &sym, again(&i.args, "r3.json")).await, ALIAS);
            // A directory alias of the rules file.
            let alias_dir = i.dir.join("alias");
            std::os::unix::fs::symlink(&i.dir, &alias_dir).unwrap();
            refused(
                run(&i, &alias_dir.join("rules.json"), again(&i.args, "r4.json")).await,
                ALIAS,
            );
            // The output's WAL spelled as the report, and an existing report.
            let output = i.dir.join("split.db");
            refused(
                run(&i, &output, again(&i.args, "split.db-wal")).await,
                ALIAS,
            );
            std::fs::write(i.dir.join("taken.json"), b"keep").unwrap();
            refused(
                run(&i, &output, again(&i.args, "taken.json")).await,
                "already exists",
            );
            assert_eq!(std::fs::read(i.dir.join("taken.json")).unwrap(), b"keep");
            assert!(!output.exists());
            assert_eq!(std::fs::read(&i.snapshot).unwrap(), snapshot_before);
            // A URI or another store kind is not an import destination.
            assert!(
                resolve_split_io(None, Some(&i.snapshot), Path::new("sqlite::memory:"), false)
                    .is_err()
            );
            assert!(
                resolve_split_io(None, Some(&i.snapshot), Path::new("postgres://h/db"), false)
                    .is_err()
            );
            std::fs::remove_dir_all(&i.dir).unwrap();
        }

        #[tokio::test]
        async fn plans_that_do_not_reproduce_are_refused() {
            let mut i = inputs();
            i.args.dry_run = true;
            let output = i.dir.join("split.db");
            // Different rules.
            let mut changed = rules();
            changed.neighbour_window_secs += 1;
            std::fs::write(&i.args.rules, serde_json::to_vec(&changed).unwrap()).unwrap();
            refused(
                run(&i, &output, again(&i.args, "r1.json")).await,
                "--rules differ",
            );
            std::fs::write(&i.args.rules, serde_json::to_vec(&rules()).unwrap()).unwrap();
            // Missing labels for a reviewed plan.
            let mut args = again(&i.args, "r2.json");
            args.reviewed_assignments = None;
            refused(
                run(&i, &output, args).await,
                "pass the same --reviewed-assignments",
            );
            // The plan's own overrides do not authorize themselves.
            let mut args = again(&i.args, "r2b.json");
            args.overrides = None;
            refused(run(&i, &output, args).await, "pass the same --overrides");
            let empty = i.dir.join("empty.jsonl");
            std::fs::write(&empty, "").unwrap();
            let mut args = again(&i.args, "r2c.json");
            args.overrides = Some(empty);
            refused(run(&i, &output, args).await, "--overrides differ");
            // An edited and resealed plan no longer reproduces.
            let mut plan = plan_for(&source());
            let row = plan
                .assignments
                .iter_mut()
                .find(|a| a.confidence == crate::session::Confidence::High)
                .unwrap();
            row.signals.push("edited".into());
            plan.plan_sha256 = crate::session::split_tests::reseal(&plan);
            crate::session::validate_plan(&plan, &source()).unwrap();
            std::fs::write(&i.args.plan, serde_json::to_vec(&plan).unwrap()).unwrap();
            let err = run(&i, &output, again(&i.args, "r3.json"))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("does not reproduce"), "{err}");
            assert!(!i.dir.join("r3.json").exists());
            std::fs::remove_dir_all(&i.dir).unwrap();
        }

        #[cfg(feature = "store-sqlite")]
        #[tokio::test(flavor = "multi_thread")]
        async fn a_report_that_fails_after_the_imports_commit_says_so_and_a_rerun_writes_nothing() {
            use std::os::unix::fs::PermissionsExt;
            let i = inputs();
            let output = i.dir.join("split.db");
            let locked = i.dir.join("locked");
            std::fs::create_dir(&locked).unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
            let mut args = again(&i.args, "unused.json");
            args.report = locked.join("report.json");
            let err = run(&i, &output, args).await.unwrap_err().to_string();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(err.contains("the imports are committed"), "{err}");
            let before = db_bytes(&output);
            let summary = run(&i, &output, again(&i.args, "report.json"))
                .await
                .unwrap();
            assert!(
                summary.contains("0 imported, 3 already present"),
                "{summary}"
            );
            assert_eq!(db_bytes(&output), before);
            std::fs::remove_dir_all(&i.dir).unwrap();
        }

        #[cfg(feature = "store-sqlite")]
        #[tokio::test(flavor = "multi_thread")]
        async fn two_concurrent_runs_on_a_new_output_fail_closed() {
            let i = inputs();
            let output = i.dir.join("split.db");
            let (a, b) = tokio::join!(
                run(&i, &output, again(&i.args, "a.json")),
                run(&i, &output, again(&i.args, "b.json"))
            );
            // At most one provisioned the file; the other either refused or
            // verified what the first committed. Never a corrupt output.
            assert!(a.is_ok() || b.is_ok(), "{a:?} {b:?}");
            let summary = run(&i, &output, again(&i.args, "c.json")).await.unwrap();
            assert!(
                summary.contains("3 already present and verified, 0 rows written")
                    || summary.contains("imported"),
                "{summary}"
            );
            let summary = run(&i, &output, again(&i.args, "d.json")).await.unwrap();
            assert!(
                summary.contains("0 imported, 3 already present"),
                "{summary}"
            );
            std::fs::remove_dir_all(&i.dir).unwrap();
        }

        #[cfg(all(feature = "store-sqlite", feature = "fixtures"))]
        #[tokio::test(flavor = "multi_thread")]
        async fn a_database_source_is_only_read() {
            use crate::store::SqliteStore;
            let i = inputs();
            // A completed standalone backup, as the operator would copy one.
            let live = i.dir.join("live.db");
            let seeded = SqliteStore::connect(live.to_str().unwrap()).unwrap();
            crate::store::GraphStore::init_schema(&seeded)
                .await
                .unwrap();
            seeded.seed(&source()).await.unwrap();
            seeded.finish_split_destination().await.unwrap();
            let db = i.dir.join("backup.db");
            let raw = sqlx::SqlitePool::connect(&format!("sqlite://{}", live.display()))
                .await
                .unwrap();
            sqlx::query("VACUUM INTO ?")
                .bind(db.to_str().unwrap())
                .execute(&raw)
                .await
                .unwrap();
            raw.close().await;
            let store = SqliteStore::connect_read_only(&db).unwrap();
            let loaded = crate::store::GraphStore::load_session(
                &store,
                &crate::types::SessionId::from("mixed"),
            )
            .await
            .unwrap();
            drop(store);
            std::fs::write(
                &i.args.plan,
                serde_json::to_vec(&plan_for(&loaded)).unwrap(),
            )
            .unwrap();
            let before = std::fs::read(&db).unwrap();
            let output = i.dir.join("split.db");
            let io = resolve_split_io(Some(&db), None, &output, false).unwrap();
            let summary = split_session(io, again(&i.args, "report.json"))
                .await
                .unwrap();
            assert!(summary.contains("3 imported"), "{summary}");
            assert!(summary.ends_with("source opened read-only"));
            assert_eq!(std::fs::read(&db).unwrap(), before);
            for suffix in ["-wal", "-shm", "-journal"] {
                assert!(!super::super::with_suffix(&db, suffix).exists(), "{suffix}");
            }
            // The output must not be the source itself.
            let io = resolve_split_io(Some(&db), None, &db, false).unwrap();
            assert!(split_session(io, again(&i.args, "report-2.json"))
                .await
                .is_err());
            assert_eq!(std::fs::read(&db).unwrap(), before);
            std::fs::remove_dir_all(&i.dir).unwrap();
        }

        /// `lambo session verify` end to end on a real split output (#33 D3).
        #[cfg(all(feature = "store-sqlite", feature = "embed-fixture"))]
        mod verify {
            use super::*;
            use crate::resolve::resolve_verify_io;

            fn verify_args(i: &Inputs, queries: &Path, report: &str) -> VerifyArgs {
                VerifyArgs {
                    session: "mixed".into(),
                    plan: i.args.plan.clone(),
                    queries: queries.to_path_buf(),
                    report: i.dir.join(report),
                    top_k: Some(10),
                    max_tokens: Some(4000),
                    traversal_depth: None,
                    at: None,
                    aggregate_only: vec![],
                }
            }

            async fn verify(
                i: &Inputs,
                snapshot: &Path,
                output: &Path,
                args: VerifyArgs,
            ) -> Result<String, CliError> {
                let config = i.dir.join("lambo.toml");
                let io = resolve_verify_io(&config, None, Some(snapshot), output)
                    .map_err(|e| CliError::Usage(e.to_string()))?;
                verify_session(io, args).await
            }

            /// The serving config verify replays: a SQLite store that is
            /// parsed but never opened, every target pinned, general layered.
            const SERVING: &str =
                "[store]\nkind = \"sqlite\"\ndsn = \"/nonexistent/never-opened.db\"\n\n\
                [embedder]\nkind = \"fixture\"\ndim = 2\n\n\
                [serve]\nsessions = [\"alpha\", \"beta\", \"general\"]\n\
                default_session = \"alpha\"\nlayers = [\"general\"]\n";

            async fn split_output(i: &Inputs) -> (PathBuf, PathBuf) {
                let output = i.dir.join("split.db");
                run(i, &output, again(&i.args, "split-report.json"))
                    .await
                    .unwrap();
                std::fs::write(i.dir.join("lambo.toml"), SERVING).unwrap();
                let queries = i.dir.join("queries.toml");
                std::fs::write(
                    &queries,
                    "[queries]\nalpha = [\"alpha cache is warm\", \"beta parser is strict\"]\n\
                     beta = [\"beta result is green\"]\n",
                )
                .unwrap();
                (output, queries)
            }

            fn tampered(output: &Path, name: &str, sql: &str) -> PathBuf {
                let copy = output.with_file_name(name);
                std::fs::copy(output, &copy).unwrap();
                let rt = tokio::runtime::Handle::current();
                let sql = sql.to_owned();
                let path = copy.clone();
                std::thread::spawn(move || {
                    rt.block_on(async move {
                        use sqlx::{ConnectOptions, Connection};
                        let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
                            .filename(&path)
                            .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete)
                            .connect()
                            .await
                            .unwrap();
                        sqlx::query(&sql).execute(&mut conn).await.unwrap();
                        conn.close().await.unwrap();
                    })
                })
                .join()
                .unwrap();
                copy
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn verify_reports_aggregates_only_and_leaves_both_inputs_unchanged() {
                let i = inputs();
                let (output, queries) = split_output(&i).await;
                let source_bytes = std::fs::read(&i.snapshot).unwrap();
                let output_bytes = std::fs::read(&output).unwrap();
                let summary = verify(
                    &i,
                    &i.snapshot,
                    &output,
                    verify_args(&i, &queries, "v1.json"),
                )
                .await
                .unwrap();
                assert!(
                    summary.contains("alpha (layers general): 2 queries"),
                    "{summary}"
                );
                assert!(summary.contains("FOREIGN-PRIMARY AFTER"), "{summary}");
                assert!(summary.contains("measures SQLite holder"), "{summary}");
                assert!(summary.ends_with("opened read-only"), "{summary}");
                // Aggregates only: no source id or query text on stdout.
                let source = source();
                for c in &source.concepts {
                    assert!(!summary.contains(&c.id.0.to_string()));
                }
                assert!(!summary.contains("alpha cache is warm"));
                assert_eq!(std::fs::read(&i.snapshot).unwrap(), source_bytes);
                assert_eq!(std::fs::read(&output).unwrap(), output_bytes);
                for suffix in ["-wal", "-shm", "-journal"] {
                    assert!(!super::super::super::with_suffix(&output, suffix).exists());
                }
                use std::os::unix::fs::PermissionsExt;
                let report = i.dir.join("v1.json");
                let mode = std::fs::metadata(&report).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
                // Deterministic, and a report is never overwritten.
                verify(
                    &i,
                    &i.snapshot,
                    &output,
                    verify_args(&i, &queries, "v2.json"),
                )
                .await
                .unwrap();
                assert_eq!(
                    std::fs::read(&report).unwrap(),
                    std::fs::read(i.dir.join("v2.json")).unwrap()
                );
                refused(
                    verify(
                        &i,
                        &i.snapshot,
                        &output,
                        verify_args(&i, &queries, "v1.json"),
                    )
                    .await,
                    "never overwritten",
                );
                std::fs::remove_dir_all(&i.dir).unwrap();
            }

            /// L5: verify replays a SQLite holder, so another store kind or
            /// a [recall] tier in the serving config is refused up front.
            #[tokio::test(flavor = "multi_thread")]
            async fn a_serving_config_verify_does_not_model_is_refused() {
                let i = inputs();
                let (output, _) = split_output(&i).await;
                let config = i.dir.join("lambo.toml");
                for (body, needle) in [
                    (
                        SERVING.replace("kind = \"sqlite\"", "kind = \"memory\""),
                        "measures SQLite-holder behaviour",
                    ),
                    (
                        format!("{SERVING}\n[recall]\nkind = \"elastic\"\nurl = \"http://127.0.0.1:1\"\n"),
                        "recall",
                    ),
                ] {
                    std::fs::write(&config, body).unwrap();
                    let err = resolve_verify_io(&config, None, Some(&i.snapshot), &output)
                        .err()
                        .expect("refused")
                        .to_string();
                    assert!(err.contains(needle), "{err}");
                }
                std::fs::remove_dir_all(&i.dir).unwrap();
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn outputs_that_do_not_match_the_source_or_plan_are_refused() {
                let i = inputs();
                let (output, queries) = split_output(&i).await;
                let args = |name: &str| verify_args(&i, &queries, name);
                const MISMATCH: &str = "does not match this source and plan";

                let edited = tampered(
                    &output,
                    "edited.db",
                    "UPDATE concepts SET content = content || ' (edited)' \
                     WHERE session_id = 'alpha' AND rowid = (SELECT min(rowid) FROM concepts \
                     WHERE session_id = 'alpha')",
                );
                refused(
                    verify(&i, &i.snapshot, &edited, args("r1.json")).await,
                    MISMATCH,
                );

                let unreceipted = tampered(
                    &output,
                    "unreceipted.db",
                    "DELETE FROM split_imports WHERE target_session = 'beta'",
                );
                refused(
                    verify(&i, &i.snapshot, &unreceipted, args("r2.json")).await,
                    MISMATCH,
                );

                let foreign = tampered(
                    &output,
                    "foreign.db",
                    "UPDATE split_imports SET split_sha256 = 'other'",
                );
                refused(
                    verify(&i, &i.snapshot, &foreign, args("r3.json")).await,
                    MISMATCH,
                );

                // Another source: the plan's seal no longer matches it.
                let mut other = source();
                other.interactions[0].prompt_text = Some("changed".into());
                let other_snapshot = i.dir.join("other.json");
                std::fs::write(&other_snapshot, serde_json::to_vec(&other).unwrap()).unwrap();
                assert!(verify(&i, &other_snapshot, &output, args("r4.json"))
                    .await
                    .is_err());

                // Not a split output at all.
                refused(
                    verify(&i, &i.snapshot, &i.snapshot, args("r5.json")).await,
                    "",
                );
                for name in ["r1.json", "r2.json", "r3.json", "r4.json", "r5.json"] {
                    assert!(!i.dir.join(name).exists(), "{name}");
                }
                std::fs::remove_dir_all(&i.dir).unwrap();
            }
        }
    }
}

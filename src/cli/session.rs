//! `lambo session classify` (read-only: an explicit input, never a writer)
//! and `lambo session split` (#33 D2: writes a validated plan's targets into
//! a new offline SQLite file; the source stays read-only).

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::caps::{require_nonempty, CliError};
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
        None => plan.overrides.clone(),
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
         origin reattached {}, dropped {}\n",
        e.source_edges,
        e.intra,
        e.cross_temporal_rebuilt,
        e.hub_reattached,
        e.origin_reattached,
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
    if !a.general_replicas_from_excluded_projects.is_empty() {
        out.push_str(&format!(
            "GENERAL PROVENANCE: {} interactions assigned to projects excluded from general are \
             copied into general as origins of audited general rulings (listed in the report); \
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
        import(&split, &output, &mut targets).await?
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

#[cfg(not(feature = "store-sqlite"))]
async fn import<'a>(
    _: &'a SplitOutput,
    _: &Path,
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
    store.close().await;
    let linked = provisioned
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
    output: &Path,
    reports: &mut Vec<TargetReport<'a>>,
) -> Result<String, CliError> {
    use crate::resolve::SplitDestination;
    use crate::store::{SplitImportOutcome, SplitReceipt};
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
    let store = SplitDestination::open(output);
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
    store.close().await;
    result
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
        use crate::session::split_tests::{labels, plan_for, rules, source};

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
            let plan = dir.join("plan.json");
            std::fs::write(&plan, serde_json::to_vec(&plan_for(&source)).unwrap()).unwrap();
            Inputs {
                args: SplitArgs {
                    session: "mixed".into(),
                    plan,
                    rules: rules_file,
                    reviewed_assignments: Some(labels_file),
                    overrides: None,
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
            assert!(summary.contains("source edges 26: intra 14, cross-session Temporal rebuilt 5, hub reattached 2, origin reattached 2, dropped 3"), "{summary}");
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
            seeded.close().await;
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
            let loaded = {
                let store = SqliteStore::connect_read_only(&db).unwrap();
                let s = crate::store::GraphStore::load_session(
                    &store,
                    &crate::types::SessionId::from("mixed"),
                )
                .await
                .unwrap();
                store.close().await;
                s
            };
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
    }
}

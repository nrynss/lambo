//! `lambo session classify` (read-only: an explicit input, never a writer)
//! and `lambo session split` (#33 D2: writes a validated plan's targets into
//! a new offline SQLite file; the source stays read-only).

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::caps::{require_nonempty, CliError};
use crate::resolve::ClassificationSource;
use crate::session::{
    classify, classify_reviewed, ClassificationRules, OperatorOverride, ReviewedAssignment,
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
}

//! Read-only `lambo session classify`: an explicit input, never a writer.

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

/// Produces a private plan, printing only aggregate counts and its path.
/// `create_new` refuses existing targets including source/config symlinks and
/// hardlinks, so an output spelling cannot truncate an input.
pub async fn classify_session(
    source: ClassificationSource,
    args: ClassifyArgs,
) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    let text = read_bounded(&args.rules, 4 * 1024 * 1024)?;
    let rules: ClassificationRules = if args.rules.extension().is_some_and(|e| e == "toml") {
        toml::from_str(&text)
            .map_err(|e| CliError::Usage(format!("classification rules: {}", e.message())))?
    } else {
        serde_json::from_str(&text)
            .map_err(|e| CliError::Usage(format!("classification rules: {e}")))?
    };
    rules
        .validate()
        .map_err(|e| CliError::Usage(e.to_string()))?;
    let mut overrides: Vec<OperatorOverride> = Vec::new();
    if let Some(path) = &args.overrides {
        for (n, line) in read_bounded(path, 32 * 1024 * 1024)?.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            overrides.push(
                serde_json::from_str(line)
                    .map_err(|e| CliError::Usage(format!("override line {}: {e}", n + 1)))?,
            );
        }
    }
    let snapshot = match source {
        ClassificationSource::Snapshot(path) => read_snapshot(&path)?,
        ClassificationSource::Database(store) => {
            // Read-only verification, not init_schema. Old schemas must be
            // exported/upgraded on a separate copy, never repaired here.
            store
                .preflight_schema()
                .await
                .map_err(|e| CliError::Runtime(format!("classification schema: {e}")))?;
            store
                .load_session(&SessionId::from(args.session.as_str()))
                .await
                .map_err(|e| CliError::Runtime(format!("classification source: {e}")))?
        }
    };
    if snapshot.session_id.0 != args.session {
        return Err(CliError::Usage(
            "snapshot session does not match --session".into(),
        ));
    }
    let plan = if let Some(path) = &args.reviewed_assignments {
        let mut labels: Vec<ReviewedAssignment> = Vec::new();
        for (n, line) in read_bounded(path, 32 * 1024 * 1024)?.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            labels.push(serde_json::from_str(line).map_err(|e| {
                CliError::Usage(format!("reviewed assignment line {}: {e}", n + 1))
            })?);
        }
        classify_reviewed(&snapshot, &rules, &labels, &overrides)
    } else {
        classify(&snapshot, &rules, &overrides)
    }
    .map_err(|e| CliError::Usage(e.to_string()))?;
    let bytes = serde_json::to_vec_pretty(&plan)
        .map_err(|e| CliError::Runtime(format!("render classification plan: {e}")))?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&args.output)
        .map_err(|e| {
            CliError::Usage(format!(
                "create new classification plan {}: {e}",
                args.output.display()
            ))
        })?;
    output
        .write_all(&bytes)
        .and_then(|_| output.write_all(b"\n"))
        .and_then(|_| output.sync_all())
        .map_err(|e| CliError::Runtime(format!("write classification plan: {e}")))?;
    Ok(format!("classification plan {}: {} interactions, {} concepts, {} low-confidence items, {} shared hubs; source unchanged", args.output.display(), plan.source.interactions, plan.source.concepts, plan.low_confidence.len(), plan.assignments.iter().filter(|a| !a.copy_into.is_empty()).count()))
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

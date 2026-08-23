//! `lambo provision` — schema bootstrap (spec §6.2).
//!
//! * `store.kind = sqlite` → [`GraphStore::init_schema`] on the resolved store
//!   (idempotent).
//! * `store.kind = postgres` → [`GraphStore::init_schema`] (pgvector + hnsw
//!   from init). Never `scripts/provision.sh` (that file is Cockroach SQL).
//! * `store.kind = cockroach` → wrap `scripts/provision.sh` (vector-index
//!   reconciliation lives there, not in `init_schema`'s timeout path).
//! * `store.kind = memory` → success; the memory store needs no schema.
//!
//! DSN is never a CLI flag; it comes from env / config as today. The Cockroach
//! arm **hands the resolved DSN to the script** rather than letting it inherit
//! whatever `LAMBO_COCKROACH_DSN` happens to be in the environment: before
//! E2E-1/E2E-F2, `store.dsn` was not passed to `scripts/provision.sh` and not
//! consulted by it, so `lambo provision` could report success against a cluster
//! the config never named. `StoreConfig::overlay_env` refuses when the two
//! disagree; this makes the file's value the one that reaches the script when
//! they do not.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Ancestor steps from cwd toward `/` when hunting `scripts/provision.sh`.
/// Bounded so a stray `scripts/provision.sh` under `/` is never reached.
const PROVISION_WALK_MAX: usize = 16;

use super::caps::CliError;
use crate::store::{GraphStore, StoreKind};

/// Provision / migrate the durable store schema.
///
/// `dsn` is the resolved `store.dsn` (file overlaid with the kind's environment
/// variable, refusing on disagreement). Only the Cockroach arm uses it: the
/// other kinds provision through the store that was already constructed from
/// the same value.
pub async fn run(
    store: Box<dyn GraphStore>,
    kind: StoreKind,
    dsn: Option<&str>,
) -> Result<String, CliError> {
    match kind {
        StoreKind::Memory => {
            Ok("memory store needs no schema (in-RAM; nothing to provision)".into())
        }
        StoreKind::Sqlite => {
            store
                .init_schema()
                .await
                .map_err(|e| CliError::Runtime(format!("init_schema: {e}")))?;
            Ok("sqlite schema provisioned (init_schema, idempotent)".into())
        }
        StoreKind::Postgres => {
            // init_schema, not scripts/provision.sh (that file is Cockroach SQL).
            store
                .init_schema()
                .await
                .map_err(|e| CliError::Runtime(format!("init_schema: {e}")))?;
            Ok("postgres schema provisioned (init_schema, idempotent, hnsw from init)".into())
        }
        StoreKind::Cockroach => {
            let script = find_provision_script().ok_or_else(|| {
                CliError::Runtime(format!(
                    "scripts/provision.sh not found beside a Cargo.toml whose package \
                     name is lambo (looked from the current directory up to \
                     {PROVISION_WALK_MAX} parents); run from the lambo repo"
                ))
            })?;
            eprintln!("lambo provision: executing {}", script.display());
            let status = provision_command(&script, dsn).status().map_err(|e| {
                CliError::Runtime(format!("failed to spawn {}: {e}", script.display()))
            })?;
            if !status.success() {
                return Err(CliError::Runtime(format!(
                    "{} exited {}",
                    script.display(),
                    status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into())
                )));
            }
            Ok(format!(
                "cockroach schema provisioned via {}",
                script.display()
            ))
        }
    }
}

/// The child that runs `scripts/provision.sh`, with the resolved DSN pushed
/// into the one variable that script reads.
///
/// The script's DSN line is `DSN="${LAMBO_COCKROACH_DSN:-}"`: it consults the
/// environment and nothing else. Inheriting the ambient value is how `lambo
/// provision` came to report success against a production cluster while the
/// operator's `lambo.toml` named a local container (E2E-1). Setting it here
/// makes the resolved config the authority. When the config carries no DSN the
/// variable is left alone, so the pre-existing "secret lives only in the
/// environment" path still works.
fn provision_command(script: &Path, dsn: Option<&str>) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg(script);
    if let Some(dsn) = dsn {
        cmd.env(crate::store::COCKROACH_DSN_ENV, dsn);
    }
    cmd
}

fn find_provision_script() -> Option<PathBuf> {
    let start = std::env::current_dir().ok()?;
    find_provision_script_from(&start, PROVISION_WALK_MAX)
}

/// Walk `start` and at most `max_up` parents. Execute only a script that sits
/// next to a `[package] name = "lambo"` Cargo.toml (repo-root marker).
fn find_provision_script_from(start: &Path, max_up: usize) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    for _ in 0..=max_up {
        let candidate = dir.join("scripts").join("provision.sh");
        if candidate.is_file() && is_lambo_repo_root(&dir) {
            return Some(candidate);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

fn is_lambo_repo_root(dir: &Path) -> bool {
    let cargo = dir.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(cargo) else {
        return false;
    };
    cargo_toml_package_name_is_lambo(&text)
}

fn cargo_toml_package_name_is_lambo(text: &str) -> bool {
    let Some(rest) = text.split("[package]").nth(1) else {
        return false;
    };
    let section = rest.split('[').next().unwrap_or(rest);
    for line in section.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == "name" {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            return v == "lambo";
        }
    }
    false
}

#[cfg(all(test, feature = "store-memory"))]
mod tests {
    use super::*;
    use crate::MemoryStore;

    #[tokio::test]
    async fn provision_memory_store_succeeds_without_sql() {
        let store: Box<dyn GraphStore> = Box::new(MemoryStore::new());
        let out = run(store, StoreKind::Memory, None)
            .await
            .expect("memory provision");
        assert!(
            out.contains("needs no schema"),
            "memory provision must say no schema is needed: {out}"
        );
    }
}

/// B2: the Postgres arm of `run` calls `init_schema` and must not run
/// `scripts/provision.sh`. A dummy store is enough; no live adapter.
#[cfg(test)]
mod postgres_arm_tests {
    use super::*;
    use crate::store::Capabilities;
    use crate::types::StoreError;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct RecordingStore {
        init_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl GraphStore for RecordingStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.init_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn capabilities(&self) -> Capabilities {
            panic!("provision Postgres arm must not touch the store");
        }
        async fn flush(
            &self,
            _batch: &crate::types::MutationBatch,
            _token: Option<u64>,
        ) -> Result<(), StoreError> {
            panic!("provision Postgres arm must not touch the store");
        }
        async fn load_session(
            &self,
            _session: &crate::types::SessionId,
        ) -> Result<crate::types::GraphSnapshot, StoreError> {
            panic!("provision Postgres arm must not touch the store");
        }
        async fn keyword_candidates(
            &self,
            _session: &crate::types::SessionId,
            _tokens: &[String],
            _limit: usize,
        ) -> Result<Vec<crate::types::Scored<crate::types::NodeId>>, StoreError> {
            panic!("provision Postgres arm must not touch the store");
        }
        async fn vector_candidates(
            &self,
            _session: &crate::types::SessionId,
            _embedding: &[f32],
            _limit: usize,
        ) -> Result<Vec<crate::types::Scored<crate::types::NodeId>>, StoreError> {
            panic!("provision Postgres arm must not touch the store");
        }
        async fn blast_radius(
            &self,
            _session: &crate::types::SessionId,
            _node: crate::types::NodeId,
            _min_edge_age: std::time::Duration,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> Result<u64, StoreError> {
            panic!("provision Postgres arm must not touch the store");
        }
        async fn interaction_span(
            &self,
            _session: &crate::types::SessionId,
            _node: crate::types::NodeId,
            _min_age: std::time::Duration,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> Result<crate::types::InteractionSpan, StoreError> {
            panic!("provision Postgres arm must not touch the store");
        }
        async fn record_canonization(
            &self,
            _event: &crate::types::CanonizationEvent,
            _token: Option<u64>,
        ) -> Result<(), StoreError> {
            panic!("provision Postgres arm must not touch the store");
        }
    }

    #[tokio::test]
    async fn provision_postgres_calls_init_schema_not_provision_sh() {
        let calls = Arc::new(AtomicUsize::new(0));
        let store: Box<dyn GraphStore> = Box::new(RecordingStore {
            init_calls: calls.clone(),
        });
        let out = run(store, StoreKind::Postgres, None)
            .await
            .expect("postgres provision must init_schema in B2");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "init_schema must run once");
        assert!(out.contains("postgres"), "{out}");
        assert!(out.contains("init_schema"), "{out}");
        assert!(
            !out.contains("provision.sh"),
            "postgres must not run Cockroach provision.sh: {out}"
        );
    }
}

#[cfg(test)]
mod marker_tests {
    use super::*;
    use std::fs;

    /// E2E-1 / E2E-F2: the resolved `store.dsn` reaches the script, so the
    /// config file names the cluster the DDL lands on. Reverting the `cmd.env`
    /// line makes this fail: the child would inherit whatever
    /// `LAMBO_COCKROACH_DSN` the shell (or `.env`) happened to carry.
    #[test]
    fn cockroach_provision_hands_the_resolved_dsn_to_the_script() {
        let script = Path::new("/tmp/does-not-run/scripts/provision.sh");
        let cmd = provision_command(script, Some("postgresql://u@local:26257/lambo"));
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(
            envs.contains(&(
                "LAMBO_COCKROACH_DSN".to_string(),
                Some("postgresql://u@local:26257/lambo".to_string())
            )),
            "the resolved DSN must be pushed into the child: {envs:?}"
        );

        // No configured DSN: the environment is left exactly as inherited, so
        // the long-standing secret-in-the-environment path still works.
        let bare = provision_command(script, None);
        assert_eq!(
            bare.get_envs().count(),
            0,
            "with no store.dsn the child's environment must not be rewritten"
        );
    }

    #[test]
    fn cargo_toml_marker_requires_package_name_lambo() {
        assert!(cargo_toml_package_name_is_lambo(
            "[package]\nname = \"lambo\"\nversion = \"0.1.0\"\n"
        ));
        assert!(!cargo_toml_package_name_is_lambo(
            "[package]\nname = \"other\"\n"
        ));
        assert!(
            !cargo_toml_package_name_is_lambo("[dependencies]\nlambo = \"1\"\n"),
            "name under [dependencies] is not the package marker"
        );
    }

    #[test]
    fn provision_script_without_lambo_marker_is_ignored() {
        let dir = std::env::temp_dir().join(format!(
            "lambo-prov-marker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("scripts")).expect("scratch");
        fs::write(dir.join("scripts").join("provision.sh"), "#!/bin/sh\n").expect("script");
        assert!(
            find_provision_script_from(&dir, 2).is_none(),
            "script without Cargo.toml marker must not be selected"
        );
        fs::write(dir.join("Cargo.toml"), "[package]\nname = \"not-lambo\"\n").expect("toml");
        assert!(
            find_provision_script_from(&dir, 2).is_none(),
            "wrong package name must not be selected"
        );
        fs::write(dir.join("Cargo.toml"), "[package]\nname = \"lambo\"\n").expect("toml");
        let found = find_provision_script_from(&dir, 2).expect("lambo marker");
        assert_eq!(found, dir.join("scripts").join("provision.sh"));
        let nested = dir.join("a").join("b").join("c");
        fs::create_dir_all(&nested).expect("nested");
        assert!(
            find_provision_script_from(&nested, 1).is_none(),
            "walk must be bounded: 1 ancestor is short of the marker"
        );
        assert!(
            find_provision_script_from(&nested, 3).is_some(),
            "3 ancestors reach the marker from a/b/c"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

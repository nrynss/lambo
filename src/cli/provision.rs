//! `lambo provision` — schema bootstrap (spec §6.2).
//!
//! * `store.kind = sqlite` → [`GraphStore::init_schema`] on the resolved store
//!   (idempotent).
//! * `store.kind = cockroach` → wrap `scripts/provision.sh` (vector-index
//!   reconciliation lives there, not in `init_schema`'s timeout path).
//! * `store.kind = memory` → success; the memory store needs no schema.
//!
//! DSN is never a CLI flag; it comes from env / config as today.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Ancestor steps from cwd toward `/` when hunting `scripts/provision.sh`.
/// Bounded so a stray `scripts/provision.sh` under `/` is never reached.
const PROVISION_WALK_MAX: usize = 16;

use super::caps::CliError;
use crate::store::{GraphStore, StoreKind};

/// Provision / migrate the durable store schema.
pub async fn run(store: Box<dyn GraphStore>, kind: StoreKind) -> Result<String, CliError> {
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
        StoreKind::Cockroach => {
            let script = find_provision_script().ok_or_else(|| {
                CliError::Runtime(format!(
                    "scripts/provision.sh not found beside a Cargo.toml whose package \
                     name is lambo (looked from the current directory up to \
                     {PROVISION_WALK_MAX} parents); run from the lambo repo"
                ))
            })?;
            eprintln!("lambo provision: executing {}", script.display());
            let status = Command::new("bash").arg(&script).status().map_err(|e| {
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
        let out = run(store, StoreKind::Memory)
            .await
            .expect("memory provision");
        assert!(
            out.contains("needs no schema"),
            "memory provision must say no schema is needed: {out}"
        );
    }
}

#[cfg(test)]
mod marker_tests {
    use super::*;
    use std::fs;

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

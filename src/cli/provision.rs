//! `lambo provision` — schema bootstrap (spec §6.2).
//!
//! * `store.kind = sqlite` → [`GraphStore::init_schema`] on the resolved store
//!   (idempotent).
//! * `store.kind = cockroach` → wrap `scripts/provision.sh` (vector-index
//!   reconciliation lives there, not in `init_schema`'s timeout path).
//! * `store.kind = memory` → success; the memory store needs no schema.
//!
//! DSN is never a CLI flag; it comes from env / config as today.

use std::path::PathBuf;
use std::process::Command;

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
                CliError::Runtime(
                    "scripts/provision.sh not found (looked from the current directory \
                     and its parents); run from the lambo repo or install the script"
                        .into(),
                )
            })?;
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
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join("scripts").join("provision.sh");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            break;
        }
    }
    None
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

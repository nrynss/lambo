//! T8.3: `lambo provision` on a fresh SQLite file makes subsequent recall work.
//!
//! Gated on `store-sqlite` (and `embed-fixture`, which is in the default set).
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture"))]

use std::process::Command;
use std::sync::{Mutex, OnceLock};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lambo"));
    for k in [
        "LAMBO_STORE",
        "LAMBO_EMBEDDER",
        "LAMBO_CONFIG",
        "LAMBO_COCKROACH_DSN",
        "DATABASE_URL",
        "LAMBO_SQLITE_PATH",
        "LAMBO_EMBED_DIM",
        "LAMBO_LLAMA_EMBED_URL",
        "LAMBO_LLAMA_MODEL",
    ] {
        cmd.env_remove(k);
    }
    cmd
}

fn scratch() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "lambo-cli-prov-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch");
    let db = dir.join("session.sqlite");
    let cfg = dir.join("lambo.toml");
    std::fs::write(
        &cfg,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n",
            db.display()
        ),
    )
    .expect("write toml");
    (dir, cfg)
}

#[test]
fn provision_on_a_fresh_sqlite_file_makes_recall_work() {
    let _g = env_lock();
    let (dir, cfg) = scratch();
    let cfg_s = cfg.to_str().unwrap();

    let provision = bin()
        .args(["--config", cfg_s, "provision"])
        .output()
        .expect("provision");
    assert!(
        provision.status.success(),
        "provision must succeed on a fresh sqlite file: stderr=\n{}",
        String::from_utf8_lossy(&provision.stderr)
    );
    let stdout = String::from_utf8_lossy(&provision.stdout);
    assert!(
        stdout.contains("sqlite") || stdout.contains("provisioned"),
        "{stdout}"
    );

    // Empty session recall after provision — load_session returns empty, not
    // `no such table`.
    let recall = bin()
        .args([
            "--config",
            cfg_s,
            "recall",
            "--session",
            "fresh-after-provision",
            "--query",
            "anything",
        ])
        .output()
        .expect("recall");
    assert!(
        recall.status.success(),
        "recall after provision must work (T82-10); stderr=\n{}",
        String::from_utf8_lossy(&recall.stderr)
    );

    let derive = bin()
        .args([
            "--config",
            cfg_s,
            "derive",
            "--session",
            "fresh-after-provision",
            "--agent",
            "agent-a",
            "--content",
            "user schema",
            "--kind",
            "entity",
        ])
        .output()
        .expect("derive");
    assert!(
        derive.status.success(),
        "derive after provision: stderr=\n{}",
        String::from_utf8_lossy(&derive.stderr)
    );

    let recall2 = bin()
        .args([
            "--config",
            cfg_s,
            "recall",
            "--session",
            "fresh-after-provision",
            "--query",
            "user schema",
        ])
        .output()
        .expect("recall after derive");
    assert!(
        recall2.status.success(),
        "recall after derive: stderr=\n{}",
        String::from_utf8_lossy(&recall2.stderr)
    );
    let ctx = String::from_utf8_lossy(&recall2.stdout);
    assert!(ctx.contains("user schema"), "{ctx}");

    let _ = std::fs::remove_dir_all(&dir);
}

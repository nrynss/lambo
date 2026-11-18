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
    common::lambo_command()
}

mod common;
use common::ScratchDir;

fn scratch() -> (ScratchDir, std::path::PathBuf) {
    let dir = ScratchDir::new("lambo-cli-prov");
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
    let (_dir, cfg) = scratch();
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

    // #256: the score report is one line on stderr, a JSON object under the
    // names every other surface uses; stdout stays the context block alone.
    assert!(!ctx.contains("recall scores"), "{ctx}");
    assert!(!ctx.contains("top_legs"), "{ctx}");
    let stderr = String::from_utf8_lossy(&recall2.stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .filter_map(|l| l.strip_prefix("recall scores: "))
        .collect();
    assert_eq!(lines.len(), 1, "one score line on stderr: {stderr}");
    let scores: serde_json::Value = serde_json::from_str(lines[0]).expect("a JSON object");
    let top = scores["top_legs"].as_object().expect("top_legs");
    let mut keys: Vec<&str> = top.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["bm25", "exact_name_match", "vector_cosine"]);
    assert!(
        top["bm25"].as_f64().is_some_and(|s| s > 0.0),
        "the keyword leg matched: {scores}"
    );
    assert_eq!(top["exact_name_match"], serde_json::json!(false));
    // SQLite searches vectors and the derive embedded the concept, so the
    // vector leg ran too. The fixture contract has no measured null median,
    // so the two-leg value is absent rather than guessed, and the
    // keyword-only one does not stand in for it.
    assert!(
        top["vector_cosine"].as_f64().is_some(),
        "the vector leg ran: {scores}"
    );
    assert!(scores.get("confidence").is_none(), "{scores}");
    assert_eq!(scores.as_object().map(|m| m.len()), Some(1), "{scores}");
}

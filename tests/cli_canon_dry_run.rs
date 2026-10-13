//! #36 PR 5: `lambo canon dry-run` and `alias-preview` as a binary, on the
//! committed fair-test fixture in a SQLite file. Isolation: the process gets
//! a fresh `XDG_RUNTIME_DIR` and `TMPDIR` (where its default scratch copy
//! goes), and both are empty again when it exits; nothing appears beside the
//! input, whose hash is unchanged; no lambo.toml is read and nothing listens
//! on a port (the command builds no server).
#![cfg(all(feature = "store-sqlite", feature = "fixtures"))]

mod common;

use std::path::{Path, PathBuf};
use std::str::FromStr;

use common::{RuntimeDir, ScratchDir};
use lambo::store::{GraphStore, SqliteStore};
use lambo::types::GraphSnapshot;
use sha2::{Digest, Sha256};

fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn digest(path: &Path) -> String {
    Sha256::digest(std::fs::read(path).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The committed fixture in a SQLite file (rollback journal, so the file is
/// final once written).
fn fixture_db(dir: &Path) -> PathBuf {
    let path = dir.join("fair.db");
    let text = std::fs::read_to_string(lambo::fixtures::fixture_path("session-fair-test")).unwrap();
    let snap: GraphSnapshot = serde_json::from_str(&text).unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
        .unwrap()
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let store = SqliteStore::new(options);
        store.init_schema().await.unwrap();
        store.seed(&snap).await.unwrap();
    });
    path
}

fn run(args: &[&str], runtime: &Path, tmp: &Path) -> std::process::Output {
    common::lambo_command()
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("TMPDIR", tmp)
        .env("LAMBO_CONFIG", "/nonexistent/lambo.toml")
        .output()
        .expect("run lambo")
}

#[test]
fn the_dry_run_binary_reads_a_copy_and_leaves_no_trace() {
    let input = ScratchDir::new("lambo-canon-bin-input");
    let tmp = ScratchDir::new("lambo-canon-bin-tmp");
    let runtime = RuntimeDir::new();
    let db = fixture_db(&input);
    let (files, hash) = (listing(&input), digest(&db));
    let out = run(
        &[
            "canon",
            "dry-run",
            "--store",
            db.to_str().unwrap(),
            "--session",
            "session-fair-test",
            "--at",
            "2026-09-22T12:00:00Z",
            "--quiet",
            "5",
            "--json",
        ],
        runtime.path(),
        &tmp,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    for policy in report["policies"].as_array().unwrap() {
        assert_eq!(policy["canonical_total"], 4, "{}", policy["policy"]);
        assert_eq!(policy["stop"], "fixed_point");
    }
    assert_eq!(report["source"]["sha256_before"], hash.as_str());
    assert_eq!(report["source"]["sha256_after"], hash.as_str());
    assert_eq!(listing(&input), files);
    assert_eq!(digest(&db), hash);
    assert!(listing(&tmp).is_empty(), "{:?}", listing(&tmp));
    assert!(
        listing(runtime.path()).is_empty(),
        "{:?}",
        listing(runtime.path())
    );
}

#[test]
fn the_alias_preview_binary_flips_the_fixture_alias_and_refuses_bad_usage() {
    let input = ScratchDir::new("lambo-canon-bin-input");
    let tmp = ScratchDir::new("lambo-canon-bin-tmp");
    let runtime = RuntimeDir::new();
    let db = fixture_db(&input);
    let text = std::fs::read_to_string(lambo::fixtures::fixture_path("session-fair-test")).unwrap();
    let snap: GraphSnapshot = serde_json::from_str(&text).unwrap();
    let id_of = |content_prefix: &str| {
        snap.concepts
            .iter()
            .find(|c| c.content.starts_with(content_prefix))
            .unwrap()
            .id
    };
    let alias = id_of("writer lease");
    let ruling = id_of("a writer keeps its lease");
    let map = input.join("aliases.map");
    std::fs::write(&map, format!("{} {}\n", alias.0, ruling.0)).unwrap();
    let hash = digest(&db);
    let out = run(
        &[
            "canon",
            "alias-preview",
            "--store",
            db.to_str().unwrap(),
            "--session",
            "session-fair-test",
            "--at",
            "2026-09-22T12:00:00Z",
            "--quiet",
            "5",
            "--map",
            map.to_str().unwrap(),
        ],
        runtime.path(),
        &tmp,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(&format!("Alias {} Entity: Venerable -> None", alias.0)),
        "{stdout}"
    );
    assert!(
        !stdout.contains("writer lease"),
        "no concept text: {stdout}"
    );
    assert_eq!(digest(&db), hash);
    assert!(listing(&tmp).is_empty());

    // A usage error exits 2 and copies nothing.
    let out = run(
        &[
            "canon",
            "dry-run",
            "--store",
            db.to_str().unwrap(),
            "--session",
            "session-fair-test",
            "--policy",
            "everyone",
        ],
        runtime.path(),
        &tmp,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(listing(&tmp).is_empty());
}

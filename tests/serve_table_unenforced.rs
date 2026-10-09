//! #32 PR 1 review L3: a `[serve]` table is parsed and validated but nothing
//! enforces it until #32's later PRs. A `lambo serve` that reads one says so,
//! once, at startup, without quoting any value from the table; a serve
//! without one says nothing.
//!
//! #32 PR 5: `[[serve.credential]]` is enforced (over HTTP; a stdio serve
//! authenticates nobody), so it no longer raises the notice, and a stdio
//! serve does not read the credentials' variables: the one here is unset
//! and the serve still starts.
//!
//! Gated on `store-sqlite,embed-fixture` like the other serve integration
//! tests; run with `--features store-sqlite,embed-fixture`.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::process::Stdio;
use std::time::Duration;

use lambo::store::{GraphStore, SqliteStore};

mod common;
use common::{RuntimeDir, ScratchDir, ServeChild};

const NOTICE: &str = "[serve] is parsed but not yet enforced";
/// Values from the table that must never appear in the notice.
const PINNED: &str = "i32-pinned-marker";
const PROJECT_PATH: &str = "/i32-project-path-marker";
const CRED_NAME: &str = "i32-cred-marker";
const CRED_ENV: &str = "LAMBO_TEST_32_UNENFORCED_CRED";

/// Run `lambo serve --transport stdio` on `serve_table` until stdin closes and
/// return its stderr.
fn serve_stderr(serve_table: &str) -> String {
    let dir = ScratchDir::new("lambo-i32-unenforced");
    let db = dir.join("unenforced.sqlite");
    let cfg = dir.join("lambo.toml");
    std::fs::write(
        &cfg,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n\n{serve_table}",
            db.display()
        ),
    )
    .expect("write config");
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let store = SqliteStore::connect(db.to_str().unwrap()).expect("connect");
        store.init_schema().await.expect("init_schema");
    });

    let runtime = RuntimeDir::new();
    let mut child = ServeChild::new(
        common::lambo_command()
            .env(common::RUNTIME_DIR_VAR, runtime.path())
            .env_remove("RUST_LOG")
            // Unset: a stdio serve must not need an HTTP credential's token.
            .env_remove(CRED_ENV)
            .args([
                "--config",
                cfg.to_str().unwrap(),
                "serve",
                "--session",
                "i32-unenforced",
                "--transport",
                "stdio",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn serve"),
    );
    let out = child
        .wait_with_output_within(Duration::from_secs(60))
        .expect("serve exits when stdin closes")
        .expect("wait");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_serve_table_is_reported_as_not_yet_enforced_without_its_values() {
    let stderr = serve_stderr(&format!(
        "[serve]\nsessions = [\"{PINNED}\"]\n\n[[serve.projects]]\npath = \"{PROJECT_PATH}\"\n\
         session = \"{PINNED}\"\n\n[[serve.credential]]\nname = \"{CRED_NAME}\"\n\
         token_env = \"{CRED_ENV}\"\nsessions = [\"{PINNED}\"]\n"
    ));
    assert_eq!(
        stderr.matches(NOTICE).count(),
        1,
        "exactly one notice: {stderr}"
    );
    let line = stderr
        .lines()
        .find(|l| l.contains(NOTICE))
        .expect("notice line");
    assert!(line.contains("WARN"), "a warning: {line}");
    assert!(line.contains("[[serve.projects]]"), "names the key: {line}");
    assert!(
        !line.contains("[[serve.credential]]"),
        "credentials are enforced since #32 PR 5: {line}"
    );
    for value in [PINNED, PROJECT_PATH, CRED_NAME, CRED_ENV] {
        assert!(!line.contains(value), "{value} quoted: {line}");
    }
}

#[test]
fn no_serve_table_no_notice() {
    let stderr = serve_stderr("");
    assert!(!stderr.contains(NOTICE), "{stderr}");
}

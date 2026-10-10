//! The `[serve]` startup notice. #32 PR 1 review L3 added it while the table
//! was parsed but not enforced; #32's later PRs enforced every key on the
//! transport it applies to. What is left is one key an HTTP serve ignores by
//! design: `[[serve.projects]]`, the stdio project map. A `lambo serve` that
//! reads it over HTTP says so, once, at startup, without quoting any value
//! from the table; a serve without it says nothing. (The file name predates
//! that change.)
//!
//! #32 PR 5: `[[serve.credential]]` is enforced (over HTTP; a stdio serve
//! authenticates nobody), so it no longer raises the notice, and a stdio
//! serve does not read the credentials' variables: the one here is unset
//! and the serve still starts.
//!
//! #32 PR 6: the bounds (`attach_concurrency`, `idle_detach_secs`,
//! `per_session_rps`) are enforced over HTTP and do not apply to a stdio
//! serve, so neither names them.
//!
//! Gated on `store-sqlite,embed-fixture` like the other serve integration
//! tests; run with `--features store-sqlite,embed-fixture`.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lambo::store::{GraphStore, SqliteStore};

mod common;
use common::{RuntimeDir, ScratchDir, ServeChild};

const NOTICE: &str = "[serve] sets keys this serve ignores";
/// Values from the table that must never appear in the notice.
const PINNED: &str = "i32-pinned-marker";
const CRED_NAME: &str = "i32-cred-marker";
const CRED_ENV: &str = "LAMBO_TEST_32_UNENFORCED_CRED";

/// Run `lambo serve --transport stdio` on `serve_table` until stdin closes and
/// return its stderr.
fn serve_stderr(serve_table: &str) -> String {
    let (dir, cfg) = write_config(serve_table);
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
    drop(dir);
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Run `lambo serve --transport http --port 0` on `serve_table` (its
/// credential's variable set to a fake token) until it listens, SIGTERM it,
/// and return its stderr.
fn http_serve_stderr(serve_table: &str) -> String {
    let (dir, cfg) = write_config(serve_table);
    let runtime = RuntimeDir::new();
    // Built at runtime, so no token-shaped literal sits in the source.
    let fake = ["fake", "unenforced", "wire", "value"].join("-");
    let mut child = ServeChild::new(
        common::lambo_command()
            .env(common::RUNTIME_DIR_VAR, runtime.path())
            .env_remove("RUST_LOG")
            .env(CRED_ENV, fake)
            .args([
                "--config",
                cfg.to_str().unwrap(),
                "serve",
                "--session",
                PINNED,
                "--transport",
                "http",
                "--port",
                "0",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn serve"),
    );
    let stderr = child.stderr.take().expect("stderr");
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut seen: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !seen
        .iter()
        .any(|l| l.contains("mcp http: listening on /mcp"))
    {
        let left = deadline.saturating_duration_since(Instant::now());
        match lines.recv_timeout(left) {
            Ok(line) => seen.push(line),
            Err(e) => panic!("no listening line ({e}):\n{}", seen.join("\n")),
        }
    }
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status();
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().expect("try_wait").is_none() {
        assert!(Instant::now() < deadline, "serve did not exit on SIGTERM");
        std::thread::sleep(Duration::from_millis(50));
    }
    while let Ok(line) = lines.recv_timeout(Duration::from_millis(300)) {
        seen.push(line);
    }
    drop(dir);
    seen.join("\n")
}

/// A scratch directory holding a provisioned SQLite store and a
/// `lambo.toml` over it with `serve_table` appended.
fn write_config(serve_table: &str) -> (ScratchDir, std::path::PathBuf) {
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
    (dir, cfg)
}

/// Over HTTP the cwd map is the one key nothing reads, so it is named, once,
/// and nothing from the table is quoted; the enforced bounds and the
/// credential are not named.
#[test]
fn an_http_serve_reports_the_ignored_cwd_map_without_its_values() {
    let stderr = http_serve_stderr(&format!(
        "[serve]\nsessions = [\"{PINNED}\"]\nper_session_rps = 5\nattach_concurrency = 1\n\
         idle_detach_secs = 60\n\n\
         [[serve.projects]]\npath = \"/\"\nsession = \"{PINNED}\"\n\n\
         [[serve.credential]]\nname = \"{CRED_NAME}\"\n\
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
    for enforced in [
        "per_session_rps",
        "attach_concurrency",
        "idle_detach_secs",
        "[[serve.credential]]",
    ] {
        assert!(
            !line.contains(enforced),
            "{enforced} is enforced (#32 PRs 5 and 6): {line}"
        );
    }
    for value in [PINNED, CRED_NAME, CRED_ENV] {
        assert!(!line.contains(value), "{value} quoted: {line}");
    }
}

/// #32 PR 6: the bounds do not apply to a one-session stdio serve, so a
/// stdio serve whose table sets them says nothing.
#[test]
fn a_stdio_serve_does_not_report_the_bounds() {
    let stderr = serve_stderr(
        "[serve]\nper_session_rps = 5\nattach_concurrency = 1\nidle_detach_secs = 60\n",
    );
    assert!(!stderr.contains(NOTICE), "{stderr}");
}

#[test]
fn no_serve_table_no_notice() {
    let stderr = serve_stderr("");
    assert!(!stderr.contains(NOTICE), "{stderr}");
}

/// #32 PR 8: `default_session` and `[[serve.projects]]` choose a stdio
/// serve's session, so a stdio serve whose table sets only those says
/// nothing, even when `--session` (which wins over them) is given.
#[test]
fn a_stdio_serve_does_not_report_its_selection_keys() {
    let stderr = serve_stderr(
        "[serve]\ndefault_session = \"i32-default\"\n\n\
         [[serve.projects]]\npath = \"/\"\nsession = \"i32-root\"\n",
    );
    assert!(!stderr.contains(NOTICE), "{stderr}");
}

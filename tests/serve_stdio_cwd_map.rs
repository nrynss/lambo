//! #32 PR 8: a stdio `lambo serve` without `--session` takes its session from
//! the `[[serve.projects]]` entry covering its working directory, then from
//! `[serve] default_session`, and refuses with the missing-`--session` text
//! when neither applies. `--session` always wins. The session a serve really
//! took is read back from its ledger's `startup` line, not from a log line.
//!
//! Gated on `store-sqlite,embed-fixture` like the other serve integration
//! tests; run with `--features store-sqlite,embed-fixture`.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use lambo::store::{GraphStore, SqliteStore};

mod common;
use common::{RuntimeDir, ScratchDir, ServeChild};

struct Fixture {
    dir: ScratchDir,
    cfg: PathBuf,
    ledger: PathBuf,
}

/// A sqlite store plus a `lambo.toml` whose `[serve]` table is `serve_table`,
/// with `{root}` replaced by the scratch directory.
fn fixture(prefix: &str, serve_table: &str) -> Fixture {
    let dir = ScratchDir::new(prefix);
    let db = dir.join("cwdmap.sqlite");
    let cfg = dir.join("lambo.toml");
    let ledger = dir.join("ledger.jsonl");
    let table = serve_table.replace("{root}", dir.to_str().unwrap());
    std::fs::write(
        &cfg,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n\n{table}",
            db.display()
        ),
    )
    .expect("write config");
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let store = SqliteStore::connect(db.to_str().unwrap()).expect("connect");
        store.init_schema().await.expect("init_schema");
    });
    Fixture { dir, cfg, ledger }
}

fn mkdir(root: &Path, rel: &str) -> PathBuf {
    let p = root.join(rel);
    std::fs::create_dir_all(&p).expect("mkdir");
    p
}

/// Run `lambo serve` in `cwd` with `extra` args until stdin closes.
fn run_serve(fx: &Fixture, cwd: &Path, extra: &[&str]) -> Output {
    run_serve_with(fx, cwd, extra, |_| {})
}

/// [`run_serve`] with a hook to adjust the command (its environment).
fn run_serve_with(
    fx: &Fixture,
    cwd: &Path,
    extra: &[&str],
    adjust: impl FnOnce(&mut std::process::Command),
) -> Output {
    let runtime = RuntimeDir::new();
    let mut cmd = common::lambo_command();
    adjust(&mut cmd);
    cmd.env(common::RUNTIME_DIR_VAR, runtime.path())
        .current_dir(cwd)
        .arg("--config")
        .arg(&fx.cfg)
        .arg("serve")
        .arg("--ledger")
        .arg(&fx.ledger)
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = ServeChild::new(cmd.spawn().expect("spawn serve"));
    child
        .wait_with_output_within(Duration::from_secs(60))
        .expect("serve exits when stdin closes")
        .expect("wait")
}

/// The `session` of the ledger's `startup` line: what the serve really took.
fn started_session(fx: &Fixture) -> String {
    let text = std::fs::read_to_string(&fx.ledger).expect("ledger written");
    let startup = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["kind"] == "startup")
        .unwrap_or_else(|| panic!("no startup line in {text}"));
    startup["session"].as_str().expect("session").to_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

const MAP: &str = "[serve]\ndefault_session = \"i32h-default\"\n\n\
    [[serve.projects]]\npath = \"{root}/work\"\nsession = \"i32h-outer\"\n\n\
    [[serve.projects]]\npath = \"{root}/work/lambo\"\nsession = \"i32h-mapped\"\n";

#[test]
fn a_stdio_serve_in_a_mapped_directory_takes_the_longest_entry() {
    let fx = fixture("lambo-i32h-mapped", MAP);
    let cwd = mkdir(&fx.dir, "work/lambo/src");
    let out = run_serve(&fx, &cwd, &["--transport", "stdio"]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert_eq!(started_session(&fx), "i32h-mapped");
    assert!(
        err.contains("[[serve.projects]] entry covering the working directory"),
        "selection logged: {err}"
    );
    assert!(
        !err.contains("not yet enforced"),
        "a selection-only [serve] table is enforced, so no notice: {err}"
    );
}

#[test]
fn an_unmapped_directory_falls_back_to_default_session() {
    let fx = fixture("lambo-i32h-default", MAP);
    let cwd = mkdir(&fx.dir, "elsewhere");
    let out = run_serve(&fx, &cwd, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(started_session(&fx), "i32h-default");
}

#[test]
fn the_session_flag_wins_over_the_map() {
    let fx = fixture("lambo-i32h-flag", MAP);
    let cwd = mkdir(&fx.dir, "work/lambo");
    let out = run_serve(&fx, &cwd, &["--session", "i32h-flag"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(started_session(&fx), "i32h-flag");
}

#[test]
fn no_session_from_anywhere_is_the_missing_flag_usage_error() {
    let fx = fixture(
        "lambo-i32h-none",
        "[[serve.projects]]\npath = \"{root}/work\"\nsession = \"i32h-outer\"\n",
    );
    let cwd = mkdir(&fx.dir, "elsewhere");
    let out = run_serve(&fx, &cwd, &[]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains("the following required arguments were not provided")
            && err.contains("--session <SESSION>"),
        "{err}"
    );
    assert!(
        !err.contains(cwd.to_str().unwrap()),
        "the cwd is not quoted: {err}"
    );
    assert!(!fx.ledger.exists(), "refused before the serve started");
}

/// The cwd map is a stdio serve's only. An HTTP serve pins sessions (#32
/// PR 4): from `--session` and `[serve] sessions`, never from
/// `[[serve.projects]]`, and `default_session` only picks `/mcp` among the
/// pinned. MAP pins nothing, so even in a mapped directory an HTTP serve
/// without `--session` gets PR 4's refusal (exit 2, before anything starts),
/// not the stdio hint and not the mapped session.
#[test]
fn an_http_serve_does_not_take_its_session_from_the_cwd_map() {
    let fx = fixture("lambo-i32h-http", MAP);
    let cwd = mkdir(&fx.dir, "work/lambo");
    let out = run_serve(&fx, &cwd, &["--transport", "http", "--port", "0"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains("--session <SESSION> is required, or name the sessions to serve in lambo.toml [serve] sessions"),
        "PR 4's pinning refusal: {err}"
    );
    assert!(
        !err.contains("without --session, a stdio serve uses"),
        "not the stdio hint: {err}"
    );
    assert!(!fx.ledger.exists(), "refused before the serve started");
}

#[test]
fn without_home_the_tilde_entries_are_skipped_with_one_warning() {
    // #32 PR 8 review L2: the rest of the map still applies; the skip is
    // logged once, quoting neither an entry nor the cwd.
    let fx = fixture(
        "lambo-i32h-nohome",
        "[serve]\ndefault_session = \"i32h-default\"\n\n\
         [[serve.projects]]\npath = \"~\"\nsession = \"i32h-home\"\n\n\
         [[serve.projects]]\npath = \"~/elsewhere\"\nsession = \"i32h-home2\"\n\n\
         [[serve.projects]]\npath = \"{root}/work\"\nsession = \"i32h-outer\"\n",
    );
    let cwd = mkdir(&fx.dir, "work/cwd-marker");
    let out = run_serve_with(&fx, &cwd, &[], |cmd| {
        cmd.env_remove("HOME");
    });
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert_eq!(started_session(&fx), "i32h-outer");
    assert_eq!(
        err.matches("entries starting with `~` were skipped")
            .count(),
        1,
        "one warning: {err}"
    );
    assert!(!err.contains("cwd-marker"), "the cwd is not quoted: {err}");
}

#[test]
fn a_map_that_cannot_be_applied_is_a_config_error_exiting_1() {
    // An ambiguous map (two entries for one directory, different sessions)
    // is a configuration error, exit 1, not the usage error's 2, and it
    // still refuses before the serve starts.
    let fx = fixture(
        "lambo-i32h-ambiguous",
        "[[serve.projects]]\npath = \"{root}/work\"\nsession = \"i32h-one\"\n\n\
         [[serve.projects]]\npath = \"{root}/alias\"\nsession = \"i32h-two\"\n",
    );
    let work = mkdir(&fx.dir, "work");
    std::os::unix::fs::symlink(&work, fx.dir.join("alias")).expect("symlink");
    let cwd = mkdir(&fx.dir, "work/cwd-marker");
    let out = run_serve(&fx, &cwd, &[]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("same directory") && err.contains("\"i32h-one\""),
        "{err}"
    );
    assert!(!err.contains("cwd-marker"), "the cwd is not quoted: {err}");
    assert!(!fx.ledger.exists(), "refused before the serve started");
}

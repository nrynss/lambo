//! #316: `lambo serve` and `lambo serve-web` refuse a non-loopback bind
//! with no credential, and `lambo serve` refuses a misused
//! `--ledger-heartbeat`, before any backend is built, with the usage exit
//! code 2 that every other preflight refusal uses.
//!
//! The config names a SQLite store in a directory that does not exist, so a
//! command line that gets past the checks has its backends built and fails
//! on the store's first use, exit 1, `unable to open database file`. A
//! refusal that never gets there was made before anything was built (a
//! store build is lazy and opens nothing, so the refusal is also the first
//! thing the command does with the store). The models
//! are `formal/LamboProofs/Resolve/Serve.lean`'s
//! `exposed_bind_refused_before_build` and
//! `heartbeat_without_ledger_refused_before_build`.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::process::{Output, Stdio};

mod common;
use common::{RuntimeDir, ScratchDir, RUNTIME_DIR_VAR};

/// A `lambo.toml` whose store cannot be opened: its first use fails.
fn unbuildable(dir: &ScratchDir) -> std::path::PathBuf {
    let cfg = dir.join("lambo.toml");
    let missing = dir.join("no-such-dir").join("x.sqlite");
    std::fs::write(
        &cfg,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n",
            missing.display()
        ),
    )
    .expect("write config");
    cfg
}

/// Run `lambo --config cfg <args>`, with `LAMBO_AUTH_TOKEN` set to `token`
/// when given (every other `LAMBO_` variable is cleared).
fn run<S: AsRef<std::ffi::OsStr>>(
    cfg: &std::path::Path,
    runtime: &RuntimeDir,
    args: &[S],
    token: Option<&str>,
) -> Output {
    let mut cmd = common::lambo_command();
    cmd.env(RUNTIME_DIR_VAR, runtime.path())
        .args(["--config", cfg.to_str().unwrap()])
        .args(args)
        .stdin(Stdio::null());
    if let Some(token) = token {
        cmd.env("LAMBO_AUTH_TOKEN", token);
    }
    cmd.output().expect("run lambo")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Refused before the backends: exit 2, `needle` in the message, and no
/// backend build was attempted.
fn refused_before_build(out: &Output, needle: &str) {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains(needle), "{err}");
    assert!(!err.contains(UNOPENABLE), "{err}");
    // Nor was the embedder built: building it logs that the fixture
    // contract has no calibration row.
    assert!(!err.contains("no calibration row"), "{err}");
}

/// What the store says on its first use.
const UNOPENABLE: &str = "unable to open database file";

/// Got past every check to the backends, whose store fails: exit 1.
fn reached_the_backends(out: &Output) {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains(UNOPENABLE), "{err}");
}

fn token() -> String {
    ["fake", "i316", "bind", "value"].join("-")
}

#[test]
fn an_exposed_bind_without_a_credential_is_refused_before_any_backend() {
    let dir = ScratchDir::new("lambo-i316-serve");
    let cfg = unbuildable(&dir);
    let runtime = RuntimeDir::new();
    let serve = |bind: &'static str| {
        [
            "serve",
            "--transport",
            "http",
            "--port",
            "0",
            "--bind",
            bind,
            "--session",
            "i316",
        ]
    };

    let exposed = serve("0.0.0.0");
    refused_before_build(&run(&cfg, &runtime, &exposed, None), "refusing to start");
    // The controls: a credential, or a loopback bind, gets to the backends.
    reached_the_backends(&run(&cfg, &runtime, &exposed, Some(&token())));
    reached_the_backends(&run(&cfg, &runtime, &serve("127.0.0.1"), None));
}

#[test]
fn an_exposed_serve_web_bind_is_refused_before_any_backend() {
    let dir = ScratchDir::new("lambo-i316-web");
    let cfg = unbuildable(&dir);
    let runtime = RuntimeDir::new();
    let web = |bind: &'static str| {
        [
            "serve-web",
            "--port",
            "0",
            "--bind",
            bind,
            "--session",
            "i316",
        ]
    };

    let exposed = web("0.0.0.0");
    refused_before_build(&run(&cfg, &runtime, &exposed, None), "refusing to start");
    reached_the_backends(&run(&cfg, &runtime, &exposed, Some(&token())));
    reached_the_backends(&run(&cfg, &runtime, &web("127.0.0.1"), None));
}

#[test]
fn ledger_heartbeat_misuse_is_refused_before_any_backend() {
    let dir = ScratchDir::new("lambo-i316-ledger");
    let cfg = unbuildable(&dir);
    let runtime = RuntimeDir::new();
    let ledger = dir.join("calls.jsonl");
    let ledger = ledger.to_str().unwrap();
    let with = |extra: &[&'static str]| -> Vec<String> {
        ["serve", "--transport", "stdio", "--session", "i316"]
            .iter()
            .chain(extra)
            .map(|a| a.to_string())
            .collect()
    };

    let with_ledger = |every: &'static str| {
        let mut args = with(&["--ledger-heartbeat", every]);
        args.extend(["--ledger".to_string(), ledger.to_string()]);
        args
    };

    let without = with(&["--ledger-heartbeat", "30"]);
    refused_before_build(&run(&cfg, &runtime, &without, None), "without --ledger");
    let zero = with_ledger("0");
    refused_before_build(&run(&cfg, &runtime, &zero, None), "at least 1 second");
    // The control: both flags, a positive interval, gets to the backends.
    let both = with_ledger("30");
    reached_the_backends(&run(&cfg, &runtime, &both, None));
}

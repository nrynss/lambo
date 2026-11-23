//! #316: `lambo serve` and `lambo serve-web` refuse a non-loopback bind
//! with no credential, and `lambo serve` refuses a misused
//! `--ledger-heartbeat`, before any backend is built, with the usage exit
//! code 2 that every other preflight refusal uses.
//!
//! The config pins `[store] vector_dim` to a width the embedder does not
//! emit. The file loads, so every check before the backends passes, and
//! `resolve_backends` refuses it after building the store and the embedder:
//! exit 1, `failed to build backends: store.vector_dim is pinned ...`. A
//! command line that never prints that line was refused before
//! `resolve_backends` ran. Before #316 all three refusals came after it
//! (`serve-web`'s in `serve_web::run`, `serve`'s in `lambo::mcp::serve`), so
//! each test here fails on that code. The models are
//! `formal/LamboProofs/Resolve/Serve.lean`'s
//! `exposed_bind_refused_before_build` and
//! `heartbeat_without_ledger_refused_before_build`.
#![cfg(all(feature = "store-memory", feature = "embed-fixture", unix))]

use std::process::{Output, Stdio};

mod common;
use common::{RuntimeDir, ScratchDir, RUNTIME_DIR_VAR};

/// What the backend resolve says about the config below.
const RESOLVE_REFUSED: &str = "store.vector_dim is pinned to 768";

/// A `lambo.toml` that loads but that `resolve_backends` refuses, after
/// building the store and the embedder.
fn unbuildable(dir: &ScratchDir) -> std::path::PathBuf {
    let cfg = dir.join("lambo.toml");
    std::fs::write(
        &cfg,
        "[store]\nkind = \"memory\"\nvector_dim = 768\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n",
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

/// Refused before the backends: exit 2, `needle` in the message, and
/// `resolve_backends` never ran.
fn refused_before_build(out: &Output, needle: &str) {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains(needle), "{err}");
    assert!(!err.contains("failed to build backends"), "{err}");
    assert!(!err.contains(RESOLVE_REFUSED), "{err}");
}

/// Got past every check to `resolve_backends`, which refuses: exit 1.
fn reached_the_backends(out: &Output) {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("failed to build backends"), "{err}");
    assert!(err.contains(RESOLVE_REFUSED), "{err}");
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

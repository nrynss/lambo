//! #32 PR 5: `[[serve.credential]]` and the legacy token, across the
//! process boundary.
//!
//! * a two-session HTTP hub with two credentials, each scoped to one
//!   session: each token reaches its own session; the other session is the
//!   unrouted 404, byte for byte; no token is the 401;
//! * the dogfood rig's shape, `--session X` with `LAMBO_AUTH_TOKEN`, is the
//!   legacy `default` credential and behaves as before: `/mcp` with the
//!   token reaches X, without it is the 401;
//! * an unset credential variable, and a configured token equal to the
//!   legacy one, refuse the start with exit 2 before any backend is built,
//!   naming the credential and never a token.
//!
//! Tokens are built at runtime so no token-shaped literal sits in the
//! source. Every spawned serve gets `XDG_RUNTIME_DIR` pointed at the test's
//! own [`RuntimeDir`], and the HTTP ports are ephemeral, never 7700. Gated
//! on `store-sqlite,embed-fixture` like the other serve integration tests.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lambo::store::{GraphStore, SqliteStore};

mod common;
use common::{RuntimeDir, ScratchDir, ServeChild, RUNTIME_DIR_VAR};

const A: &str = "i32-cred-a";
const B: &str = "i32-cred-b";
const ENV_A: &str = "LAMBO_TEST_32E_CRED_A";
const ENV_B: &str = "LAMBO_TEST_32E_CRED_B";
const LEGACY_ENV: &str = "LAMBO_AUTH_TOKEN";

fn token(label: &str) -> String {
    ["fake", label, "i32e", "value"].join("-")
}

/// A scratch SQLite store and a `lambo.toml` over it ending in `extra`.
fn scratch(extra: &str) -> (ScratchDir, std::path::PathBuf) {
    let dir = ScratchDir::new("lambo-i32-cred");
    let db = dir.join("cred.sqlite");
    let cfg = dir.join("lambo.toml");
    std::fs::write(
        &cfg,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n\n{extra}",
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

/// Two credentials, each scoped to one of the two sessions.
fn two_credentials() -> String {
    format!(
        "[[serve.credential]]\nname = \"cred-a\"\ntoken_env = \"{ENV_A}\"\nsessions = [\"{A}\"]\n\n\
         [[serve.credential]]\nname = \"cred-b\"\ntoken_env = \"{ENV_B}\"\nsessions = [\"{B}\"]\n"
    )
}

/// `lambo serve --transport http --port 0` on `cfg` with `args` and `envs`
/// (every credential variable unset unless given).
fn serve_command(
    cfg: &std::path::Path,
    runtime: &RuntimeDir,
    args: &[&str],
    envs: &[(&str, String)],
) -> Command {
    let mut cmd = common::lambo_command();
    cmd.env(RUNTIME_DIR_VAR, runtime.path())
        .env_remove("RUST_LOG")
        .env_remove(ENV_A)
        .env_remove(ENV_B)
        .env_remove(LEGACY_ENV)
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "serve",
            "--transport",
            "http",
        ])
        .args(["--port", "0", "--agent", "agent-i32e"])
        .args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd
}

/// A spawned HTTP hub, its stderr collected line by line.
struct Hub {
    child: ServeChild,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
    addr: SocketAddr,
}

impl Hub {
    fn spawn(mut cmd: Command) -> Self {
        let mut child = ServeChild::new(
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn hub"),
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
        let mut hub = Self {
            child,
            lines,
            seen: Vec::new(),
            addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        };
        hub.listening();
        hub
    }

    /// Wait for the listening line and take the bound address from it.
    fn listening(&mut self) {
        const LISTENING: &str = "mcp http: listening on /mcp";
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.seen.iter().any(|l| l.contains(LISTENING)) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => self.seen.push(line),
                Err(e) => panic!("no listening line ({e}):\n{}", self.seen.join("\n")),
            }
        }
        let line = self.seen.iter().find(|l| l.contains(LISTENING)).unwrap();
        let at = line.find("127.0.0.1:").expect("a loopback address");
        let port: String = line[at + "127.0.0.1:".len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        self.addr = SocketAddr::from(([127, 0, 0, 1], port.parse().expect("a port")));
    }

    /// SIGTERM, then wait for the exit; returns every stderr line seen.
    fn stop(mut self) -> Vec<String> {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status();
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.child.try_wait().expect("try_wait").is_none() {
            assert!(Instant::now() < deadline, "hub did not exit on SIGTERM");
            std::thread::sleep(Duration::from_millis(50));
        }
        while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(300)) {
            self.seen.push(line);
        }
        self.seen
    }
}

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"i32e","version":"1"}}}"#;

/// One raw HTTP/1.1 exchange, minus the `date` header.
fn exchange(addr: SocketAddr, path: &str, auth: Option<&str>, body: &str) -> String {
    let mut sock = TcpStream::connect(addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(20))).ok();
    let auth = auth
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json, text/event-stream\r\n\
         Content-Type: application/json\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).expect("write");
    sock.write_all(body.as_bytes()).expect("write body");
    let mut raw = Vec::new();
    let _ = sock.read_to_end(&mut raw);
    String::from_utf8_lossy(&raw)
        .split("\r\n")
        .filter(|l| !l.to_ascii_lowercase().starts_with("date:"))
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// `initialize` at `path` with `auth` reaches `session`.
fn reaches(addr: SocketAddr, path: &str, auth: Option<&str>, session: &str) {
    let reply = exchange(addr, path, auth, INITIALIZE);
    assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{path}: {reply}");
    assert!(
        reply.contains(&format!("session '{session}'")),
        "{path} must reach {session}: {reply}"
    );
}

#[test]
fn each_credential_reaches_only_its_own_session() {
    let (_dir, cfg) = scratch(&two_credentials());
    let runtime = RuntimeDir::new();
    let hub = Hub::spawn(serve_command(
        &cfg,
        &runtime,
        &["--session", A, "--session", B],
        &[(ENV_A, token("a")), (ENV_B, token("b"))],
    ));
    let addr = hub.addr;
    let (ta, tb) = (token("a"), token("b"));

    reaches(addr, &format!("/mcp/s/{A}"), Some(&ta), A);
    reaches(addr, &format!("/mcp/s/{B}"), Some(&tb), B);
    // `/mcp` is the default session (A), authorized like its own route.
    reaches(addr, "/mcp", Some(&ta), A);

    let unrouted_a = exchange(addr, "/not/routed", Some(&ta), "");
    assert!(
        unrouted_a.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "{unrouted_a}"
    );
    for path in [format!("/mcp/s/{B}"), "/mcp/s/i32-cred-unknown".into()] {
        assert_eq!(
            exchange(addr, &path, Some(&ta), ""),
            unrouted_a,
            "cred-a {path}"
        );
    }
    let unrouted_b = exchange(addr, "/not/routed", Some(&tb), "");
    for path in [format!("/mcp/s/{A}"), "/mcp".into()] {
        assert_eq!(
            exchange(addr, &path, Some(&tb), ""),
            unrouted_b,
            "cred-b {path}"
        );
    }

    // A credential is configured, so loopback no longer serves without one.
    for auth in [None, Some(token("nobody"))] {
        let reply = exchange(addr, &format!("/mcp/s/{A}"), auth.as_deref(), INITIALIZE);
        assert!(
            reply.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "{reply}"
        );
    }

    let lines = hub.stop();
    let all = lines.join("\n");
    for secret in [&ta, &tb] {
        assert!(!all.contains(secret.as_str()), "a token reached the log");
    }
    assert!(
        !all.contains("parsed but not yet enforced"),
        "credentials are enforced: {all}"
    );
}

/// The dogfood rig's shape: one `--session` and `LAMBO_AUTH_TOKEN`, no
/// `[serve]` table. The token is the legacy `default` credential; `/mcp`
/// and the session's own route serve with it and refuse without it.
#[test]
fn the_legacy_token_alone_behaves_as_before() {
    let (_dir, cfg) = scratch("");
    let runtime = RuntimeDir::new();
    let legacy = token("legacy");
    let hub = Hub::spawn(serve_command(
        &cfg,
        &runtime,
        &["--session", A],
        &[(LEGACY_ENV, legacy.clone())],
    ));
    let addr = hub.addr;
    reaches(addr, "/mcp", Some(&legacy), A);
    reaches(addr, &format!("/mcp/s/{A}"), Some(&legacy), A);
    for auth in [None, Some(token("wrong"))] {
        let reply = exchange(addr, "/mcp", auth.as_deref(), INITIALIZE);
        assert!(
            reply.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "{reply}"
        );
    }
    hub.stop();
}

/// Run a serve that must refuse to start; returns (exit code, stderr).
fn refused(cfg: &std::path::Path, envs: &[(&str, String)]) -> (Option<i32>, String) {
    let runtime = RuntimeDir::new();
    let out = ServeChild::new(
        serve_command(cfg, &runtime, &["--session", A, "--session", B], envs)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn serve"),
    )
    .wait_with_output_within(Duration::from_secs(60))
    .expect("the refusal is prompt")
    .expect("wait");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn an_unset_credential_variable_refuses_the_start_before_the_backends() {
    let (_dir, cfg) = scratch(&two_credentials());
    let (code, stderr) = refused(&cfg, &[(ENV_A, token("a"))]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(
        stderr.contains("\"cred-b\"") && stderr.contains(ENV_B) && stderr.contains("not set"),
        "{stderr}"
    );
    assert!(!stderr.contains("failed to build backends"), "{stderr}");
    assert!(!stderr.contains(&token("a")), "a token reached stderr");
}

#[test]
fn a_credential_sharing_the_legacy_token_refuses_the_start_without_quoting_it() {
    let (_dir, cfg) = scratch(&two_credentials());
    let shared = token("shared");
    let (code, stderr) = refused(
        &cfg,
        &[
            (ENV_A, shared.clone()),
            (ENV_B, token("b")),
            (LEGACY_ENV, shared.clone()),
        ],
    );
    assert_eq!(code, Some(2), "{stderr}");
    assert!(
        stderr.contains("\"cred-a\"") && stderr.contains("LAMBO_AUTH_TOKEN"),
        "{stderr}"
    );
    assert!(!stderr.contains(&shared), "the token reached stderr");
    assert!(!stderr.contains("failed to build backends"), "{stderr}");
}

/// #32 PR 5 review L3: a token no request could present (here a trailing
/// space, as an env file leaves one) refuses the start with exit 2 before
/// the backends, naming where it came from and never quoting it, whether it
/// is a configured credential's, `LAMBO_AUTH_TOKEN` or `--auth-token`.
#[test]
fn a_token_no_request_could_present_refuses_the_start_without_quoting_it() {
    let (_dir, cfg) = scratch(&two_credentials());
    let padded = format!("{} ", token("padded"));
    let core = token("padded");

    let (code, stderr) = refused(&cfg, &[(ENV_A, padded.clone()), (ENV_B, token("b"))]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(
        stderr.contains("\"cred-a\"") && stderr.contains(ENV_A) && stderr.contains("whitespace"),
        "{stderr}"
    );
    assert!(!stderr.contains(&core), "the token reached stderr");
    assert!(!stderr.contains("failed to build backends"), "{stderr}");

    let (code, stderr) = refused(
        &cfg,
        &[
            (ENV_A, token("a")),
            (ENV_B, token("b")),
            (LEGACY_ENV, padded.clone()),
        ],
    );
    assert_eq!(code, Some(2), "{stderr}");
    assert!(
        stderr.contains(LEGACY_ENV) && stderr.contains("whitespace"),
        "{stderr}"
    );
    assert!(!stderr.contains(&core), "the token reached stderr");

    let runtime = RuntimeDir::new();
    let out = ServeChild::new(
        serve_command(
            &cfg,
            &runtime,
            &["--session", A, "--auth-token", &padded],
            &[],
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve"),
    )
    .wait_with_output_within(Duration::from_secs(60))
    .expect("the refusal is prompt")
    .expect("wait");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("--auth-token") && stderr.contains("whitespace"),
        "{stderr}"
    );
    assert!(!stderr.contains(&core), "the token reached stderr");
}

//! #23: `lambo erase-session` end to end on a SQLite file, through the binary.
//!
//! Gated on `store-sqlite` (and `embed-fixture`, which is in the default set).
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture"))]

use std::process::{Command, Output};

mod common;
use common::ScratchDir;

fn scratch() -> (ScratchDir, String) {
    let dir = ScratchDir::new("lambo-cli-erase");
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
    let cfg = cfg.to_str().expect("utf-8 path").to_string();
    (dir, cfg)
}

fn lambo(cfg: &str, args: &[&str]) -> Output {
    let mut cmd: Command = common::lambo_command();
    cmd.args(["--config", cfg]).args(args);
    cmd.output().expect("run lambo")
}

fn ok(out: &Output, what: &str) -> String {
    assert!(
        out.status.success(),
        "{what} must succeed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn derive(cfg: &str, session: &str, content: &str) -> Output {
    lambo(
        cfg,
        &[
            "derive",
            "--session",
            session,
            "--agent",
            "agent-a",
            "--content",
            content,
            "--kind",
            "entity",
        ],
    )
}

fn erase(cfg: &str, session: &str, confirm: &str) -> Output {
    lambo(
        cfg,
        &["erase-session", "--session", session, "--confirm", confirm],
    )
}

#[test]
fn erase_session_removes_the_session_refuses_rewrites_and_is_safe_to_repeat() {
    let (_dir, cfg) = scratch();
    ok(&lambo(&cfg, &["provision"]), "provision");
    ok(&derive(&cfg, "user-42", "prefers linen shirts"), "derive");
    ok(
        &derive(&cfg, "user-7", "prefers wool coats"),
        "derive other",
    );

    // A confirm that does not repeat the id is a usage error and erases nothing.
    let typo = erase(&cfg, "user-42", "user-24");
    assert_eq!(typo.status.code(), Some(2), "a typo is a usage error");
    let recall = ok(
        &lambo(
            &cfg,
            &["recall", "--session", "user-42", "--query", "linen shirts"],
        ),
        "recall before",
    );
    assert!(recall.contains("prefers linen shirts"), "{recall}");

    let report: serde_json::Value =
        serde_json::from_str(ok(&erase(&cfg, "user-42", "user-42"), "erase").trim())
            .expect("one JSON report");
    assert_eq!(report["session"], "user-42");
    assert_eq!(report["already_absent"], false);
    assert_eq!(report["removed"]["sessions"], 1);
    assert!(
        report["removed"]["concepts"].as_u64().unwrap() >= 1,
        "{report}"
    );
    assert!(
        report["removed"]["interactions"].as_u64().unwrap() >= 1,
        "{report}"
    );

    // Recall, inspect and stats find nothing of it.
    let recall = ok(
        &lambo(
            &cfg,
            &["recall", "--session", "user-42", "--query", "linen shirts"],
        ),
        "recall after",
    );
    assert!(!recall.contains("linen"), "{recall}");
    let stats = ok(
        &lambo(&cfg, &["stats", "--session", "user-42"]),
        "stats after",
    );
    assert!(stats.contains("nodes=0 edges=0 concepts=0"), "{stats}");
    let inspect = lambo(
        &cfg,
        &[
            "inspect",
            "--session",
            "user-42",
            "--focus",
            "prefers linen shirts",
        ],
    );
    assert!(
        !String::from_utf8_lossy(&inspect.stdout).contains("linen"),
        "inspect finds nothing"
    );

    // A later write to the erased id is refused and recreates nothing.
    let rewrite = derive(&cfg, "user-42", "prefers silk");
    assert_eq!(rewrite.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&rewrite.stderr);
    assert!(stderr.contains("was erased"), "{stderr}");
    let stats = ok(
        &lambo(&cfg, &["stats", "--session", "user-42"]),
        "stats after refused write",
    );
    assert!(stats.contains("nodes=0 edges=0 concepts=0"), "{stats}");

    // A repeat succeeds and says there was nothing left.
    let again: serde_json::Value =
        serde_json::from_str(ok(&erase(&cfg, "user-42", "user-42"), "repeat").trim())
            .expect("one JSON report");
    assert_eq!(again["already_absent"], true);
    assert_eq!(again["fence_token"], report["fence_token"]);

    // The other session is untouched.
    let other = ok(
        &lambo(
            &cfg,
            &["recall", "--session", "user-7", "--query", "wool coats"],
        ),
        "recall other",
    );
    assert!(other.contains("prefers wool coats"), "{other}");
}

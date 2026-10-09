//! #32 PR 8: the stdio session resolver. Every test builds its own directory
//! tree under a scratch dir; nothing reads the process's real cwd or `$HOME`.

use std::path::{Path, PathBuf};

use super::*;
use crate::config::ProjectConfig;
use crate::test_util::ScratchDir;
use crate::LamboFile;

fn project(path: impl Into<String>, session: &str) -> ProjectConfig {
    ProjectConfig {
        path: path.into(),
        session: session.to_owned(),
    }
}

fn config(projects: Vec<ProjectConfig>, default: Option<&str>) -> ServeConfig {
    ServeConfig {
        projects,
        default_session: default.map(str::to_owned),
        ..ServeConfig::default()
    }
}

fn mkdir(root: &Path, rel: &str) -> PathBuf {
    let p = root.join(rel);
    std::fs::create_dir_all(&p).expect("mkdir");
    p
}

fn s(p: &Path) -> String {
    p.to_str().expect("utf-8 path").to_owned()
}

/// Select with `cwd` as the working directory and no home.
fn select(cfg: &ServeConfig, cwd: &Path) -> Result<SelectedSession, SessionSelectionError> {
    let cwd = cwd.to_path_buf();
    cfg.select_stdio_session_with(None, move || Ok(cwd), None)
}

fn session(r: Result<SelectedSession, SessionSelectionError>) -> String {
    r.expect("a session").session
}

fn config_error(r: Result<SelectedSession, SessionSelectionError>) -> String {
    match r {
        Err(SessionSelectionError::Config(e)) => e.to_string(),
        other => panic!("expected a config error, got {other:?}"),
    }
}

#[test]
fn the_flag_always_wins_and_never_reads_the_cwd() {
    let root = ScratchDir::new("lambo-pr8-flag");
    let cfg = config(vec![project(s(&root), "mapped")], Some("general"));
    let got = cfg
        .select_stdio_session_with(
            Some("any thing/the flag keeps its looser rule"),
            || panic!("the cwd must not be read when --session is given"),
            None,
        )
        .expect("flag");
    assert_eq!(got.session, "any thing/the flag keeps its looser rule");
    assert_eq!(got.source, SessionSource::Flag);
}

#[test]
fn longest_prefix_wins_whatever_the_file_order() {
    let root = ScratchDir::new("lambo-pr8-longest");
    let outer = mkdir(&root, "work");
    let inner = mkdir(&root, "work/lambo");
    let cwd = mkdir(&root, "work/lambo/src/deep");
    let a = config(
        vec![project(s(&outer), "outer"), project(s(&inner), "inner")],
        Some("general"),
    );
    let b = config(
        vec![project(s(&inner), "inner"), project(s(&outer), "outer")],
        Some("general"),
    );
    for cfg in [&a, &b] {
        let got = select(cfg, &cwd).expect("mapped");
        assert_eq!(got.session, "inner");
        assert_eq!(
            got.source,
            SessionSource::Project { path: s(&inner) },
            "the source names the entry as written"
        );
    }
    // A cwd under the outer entry only takes the outer one.
    let sibling = mkdir(&root, "work/other");
    assert_eq!(session(select(&a, &sibling)), "outer");
}

#[test]
fn the_project_directory_itself_matches() {
    let root = ScratchDir::new("lambo-pr8-exact");
    let dir = mkdir(&root, "proj");
    let cfg = config(vec![project(s(&dir), "proj")], None);
    assert_eq!(session(select(&cfg, &dir)), "proj");
}

#[test]
fn prefixes_compare_by_component_not_by_string() {
    let root = ScratchDir::new("lambo-pr8-component");
    let lambo = mkdir(&root, "lambo");
    let worktree = mkdir(&root, "lambo-wt-32h");
    let cfg = config(vec![project(s(&lambo), "lambo")], Some("general"));
    let got = select(&cfg, &worktree).expect("default");
    assert_eq!(got.session, "general");
    assert_eq!(got.source, SessionSource::DefaultSession);
}

#[test]
fn a_trailing_slash_on_the_entry_changes_nothing() {
    let root = ScratchDir::new("lambo-pr8-slash");
    let dir = mkdir(&root, "proj");
    let cwd = mkdir(&root, "proj/a");
    let cfg = config(vec![project(format!("{}/", s(&dir)), "proj")], None);
    assert_eq!(session(select(&cfg, &cwd)), "proj");
}

#[test]
fn tilde_expands_to_home() {
    let home = ScratchDir::new("lambo-pr8-home");
    let proj = mkdir(&home, "Documents/work/lambo");
    let cwd = mkdir(&home, "Documents/work/lambo/src");
    let elsewhere = mkdir(&home, "Downloads");
    let cfg = config(
        vec![
            project("~/Documents/work/lambo", "lambo"),
            project("~", "home"),
        ],
        None,
    );
    let pick = |cwd: &Path| {
        let cwd = cwd.to_path_buf();
        cfg.select_stdio_session_with(None, move || Ok(cwd), Some(&home))
    };
    assert_eq!(session(pick(&cwd)), "lambo");
    assert_eq!(session(pick(&proj)), "lambo");
    assert_eq!(session(pick(&elsewhere)), "home", "bare ~ is $HOME");
}

#[test]
fn a_tilde_entry_without_home_is_refused_without_quoting_the_cwd() {
    let root = ScratchDir::new("lambo-pr8-nohome");
    let cwd = mkdir(&root, "secretive-cwd-marker");
    let cfg = config(vec![project("~/proj", "proj")], Some("general"));
    let msg = config_error(select(&cfg, &cwd));
    assert!(msg.contains("\"~/proj\"") && msg.contains("HOME"), "{msg}");
    assert!(!msg.contains("secretive-cwd-marker"), "cwd quoted: {msg}");
}

#[cfg(unix)]
#[test]
fn symlinks_resolve_on_both_sides() {
    let root = ScratchDir::new("lambo-pr8-symlink");
    let real = mkdir(&root, "real/proj");
    let cwd_real = mkdir(&root, "real/proj/src");
    let link = root.join("link");
    std::os::unix::fs::symlink(root.join("real"), &link).expect("symlink");

    // Entered through a link: the entry for the real directory matches.
    let by_real = config(vec![project(s(&real), "proj")], None);
    assert_eq!(session(select(&by_real, &link.join("proj/src"))), "proj");

    // Written through a link: a cwd in the real directory matches.
    let by_link = config(vec![project(s(&link.join("proj")), "proj")], None);
    assert_eq!(session(select(&by_link, &cwd_real)), "proj");

    // A link INSIDE a project pointing outside it is followed: the cwd is
    // where the link leads, not where it was entered.
    let outside = mkdir(&root, "outside");
    std::os::unix::fs::symlink(&outside, real.join("escape")).expect("symlink");
    let got = select(&by_real, &real.join("escape")).map(|s| s.session);
    assert!(
        matches!(got, Err(SessionSelectionError::Missing)),
        "a link out of the project leaves it: {got:?}"
    );
}

#[test]
fn dot_dot_in_the_cwd_is_resolved_before_matching() {
    let root = ScratchDir::new("lambo-pr8-dotdot");
    let proj = mkdir(&root, "proj");
    mkdir(&root, "other");
    let cfg = config(vec![project(s(&proj), "proj")], Some("general"));
    // Lexically under proj/, really in other/.
    let tricky = proj.join("..").join("other");
    assert_eq!(session(select(&cfg, &tricky)), "general");
    // And the reverse: lexically elsewhere, really in proj/.
    let back = root.join("other").join("..").join("proj");
    assert_eq!(session(select(&cfg, &back)), "proj");
}

#[test]
fn an_entry_that_does_not_exist_is_skipped() {
    let root = ScratchDir::new("lambo-pr8-missing-entry");
    let cwd = mkdir(&root, "proj");
    let cfg = config(
        vec![project(s(&root.join("gone")), "gone")],
        Some("general"),
    );
    assert_eq!(session(select(&cfg, &cwd)), "general");
}

#[test]
fn an_unreadable_cwd_falls_back_to_default_session() {
    let root = ScratchDir::new("lambo-pr8-nocwd");
    let cfg = config(vec![project(s(&root), "proj")], Some("general"));
    let got = cfg
        .select_stdio_session_with(
            None,
            || Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            None,
        )
        .expect("default");
    assert_eq!(got.session, "general");
    assert_eq!(got.source, SessionSource::DefaultSessionCwdUnavailable);

    // A cwd that exists to `current_dir` but no longer canonicalizes.
    let deleted = root.join("deleted-cwd");
    let got = select(&cfg, &deleted).expect("default");
    assert_eq!(got.source, SessionSource::DefaultSessionCwdUnavailable);

    // Without a default it is the plain refusal.
    let none = config(vec![project(s(&root), "proj")], None);
    assert!(matches!(
        none.select_stdio_session_with(
            None,
            || Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            None
        ),
        Err(SessionSelectionError::Missing)
    ));
}

#[test]
fn no_match_and_no_default_is_the_missing_session_refusal() {
    let root = ScratchDir::new("lambo-pr8-none");
    let proj = mkdir(&root, "proj");
    let cwd = mkdir(&root, "elsewhere");
    let cfg = config(vec![project(s(&proj), "proj")], None);
    let err = select(&cfg, &cwd).expect_err("refused");
    assert!(matches!(err, SessionSelectionError::Missing));
    assert_eq!(err.to_string(), SESSION_REQUIRED);
    assert!(SESSION_REQUIRED.contains("--session <SESSION>"));
}

#[test]
fn without_a_map_the_cwd_is_never_read() {
    let cfg = config(Vec::new(), Some("general"));
    let got = cfg
        .select_stdio_session_with(None, || panic!("no map, no cwd read"), None)
        .expect("default");
    assert_eq!(got.session, "general");
    assert_eq!(got.source, SessionSource::DefaultSession);

    let empty = ServeConfig::default();
    assert!(matches!(
        empty.select_stdio_session_with(None, || panic!("no map, no cwd read"), None),
        Err(SessionSelectionError::Missing)
    ));
}

#[cfg(unix)]
#[test]
fn two_entries_for_one_directory_must_agree() {
    let root = ScratchDir::new("lambo-pr8-ambiguous");
    let proj = mkdir(&root, "proj");
    let cwd = mkdir(&root, "proj/src/cwd-marker");
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&proj, &alias).expect("symlink");

    let clash = config(
        vec![project(s(&proj), "one"), project(s(&alias), "two")],
        Some("general"),
    );
    let msg = config_error(select(&clash, &cwd));
    assert!(
        msg.contains(&format!("{:?}", s(&proj)))
            && msg.contains(&format!("{:?}", s(&alias)))
            && msg.contains("\"one\"")
            && msg.contains("\"two\""),
        "{msg}"
    );
    assert!(!msg.contains("cwd-marker"), "cwd quoted: {msg}");

    let agree = config(
        vec![project(s(&proj), "one"), project(s(&alias), "one")],
        None,
    );
    assert_eq!(session(select(&agree, &cwd)), "one");

    // A clash above a deeper match does not matter: the deeper entry wins.
    let deeper = mkdir(&root, "proj/src");
    let shadowed = config(
        vec![
            project(s(&proj), "one"),
            project(s(&alias), "two"),
            project(s(&deeper), "deep"),
        ],
        None,
    );
    assert_eq!(session(select(&shadowed, &cwd)), "deep");
}

#[test]
fn mapped_and_default_sessions_pass_the_addressed_rule() {
    // `validate` refuses these when the file is read; a struct built in code
    // that skips it is still refused here rather than served.
    let root = ScratchDir::new("lambo-pr8-addressed");
    let cfg = config(vec![project(s(&root), "has space")], None);
    let msg = config_error(select(&cfg, &root));
    assert!(msg.contains("not an addressable session id"), "{msg}");
    let cfg = config(Vec::new(), Some(".hidden"));
    let msg = config_error(cfg.select_stdio_session_with(None, || unreachable!(), None));
    assert!(msg.contains("default_session"), "{msg}");
}

fn parse(projects: &str) -> Result<LamboFile, crate::types::LamboError> {
    LamboFile::from_toml_str(&format!("[store]\nkind = \"memory\"\n\n{projects}"))
}

#[test]
fn validate_accepts_absolute_and_home_paths_only() {
    for ok in ["/srv/proj", "~", "~/proj", "~/"] {
        parse(&format!(
            "[[serve.projects]]\npath = {ok:?}\nsession = \"a\"\n"
        ))
        .unwrap_or_else(|e| panic!("{ok:?} refused: {e}"));
    }
    for (bad, why) in [
        ("relative/proj", "must be absolute"),
        ("./proj", "must be absolute"),
        ("~other/proj", "only `~` and `~/...`"),
        ("~other", "only `~` and `~/...`"),
    ] {
        let err = parse(&format!(
            "[[serve.projects]]\npath = {bad:?}\nsession = \"a\"\n"
        ))
        .expect_err(bad)
        .to_string();
        assert!(err.contains(why) && err.contains(bad), "{bad:?}: {err}");
    }
}

#[test]
fn only_selection_keys_count_as_enforced() {
    assert!(!ServeConfig::default().has_unenforced_keys());
    let selection_only = config(vec![project("/p", "a")], Some("general"));
    assert!(!selection_only.has_unenforced_keys());
    let pinned = ServeConfig {
        sessions: vec!["a".into()],
        ..selection_only.clone()
    };
    assert!(pinned.has_unenforced_keys());
    let bounded = ServeConfig {
        max_attached: Some(4),
        ..selection_only
    };
    assert!(bounded.has_unenforced_keys());
}

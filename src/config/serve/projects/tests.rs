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
fn a_tilde_entry_without_home_is_skipped_and_reported() {
    // #32 PR 8 review L2: HOME unset skips the `~` entries (the caller warns
    // once) instead of refusing a map whose other entries still apply.
    let root = ScratchDir::new("lambo-pr8-nohome");
    let proj = mkdir(&root, "proj");
    let cwd = mkdir(&root, "proj/secretive-cwd-marker");

    // Only a `~` entry: default_session, flagged.
    let cfg = config(vec![project("~/proj", "tilde")], Some("general"));
    let got = select(&cfg, &cwd).expect("default");
    assert_eq!(got.session, "general");
    assert_eq!(got.source, SessionSource::DefaultSession);
    assert!(got.tilde_entries_skipped);

    // An absolute entry still applies, and the skip is still reported.
    let mixed = config(vec![project("~", "tilde"), project(s(&proj), "proj")], None);
    let got = select(&mixed, &cwd).expect("absolute entry");
    assert_eq!(got.session, "proj");
    assert!(got.tilde_entries_skipped);

    // No `~` entry: nothing to report.
    let plain = config(vec![project(s(&proj), "proj")], None);
    assert!(!select(&plain, &cwd).expect("plain").tilde_entries_skipped);

    // Nothing left and no default: the usage refusal, whose hint says why
    // without quoting the cwd, an entry or HOME's value.
    let bare = config(vec![project("~/proj", "tilde")], None);
    match select(&bare, &cwd) {
        Err(SessionSelectionError::Missing(missing)) => {
            assert!(missing.tilde_entries_skipped);
            let hint = missing.hint();
            assert!(hint.contains("HOME") && hint.contains('~'), "{hint}");
            assert!(!hint.contains("secretive-cwd-marker"), "cwd quoted: {hint}");
            assert!(!hint.contains("~/proj"), "entry quoted: {hint}");
        }
        other => panic!("expected Missing, got {other:?}"),
    }
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
        matches!(got, Err(SessionSelectionError::Missing(_))),
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
        Err(SessionSelectionError::Missing(_))
    ));
}

#[test]
fn no_match_and_no_default_is_the_missing_session_refusal() {
    let root = ScratchDir::new("lambo-pr8-none");
    let proj = mkdir(&root, "proj");
    let cwd = mkdir(&root, "elsewhere");
    let cfg = config(vec![project(s(&proj), "proj")], None);
    let err = select(&cfg, &cwd).expect_err("refused");
    assert!(matches!(err, SessionSelectionError::Missing(_)));
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
        Err(SessionSelectionError::Missing(_))
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

/// A relative path that names `target` when resolved against the process's
/// own working directory: `../` up to the root, then `target` without it.
fn relative_from_process_cwd(target: &Path) -> PathBuf {
    let here = std::fs::canonicalize(std::env::current_dir().expect("cwd")).expect("canonical");
    let target = std::fs::canonicalize(target).expect("canonical target");
    let mut rel = PathBuf::new();
    for _ in 0..depth(&here) {
        rel.push("..");
    }
    rel.push(target.strip_prefix("/").expect("absolute target"));
    rel
}

#[test]
fn a_relative_home_is_treated_as_unset() {
    // #32 PR 8 review L1: `fs::canonicalize` resolves a relative path against
    // the process cwd, which for a stdio serve is the project, so a relative
    // `$HOME` would let a `~` catch-all match whatever directory the client
    // chose and beat the real entry.
    let root = ScratchDir::new("lambo-pr8-relhome");
    let cwd = mkdir(&root, "proj");
    let home = relative_from_process_cwd(&cwd);
    assert!(home.is_relative());
    let cfg = config(vec![project("~", "home")], Some("general"));
    let cwd_owned = cwd.clone();
    let got = cfg
        .select_stdio_session_with(None, move || Ok(cwd_owned), Some(&home))
        .expect("default");
    assert_eq!(
        got.session, "general",
        "a relative HOME resolved against the process cwd"
    );
    assert!(
        got.tilde_entries_skipped,
        "the `~` entry was skipped as with no HOME"
    );
}

#[test]
fn the_refusal_hint_says_when_the_map_was_not_checked() {
    // #32 PR 8 review L4: an unresolvable cwd skips the map, so "neither
    // applies here" would tell the operator the map does not cover a
    // directory it never looked at.
    let root = ScratchDir::new("lambo-pr8-hint");
    let proj = mkdir(&root, "proj");
    let cfg = config(vec![project(s(&proj), "proj")], None);

    let missing = |r: Result<SelectedSession, SessionSelectionError>| match r {
        Err(SessionSelectionError::Missing(m)) => m,
        other => panic!("expected Missing, got {other:?}"),
    };

    let skipped = missing(cfg.select_stdio_session_with(
        None,
        || Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        None,
    ));
    let hint = skipped.hint();
    assert!(
        hint.contains("could not be resolved") && hint.contains("not checked"),
        "{hint}"
    );
    assert!(!hint.contains("neither applies"), "{hint}");

    let elsewhere = mkdir(&root, "elsewhere");
    let unmatched = missing(select(&cfg, &elsewhere));
    let hint = unmatched.hint();
    assert!(hint.contains("neither applies here"), "{hint}");
    assert!(!hint.contains("could not be resolved"), "{hint}");
    assert_ne!(skipped, unmatched);
}

#[test]
fn the_root_directory_is_a_catch_all_that_loses_to_any_deeper_entry() {
    let root = ScratchDir::new("lambo-pr8-slash-root");
    let proj = mkdir(&root, "proj");
    let cwd = mkdir(&root, "proj/src");
    let elsewhere = mkdir(&root, "elsewhere");
    let cfg = config(
        vec![project("/", "everything"), project(s(&proj), "proj")],
        Some("general"),
    );
    assert_eq!(session(select(&cfg, &cwd)), "proj");
    let got = select(&cfg, &elsewhere).expect("catch-all");
    assert_eq!(got.session, "everything", "`/` covers every directory");
    assert_eq!(got.source, SessionSource::Project { path: "/".into() });
}

/// macOS volumes are case- and normalization-insensitive by default, and
/// `fs::canonicalize` (realpath) returns the on-disk spelling, so entries
/// that differ from the directory only in case or Unicode normalization
/// still match, and two such variants naming different sessions are one
/// directory and refused. Skips itself on a case-sensitive volume.
#[cfg(target_os = "macos")]
#[test]
fn macos_case_and_unicode_normalization_variants_are_one_directory() {
    let root = ScratchDir::new("lambo-pr8-macos-fold");
    let real = mkdir(&root, "Lambo");
    let cwd = mkdir(&root, "Lambo/src");
    if !root.join("lambo").exists() {
        eprintln!("skipping: {} is on a case-sensitive volume", root.display());
        return;
    }

    // Case: the entry spells the directory in lower case.
    let lower = config(vec![project(s(&root.join("lambo")), "lambo")], None);
    assert_eq!(session(select(&lower, &cwd)), "lambo");
    // The cwd spelled differently canonicalizes to the on-disk case too.
    assert_eq!(session(select(&lower, &root.join("LAMBO/SRC"))), "lambo");

    // Two case variants naming different sessions: refused, not file order.
    let clash = config(
        vec![
            project(s(&real), "upper"),
            project(s(&root.join("lambo")), "lower"),
        ],
        None,
    );
    let msg = config_error(select(&clash, &cwd));
    assert!(msg.contains("same directory"), "{msg}");

    // Normalization: directory created NFD, entry written NFC (and back).
    let nfd = "cafe\u{301}";
    let nfc = "caf\u{e9}";
    let dir = mkdir(&root, nfd);
    let inner = mkdir(&root, &format!("{nfd}/src"));
    assert!(
        root.join(nfc).exists(),
        "APFS looks names up normalization-insensitively"
    );
    let by_nfc = config(vec![project(s(&root.join(nfc)), "cafe")], None);
    assert_eq!(session(select(&by_nfc, &inner)), "cafe");
    let by_nfd = config(vec![project(s(&dir), "cafe")], None);
    assert_eq!(
        session(select(&by_nfd, &root.join(nfc).join("src"))),
        "cafe"
    );
    let both = config(
        vec![project(s(&dir), "nfd"), project(s(&root.join(nfc)), "nfc")],
        None,
    );
    let msg = config_error(select(&both, &inner));
    assert!(msg.contains("same directory"), "{msg}");
}

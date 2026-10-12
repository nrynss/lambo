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

/// #32 PR 8 merged with PR 4: on stdio the selection keys are enforced, so
/// a table with only them raises no notice; over HTTP the cwd map is never
/// read and is still named. `sessions` and `max_attached` are PR 4's, and
/// the other bounds PR 6's, read over HTTP and not applicable to stdio, so
/// neither transport lists them.
#[test]
fn only_selection_keys_count_as_enforced() {
    assert!(ServeConfig::default().unenforced_keys(true).is_empty());
    assert!(ServeConfig::default().unenforced_keys(false).is_empty());
    let selection_only = config(vec![project("/p", "a")], Some("general"));
    assert!(selection_only.unenforced_keys(true).is_empty());
    assert_eq!(
        selection_only.unenforced_keys(false),
        vec!["[[serve.projects]]"]
    );
    let pinned = ServeConfig {
        sessions: vec!["general".into()],
        max_attached: Some(4),
        ..selection_only.clone()
    };
    assert!(pinned.unenforced_keys(true).is_empty());
    let bounded = ServeConfig {
        idle_detach_secs: Some(60),
        ..selection_only
    };
    assert!(bounded.unenforced_keys(true).is_empty());
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

#[test]
fn patterns_cover_siblings_and_descendants_with_whole_name_boundaries() {
    let root = ScratchDir::new("lambo-pattern-siblings");
    let pattern = format!("{}/lambo-wt-*", s(&root));
    let cfg = config(vec![project(&pattern, "lambo")], Some("general"));
    for rel in ["lambo-wt-", "lambo-wt-one", "lambo-wt-two/src/deep"] {
        let cwd = mkdir(&root, rel);
        let got = select(&cfg, &cwd).unwrap();
        assert_eq!(got.session, "lambo");
        assert_eq!(
            got.source,
            SessionSource::Project {
                path: pattern.clone()
            }
        );
    }
    for rel in [
        "lambo-wt",
        "xlambo-wt-one",
        "other/lambo-wt-one",
        "lambo-wt-otherish",
    ] {
        let cwd = mkdir(&root, rel);
        if rel == "lambo-wt-otherish" {
            let suffix = config(
                vec![project(format!("{}/lambo-wt-*ishx", s(&root)), "bad")],
                Some("general"),
            );
            assert_eq!(session(select(&suffix, &cwd)), "general");
        } else {
            assert_eq!(session(select(&cfg, &cwd)), "general");
        }
    }
    assert_eq!(session(select(&cfg, &root)), "general");
    let absent = config(
        vec![project(format!("{}/absent/*", s(&root)), "bad")],
        Some("general"),
    );
    assert_eq!(session(select(&absent, &root)), "general");
    let no_default = config(vec![project(&pattern, "lambo")], None);
    assert!(matches!(
        select(&no_default, &root),
        Err(SessionSelectionError::Missing(_))
    ));
}

#[test]
fn pattern_priority_is_depth_then_literal_and_conflicting_ties_refuse() {
    let root = ScratchDir::new("lambo-pattern-priority");
    let dir = mkdir(&root, "lambo-wt-one");
    let deep = mkdir(&dir, "src/private-cwd-marker");
    let broad = project(format!("{}/*", s(&root)), "broad");
    let narrow = project(format!("{}/lambo-wt-*", s(&root)), "narrow");
    for projects in [
        vec![broad.clone(), narrow.clone()],
        vec![narrow.clone(), broad.clone()],
    ] {
        let msg = config_error(select(&config(projects, Some("general")), &deep));
        assert!(msg.contains("equal priority"));
        assert!(!msg.contains("private-cwd-marker"));
    }
    for projects in [
        vec![broad.clone(), project(s(&dir), "literal")],
        vec![project(s(&dir), "literal"), broad.clone()],
    ] {
        assert_eq!(session(select(&config(projects, None), &deep)), "literal");
    }
    let deeper = project(s(&dir.join("src")), "deep");
    assert_eq!(
        session(select(&config(vec![broad, narrow, deeper], None), &deep)),
        "deep"
    );
    let agree = config(
        vec![
            project(format!("{}/*", s(&root)), "same"),
            project(format!("{}/lambo*", s(&root)), "same"),
        ],
        None,
    );
    assert_eq!(session(select(&agree, &deep)), "same");
}

#[test]
fn patterns_expand_home_and_preserve_flag_and_fallback_behavior() {
    let home = ScratchDir::new("lambo-pattern-home");
    let cwd = mkdir(&home, "work/lambo-wt-one/src");
    let cfg = config(vec![project("~/work/lambo-wt-*", "lambo")], Some("general"));
    assert_eq!(
        session(cfg.select_stdio_session_with(None, || Ok(cwd.clone()), Some(&home))),
        "lambo"
    );
    for home in [None, Some(Path::new("relative-home"))] {
        let got = cfg
            .select_stdio_session_with(None, || Ok(cwd.clone()), home)
            .unwrap();
        assert_eq!(got.session, "general");
        assert!(got.tilde_entries_skipped);
    }
    assert_eq!(
        session(cfg.select_stdio_session_with(Some("flag"), || panic!("flag bypass"), None)),
        "flag"
    );
    let got = select(&cfg, &home.join("gone")).unwrap();
    assert_eq!(got.source, SessionSource::DefaultSessionCwdUnavailable);
}

#[cfg(unix)]
#[test]
fn patterns_canonicalize_parent_children_and_cwd_symlinks() {
    let root = ScratchDir::new("lambo-pattern-links");
    let parent = mkdir(&root, "work");
    let target = mkdir(&root, "outside/target/src");
    std::os::unix::fs::symlink(root.join("outside/target"), parent.join("lambo-wt-link")).unwrap();
    std::os::unix::fs::symlink(&parent, root.join("alias")).unwrap();
    std::os::unix::fs::symlink(root.join("gone"), parent.join("lambo-wt-broken")).unwrap();
    std::fs::write(parent.join("lambo-wt-file"), "ordinary file").unwrap();
    let cfg = config(
        vec![project(
            format!("{}/alias/../alias/lambo-wt-*", s(&root)),
            "lambo",
        )],
        None,
    );
    for cwd in [&target, &parent.join("lambo-wt-link/src")] {
        assert_eq!(session(select(&cfg, cwd)), "lambo");
    }
    let literal = project(s(&root.join("outside/target")), "literal");
    assert_eq!(
        session(select(
            &config(vec![cfg.projects[0].clone(), literal], None),
            &target
        )),
        "literal"
    );
    let bad_parent = config(
        vec![project(format!("{}/work/lambo-wt-file/*", s(&root)), "bad")],
        Some("general"),
    );
    assert_eq!(session(select(&bad_parent, &root)), "general");
}

#[test]
fn pattern_syntax_is_only_final_component_stars() {
    for ok in [
        "/work/lambo-wt-*",
        "/work/*/",
        "~/work/*a*b**",
        "/work/[?]*",
    ] {
        check_project_path(ok).unwrap();
    }
    for bad in ["/work/*/child", "~/work/*/..", "/work/*/.", "/work/a*/b*"] {
        let msg = parse(&format!(
            "[[serve.projects]]\npath = {bad:?}\nsession = \"a\"\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(msg.contains("final component"), "{msg}");
    }
    for (pattern, name, yes) in [
        ("a*b*c", "abc", true),
        ("a**b", "axxb", true),
        ("*", "", true),
        ("a*b", "axxbc", false),
        ("[?]*", "[?]x", true),
        ("[?]*", "ax", false),
        ("Lambo*", "lambo-one", false),
        ("café*", "cafe\u{301}-one", false),
    ] {
        assert_eq!(
            matches_name(pattern.as_bytes(), name.as_bytes()),
            yes,
            "{pattern} / {name}"
        );
    }
}

#[cfg(unix)]
#[test]
fn patterns_match_non_utf8_names_without_lossy_conversion() {
    use std::os::unix::ffi::OsStringExt;
    let root = ScratchDir::new("lambo-pattern-bytes");
    let dir = root.join(std::ffi::OsString::from_vec(b"lambo-wt-\xff".to_vec()));
    assert!(matches_name(b"lambo-wt-*", b"lambo-wt-\xff"));
    if let Err(error) = std::fs::create_dir(&dir) {
        // APFS rejects invalid UTF-8 names; byte matching is still verified.
        assert_eq!(
            error.raw_os_error(),
            Some(92),
            "unexpected mkdir failure: {error}"
        );
        return;
    }
    let cfg = config(
        vec![project(format!("{}/lambo-wt-*", s(&root)), "lambo")],
        None,
    );
    assert_eq!(session(select(&cfg, &dir)), "lambo");
}

#[test]
fn pattern_case_and_unicode_are_byte_exact_even_on_insensitive_filesystems() {
    let root = ScratchDir::new("lambo-pattern-case");
    for (name, pattern) in [("Lambo-wt-one", "lambo-wt-*"), ("cafe\u{301}-one", "café*")] {
        let dir = mkdir(&root, name);
        let cfg = config(
            vec![project(format!("{}/{pattern}", s(&root)), "bad")],
            Some("general"),
        );
        assert_eq!(session(select(&cfg, &dir)), "general");
    }
}

#[cfg(unix)]
#[test]
fn an_unreadable_pattern_parent_is_skipped() {
    use std::os::unix::fs::PermissionsExt;
    let root = ScratchDir::new("lambo-pattern-unreadable");
    let parent = mkdir(&root, "parent");
    let target = mkdir(&root, "outside/target");
    std::os::unix::fs::symlink(&target, parent.join("lambo-one")).unwrap();
    let old = std::fs::metadata(&parent).unwrap().permissions();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o0)).unwrap();
    let denied = std::fs::read_dir(&parent).is_err();
    let cfg = config(
        vec![project(format!("{}/lambo-*", s(&parent)), "bad")],
        Some("general"),
    );
    let result = select(&cfg, &target);
    std::fs::set_permissions(&parent, old).unwrap();
    if denied {
        assert_eq!(session(result), "general");
    }
}

#[test]
fn large_parent_scans_match_without_retaining_child_lists() {
    let root = ScratchDir::new("lambo-pattern-many");
    for n in 0..256 {
        mkdir(&root, &format!("sibling-{n}"));
    }
    let cwd = mkdir(&root, "lambo-wt-last/src");
    let cfg = config(
        vec![project(format!("{}/lambo-wt-*", s(&root)), "lambo")],
        None,
    );
    assert_eq!(session(select(&cfg, &cwd)), "lambo");
}

#[cfg(unix)]
#[test]
fn a_deeper_canonical_pattern_root_beats_a_shallower_literal() {
    let root = ScratchDir::new("lambo-pattern-depth");
    let parent = mkdir(&root, "parent");
    let target = mkdir(&root, "target/deep/src");
    std::os::unix::fs::symlink(root.join("target/deep"), parent.join("project-link")).unwrap();
    let cfg = config(
        vec![
            project(s(&root.join("target")), "literal"),
            project(format!("{}/project-*", s(&parent)), "pattern"),
        ],
        None,
    );
    assert_eq!(session(select(&cfg, &target)), "pattern");
}

#[cfg(windows)]
#[test]
fn native_windows_parent_component_patterns_are_refused() {
    for bad in [r"C:\work\*\child", r"C:\work\a*\..", r"C:\work\*\."] {
        assert!(check_project_path(bad)
            .unwrap_err()
            .to_string()
            .contains("final component"));
    }
    check_project_path(r"C:\work\lambo-wt-*").unwrap();
}

#[cfg(unix)]
#[test]
fn stars_in_home_are_literal_data_not_project_pattern_syntax() {
    let root = ScratchDir::new("lambo-pattern-literal-home");
    let home = mkdir(&root, "user*");
    let descendant = mkdir(&home, "src/deep");
    let sibling = mkdir(&root, "user-unrelated/src");
    for path in ["~", "~/"] {
        let cfg = config(vec![project(path, "home")], Some("general"));
        for cwd in [&home, &descendant] {
            let got = cfg
                .select_stdio_session_with(None, || Ok(cwd.clone()), Some(&home))
                .unwrap();
            assert_eq!(got.session, "home", "{path}");
        }
        assert_eq!(
            session(cfg.select_stdio_session_with(None, || Ok(sibling.clone()), Some(&home))),
            "general",
            "{path}"
        );

        // The literal HOME root must retain literal priority against a pattern
        // that matches the same directory, whichever entry appears first.
        let pattern = project(format!("{}/user*", s(&root)), "pattern");
        for projects in [
            vec![project(path, "home"), pattern.clone()],
            vec![pattern.clone(), project(path, "home")],
        ] {
            let cfg = config(projects, None);
            assert_eq!(
                session(cfg.select_stdio_session_with(
                    None,
                    || Ok(descendant.clone()),
                    Some(&home)
                )),
                "home"
            );
        }
    }
    let child = mkdir(&home, "lambo-wt-one/src");
    let unrelated = mkdir(&root, "user-other/lambo-wt-one/src");
    let cfg = config(vec![project("~/lambo-wt-*", "project")], Some("general"));
    assert_eq!(
        session(cfg.select_stdio_session_with(None, || Ok(child.clone()), Some(&home))),
        "project"
    );
    assert_eq!(
        session(cfg.select_stdio_session_with(None, || Ok(unrelated.clone()), Some(&home))),
        "general"
    );
}

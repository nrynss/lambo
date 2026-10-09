use super::*;
use crate::surface::session::{RefusalReason, SessionNeed};
use crate::LamboFile;

/// Obviously fake token values. Never real secrets; set only through
/// `crate::test_util::env_lock()`.
const FAKE_AGENTS: &str = "fake-agents-value-for-tests-only";
const FAKE_APP: &str = "fake-app-value-for-tests-only";
const FAKE_OPERATOR: &str = "fake-operator-value-for-tests-only";

/// Environment variable names private to these tests.
const AGENTS_ENV: &str = "LAMBO_TEST_32_AGENTS_CRED";
const APP_ENV: &str = "LAMBO_TEST_32_APP_CRED";
const OPERATOR_ENV: &str = "LAMBO_TEST_32_OPERATOR_CRED";

fn full_toml() -> String {
    format!(
        r#"
[store]
kind = "memory"

[serve]
sessions = ["lambo", "rustydocs", "general"]
default_session = "general"
max_attached = 8
attach_concurrency = 1
idle_detach_secs = 600
per_session_rps = 20

[[serve.projects]]
path = "~/Documents/work/lambo"
session = "lambo"

[[serve.projects]]
path = "/srv/rustydocs"
session = "rustydocs"

[[serve.credential]]
name = "agents"
token_env = "{AGENTS_ENV}"
sessions = ["lambo", "rustydocs", "general"]

[[serve.credential]]
name = "dresscode"
token_env = "{APP_ENV}"
session_prefix = "dc-u-"
create = true
erase = true

[[serve.credential]]
name = "operator"
token_env = "{OPERATOR_ENV}"
sessions = ["*"]
erase = true
admin = true
"#
    )
}

fn parse(toml: &str) -> Result<LamboFile, LamboError> {
    LamboFile::from_toml_str(toml)
}

fn refused(toml: &str) -> String {
    match parse(toml) {
        Ok(f) => panic!("must be refused, parsed as {:?}", f.serve),
        Err(e) => e.to_string(),
    }
}

fn id(raw: &str) -> AddressedSessionId {
    parse_addressed(raw).expect("valid id")
}

/// No `[serve]` table is the empty config, and the empty config is what
/// every existing file keeps: no behaviour hangs off it in PR 1.
#[test]
fn an_absent_serve_table_is_empty_and_defaults_apply() {
    let f = parse("[store]\nkind = \"memory\"\n").expect("parse");
    assert!(f.serve.is_empty());
    assert_eq!(f.serve, ServeConfig::default());
    assert_eq!(f.serve.max_attached(), DEFAULT_MAX_ATTACHED);
    assert_eq!(f.serve.attach_concurrency(), DEFAULT_ATTACH_CONCURRENCY);
    assert_eq!(
        f.serve.idle_detach(),
        std::time::Duration::from_secs(DEFAULT_IDLE_DETACH_SECS)
    );
    assert_eq!(
        f.serve.per_session_rps(50),
        50,
        "defaults to the global rps"
    );
    assert_eq!(f.serve.per_session_rps(0), 0);
    assert_eq!((DEFAULT_MAX_ATTACHED, DEFAULT_ATTACH_CONCURRENCY), (16, 2));
    assert_eq!(DEFAULT_IDLE_DETACH_SECS, 900);
    // An empty `[serve]` header is the same thing.
    assert!(parse("[serve]\n").expect("parse").serve.is_empty());
    assert!(
        f.serve
            .resolve_credentials_with(|_| None)
            .expect("none")
            .is_empty(),
        "no credentials configured reads no environment"
    );
}

/// The §7.1 shape parses into the documented fields.
#[test]
fn the_design_shape_parses() {
    let f = parse(&full_toml()).expect("parse");
    let s = &f.serve;
    assert!(!s.is_empty());
    assert_eq!(s.sessions, ["lambo", "rustydocs", "general"]);
    assert_eq!(s.default_session.as_deref(), Some("general"));
    assert_eq!(s.max_attached(), 8);
    assert_eq!(s.attach_concurrency(), 1);
    assert_eq!(s.idle_detach(), std::time::Duration::from_secs(600));
    assert_eq!(s.per_session_rps(50), 20);
    assert_eq!(s.projects.len(), 2);
    assert_eq!(s.projects[0].path, "~/Documents/work/lambo");
    assert_eq!(s.projects[0].session, "lambo");
    assert_eq!(s.credentials.len(), 3);
    let app = &s.credentials[1];
    assert_eq!(app.name, "dresscode");
    assert_eq!(app.token_env.as_deref(), Some(APP_ENV));
    assert_eq!(app.session_prefix.as_deref(), Some("dc-u-"));
    assert!(app.create && app.erase && !app.admin);
    assert!(app.token.is_none());

    let hosted = s.hosted_sessions();
    for inside in ["lambo", "rustydocs", "general", "dc-u-1"] {
        assert!(hosted.contains(&id(inside)), "{inside}");
    }
    assert!(!hosted.contains(&id("other")));
}

/// Level B: an unknown key anywhere under `[serve]` is a hard error.
#[test]
fn unknown_serve_keys_fail_closed() {
    for typo in [
        "[serve]\nsesions = [\"a\"]\n",
        "[serve]\nmax_attach = 4\n",
        "[serve]\nidle_detach = 4\n",
        "[serve]\nauth_token = \"x\"\n",
        "[[serve.projects]]\npath = \"/a\"\nsession = \"a\"\ncwd = \"/a\"\n",
        "[[serve.projects]]\npath = \"/a\"\n",
        "[[serve.credential]]\nname = \"a\"\ntoken_env = \"A\"\nsessions = [\"a\"]\nread = true\n",
        "[[serve.credentials]]\nname = \"a\"\ntoken_env = \"A\"\nsessions = [\"a\"]\n",
        "[[serve.credential]]\nname = \"a\"\ntokenenv = \"A\"\nsessions = [\"a\"]\n",
    ] {
        let err = refused(typo);
        assert!(err.contains("lambo.toml"), "{typo:?}: {err}");
    }
}

/// An inline token is refused, whatever its type, with a message that names
/// the fix (`token_env`) rather than a bare unknown-field error, and the
/// refusal never quotes the value.
#[test]
fn an_inline_token_is_refused_without_echoing_it() {
    for value in [
        "\"fake-inline-value-xyzzy\"",
        "\"\"",
        "12345",
        "{ value = \"fake-inline-value-xyzzy\" }",
        "[\"fake-inline-value-xyzzy\"]",
    ] {
        let toml = format!(
            "[[serve.credential]]\nname = \"agents\"\ntoken = {value}\nsessions = [\"lambo\"]\n"
        );
        let err = refused(&toml);
        assert!(err.contains("inline token"), "{value}: {err}");
        assert!(!err.contains("xyzzy"), "the value leaked: {err}");
        assert!(!err.contains("12345"), "the value leaked: {err}");
        // Also refused with token_env present: the inline key is the error.
        let both = format!(
            "[[serve.credential]]\nname = \"agents\"\ntoken_env = \"{AGENTS_ENV}\"\n\
             token = {value}\nsessions = [\"lambo\"]\n"
        );
        let err = refused(&both);
        assert!(
            err.contains("inline token") && !err.contains("xyzzy"),
            "{err}"
        );
    }
    // Parsed directly (bypassing validate), the value is still not held.
    let raw: LamboFile = toml::from_str(
        "[[serve.credential]]\nname = \"agents\"\ntoken = \"fake-inline-value-xyzzy\"\n",
    )
    .expect("raw parse");
    assert!(!format!("{raw:?}").contains("xyzzy"));
    assert!(!toml::to_string(&raw).expect("serialize").contains("xyzzy"));
}

/// #32 §8 PR 1 acceptance: more pinned sessions than `max_attached` and zero
/// bounds are refused; exactly `max_attached` pinned is fine.
#[test]
fn bounds_are_validated() {
    let err = refused("[serve]\nsessions = [\"a\", \"b\", \"c\"]\nmax_attached = 2\n");
    assert!(err.contains("max_attached"), "{err}");
    parse("[serve]\nsessions = [\"a\", \"b\"]\nmax_attached = 2\n").expect("equal is fine");

    let seventeen: Vec<String> = (0..17).map(|i| format!("\"s{i}\"")).collect();
    let err = refused(&format!("[serve]\nsessions = [{}]\n", seventeen.join(", ")));
    assert!(
        err.contains("17 pinned") && err.contains("(16)"),
        "the default cap applies: {err}"
    );

    for (toml, key) in [
        ("[serve]\nmax_attached = 0\n", "max_attached"),
        ("[serve]\nattach_concurrency = 0\n", "attach_concurrency"),
        ("[serve]\nidle_detach_secs = 0\n", "idle_detach_secs"),
    ] {
        let err = refused(toml);
        assert!(err.contains(key), "{toml:?}: {err}");
    }
    let err = refused("[serve]\nsessions = [\"a\", \"a\"]\n");
    assert!(err.contains("twice"), "{err}");
}

/// A session named in `[serve]` must pass the strict addressed charset
/// (decision 16), and the refusal says how to serve it anyway. Existing names
/// like `lambo-dev` pass.
#[test]
fn a_session_the_strict_charset_refuses_is_a_startup_error() {
    parse("[serve]\nsessions = [\"lambo-dev\", \"a.b\", \"x:y\"]\n").expect("deployed names pass");
    for bad in ["has space", ".hidden", "a/b", "a%2Fb", "*", ""] {
        let err = refused(&format!("[serve]\nsessions = [{bad:?}]\n"));
        assert!(
            err.contains("cannot be addressed by URL") && err.contains("lambo serve --session"),
            "{bad:?}: {err}"
        );
        let err = refused(&format!("[serve]\ndefault_session = {bad:?}\n"));
        assert!(err.contains("default_session"), "{bad:?}: {err}");
        let err = refused(&format!(
            "[[serve.projects]]\npath = \"/p\"\nsession = {bad:?}\n"
        ));
        assert!(err.contains("projects"), "{bad:?}: {err}");
    }
    let long = "a".repeat(MAX_ADDRESSED_LEN + 1);
    refused(&format!("[serve]\nsessions = [{long:?}]\n"));
}

#[test]
fn project_entries_are_validated() {
    let err = refused("[[serve.projects]]\npath = \"  \"\nsession = \"a\"\n");
    assert!(err.contains("empty path"), "{err}");
    let err = refused(
        "[[serve.projects]]\npath = \"/p\"\nsession = \"a\"\n\n\
         [[serve.projects]]\npath = \"/p\"\nsession = \"b\"\n",
    );
    assert!(err.contains("twice"), "{err}");
}

fn credential(body: &str) -> String {
    format!("[[serve.credential]]\n{body}\n")
}

/// Every malformed credential is refused, naming the problem.
#[test]
fn malformed_credentials_are_refused() {
    let env = format!("token_env = \"{AGENTS_ENV}\"");
    for (body, needle) in [
        (format!("{env}\nsessions = [\"a\"]"), "no name".to_string()),
        (
            format!("name = \"has space\"\n{env}\nsessions = [\"a\"]"),
            "credential name".to_string(),
        ),
        (
            format!("name = \"default\"\n{env}\nsessions = [\"a\"]"),
            "reserved".to_string(),
        ),
        (
            format!("name = \"local\"\n{env}\nsessions = [\"a\"]"),
            "reserved".to_string(),
        ),
        (
            "name = \"a\"\nsessions = [\"a\"]".to_string(),
            "no token_env".to_string(),
        ),
        (
            "name = \"a\"\ntoken_env = \"1BAD\"\nsessions = [\"a\"]".to_string(),
            "not an environment variable name".to_string(),
        ),
        (
            "name = \"a\"\ntoken_env = \"HAS-DASH\"\nsessions = [\"a\"]".to_string(),
            "not an environment variable name".to_string(),
        ),
        (
            "name = \"a\"\ntoken_env = \"\"\nsessions = [\"a\"]".to_string(),
            "not an environment variable name".to_string(),
        ),
        (
            format!(
                "name = \"a\"\ntoken_env = \"{}\"\nsessions = [\"a\"]",
                crate::mcp::AUTH_TOKEN_ENV
            ),
            "legacy".to_string(),
        ),
        (
            format!("name = \"a\"\n{env}"),
            "covers no session".to_string(),
        ),
        (
            format!("name = \"a\"\n{env}\nsessions = []"),
            "covers no session".to_string(),
        ),
        (
            format!("name = \"a\"\n{env}\nsessions = [\"a/b\"]"),
            "cannot be addressed".to_string(),
        ),
        (
            format!("name = \"a\"\n{env}\nsessions = [\"a\", \"a\"]"),
            "twice".to_string(),
        ),
        (
            format!("name = \"a\"\n{env}\nsession_prefix = \".x\""),
            "session_prefix".to_string(),
        ),
        (
            format!("name = \"a\"\n{env}\nsession_prefix = \"\""),
            "session_prefix".to_string(),
        ),
    ] {
        let err = refused(&credential(&body));
        assert!(
            err.contains(&needle),
            "{body:?} must name {needle:?}: {err}"
        );
    }

    let err = refused(&format!(
        "{}{}",
        credential(&format!("name = \"a\"\n{env}\nsessions = [\"x\"]")),
        credential(&format!(
            "name = \"a\"\ntoken_env = \"{APP_ENV}\"\nsessions = [\"y\"]"
        ))
    ));
    assert!(err.contains("named \"a\""), "{err}");
    let err = refused(&format!(
        "{}{}",
        credential(&format!("name = \"a\"\n{env}\nsessions = [\"x\"]")),
        credential(&format!("name = \"b\"\n{env}\nsessions = [\"y\"]"))
    ));
    assert!(err.contains("token_env"), "{err}");
}

fn set_fake_tokens(env: &crate::test_util::EnvGuard) {
    env.set(AGENTS_ENV, FAKE_AGENTS);
    env.set(APP_ENV, FAKE_APP);
    env.set(OPERATOR_ENV, FAKE_OPERATOR);
}

/// Tokens come from the named variables at runtime, and the resolved grants
/// carry the configured scope and flags.
#[test]
fn credentials_resolve_from_their_environment_variables() {
    let env = crate::test_util::env_lock();
    set_fake_tokens(&env);
    let f = parse(&full_toml()).expect("parse");
    let creds = f.serve.resolve_credentials().expect("resolve");
    assert_eq!(creds.len(), 3);
    let hosted = f.serve.hosted_sessions();

    let agents = &creds[0];
    assert_eq!(agents.grant.name(), "agents");
    assert_eq!(agents.token, SecretToken::new(FAKE_AGENTS).unwrap());
    agents
        .grant
        .authorize(&id("lambo"), SessionNeed::Use, &hosted)
        .expect("agents may use lambo");
    assert_eq!(
        agents
            .grant
            .authorize(&id("dc-u-1"), SessionNeed::Use, &hosted)
            .unwrap_err()
            .reason(),
        RefusalReason::OutOfScope
    );
    assert_eq!(
        agents
            .grant
            .authorize(&id("lambo"), SessionNeed::Erase, &hosted)
            .unwrap_err()
            .reason(),
        RefusalReason::MissingCapability
    );

    let app = &creds[1];
    assert_eq!(app.token, SecretToken::new(FAKE_APP).unwrap());
    app.grant
        .authorize(&id("dc-u-42"), SessionNeed::Create, &hosted)
        .expect("app may create under its prefix");
    app.grant
        .authorize(&id("dc-u-42"), SessionNeed::Erase, &hosted)
        .expect("app may erase under its prefix");
    assert!(app
        .grant
        .authorize(&id("dc-u-42"), SessionNeed::Admin, &hosted)
        .is_err());

    let operator = &creds[2];
    for s in ["lambo", "general", "dc-u-7"] {
        operator
            .grant
            .authorize(&id(s), SessionNeed::Admin, &hosted)
            .unwrap_or_else(|e| panic!("operator over {s}: {e}"));
    }
    assert!(
        operator
            .grant
            .authorize(&id("not-hosted"), SessionNeed::Use, &hosted)
            .is_err(),
        "\"*\" means hosted sessions, not every possible id"
    );
}

/// Credentials are never printed: neither the resolved set's `Debug` nor any
/// resolve error carries a token value.
#[test]
fn resolved_credentials_and_resolve_errors_never_carry_a_token() {
    let env = crate::test_util::env_lock();
    set_fake_tokens(&env);
    let f = parse(&full_toml()).expect("parse");
    let creds = f.serve.resolve_credentials().expect("resolve");
    let shown = format!("{creds:?}");
    for fake in [FAKE_AGENTS, FAKE_APP, FAKE_OPERATOR] {
        assert!(!shown.contains(fake), "a token leaked into Debug: {shown}");
    }
    assert!(!format!("{f:?}").contains(FAKE_AGENTS));

    // Two variables holding one token: refused, naming both credentials and
    // neither value.
    env.set(APP_ENV, FAKE_AGENTS);
    let err = f.serve.resolve_credentials().unwrap_err().to_string();
    assert!(
        err.contains("\"agents\"") && err.contains("\"dresscode\""),
        "{err}"
    );
    assert!(!err.contains(FAKE_AGENTS), "{err}");
}

/// Unset, empty, whitespace-only and non-UTF-8 variables all fail closed and
/// name the variable.
#[test]
fn a_missing_or_empty_token_variable_fails_closed() {
    let env = crate::test_util::env_lock();
    set_fake_tokens(&env);
    let f = parse(&full_toml()).expect("parse");

    env.remove(APP_ENV);
    let err = f.serve.resolve_credentials().unwrap_err().to_string();
    assert!(err.contains(APP_ENV) && err.contains("not set"), "{err}");

    for blank in ["", "   ", "\t"] {
        env.set(APP_ENV, blank);
        let err = f.serve.resolve_credentials().unwrap_err().to_string();
        assert!(
            err.contains(APP_ENV) && err.contains("empty"),
            "{blank:?}: {err}"
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let err = f
            .serve
            .resolve_credentials_with(|name| {
                (name == AGENTS_ENV).then(|| OsString::from_vec(vec![0x66, 0xff, 0x66]))
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains(AGENTS_ENV) && err.contains("UTF-8"), "{err}");
    }
}

/// A populated `[serve]` round-trips through TOML; an empty one is not
/// written at all, so a serialized file without it stays readable by a
/// binary that predates `[serve]`.
#[test]
fn serve_round_trips_and_an_empty_table_is_not_serialized() {
    let f = parse(&full_toml()).expect("parse");
    let text = toml::to_string(&f).expect("serialize");
    let back = parse(&text).expect("reparse");
    assert_eq!(back.serve, f.serve);

    let plain = toml::to_string(&LamboFile::default()).expect("serialize");
    assert!(!plain.contains("serve"), "{plain}");
}

/// No behaviour change: a file with a full `[serve]` table resolves exactly
/// what the same file without it resolves.
#[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
#[test]
fn a_serve_table_changes_nothing_the_backends_resolve() {
    let base = "[store]\nkind = \"memory\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n";
    let with = format!(
        "{base}{}",
        full_toml().replace("[store]\nkind = \"memory\"\n", "")
    );
    let a = crate::resolve_backends(parse(base).expect("base")).expect("resolve base");
    let b = crate::resolve_backends(parse(&with).expect("with")).expect("resolve with");
    assert_eq!(a.embedding, b.embedding);
    assert_eq!(a.config, b.config);
    assert_eq!(a.store_cfg, b.store_cfg);
    assert_eq!(a.embedder_cfg, b.embedder_cfg);
}

/// The commented `[serve]` block in `lambo.example.toml`, uncommented, is a
/// valid file: the documented example cannot drift from what parses.
#[test]
fn the_example_files_serve_block_parses_when_uncommented() {
    let raw = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/lambo.example.toml"));
    let begin = raw
        .find("# --- [serve] example begins ---")
        .expect("begin marker");
    let end = raw
        .find("# --- [serve] example ends ---")
        .expect("end marker");
    let block: String = raw[begin..end]
        .lines()
        .skip(1)
        .map(|l| l.strip_prefix("# ").unwrap_or(l.trim_start_matches('#')))
        .collect::<Vec<_>>()
        .join("\n");
    let f = parse(&block).unwrap_or_else(|e| panic!("example [serve] must parse: {e}"));
    assert_eq!(f.serve.sessions.len(), 3);
    assert_eq!(f.serve.credentials.len(), 3);
    assert_eq!(f.serve.projects.len(), 1);
    // And the shipped example, commented, leaves `[serve]` empty.
    assert!(parse(raw).expect("example").serve.is_empty());
}

/// Values an operator might paste into `token_env` when they mean the token
/// itself. All fake; each carries the `xyzzy` marker so a leak is easy to
/// spot. The known-prefix ones are split with `concat!` so the source never
/// holds a contiguous token-shaped literal.
const TOKEN_SHAPED_TOKEN_ENVS: [&str; 6] = [
    "tok-xyzzy-not-a-real-value",
    concat!("gh", "p_", "xyzzyNotARealValue0000"),
    "xyzzy_fake_lower_case",
    "XYZZY0FAKE1NOT2REAL3",
    concat!("AK", "IA", "XYZZY000FAKE0000"),
    concat!("sk", "-", "xyzzy-fake"),
];

/// #32 PR 1 review M1: a token pasted into `token_env` never reaches an error
/// message, on any of the paths that name the variable: the name check, the
/// duplicate check, and the three resolve failures (not set, not UTF-8,
/// empty). It is refused at parse time, so it cannot get as far as PR 5's
/// resolve call.
#[test]
fn a_token_pasted_into_token_env_is_never_echoed() {
    for shaped in TOKEN_SHAPED_TOKEN_ENVS {
        let one = credential(&format!(
            "name = \"a\"\ntoken_env = \"{shaped}\"\nsessions = [\"a\"]"
        ));
        let err = refused(&one);
        assert!(
            err.contains("token_env") && err.contains("value not shown"),
            "{err}"
        );
        assert!(
            !err.contains("xyzzy") && !err.contains("XYZZY"),
            "leaked: {err}"
        );

        let two = format!(
            "{one}{}",
            credential(&format!(
                "name = \"b\"\ntoken_env = \"{shaped}\"\nsessions = [\"b\"]"
            ))
        );
        let err = refused(&two);
        assert!(
            !err.contains("xyzzy") && !err.contains("XYZZY"),
            "leaked: {err}"
        );

        // The resolve paths, on a table that skipped `from_toml_str`.
        let raw: LamboFile = toml::from_str(&one).expect("raw parse");
        #[cfg(unix)]
        let not_utf8 = {
            use std::os::unix::ffi::OsStringExt;
            Some(OsString::from_vec(vec![0x66, 0xff, 0x66]))
        };
        #[cfg(not(unix))]
        let not_utf8 = Some(OsString::from(""));
        for value in [
            None,
            not_utf8,
            Some(OsString::from("")),
            Some(OsString::from(" ")),
        ] {
            let err = raw
                .serve
                .resolve_credentials_with(|_| value.clone())
                .unwrap_err()
                .to_string();
            assert!(
                !err.contains("xyzzy") && !err.contains("XYZZY"),
                "leaked: {err}"
            );
        }
    }
}

/// The name rule is the conventional one: upper case, digits and `_`, not
/// starting with a digit, at most 64 bytes. A lower-case name is refused (it
/// is far more likely a pasted token than a real variable), without echoing.
#[test]
fn token_env_must_be_a_conventional_variable_name() {
    for good in ["A", "_A", "LAMBO_AGENTS_TOKEN", "TOKEN_2"] {
        parse(&credential(&format!(
            "name = \"a\"\ntoken_env = \"{good}\"\nsessions = [\"a\"]"
        )))
        .unwrap_or_else(|e| panic!("{good}: {e}"));
    }
    let long = format!("A{}", "_".repeat(64));
    for bad in [
        "lower_case",
        "Mixed_Case",
        "1BAD",
        "HAS-DASH",
        "",
        long.as_str(),
    ] {
        let err = refused(&credential(&format!(
            "name = \"a\"\ntoken_env = \"{bad}\"\nsessions = [\"a\"]"
        )));
        assert!(
            err.contains("not an environment variable name") && err.contains("value not shown"),
            "{bad:?}: {err}"
        );
        if !bad.is_empty() {
            assert!(!err.contains(bad), "{bad:?} echoed: {err}");
        }
    }
}

/// #32 PR 4: over HTTP `sessions`, `default_session` and `max_attached` are
/// enforced, so a table with only those raises no notice; every key still
/// parsed but not enforced is named, each its own entry (never its value).
#[test]
fn only_unenforced_keys_raise_the_notice() {
    let enforced =
        parse("[serve]\nsessions = [\"a\", \"b\"]\ndefault_session = \"b\"\nmax_attached = 4\n")
            .expect("parse");
    assert!(enforced.serve.unenforced_keys(false).is_empty());
    // A stdio serve does not use default_session yet (#32 PR 8).
    assert_eq!(
        enforced.serve.unenforced_keys(true),
        vec!["default_session"]
    );

    let rest = parse(
        "[serve]\nsessions = [\"a\"]\nattach_concurrency = 1\nidle_detach_secs = 60\n\
         per_session_rps = 5\n\n[[serve.projects]]\npath = \"/p\"\nsession = \"a\"\n\n\
         [[serve.credential]]\nname = \"agents\"\ntoken_env = \"LAMBO_T32_KEYS\"\nsessions = [\"a\"]\n",
    )
    .expect("parse");
    assert_eq!(
        rest.serve.unenforced_keys(false),
        vec![
            "[[serve.credential]]",
            "[[serve.projects]]",
            "attach_concurrency",
            "idle_detach_secs",
            "per_session_rps",
        ]
    );
}

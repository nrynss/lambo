//! #32 PR 5: the serve's credential set (`authority`), unit-level. Which
//! credentials exist for which configuration, what the startup refuses, and
//! the default session's authorization. The wire behaviour (byte-identical
//! 404s, zero store calls) is in `registry::authority`.

use super::*;
use crate::config::ServeCredential;
use crate::surface::session::{
    parse_addressed, RefusalReason, SessionCapabilities, SessionGrant, SessionNeed, SessionPrefix,
    SessionScope, LEGACY_CREDENTIAL_NAME, LOCAL_CREDENTIAL_NAME,
};

/// A fake token, built at runtime so no token-shaped literal sits in the
/// source.
fn fake(label: &str) -> SecretToken {
    SecretToken::new(["fake", label, "pr5", "value"].join("-")).expect("non-empty")
}

fn bearer(label: &str) -> String {
    format!("Bearer {}", ["fake", label, "pr5", "value"].join("-"))
}

fn credential(name: &str, sessions: &[&str], prefix: Option<&str>) -> ServeCredential {
    ServeCredential {
        grant: SessionGrant::new(
            name,
            SessionScope::new(
                sessions
                    .iter()
                    .map(|s| parse_addressed(s).expect("addressable")),
                false,
                prefix.map(|p| SessionPrefix::new(p).expect("a prefix")),
            ),
            SessionCapabilities::default(),
        ),
        token: fake(name),
    }
}

/// An HTTP serve pinning `sessions` (the first the default), on `bind`.
fn http_opts(sessions: &[&str], bind: &str) -> ServeOptions {
    let mut opts = ServeOptions::new(sessions[0], "agent-test");
    opts.sessions = sessions.iter().map(|s| s.to_string()).collect();
    opts.transport = Transport::Http;
    opts.bind = bind.parse().expect("an address");
    opts
}

/// Nothing configured on loopback: the implicit `local` credential answers
/// every request without a header, over the pinned sessions only, with no
/// capability (decision 5: wire erase always needs a configured credential).
#[test]
fn an_unconfigured_loopback_serve_has_the_implicit_local_credential() {
    for bind in ["127.0.0.1", "::1"] {
        let authority = authority_for(&http_opts(&["pin-a", "pin-b"], bind));
        assert!(!authority.requires_bearer(), "{bind}");
        let grant = authority.authenticate(None).expect("implicit");
        assert_eq!(grant.name(), LOCAL_CREDENTIAL_NAME);
        assert_eq!(grant.capabilities(), SessionCapabilities::default());
        for pinned in ["pin-a", "pin-b"] {
            authority
                .authorize(&grant, pinned, SessionNeed::Use)
                .expect(pinned);
        }
        assert_eq!(
            authority
                .authorize(&grant, "pin-c", SessionNeed::Use)
                .expect_err("not pinned")
                .reason(),
            RefusalReason::OutOfScope
        );
        for need in [SessionNeed::Create, SessionNeed::Erase, SessionNeed::Admin] {
            assert!(
                authority.authorize(&grant, "pin-a", need).is_err(),
                "{need:?}"
            );
        }
    }
}

/// The implicit `local` exists only while nothing is configured: a legacy
/// token or any `[[serve.credential]]` removes it, so a request without a
/// header is refused (401) even on loopback.
#[test]
fn any_configured_credential_removes_the_implicit_local_one() {
    let mut legacy = http_opts(&["pin-a"], "127.0.0.1");
    legacy.auth_token = Some(fake("legacy"));
    let mut configured = http_opts(&["pin-a"], "127.0.0.1");
    configured.credentials = vec![credential("agents", &["pin-a"], None)];
    for opts in [legacy, configured] {
        let authority = authority_for(&opts);
        assert!(authority.requires_bearer());
        assert!(authority.authenticate(None).is_none());
        assert!(authority.authenticate(Some("Bearer anything")).is_none());
    }
}

/// The legacy token is the credential `default`: every pinned session, no
/// capability. Beside configured credentials each token resolves to its own
/// grant.
#[test]
fn the_legacy_token_is_the_default_credential_beside_the_configured_ones() {
    let mut opts = http_opts(&["pin-a", "pin-b"], "127.0.0.1");
    opts.auth_token = Some(fake("legacy"));
    opts.credentials = vec![
        credential("agents", &["pin-a"], None),
        credential("app", &[], Some("app-u-")),
    ];
    let authority = authority_for(&opts);
    assert_eq!(
        authority.credential_names(),
        [LEGACY_CREDENTIAL_NAME, "agents", "app"]
    );

    let default = authority
        .authenticate(Some(&bearer("legacy")))
        .expect("the legacy token");
    assert_eq!(default.name(), LEGACY_CREDENTIAL_NAME);
    assert_eq!(default.capabilities(), SessionCapabilities::default());
    for pinned in ["pin-a", "pin-b"] {
        authority
            .authorize(&default, pinned, SessionNeed::Use)
            .expect(pinned);
    }
    // Not "*": a name inside a configured prefix is outside `default`.
    assert!(authority
        .authorize(&default, "app-u-1", SessionNeed::Use)
        .is_err());

    let agents = authority
        .authenticate(Some(&bearer("agents")))
        .expect("agents");
    assert_eq!(agents.name(), "agents");
    authority
        .authorize(&agents, "pin-a", SessionNeed::Use)
        .expect("in scope");
    assert!(authority
        .authorize(&agents, "pin-b", SessionNeed::Use)
        .is_err());

    let app = authority.authenticate(Some(&bearer("app"))).expect("app");
    authority
        .authorize(&app, "app-u-1", SessionNeed::Use)
        .expect("in the prefix");
    assert!(authority
        .authorize(&app, "pin-a", SessionNeed::Use)
        .is_err());
}

/// #32 PR 1 review: a configured token equal to the legacy one is refused at
/// startup, naming the credential and never either value. So are a reserved
/// name, a repeated name and a repeated token (a library caller may build
/// `ServeOptions` without the `[serve]` parser's checks).
#[test]
fn the_startup_refuses_ambiguous_credentials_without_quoting_a_token() {
    let mut shared = credential("agents", &["pin-a"], None);
    shared.token = fake("legacy");
    let err = check_serve_credentials(Some(&fake("legacy")), &[shared])
        .expect_err("the legacy token reused")
        .to_string();
    assert!(
        err.contains("\"agents\"") && err.contains("LAMBO_AUTH_TOKEN"),
        "{err}"
    );

    for reserved in [LEGACY_CREDENTIAL_NAME, LOCAL_CREDENTIAL_NAME] {
        let err = check_serve_credentials(None, &[credential(reserved, &["pin-a"], None)])
            .expect_err(reserved)
            .to_string();
        assert!(err.contains("reserved"), "{err}");
    }

    let err = check_serve_credentials(
        None,
        &[
            credential("twin", &["pin-a"], None),
            ServeCredential {
                token: fake("other"),
                ..credential("twin", &["pin-b"], None)
            },
        ],
    )
    .expect_err("a repeated name")
    .to_string();
    assert!(err.contains("\"twin\""), "{err}");

    let err = check_serve_credentials(
        None,
        &[
            credential("one", &["pin-a"], None),
            ServeCredential {
                token: fake("one"),
                ..credential("two", &["pin-b"], None)
            },
        ],
    )
    .expect_err("a repeated token")
    .to_string();
    assert!(err.contains("same token"), "{err}");

    let secret = ["fake", "legacy", "pr5", "value"].join("-");
    for label in ["legacy", "one"] {
        let secret_of = ["fake", label, "pr5", "value"].join("-");
        assert!(!err.contains(&secret_of), "a token leaked: {err}");
    }
    assert!(!err.contains(&secret), "a token leaked: {err}");

    // And `serve_authority` runs the same check.
    let mut opts = http_opts(&["pin-a"], "127.0.0.1");
    opts.auth_token = Some(fake("legacy"));
    let mut reused = credential("agents", &["pin-a"], None);
    reused.token = fake("legacy");
    opts.credentials = vec![reused];
    assert!(serve_authority(&opts).is_err());

    check_serve_credentials(
        Some(&fake("legacy")),
        &[
            credential("a", &["pin-a"], None),
            credential("b", &["pin-b"], None),
        ],
    )
    .expect("distinct names and tokens");
}

/// The bind rule counts a configured credential as a credential: a
/// non-loopback serve with only `[[serve.credential]]` starts, and one with
/// none at all is still refused.
#[test]
fn a_configured_credential_satisfies_the_non_loopback_bind_rule() {
    let mut opts = http_opts(&["pin-a"], "203.0.113.7");
    assert!(any_credential(&opts).is_none());
    let err = authorize_bind(opts.transport, opts.bind, any_credential(&opts))
        .expect_err("no credential off loopback")
        .to_string();
    assert!(err.contains("[[serve.credential]]"), "{err}");
    assert!(
        serve_authority(&opts).is_err(),
        "never implicit off loopback"
    );

    opts.credentials = vec![credential("agents", &["pin-a"], None)];
    authorize_bind(opts.transport, opts.bind, any_credential(&opts))
        .expect("a configured credential satisfies the rule");
    assert!(authority_for(&opts).requires_bearer());
}

/// `/mcp`'s authorization: an addressable default is authorized as
/// `/mcp/s/{default}` would be; a default only the loose `--session` rule
/// allows is reachable by a scope over every pinned session alone (`"*"`,
/// `default`, `local`), since no exact name or prefix can spell it.
#[test]
fn the_default_session_is_authorized_like_its_addressed_route() {
    let mut opts = http_opts(&["pin-a", "pin-b"], "127.0.0.1");
    opts.auth_token = Some(fake("legacy"));
    opts.credentials = vec![credential("only-b", &["pin-b"], None)];
    let authority = authority_for(&opts);
    let default = authority.authenticate(Some(&bearer("legacy"))).unwrap();
    let only_b = authority.authenticate(Some(&bearer("only-b"))).unwrap();
    authorize_default(&authority, &default, "pin-a").expect("default covers pin-a");
    assert_eq!(
        authorize_default(&authority, &only_b, "pin-a")
            .expect_err("pin-a is outside only-b")
            .reason(),
        RefusalReason::OutOfScope
    );

    // One session under the loose rule (a `/` is outside the strict charset).
    let loose = "team/notes";
    let mut opts = http_opts(&[loose], "127.0.0.1");
    opts.auth_token = Some(fake("legacy"));
    opts.credentials = vec![
        credential("named", &["pin-a"], None),
        ServeCredential {
            grant: SessionGrant::new(
                "operator",
                SessionScope::new([], true, None),
                SessionCapabilities::default(),
            ),
            token: fake("operator"),
        },
    ];
    let authority = authority_for(&opts);
    for (label, allowed) in [("legacy", true), ("operator", true), ("named", false)] {
        let grant = authority.authenticate(Some(&bearer(label))).unwrap();
        assert_eq!(
            authorize_default(&authority, &grant, loose).is_ok(),
            allowed,
            "{label}"
        );
    }
    let local = authority_for(&http_opts(&[loose], "127.0.0.1"));
    let grant = local.authenticate(None).unwrap();
    authorize_default(&local, &grant, loose).expect("local covers the pinned session");
}

/// #32 PR 5 review I3 and I5, as PR 6 leaves them: the startup warnings. A
/// legacy token beside configured credentials is named (it keeps
/// `default`); a credential without `create` whose scope reaches past the
/// pinned sessions is named with that reach, since it attaches such a
/// session only once it exists. One with `create` reaches them on demand and
/// is not named. No warning names a token, and a legacy-only, a
/// credentials-only-and-pinned or a stdio serve gets none.
#[test]
fn startup_warns_of_a_leftover_legacy_token_and_unreachable_names() {
    let mut opts = http_opts(&["pin-a", "pin-b"], "127.0.0.1");
    opts.auth_token = Some(fake("legacy"));
    assert!(startup_warnings(&opts).is_empty(), "legacy alone");

    let mut maker = credential("maker", &["later-3"], Some("dc-m-"));
    maker.grant = SessionGrant::new(
        "maker",
        maker.grant.scope().clone(),
        SessionCapabilities {
            create: true,
            ..SessionCapabilities::default()
        },
    );
    opts.credentials = vec![
        credential("fine", &["pin-a"], None),
        credential("partly", &["pin-b", "later-1"], None),
        credential("nowhere", &["later-2"], None),
        credential("prefixed", &[], Some("dc-u-")),
        maker,
    ];
    let warnings = startup_warnings(&opts);
    let all = warnings.join("\n");
    assert!(
        warnings[0].contains("LAMBO_AUTH_TOKEN") && warnings[0].contains("\"default\""),
        "{all}"
    );
    assert!(
        all.contains(
            "\"partly\" reaches sessions this serve does not pin (later-1) without create"
        ),
        "{all}"
    );
    assert!(
        all.contains("\"nowhere\" reaches sessions this serve does not pin (later-2)"),
        "{all}"
    );
    assert!(
        all.contains("\"prefixed\" reaches sessions this serve does not pin (prefix \"dc-u-\")"),
        "{all}"
    );
    assert!(!all.contains("\"fine\""), "{all}");
    assert!(!all.contains("\"maker\""), "create reaches them: {all}");
    assert!(!all.contains("fake-"), "a token leaked: {all}");

    opts.auth_token = None;
    opts.credentials.retain(|c| c.grant.name() == "fine");
    assert!(startup_warnings(&opts).is_empty());
    opts.transport = Transport::Stdio;
    opts.auth_token = Some(fake("legacy"));
    opts.credentials = vec![credential("nowhere", &["later-2"], None)];
    assert!(
        startup_warnings(&opts).is_empty(),
        "stdio has no credentials"
    );
}

/// #32 PR 6: a serve attaches on demand exactly when a configured
/// credential reaches past its pinned sessions (an unpinned name or a
/// prefix), over HTTP. The legacy and implicit credentials cover the pinned
/// sessions only, so the dogfood rig's shape never attaches on demand.
#[test]
fn a_scope_past_the_pinned_sessions_attaches_on_demand() {
    let mut opts = http_opts(&["pin-a"], "127.0.0.1");
    assert!(!reaches_past_pinned(&opts), "implicit local");
    opts.auth_token = Some(fake("legacy"));
    assert!(!reaches_past_pinned(&opts), "the rig's legacy token");
    opts.credentials = vec![credential("pinned-only", &["pin-a"], None)];
    assert!(!reaches_past_pinned(&opts), "names only pinned sessions");
    opts.credentials
        .push(credential("unpinned", &["later-1"], None));
    assert!(reaches_past_pinned(&opts), "an unpinned name");
    opts.credentials = vec![credential("prefixed", &[], Some("dc-u-"))];
    assert!(reaches_past_pinned(&opts), "a prefix");
    opts.transport = Transport::Stdio;
    assert!(!reaches_past_pinned(&opts), "stdio authenticates nobody");
}

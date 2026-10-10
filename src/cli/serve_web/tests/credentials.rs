//! Per-credential read scope and the opt-in listing (#4 PR 3, design 4.1,
//! 4.2 and 6.2): one test per acceptance item of the PR 3 row, over the
//! wire where the item is about a response.
//!
//! Tokens are built at runtime from fragments and are never printed: no
//! assertion message carries one.

use std::collections::HashMap;
use std::ffi::OsString;

use super::routing::{refused_ids, two_sessions, wire, Recording, METHODS};
use super::*;
use crate::config::WebConfig;
use crate::surface::session::{SessionCapabilities, SessionGrant, SessionScope};
use crate::LamboFile;

/// The allowlist every test here serves: the default first.
const SERVED: &[&str] = &["t4-a", "t4-b", "t4c-p-1"];

/// A session with data in the store, under the prefix grant's prefix, that
/// is **not** on the allowlist: nothing may ever load or name it.
const UNSERVED: &str = "t4c-p-2";

/// What each session holds, to catch a leak by content as well as by name.
fn content_of(session: &str) -> &'static str {
    match session {
        "t4-a" => "user schema",
        "t4-b" => "billing ledger",
        "t4c-p-1" => "prefix pantry",
        UNSERVED => "secret orchard",
        _ => unreachable!("{session}"),
    }
}

/// A token for credential `name`, built at runtime.
fn tok(name: &str) -> String {
    ["t4c", "-", name, "-", "k", "e", "y"].concat()
}

const VIEWERS_ENV: &str = "LAMBO_TEST_4C_VIEWERS";
const PREFIX_ENV: &str = "LAMBO_TEST_4C_PREFIX";
const STAR_ENV: &str = "LAMBO_TEST_4C_STAR";
const AGENTS_ENV: &str = "LAMBO_TEST_4C_AGENTS";

/// The `lambo.toml` of these tests: three web credentials (one exact name,
/// one prefix, one `"*"`) and one serve credential with every capability.
fn config(list: bool, inherit: bool) -> String {
    format!(
        "[serve]\nsessions = [\"t4-b\"]\n\n[[serve.credential]]\nname = \"agents\"\n\
         token_env = \"{AGENTS_ENV}\"\nsessions = [\"t4-b\"]\ncreate = true\nerase = true\n\
         admin = true\n\n\
         [web]\nsessions = [\"t4-a\", \"t4-b\", \"t4c-p-1\"]\nlist_sessions = {list}\n\
         inherit_serve_credentials = {inherit}\n\n\
         [[web.credential]]\nname = \"viewers\"\ntoken_env = \"{VIEWERS_ENV}\"\n\
         sessions = [\"t4-a\"]\n\n\
         [[web.credential]]\nname = \"pre\"\ntoken_env = \"{PREFIX_ENV}\"\n\
         session_prefix = \"t4c-p-\"\n\n\
         [[web.credential]]\nname = \"star\"\ntoken_env = \"{STAR_ENV}\"\nsessions = [\"*\"]\n"
    )
}

/// The environment the credentials read: each variable holds its
/// credential's token.
fn env() -> HashMap<String, OsString> {
    [
        (VIEWERS_ENV, "viewers"),
        (PREFIX_ENV, "pre"),
        (STAR_ENV, "star"),
        (AGENTS_ENV, "agents"),
    ]
    .into_iter()
    .map(|(var, name)| (var.to_string(), OsString::from(tok(name))))
    .collect()
}

/// Parse `toml` and resolve its credentials from `vars`, as the CLI does.
fn resolve(
    toml: &str,
    vars: &HashMap<String, OsString>,
) -> Result<(WebConfig, Vec<WebCredential>), CliError> {
    let file = LamboFile::from_toml_str(toml).map_err(|e| CliError::Usage(e.to_string()))?;
    let creds = super::super::auth::resolve_web_credentials_with(&file.web, &file.serve, |n| {
        vars.get(n).cloned()
    })?;
    Ok((file.web, creds))
}

/// One concept in `session`, so its view is not empty.
async fn derive_in(store: &Arc<MemoryStore>, session: &str) {
    derive_text(store, session, content_of(session)).await;
}

/// One concept holding `content` in `session`.
async fn derive_text(store: &Arc<MemoryStore>, session: &str, content: &str) {
    crate::cli::derive::run(
        backends_on(store.clone()),
        crate::cli::derive::Args {
            session: session.into(),
            agent: "agent-4c".into(),
            content: content.into(),
            kind: ConceptKind::Entity,
            parent_of: vec![],
            concept: vec![],
        },
    )
    .await
    .expect("derive");
}

/// `t4-a` and `t4-b` from [`two_sessions`], plus the prefix pair.
async fn four_sessions() -> Arc<MemoryStore> {
    let store = two_sessions().await;
    derive_in(&store, "t4c-p-1").await;
    derive_in(&store, UNSERVED).await;
    store
}

/// A portal over [`SERVED`] with `credentials`, an optional legacy token,
/// and `web`'s listing switch, on a recording store.
async fn portal(
    legacy: Option<AuthToken>,
    credentials: Vec<WebCredential>,
    web: &WebConfig,
) -> (SocketAddr, tokio::task::JoinHandle<()>, Recording) {
    let recording = Recording::new(four_sessions().await);
    let ids: Vec<SessionId> = SERVED.iter().map(|s| SessionId::new(*s)).collect();
    let exposed = legacy.is_some() || !credentials.is_empty();
    let state = Arc::new(AppState::new(
        ids[0].clone(),
        ids,
        backends_with_store(Box::new(recording.clone())),
        exposed,
        legacy,
        credentials,
        &[],
        web,
    ));
    let (addr, handle) = spawn(state).await;
    (addr, handle, recording)
}

/// [`portal`] with every web credential of [`config`].
async fn credentialed(list: bool) -> (SocketAddr, tokio::task::JoinHandle<()>, Recording) {
    let (web, creds) = resolve(&config(list, false), &env()).expect("resolve");
    portal(None, creds, &web).await
}

/// `method path` presenting `token` (none: no header), `Host` loopback.
async fn as_caller(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> HttpResponse {
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    send_raw(
        addr,
        &format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Accept: application/json\r\n\
             Connection: close\r\n\r\n"
        ),
    )
    .await
}

/// Which served sessions each credential of [`config`] reads.
fn scope_of(credential: &str) -> &'static [&'static str] {
    match credential {
        "viewers" => &["t4-a"],
        "pre" => &["t4c-p-1"],
        "star" | "default" | "local" => SERVED,
        "agents" => &["t4-b"],
        _ => unreachable!("{credential}"),
    }
}

/// The data routes under a session, with query strings that name no
/// session.
const DATA: &[&str] = &[
    "/api/session",
    "/api/stats",
    "/api/events",
    "/api/pulse",
    "/api/graph",
    "/api/inspect?focus=nothing-here",
    "/api/recall?q=schema",
];

// ---- parse and resolution -------------------------------------------

/// Acceptance: duplicate tokens across credentials and `LAMBO_AUTH_TOKEN`
/// reserved (beside the parse refusals in `config::web::tests`). Every
/// refusal names the credential, never a token, and is a usage error
/// (exit 2).
#[test]
fn resolution_fails_closed_naming_the_credential_never_the_token() {
    let mut vars = env();
    // Unset.
    vars.remove(PREFIX_ENV);
    let err = resolve(&config(false, false), &vars).expect_err("unset");
    let msg = err.to_string();
    assert!(msg.contains("\"pre\"") && msg.contains("not set"), "{msg}");
    assert_eq!(err.exit_code(), 2);
    // Two web credentials with one token.
    let mut vars = env();
    vars.insert(PREFIX_ENV.into(), OsString::from(tok("viewers")));
    let msg = resolve(&config(false, false), &vars)
        .expect_err("same token")
        .to_string();
    assert!(
        msg.contains("same token") && msg.contains("\"pre\""),
        "{msg}"
    );
    // An unpresentable token.
    let mut vars = env();
    vars.insert(STAR_ENV.into(), OsString::from(format!(" {}", tok("star"))));
    let msg = resolve(&config(false, false), &vars)
        .expect_err("whitespace")
        .to_string();
    assert!(
        msg.contains("\"star\"") && msg.contains("whitespace"),
        "{msg}"
    );
    assert!(!msg.contains(&tok("star")), "a token leaked");
    // A web credential and an inherited one with one token: the set check.
    let mut vars = env();
    vars.insert(AGENTS_ENV.into(), OsString::from(tok("star")));
    let (_, creds) = resolve(&config(false, true), &vars).expect("resolves per table");
    let msg = check_web_credentials(None, &creds)
        .expect_err("shared across tables")
        .to_string();
    assert!(
        msg.contains("\"star\"") && msg.contains("\"agents\""),
        "{msg}"
    );
    assert!(!msg.contains(&tok("star")), "a token leaked");
    // A configured token equal to the legacy one.
    let (_, creds) = resolve(&config(false, false), &env()).expect("resolve");
    let legacy = AuthToken::new(tok("viewers")).expect("token");
    let msg = check_web_credentials(Some(&legacy), &creds)
        .expect_err("legacy collision")
        .to_string();
    assert!(
        msg.contains("LAMBO_AUTH_TOKEN") && msg.contains("\"viewers\""),
        "{msg}"
    );
    assert!(!msg.contains(&tok("viewers")), "a token leaked");
    // A library caller naming a credential `default` or `local`.
    for reserved in ["default", "local"] {
        let cred = WebCredential {
            grant: SessionGrant::new(
                reserved,
                SessionScope::new([], true, None),
                SessionCapabilities::default(),
            ),
            token: AuthToken::new(tok("lib")).expect("token"),
        };
        let msg = check_web_credentials(None, &[cred])
            .expect_err("reserved")
            .to_string();
        assert!(msg.contains("reserved"), "{msg}");
    }
    // Debug of a resolved set never shows a token.
    let (_, creds) = resolve(&config(false, true), &env()).expect("resolve");
    let shown = format!("{creds:?}");
    for name in ["viewers", "pre", "star", "agents"] {
        assert!(!shown.contains(&tok(name)), "a token leaked into Debug");
    }
}

/// Acceptance: `inherit_serve_credentials` imports each
/// `[[serve.credential]]` with its scope and drops its capabilities; off,
/// nothing is imported.
#[test]
fn inherit_imports_serve_scopes_and_drops_capabilities() {
    let (_, without) = resolve(&config(false, false), &env()).expect("resolve");
    assert_eq!(
        without.iter().map(|c| c.grant.name()).collect::<Vec<_>>(),
        ["viewers", "pre", "star"]
    );
    let (_, with) = resolve(&config(false, true), &env()).expect("resolve");
    assert_eq!(
        with.iter().map(|c| c.grant.name()).collect::<Vec<_>>(),
        ["viewers", "pre", "star", "agents"],
        "web credentials first, then the imported ones"
    );
    let agents = &with[3];
    assert_eq!(agents.grant.capabilities(), SessionCapabilities::default());
    let names: Vec<_> = agents.grant.scope().names().map(|n| n.as_str()).collect();
    assert_eq!(names, ["t4-b"]);
    // Without the switch, a serve-only file resolves to no web credential
    // and reads none of serve's variables.
    let (_, none) = resolve(
        "[serve]\nsessions = [\"t4-b\"]\n\n[[serve.credential]]\nname = \"agents\"\n\
         token_env = \"LAMBO_TEST_4C_UNSET\"\nsessions = [\"t4-b\"]\n",
        &HashMap::new(),
    )
    .expect("nothing to resolve");
    assert!(none.is_empty());
}

/// Over the wire: the imported credential reads its serve scope, read-only.
#[tokio::test]
async fn an_inherited_credential_reads_its_serve_scope_only() {
    let (web, creds) = resolve(&config(false, true), &env()).expect("resolve");
    let (addr, handle, _) = portal(None, creds, &web).await;
    let agents = tok("agents");
    let ok = as_caller(addr, "GET", "/s/t4-b/api/session", Some(&agents)).await;
    assert_eq!(ok.status, 200);
    let unrouted = wire(&as_caller(addr, "GET", "/no/such/path", Some(&agents)).await);
    let refused = as_caller(addr, "GET", "/s/t4-a/api/session", Some(&agents)).await;
    assert_eq!(wire(&refused), unrouted);
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let r = as_caller(addr, method, "/s/t4-b/api/session", Some(&agents)).await;
        assert_eq!(r.status, 405, "{method}: still read-only");
    }
    handle.abort();
}

/// Review M1: an inherited `"*"` means what it means on serve (the
/// `[serve] sessions` plus every name under a `[[serve.credential]]`
/// prefix), never the portal's allowlist. Serve pins `lambo` and the `app`
/// credential's prefix is `dc-u-`; the portal also serves `hr-private`,
/// which the `agents` token could never reach through MCP, so it must not
/// read, list or count it here either. A web `"*"` still covers the whole
/// allowlist.
#[tokio::test]
async fn an_inherited_star_is_serves_hosted_set_never_the_allowlist() {
    const SERVED_M1: &[&str] = &["lambo", "hr-private", "dc-u-1"];
    const HR: &str = "payroll grievances";
    let toml = format!(
        "[serve]\nsessions = [\"lambo\"]\n\n         [[serve.credential]]\nname = \"agents\"\ntoken_env = \"{AGENTS_ENV}\"\n         sessions = [\"*\"]\n\n         [[serve.credential]]\nname = \"app\"\ntoken_env = \"{PREFIX_ENV}\"\n         session_prefix = \"dc-u-\"\n\n         [web]\nsessions = [\"lambo\", \"hr-private\", \"dc-u-1\"]\nlist_sessions = true\n         inherit_serve_credentials = true\n\n         [[web.credential]]\nname = \"star\"\ntoken_env = \"{STAR_ENV}\"\nsessions = [\"*\"]\n"
    );
    let (web, creds) = resolve(&toml, &env()).expect("resolve");
    assert_eq!(
        creds.iter().map(|c| c.grant.name()).collect::<Vec<_>>(),
        ["star", "agents", "app"]
    );
    let ids: Vec<SessionId> = SERVED_M1.iter().map(|s| SessionId::new(*s)).collect();
    let authority = portal_authority(None, creds.clone(), &ids);
    assert_eq!(
        super::super::auth::credential_reach(&authority, &ids),
        [("star", 3), ("agents", 2), ("app", 1)],
        "agents reaches lambo (pinned) and dc-u-1 (serve's prefix), not hr-private"
    );
    assert!(
        super::super::auth::unserved_names(&authority, &ids).is_empty(),
        "an expanded star names no session the operator did not write"
    );

    let store = four_sessions().await;
    derive_text(&store, "lambo", "agent notes").await;
    derive_text(&store, "hr-private", HR).await;
    derive_text(&store, "dc-u-1", "user pantry").await;
    let recording = Recording::new(store);
    let state = Arc::new(AppState::new(
        ids[0].clone(),
        ids,
        backends_with_store(Box::new(recording.clone())),
        true,
        None,
        creds,
        &[],
        &web,
    ));
    let (addr, handle) = spawn(state).await;
    let agents = tok("agents");
    let unrouted = wire(&as_caller(addr, "GET", "/no/such/path", Some(&agents)).await);
    for ok in ["lambo", "dc-u-1"] {
        let r = as_caller(addr, "GET", &format!("/s/{ok}/api/graph"), Some(&agents)).await;
        assert_eq!(r.status, 200, "agents reads {ok}, as on serve");
    }
    let before = recording.calls();
    for route in DATA {
        let r = as_caller(addr, "GET", &format!("/s/hr-private{route}"), Some(&agents)).await;
        assert_eq!(wire(&r), unrouted, "agents: {route} on hr-private");
    }
    assert_eq!(recording.calls(), before, "the refusal reads no store");
    assert_eq!(listing(addr, Some(&agents)).await, ["lambo"]);
    assert_eq!(listing(addr, Some(&tok("pre"))).await, Vec::<String>::new());
    for path in ["/", "/api/sessions", "/api/graph", "/api/session"] {
        let r = as_caller(addr, "GET", path, Some(&agents)).await;
        assert!(
            !r.body.contains("hr-private") && !r.body.contains(HR),
            "agents: GET {path} names hr-private"
        );
    }
    // A web `"*"` is the allowlist: it reads and lists hr-private.
    let star = tok("star");
    let r = as_caller(addr, "GET", "/s/hr-private/api/graph", Some(&star)).await;
    assert_eq!(r.status, 200);
    assert_eq!(listing(addr, Some(&star)).await, SERVED_M1);
    handle.abort();
}

// ---- who has which credential ---------------------------------------

/// Acceptance: `local` is gone once anything is configured. A
/// `[[web.credential]]` alone (no legacy token) makes every request need a
/// bearer, on loopback too.
#[tokio::test]
async fn local_is_gone_once_a_web_credential_is_configured() {
    let (_, creds) = resolve(&config(false, false), &env()).expect("resolve");
    let ids = [SessionId::new("t4-a")];
    let authority = portal_authority(None, creds.clone(), &ids);
    assert!(authority.requires_bearer());
    assert_eq!(authority.credential_names(), ["viewers", "pre", "star"]);
    let with_legacy = portal_authority(Some(AuthToken::new(tok("d")).expect("t")), creds, &ids);
    assert_eq!(
        with_legacy.credential_names(),
        ["default", "viewers", "pre", "star"],
        "the legacy grant first, beside the configured ones"
    );

    let (addr, handle, recording) = credentialed(false).await;
    let before = recording.calls();
    for path in [
        "/",
        "/app.js",
        "/healthz",
        "/api/session",
        "/s/t4-a/",
        "/no/such",
    ] {
        let r = as_caller(addr, "GET", path, None).await;
        assert_eq!(r.status, 401, "GET {path} without a bearer");
        assert_eq!(
            r.body,
            "unauthorized: this endpoint requires 'Authorization: Bearer <token>'\n"
        );
        let wrong = as_caller(addr, "GET", path, Some(&tok("nobody"))).await;
        assert_eq!(
            wire(&wrong),
            wire(&r),
            "GET {path}: no header and a wrong token"
        );
    }
    assert_eq!(recording.calls(), before, "a 401 makes no store call");
    handle.abort();
}

/// Acceptance (design 4.5, Q18), the Host rule with several credentials:
/// the Host check exists only under the implicit grant. Legacy, web,
/// inherited, or any mix: any Host is accepted. Without any: only loopback
/// names and the allowed hosts.
#[test]
fn the_host_check_applies_only_while_no_credential_exists() {
    let ids = [SessionId::new("t4-a")];
    let (_, web) = resolve(&config(false, false), &env()).expect("resolve");
    let (_, inherited) = resolve(&config(false, true), &env()).expect("resolve");
    let only_inherited: Vec<_> = inherited.into_iter().skip(3).collect();
    let legacy = || Some(AuthToken::new(tok("d")).expect("t"));
    let extra = [AllowedHost::parse("lambo.example.com").expect("host")];
    for (label, authority, checked) in [
        ("nothing", portal_authority(None, vec![], &ids), true),
        ("legacy", portal_authority(legacy(), vec![], &ids), false),
        ("web", portal_authority(None, web.clone(), &ids), false),
        ("legacy+web", portal_authority(legacy(), web, &ids), false),
        (
            "inherited",
            portal_authority(None, only_inherited, &ids),
            false,
        ),
    ] {
        let check = HostCheck::for_authority(&authority, &extra);
        assert_eq!(matches!(check, HostCheck::Only(_)), checked, "{label}");
    }
}

/// The same over the wire: with web credentials only, a foreign `Host`
/// carrying a valid token is served, and without a token it is the 401,
/// never the Host 403.
#[tokio::test]
async fn any_host_is_accepted_once_a_web_credential_exists() {
    let (addr, handle, _) = credentialed(false).await;
    let send = |token: Option<String>| async move {
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        send_raw(
            addr,
            &format!(
                "GET /s/t4-a/api/session HTTP/1.1\r\nHost: rebound.example\r\n{auth}\
                 Connection: close\r\n\r\n"
            ),
        )
        .await
    };
    assert_eq!(send(Some(tok("viewers"))).await.status, 200);
    assert_eq!(send(None).await.status, 401);
    handle.abort();
}

// ---- scope ------------------------------------------------------------

/// Acceptance: each credential reaches only its sessions; every other id
/// (served elsewhere, unserved, malformed, percent-encoded, oversized) is
/// the uniform 404, byte-identical to an unrouted path for that caller, on
/// every method, with zero store calls.
#[tokio::test]
async fn each_credential_reaches_only_its_sessions() {
    let (addr, handle, recording) = credentialed(false).await;
    for cred in ["viewers", "pre", "star"] {
        let token = tok(cred);
        let scope = scope_of(cred);
        for session in SERVED.iter().filter(|s| scope.contains(s)) {
            for route in DATA {
                let path = format!("/s/{session}{route}");
                let r = as_caller(addr, "GET", &path, Some(&token)).await;
                assert_eq!(r.status, 200, "{cred}: GET {path}: {}", r.body);
            }
        }
        let mut refused: Vec<String> = SERVED
            .iter()
            .filter(|s| !scope.contains(s))
            .map(|s| (*s).to_string())
            .collect();
        refused.push(UNSERVED.into());
        refused.extend(refused_ids());
        let before = recording.calls();
        for method in METHODS {
            let unrouted = wire(&as_caller(addr, method, "/no/such/path", Some(&token)).await);
            for id in &refused {
                for rest in [
                    "",
                    "/",
                    "/api/pulse",
                    "/api/recall?q=schema",
                    "/api/sessions",
                ] {
                    let path = format!("/s/{id}{rest}");
                    let r = as_caller(addr, method, &path, Some(&token)).await;
                    assert_eq!(wire(&r), unrouted, "{cred}: {method} {path}");
                }
            }
        }
        assert_eq!(
            recording.calls(),
            before,
            "{cred}: a refused request makes zero store calls"
        );
    }
    handle.abort();
}

/// Acceptance (design 4.2, Q8): a prefix grant reaches only allowlisted ids
/// under its prefix. `t4c-p-2` has data in the store and matches the
/// prefix, but is not served: the uniform 404 and no load, ever.
#[tokio::test]
async fn a_prefix_grant_reaches_only_allowlisted_ids_under_it() {
    let (addr, handle, recording) = credentialed(false).await;
    let token = tok("pre");
    let ok = as_caller(addr, "GET", "/s/t4c-p-1/api/graph", Some(&token)).await;
    assert_eq!(ok.status, 200);
    assert!(ok.body.contains(content_of("t4c-p-1")), "{}", ok.body);
    let unrouted = wire(&as_caller(addr, "GET", "/no/such/path", Some(&token)).await);
    let loads = recording.loads();
    for route in DATA {
        let path = format!("/s/{UNSERVED}{route}");
        let r = as_caller(addr, "GET", &path, Some(&token)).await;
        assert_eq!(wire(&r), unrouted, "GET {path}");
    }
    // The bare prefix is in no prefix scope.
    let bare = as_caller(addr, "GET", "/s/t4c-p-/api/graph", Some(&token)).await;
    assert_eq!(wire(&bare), unrouted);
    assert_eq!(recording.loads(), loads, "no load for an unserved id");
    handle.abort();
}

/// The aliases (design Q10) follow the caller's scope: they serve the
/// default only to a grant that may read it, and are otherwise the uniform
/// 404, as `/s/{default}/` would be.
#[tokio::test]
async fn the_aliases_follow_the_callers_scope() {
    let (addr, handle, _) = credentialed(false).await;
    for (cred, reads_default) in [("viewers", true), ("pre", false), ("star", true)] {
        let token = tok(cred);
        let unrouted = wire(&as_caller(addr, "GET", "/no/such/path", Some(&token)).await);
        for route in DATA {
            let alias = as_caller(addr, "GET", route, Some(&token)).await;
            let scoped = as_caller(addr, "GET", &format!("/s/t4-a{route}"), Some(&token)).await;
            if reads_default {
                assert_eq!(alias.status, 200, "{cred}: GET {route}");
                assert_eq!(scoped.status, 200, "{cred}: GET /s/t4-a{route}");
            } else {
                assert_eq!(wire(&alias), unrouted, "{cred}: GET {route}");
                assert_eq!(wire(&scoped), unrouted, "{cred}: GET /s/t4-a{route}");
            }
        }
        // The page and its assets name no session: every caller gets them.
        for path in ["/", "/app.js", "/app.css", "/healthz"] {
            let r = as_caller(addr, "GET", path, Some(&token)).await;
            assert_eq!(r.status, 200, "{cred}: GET {path}");
        }
    }
    handle.abort();
}

// ---- the listing --------------------------------------------------------

/// Acceptance: `/api/sessions` is absent by default: unrouted, so every
/// method is the uniform 404, the same bytes as any unrouted path, for an
/// authenticated caller and under the implicit grant alike. No store call.
#[tokio::test]
async fn the_listing_is_absent_by_default() {
    let (with_creds, h1, r1) = credentialed(false).await;
    let (implicit, h2, r2) = portal(None, vec![], &WebConfig::default()).await;
    for (addr, token, recording) in [(with_creds, Some(tok("star")), &r1), (implicit, None, &r2)] {
        let before = recording.calls();
        for method in METHODS {
            let unrouted = wire(&as_caller(addr, method, "/no/such/path", token.as_deref()).await);
            for path in ["/api/sessions", "/api/sessions/", "/api/sessions?x=1"] {
                let r = as_caller(addr, method, path, token.as_deref()).await;
                assert_eq!(wire(&r), unrouted, "{method} {path}");
            }
        }
        assert_eq!(recording.calls(), before);
    }
    h1.abort();
    h2.abort();
}

/// The listing of `addr` as `token`: the names, after checking the status
/// and `no-store`.
async fn listing(addr: SocketAddr, token: Option<&str>) -> Vec<String> {
    let r = as_caller(addr, "GET", "/api/sessions", token).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        r.headers
            .to_ascii_lowercase()
            .contains("cache-control: no-store"),
        "{}",
        r.headers
    );
    let v: serde_json::Value = serde_json::from_str(&r.body).expect("json");
    assert_eq!(v.as_object().map(|o| o.len()), Some(1), "names only: {v}");
    v["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|s| s.as_str().expect("name").to_string())
        .collect()
}

/// Acceptance: with `list_sessions` on, `/api/sessions` lists the caller's
/// exact-name scope ∩ the allowlist, in allowlist order: never a prefix
/// expansion, never another credential's names. `"*"`, the legacy
/// `default` and the implicit `local` list every served session. A caller
/// without a bearer gets the 401; under a session it is the uniform 404.
/// No store call.
#[tokio::test]
async fn the_listing_names_only_the_callers_exact_scope() {
    let (web, creds) = resolve(&config(true, true), &env()).expect("resolve");
    let legacy = AuthToken::new(tok("default")).expect("token");
    let (addr, handle, recording) = portal(Some(legacy), creds, &web).await;
    let before = recording.calls();
    for (cred, expected) in [
        ("viewers", vec!["t4-a"]),
        ("pre", vec![]),
        ("star", SERVED.to_vec()),
        ("default", SERVED.to_vec()),
        ("agents", vec!["t4-b"]),
    ] {
        assert_eq!(listing(addr, Some(&tok(cred))).await, expected, "{cred}");
    }
    assert_eq!(
        as_caller(addr, "GET", "/api/sessions", None).await.status,
        401
    );
    let unrouted = wire(&as_caller(addr, "GET", "/no/such/path", Some(&tok("star"))).await);
    let scoped = as_caller(addr, "GET", "/s/t4-a/api/sessions", Some(&tok("star"))).await;
    assert_eq!(
        wire(&scoped),
        unrouted,
        "the listing is never under a session"
    );
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let r = as_caller(addr, method, "/api/sessions", Some(&tok("star"))).await;
        assert_eq!(r.status, 405, "{method}: read-only");
    }
    assert_eq!(recording.calls(), before, "the listing reads no store");
    handle.abort();

    // The implicit grant lists every served session.
    let web = WebConfig {
        list_sessions: true,
        ..Default::default()
    };
    let (addr, handle, _) = portal(None, vec![], &web).await;
    assert_eq!(listing(addr, None).await, SERVED);
    handle.abort();
}

/// Acceptance: a sweep proves no response names a session outside the
/// request's scope, by name or by content. Every caller, every alias and
/// scoped data route for every id (served or not), the page, and the
/// listing.
#[tokio::test]
async fn no_response_names_a_session_outside_the_callers_scope() {
    let (web, creds) = resolve(&config(true, true), &env()).expect("resolve");
    let legacy = AuthToken::new(tok("default")).expect("token");
    let (addr, handle, _) = portal(Some(legacy), creds, &web).await;
    let every: Vec<&str> = SERVED.iter().copied().chain([UNSERVED]).collect();
    let mut paths: Vec<String> = vec!["/".into(), "/api/sessions".into()];
    for route in DATA {
        paths.push((*route).to_string());
        for id in &every {
            paths.push(format!("/s/{id}{route}"));
            paths.push(format!("/s/{id}/"));
        }
    }
    let mut checked = 0;
    for cred in ["viewers", "pre", "star", "default", "agents"] {
        let token = tok(cred);
        let scope = scope_of(cred);
        let outside: Vec<&str> = every
            .iter()
            .copied()
            .filter(|s| !scope.contains(s))
            .collect();
        for path in &paths {
            let r = as_caller(addr, "GET", path, Some(&token)).await;
            for session in &outside {
                assert!(
                    !r.body.contains(session) && !r.body.contains(content_of(session)),
                    "{cred}: GET {path} names {session} ({} bytes)",
                    r.body.len()
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 100, "the sweep ran ({checked} responses)");
    handle.abort();
}

// ---- startup and source -------------------------------------------------

/// Design 4.1: one count line per credential, in scan order. A single
/// legacy token is the one line it always was.
#[test]
fn each_credential_is_counted_at_startup() {
    let ids: Vec<SessionId> = SERVED.iter().map(|s| SessionId::new(*s)).collect();
    let (_, creds) = resolve(&config(false, true), &env()).expect("resolve");
    let legacy = AuthToken::new(tok("default")).expect("token");
    let authority = portal_authority(Some(legacy.clone()), creds, &ids);
    assert_eq!(
        super::super::auth::credential_reach(&authority, &ids),
        [
            ("default", 3),
            ("viewers", 1),
            ("pre", 1),
            ("star", 3),
            ("agents", 1)
        ]
    );
    let one = [SessionId::new("t4-a")];
    let single = portal_authority(Some(legacy), vec![], &one);
    assert_eq!(
        super::super::auth::credential_reach(&single, &one),
        [("default", 1)]
    );
    let implicit = portal_authority(None, vec![], &one);
    assert_eq!(
        super::super::auth::credential_reach(&implicit, &one),
        [("local", 1)]
    );
    // A loose single default is counted for the grants that reach it.
    let loose = [SessionId::new("team notes")];
    let implicit = portal_authority(None, vec![], &loose);
    assert_eq!(
        super::super::auth::credential_reach(&implicit, &loose),
        [("local", 1)]
    );
}

/// The startup warnings: a legacy token beside configured credentials, a
/// credential that reads nothing, a listing under the implicit grant; and
/// none for a single-token portal.
#[test]
fn startup_warnings_name_credentials_never_tokens() {
    use super::super::startup_warnings;
    assert!(startup_warnings(false, &[("default", 1)], &[], false).is_empty());
    assert!(startup_warnings(false, &[("local", 2)], &[], false).is_empty());
    let w = startup_warnings(
        true,
        &[("default", 2), ("ghost", 0)],
        &[("ghost", vec!["t4-z", "t4-y"])],
        false,
    );
    assert_eq!(w.len(), 3, "{w:?}");
    assert!(w[0].contains("LAMBO_AUTH_TOKEN") && w[0].contains("\"default\""));
    assert!(w[1].contains("'ghost'") && w[1].contains("(t4-z, t4-y)"));
    assert!(w[2].contains("'ghost'") && w[2].contains("no served session"));
    let w = startup_warnings(false, &[("local", 2)], &[], true);
    assert_eq!(w.len(), 1);
    assert!(w[0].contains("list_sessions") && w[0].contains("anyone"));

    // What `run` passes: only the grants naming an unserved session.
    let ids: Vec<SessionId> = SERVED.iter().map(|s| SessionId::new(*s)).collect();
    let id = |s: &str| crate::surface::session::parse_addressed(s).expect("id");
    let ghost = WebCredential {
        grant: SessionGrant::new(
            "ghost",
            SessionScope::new([id("t4-a"), id("t4-z")], false, None),
            SessionCapabilities::default(),
        ),
        token: AuthToken::new(tok("ghost")).expect("token"),
    };
    let (_, mut creds) = resolve(&config(false, false), &env()).expect("resolve");
    creds.push(ghost);
    let authority = portal_authority(None, creds, &ids);
    assert_eq!(
        super::super::auth::unserved_names(&authority, &ids),
        [("ghost", vec!["t4-z"])]
    );
}

/// Acceptance: the constant-time scan is reused, not re-implemented. Every
/// credential goes into the one `SessionAuthority` (one construction
/// site), which `the_portal_uses_the_shared_bearer_check` already pins to
/// `surface::bearer`; no portal source compares secrets itself.
#[test]
fn every_credential_goes_through_the_one_shared_scan() {
    let prod = production_source();
    assert_eq!(
        prod.matches("SessionAuthority::with_credentials(").count(),
        1,
        "one credential set, built in one place"
    );
    for banned in [
        "secret_bytes() ==",
        "as_bytes() ==",
        "ct_eq",
        "constant_time",
        ".token ==",
    ] {
        assert!(
            !prod.contains(banned),
            "serve_web compares a secret itself ('{banned}'); use SessionAuthority"
        );
    }
}

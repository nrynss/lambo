//! The page's session picker (#4 PR 4, design 6.1 and 6.2): when it shows
//! (`/api/session`'s `switchable`), the URLs it builds, and source pins on
//! the served assets (names rendered as text, every URL relative, history
//! failures tolerated). There is no JS runner in the repo, so the script is
//! checked by what it contains and the server by what those URLs answer.

use super::routing::{portal, two_sessions};
use super::*;
use crate::config::WebConfig;
use crate::surface::session::{parse_addressed, MAX_ADDRESSED_LEN};

/// A token built at runtime, never printed.
fn solo_token() -> String {
    ["t4d", "-", "solo", "-", "k", "e", "y"].concat()
}

/// `GET path` presenting `token`, `Host` loopback.
async fn get_with(addr: SocketAddr, path: &str, token: &str) -> HttpResponse {
    send_raw(
        addr,
        &format!(
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\
             Accept: application/json\r\nConnection: close\r\n\r\n"
        ),
    )
    .await
}

/// Acceptance: the picker is hidden in single-session mode. One served
/// session gives a payload with no `switchable` key at all, at the alias
/// and the scoped path, implicit grant and legacy token alike, so the
/// page and its wire bytes are what they were.
#[tokio::test]
async fn a_single_session_page_is_not_switchable() {
    let state = portal(
        backends_on(seed("t4-a").await),
        &["t4-a"],
        None,
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    for path in ["/api/session", "/s/t4-a/api/session"] {
        let info = get_json(addr, path).await;
        assert_eq!(info["session"], "t4-a");
        assert!(info.get("switchable").is_none(), "{path}: {info}");
    }
    handle.abort();

    let token = AuthToken::new(solo_token()).expect("token");
    let state = portal(
        backends_on(seed("t4-a").await),
        &["t4-a"],
        Some(token),
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    for path in ["/api/session", "/s/t4-a/api/session"] {
        let r = get_with(addr, path, &solo_token()).await;
        assert_eq!(r.status, 200, "{path}");
        let info: serde_json::Value = serde_json::from_str(&r.body).expect("json");
        assert!(info.get("switchable").is_none(), "{path}: {info}");
    }
    handle.abort();
}

/// Two served sessions under one grant that reads both: every page, the
/// alias included, may switch.
#[tokio::test]
async fn every_page_of_a_multi_session_portal_is_switchable() {
    let state = portal(
        backends_on(two_sessions().await),
        &["t4-a", "t4-b"],
        None,
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    for (path, session) in [
        ("/api/session", "t4-a"),
        ("/s/t4-a/api/session", "t4-a"),
        ("/s/t4-b/api/session", "t4-b"),
    ] {
        let info = get_json(addr, path).await;
        assert_eq!(info["session"], session, "{path}");
        assert_eq!(info["switchable"], true, "{path}: {info}");
    }
    handle.abort();
}

/// `url` (relative) resolved against the page path `base`, as a browser
/// does (RFC 3986 5.2, paths only): the directory of `base`, then `.` and
/// `..` removed, never above the root.
fn resolve_relative(base: &str, url: &str) -> String {
    let dir = &base[..=base.rfind('/').expect("absolute base")];
    let joined = format!("{dir}{url}");
    let mut out: Vec<&str> = Vec::new();
    let segments: Vec<&str> = joined.split('/').skip(1).collect();
    for (i, seg) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        match *seg {
            "." => {
                if last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// The page's `rootPath()`, mirrored: `../../` on a scoped page.
fn root_of(page: &str) -> &'static str {
    if page.starts_with("/s/") {
        "../../"
    } else {
        ""
    }
}

#[test]
fn resolve_relative_follows_the_browser_rule() {
    assert_eq!(resolve_relative("/", "api/sessions"), "/api/sessions");
    assert_eq!(
        resolve_relative("/s/t4-a/", "../../api/sessions"),
        "/api/sessions"
    );
    assert_eq!(resolve_relative("/s/t4-a/", "../../s/t4-b/"), "/s/t4-b/");
    assert_eq!(
        resolve_relative("/s/t4-a/", "api/pulse"),
        "/s/t4-a/api/pulse"
    );
    assert_eq!(resolve_relative("/", "../../api/sessions"), "/api/sessions");
}

/// Acceptance: switching navigates to the other session's own page, and the
/// listing is read at the root, from every page. The picker's URLs, built as
/// the script builds them, resolve to `/api/sessions` and `/s/{name}/`; the
/// `HEAD` it sends before navigating is 200 for a session the caller reads
/// and the uniform 404 otherwise, and reads no store.
#[tokio::test]
async fn the_pickers_urls_resolve_from_every_page() {
    assert!(APP_JS.contains(r#"get(rootPath() + "api/sessions")"#));
    assert!(APP_JS.contains(r#"return rootPath() + "s/" + name + "/";"#));
    assert!(APP_JS.contains(r#"? "../../" : """#));

    let recording = super::routing::Recording::new(two_sessions().await);
    let web = WebConfig {
        list_sessions: true,
        ..Default::default()
    };
    let state = portal(
        backends_with_store(Box::new(recording.clone())),
        &["t4-a", "t4-b"],
        None,
        &web,
    );
    let (addr, handle) = spawn(state).await;
    let before = recording.calls();
    for page in ["/", "/s/t4-a/", "/s/t4-b/"] {
        assert_eq!(request(addr, "GET", page).await.status, 200, "{page}");
        let root = root_of(page);

        let listing = resolve_relative(page, &format!("{root}api/sessions"));
        assert_eq!(listing, "/api/sessions", "{page}");
        let names = get_json(addr, &listing).await;
        assert_eq!(names["sessions"], serde_json::json!(["t4-a", "t4-b"]));

        for (name, status) in [("t4-a", 200), ("t4-b", 200), ("t4-nope", 404)] {
            let target = resolve_relative(page, &format!("{root}s/{name}/"));
            assert_eq!(target, format!("/s/{name}/"), "{page} -> {name}");
            let r = request(addr, "HEAD", &target).await;
            assert_eq!(r.status, status, "HEAD {target} from {page}");
            if status == 404 {
                let unrouted = request(addr, "HEAD", "/no/such/path").await;
                assert_eq!(
                    super::routing::wire(&r),
                    super::routing::wire(&unrouted),
                    "a refused name is the uniform 404"
                );
            }
        }
    }
    assert_eq!(
        recording.calls(),
        before,
        "the picker's listing and probes read no store"
    );
    handle.abort();
}

/// The script's name rule is the server's: a name the picker accepts is one
/// `parse_addressed` accepts, and the other way round, so the picker never
/// sends a name the server must refuse by shape nor refuses one it serves.
#[test]
fn the_pickers_name_rule_is_the_servers() {
    assert_eq!(MAX_ADDRESSED_LEN, 128, "the script's {{0,127}} assumes 128");
    assert!(APP_JS.contains(r"var SESSION_NAME_RE = /^[A-Za-z0-9_:-][A-Za-z0-9._:-]{0,127}$/;"));
    // The script's regex, written out.
    let js_rule = |n: &str| {
        let b = n.as_bytes();
        let ok = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'-');
        !b.is_empty() && b.len() <= 128 && b[0] != b'.' && b.iter().all(|c| ok(*c))
    };
    let long_ok = "a".repeat(128);
    let long_bad = "a".repeat(129);
    for name in [
        "lambo",
        "t4-a",
        "dc-u-1",
        "a:b.c_d-e",
        "_x",
        ":x",
        "-x",
        "9",
        long_ok.as_str(),
        "",
        ".hidden",
        "a/b",
        "a%2Fb",
        "a b",
        "naïve",
        "<b>x</b>",
        long_bad.as_str(),
    ] {
        assert_eq!(
            js_rule(name),
            parse_addressed(name).is_ok(),
            "the rules disagree on {name:?}"
        );
    }
}

/// XSS and URL discipline in the served script: no markup sink at all (names
/// and data are set with `textContent`), one network call, every URL
/// relative (no absolute `/api` and no absolute `/s/` literal), and the
/// picker is gated on `switchable`.
#[test]
fn the_script_renders_names_as_text_and_builds_relative_urls() {
    for sink in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "new Function",
    ] {
        assert!(!APP_JS.contains(sink), "app.js must not use {sink}");
    }
    assert_eq!(APP_JS.matches("fetch(").count(), 1, "one network call");
    for absolute in ["\"/api", "'/api", "\"/s/", "'/s/"] {
        assert!(
            !APP_JS.contains(absolute),
            "app.js names an absolute URL: {absolute}"
        );
    }
    assert!(APP_JS.contains("if (!info.switchable || picker.mode) return;"));
    assert_eq!(
        APP_JS.matches("window.location.assign(").count(),
        1,
        "switching is one navigation"
    );
    // Names reach the DOM as text: the picker's options and message.
    assert!(APP_JS.contains("var o = el(\"option\", null, name);"));
    assert!(APP_JS.contains("$(\"session-picker-msg\").textContent = text"));
}

/// Acceptance: `localStorage` failures are tolerated. Every access sits in
/// a `try`, the history is one key, and what it holds is filtered through
/// the name rule before use (a value this page did not write is ignored).
#[test]
fn every_storage_access_is_guarded() {
    let lines: Vec<&str> = APP_JS.lines().collect();
    let mut seen = 0;
    for (i, line) in lines.iter().enumerate() {
        if !line.contains("localStorage.") {
            continue;
        }
        seen += 1;
        let guarded =
            line.contains("try {") || (i > 0 && lines[i - 1].trim_end().ends_with("try {"));
        assert!(guarded, "unguarded storage access: {}", line.trim());
    }
    assert!(seen >= 4, "theme and history: {seen}");
    assert!(APP_JS.contains(r#"var SESSION_HISTORY_KEY = "lambo-sessions";"#));
    assert!(APP_JS.contains("typeof n === \"string\" && SESSION_NAME_RE.test(n)"));
}

/// The picker's markup: hidden until the script shows it, a labelled
/// control, a submit button (Enter works from the keyboard), and a polite
/// live region for its messages.
#[test]
fn the_picker_markup_is_hidden_labelled_and_keyboard_operable() {
    assert!(INDEX_HTML.contains(
        r#"<form class="session-picker hidden" id="session-picker" aria-label="Switch session""#
    ));
    assert!(INDEX_HTML.contains(r#"for="session-choice">Switch to session</label>"#));
    assert!(INDEX_HTML.contains(r#"<button class="picker-btn" id="session-open" type="submit">"#));
    assert!(INDEX_HTML.contains(r#"id="session-picker-msg" role="status" aria-live="polite""#));
    assert!(
        APP_JS.contains("$(\"session-picker\").addEventListener(\"submit\", openPickedSession)")
    );
    assert!(APP_JS.contains("ev.preventDefault();"));
    assert!(APP_CSS.contains(".sr-only"));
}

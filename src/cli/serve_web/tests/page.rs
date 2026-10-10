//! The page's session picker (#4 PR 4, design 6.1 and 6.2): when it shows
//! (`/api/session`'s `switchable`), the URLs it builds, and source pins on
//! the served assets (names rendered as text, every URL relative, history
//! failures tolerated). There is no JS runner in the repo, so the script is
//! checked by what it contains and the server by what those URLs answer.

use super::routing::{portal, two_sessions};
use super::*;
use crate::config::WebConfig;
use crate::surface::session::{
    parse_addressed, SessionCapabilities, SessionGrant, SessionPrefix, SessionScope,
    MAX_ADDRESSED_LEN,
};

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

/// A token for credential `name`, built at runtime, never printed.
fn cred_token(name: &str) -> String {
    ["t4d", "-", name, "-", "k", "e", "y"].concat()
}

/// A read credential called `name` over the exact `names` and an optional
/// `prefix`.
fn credential(name: &str, names: &[&str], prefix: Option<&str>) -> WebCredential {
    WebCredential {
        grant: SessionGrant::new(
            name,
            SessionScope::new(
                names.iter().map(|n| parse_addressed(n).expect("name")),
                false,
                prefix.map(|p| SessionPrefix::new(p).expect("prefix")),
            ),
            SessionCapabilities::default(),
        ),
        token: AuthToken::new(cred_token(name)).expect("token"),
    }
}

/// The sessions [`credentialed`] serves, the default first. `t4q-1` holds
/// nothing: it is served as an empty session.
const SERVED: &[&str] = &["t4-a", "t4-b", "t4q-1"];

/// A portal over [`SERVED`] with three prefix credentials:
///
/// | credential | scope | reads |
/// |---|---|---|
/// | `pre-two` | prefix `t4-` | `t4-a`, `t4-b` |
/// | `pre-one` | prefix `t4q-` | `t4q-1` |
/// | `mixed` | `t4-a` and prefix `t4q-` | `t4-a`, `t4q-1` |
async fn credentialed(list: bool) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let ids: Vec<SessionId> = SERVED.iter().map(|s| SessionId::new(*s)).collect();
    let web = WebConfig {
        list_sessions: list,
        ..Default::default()
    };
    let state = Arc::new(AppState::new(
        ids[0].clone(),
        ids,
        backends_on(two_sessions().await),
        true,
        None,
        vec![
            credential("pre-two", &[], Some("t4-")),
            credential("pre-one", &[], Some("t4q-")),
            credential("mixed", &["t4-a"], Some("t4q-")),
        ],
        &[],
        &web,
    ));
    spawn(state).await
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
    // The shipped regex, read out of the script rather than transcribed
    // (review L5): `^[FIRST][REST]{0,MAX}$`, its two classes parsed into
    // byte sets and its bound into a number, then run as that pattern runs.
    let literal = APP_JS
        .split("var SESSION_NAME_RE = /^[")
        .nth(1)
        .and_then(|rest| rest.split("$/;").next())
        .expect("the regex literal");
    let (first, rest) = literal.split_once("][").expect("two classes");
    let (rest, bound) = rest.split_once("]{0,").expect("a bounded repeat");
    let max_rest: usize = bound.trim_end_matches('}').parse().expect("bound");
    let class = |src: &str| -> Vec<u8> {
        let b = src.as_bytes();
        let mut set = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if i + 2 < b.len() && b[i + 1] == b'-' {
                set.extend(b[i]..=b[i + 2]);
                i += 3;
            } else {
                set.push(b[i]);
                i += 1;
            }
        }
        set
    };
    let (first, rest) = (class(first), class(rest));
    let js_rule = |n: &str| {
        let b = n.as_bytes();
        !b.is_empty()
            && b.len() <= 1 + max_rest
            && first.contains(&b[0])
            && b[1..].iter().all(|c| rest.contains(c))
    };
    let long_ok = "a".repeat(128);
    let long_bad = "a".repeat(129);
    for name in [
        "lambo",
        "t4-a",
        "dc-u-1",
        "a:b.c_d-e",
        "x.",
        "Z9",
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
    // The page's own markup stays free of inline script, style, and HTML
    // event-handler attributes, so `script-src 'self'` / `style-src 'self'`
    // need no `'unsafe-inline'`. `element.onclick =` in app.js is a property
    // assignment from the external script, not an HTML handler.
    assert_eq!(INDEX_HTML.matches("<script").count(), 1, "{INDEX_HTML}");
    assert!(INDEX_HTML.contains(r#"<script src="/app.js"></script>"#));
    assert!(
        !INDEX_HTML.to_ascii_lowercase().contains("<style"),
        "index.html must not carry an inline style"
    );
    let handlers = inline_event_handler_attributes(INDEX_HTML);
    assert!(
        handlers.is_empty(),
        "index.html has inline event-handler attributes: {handlers:?}"
    );
}

/// Attribute names inside tags that are HTML event handlers (`onclick`, …).
fn inline_event_handler_attributes(html: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        if rest.starts_with('!') || rest.starts_with('/') || rest.starts_with('?') {
            let Some(end) = rest.find('>') else { break };
            rest = &rest[end + 1..];
            continue;
        }
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[..end];
        rest = &rest[end + 1..];
        let bytes = tag.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_whitespace() {
                let name_at = i + 1;
                let mut j = name_at;
                while j < bytes.len() && bytes[j].is_ascii_alphanumeric() {
                    j += 1;
                }
                if j < bytes.len()
                    && bytes[j] == b'='
                    && j > name_at + 2
                    && tag[name_at..name_at + 2].eq_ignore_ascii_case("on")
                    && tag.as_bytes()[name_at + 2].is_ascii_alphabetic()
                {
                    found.push(tag[name_at..j].to_string());
                }
                i = j;
            } else {
                i += 1;
            }
        }
    }
    found
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
    // Both controls point at the message (review I6).
    assert!(INDEX_HTML.contains(
        r#"<select class="picker-control hidden" id="session-choice" aria-describedby="session-picker-msg">"#
    ));
    assert!(INDEX_HTML.contains(
        r#"maxlength="128" placeholder="session name" aria-describedby="session-picker-msg">"#
    ));
    assert!(
        APP_JS.contains("$(\"session-picker\").addEventListener(\"submit\", openPickedSession)")
    );
    assert!(APP_JS.contains("ev.preventDefault();"));
    assert!(APP_CSS.contains(".sr-only"));
}

/// Design Q9: an allowlisted session with no memory says so, rather than
/// "concepts are being recorded".
#[test]
fn an_empty_session_says_so() {
    assert!(APP_JS.contains(r#"var EMPTY_SESSION = "No memory in this session yet";"#));
    // Review L4: the poll's count (every poll interval) decides once it has
    // answered; the structure (every 20 s) only before that.
    let rule = APP_JS
        .split("function isEmptySession() {")
        .nth(1)
        .and_then(|rest| rest.split('}').next())
        .expect("isEmptySession");
    let count = rule
        .find("if (state.concepts !== null) return state.concepts === 0;")
        .expect("the count decides");
    let structure = rule
        .find("return !!state.graph && state.graph.nodes.length === 0;")
        .expect("the structure falls back");
    assert!(count < structure, "{rule}");
    // A transition repaints the hero from the poll, and an emptied session
    // shows no pillar from a structure loaded before the erase.
    let poll = APP_JS
        .split("function poll() {")
        .nth(1)
        .and_then(|rest| rest.split("function schedule()").next())
        .expect("poll");
    assert!(poll.contains("var wasEmpty = isEmptySession();"));
    assert!(poll.contains("if (wasEmpty !== isEmptySession()) renderHero();"));
    assert!(APP_JS.contains("var pillar = isEmptySession() ? null : pickPillar();"));
    assert!(INDEX_HTML.contains(r#"<h1 id="hero-empty-heading">Nothing relied on yet</h1>"#));
}

/// Review L1: the picker's `HEAD` probe drops a name from the history only
/// on the portal's uniform 404, the one answer that means "this caller
/// cannot read that name". On the wire: a name out of the credential's
/// scope and one not served are that 404, byte for byte an unrouted path's;
/// a missing or wrong bearer is a 401, which the script keeps the name on
/// and reports as retryable (as it does a 5xx or a network failure).
#[tokio::test]
async fn only_the_probes_404_means_a_name_cannot_be_read() {
    let (addr, handle) = credentialed(false).await;
    let token = cred_token("pre-one");
    let wrong = cred_token("nobody");
    let r = as_caller(addr, "HEAD", "/s/t4q-1/", Some(&token)).await;
    assert_eq!(r.status, 200, "a session in reach");
    let unrouted = as_caller(addr, "HEAD", "/no/such/path", Some(&token)).await;
    for target in ["/s/t4-a/", "/s/t4q-2/", "/s/.x/"] {
        let r = as_caller(addr, "HEAD", target, Some(&token)).await;
        assert_eq!(r.status, 404, "{target}");
        assert_eq!(
            super::routing::wire(&r),
            super::routing::wire(&unrouted),
            "{target}: the uniform 404"
        );
    }
    for presented in [None, Some(wrong.as_str())] {
        for target in ["/s/t4q-1/", "/s/t4-a/", "/s/t4q-2/"] {
            let r = as_caller(addr, "HEAD", target, presented).await;
            assert_eq!(r.status, 401, "{target} without a valid bearer");
        }
    }
    handle.abort();

    // The script: the forget and the "cannot be read" message sit under
    // `r.status === 404` alone; anything else keeps the name.
    let probe = APP_JS
        .split("send(target, { method: \"HEAD\"")
        .nth(1)
        .and_then(|rest| rest.split("function initPicker").next())
        .expect("the probe");
    let on_404 = probe
        .split("if (r.status === 404) {")
        .nth(1)
        .and_then(|rest| rest.split("return;").next())
        .expect("a 404 branch");
    assert!(on_404.contains("forgetSession(name);"), "{on_404}");
    assert!(on_404.contains("can be read here."), "{on_404}");
    assert_eq!(probe.matches("forgetSession(").count(), 1, "{probe}");
    assert_eq!(probe.matches("can be read here.").count(), 1, "{probe}");
    assert!(probe.contains(
        r#"pickerMessage("Could not open " + name + " (HTTP " + r.status + "). Try again.");"#
    ));
    assert!(probe.contains(r#"pickerMessage("Could not reach the server. Try again.");"#));
}

/// Review L2: listing mode can name fewer sessions than the caller reads.
/// The listing gives a credential's exact names only, never a prefix
/// expansion, so `mixed` (`t4-a` plus prefix `t4q-`) lists `t4-a` alone
/// while it reads, and may switch to, `t4q-1` too. The select therefore
/// ends with "Other session…", which reveals the labelled text field, and
/// a typed name is what the picker then opens.
#[tokio::test]
async fn listing_mode_still_offers_a_typed_name() {
    let (addr, handle) = credentialed(true).await;
    let token = cred_token("mixed");
    let r = as_caller(addr, "GET", "/api/sessions", Some(&token)).await;
    assert_eq!(r.status, 200);
    let listed: serde_json::Value = serde_json::from_str(&r.body).expect("json");
    assert_eq!(listed["sessions"], serde_json::json!(["t4-a"]));
    let r = as_caller(addr, "GET", "/s/t4q-1/api/session", Some(&token)).await;
    assert_eq!(r.status, 200, "read through the prefix");
    let info: serde_json::Value = serde_json::from_str(&r.body).expect("json");
    assert_eq!(info["switchable"], true, "{info}");
    let r = as_caller(addr, "HEAD", "/s/t4q-1/", Some(&token)).await;
    assert_eq!(r.status, 200, "the typed name's probe");
    handle.abort();

    let choice = APP_JS
        .split("function showSessionChoice(names) {")
        .nth(1)
        .and_then(|rest| rest.split("function showSessionEntry()").next())
        .expect("showSessionChoice");
    assert!(choice.contains(r#"var other = el("option", null, "Other session…");"#));
    assert!(choice.contains("other.value = OTHER_SESSION;"));
    assert!(choice.contains(r#"show($("session-entry"), choosingOther());"#));
    assert!(APP_JS.contains(r#"var OTHER_SESSION = "";"#));
    assert!(APP_JS.contains(
        r#"return picker.mode === "choice" && $("session-choice").value === OTHER_SESSION;"#
    ));
    assert!(APP_JS.contains(r#"var typed = picker.mode === "entry" || choosingOther();"#));
    assert!(APP_JS.contains(
        r#"var name = (typed ? $("session-entry").value : $("session-choice").value).trim();"#
    ));
    // Each control has its own label, in either mode.
    assert!(!APP_JS.contains("htmlFor"), "labels are static");
    assert!(INDEX_HTML.contains(r#"for="session-choice">Switch to session</label>"#));
    assert!(INDEX_HTML.contains(r#"for="session-entry">Session name to open</label>"#));
}

/// Review L3: a session name is up to 128 characters with no break
/// opportunity, and the picker's message quotes a typed one. At 375 px
/// neither may widen the page: the name is cut with an ellipsis (its full
/// text in `title`, set as a property, never markup) inside a bar it cannot
/// outgrow, and the message wraps anywhere. Checked in a browser at 375 px
/// with a served 128-character session and a typed 128-character name
/// (document scroll width 375 in both).
#[test]
fn a_long_name_never_widens_a_narrow_page() {
    let rule = |selector: &str| -> &str {
        APP_CSS
            .split(&format!("\n{selector} {{"))
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .unwrap_or_else(|| panic!("no rule {selector}"))
    };
    let name = rule(".session-name");
    for decl in [
        "min-width: 0;",
        "max-width: 100%;",
        "overflow: hidden;",
        "text-overflow: ellipsis;",
        "white-space: nowrap;",
    ] {
        assert!(name.contains(decl), ".session-name lacks {decl}");
    }
    let msg = rule(".picker-msg");
    for decl in [
        "min-width: 0;",
        "max-width: 100%;",
        "overflow-wrap: anywhere;",
    ] {
        assert!(msg.contains(decl), ".picker-msg lacks {decl}");
    }
    assert!(rule(".topbar-left").contains("max-width: 100%;"));
    assert!(rule(".session-picker").contains("max-width: 100%;"));
    assert!(APP_JS.contains(r#"$("session-name").title = info.session;"#));
}

/// Review L5: `switchable` for a prefix grant is the count of served
/// sessions it reads, not a property of having a prefix. `pre-two`'s prefix
/// reads two served sessions, so every page it may read is switchable;
/// `pre-one`'s reads one, so no page carries the key (and the alias, the
/// default it cannot read, is the uniform 404); `mixed` reads one by name
/// and one by prefix, two in all.
#[tokio::test]
async fn a_prefix_grant_is_switchable_only_over_two_served_sessions() {
    let (addr, handle) = credentialed(false).await;
    for (cred, reads, switchable) in [
        ("pre-two", &["t4-a", "t4-b"][..], true),
        ("pre-one", &["t4q-1"][..], false),
        ("mixed", &["t4-a", "t4q-1"][..], true),
    ] {
        let token = cred_token(cred);
        let mut paths: Vec<String> = reads
            .iter()
            .map(|s| format!("/s/{s}/api/session"))
            .collect();
        if reads.contains(&SERVED[0]) {
            paths.push("/api/session".to_string());
        } else {
            let r = as_caller(addr, "GET", "/api/session", Some(&token)).await;
            assert_eq!(r.status, 404, "{cred}: the default is out of reach");
        }
        for path in paths {
            let r = as_caller(addr, "GET", &path, Some(&token)).await;
            assert_eq!(r.status, 200, "{cred} {path}");
            let v: serde_json::Value = serde_json::from_str(&r.body).expect("json");
            match switchable {
                true => assert_eq!(v["switchable"], true, "{cred} {path}"),
                false => assert!(v.get("switchable").is_none(), "{cred} {path}: {v}"),
            }
        }
    }
    handle.abort();
}

/// Review I3: the listing refuses a request that arrived scoped by itself,
/// not only through `scope::scoped`'s path check. Called directly with the
/// marker the scoped resolution attaches, it is the uniform 404 even with a
/// caller that may list; without the marker the same caller is listed.
#[tokio::test]
async fn the_listing_refuses_a_scoped_request_itself() {
    let web = WebConfig {
        list_sessions: true,
        ..Default::default()
    };
    let state = portal(
        backends_on(two_sessions().await),
        &["t4-a", "t4-b"],
        None,
        &web,
    );
    let grant = Arc::new(credential("pre-two", &[], Some("t4-")).grant);
    let caller = || Some(axum::Extension(Caller(grant.clone())));
    let scoped = super::super::scope::ScopedRequest;
    let refused = api_sessions(
        axum::extract::State(state.clone()),
        caller(),
        Some(axum::Extension(scoped)),
    )
    .await;
    assert_eq!(refused.status(), 404);
    let listed = api_sessions(axum::extract::State(state), caller(), None).await;
    assert_eq!(listed.status(), 200);
}

//! Security headers (#92): the page `Content-Security-Policy`, and
//! `X-Content-Type-Options: nosniff` on every serve-web response.

use super::routing::portal;
use super::*;
use crate::config::WebConfig;

/// The policy the page must send, in full. A substring check would stay
/// green if a directive were added (`data:`, `'unsafe-inline'`, …).
const PAGE_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; font-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'";

/// Every value of header `name` (compared case-insensitively), in order.
fn header_values(response: &HttpResponse, name: &str) -> Vec<String> {
    response
        .headers
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
        .collect()
}

fn assert_nosniff(response: &HttpResponse, what: &str) {
    assert_eq!(
        header_values(response, "x-content-type-options"),
        vec!["nosniff".to_string()],
        "{what}"
    );
}

fn assert_csp(response: &HttpResponse, what: &str) {
    assert_eq!(
        header_values(response, "content-security-policy"),
        vec![PAGE_CSP.to_string()],
        "{what}"
    );
}

fn assert_no_csp(response: &HttpResponse, what: &str) {
    assert!(
        header_values(response, "content-security-policy").is_empty(),
        "{what} must not carry a content-security-policy"
    );
}

#[test]
fn only_text_html_counts_as_html() {
    use super::super::headers::content_type_is_html;
    use axum::http::HeaderValue;
    for (value, html) in [
        ("text/html", true),
        ("text/html; charset=utf-8", true),
        ("TEXT/HTML;charset=UTF-8", true),
        (" text/html ; charset=utf-8", true),
        ("text/htmlx", false),
        ("text/html-foo", false),
        ("application/xhtml+xml", false),
        ("text/plain", false),
        ("text/plain; note=text/html", false),
        ("", false),
    ] {
        let header = HeaderValue::from_str(value).expect("header value");
        assert_eq!(content_type_is_html(&header), html, "{value:?}");
    }
    let not_utf8 = HeaderValue::from_bytes(b"text/html; charset=\xff").expect("opaque bytes");
    assert!(not_utf8.to_str().is_err());
    assert!(!content_type_is_html(&not_utf8));
}

/// A bearer built at runtime. Never interpolated into an assertion.
fn bearer(label: &str) -> String {
    ["csp92", label, "token"].join("-")
}

async fn with_bearer(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> HttpResponse {
    let authorization = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    send_raw(
        addr,
        &format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{authorization}Connection: close\r\n\r\n"
        ),
    )
    .await
}

/// `GET` and `HEAD` of both pages carry the exact policy and `nosniff`.
/// Everything else carries `nosniff` and no policy, including a Host
/// refusal that never reaches the router.
#[tokio::test]
async fn pages_carry_the_exact_csp_and_every_response_is_nosniff() {
    let store = seed("csp92").await;
    let state = state_on(store, "csp92");
    let (addr, handle) = spawn(state).await;

    // First, so a nosniff regression on the pre-routing Host refusal fails
    // here rather than on a later page that did reach the router.
    let refused = send_raw(
        addr,
        "GET / HTTP/1.1\r\nHost: rebind.example\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(refused.status, 403, "non-loopback Host");
    assert_nosniff(&refused, "pre-routing Host 403");
    assert_no_csp(&refused, "pre-routing Host 403");

    for (method, path) in [
        ("GET", "/"),
        ("HEAD", "/"),
        ("GET", "/s/csp92/"),
        ("HEAD", "/s/csp92/"),
    ] {
        let response = request(addr, method, path).await;
        assert_eq!(response.status, 200, "{method} {path}");
        assert!(
            header_values(&response, "content-type")
                .iter()
                .any(|value| value.to_ascii_lowercase().starts_with("text/html")),
            "{method} {path} content-type"
        );
        assert_csp(&response, &format!("{method} {path}"));
        assert_nosniff(&response, &format!("{method} {path}"));
    }

    for (method, path) in [
        ("GET", "/app.js"),
        ("GET", "/app.css"),
        ("GET", "/healthz"),
        ("GET", "/api/stats"),
        ("GET", "/no/such/path"),
        ("GET", "/s/not-served/"),
        ("POST", "/"),
        ("POST", "/s/csp92/"),
        ("GET", "/s/csp92"),
        ("HEAD", "/s/csp92"),
    ] {
        let response = request(addr, method, path).await;
        let expect = match (method, path) {
            ("POST", _) => 405,
            (_, "/s/csp92") => 308,
            ("GET", "/no/such/path" | "/s/not-served/") => 404,
            _ => 200,
        };
        assert_eq!(response.status, expect, "{method} {path}");
        assert_nosniff(&response, &format!("{method} {path}"));
        assert_no_csp(&response, &format!("{method} {path}"));
    }

    handle.abort();
}

/// A configured bearer: a missing token and a wrong one are both `401`,
/// with `nosniff` and no page policy. The token is not part of the answer.
#[tokio::test]
async fn a_bearer_refusal_carries_nosniff_and_no_csp() {
    let store = seed("csp92").await;
    let token = AuthToken::new(bearer("reader")).expect("token");
    let state = portal(
        backends_on(store),
        &["csp92"],
        Some(token),
        &WebConfig::default(),
    );
    let (addr, handle) = spawn(state).await;
    let wrong = bearer("other");

    for (path, presented) in [
        ("/api/stats", None),
        ("/s/csp92/", Some(wrong.as_str())),
        ("/no/such/path", None),
        ("/no/such/path", Some(wrong.as_str())),
    ] {
        let response = with_bearer(addr, "GET", path, presented).await;
        assert_eq!(response.status, 401, "{path}");
        assert_nosniff(&response, path);
        assert_no_csp(&response, path);
    }

    handle.abort();
}

/// JSON a handler answers, scoped or not, and its refusals (a bad query is
/// `400`, a saturated recall bound `503`): `nosniff` and no page policy.
#[tokio::test]
async fn handler_json_and_its_refusals_carry_nosniff_and_no_csp() {
    let store = seed("csp92").await;
    let state = state_with_web(
        backends_on(store),
        "csp92",
        None,
        &WebConfig {
            recall_concurrency: Some(1),
            ..Default::default()
        },
    );
    let (addr, handle) = spawn(state.clone()).await;

    for (path, expect) in [
        ("/s/csp92/api/stats", 200),
        ("/s/csp92/api/session", 200),
        ("/api/recall?q=", 400),
        ("/s/csp92/api/recall?q=", 400),
        ("/s/csp92/api/recall", 400),
    ] {
        let response = request(addr, "GET", path).await;
        assert_eq!(response.status, expect, "{path}: {}", response.body);
        assert!(
            header_values(&response, "content-type")
                .iter()
                .any(|value| value.starts_with("application/json")),
            "{path} content-type"
        );
        assert_nosniff(&response, path);
        assert_no_csp(&response, path);
    }

    let held = state.views.recall_permit().await.expect("the one permit");
    let busy = request(addr, "GET", "/s/csp92/api/recall?q=user%20schema").await;
    assert_eq!(busy.status, 503, "{}", busy.body);
    assert_nosniff(&busy, "recall 503");
    assert_no_csp(&busy, "recall 503");
    drop(held);

    handle.abort();
}

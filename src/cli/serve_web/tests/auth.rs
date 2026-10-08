//! Authorization: fail-closed binds and bearer tokens.

use super::*;

/// Like [`request`], but with an optional `Authorization` header so the
/// auth gate can be exercised.
async fn request_authed(
    addr: SocketAddr,
    method: &str,
    path: &str,
    authorization: Option<&str>,
) -> HttpResponse {
    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let auth = authorization
        .map(|a| format!("Authorization: {a}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\n{auth}Connection: close\r\n\r\n"
    );
    sock.write_all(req.as_bytes()).await.expect("write");
    sock.flush().await.expect("flush");
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.expect("read");
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status line in: {head}"));
    HttpResponse {
        status,
        headers: head.to_string(),
        body: body.to_string(),
    }
}

// ---- auth: fail-closed non-loopback bind (mirrors T8.7) -------------

/// A non-loopback bind without a token is a startup refusal; loopback
/// stays optional-auth. Mirrors `mcp::serve`'s `authorize_bind` test.
#[test]
fn authorize_bind_web_fails_closed_off_loopback() {
    let public: IpAddr = "0.0.0.0".parse().unwrap();
    let any_v6: IpAddr = "0:0:0:0:0:0:0:0".parse().unwrap();
    let lan: IpAddr = "192.168.1.10".parse().unwrap();
    let loopback: IpAddr = "127.0.0.1".parse().unwrap();
    let loopback_v6: IpAddr = "::1".parse().unwrap();
    let loopback_subnet: IpAddr = "127.9.9.9".parse().unwrap();

    for bind in [public, any_v6, lan] {
        let err =
            authorize_bind_web(bind, None).expect_err("{bind} without a token must not start");
        let msg = err.to_string();
        assert!(msg.contains("refusing to start"), "{msg}");

        let token = AuthToken::new("s3cret").expect("valid");
        authorize_bind_web(bind, Some(&token)).expect("a token satisfies the rule");
    }

    for bind in [loopback, loopback_v6, loopback_subnet] {
        authorize_bind_web(bind, None).expect("loopback stays optional-auth");
    }
}

/// AuthToken rejects empty/whitespace tokens, so a set-but-empty
/// LAMBO_AUTH_TOKEN is a usage error rather than authenticate-everything.
#[test]
fn an_empty_auth_token_is_refused() {
    assert!(AuthToken::new("").is_err());
    assert!(AuthToken::new("   ").is_err());
    assert!(AuthToken::new("s3cret").is_ok());
}

/// The `Authorization` header is parsed strictly: scheme case-insensitive,
/// credential exact.
#[test]
fn bearer_header_is_parsed_strictly() {
    let expected = AuthToken::new("s3cret").expect("valid");
    assert!(bearer_ok(Some("Bearer s3cret"), &expected));
    assert!(bearer_ok(Some("bearer s3cret"), &expected), "RFC 7235 §2.1");
    assert!(bearer_ok(Some("  Bearer s3cret  "), &expected));
    assert!(!bearer_ok(None, &expected), "a missing header is a refusal");
    assert!(!bearer_ok(Some("Bearer wrong"), &expected));
    assert!(!bearer_ok(Some("Basic s3cret"), &expected), "wrong scheme");
    assert!(!bearer_ok(Some("s3cret"), &expected), "no scheme at all");
    assert!(!bearer_ok(Some("Bearer"), &expected));
    assert!(!bearer_ok(Some(""), &expected));
}

/// The comparator (`crate::surface::bearer::tokens_match`, shared with
/// `lambo serve` since #28) performs a full scan of the presented input on
/// every call: there is no early return on a mismatch, and the loop count
/// does not depend on the secret's length. Boolean results alone
/// cannot tell a short-circuited compare from a full scan, so this pins
/// the full-scan *semantics* (every position is compared, a late-only
/// difference is caught, exact input matches, and a length change is a
/// refusal even when the shorter input is an exact prefix — the case a
/// naive `zip`-style short-circuit would wrongly accept). The loop count
/// itself is pinned where the loop lives:
/// `surface::bearer::tests::the_loop_count_follows_the_presented_length_not_the_secret`.
#[test]
fn tokens_match_scans_the_full_length_without_short_circuiting() {
    let token = b"s3cret".as_slice();

    // Exact match across the whole length -> true.
    assert!(tokens_match(b"s3cret", token), "correct token matches");

    // A difference at a single position is caught wherever it falls:
    // first, middle, or last. (These alone do not prove a full scan — a
    // short-circuit catches a first divergence at any depth too.)
    assert!(!tokens_match(b"x3cret", token), "first byte differs");
    assert!(!tokens_match(b"s3xret", token), "middle byte differs");
    assert!(!tokens_match(b"s3crex", token), "last byte differs");

    // The genuine non-short-circuit guard is the truncated-prefix refusal
    // below: a naive `zip`-style compare would stop at the common 4 bytes,
    // find them equal, and wrongly accept `"s3cr"`. Requiring `false` here
    // proves the accumulator folds the length difference and the scan
    // commits no early-return on the secret bytes.

    // A longer presented value whose prefix matches must refuse: the scan
    // and the length fold both see the trailing bytes.
    assert!(
        !tokens_match(b"s3cret-extra", token),
        "padded token refuses"
    );

    // A shorter presented value that is an exact prefix must refuse: a
    // naive full-scan-equal-length or zip-short-circuit implementation
    // would accept the common prefix and return true.
    assert!(!tokens_match(b"s3cr", token), "truncated prefix refuses");

    // Empty presented input is refused.
    assert!(!tokens_match(b"", token));
}

/// When a token is configured, every route — API, asset, healthz — refuses
/// a request without it and serves one that carries it. This is the
/// middleware wiring that the `authorize_bind_web` unit test only proves is
/// *called*.
#[tokio::test]
async fn a_configured_token_is_required_on_every_route() {
    let store = seed("t85-auth").await;
    let (addr, handle) = spawn(state_with_auth(
        store,
        "t85-auth",
        Some(AuthToken::new("s3cret").unwrap()),
    ))
    .await;

    // No token and a wrong token are refused identically (terse 401).
    for path in ROUTES {
        let r = request_authed(addr, "GET", path, None).await;
        assert_eq!(r.status, 401, "GET {path} without a token must be 401");
    }
    let r = request_authed(addr, "GET", "/api/session", Some("Bearer wrong")).await;
    assert_eq!(r.status, 401, "a wrong token must be refused");

    // The correct bearer token is accepted on the data API and the page.
    let r = request_authed(addr, "GET", "/api/session", Some("Bearer s3cret")).await;
    assert_eq!(r.status, 200, "the correct token must be served");
    let info: serde_json::Value = serde_json::from_str(&r.body).expect("json");
    assert_eq!(info["read_only"], true);
    assert_eq!(info["exposed_beyond_loopback"], true);
    let r = request_authed(addr, "GET", "/", Some("Bearer s3cret")).await;
    assert_eq!(r.status, 200, "the correct token must serve the page");

    handle.abort();
}

/// #28: the portal checks bearer tokens through the comparator `lambo serve`
/// uses (`crate::surface::bearer`), not a copy of its own.
///
/// The copies diverged once already: T1-P3-1's remediation rewrote only this
/// surface's comparator, in the opposite loop direction, so its iteration
/// count followed the secret's length. Boolean results cannot tell the two
/// apart, so the pin is structural: no comparator is defined in the portal,
/// and its gate goes through the shared check.
#[test]
fn the_portal_uses_the_shared_bearer_check() {
    let prod = production_source();
    assert!(
        !prod.contains("fn tokens_match"),
        "serve_web defines its own token comparator; use crate::surface::bearer"
    );
    assert!(
        prod.contains("crate::surface::bearer::bearer_ok"),
        "serve_web's bearer gate must delegate to crate::surface::bearer::bearer_ok"
    );
}

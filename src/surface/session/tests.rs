use super::*;
use crate::test_util::on_the_wire;

fn id(raw: &str) -> AddressedSessionId {
    parse_addressed(raw).unwrap_or_else(|e| panic!("{raw:?} must parse: {e}"))
}

fn prefix(raw: &str) -> SessionPrefix {
    SessionPrefix::new(raw).unwrap_or_else(|e| panic!("{raw:?} must be a prefix: {e}"))
}

/// Decision 16's charset, bounds and leading-dot rule, as a table.
///
/// Mutation: widen `is_addressed_byte` (e.g. allow `/` or `%`), drop the
/// leading-dot test, or change either bound by one → red.
#[test]
fn parse_addressed_accepts_exactly_the_strict_charset() {
    let max = "a".repeat(MAX_ADDRESSED_LEN);
    for ok in [
        "a",
        "Z",
        "0",
        "lambo",
        "lambo-dev",
        "rustydocs",
        "general",
        "dc-u-12345",
        "a.b",
        "a..b",
        "trailing.",
        "x:y",
        "A_Z-09.:_",
        "-leading-hyphen",
        "_leading_underscore",
        ":leading:colon",
        max.as_str(),
    ] {
        assert_eq!(id(ok).as_str(), ok);
    }

    let too_long = "a".repeat(MAX_ADDRESSED_LEN + 1);
    for bad in [
        "",
        ".",
        "..",
        ".hidden",
        "./a",
        too_long.as_str(),
        "a/b",
        "/a",
        "a\\b",
        "a b",
        " a",
        "a ",
        "a\tb",
        "a\nb",
        "a\0b",
        "a*",
        "*",
        "a?b",
        "a#b",
        "a+b",
        "a@b",
        "a,b",
        "a;b",
        "a=b",
        "a~b",
        "é",
        "caf\u{e9}",
        "a\u{200b}b",
        "\u{202e}a",
    ] {
        let refusal = parse_addressed(bad).expect_err(bad);
        assert_eq!(refusal.reason(), RefusalReason::Malformed, "{bad:?}");
    }
}

/// No percent-decoding (decision 16): every percent form is refused as it
/// stands, including the ones that would decode to an allowed id or to a
/// traversal.
#[test]
fn parse_addressed_refuses_percent_forms_without_decoding_them() {
    for bad in [
        "%2e%2e",
        "%2E%2E",
        "a%2Fb",
        "a%2fb",
        "%61",
        "lambo%2Ddev",
        "a%20b",
        "%",
        "a%",
        "%%",
    ] {
        let refusal = parse_addressed(bad).expect_err(bad);
        assert_eq!(refusal.reason(), RefusalReason::Malformed, "{bad:?}");
    }
}

/// The length bound is in bytes, not chars, and a refused id never reaches
/// the charset check's caller as anything but `Malformed`.
#[test]
fn the_length_bound_is_in_bytes() {
    let at = "z".repeat(MAX_ADDRESSED_LEN);
    assert!(parse_addressed(&at).is_ok());
    let over = format!("{at}z");
    assert_eq!(over.len(), MAX_ADDRESSED_LEN + 1);
    assert!(parse_addressed(&over).is_err());
}

async fn parts(resp: Response) -> (StatusCode, usize, Vec<u8>) {
    let status = resp.status();
    let headers = resp.headers().len();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    (status, headers, body)
}

/// One 404 constant: every refusal reason renders the same status, headers
/// and body as a `Response` value (status 404, no headers of its own, empty
/// body). What the wire carries is pinned by
/// `a_refused_session_and_an_unrouted_path_are_identical_on_the_wire`.
///
/// Mutation: give any reason its own status, header or body text → red.
#[tokio::test]
async fn every_refusal_renders_the_same_bytes_as_an_unrouted_path() {
    let unrouted = parts(StatusCode::NOT_FOUND.into_response()).await;
    assert_eq!(unrouted, (StatusCode::NOT_FOUND, 0, Vec::new()));
    assert_eq!(parts(not_found_response()).await, unrouted);
    assert_eq!(unrouted.0, NOT_FOUND_STATUS);
    assert_eq!(unrouted.2, NOT_FOUND_BODY);
    for reason in [
        RefusalReason::Malformed,
        RefusalReason::OutOfScope,
        RefusalReason::MissingCapability,
        RefusalReason::Absent,
    ] {
        let rendered = parts(SessionRefusal::new(reason).not_found_response()).await;
        assert_eq!(rendered, unrouted, "{reason:?} must render the uniform 404");
    }
}

/// The operator-facing text names the reason class and never the id.
#[test]
fn a_refusal_never_carries_the_probed_id() {
    let probed = "secret-tenant-name";
    let refusal = parse_addressed(&format!("{probed}/x")).expect_err("malformed");
    assert!(!refusal.to_string().contains(probed));
    assert!(!format!("{refusal:?}").contains(probed));
}

#[test]
fn a_prefix_is_validated_and_never_covers_itself() {
    let p = prefix("dc-u-");
    assert!(p.covers(&id("dc-u-1")));
    assert!(p.covers(&id("dc-u-alice")));
    assert!(!p.covers(&id("dc-u-")), "the bare prefix names no user");
    assert!(!p.covers(&id("dc-u")));
    assert!(!p.covers(&id("xdc-u-1")));
    assert!(!p.covers(&id("DC-U-1")), "case-sensitive, like ids");

    let longest = "p".repeat(MAX_ADDRESSED_LEN - 1);
    assert!(SessionPrefix::new(&longest).is_ok());
    for bad in [
        "",
        ".",
        ".x-",
        "a/",
        "a%",
        "*",
        "dc u",
        &"p".repeat(MAX_ADDRESSED_LEN),
    ] {
        assert!(SessionPrefix::new(bad).is_err(), "{bad:?}");
    }
}

fn hosted() -> HostedSessions {
    HostedSessions::new(
        [id("lambo"), id("general")],
        [prefix("dc-u-"), prefix("kl-")],
    )
}

/// Exact names, one prefix, and `"*"` (every pinned name plus every name
/// inside any configured prefix) — and nothing else.
#[test]
fn a_scope_covers_its_names_its_prefix_and_with_star_every_hosted_session() {
    let h = hosted();

    let agents = SessionScope::new([id("lambo"), id("rustydocs")], false, None);
    assert!(agents.covers(&id("lambo"), &h));
    assert!(
        agents.covers(&id("rustydocs"), &h),
        "an exact name need not be pinned"
    );
    assert!(!agents.covers(&id("general"), &h));
    assert!(!agents.covers(&id("dc-u-1"), &h));

    let app = SessionScope::new([], false, Some(prefix("dc-u-")));
    assert!(app.covers(&id("dc-u-1"), &h));
    assert!(!app.covers(&id("kl-1"), &h), "another credential's prefix");
    assert!(!app.covers(&id("lambo"), &h));

    let star = SessionScope::new([], true, None);
    for inside in ["lambo", "general", "dc-u-1", "kl-9"] {
        assert!(star.covers(&id(inside), &h), "{inside}");
    }
    for outside in ["rustydocs", "dc-u-", "other"] {
        assert!(!star.covers(&id(outside), &h), "{outside}");
    }

    assert!(SessionScope::default().is_empty());
    assert!(!SessionScope::default().covers(&id("lambo"), &h));
    assert!(!agents.is_empty() && !app.is_empty() && !star.is_empty());
}

/// Capabilities gate `create`, `erase` and `admin`; plain use needs none.
#[test]
fn capabilities_gate_create_erase_and_admin() {
    let none = SessionCapabilities::default();
    assert!(none.allows(SessionNeed::Use));
    for need in [SessionNeed::Create, SessionNeed::Erase, SessionNeed::Admin] {
        assert!(!none.allows(need), "{need:?}");
    }
    let all = SessionCapabilities {
        create: true,
        erase: true,
        admin: true,
    };
    for need in [
        SessionNeed::Use,
        SessionNeed::Create,
        SessionNeed::Erase,
        SessionNeed::Admin,
    ] {
        assert!(all.allows(need), "{need:?}");
    }
    let erase_only = SessionCapabilities {
        erase: true,
        ..SessionCapabilities::default()
    };
    assert!(erase_only.allows(SessionNeed::Erase));
    assert!(!erase_only.allows(SessionNeed::Create));
    assert!(!erase_only.allows(SessionNeed::Admin));
}

/// Out of scope and missing capability both refuse, with distinct reasons
/// for the log and one rendering for the wire.
#[test]
fn a_grant_refuses_out_of_scope_and_missing_capability() {
    let h = hosted();
    let app = SessionGrant::new(
        "dresscode",
        SessionScope::new([], false, Some(prefix("dc-u-"))),
        SessionCapabilities {
            create: true,
            ..SessionCapabilities::default()
        },
    );
    assert_eq!(app.name(), "dresscode");
    app.authorize(&id("dc-u-1"), SessionNeed::Use, &h)
        .expect("in scope");
    app.authorize(&id("dc-u-1"), SessionNeed::Create, &h)
        .expect("create allowed");
    assert_eq!(
        app.authorize(&id("dc-u-1"), SessionNeed::Erase, &h)
            .expect_err("no erase")
            .reason(),
        RefusalReason::MissingCapability
    );
    assert_eq!(
        app.authorize(&id("lambo"), SessionNeed::Use, &h)
            .expect_err("out of scope")
            .reason(),
        RefusalReason::OutOfScope
    );
    // Out of scope wins over a missing capability, so a probe for an
    // out-of-scope id never learns whether the capability would have mattered.
    assert_eq!(
        app.authorize(&id("lambo"), SessionNeed::Admin, &h)
            .expect_err("out of scope")
            .reason(),
        RefusalReason::OutOfScope
    );
}

/// §6.2's fixed order: the id shape is checked before the scope, so even a
/// grant over every hosted session refuses a malformed id as `Malformed`.
///
/// Mutation: authorize before parsing (or skip the parse) → red.
#[test]
fn authorize_addressed_parses_before_it_checks_scope() {
    let h = hosted();
    let operator = SessionGrant::new(
        "operator",
        SessionScope::new([], true, None),
        SessionCapabilities {
            create: true,
            erase: true,
            admin: true,
        },
    );
    for malformed in ["", "..", "lambo/..", "%2e%2e", "lambo%2F"] {
        assert_eq!(
            authorize_addressed(malformed, &operator, SessionNeed::Use, &h)
                .expect_err(malformed)
                .reason(),
            RefusalReason::Malformed,
            "{malformed:?}"
        );
    }
    assert_eq!(
        authorize_addressed("lambo", &operator, SessionNeed::Erase, &h)
            .expect("in scope")
            .as_str(),
        "lambo"
    );
    assert_eq!(
        authorize_addressed("rustydocs", &operator, SessionNeed::Use, &h)
            .expect_err("not hosted")
            .reason(),
        RefusalReason::OutOfScope
    );
}

#[test]
fn an_addressed_id_converts_to_the_crate_session_type() {
    assert_eq!(
        id("lambo-dev").to_session_id(),
        crate::types::SessionId("lambo-dev".into())
    );
    assert_eq!(id("lambo-dev").to_string(), "lambo-dev");
}

/// Serve `app` on an ephemeral loopback port and return its address.
async fn serve_on_loopback(app: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    addr
}

/// A response header every response through the layer carries, standing in
/// for what the real bearer guard and transport layers add.
async fn mark(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        "x-lambo-test-layer",
        axum::http::HeaderValue::from_static("1"),
    );
    resp
}

async fn refuse() -> Response {
    SessionRefusal::new(RefusalReason::OutOfScope).not_found_response()
}

/// #32 PR 1 review L2: through a real router and a layer added with
/// `.layer` (as `serve` and the portal do), a refused session and an unrouted
/// path are byte-identical on the wire: status line, every header (including
/// the `content-length: 0` the server adds) and body. The session route
/// answers every method, as PR 4's must; an unrouted path answers every
/// method with the 404 too.
///
/// It also pins the two ways to break the claim: `.route_layer` (the layer
/// then skips unrouted paths) and a method-specific route (an unrouted method
/// gets 405 plus `Allow`).
///
/// Mutation: give `not_found_response` a header or body, or switch the real
/// router to `.route_layer` → red.
#[tokio::test]
async fn a_refused_session_and_an_unrouted_path_are_identical_on_the_wire() {
    let app = axum::Router::new()
        .route("/mcp/s/{session}", axum::routing::any(refuse))
        .layer(axum::middleware::from_fn(mark));
    let addr = serve_on_loopback(app).await;

    let reference = on_the_wire(addr, "GET", "/not/routed").await;
    assert!(
        reference.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "{reference}"
    );
    assert!(
        reference.contains("\r\ncontent-length: 0\r\n"),
        "{reference}"
    );
    assert!(
        reference.contains("\r\nx-lambo-test-layer: 1\r\n"),
        "{reference}"
    );
    assert!(reference.ends_with("\r\n\r\n"), "empty body: {reference:?}");
    for method in ["GET", "POST", "DELETE", "PUT"] {
        for path in ["/mcp/s/refused-id", "/not/routed", "/mcp/s"] {
            assert_eq!(
                on_the_wire(addr, method, path).await,
                reference,
                "{method} {path} must be the unrouted 404"
            );
        }
    }

    // `.route_layer` would not hold: the unrouted path skips the layer.
    let route_layered = axum::Router::new()
        .route("/mcp/s/{session}", axum::routing::any(refuse))
        .route_layer(axum::middleware::from_fn(mark));
    let addr = serve_on_loopback(route_layered).await;
    assert_ne!(
        on_the_wire(addr, "GET", "/mcp/s/refused-id").await,
        on_the_wire(addr, "GET", "/not/routed").await,
        "route_layer distinguishes a refused session from an unrouted path"
    );

    // Nor would a GET-only route: another method gets 405 and `Allow`.
    let get_only = axum::Router::new()
        .route("/mcp/s/{session}", axum::routing::get(refuse))
        .layer(axum::middleware::from_fn(mark));
    let addr = serve_on_loopback(get_only).await;
    let post = on_the_wire(addr, "POST", "/mcp/s/refused-id").await;
    assert!(post.starts_with("HTTP/1.1 405"), "{post}");
    assert!(post.to_ascii_lowercase().contains("\r\nallow: "), "{post}");
}

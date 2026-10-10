//! Response headers for `lambo serve-web` (#92).
//!
//! [`security_headers`] is the outermost layer on [`super::routes::router`],
//! outside [`super::auth::host_guard`] and [`super::scope::resolve_session`],
//! so it stamps responses those layers return without calling onward: the
//! Host `403`, the bearer `401`, the uniform `404`, and the scoped `308`.
//!
//! `X-Content-Type-Options: nosniff` goes on every response the router
//! answers. Hyper's own replies to a request it cannot parse (a malformed
//! request line, a `431` for oversized headers) never reach the router.
//! `Content-Security-Policy` goes only on `text/html` (the page handler's
//! `text/html; charset=utf-8`, which `HEAD` keeps because axum runs the
//! `GET` handler and drops the body). A `<meta http-equiv>` policy cannot
//! carry `frame-ancestors` or `nosniff`, so these are response headers.

use axum::extract::Request;
use axum::http::{header, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

/// The page policy. No `'unsafe-inline'` and no `'unsafe-eval'`.
///
/// `img-src` is `'self'` only: the page has no `<img>`, no CSS `url(`, and
/// no `data:` URL. `font-src 'self'` allows no third-party font (the page
/// loads none). `form-action 'self'` matches the picker's `<form>`, which
/// has no `action` and therefore submits to the current document. CSSOM
/// writes (`element.style`) and `element.onclick =` from the external
/// script are allowed by `style-src 'self'` and `script-src 'self'`.
pub(super) const PAGE_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; font-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'";

/// Stamp `nosniff` on every response the router answers, and [`PAGE_CSP`] only when
/// the response media type is `text/html`.
pub(super) async fn security_headers(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let html = response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(content_type_is_html);
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if html {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(PAGE_CSP),
        );
    }
    response
}

/// `text/html` or `text/html; charset=utf-8`, and not a lookalike type.
pub(super) fn content_type_is_html(value: &HeaderValue) -> bool {
    let Ok(raw) = value.to_str() else {
        return false;
    };
    raw.split(';')
        .next()
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/html"))
}

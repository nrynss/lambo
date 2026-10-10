//! Which session a request reads (#4 PR 2, design 3.2 and 3.3).
//!
//! The portal holds no "current session". [`resolve_session`] runs on every
//! request, after the Host guard and **before routing**: for a scoped path
//! it checks the bearer, then the session, and attaches the session the
//! request may read as a [`SessionCtx`]. An unscoped request gets the
//! default session from the gate over the routes (`auth::gate`). Every data
//! handler takes a `SessionCtx`; a request that names no session it may
//! read carries none, and a handler without one answers the uniform 404.
//!
//! | path | session |
//! |---|---|
//! | `/s/{session}` | `GET`/`HEAD`: a `308` to `/s/{session}/` (query kept), so the page's relative `api/...` URLs resolve under the session; other methods: the page route's 405 |
//! | `/s/{session}/` | `{session}`'s page (the same `INDEX_HTML`, plus `no-store` and `Referrer-Policy: same-origin`) |
//! | `/s/{session}/api/{route}` | `{session}`, served by the same `GET`-only route as the alias |
//! | `/s/{session}/api/sessions` | none: the uniform 404 (the listing is unscoped, #4 PR 3; refused by the path check here and again by the listing, which refuses any request marked [`ScopedRequest`]) |
//! | `/s/{session}/{anything else}` | none: the uniform 404 |
//! | `/`, `/api/{route}` | the default session (aliases, design Q10), authorized as `SessionAuthority::authorize_default` rules |
//! | `/app.css`, `/app.js`, `/healthz` | none needed |
//! | `/api/sessions` | none: the caller's own listing, when `[web] list_sessions` registers it |
//!
//! For a scoped path the order is design 3.3's, after the guard's bearer
//! check: the id is the **raw** path segment (never percent-decoded), its
//! shape is [`parse_addressed`](crate::surface::session::parse_addressed)'s,
//! then the grant's scope, then membership of the allowlist. Every refusal is
//! `surface::session`'s uniform 404, for every method, before routing, so it
//! is byte-identical to an unrouted path. In scope the request is rewritten
//! to its unscoped path and routed: a non-`GET` gets the route's own 405
//! (design Q11), an unknown route axum's own 404.
//!
//! Nothing here can reach a store: the checks are `surface::session`'s, in
//! memory, so a refused request makes zero store calls by construction.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::uri::PathAndQuery;
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::auth::{authenticate, unauthorized, Authenticated, Caller};
use super::state::AppState;
use crate::surface::session::{not_found_response, SessionGrant, SessionNeed};
use crate::types::SessionId;

/// The scoped paths' prefix: `/s/{session}/...`.
const SCOPE_PREFIX: &str = "/s/";

/// The listing route, as it would follow `/s/{session}/`: refused there.
const LISTING: &str = "api/sessions";

/// Marks a request that arrived on a scoped path (`/s/{session}/...`),
/// inserted by [`resolve_session`] only. The listing refuses a request that
/// carries it (review I3): [`scoped`] already never routes
/// `/s/{session}/api/sessions`, and this keeps that true should the path
/// check ever miss a spelling.
#[derive(Clone, Copy, Debug)]
pub(super) struct ScopedRequest;

/// The session a request was authorized to read. Inserted by
/// [`resolve_session`] only; a handler extracts it.
#[derive(Clone, Debug)]
pub(super) struct SessionCtx {
    pub(super) session: SessionId,
}

impl<S: Send + Sync> FromRequestParts<S> for SessionCtx {
    type Rejection = Response;

    /// The resolved session, or the uniform 404 when the request carries
    /// none (it named no session it may read).
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<SessionCtx>()
            .cloned()
            .ok_or_else(not_found_response)
    }
}

/// Attach the session this request may read, before routing.
///
/// Runs inside the Host guard. A scoped path is authenticated, authorized
/// and rewritten to its unscoped form (or refused: the 401, then the
/// uniform 404); an unscoped request passes through untouched, to the gate
/// over the routes.
pub(super) async fn resolve_session(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    if !req.uri().path().starts_with(SCOPE_PREFIX) {
        // Unscoped: authenticated and given the default session by the
        // gate over the routes (`auth::gate`), where the bearer check has
        // always run for these paths.
        return next.run(req).await;
    }
    // Scoped: the bearer check runs here, before anything about the
    // session is evaluated (design 3.3), so its 401 is the same for every
    // id, served or not.
    let Some(grant) = authenticate(&state, &req) else {
        return unauthorized();
    };
    let after = &req.uri().path()[SCOPE_PREFIX.len()..];
    let Some(Scoped {
        session,
        target,
        slash,
    }) = scoped(&state, &grant, after)
    else {
        return not_found_response();
    };
    let page = target == "/";
    if page && !slash && (req.method() == Method::GET || req.method() == Method::HEAD) {
        // `/s/{id}` without its slash: the page's script fetches relative
        // `api/...` URLs, which against `/s/{id}` would resolve to
        // `/s/api/...` (review M1). Send the browser to `/s/{id}/`. The id
        // passed the strict charset, so it is safe in a header as is.
        return page_redirect(session.as_str(), req.uri().query());
    }
    // The query string rides along unchanged (`?since=`, `?q=`, `?focus=`).
    let rewritten = match req.uri().query() {
        Some(query) => format!("{target}?{query}"),
        None => target,
    };
    let mut parts = req.uri().clone().into_parts();
    let Ok(path_and_query) = PathAndQuery::try_from(rewritten) else {
        return not_found_response();
    };
    parts.path_and_query = Some(path_and_query);
    let Ok(uri) = Uri::from_parts(parts) else {
        return not_found_response();
    };
    *req.uri_mut() = uri;
    req.extensions_mut().insert(SessionCtx { session });
    req.extensions_mut().insert(Caller(grant));
    req.extensions_mut().insert(Authenticated);
    req.extensions_mut().insert(ScopedRequest);
    let mut response = next.run(req).await;
    if page && response.status().is_success() {
        // Only on the page itself, so a scoped 405 is the alias's 405 byte
        // for byte.
        page_headers(&mut response);
    }
    response
}

/// The page's URL names the session: never cache it, and never send the
/// name onward in a Referer (design 6.1).
fn page_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
}

/// `308` from `/s/{session}` to `/s/{session}/`, the query string kept,
/// with the page's own `no-store` and `Referrer-Policy`. Only for a served
/// session in the grant's scope, so it says no more than the page would.
fn page_redirect(session: &str, query: Option<&str>) -> Response {
    let location = match query {
        Some(query) => format!("{SCOPE_PREFIX}{session}/?{query}"),
        None => format!("{SCOPE_PREFIX}{session}/"),
    };
    let Ok(location) = HeaderValue::try_from(location) else {
        return not_found_response();
    };
    let mut response = (
        StatusCode::PERMANENT_REDIRECT,
        [(header::LOCATION, location)],
    )
        .into_response();
    page_headers(&mut response);
    response
}

/// An authorized scoped path: the session, the unscoped path to route, and
/// whether the id was followed by a `/` (`/s/{id}/...` rather than the bare
/// `/s/{id}`).
struct Scoped {
    session: SessionId,
    target: String,
    slash: bool,
}

/// Authorize the scoped path `after` (what follows `/s/`) for `grant`: the
/// session it names and the unscoped path to route, or `None` for the
/// uniform 404.
///
/// The id is the raw segment up to the next `/`: no percent-decoding, so a
/// `%` (outside the charset) is refused as malformed. Shape, then scope,
/// then the allowlist (design 3.3 and 4.2), all in memory. Only the page
/// (`/s/{id}` or `/s/{id}/`) and the data routes (`/s/{id}/api/...`) exist
/// under a session; anything else is the same 404, in scope or not.
fn scoped(state: &AppState, grant: &SessionGrant, after: &str) -> Option<Scoped> {
    let (raw, rest, slash) = match after.split_once('/') {
        Some((raw, rest)) => (raw, rest, true),
        None => (after, "", false),
    };
    let id = match state.authority.authorize(grant, raw, SessionNeed::Use) {
        Ok(id) => id,
        Err(refusal) => {
            // The reason class only, never the probed id.
            tracing::debug!(reason = %refusal, "serve-web: scoped request refused");
            return None;
        }
    };
    if !state.authority.is_pinned(&id) {
        tracing::debug!("serve-web: scoped request refused: session not served here");
        return None;
    }
    let target = if rest.is_empty() {
        "/".to_string()
    } else if rest == LISTING {
        // The listing is the caller's, not a session's (design 3.2): only
        // at `/api/sessions`, never under `/s/{session}/`.
        return None;
    } else if rest.starts_with("api/") {
        format!("/{rest}")
    } else {
        return None;
    };
    Some(Scoped {
        session: id.to_session_id(),
        target,
        slash,
    })
}

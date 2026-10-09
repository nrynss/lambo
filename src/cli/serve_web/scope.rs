//! Which session a request reads (#4 PR 2, design 3.2 and 3.3).
//!
//! The portal holds no "current session". [`resolve_session`] runs on every
//! request, after the bearer guard and **before routing**, and attaches the
//! session the request may read as a [`SessionCtx`]; every data handler
//! takes one. A request that names no session it may read carries none,
//! and a handler without one answers the uniform 404.
//!
//! | path | session |
//! |---|---|
//! | `/s/{session}`, `/s/{session}/` | `{session}`'s page (the same `INDEX_HTML`, plus `no-store` and `Referrer-Policy: same-origin`) |
//! | `/s/{session}/api/{route}` | `{session}`, served by the same `GET`-only route as the alias |
//! | `/s/{session}/{anything else}` | none: the uniform 404 |
//! | `/`, `/api/{route}` | the default session (aliases, design Q10), authorized as `SessionAuthority::authorize_default` rules |
//! | `/app.css`, `/app.js`, `/healthz` | none needed |
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
use axum::http::{header, HeaderValue, Uri};
use axum::middleware::Next;
use axum::response::Response;

use super::auth::Authenticated;
use super::state::AppState;
use crate::surface::session::{not_found_response, SessionGrant, SessionNeed};
use crate::types::SessionId;

/// The scoped paths' prefix: `/s/{session}/...`.
const SCOPE_PREFIX: &str = "/s/";

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
/// Runs inside the bearer guard, which attached the request's grant. A
/// scoped path is authorized and rewritten to its unscoped form (or refused
/// with the uniform 404); an unscoped request reads the default session when
/// the grant may.
pub(super) async fn resolve_session(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(Authenticated(grant)) = req.extensions().get::<Authenticated>().cloned() else {
        // Served without the guard: refuse rather than serve.
        return not_found_response();
    };
    let Some(after) = req.uri().path().strip_prefix(SCOPE_PREFIX) else {
        if state
            .authority
            .authorize_default(&grant, state.default_session.as_str())
            .is_ok()
        {
            req.extensions_mut().insert(SessionCtx {
                session: state.default_session.clone(),
            });
        }
        return next.run(req).await;
    };
    let Some((session, target)) = scoped(&state, &grant, after) else {
        return not_found_response();
    };
    let page = target == "/";
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
    let mut response = next.run(req).await;
    if page && response.status().is_success() {
        // The page's URL names the session: never cache it, and never send
        // the name onward in a Referer (design 6.1). Only on the page
        // itself, so a scoped 405 is the alias's 405 byte for byte.
        let headers = response.headers_mut();
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("same-origin"),
        );
    }
    response
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
fn scoped(state: &AppState, grant: &SessionGrant, after: &str) -> Option<(SessionId, String)> {
    let (raw, rest) = after.split_once('/').unwrap_or((after, ""));
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
    } else if rest.starts_with("api/") {
        format!("/{rest}")
    } else {
        return None;
    };
    Some((id.to_session_id(), target))
}

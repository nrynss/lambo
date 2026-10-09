//! Which session a request reads (#4 PR 2, design 3.2 and 3.3).
//!
//! The portal holds no "current session". [`resolve_session`] runs on every
//! request, after the bearer guard and **before routing**, and attaches the
//! session the request may read as a [`SessionCtx`]; every data handler
//! takes one. A request that names no session it may read carries none,
//! and a handler without one answers the uniform 404.
//!
//! The unscoped routes (`/`, `/api/...`) are aliases for the default
//! session (design Q10), authorized as `SessionAuthority::authorize_default`
//! rules.
//!
//! Nothing here can reach a store: the checks are `surface::session`'s, in
//! memory, so a refused request makes zero store calls by construction.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;

use super::auth::Authenticated;
use super::state::AppState;
use crate::surface::session::not_found_response;
use crate::types::SessionId;

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
/// Runs inside the bearer guard, which attached the request's grant. An
/// unscoped request reads the default session when the grant may.
pub(super) async fn resolve_session(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(Authenticated(grant)) = req.extensions().get::<Authenticated>().cloned() else {
        // Served without the guard: refuse rather than serve.
        return not_found_response();
    };
    if state
        .authority
        .authorize_default(&grant, state.default_session.as_str())
        .is_ok()
    {
        req.extensions_mut().insert(SessionCtx {
            session: state.default_session.clone(),
        });
    }
    next.run(req).await
}

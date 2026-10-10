//! The operator surface of an HTTP serve (#32 PR 7, design §6.3): outside
//! MCP, behind credentials an agent's client does not hold.
//!
//! | route | needs | answers |
//! |---|---|---|
//! | `POST /admin/s/{session}/erase` | `erase`, the session in scope | the #23 `EraseReport` |
//! | `GET /admin/sessions` | `admin` | the slots in the credential's scope |
//!
//! **Erase is not an MCP tool**, and the MCP tool list does not change:
//! agents receive the tool list, and #23's point is that an agent cannot
//! erase memory it was asked to keep. A route outside MCP, behind a
//! credential the agent's client does not hold, enforces that structurally.
//!
//! # The order every admin request follows (§6.2)
//!
//! 1. The guard's bearer check (401), as for every route.
//! 2. The addressed id's shape, then the grant's scope, then its capability,
//!    all in memory ([`SessionAuthority::authorize`](crate::surface::session::SessionAuthority)
//!    with [`SessionNeed::Erase`], or the `admin` flag for the listing). Any
//!    refusal is the uniform 404, byte-identical to an unrouted path, so a
//!    credential without `erase`, or one whose scope does not cover the id,
//!    learns nothing, and the store is not called.
//! 3. Only then is the method checked (405 inside scope), the body read and
//!    the confirm compared (400), and the registry asked.
//!
//! # `POST /admin/s/{session}/erase`
//!
//! Body `{"confirm": "<session>"}`, the CLI's typo guard on the wire.
//!
//! | status | when |
//! |---|---|
//! | 200 | erased; the body is the `EraseReport` JSON, the same bytes `lambo erase-session` prints. A repeat reports `already_absent: true` |
//! | 400 | the body is not `{"confirm": ...}`, or the confirm does not repeat the session id; nothing was erased |
//! | 405 | not a `POST` (inside scope only) |
//! | 408 | the body did not arrive within `REQUEST_BODY_TIMEOUT`; nothing was erased |
//! | 409 | a live writer in another process holds the session; nothing was erased (the CLI's exit 1) |
//! | 500 | the store failed; the body says whether the session is durably erased (repeat the request), untouched (an attached session's unflushed writes are discarded all the same), or unknown (repeat the request) |
//! | 503 | the session is being erased, attached or detached, or the serve is shutting down; `Retry-After` |
//!
//! # `GET /admin/sessions`
//!
//! `{"sessions": [...]}`: one row per session in the credential's scope
//! (`registry::SlotView`), the only enumeration this serve offers, and only
//! to a credential with `admin`. Reads RAM only, but not for free: each live
//! session's size is a scan of its concepts under its graph's read lock, so
//! it runs on the blocking pool, not on a runtime worker (#32 PR 7 review
//! L5). An operator-only cost, bounded by the credential's request rate.
//!
//! `POST /admin/s/{s}/detach` (design §6.3, for #33's cut-over) is not
//! served yet: a pinned session's detach is re-elected by the background
//! retry within seconds, so it needs PR 6's detach semantics first.

use std::sync::Arc;

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use super::authority::{Authenticated, ServeAuthority};
use super::http_guards::REQUEST_BODY_TIMEOUT;
use super::registry::{EraseAnswer, SessionRegistry};
use crate::store::erase::Tombstone;
use crate::surface::session::{
    parse_addressed, RefusalReason, SessionGrant, SessionNeed, SessionRefusal,
};

/// The route prefix an admin-addressed session id follows.
const ADMIN_SESSION_PREFIX: &str = "/admin/s/";

/// The suffix of the erase route.
const ERASE_SUFFIX: &str = "/erase";

/// The largest erase body read: `{"confirm": "<128-byte id>"}` with room
/// for whitespace. A larger body is refused (400) once this much of it has
/// been read; the rest is not.
const MAX_ADMIN_BODY_BYTES: usize = 1024;

/// What the admin routes need.
#[derive(Clone)]
struct Admin {
    registry: Arc<SessionRegistry>,
    authority: Arc<ServeAuthority>,
}

/// The admin routes, without the guards (`transport::http_app` puts them
/// behind the same guard layer as the MCP routes). Both answer every
/// method, so a refused request is the uniform 404 whatever the method.
pub(super) fn admin_router(
    registry: Arc<SessionRegistry>,
    authority: Arc<ServeAuthority>,
) -> axum::Router {
    axum::Router::new()
        .route("/admin/sessions", axum::routing::any(list_sessions))
        .route(
            "/admin/s/{session}/erase",
            axum::routing::any(erase_session),
        )
        .with_state(Admin {
            registry,
            authority,
        })
}

/// The grant the guard resolved, or `None` (never past the guard).
fn granted(req: &axum::extract::Request) -> Option<Arc<SessionGrant>> {
    req.extensions()
        .get::<Authenticated>()
        .map(|Authenticated(grant)| Arc::clone(grant))
}

/// The uniform 404, logged by reason and credential name, never by id.
fn refused(grant: Option<&SessionGrant>, refusal: SessionRefusal) -> Response {
    tracing::debug!(
        credential = grant.map_or("(none)", SessionGrant::name),
        reason = %refusal,
        "admin http: refused a request"
    );
    refusal.not_found_response()
}

/// A plain-text answer with a trailing newline, like the MCP routes' 503s.
fn text(status: StatusCode, body: impl Into<String>) -> Response {
    let mut body = body.into();
    body.push('\n');
    (status, body).into_response()
}

/// 405 for a method the route does not take, reached only inside scope.
fn wrong_method(allow: &'static str) -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, allow)],
        format!("this route takes {allow} only\n"),
    )
        .into_response()
}

/// `GET /admin/sessions`: the slots in the caller's scope, for a credential
/// with `admin`.
async fn list_sessions(
    axum::extract::State(admin): axum::extract::State<Admin>,
    req: axum::extract::Request,
) -> Response {
    let Some(grant) = granted(&req) else {
        return refused(None, SessionRefusal::new(RefusalReason::OutOfScope));
    };
    if !grant.capabilities().admin {
        return refused(
            Some(&grant),
            SessionRefusal::new(RefusalReason::MissingCapability),
        );
    }
    if req.method() != axum::http::Method::GET {
        return wrong_method("GET");
    }
    // Off the runtime's workers (#32 PR 7 review L5): each live session's
    // size is a scan of its concepts under its graph's read lock, which a
    // writer can hold, so many large sessions would otherwise pin a worker
    // that every other request needs.
    let registry = Arc::clone(&admin.registry);
    let Ok(views) = tokio::task::spawn_blocking(move || registry.slot_views()).await else {
        return text(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the session listing failed (see the serve log)",
        );
    };
    let sessions: Vec<_> = views
        .into_iter()
        .filter(|view| in_admin_scope(&admin.authority, &grant, &view.session, view.pinned))
        .collect();
    let body = serde_json::json!({ "sessions": sessions });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        format!("{body}\n"),
    )
        .into_response()
}

/// Whether `grant` may see session `id` in the listing: the same scope test
/// the session routes apply (`SessionNeed::Admin`), and for a pinned name
/// the strict charset refuses (a one-session serve's loose `--session`), a
/// scope over every pinned session, as `/mcp` authorizes it.
fn in_admin_scope(
    authority: &ServeAuthority,
    grant: &SessionGrant,
    id: &str,
    pinned: bool,
) -> bool {
    if parse_addressed(id).is_ok() {
        return authority.authorize(grant, id, SessionNeed::Admin).is_ok();
    }
    pinned && grant.scope().covers_every_pinned()
}

/// The erase body (`{"confirm": "<session>"}`).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EraseBody {
    confirm: String,
}

/// `POST /admin/s/{session}/erase`.
async fn erase_session(
    axum::extract::State(admin): axum::extract::State<Admin>,
    req: axum::extract::Request,
) -> Response {
    let Some(grant) = granted(&req) else {
        return refused(None, SessionRefusal::new(RefusalReason::OutOfScope));
    };
    // The raw path segment, never percent-decoded (#32 decision 16), as
    // the MCP route reads it.
    let raw = req
        .uri()
        .path()
        .strip_prefix(ADMIN_SESSION_PREFIX)
        .and_then(|rest| rest.strip_suffix(ERASE_SUFFIX))
        .unwrap_or_default()
        .to_string();
    let id = match admin.authority.authorize(&grant, &raw, SessionNeed::Erase) {
        Ok(id) => id,
        Err(refusal) => return refused(Some(&grant), refusal),
    };
    if req.method() != axum::http::Method::POST {
        return wrong_method("POST");
    }
    // The body, bounded in size and time, read only now: an unauthorized
    // request is answered without a byte of it read.
    let body = match tokio::time::timeout(
        REQUEST_BODY_TIMEOUT,
        axum::body::to_bytes(req.into_body(), MAX_ADMIN_BODY_BYTES),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(_)) => {
            return text(
                StatusCode::BAD_REQUEST,
                format!(
                    "the body must be {{\"confirm\": \"<session>\"}} (at most \
                     {MAX_ADMIN_BODY_BYTES} bytes); nothing was erased"
                ),
            );
        }
        Err(_) => {
            return text(
                StatusCode::REQUEST_TIMEOUT,
                "the request body did not arrive in time; nothing was erased",
            );
        }
    };
    let confirm = match serde_json::from_slice::<EraseBody>(&body) {
        Ok(body) => body.confirm,
        Err(_) => {
            return text(
                StatusCode::BAD_REQUEST,
                "the body must be {\"confirm\": \"<session>\"}; nothing was erased",
            );
        }
    };
    if confirm != id.as_str() {
        return text(
            StatusCode::BAD_REQUEST,
            "confirm must repeat the session id in the path exactly; nothing was erased",
        );
    }
    tracing::info!(
        session = %id,
        credential = grant.name(),
        "admin http: erase requested"
    );
    match admin.registry.erase(id.as_str()).await {
        EraseAnswer::Erased(report) => match serde_json::to_string(&report) {
            Ok(json) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                format!("{json}\n"),
            )
                .into_response(),
            Err(e) => text(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the session was erased, but the report could not be rendered: {e}"),
            ),
        },
        EraseAnswer::HeldElsewhere { holder, age } => text(
            StatusCode::CONFLICT,
            format!(
                "a live writer holds this session ({holder}, holding the single-writer lease for \
                 {}s); nothing was erased. Stop that writer (a clean stop flushes and releases \
                 the lease), then retry",
                age.as_secs()
            ),
        ),
        EraseAnswer::Busy { retry_after } => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(
                header::RETRY_AFTER,
                retry_after.as_secs().max(1).to_string(),
            )],
            "this session is being erased, attached or detached, or the server is shutting \
             down: retry later\n",
        )
            .into_response(),
        EraseAnswer::Failed {
            error,
            erased: Tombstone::Yes,
        } => text(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "the session is erased from the durable store, but a later step failed: {error}. \
                 Repeat the request to finish it"
            ),
        ),
        EraseAnswer::Failed {
            error,
            erased: Tombstone::No,
        } => text(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "the erase failed and nothing was erased: {error}. If this server held the \
                 session, writes it had acknowledged but not yet flushed were discarded"
            ),
        ),
        // #32 PR 7 review L2: the store failed and the lease row could not
        // be read back, so whether it committed is not known.
        EraseAnswer::Failed {
            error,
            erased: Tombstone::Unknown,
        } => text(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "the erase failed and its outcome is unknown: {error}. Repeat the request (it is \
                 idempotent)"
            ),
        ),
    }
}

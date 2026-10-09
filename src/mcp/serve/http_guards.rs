//! T8.7, the HTTP surface's hardening: the bearer token, the fail-closed
//! bind rule, the request-rate bucket, the concurrent-session cap and the
//! body-size ceiling, applied by [`guard_request`] in front of rmcp.
//!
//! The comparison itself is `crate::surface::bearer`'s, shared with the web
//! portal; this module owns the serve-side token type, its environment
//! precedence and the order the checks run in.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

use super::Transport;
use crate::types::LamboError;

/// Environment variable holding the HTTP bearer token. **Takes precedence over
/// `--auth-token`**: a process manager can inject the secret without it ever
/// appearing in a command line (where `ps` and shell history would expose it).
pub const AUTH_TOKEN_ENV: &str = "LAMBO_AUTH_TOKEN";

/// Default ceiling on concurrently live MCP sessions (T82-16).
///
/// The HTTP transport mints one session per `initialize`, and before this
/// nothing bounded that: a client that reconnects in a loop grows
/// `LocalSessionManager`'s map — and its per-session worker tasks — without
/// limit, against a process that owns exactly one `Memory`. 32 is chosen to sit
/// far above any real fan-in (the demo uses one; a swarm uses a handful of
/// long-lived sessions) while still being a bound.
pub const DEFAULT_MAX_SESSIONS: usize = 32;

/// Default sustained request rate for the HTTP transport, in requests/second.
///
/// Generous on purpose: this is an abuse bound, not a quality-of-service knob,
/// and a limit that a legitimate agent can trip is a limit that gets disabled.
pub const DEFAULT_RATE_LIMIT_RPS: u32 = 50;

/// Burst allowance as a multiple of [`DEFAULT_RATE_LIMIT_RPS`] — the bucket
/// capacity. An agent that fires a batch of calls after an idle pause should not
/// be refused for being bursty; only a *sustained* excess is.
pub(super) const RATE_LIMIT_BURST_FACTOR: u32 = 2;

/// A bearer token that cannot be printed.
///
/// The redacting [`std::fmt::Debug`] is the point: `ServeOptions` and clap's
/// `Commands` both derive `Debug`, and `serve` logs its options-adjacent fields
/// at startup. Holding the secret in a type with no `Display` and a redacting
/// `Debug` makes "never logged" a property of the type rather than a promise
/// every future caller has to keep.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(String);

impl SecretToken {
    /// Reject empty and whitespace-only tokens.
    ///
    /// Fail closed rather than quietly accept: an empty `LAMBO_AUTH_TOKEN` is
    /// almost always an unset variable that expanded to nothing, and treating it
    /// as a valid credential would authenticate every request that sends
    /// `Authorization: Bearer `.
    pub fn new(raw: impl Into<String>) -> Result<Self, String> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(
                "auth token is empty — pass a non-empty secret, or omit it entirely to \
                        run unauthenticated on loopback"
                    .into(),
            );
        }
        Ok(Self(raw))
    }

    pub(super) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretToken(<redacted>)")
    }
}

impl std::str::FromStr for SecretToken {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Does an `Authorization` header value carry the expected bearer token?
///
/// The scheme is matched case-insensitively (RFC 7235 §2.1); the credential
/// itself is compared byte-for-byte in constant time. The parse and the
/// comparison are `crate::surface::bearer`'s, shared with the web portal (#28).
pub(super) fn bearer_ok(header: Option<&str>, expected: &SecretToken) -> bool {
    crate::surface::bearer::bearer_ok(header, expected.as_bytes())
}

/// Resolve the effective token from the flag and the environment.
///
/// **The environment wins.** A token on the command line is visible in `ps` and
/// in shell history, so the deployment-friendly channel is the one that takes
/// precedence — an operator who exports [`AUTH_TOKEN_ENV`] does not have to also
/// remember to drop the flag.
///
/// A *set but empty* environment variable is an error rather than a silent
/// fallback to the flag: that shape is nearly always an unset variable that
/// expanded to nothing, and resolving it to "whatever the flag said" would make
/// a typo look like it worked.
pub fn resolve_auth_token(flag: Option<SecretToken>) -> Result<Option<SecretToken>, LamboError> {
    resolve_auth_token_from(flag, std::env::var(AUTH_TOKEN_ENV).ok())
}

pub(super) fn resolve_auth_token_from(
    flag: Option<SecretToken>,
    env: Option<String>,
) -> Result<Option<SecretToken>, LamboError> {
    match env {
        Some(raw) => SecretToken::new(raw)
            .map(Some)
            .map_err(|e| LamboError::Config(format!("{AUTH_TOKEN_ENV}: {e}"))),
        None => Ok(flag),
    }
}

/// Fail closed when a non-loopback bind has no token (T82-16).
///
/// The rule, and why it is a *startup* check rather than a warning:
///
/// * **stdio** is process-local — the client already owns the process it
///   spawned, so there is nothing for a token to protect. Unaffected.
/// * **HTTP on loopback** keeps today's behaviour: auth is optional, because
///   reaching the socket already means local code execution. A token is still
///   honoured if given.
/// * **HTTP on anything else** — `0.0.0.0`, a LAN address, a public one —
///   *requires* a token. This process is a session **writer** with an
///   unauthenticated write surface; binding it to the world without a
///   credential is not a configuration worth starting, so `serve` refuses
///   rather than coming up and hoping a proxy is in front of it.
///
/// ## What J2 changed, and what it deliberately did not
///
/// J2 made every holder bind a **second** listener — the session endpoint, a
/// unix socket, bound even under `--transport stdio` so a refused serve can
/// proxy to it. That threatened this function's ordering argument, which is that
/// running before the lease-taking attach means *"refusing here means no lease
/// is taken"*, so a misconfigured start leaves nothing behind and the operator's
/// retry is not blocked by a lease their own refused start is holding. (That
/// attach was `build_memory` before J2 and is `resolve_role` after it — see
/// J2-R1-7; `serve` does not call `build_memory` at all any more.)
///
/// **That sentence is still literally true, by construction rather than by
/// luck.** The endpoint's *address* is derived (a function of session and store
/// — see [`crate::mcp::endpoint::SessionEndpoint`]) so it can be published into
/// the lease row by the very acquire that takes the lease, while the *socket* is
/// bound only afterwards, only by the winner. A serve that loses the lease
/// therefore binds nothing, creates nothing and unlinks nothing — exactly the
/// property this ordering exists to give.
///
/// **And J2 adds no new pre-lease refusal at all.** The round-1 review found
/// that the endpoint's `sun_path` length check made an over-long base directory
/// a hard startup failure while a *failed bind* deliberately degraded, so the
/// harsher outcome sat on the cheaper problem (J2-R1-5). It now degrades too:
/// `SessionEndpoint::for_store` logs at ERROR and yields `None`, and this
/// function is still the only pre-lease *refusal* on the serve path besides
/// `authorize_ledger`. The endpoint derivation is still in the pre-lease group,
/// for the reason that group exists — it creates nothing and binds nothing. Its
/// one filesystem access is a read-only `canonicalize` of the store path
/// (J2-R1-2), which leaves nothing behind and so cannot block an operator's
/// retry.
///
/// ## What J4 changed, found by JE2E-6's sweep
///
/// J4 put a **third** member in the pre-lease group: `Ledger::open` and the
/// `startup` line it appends, moved above `resolve_role` so a serve that is
/// about to *lose* the lease has already left an artifact recording that it
/// tried. That member is the first one in this group that does leave something
/// behind — a ledger file, and a line in it — so the group's shorthand
/// ("nothing is created before the lease") stops being literally true and the
/// real claim has to be stated instead.
///
/// The real claim is unchanged, and it is about **retries, not about traces**:
/// refusing here takes no lease, so an operator who fixes the token and starts
/// again is not blocked by a lease their own refused start is holding. An
/// append-only observability file blocks nothing, is the artifact J4 exists to
/// leave, and is written only when the operator asked for it with `--ledger`.
/// The ordering between the two is also deliberate: `authorize_ledger` runs
/// *above* the open, so the misconfigured `--ledger-heartbeat`-without-`--ledger`
/// pairing is still refused before any file is touched.
///
/// ## What J6 changed: nothing here, and that is the claim
///
/// J6 added a signal registration at the **acquire** — see [`EarlyShutdown`](super::EarlyShutdown) —
/// so "the arming" is no longer one point on the far side of `resolve_role`.
/// This group is unaffected: it runs above the acquire, so it is now the
/// **pre-arm** group as well as the pre-lease one, and every member of it still
/// runs under the default signal disposition. That is deliberate rather than
/// incidental. A start that is about to refuse here, or that is about to sit in
/// the election for up to `ELECTION_BUDGET`, must stay killable by a plain
/// SIGTERM (J2-R1-7), and it holds nothing — no lease, no tail, no graph — that
/// a handler could save. J6 adds no member to this group and takes none away.
pub(super) fn authorize_bind(
    transport: Transport,
    bind: IpAddr,
    token: Option<&SecretToken>,
) -> Result<(), LamboError> {
    if transport != Transport::Http || bind.is_loopback() || token.is_some() {
        return Ok(());
    }
    Err(LamboError::Config(format!(
        "refusing to start: --transport http --bind {bind} exposes an unauthenticated MCP \
         *writer* beyond loopback. Set {AUTH_TOKEN_ENV} (or pass --auth-token) to require \
         'Authorization: Bearer <token>' on every request, or bind 127.0.0.1 and reach it \
         through a tunnel or an authenticating proxy."
    )))
}

/// A token bucket over the whole HTTP transport.
///
/// **Scope, stated honestly:** this limits *HTTP requests to `/mcp`*, not
/// `tools/call` specifically. Singling out `tools/call` means buffering and
/// re-injecting every request body to read the JSON-RPC `method` — real
/// machinery, and a correctness risk on the streaming transport — for a
/// distinction that barely matters here: on streamable HTTP each `tools/call` is
/// its own POST, so a request-rate bound *is* a call-rate bound plus a few
/// cheap handshake requests. The wider net is the cheaper and safer cut.
///
/// The limit is **global**, not per-connection: per-connection state would be
/// trivially defeated by opening more connections, which is exactly the abuse
/// shape the session cap and this bound exist to bound together.
pub(crate) struct RateLimiter {
    pub(super) capacity: f64,
    pub(super) refill_per_sec: f64,
    pub(super) state: parking_lot::Mutex<BucketState>,
}

pub(super) struct BucketState {
    pub(super) tokens: f64,
    pub(super) last: Instant,
}

impl RateLimiter {
    /// `None` when `rps == 0` — the documented way to disable the limit.
    pub(super) fn new(rps: u32, now: Instant) -> Option<Self> {
        if rps == 0 {
            return None;
        }
        let capacity = f64::from(rps.saturating_mul(RATE_LIMIT_BURST_FACTOR));
        Some(Self {
            capacity,
            refill_per_sec: f64::from(rps),
            state: parking_lot::Mutex::new(BucketState {
                tokens: capacity,
                last: now,
            }),
        })
    }

    /// Take one token if the bucket has one. `now` is a parameter so the tests
    /// drive the refill deterministically instead of sleeping.
    pub(super) fn try_acquire_at(&self, now: Instant) -> bool {
        let mut st = self.state.lock();
        let elapsed = now.saturating_duration_since(st.last).as_secs_f64();
        st.last = now;
        st.tokens = (st.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        if st.tokens >= 1.0 {
            st.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    pub(super) fn try_acquire(&self) -> bool {
        self.try_acquire_at(Instant::now())
    }
}

/// How many MCP sessions are live right now.
///
/// A trait so the cap is testable without standing up a real transport:
/// `LocalSessionManager` is the production implementation, a counter is the
/// test one.
#[async_trait::async_trait]
pub(crate) trait LiveSessions: Send + Sync + 'static {
    async fn live(&self) -> usize;
}

#[async_trait::async_trait]
impl LiveSessions for LocalSessionManager {
    async fn live(&self) -> usize {
        // `sessions` is rmcp's own public map, and it is the authority: a DELETE
        // /mcp or a dropped worker removes the entry, so counting it here needs
        // no bookkeeping of ours that could drift from the truth.
        self.sessions.read().await.len()
    }
}

/// The three checks every HTTP request passes before it reaches rmcp.
#[derive(Clone)]
pub(crate) struct HttpGuard {
    pub(super) auth: Option<SecretToken>,
    pub(super) max_sessions: usize,
    pub(super) live: Arc<dyn LiveSessions>,
    pub(super) rate: Option<Arc<RateLimiter>>,
}

/// Ceiling on the size of a single HTTP request body (T82-16 remainder).
///
/// The tool layer already bounds every client string (16 KiB) and the per-call
/// concept count (64 ≈ ~1 MiB of content), and the rate limit bounds request
/// *count* — but the transport itself imposed no ceiling, so a body padded with
/// rejected or oversized fields still incurred parse + validation cost before
/// the tool layer refused it. This caps the *declared* body of a request before
/// any of it is parsed. A body that arrives without `Content-Length` (chunked)
/// keeps the tool-layer caps + the rate limit as its bound.
pub(super) const MAX_HTTP_BODY_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB

/// Is this the request that would mint a **new** MCP session?
///
/// Streamable HTTP assigns the session id in the `initialize` response, so the
/// one request that arrives without an `Mcp-Session-Id` — and can create state —
/// is that POST. Everything else either carries the header or is a GET/DELETE
/// against an existing session, and must not be counted against the cap.
pub(super) fn opens_a_new_session(req: &axum::extract::Request) -> bool {
    req.method() == axum::http::Method::POST
        && req.headers().get("Mcp-Session-Id").is_none()
        && req.headers().get("Last-Event-ID").is_none()
}

/// Auth, then rate, then the session cap — in that order, deliberately.
///
/// Authentication runs **first and alone**: an unauthenticated caller must not
/// be able to consume rate-limit budget or read the live-session count (a 503
/// vs 401 difference would leak how loaded the server is), and it must be
/// refused before rmcp sees the request at all — before any session is minted,
/// any worker task spawned, or any body parsed.
pub(super) async fn guard_request(
    axum::extract::State(guard): axum::extract::State<HttpGuard>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    if let Some(expected) = &guard.auth {
        let presented = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if !bearer_ok(presented, expected) {
            // Deliberately terse and identical for "no header" and "wrong
            // token": the difference is not the caller's business, and the
            // token itself is never echoed.
            tracing::warn!(
                had_header = presented.is_some(),
                "mcp http: rejected an unauthenticated request"
            );
            return (
                StatusCode::UNAUTHORIZED,
                [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
                "unauthorized: this endpoint requires 'Authorization: Bearer <token>'\n",
            )
                .into_response();
        }
    }

    if let Some(rate) = &guard.rate
        && !rate.try_acquire()
    {
        tracing::warn!("mcp http: request refused by the rate limit");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(axum::http::header::RETRY_AFTER, "1")],
            "rate limit exceeded: slow down and retry\n",
        )
            .into_response();
    }

    if opens_a_new_session(&req) {
        let live = guard.live.live().await;
        if live >= guard.max_sessions {
            tracing::warn!(
                live,
                max = guard.max_sessions,
                "mcp http: refusing a new session — at the concurrent-session cap"
            );
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::RETRY_AFTER, "5")],
                format!(
                    "at the concurrent-session cap ({live}/{max} sessions live): this server \
                     will not open another. Close an idle session (HTTP DELETE /mcp with its \
                     Mcp-Session-Id), or restart with a higher --max-sessions.\n",
                    max = guard.max_sessions
                ),
            )
                .into_response();
        }
    }

    // T8.7 body-size ceiling — checked before the body is streamed to rmcp.
    // A declared body over the cap is refused up front: parse and validation
    // never see it, so amplification through an oversized body is bounded.
    if let Some(cl) = req.headers().get(axum::http::header::CONTENT_LENGTH)
        && let Some(len) = cl.to_str().ok().and_then(|s| s.parse::<u64>().ok())
        && len > MAX_HTTP_BODY_BYTES
    {
        tracing::warn!(
            len,
            max = MAX_HTTP_BODY_BYTES,
            "mcp http: refusing an oversized request body"
        );
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("request body too large (limit {MAX_HTTP_BODY_BYTES} bytes)\n"),
        )
            .into_response();
    }

    next.run(req).await
}

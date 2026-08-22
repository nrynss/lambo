//! `lambo serve` — process lifecycle for the MCP server.
//!
//! One process owns one session (spec §2.2). This module builds **one**
//! [`Memory`] from **one** [`ResolvedBackends`], serves it over stdio or
//! streamable HTTP, and guarantees [`Memory::close`] runs on the way out so the
//! final flush happens.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::transport::io::stdio;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServiceExt;

use crate::ledger::Ledger;
use crate::mcp::endpoint::SessionEndpoint;
use crate::mcp::server::LamboServer;
use crate::memory::Memory;
use crate::resolve::{resolve_from_config_path, ResolvedBackends};
use crate::store::lease;
use crate::types::{DaemonEvent, LamboError};

/// How long a transport gets to wind itself down after the shutdown signal
/// before it is dropped and `close()` runs anyway.
///
/// This bound is the whole point (R1/T82-2). `axum::serve(..).with_graceful_shutdown`
/// waits for **every in-flight connection to finish**, and a streamable-HTTP MCP
/// client holds its server→client SSE channel open for the life of the session
/// (kept alive by `sse_keep_alive`, so it never idles out). Without a deadline,
/// graceful shutdown never returns, `Memory::close` never runs, and the tail is
/// lost — the exact durability failure the signal handling exists to prevent.
/// A dropped connection is recoverable; a dropped write-behind tail is not.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Hard bound on the final [`Memory::close`] (R2-b).
///
/// `close()` is otherwise unbounded: a store that hangs on the final flush would
/// hang the process with the shutdown path already spent, which is exactly the
/// durability-vs-liveness trade the grace windows above exist to resolve.
/// Bounding it is safe *for liveness* — dropping the `close()` future returns
/// the drained tail to the front of the graph log (see `Memory::close`'s
/// *Cancellation* section), leaves the session closed to writers, and latches no
/// success, so the process can exit instead of wedging. It is **not** safe for
/// durability: that "returned to the log" is the *in-memory* write-behind log
/// (`src/graph/mod.rs`), and there is **no on-disk WAL**. On the serve path an
/// abandoned close is immediately followed by process exit, so the un-flushed
/// tail dies with the process — it is LOST, not recoverable on restart. (The
/// within-process retry semantics `Memory::close` documents apply only to a
/// caller that stays alive and calls `close()` again; `serve` does not.) Larger
/// than [`SHUTDOWN_GRACE`] because by the time close runs the transport is
/// already down and this is the last thing standing between the process and exit.
///
/// This is the budget for the **whole** close phase, split between the flush
/// attempt ([`CLOSE_FLUSH_GRACE`]) and the lease release that follows an
/// abandoned one ([`LEASE_RELEASE_GRACE`]).
const CLOSE_GRACE: Duration = Duration::from_secs(10);

/// What [`Memory::close`] itself gets — [`CLOSE_GRACE`] minus the slice reserved
/// for [`LEASE_RELEASE_GRACE`].
///
/// The live review (L82-1) found that a close which blows its deadline is
/// *dropped*, so the lease release inside it never runs and the session stays
/// wedged for the full `LEASE_TTL`. Fixing that needs a window to release in,
/// and that window is carved **out of** `CLOSE_GRACE` rather than added on top:
/// [`SHUTDOWN_BUDGET`] is a number operators have already sized their
/// supervisor's SIGKILL timeout against, and a durability bug is not a reason to
/// quietly move it.
///
/// The two seconds this costs the flush are affordable because the same finding
/// was fixed at the root: [`crate::store::batch`] turned a flush from one
/// network round-trip per mutation into one per few hundred rows, so the
/// 784-mutation tail that could not drain in 10 s now plans into single-digit
/// statements.
///
/// `pub(crate)` so the burst-drain regression test in `memory` can assert
/// against the real budget instead of a copy of the number.
pub(crate) const CLOSE_FLUSH_GRACE: Duration = Duration::from_secs(8);

/// Bound on the best-effort lease release that follows an abandoned `close()`
/// (L82-1).
///
/// One `DELETE ... WHERE session_id = $1 AND holder = $2` against a cluster the
/// flush was just talking to. Two seconds is several round-trips' worth; if it
/// does not land in that, the row lapses at TTL exactly as it did before this
/// existed, and the process still exits.
const LEASE_RELEASE_GRACE: Duration = Duration::from_secs(2);

/// Build-time invariant: the close phase's two halves add up to its budget.
///
/// An edit that grows either without shrinking the other — or that grows
/// [`CLOSE_GRACE`] expecting the flush to receive it — fails the build.
const _: () = assert!(
    CLOSE_FLUSH_GRACE.as_secs() + LEASE_RELEASE_GRACE.as_secs() == CLOSE_GRACE.as_secs(),
    "CLOSE_FLUSH_GRACE + LEASE_RELEASE_GRACE must be exactly CLOSE_GRACE — the close phase is \
     those two steps in series and nothing else, and SHUTDOWN_BUDGET is sized on CLOSE_GRACE",
);

/// Documented worst-case wall-clock a clean shutdown can take, end to end (R4).
///
/// The shutdown is two bounded phases in series: the transport winds down within
/// [`SHUTDOWN_GRACE`] (rmcp's own graceful drain happens *inside* that window —
/// `run_until_shutdown` gives the whole transport, drain included, exactly
/// `SHUTDOWN_GRACE` after cancel), then the final flush runs within
/// [`CLOSE_GRACE`]. The only work outside these two is `event_pump.abort()` and
/// process teardown, both effectively instant. So the true aggregate cap is
/// `SHUTDOWN_GRACE + CLOSE_GRACE`, and this is the number an operator must budget
/// for: a supervisor's SIGKILL escalation (systemd `TimeoutStopSec`, Kubernetes
/// `terminationGracePeriodSeconds` — default 30 s) must exceed it, or the final
/// flush is cut off and the tail is lost. The compile-time guard just below (and
/// `the_grace_windows_are_sane`) pins the sum to this budget so a later bump to
/// either window cannot silently push the aggregate past what a supervisor allows.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(15);

/// Build-time invariant: the end-to-end shutdown cost fits [`SHUTDOWN_BUDGET`].
/// A future edit that pushes `SHUTDOWN_GRACE + CLOSE_GRACE` over the budget fails
/// the build, not just the test (`Duration::as_secs` is `const`).
const _: () = assert!(
    SHUTDOWN_GRACE.as_secs() + CLOSE_GRACE.as_secs() <= SHUTDOWN_BUDGET.as_secs(),
    "SHUTDOWN_GRACE + CLOSE_GRACE exceeds SHUTDOWN_BUDGET — a supervisor's SIGKILL timeout \
     is sized against the budget; lower a window or justify raising the budget",
);

/// Build-time invariant: the single-writer lease TTL comfortably outlasts the
/// whole shutdown budget (T8.6).
///
/// `Memory::close` releases the lease on a graceful shutdown, but the release
/// only lands after the transport has wound down and the final flush has run —
/// up to [`SHUTDOWN_BUDGET`] later. If the TTL were not larger than that budget,
/// a slow-but-graceful close could let the lease **expire mid-shutdown**, briefly
/// admitting a second writer while the first is still flushing its tail — the
/// exact hazard the lease exists to prevent. `LEASE_TTL` (45s) is 3× the budget;
/// this pins the relationship so a later bump to either window cannot silently
/// invert it.
const _: () = assert!(
    lease::LEASE_TTL.as_secs() > SHUTDOWN_BUDGET.as_secs(),
    "LEASE_TTL must exceed SHUTDOWN_BUDGET so a slow-but-graceful close releases the lease \
     rather than letting it expire mid-shutdown (T8.6)",
);

/// Which transport `lambo serve` should listen on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Newline-delimited JSON-RPC over stdin/stdout — what an MCP client
    /// launching `lambo serve` as a subprocess speaks.
    Stdio,
    /// Streamable HTTP (`POST /mcp`), for the T8.5 demo app and remote clients.
    Http,
}

impl std::str::FromStr for Transport {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stdio" => Ok(Self::Stdio),
            "http" => Ok(Self::Http),
            other => Err(format!(
                "unknown transport '{other}' (expected 'stdio' or 'http')"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// T8.7 — HTTP surface hardening
// ---------------------------------------------------------------------------

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
const RATE_LIMIT_BURST_FACTOR: u32 = 2;

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

    fn as_bytes(&self) -> &[u8] {
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

/// Compare a presented token against the expected one without an early exit.
///
/// The accumulate-then-test shape keeps the time taken independent of *where*
/// the first differing byte falls, so a caller cannot recover the secret byte by
/// byte from response timing. Two deliberate details:
///
/// * the loop runs over the **presented** input and indexes the expected token
///   modulo its length, so a wrong-length guess does not return early and
///   thereby disclose the expected length;
/// * [`std::hint::black_box`] stops the optimiser from proving the accumulator
///   can be short-circuited.
///
/// The honest caveat: the *duration* still scales with the presented length,
/// which is attacker-controlled and reveals nothing about the secret. This is
/// the same guarantee `subtle::ConstantTimeEq` gives on slices, reached without
/// adding a dependency for one comparison.
fn tokens_match(presented: &[u8], expected: &[u8]) -> bool {
    if expected.is_empty() {
        // Unreachable via `SecretToken::new`, which rejects empty tokens; a
        // belt-and-braces guard so the `%` below cannot divide by zero.
        return false;
    }
    let mut diff = (presented.len() ^ expected.len()) as u64;
    for (i, byte) in presented.iter().enumerate() {
        diff |= u64::from(byte ^ expected[i % expected.len()]);
    }
    std::hint::black_box(diff) == 0
}

/// Does an `Authorization` header value carry the expected bearer token?
///
/// The scheme is matched case-insensitively (RFC 7235 §2.1); the credential
/// itself is compared byte-for-byte in constant time.
fn bearer_ok(header: Option<&str>, expected: &SecretToken) -> bool {
    let Some(raw) = header else {
        return false;
    };
    let raw = raw.trim();
    let Some((scheme, credential)) = raw.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    tokens_match(credential.trim().as_bytes(), expected.as_bytes())
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

fn resolve_auth_token_from(
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
fn authorize_bind(
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
    capacity: f64,
    refill_per_sec: f64,
    state: parking_lot::Mutex<BucketState>,
}

struct BucketState {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// `None` when `rps == 0` — the documented way to disable the limit.
    fn new(rps: u32, now: Instant) -> Option<Self> {
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
    fn try_acquire_at(&self, now: Instant) -> bool {
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

    fn try_acquire(&self) -> bool {
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
    auth: Option<SecretToken>,
    max_sessions: usize,
    live: Arc<dyn LiveSessions>,
    rate: Option<Arc<RateLimiter>>,
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
const MAX_HTTP_BODY_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB

/// Is this the request that would mint a **new** MCP session?
///
/// Streamable HTTP assigns the session id in the `initialize` response, so the
/// one request that arrives without an `Mcp-Session-Id` — and can create state —
/// is that POST. Everything else either carries the header or is a GET/DELETE
/// against an existing session, and must not be counted against the cap.
fn opens_a_new_session(req: &axum::extract::Request) -> bool {
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
async fn guard_request(
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

    if let Some(rate) = &guard.rate {
        if !rate.try_acquire() {
            tracing::warn!("mcp http: request refused by the rate limit");
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(axum::http::header::RETRY_AFTER, "1")],
                "rate limit exceeded: slow down and retry\n",
            )
                .into_response();
        }
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
    if let Some(cl) = req.headers().get(axum::http::header::CONTENT_LENGTH) {
        if let Some(len) = cl.to_str().ok().and_then(|s| s.parse::<u64>().ok()) {
            if len > MAX_HTTP_BODY_BYTES {
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
        }
    }

    next.run(req).await
}

/// Everything `serve` needs that is not in `lambo.toml`.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    /// Session this process owns.
    pub session: String,
    /// Agent identity this process writes as. See the attribution note on
    /// [`LamboServer`] — `Memory` binds one agent per session handle.
    pub agent: String,
    pub transport: Transport,
    pub port: u16,
    /// Bind address for `--transport http`. Defaults to loopback. Binding
    /// anywhere else requires [`ServeOptions::auth_token`] — see
    /// `authorize_bind`.
    pub bind: IpAddr,
    /// Bearer token required on every HTTP request (T8.7). `None` is allowed
    /// only on loopback; [`AUTH_TOKEN_ENV`] overrides whatever the flag said.
    pub auth_token: Option<SecretToken>,
    /// Ceiling on concurrently live MCP sessions.
    pub max_sessions: usize,
    /// Sustained requests/second on the HTTP transport; `0` disables the limit.
    pub rate_limit_rps: u32,
    /// I1 — append one JSONL line per MCP tool call to this path. `None` (the
    /// default) is off: no writer thread, no per-call facts, and `lambo_stats`
    /// reports the payload it reported before the ledger existed.
    pub ledger: Option<PathBuf>,
    /// I2 — append a `stats` heartbeat line on this interval. Requires
    /// [`ServeOptions::ledger`]; `None` is off.
    pub ledger_heartbeat: Option<Duration>,
}

impl ServeOptions {
    pub fn new(session: impl Into<String>, agent: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            agent: agent.into(),
            transport: Transport::Stdio,
            port: 7700,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auth_token: None,
            max_sessions: DEFAULT_MAX_SESSIONS,
            rate_limit_rps: DEFAULT_RATE_LIMIT_RPS,
            ledger: None,
            ledger_heartbeat: None,
        }
    }
}

/// Both ledger configuration errors, refused in one place.
///
/// **`--ledger-heartbeat` without `--ledger`.** An operator who asked for
/// heartbeats and got a server with no ledger at all would find out a day later,
/// from an absent file. Refusing at startup costs them one flag; the alternative
/// costs them the run.
///
/// **A zero heartbeat interval.** `tokio::time::interval` panics on a zero
/// period, and a heartbeat that fired as fast as the executor allows would be a
/// flood, not a heartbeat. The guard used to live only in `main.rs`, which left
/// two holes: a `serve()` caller that is not the CLI got a silently-panicked
/// heartbeat task, and the two configuration errors exited with two different
/// codes (1 and 2) for the same class of mistake. Both are refused here now, so
/// both take the same path out — and the CLI's wording is kept verbatim, since it
/// is the message an operator has already learned to read.
///
/// Split out from [`serve`] so it is testable without a store, a transport, or a
/// lease, and called before any lease is taken.
pub fn authorize_ledger(opts: &ServeOptions) -> Result<(), LamboError> {
    match (&opts.ledger, opts.ledger_heartbeat) {
        (None, Some(secs)) => Err(LamboError::Config(format!(
            "--ledger-heartbeat {}s was given without --ledger: heartbeat lines are written TO \
             the call ledger, so there is nowhere to put them. Pass --ledger <path> as well, or \
             drop --ledger-heartbeat.",
            secs.as_secs()
        ))),
        (_, Some(every)) if every.is_zero() => Err(LamboError::Config(
            "--ledger-heartbeat must be at least 1 second (0 given); omit the flag to disable \
             heartbeats"
                .to_string(),
        )),
        _ => Ok(()),
    }
}

/// Append a `stats` heartbeat line every `every` (I2).
///
/// The first line lands immediately rather than one interval in: it stamps the
/// binary's version and sha at the moment the session attached, which is the
/// "which pinned binary produced this stretch of ledger" question the heartbeat
/// exists to answer. Waiting an interval would leave the first stretch
/// unattributed.
///
/// Runs until aborted. `Memory::stats()` is synchronous and holds no lock
/// across an await (spec §6.4) — it takes the graph read lock, counts, and
/// releases before this function's next `tick()`.
async fn heartbeat_loop(server: LamboServer, ledger: Arc<Ledger>, every: Duration) {
    let mut ticker = tokio::time::interval(every);
    // Skip missed ticks rather than firing a burst to catch up: a heartbeat
    // backlog after a stall would be a pile of near-identical lines stamped
    // microseconds apart, which is noise, not history.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        ledger.append(&server.heartbeat_line());
    }
}

/// Build the single [`Memory`] this process owns from an **already-resolved**
/// [`ResolvedBackends`].
///
/// **Level B, single construction site.** This function deliberately does *not*
/// resolve: the caller resolves once and hands the result in, so there is
/// exactly one store and one embedder per process and no second config pass.
/// Fail-closed behaviour — uncompiled `kind`, unknown TOML key, store×embedder
/// dim mismatch — lives in that one resolve; see [`resolve_serve_backends`].
///
/// # `serve` does not call this any more (J2-R1-7)
///
/// It is a **library entry point**, kept because it is `pub` and re-exported at
/// `crate::mcp`, and because "build the one `Memory` a serve-shaped process
/// owns, with the `[daemon]` cadence applied" is a useful thing for an embedder
/// to be able to ask for in one call. J2 replaced the serve path's use of it
/// with [`serve_builder`] plus `resolve_role`, because the startup election has
/// to retry the *attach* against the same configuration and therefore needs the
/// builder rather than the built `Memory`. `rg build_memory` finds no call site
/// in this tree.
///
/// The consequence for a reader: comments describing serve startup name
/// `resolve_role`, not this function. The round-1 review found nine sites that
/// still named this one; they were rewritten in the same commit as this
/// paragraph.
///
/// [`ResolvedBackends`]: crate::resolve::ResolvedBackends
pub async fn build_memory(
    opts: &ServeOptions,
    backends: ResolvedBackends,
    endpoint: Option<&SessionEndpoint>,
) -> Result<Memory, LamboError> {
    // Cadence overrides from `[daemon]` reach the writer here. Without this the
    // daemon always runs at Config::default() and `gc_interval` in lambo.toml
    // would parse, validate, and then do nothing at all.
    serve_builder(opts, backends, endpoint, None)
        .build()
        .await
        .map_err(explain_startup_failure)
}

/// The one [`MemoryBuilder`] a serve process configures.
///
/// Split out of [`build_memory`] so J2's startup election can retry the attach
/// against the **same** configuration: `MemoryBuilder` is `Clone` and every
/// backend inside it is an `Arc`, so a retry is a clone rather than a second
/// resolve. Level B's single construction site is unchanged — `main` still
/// resolves once and this is still the only place `Memory::builder()` is called
/// on the serve path. Since that split, this — not `build_memory` — is what the
/// serve path uses; `build_memory` became a library-only entry point that
/// delegates here (J2-R1-7).
fn serve_builder(
    opts: &ServeOptions,
    backends: ResolvedBackends,
    endpoint: Option<&SessionEndpoint>,
    ledger: Option<Arc<Ledger>>,
) -> crate::memory::MemoryBuilder {
    let config = backends.config.clone();
    let mut builder = Memory::builder()
        .session(opts.session.clone())
        .agent(opts.agent.clone())
        .config(config);
    // J2: published by the acquire that takes the lease, so a live row always
    // names the current holder's address. The socket itself is bound by the
    // caller AFTER this returns — see `authorize_bind`. `None` (a store no
    // second process can see) publishes nothing, which is the honest row for a
    // holder nothing can reach.
    if let Some(endpoint) = endpoint {
        builder = builder.endpoint(endpoint.published());
    }
    // J4: the ledger this process opens pre-lease (so its own conflict and
    // write-intent completion lines ride it) is handed straight into the
    // Memory it builds.
    builder = builder.ledger(ledger);
    builder.backends(backends)
}

/// Turn a raw driver error at attach time into an actionable message.
///
/// Pointing `serve` at a fresh SQLite file or an unmigrated Cockroach database
/// failed with nothing but `no such table: sessions` (R1/T82-10). Schema
/// bootstrap belongs to `lambo provision` (T8.3) and `serve` deliberately does
/// not auto-init — but the *message* is T8.2's, and "run provision" is the one
/// thing the operator needs to be told.
fn explain_startup_failure(err: LamboError) -> LamboError {
    let text = err.to_string();
    let lower = text.to_lowercase();
    // The shapes the two SQL backends use for "the schema isn't there":
    // SQLite says `no such table`, Postgres/Cockroach say `relation "x" does
    // not exist` (SQLSTATE 42P01) or `undefined_table`.
    let unprovisioned = lower.contains("no such table")
        || lower.contains("does not exist")
        || lower.contains("undefined_table")
        || lower.contains("42p01");
    if unprovisioned {
        LamboError::Config(format!(
            "session store is not provisioned — run 'lambo provision' \
             (or scripts/provision.sh) against this store first, then retry. \
             Underlying error: {text}"
        ))
    } else {
        err
    }
}

/// The one resolve a serve process performs (Level B).
///
/// Thin by design — it exists so the single construction site is named and
/// greppable, not to add behaviour. Config precedence (`--config`, then
/// `LAMBO_CONFIG`, then `./lambo.toml`, then defaults; env overrides file) and
/// every fail-closed check live inside `resolve_from_config_path`.
pub fn resolve_serve_backends(config: Option<&Path>) -> Result<ResolvedBackends, LamboError> {
    resolve_from_config_path(config).map_err(|e| LamboError::Config(e.to_string()))
}

/// Slack added to a lease's own remaining time before the election decides
/// whether waiting for it is worth doing.
///
/// Absorbs store-clock skew and a refresh landing between the row read and this
/// decision: a row whose `expires_at` is a second away may already have been
/// pushed out by a live holder, and a clock that disagrees slightly must not make
/// the election give up a moment too early.
///
/// It does **not** absorb "one missed refresh interval", which is what this
/// docstring used to claim (J2-R2-2's sweep). That interval is
/// [`lease::LEASE_HEARTBEAT_INTERVAL`] = 15s, three times this value, and it does
/// not need absorbing: a holder that misses a refresh has two more chances
/// inside one [`lease::LEASE_TTL`], and if it misses all three the lease is
/// *supposed* to lapse. 5s covers the race between reading the row and acting on
/// it, which is the only thing that can make an honest arithmetic answer wrong.
///
/// Where the number lands is worked out at [`ELECTION_BUDGET`]: it is subtracted
/// from the budget, so the largest lapse the election will wait out is
/// `ELECTION_BUDGET - ELECTION_SLACK`.
const ELECTION_SLACK: Duration = Duration::from_secs(5);

/// The longest the startup election may block the client that spawned this
/// process.
///
/// # This is a *client tolerance* budget, not a lease budget (J2-L2)
///
/// It used to be `LEASE_TTL + ELECTION_SLACK` — 50 seconds — reasoned entirely
/// from the lease: a holder that stopped heartbeating loses its row within one
/// TTL, so a wait of one TTL plus slack either finds a live hub or wins the
/// lease. That reasoning is sound about the *lease* and wrong about the *client*.
/// An MCP client spawns this process and waits for it; if it waits too long it
/// does not report "starting", it reports **failed**, and a failed server has no
/// tools at all. Measured live: `opencode` 1.18.18 gave up at **31.96s** and the
/// model then reported having no lambo tools — a recoverable wait turned into a
/// total outage, which is the shape J2 exists to remove.
///
/// 20s, so there is real margin under the tightest tolerance measured (12s)
/// for the client's own spawn, this process's resolve, and a loaded machine.
/// It is deliberately a *different kind* of number from `LEASE_TTL` and must not
/// be re-derived from it: they answer to different constraints, and the lease's
/// is the one that may not move.
///
/// # What replaced the guarantee it used to give
///
/// Nothing waits blindly any more. The lease row says when the current holder's
/// lease expires, so [`resolve_role`] does arithmetic instead of hoping: if the
/// row lapses inside this budget it waits exactly that long and takes the
/// session; if it does not, it refuses **immediately** and names the seconds. A
/// fast, actionable refusal beats spending a client's entire startup gate to
/// arrive at the same place.
///
/// # What the wait actually catches, derived from the constants (J2-R2-2)
///
/// This docstring used to end "— and in the majority of real cases (a lease
/// expires uniformly somewhere inside its TTL) the wait still succeeds". The
/// parenthesis was false and it carried the conclusion with it. A lease's
/// remaining time is **not** uniform on `[0, LEASE_TTL]`: a live holder refreshes
/// every [`lease::LEASE_HEARTBEAT_INTERVAL`] and each refresh sets
/// `expires_at = now + LEASE_TTL`, so an **abrupt** death (a `kill -9`, a panic,
/// a lost machine — the case this budget exists for) leaves
/// `[LEASE_TTL - LEASE_HEARTBEAT_INTERVAL, LEASE_TTL]` = **[30s, 45s]** of lease
/// behind. Never less than 30.
///
/// [`waiting_fits`] waits only while `lapses_in + ELECTION_SLACK <= left`, so it
/// refuses whenever `lapses_in` exceeds `ELECTION_BUDGET - ELECTION_SLACK` = 15s
/// at the very best, and about 13s in practice once the attach attempt and the
/// endpoint probe have spent some of the budget. Every value in [30, 45] is above
/// that. Therefore:
///
/// * **a client starting promptly after an abrupt holder death is refused —
///   always**, not in a minority of cases. Measured live: lease freshly
///   refreshed with 40s remaining, holder `kill -9`'d, a fresh serve started
///   immediately, refused in **2.12s** with "does not lapse for 38s … Retry in
///   39s".
/// * the wait succeeds for a client starting roughly **17–32s after** the death —
///   late enough that under ~13s of lease remains, early enough that the row has
///   not already lapsed.
/// * once the lease has lapsed there is no wait at all: the next start attaches.
///
/// **None of this is an argument for moving the budget**, and the refuse-fast
/// behaviour is correct: `opencode`'s measured 31.96s tolerance means the
/// pre-J2-L2 50s wait failed that client anyway, so a 2.12s refusal carrying a
/// retry interval is strictly better for it. What changed is that the
/// justification written down is now the one that survives contact with the
/// constants — the third instance of the register failure J2-R1-7 was — and that
/// [`waiting_fits`] exists so a test asserts the arithmetic instead of a
/// docstring asserting it.
const ELECTION_BUDGET: Duration = Duration::from_secs(20);

/// How often the startup election retries while no holder is reachable.
const ELECTION_RETRY: Duration = Duration::from_secs(1);

/// Would waiting for the current holder's lease to lapse fit inside the budget
/// that is left? (J2-L2's arithmetic, extracted so J2-R2-2 can pin it.)
///
/// `lapses_in` is the row's `expires_at` minus now; `left` is what remains of
/// [`ELECTION_BUDGET`]. [`ELECTION_SLACK`] is added to the lapse, not subtracted
/// from the budget, because it exists to cover the holder possibly refreshing
/// once more — see its own doc.
///
/// A function rather than an inline comparison because the *claim about* it was
/// wrong twice in two rounds. See [`ELECTION_BUDGET`] for what the numbers make
/// true, and `an_abrupt_holder_death_outlasts_the_election_budget` for the
/// assertion.
fn waiting_fits(lapses_in: Duration, left: Duration) -> bool {
    lapses_in + ELECTION_SLACK <= left
}

/// What this process turned out to be.
enum Role {
    /// It won the lease: a real writer, with a graph, a tail and a socket to
    /// bind. Boxed for the same reason [`crate::memory::Attach`] boxes it.
    Holder(Box<Memory>),
    /// The lease is held by a reachable local holder: forward to it (J2).
    Proxy(Box<crate::mcp::proxy::HubProxy>),
}

/// Can this process reach the holder named by `held`, right now?
///
/// `Ok(())` means yes and the caller may become a proxy. `Err(why)` carries the
/// operator-facing reason, which the election either logs while it waits or
/// folds into its refusal.
///
/// **Probe only.** The connection is dropped rather than carried in, because
/// `HubProxy::run` re-reads the row and dials whoever is current at *that*
/// moment — the wedge invariant depends on the row being the authority, not on a
/// connection taken at startup.
async fn probe_holder(
    held: &crate::memory::LeaseHeldElsewhere,
    endpoint: &SessionEndpoint,
    our_host: &str,
) -> Result<(), String> {
    let address = crate::mcp::proxy::proxyable(&held.current, endpoint, our_host)
        .map_err(|why| why.explain())?;
    crate::mcp::proxy::dial_dir(&address)
        .map_err(|e| format!("the holder's endpoint is not safe to dial ({e})"))?;
    match crate::mcp::proxy::connect(&address).await {
        Ok(stream) => {
            drop(stream);
            Ok(())
        }
        Err(e) => Err(format!("{ENDPOINT_NOT_ACCEPTING} ({e})")),
    }
}

/// The one probe outcome that is strong evidence the holder is **gone** rather
/// than merely unreachable (J2-R2-3).
///
/// A named constant, not a literal at each site, so
/// [`correct_the_refresh_claim`] cannot drift out of step with the message it
/// looks for — the class of bug this whole round is about.
const ENDPOINT_NOT_ACCEPTING: &str = "the holder's endpoint is not accepting connections";

/// What replaces `crate::memory::STILL_REFRESHING_CLAUSE` when the probe says the endpoint is not answering.
const PROBABLY_DEAD: &str = "has not yet let its lease lapse — but its endpoint is not \
     answering, so it has most likely died";

/// Repair the lease refusal before folding a probe outcome into it (J2-R2-3).
///
/// `build_attach`'s message says the holder "is still refreshing" its lease,
/// which is the right thing to say to a serve that simply lost a race. J2-L2
/// newly composes that message with [`probe_holder`]'s outcome, and when the
/// outcome is [`ENDPOINT_NOT_ACCEPTING`] the composition contradicts itself
/// inside one paragraph — with the **false half first**, so an operator reading
/// the opening sentence goes looking for a live process that no longer exists.
/// The probe is the better evidence of the two: a lease row is a claim made up to
/// [`lease::LEASE_HEARTBEAT_INTERVAL`] ago, a refused connect is now.
///
/// Only that one clause changes, and only on that one outcome. Every other
/// refusal — another host, no endpoint published, a foreign address name — is a
/// live holder this process merely cannot forward to, and "is still refreshing
/// it" is exactly true for it.
fn correct_the_refresh_claim(message: &str, outcome: &str) -> String {
    if outcome.contains(ENDPOINT_NOT_ACCEPTING) {
        message.replacen(crate::memory::STILL_REFRESHING_CLAUSE, PROBABLY_DEAD, 1)
    } else {
        message.to_string()
    }
}

/// Decide whether this process holds the session or proxies to whoever does.
///
/// # The election, and why it lives HERE and not in the proxy
///
/// This function may re-attempt the acquire — that is the whole election — and
/// it is the **only** place allowed to. It runs before a single byte has been
/// exchanged with this process's own MCP client, so winning the lease here makes
/// this process a real holder that can actually serve. Once
/// [`crate::mcp::proxy::HubProxy::run`] is entered the client has handshaken
/// with the *holder*, and a lease won after that point could not be served —
/// the process would heartbeat a session it cannot answer, wedging every other
/// process on the machine. `HubProxy` therefore only ever reads the row.
/// Acquisition and promotion are one decision; see that function's invariant.
///
/// # What it waits for, and what it refuses
///
/// A refusal returns immediately, unchanged from pre-J2 behaviour, when there is
/// no prospect of proxying at all:
///
/// * `--transport http` — the proxy's client-facing wire is a line pipe and
///   streamable HTTP is not line-framed. Exits 1 exactly as before.
/// * a store no second process can see, so there is no endpoint at all.
///
/// Otherwise it waits for either a reachable holder (→ proxy) or the lease to
/// lapse (→ hold). Waiting is the right trade against the exit-1 this workstream
/// exists to remove: a slow start that ends in working memory beats a fast start
/// that ends in none, and progress is logged so the delay is never silent.
///
/// **But only a wait a client will sit through** (J2-L2). The wait is bounded by
/// [`ELECTION_BUDGET`], which is a *client tolerance* number and not a lease
/// number, and nothing waits blindly: the lease row carries `expires_at`, so
/// each pass asks whether the lapse falls inside the budget that is left. If it
/// does not, this refuses **at once** and names the seconds, because burning a
/// client's entire startup gate to arrive at the same refusal is how a
/// recoverable wait turns into "this server has no tools" — measured live at
/// 31.96 s on one real client.
async fn resolve_role(
    opts: &ServeOptions,
    builder: &crate::memory::MemoryBuilder,
    endpoint: Option<&SessionEndpoint>,
    ledger: &Option<Arc<Ledger>>,
) -> Result<Role, LamboError> {
    let our_host =
        lease::LeaseHolder::for_this_process(&crate::types::AgentId::new(&opts.agent)).host;
    let deadline = Instant::now() + ELECTION_BUDGET;
    let mut waited_for = None;
    let session = crate::types::SessionId::new(&opts.session);
    let my_token =
        lease::LeaseHolder::for_this_process(&crate::types::AgentId::new(&opts.agent)).token();
    // J4: the loser side of a refused acquisition is recorded in
    // [`record_refused_loser`] below.
    loop {
        let held = match builder
            .clone()
            .build_attach()
            .await
            .map_err(explain_startup_failure)?
        {
            crate::memory::Attach::Attached(mem) => {
                if let Some(reason) = waited_for {
                    tracing::info!(
                        %reason,
                        "lambo serve: the previous holder's lease lapsed — taking the session"
                    );
                }
                return Ok(Role::Holder(mem));
            }
            crate::memory::Attach::Held(held) => held,
        };

        // No prospect of proxying: refuse now, with exactly the message a
        // pre-J2 serve produced.
        if opts.transport != Transport::Stdio {
            record_refused_loser(
                ledger,
                &held.store,
                &session,
                &opts.agent,
                &my_token,
                &held.current.holder,
            )
            .await;
            tracing::warn!(
                "lambo serve: --transport http cannot proxy to the session holder (its \
                 client-facing wire is not line-framed); refusing as it did before J2"
            );
            return Err(LamboError::Conflict(held.message));
        }
        let Some(endpoint) = endpoint else {
            record_refused_loser(
                ledger,
                &held.store,
                &session,
                &opts.agent,
                &my_token,
                &held.current.holder,
            )
            .await;
            return Err(LamboError::Conflict(held.message));
        };
        // Can we forward to this holder? Three checks, no guessing.
        // The address to dial, which may sit in a directory this process would
        // not have derived — see `proxy::proxyable` and J2-L1.
        let outcome = match probe_holder(&held, endpoint, &our_host).await {
            Ok(()) => {
                // J4 — the runner-up side of "from both sides" on the proxy
                // path: this loser was refused the acquisition even though it
                // can still proxy to the holder, so the holder must learn it
                // was contended. Best-effort, exactly like the terminal-refusal
                // exits above; the refusal decision never changes.
                record_refused_loser(
                    ledger,
                    &held.store,
                    &session,
                    &opts.agent,
                    &my_token,
                    &held.current.holder,
                )
                .await;
                return Ok(Role::Proxy(Box::new(crate::mcp::proxy::HubProxy::new(
                    crate::types::SessionId::new(&opts.session),
                    endpoint.clone(),
                    Arc::clone(&held.store),
                    our_host,
                    opts.agent.clone(),
                    ledger.clone(),
                ))));
            }
            Err(why) => why,
        };

        // J2-L2. Would waiting even help, inside the budget a client will
        // tolerate? The row says when this holder's lease expires, so this is
        // arithmetic rather than hope — and refusing in milliseconds with the
        // number in the message beats burning the client's whole startup gate to
        // arrive at the same refusal.
        let lapses_in = (held.current.expires_at - chrono::Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO);
        let left = deadline.saturating_duration_since(Instant::now());
        if !waiting_fits(lapses_in, left) {
            record_refused_loser(
                ledger,
                &held.store,
                &session,
                &opts.agent,
                &my_token,
                &held.current.holder,
            )
            .await;
            return Err(LamboError::Conflict(format!(
                "{} {outcome} That holder's lease does not lapse for {}s, and this process will \
                 not block the client that spawned it for longer than {}s waiting — an MCP \
                 client that gives up on a slow server reports NO TOOLS rather than 'starting', \
                 which would be a worse outcome than this message. Retry in {}s, or stop the \
                 other holder.",
                correct_the_refresh_claim(&held.message, &outcome),
                lapses_in.as_secs(),
                ELECTION_BUDGET.as_secs(),
                lapses_in.as_secs() + 1
            )));
        }

        // Not proxyable *yet*. The two live cases are a CLI verb holding the
        // lease for one command and a holder that died without releasing; both
        // resolve inside one TTL, the first by finishing and the second by
        if Instant::now() >= deadline {
            record_refused_loser(
                ledger,
                &held.store,
                &session,
                &opts.agent,
                &my_token,
                &held.current.holder,
            )
            .await;
            // Backstop. The arithmetic above normally refuses first; this fires
            // for a row whose `expires_at` keeps moving (a live holder that
            // refreshes but cannot be forwarded to) or has already passed
            // without the row being swept.
            return Err(LamboError::Conflict(format!(
                "{} {outcome} Waited {}s for that holder's lease to lapse or its endpoint to \
                 answer, and neither happened.",
                correct_the_refresh_claim(&held.message, &outcome),
                ELECTION_BUDGET.as_secs()
            )));
        }
        if waited_for.as_deref() != Some(outcome.as_str()) {
            tracing::info!(
                reason = %outcome,
                budget_secs = ELECTION_BUDGET.as_secs(),
                lapses_in_secs = lapses_in.as_secs(),
                "lambo serve: the session is held by a writer this process cannot forward to — \
                 waiting for its lease to lapse so this process can take the session"
            );
        }
        waited_for = Some(outcome);
        tokio::time::sleep(ELECTION_RETRY).await;
    }
}

/// The J4 pre-lease startup line: this serve's intent to acquire the
/// single-writer lease, written to the ledger before `resolve_role` makes its
/// first acquire attempt. See [`crate::ledger::startup_line`].
fn serve_startup_line(
    opts: &ServeOptions,
    _endpoint: &Option<SessionEndpoint>,
) -> serde_json::Value {
    crate::ledger::startup_line(
        &opts.session,
        &opts.agent,
        match opts.transport {
            Transport::Stdio => "stdio",
            Transport::Http => "http",
        },
    )
}

/// J4 — the loser side of a refused acquisition: append the serve's own
/// `lease:refused` line to its ledger AND persist the fact to the store so the
/// incumbent holder learns it turned away a takeover ("from both sides").
/// Best-effort: neither failure is allowed to change the refusal decision.
async fn record_refused_loser(
    ledger: &Option<Arc<Ledger>>,
    store: &Arc<dyn crate::store::GraphStore>,
    session: &crate::types::SessionId,
    agent: &str,
    my_token: &str,
    holder: &str,
) {
    if let Some(ledger) = ledger {
        ledger.append(&crate::ledger::lease_line(
            "refused",
            "loser",
            &session.to_string(),
            agent,
            holder,
            None,
        ));
    }
    let _ = store.record_lease_refusal(session, my_token, holder).await;
}

/// How often the holder's refusal-recorder task re-checks the store.
const REFUSAL_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// J4 — the holder side of a refused acquisition. Spawned only in the holder
/// branch of [`serve`]: it polls the store for lease refusals this process
/// turned away and appends a `lease:refused_takeover` line for each it has not
/// yet recorded. This and [`record_refused_loser`] together make "a refused
/// lease acquisition appears in the ledger from both sides" true.
///
/// Refusals recorded against a *previous* holder are filtered out by matching
/// `current_holder` against this process's own lease token, and each refusal is
/// deduped by (refused_by, at) so a repeated poll never double-logs. The read
/// window starts a little before the poller's own start so no refusal at the
/// acquire boundary is missed; the dedup set is what keeps it exact.
async fn record_refused_takeovers(
    store: Arc<dyn crate::store::GraphStore>,
    session: crate::types::SessionId,
    agent: crate::types::AgentId,
    my_token: String,
    ledger: Arc<Ledger>,
) {
    let mut seen: std::collections::HashSet<(String, String)> = Default::default();
    let since = chrono::Utc::now()
        - chrono::Duration::from_std(lease::LEASE_TTL)
            .unwrap_or_else(|_| chrono::Duration::seconds(0));
    loop {
        tokio::time::sleep(REFUSAL_POLL_INTERVAL).await;
        match store.pending_lease_refusals(&session, since).await {
            Ok(refusals) => {
                for r in refusals {
                    if r.current_holder != my_token {
                        continue;
                    }
                    let key = (r.refused_by.clone(), r.at.to_rfc3339());
                    if seen.insert(key) {
                        ledger.append(&crate::ledger::lease_line(
                            "refused_takeover",
                            "holder",
                            &session.to_string(),
                            &agent.to_string(),
                            &r.refused_by,
                            Some(serde_json::json!({ "at": r.at.to_rfc3339() })),
                        ));
                    }
                }
            }
            Err(_) => {
                // A seed / store blip; the next poll retries.
            }
        }
    }
}

/// Run the MCP server to completion, then close the session.
///
/// [`Memory::close`] runs on **every** exit path — clean client disconnect,
/// SIGINT/SIGTERM, or a transport error — because the tail is only durable once
/// it has run. Its error is surfaced: an `Err` from `close()` means the tail is
/// *not* durable and was kept (T8.1 semantics).
///
/// **The single-writer lease (T8.6) rides the same exit paths.** `resolve_role`
/// acquired the lease as this process attached — failing closed here if another
/// writer holds it *and* cannot be proxied to, and returning `Role::Proxy`
/// rather than reaching this function if it can (J2). A successful `close()`
/// **releases**
/// it so the next writer takes over at once. On the one exit that abandons
/// `close()` — the `close_bounded` timeout or a second signal — the lease is
/// *not* released and instead lapses at [`lease::LEASE_TTL`], exactly as it would
/// on a crash. That is why the TTL is sized to outlast `SHUTDOWN_BUDGET`: a
/// graceful-but-slow close still holds a valid lease at the moment it releases.
///
/// Both transports route their shutdown through `run_until_shutdown`, so the
/// signal path is the same on each: cancel, wait up to `SHUTDOWN_GRACE`, then
/// drop the transport and close regardless. A client that will not let go
/// cannot hold the tail hostage.
///
/// The shutdown signal is armed **before** the transport handoff (R2-a): a
/// single, continuously-live registration threads through the pre-handshake
/// window (the stdio handshake, the HTTP `bind`) and the transport itself, so a
/// signal in *any* of those still reaches [`Memory::close`] instead of hitting
/// the default disposition and killing the process with the tail un-flushed.
pub async fn serve(opts: ServeOptions, backends: ResolvedBackends) -> Result<(), LamboError> {
    // T8.7, and it runs FIRST — before `resolve_role`, which attaches to the
    // store and takes the single-writer lease. A misconfigured bind must cost
    // nothing and leave nothing behind: refusing here means no lease is taken,
    // so the operator's retry after setting a token is not blocked by the
    // lease their own refused start would otherwise be holding.
    authorize_bind(opts.transport, opts.bind, opts.auth_token.as_ref())?;
    // Same argument as the bind check: refuse before any lease is taken.
    authorize_ledger(&opts)?;
    // J2, and it belongs in this pre-lease group for the same reason the two
    // above do: it creates nothing and binds nothing (its one filesystem access
    // is a read-only `canonicalize` of the store path, which is what makes the
    // derived address a store IDENTITY rather than a store spelling — J2-R1-2),
    // so deriving it costs nothing and leaves no lease behind. See
    // `authorize_bind`'s "What J2 changed" section, which restates that claim
    // rather than letting unconditional binding quietly falsify it.
    //
    // It cannot refuse the start (J2-R1-5): an unusable path degrades to `None`,
    // logged at ERROR inside `for_store`, and this process serves its own client
    // exactly as it did before J2. A losing serve on such a machine then refuses
    // as it did before J2 too, because a proxy needs the holder to have bound —
    // one client working instead of none.
    let endpoint = SessionEndpoint::for_store(&opts.session, &backends.store_cfg);

    // J4 — the ledger opens HERE, before the acquire attempt inside
    // `resolve_role`. That is the whole of J4's first half: a serve that LOSES
    // the lease exits before it can reach the holder path below, where the
    // ledger used to open, so the acquire itself could not be where a losing
    // serve's story starts. Opening it pre-lease and writing the startup line
    // means a serve about to lose the lease has already left an artifact.
    // `Ledger::open` never fails or blocks the caller, so this is free to move
    // into the pre-lease group; `authorize_ledger` above still refuses the
    // misconfigured `--ledger-heartbeat`-without-`--ledger` pairing first.
    let ledger = opts.ledger.as_ref().map(|path| Ledger::open(path.clone()));
    if let Some(ledger) = &ledger {
        ledger.append(&serve_startup_line(&opts, &endpoint));
    }
    let builder = serve_builder(&opts, backends, endpoint.as_ref(), ledger.clone());
    let role = resolve_role(&opts, &builder, endpoint.as_ref(), &ledger).await;
    let mem: Arc<Memory> = match role {
        // The startup election refused (or otherwise failed): this process's
        // pre-lease ledger was opened and the loser recorded its `startup` and
        // `lease:refused` lines — DRAIN it before exiting, or the very artifact
        // J4 exists to leave would sit unflushed in the writer thread's channel
        // and die with the process. This is the exit the pre-lease line was
        // written for, so it is the one that must not drop it.
        Err(e) => {
            if let Some(ledger) = &ledger {
                ledger.shutdown();
            }
            return Err(e);
        }
        Ok(Role::Holder(mem)) => Arc::from(mem),
        Ok(Role::Proxy(proxy)) => {
            // **The proxy branch is deliberately NOT armed for durability, and
            // this is the design decision the J0 review asked for by name.**
            //
            // The hazard it warned about is real: a refused serve never reaches
            // the holder path below, so naive proxy code would run above the
            // arming point — I-R2-1's pre-handshake hole through a new door. The
            // answer is not to move the arming but to notice that *the hole is
            // not there*. What that arming protects is `Memory::close`: a lease
            // taken, an in-RAM write-behind tail, a graph. A proxy has none of
            // the three. It holds no lease (`resolve_role` returned this branch
            // precisely because it lost), no tail (every write happens inside
            // the holder, under the holder's fencing token) and no graph. There
            // is nothing a signal handler could save, so arming for durability
            // would be theatre — a handler that logs.
            //
            // A registration is still installed, for **liveness**: it is how the
            // pump's `select!` learns to stop, so SIGTERM ends the proxy with a
            // log line and a closed socket instead of a bare kill. It is polled
            // first in that `select!`, so this does not make the process
            // SIGTERM-immune the way arming above the lease-taking attach
            // would.
            //
            // Nothing else on this branch is skipped by accident (J4): the
            // ledger this process opened **pre-lease** IS passed into the proxy
            // (`HubProxy::new`), which now books its own `proxying` /
            // `proxying_stopped` lines on it — a proxy is alive and can write,
            // which is the whole of §J2's J4 handoff. What it still does not do
            // is spawn a heartbeat, bind a socket or build a
            let outcome = proxy.run(shutdown_signal()).await;
            tracing::info!("lambo serve: proxy closed (no lease was ever taken by this process)");
            // J4: the ledger this process opened pre-lease was used by the
            // proxy's own `proxying` / `proxying_stopped` lines; drain it
            // before exit. (This branch returns before the holder-path shutdown
            // below, so exactly one of the two runs.)
            if let Some(ledger) = &ledger {
                ledger.shutdown();
            }
            return outcome;
        }
    };

    // One signal registration for the whole life of the transport, armed HERE —
    // the first statement after the lease-taking attach returns (`resolve_role`,
    // which builds through the same `serve_builder` `MemoryBuilder`), and
    // before ANY of the startup work below it (`Ledger::open`, `LamboServer::new`
    // and its `#[tool_router]` JSON-schema build, the heartbeat spawn, the J2
    // session-endpoint bind and its accept loop, the event pump, and the
    // serve-level attach log). Registration is eager (see
    // `shutdown_signal`), so every one of those runs guarded (R2-a): a SIGTERM
    // arriving during them is taken by this future, `Memory::close` runs, the
    // tail reaches the store, and the single-writer lease is released.
    //
    // The precise property, stated where the previous comment overclaimed
    // (I-R2-1): the guard begins the instant `resolve_role` returns. It does NOT
    // cover `resolve_role` itself, so the memory-level "Memory session attached
    // (daemon + flush + canonization running)" line — emitted from inside the
    // `Memory` build, after the lease is taken — is still followed by a residual
    // unguarded window until this arming, exactly as it was pre-I. The
    // serve-level "lambo serve: session attached" line below is fully guarded.
    // The earlier wording claimed the stronger property for both lines; it was
    // false for the memory-level one, and I moving `LamboServer::new` up from
    // inside `serve_stdio` widened that residual window from ~6 µs to ~1.1 ms,
    // which is the durability regression I-R2-1 records.
    //
    // Arming *before* the attach would shrink the residual window to zero, and
    // pre-J2 the argument against it was a trade: a durability hazard for an
    // availability one, since the signal would be deferred rather than honoured
    // until a hung build finished. **J2 makes that argument much stronger, and
    // this is the sentence the round-1 review found missing (J2-R1-7).** The
    // thing that would be made SIGTERM-immune is no longer `build_memory` —
    // which `serve` does not call at all any more — but `resolve_role`, and
    // `resolve_role` is a loop that can *legitimately* run for
    // `LEASE_TTL + ELECTION_SLACK` = **50 seconds**: that is its designed
    // behaviour when the holder it lost to is not proxyable yet, not a hang.
    // Arming above it would therefore make every such start unkillable for the
    // better part of a minute, on purpose, in exchange for closing a ~1.1 ms
    // durability window in a process that holds no lease and no tail while it
    // waits. The residual window is real and worth closing, but only by racing
    // the attach against the shutdown future, which is a design change and is
    // deferred; see I-R2-1's recommendation.
    //
    // A fresh registration in `close_bounded` re-arms it for the close phase.
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    // I1/I2. `Ledger::open` never fails — a bad path warns once and counts
    // every line as a drop — so nothing here can stop a memory server from
    // serving memory. ONE server handle for the whole process (the HTTP factory
    // clones it), so every transport appends to the same file and the
    // heartbeat's uptime is the session's, not a request's.
    let server = match &ledger {
        Some(ledger) => LamboServer::with_ledger(mem.clone(), Arc::clone(ledger)),
        None => LamboServer::new(mem.clone()),
    };
    let heartbeat = match (&ledger, opts.ledger_heartbeat) {
        (Some(ledger), Some(every)) => {
            tracing::info!(
                path = %ledger.path().display(),
                interval_secs = every.as_secs(),
                version = crate::ledger::VERSION,
                git_sha = crate::ledger::GIT_SHA,
                "lambo serve: call ledger open, heartbeat armed"
            );
            Some(tokio::spawn(heartbeat_loop(
                server.clone(),
                Arc::clone(ledger),
                every,
            )))
        }
        (Some(ledger), None) => {
            tracing::info!(
                path = %ledger.path().display(),
                "lambo serve: call ledger open (no heartbeat)"
            );
            None
        }
        _ => None,
    };
    // J4 — the holder side of a refused takeover: record the incumbent's
    // line when the store reports a refusal this process turned away. Spawned
    // only when a ledger is attached, and only on the holder path (the proxy
    // branch returned above). Aborted at close like the heartbeat.
    let refusal_poller = match &ledger {
        Some(ledger) => {
            let holder_token =
                crate::store::lease::LeaseHolder::for_this_process(mem.agent()).token();
            Some(tokio::spawn(record_refused_takeovers(
                mem.store().clone(),
                mem.session().clone(),
                mem.agent().clone(),
                holder_token,
                Arc::clone(ledger),
            )))
        }
        None => None,
    };

    // J2 — the session endpoint, bound HERE: below the arming (so a signal
    // during it still reaches `Memory::close`) and below `LamboServer`, which it
    // needs. This is also the first moment the unlink inside `bind` is licensed:
    // we hold the lease, so a socket file already at this path cannot belong to
    // a live holder.
    //
    // **A bind failure does not stop this process serving memory** — the same
    // posture `Ledger::open` takes, for the same reason: reachability is a
    // service to *other* processes, and losing it must not cost this client its
    // memory. The consequence is stated at ERROR rather than swallowed, because
    // the lease row now advertises an address nothing is listening on: a proxy
    // that dials it fails honestly per call (the holder-unreachable path), which
    // is loud but is a real degradation, so the log line names it.
    // No endpoint: a store no second process can see, so there is no hub to be.
    // See `SessionEndpoint::for_store`.
    let hub = match endpoint
        .as_ref()
        .map(|ep| (ep.path().display().to_string(), ep.bind()))
    {
        None => None,
        Some((path, Ok(listener))) => {
            tracing::info!(
                endpoint = %path,
                "lambo serve: session endpoint bound — other clients on this machine can attach \
                 to this session through it"
            );
            Some(tokio::spawn(serve_endpoint(
                listener,
                server.clone(),
                opts.max_sessions,
            )))
        }
        Some((path, Err(e))) => {
            tracing::error!(
                error = %e,
                endpoint = %path,
                "lambo serve: the session endpoint could not be bound — this process still serves \
                 its own client normally, but other clients on this machine CANNOT attach to this \
                 session and their calls will fail honestly rather than reaching memory"
            );
            None
        }
    };

    // Exactly once, at startup: `events()` is stateful on its first call — it
    // hands out the receiver subscribed *before* the daemon spawned, so the
    // spec §2.5 warm-up condition set (on a resumed session, the whole restored
    // set) is not lost. Draining it here also stops the broadcast channel from
    // filling and lagging the daemon.
    let events = mem.events();
    let event_pump = tokio::spawn(log_events(events));

    tracing::info!(
        session = %opts.session,
        agent = %opts.agent,
        transport = ?opts.transport,
        "lambo serve: session attached"
    );

    let transport = async {
        match opts.transport {
            Transport::Stdio => serve_stdio(server, shutdown.as_mut()).await,
            Transport::Http => serve_http(server, &opts, shutdown.as_mut()).await,
        }
    };

    let outcome = run_and_close(mem.clone(), transport, event_pump).await;

    // After `close()`, deliberately: the tail's durability is the load-bearing
    // guarantee and the ledger is not allowed to be in front of it. The
    // heartbeat is stopped first so it cannot enqueue a line into a ledger
    // that is draining, and `shutdown` is bounded — a writer stuck on a hung
    // filesystem is abandoned, never allowed to hold process exit.
    if let Some(heartbeat) = heartbeat {
        heartbeat.abort();
    }
    // J4. The refusal-recorder task is stopped before the ledger drains, so it
    // cannot enqueue a line into a closing ledger.
    if let Some(poller) = refusal_poller {
        poller.abort();
    }
    // J2. Aborted AFTER `close()` (that is where `run_and_close` returned from),
    // deliberately: a proxy's in-flight call must not be cut off before the tail
    // it may have just written is durable. Then the socket file goes, so the
    // next start does not log a stale-socket warning it did not earn.
    if let Some(hub) = hub {
        hub.abort();
    }
    if let Some(endpoint) = &endpoint {
        endpoint.unlink();
    }
    if let Some(ledger) = ledger {
        ledger.shutdown();
        tracing::info!(
            written = ledger.counters().written(),
            dropped = ledger.counters().dropped(),
            path = %ledger.path().display(),
            "lambo serve: call ledger closed"
        );
    }

    outcome
}

/// Run the transport future, then close the session — **on every exit path**.
///
/// Split out from [`serve`] so the "close always runs" guarantee is testable
/// without a real socket or handshake: the guarantee lives here, not tangled
/// with transport construction. Whatever the transport returns — clean
/// disconnect, forced-close `Ok`, or a transport `Err` — [`Memory::close`] runs
/// afterward, bounded by [`close_bounded`].
///
/// The event pump is aborted *after* `close()` (R1/T82-17): canonization and
/// conflict events emitted during the final drain are exactly what an operator
/// debugging a failed close wants on stderr, and aborting first threw them away.
async fn run_and_close(
    mem: Arc<Memory>,
    transport: impl Future<Output = Result<(), LamboError>>,
    event_pump: tokio::task::JoinHandle<()>,
) -> Result<(), LamboError> {
    let outcome = transport.await;
    let closed = close_bounded(&mem).await;
    event_pump.abort();

    match (outcome, closed) {
        (Err(e), _) => Err(e),
        (Ok(()), Err(e)) => {
            tracing::error!(error = %e, "lambo serve: final flush failed — tail lost on exit, not durable (no on-disk WAL)");
            Err(e)
        }
        (Ok(()), Ok(())) => {
            tracing::info!("lambo serve: session closed, tail durable");
            Ok(())
        }
    }
}

/// [`Memory::close`], bounded two ways (R2-b).
///
/// A [`CLOSE_GRACE`] deadline caps a store that hangs on the final flush, and a
/// **re-armed** shutdown signal lets an operator who sees the close stall press
/// Ctrl-C a second time to force the exit rather than have it swallowed. Either
/// path abandons the `close()` future, which is safe *for liveness*: the drained
/// tail returns to the front of the (in-memory) graph log, the session stays
/// closed to writers, and no success is latched — so the returned `Err` is honest.
///
/// It is **not** durable. Because `serve` exits right after abandoning close and
/// there is no on-disk WAL, the abandoned tail is LOST — the in-memory log dies
/// with the process. The messages below say exactly that; they must not promise a
/// restart will recover it (R4/P2 — a prior wording did, and it was false).
///
/// ## The lease is released even when the tail is lost (L82-1)
///
/// Abandoning `close()` drops it mid-flight, so the release on its own success
/// path never runs. The live review watched exactly that: SIGTERM under an
/// at-cap burst timed the close out and left a **stale lease row**, wedging the
/// session for the whole `LEASE_TTL` on top of losing the tail. Two failures for
/// one cause, and the second one is not inherent — the release is a statement
/// about this process being gone, not a claim that anything was written.
///
/// So both abandon paths below run [`Memory::release_lease_after_abandoned_close`]
/// under [`LEASE_RELEASE_GRACE`] before returning. It cannot rescue the tail and
/// does not pretend to: the returned error is unchanged, and a release that
/// itself times out leaves the row to lapse at TTL, which is where this started.
async fn close_bounded(mem: &Memory) -> Result<(), LamboError> {
    close_bounded_until(mem, shutdown_signal()).await
}

/// [`close_bounded`] with the re-armed signal passed in.
///
/// `shutdown_signal()` registers process-wide SIGINT/SIGTERM handlers, which a
/// unit test must not do to the whole test binary. Taking the future as an
/// argument lets `memory`'s tests drive the real body with
/// `std::future::pending()` — see `an_abandoned_close_releases_the_lease_through_serve`.
pub(crate) async fn close_bounded_until(
    mem: &Memory,
    shutdown: impl Future<Output = ()>,
) -> Result<(), LamboError> {
    // Scoped so the abandoned `close()` future is *dropped* before the release
    // below: it holds `close_state` and the writers' write guard, and its
    // documented cancellation behaviour (returning the drained tail to the front
    // of the log, latching no success) should run before anything else does.
    let outcome = {
        let close = mem.close();
        tokio::pin!(close);
        tokio::select! {
        // Bias toward the close itself: if it is already done, take that answer
        // rather than a signal delivered in the same poll.
        biased;
        r = tokio::time::timeout(CLOSE_FLUSH_GRACE, &mut close) => match r {
            Ok(r) => r,
            Err(_) => {
                tracing::error!(
                    grace_secs = CLOSE_FLUSH_GRACE.as_secs(),
                    "lambo serve: close() did not finish within the grace window — abandoning \
                     it and exiting; the un-flushed tail is LOST (the write-behind log is \
                     in-memory only, there is no on-disk WAL, and a restart will NOT recover it)"
                );
                Err(LamboError::Config(format!(
                    "close timed out after {}s; tail lost on exit, not durable",
                    CLOSE_FLUSH_GRACE.as_secs()
                )))
            }
        },
        () = shutdown => {
            tracing::warn!(
                "lambo serve: a second shutdown signal arrived during close — abandoning it \
                 and exiting; the un-flushed tail is LOST (in-memory write-behind log, no \
                 on-disk WAL, not recoverable on restart)"
            );
            Err(LamboError::Config(
                "close interrupted by a second shutdown signal; tail lost on exit, not durable"
                    .into(),
            ))
        }
        }
    };

    if outcome.is_err() {
        release_lease_bounded(mem).await;
    }
    outcome
}

/// Best-effort lease release on the way out of an abandoned close (L82-1).
///
/// Bounded so a store that is *why* the close hung cannot hang the exit too. A
/// timeout is logged, not returned: the caller's error is already the honest
/// account of what went wrong, and "we also could not tidy the lease" does not
/// change what an operator must do.
async fn release_lease_bounded(mem: &Memory) {
    if tokio::time::timeout(
        LEASE_RELEASE_GRACE,
        mem.release_lease_after_abandoned_close(),
    )
    .await
    .is_err()
    {
        tracing::warn!(
            grace_secs = LEASE_RELEASE_GRACE.as_secs(),
            "lambo serve: could not release the single-writer lease within its window after an \
             abandoned close; the row will lapse at LEASE_TTL instead, and until then this \
             session refuses new writers"
        );
    }
}

/// Run a setup step (the stdio handshake, the HTTP `bind`) but bail the moment
/// the shutdown signal fires first (R2-a).
///
/// Before this, a signal that landed in the pre-handshake window — after the
/// session is attached (a clean run already has `mutations=1` to flush at that
/// point) but before the transport's own signal handling is live — hit the
/// default disposition and killed the process with `close()` un-run. `None`
/// means the signal won: the caller returns so `serve` still reaches close.
async fn setup_or_shutdown<T>(
    setup: impl Future<Output = T>,
    shutdown: impl Future<Output = ()>,
) -> Option<T> {
    tokio::select! {
        biased;
        v = setup => Some(v),
        () = shutdown => None,
    }
}

/// Drain the daemon's event stream into the log.
///
/// A dropped or lagging receiver is not an error (spec §6.1); a lagging one
/// re-syncs.
async fn log_events(mut rx: tokio::sync::broadcast::Receiver<DaemonEvent>) {
    loop {
        match rx.recv().await {
            Ok(ev) => tracing::info!(event = ?ev, "daemon event"),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "daemon event stream lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Outcome of running a transport under a shutdown signal.
#[derive(Debug, PartialEq, Eq)]
enum Exit<T> {
    /// The transport finished on its own, or wound down within the grace window.
    Finished(T),
    /// The grace window expired with the transport still running; it was
    /// dropped so shutdown could proceed.
    Forced,
}

/// Run `running` to completion, but never past `shutdown` + `grace`.
///
/// The shape both transports need and the fix for R1/T82-1 and T82-2:
///
/// 1. race the transport against the shutdown signal;
/// 2. on the signal, call `cancel` — for stdio that cancels the rmcp service
///    loop, for HTTP it triggers axum's graceful shutdown;
/// 3. give the transport `grace` to actually finish, and if it does not,
///    return [`Exit::Forced`] so the caller can drop it and close the session.
///
/// Step 3 is the part that matters: a graceful shutdown with no deadline is
/// indistinguishable from a hang when a client holds a long-lived stream open,
/// and a hang here means the write-behind tail is never flushed.
async fn run_until_shutdown<T>(
    running: impl Future<Output = T>,
    cancel: impl FnOnce(),
    shutdown: impl Future<Output = ()>,
    grace: Duration,
) -> Exit<T> {
    tokio::pin!(running);
    tokio::select! {
        // Bias is deliberate: if the transport is already done, take that
        // answer rather than a signal that arrived in the same poll.
        biased;
        v = &mut running => return Exit::Finished(v),
        () = shutdown => {}
    }
    tracing::info!("lambo serve: shutdown signal received, winding down");
    cancel();
    match tokio::time::timeout(grace, &mut running).await {
        Ok(v) => Exit::Finished(v),
        Err(_) => Exit::Forced,
    }
}

/// Serve the session endpoint (J2) — the hub half of multi-client survivability.
///
/// One MCP session per accepted connection, all against the **one** [`Memory`]
/// this process owns: `server` is cloned exactly as `serve_http`'s factory
/// clones it, so every connection shares the same graph, the same write-behind
/// log, the same single-writer lease and the same call ledger. Nothing here
/// builds a second `Memory`.
///
/// `UnixStream` is an rmcp transport without any new dependency: the
/// `transport-io` feature already in use pulls `transport-async-rw`, whose
/// `IntoTransport` covers any `AsyncRead + AsyncWrite`. The wire is the same
/// newline-delimited JSON-RPC the stdio transport speaks — which is what lets a
/// proxy be a byte pipe rather than a re-implementation of the tool surface.
///
/// Runs until aborted. The caller aborts it alongside the heartbeat, *after*
/// [`Memory::close`], so a proxy's in-flight call is not cut off before the tail
/// it may have just written is durable.
async fn serve_endpoint(
    listener: tokio::net::UnixListener,
    server: LamboServer,
    max_sessions: usize,
) {
    // The same ceiling `--max-sessions` puts on the HTTP transport, for the same
    // reason: concurrently live MCP sessions are the resource, and the endpoint
    // is another door onto it. A refused connection is closed immediately, which
    // a proxy reports to its caller as an honest failure rather than a hang.
    let permits = Arc::new(tokio::sync::Semaphore::new(max_sessions.max(1)));
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                // A per-connection accept error must never take down the
                // session: this process is still serving its own client.
                tracing::warn!(error = %e, "lambo serve: session endpoint accept failed");
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            tracing::warn!(
                max_sessions,
                "lambo serve: session endpoint at the concurrent-session ceiling — \
                 refusing a connection (raise --max-sessions if this is a real workload)"
            );
            drop(stream);
            continue;
        };
        let server = server.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match server.serve(stream).await {
                Ok(service) => {
                    if let Err(e) = service.waiting().await {
                        tracing::warn!(error = %e, "lambo serve: endpoint session ended in error");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "lambo serve: endpoint handshake failed");
                }
            }
        });
    }
}

/// stdio transport — the shape an MCP client launches as a subprocess.
///
/// **stdout is the protocol channel.** Nothing but JSON-RPC may be written to
/// it; diagnostics go to stderr (see [`crate::mcp::init_tracing`]).
///
/// Returns on client disconnect (EOF on stdin) **or** on SIGINT/SIGTERM, so
/// `Memory::close` runs either way. Before R1/T82-1 this awaited only the
/// service, and the default signal disposition killed the process outright with
/// the tail still in the log.
async fn serve_stdio(
    server: LamboServer,
    mut shutdown: Pin<&mut impl Future<Output = ()>>,
) -> Result<(), LamboError> {
    // Race the handshake against the shutdown signal (R2-a). `shutdown.as_mut()`
    // reborrows, so the same registration is still live for the transport race
    // below if the handshake wins.
    let service = match setup_or_shutdown(server.serve(stdio()), shutdown.as_mut()).await {
        Some(r) => r.map_err(|e| LamboError::Config(format!("mcp stdio: {e}")))?,
        None => {
            tracing::info!(
                "mcp stdio: shutdown signal during handshake — closing the session without serving"
            );
            return Ok(());
        }
    };

    // Taken before `waiting()` consumes the service — it is the only handle
    // left once the service is inside the future.
    let cancel_token = service.cancellation_token();
    match run_until_shutdown(
        service.waiting(),
        move || cancel_token.cancel(),
        shutdown,
        SHUTDOWN_GRACE,
    )
    .await
    {
        Exit::Finished(Ok(reason)) => {
            tracing::info!(?reason, "mcp stdio: client disconnected");
            Ok(())
        }
        Exit::Finished(Err(e)) => Err(LamboError::Config(format!("mcp stdio: {e}"))),
        Exit::Forced => {
            tracing::warn!(
                grace_secs = SHUTDOWN_GRACE.as_secs(),
                "mcp stdio: service did not stop within the grace window — \
                 dropping the transport and closing the session anyway"
            );
            Ok(())
        }
    }
}

/// Streamable HTTP transport, served by the axum already in the tree.
///
/// The service factory clones an `Arc<Memory>` per request — it never builds a
/// second [`Memory`].
async fn serve_http(
    server: LamboServer,
    opts: &ServeOptions,
    mut shutdown: Pin<&mut impl Future<Output = ()>>,
) -> Result<(), LamboError> {
    // CLONED, not rebuilt (I1): every request handler must share the one call
    // ledger, and `LamboServer::new` per request would also rebuild the whole
    // `ToolRouter` — every tool's JSON schema included — on every request,
    // which is the cost `#[tool_handler(router = self.tool_router)]` exists to
    // avoid. Cloning shares the `Arc<Memory>` exactly as before.
    let factory_server = server.clone();
    // Held as its own `Arc` so the session cap can read the live count from the
    // same manager rmcp mutates — see [`LiveSessions`].
    let sessions = Arc::new(LocalSessionManager::default());
    let service =
        StreamableHttpService::new(move || Ok(factory_server.clone()), sessions.clone(), {
            // `#[non_exhaustive]` — mutate the SDK default rather than
            // constructing, so a new field cannot silently break the build.
            let mut cfg = StreamableHttpServerConfig::default();
            cfg.sse_keep_alive = Some(Duration::from_secs(15));
            cfg
        });

    let guard = HttpGuard {
        auth: opts.auth_token.clone(),
        max_sessions: opts.max_sessions,
        live: sessions,
        rate: RateLimiter::new(opts.rate_limit_rps, Instant::now()).map(Arc::new),
    };
    // T8.7 posture, logged once at startup so an operator can see what this
    // process is actually enforcing. The token itself is never logged — only
    // whether one is required.
    tracing::info!(
        auth_required = guard.auth.is_some(),
        max_sessions = guard.max_sessions,
        rate_limit_rps = opts.rate_limit_rps,
        "mcp http: request guard armed"
    );

    let app = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(guard, guard_request));
    let addr = SocketAddr::new(opts.bind, opts.port);
    // Race `bind` against the shutdown signal too (R2-a): the ~5 ms bind window
    // is small but non-zero, and a signal in it must still reach `close()`.
    let listener =
        match setup_or_shutdown(tokio::net::TcpListener::bind(addr), shutdown.as_mut()).await {
            Some(r) => r.map_err(|e| LamboError::Config(format!("mcp http: bind {addr}: {e}")))?,
            None => {
                tracing::info!(
                    "mcp http: shutdown signal before bind — closing the session without serving"
                );
                return Ok(());
            }
        };
    tracing::info!(%addr, "mcp http: listening on /mcp");

    serve_http_bounded(listener, app, shutdown, SHUTDOWN_GRACE).await
}

/// `axum::serve` with a **bounded** graceful shutdown (R1/T82-2).
///
/// Split out from [`serve_http`] so the bound is testable without a `Memory`:
/// the test holds a never-ending response open, exactly as a streamable-HTTP
/// MCP client's SSE channel does, and asserts this returns anyway.
async fn serve_http_bounded(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    shutdown: impl Future<Output = ()>,
    grace: Duration,
) -> Result<(), LamboError> {
    // axum's graceful shutdown takes a future; this oneshot is how the shared
    // `cancel` step reaches it.
    let (graceful_tx, graceful_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = graceful_rx.await;
    });

    // `WithGracefulShutdown` is `IntoFuture`, not `Future` — the async block is
    // what turns it into one.
    match run_until_shutdown(
        async move { server.await },
        move || {
            let _ = graceful_tx.send(());
        },
        shutdown,
        grace,
    )
    .await
    {
        Exit::Finished(r) => r.map_err(|e| LamboError::Config(format!("mcp http: {e}"))),
        Exit::Forced => {
            tracing::warn!(
                grace_secs = grace.as_secs(),
                "mcp http: connections still open after the grace window (an SSE stream \
                 never finishes on its own) — forcing the close so the tail is flushed"
            );
            Ok(())
        }
    }
}

/// Ctrl-C (and SIGTERM on unix), so `close()` still runs.
///
/// Registration is EAGER: the handlers are installed when this function is
/// *called*, not when the returned future is first polled. An `async fn` body
/// runs on first poll, which left a window between the "session attached" log
/// and the transport's first poll of this future where a signal still had the
/// default disposition — a SIGTERM in that window killed the process outright
/// (R2-a; observed as a CI-only failure of the pre-handshake durability test
/// on a loaded runner). `tokio::signal::unix::signal()` registers with the
/// runtime immediately and buffers a signal that arrives before `recv()` is
/// polled, so calling this before the attach log closes that window. Eagerness
/// only makes the arming *point* effective; it does not move it. Everything
/// before the call site in [`serve`] — `resolve_role`, which takes the lease —
/// is still unguarded, which is why the call site sits as early as it does
/// (I-R2-1). It cannot move above `resolve_role`: that loop is allowed to run
/// for 50 seconds by design, and arming over it would make the wait unkillable
/// (J2-R1-7).
fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // Register both handlers NOW. Errors (exotic platforms, exhausted
        // signal slots) degrade to the lazy ctrl_c path rather than failing.
        let int = signal(SignalKind::interrupt());
        let term = signal(SignalKind::terminate());
        async move {
            match (int, term) {
                (Ok(mut int), Ok(mut term)) => {
                    tokio::select! {
                        _ = int.recv() => {}
                        _ = term.recv() => {}
                    }
                }
                (Ok(mut int), Err(_)) => {
                    let _ = int.recv().await;
                }
                (Err(_), Ok(mut term)) => {
                    let _ = term.recv().await;
                }
                (Err(_), Err(_)) => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        async {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // J2-R2-2 / J2-R2-3 — the election's arithmetic and the refusal it composes
    // -----------------------------------------------------------------------

    /// J2-R2-2: the claim [`ELECTION_BUDGET`]'s docstring makes, asserted.
    ///
    /// The docstring used to say the wait "still succeeds" in "the majority of
    /// real cases" because "a lease expires uniformly somewhere inside its TTL".
    /// It does not expire uniformly: every refresh sets `expires_at` to
    /// `now + LEASE_TTL` and refreshes come every `LEASE_HEARTBEAT_INTERVAL`, so
    /// an abrupt death leaves at least `LEASE_TTL - LEASE_HEARTBEAT_INTERVAL`.
    /// This test is what makes that arithmetic falsifiable instead of asserted in
    /// prose — moving any of the three constants into a shape where a prompt
    /// restart could be waited out turns it red.
    #[test]
    fn an_abrupt_holder_death_outlasts_the_election_budget() {
        // The whole reachable range after an abrupt death, both ends.
        let least = lease::LEASE_TTL - lease::LEASE_HEARTBEAT_INTERVAL;
        assert_eq!(least, Duration::from_secs(30), "the floor moved");
        for lapses_in in [least, lease::LEASE_TTL] {
            assert!(
                !waiting_fits(lapses_in, ELECTION_BUDGET),
                "a client starting promptly after an abrupt holder death must be refused, not \
                 waited out: {lapses_in:?} of lease against a {ELECTION_BUDGET:?} budget"
            );
        }
        // And the window that IS waited out, at its exact boundary: a client
        // arriving late enough that only the slack separates the lapse from the
        // budget.
        let widest = ELECTION_BUDGET - ELECTION_SLACK;
        assert!(
            waiting_fits(widest, ELECTION_BUDGET),
            "the largest lapse the budget can absorb must still be waited out"
        );
        assert!(
            !waiting_fits(widest + Duration::from_secs(1), ELECTION_BUDGET),
            "one second past it must refuse"
        );
        // The floor is above the widest waitable lapse — which is the whole
        // finding in one line.
        assert!(
            least > widest,
            "if this ever inverts, the docstring's 'a prompt start is refused by design' is \
             no longer true and must be rewritten with it"
        );
    }

    /// J2-R2-3: the refusal must not tell an operator that a dead holder "is
    /// still refreshing" its lease.
    ///
    /// The false clause came *first* in the composed paragraph and the probe's
    /// contradiction second, so the opening sentence sent an operator looking for
    /// a process that no longer exists.
    #[test]
    fn a_dead_holders_refusal_does_not_claim_it_is_still_refreshing() {
        let held = format!(
            "session s is already held by another writer (a@h#1) — it acquired the \
             single-writer lease 4s ago and {}. Refusing to open a second writer.",
            crate::memory::STILL_REFRESHING_CLAUSE
        );
        let refused = format!("{ENDPOINT_NOT_ACCEPTING} (Connection refused (os error 61))");
        let corrected = correct_the_refresh_claim(&held, &refused);
        assert!(
            !corrected.contains(crate::memory::STILL_REFRESHING_CLAUSE),
            "the probe is the better evidence and the claim must go: {corrected}"
        );
        assert!(
            corrected.contains("most likely died"),
            "and it must be replaced by what the probe actually found: {corrected}"
        );
        // J2-R1-9's rule, applied to the new literal: a continuation that lost
        // its `\\` shows up as a double space, and a phrase that spans one shows
        // up nowhere. Assert both.
        assert!(
            !corrected.contains("  "),
            "a broken string continuation leaves a double space: {corrected}"
        );
        assert!(
            corrected.contains("endpoint is not answering"),
            "the phrase spanning the continuation must survive it: {corrected}"
        );
        // Narrow on purpose: every other refusal is a LIVE holder this process
        // merely cannot forward to, and the clause is true for it.
        for other in [
            "That holder published no endpoint, so there is nothing to forward tool calls to",
            "That holder is on another host (a@elsewhere#2)",
            "the holder's endpoint is not safe to dial (a directory check failed)",
        ] {
            assert!(
                correct_the_refresh_claim(&held, other)
                    .contains(crate::memory::STILL_REFRESHING_CLAUSE),
                "a live-but-unforwardable holder IS still refreshing its lease: {other}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // I1 / I2 — ledger flags and the heartbeat timer
    // -----------------------------------------------------------------------

    /// **I1.** Both ledger knobs are off in the default options: `--ledger` is
    /// opt-in, and nobody who did not ask for it gets a writer thread.
    #[test]
    fn i1_the_ledger_is_off_in_the_default_serve_options() {
        let opts = ServeOptions::new("s", "a");
        assert!(opts.ledger.is_none(), "no ledger path by default");
        assert!(opts.ledger_heartbeat.is_none(), "no heartbeat by default");
        assert!(
            authorize_ledger(&opts).is_ok(),
            "the default options are a legal configuration"
        );
    }

    /// **I2.** `--ledger-heartbeat` without `--ledger` is refused at startup
    /// with a message that names the fix, rather than accepted as a no-op that
    /// writes heartbeats nowhere.
    #[test]
    fn i2_a_heartbeat_without_a_ledger_is_refused_and_says_why() {
        let mut opts = ServeOptions::new("s", "a");
        opts.ledger_heartbeat = Some(Duration::from_secs(60));
        let err = authorize_ledger(&opts).expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("--ledger"), "names the missing flag: {msg}");
        assert!(
            msg.contains("60"),
            "quotes the interval it was given: {msg}"
        );

        // With a path, the same interval is fine.
        opts.ledger = Some(std::path::PathBuf::from("/tmp/nonexistent/calls.jsonl"));
        assert!(authorize_ledger(&opts).is_ok(), "a path makes it legal");

        // A ledger with no heartbeat is also fine — heartbeats are optional.
        opts.ledger_heartbeat = None;
        assert!(authorize_ledger(&opts).is_ok());
    }

    /// **I-R1-12.** A zero heartbeat interval is refused at the *library*
    /// boundary, not only by the CLI.
    ///
    /// `tokio::time::interval` panics on a zero period, so `serve()` used to hand
    /// a non-CLI caller a heartbeat task that panicked on its first tick while
    /// the CLI refused the same options with a different exit code from the
    /// heartbeat-without-ledger case. One check, one path out, for both.
    #[test]
    fn i2_a_zero_heartbeat_interval_is_refused_at_the_library_boundary() {
        let mut opts = ServeOptions::new("s", "a");
        opts.ledger = Some(std::path::PathBuf::from("/tmp/nonexistent/calls.jsonl"));
        opts.ledger_heartbeat = Some(Duration::ZERO);
        let err = authorize_ledger(&opts).expect_err("a zero interval must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("at least 1 second"),
            "keeps the CLI's wording: {msg}"
        );
        assert!(
            matches!(err, LamboError::Config(_)),
            "a configuration error, so it exits the way the other one does: {err:?}"
        );

        // One second is the smallest legal interval.
        opts.ledger_heartbeat = Some(Duration::from_secs(1));
        assert!(authorize_ledger(&opts).is_ok());

        // And zero is refused with no ledger too — that arm reports the missing
        // flag first, which is the more useful of the two messages.
        opts.ledger = None;
        opts.ledger_heartbeat = Some(Duration::ZERO);
        assert!(authorize_ledger(&opts).is_err());
    }

    /// **I2 acceptance.** The heartbeat interval actually fires, repeatedly, on
    /// the interval it was given — asserted on a paused clock so the test pins
    /// the *period* rather than racing a wall-clock sleep.
    // Same gate the other Memory-building tests in this module use: the store
    // and embedder it needs are feature-gated, and a bare `#[cfg(test)]` would
    // not compile under `--no-default-features --features store-sqlite`.
    #[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
    #[tokio::test(start_paused = true)]
    async fn i2_the_heartbeat_fires_on_its_interval() {
        let dir = std::env::temp_dir().join(format!(
            "lambo-i2-hb-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("calls.jsonl");
        let ledger = Ledger::open(&path);

        let mem = Arc::new(
            Memory::builder()
                .session("i2-heartbeat-timer")
                .agent("agent-a")
                .flush_interval(Duration::from_secs(3_600))
                .store(
                    Arc::new(crate::store::MemoryStore::new()) as Arc<dyn crate::store::GraphStore>
                )
                .embedder(Arc::new(crate::embed::FixtureEmbedder::new())
                    as Arc<dyn crate::embed::Embedder>)
                .embedding_contract(crate::types::EmbeddingContract {
                    kind: "fixture".into(),
                    model: None,
                    dim: 1024,
                })
                .build()
                .await
                .expect("build"),
        );
        let server = LamboServer::with_ledger(mem.clone(), Arc::clone(&ledger));

        let every = Duration::from_secs(30);
        let task = tokio::spawn(heartbeat_loop(server, Arc::clone(&ledger), every));

        // The writer is a real OS thread, so each step yields to it until the
        // count lands rather than assuming an instant write.
        async fn wait_for(ledger: &Ledger, want: u64) -> u64 {
            for _ in 0..2_000 {
                if ledger.counters().written() >= want {
                    break;
                }
                tokio::task::yield_now().await;
                std::thread::sleep(Duration::from_millis(1));
            }
            ledger.counters().written()
        }

        // The first beat is immediate — it stamps the binary's identity at the
        // moment the session attached, so the first stretch of ledger is not
        // left unattributed.
        assert_eq!(wait_for(&ledger, 1).await, 1, "the first beat is immediate");

        // Advancing by less than the interval must NOT produce a beat.
        tokio::time::advance(every / 2).await;
        assert_eq!(
            ledger.counters().written(),
            1,
            "half an interval is not a beat"
        );

        // Crossing the interval produces exactly one more, twice over.
        tokio::time::advance(every).await;
        assert_eq!(wait_for(&ledger, 2).await, 2, "one beat per interval");
        tokio::time::advance(every).await;
        assert_eq!(wait_for(&ledger, 3).await, 3, "and again");

        task.abort();
        ledger.shutdown();

        // Every beat is a `stats` line carrying the sha.
        let text = std::fs::read_to_string(&path).expect("ledger file");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("parses");
            assert_eq!(v["kind"], serde_json::json!("stats"), "{line}");
            assert_eq!(
                v["git_sha"],
                serde_json::json!(crate::ledger::GIT_SHA),
                "an upgrade shows here as a sha change: {line}"
            );
            assert_eq!(
                v["version"],
                serde_json::json!(crate::ledger::VERSION),
                "{line}"
            );
        }
        mem.close().await.expect("close");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn transport_parses_both_and_rejects_junk() {
        assert_eq!("stdio".parse::<Transport>().unwrap(), Transport::Stdio);
        assert_eq!("  HTTP ".parse::<Transport>().unwrap(), Transport::Http);
        assert!("grpc".parse::<Transport>().is_err());
    }

    #[test]
    fn serve_options_default_to_stdio_on_loopback() {
        let o = ServeOptions::new("s", "a");
        assert_eq!(o.transport, Transport::Stdio);
        assert_eq!(o.port, 7700);
        assert!(o.bind.is_loopback());
    }

    /// **Test-gap (c).** The grace windows are sane bounds, not accidents: both
    /// are non-zero (a zero window would force-drop instantly, defeating the
    /// point) and short enough that a supervisor's kill escalation (commonly
    /// ~30 s) never beats them, and `CLOSE_GRACE` — the last thing before exit —
    /// is at least as generous as the transport window.
    ///
    /// It also pins the **aggregate** (R4): `SHUTDOWN_GRACE + CLOSE_GRACE` is the
    /// whole end-to-end shutdown cost and must stay within [`SHUTDOWN_BUDGET`],
    /// the number an operator sizes their SIGKILL timeout against. Checking each
    /// window alone let the sum drift past a tight supervisor window unnoticed.
    #[test]
    fn the_grace_windows_are_sane() {
        assert!(
            !SHUTDOWN_GRACE.is_zero(),
            "a zero transport grace force-drops instantly"
        );
        assert!(
            !CLOSE_GRACE.is_zero(),
            "a zero close grace never lets the flush finish"
        );
        assert!(
            SHUTDOWN_GRACE < Duration::from_secs(30),
            "must beat SIGKILL escalation"
        );
        assert!(
            CLOSE_GRACE < Duration::from_secs(30),
            "must beat SIGKILL escalation"
        );
        assert!(
            CLOSE_GRACE >= SHUTDOWN_GRACE,
            "the final close gets at least the transport window"
        );
        assert!(
            SHUTDOWN_GRACE + CLOSE_GRACE <= SHUTDOWN_BUDGET,
            "the end-to-end shutdown cost ({}s + {}s) must fit the documented budget ({}s) — \
             a supervisor's SIGKILL timeout is sized against SHUTDOWN_BUDGET",
            SHUTDOWN_GRACE.as_secs(),
            CLOSE_GRACE.as_secs(),
            SHUTDOWN_BUDGET.as_secs(),
        );
        // T8.6: the lease TTL must outlast the whole shutdown budget, so a
        // graceful close releases the lease rather than letting it expire while
        // the final flush is still running.
        assert!(
            lease::LEASE_TTL > SHUTDOWN_BUDGET,
            "LEASE_TTL ({}s) must exceed SHUTDOWN_BUDGET ({}s)",
            lease::LEASE_TTL.as_secs(),
            SHUTDOWN_BUDGET.as_secs(),
        );
        // L82-1: the lease-release window is carved OUT of the close budget,
        // not added to it, so the operator-facing SHUTDOWN_BUDGET is unchanged.
        assert_eq!(
            CLOSE_FLUSH_GRACE + LEASE_RELEASE_GRACE,
            CLOSE_GRACE,
            "the close phase is the flush attempt then the lease release, and nothing else"
        );
        assert!(
            !LEASE_RELEASE_GRACE.is_zero(),
            "a zero release window makes the abandoned-close release a no-op"
        );
        assert!(
            CLOSE_FLUSH_GRACE > LEASE_RELEASE_GRACE,
            "the flush keeps the bulk of the close budget; the release is one statement"
        );
    }

    /// **R2-a pinned, mechanism level.** A setup step that beats the signal is
    /// kept; a signal that fires before setup finishes bails with `None`, which
    /// is the caller's cue to skip serving and go straight to `close()`.
    #[tokio::test]
    async fn setup_or_shutdown_prefers_setup_but_bails_on_an_early_signal() {
        // Setup wins: the signal is `pending`, so the value comes through.
        assert_eq!(
            setup_or_shutdown(async { 5u32 }, std::future::pending::<()>()).await,
            Some(5)
        );
        // Signal wins: setup never finishes, so the caller is told to bail.
        assert_eq!(
            setup_or_shutdown(std::future::pending::<u32>(), async {}).await,
            None
        );
    }

    /// A transport that ends on its own is never cancelled and never waits on
    /// the signal — the ordinary client-disconnect path.
    #[tokio::test]
    async fn a_transport_that_finishes_on_its_own_is_not_cancelled() {
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = cancelled.clone();
        let exit = run_until_shutdown(
            async { 7u32 },
            move || c.store(true, std::sync::atomic::Ordering::SeqCst),
            std::future::pending::<()>(),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(exit, Exit::Finished(7));
        assert!(
            !cancelled.load(std::sync::atomic::Ordering::SeqCst),
            "cancel must not fire when the transport ended by itself"
        );
    }

    /// **R1/T82-1 pinned.** A shutdown signal cancels the transport and the
    /// function returns, so the caller reaches `Memory::close`.
    #[tokio::test]
    async fn a_shutdown_signal_cancels_the_transport_and_returns() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let exit = run_until_shutdown(
            async move {
                // Stands in for the rmcp service loop: it ends only when
                // cancelled, exactly like `service.waiting()`.
                let _ = rx.await;
                "cancelled"
            },
            move || {
                let _ = tx.send(());
            },
            async {},
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(exit, Exit::Finished("cancelled"));
    }

    /// **R1/T82-2 pinned, mechanism level.** A transport that ignores
    /// cancellation is abandoned when the grace window expires rather than
    /// holding the session open forever.
    #[tokio::test(start_paused = true)]
    async fn a_transport_that_ignores_cancellation_is_forced_after_the_grace_window() {
        let exit = run_until_shutdown(
            std::future::pending::<()>(),
            || {},
            async {},
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(exit, Exit::Forced, "the tail must not be held hostage");
    }

    /// **R1/T82-2 pinned, end to end through axum.** The reviewer's
    /// reproduction in miniature: a client holds a connection open with a
    /// request in flight that never completes — which is what a
    /// streamable-HTTP MCP client's SSE channel is to hyper, a connection that
    /// never goes idle — the signal fires, and `serve_http_bounded` must still
    /// return so `close()` runs. Before the fix `with_graceful_shutdown` waited
    /// on this connection forever and `Memory::close` was never reached.
    ///
    /// Verified to be a real pin: with the grace window raised past the test
    /// timeout, this test hangs.
    #[tokio::test]
    async fn http_shutdown_is_bounded_even_with_a_request_in_flight() {
        use tokio::io::AsyncWriteExt;

        let app = axum::Router::new().route(
            "/stream",
            axum::routing::get(|| async {
                // Outlives the test by far: the connection never goes idle.
                tokio::time::sleep(Duration::from_secs(3_600)).await;
                "unreachable"
            }),
        );
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let (sig_tx, sig_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            serve_http_bounded(
                listener,
                app,
                async move {
                    let _ = sig_rx.await;
                },
                Duration::from_millis(200),
            )
            .await
        });

        let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
        sock.write_all(
            b"GET /stream HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n",
        )
        .await
        .expect("request");
        // Let the server accept the connection and enter the handler.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let _ = sig_tx.send(());
        let out = tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect(
                "serve_http_bounded must return within the grace window, not block on the \
                 open connection",
            )
            .expect("server task");
        assert!(out.is_ok(), "a forced close is not an error: {out:?}");
        drop(sock);
    }

    /// A missing schema must name the remedy, not just the driver's complaint.
    #[test]
    fn an_unprovisioned_store_names_the_provision_step() {
        for raw in [
            "backend: lookup session: error returned from database: (code: 1) no such table: sessions",
            "relation \"sessions\" does not exist",
            "undefined_table",
        ] {
            let out = explain_startup_failure(LamboError::Config(raw.into())).to_string();
            assert!(
                out.contains("lambo provision"),
                "unprovisioned store must name the remedy, got: {out}"
            );
            assert!(
                out.contains(raw),
                "the underlying error must be kept, got: {out}"
            );
        }
    }

    /// …and an unrelated failure must be passed through untouched, so the
    /// hint never masks a different root cause.
    #[test]
    fn an_unrelated_startup_failure_is_passed_through() {
        let out =
            explain_startup_failure(LamboError::Config("connection refused".into())).to_string();
        assert!(out.contains("connection refused"), "{out}");
        assert!(
            !out.contains("lambo provision"),
            "an unrelated failure must not be relabelled as a provisioning problem: {out}"
        );
    }

    // -----------------------------------------------------------------------
    // T8.7 — HTTP surface hardening
    // -----------------------------------------------------------------------

    /// A secret must not be printable, because `ServeOptions` and clap's
    /// `Commands` both derive `Debug` and a future edit will eventually log one.
    #[test]
    fn a_secret_token_never_prints_itself() {
        let t = SecretToken::new("hunter2-the-real-secret").expect("valid");
        let shown = format!("{t:?}");
        assert!(
            !shown.contains("hunter2"),
            "the token leaked through Debug: {shown}"
        );
        assert_eq!(shown, "SecretToken(<redacted>)");
        // And it must not be `Display`-able either — that is the other way a
        // secret reaches a log line. (Compile-time: no `Display` impl exists.)
    }

    /// Empty and whitespace-only tokens are refused rather than accepted as a
    /// credential — an unset variable that expanded to nothing must not
    /// authenticate `Authorization: Bearer `.
    #[test]
    fn an_empty_token_is_refused() {
        assert!(SecretToken::new("").is_err());
        assert!(SecretToken::new("   \t\n ").is_err());
        assert!(SecretToken::new("s").is_ok());
    }

    /// The comparison must be correct first — constant-time is worthless if it
    /// gets the answer wrong.
    #[test]
    fn token_comparison_is_correct_including_lengths() {
        assert!(tokens_match(b"abc123", b"abc123"));
        assert!(!tokens_match(b"abc123", b"abc124"));
        assert!(!tokens_match(b"abc", b"abc123"), "prefix must not match");
        assert!(!tokens_match(b"abc123", b"abc"), "extension must not match");
        assert!(!tokens_match(b"", b"abc"));
        assert!(
            !tokens_match(b"abc", b""),
            "an empty expected token must never match"
        );
    }

    /// The `Authorization` header parse: scheme case-insensitive per RFC 7235,
    /// credential exact.
    #[test]
    fn bearer_header_is_parsed_strictly() {
        let expected = SecretToken::new("s3cret").expect("valid");
        assert!(bearer_ok(Some("Bearer s3cret"), &expected));
        assert!(bearer_ok(Some("bearer s3cret"), &expected), "RFC 7235 §2.1");
        assert!(bearer_ok(Some("BEARER s3cret"), &expected));
        assert!(bearer_ok(Some("  Bearer s3cret  "), &expected));

        assert!(!bearer_ok(None, &expected), "a missing header is a refusal");
        assert!(!bearer_ok(Some("Bearer wrong"), &expected));
        assert!(!bearer_ok(Some("Basic s3cret"), &expected), "wrong scheme");
        assert!(!bearer_ok(Some("s3cret"), &expected), "no scheme at all");
        assert!(!bearer_ok(Some("Bearer"), &expected));
        assert!(!bearer_ok(Some(""), &expected));
    }

    /// **The environment wins.** A token in argv is visible in `ps` and shell
    /// history, so the deployment channel takes precedence — and a set-but-empty
    /// variable is an error, not a silent fallback to the flag.
    #[test]
    fn the_environment_overrides_the_flag() {
        let flag = || Some(SecretToken::new("from-flag").expect("valid"));

        let out = resolve_auth_token_from(flag(), Some("from-env".into())).expect("ok");
        assert_eq!(out, Some(SecretToken::new("from-env").unwrap()));

        let out = resolve_auth_token_from(flag(), None).expect("ok");
        assert_eq!(out, Some(SecretToken::new("from-flag").unwrap()));

        let out = resolve_auth_token_from(None, None).expect("ok");
        assert_eq!(out, None);

        let err = resolve_auth_token_from(flag(), Some("   ".into()))
            .expect_err("a set-but-empty env var must fail closed, not fall back to the flag");
        assert!(err.to_string().contains(AUTH_TOKEN_ENV), "{err}");
    }

    /// **T82-16 pinned, the load-bearing half.** A non-loopback bind without a
    /// token must refuse to start, and the message must tell the operator both
    /// ways out.
    #[test]
    fn a_non_loopback_bind_without_a_token_refuses_to_start() {
        let public: IpAddr = "203.0.113.7".parse().unwrap();
        let any: IpAddr = "0.0.0.0".parse().unwrap();
        let any_v6: IpAddr = "::".parse().unwrap();
        let token = SecretToken::new("t").expect("valid");

        for bind in [public, any, any_v6] {
            let err = authorize_bind(Transport::Http, bind, None)
                .expect_err("{bind} without a token must not start");
            let msg = err.to_string();
            assert!(msg.contains("refusing to start"), "{msg}");
            assert!(msg.contains(AUTH_TOKEN_ENV), "must name the env var: {msg}");
            assert!(msg.contains("--auth-token"), "must name the flag: {msg}");
            assert!(msg.contains("127.0.0.1"), "must name the safe bind: {msg}");
            // The same bind WITH a token is fine.
            authorize_bind(Transport::Http, bind, Some(&token)).expect("token satisfies the rule");
        }
    }

    /// The other three legs of the rule: loopback keeps today's optional-auth
    /// behaviour, and stdio is untouched because it is process-local.
    #[test]
    fn loopback_and_stdio_do_not_require_a_token() {
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let loopback_v6: IpAddr = "::1".parse().unwrap();
        let public: IpAddr = "203.0.113.7".parse().unwrap();

        authorize_bind(Transport::Http, loopback, None).expect("loopback stays optional-auth");
        authorize_bind(Transport::Http, loopback_v6, None).expect("::1 is loopback too");
        // 127.0.0.0/8 in full, not just the .1 host.
        authorize_bind(Transport::Http, "127.9.9.9".parse().unwrap(), None).expect("127/8");
        // stdio ignores --bind entirely: there is no socket to protect.
        authorize_bind(Transport::Stdio, public, None).expect("stdio is process-local");
    }

    /// The bucket allows a burst, refuses past it, and refills with time.
    #[test]
    fn the_rate_limiter_bounds_a_sustained_excess_but_allows_a_burst() {
        let t0 = Instant::now();
        assert!(
            RateLimiter::new(0, t0).is_none(),
            "0 rps is the documented way to disable the limit"
        );

        let rl = RateLimiter::new(10, t0).expect("enabled");
        // Capacity is rps * burst factor, all available at t0.
        for i in 0..20 {
            assert!(rl.try_acquire_at(t0), "burst token {i} must be allowed");
        }
        assert!(
            !rl.try_acquire_at(t0),
            "the 21st in the same instant is out"
        );

        // Half a second later, 10 rps has put ~5 tokens back.
        let t1 = t0 + Duration::from_millis(500);
        for i in 0..5 {
            assert!(rl.try_acquire_at(t1), "refilled token {i}");
        }
        assert!(!rl.try_acquire_at(t1), "but only what was refilled");

        // A long idle refills to capacity and no further.
        let t2 = t1 + Duration::from_secs(3_600);
        for i in 0..20 {
            assert!(rl.try_acquire_at(t2), "post-idle token {i}");
        }
        assert!(
            !rl.try_acquire_at(t2),
            "the bucket must cap at capacity, not accumulate an unbounded idle credit"
        );
    }

    /// Only the request that can mint a session counts against the cap.
    #[test]
    fn only_a_sessionless_post_opens_a_new_session() {
        let req = |method: axum::http::Method, session: Option<&str>| {
            let mut b = axum::http::Request::builder().method(method).uri("/mcp");
            if let Some(s) = session {
                b = b.header("Mcp-Session-Id", s);
            }
            b.body(axum::body::Body::empty()).expect("request")
        };
        assert!(opens_a_new_session(&req(axum::http::Method::POST, None)));
        assert!(
            !opens_a_new_session(&req(axum::http::Method::POST, Some("abc"))),
            "a POST inside an existing session must not be counted again"
        );
        assert!(
            !opens_a_new_session(&req(axum::http::Method::GET, None)),
            "a GET opens an SSE stream, it does not mint a session"
        );
        assert!(!opens_a_new_session(&req(
            axum::http::Method::DELETE,
            Some("abc")
        )));
    }

    /// A fixed live-session count, so the cap is testable without standing up
    /// real MCP sessions.
    struct FakeSessions(usize);

    #[async_trait::async_trait]
    impl LiveSessions for FakeSessions {
        async fn live(&self) -> usize {
            self.0
        }
    }

    fn guard_with(auth: Option<&str>, max_sessions: usize, live: usize, rps: u32) -> HttpGuard {
        HttpGuard {
            auth: auth.map(|t| SecretToken::new(t).expect("valid")),
            max_sessions,
            live: Arc::new(FakeSessions(live)),
            rate: RateLimiter::new(rps, Instant::now()).map(Arc::new),
        }
    }

    /// Stand the guard up in front of a marker route on a real socket.
    ///
    /// Raw TCP rather than an HTTP client: this crate has no client in
    /// dev-dependencies, the status line is all these tests assert on, and the
    /// file already drives axum this way in
    /// `http_shutdown_is_bounded_even_with_a_request_in_flight`.
    async fn spawn_guarded(guard: HttpGuard) -> (SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
        let reached = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits = reached.clone();
        let app = axum::Router::new()
            .route(
                "/mcp",
                axum::routing::any(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        "inner service reached"
                    }
                }),
            )
            .layer(axum::middleware::from_fn_with_state(guard, guard_request));

        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, reached)
    }

    /// Fire one request and return `(status_code, body_ish)`.
    async fn request(addr: SocketAddr, head: &str) -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
        sock.write_all(head.as_bytes()).await.expect("write");
        let mut raw = Vec::new();
        // The marker route and every refusal close or complete promptly; the
        // timeout keeps a regression from hanging the suite.
        let _ = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut raw)).await;
        let text = String::from_utf8_lossy(&raw).to_string();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("no status line in response: {text:?}"));
        (status, text)
    }

    fn post(auth: Option<&str>, session: Option<&str>) -> String {
        let mut h = String::from("POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n");
        if let Some(a) = auth {
            h.push_str(&format!("Authorization: {a}\r\n"));
        }
        if let Some(s) = session {
            h.push_str(&format!("Mcp-Session-Id: {s}\r\n"));
        }
        h.push_str("Connection: close\r\n\r\n");
        h
    }

    /// **T82-16 pinned, request path.** Without a token the request is refused
    /// with 401 and — the part that matters — the inner service is never
    /// reached, so no session is minted and no body is parsed on behalf of an
    /// unauthenticated caller.
    #[tokio::test]
    async fn an_unauthenticated_request_is_refused_before_the_session() {
        let (addr, reached) = spawn_guarded(guard_with(Some("s3cret"), 32, 0, 0)).await;

        let (status, body) = request(addr, &post(None, None)).await;
        assert_eq!(status, 401, "no credential must be 401: {body}");
        assert!(
            body.to_lowercase().contains("www-authenticate: bearer"),
            "a 401 must advertise the scheme: {body}"
        );

        let (status, _) = request(addr, &post(Some("Bearer wrong"), None)).await;
        assert_eq!(status, 401, "a wrong token must be 401");

        let (status, _) = request(addr, &post(Some("Basic s3cret"), None)).await;
        assert_eq!(
            status, 401,
            "the right secret under the wrong scheme is 401"
        );

        assert_eq!(
            reached.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an unauthenticated request must never reach the MCP service"
        );
        // And the token is never echoed back to the caller.
        let (_, body) = request(addr, &post(None, None)).await;
        assert!(!body.contains("s3cret"), "the 401 leaked the token: {body}");
    }

    /// **T82-16 request-size limit.** An oversized declared body is refused
    /// up front with 413 and never reaches the MCP service; a normal-size body
    /// still gets through.
    #[tokio::test]
    async fn an_oversized_request_body_is_refused_before_the_service() {
        let (addr, reached) = spawn_guarded(guard_with(Some("s3cret"), 32, 0, 0)).await;

        let mut h = String::from("POST /mcp HTTP/1.1\r\nHost: localhost\r\n");
        h.push_str(&format!("Content-Length: {}\r\n", MAX_HTTP_BODY_BYTES + 1));
        h.push_str("Authorization: Bearer s3cret\r\nConnection: close\r\n\r\n");
        let (status, body) = request(addr, &h).await;
        assert_eq!(
            status, 413,
            "an oversized declared body must be refused with 413: {body}"
        );
        assert!(
            body.to_lowercase().contains("too large"),
            "the refusal should name the reason: {body}"
        );
        assert_eq!(
            reached.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an oversized body must never reach the MCP service"
        );

        // A body within the cap is still served.
        let (status, _) = request(addr, &post(Some("Bearer s3cret"), None)).await;
        assert_eq!(
            status, 200,
            "a normal-size body must still be served: {status}"
        );
    }

    /// The accepted path: the right token gets through to the service.
    #[tokio::test]
    async fn an_authenticated_request_is_accepted() {
        let (addr, reached) = spawn_guarded(guard_with(Some("s3cret"), 32, 0, 0)).await;
        let (status, body) = request(addr, &post(Some("Bearer s3cret"), None)).await;
        assert_eq!(status, 200, "the right token must be accepted: {body}");
        assert!(body.contains("inner service reached"), "{body}");
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Loopback with no token configured keeps working exactly as before — this
    /// hardening must not break the default local workflow.
    #[tokio::test]
    async fn an_unauthenticated_server_still_serves_loopback() {
        let (addr, reached) = spawn_guarded(guard_with(None, 32, 0, 0)).await;
        let (status, _) = request(addr, &post(None, None)).await;
        assert_eq!(status, 200, "no auth configured means no auth required");
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// **T82-16 pinned, the unbounded-session half.** At the cap the next
    /// `initialize` is refused with an honest 503 that says what the limit is
    /// and how to get under it — and requests belonging to sessions that
    /// already exist keep working.
    #[tokio::test]
    async fn the_thirty_third_session_is_refused_honestly() {
        // 32 live against a cap of 32: the next new session is the 33rd.
        let (addr, reached) = spawn_guarded(guard_with(None, 32, 32, 0)).await;

        let (status, body) = request(addr, &post(None, None)).await;
        assert_eq!(status, 503, "past the cap must refuse: {body}");
        assert!(body.contains("32/32"), "say what the limit is: {body}");
        assert!(
            body.contains("--max-sessions"),
            "name the way to raise it: {body}"
        );
        assert!(
            body.contains("DELETE /mcp"),
            "name the way to free one: {body}"
        );
        assert!(
            body.to_lowercase().contains("retry-after"),
            "a 503 should tell the client when to come back: {body}"
        );
        assert_eq!(
            reached.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a refused session must not reach the service"
        );

        // The cap bounds NEW sessions only — the 32 already established must
        // keep being served, or the cap becomes an outage.
        let (status, _) = request(addr, &post(None, Some("existing-session"))).await;
        assert_eq!(status, 200, "an established session must keep working");
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// One under the cap still opens.
    #[tokio::test]
    async fn a_new_session_under_the_cap_is_admitted() {
        let (addr, reached) = spawn_guarded(guard_with(None, 32, 31, 0)).await;
        let (status, _) = request(addr, &post(None, None)).await;
        assert_eq!(status, 200, "31 live against a cap of 32 has room");
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// The rate limit refuses with 429 once the bucket is dry.
    #[tokio::test]
    async fn a_flood_is_refused_by_the_rate_limit() {
        // 1 rps => capacity 2. The third request in the same moment is out.
        let (addr, _) = spawn_guarded(guard_with(None, 32, 0, 1)).await;
        for i in 0..2 {
            let (status, _) = request(addr, &post(None, None)).await;
            assert_eq!(status, 200, "burst request {i} must be allowed");
        }
        let (status, body) = request(addr, &post(None, None)).await;
        assert_eq!(status, 429, "a sustained excess must be refused: {body}");
        assert!(
            body.to_lowercase().contains("retry-after"),
            "a 429 must tell the client when to retry: {body}"
        );
    }

    /// **Ordering pinned.** Authentication runs before the rate limit and
    /// before the session-count read, so an anonymous flood cannot exhaust the
    /// budget for authenticated callers or probe how loaded the server is.
    #[tokio::test]
    async fn auth_is_checked_before_the_rate_limit_and_the_cap() {
        // A dry-able bucket (capacity 2) AND a server already at its cap.
        let (addr, _) = spawn_guarded(guard_with(Some("s3cret"), 32, 32, 1)).await;

        // Five anonymous requests: all 401, none of them 429 or 503 — so none
        // of them spent a token or learned the session count.
        for i in 0..5 {
            let (status, _) = request(addr, &post(None, None)).await;
            assert_eq!(status, 401, "anonymous request {i} must be refused as 401");
        }

        // The authenticated caller still has its full burst budget.
        let (status, body) = request(addr, &post(Some("Bearer s3cret"), None)).await;
        assert_eq!(
            status, 503,
            "the authenticated caller reaches the cap check with budget intact \
             (503, not 429): {body}"
        );
    }

    /// The fail-closed rule through the **real entry point**, not just the
    /// helper: `serve` itself must refuse a non-loopback bind with no token.
    ///
    /// Worth its own test because the unit test on [`authorize_bind`] proves the
    /// rule and says nothing about whether anyone calls it — an edit that drops
    /// the check from `serve` leaves that test green while shipping an
    /// unauthenticated writer to the world.
    #[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
    mod refuses_to_start {
        use super::*;
        use crate::embed::FixtureEmbedder;
        use crate::store::{MemoryStore, StoreConfig};
        use crate::types::EmbeddingContract;

        fn backends() -> ResolvedBackends {
            ResolvedBackends {
                store: Box::new(MemoryStore::new()),
                embedder: Box::new(FixtureEmbedder::new()),
                store_cfg: StoreConfig {
                    kind: Default::default(),
                    dsn: None,
                    path: None,
                    vector_dim: None,
                },
                embedder_cfg: Default::default(),
                embedding: EmbeddingContract {
                    kind: "fixture".into(),
                    model: None,
                    dim: 1024,
                },
                allow_embedding_mismatch: false,
                config: crate::Config::default(),
            }
        }

        #[tokio::test]
        async fn serve_refuses_an_unauthenticated_non_loopback_bind() {
            let opts = ServeOptions {
                transport: Transport::Http,
                // The shape that actually ships by accident: bind-all in a
                // container, no token, reachable from the network.
                bind: "0.0.0.0".parse().expect("addr"),
                port: 0,
                ..ServeOptions::new("t87-refuse", "agent-a")
            };
            let err = serve(opts, backends())
                .await
                .expect_err("serve must refuse to start, not come up unauthenticated");
            let msg = err.to_string();
            assert!(msg.contains("refusing to start"), "{msg}");
            assert!(msg.contains(AUTH_TOKEN_ENV), "{msg}");
        }

        // The two *positive* legs — an HTTP bind that a token satisfies, and a
        // stdio serve that ignores `--bind` entirely — are pinned by the unit
        // tests on `authorize_bind` rather than through `serve`, deliberately.
        // Driving either one end-to-end means actually starting a server in the
        // test binary: the HTTP leg would bind 0.0.0.0 (firewall prompts, and a
        // listening socket on every interface of a CI box), and the stdio leg
        // would park a *blocking* read on the test harness's stdin — which
        // `Runtime::drop` then waits for, hanging the suite on any machine where
        // stdin is a TTY instead of EOF. Neither risk buys coverage the unit
        // tests do not already give.
    }

    /// **Test-gap (a) pinned.** `run_and_close` is the seam that guarantees
    /// [`Memory::close`] runs on *every* transport exit path. This drives it
    /// with a transport that returns `Ok` and one that returns `Err`, and after
    /// each asserts the session is genuinely closed — a synchronous write is now
    /// refused — so a future edit that skips the close on one branch fails here
    /// rather than in production as a silently-dropped tail.
    #[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
    mod close_runs {
        use super::*;
        use crate::embed::{Embedder, FixtureEmbedder};
        use crate::graph::action::Action;
        use crate::memory::Memory;
        use crate::store::{GraphStore, MemoryStore};
        use crate::types::EmbeddingContract;

        async fn mem(session: &str) -> Arc<Memory> {
            let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
            let m = Memory::builder()
                .session(session)
                .agent("agent-a")
                .flush_interval(Duration::from_secs(3_600))
                .store(store)
                .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
                .embedding_contract(EmbeddingContract {
                    kind: "fixture".into(),
                    model: None,
                    dim: 1024,
                })
                .build()
                .await
                .expect("build");
            Arc::new(m)
        }

        fn assert_closed(m: &Memory, ctx: &str) {
            let action = Action {
                action: "post-close write",
                produces: &[],
                modifies: &[],
                depends_on: &[],
            };
            assert!(
                m.record_action(&action).is_err(),
                "{ctx}: the session must be closed to writers after run_and_close"
            );
        }

        #[tokio::test]
        async fn close_runs_when_the_transport_returns_ok() {
            let m = mem("serve-close-ok").await;
            let pump = tokio::spawn(std::future::pending::<()>());
            let out = run_and_close(m.clone(), async { Ok(()) }, pump).await;
            assert!(out.is_ok(), "clean transport exit closes cleanly: {out:?}");
            assert_closed(&m, "ok path");
        }

        #[tokio::test]
        async fn close_runs_even_when_the_transport_errors() {
            let m = mem("serve-close-err").await;
            let pump = tokio::spawn(std::future::pending::<()>());
            let out = run_and_close(
                m.clone(),
                async { Err(LamboError::Config("transport blew up".into())) },
                pump,
            )
            .await;
            assert!(out.is_err(), "the transport error is surfaced: {out:?}");
            // The whole point: the error path still closed the session.
            assert_closed(&m, "err path");
        }

        /// **T8.6 release-on-close, tied into the lifecycle seam.** A serve
        /// process acquires the single-writer lease on start; a clean exit
        /// through `run_and_close` must **release** it (hand off), not leave it
        /// to expire at the TTL. Proven by a fresh writer — a *different* holder
        /// on the same store — attaching immediately after the close.
        #[tokio::test]
        async fn a_clean_close_releases_the_lease() {
            let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
            let contract = EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 1024,
            };
            let first = Arc::new(
                Memory::builder()
                    .session("serve-lease-release")
                    .agent("agent-a")
                    .flush_interval(Duration::from_secs(3_600))
                    .store(store.clone())
                    .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
                    .embedding_contract(contract.clone())
                    .build()
                    .await
                    .expect("build A"),
            );

            let pump = tokio::spawn(std::future::pending::<()>());
            let out = run_and_close(first.clone(), async { Ok(()) }, pump).await;
            assert!(out.is_ok(), "clean close: {out:?}");
            assert_closed(&first, "release path");

            // The lease was released, so a *different* writer attaches at once
            // (a still-held lease would refuse this with a Conflict).
            let second = Memory::builder()
                .session("serve-lease-release")
                .agent("agent-b")
                .flush_interval(Duration::from_secs(3_600))
                .store(store)
                .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
                .embedding_contract(contract)
                .build()
                .await
                .expect("a clean close must release the lease so a new writer can attach");
            second.close().await.expect("close B");
        }
    }
}

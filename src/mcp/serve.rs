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

use rmcp::service::ServerInitializeError;
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

/// Build-time invariant: the write queue's close quiesce cannot become the
/// reason a `close()` blows the deadline `serve` gives it.
///
/// Asserted here, on the serving side, because the dependency runs this way:
/// the write queue is core and knows nothing about transports, while `serve`
/// is the consumer that sizes `close()`'s budget around it (#27).
const _: () = assert!(
    crate::writeq::WRITE_QUEUE_DRAIN_BUDGET.as_secs() * 4 <= CLOSE_FLUSH_GRACE.as_secs(),
    "WRITE_QUEUE_DRAIN_BUDGET must stay at or under a quarter of CLOSE_FLUSH_GRACE — the write \
     queue quiesce runs in series BEFORE the final flush, so it is carved out of close()'s \
     budget, not added to it",
);

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

/// Does an `Authorization` header value carry the expected bearer token?
///
/// The scheme is matched case-insensitively (RFC 7235 §2.1); the credential
/// itself is compared byte-for-byte in constant time. The parse and the
/// comparison are `crate::surface::bearer`'s, shared with the web portal (#28).
fn bearer_ok(header: Option<&str>, expected: &SecretToken) -> bool {
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
/// J6 added a signal registration at the **acquire** — see [`EarlyShutdown`] —
/// so "the arming" is no longer one point on the far side of `resolve_role`.
/// This group is unaffected: it runs above the acquire, so it is now the
/// **pre-arm** group as well as the pre-lease one, and every member of it still
/// runs under the default signal disposition. That is deliberate rather than
/// incidental. A start that is about to refuse here, or that is about to sit in
/// the election for up to `ELECTION_BUDGET`, must stay killable by a plain
/// SIGTERM (J2-R1-7), and it holds nothing — no lease, no tail, no graph — that
/// a handler could save. J6 adds no member to this group and takes none away.
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
/// with `serve_builder` plus `resolve_role`, because the startup election has
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
    // J6: no pre-arm. This is the *library* entry point (`serve` has not called
    // it since J2-R1-7), and installing process-wide signal handlers is a
    // decision that belongs to a process, not to a builder — the same reason
    // `close_bounded_until` takes its re-armed signal as an argument. An
    // unarmed handle registers nothing and never fires.
    serve_builder(opts, backends, endpoint, None, EarlyShutdown::unarmed())
        .build()
        .await
        .map_err(explain_startup_failure)
}

/// The one [`MemoryBuilder`](crate::MemoryBuilder) a serve process configures.
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
    early: EarlyShutdown,
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
    // J6: armed by the acquire itself, from inside `build_attach`. Handed in
    // here because this is the one place the serve path configures the builder,
    // and the acquire is the only point at which arming is both safe (the
    // election is over) and necessary (a lease and a tail now exist).
    builder = builder.early_shutdown(early);
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
///
/// # It takes the builder by value, so a proxy holds no model (issue #13 review)
///
/// The builder carries the resolved backends, embedder included: with candle
/// on Metal that is ~1.1 GB of weight buffers plus the coalescer's threads.
/// `serve` used to lend it here by reference and keep it alive across the
/// whole `Role::Proxy` arm, so every proxying serve held a full model it never
/// embeds with, for as long as its client stayed attached. Moving the builder
/// in means it is dropped when this returns: a holder's embedder lives on in
/// its `Memory`, a proxy's is released before `HubProxy::run` starts, and
/// `serve` cannot reintroduce the retention because it no longer owns the
/// value. `a_proxy_does_not_retain_the_embedder` pins the proxy half.
async fn resolve_role(
    opts: &ServeOptions,
    builder: crate::memory::MemoryBuilder,
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
///
/// # The cursor moves (JE2E-1)
///
/// `since` used to be computed **once**, at task start, and never advanced:
/// every 500 ms poll therefore re-read and re-allocated the whole accumulated
/// history from `start − LEASE_TTL` to now, and `seen` grew monotonically
/// beside it. On a long-lived `--ledger` holder facing the workstream's own
/// founding scenario — a client that auto-respawns a losing serve — that is
/// quadratic work over the holder's uptime and an unbounded set.
///
/// The cursor is now advanced to the **maximum `at` this poll saw**, which is
/// sound because [`crate::store::GraphStore::pending_lease_refusals`] is
/// inclusive at its lower bound (`refused_at >= since`): the next poll re-reads
/// exactly the newest instant, and the dedup set retires the duplicate. The
/// overlap is deliberate — a cursor advanced *past* the newest row would drop a
/// second refusal stamped at the same store instant.
///
/// `seen` is bounded by the same move: it only ever needs to hold the rows the
/// next poll can re-deliver, which is the rows at the cursor instant, so it is
/// rebuilt per poll from that instant rather than accumulated. Rows *older*
/// than the cursor are unreachable by construction and cannot be re-logged.
/// The bookkeeping is [`RefusalCursor`], extracted for the same reason
/// [`waiting_fits`] was: the *claim about* it is what a review can check.
async fn record_refused_takeovers(
    store: Arc<dyn crate::store::GraphStore>,
    session: crate::types::SessionId,
    agent: crate::types::AgentId,
    my_token: String,
    ledger: Arc<Ledger>,
) {
    let mut cursor = RefusalCursor::starting_at(
        chrono::Utc::now()
            - chrono::Duration::from_std(lease::LEASE_TTL)
                .unwrap_or_else(|_| chrono::Duration::seconds(0)),
    );
    loop {
        tokio::time::sleep(REFUSAL_POLL_INTERVAL).await;
        match store.pending_lease_refusals(&session, cursor.since()).await {
            Ok(refusals) => {
                for r in cursor.take_new(refusals, &my_token) {
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
            Err(_) => {
                // A seed / store blip; the next poll retries.
            }
        }
    }
}

/// How far **below** the cursor each poll re-reads (JE2E-R2-5).
///
/// A refusal's `refused_at` and the moment its row becomes *visible* are not the
/// same instant. On Cockroach `now()` is the transaction's read timestamp while
/// visibility is commit-ordered, so a slow-committing INSERT can surface a row
/// stamped *earlier* than one that committed before it — and a cursor that had
/// already advanced past that stamp would exclude it with `refused_at >= since`
/// forever. A cursor that only moves forward trades unbounded re-reads for that
/// window; re-reading a fixed slice below it trades the window back for a
/// bounded, constant overlap.
///
/// One second is chosen against the thing being absorbed — commit latency plus
/// store-clock offset between two processes — the same quantity
/// [`lease::LEASE_TTL`]'s slack covers at a much larger scale, and two orders
/// above the measured refusal path (a refused start's INSERT is a single
/// statement). It costs one extra second of rows per poll, deduped, and the
/// dedup set is bounded by the same second rather than by uptime.
const REFUSAL_OVERLAP_SECS: i64 = 1;

/// The holder-side refusal poller's read window and its dedup set (JE2E-1,
/// widened by JE2E-R2-5).
///
/// Three invariants:
///
/// * **No refusal is logged twice.** The read is inclusive at its lower bound
///   *and* deliberately overlaps the previous one, so rows come back; `seen` is
///   what retires them.
/// * **No refusal is skipped because it arrived late.** The read starts
///   [`REFUSAL_OVERLAP_SECS`] below the cursor, so a row whose stamp lands under
///   an already-advanced cursor — commit-order versus stamp-order, see the
///   constant — is still delivered and still logged.
/// * **Neither the window nor the set grows with uptime.** The cursor moves
///   forward with the newest row seen, and `seen` is pruned to the overlap
///   window on every poll, so both are bounded by a second of traffic rather
///   than by how long this holder has been up.
///
/// The cursor lands **on** the newest row seen, never past it: a store stamp has
/// finite resolution, so two refusals can share one instant and advancing past
/// it would drop the second. The overlap subsumes that, but the property is
/// kept because it is the cheaper of the two guarantees and does not depend on
/// the constant being right.
///
/// **The residual, stated rather than implied.** A row that becomes visible more
/// than [`REFUSAL_OVERLAP_SECS`] below the cursor is still never logged. The
/// window is not "rows older than the cursor cannot be *re*-logged" — it is that
/// they can never be logged at all — and the constant is what bounds how late a
/// row may be. What survives regardless: the loser's own `refused` line (written
/// by the loser, on its own ledger) and the store row itself, retained for
/// [`lease::LEASE_REFUSAL_RETENTION`]. Only the holder-side `refused_takeover`
/// line is lost, and the recorder is best-effort by construction — a store error
/// already drops a poll.
struct RefusalCursor {
    cursor: chrono::DateTime<chrono::Utc>,
    /// `(refused_by, at)` already logged, for rows inside the overlap window.
    /// Pruned to that window on every poll, which is what bounds it.
    seen: std::collections::HashSet<(String, chrono::DateTime<chrono::Utc>)>,
}

impl RefusalCursor {
    fn starting_at(cursor: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            cursor,
            seen: Default::default(),
        }
    }

    fn overlap() -> chrono::Duration {
        chrono::Duration::seconds(REFUSAL_OVERLAP_SECS)
    }

    /// The lower bound to read from on the next poll: the cursor, less the
    /// overlap.
    fn since(&self) -> chrono::DateTime<chrono::Utc> {
        self.cursor - Self::overlap()
    }

    /// Consume one poll's rows: return the ones that are new to this holder,
    /// and advance the window over them.
    ///
    /// Rows whose `current_holder` is not `my_token` were refused by a
    /// *previous* holder of this session and are none of this process's
    /// business — they are skipped and, deliberately, do **not** move the
    /// cursor: moving it over another holder's row could carry the window past
    /// one of ours stamped at the same instant.
    fn take_new(
        &mut self,
        refusals: Vec<crate::store::lease::LeaseRefusal>,
        my_token: &str,
    ) -> Vec<crate::store::lease::LeaseRefusal> {
        let mut new = Vec::new();
        let mut newest = self.cursor;
        for r in refusals {
            if r.current_holder != my_token {
                continue;
            }
            // One key, one question: has this exact refusal been logged? The
            // overlap makes re-delivery the normal case rather than an edge, so
            // the dedup carries the stamp as well as the token.
            if self.seen.insert((r.refused_by.clone(), r.at)) {
                new.push(r.clone());
            }
            if r.at > newest {
                newest = r.at;
            }
        }
        self.cursor = newest;
        // Everything the next read can re-deliver, and nothing else. This is
        // the line that keeps the set bounded by a second of traffic instead of
        // by this holder's uptime (JE2E-1's other half).
        let floor = self.since();
        self.seen.retain(|(_, at)| *at >= floor);
        new
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
/// continuously-live registration threads through the pre-handshake window (the
/// stdio handshake, the HTTP `bind`) and the transport itself, so a signal in
/// *any* of those still reaches [`Memory::close`] instead of hitting the default
/// disposition and killing the process with the tail un-flushed.
///
/// **Two registrations, not one** (J6). It said "a single" until the pre-arm,
/// and the count is the whole point: the arming below happens the first
/// statement after `resolve_role` returns, but the lease is taken *inside* it,
/// and the span between the two used to run under the default disposition — a
/// SIGTERM there killed the process with `close()` un-run (CI run 32710994512,
/// `unix_wait_status(15)`). `EarlyShutdown` is armed at the acquire itself and
/// only records; `wind_down` reads the record alongside the fresh signal, so a
/// signal in that span cancels the transport on its first poll instead. Neither
/// registration covers the startup election above the acquire, which is what
/// keeps a serve waiting out `ELECTION_BUDGET` killable (J2-R1-7).
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
    // Issue #13 — read before `serve_builder` consumes the backends. Pure (a
    // config lookup and a type downcast), no I/O, so it belongs in this
    // create-nothing pre-lease group; the task it configures is spawned on the
    // holder path only, below the arming.
    let keep_warm = backends.keep_warm_interval();
    // J6 — the pre-arm. Constructing it installs NOTHING; it is armed from
    // inside `build_attach`, in the `LeaseOutcome::Acquired` arm, so the
    // election below stays killable and a serve that loses never arms at all.
    // See `EarlyShutdown` for the window it closes and why the arming point
    // could not simply move above `resolve_role` (J2-R1-7).
    let early = EarlyShutdown::unarmed();
    let builder = serve_builder(
        &opts,
        backends,
        endpoint.as_ref(),
        ledger.clone(),
        early.clone(),
    );
    // Moved, not lent: see `resolve_role` — a proxy must not keep the model.
    let role = resolve_role(&opts, builder, endpoint.as_ref(), &ledger).await;
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
            // **J6 asked whether this branch has the holder's window too, and
            // it does not.** The holder's window is real because the span from
            // the acquire to the arming contains the whole tail of the `Memory`
            // build — including the "Memory session attached" line the
            // pre-handshake test signals on — so a descheduled process can sit
            // in it for milliseconds. This branch's equivalent span is
            // `resolve_role` returning `Role::Proxy`, the match above, and the
            // evaluation of `shutdown_signal()` as the argument on the next
            // line: **no `await` at all**, and no log line a test could
            // synchronise on (the proxy's own sync point, "proxying to the
            // session holder", is logged from inside `run`, already under the
            // registration). Even granting the span, the durability argument
            // above still applies unchanged — no lease, no tail, no graph — so
            // there is nothing for a pre-arm to save. The one thing a kill here
            // does cost is the pre-lease ledger lines, and those are already
            // forfeit to any signal during the election above, which must stay
            // killable. So: no pre-arm on this branch, deliberately.
            //
            // The wedge invariant is what makes that safe to state so flatly.
            // `EarlyShutdown::arm` is called from the `LeaseOutcome::Acquired`
            // arm of `build_attach` and nowhere else, so "a proxy never arms"
            // is not a second rule to keep true — it is "a proxy never takes
            // the lease", read through the signal disposition.
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

    // The signal registration for the whole life of the transport, armed HERE —
    // the first statement after the lease-taking attach returns (`resolve_role`,
    // which builds through the same `serve_builder` `MemoryBuilder`), and
    // before ANY of the startup work below it: `LamboServer::new` and its
    // `#[tool_router]` JSON-schema build, the heartbeat spawn, the issue-13
    // embedder keep-warm spawn, the J4 refusal-recorder spawn, the J2
    // session-endpoint bind and its accept loop, the event pump, and the
    // serve-level attach log. Registration is eager (see
    // `shutdown_signal`), so every one of those runs guarded (R2-a): a SIGTERM
    // arriving during them is taken by this future, `Memory::close` runs, the
    // tail reaches the store, and the single-writer lease is released.
    //
    // **`Ledger::open` is NOT in that list any more, and its absence is the
    // load-bearing correction** (JE2E-6). J4 moved it, and the startup line it
    // writes, into the pre-lease group above `resolve_role`, so both run
    // *unguarded*. Harmless, and for a reason rather than by luck: `Ledger::open`
    // spawns a thread and returns — it performs no I/O of its own, which is the
    // guarantee `opening_a_ledger_does_not_block_even_when_the_paths_open_blocks`
    // pins — so there is no window in it for a signal to be deferred across.
    // What the enumeration is FOR is the R2-a claim, and a stale enumeration
    // makes that claim wrong in the direction that matters: it credits the
    // arming with covering work it no longer covers.
    //
    // The precise property, stated where the previous comment overclaimed
    // (I-R2-1): *this* guard begins the instant `resolve_role` returns. It does
    // NOT cover `resolve_role` itself, so the memory-level "Memory session
    // attached (daemon + flush + canonization running)" line — emitted from
    // inside the `Memory` build, after the lease is taken — is followed by a
    // residual window until this arming. The serve-level "lambo serve: session
    // attached" line below is covered by this one. The earlier wording claimed
    // the stronger property for both lines; it was false for the memory-level
    // one, and I moving `LamboServer::new` up from inside `serve_stdio` widened
    // that residual window from ~6 µs to ~1.1 ms, which is the durability
    // regression I-R2-1 records.
    //
    // **That residual window is no longer unguarded, and J6 is what closed it.**
    // It said "still unguarded ... exactly as it was pre-I" until CI run
    // 32710994512 collected on it: `a_pre_handshake_sigterm_still_flushes_the_
    // session_row` failed with `unix_wait_status(15)` — the process KILLED by
    // the signal, not exited on it, so `close()` never ran and the tail died.
    // Not a flake; timing variance on a loaded runner against a real window that
    // three rounds had priced and accepted. [`EarlyShutdown`] now arms at the
    // acquire, inside `build_attach`, and only RECORDS; `wind_down` reads the
    // record beside the fresh signal below, so a SIGTERM in the residual window
    // makes the transport's very first poll of the shutdown future ready.
    //
    // Arming *before* the attach would shrink the residual window to zero too,
    // and pre-J2 the argument against it was a trade: a durability hazard for an
    // availability one, since the signal would be deferred rather than honoured
    // until a hung build finished. **J2 makes that argument much stronger, and
    // this is the sentence the round-1 review found missing (J2-R1-7).** The
    // thing that would be made SIGTERM-immune is no longer `build_memory` —
    // which `serve` does not call at all any more — but `resolve_role`, and
    // `resolve_role` is a loop that can *legitimately* run for the whole of
    // `ELECTION_BUDGET` = **20 seconds**: that is its designed behaviour when
    // the holder it lost to is not proxyable yet, not a hang.
    //
    // (This said 50 seconds — `LEASE_TTL + ELECTION_SLACK` — until JE2E-7. That
    // WAS the budget; J2-L2 cut it to a client-tolerance number, and
    // `ELECTION_BUDGET`'s own docstring has said the 50s formula "used to be"
    // the rule ever since, in this same file. The argument survives the true
    // number and is re-made at it below, which is the only thing that makes
    // restating it worthwhile.)
    //
    // Twenty seconds of unkillable wait is still the wrong trade, and by a wide
    // margin: it would be spent to close a ~1.1 ms durability window in a
    // process that holds no lease and no tail while it waits — four orders of
    // magnitude of deliberate deafness bought with none of the thing being
    // protected. And the 20s is not a worst case that rarely fires; the
    // dead-holder election measured 10.2s live at the two-client probe, so it
    // is an ordinary start. **That ruling stands, and J6 obeys it rather than
    // overturning it**: the pre-arm sits BELOW the acquire, so the election is
    // over before anything is registered — a serve still waiting for a lease
    // still dies to a SIGTERM, and a serve that loses and proxies never arms at
    // all. This paragraph used to end "the residual window is real and worth
    // closing, but only by racing the attach against the shutdown future, which
    // is a design change and is deferred; see I-R2-1's recommendation". That is
    // what J6 did, at the one `await` where it matters: the startup load inside
    // `build_attach` is raced against the record, so the pre-arm covers no
    // unbounded wait and the immunity is not re-created one `await` down.
    //
    // A fresh registration in `close_bounded` re-arms it for the close phase.
    //
    // JE2E-4: and the *other* thing this process winds down for. `wind_down`
    // races the signal against the lease fence, so losing the lease ends the
    // transport by the same route SIGTERM does. `shutdown_signal()` is still
    // CALLED here, so its eager registration is unmoved — only the polling is
    // wrapped; the contract is documented at `shutdown_signal` itself, which is
    // the function it is a property of (JE2E-R2-1).
    //
    // The ledger goes in too (JE2E-R2-4), so the fence arm can book the
    // `lease:lost` line before it cancels the transport.
    //
    // **This line is the whole of the ruling's behaviour**, and severing it —
    // passing a bare `shutdown_signal()` here while still constructing
    // `wind_down` — used to pass the entire suite (JE2E-R2-2). It is now pinned
    // by `serve_feeds_the_fence_into_the_transports_shutdown`, which drives
    // `run_transport_until_shutdown` the way this call site does.
    // Cloned, not moved: the same signal record is read twice on this path —
    // by `wind_down` for the shutdown itself, and by `close_bounded` for the
    // "was that a SECOND Ctrl-C?" escape hatch. Both readers must see one
    // count, which is exactly why `EarlyShutdown` is `Clone` over shared state.
    let shutdown = holder_shutdown(mem.clone(), ledger.clone(), early.clone());
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
    // Issue #13 — embedder keep-warm. Spawned here, below the arming and
    // beside the heartbeat, for the same reasons: spawning awaits nothing, so
    // the pre-handshake window is not widened, and the loop's first touch is
    // one full interval out, so startup gains no forward. Holder path only: a
    // proxy holds no embedder (it is released when `resolve_role` returns).
    // Aborted when the transport returns, before the close (see
    // `run_and_close`), and again beside the heartbeat after it.
    let keep_warm_task = keep_warm.map(|every| {
        tracing::info!(
            interval_secs = every.as_secs(),
            "lambo serve: embedder keep-warm armed"
        );
        tokio::spawn(crate::embed::keep_warm::keep_warm_loop(
            Arc::clone(mem.embedder()),
            every,
        ))
    });
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
    // JE2E-2: the identity of the socket file this process creates, captured
    // the instant after the bind. It is what licenses the unlink at exit; see
    // `SessionEndpoint::unlink_if_ours` for why the path and the lease are not
    // licences of their own.
    let mut bound_socket: Option<crate::mcp::endpoint::SocketIdentity> = None;
    let hub = match endpoint
        .as_ref()
        .map(|ep| (ep.path().display().to_string(), ep.bind()))
    {
        None => None,
        Some((path, Ok(listener))) => {
            bound_socket = endpoint.as_ref().and_then(|ep| ep.file_identity());
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

    // Issue #13: the keep-warm stops when the transport does, before the close
    // and its final drain; see `run_and_close`.
    let stop_before_close: Vec<_> = keep_warm_task
        .iter()
        .map(tokio::task::JoinHandle::abort_handle)
        .collect();
    let outcome = run_and_close(
        mem.clone(),
        transport,
        event_pump,
        &stop_before_close,
        &early,
    )
    .await;

    // After `close()`, deliberately: the tail's durability is the load-bearing
    // guarantee and the ledger is not allowed to be in front of it. The
    // heartbeat is stopped first so it cannot enqueue a line into a ledger
    // that is draining, and `shutdown` is bounded — a writer stuck on a hung
    // filesystem is abandoned, never allowed to hold process exit.
    if let Some(heartbeat) = heartbeat {
        heartbeat.abort();
    }
    // Issue #13. Already aborted inside `run_and_close`, before the close;
    // repeated here (idempotent) so this exit path aborts it without relying
    // on that. Nothing to drain: a touch writes nothing.
    if let Some(task) = keep_warm_task {
        task.abort();
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
    // JE2E-2. Licensed by the socket's own identity, not by the path and not by
    // the lease: this process may be a FENCED ex-holder whose successor is
    // already listening at this address, and even a clean close released the
    // lease a few statements ago. `unlink_if_ours` removes the file only while
    // it is still the inode this process bound.
    if let Some(endpoint) = &endpoint {
        endpoint.unlink_if_ours(bound_socket);
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
///
/// `stop_before_close` is the opposite case: tasks nothing needs during the
/// close, aborted the moment the transport returns and before `close()`
/// starts. Today that is the issue-13 embedder keep-warm: once no client can
/// call, a touch only competes with the final drain (and on a slow remote
/// embedder could keep a request in flight across it). Aborting is idempotent,
/// so `serve` still aborts the same task after close on its usual path.
pub(crate) async fn run_and_close(
    mem: Arc<Memory>,
    transport: impl Future<Output = Result<(), LamboError>>,
    event_pump: tokio::task::JoinHandle<()>,
    stop_before_close: &[tokio::task::AbortHandle],
    early: &EarlyShutdown,
) -> Result<(), LamboError> {
    let outcome = transport.await;
    for task in stop_before_close {
        task.abort();
    }
    let closed = close_bounded(&mem, early).await;
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
async fn close_bounded(mem: &Memory, early: &EarlyShutdown) -> Result<(), LamboError> {
    close_bounded_until(mem, early.second_signal()).await
}

/// [`close_bounded`] with the re-armed signal passed in.
///
/// `shutdown_signal()` registers process-wide SIGINT/SIGTERM handlers, which a
/// unit test must not do to the whole test binary. Taking the future as an
/// argument lets `memory`'s tests drive the real body with
/// `std::future::pending()` — see `an_abandoned_close_releases_the_lease_through_serve`.
///
/// # J6's pre-arm is deliberately NOT wired in here
///
/// A third registration now exists in a serve process — [`EarlyShutdown`],
/// armed at the acquire — and the question it raises is whether the escape
/// hatch above still works, because that pre-arm's record is *latched*: once a
/// signal sets it, it stays set for the life of the process. Feeding it into
/// this `select!` would make the second arm ready on the first poll of every
/// signal-initiated close, so the close it is meant to rescue would be
/// abandoned before it had a chance to run — the tail lost by the very
/// mechanism that exists to save it.
///
/// So [`close_bounded`] keeps building a **fresh** `shutdown_signal()`, and the
/// property that makes that correct is `tokio::signal`'s: a registration
/// created after a signal was delivered does not replay it, and a signal is
/// delivered to *every* live registration rather than consumed by the first.
/// The first Ctrl-C therefore starts the shutdown and does not abandon the
/// close; a genuine second one reaches this fresh registration and does. The
/// pre-arm can neither trip this early nor swallow the signal that should.
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

/// Did this failed handshake just mean "the client hung up"?
///
/// [`ServerInitializeError::ConnectionClosed`] is the one variant rmcp raises
/// when the transport stream *ended* while it was waiting for a handshake frame
/// — `expect_next_message` seeing `None`, which is its only construction site
/// in the crate. On stdio that is EOF on stdin: the client went away between
/// launching this process and sending `initialize`. That is a disconnect,
/// indistinguishable in kind from the post-handshake EOF [`serve_stdio`]
/// already logs as `client disconnected` and exits 0 on.
///
/// One honest caveat, because the variant is very slightly broader than "clean
/// EOF": rmcp's `AsyncRwTransport::receive` also returns `None` when the
/// underlying read *fails*, not only when it hits end-of-stream. So a genuine
/// I/O fault on stdin arrives here wearing the same variant, and is treated as
/// a hangup. That is accepted deliberately rather than overlooked — rmcp logs
/// the fault itself at ERROR (`Error reading from stream`) so it is never
/// silent, and a process whose stdin is unreadable cannot serve a stdio client
/// by any route: there is no configuration an operator could fix in response,
/// which is what `LamboError::Config` would have been claiming. The variants
/// that *do* describe a fixable fault stay fatal, below.
///
/// Every other variant stays fatal on purpose, because each one is a live peer
/// saying something wrong rather than a peer leaving: `ExpectedInitializeRequest`
/// is a client that opened with the wrong frame, `InitializeFailed` /
/// `UnexpectedInitializeResponse` are protocol violations, and `TransportError`
/// is an I/O fault worth an operator's attention. Folding those into a clean
/// exit would hide real breakage behind a zero exit status.
fn is_pre_handshake_disconnect(e: &ServerInitializeError) -> bool {
    matches!(e, ServerInitializeError::ConnectionClosed(_))
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
    mut shutdown: Pin<&mut HolderShutdown>,
) -> Result<(), LamboError> {
    // Race the handshake against the shutdown signal (R2-a). `shutdown.as_mut()`
    // reborrows, so the same registration is still live for the transport race
    // below if the handshake wins.
    let service = match setup_or_shutdown(server.serve(stdio()), shutdown.as_mut()).await {
        Some(Ok(service)) => service,
        // A client that hangs up *before* it finishes `initialize` has
        // disconnected; it has not misconfigured anything. Reporting that as
        // `LamboError::Config` made `serve` exit non-zero on a completely
        // ordinary lifecycle event, and it split the contract across the
        // handshake boundary: an EOF one frame later lands in
        // `Exit::Finished(Ok(reason))` below and exits 0 with an INFO line.
        //
        // This is the defect CI run 33085161710 collected on, and it is worth
        // spelling out why it presents as a *flaky* test rather than a
        // deterministic one. `Child::wait()` closes the child's stdin before it
        // waits, so the pre-handshake durability test's holder gets two
        // shutdown stimuli in a rush: the `SIGTERM` it sends explicitly, and
        // the stdin EOF that `wait()` causes an instant later. Whichever the
        // holder observes first decides the exit status, because
        // `setup_or_shutdown` is `biased` toward the setup future — so on an
        // unloaded box the signal usually wins and the process exits 0 down the
        // `None` arm, while on a loaded runner the holder is descheduled long
        // enough for the EOF to be sitting there ready when it next polls, the
        // biased arm takes it, and the same run exits 1. Nothing about the test
        // was timing-dependent except which of two correct-to-handle events got
        // there first; only one of them was actually handled.
        //
        // So the fix is here rather than in the test: both orderings are a
        // client going away pre-handshake, and both must close the session and
        // exit 0. `ConnectionClosed` is rmcp's "the transport ended while I was
        // waiting for a frame" — a peer hangup and never a config fault — so it
        // is the only variant folded in; a malformed or rejected `initialize`
        // still fails loudly.
        Some(Err(e)) if is_pre_handshake_disconnect(&e) => {
            tracing::info!(
                reason = %e,
                "mcp stdio: client disconnected before the handshake completed — \
                 closing the session without serving"
            );
            return Ok(());
        }
        Some(Err(e)) => return Err(LamboError::Config(format!("mcp stdio: {e}"))),
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
    mut shutdown: Pin<&mut HolderShutdown>,
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

/// What ends a holder's transport: a signal, **or** losing the single-writer
/// lease (JE2E-4; operator ruling, 2026-08-22).
///
/// The `signal` half is [`shutdown_signal`]'s, and the **eager registration**
/// that makes it work is a property of *that* function and of its call site —
/// `shutdown_signal()` is evaluated as this function's argument, so wrapping it
/// here defers only the polling. Nothing about the arming point moves; see
/// there.
///
/// # A fenced ex-holder winds down instead of living forever
///
/// The fence is an `AtomicBool` that gates *writes*. Before this, nothing tore
/// the serve down when it latched: an ex-holder kept its endpoint listener, its
/// established proxy connections and its own client, answering honest write
/// refusals and — the part that is not honest — silently **stale reads**, with
/// no staleness label, for as long as the process lived. That was the same trade
/// pre-J2, so nobody had re-made it. J2 changed the alternatives: a fenced serve
/// that *exits* is respawned by the client that spawned it, and the respawn
/// finds a live holder and comes back as a **proxy** to it. So the wind-down is
/// a self-heal, and staying up is now the strictly worse option.
///
/// # It is the SIGTERM path, deliberately, and not a faster one
///
/// The fence trip enters exactly the future the signal enters, so everything
/// downstream is unchanged and unduplicated: the transport is cancelled with its
/// grace window, `Memory::close` runs (its fenced branch drops the tail and does
/// **not** release a lease that is no longer ours — there is nothing to flush
/// past the fence, because writes have been refused since it latched), the
/// refusal poller and the heartbeat are aborted, the endpoint is unlinked *only
/// if it is still ours* (JE2E-2 — a fenced holder's successor is listening at
/// the same address, and these two findings compose here), and the ledger is
/// drained last with its `startup` / `lease` / `completion` lines intact. The
/// ordering at `serve`'s tail is what makes that true and it is not changed by
/// this: the drain runs after `run_and_close` returns, on the error path as much
/// as the success one.
///
/// The exit is therefore non-zero — `close`'s fenced branch returns its refusal
/// — which is the honest code: this process's tail was discarded. Both facts a
/// reader needs are in one line here, before any of it runs.
///
/// # It leaves an artifact, not just a stderr line (JE2E-R2-4)
///
/// §J4's bar is that **lease conflicts leave an artifact**, and a lease *loss*
/// is the largest lease event a holder can suffer. It was stderr-only: this
/// arm's `tracing::warn!` and `close()`'s fenced `tracing::error!`, neither of
/// which reaches the shared ledger. `completion` lines cover it only when
/// writes were in flight at the fence — and the commonest fence, like the
/// commonest holder death (JE2E-3's own argument), is **idle**, so an idle
/// fenced holder's ledger simply stopped mid-air. An operator asking "why did
/// this holder exit at T" could reconstruct it from the respawn's `startup` and
/// `proxying` lines, but only inferentially, which is the exact state JE2E-3
/// was filed against.
///
/// So the arm appends `kind:"lease", event:"lost", side:"holder"` naming the
/// winner, **before** it returns and thereby cancels the transport. It survives
/// by the existing ordering rather than by a new guarantee: the ledger is
/// drained at the very end of [`serve`], after `run_and_close`, on the error
/// path as much as the success one. [`crate::ledger::party_key`]'s fallback
/// already files an unlisted event's other party under `counterparty`, which is
/// what this is — a lease token, not a socket path.
/// The only shutdown future [`serve`]'s transports will accept (JE2E-R2-2).
///
/// # Why a newtype instead of `impl Future`
///
/// The ruling's entire behaviour lives in one expression — which future `serve`
/// hands to the transport — and round 2 demonstrated that **severing it passed
/// the whole suite**: replacing [`wind_down`]'s result with a bare
/// `shutdown_signal()` left 1016 tests green while every fenced holder went back
/// to living forever. Two tests pinned `wind_down` and the fenced close in
/// isolation; nothing pinned that `serve` composes them.
///
/// A test is the weaker answer to that, because it pins one spelling of a line
/// that a refactor is free to re-spell. So the transports take
/// `Pin<&mut HolderShutdown>` rather than a generic, and this type has exactly
/// one constructor — [`holder_shutdown`], which always wraps `wind_down`. The
/// severing mutation is now a **type error**, and any future re-plumbing of
/// `serve`'s shutdown still has to produce one of these, which still runs the
/// fence race. Closed by construction rather than by vigilance.
///
/// The box costs one allocation per serve process, at startup, for a future
/// that is polled until the process ends. `wind_down` is an `async fn` and so
/// has no nameable type; boxing is what lets the *type* be the guarantee.
pub(crate) struct HolderShutdown(Pin<Box<dyn Future<Output = ()> + Send>>);

impl Future for HolderShutdown {
    type Output = ();

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.0.as_mut().poll(cx)
    }
}

/// Build this holder's wind-down future — **the only way to get a
/// [`HolderShutdown`]** (JE2E-R2-2).
///
/// `shutdown_signal()` is evaluated here, as an argument, so its eager handler
/// registration happens at this call and not at the first poll; see that
/// function for why that matters and what a lazier spelling would re-open.
///
/// `early` is J6's pre-arm — the registration installed back at the acquire —
/// and it is passed in **alongside** the fresh `shutdown_signal()`, not in
/// place of it. A signal that landed in the window between the two arming
/// points is recorded only by `early`; one that lands after is seen by both.
/// See [`EarlyShutdown`].
fn holder_shutdown(
    mem: Arc<Memory>,
    ledger: Option<Arc<Ledger>>,
    early: EarlyShutdown,
) -> HolderShutdown {
    HolderShutdown(Box::pin(wind_down(shutdown_signal(), early, mem, ledger)))
}

async fn wind_down(
    signal: impl std::future::Future<Output = ()>,
    early: EarlyShutdown,
    mem: Arc<Memory>,
    ledger: Option<Arc<Ledger>>,
) {
    tokio::select! {
        () = signal => {}
        // J6. Ready on the FIRST poll when a signal already arrived in the
        // pre-handshake window, so the transport is cancelled before it serves
        // a byte and `close()` still flushes the tail. Nothing downstream
        // distinguishes the two signal arms — this is the same exit, learned
        // through the earlier registration.
        () = early.fired() => {}
        winner = mem.lease_lost_latched() => {
            if let Some(ledger) = &ledger {
                ledger.append(&crate::ledger::lease_line(
                    "lost",
                    "holder",
                    &mem.session().to_string(),
                    &mem.agent().to_string(),
                    &winner,
                    None,
                ));
            }
            tracing::warn!(
                session = %mem.session(),
                holder = %winner,
                "lambo serve: lease lost to {winner}, exiting so the client can respawn into a \
                 proxy — this process's writes have been refused since the fence latched and its \
                 reads would go on silently serving a graph another writer now owns. The tail it \
                 could not flush is discarded, exactly as a crash would discard it",
            );
        }
    }
}

/// The signal registration armed the instant the single-writer lease is taken
/// (J6), which **only records** that a signal arrived.
///
/// # The window this closes
///
/// [`shutdown_signal`] is armed at [`holder_shutdown`], the first statement
/// after [`resolve_role`] returns. The lease, though, is taken *inside*
/// `resolve_role` — inside the `build_attach` call at the top of its election
/// loop — and everything between the two ran under the **default disposition**:
/// a SIGTERM landing there killed the process outright, so `Memory::close`
/// never ran and the write-behind tail (a clean run already has `mutations=1`,
/// the session-attach record) died with it. That is not a hypothetical: it is
/// CI run 32710994512 failing
/// `a_pre_handshake_sigterm_still_flushes_the_session_row` with
/// `unix_wait_status(15)` — killed by the signal, not exited on it.
///
/// The window is the reason that test's `"session attached"` matcher is loose:
/// it fires on the **memory-level** line, logged from inside the `Memory` build
/// right after the lease is acquired, precisely so the signal lands here rather
/// than in the guarded region below the arming (I-R2-2).
///
/// # Why the arming could not simply move up
///
/// J2-R1-7 rejected arming above `resolve_role`, and that ruling stands: the
/// election loop is allowed to run for the whole of [`ELECTION_BUDGET`] — 20
/// seconds — by design, and a registration nothing polls makes the process
/// **SIGTERM-immune** for exactly as long as nothing polls it. Twenty seconds
/// of unkillable wait, bought to protect a process that holds no lease and no
/// tail, is the worse trade; see the arming comment in [`serve`].
///
/// This type is armed on the **winning** branch only — from the
/// `LeaseOutcome::Acquired` arm of `MemoryBuilder::build_attach`, and from
/// nowhere else. A serve that is still electing has not armed, so the election
/// stays killable; a serve that loses and becomes a proxy never arms at all,
/// which is also how the wedge invariant survives: the hook sits behind the
/// acquire, so "a proxy never arms" is the same statement as "a proxy never
/// takes the lease".
///
/// # Why it does not re-create the immunity one `await` down
///
/// Arming at the acquire puts one genuinely unbounded `await` under the guard:
/// the startup load, which reads the whole durable session back. A passive flag
/// there would be J2-R1-7's trade again with a worse bound — *unbounded*
/// deafness instead of 20 seconds. So `build_attach` **races** that load against
/// [`EarlyShutdown::fired`]: a signal during the load abandons it, releases the
/// freshly-taken lease through the startup-error path that was already there,
/// and returns. Every remaining step under the guard — the daemon / flush /
/// canonization spawns, the attach log, the write pipeline, the `Memory`
/// construction, the return through `resolve_role` and the match in [`serve`] —
/// is synchronous, so there is no second place a signal can be parked across.
/// The guard therefore covers no unbounded wait at all, which is the property
/// that makes it a durability fix rather than an availability regression.
///
/// # What "observe it" means
///
/// The record is a `watch<bool>`, set by a task that awaits one
/// [`shutdown_signal`] and does nothing else — it never blocks, holds no lock
/// and touches no store. Two places read it, which is why it is a watch rather
/// than the signal future itself: the startup-load race above, and
/// [`wind_down`], which selects it alongside the fresh `shutdown_signal()` that
/// [`holder_shutdown`] still arms exactly as it did before. `wait_for` checks
/// the current value first, so if the signal already landed in the window,
/// `wind_down` completes on its **first poll** — the transport is cancelled
/// before it serves a byte, `close()` runs, and the process exits 0 with the
/// tail durable.
///
/// # It cannot swallow a second signal
///
/// `tokio::signal::unix` delivers to *every* live registration for a kind, not
/// to the first one to ask, so this one consuming a SIGTERM does not consume it
/// for the others. In particular [`close_bounded`]'s re-arm — the operator's
/// "press Ctrl-C again to give up on a stalled close" escape — still works, and
/// still is not tripped by the signal that started the shutdown: a `watch`
/// receiver created after a value was sent does replay it, but a *fresh*
/// `signal()` registration does not replay a signal delivered before it
/// existed, and `close_bounded` builds a fresh one.
#[derive(Clone)]
pub(crate) struct EarlyShutdown {
    /// How many shutdown signals this process has been sent, not merely
    /// *whether* it has been sent one. The count is what makes
    /// [`EarlyShutdown::second_signal`] correct — see it for the CI failure
    /// that a boolean could not tell apart.
    signals: tokio::sync::watch::Receiver<u64>,
    /// The sender, kept beside the receiver so [`EarlyShutdown::arm`] can be a
    /// `&self` method on the same handle the builder carries.
    tx: Arc<tokio::sync::watch::Sender<u64>>,
    /// Latches on the first [`EarlyShutdown::arm`] so a second call cannot
    /// install a second registration. `build_attach` calls it exactly once per
    /// acquire and `MemoryBuilder` is `Clone`, so this is belt-and-braces
    /// rather than load-bearing — but a duplicate registration would be a real
    /// leak, and the check is one atomic.
    armed: Arc<std::sync::atomic::AtomicBool>,
}

impl EarlyShutdown {
    /// A handle that is **not yet armed** — no signal handler is installed
    /// until [`EarlyShutdown::arm`] is called.
    ///
    /// Constructing one is free and installs nothing, which is what lets
    /// [`serve`] hand it into the builder *before* the election runs while
    /// still arming only on the winning branch.
    pub(crate) fn unarmed() -> Self {
        let (tx, signals) = tokio::sync::watch::channel(0u64);
        Self {
            signals,
            tx: Arc::new(tx),
            armed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Install the registration. Called from the `LeaseOutcome::Acquired` arm
    /// of `MemoryBuilder::build_attach` and from nowhere else.
    ///
    /// Synchronous and non-blocking on purpose: it must not add an `await` to
    /// the acquire it follows. [`shutdown_signal`]'s registration is eager, so
    /// the handlers are installed by the time this returns — a signal that
    /// arrives one instruction later is already buffered by the registration
    /// rather than killing the process.
    pub(crate) fn arm(&self) {
        use std::sync::atomic::Ordering;
        if self.armed.swap(true, Ordering::SeqCst) {
            return;
        }
        // EAGER: `shutdown_signal()` is called HERE, not inside the spawned
        // task, so the handlers exist before this function returns rather than
        // whenever the scheduler first polls the task. Moving the call into the
        // `async move` below would re-open the very window this type closes,
        // with every gate green — the same trap `shutdown_signal`'s own
        // docstring records for `wind_down`.
        //
        // It counts rather than latches, and keeps counting after the first:
        // the close-phase escape hatch reads this record to tell an operator's
        // *second* Ctrl-C from the first, and it cannot do that from a boolean.
        let counter = shutdown_signal_counter();
        let tx = Arc::clone(&self.tx);
        tokio::spawn(counter(tx));
    }

    /// Whether [`EarlyShutdown::arm`] has run on this handle.
    ///
    /// The J6 pin that `build_attach` arms on the winning branch **and only**
    /// there: a losing attach must leave this `false`, which is the wedge
    /// invariant read through the signal disposition — a process that never
    /// took the lease never changed how it dies.
    // Gated exactly as their only consumers are: the J6 disposition tests live
    // in a `store-memory` + `embed-fixture` module, so a bare `#[cfg(test)]`
    // leaves these dead under `--no-default-features --features store-sqlite`
    // and CI's RUSTFLAGS `-D warnings` turns that into a build failure.
    #[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
    pub(crate) fn is_armed(&self) -> bool {
        self.armed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Record a signal without sending one.
    ///
    /// [`EarlyShutdown::arm`] installs process-wide SIGINT/SIGTERM handlers,
    /// which a unit test must not do to the whole test binary just to assert
    /// what happens *after* the record exists — the same reason
    /// [`close_bounded_until`] takes its re-armed signal as an argument. This
    /// sets the watch directly, so the observer side can be driven with no
    /// handler and no real signal.
    // Gated exactly as their only consumers are: the J6 disposition tests live
    // in a `store-memory` + `embed-fixture` module, so a bare `#[cfg(test)]`
    // leaves these dead under `--no-default-features --features store-sqlite`
    // and CI's RUSTFLAGS `-D warnings` turns that into a build failure.
    #[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
    pub(crate) fn simulate_signal(&self) {
        self.tx.send_modify(|n| *n += 1);
    }

    /// Resolve once a signal has been recorded — **immediately** if one already
    /// was.
    ///
    /// `wait_for` inspects the current value before it waits, which is the
    /// whole point: the interesting case is a signal that landed while the
    /// session was still attaching, long before anything asked.
    pub(crate) async fn fired(&self) {
        self.at_least(1).await;
    }

    /// Resolve once a **second** shutdown signal has been recorded.
    ///
    /// This is [`close_bounded`]'s escape hatch — the operator who watches a
    /// close stall and presses Ctrl-C again — and it is expressed as "the
    /// count reached two" rather than as a fresh `signal()` registration
    /// because the fresh registration got it wrong, and lost data doing so.
    ///
    /// The old spelling reasoned that a registration created after a signal was
    /// *delivered* does not replay it, so anything it caught had to be a second
    /// signal. Delivery is not the event that matters, though: tokio's unix
    /// handler only sets a flag and writes a byte to its self-pipe, and the
    /// watch that wakes registrations is not sent until the signal driver task
    /// gets scheduled to drain that pipe. Under CPU contention those are
    /// milliseconds apart, and any registration created in the gap sees the
    /// *first* signal and calls it the second.
    ///
    /// That is exactly what the pre-handshake durability test hit once the
    /// stdio hangup above was classified correctly: the holder closed on the
    /// stdin EOF that `Child::wait()` causes, `close_bounded` armed a fresh
    /// registration, and the single `SIGTERM` the test had already sent then
    /// landed on it — so the close was abandoned, `final flush failed — tail
    /// lost on exit` was logged, and the holder exited 1 with the session row
    /// gone. One signal, read as two. And it is not a test-only shape: closing
    /// stdin and then sending `SIGTERM` is the shutdown sequence the MCP spec
    /// prescribes for clients, so a real client shutting a holder down could
    /// lose the tail the same way.
    ///
    /// A count cannot be fooled by when the record is written: one `kill` is
    /// one increment whenever the driver gets round to it. Two signals close
    /// enough together to coalesce inside one `watch` value would count as one,
    /// which errs toward finishing the close — the safe direction, and not the
    /// shape of the impatient-operator case this serves anyway.
    ///
    /// Why an absolute two rather than "one more than the count when the close
    /// began": reading a baseline at the top of the close would reintroduce the
    /// very race this fixes, because the whole problem is that the first
    /// signal's increment may not have been written *yet* when the close
    /// starts. A baseline of zero read in that window makes the first signal
    /// look like the increment, and the close is abandoned again. Two is
    /// immune precisely because it does not depend on reading anything at a
    /// particular moment.
    ///
    /// The one behaviour this trades away, stated plainly: on a close reached
    /// *without* any signal — a client hangup, the `ConnectionClosed` path
    /// above — an operator's first Ctrl-C no longer abandons the close; it
    /// takes two. That is a deliberate trade and a small one, because the
    /// close is already bounded by [`CLOSE_GRACE`], so the cost is a bounded
    /// wait rather than a hang, and the thing bought with it is that a tail is
    /// never thrown away on a signal the operator only sent once.
    pub(crate) async fn second_signal(&self) {
        self.at_least(2).await;
    }

    /// Resolve once at least `n` shutdown signals have been recorded —
    /// **immediately** if that many already were.
    ///
    /// Parks forever on an unarmed handle: nothing is counting, so no claim
    /// about signals can honestly be made. On the serve path that cannot
    /// happen for anything that closes a session — [`EarlyShutdown::arm`] runs
    /// in the acquire, and only a process that acquired has a `Memory` to
    /// close — and on the library path ([`build_memory`]) an unarmed handle is
    /// the whole point.
    async fn at_least(&self, n: u64) {
        let mut rx = self.signals.clone();
        // `Err` means every sender is gone, which cannot happen while `self`
        // holds one; treat it as "no signal" and park rather than reporting a
        // shutdown nobody asked for.
        if rx.wait_for(|seen| *seen >= n).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// The builder's view of the pre-arm (#27): `memory` names this trait, not
/// the type, so the core does not depend on the serving layer. Both methods
/// forward to the inherent ones above.
impl crate::memory::AttachShutdown for EarlyShutdown {
    fn arm(&self) {
        EarlyShutdown::arm(self);
    }

    fn fired(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(EarlyShutdown::fired(self))
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
/// only makes the arming *point* effective; it does not move it. The call site
/// in [`serve`] sits as early as it does for that reason (I-R2-1), and it
/// cannot move above `resolve_role`: that loop is allowed to run for the whole
/// of [`ELECTION_BUDGET`], **20 seconds**, by design, and arming over it would
/// make that wait unkillable (J2-R1-7). The figure was written here as 50
/// seconds — the pre-J2-L2 budget — until JE2E-7; the argument holds at 20s.
/// See the arming comment in [`serve`] for the trade written out.
///
/// # It is not the *only* registration any more (J6)
///
/// This paragraph said "everything before the call site in [`serve`] — the
/// pre-lease group (the endpoint derivation, `Ledger::open` and its startup
/// line, J4) and `resolve_role`, which takes the lease — is still unguarded".
/// Half of that is now false, and the false half is the half that lost data:
/// CI run 32710994512 killed a serve with `unix_wait_status(15)` inside
/// `resolve_role`, after the lease was taken, with `close()` un-run.
///
/// [`EarlyShutdown`] arms a second registration at the acquire — inside
/// `build_attach`, in the `LeaseOutcome::Acquired` arm — for the same eager
/// reason this function documents, and *only* records the arrival for
/// [`wind_down`] to read. So the accurate statement is now:
///
/// * the **pre-lease group** and the **election** above the acquire are
///   unguarded, deliberately, and stay killable — that is J2-R1-7's ruling and
///   J6 does not touch it;
/// * from the **acquire** onward the process is covered, first by the pre-arm
///   and then by this call site, with no gap between them.
///
/// The pre-arm calls *this* function, so its eagerness is this contract, used
/// twice. A future edit that made `EarlyShutdown::arm` construct the future
/// lazily instead would re-open the window with every gate green, exactly as
/// the `wind_down` trap below would.
///
/// **The eagerness survives [`wind_down`]** (JE2E-4), and the reason is which
/// expression runs when: `serve` writes `wind_down(shutdown_signal(), …)`, so
/// this function is *called* — and its handlers installed — while the argument
/// is evaluated, before `wind_down`'s body has been polled at all. A future
/// edit that moved the call inside `wind_down`'s body, or replaced the argument
/// with a lazily-constructed future, would silently re-open the R2-a window with
/// every gate green. That is the whole reason this contract is documented here,
/// on the function it is a property of, rather than beside the wrapper.
/// (JE2E-R2-1: it briefly *was* beside the wrapper — a `wind_down` inserted
/// between this docstring and this signature took the block with it, leaving
/// `shutdown_signal` undocumented and the eager-registration contract attached
/// to an `async fn` that installs nothing at call time.)
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

/// [`shutdown_signal`], but it never stops listening — it counts.
///
/// Returns a *builder* of the counting future rather than the future itself so
/// the registration keeps [`shutdown_signal`]'s eager contract: the `signal()`
/// calls run when this function is called, inside [`EarlyShutdown::arm`], not
/// when the spawned task is first polled. A signal arriving one instruction
/// after `arm` returns is therefore already buffered by a live registration
/// rather than hitting the default disposition, which is the property R2-a and
/// J6 both turn on.
///
/// One registration, polled in a loop, is deliberate and is the part that makes
/// the count trustworthy. Creating a *new* registration per signal — the
/// obvious alternative — leaves a window between one signal being recorded and
/// the next registration existing, and a signal in that window is not counted
/// at all. That is the same class of mistake as the one
/// [`EarlyShutdown::second_signal`] documents, only in the other direction: it
/// would make the operator's second Ctrl-C occasionally do nothing.
fn shutdown_signal_counter(
) -> impl FnOnce(Arc<tokio::sync::watch::Sender<u64>>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // Register both handlers NOW, exactly as `shutdown_signal` does and for
        // exactly the same reason. Errors (exotic platforms, exhausted signal
        // slots) degrade to the lazy ctrl_c path rather than failing.
        let int = signal(SignalKind::interrupt());
        let term = signal(SignalKind::terminate());
        move |tx| {
            Box::pin(async move {
                match (int, term) {
                    (Ok(mut int), Ok(mut term)) => loop {
                        // The `Option` is load-bearing and must not be dropped
                        // on the floor. `recv()` yields `None` when the
                        // registration behind it is gone, and `None` is
                        // *immediately* ready forever after — so a loop that
                        // treated it as an arrival would spin a core at full
                        // tilt and, far worse, drive this counter up until it
                        // tripped `second_signal` and abandoned a close that
                        // nobody had asked to abandon. That is the tail-loss
                        // failure this whole type exists to prevent, delivered
                        // by the fix for it. Tokio's global signal registry
                        // never drops its sender, so this is a guard rather
                        // than an expected path — which is exactly why it has
                        // to be written down rather than assumed.
                        let arrived = tokio::select! {
                            v = int.recv() => v.is_some(),
                            v = term.recv() => v.is_some(),
                        };
                        if !arrived {
                            return;
                        }
                        // A receiver is always alive (the `EarlyShutdown` this
                        // was spawned from holds one), and a failed send would
                        // mean the shutdown path is already gone.
                        tx.send_modify(|n| *n += 1);
                    },
                    (Ok(mut int), Err(_)) => loop {
                        if int.recv().await.is_none() {
                            return;
                        }
                        tx.send_modify(|n| *n += 1);
                    },
                    (Err(_), Ok(mut term)) => loop {
                        if term.recv().await.is_none() {
                            return;
                        }
                        tx.send_modify(|n| *n += 1);
                    },
                    // No registration at all: `ctrl_c()` resolves once, so this
                    // records the first signal and nothing after it. Degraded,
                    // and honest about being degraded — the escape hatch simply
                    // never trips, which loses no data.
                    (Err(_), Err(_)) => {
                        if tokio::signal::ctrl_c().await.is_ok() {
                            tx.send_modify(|n| *n += 1);
                        }
                    }
                }
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        }
    }
    #[cfg(not(unix))]
    {
        move |tx: Arc<tokio::sync::watch::Sender<u64>>| {
            Box::pin(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    tx.send_modify(|n| *n += 1);
                }
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        }
    }
}

#[cfg(test)]
mod tests;

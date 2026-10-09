//! T8.7, the HTTP surface's hardening: the bearer token, the fail-closed
//! bind rule, the request-rate bucket, the concurrent-session cap and the
//! body-size ceiling, applied by [`guard_request`] in front of rmcp.
//!
//! The comparison itself is `crate::surface::bearer`'s, shared with the web
//! portal; this module owns the serve-side token type, its environment
//! precedence and the order the checks run in. Since #32 PR 5 the bearer
//! check resolves the request's credential among every configured one
//! (`super::authority`) and hands its grant to the router, which checks the
//! session scope.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

use super::authority::{Authenticated, ServeAuthority};
use super::Transport;
use crate::types::LamboError;

/// Environment variable holding the HTTP bearer token. **Takes precedence over
/// `--auth-token`**: a process manager can inject the secret without it ever
/// appearing in a command line (where `ps` and shell history would expose it).
pub const AUTH_TOKEN_ENV: &str = crate::config::secret_env::SERVE_AUTH_TOKEN_ENV;

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
    /// Reject empty and whitespace-only tokens, and tokens no request could
    /// present: surrounding whitespace, or a byte outside printable ASCII
    /// (0x20 to 0x7E; a space inside the token is presentable). The errors
    /// never quote the token.
    ///
    /// Fail closed rather than quietly accept: an empty `LAMBO_AUTH_TOKEN` is
    /// almost always an unset variable that expanded to nothing, and treating it
    /// as a valid credential would authenticate every request that sends
    /// `Authorization: Bearer `.
    ///
    /// Also reject a token longer than
    /// [`MAX_BEARER_CREDENTIAL_BYTES`](crate::surface::bearer::MAX_BEARER_CREDENTIAL_BYTES)
    /// (#32 PR 5 review L2): every presented credential over that length is
    /// refused unread, so such a token could never authenticate anything.
    pub fn new(raw: impl Into<String>) -> Result<Self, String> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(
                "auth token is empty — pass a non-empty secret, or omit it entirely to \
                        run unauthenticated on loopback"
                    .into(),
            );
        }
        // #32 PR 5 review L3: a token no request can carry. The guard trims
        // the presented credential, so surrounding whitespace never
        // matches, and an HTTP header value that is not printable ASCII is
        // unreadable to it, so such a byte never matches either. Either
        // used to start a serve that answered every request 401 with no
        // hint why (a trailing newline or space from an env file, say).
        if raw.trim() != raw {
            return Err(
                "auth token has leading or trailing whitespace, which a request cannot \
                        carry (the presented credential is trimmed); remove it"
                    .into(),
            );
        }
        if raw.bytes().any(|b| !(0x20..=0x7e).contains(&b)) {
            return Err(
                "auth token contains a character outside printable ASCII, which an HTTP \
                 Authorization header cannot carry"
                    .into(),
            );
        }
        if raw.len() > crate::surface::bearer::MAX_BEARER_CREDENTIAL_BYTES {
            return Err(format!(
                "auth token is longer than {} bytes, so no request could present it",
                crate::surface::bearer::MAX_BEARER_CREDENTIAL_BYTES
            ));
        }
        Ok(Self(raw))
    }

    /// The secret, for the constant-time comparison only.
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
///
/// A set variable that is not valid UTF-8 is an error too (#32 PR 5 review
/// S2), never "unset": read as unset, a loopback serve with no other
/// credential would start unauthenticated while the operator believes a
/// token is required. The error names the variable, never its value.
pub fn resolve_auth_token(flag: Option<SecretToken>) -> Result<Option<SecretToken>, LamboError> {
    resolve_auth_token_from(flag, std::env::var_os(AUTH_TOKEN_ENV))
}

pub(super) fn resolve_auth_token_from(
    flag: Option<SecretToken>,
    env: Option<std::ffi::OsString>,
) -> Result<Option<SecretToken>, LamboError> {
    match env {
        Some(raw) => {
            let raw = raw
                .into_string()
                .map_err(|_| LamboError::Config(format!("{AUTH_TOKEN_ENV}: is not valid UTF-8")))?;
            SecretToken::new(raw)
                .map(Some)
                .map_err(|e| LamboError::Config(format!("{AUTH_TOKEN_ENV}: {e}")))
        }
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
///
/// ## What #32 PR 5 changed
///
/// `token` is now *any* credential the serve has (`authority::any_credential`):
/// the legacy token or a configured `[[serve.credential]]`. Either one means
/// every request must present a bearer token, so either satisfies the rule.
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
         *writer* beyond loopback. Set {AUTH_TOKEN_ENV} (or pass --auth-token), or configure a \
         [[serve.credential]] in lambo.toml, to require 'Authorization: Bearer <token>' on every \
         request, or bind 127.0.0.1 and reach it through a tunnel or an authenticating proxy."
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
/// The limit is **per credential** (#32 PR 5 review M2, see
/// [`CredentialRates`]), not per connection: per-connection state would be
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
}

/// One request-rate bucket per credential (#32 PR 5 review M2).
///
/// Before credentials the serve had one caller, so one global bucket was one
/// caller's bucket. With several, a global bucket let one credential (a
/// lower-trust tenant on its own prefix, say) spend everyone's budget and
/// turn the operator's requests into 429s. Each credential now draws from
/// its own bucket at `--rate-limit-rps` (burst 2x), keyed by the grant's
/// name, so no credential can starve another. A serve with one credential
/// (the dogfood rig's legacy `default`, or the implicit `local`) has exactly
/// one bucket at that rate, which is the limit it always had. The process
/// as a whole is bounded at `--rate-limit-rps` times the number of
/// credentials, which the operator configures.
///
/// Buckets are made on a credential's first request. The names are the
/// configured set (a request reaches this only once authenticated), so the
/// map is bounded by it.
pub(crate) struct CredentialRates {
    rps: u32,
    buckets: parking_lot::Mutex<std::collections::HashMap<String, Arc<RateLimiter>>>,
}

impl CredentialRates {
    /// `None` when `rps == 0`, the documented way to disable the limit.
    pub(super) fn new(rps: u32) -> Option<Self> {
        (rps != 0).then(|| Self {
            rps,
            buckets: parking_lot::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Take one token from `credential`'s bucket if it has one. `now` is a
    /// parameter so the tests drive the refill deterministically.
    pub(super) fn try_acquire_at(&self, credential: &str, now: Instant) -> bool {
        // The map's lock only finds the bucket; the bucket's own lock takes
        // the token, after the map's is released.
        let bucket = {
            let mut buckets = self.buckets.lock();
            match buckets.get(credential) {
                Some(bucket) => Arc::clone(bucket),
                None => match RateLimiter::new(self.rps, now) {
                    Some(bucket) => {
                        let bucket = Arc::new(bucket);
                        buckets.insert(credential.to_string(), Arc::clone(&bucket));
                        bucket
                    }
                    None => return true,
                },
            }
        };
        bucket.try_acquire_at(now)
    }

    pub(super) fn try_acquire(&self, credential: &str) -> bool {
        self.try_acquire_at(credential, Instant::now())
    }
}

/// Each credential's share of `--max-sessions` (#32 PR 5 review M2): the
/// cap divided evenly among the credentials a request can arrive as,
/// rounded down, and at least 1.
///
/// Rounded down so the shares never add up to more than the cap: every
/// credential can always open its share, whatever the others hold, which is
/// the guarantee that one credential cannot lock the others (the operator's
/// included) out. With one credential the share is the whole cap, so a
/// one-credential serve behaves exactly as before. An idle credential's
/// share is not lent out; an operator who needs more per credential raises
/// `--max-sessions`. With more credentials than the cap, each gets 1 and the
/// process-wide cap still applies first, so the guarantee then fails
/// gracefully rather than refusing the start.
pub(super) fn credential_share(max_sessions: usize, credentials: usize) -> usize {
    (max_sessions / credentials.max(1)).max(1)
}

/// How often the guard logs a refused bearer token at WARN (#32 PR 5 review
/// L2): once per window, with the count of refusals it held back.
pub(super) const REFUSAL_WARN_WINDOW: Duration = Duration::from_secs(10);

/// One WARN line per [`REFUSAL_WARN_WINDOW`] for unauthenticated requests
/// (#32 PR 5 review L2).
///
/// A 401 is answered before the rate limit (an unauthenticated caller must
/// not spend anyone's budget), so nothing bounded how many of them a caller
/// could send, and each one wrote a WARN line: a flood of bad tokens was a
/// log flood. Now the first refusal in a window is logged at WARN, carrying
/// how many were held back since the last one, and the rest at DEBUG.
pub(super) struct RefusalLog {
    window: Duration,
    state: parking_lot::Mutex<RefusalState>,
}

struct RefusalState {
    last_warned: Option<Instant>,
    held_back: u64,
}

impl RefusalLog {
    pub(super) fn new(window: Duration) -> Self {
        Self {
            window,
            state: parking_lot::Mutex::new(RefusalState {
                last_warned: None,
                held_back: 0,
            }),
        }
    }

    /// Note one refusal at `now`: `Some(held_back)` when it should be
    /// logged at WARN (and how many were held back before it), `None` when
    /// it falls inside the current window.
    pub(super) fn note_at(&self, now: Instant) -> Option<u64> {
        let mut state = self.state.lock();
        let due = state
            .last_warned
            .is_none_or(|last| now.saturating_duration_since(last) >= self.window);
        if due {
            state.last_warned = Some(now);
            Some(std::mem::take(&mut state.held_back))
        } else {
            state.held_back += 1;
            None
        }
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

    /// How many of them `credential` opened (#32 PR 5 review M2). The
    /// default counts every live one, the conservative answer for a source
    /// that does not know who opened what.
    async fn live_opened_by(&self, _credential: &str) -> usize {
        self.live().await
    }
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

/// The checks every HTTP request passes before it reaches rmcp.
#[derive(Clone)]
pub(crate) struct HttpGuard {
    /// Who may call at all, and as which credential (#32 PR 5).
    pub(super) authority: Arc<ServeAuthority>,
    /// The process-wide MCP-session cap (`--max-sessions`).
    pub(super) max_sessions: usize,
    /// Each credential's share of [`Self::max_sessions`]
    /// ([`credential_share`]).
    pub(super) credential_sessions: usize,
    pub(super) live: Arc<dyn LiveSessions>,
    /// Each credential's request-rate bucket; `None` when disabled.
    pub(super) rate: Option<Arc<CredentialRates>>,
    /// The throttle on the 401's WARN line.
    pub(super) refusals: Arc<RefusalLog>,
    /// Session-opening requests admitted but not yet counted by
    /// [`Self::live`] (#32 PR 5 review S4).
    pub(super) openings: Arc<Openings>,
    /// How long a request's body may take to arrive
    /// ([`REQUEST_BODY_TIMEOUT`]; the tests shorten it).
    pub(super) body_timeout: Duration,
}

impl HttpGuard {
    /// The guard for `authority`, with `--max-sessions` and
    /// `--rate-limit-rps` divided among its credentials as
    /// [`credential_share`] and [`CredentialRates`] say.
    pub(super) fn new(
        authority: Arc<ServeAuthority>,
        max_sessions: usize,
        live: Arc<dyn LiveSessions>,
        rate_limit_rps: u32,
    ) -> Self {
        let credential_sessions = credential_share(max_sessions, authority.credential_count());
        Self {
            authority,
            max_sessions,
            credential_sessions,
            live,
            rate: CredentialRates::new(rate_limit_rps).map(Arc::new),
            refusals: Arc::new(RefusalLog::new(REFUSAL_WARN_WINDOW)),
            openings: Arc::new(Openings::default()),
            body_timeout: REQUEST_BODY_TIMEOUT,
        }
    }
}

/// The session-opening requests the cap has admitted and that may not yet
/// show in [`LiveSessions`] (#32 PR 5 review S4).
///
/// The cap and the share are a check, then an act: the guard reads the live
/// count, and the MCP session appears in it (and is attributed to its
/// opener) only once rmcp has handled the request. Without this, N
/// `initialize`s arriving together at one under the cap all read the same
/// count and all pass, overshooting `--max-sessions` and a credential's
/// share by up to the rate limit's burst. Each admitted opener now holds an
/// [`Opening`] until its MCP session is minted and attributed (or the
/// request ends without one), and the guard counts those beside the live
/// ones. The guard reserves and reads the others in one step, *before* it
/// reads the live count: a reservation released after that read is counted
/// here, and one released before it already shows as live (the session is
/// minted and attributed before its reservation goes). Two openers racing
/// for the last slot can both be refused (the conservative side, and
/// either retries after `Retry-After`); neither can overshoot.
#[derive(Default)]
pub(super) struct Openings {
    counts: parking_lot::Mutex<OpeningCounts>,
}

#[derive(Default)]
struct OpeningCounts {
    total: usize,
    by_credential: std::collections::HashMap<Arc<str>, usize>,
}

impl Openings {
    /// Reserve an opening for `credential`. Returns the reservation and the
    /// openings already held, in all and by `credential`, not counting
    /// this one.
    fn reserve(self: &Arc<Self>, credential: &str) -> (Opening, usize, usize) {
        let mut counts = self.counts.lock();
        let others = counts.total;
        counts.total += 1;
        let credential: Arc<str> = Arc::from(credential);
        let mine = counts
            .by_credential
            .entry(Arc::clone(&credential))
            .or_default();
        let others_mine = *mine;
        *mine += 1;
        drop(counts);
        (
            Opening {
                openings: Arc::clone(self),
                credential,
            },
            others,
            others_mine,
        )
    }
}

/// One admitted session-opening request's place in [`Openings`], given
/// back on drop: when `transport::serve_live` has recorded the opener of
/// the MCP session rmcp minted, or when the request ends without reaching
/// it (refused later, routed elsewhere, the client gone before rmcp).
pub(crate) struct Opening {
    openings: Arc<Openings>,
    credential: Arc<str>,
}

impl Drop for Opening {
    fn drop(&mut self) {
        let mut counts = self.openings.counts.lock();
        counts.total -= 1;
        if let Some(n) = counts.by_credential.get_mut(&self.credential) {
            *n -= 1;
            if *n == 0 {
                counts.by_credential.remove(&self.credential);
            }
        }
    }
}

/// An [`Opening`] as a request extension (extensions must be `Clone`).
/// `transport::serve_live` takes it out before rmcp sees the request, so
/// rmcp never keeps it alive with the request's parts.
#[derive(Clone)]
pub(crate) struct OpeningReservation {
    _opening: Arc<Opening>,
}

/// Ceiling on the size of a single HTTP request body (T82-16 remainder).
///
/// The tool layer already bounds every client string (16 KiB) and the per-call
/// concept count (64 ≈ ~1 MiB of content), and the rate limit bounds request
/// *count* — but the transport itself imposed no ceiling, so a body padded with
/// rejected or oversized fields still incurred parse + validation cost before
/// the tool layer refused it. This caps the *declared* body of a request before
/// any of it is parsed, and the guard's read of the body (#32 PR 5 second
/// review L2) caps what actually arrives, so a chunked body is held to it too.
pub(super) const MAX_HTTP_BODY_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB

/// How long [`guard_request`] waits for the whole body of a session-opening
/// request ([`opens_a_new_session`]; no other body is read by the guard),
/// counted from when it starts reading it (#32 PR 5 second review L2).
///
/// The guard reads the body before the session cap reserves a slot, so a
/// client that dribbles its body holds no slot while it does; this bounds
/// how long it holds its connection. 30 s is far beyond any honest client:
/// MCP bodies are a few KiB and capped at [`MAX_HTTP_BODY_BYTES`]. A body
/// not complete by then is refused with `408 Request Timeout`. The serve
/// had no request timeout of any kind before this one.
pub(super) const REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Why [`read_body`] gave up.
enum BodyRead {
    /// More than [`MAX_HTTP_BODY_BYTES`] arrived (a chunked body, or one
    /// longer than its `Content-Length` said).
    TooLarge,
    /// The client's stream failed (it went away mid-body).
    Failed,
}

/// The whole of `body`, at most [`MAX_HTTP_BODY_BYTES`] of it. Trailers
/// are dropped: rmcp reads only the data.
async fn read_body(body: axum::body::Body) -> Result<axum::body::Bytes, BodyRead> {
    use axum::body::HttpBody;
    let mut body = std::pin::pin!(body);
    let mut collected = Vec::new();
    while let Some(frame) = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await {
        let frame = frame.map_err(|_| BodyRead::Failed)?;
        if let Ok(data) = frame.into_data() {
            let room = MAX_HTTP_BODY_BYTES.saturating_sub(collected.len() as u64);
            if data.len() as u64 > room {
                return Err(BodyRead::TooLarge);
            }
            collected.extend_from_slice(&data);
        }
    }
    Ok(axum::body::Bytes::from(collected))
}

/// The MCP-session id header of streamable HTTP.
pub(super) const MCP_SESSION_ID: &str = "mcp-session-id";

/// The MCP-session id `headers` name, read **exactly** as rmcp 3.1.2 reads
/// it (`StreamableHttpService::handle_post`, `handle_get`, `handle_delete`:
/// `headers.get(HEADER_SESSION_ID).and_then(|v| v.to_str().ok())`): the
/// first such header, and only when it is visible ASCII. A header that is
/// present but not readable that way names no session to rmcp, so it names
/// none here either.
///
/// The one reading of the header (#32 PR 5 review S1). The session cap
/// ([`opens_a_new_session`]) and the MCP-session binding
/// (`transport::serve_live`) both use it, so "this request opens a new
/// MCP session" means what rmcp will do with it, in both places. Reading
/// the header any other way (present at all, the last value, lossily) lets
/// a request the guard counts as "inside a session" mint one in rmcp,
/// uncounted and unattributed.
pub(super) fn usable_session_id(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers.get(MCP_SESSION_ID).and_then(|v| v.to_str().ok())
}

/// Is this the request that would mint a **new** MCP session?
///
/// Streamable HTTP assigns the session id in the `initialize` response, so the
/// one request that arrives without a usable `Mcp-Session-Id`
/// ([`usable_session_id`]) — and can create state — is that POST. Everything
/// else either names a session or is a GET/DELETE, and must not be counted
/// against the cap.
///
/// `Last-Event-ID` plays no part: rmcp reads it only on a GET (resuming a
/// stream), never on a POST, so a POST carrying it with no usable session id
/// mints a session like any other (#32 PR 5 review S1).
pub(super) fn opens_a_new_session(req: &axum::extract::Request) -> bool {
    req.method() == axum::http::Method::POST && usable_session_id(req.headers()).is_none()
}

/// Can rmcp mint an MCP session for a session-opening request
/// ([`opens_a_new_session`]) whose whole body is `body`? (#32 PR 5 third
/// review L2.)
///
/// rmcp 3.1.2 (`StreamableHttpService::handle_post`) mints a session for a
/// POST with no usable session id only when the body is a JSON-RPC
/// *request* whose method is `initialize`: a sessionless `server/discover`
/// or per-request-protocol call (`tools/call` carrying its protocol
/// version) is answered directly, without a session, and anything else
/// without an id is refused. So only an `initialize` needs a reservation
/// of the cap; a sessionless call, which can run for as long as its tool
/// does, must not hold one, or a client making parallel stateless calls
/// is refused at its share although it opens nothing.
///
/// The body is classified with **exactly rmcp's own reading of it**:
/// `serde_json::from_slice::<ClientJsonRpcMessage>`, the call rmcp's
/// `expect_json` makes, on the very bytes rmcp is then handed, so the two
/// cannot disagree about what the message is. The answer leans one way
/// only: a body that does not parse counts as one that can mint (rmcp
/// refuses it, so the reservation is brief), so a disagreement could only
/// over-count, the safe direction for the cap, never let a session in
/// uncounted.
pub(super) fn can_mint_a_session(body: &[u8]) -> bool {
    use rmcp::model::{ClientJsonRpcMessage, ClientRequest};
    match serde_json::from_slice::<ClientJsonRpcMessage>(body) {
        Ok(ClientJsonRpcMessage::Request(req)) => {
            matches!(req.request, ClientRequest::InitializeRequest(_))
        }
        Ok(_) => false,
        Err(_) => true,
    }
}

/// How the cap refusal tells a client to free an MCP session, for a request
/// on `path` (#32 review L4): an MCP session is closed on the route it was
/// opened at, `/mcp` for the default session or `/mcp/s/{session}`. At
/// `/mcp` the text is the one a single-session serve has always sent. The
/// path is echoed only when it is one of those routes, never as the caller
/// spelled anything else.
fn how_to_close(path: &str) -> String {
    const AT_MCP: &str = "HTTP DELETE /mcp with its Mcp-Session-Id";
    if path == "/mcp" {
        return AT_MCP.to_string();
    }
    if let Some(raw) = path.strip_prefix("/mcp/s/")
        && let Ok(id) = crate::surface::session::parse_addressed(raw)
    {
        return format!(
            "HTTP DELETE /mcp/s/{} with its Mcp-Session-Id; a session opened on another route \
             is closed on that route",
            id.as_str()
        );
    }
    "HTTP DELETE, with its Mcp-Session-Id, on the route it was opened at: /mcp or \
     /mcp/s/<session>"
        .to_string()
}

/// Auth, then rate, then the declared body size; then, for a request that
/// would open an MCP session ([`opens_a_new_session`]) and only for it, the
/// whole body within [`REQUEST_BODY_TIMEOUT`], and, when rmcp can mint a
/// session for that body ([`can_mint_a_session`]: an `initialize`), the
/// session cap (the process's, then the credential's share) — in that
/// order, deliberately. Every other request goes on to the router with its
/// body unread.
///
/// Authentication runs **first and alone**: an unauthenticated caller must not
/// be able to consume rate-limit budget or read the live-session count (a 503
/// vs 401 difference would leak how loaded the server is), and it must be
/// refused before rmcp sees the request at all — before any session is minted,
/// any worker task spawned, or any body parsed.
///
/// Since #32 PR 5 authentication resolves *which* credential called
/// ([`ServeAuthority::authenticate`], a constant-time scan over every
/// configured one) and attaches its grant to the request as
/// [`Authenticated`]; the router checks that grant's scope before it looks
/// a session up. Nothing here depends on the addressed session, so every
/// answer from this guard (401, 429, 413, 408, the cap's 503) is the same
/// for every session id.
pub(super) async fn guard_request(
    axum::extract::State(guard): axum::extract::State<HttpGuard>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    // Exactly one `Authorization` header, or none (#32 PR 5 review I2). A
    // request carrying two is refused like a wrong token: reading only the
    // first would let a proxy that appends its own header, or one that
    // keeps the last, disagree with this guard about who is calling.
    let mut authorizations = req
        .headers()
        .get_all(axum::http::header::AUTHORIZATION)
        .iter();
    let first = authorizations.next();
    let presented = if authorizations.next().is_some() {
        Some("")
    } else {
        first.and_then(|v| v.to_str().ok())
    };
    let Some(grant) = guard.authority.authenticate(presented) else {
        // Deliberately terse and identical for "no header" and "wrong
        // token": the difference is not the caller's business, and the
        // token itself is never echoed. Throttled (#32 PR 5 review L2).
        match guard.refusals.note_at(Instant::now()) {
            Some(held_back) => tracing::warn!(
                had_header = presented.is_some(),
                held_back,
                "mcp http: rejected an unauthenticated request (held_back: refusals not logged \
                 at WARN since the last one)"
            ),
            None => tracing::debug!(
                had_header = presented.is_some(),
                "mcp http: rejected an unauthenticated request"
            ),
        }
        return (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized: this endpoint requires 'Authorization: Bearer <token>'\n",
        )
            .into_response();
    };
    let credential = Arc::clone(&grant);
    let mut req = req;
    // A configured credential's name, for the call ledger (#32 PR 5 review
    // I6); the legacy `default` and the implicit `local` leave no mark.
    if credential.name() != crate::surface::session::LEGACY_CREDENTIAL_NAME
        && credential.name() != crate::surface::session::LOCAL_CREDENTIAL_NAME
    {
        req.extensions_mut()
            .insert(crate::mcp::server::CallCredential(Arc::from(
                credential.name(),
            )));
    }
    req.extensions_mut().insert(Authenticated(grant));

    if let Some(rate) = &guard.rate
        && !rate.try_acquire(credential.name())
    {
        tracing::warn!("mcp http: request refused by the rate limit");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(axum::http::header::RETRY_AFTER, "1")],
            "rate limit exceeded: slow down and retry\n",
        )
            .into_response();
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

    // Only a request that would open an MCP session goes on to the cap,
    // and only it is buffered here (#32 PR 5 third review L1). Every other
    // request streams to rmcp as it always did: its body is read by rmcp,
    // after routing, so a request the router refuses (an unknown path, a
    // session out of scope or not hosted) is answered without a byte of
    // its body being read or held.
    if !opens_a_new_session(&req) {
        return next.run(req).await;
    }

    // The whole body of an opener, read here within `body_timeout` (#32 PR
    // 5 second review L2) and before the cap reserves anything: a client
    // that dribbles its body holds no slot of the session cap while it
    // does, and holds its connection for at most the timeout. rmcp reads
    // the whole body before it acts anyway (and to the same 4 MiB), so
    // buffering it here costs nothing rmcp would not spend.
    let (parts, body) = req.into_parts();
    let body = match tokio::time::timeout(guard.body_timeout, read_body(body)).await {
        Ok(Ok(body)) => body,
        Ok(Err(BodyRead::TooLarge)) => {
            tracing::warn!(
                max = MAX_HTTP_BODY_BYTES,
                "mcp http: refusing an oversized request body"
            );
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("request body too large (limit {MAX_HTTP_BODY_BYTES} bytes)\n"),
            )
                .into_response();
        }
        Ok(Err(BodyRead::Failed)) => {
            return (
                StatusCode::BAD_REQUEST,
                "the request body could not be read\n",
            )
                .into_response();
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = guard.body_timeout.as_secs_f64(),
                "mcp http: refusing a request whose body did not arrive in time"
            );
            return (
                StatusCode::REQUEST_TIMEOUT,
                [(axum::http::header::CONNECTION, "close")],
                format!(
                    "the request body did not arrive within {} s\n",
                    guard.body_timeout.as_secs()
                ),
            )
                .into_response();
        }
    };
    // A sessionless call rmcp answers without a session (a per-request
    // `tools/call`, `server/discover`) holds no reservation for its
    // duration (#32 PR 5 third review L2): only a body rmcp can mint a
    // session for goes on to the cap.
    if !can_mint_a_session(&body) {
        let req = axum::extract::Request::from_parts(parts, axum::body::Body::from(body));
        return next.run(req).await;
    }
    let mut req = axum::extract::Request::from_parts(parts, axum::body::Body::from(body));

    // Reserve first, then read the live count (see `Openings`).
    let (opening, opening_total, opening_mine) = guard.openings.reserve(credential.name());
    let live = guard.live.live().await + opening_total;
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
                 will not open another. Close an idle session ({close}), or restart with a \
                 higher --max-sessions.\n",
                max = guard.max_sessions,
                close = how_to_close(req.uri().path()),
            ),
        )
            .into_response();
    }
    // This credential's share (#32 PR 5 review M2). Counted only when
    // it is smaller than the cap: with one credential it is the cap,
    // and the check above already decided.
    if guard.credential_sessions < guard.max_sessions {
        let mine = guard.live.live_opened_by(credential.name()).await + opening_mine;
        if mine >= guard.credential_sessions {
            tracing::warn!(
                credential = credential.name(),
                live = mine,
                share = guard.credential_sessions,
                max = guard.max_sessions,
                "mcp http: refusing a new session — the credential is at its share of the \
                 concurrent-session cap"
            );
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::RETRY_AFTER, "5")],
                format!(
                    "this credential is at its share of the concurrent-session cap ({mine}/\
                     {share} of {max} sessions): this server will not open another for it. \
                     Close an idle session ({close}), or restart with a higher \
                     --max-sessions.\n",
                    share = guard.credential_sessions,
                    max = guard.max_sessions,
                    close = how_to_close(req.uri().path()),
                ),
            )
                .into_response();
        }
    }
    req.extensions_mut().insert(OpeningReservation {
        _opening: Arc::new(opening),
    });

    next.run(req).await
}

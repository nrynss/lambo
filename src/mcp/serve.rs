//! `lambo serve` — process lifecycle for the MCP server.
//!
//! One process owns one session (spec §2.2). This module builds **one**
//! [`Memory`] from **one** [`ResolvedBackends`], serves it over stdio or
//! streamable HTTP, and guarantees [`Memory::close`] runs on the way out so the
//! final flush happens.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
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
use crate::resolve::ResolvedBackends;
use crate::store::lease;
use crate::types::LamboError;

mod builder;
mod heartbeat;
mod http_guards;
mod hub;
mod roles;

pub use builder::{build_memory, resolve_serve_backends};
pub use heartbeat::authorize_ledger;

use builder::{explain_startup_failure, serve_builder};
use heartbeat::{heartbeat_loop, log_events, record_refused_takeovers, serve_startup_line};
use http_guards::{authorize_bind, guard_request, HttpGuard, RateLimiter};
pub use http_guards::{
    resolve_auth_token, SecretToken, AUTH_TOKEN_ENV, DEFAULT_MAX_SESSIONS, DEFAULT_RATE_LIMIT_RPS,
};
use hub::serve_endpoint;
use roles::{resolve_role, Role};

#[allow(unused_imports)] // rustdoc links only
use roles::ELECTION_BUDGET;

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
            // is spawn a heartbeat, bind a socket or build a `LamboServer`. It
            // is a pipe.
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

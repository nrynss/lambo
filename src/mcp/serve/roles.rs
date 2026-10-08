//! Holder or proxy: the startup election (J2, J2-L2) that decides what this
//! process is, and the loser-side record of a refused acquisition (J4).
//!
//! [`resolve_role`] is the only place on the serve path that attempts the
//! lease acquire. Its interface with the proxy is [`Role::Proxy`], built
//! from the holder's published endpoint once `probe_holder` (in
//! [`hub`](super::hub)) reports it reachable.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::hub::probe_holder;
use super::hub::{HubProxy, SessionEndpoint};
use super::{explain_startup_failure, ServeOptions, Transport};
use crate::ledger::Ledger;
use crate::memory::Memory;
use crate::store::lease;
use crate::types::LamboError;

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
pub(super) const ELECTION_SLACK: Duration = Duration::from_secs(5);

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
pub(super) const ELECTION_BUDGET: Duration = Duration::from_secs(20);

/// How often the startup election retries while no holder is reachable.
pub(super) const ELECTION_RETRY: Duration = Duration::from_secs(1);

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
pub(super) fn waiting_fits(lapses_in: Duration, left: Duration) -> bool {
    lapses_in + ELECTION_SLACK <= left
}

/// What this process turned out to be.
pub(super) enum Role {
    /// It won the lease: a real writer, with a graph, a tail and a socket to
    /// bind. Boxed for the same reason [`crate::memory::Attach`] boxes it.
    Holder(Box<Memory>),
    /// The lease is held by a reachable local holder: forward to it (J2).
    Proxy(Box<HubProxy>),
}

/// The one probe outcome that is strong evidence the holder is **gone** rather
/// than merely unreachable (J2-R2-3).
///
/// A named constant, not a literal at each site, so
/// [`correct_the_refresh_claim`] cannot drift out of step with the message it
/// looks for — the class of bug this whole round is about.
pub(super) const ENDPOINT_NOT_ACCEPTING: &str =
    "the holder's endpoint is not accepting connections";

/// What replaces `crate::memory::STILL_REFRESHING_CLAUSE` when the probe says the endpoint is not answering.
pub(super) const PROBABLY_DEAD: &str = "has not yet let its lease lapse — but its endpoint is not \
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
pub(super) fn correct_the_refresh_claim(message: &str, outcome: &str) -> String {
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
/// [`HubProxy::run`] is entered the client has handshaken
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
pub(super) async fn resolve_role(
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
                return Ok(Role::Proxy(Box::new(HubProxy::new(
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
        // lapsing.
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

/// J4 — the loser side of a refused acquisition: append the serve's own
/// `lease:refused` line to its ledger AND persist the fact to the store so the
/// incumbent holder learns it turned away a takeover ("from both sides").
/// Best-effort: neither failure is allowed to change the refusal decision.
pub(super) async fn record_refused_loser(
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

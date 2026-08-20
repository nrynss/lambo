# Adversarial review — mooshik J2, round 3 (verification)

**Reviewer**: independent adversarial reviewer (Opus 5), agent_id `j2-reviewer-r3`. Wrote
nothing under review.
**Scope**: the two remediation commits `920e096..6442d95` on `wt/j2` — `72050b7`
(J2-R2-1 the P2, plus the proxy-local P3s J2-R2-4/6/7) and `6442d95` (J2-R2-2 the P2, plus
J2-R2-3/5 and the §J2 record). Against the seven-finding checklist of
`adve-review-mooshik-J2-round2.md` (2 P2 / 5 P3) and the operator's round-3 scope ruling.
**Worktree**: `/Users/narayan/Documents/work/lambo/.claude/worktrees/j2`, branch `wt/j2`.
No commit amended; tree left clean but for this file.
**Verdict**: **CLEAN** — all seven round-2 findings verified closed at the artifact, every
gate exact, all seven declared mutations reproduced, both worst-case tables re-derived from
source, and the J2-R2-1 fix demonstrated **at the binary** (a SIGTERM arriving inside a live
dial window is honoured in 4 ms). Five **P3** advisories, all doc-precision, none blocking.

## Method

1. `lambo_recall` as `j2-reviewer-r3` on "J2 round 2 remediation DIAL_BUDGET operator ruling" —
   12 hits carrying the operator ruling verbatim, the remediation's own derived record, and
   the `DIAL_BUDGET`-is-bounded-from-both-sides constraint. Graph read as context;
   adjudication is against the round-2 checklist → the operator ruling → §J2 → source, in
   that order.
2. Read the round-2 review's seven findings in full, then both remediation commit messages,
   then every line of code and prose each one claims to change (`git diff 920e096..6442d95`
   over all six touched files).
3. Independent re-derivation of the two load-bearing arithmetic claims — sqlx's default
   `acquire_timeout` at the pinned version, and both adapters' pool options — **at source**,
   not from the commit message.
4. Seven mutations, applied and reverted from byte copies taken before each edit.
5. Nine gates re-run from scratch at `6442d95`, plus a two-gate spot-check at the
   intermediate commit `72050b7`. Every count re-derived; every delta reconciled to a named
   test from an independently-extracted test-name set diff.
6. My own register sweep over the touched files and one file over, plus a repo-wide sweep of
   both P2 claim families.
7. **Part B, the live spot-check**: the remediated binary built in its own target dir and
   driven over stdio JSON-RPC — two `serve` processes, a scratch sqlite store, no client
   products — through three scenarios including the one the brief called cheap-if-constructible.

**Order of authority.** The round-2 checklist is the contract; the **operator ruling** is what
overrides my predecessor on J2-R2-1 and is the authority on shape; §J2 is the claim; the
source is what ships. Where a commit message and the code disagree the code wins and the
disagreement is a finding — two of this round's five advisories are exactly that.

## Part A — the seven at the artifact

### The two P2

| # | Finding | Verdict | Evidence |
|---|---|---|---|
| **J2-R2-1** | the 2 × `CONNECT_BUDGET` SIGTERM-deafness bound omits the store read in the arm body | **CLOSED, as a code fix, per the operator ruling** | `dial_bounded` (`src/mcp/proxy.rs:974-1000`) races the whole dial — `reconnect_and_replay`, which is `dial` (row read + `proxyable` + connect) then `Handshake::replay` — against the shutdown future, `tokio::select!` with `biased;` and the shutdown arm **first**, wrapped in `tokio::time::timeout(DIAL_BUDGET, …)`. `DIAL_BUDGET = 6s` (`:212`) bounded from **both** sides: the `const _: () = assert!` at `:218-224` pins `DIAL_BUDGET > 2·CONNECT_BUDGET + CONNECT_RETRY` (≈4.1s) at build time, and `a_hung_lease_read_is_cut_off_at_the_chosen_dial_budget` asserts `DIAL_BUDGET < 8s` (sqlite's `busy_timeout`) at run time. Three outcomes as an enum (`Dialled`, `:798-816`) so a shutdown-cut dial is not reported as a failure. First dial moved inside the raced region (`tokio::pin!` at `:1125`, dial at `:1131`). All four declared mutations reproduced — see the table below |
| **J2-R2-2** | "in the majority of real cases … the wait still succeeds" is false, and so is "the client's next start succeeds" | **CLOSED, doc + test, behaviour unchanged** | `waiting_fits` extracted (`src/mcp/serve.rs:907-909`) and **behaviour-identical**: the old inline `if lapses_in + ELECTION_SLACK > left` became `if !waiting_fits(lapses_in, left)`, and `!(a <= b) ≡ a > b`. Exactly one production call site (`:1084`); the second refusal site at `:1108` never carried the predicate. `ELECTION_BUDGET`'s docstring (`:853-889`, the const at `:890`) now carries the derivation, the `[30s, 45s]` range, "refused — always", the ~17–32s window that does wait, and the live 2.12s measurement. §J2's residual corrected at `J-multi-client.md:1040-1052`. Pinned by `an_abrupt_holder_death_outlasts_the_election_budget`, which asserts both ends of the range, the exact boundary of the waitable window, and the `least > widest` inversion guard |

**The re-derived worst-case table — I checked it myself, and it is right.** The remediation
claims sqlx's default `acquire_timeout` is 30s at the pinned version and applies in both
adapters. Confirmed at source, not from the docs: `sqlx-core-0.8.6/src/pool/options.rs:160`
sets `acquire_timeout: Duration::from_secs(30)` in `PoolOptions::new()`, and `Default`
delegates to `new()` (`:154-157`). Neither adapter overrides it — sqlite builds
`SqlitePoolOptions::new()…max_connections(1).connect_lazy_with(…)`
(`src/store/sqlite.rs:444-448`), cockroach builds
`PgPoolOptions::new().max_connections(MAX_POOL_CONNECTIONS).connect_lazy_with(…)`
(`src/store/cockroach.rs:1183-1185`); `rg acquire_timeout src/` returns nothing. The
per-statement terms are as named: `busy_timeout(Duration::from_secs(8))` at `sqlite.rs:397`,
`STATEMENT_TIMEOUT: Duration = Duration::from_secs(20)` at `cockroach.rs:641`. And neither
`read_lease` is wrapped in a retry — both are a single `fetch_optional`
(`sqlite.rs:725-732`, `cockroach.rs:1825-1833`), so `tx_retry`'s 5 attempts cannot multiply
the figure. **≈38s sqlite and ≈50s cockroach are correct**, and both are worse than my
predecessor's 12s/24s estimate — a self-correction against the remediation's own interest,
which is the right direction for a number to move under audit.

**The "first dial previously ran un-raced above the pin" claim: verified.** At `920e096`,
`run` awaited `reconnect_and_replay` at `proxy.rs:907` and `tokio::pin!(shutdown)` came
later, immediately above the loop. So the whole store-emergent timeout was spent before the
pump could observe a signal. One nuance in the remediation's favour that it does not claim:
`shutdown_signal()` registers both `SignalKind` handlers **eagerly**, at construction
(`serve.rs:1861-1865`), before `run` is entered — so pre-fix the signal was *queued*, not
lost. The process was deaf, not deafened permanently. The fix is still the fix; the blast
radius was latency, not a dropped signal.

### The five P3

| # | Verdict | Evidence |
|---|---|---|
| **J2-R2-3** | **CLOSED, and the narrowness is pinned in both directions** | `correct_the_refresh_claim` (`serve.rs:975-981`) rewrites the clause **only** when the outcome contains `ENDPOINT_NOT_ACCEPTING`. Both literals are shared consts — `memory::STILL_REFRESHING_CLAUSE` (`src/memory.rs:402`) and `serve::ENDPOINT_NOT_ACCEPTING` (`serve.rs:954`) — and `rg` confirms **no duplicate literal survives in production code**: every other occurrence is a test assertion or a deliberately-unchanged CLI transcript in `docs/`. So the drift the sharing is meant to prevent is prevented at compile time, not by discipline. Binary-level pin `a_holder_whose_endpoint_refuses_is_not_described_as_still_refreshing` (a published endpoint that was never bound), narrow half pinned in `a_holder_whose_lease_outlasts_the_client_budget_is_refused_at_once` (a live CLI holder that **must** keep saying it). Mutation 5: exactly two red |
| **J2-R2-4** | **CLOSED in substance; the claim about it overstates — J2-R3-5** | `dial` returns `(stream, address)` (`:1040-1045`), `Dialled::Hub` carries it, and the headline line logs `dialled=` **and** `derived=` (`:1155-1156`). Confirmed at the binary in two live runs: `dialled=/tmp/lambo-501/j2-r3-live-abc1a52120975613.sock derived=/tmp/…`. But the **reconnect** line (`:1289-1293`) logs `dialled=` only, while both the commit message and §J2 say both lines carry both fields |
| **J2-R2-5** | **CLOSED as narrowed, and the reason not to close is sound** | Claim narrowed at `src/mcp/endpoint.rs:548-569` with the mechanism, the consequence and the safety argument. **I re-measured the mechanism** via `ctypes` on `realpath(3)` (Python's `os.path.realpath` does not reproduce it): a dangling link gives `ERRNO 2 (No such file or directory)`; once the target exists the same call returns the **target**; the not-exists branch returns `<parent>/link.db`. Exactly two identities on either side of the file's creation, as stated. Not closing is right: a hand-rolled link-chain resolver beside `canonicalize` is a second path resolver for a configuration no documented wiring produces, and the degradation is a refusal under a lease that still serialises the writers — nothing is corrupted |
| **J2-R2-6** | **CLOSED** | `if !published.is_absolute()` → `EndpointIsNotOurs` (`proxy.rs:465-482`) **before** the name comparison, with the reasoning at the call site and the case named in `NotProxyable::EndpointIsNotOurs`'s docs (`:377-382`). Two spellings added to `a_holder_publishing_a_different_address_name_is_not_proxyable`. Mutation 7 red. Attacked further: `/foo/../..` passes `is_absolute` but `assert_private_dir` stats the kernel-resolved parent and refuses it on ownership or mode, so the check is not a lone gate |
| **J2-R2-7** | **CLOSED as a declined cap, and the declining argument is TRUE at the code** | I read the pump to adjudicate the mechanism rather than accept it. `proxy.rs:1371-1393`: a response frame whose id matches nothing in `inflight` falls through the `answers` branch, and when `gen == generation` it is **forwarded unconditionally** at `:1393`. So a cap that pre-emptively errored an id would let the holder's real answer reach the client as a **second response to that id**. The mechanism is real and the deviation is correct. `INFLIGHT_DEPTH_WARN = 64` (`:250`) is adequate insurance: latched by `inflight_warned` so it cannot flood, two orders above the claimed ceiling, and the O(n) scan is gated behind `response_id(&frame).and_then(…)` so notifications never pay it. The J3 re-derive note is in the right place (at the declaration and in §J2) |

### The self-caught bug

`PROBABLY_DEAD` (`serve.rs:957-958`, the const at `:957`) had lost its line-continuation backslash — written
through a Python heredoc, where `\` before a newline is Python's own continuation and is
consumed before Rust sees it. **Fix verified**: the literal now reads `… is not \` +
newline + indentation, which Rust collapses to a single space, and
`a_dead_holders_refusal_does_not_claim_it_is_still_refreshing` asserts both J2-R1-9 rules on
it (`!contains("  ")` and the phrase spanning the continuation surviving).

Their claim that the remaining double-space literals are intentional fixtures: **verified.**
A repo-wide sweep for double spaces inside string literals in `src/` returns 24 hits, none
operator-facing. Two spot-checked as the brief asks: `src/store/mod.rs:769`
— `"  postgres  ".parse::<StoreKind>()`, a whitespace-trimming assertion in a `StoreKind`
parse test; and `src/cli/saints.rs:29` — `"  {} [{:?}, canonical]  blast_radius={}  …"`, a
column-alignment format string for terminal output. Both deliberate. I also scanned every
multi-line string literal in `src/mcp/*.rs` and `src/memory.rs` for a newline not preceded by
`\`: no true positive.

## New findings

Five, all **P3**, all doc-precision. None requires a behaviour change, a new test, or a gate.

### J2-R3-1 (P3) — the new shared const inherits a stray summary line

`src/memory.rs:390-402`. `STILL_REFRESHING_CLAUSE` was inserted directly beneath the
pre-existing stray doc line `/// Builder for [`Memory`] — spec §6.1.`, which at `920e096`
was (wrongly) attached to `LeaseHeldElsewhere`. There is no item between them, so rustdoc
now renders the **new const's summary** — the line shown in the item index — as "Builder for
Memory — spec §6.1.". The side effect is that `LeaseHeldElsewhere`'s summary was accidentally
repaired, so the stray line did not disappear; it moved onto the item this round created.

This is small, but it is the same class the round is about: a doc line that says something
untrue about the thing it is attached to, created by an edit that was fixing exactly that
class elsewhere.

**Remediation.** Delete the stray `/// Builder for [`Memory`] — spec §6.1.` line — the
`// ----- Builder -----` banner two lines above already does its job, and it documents
neither item correctly. One line.

### J2-R3-2 (P3) — a published mutation result names an assertion that cannot fire for it

§J2's round-3 mutation table (`J-multi-client.md:1164`) and `72050b7`'s commit message both
record:

> `DIAL_BUDGET` → 3600s (i.e. "let the store decide") | **red** — "waited 3600s"

Re-run: it **is** red, and only that one test, but it fails on the **first** assertion in
`a_hung_lease_read_is_cut_off_at_the_chosen_dial_budget` —

```
panicked at src/mcp/proxy.rs:2189:9:
DIAL_BUDGET must stay below sqlite's 8s busy_timeout, or the number an operator reads at
the constant is not the number that decides
```

— and never reaches the elapsed-time assertion whose message is `"the dial must end at the
budget, not at the store: waited {waited:?}"`. The quoted evidence belongs to that later
assertion, and under `start_paused = true` that assertion would have **passed** at 3600
(the runtime auto-advances, so `waited == DIAL_BUDGET` exactly). So "waited 3600s" is not
merely the wrong line — it is an observation that mutation cannot produce.

The substance is unaffected and arguably better than recorded: the ceiling assertion is the
*right* thing to catch a 3600s budget, because it catches it as a violated design invariant
rather than as an elapsed-time surprise. But the record is the artifact a later reviewer
reproduces from, and it does not reproduce.

**Remediation.** Correct the two cells to name the assertion that fires — e.g. *red on
`DIAL_BUDGET < 8s`, the sqlite `busy_timeout` ceiling* — and, if a mutation that exercises
the elapsed assertion is wanted, note that it is the *opposite* direction (dropping the
`timeout` wrapper, which the docstring at `:2143-2147` already describes as the honest hang).

### J2-R3-3 (P3) — "the pump's two frame writes" undercounts the new residual, and hides its coupling to J2-R2-7

Stated in both places the remediation claims (`DIAL_BUDGET`'s last section,
`proxy.rs:200-210`, and §J2's residual list, `J-multi-client.md:775-783`) as:

> The pump's **two** frame writes are still unbounded arm-body awaits

There are **six** un-raced `Self::send` sites in the pump: `proxy.rs:1298` (the preamble
forward, itself a loop over up to `MAX_REPLAY_FRAMES` = 64 frames), `:1321` (forward to the
holder), `:1349` (the `HUB_UNREACHABLE` reply), `:1375` and `:1393` (both to the client's
stdout, in the `hub_rx` arm), and `:1457` inside `answer_lost`. Read charitably, "two" means
the two *directions* in the `client_rx` arm body, which is what that section is about — but
§J2's phrasing is about "the pump", and the `hub_rx` arm's writes have the same property.

The omission that matters is `:1457`. `answer_lost` writes **one frame per in-flight id**,
and the size of that burst is bounded by nothing except the `inflight` list — the very list
J2-R2-7 deliberately declined to cap in the same round. So the two residuals are coupled:
the honest-answer path J2-R1-1 exists to guarantee is the one un-raced write whose length is
governed by the one collection with no ceiling. Both residuals are handed to J3, and neither
text says the other exists.

**Remediation.** Say "the pump's frame writes" rather than "two", name `answer_lost` among
them, and add one sentence in each place: the burst length is the `inflight` depth, so
whatever J3 decides about the ceiling (J2-R2-7) also decides this residual's worst case.

### J2-R3-4 (P3) — a remaining sibling in the `ELECTION_SLACK` claim family, one file over

The remediation's sweep found one false stated reason the round-2 review missed
(`ELECTION_SLACK`'s "absorbs … one missed refresh interval"). The family has one more member,
in the file that owns the constants the corrected sentence now reasons about —
`src/store/lease.rs:94-100`:

> How often a live holder refreshes its lease — one third of [`LEASE_TTL`].
>
> A third means a holder survives two consecutive missed refreshes (a transient store blip)
> **before its lease can lapse** …

The arithmetic does not give that. A refresh at `t` sets `expires_at = t + 45`; refreshes are
attempted at `t+15`, `t+30`, `t+45`. Missing the first two puts the third attempt **at** the
expiry instant, not before it, so the lease **does** lapse. What actually saves the holder is
`acquire_or_refresh`'s guard — `WHERE session_leases.expires_at <= now OR
session_leases.holder = excluded.holder` (`src/store/sqlite.rs:1466-1467`) — which lets it
re-acquire its own lapsed row with its fencing token preserved, *provided no contender took
the row in that instant*. So the holder survives by re-acquisition under no contention, not
by having lease left.

The guard beside it pins the weaker relation only: `assert!(LEASE_HEARTBEAT_INTERVAL * 2 <
LEASE_TTL)` (`lease.rs:291`) is `30 < 45`, which pins "survives **one** missed refresh with
room". Nothing pins the stated claim, and the claim as stated is the kind of sentence
`ELECTION_SLACK`'s own correction now contradicts one file away — that rewrite says a holder
"is *supposed* to lose the lease if it misses all three".

**Remediation.** One sentence: a third gives three attempts inside one TTL, the third landing
at the expiry instant, so two consecutive misses are survived by re-acquiring an
uncontested lapsed row rather than by lease remaining — and a contender arriving in that
instant wins. Optionally strengthen the neighbouring assertion to
`LEASE_HEARTBEAT_INTERVAL * 3 <= LEASE_TTL`, which is the relation the sentence is about.

### J2-R3-5 (P3) — the reconnect log line carries only one of the two fields claimed for it

`72050b7`'s commit message and §J2 (`J-multi-client.md:1216-1219`) both say:

> both that line and the reconnect line now log `dialled=` and `derived=`

The headline line does (`proxy.rs:1152-1158`). The reconnect line does not — `:1288-1293`
logs `generation` and `dialled` only. The substance of J2-R2-4 is fixed there (the line names
what was dialled, which is the finding), so this is a claim defect rather than a code one; but
the whole point of adding `derived=` was to make the J2-L1 divergence visible **without**
reading the earlier directory-differs line, and a reconnect is precisely when a proxy may
land on a *different* holder's directory than the one the first dial used.

**Remediation.** Add `derived = %self.endpoint.path().display(),` to the reconnect line (one
line, and it makes the published claim true), or correct the sentence to say the reconnect
line logs `dialled=`.

## Attacks that did not land

Recorded so they are not re-run. Rounds 1 and 2 kept their own lists; I did not repeat them.

* **Can an abandoned dial leak a half-initialized connection the holder keeps around —
  a holder-side resource leak, repeated every retry?** *No, and the mechanism is clean.*
  `LamboServer` is `#[derive(Clone)]` over `Arc<Memory>`, a `ToolRouter`, an
  `Option<Arc<Ledger>>` and an `Instant` (`src/mcp/server.rs:258-271`) — there is **no
  per-connection registry** for an abandoned connection to be stranded in. `serve_endpoint`
  (`serve.rs:1636-1679`) spawns one task per accepted connection holding an *owned* semaphore
  permit; a torn `initialize` followed by an EOF either fails `server.serve(stream)` or ends
  `service.waiting()`, the task returns, and the permit drops with it. I saw the exact
  outcome in mutation 4's captured holder stderr: `WARN … endpoint handshake failed
  error=connection closed: initialize request`, twice, with no accumulation. The only
  holder-side residue is that WARN line, which attributes to the holder a failure the proxy's
  own budget or signal caused — worth knowing when reading logs, but not a leak and not new
  (any closed connection produces it). And the repetition premise is weak in the first place:
  reaching the `DIAL_BUDGET` cap *inside* the replay requires the row read to consume 2–4s
  while connect + replay consume the rest, because otherwise the inner budgets (≈4.1s total)
  fire first and return `Dialled::Failed` with the socket dropped the same way.
* **Does the biased shutdown arm starve dial retries under a flapping shutdown future?**
  *No.* `biased` fixes only the *order* of polling within one `select!`, not the opportunity:
  a shutdown future that returns `Pending` is polled, yields, and the timeout-wrapped dial is
  polled immediately after, on every wake. There is no "flapping" state to exploit either —
  a future cannot un-complete, and once `Dialled::ShutdownRequested` is returned both call
  sites leave the pump (`run` returns `Ok(())` at `:1136`, the loop `break`s at `:1315`), so
  the completed future is never re-polled. Spurious wakes would cost polls, not progress.
* **Can the race discard a dial that already succeeded?** *No.* In a two-arm `select!` the
  shutdown arm can only win by being `Ready` when polled, which happens *before* the dial
  future is polled in that iteration — so the dial has not returned. The connection being
  built is dropped, which is the abandoned-dial case above, and no `Dialled::Hub` is ever
  constructed and thrown away.
* **Did extracting `waiting_fits` change behaviour?** *No.* `!(lapses_in + ELECTION_SLACK <=
  left)` is `lapses_in + ELECTION_SLACK > left`, the exact prior expression, and there is
  exactly one production caller. The second refusal site never had the predicate. `Duration`
  addition overflow is not reachable: `lapses_in` comes from a `chrono::TimeDelta` whose
  range caps around 9.2e15 seconds against `Duration::MAX`'s 1.8e19.
* **Can `STILL_REFRESHING_CLAUSE` drift out of step with the correction?** *Not silently.*
  Both ends reference the same `const`, so a reword changes both in one edit; `rg` confirms
  no surviving duplicate of the literal in production code. The two `docs/` occurrences
  (`cli.mdx`, `end-to-end.mdx`) are `lambo derive` transcripts with no endpoint probe in
  scope, where the clause is true — deliberately unchanged, and the narrowness test pins that
  those cases keep it.
* **Does `is_absolute()` leave a bypass?** *No useful one.* `/foo/../..` is absolute and
  passes, but `dial_dir` hands the kernel-resolved parent to `assert_private_dir`, which
  refuses on `symlink_metadata`, ownership or mode. The check removes an empty and a
  cwd-relative path from an operator-facing message; it was never the trust boundary and does
  not become one.
* **The J2-R2-2 claim family, swept repo-wide** for `uniformly`, `majority of real cases`,
  `next start succeeds`, `immediately after a heartbeat`, `four seconds`, `2 × CONNECT_BUDGET`
  and `in microseconds`. Every surviving occurrence of a corrected claim is either an
  explicitly past-tense quotation inside the correction itself (`serve.rs:856`,
  `proxy.rs:134`, `:2139`), a §J2 record entry, or a review file. The one hit outside that
  set — `web/app.js:759`, "Roughly four seconds in production" — is the web demo's
  loading-spinner cadence and unrelated. **No live restatement of either false claim
  survives.**

## Positive observations

* **The operator's ruling was taken as a re-decision, not as a doc chore, and the shape
  chosen is the right one.** The argument for declining the full hoist is genuinely
  independent of the number that was wrong — *every* `send` in this pump is an arm-body await,
  and that is what keeps frames from being torn by `select!` cancellation, so hoisting one
  await buys a state machine and leaves the class. Racing the one await that *can* be
  abandoned without consequence is the minimum change that closes the finding, and the reason
  it is safe (the connection belongs to nobody yet) is stated where the code is.
* **The worst-case table moved against the remediation's own interest, by reading source.**
  12s/24s became 38s/50s because someone opened `sqlx-core`'s `PoolOptions` instead of
  trusting the adapter's own tuning to be the whole story. That is the correct direction for
  a number to move under audit, and it makes the shipped decision look *more* necessary, not
  less.
* **The `const _: () = assert!` is the strongest single artifact in this round.** A build-time
  floor under a latency constant, whose failure text explains *why* the floor exists, is a
  register rule the compiler enforces. Combined with the run-time ceiling assertion inside the
  test, `DIAL_BUDGET` is now pinned from both sides — and the memory note recording that both
  sides are load-bearing means the next person to move a store timeout will find out.
* **The test seam is exactly right, and the tests are their own negative controls.** An
  injected `GraphStore` whose `read_lease` is `pending()` — the `BatchSink` precedent — plus
  `start_paused = true` makes three shutdown-race properties assertable at zero wall clock.
  And the docstring at `proxy.rs:2143-2147` states in advance that dropping the biased arm
  fails *cleanly* rather than hanging, which I confirmed: 2 red in **0.03s**. A mutation-proof
  test that also documents how its own mutation fails is unusual and worth copying.
* **J2-R2-7 was declined with an argument that survives reading the code.** I went to
  `:1371-1393` expecting the duplicate-response path to be hypothetical; it is not — an
  unmatched response id from the current generation is forwarded unconditionally. Declining a
  cap because the cap manufactures a protocol violation is the right call, and adding the
  observability the argument depends on rather than the cap the reviewer half-suggested is
  better than either.
* **The self-caught `PROBABLY_DEAD` bug is J2-R1-9's rule earning its keep for the second
  time**, and the diagnosis (a Python heredoc eating Rust's continuation backslash) is a
  tooling hazard worth carrying forward rather than a one-off typo.
* **The sweep found `ELECTION_SLACK`, which round 2 missed.** A remediation that audits its
  own register and reports a defect the reviewer did not find is the behaviour the recurring
  false-stated-reason family needs. My own sweep found one more sibling one file over
  (J2-R3-4) — which is a comment on how deep the family goes, not on the sweep's honesty.

## Part B — live spot-check at the binary

Built in its own target dir (`<scratchpad>/j2-r3-target`), `LAMBO_GIT_SHA=6442d95`,
`--release --no-default-features --features store-sqlite,embed-bge`, embedder live at
`127.0.0.1:8080` (`{"status":"ok"}`). Provenance verified by embedded strings rather than by
trust: `"the lease row read, the connect and the handshake replay"`, `"has not yet let its
lease lapse"`, `"most likely died"` and `"more unanswered forwarded"` are all **present**.
Two `serve` processes over stdio, JSON-RPC piped as the tests do, scratch sqlite store, no
client products.

| Scenario | Result |
|---|---|
| holder up, second `serve` becomes a proxy | `initialize` answered in **0.04s**; `tools/list` returns a real tool list through the pipe. Headline line at the binary: `dialled=/tmp/lambo-501/j2-r3-live-abc1a52120975613.sock derived=/tmp/lambo-501/…` — **J2-R2-4 confirmed live, both fields** |
| **SIGTERM the proxy mid-session** | `rc=0` in **0.002s** |
| **SIGTERM the proxy INSIDE an active dial window** — constructible after all: `kill -9` the holder, send a call (which enters the ≈2.1s connect-retry dial onto a refusing socket), SIGTERM 0.4s in | `rc=0` in **0.004s**, and the binary emits the new arm's own line: `INFO … shutdown signal while dialling the session holder — closing the proxy`. **This is the J2-R2-1 fix demonstrated end to end.** Pre-fix this signal waited out the dial; with a store wedged at its pool it waited out 38s or 50s. No paused-clock unit test can show this, and it is the single most load-bearing measurement in the round |
| holder `kill -9`'d **idle** (no call in flight), then one call | **`-32001`** in **2.049s**, message "…NOTHING WAS READ OR WRITTEN. … the previous holder's lease lapses within 45 seconds…". The **-32001 path is still honest at the binary**, and 2.049s is an independent confirmation of the round's re-derived `CONNECT_BUDGET + CONNECT_RETRY` ≈ 2.1s figure — the number that replaced "immediately" in `run`'s docstring |
| second proxy SIGTERM after the failed call | `rc=0` in **0.004s** |

Nothing skipped. The one thing the brief allowed me to skip — constructing the hung-dial
window — turned out to be cheap, so it was run, and it is the row that matters.

## Gate results

All re-run from scratch in the worktree at `6442d95`,
`CARGO_TARGET_DIR=/Users/narayan/Documents/work/lambo/target`.

| Gate | Claimed | Re-derived | |
|---|---|---|---|
| `cargo fmt --all -- --check` | clean | **clean** | ✓ |
| `cargo clippy --all-targets -- -D warnings` | clean | **clean** | ✓ |
| …`--features store-sqlite,fixtures` | clean | **clean** | ✓ |
| …`--features ship,fixtures` | clean | **clean** | ✓ |
| …`--no-default-features --features store-cockroach,embed-fixture` | clean | **clean** | ✓ |
| `cargo test --all --features fixtures` | 858/0/3 | **858/0/3** | ✓ exact |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | 946/0/3 | **946/0/3** | ✓ exact |
| `cargo test --no-default-features --features store-cockroach` | 550/0/0 | **550/0/0** | ✓ exact |
| `scripts/observability/verify.sh` | ALL CHECKS PASSED, 40 ok | **ALL CHECKS PASSED, 40 ok** | ✓ exact |

**Intermediate commit spot-checked**, as the brief asks, on the two cheapest gates at
`72050b7`: `cargo fmt --all -- --check` **clean**, `cargo test --all --features fixtures`
**856/0/3** — exactly 853 plus the three new proxy tests, so the claim that the intermediate
commit was green is corroborated on the gates I ran.

**Every delta reconciles to a named test.** Test-name sets extracted independently at both
revisions (regex over `#[test]` / `#[tokio::test(…)]` plus intervening attributes, across
`src/` and `tests/`): **965 → 971, six added, none removed, none renamed.**

| Added test | Where | Profile |
|---|---|---|
| `a_shutdown_during_the_dial_is_honoured_and_not_left_to_the_store` | `src/mcp/proxy.rs` | all three |
| `a_hung_lease_read_is_cut_off_at_the_chosen_dial_budget` | `src/mcp/proxy.rs` | all three |
| `a_shutdown_during_the_proxys_first_dial_exits_cleanly` | `src/mcp/proxy.rs` | all three |
| `an_abrupt_holder_death_outlasts_the_election_budget` | `src/mcp/serve.rs` | all three |
| `a_dead_holders_refusal_does_not_claim_it_is_still_refreshing` | `src/mcp/serve.rs` | all three |
| `a_holder_whose_endpoint_refuses_is_not_described_as_still_refreshing` | `tests/serve_proxy_multi_client.rs` | sqlite only |

* `src/` net = **+5** → fixtures 853 + 5 = **858** ✓; cockroach 545 + 5 = **550** ✓
* `src/` + `tests/` = **+6** → sqlite 940 + 6 = **946** ✓

J2-R2-6 shows no count change because its two cases were added *inside* an existing test —
which the record states, and which the set diff confirms.

### Mutations — all seven reproduced, run and reverted

| Mutation | Expected | Observed |
|---|---|---|
| 1. drop the `biased` shutdown arm from `dial_bounded` | 2 red, cleanly, fast | **2 red in 0.03s**, no hang: `a_shutdown_during_the_dial…` on the variant assertion, `a_shutdown_during_the_proxys_first_dial…` on the `Conflict` return. Exactly as the test's own docstring predicts |
| 2. `DIAL_BUDGET` → 3600s | red at "waited 3600s" | **1 red**, but on the `< 8s` ceiling assertion at `proxy.rs:2189`, **not** the message recorded — **J2-R3-2** |
| 3. `DIAL_BUDGET` → 4s | build fails | **build fails**, `error[E0080]: evaluation panicked:` carrying the assertion's own text verbatim |
| 4. `proxy.run(std::future::pending::<()>())` (the round-2 negative control) | still red for the right reason | **red** — "a SIGTERM to a proxy must be handled, not fatal", `tests/serve_pre_handshake_durability.rs:370`; `a_pre_handshake_sigterm_still_flushes_the_session_row` green. The new bounded dial does **not** turn this into a spurious pass: the holder is live, so the first dial succeeds and the pump loops with no shutdown to observe |
| 5. neuter `correct_the_refresh_claim` (`if false && …`) | exactly two red | **exactly two**, run with `--no-fail-fast` over the whole sqlite profile: `mcp::serve::tests::a_dead_holders_refusal…` and `a_holder_whose_endpoint_refuses_is_not_described_as_still_refreshing`. Nothing else — and the narrow-half assertion in `a_holder_whose_lease_outlasts_the_client_budget_is_refused_at_once` stayed green, which is the half that proves the correction is not blanket |
| 6. `ELECTION_BUDGET` → 50s | `an_abrupt_holder_death…` red | **red** — "a client starting promptly after an abrupt holder death must be refused, not waited out: 30s of lease against a 50s budget" |
| 7. remove `published.is_absolute()` | `proxyable` test red | **red** on the bare-name case: `a different address name must be refused: "s-6a39396e07e3462b.sock"` |

**Verification-only edits, all reverted from byte copies taken before each edit.**
`src/mcp/proxy.rs` and `src/mcp/serve.rs` restored and confirmed byte-identical by `shasum`;
`git status --short` empty but for this review file.

## Verdict

**CLEAN.** All seven round-2 findings closed. Five P3 advisories, no P2, no P1.

The operator was right to overturn my predecessor on J2-R2-1, and the remediation did the
harder thing well. The number was re-derived at source and came out *worse* than the review's
estimate; the shape chosen answers the two questions separately (deafness by racing, the
client's wait by a chosen constant) instead of conflating them; the constant is pinned from
below by the compiler and from above by a test; the first dial was moved inside the raced
region on the remediation's own initiative, closing a startup window nobody had asked about;
and the residual it creates is written down in both places it belongs. The sweep then found a
false stated reason the review had missed. That is the register discipline working rather than
being described.

And the fix is real at the binary, which is the part a paused-clock test cannot give: a
SIGTERM arriving inside an active dial window is honoured in **4 ms**, with the new arm's own
log line, on a process that pre-fix would have finished the dial first.

The five advisories are all the same species and none of them touches behaviour: a stray
rustdoc summary line the new const inherited (J2-R3-1); a mutation result whose recorded
evidence names an assertion that mutation cannot reach (J2-R3-2); a residual stated as "two
writes" when there are six, hiding the fact that the longest of them is bounded by the one
collection this round deliberately left uncapped (J2-R3-3); one more member of the very claim
family this round is about, one file over in `lease.rs` (J2-R3-4); and a log line that carries
one of the two fields its own record claims for it (J2-R3-5).

**Do they meet the carryover bar?** Mechanically, **all five do** — every one is
doc-precision with no behaviour change, no new test and no gate movement. But that is the
weaker question. The stronger one is cost, and the total cost here is **one deleted line, two
corrected sentences in §J2, one added sentence in `lease.rs`, and one added `derived=` field**
— call it fifteen minutes. On the J0/J1 precedent, cheap in-scope advisories get closed by the
orchestrator **at integration** rather than carried, and I recommend that for all five. Two
of them (J2-R3-2, J2-R3-5) are wrong *statements in the published record about this round's
own artifact*, and carrying those forward is precisely the mechanism J2-R1-7 documented: a
false sentence that survives one sweep is the thing that lets the next gap hide. Closing them
at integration costs less than writing them into a J-round backlog.

**Is J2 ready to integrate NOW?** **Yes.** No advisory blocks it and none needs a code
decision. Everything §J2's `Done when` box claims is now either verified at the binary in
this round or was verified at the binary in round 2 and re-verified green here, and the one
exclusion — a client starting promptly after an abrupt holder death is refused within that
start, not recovered — is stated in the box itself, in the operator's words, and pinned by a
test over the extracted predicate rather than asserted in prose. The two behaviour-bearing
constants an operator will meet (`DIAL_BUDGET` 6s, `ELECTION_BUDGET` 20s) are both chosen,
both explained where they live, and both bounded by assertions that fail if someone moves the
numbers they depend on.

## Residuals handed forward

* **J3** — inherits **two coupled residuals, and should treat them as one item** (J2-R3-3):
  the uncapped `inflight` list (J2-R2-7) and the un-raced client-facing writes. `answer_lost`
  writes one frame per in-flight id, so the ceiling J3 chooses for outstanding receipts is
  also the worst-case length of an un-raced write burst to a client that may not be reading.
  The declined cap's reason (a cap manufactures duplicate responses) is verified at
  `proxy.rs:1393` and J3 should not re-litigate it; what J3 must do is re-derive the ceiling
  now that a receipt outlives a call. The `-32001`/`-32002` split is measured again this round
  (2.049s, the idle-holder-death path) and receipts should inherit it rather than reinvent it.
* **J4** — round 2 asked for an "election refused because the lease outlasts the client
  budget" ledger state; add a second from this round: **"shutdown during a dial"** is now a
  distinct, deliberate exit (`Dialled::ShutdownRequested`, two log lines, measured at 4 ms)
  and is currently visible only on the proxy's stderr.
* **J5** — J2-R3-2, J2-R3-3 and J2-R3-5 all land in `J-multi-client.md`; if the operator
  carries rather than closes them, they should ride the same act as the DOGFOOD-SETUP re-pin.
  Nothing in this round touched `docs/` or `site/`, so J2-R1-19's mirror-gate test case is
  unchanged.
* **`lease.rs`** — J2-R3-4 is outside J2's touched set but inside the family J2 has now
  corrected three times. Whoever next opens `src/store/lease.rs` should fix the sentence and
  consider strengthening the neighbouring assertion to
  `LEASE_HEARTBEAT_INTERVAL * 3 <= LEASE_TTL`, which is the relation the docstring is
  actually about.
* **The store-timeout dependency, now load-bearing** — `DIAL_BUDGET`'s upper bound is
  sqlite's 8s `busy_timeout`. Moving that timeout *down* silently invalidates the constant,
  and the only thing that would catch it is
  `a_hung_lease_read_is_cut_off_at_the_chosen_dial_budget`'s in-test assertion. That is
  adequate, but the coupling deserves a line in whatever note governs store tuning, because
  the failure mode is a docstring quietly ceasing to be true rather than a red test in the
  file being edited.
* **Tooling hazard, for every future round** — writing Rust through a Python heredoc consumes
  `\`-continuations before Rust sees them. It cost a real operator-facing defect this round
  and was caught only because J2-R1-9's rule was applied to the new literal. Apply that rule
  to *every* new operator-facing const, and prefer a real editor for multi-line literals.
* **Beyond J** — in-process promotion still owns the last of J2-L2's residual, and it now
  inherits one more thing: the raced dial. A promoted proxy *binds*, and a bind abandoned
  mid-flight is not the consequence-free abandonment `dial_bounded` relies on.

---

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>

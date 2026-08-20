# J — Multi-client survivability: a second agent never silently loses memory

**Goal:** on a machine running more than one agent client, every client gets working
memory: full read, full write, and a usable lock. Not a refusal, not a degraded mode,
not silence.

**Why now (found by dogfooding, 2026-08-19):** two independent clients on one machine,
Claude Code and pi v0.84.1, each spawned their own `lambo serve` per the documented stdio
wiring. The lease admitted one; the others exited 1. In Claude Code's case no error reached
the agent at all.

The lease is not the problem and must stay. It is sized to serve's shutdown budget, not to
a write, because it protects an in-RAM write-behind log rather than a row. What is wrong is
that the default wiring gives every client its own serve, turning a correct process-level
lock into an agent-level outage. **Agents never clash; serve processes do.**

---

## Order

J4 was originally last. It is first: a proxy cannot forward a call without carrying the
caller's identity, and `lambo_reserve` is already broken without it. Renumbered.

| # | Task | Depends on |
| --- | --- | --- |
| J0 | Carryover from workstream I, round 3 (CLEAN) — **DONE `0c81419`** | nothing |
| J1 | Per-call agent identity — **DONE** | nothing |
| J2 | A losing serve proxies instead of exiting | J1 |
| J3 | Writes acknowledged before the embedder | J1 (receipt scoping) |
| J4 | Lease conflicts leave an artifact | I1 |
| J5 | Transport defaults and config layering | nothing |

J2 and J3 were previously one item. Splitting them ships the outage fix without waiting on
the write-path change.

---

## J0 — Carryover from workstream I (round 3, CLEAN — decision 2026-08-20)

**Status: done, `0c81419`.** Reviewed CLEAN with six P3 advisories at `77f119f`
([adve-review-mooshik-J0-round1.md](../adversarial-review/adve-review-mooshik-J0-round1.md)),
all six remediated in the round-1 remediation commit that follows it rather than carried
again — the three numbered items below stay as the spec they were, not as open work. The
closure narrative lives in [I-observability.md](I-observability.md)'s Handoff Log, which is
the right home for I's remediation history; this is the pointer from J's board.

That remediation was itself reviewed CLEAN — one P2, five P3
([adve-review-mooshik-J0-round2.md](../adversarial-review/adve-review-mooshik-J0-round2.md))
— and all six are closed in the round-2 remediation commit. The P2 was a gate hole, not a
prose defect: the new `verify.sh` queue-depth fixture wrote its heartbeats in stamp order,
so deleting `queued_lines()`'s `ts` sort outright still passed. Still doc-precision plus
that one fixture reorder; still no behaviour change.

Remediating the six rather than carrying them is a deliberate reversal of the decision that
created J0. The reasoning does not transfer twice: carrying I's advisories was defensible
because J1/J2/J4 rebuild the surface they sit on, so a fourth round would have re-touched
prose about to be rewritten anyway. These six are two-word register fixes, one `jq` field,
one grammatical mood, and one `verify.sh` fixture — cheap, entirely inside J0's own scope,
and none of them on ground J1–J5 rebuild. Carrying advisories twice in a row would also
stop being an exception and start being how this workstream handles P3s.

Workstream I closed CLEAN with three P3 advisories
([adve-review-mooshik-I-round3.md](../adversarial-review/adve-review-mooshik-I-round3.md)).
By decision, they ride here instead of a fourth remediation round: I's residual surface is
serve startup and ledger accounting, which J1/J2/J4 rebuild anyway. All three are
doc-precision; none touches behaviour.

1. **I-R3-1** — `src/ledger.rs:250-254`: `Ledger::open`'s docstring still asserts serve
   arms the SIGTERM handler *after* calling it; I-R2-1 inverted that. Reword to the
   current ordering (a blocking `open` there now wedges a server that never serves —
   availability, not durability). Keep the probe-placement conclusion; it stands on its
   own.
2. **I-R3-2** — `scripts/observability/README.md:320-327`: the parked-writer reading
   names the wrong transport. A parked writer writes nothing, heartbeats included, so the
   case is visible only through **live `lambo_stats`**, never in the file; the
   heartbeat-trend reading belongs to the writer-*behind* case. One clause. Optionally:
   `header()` mentions a non-zero last-heartbeat `queued` beside the dropped line.
3. **I-R3-3** — `I-observability.md`'s Handoff Log lacks an entry for `1f86792`'s two
   behavioural changes: the arming move (option 2 deferred, and why) and the
   `ledger_queued_lines` key — fold in the arithmetic deviation's reasoning
   (`accepted − written − write_failed`, because channel-full rejects never enter
   `accepted`), which is the part most likely to be re-derived.

Guidance handed forward with them: serve-startup ordering claims live in more prose sites
than any one of them signals — the cheap defence when touching that ordering is an `rg`
sweep for the claim, not a read of the neighbourhood. J2 moves this ordering again;
apply the sweep then.

Sweep once per **claim-family**, not once per change. Round 1's own conclusion was that
"sweep for the ordering claim" is too narrow: the ordering sweep came back clean, while
widening by one term — the `ledger_*` key set — found a stale count immediately (J0-R1-5),
and round 2 then found the same class again in the very file the fix touched (J0-R2-2). So
J2 gets **two** sweeps, because it moves two families: the startup ordering, **and** the
lease/endpoint schema. The second one is not hypothetical — adding `endpoint` to
`session_leases` falsifies a column list quoted verbatim at J2's own bullet below, both
`001_init.sql` files, and the two `INSERT INTO session_leases (…)` column lists in
`store/sqlite.rs` and `store/cockroach.rs`. Before finishing a change, sweep for whatever
the change is a claim *about*, and count what you find — the recurring defect across three
review rounds was never the ordering claim, it was the register not keeping up.

One more, from round 2's P2: **a fixture for an ordering claim must oppose file order and
claimed order**, or it cannot see the ordering at all. J0's `queued` fixture first wrote
its heartbeats in stamp order, so deleting the `ts` sort outright still passed the gate
40/40 — while the same commit hardened the prose calling that sort load-bearing. Write the
newer record first; prove the fixture by making the mutation it exists to catch and
watching it go red.

**Depends on:** nothing.

## J1 — Per-call agent identity

**Status: done.** Landed on `wt/j1` as one implementation commit plus one round-1 review
  remediation commit; see the J1 Status note and the round-1 remediation note at the end of this
  section for what shipped, what it decided, and what the review changed.

The serve applies its own `--agent` to every connected client, so per-call `agent_id` is
accepted, warned about, and ignored. Under a shared writer there is no correct value for
`--agent`: any id naming one client falsifies the others.

Not a provenance nicety. **`lambo_reserve` is hard-refused for any client whose `agent_id`
differs from the serve's:**

```
lambo_reserve: refusing to take a soft lock on behalf of 'claude-orchestrator': this
process holds the session as agent 'omp-agent' ... you could release a lock you do not
hold. NOTHING WAS RESERVED OR RELEASED.
```

Fail-closed and correct, but a shared writer therefore leaves every client but one without
the only mutual-exclusion primitive lambo has. Connectivity restored, coordination still
broken.

**The work:** accept `agent_id` as an override at the Memory level rather than only at
session attach, and record it on the interaction. Small, and it gates J2, J3, and the
ledger's usefulness. Mechanically smaller than this section implies: `graph::derive`,
`record_action`, `reserve` and `release` all already take `&AgentId` as an explicit
parameter — only `Memory` hardcodes `&self.agent` at its call sites, and the ledger
already attributes lines to the *caller's* id, not the process agent.

**The catch (found reviewing J0, decided by the operator, not the implementor):**
`agent_id` arrives on the wire **unauthenticated**. Over stdio the client owns the
process; over HTTP there is one bearer token for the server, not one per agent. Honouring
the field naively restores the exact defect `require_session_agent`'s docstring names —
*"mutual exclusion that reports success without providing exclusion is worse than no
mutual exclusion"*: two clients that both default to the same id (`"claude"`) share every
lock while each is told it holds one, and any client can release a lock it does not hold
by naming the holder. The candidate designs differ materially (bind identity to the
transport connection; require distinct declared identities at attach; accept
caller-asserted identity as cooperative-only and say so in the tool description), so
**J1 does not start until the operator has picked one** — the choice is recorded here
when made.

### J1 Status — landed

**What shipped.**

* **`Memory` grew `_as` twins**, not a changed signature: `derive_as`, `record_action_as`,
  `reserve_as`, `release_as`, and the private `begin_interaction_as`, each taking
  `&AgentId` first. The plain methods delegate (`self.derive_as(&self.agent, …)`), so every
  CLI, demo and test call site is untouched. Rejected: (a) making the existing methods take
  an agent — churn across every caller for no gain; (b) an `as_agent(&id) -> handle` clone —
  a second handle shape invites someone to give it its own `ACTIVE_SESSIONS` slot or lease,
  which is precisely the confusion J1 must not create. One handle, one lease, one registry
  slot; the per-call id names a **writer**, never a session.
* **`Memory::agent()` changed meaning, and says so.** It is now the handle's *default*
  writer id plus its process identity (lease holder, `ACTIVE_SESSIONS` key, heartbeat and
  `lambo_stats` owner field). It is no longer "the only id this handle writes as". Consumers
  asking "who wrote this" must read the interaction's `agent_id`. `ACTIVE_SESSIONS` is
  untouched by per-call ids because no new handle is created, so `SecondSessionWriter`
  detection is neither weakened nor spuriously tripped by a foreign per-call id.
* **`LamboServer::attribution` is gone**, replaced by `check_agent_id` (shape only:
  non-empty, size cap, and — since the round-1 remediation below — single-line) and
  `caller_agent` (→ `AgentId` for the write path). The mismatch
  warning was **deleted rather than reworded**: after J1 the caller's id is what gets
  recorded, so a warning saying otherwise is simply false. The id is taken **verbatim and
  untrimmed** — normalising would silently merge two callers' locks, the one failure mode
  this design exists to avoid.
* **`require_session_agent` is deleted** with its call site. The refusal's own reasoning
  ("mutual exclusion that reports success without providing exclusion is worse than no
  mutual exclusion") is honoured by *providing* the exclusion instead of refusing to try:
  `reserve_as`/`release_as` contend on the caller's id, so two clients through one serve
  hold distinct locks and a non-holder still cannot release.
* **The declaration** (the operator's compensating control) is in three places: all seven
  `agent_id` param descriptions, `lambo_reserve`'s tool description, and the server
  instructions string — caller-asserted and unverified, one stable id per agent, distinct
  ids get distinct locks, a shared id shares locks. Mirrored in `docs/reference/mcp.mdx`
  and `site/src/content/docs/mcp.mdx` ("How `agent_id` is used", the `lambo_reserve`
  argument row, the quoted instructions block).
* **Ledger:** unchanged and already correct — `ledger_agent` copies the caller's `agent_id`
  onto the line. Verified for the case that did not exist before: a *foreign-id reserve
  that now succeeds* books `op=reserve, granted=true, outcome=ok, agent_id=<caller>`, and
  the only remaining reserve refusal is a real §11 conflict (`error_kind="conflict"`).
* **Read side needed nothing.** `recall`'s reservation line comes from
  `active_reservation(graph, id, now)` unfiltered by caller and renders
  `"Reserved by <holder> until <ts>"`, so it already surfaces *other* agents' locks with
  the holder named; phase-2 traversal is agent-agnostic. Pinned by a test rather than
  assumed.

**Not done, deliberately:** `Memory::demote` has no `_as` twin — it is not on the MCP
surface, so J1 added no caller for one. Nothing about authentication: no tokens, no
connection binding, no attach handshake. The lease and its fencing token are untouched.

**Sweep (claim-family rule).** Corrected: the seven param docs, the `lambo_reserve` tool
doc, the server instructions, the two `mcp.mdx` mirrors, and
`scripts/observability/make_sample.py`'s reserve-error line — whose `error_kind` was
`"refused: foreign agent"`, a class no code can emit any more; reclassified to `"conflict"`
(the only refusal that now exists) and the committed sample regenerated so the `verify.sh`
drift gate stayed green. Left, as accurate history: `adve-review-t8.2-mcp.md` and
`-r2.md` (past-tense review findings, including the ones that *proposed* `reserve_as`),
this section's own quoted refusal text above (it is the defect J1 fixes, not a claim about
today), and `skills/lambo-cloudops/SKILL.md`'s "every MCP tool takes your `agent_id`" plus
its "do not run two writer processes against one session" — both still true, the second
because the lease is untouched. `PHASE-8-surface.md`'s T8.2 finding, which promised exactly
this change, gained a one-line closure pointer rather than an edit to its narrative.
Missed by this sweep and corrected in the round-1 remediation: `evidence/mcp-client-stdio/`,
whose README carried the deleted refusal and the deleted warning in the present tense. Its
treatment is an annotation, not a rewrite — a capture is only evidence while it stays
byte-exact, so the header now declares what J1 superseded and every transcript is untouched.

**The four kit scripts, read against a two-agent ledger** (synthetic, interleaved
`agent-a`/`agent-b` traffic with one restart mid-file). Nothing is wrong; all four were
already agent-partitioned or agent-agnostic in the right places, so nothing was changed:

* `recall_first.py` filters `calls` by `agent_id` **before** cutting work sessions, so
  interleaved traffic does not merge two agents' sessions — verified: the two agents get
  separate session rows and separate compliance figures. The process restart splits *both*
  agents' sessions, which is correct: one process, one in-RAM graph, so a restart really is
  every agent's boundary.
* `_ledger.py`'s `agents()` is a sorted distinct-id set — degenerate before J1, genuinely
  multi-valued now, correct either way. `restart_times()` reads heartbeat `uptime_secs`,
  and heartbeats are per-*process*: J1 adds agents, not processes, so there is still exactly
  one restart stream. No per-agent notion of restart is needed or implied.
* `dedup_rate.py`'s time buckets aggregate every agent, and its `per_agent` block breaks
  the same traffic down. That is the right pair: dedup is a property of the shared graph and
  cross-agent matching is the *point*, so a bucket mixing agents is the interesting number.
  Worth knowing when reading the report: a single agent's rate is the "Per agent" block,
  not the bucket table.
* `warnings.py`'s "By agent (who was warned)" attributes to the caller and becomes
  meaningful for the first time.

The heartbeat's `stats.agent` stays the process agent, and no script reads it — so nothing
in the kit infers "who does the work here" from process identity.

**Done-when.** The first box is ticked: two clients through one serve process hold distinct
locks, a foreign id takes and releases its own lock, and a non-holder cannot release —
pinned by `two_agents_through_one_server_hold_distinct_locks`. "One hub" in the box's
wording arrives in its proxy sense with J2; J1 pins the shared-process sense.

**Tests** (all in `mcp::server::tests` unless noted):
`two_agents_through_one_server_hold_distinct_locks` (contention, non-holder release refused,
foreign lock granted, holder can release);
`a_foreign_agent_ids_write_is_recorded_under_the_callers_id` (asserts on the graph's
interaction `agent_id`, for both `derive` and the `spawn_blocking` `record_action` path);
`a_foreign_agent_id_is_honoured_without_an_attribution_warning` (the warning and the
one-serve-per-agent advice are gone);
`the_memory_default_agent_path_is_unchanged` (the plain `Memory` methods still stamp the
handle's agent);
`i1_record_action_reports_edges_and_reserve_reports_grant_or_refusal` (extended: the
foreign-id grant line, and `error_kind="conflict"` for the refusal);
`warnings_reach_the_text_content_not_only_structured_content` (retargeted onto
`lambo_reserve`'s advisory warning, since the attribution warning it used to ride on is
gone, and extended to assert recall names another agent's lock holder).
Added by the round-1 remediation:
`a_multiline_agent_id_cannot_inject_lines_into_another_agents_context` (the reviewer's probe,
both render paths, plus a recall as an innocent agent) and
`every_tool_refuses_an_unusable_agent_id` (the whole refusal set × all seven tools);
`two_agents_through_one_server_hold_distinct_locks` extended to pin the holder and the
expiry in the conflict text.

**Uncosted consumer dependency (J0 round 1):** the observability kit becomes genuinely
multi-agent the moment J1 lands. `recall_first.py` groups compliance by agent,
`_ledger.py`'s `agents()` enumerates ids, and `dedup_rate.py`'s bucketing plus
`restart_times()`'s work-session boundaries were all written under the
one-agent-per-file assumption that one serve stamping one `--agent` makes degenerate
today — none of them declares it. J1's Done-when includes re-reading those four against
a two-agent ledger, not just the server side.

### J1 round-1 review remediation

Reviewed **REQUEST_CHANGES** at `00cf4c9` — one P1, one P2, six P3
([adve-review-mooshik-J1-round1.md](../adversarial-review/adve-review-mooshik-J1-round1.md)).
All eight are closed in the round-1 remediation commit; nothing is carried. The design
decision is untouched: everything below is a guard, a rendering, or a declaration.

* **J1-R1-1 (P1, the blocker) — a caller-asserted `agent_id` could inject whole lines
  into another agent's context block.** `check_size` allows `\n` and `\t` on purpose,
  because both are legitimate inside a concept's `content`; this id is not content. Since
  J1 it is rendered verbatim into the T5.3 block *another* agent reads, by two renderers
  that do not sanitise — the soft-lock holder (`recall::format::reservation_warning`) and
  the §13 conflict sentence's writer (`recall::format::conflict_warning`, which needs no
  lock at all, just one `lambo_derive`). The reviewer's probe landed a line wearing
  Lambo's own `⚑ CANONICAL` marker; it is now a committed test that fails without the
  guard. **The guard refuses `\n`, `\r` and `\t` in `check_agent_id`** (`\r` is already
  refused upstream; named so the rule reads complete if the caps exception table changes),
  and the refusal names the parameter, the codepoint and the reason.
  **At the door, not in `AgentId::new`:** the type is also built from the operator's own
  `--agent` by the CLI and by library callers — trusted input on the same side of the
  boundary as the process — so tightening the type would change its semantics for every
  caller, which is not J1's to do. `check_agent_id` is the single place an unauthenticated
  remote string becomes a write identity and a lock name. Nothing was added to
  `recall::format`: sanitising there would put the guard downstream of the graph, where a
  poisoned id is already durable.
  **Length deliberately not tightened by the remediation, and the consequence declared
  rather than dropped:** an over-long id forges no structure, and an MCP-only cap would
  diverge from `--agent` and from `AgentId` itself — but because assembly keeps the longest
  score-ordered prefix that *fits* `max_tokens`, dropping whole blocks, a 16 KiB holder id
  can evict the very block it annotates from another agent's context.
  **Ruled the same day (operator, 2026-08-20): capped at the door.** `MAX_AGENT_ID_CHARS
  = 256` in `check_agent_id`, beside the single-line guard — an id is a name other agents
  read, 256 is generous for any real client id, and the divergence from the uncapped
  `--agent`/`AgentId` is deliberate (that door is where unauthenticated remote identity is
  policed; trusted process-side callers keep the type's semantics). Boundary pinned from
  both sides in `every_tool_refuses_an_unusable_agent_id`: 257 refused on all seven tools,
  exactly 256 accepted. The rendering-side bound question is thereby closed for J2 —
  eviction needed the uniform cap's headroom, which no longer reaches the graph.
* **J1-R1-2 (P2) — the loser of a race was told `conflict` and nothing else.** `tool_err`'s
  N4 policy discards a `Memory` error's message because it can interpolate a DSN, a store
  URL or a driver string. §11's two conflict messages carry none of that — a node id the
  caller just sent, the holder's id, an expiry — and the last two are *already* model-facing,
  since recall renders the same pair into the context block. They are also exactly what
  "coordinate by ids" needs. So the reserve path gets `conflict_err`: **one variant on one
  path**, not a general opening of N4. Everything else on that path still goes through
  `tool_err`, the ledger books the same `error_kind="conflict"`, and the message is folded to
  one line on the way out as defence in depth for a holder that entered by another path.
* **J1-R1-3 (P3)** — the four `warnings` vectors that could no longer hold a warning are
  gone; `structuredContent` keeps its `warnings` key (response shape) as a literal `[]`, with
  a comment at `derive_impl` saying a future warning must also go through `attach_warnings`.
* **J1-R1-4 (P3)** — the split `use crate::types::` imports are one line.
* **J1-R1-5 (P3)** — both over-long lines rewrapped to their local width: the
  `attach_warnings` doc comment to ~78, and the `mcp.mdx` fenced instructions block to ~88
  in both mirrors, wrapped the way the rest of that block is rather than mirroring the Rust
  string's breaks. The two remaining >100-char lines in `src/mcp/server.rs` are pre-existing
  `json!` bodies inside tests, which `rustfmt` does not reformat and J1 did not touch.
* **J1-R1-6 (P3)** — `evidence/mcp-client-stdio/` annotated, not rewritten; see the sweep above.
* **J1-R1-7 (P3)** — `every_tool_refuses_an_unusable_agent_id` pins empty, blank, `\n`,
  `\r\n`, `\t`, oversize, and over-cap (257) across all seven tools, and asserts each
  refusal *names* `agent_id` so a downstream failure cannot pass for the guard; exactly 256
  is asserted accepted. Written as its own test rather than folded into
  `bad_parameters_are_refused_as_readable_tool_errors`, whose table is per-tool parameters;
  this one is the parameter all seven share.
* **J1-R1-8 (P3)** — declared in `src/daemon/conflict.rs`'s NEW-4 block: J1 is what makes
  same-instant collisions non-degenerate, so `writer` ("smallest interaction id at that
  instant") is now a deterministic choice between two *live* agents. Behaviour unchanged and
  still right for its purpose — the §13 sentence's job is to make the reader look — but it
  can now name the wrong one of two real agents, which is why J3's Done-when asks for it to
  be measured.

## J2 — A losing serve proxies instead of exiting

On refusal, become a thin proxy to the holder and forward every tool call.

* Add an `endpoint` column to `session_leases` (today: `session_id`, `holder`,
  `acquired_at`, `expires_at`, `current_token`).
* `serve` always binds a local endpoint, a unix socket keyed by session, even under
  `--transport stdio`. Being reachable stops being a transport choice.
* A refused serve reads the endpoint from the lease it lost, connects, proxies. First
  process to start becomes the hub.

Full read and write for every client, real read-your-writes, no staleness label, and **no
client config change at all**: the stdio wiring that broke would simply work. A proxy is
also cheaper than a serve, running no `store::load` replay and holding no in-RAM graph, so
N clients cost one graph instead of N.

Costs are operational rather than semantic: a socket to bind and clean up, a stale endpoint
after unclean holder death (bounded by the 45s TTL), a busier startup path.

**Two catches found before J2 was written (J0 round 1, verified at the source):**

* **The proxy branch reopens I-R2-1's hole through a new door.** A *refused* serve never
  reaches `build_memory`'s success path, so everything the proxy does — bind, read the
  lost lease's endpoint, connect — runs **above** the shutdown-arming point at
  `serve.rs:795`, unguarded, holding a socket. And the existing regression test cannot
  see it: `serve_pre_handshake_durability`'s deliberately loose matcher fires on
  "session attached", which a proxy never logs. J2 must place the arming relative to the
  refusal branch *first*, as a design decision rather than a consequence, and extend that
  test with a proxy-path case whose matcher fires on whatever line the proxy does emit.
* **Unconditional binding collides with `authorize_bind`.** `authorize_bind` runs before
  `build_memory` *specifically* so a misconfigured bind costs nothing and leaves no lease
  behind — its docstring at `serve.rs:754-759` states the reason out loud, and "refusing
  here means no lease is taken" stops being true the moment every serve binds a socket.
  That sentence and the ordering it justifies must be restated deliberately, not
  falsified in passing.

Per the claim-family rule above: J2 runs **two sweeps** before it lands — startup-ordering
claims, and the lease/endpoint schema (this section's own column list, both
`001_init.sql` files, and the `INSERT INTO session_leases (…)` lists in
`store/sqlite.rs` and `store/cockroach.rs` all quote the current five columns).

**Considered and rejected: a durable write-intent queue.** The loser appends intents, the
holder drains them. Rejected because **validation needs the graph** — resolving concepts
and checking resolved-reflexive pairs cannot happen in a process holding none, so a queued
write is validated late, which is the silent-failure shape being designed out. It also
answers a contention problem that does not exist. Keep in reserve only for the
holder-restart window, where nothing else helps.

**Fallback if always-binding proves unworkable:** read-only attach, exposing the read verbs
against the durable store and returning the lease conflict from write verbs as a tool
result. Strictly worse: a store reader trails the holder's in-RAM tail by up to one flush
interval, so every response would need a freshness label. The CLI already models both
halves — `recall` takes no lease, `stats` marks writer-only fields unavailable.

## J3 — Writes acknowledged before the embedder

A warm `lambo_derive` is 27ms, of which 22 to 25ms is the embedding call. Durability is
*already* async: write-behind returns long before anything reaches disk. The wait buys the
agent nothing it is waiting for.

**Rule:** a write may be acknowledged asynchronously when its result does not gate the
caller's next action. `lambo_derive` and `lambo_record_action` qualify. **`lambo_reserve`
does not** — its result *is* the caller's next action, and an async reservation has two
agents editing while each believes it holds the lock.

**Shape:**

1. **Synchronous validation pre-pass, then ack.** `derive` already validates in a read-only
   pre-pass needing no embedding; keep it on the call path so common errors still surface
   at call time. This keeps one synchronous hop, since validation resolves against the
   graph and a proxy holds none. Async removes the embedder wait, not the round trip, and
   at 0.4ms the round trip is not worth removing.
2. **Embed, canonicalize and insert in the background** through the ordinary path.
3. **The ack carries a receipt id.** Outcomes are stored against it and delivered two ways:
   piggybacked on that agent's next tool response (tagged, so self-identifying), and
   fetchable by id. Not an MCP notification as the mechanism — notifications land in a
   client log rather than the model's context, repeating the exact failure this workstream
   exists to fix. The receipt doubles as **opt-in synchrony**: an agent that needs its write
   applied waits on the receipt, restoring read-your-writes on demand without charging
   every agent for it. No `await` flag needed.

**Dedup is unaffected.** Embedding precedes insertion either way, so the vector is present
when matching happens.

**Constraints on the background path:**

* **Backpressure.** Async adds no capacity, and batching does not help on this rig. The
  queue bound must be derived from a ceiling **measured on the deployment's own embedder**,
  never a constant: a hosted or GPU embedder may be slower per call while parallelising far
  better, at which point batching pays and this rig's result inverts. Drop policy is fixed
  regardless — bound, drop, log once, count in `lambo_stats`.
* **Per-agent FIFO.** Derive chains `Temporal` edges between consecutive interactions;
  out-of-order draining corrupts the chain. Interleaving across agents is fine.
* **Receipts:** expired must not read as unknown, and restart-lost must not either.
  Per-agent scoping is why this depends on J1.
* **The crash window widens but is not new** — the write-behind tail already dies with a
  crashed holder today.
* **The `ledger_queued_lines` arithmetic must be re-derived, not assumed.** Its gauge is
  `accepted − written − write_failed`, and that formula is correct *only because*
  `try_send`'s `Err(Full)` arm never touches `accepted` — the whole exclusivity argument
  is pinned by a test the alternative formula fails (see I-observability.md's Handoff
  Log). A receipt queue is a second bounded channel with its own accept/reject
  accounting; if receipts ride the ledger's counters, or the background path adds a
  third drop class, the shared subtraction in `Ledger::shutdown` and `queued()` —
  deliberately one expression so the live gauge and the exit count cannot drift — stops
  being obviously right. Re-derive it against the new counter sites;
  `adve-review-mooshik-I-round3.md`'s flip D is the map for the never-`accepted`
  classes.

## J4 — Lease conflicts leave an artifact

A serve that loses the lease exits before it can open a ledger, so the most common
multi-agent failure is structurally invisible to I1 as specified. Two halves: a **pre-lease
startup line** written before the acquire attempt, and the **holder recording refused
takeovers**. Without these, metric 6 friction and every "why did this agent have no memory"
question stay unanswerable from artifacts.

J2 makes it cheaper, since a proxying serve is alive and can write its own lines.

## J5 — Transport defaults and config layering

* Document HTTP as the default for any machine running more than one client. DOGFOOD.md's
  option (a) reasoned that one orchestrator holds one connection and subagents inherit it;
  that holds for subagents, not across sessions, which the client app multiplies by
  restoring prior ones at launch.
* **A transport migration touches every config layer on the machine.** Repointing the
  project `.mcp.json` while pi's user-scope `~/.pi/agent/mcp.json` still carried a
  `command` entry produced a server holding both `command` and `url`, which pi rejected
  even though its probe of the endpoint succeeded.
* Consider `lambo serve --print-client-config <client>` so migration is a copy rather than
  a hand-edit per layer.

Lowest priority: once J2 lands, transport stops being the user's problem to get right.
Worth keeping as hygiene.

**Catch:** the `--ledger`/transport prose lives in four hand-maintained mirrors —
`docs/reference/{cli,mcp}.mdx` and `site/src/content/docs/{cli,mcp}.mdx` — kept as
byte-identical pairs with **no drift gate** (`verify.sh` drift-checks only the
observability sample). J5 edits all four; add the one-line `diff` of each pair to CI
*before* editing them, so the edit lands against a gate instead of installing the first
drift.

---

## Measurements (2026-08-19, pinned `3039b82`, SQLite + BGE-M3 q8_0 on CPU)

| | measured |
| --- | --- |
| Local MCP round trip, no work (the proxy hop) | 0.31 to 0.48 ms |
| `lambo_recall` | 27 to 123 ms |
| `lambo_derive`, warm | 27 ms |
| Raw embedding call, same text | 22 to 27 ms |
| 4 recalls sequential vs concurrent | 380 ms vs 64 ms (5.94x) |
| 10 embeddings batched vs one-at-a-time | 234 ms vs 198 ms (no win) |
| `flush_lag` observed | 145 to 227 s |
| `LEASE_TTL` / heartbeat | 45 s / 15 s |

The proxy hop is under 1% of any call that embeds, and roughly 100x smaller than a single
recall's run-to-run jitter. The hub parallelises, so it is not a serialization point.
Configured ceilings (`rate_limit_rps=50`, `max_sessions=32`) bite long before the hop does.
Every figure is one rig, not a property of lambo.

## What J does not change

* **The lease stays.** No weakening, no preemption, no change to the fencing token. A proxy
  takes no lease and presents no token; every durable write still happens in the holder,
  under the holder's token. The proxy moves the call, not the write.
* **Single-writer stays the deployment model** (spec §2.2). J is about what the losers do.
* **Not a substitute for I.** J4 is a requirement placed on I1, not a second ledger.

## Done when

- [x] A client whose `agent_id` differs from the serve's can take and release a soft lock,
      and two clients through one hub hold distinct locks (J1) — through one serve process;
      the proxy sense of "hub" is J2's
- [ ] `lambo serve` against a held session starts as a proxy instead of exiting 1, and every
      tool call including writes succeeds through it (J2)
- [ ] A write through a proxy is durable in the holder and visible to that client's next
      recall, pinning read-your-writes across the hop (J2)
- [ ] `session_leases` carries the holder's endpoint and `serve` binds a local socket even
      under `--transport stdio` (J2)
- [ ] Killing the holder uncleanly leaves proxies failing honestly rather than hanging, and
      a new holder is electable within one `LEASE_TTL` (J2)
- [ ] Two clients on one machine, both wired over stdio, both fully working — verified with
      two different client products, not two sessions of one (J2)
- [ ] `lambo_derive` returns after validation without waiting on the embedder, and its call
      time drops to the round-trip floor (J3)
- [ ] Every write ack carries a receipt; outcomes are retrievable by it; expired and
      restart-lost answer distinctly, never "unknown" (J3)
- [ ] Waiting on a receipt restores read-your-writes for a caller that asks (J3)
- [ ] The queue bound comes from a ceiling measured on the deployment's own embedder, drops
      are counted in `lambo_stats`, and a burst degrades visibly (J3)
- [ ] One agent's writes apply in submission order, pinning the `Temporal` chain (J3) — and
      with two agents interleaving through one process, the §13 conflict sentence's `writer`
      is **measured** rather than assumed: J1 made the same-instant collision path
      non-degenerate (J1-R1-8)
- [ ] A refused lease acquisition appears in the ledger from both sides (J4)
- [ ] Docs state the multi-client default and the every-layer config rule (J5)
- [ ] The concurrent-client probe from 2026-08-19 is a committed test, not a shell transcript

---

## Rig re-pin rides with J's landing

When J lands, the dogfood **rig re-pin and the runbook update are one act, not two**:
J changes `serve`'s startup behaviour (J2's proxy-on-refusal, J5's transport defaults) in
ways that make parts of [DOGFOOD-SETUP.md](DOGFOOD-SETUP.md) §4/§5 stale the moment a
J-carrying binary serves — §5's "one client at a time" interim rule is *specifically*
written to be deleted by J2, and the per-client stdio blocks change meaning when a losing
serve proxies instead of exiting. Re-pinning without the runbook edit ships a rig whose
operating instructions describe the previous binary; editing the runbook without the
re-pin describes a binary that is not running. Do both in the same commit/upgrade event,
and let the heartbeat's `git_sha` change be the proof, per §2.

Status note: this machine's rig was re-pinned to `0f672f1` (the I-close, ledger-carrying
binary) on 2026-08-20; other machines re-pin per the runbook whenever they next set up —
nothing else re-pins tonight.

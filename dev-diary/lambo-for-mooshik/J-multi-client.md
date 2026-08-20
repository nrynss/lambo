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
| J1 | Per-call agent identity | nothing |
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
again — the three numbered items below stay as the spec they were, not as open work. The closure narrative lives in
[I-observability.md](I-observability.md)'s Handoff Log, which is the right home for I's
remediation history; this is the pointer from J's board.

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

**Depends on:** nothing.

## J1 — Per-call agent identity

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
ledger's usefulness.

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

- [ ] A client whose `agent_id` differs from the serve's can take and release a soft lock,
      and two clients through one hub hold distinct locks (J1)
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
- [ ] One agent's writes apply in submission order, pinning the `Temporal` chain (J3)
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

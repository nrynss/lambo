# Future guidelines — decided direction, deliberately unbuilt

Decisions about work that is **not scheduled and not being built now**, recorded so that
when the time comes the design starts from a decision rather than a debate — and so that
no current document accidentally implies these constraints bind today's work. Entries
here are direction, not commitment: each names its trigger, its blockers, and where its
pieces already live. Nothing in this file may be treated as an open task by an agent; the
workstream docs are the only source of schedulable work.

---

## The bootstrap ingest is a separate method, not a serve-path workload

**Decided 2026-08-22 (operator). Trigger: Mooshik's bootstrap design, post-D. Built: no.**

Mooshik's decade-scale bootstrap — ten years of history arriving in one sitting — will
not ride the interactive serve path. That path is shaped for interactive traffic:
per-write acks, receipts, the J3 durable-intent WAL, a close budget, a lease per session.
A bulk ingest wants none of it. There is no agent awaiting an ack, no second client to
coordinate with, and no reason to pay per-write intent bookkeeping when the whole run is
one resumable job.

**The arithmetic that forces the split, measured (F4, live serverless Cockroach):**
`close_ms ≈ 221 + 249.4·K`, ~110 ms effective per statement — the J3 close-flush envelope
was accepted at durable-intent tails ≤ ~150. A decade of history through that path is not
slow; it is unviable. The bulk path is therefore not an optimisation of the serve path
but the only shape the numbers permit. Conversely: **the K ≤ ~150 envelope in
[J3-durability-redesign.md](J3-durability-redesign.md) is an interactive-path constraint
and must never be read as a bound on the bootstrap**, and Option 2 (folding per-concept
embedding into the concept upsert) is not a bootstrap prerequisite.

**The shape, when it is built:** a bulk ingest verb — single process with exclusive store
ownership, batch embedding, direct bulk flushes, checkpoint/resume at file granularity
(the WAL's job done per checkpoint instead of per write).

**The pieces already on the map — this composes, it does not invent:**

* `seed()` is deliberately **off-lease**: `lease_permits_write` passes when no lease was
  ever minted, so the bulk bypass is part of the fencing contract, not a hack around it.
* **K2's `re-embed` verb is the same pipeline** (read texts → embed in batches → bulk
  write); a bootstrap is re-embed plus create-the-concepts.
* **D is the true blocker, not throughput.** Without event time, a bulk ingest produces
  the everything-happened-at-once graph that breaks every temporal gate — D's founding
  premise. No bootstrap verb before D2.

**Sequencing:** D → K2's plumbing → a thin bootstrap verb composing the three, designed
under E/Mooshik planning. Not a J concern, not a scheduled workstream, not to be started
from this file.

---

## Cross-host proxying: a loser forwards to the holder over HTTP

**Decided 2026-08-23 (operator) as the direction, alongside the ruling that B ships
park-and-fail-over instead. Trigger: a requirement for two machines to write one session
*simultaneously*. Built: no.**

B's ruling (see [B-postgres-store.md](B-postgres-store.md), item 4) is that the losing
machine's writer parks, serves reads, and takes the lease when the holder's lapses. That
costs simultaneity: a write on one machine is visible on the other in about a second, but
the second machine cannot write while the first holds the lease. Cross-host proxying is
what removes that cost, and it is deliberately not in B.

**The shape.** Today `proxyable` refuses with `HolderIsOnAnotherHost`, and correctly: the
lease row's `endpoint` is a **unix socket path**, which names a socket on the holder's own
machine and is meaningless anywhere else. Cross-host proxying replaces that refusal with a
dial — the loser forwards to the holder's HTTP endpoint rather than to a local socket.

**Why it is now cheap enough to be worth naming.** The 2026-08-23 HTTP ruling already makes
every rig writer an HTTP server on a port under a supervisor. The transport this needs
therefore exists; what is missing is the addressing and the trust, not the server.

**The pieces already on the map — this composes, it does not invent:**

* **J2's proxy is a byte-level JSON-RPC line pipe**, not a tool-level forwarder. That is the
  property to preserve: the caller's per-call `agent_id` crosses **verbatim** by
  construction, which is *why* J1 gated J2. A cross-host forwarder that rebuilt requests is
  exactly where that contract would regress.
* **The lease row already carries an `endpoint` column** (J2), and it is deliberately not
  part of `LeaseHolder::token()` — the token is the identity a refresh and a release match
  on, and must stay stable even if a holder's reachability changed under it. A reachable
  address is a change of *content*, not of shape.
* **The holder token already names the host** (`agent@host#pid`), which is what
  `holder_is_on_host` reads. The information needed to route is present; only the routing is
  missing.
* **`serve` already refuses to bind beyond loopback without `--auth-token`**, because it is a
  session writer. That refusal is the security design for this feature, already written.

**What it costs, and why that keeps it out of B.** A non-loopback bind, a shared secret
between machines, and a network the machines may trust. Those are operational and security
commitments, not a refactor, and B's goal — a unified cross-machine *store* — is met without
them. Park-and-fail-over is the smaller correct thing; a parked writer is precisely the
process that would learn to dial, so nothing here has to be undone first.

**Not to be confused with in-process promotion**, which J2 rejected as too large and keeps in
reserve (`Local(Arc<Memory>) | Remote(hub)` on `LamboServer`). That answers a different
question — how a *same-machine* proxy becomes the holder when the hub dies — and is the fix
for the stdio path's lifetime coupling. The two are independent; either can land first.

**Sequencing:** after B closes and only on the trigger above. Nothing in this file is
schedulable work.

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

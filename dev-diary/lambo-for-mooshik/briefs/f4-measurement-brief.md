# F4 — live close-time flush latency vs the real Cockroach cluster (measurement)

## Context
The "measurement to schedule" from the J3 durability redesign, deferred by the remediation
agent and still open. Repo: `/home/nryn/work/lambo`, on `lambo-for-mooshik` at `fe98b23`.
The live Cockroach DSN is on this machine (`.env` → `LAMBO_COCKROACH_DSN`; serverless, GCP
asia-south1; AGENTS.md: DSN uses `sslmode=verify-full&sslrootcert=system`). BGE-M3 embedder
at 127.0.0.1:8080 is DOWN.

## What F4 asks (design of record)
`dev-diary/lambo-for-mooshik/J3-durability-redesign.md` §"The Cockroach cost this section
named is understated" (around lines 298-332): the close-time flush carries the durable
intents, and on Cockroach each durable-intent write is **3 extra statements (UPDATE +
retention DELETE + insert)**, and both intent mutations land in `plan_flush`'s `barrier`
arm which calls `buckets.drain_into` before each step → **2 bucket drains per write**,
fragmenting the multi-row batching L82-1 introduced. Against a serverless cluster at
~40 ms RTT this is "the shape of problem that shows up as a blown `CLOSE_FLUSH_GRACE` and
nothing else." `CLOSE_FLUSH_GRACE = 8 s` (`src/mcp/serve.rs:82`; CLOSE_GRACE 10 s,
SHUTDOWN_BUDGET 15 s). A blown 8 s budget abandons the close and **loses the tail** on the
serve path (no on-disk WAL).

A local fake-store test already pins the arithmetic: `an_at_cap_burst_drains_within_the_close_window`
(`src/memory.rs:3236`) with a 30 ms RTT store shows the per-mutation model blows
`CLOSE_FLUSH_GRACE` while the planned-statement model finishes in a fraction. Its docstring
says "no local test can reach a cluster" — **that live measurement is your job.**

## Your deliverable
A measured, evidence-backed answer: **at what durable-intent tail does the close-time
flush exceed `CLOSE_FLUSH_GRACE` (8 s) against the real serverless cluster, and what is the
per-durable-intent cost in ms (round trips + the 2 bucket drains)?** Report the honest
numbers even (especially) if the budget blows — that IS the finding F4 exists to surface.

## Method (adapt the existing driver, do not reinvent)

1. **Build**: `LAMBO_GIT_SHA=$(git rev-parse --short HEAD) cargo build --release --features
   store-cockroach,embed-fixture`. (store-cockroach for the live cluster; embed-fixture
   because BGE-M3 is down and derive must ack — the flush cost you're measuring is
   independent of the embedder, which is instant/uniform under fixture.)

2. **Driver**: follow `scripts/loadtest/capture_sigterm.sh` — it is the exact pattern
   (provision scratch store, `serve --transport http --port <free>` with a scratch
   `LAMBO_AUTH_TOKEN`, burst at-cap `record_action` calls via `scripts/loadtest/mcp_load.py`
   to build a large un-flushed tail, SIGTERM mid-burst, time **signal → "session closed,
   tail durable" / process exit**, then durability-check) — but with `[store] kind =
   "cockroach"` (the `.env` DSN) instead of sqlite. Use a **scratch session id**
   (e.g. `f4-cr-<ts>`) and a **scratch/dedicated schema or freshly-provisioned
   store-objects you clean up after**; never touch shared/live session data beyond a
   throwaway session you drop when done. Never print, log, or commit the DSN or password.

3. **Sweep**: run the burst+SIGTERM at increasing tail sizes (vary `--workers` /
   `--overdrive-calls` / burst rate; or a focused driver that writes N `record_action`
   calls producing many concepts/dependencies so the durable-intent tail is N and
   controllable) targeting roughly K ∈ {25, 50, 100, 200, 400} durable intents pending at
   close. For each K record: session, K, close-flush ms (signal→"tail durable"), exit code,
   whether the flush completed or was abandoned (abandoned → the "tail is LOST" error line
   at `src/mcp/serve.rs:1485`), and the resulting store count of intents/concepts.

4. **Fit**: report ms per durable-intent, the intercept (base close cost), and the K at
   which the fitted line crosses 8 s. State whether the live per-write cost matches the
   pessimistic barrier model (2 drains + 3 statements per write, ~4-5 RTTs) or the benign
   planned-statement model.

5. **Cross-check**: confirm the flush was honest — after the SIGTERM close, query the
   scratch session's `write_intents`/`concepts` rows via the DSN to verify the claimed
   durable count matches what the receipts said (acked ⇒ applied ∨ durable intent).

6. **Evidence + disposition**:
   - Capture timestamped evidence files under `evidence/` (a fresh `evidence/mooshik-f4-cockroach/`
     dir), following the repo evidence convention (README.md describes the format), with the
     machine, binary sha (`LAMBO_GIT_SHA`), token redacted, per-K table, and the fitted
     per-write cost + budget-corner number.
   - Record the disposition in `dev-diary/lambo-for-mooshik/J3-durability-redesign.md` where
     the F4 "Items that want the Cockroach-capable machine" bullet sits: replace the
     "still unmeasured" framing with the measured numbers and your verdict (stays under the
     budget at realistic tails / blows at K≈N, whichever is true), and any recommendation
     (e.g. attack the barriers) only if the data demands it.

## Truth-telling rules
- Never fudge the numbers to make F4 "pass." A blown budget at realistic scale is a real
  finding; record it and say so.
- If the live cluster refuses from this machine (allowlist), report the exact error and mark
  F4 as genuinely not measurable here — but the operator confirms it is reachable, so try
  for real first.
- Do not modify `.env`, the dogfood rig, or any committed code. DO NOT commit anything
  (the evidence + disposition doc entry are the deliverable; the operator handles commits).
- Leave the git working tree clean apart from your new evidence files and the doc edit.

## Report back (concise)
- Build + config used (path, features, sha).
- Per-K table (K, close-flush ms, exit, completed/abandoned, store counts).
- Fitted per-durable-intent cost (ms), the base intercept, and the K that crosses 8 s.
- Verdict: does it stay inside CLOSE_FLUSH_GRACE at realistic tails? Honest conclusion.
- Evidence file paths + the disposition edit made.
- Anything you could not do and why.

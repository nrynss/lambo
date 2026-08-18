# Concurrency capture (C1 to C5) — closing P8's last exit box

**Tasks are C1 to C5.** Numbered separately from remediation (T1 to T12),
hardening (H1 to H7) and deployment (D1 to D3), same as those are numbered apart
from each other.

This closes the one unchecked box in [PHASE-8's exit criteria](../PHASE-8-surface.md):
*surface holds under concurrency (T8.2 N1/N2 closure)*. It is the last open item
in P8 and it is **optional for the hackathon submission** — no §12.4 deliverable
depends on it. Do it because the claim "the MCP surface holds under load" is
currently unevidenced, not because the deadline needs it.

---

## What is already done, so nobody re-fixes it

N1 and N2 were findings from the T8.2 MCP review (round 3, 2026-08-14). The
review's verdict was that the surface was well hardened against string attacks
and weakly hardened against cardinality and lifecycle. **Both are fixed in
`main`:**

| Finding | The harm | Fix, verified in source |
|---|---|---|
| **N1 cardinality** | `produces`/`modifies`/`depends_on` had no cap; 50k entries took 116.8s | `MAX_ACTION_TARGETS` enforced at `src/mcp/server.rs:793` |
| **N1 starvation** | `record_action` ran synchronously on a Tokio worker; 12 concurrent 6k-entry calls starved every worker, `shutdown_signal()` could not be polled, SIGTERM sat for 300s | `tokio::task::spawn_blocking` at `src/mcp/server.rs:833`, with the SIGTERM reason in the comment |
| **N2 control chars** | A NUL in `lambo_derive` content was accepted, echoed by recall, then rejected by Cockroach `STRING`; the SQLSTATE mapped to a *retryable* `StoreError::Other`, so the flush loop kept the batch forever and the session went degraded | Refusal at the MCP layer, with a test at `src/mcp/server.rs:2229` covering bidi, zero-width and tag characters, including `("rtl override", "user\u{202E}schema")` |

The 2026-08-14 live pass found two residuals, **L82-1** (a close that blows its
deadline is dropped, so the lease release never runs) and **L82-2** (U+202E
accepted and landed in Cockroach). Both were subsequently remediated: the close
budget is now split (`CLOSE_GRACE` = `CLOSE_FLUSH_GRACE` + `LEASE_RELEASE_GRACE`,
documented at `src/mcp/serve.rs:50-70`), and the control-character refusal was
widened past the original ask.

**So the open box is not "N1/N2 were never fixed". It is "nobody captured the
concurrent-client proof the exit criterion named."**

---

## The criterion, restated as acceptance

K concurrent clients, K at or above the worker count (~12 to 32), issuing a mix
of valid and adversarial tool calls, and:

1. The process does not starve: SIGTERM still prints
   `lambo serve: session closed, tail durable` (`src/mcp/serve.rs:756`) rather
   than either `tail lost on exit` line (`:752`, `:824`, `:836`).
2. An oversized `record_action` gets the honest cap refusal, not a hang.
3. No internal detail crosses the wire: no DSN fragments, no
   `cockroachlabs.cloud` host, no sqlx/driver text, no internal URLs in any
   response body or error message.
4. Evidence lands in `evidence/`.

---

## C1 — Load driver

**Owns:** `scripts/loadtest/mcp_load.py` (new), `scripts/loadtest/README.md`
**Status:** DONE — 2026-08-18. Stdlib-only driver, deterministic seed, K workers
(default 12), weighted valid + adversarial mix, every response to a JSONL
ledger. Phases shaped so refusals never crowd out the measurement:
cap-probe (503s at the 32-session ceiling) → overdrive (free-run for the
first `--overdrive-calls` calls per worker, then paced ~20 rps; 1682
rate-limit 429s observed, 1673 of them inside the overdrive window) → paced
main window (40 rps; zero rate-limit refusals — 9 429s straddle into the
window's opening as boundary carryover, and 21 in-window `lambo_derive`
calls returned tool-level `store error` responses) → paced at-cap burst (the
SIGTERM tail).
Drive `lambo serve --transport http --bind 127.0.0.1 --port 7700 --auth-token <tok>`
against a **scratch** session (`--session c-load-<date>`), never `cloudops-exhibit`.

Each of K workers loops a weighted mix:

- valid: `lambo_derive` (a few concepts, some with `parent_of`),
  `lambo_record_action`, `lambo_recall`
- adversarial: `record_action` exceeding `MAX_ACTION_TARGETS`; content carrying
  NUL and `U+202E`; content over `MAX_CONTENT_BYTES`; an unknown tool name;
  malformed params

Record every response. The HTTP surface has a documented rate limit
(`DEFAULT_RATE_LIMIT_RPS = 50`, burst ×2) and a session cap
(`DEFAULT_MAX_SESSIONS = 32`) from T8.7 — a refusal from either is a **correct**
observation, not a failure, but the run should be shaped so refusals do not
crowd out the thing being measured.

## C2 — Run it, and pull the SIGTERM

**Owns:** `evidence/concurrency/` (new)
**Requires:** C1
**Status:** DONE — 2026-08-18. Run `20260817-204139`, session `c-load-20260818`,
K=12, SIGTERM 5 s into the burst. Exact line present, `tail lost on exit`
absent, **signal→exit 1419 ms, exit 0**. Stderr transcript, ledger, run
metadata and the SQLite store are in `evidence/concurrency/` with a runbook.

Start the server, ramp K to target, and send SIGTERM **while load is in
flight**. Capture the server's stderr in full, the exit code, and wall-clock
time from signal to exit. (The run did land SIGTERM mid-burst with load in
flight — 120 transport failures and 11 `ok` responses after the SIGTERM
timestamp — though the close-time tail turned out to be empty; see the
honest number in C3.)

The assertion is the exact line, not a vibe: `session closed, tail durable`.

**Requires:** C2
**Status:** DONE — 2026-08-18. Post-exit readback vs ledger:

| Metric | Ledger expected | Store | Verdict |
|---|---|---|---|
| interactions (1:1 per write call; append-only) | 841 ok writes | 862 | **AHEAD by 21** — in-flight calls landed after the SIGTERM timestamp (11 `ok` responses arrived after it; transport-failure calls' mutations landed too), flushed by the daemon's 1 s flush loop |
| concepts (created) | 1466 | 1359 | shortfall 107 — **fully explained**: one daemon GC sweep collected 107 (`concepts_collected=107`; spec §9 housekeeping). Created − store == collected exactly |
| edges (record_action lower bound) | ≥ 5506 | 9279 | OK |

**The honest number:** 0 shortfall on the interaction yardstick; 0
*unexplained* concept shortfall. The `CLOSE_GRACE` 10 s budget was **not**
tested to its limit on SQLite — the close-time tail was empty (no `Memory
session closed (tail flushed)` line in the transcript), so the close drain
was a no-op; the verified surplus is the 21 interactions that landed beyond
the ledger-ok set (11 `ok` responses arrived after the SIGTERM timestamp).
The concept-count comparison is GC-confounded by design; the durable-tail
claim rests on interactions (append-only, never GC'd). The GC accounting is
proven by the `gc_interval=1` control run (collected == gap exactly —
11 == 11), whose artifacts are committed under
`evidence/concurrency/control-gc/`.
**Status:** DONE — 2026-08-18 (see the table above).

The review never did this half. After the process exits, reconnect to the store
and count what should have survived. A reassuring log line is not durability.

Compare against what the driver believes it wrote (successful tool calls only).
A shortfall here is the real finding, and it would be about the budget rather
than a bug: `CLOSE_GRACE` is 10s, split with the lease release, so a large tail
against a live cluster may simply not clear it. If that happens, the honest
resolution is a decision about the number, recorded in this note, not a silent
bump.

## C4 — Disposition and docs

**Requires:** C3
**Status:** DONE — 2026-08-18. Statuses above updated; P8 exit criteria box
ticked with the hardware caveat recorded; board row updated; runbooks in
`evidence/concurrency/README.md`. C5 below is the only open piece.
**Owns:** this note, `dev-diary/PHASE-8-surface.md` exit criteria, `dev-diary/README.md` status board

Tick the P8 box, or record precisely why it stays open. Either way the diary
should stop describing this as "concurrency-on-MBP" with no further detail: the
missing thing was always the capture, and after C3 it is either captured or
explained.

**Hardware caveat to write down:** the criterion says *runs on the MBP*. If it
runs on the Linux box (16 cores, 2560x1440 display, CachyOS) the starvation
threshold differs, so say which machine produced the numbers.
## C5 — Optional: drive it with real local models

**Requires:** C2 green
**Status:** DONE-with-findings — 2026-08-18, re-run across three local models
and, decisively, across two harnesses.

**The result that matters.** Handed the lambo-cloudops skill text verbatim as
its system prompt and the four lambo MCP tools and nothing else, **Qwen3-0.6B
calls lambo correctly on its own** (`scripts/loadtest/mcp_agentic.py`, llama.cpp
OpenAI tools API). 3 agents × 151.0 s, 55 tasks, 173 tool calls (86 recall /
45 derive / 40 record_action / 2 inspect; 165 ok): **43/55 tasks recall-first,
and 0 of 45 derives without a prior recall in the same task**. The 12
non-recall-first tasks made no tool calls at all, so nothing derived blind.
Dedup 0.857 (36 of 42), 1120 completed tasks/hour, durability exact after clean
SIGTERM (interactions 82 == 82, concepts 12 == 12, edges 132, lease 0). A 0.6B
model given sufficient instructions reaches for recall before it writes: that is
a claim about lambo's surface, not about Qwen. Throughput is the secondary
number here.

**Tool-call capability, per model.** **Qwen3-0.6B**: emits a *correct*
`lambo_derive` `tool_calls` at the raw protocol level, and under a narrowed OMP
toolset selects it correctly; under OMP's *default* full toolset it picks the
wrong tool (`lsp`, hallucinated arguments). **LFM2-350M**: the original finding
stands, no tool calls (garbled pseudo-tool text under OMP, prose with
`finish_reason=stop` under the OpenAI tools API). **functiongemma-270m**: a
second no-tool-call finding, refusing in prose under OMP, with its native
`<start_function_call>` markup returned as content by this llama.cpp build, so
no swarm ran for it.

**The `mcp_swarm.py` figures are not agency.** That fallback harness hardcodes
prompt → derive → recall and gives the model no lambo semantics, so LFM2's
3961 derive-calls/hour (dedup 0.183) and Qwen3's 2956/hour (dedup 0.893, 22%
unparseable turns, 35/159) measure loop throughput and concept-text behavior
only. Qwen's higher dedup is inflated by repetition, including derives echoing
recall context verbatim and one that shipped literal `<concept text>`
placeholders. Both runs were store ledger-exact after clean SIGTERM.

All probe transcripts committed (fresh-run) under `evidence/swarm/probes/`;
per-model results table, durability figures and portal visuals (Qwen-derived
concepts as recall cards) in `evidence/swarm/README.md`.
**Relates to:** T9.6 (the LFM2 swarm, P9's cut-order #2)

**C5M round-1 remediation (2026-08-18, branch `codex/c5-models`):** the
round-1 review (adve-review-c5-models-round1.md) verified every probe,
ledger, durability and portal claim artifact-exact, and required two
evidence-completeness fixes that are now done:

* **C5M-R1-1 (narrowed OMP toolset):** `omp --no-tools` cuts the built-ins
  to read/write/edit (request-verified; the exact 15-tool array is in
  `evidence/swarm/probes/omp-request-tool-context.jsonl`), but OMP cannot
  drop read/write/edit or the MCP servers it inherits from the parent
  session, so a lambo-only toolset is not achievable. Under the narrowed
  toolset Qwen3-0.6B selects `lambo_derive` correctly under OMP
  (`probes/omp-harness-qwen3-narrowed.txt`) — the round-1 counterfactual —
  with the execution-target caveat that the inherited `mcp__lambo_*` server
  shadows the workspace-scoped scratch lambo (the OMP leg's calls landed on
  the harness's live lambo, agent 'cursor-agent'; scratch store read back
  0 rows). The OMP swarm re-run with the skill in the system prompt is in
  `evidence/swarm/probes/omp-swarm-qwen3-narrowed/`.
* **C5M-R1-2 (genuine agentic re-run):** `scripts/loadtest/mcp_agentic.py`
  ran with the lambo-cloudops skill as the system prompt, the four lambo
  MCP tools only, and the model choosing every call (llama.cpp OpenAI tools
  API). 3 agents × 151.0 s: 55 tasks (1120 completed tasks/hour), 173 tool
  calls (86 recall / 45 derive / 40 record_action / 2 inspect; 165 ok),
  **pre-flight protocol adherence 43/55 tasks recall-first and 0 of 45
  derives without a prior recall**, dedup 0.857 (36/42), 8 llama-server
  HTTP 500s and 15 degenerate turns recorded, durability green after clean
  SIGTERM (interactions 82 == 82, concepts 12 == 12, edges 132, lease 0 —
  `durability-agentic-qwen3-1787022500.txt`). Ledger +
  transcripts under `evidence/swarm/`.

**Task-text contamination, found 2026-08-18 while building a control arm.** The
six task strings hardcoded in `mcp_agentic.py` each read "modify the 'X'
resource: run the pre-flight recall protocol for it, derive the concept, record
the action with its depends-on edges, then re-check with recall." The model was
therefore *told* the recall-first sequence in every task, so the agentic run's
43/55 measures whether a 0.6B model can **execute** the protocol, not whether
the skill causes it to reach for memory unprompted. The first control arm
inherited the same task text in both arms and so could not isolate the system
prompt either; its 78.2% vs 35.0% headline also conflated adherence with
liveness, since the control was inert on 62% of tasks against the treatment's
22% under heavier machine load. Conditioned on tasks where the model acted at
all, that comparison is 43/43 (100%) vs 21/23 (91.3%). The corrected
experiment, neutral task text plus both arms re-run under equal load, is
recorded under `evidence/swarm/experiment2/`; the first control arm is kept
under `evidence/swarm/control/` as superseded. The public README claims only
the execution result.

**Experiment 2 result, and it is a negative one (2026-08-18).** With task text
reduced to "modify the 'X' resource and record what you changed" (banned-word
grep clean, `tasks-neutral.txt` sha `915bc889`), both arms were re-run back to
back: arm A with the skill (`SKILL.md` sha `fb9462e5`), arm B with the
protocol-free baseline (sha `a9103b28`). **Neither arm called `lambo_recall`
once.** Arm A made 21 `lambo_record_action` and 2 `lambo_derive` calls, arm B
made 42 `lambo_record_action` calls, and no recall appears even among the tool
calls the model *proposed* and did not execute, so this is not an execution
filter. Both runs completed cleanly with durability MATCH.

The reading, stated conservatively: **the skill's system-prompt pre-flight
protocol produced no recall-first behavior above a protocol-free baseline once
the task stopped asking for it.** What the model did instead was exactly what
the neutral task said, record what changed, which is defensible task-following
rather than misbehavior. So this is evidence about how little a 0.6B model
carries from a long system prompt into behavior the immediate task does not
restate, not evidence that lambo's surface is unclear. It does mean the
honest public claim is the execution one: given the protocol, a very small
model runs it correctly, 43 of 43 acting tasks, which is what the README
says and all it says.

Caveat carried from the run: the machine was not quiesced and 1-minute load
trended down across the session (5.97 → 5.09 → 3.46), so arm A ran under
somewhat heavier ambient load than arm B. Irrelevant to a 0 vs 0 primary
result, recorded anyway. Single run per arm, no retries.

**Round 2 waived (2026-08-18).** The independent re-review the C-series ran for
every other branch was waived here by operator decision, and the branch merged
after round 1 plus remediation. The reason: round 1 verified every number
artifact-exact, and the remediation added evidence rather than revising a
checked claim. What that leaves unverified by a second pair of eyes is the
`mcp_agentic.py` ledger arithmetic, its durability readback, and the
narrowed-toolset OMP transcripts, all of which rest on this branch's own
accounting. Two open operator items ride along: the OMP legs' writes to the
harness-inherited live lambo (agent 'cursor-agent') are still in that store,
and one of the fallback swarm's stored concepts may literally be
`<concept text>` from the placeholder-echo derive.

C1's driver is synthetic: deterministic, fast, and precise about what it sent.
That is the right instrument for the correctness half. The scale half wants real
agents.

This box already has the pieces:

- `llama-server` is installed and already serving BGE-M3 on `:8080` for
  embeddings, so a second instance on another port can serve a chat model.
- Local weights in `~/models/`: `LiquidAI_LFM2.5-350M` (the model T9.6 names),
  `kat-E2B-Q4_K_M`, `kat-E4B-Q6_K`, `gemma-4-E4B-it`, `Qwen3-30B-A3B-Q4_K_M`.
- The MCP surface is already proven with real agents: see
  [notes/video-shoot.md](video-shoot.md) and the `agent-skill` page for a model
  calling `lambo_recall` and `lambo_inspect` unprompted.

Smallest useful version: N instances of LFM2.5-350M through an MCP-capable
client, each given a task that requires a derive then a recall, run against one
session. Measure sustained tasks/hour and the canonization dedup rate. If it
fails, the failure is the finding and it is cut per P9's cut order without
touching the submission.

---

## Where things live

| Artifact | Path |
|---|---|
| Driver | `scripts/loadtest/mcp_load.py` |
| Run transcripts, server stderr, durability counts | `evidence/concurrency/` |
| Swarm evidence, if C5 happens | `evidence/swarm/` (the path T9.6 reserves) |

Scratch sessions only. Never point a load test at `cloudops-exhibit`: it is the
session the portal, the video and the submission all read from.

# P8 — Surface (binary, MCP, demo)

```yaml
id:       P8
branch:   phase/p8-surface
requires: [T2.3, T2.4, T4.3, T5.3, T6.4]   # soft: T3.2 (live store), T7.x (hybrid)
blocks:   P9
parallel: NO — serial by decision 2026-08-13 (see §Execution protocol)
```

**Goal:** assemble the library into `lambo`, expose it over MCP, and make the spec §13
two-agent demo scripted and reproducible. This is where the tracks converge; expect
integration friction here, not in the tracks — budget for it.

**Status as of 2026-08-13:** every hard prerequisite is on `main` (P2, P3, P4, P5, P6 merged;
P7 merged except authorization-blocked T7.1). Nothing in P8 is waiting on another phase.

---

## Execution protocol (binding — read before claiming anything)

P8 does **not** use the wide worktree swarm that P2–P7 used. It runs **serial, on one
branch, with a fixed multi-agent review loop per task.** This was decided 2026-08-13
because P8 is the convergence point: its tasks share `src/main.rs` and `src/cli/`, and
integration bugs here are the ones that sink the demo.

### Branch

One branch for the whole phase: **`phase/p8-surface`**, cut from `main`. No task worktrees,
no task branches. Merge `phase/p8-surface → main` when the phase exit criteria are met.

### The loop, per task

Each task runs this cycle. **Every step is a separate agent invocation, and there is a hard
stop after each one** — the orchestrator commits and waits for a human go-ahead before
spawning the next agent. No agent runs two roles. The orchestrator does not implement,
review, or remediate; it briefs, commits, and gates.

```text
  ┌─────────────────────────────────────────────────────────────┐
  │  1. TASK AGENT          implements the task                 │
  │       ↓ orchestrator commits          ↓ HARD STOP           │
  │  2. ADVERSARIAL REVIEW AGENT   finds defects, writes report │
  │       ↓ orchestrator commits          ↓ HARD STOP           │
  │  3. REMEDIATION AGENT   fixes the findings                  │
  │       ↓ orchestrator commits          ↓ HARD STOP           │
  │  4. REVIEW AGENT        re-reviews the remediation          │
  │       ↓ orchestrator commits          ↓ HARD STOP           │
  │       └── not clean? → back to 3.  clean? → next task.      │
  └─────────────────────────────────────────────────────────────┘
```

| Step | Role | Writes to | Must NOT |
|---|---|---|---|
| 1 | **Task agent** | the task's `owns` paths | review its own work; skip the Handoff Log |
| 2 | **Adversarial review agent** | `dev-diary/adversarial-review/adve-review-t8.N-<slug>.md` | fix anything it finds — findings only |
| 3 | **Remediation agent** | the task's `owns` paths + the review file (mark each finding) | invent new scope; silently reject a finding without recording why |
| 4 | **Review agent** | the review file (verdict) | rewrite code — verdict only |

**Termination:** repeat 3 → 4 until the review agent returns **CLEAN**. A review that returns
findings goes back to remediation. Record every round in the review file (R1, R2, R3 …), the
same way P6 did — see `adve-review-p6-canonization-fable.md` for the shape to copy.

**Finding severities:** P1 = must fix before the task is done. P2 = fix unless it forces new
scope; if deferred, name the task that inherits it. P3 = record, fix only if cheap.

**Commit discipline:** one commit per agent, message prefixed with the step —
`feat(P8):` / `adve-review:` / `fix(P8):` / `docs(P8):`. The orchestrator commits; agents do
not commit. This keeps the phase's history readable as the audit trail it is.

### Why serial (do not "optimize" this back to parallel)

T8.1 is a hard serial gate — nothing else can compile without `Memory`. After it, T8.2/T8.3/
T8.5 are *nominally* parallel but all three write dispatch arms into `src/main.rs`, and T8.4
lives inside T8.3's directory. Running them wide buys hours and costs merge conflicts in the
one file that must work for the demo. Serial it is.

---

## Shared-file rules for P8 (the `owns` collisions, resolved)

The original phase doc had three tasks claiming overlapping paths, which violates the
`dev-diary/README.md` rule that no two tasks may own the same path. Resolved 2026-08-13:

**`src/cli/` is split by file, not claimed wholesale.**

| Path | Owner |
|---|---|
| `src/cli/mod.rs` | T8.3 (module decls; later tasks append their `pub mod` line only) |
| `src/cli/recall.rs`, `saints.rs`, `inspect.rs`, `stats.rs`, `provision.rs` | T8.3 |
| `src/cli/demo.rs` | **T8.4** — not T8.3 |
| `src/cli/serve_web.rs` | **T8.5** — not T8.3 |

**`src/main.rs` is a shared file with a primary owner.** T8.2 owns it (serve flags + wiring).
T8.3, T8.4, and T8.5 may **append their own dispatch arm** to the existing `match` and add
their subcommand's flags — nothing else. Any other edit to `main.rs` goes in the Handoff Log
and gets flagged. Serial execution means this never races; the rule exists so the record is
honest and so a future parallel run does not break.

**Cross-phase path authorizations (approved 2026-08-13).** P8 tasks may write these files
outside their own phase's paths. Each use MUST be named in the Handoff Log:

| Task | May also write | Strictly limited to |
|---|---|---|
| T8.1 | `src/store/flush.rs` (P3 path) | adding the `Notify` stop channel + `stop()`; nothing else |
| T8.1 | `src/graph/graph.rs` (P2 path) | adding the push-front-to-log helper; nothing else |
| any | `Cargo.toml`, `Cargo.lock`, `src/lib.rs` | additive only, announced in the Handoff Log (standing rule) |

---

## What already exists (do not re-derive this — verified 2026-08-13)

Every piece below is on `main`, compiles, and is covered by tests. `cargo test` on default
features is green: 507 lib + 5 integration passing, 3 ignored (live-infra gated).

| Concern | Call it like this | Notes |
|---|---|---|
| Startup load | `store::load_session(&*store, &session) -> Result<LoadedSession, StoreError>` | `LoadedSession { graph: Graph, index: InvertedIndex }`. A missing session is **not** an error — returns an empty graph + empty index |
| Graph mutations | `graph::derive(&mut Graph, interaction: NodeId, &AgentId, &[(&str, ConceptType)], &ParentOf, max_cooccurrence) -> Result<DeriveOutcome, _>` | **sync** |
| | `graph::record_action(&mut Graph, …, Action)` , `graph::demote(&mut Graph, interaction, &AgentId, chunk, chunk_group_id)` | **sync** |
| | `graph::reserve(…)`, `graph::release(&mut Graph, node, &AgentId)` | **sync** |
| Hybrid derive | `graph::hybrid::derive(Arc<RwLock<Graph>>, &dyn GraphStore, &dyn Embedder, &EmbeddingContract, interaction, &AgentId, concepts, &ParentOf, max_cooccurrence, semantic_match_threshold)` | **async** — see the sync/async ruling in T8.1 |
| Daemon | `Daemon::from_config(Arc<RwLock<Graph>>, &Config)` → `.with_index(Arc<RwLock<InvertedIndex>>)` → `.spawn()` | also `events()`, `event_sender()`, `score_table()`, `hot_list()`, `wake()`, `cycles()` |
| Recall | `daemon.recall(&SessionId, RecallQuery, &dyn GraphStore, Option<&[f32]>, RecallWeights, &mut RecallCache<RecallPipeline>) -> RecallResult` | **async**. `Memory` must own the cache |
| Flush | `FlushTask::new(Arc<RwLock<Graph>>, Arc<dyn GraphStore>, FlushParams)` → `.spawn()` | `stats() -> FlushStats { lag, depth, .. }`, `degraded() -> bool` |
| Canonization | `CanonizationTask::from_daemon(Arc<RwLock<Graph>>, Arc<dyn GraphStore>, &Daemon, &Config)` → `.spawn()` | shares the daemon's score table + event sender. `spawn()` **panics if called twice** |
| Level B resolve | `resolve_from_config_path(Option<&Path>) -> Result<ResolvedBackends, _>` | `ResolvedBackends { store, embedder, store_cfg, embedder_cfg, embedding }` |
| Contract check | `assert_session_embedding_compatible(...)` | the model-mixing refusal |
| Blast radius | `store.blast_radius(&SessionId, node, min_edge_age: Duration, now: DateTime<Utc>) -> Result<u64, _>` | **async**; the substrate for `retract` |

`Config` already carries every knob P8 needs: `canonization_edge_min_age`,
`canonization_eval_interval`, `canonization_eval_batch_size`, `semantic_match_threshold`,
`max_cooccurrence_per_derive`, `default_top_k` / `_max_tokens` / `_traversal_depth`,
`match_strategy`, plus all flush and daemon timings. Do not add new knobs without a
Handoff Log entry.

`axum` 0.8 is **already** a dependency (T8.5 needs no Cargo change). `rmcp` is **not** —
see T8.2.

---

## Four things P8 must BUILD, not wire (survey 2026-08-13)

The task descriptions below read like pure assembly. These four are not. They were found by
grepping the tree, not by reading docs — budget for them.

1. **`retract(_, DryRun)` does not exist.** Zero hits for `retract` or `DryRun` anywhere in
   `src/`. Spec §6.1 lists it; T8.1 owns building it. The substrate — `GraphStore::blast_radius`
   — does exist. **Ruled 2026-08-13: this is inside T8.1**, not split out, because splitting
   it complicates T8.3.
2. **`canonical_memories()` (the "saints" list) does not exist.** No function, and no store
   query for it either — only a `CanonizationStatus` field per concept. T8.1 builds it;
   `lambo saints` (T8.3) and `lambo_saints` (T8.2) both depend on it. **Also ruled inside T8.1.**
3. **`close()`'s drain needs code outside `src/memory.rs`.** `FlushTask` today exposes only
   `new / spawn / stats / degraded` — there is no stop mechanism at all. The full design is
   in T8.1 below; it requires the two cross-phase authorizations granted above.
4. **`Memory` must manually mirror the inverted index.** This is a written contract at
   `src/graph/mod.rs:42`: the graph is index-free by design, and **the session owner MUST**
   call `index.add` on every concept create — including creations inside `derive`,
   `record_action`, **and `demote`** — and `index.remove` on `remove_node`. A forgotten
   mirror is *silent* staleness: recall returns stale keyword candidates and nothing
   crashes. This is the single most likely integration bug in P8. The contract is tested by
   `tests/p2_integration.rs::inverted_index_manual_sync_contract` — read that test before
   writing `Memory`.

---

**Level B:** process start uses **`resolve_from_config_path` / `resolve_backends`** once
(spec §3.4, `notes/level-b-pluggability.md`) and hands **`ResolvedBackends`** into the
command. Serve and CLI never hard-code `CockroachStore::connect`, never rebuild store/
embedder with a second config pass, and stamp/check `EmbeddingContract` on session attach.

**P6 review carryover (adve-review-p6-canonization-fable.md, CLOSED 2026-08-13) — P8-owned
checklist items:**
- **F18 → T8.2:** the MCP layer MUST stamp `created_at` server-side. Interactions/edges
  inherit caller-supplied timestamps end-to-end today; if `lambo_record_action` accepts a
  client timestamp, backdating by 61s makes the entire `canonization_edge_min_age`
  inflation guard a no-op.
- **R3-1 → T8.4:** `seed()` on SQLite/Cockroach routes through the single-writer concept
  upsert and does NOT restore canonization state over an existing session (MemoryStore
  does — divergence). Seed fixtures only into fresh sessions, or reset the session first;
  do not re-seed a live demo session and expect canonization state to follow.
- **F13/R3-4 scale note → T8.2/T8.4:** each canonization cycle issues up to
  `canonization_eval_batch_size` (50) sequential structural queries (`interaction_span`/
  `blast_radius`) against the live store every 60s. Fine for the demo; budget for it when
  sizing the cluster/session.

---

### T8.1 — `Memory` builder & assembly ★
```yaml
requires:   T2.3, T2.4, T2.5, T3.4, T3.5, T4.1, T4.6, T5.3, T6.4, T1.5   # T6.4: build() wires CanonizationTask
fixture-ok: yes   # assembles against MemoryStore first
owns:       src/memory.rs
also-writes: src/store/flush.rs (stop channel ONLY), src/graph/graph.rs (push-front helper ONLY)
status:     not-started
flow:       serial; task → adve-review → remediation → review (repeat to CLEAN); hard stop after each agent
```
The spec §6.1 surface, exactly: builder (`session`, `agent`, `store`, `embedder`,
`match_strategy`, `flush_interval`, `scoring_weights`) → `build()` wires graph + daemon +
flush task + **canonization task** + startup load. **Canonization wiring (P6 review
2026-08-13):** construct `canon::CanonizationTask::from_daemon` alongside `FlushTask` —
it shares the daemon's graph handle, `EventSender`, and `score_table()`, and consumes
`canonization_eval_interval` (60s default). Without it no node ever transitions and T8.4
step 2 is impossible. Methods: `set_root_goal`, `declare_synonym`, `recall`, `derive`,
`record_action`, `demote`, `retract(_, DryRun)` (dry-run = blast-radius report, no
mutation), `reserve`, `canonical_memories`, `stats` (must expose flush lag + log depth),
`events`, `close` (final flush, clean shutdown of all three tasks — daemon, flush,
canonization; stop the canonization task **before** the final-flush drain so no new
mutations land after the drain — `JoinHandle::abort()` is documented safe for it: no
guard is live across its awaits and the write-behind log carries any hop whose phase-4
record was cancelled). Cut list stays cut: no
`correct`, `merge_concepts`, `resume`, `restart_daemon`, `checkpoint`.

**Build the two missing methods (see §Four things above) — they are IN SCOPE for T8.1:**
- **`retract(&self, target, DryRun) -> ImpactReport`** — build on
  `GraphStore::blast_radius`. `DryRun::Yes` reports and mutates nothing; that is the
  spec §13 blast-radius story and it is on the never-cut list.
- **`canonical_memories(&self) -> Vec<…>`** — scan the graph for
  `CanonizationStatus::Canonical`. No store query exists for this and none is required.

**Mirror the inverted index on every mutation** (contract at `src/graph/mod.rs:42`,
tested by `tests/p2_integration.rs::inverted_index_manual_sync_contract`). `Memory` holds
the `Arc<RwLock<InvertedIndex>>` that `load_session` returned and that the daemon got via
`with_index`. Forgetting this is silent recall staleness.

**Sync/async ruling (2026-08-13): `Memory::derive` is `async`.** `graph::derive` is sync but
`graph::hybrid::derive` is async, and hybrid is P7's headline capability. One async shape for
both, dispatched on `match_strategy`, beats two divergent signatures. The spec §6.1 snippet
shows `mem.derive(...)?` without `.await`; the doc-test is already inside an async block for
`build().await`, so adding `.await` is a doc-test edit, not a spec violation. Note it in the
Handoff Log.

**`close()` final-flush drain (COH-6, 2026-08-12) — P8-owned, hand-rolled:** spec §6.1
`close()` requires a final flush in v0.1, but `FlushTask` exposes only
`new/spawn/stats/degraded` (no drain API; COH-6 adds only a stop signal, below); the
opus46-S1 "shutdown drain = v0.7.0" deferral was closed as unsound and lands here.
Implement the drain inside `Memory::close` — do NOT add a drain API to `FlushTask`:
`FlushLoop.pending` (`src/store/flush.rs` ~285) is task-owned, so a hard
`JoinHandle::abort()` would drop not-yet-durable mutations — most importantly a batch
RETAINED after a failed flush (retained batches sit at the front, flush.rs ~283).
**Stop mechanism: a `tokio::sync::Notify` stop channel.** One `Arc<Notify>` on
`FlushTask` (in `Shared`, cloned into the loop); `FlushTask::stop()` = `notify_one()`.
The loop's `select!` (flush.rs ~301-304) gains a third branch, FIRST and
`biased;`: `tokio::select! { biased; _ = stop.notified() => { self.requeue_pending();
break; }, _ = interval.tick() => ..., _ = sleep(POLL_QUANTUM) => ... }`. Chosen over an
`AtomicBool` poll because the `select!` already awaits futures — `notified()` is a
native branch (no `POLL_QUANTUM` coupling, no extra sleep) — and `Notify` latches: a
`notify_one()` during an in-flight `cycle()` stores a permit, so the current flush
and its retry/backoff awaits run to completion and the loop breaks on the next
`select!` poll. **The `biased;` + stop-first ordering is REQUIRED:** an unbiased
`select!` polls all branches in random-start order, so a concurrently-ready
`interval.tick()` can be polled first, consume-and-drop the stored permit, and the
stop is lost forever — `close()`'s `join_handle.await` would hang (the tick is
concurrently ready whenever an in-flight flush outlasts `backend_flush_interval`,
which is the normal slow-flush shutdown case). With `biased;`, polling is in
written order and a ready stop is selected before the tick is polled.
1. `stop.notify_one()` — the loop finishes its current `cycle()` (any in-flight flush
   and retry/backoff completes; a post-retry `RETAINED_BACKOFF` hold is NOT waited
   out), then re-appends whatever is still in `self.pending` to the FRONT of the
   graph log (a small push-front helper on the log — chronological order preserved;
   NOT a FlushTask drain API), and the task exits.
2. `join_handle.await` — the task is gone; it can no longer re-take the graph lock.
3. Take the graph lock, `drain_log()` the remaining mutations, release the lock.
4. Call `store.flush(&batch)` directly on the drained batch (`.await` — no lock held),
   and surface the result as `close()`'s error.
A **retained post-retry batch is flushed or surfaced by this path**: it is still in
`self.pending` (already drained from the log, invisible to `close()`'s later
`drain_log()`), and the step-1 re-append is what puts it back where that drain can see
it — a hard abort would drop it with the task. The final attempt either flushes it or
surfaces the failure as `close()`'s error; it is never silently lost. `close()` then
satisfies "final flush + clean shutdown of both tasks"; the doc-test must assert the
tail is durable after `close()` (it holds whenever the store accepts the final flush —
including the retained-batch case). This is P8-owned scope, not T3.4's.

**Level B:** builder accepts `ResolvedBackends` (or `Box<dyn GraphStore>` +
`Box<dyn Embedder>` + `EmbeddingContract` from that resolve). Prefer
`resolve_backends(LamboFile)` over raw `build_*`. On `load_session`, if
`snap.embedding` is set, call `assert_session_embedding_compatible`.

**Owner (STORE-1, 2026-08-12):** contract enforcement on attach is **T8.1-owned** — the
`assert_session_embedding_compatible` check above (kind/model/dim vs the live
`EmbeddingContract`) is the second half of the model-mixing refusal; the persistence
half (seed write path + `load_session` materialization) shipped in Wave 5. If the check
is missing at P8 time, T8.1 must build it — it is not optional.

**Done when:** a doc-test mirroring the spec §6.1 snippet compiles and runs against
`MemoryStore` (default features), `close()` flushes the tail, session attach rejects
embedder kind/model/dim mismatches, and `retract` + `canonical_memories` exist with tests.

---

### T8.2 — MCP server ★
```yaml
requires:   T8.1
fixture-ok: yes
owns:       src/mcp/, src/main.rs (primary owner — see shared-file rules)
status:     not-started
flow:       serial; task → adve-review → remediation → review (repeat to CLEAN); hard stop after each agent
```
`lambo serve --session S --transport stdio|http [--port 7700] [--config PATH]` via `rmcp`;
**fallback authorized by spec §6.3: hand-rolled stdio JSON-RPC if rmcp fights — timebox the
fight to half a day.** Tools: `lambo_recall`, `lambo_derive`, `lambo_record_action`,
`lambo_reserve`, `lambo_inspect`, `lambo_saints`, `lambo_stats`. One process owns the
session (spec §2.2); tool calls from multiple MCP clients are tasks inside it, each
carrying `agent_id`.

**rmcp version ruling (2026-08-13 — supersedes the COH-2 "0.1.x vs v3" framing, which is
stale; 0.1.x is long gone).** Researched against crates.io and the SDK's release notes:

| | rmcp 2.2.0 | **rmcp 3.1.2 (chosen)** |
|---|---|---|
| Published | 2026-07-08 | 2026-08-07 |
| Downloads | 965k | 52k (3.x line ~310k) |
| MSRV | unset | 1.88 — repo is on 1.97.1 ✓ |
| Protocol `LATEST` | 2025-11-25 | **2025-11-25** (2026-07-28 is opt-in) |

Use exactly this, and do **not** take default features:
```toml
rmcp = { version = "3.1.2", default-features = false, features = [
  "server", "macros", "transport-io", "transport-streamable-http-server" ] }
```
- **Why 3.x is safe:** its `ProtocolVersion::LATEST` is still `V_2025_11_25`. The
  2026-07-28 sessionless-lifecycle rewrite that dominates the 3.0 release notes is
  **opt-in**, so 3.x negotiates with Claude Code exactly as 2.x does.
- **The one 3.0 break that touches us:** `ServerHandler::call_tool` / `get_prompt` /
  `read_resource` now return MRTR-aware enums, and exhaustive `ServerResult` matches must
  handle `InputRequiredResult`. Behind the `#[tool_router]` / `#[tool]` macros this is
  largely hidden; in a hand-written `ServerHandler` it is not. Prefer the macros.
- **Why `default-features = false` is REQUIRED, not style:** rmcp's optional `reqwest`
  dependency is `^0.13.2` and this repo pins `reqwest 0.12` for BGE-M3. Pulling any
  reqwest-flavoured rmcp feature compiles reqwest **twice**. The four features above were
  traced at tag `rmcp-v3.1.2` and none reach `reqwest`: `server` → transport-async-rw +
  schemars + pastey + uuid; `transport-io` → transport-async-rw + tokio/io-std;
  `transport-streamable-http-server` → server-side-http (tower/http/sse-stream/bytes).
- **Fallback ladder, in order:** if the tool-router macro path will not compile against a
  `Memory` handle within ~2 hours → drop to `rmcp 2.2.0` (feature names are identical, so
  it is a one-line Cargo edit) → if that also fights, hand-roll stdio JSON-RPC per §6.3.
- **Caveat:** the above is metadata + release-note analysis; nothing has been compiled
  against rmcp yet. **Validate the macro shape on a trivial tool BEFORE writing all seven
  tools on top of it.**

**F18 (P6 carryover) — server-side timestamps.** Every tool that creates an interaction MUST
stamp `created_at` on the server. `derive` / `record_action` / `demote` all take their
logical timestamp from the *interaction node's* `created_at`, so a client-supplied timestamp
propagates to every concept and edge below it — and backdating by 61s neuters the whole
`canonization_edge_min_age` inflation guard. Do not accept a client timestamp.

**Level B:** on start, `resolve_from_config_path` → **`ResolvedBackends`** → inject into
`Memory` (single construction). Fail closed if kinds are uncompiled, TOML has unknown keys,
or store×embedder dims disagree. Document demo features (`--features demo`).

**Done when:** `lambo serve` pasted into a Claude Code MCP config works — recall through a
real client returns the T5.3 context block. Config + resolve proven in `dev-diary/evidence/`.

---

### T8.3 — CLI subcommands
```yaml
requires:   T8.1
fixture-ok: yes
owns:       src/cli/mod.rs, src/cli/recall.rs, src/cli/saints.rs, src/cli/inspect.rs,
            src/cli/stats.rs, src/cli/provision.rs
not-owned:  src/cli/demo.rs (T8.4), src/cli/serve_web.rs (T8.5)   # collision fixed 2026-08-13
appends-to: src/main.rs (dispatch arms + own flags only; T8.2 is primary owner)
status:     not-started
flow:       serial; task → adve-review → remediation → review (repeat to CLEAN); hard stop after each agent
```
Spec §6.2: `recall --session --query --top-k`, `saints --session`,
`inspect --session --focus --depth`, `stats --session`, `provision` (wraps
`scripts/provision.sh`). `demo --scenario rest-api` belongs to **T8.4**, not here.
Global/shared `--config` where a store is needed. Read-only commands go straight to the
store as reader processes (spec §2.2) — they must not spin up a writer against a session
another process owns.

`saints` consumes `Memory::canonical_memories` from T8.1 — if it is missing, stop and fix
T8.1 rather than reimplementing the scan here.

**Level B:** reader CLIs use `build_store` from resolved config (sqlite or cockroach under
the matching feature). Do not open a second writer.

**Done when:** each subcommand runs against a SQLite session (`--features store-sqlite`);
`saints` and `stats` also verified against the live cluster (`store-cockroach`).

---

### T8.4 — Two-agent demo scenario ★★ (the video's script)
```yaml
requires:   T8.2, T6.4, T4.3   # live store strongly preferred: T3.2, T3.6
fixture-ok: partial   # logic testable on MemoryStore; the artifact must run live
owns:       src/cli/demo.rs, demo/
appends-to: src/main.rs (demo dispatch arm only)
status:     not-started
flow:       serial; task → adve-review → remediation → review (repeat to CLEAN); hard stop after each agent
```
Spec §13, scripted and **deterministic** — a demo that works 3 times in 5 is not done:

1. Agent A derives `user schema` / `auth middleware` / `session store`, records actions
   across ~12 interactions (compressed clock or config-shortened
   `canonization_edge_min_age` — document the knob; do not fake transitions).
2. `user schema` progresses Candidate → Venerable → Canonical; `canonization_events` gets
   each row.
3. Agent B calls `recall("update user schema")` → context block with
   `user schema [Entity, canonical]`, the ⚑ 9-nodes warning, and the 11-seconds-ago
   conflict line.
4. Split screen: Claude Code queries `canonization_events` via **CockroachDB's managed MCP
   server** (read-only — the spec §2.2 reader story made concrete; needs console-side
   setup, do it early, it's an external dependency).

**R3-1 (P6 carryover):** `seed()` on SQLite/Cockroach does NOT restore canonization state
over an existing session (MemoryStore does). Seed fixtures only into a **fresh** session, or
reset the session first. Re-seeding a live demo session and expecting canonization state to
follow will silently produce a demo that does not transition.

**External dependency — start now, not at T8.4.** The CockroachDB managed MCP server needs
console-side setup and is outside our control. So is the index-favorable `EXPLAIN`
camera-proof carried over from T7.3 (`vector_explain_camera_proof` with
`LAMBO_REQUIRE_VECTOR_INDEX=1` on a vector-search-favorable deployment). Neither is started.

**Done when:** `cargo run --features demo -- demo --scenario rest-api` (or equivalent)
runs end-to-end against the live cluster twice consecutively with identical outcomes, and
the MCP-server split-screen query is rehearsed and screenshotted into `dev-diary/evidence/`.

---

### T8.5 — Demo app (hosted client)
```yaml
requires:   T8.1        # http transport from T8.2 when it lands
fixture-ok: yes
owns:       web/, src/cli/serve_web.rs
appends-to: src/main.rs (serve-web dispatch arm only, if any)
status:     not-started
flow:       serial; task → adve-review → remediation → review (repeat to CLEAN); hard stop after each agent
```
The "functional demo app URL" deliverable (spec §12.4). Minimal axum-served page over the
http transport: session view, live recall box showing the context block verbatim,
canonization event feed, stats (flush lag / log depth). No framework ceremony — this is a
window onto T5.3's text and T6.4's feed, not a product. Deployment target decided in P9
(any public URL satisfies the judges).

`axum` 0.8 is **already** in `Cargo.toml` — no dependency change needed.

**Done when:** a browser against `lambo serve --transport http` shows a live recall and the
event feed updating during the demo scenario.

---

## Exit criteria

- [ ] Spec §6.1 doc-test green (Level B `resolve_backends`); §6.2 commands all exist
- [ ] `retract(_, DryRun)` and `canonical_memories()` exist and are tested (T8.1 build items)
- [ ] Inverted-index mirroring holds for `derive` / `record_action` / `demote` / removal
- [ ] `serve` / CLI use **one** `ResolvedBackends` (no double construction); fail closed
- [ ] MCP flow proven from a real Claude Code config
- [ ] MCP tools stamp `created_at` server-side (F18)
- [ ] Demo scenario deterministic ×2 on live infra under `--features demo`, evidence captured
- [ ] Demo app reachable and honest (renders real recall output, not canned text)
- [ ] Every task reached a CLEAN review verdict; all review files closed in
      `dev-diary/adversarial-review/`

---

## Handoff Log

> _Fill on completion._

### P8 setup (2026-08-13) — orchestration decisions, before any task started
- **Serial execution on one branch** (`phase/p8-surface`), not the P2–P7 worktree swarm.
  Per-task agent loop with a hard stop after every agent: task → adversarial review →
  remediation → review, repeating remediation/review until CLEAN. Orchestrator commits;
  agents do not. Rationale in §Execution protocol.
- **`owns` collisions fixed.** T8.3 previously claimed `src/cli/` wholesale while T8.4 and
  T8.5 claimed files inside it. `src/cli/` is now split file-by-file; `src/main.rs` has T8.2
  as primary owner with append-only dispatch arms for the others.
- **Cross-phase writes authorized for T8.1:** `src/store/flush.rs` (stop channel only) and
  `src/graph/graph.rs` (push-front log helper only), both required by the `close()` drain.
- **Survey findings recorded** (§Four things P8 must BUILD): `retract`/`DryRun` and
  `canonical_memories` do not exist anywhere in `src/` and are ruled **inside T8.1**;
  `FlushTask` has no stop mechanism; the inverted index must be mirrored by hand.
- **`Memory::derive` is async** (hybrid derive is async; one shape for both).
- **rmcp 3.1.2 chosen** with `default-features = false` and four server-side features —
  the reqwest 0.12/0.13 duplication trap and the fallback ladder are documented in T8.2.

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
status:     done
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
status:     done
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
console-side setup and is outside our control. **Status 2026-08-13: console-side setup is
DONE; the config may only take effect after a client restart, and the split-screen query
still needs rehearsing.**

**The `EXPLAIN` camera-proof is no longer an "external dependency" — it moved to
[T7.4](PHASE-7-embeddings.md).** Root-caused 2026-08-13: it never failed for
cost/deployment reasons. The test asserts `"vector search"` against `EXPLAIN (OPT,
VERBOSE)`, which emits `"vector-search"`, so it could not pass on any cluster; and the
query's `WHERE embedding IS NOT NULL` defeats a non-partial vector index (a partial index
on the canonical name fixes that with no query change). **Note for anyone running live
demos before T7.4 lands: the cluster schema currently DIVERGES from
`migrations/cockroach/001_init.sql`** — a hand-created `concepts_embedding_nonnull_idx`
exists on it — and the cluster was seeded to 2833 concepts / 2004 distinct vectors.

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

### T8.1 — `Memory` builder & assembly (task agent, 2026-08-13)

**Gates:** `cargo fmt --all -- --check` clean, `cargo clippy --all-targets -- -D warnings`
clean, `cargo test` **528 lib + 5 integration + 1 doc-test passing, 3 ignored** (baseline was
507 + 5, 3 ignored — +21 lib, +1 doc-test, no regressions). `--no-default-features` and
`--no-default-features --features store-memory,embed-fixture` both compile.

#### Files touched outside `src/memory.rs` — every one, with the reason

| File | Change | Authorization |
|---|---|---|
| `src/store/flush.rs` | `Shared.stop: Arc<tokio::sync::Notify>`; `FlushTask::stop()`; the loop's `select!` gained `biased;` + a stop branch FIRST; new private `FlushLoop::requeue_pending()`; module-doc "no shutdown signal" paragraph replaced by a "Shutdown (COH-6)" one | cross-phase grant, stop channel only. **Nothing else in the file was touched** — no refactor, no behaviour change to any existing path |
| `src/graph/graph.rs` | added `Graph::push_front_log(Vec<Mutation>)` next to `drain_log` | cross-phase grant, push-front helper only |
| `src/lib.rs` | `pub mod memory;` + `pub use memory::{CanonicalMemory, DryRun, ImpactReport, Memory, MemoryBuilder, MemoryStats};` | standing additive rule |
| `Cargo.toml` | **not touched** — no new dependency was needed | — |

#### What exists now

- **`Memory` + `MemoryBuilder`** (`src/memory.rs`). `build().await` does startup load →
  Level B contract check → daemon → flush task → **canonization task**, all three spawned
  exactly once. Methods: `set_root_goal`, `declare_synonym`, `recall`, `derive`,
  `record_action`, `demote`, `retract`, `reserve`, `release`, `canonical_memories`, `stats`,
  `events`, `close`. Cut list stayed cut.
- **`retract(target, DryRun) -> ImpactReport`** — built from scratch. Resolves the target
  through canonicalization (synonyms/casing work) with an exact-`content` fallback so
  demoted `Observation`s are reachable. `DryRun::Yes` provably mutates nothing (test asserts
  node count, edge count, **epoch** and index all unchanged).
- **`canonical_memories() -> Vec<CanonicalMemory>`** — built from scratch; graph scan for
  `CanonizationStatus::Canonical`, totally ordered (blast radius desc, then created_at, then
  id) so `lambo saints` is stable across runs.
- **`close()`** — the COH-6 drain, implemented exactly as specified: stop canonization →
  stop daemon → `FlushTask::stop()` → join → `drain_log()` → `store.flush()`.

#### What the next agent should NOT re-derive

- **Lock order is `graph → index → hot`.** Set by `daemon::run_loop`'s GC sync. `Memory`
  follows it in `mirror_concepts` and `retract`. Taking index-then-graph anywhere will
  deadlock against a concurrent GC.
- **`store::load::load_session_async` is the right call in async code, not
  `load_session`.** The sync wrapper parks a worker thread and `join()`s it, which blocks a
  runtime worker from inside an async fn. The phase doc's `store::load_session(...)` line is
  the sync API; `build()` uses the async core.
- **A fresh session's log starts at depth 1**, not 0: `build()` stamps the embedding
  contract and that stamp is a real `Mutation::SetEmbedding`. Any test asserting "log is
  empty after build" is wrong.
- **`Memory::stats()` must not read `FlushStats.depth` alone.** It only refreshes inside the
  flush loop's cycle, so between cycles it is a stale zero — this cost three test failures
  before it was found. `stats()` now reports `log_depth` (from `Graph::log_len`, always
  current) **and** `flush_depth` (the task's view) as separate fields.
- **`Daemon::events()` must be called before `Daemon::spawn()`** (CONC-3). `build()` does,
  and hands that pre-spawn receiver to the **first** `Memory::events()` caller so the warm-up
  condition set is not lost. T8.2 should call `events()` once at startup and keep it.
- **The `spawn()`-panics-if-called-twice rule is already satisfied**: each of the three tasks
  is spawned in exactly one place, inside `build()`. Do not add another `spawn` call.
- `MemoryStore` reports `Capabilities::empty()` — no `VECTOR_SEARCH`. Hybrid derive and the
  recall vector leg both degrade to keyword-only against it. That is why `Memory::recall`
  skips the embed call entirely unless the store claims the capability.

#### Where the phase doc met the real code

- **Ruled-async `derive` is implemented as ruled** (one async shape, dispatched on
  `match_strategy`). The doc-test carries the `.await`, as the ruling anticipated.
  `record_action` and `demote` were left **sync** — the ruling named only `derive`, and
  neither has an async twin or any I/O. Flagging in case the reviewer expected uniformity.
- **`retract`'s headline radius is the in-RAM count, not the store's** — a deliberate
  partial deviation from "build on `GraphStore::blast_radius`". The store lags the graph by
  up to one flush interval and answers `SessionNotFound` for a never-flushed session, which
  is exactly the demo's state. `ImpactReport` therefore carries **both**: `blast_radius`
  (in-RAM, always available, same source as recall's `⚑ N nodes` line) and
  `durable_blast_radius: Option<u64>` (the store query, `None` + a `warnings` entry when the
  store cannot answer). Tested both ways. **If the reviewer wants the store value as the
  headline, that is a one-line swap** — but a fresh session would then report 0.

#### Weak spots I am flagging myself

1. **`close()` waits on the flush join unboundedly** (P2). As designed by COH-6: the loop
   finishes its current `cycle()` first. Worst case that is
   `FLUSH_ATTEMPT_TIMEOUT (30s) × (retries + 1)` ≈ 2 minutes against a hung store. I
   implemented the doc's design rather than adding an unauthorized timeout, because a
   timeout + abort would drop exactly the batch the design exists to save. **If the demo
   needs a bounded `close()`, that is a real decision to make, not an oversight.**
2. **`stats()`'s "not yet durable" is a lower bound** (P2). `FlushLoop.pending` is
   task-owned with no accessor, so `log_depth.max(flush_depth)` undercounts while a retained
   batch and fresh writes coexist. Exact fix is `FlushTask::pending_len()` — deliberately
   not added, since the flush.rs grant covers the stop channel only. Documented on the
   `MemoryStats::flush_depth` field.
3. **`derive` is not atomic end-to-end.** `begin_interaction` and the concept write are two
   separate lock acquisitions, so another writer can interleave between them. This is safe
   (the write path references the interaction by id and re-validates it; hybrid re-plans on
   epoch change), but it is not serializable, and a `derive` that fails after the interaction
   was opened leaves an empty interaction in the append-only chain.
4. **`events()` is stateful on its first call** (returns the pre-spawn receiver, then fresh
   subscriptions). It is the only way to give the first consumer the warm-up set without
   changing the daemon, but it is surprising and worth a second opinion.
5. **`close()` takes `&self`, not `self`.** Required so `Drop` can act as the leak guard
   (aborting tasks when `close` was never called). `mem.close().await?` reads identically.
6. **Not exercised anywhere yet:** the degraded-session branch of `close()` (returns an
   error rather than silently skipping the final flush) has no test — reaching it needs a
   backlog past `backend_log_max`. The branch is small and reads correctly, but it is
   untested.

### T8.1 — remediation of the adversarial review (remediation agent, 2026-08-13)

All nine findings (`adve-review-t8.1-memory-fable.md`) plus the implementer's self-flag #6.
Gates: `cargo fmt --check` clean; `RUSTFLAGS="-D warnings" cargo check --all-targets` clean
on default / `store-sqlite` / `store-cockroach`; `cargo test` **539 lib + 5 integration + 1
doc, 0 failed, 3 ignored** (review baseline 528 + 5 + 1 — +11 lib, no regressions);
`--features store-sqlite` **579 lib** (baseline 568), 0 failed.

| Finding | Fix |
|---|---|
| T81-1 (P1) | **Writers gate.** New `Memory::writers: tokio::sync::RwLock<()>`. Every mutating method holds a READ permit for its whole body (awaits included) and re-checks `closed` after acquiring it; `close()` latches `closed` then takes the WRITE side **before** stopping tasks or draining. Sync methods use `try_read` (they never await, so the permit covers their whole mutation; a failed `try_read` means a close is in progress → closed error). Reads take no permit. |
| T81-2 (P2) | `close()`'s step-4 flush now reuses `FLUSH_ATTEMPT_TIMEOUT` (made `pub(crate)`) and `CatchUnwindPoll` + `panic_message` from the flush path. |
| T81-3 (P2) | Requeue chronology asserted as a **sequence** (retained batch first, in order; no edge upsert before its endpoints) + a direct `Graph::push_front_log` unit test. |
| T81-4 (P3) | Long-in-flight-flush + stop + join-with-timeout test on the paused clock. |
| T81-5 (P3) | A failed `close()` pushes the drained batch back to the log front and leaves the success flag unset: **close is retryable**, and no later `close()` returns `Ok` while the tail is undurable. Same for the degraded branch (which keeps returning its error). |
| T81-6 (P3) | The close body is serialized behind a `tokio::sync::Mutex<bool>` carrying that flag; a concurrent second caller awaits the first and returns its outcome. |
| T81-7 (P3) | Durability disclosure on `declare_synonym` and `reserve` (see the T8.2 warning below). |
| T81-8 (P3) | Process-global session registry that **reports, does not refuse** — an ERROR line naming both agents. Refusal was rejected: it would add a `build()` failure mode for legitimate re-attaches while still being blind to the collisions that matter (other processes/hosts), i.e. a false sense of protection over a policy spec §2.2 gives deployment. |
| T81-9 (P3) | Doc note: `ImpactReport` is measured before removal and can be stale vs a concurrent writer. |
| self-flag #6 | The degraded-`close()` branch now has a test. |

**Cross-phase writes in this pass** (both inside the existing T8.1 grants): `src/store/flush.rs`
— `FLUSH_ATTEMPT_TIMEOUT` visibility `const` → `pub(crate)`, no behaviour change, nothing else
touched; `src/graph/graph.rs` — one unit test for the granted `push_front_log` helper, no
production change.

**⚠ T8.2 must not assume synonym or reservation durability.** `declare_synonym` and `reserve`
are **RAM-local for one handle's lifetime**: pinned contract S5 gives them no `Mutation` kind,
so nothing — not the background flush, not `close()`'s final one — persists them, and
`load_session` cannot restore them. Consequences an MCP server will hit: after a restart the
same phrase that used to match an existing concept **creates a duplicate** instead, `retract`
loses the alias as a resolution route, and "no reservation" does not mean "nobody else is
working on this". A server that offers a synonym tool must re-declare its mappings on every
attach (right after `build()`, before the first `derive`) or persist them itself.

**`close()` semantics T8.2 should surface:** `Ok(())` means the tail is durable; an `Err` means
it is not *and the tail was kept* — call `close()` again once the store is healthy. A second
concurrent `close()` blocks until the first finishes rather than returning early, so a shutdown
path may call it from more than one place safely.

### T8.2 — MCP server (task agent, 2026-08-14)

**Gates:** `cargo fmt --all -- --check` clean; `cargo clippy --all-targets -- -D warnings`
clean; `cargo clippy --all-targets --features store-cockroach,store-memory,fixtures
-D warnings` clean; `cargo check --no-default-features` clean; `cargo test` **562 lib + 5
integration + 1 doc-test passing, 3 ignored** (baseline 548 + 5 + 1, 3 ignored — +14 lib,
no regressions, nothing removed).

#### rmcp rung: the TOP one. Nothing fought.

**`rmcp 3.1.2`, `default-features = false`, the four features exactly as ruled.** The macro
spike (one trivial tool, `#[tool_router(server_handler)]`) compiled **first try, inside 20
minutes** of the 2-hour timebox. No drop to 2.2.0, no hand-rolled JSON-RPC. Confirmed by
`Cargo.lock`: **`reqwest` resolves to a single `0.12.28`** — the duplication trap the ruling
exists to avoid did not materialize.

Three rmcp 3.x details the next agent should not re-derive:

1. **`#[tool_handler]`'s default router argument is `Self::tool_router()` — the *static*
   constructor.** Left at the default it **rebuilds the entire router, every tool's JSON
   schema included, on every `tools/list` and every `tools/call`**. `server.rs` uses
   `#[tool_handler(router = self.tool_router)]` so per-call work is a map lookup against the
   router built once in `new()`. The give-away is a `field 'tool_router' is never read`
   dead-code warning — if that warning ever reappears, the router went back to being rebuilt
   per call.
2. **`#[tool_router(server_handler)]` is fine for a tools-only server but generates the
   `ServerHandler` impl for you, so you cannot override `get_info`.** Since we need
   `get_info` (capabilities, server name, and the session-naming instructions), the explicit
   pair — `#[tool_router]` on the inherent impl + `#[tool_handler(...)] impl ServerHandler` —
   is required.
3. **`ServerInfo`, `Implementation` and `StreamableHttpServerConfig` are `#[non_exhaustive]`.**
   Struct-literal construction does not compile even with `..Default::default()`. Start from
   the SDK default and assign fields.

The 3.0 `InputRequiredResult` / MRTR break never surfaced: the macros hide it, exactly as the
ruling predicted.

#### What exists now

- **`src/mcp/server.rs`** — `LamboServer` and the seven spec §6.2 tools: `lambo_recall`,
  `lambo_derive`, `lambo_record_action`, `lambo_reserve`, `lambo_inspect`, `lambo_saints`,
  `lambo_stats`. Every tool takes a **required** `agent_id`. `lambo_recall` returns the T5.3
  context block as the **text** content (verbatim — it is the artifact the agent reads) with
  hits/warnings as structured content; the others return a human-readable summary plus
  structured data.
- **`src/mcp/serve.rs`** — `ServeOptions`, `Transport`, `build_memory`, `serve`. Both
  transports (stdio, streamable HTTP on `/mcp` via the axum already in the tree). `close()`
  runs on **every** exit path and its error is surfaced.
- **`src/mcp/mod.rs`** — `init_tracing()`, which pins diagnostics to **stderr**. Under stdio
  this is not cosmetic: stdout is the JSON-RPC channel and one stray log line breaks framing.
- **`src/main.rs`** — the real `serve` dispatch arm, plus new `--agent` and `--bind` flags;
  `--session` is now **required** (was `Option<String>`).

#### Files touched outside `owns`

| File | Change | Authorization |
|---|---|---|
| `Cargo.toml` | added `rmcp 3.1.2` (`default-features = false`, 4 features) and `schemars 1` | standing additive rule |
| `Cargo.lock` | resolved the above | standing additive rule |
| `dev-diary/evidence/t8.2-mcp-client/` | new evidence directory | required by the task's Done-when |
| `src/lib.rs` | **not touched** — `pub mod mcp;` already existed | — |

#### Level B — single construction site

`main.rs` performs the **one** `resolve_from_config_path` (in the pre-existing
`resolve_for_command`) and hands the single `ResolvedBackends` into `mcp::serve`.
`mcp::serve::build_memory` **takes `ResolvedBackends`, not a config path**, deliberately: a
second resolve is not expressible through the API. Fail-closed verified four ways (unknown
TOML key, uncompiled store kind, bad transport, missing `--session`) — all exit before any
session is attached. Captured in `dev-diary/evidence/t8.2-mcp-client/README.md`.

#### F18 — server-side timestamps

No tool accepts a timestamp. Beyond simply not adding the parameter, this is **pinned by a
test** — `f18_no_tool_schema_accepts_a_client_timestamp` walks every *published* tool schema
and fails on a property named `timestamp` / `created_at` / `now` / `time` / `when` / `date` /
`occurred_at` / `logical_time`. A future agent who adds a timestamp field to any params
struct breaks that test. The server's `initialize` instructions also tell the model "Never
send a timestamp: the server stamps them."

#### T8.1 constraints — how each is honoured

- **`events()` called exactly once, at startup**, in `serve()`, before either transport runs.
  Its receiver is drained by one long-lived task (which also stops the broadcast channel
  lagging the daemon).
- **No assumption of synonym or reservation durability.** No synonym tool is exposed at all.
  `lambo_reserve` attaches an explicit warning to every successful reservation saying it is
  advisory and lost on restart, and the rustdoc says so.
- **One `Memory` per session.** `LamboServer` holds `Arc<Memory>`; `Clone` clones the `Arc`.
  The HTTP service factory — called per request — clones the same `Arc`. There is exactly one
  `Memory::builder()` call in the serve path.
- **No graph lock across an `.await`.** The only lock `src/mcp/` takes directly is the
  `lambo_inspect` read guard, in a block containing no `.await`; `render_neighbourhood` is a
  sync free function.

#### Flagged finding — per-call `agent_id` cannot reach the graph (needs a `Memory` change)

**This is the one place the task could not be fully satisfied without touching
`src/memory.rs`, which the brief forbids. Reporting instead of reaching across.**

Spec §6.2/§2.2 require tool calls from several MCP clients to be tasks in one process, "each
carrying `agent_id`". The schema carries it — but **`Memory` binds a single `AgentId` at
`build()` and exposes no per-call override**: `derive`, `record_action`, `demote`, `reserve`
and `release` all pass `self.agent`. The *graph* layer already takes `&AgentId` per call
(`graph::derive(&mut Graph, interaction, &AgentId, …)`), so the gap is only in `Memory`'s
surface.

Consequences, concretely:
- Interactions written by "agent-b" are attributed to the process agent.
- **`lambo_reserve` cannot detect cross-agent contention through MCP.** Two clients holding
  different `agent_id`s are the same `AgentId` to `graph::reserve`, so the §11 conflict that
  should fire never does.
- The spec §13 / T8.4 demo's two-agent conflict line is affected: with one process and one
  `Memory`, Agent A and Agent B are indistinguishable to the graph.

The surface does **not** hide this: a call whose `agent_id` differs from the session owner
gets an explicit `attribution:` warning naming both identities (verified in the evidence).

**Suggested fix, for whoever owns it:** add agent-parameterised twins on `Memory`
(`derive_as(&AgentId, …)`, `record_action_as`, `reserve_as`, `release_as`) that pass the
caller's id straight through to the already-agent-taking graph functions, keeping the
existing methods as thin wrappers over `self.agent`. That is a `src/memory.rs` change and a
T8.1 re-open, not a T8.2 edit. **T8.4 should treat this as a blocker for the two-agent
conflict story.**

#### Second flagged finding — no schema bootstrap for a SQLite/Cockroach serve

`serve` against `store.kind = "sqlite"` on a fresh file fails at startup with
`no such table: sessions`. Nothing in the production path calls `GraphStore::init_schema()`
— it has no non-test caller anywhere in `src/`. Spec §6.2 assigns schema bootstrap to
`lambo provision`, which is **T8.3** and still a stub, so this is a real ordering dependency
rather than a T8.2 bug: I deliberately did not add auto-init to `serve`, because that would
preempt T8.3's ownership and duplicate the provision path. **T8.3 must make `lambo provision`
create the SQLite schema**, or `serve` must gain an explicit opt-in. The evidence run
therefore uses `store.kind = "memory"`.

#### Verification — what was and was not proven

**A real Claude Code client (2.1.226) handshakes with `lambo serve --transport stdio`:**
registered with `claude mcp add`, `claude mcp list` reports `✔ Connected`. That is a genuine
`initialize` over stdio by a real client, and it is the definitive proof the rmcp rung
interoperates. Registration was made in a scratch directory, never the repo, and removed
afterwards.

~~**All seven tools were driven end-to-end over the real MCP wire protocol** (`initialize` →
`lambo_derive` → `lambo_record_action` → `lambo_recall` → `lambo_stats`)~~ — **overclaimed;
corrected in R1 remediation (T82-8).** As originally captured, **four** tools were driven
(`lambo_derive`, `lambo_record_action`, `lambo_recall`, `lambo_stats`) and the transcript held
responses only. R1 remediation drove all seven over the real stdio wire with requests *and*
responses captured, in `dev-diary/evidence/t8.2-mcp-client/stdio-all-seven-tools.jsonl`; that
file, not this sentence, is what supports the seven-tool claim. `lambo_recall` returns the
T5.3 context block verbatim. HTTP transport verified with `curl POST /mcp`. Transcripts in
`dev-diary/evidence/t8.2-mcp-client/`.

**NOT verified: a model-driven tool call.** `claude -p` failed reproducibly with
`Failed to authenticate: OAuth session expired and could not be refreshed` — the nested CLI
never reached the model, so no tool call was attempted. This is an environment auth failure,
not an MCP one (the same client health-checks the server as connected). So it is proven that
the tools *work* through a real client, and **not** proven that a model *chooses* them
correctly from their descriptions. Re-run with working credentials; exact commands are in the
evidence README.

**Also honest about the context block:** the captured block shows scored concept lines but no
`canonical` marker, no `⚑ N nodes` warning and no conflict line — because a fresh session has
nothing canonized, no large blast radius and one writer. Those come from the same
`recall::format` path and are covered by T5.3's tests; showing them on the wire needs T8.4's
aged session.

#### My own weak spots (a reviewer will find these — flagging them first)

1. **Protocol-level dispatch is untested in-process.** The unit tests drive tool methods
   directly (through real `Parameters<T>` deserialization) rather than `ToolRouter::call`,
   because building a `RequestContext` needs a live `Peer`. Registration is pinned
   (`the_router_publishes_exactly_the_seven_spec_tools`) and a guard test fails if a published
   name has no harness arm, but the name→method wiring is only proven by the manual JSONL
   session. An in-process client over `tokio::io::duplex` would close this — it needs rmcp's
   `client` feature as a dev-dependency, which I did not add unilaterally given the reqwest
   trap the ruling warns about.
2. **`lambo_inspect`'s fallback resolution is mine, not the canonicalization pipeline's.** It
   tries UUID → exact content (case-insensitive) → *substring*. `Memory::retract` resolves
   through canonicalization with an exact-content fallback. The substring leg can pick a
   different concept than a user expects when several share a prefix, and it takes the first
   iteration-order match rather than the best one — non-deterministic across runs if two
   concepts both contain the needle.
3. **`MAX_*` limits are my invention.** No spec or config knob backs `MAX_TOP_K = 100`,
   `MAX_CONCEPTS_PER_DERIVE = 64`, `MAX_INSPECT_NODES = 200` etc. They exist because one
   client can otherwise stall the single process every other client shares. They are not
   configurable, which may be wrong.
4. **`serve()` aborts the event-pump task rather than joining it.** Bounded and harmless (it
   only logs), but it is an `abort()` on a shutdown path, which is the exact pattern the T8.1
   review spent three rounds tightening elsewhere.
5. **The HTTP transport is unauthenticated.** Mitigated by defaulting `--bind` to loopback,
   and documented on the flag — but `--bind 0.0.0.0` exposes a session *writer* with no
   authentication whatsoever. There is no rate limit either.
6. **A `close()` failure is logged and returned but the process still exits non-zero without
   retrying.** T8.1's semantics say a failed close keeps the tail and is retryable; `serve`
   does not retry. For a server shutting down that is arguably right, but it means a transient
   store outage at exit leaves the tail undurable with only a log line.
7. **`lambo_reserve` carries a `release` boolean rather than being two tools**, to keep the
   published set at exactly the seven names spec §6.2 lists. It is a mild API smell.

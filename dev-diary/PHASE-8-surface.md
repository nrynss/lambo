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

### Gates (binding — every agent, every step, no exceptions)

Run ALL of these before claiming done. This list matches CI's feature matrix — the two
`--no-default-features` rows were missing from agents' local runs on 2026-08-14 and the
resulting unused-import/dead-code `-D warnings` failures reached CI (run 31791918843).
The two `--no-run` rows now prefix `RUSTFLAGS="-D warnings"` so a feature-mismatched dead
import fails locally exactly as CI's global `RUSTFLAGS` catches it; `-- -D warnings` would
pass the flag to the test binary, not the compiler, so the env-var form is the correct one.
Do not restate a subset in a handoff entry; run the block.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings
cargo clippy --all-targets --features store-sqlite -- -D warnings
cargo test
cargo test --features store-sqlite
RUSTFLAGS="-D warnings" cargo test --no-default-features --features store-sqlite --no-run    # CI: sqlite-minimal
RUSTFLAGS="-D warnings" cargo test --no-default-features --features store-cockroach --no-run # CI: cockroach row
cargo check --no-default-features                                     # CI: minimal row
```

Anything `#[cfg(test)]`-gated whose only users live behind feature-gated test modules must
carry the SAME feature gate as its users, or the minimal rows fail on dead code.

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
| `src/cli/recall.rs`, `saints.rs`, `inspect.rs`, `stats.rs`, `provision.rs` (read verbs) | T8.3 |
| `src/cli/derive.rs`, `record_action.rs`, `reserve.rs` (write verbs, lease-held) | T8.3 |
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
real client returns the T5.3 context block. Config + resolve proven in `evidence/`.

---

### T8.3 — CLI subcommands (read + write parity)
```yaml
requires:   T8.1, T8.6      # T8.6 lease must land first — write verbs acquire it
fixture-ok: yes
owns:       src/cli/mod.rs,
            # read verbs (reader processes):
            src/cli/recall.rs, src/cli/saints.rs, src/cli/inspect.rs, src/cli/stats.rs,
            src/cli/provision.rs,
            # write verbs (lease-holding writers, decided 2026-08-14):
            src/cli/derive.rs, src/cli/record_action.rs, src/cli/reserve.rs
not-owned:  src/cli/demo.rs (T8.4), src/cli/serve_web.rs (T8.5)   # collision fixed 2026-08-13
appends-to: src/main.rs (dispatch arms + own flags only; T8.2 is primary owner)
status:     DONE/CLEAN — 2026-08-15 (main review CLEAN + R2/R3 reverify CLEAN at HEAD 596f40f;
            P3s T88-H6/H7/H10 remediated through R2/R3; see Handoff 1856-1878)
flow:       serial; task → adve-review → remediation → review (repeat to CLEAN); hard stop after each agent
```
**Read verbs (spec §6.2, reader processes):** `recall --session --query --top-k`,
`saints --session`, `inspect --session --focus --depth`, `stats --session`, `provision`
(wraps `scripts/provision.sh`). `demo --scenario rest-api` belongs to **T8.4**, not here.
Read-only commands go straight to the store as reader processes (spec §2.2) — they must not
spin up a writer against a session another process owns.

**Write verbs (mirror the MCP tools 1:1, lease-held):**
- `derive --session --agent --content --kind [--parent-of CHILD:PARENT ...]`
- `record-action --session --agent --action [--produces N ...] [--modifies N ...] [--depends-on N ...]`
- `reserve --session --agent --node` and `release --session --agent --node`

Same argument names, same `MAX_*` caps, same control-char / size validation as the MCP
surface (share the validators — do not re-implement them). Global/shared `--config` where a
store is needed.

**Why write verbs are first-class (decided 2026-08-14 — NOT a stretch).** Measured agent
behavior: MCP burns context on tool schemas and meanders over tool choice, while a CLI
invocation is one deterministic line. For small local models (the swarm story) the CLI is
the *primary* agent surface and MCP the compatibility surface. Non-negotiables:

- **Single-writer is enforced by the T8.6 writer lease** — a CLI write acquires the
  session lease, writes through the same `Memory` API the MCP tools use, releases. If the
  lease is held (a `serve` owns the session), the CLI write **fails closed with an honest
  error naming the holder** — it must never become a silent second writer. This is why
  T8.6 is a hard `requires`.
- **Both surfaces are thin adapters over one `Memory`** — no graph logic in either
  `src/cli/*` or `src/mcp/server.rs`, and the arg validators (caps, control-char, size)
  are shared, not duplicated. This is the parity that counts.
- **Differential test (in the Done bar):** the same op driven via CLI and via MCP yields
  identical results — same session state, same recall output.
- **Help text is authored here.** Every subcommand and flag carries clap help
  (`about`/`long_about` + per-arg help), phrased to match the corresponding MCP tool and
  argument descriptions so the two surfaces read consistently. A subcommand without help
  text is not done. (T8.8 later *verifies* this; it does not write it.)

`saints` consumes `Memory::canonical_memories` from T8.1 — if it is missing, stop and fix
T8.1 rather than reimplementing the scan here.

**Level B:** reader CLIs use `build_store` from resolved config (sqlite or cockroach under
the matching feature). Read verbs never open a writer; write verbs open exactly one via the
lease.

**Done when:** each subcommand runs against a SQLite session (`--features store-sqlite`);
`saints` and `stats` also verified against the live cluster (`store-cockroach`); write
verbs demonstrate lease acquire/refuse both ways (write succeeds with no serve running;
fails closed naming the holder while a serve owns the session); the CLI↔MCP differential
test passes.

---

### T8.6 — Single-writer lease (store-enforced §2.2)
```yaml
requires:   T8.2 (CLEAN — serve.rs must be stable before it takes the lease)
fixture-ok: yes (memory store lease is in-process; sqlite/cockroach are the real targets)
owns:       src/store/lease.rs; lease columns/DDL in src/store/{memory,sqlite,cockroach}.rs;
            scripts/provision.sh (schema addition)
appends-to: src/memory.rs (build acquires/refreshes lease), src/mcp/serve.rs (holder
            identity + release on close), src/store/mod.rs (trait method)
status:     CLEAN — re-verified 2026-08-15 at HEAD (adve-review-t8.6-lease-r2.md; R1 11 +
            R2/R3 T83-12 all closed, mutation-verified). T86R2-2 (live Cockroach lease leg)
            CLOSED 2026-08-15 by the live conformance 8/8 run — see Handoff 2014-2019, 2026-2027.
flow:       serial; task → adve-review → remediation → review (repeat to CLEAN); hard stop after each agent
```
Decided 2026-08-14: promote spec §2.2 single-writer from advisory (the process-local
`ACTIVE_SESSIONS` ERROR log, `src/memory.rs:237-266` — which cannot see other processes)
to **store-enforced**. A lease row per session: holder identity (agent + pid + host),
acquired_at, TTL with heartbeat refresh. Semantics:

- `Memory::build()` (writer mode) **acquires or fails closed** with an error naming the
  current holder and its age. `Memory::close()` releases. Heartbeat refreshes at some
  fraction of TTL; a crashed holder's lease expires rather than wedging the session.
- Acquisition must be atomic per backend (`INSERT ... ON CONFLICT` guarded by expiry
  check in one statement/transaction — no read-then-write race).
- Readers never touch the lease (T8.3 read verbs stay lease-free).
- The advisory in-process log stays — it catches the same-process case cheaply.
- **Clock discipline:** lease timestamps come from the store's clock or the holder's
  process clock per the existing timestamp rules — never from a client argument. The
  F18 golden-allowlist guard must not gain wire-visible lease fields.

Known risks to design against (carry into the adversarial review brief): TTL vs
`SHUTDOWN_GRACE` interaction (a graceful close must release, not expire); a wedged-but-
heartbeating process squatting the lease (document the operator override); crash-expiry
window where the tail was never flushed (the lease expiring does NOT imply the log was
drained — new holder must go through startup load, which already replays).

**Done when:** two concurrent writer opens on one session — across two *processes* —
deterministically yield one holder and one honest refusal, on memory, sqlite, and
cockroach backends; expiry-after-crash and release-on-close each have a test; `serve`
acquires on start and releases on every exit path (tie into the T8.2 lifecycle tests).

---

### T8.4 — Two-agent demo scenario ★★ (the video's script)
```yaml
requires:   T8.2, T6.4, T4.3   # live store strongly preferred: T3.2, T3.6
fixture-ok: partial   # logic testable on MemoryStore; the artifact must run live
owns:       src/cli/demo.rs, demo/
appends-to: src/main.rs (demo dispatch arm only)
status:     CLEAN + live-verified — 2026-08-15 (adve-review-t8.4-demo.md; T84-2 FIXED; T84-1
            live legs CLOSED by the cluster run — see Handoff 1904-1925, 1990-1999)
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

  > **SUPERSEDED (2026-08-15):** the divergence/seed warning above was scoped "before
  > T7.4 lands". **T7.4 has since landed DONE** (PHASE-7-embeddings.md:322, camera-proof
  > GREEN 2026-08-13): it dropped the hand-created `concepts_embedding_nonnull_idx`,
  > re-provisioned the cluster from `migrations/cockroach/001_init.sql` alone, and removed
  > the 2833/2004 seed session (`--clean`). The vector `EXPLAIN` camera proof passes on the
  > reconciled cluster (see Handoff 2014-2019). The cluster is now provisioned-from-migration;
  > the live demo session writes only its own fresh session rows.

**Done when:** `cargo run --features demo -- demo --scenario rest-api` (or equivalent)
runs end-to-end against the live cluster twice consecutively with identical outcomes, and
the MCP-server split-screen query is rehearsed and screenshotted into `evidence/`.

---

### T8.5 — Demo app (hosted client)
```yaml
requires:   T8.1        # http transport from T8.2 when it lands
fixture-ok: yes
owns:       web/, src/cli/serve_web.rs
appends-to: src/main.rs (serve-web dispatch arm only, if any)
status:     CLEAN + live-verified — 2026-08-15 (adve-review-t8.5-web.md, reverify CLEAN;
            serve-web Cockroach leg verified live — see Handoff 1927-1956, 2020-2025)
flow:       serial; task → adve-review → remediation → review (repeat to CLEAN); hard stop after each agent
```
The "functional demo app URL" deliverable (spec §12.4). Minimal axum-served page over the
http transport: session view, live recall box showing the context block verbatim,
canonization event feed, stats (flush lag / log depth). No framework ceremony — this is a
window onto T5.3's text and T6.4's feed, not a product. Deployment target decided in P9
(any public URL satisfies the judges).

`axum` 0.8 is **already** in `Cargo.toml` — no dependency change needed.

**Done when:** a browser against `lambo serve-web` (the separate read-only demo
command, port 7710) shows a live recall and the event feed updating during the
demo scenario, with `lambo serve-web` running **beside** the MCP writer
`lambo serve` on the same session.

**Optional swarm showcase (non-blocking — must NOT jeopardize the base demo above):**
if time allows after the two-agent demo works, add a swarm view — N concurrent small
agents (any local small-model swarm) writing into one session, with the canonization feed
visibly collapsing duplicates and `reserve` coordination visible. This is a video/Devpost
asset, not a deliverable; the base demo is what the video requires. See T9.3 for the
benchmark that feeds it.

---

### T8.7 — MCP surface hardening (the non-demo remainder of T8.2)
```yaml
requires:   T8.2 (CLEAN)
fixture-ok: yes
owns:       src/mcp/server.rs, src/mcp/serve.rs (hardening only — no new tools)
appends-to: src/main.rs (serve flags for auth/rate-limit config, if any)
status:     minimal-hardening cut done — 2026-08-15 (branch task/t8.7-hardening).
            Bearer auth (fail-closed off-loopback), concurrent-session cap, global
            rate limit, and the T88-H1 wire-hygiene fix are IN with tests; full
            binding gate block green, 703 lib (baseline 685). R1 remediation
            (2026-08-15) closed the T8.7 review findings: residual #3 fixed via a
            graph-size guard on the `resolve_focus` fuzzy leg; residuals #1/#2
            closed with dated accepted-rationales in the review file; and an HTTP
            request-body size limit added (413 past 4 MiB). Gate block re-run green.
            See dev-diary/adversarial-review/adve-review-t8.7-hardening.md + the
            Handoff Log entry below.
flow:       serial; task → adve-review → remediation → review (repeat to CLEAN); hard stop after each agent
```
Created 2026-08-14: collects everything the T8.2 review deferred that is **not** demo-app
work (it was loosely parked at "T8.5/P9" — T8.5 is the demo page and does no hardening, so
that pointer was drift). This is the security/robustness half of the agent-facing surface.

**Contents:**

- **T82-16 remainder** (the half not fixed in T8.2): the HTTP transport is
  **unauthenticated, unrate-limited, and mints an MCP session per connection unboundedly**
  (`LocalSessionManager`). Add: a bearer/token gate on the HTTP transport (loopback-only
  stays the default; auth is required the moment `--bind` is non-loopback), a request
  **rate limit**, a request-size limit if not already bounded, and a **cap on concurrent
  MCP sessions** with honest refusal past it. This is the precondition for exposing the
  surface to a real swarm.
- **R5-verify residual #3 — `resolve_focus` O(total-content) `to_lowercase`**
  (`src/mcp/server.rs`, the `lambo_inspect` fuzzy leg). Only a real amplification vector
  *with* the missing rate limit, which is why it lands here alongside it. Fix the
  allocation (allocation-free, Unicode-correct case-fold) **or** rely on the rate limit +
  a graph-size guard — decide during the task.
- **R5-verify residual #1 — `concept_type` variant-error echoes an escaped control byte.**
  Not interceptable at the lambo layer today (the error is built inside rmcp's
  `Parameters<T>` extractor). Track here; fix if rmcp exposes an extraction-error hook,
  else document as accepted with a dated rationale.
- **R5-verify residual #2 — `redact_urls` misses a bare `host:port`.** Latent (no live
  emitter). Harden `redact_urls` to redact-at-source, or confirm no schemeless-endpoint
  warning path exists and close it as won't-fix with reasoning.

**Explicitly NOT here:** anything the demo page renders (that is T8.5); provisioning
(T8.3's `provision`); the swarm *benchmark* (P9 T9.3).

**Done when:** the HTTP transport refuses unauthenticated non-loopback requests, enforces a
documented rate limit and concurrent-session cap (each with a test), residual #3 is fixed
or provably defused by the rate limit + a graph-size guard, and residuals #1/#2 are each
either fixed or closed with a dated accepted-rationale in the review file.

---

### T8.8 — Documentation of the P8 surfaces (MCP · CLI · API · end-to-end)
```yaml
requires:   T8.2, T8.3, T8.6, T8.7   # documents their FINAL behavior; soft: T8.1 (API), T8.4/T8.5 (flows)
fixture-ok: n/a
owns:       docs/reference/  (mcp.md, cli.md, api.md, config.md, end-to-end.md)
appends-to: rustdoc on public items across src/ (doc-comments only; safe — T8.8 runs last
            in the serial queue, nothing else is editing these files concurrently)
status:     docs-verification pass done — 2026-08-15 (branch task/t8.8-docs, docs/reference/ only).
            All five reference pages verified against the 34c9959 binary and rewritten where
            they disagreed with it.
            DELTA PASS DONE — 2026-08-15 (branch task/t8.8-delta, from 8134a3c). Covers
            docs/reference/**, dev-diary/**, and comment-only rustdoc in src/ EXCLUDING
            src/mcp/** (held by a parallel branch). Landed:
              * L82 delta written into mcp.mdx / cli.mdx / end-to-end.mdx; all three
                {/* L82 delta pass pending */} markers removed. Control + invisible character
                rules, organic vector recall with a per-store leg table (only CockroachDB
                reports VECTOR_SEARCH), and batched flush paired with the honest
                abandoned-tail loss. Verified live against a store-sqlite binary at 8134a3c.
              * Help-text triage table for T88-H1..H11 at the top of
                dev-diary/notes/t8.8-surface-audit.md. T88-H1 marked ASSIGNED to the parallel
                src/mcp branch. T88-H8 FIXED (src/cli/caps.rs, comment-only: MAX_INSPECT_NODES
                is a total budget across all hops, not per frontier level).
              * Rustdoc: cargo doc --no-deps 62 -> 6 warnings (63 -> 6 with
                store-sqlite,store-cockroach). All 6 remaining are src/mcp/** and are NOT
                this pass's; outside src/mcp the count is ZERO. Missing docs 358 -> 185:
                src/types/mod.rs 159 -> 0, src/memory.rs 14 -> 0, src/lib.rs expanded into a
                real crate landing page with a compiling no_run example. Not chased to zero
                by design; 185 recorded honestly in the audit note, best next target is
                src/config.rs (30, prose already exists in config.mdx).
            RESOLVED — 2026-08-15 (the PENDING note below is the point-in-time snapshot):
            (a) T88-H2/H3/H4 wire text + schema maxima closed by the T8.7 / src/mcp merge
            (see Handoff 1746-1759; T88-H1/H2/H3/H4 all fixed in src/mcp/server.rs);
            T88-H6/H7/H10 closed by T8.3 R2/R3, T88-H9 by T8.4. (b) The 6 cargo doc
            warnings and 31 missing docs inside src/mcp/** closed by the T8.7 merge — the
            R2/rustdoc sweep left zero warnings (outside src/mcp it was already 0).
            (c) config.mdx now documents the shipped auth/rate-limit/session-cap state
            (no longer "no-auth").
            PENDING-ORIGINAL-2026-08-14: (a) T88-H2/H3/H4 wire text + schema maxima — need
            the T8.7 / src/mcp branch; T88-H6/H7/H9/H10 are string-literal (code) changes
            owned by T8.3/T8.4, outside a comment-only pass. Each finding's docs half is
            already done. (b) 6 cargo doc warnings and 31 missing docs inside src/mcp/**,
            pending that branch's merge. (c) T8.7-dependent config sections (HTTP auth,
            rate limit, session cap) — config.mdx documents the current no-auth state.
flow:       serial; task → adve-review → remediation → review (repeat to CLEAN); hard stop after each agent
```
Created 2026-08-14: everything P8 ships is a **user-facing surface** and needs proper
documentation, written against what was actually built (not the spec's aspirations) and
kept honest by the review loop. This is the **reference** layer.

**Boundary with P9 T9.1 (to prevent T8.5-style drift):** T8.8 is the *reference* — the
complete, precise description of each surface. T9.1 is the *README / getting-started* — the
cold-reader clone→provision→serve→demo path — and it **links into** T8.8 rather than
duplicating it. T9.2 owns the architecture diagram. If a doc is "how do I run the demo,"
it's T9.1; if it's "what does this tool/flag/method do," it's T8.8.

**Boundary with the surfaces' own help text:** the inline help text — CLI `--help`
(clap `about`/per-arg help, authored in T8.3) and the MCP tool/argument descriptions +
`get_info` instructions (authored in T8.2) — is **NOT** written here. T8.8 *verifies* it:
every tool/subcommand/flag has help text, it is accurate against the binary, and it does
not contradict the reference pages (which are the expanded form of the same truth). Report
any missing or stale help text as a finding for the owning task, don't silently paper over
it in the reference.

**Contents:**

- **`mcp.md` — MCP tool reference.** All seven tools (`derive`, `recall`, `record_action`,
  `reserve`, `inspect`, `stats`, `saints`): purpose, argument schema, the `MAX_*` caps and
  validation rules, the error taxonomy the client sees, and a real wire example
  (request+response JSON) per tool. State the stdio vs streamable-HTTP transports, the
  loopback-default / auth-on-non-loopback rule (T8.7), and that it is a session *writer*.
- **`cli.md` — CLI reference.** Every subcommand — read verbs and the write verbs at MCP
  parity — with flags, exit codes, and worked examples. Document the single-writer/lease
  behavior explicitly: a CLI write acquires the T8.6 lease and **fails closed naming the
  holder** if a `serve` owns the session. Show the swarm idiom (one deterministic
  `lambo derive …` per small agent) as the intended high-throughput pattern.
- **`api.md` + rustdoc — library/API reference.** The public `Memory` API (`build`,
  `derive`, `recall`, `record_action`, `reserve`/`release`, `canonical_memories`, `close`)
  and the Level B adapter traits (`GraphStore`, `Embedder`) with the feature→registry→`dyn
  Trait` resolution. Ensure public items carry accurate rustdoc; `cargo doc` builds clean.
- **`config.md` — configuration & deployment reference.** `lambo.toml` keys + the env
  override rules (Level B), the feature flags (`store-*`, `embed-*`, `demo`), `provision`,
  and the T8.7 HTTP auth / rate-limit / session-cap knobs. Never instruct mixing embedder
  models mid-session without re-embed.
- **`end-to-end.md` — how it composes.** Serve a session; drive it via MCP *and* CLI;
  the single-writer discipline across processes; readers vs the writer; the swarm topology
  (one writer + many CLI agents, canonization dedup, `reserve` coordination). One runnable
  walkthrough a reader can follow verbatim.

**Done when:** each surface built in P8 has a reference page that a reader can act on
without reading the source; every documented command/flag/tool/method actually exists and
behaves as described (spot-checked against the binary, not the spec); `cargo doc` builds
with no warnings; and T9.1's README links into `docs/reference/` instead of restating it.

---

### T8.9 — Release process & binary distribution
```yaml
requires:   T8.2, T8.3, T8.6 (the shippable surfaces); soft: T8.8 (install docs)
fixture-ok: n/a
owns:       .github/workflows/release.yml, scripts/install.sh (if built),
            release notes template, Cargo.toml [package] version/metadata
appends-to: docs/reference/installation.md (prebuilt-binary install path)
status:     done — 2026-08-16 (branch task/release, merged to main).
            Implemented + adversarially reviewed CLEAN (adve-review-t8.9-release.md),
            then a binary+toml parity gate added so every tagged build proves the
            released binary works with a real config. See the Handoff Log entry below.
flow:       serial; task → adve-review → remediation → review (repeat to CLEAN); hard stop after each agent
```
Created 2026-08-14. How Lambo gets from source to a user's machine.

**Binary shape (decided 2026-08-14):** **one `lambo` binary**, not three. It carries both
runtime surfaces — the MCP server (`lambo serve`) and the CLI verbs (`lambo recall`,
`lambo derive`, …). The third surface, the **API, is the library crate** (`src/lib.rs`),
consumed as a Cargo dependency, not distributed as an executable. So a release is: one
`lambo` binary built per target platform, plus (optionally) the published library crate.

**Decisions this task must make and record:**

- **Release feature profile — and how one binary carries all adapters.** Adapters are on a
  *different axis* than the surfaces: selected by Cargo feature at **compile time** and by
  `lambo.toml` at **runtime**. One binary contains *every* adapter it was compiled with (not
  one-per-binary), and the config's `[store] kind` / `[embedder] kind` pick which to use at
  runtime. So the published binary should be built with the **full adapter feature set** (the
  `demo`/ship profile: `store-memory,store-cockroach,store-sqlite,embed-bge,embed-fixture`,
  and `embed-bedrock` only if the account is authorized), and a user switches stores by
  editing config, not by downloading a different binary. State the exact shipped feature list
  and note the one caveat: the adapter code is compiled in, but its backing service must be
  reachable at runtime (a `llama-server` for BGE, a cluster for Cockroach). Building *without*
  some adapters is only for a leaner binary or to drop a heavy/gated dep, never a distribution
  requirement.
- **Target platforms.** At minimum macOS arm64 + x86_64 and Linux x86_64. Decide on Linux
  arm64 and Windows.
- **Versioning.** Adopt semver, set `[package] version`, tag `v0.1.0`. Match the version the
  binary reports (`lambo --version`).
- **Distribution channel.** GitHub Releases with a prebuilt binary per platform (checksums
  included), plus the build-from-source path already in `installation.md`. Decide whether to
  also support `cargo install --git` and/or a `curl | sh` install script.
- **Release automation.** A GitHub Actions workflow that, on a version tag, builds each
  target and attaches the artifacts to the release. Keep it reproducible.
- **Library crate.** Decide whether the API crate is published (crates.io) or repo-only for
  v0.1.

**Boundary:** the *getting-started* prose (build from source, first run, connecting an MCP
client) is T8.8's `installation.md`. T8.9 adds the *prebuilt-binary* install path to that
page and owns the release machinery. The Devpost/README ship checklist stays in P9 (T9.1,
T9.5).

**Done when:** a tagged release produces a downloadable `lambo` binary for each target
platform with checksums, `lambo --version` matches the tag, and a user can install from the
release (not only build from source) and run `lambo serve` on a clean machine.

---

## Exit criteria

- [x] Spec §6.1 doc-test green (Level B `resolve_backends`); §6.2 commands all exist  ·  T8.1/T8.3 CLEAN (Handoff 1856-1878)
- [x] `retract(_, DryRun)` and `canonical_memories()` exist and are tested (T8.1 build items)  ·  T8.1 CLEAN
- [x] Inverted-index mirroring holds for `derive` / `record_action` / `demote` / removal  ·  T8.1 CLEAN
- [x] `serve` / CLI use **one** `ResolvedBackends` (no double construction); fail closed  ·  T8.2 CLEAN (Handoff 1856-1866)
- [x] MCP flow proven from a real Claude Code config  ·  evidence `evidence/mcp-client-stdio/`
- [x] MCP tools stamp `created_at` server-side (F18)  ·  T8.2 F18 tests green
- [x] **T8.6 writer lease enforced cross-process** (two writer opens → one holder, one
      honest refusal; expiry-after-crash tested) and **CLI write verbs land behind it**
      with the CLI↔MCP differential test green  ·  `serve_single_writer_lease` + `cli_write_lease` green (Handoff 1958-1981)
- [x] Demo scenario deterministic ×2 on live infra under `--features demo`, evidence captured  ·  T84-1 CLOSED; `demo-live-{1,2,diff}.txt`
- [x] **Surface holds under concurrency (T8.2 N1/N2 closure):** K concurrent clients
      (K ≥ the CPU worker count, ~12–32 via a local small-model swarm or a raw MCP load
      driver) issuing a mix of valid + adversarial tool calls do **not** starve the
      process — SIGTERM still flushes the tail (`session closed, tail durable`), oversized
      `record_action` gets the honest cap refusal, and no internal detail (URLs/DSNs/driver
      text) crosses the wire. Evidence into `evidence/`. This is
      the correctness half; the P9 T9.3 benchmark is the scale half.
      **CLOSED 2026-08-18 by the C-series capture (C1–C3, branch `codex/c-series`):**
      K=12 raw MCP load driver against a scratch SQLite store; exact
      `lambo serve: session closed, tail durable`, 0 `tail lost on exit`,
      signal→exit 1419 ms, exit 0; interaction yardstick AHEAD by 21
      (in-flight writes already swept by the 1 s flush loop, so the close
      drain was a no-op); concept shortfall 107 fully explained by one daemon
      GC sweep; wire scan clean. Evidence: `evidence/concurrency/` (+ runbook).
      Produced on the Linux box (cachyos-x8664, Ryzen 5 3600, 12 threads,
      CachyOS), which satisfies K ≥ the CPU worker count. The machine is named
      in every artifact because starvation thresholds are hardware-dependent.
- [x] Demo app reachable and honest (renders real recall output, not canned text)  ·  T8.5 reverify CLEAN + live Cockroach `serve-web` (Handoff 1927-1956, 2020-2025)
- [x] **T8.7 surface hardening:** HTTP transport refuses unauthenticated non-loopback
      requests, enforces a documented rate limit + concurrent-session cap (tested);
      T82-16 remainder + R5-verify residuals #1/#2/#3 each fixed or closed with a dated
      accepted-rationale  ·  T8.7 CLEAN (adve-review-t8.7-hardening.md R2)
- [x] **T8.8 reference docs** exist for every P8 surface (MCP tools, CLI read+write verbs,
      `Memory`/adapter API, config, end-to-end); every documented command/flag/tool/method
      verified against the binary; `cargo doc` builds warning-free  ·  T8.8 RESOLVED (see the T8.8 yaml block; docs/reference/)
- [x] Every task reached a CLEAN review verdict; all review files closed in
      `dev-diary/adversarial-review/`  ·  every T8.x review CLEAN/CLOSED

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
| `evidence/mcp-client-stdio/` | new evidence directory | required by the task's Done-when |
| `src/lib.rs` | **not touched** — `pub mod mcp;` already existed | — |

#### Level B — single construction site

`main.rs` performs the **one** `resolve_from_config_path` (in the pre-existing
`resolve_for_command`) and hands the single `ResolvedBackends` into `mcp::serve`.
`mcp::serve::build_memory` **takes `ResolvedBackends`, not a config path**, deliberately: a
second resolve is not expressible through the API. Fail-closed verified four ways (unknown
TOML key, uncompiled store kind, bad transport, missing `--session`) — all exit before any
session is attached. Captured in `evidence/mcp-client-stdio/README.md`.

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

> **Closed by J1** (`dev-diary/lambo-for-mooshik/J-multi-client.md` §J1). The suggested fix
> is what shipped: `Memory` gained `derive_as` / `record_action_as` / `reserve_as` /
> `release_as` (plus a private `begin_interaction_as`) with the existing methods delegating
> to `self.agent`, and `LamboServer` passes the caller's `agent_id` through. The
> `attribution:` warning and `require_session_agent`'s refusal described above are both
> gone. Everything above this line remains an accurate account of the T8.2-era surface.

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
responses captured, in `evidence/mcp-client-stdio/stdio-all-seven-tools.jsonl`; that
file, not this sentence, is what supports the seven-tool claim. `lambo_recall` returns the
T5.3 context block verbatim. HTTP transport verified with `curl POST /mcp`. Transcripts in
`evidence/mcp-client-stdio/`.

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

---

### T8.6 — Single-writer lease (store-enforced §2.2) (task agent, 2026-08-14)

**Gates:** `cargo fmt --all -- --check` clean; `cargo clippy --all-targets -- -D warnings`
clean; `cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -D
warnings` clean; `cargo clippy --all-targets --features store-sqlite -D warnings` clean;
`cargo test` **598 lib passing, 1 ignored** (baseline ~588 → +10, no regressions, nothing
removed); `cargo test --features store-sqlite` **641 lib + the two serve subprocess
integration tests passing, 1 ignored**. The one ignored lib test is the live cockroach lease
test (`#[ignore]`d without `LAMBO_COCKROACH_DSN`, matching the existing convention).

#### What exists now

- **`src/store/lease.rs`** (new, owned) — `LeaseHolder` (agent + pid + host; `token()` =
  `agent@host#pid`), `LeaseInfo` (holder token + `acquired_at` + `expires_at`), `LeaseOutcome`
  (`Acquired` | `Held { current, age }`), `LEASE_TTL = 45s`, `LEASE_HEARTBEAT_INTERVAL = 15s`,
  and `OPERATOR_OVERRIDE` (the DELETE string).
- **`GraphStore` trait** (`src/store/mod.rs`) — three new methods `acquire_lease` /
  `refresh_lease` / `release_lease`, each with an **advisory default** (always-grants,
  persists nothing) so the ~15 test-double impls keep their prior behaviour untouched. The
  three real backends override with enforcement.
- **`MemoryStore`** — a per-instance keyed lease map (`RwLock<HashMap<String, LeaseRow>>`),
  one map lock held for the whole decision (the in-RAM analogue of the SQL atomic upsert).
  Makes the same-*store* collision enforced, not just logged.
- **`SqliteStore` / `CockroachStore`** — a `session_leases` table (added to both migration
  files and applied automatically by `provision.sh`'s splitter). Acquire/refresh is ONE
  statement: `INSERT ... ON CONFLICT DO UPDATE SET ... WHERE (expired OR mine) RETURNING`.
  A returned row that is ours ⇒ `Acquired`; an empty RETURNING ⇒ the guard was false ⇒ read
  the row back and report `Held` (bounded retry if it was released in the gap). Release is a
  holder-scoped DELETE.
- **`Memory::build`** acquires the lease as the **last fallible step** before the (infallible)
  task spawns, so a refusal spawns and leaks nothing. It fails closed with `LamboError::Conflict`
  naming the current holder, its age, and the operator override. A heartbeat task refreshes at
  TTL/3. **`Memory::close`** releases on the **success** paths only (a graceful close hands off;
  a failed close keeps the lease for a retry and lets it lapse at TTL). **`Drop`** aborts the
  heartbeat so a leaked handle's lease actually lapses — it does *not* release (Drop cannot
  `await`, and a dropped-without-close handle is the crash-shaped path where expiry is correct).
- **`src/mcp/serve.rs`** — a build-time `assert!(LEASE_TTL > SHUTDOWN_BUDGET)` (and a matching
  runtime assertion in `the_grace_windows_are_sane`), plus doc on how the lease rides the exit
  paths. No new code was needed for release-on-exit: `serve` already calls `close()` on every
  exit path, and `close()` now releases.

#### Files touched outside `owns`

| File | Change | Authorization |
|---|---|---|
| `src/store/mod.rs` | trait methods + `pub mod lease` + re-exports | `appends-to` (trait method) |
| `src/memory.rs` | build acquires/heartbeats, close/Drop release/abort | `appends-to` |
| `src/mcp/serve.rs` | TTL-vs-budget assertion + doc + release-on-close test | `appends-to` (holder identity + release) |
| `tests/serve_single_writer_lease.rs` | new subprocess test | test for the owned feature |

#### Key decisions (and why)

- **TTL 45s, heartbeat 15s (= TTL/3).** `SHUTDOWN_BUDGET` is 15s (5s transport + 10s final
  flush). The TTL must outlast the whole graceful-shutdown budget so a slow-but-graceful close
  *releases* rather than *expires* mid-flush (which would admit a second writer while the first
  is still flushing). 45s is 3× the budget; a `const _` assertion in `serve.rs` pins the
  relationship. Heartbeat at a third means two consecutive missed refreshes are survivable
  while a genuine crash still lapses within one TTL.
- **Holder = agent + pid + host, no per-handle nonce.** The reported identity is what an
  operator sees. Same-process **same-agent** double-open produces an identical token, so a
  second acquire looks like a refresh and is *not* caught by the lease — deliberately delegated
  to the retained `ACTIVE_SESSIONS` advisory log (kept as the cheap same-process catch). The
  lease's real job is cross-process / cross-host, where pid or host differ.
- **Clock discipline / F18.** `acquire`/`refresh` take a TTL **duration**, never an absolute
  timestamp. Each backend stamps `acquired_at`/`expires_at` from its own clock (Cockroach
  `now()`, SQLite `strftime(...,'now')`, Memory `Utc::now()`). No wire-visible lease field
  exists, so the F18 golden-allowlist guard is untouched (verified: `cargo test` green,
  including the `f18_*` tests).
- **Crash-expiry ≠ tail drained.** A comment in `build()` and the lease module pins that
  acquiring a lease says nothing about durability: the previous holder's tail died with it, so
  the new holder still goes through the unconditional `load_session_async` replay. The lease is
  a concurrency gate, not a completeness guarantee.
- **Advisory default on the trait, not a per-impl port.** There are ~15 `GraphStore` impls
  (mostly test doubles). A permissive default keeps them all compiling and behaving exactly as
  before; only the three real backends enforce. This is why the T8.6 change adds no churn to
  `flush.rs` / `canon` / `hybrid` test stores.

#### The one existing test I changed (not weakened)

`a_second_handle_on_one_session_is_reported_loudly` (T81-8) previously built a **second
handle on a shared store** and asserted it *succeeded* ("reported, not refused"). That is
exactly the behaviour T8.6 promotes to a refusal, so the old setup is now impossible. I
**retargeted** it to two *separate* stores on one logical session — the post-T8.6 domain the
advisory log still owns (the per-store lease cannot see across store handles / processes; the
process-global registry can). Every assertion is preserved (loud log, both agent ids, release
on drop); coverage is not reduced. The shared-store refusal it used to (accidentally) not
cover is now a **new** test, `a_second_writer_sharing_a_store_is_refused_by_the_lease`.

#### Tests → property

| Test | Property | Kind |
|---|---|---|
| `store::memory::tests::lease_grants_one_holder_and_refuses_another` | acquire / Held / refresh / release | in-process (memory) |
| `…::a_stale_release_does_not_evict_the_new_holder` | holder-scoped release | in-process |
| `…::an_unreleased_lease_expires_and_is_reacquirable` | expiry-after-crash (before/after TTL) | in-process |
| `…::refresh_preserves_the_original_acquired_at` | heartbeat keeps acquired_at | in-process |
| `memory::tests::a_second_writer_sharing_a_store_is_refused_by_the_lease` | shared-store refusal + release-on-close | in-process (Memory) |
| `memory::tests::one_store_two_builds_yield_one_holder_and_one_refusal` | one holder / one refusal | in-process (Memory) |
| `store::sqlite::tests::lease_lifecycle_on_one_connection` | acquire/Held/refresh/release | in-process (sqlite) |
| `…::an_unreleased_lease_expires_on_sqlite` | expiry-after-crash | in-process (sqlite) |
| `…::two_connections_on_one_file_serialize_on_the_lease` | cross-connection serialize | in-process, shared file |
| `tests/serve_single_writer_lease.rs` | **cross-process** one-holder/one-refusal | **subprocess**, gated `store-sqlite` |
| `mcp::serve::…::a_clean_close_releases_the_lease` | release-on-close via lifecycle seam | in-process |
| `store::cockroach::conformance::single_writer_lease_is_enforced_across_pools` | cross-pool enforce + expiry | live, `#[ignore]`d (needs `LAMBO_COCKROACH_DSN`) |

#### What the adversarial reviewer should probe

1. **Release-before-durable window on a *failed* close.** A close that fails keeps the lease
   and stops the heartbeat, so the lease then lapses at TTL even though the owner still holds
   the handle and might retry `close()`. If the retry comes after the TTL, another writer could
   have taken over in between — the retry still flushes (it doesn't re-acquire), and the new
   writer replayed from durable state, but the two could briefly coexist. Documented as an
   accepted edge; a reviewer may argue the heartbeat should keep beating until a *successful*
   close instead.
2. **SQLite fractional-second TTLs.** Acquire formats the TTL as a `strftime` modifier
   (`+{as_secs_f64} seconds`). The sqlite expiry test uses a whole 1s TTL to stay clear of any
   fractional-second rounding; a reviewer should confirm sub-second TTLs (only used if a caller
   passes one — production is 45s) behave on the bundled SQLite version.
3. **Held-with-empty-RETURNING retry loop.** Both SQL backends retry ≤3× when the contended
   row vanishes between the failed upsert and the read-back. A reviewer should check the loop
   can't spin or misreport (it returns a Backend error after exhausting retries).
4. **Heartbeat on a lost lease keeps running.** If a refresh returns `Held` (this handle lost
   the session after a store outage starved the beat), it logs loudly but does **not**
   self-destruct — two writers are diverging and recovery is an operator's. A reviewer may want
   a stronger response (e.g. latch the session closed to writers).
5. **`Drop` does not release the lease.** Intentional (crash-shaped path; Drop can't await),
   but it means a library caller who drops a `Memory` without `close()` holds the session for a
   full TTL. The heartbeat is aborted in Drop so it *does* lapse — worth confirming that abort
   is reliable even during runtime shutdown.
6. **Cross-process test only covers the loser failing.** `serve_single_writer_lease` asserts
   the second process exits non-zero naming the holder; it does not assert the *winner* keeps
   serving cleanly through the contention. The live cockroach test covers cross-pool re-acquire
   after release/expiry; the winner-liveness angle is only implicit.

### T8.3 — CLI subcommands (task agent, 2026-08-14)

**Gates:** `cargo fmt --all -- --check` clean; `cargo clippy --all-targets -- -D warnings`
clean; `cargo clippy --all-targets --features store-sqlite -- -D warnings` clean;
`cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings`
clean; `cargo check --no-default-features` clean.

**`cargo test`:** **613 lib + 4 bin + 3 integration + 1 doctest passing, 3 ignored** (1 lib
`embed::bge_m3::tests::live_smoke_against_llama_server`, 2 integration live-calibration).
Baseline on this branch was ~598 lib — **+15 lib**, no regressions, nothing removed.
(Same counting convention as sqlite: lib / bin / integration / doctest named separately.
The earlier "4 integration" folded the doctest in; T83-8 unifies the two lines.)

**`cargo test --features store-sqlite`:** **657 lib + 4 bin + 8 integration + 1 doctest
passing, 3 ignored**. Baseline ~641 lib — **+16 lib** (the extra one is
`cli::sqlite_tests::provision_then_every_subcommand_against_sqlite`). New integration:
`tests/cli_provision_sqlite.rs`, `tests/cli_write_lease.rs`. Existing serve lease /
durability tests still green. (R1 measured 8 integration + 1 doctest, not 7.)

**Ignored honesty:** live cockroach `saints`/`stats`
(`cli::saints::live::saints_and_stats_against_live_cockroach`) is `#[ignore = "live:
requires LAMBO_COCKROACH_DSN"]` and only compiled under `store-cockroach`. It was **not
run** in this environment (no DSN). Default `cargo test` does not require a cluster.

#### Files touched outside `owns`

| File | Change | Authorization |
|---|---|---|
| `src/main.rs` | dispatch arms + write variants + help/F18 tests + `Resolved::StoreOnly { store, kind }` so provision keeps the store | `appends-to` (dispatch + own flags + clap help). Serve lifecycle **untouched**. Demo stays a stub. |
| `src/mcp/server.rs` | `MAX_*` / `check_size` / `clamp_cfg_default` / `resolve_focus` / `render_neighbourhood` now imported from `cli::caps` / `cli::inspect`. `derive_impl` / `record_action_impl` / `recall_impl` made `pub(crate)` for the differential test. **No tool behaviour, schemas, or error classes changed.** | necessary shared-file extract |
| `tests/cli_provision_sqlite.rs` | subprocess: `lambo provision` on a fresh sqlite file then recall/derive | standing additive |
| `tests/cli_write_lease.rs` | subprocess: derive succeeds with no serve; fails closed naming the holder while serve owns the session; readers still succeed | standing additive |
| `src/lib.rs` | **not touched** — `pub mod cli;` already existed | — |
| `Cargo.toml` / `Cargo.lock` | **not touched** — no new dep | — |

#### Reader vs writer construction (do not re-derive)

- **Readers** (`recall`, `saints`, `inspect`, `stats`): `store::load::load_session_async`
  (async core, never the sync wrapper). Wrap `graph`/`index` in `Arc<parking_lot::RwLock<_>>`.
  `recall` builds `Daemon::from_config(graph, &Config::default()).with_index(index)` and
  **does not spawn** (spawn would run GC = writer). Embed the query only if
  `store.capabilities().contains(VECTOR_SEARCH)`. Print `RecallResult.context`.
  **Never call `Memory::build()`.** Never touch the lease.
- **Writers** (`derive`, `record-action`, `reserve`, `release`): exactly one
  `Memory::builder().session().agent().backends(backends).build().await`, the op, then
  `close().await`. `LamboError::Conflict` is printed as-is (names holder, age,
  `OPERATOR_OVERRIDE`) and exits 1. A failed `close` is printed and exits 1. Always
  `runtime.shutdown_background()` afterwards (Memory spawns tasks).
- **`provision`:** `resolve_store_only` path (no embedder). Kind is read from a second
  `LamboFile::load_resolved` (file parse only — the store is still constructed once).

#### Provision sqlite vs cockroach split

- `store.kind = sqlite` → `GraphStore::init_schema().await` on the resolved store
  (idempotent). This is T82-10's missing half: `lambo provision --config sqlite.toml` on a
  fresh file then `recall`/`derive` works (`tests/cli_provision_sqlite.rs`).
- `store.kind = cockroach` → `bash scripts/provision.sh` (walk cwd + parents). Fail closed
  if the script is missing or non-zero. DSN is **not** a CLI flag.
- `store.kind = memory` → success, "needs no schema". Proven:
  `cli::provision::tests::provision_memory_store_succeeds_without_sql`.

#### Where validators live now

`src/cli/caps.rs` owns every `MAX_*` constant, `check_size` (returns `Result<(), String>`,
names the codepoint, never echoes the raw byte), and `clamp_cfg_default`. MCP wraps
`check_size` into a tool error; CLI maps it to exit 2 (`CliError::Usage`).
`resolve_focus` / `render_neighbourhood` / `Focus` live in `src/cli/inspect.rs`
(`pub(crate)`); MCP imports them. There is one implementation — MCP and CLI cannot drift.

#### What the next agent must not re-derive

- `Memory::build()` **is** the writer lease. Readers must not call it. T8.3 did not add a
  reader mode on `Memory`.
- `--parent-of CHILD:PARENT` is child-left, parent-right. MCP `WireParentOf` is
  `{parent, child}` — map parent=right, child=left into `ParentOf::from_pairs`.
- `--concept CONTENT:KIND` splits on the **last** colon (kind is a closed token).
- CLI `--agent` **does** bind `Memory` at `build()`, so unlike MCP's per-call `agent_id`
  gap, a CLI write is attributed to that flag. Sequential CLI writers with different
  `--agent` are different `AgentId`s; they still serialize on the T8.6 lease.
- `lambo reserve` then process-exit **drops the reservation** (RAM-local, S5). A later
  `lambo release` cannot see it and fails with `not found: no reservation`. That is
  honest, not a bug in `release`. MCP reserve lasts because serve keeps one `Memory` alive.
- Help-walk test skips `demo` (T8.4 owns `--scenario` help). Serve flags were already
  documented by T8.2.
- Provision's `Resolved::StoreOnly` now **carries the store**. The old `let _store =
  resolve_store_only(...)` discard is gone.

#### Weak spots I am flagging myself

1. **CLI `reserve` cannot outlive the command.** Close drops RAM-local reservations, so
   the success path is "reserved for the lifetime of this process, which is about to
   exit". The command still exists for MCP parity and prints the advisory warning. A
   reviewer may want reserve/release to share a long-lived writer — that would violate
   "open, op, close" and is not T8.3's to invent.
2. **Reader `recall` uses `Config::default()` knobs**, same as `lambo serve` today
   (T82-12 is not ours). A `lambo.toml` `default_top_k` does not reach the reader daemon.
3. **`canonical_memories` scan is copied** from `Memory::canonical_memories` into
   `cli::saints::canonical_memories_from_graph`. T8.3 cannot change `memory.rs`. If the
   sort order ever changes, both copies must move together.
4. **Live cockroach saints/stats not executed here.** The test exists, is `#[ignore]`d,
   and needs `LAMBO_COCKROACH_DSN` + `--features store-cockroach -- --ignored`.
5. **Provision does two `LamboFile::load_resolved` calls** (kind + `resolve_store_only`).
   One store construction. Changing `resolve_store_only` to return kind was out of owns
   (`src/resolve.rs`).
6. **Differential compares hit *texts*, not scores.** MCP recall runs against a live
   spawned daemon; CLI recall does not spawn. Scores / seconds-ago can differ; concept
   texts, types, and action strings are the parity that counts.
7. **`--max-tokens` / `--traversal-depth` on `recall`** are extra vs the phase yaml's
   `recall --session --query --top-k`. Added so CLI can match MCP knobs without a second
   pass. A reviewer who wants the yaml-minimal flag set can drop them; defaults still
   come from `Config::default()` when omitted.

### T8.3 — R1 remediation (remediation agent, 2026-08-14)

Fixes every R1 finding in `dev-diary/adversarial-review/adve-review-t8.3-cli.md`
(T83-1 through T83-11). No deferrals. `src/mcp/server.rs` untouched. T8.8/T8.9 markdown
left alone except this append, the T8.3 yaml `status: remediating:r1`, and the T83-8
count correction in the task-agent entry above.

| Id | What changed |
|---|---|
| T83-1 | `cli::tests::parent_of_writes_hierarchical_edge_parent_to_child` asserts `edge_between(parent, child, Hierarchical)` after `derive::run`. Shipped direction unchanged. |
| T83-2 | `--parent-of` with more than one colon is `CliError::Usage` naming the ambiguity. Flag help updated. Not `rsplit_once`. |
| T83-3 | F18 walk covers `get_id()`, `get_long()`, `get_all_aliases()`; banned tokens match as substrings. |
| T83-4 | After reader `recall`, epoch and canonization statuses are unchanged; production `recall.rs` must not contain `.spawn()`. |
| T83-5 | Embed-failure / daemon warnings prepend `⚑` lines onto recall stdout (tracing is not installed on the CLI path). |
| T83-6 | Cockroach `provision` requires a `[package] name = "lambo"` Cargo.toml beside `scripts/provision.sh`, bounds the walk at 16 ancestors, echos the resolved path. |
| T83-7 | `saints` / `stats` / `inspect` take `Resolved::StoreOnly` (`needs_embedder()` false); they no longer construct an embedder. |
| T83-8 | Task-agent Handoff counts unified: 3 integration + 1 doctest (default), 8 integration + 1 doctest (sqlite). |
| T83-9 | After the no-serve derive, a second writer derive must succeed — names the lease-release property. |
| T83-10 | `reserve` success text: the reservation ends when this process exits, not "on restart" / `until <timestamp>`. |
| T83-11 | `cli::saints::parity::canonical_memories_from_graph_agrees_with_memory`. |

**Gates (this round):** `cargo fmt --all -- --check` clean; both clippy `-D warnings`
lines exit 0.

**`cargo test`:** **620 lib + 5 bin + 3 integration + 1 doctest passing, 3 ignored**
(R1 baseline 613 lib + 4 bin + 3 integration + 1 doctest — **+7 lib, +1 bin**, no
regressions).

**`cargo test --features store-sqlite`:** **664 lib + 5 bin + 8 integration + 1
doctest passing, 3 ignored** (R1 baseline 657 lib + 4 bin + 8 integration + 1
doctest — **+7 lib, +1 bin**).

---

### T8.5 — Demo app / read-only session window (task agent, 2026-08-14)

**Gates:** all nine lines of the binding block run, exit 0. `cargo fmt --all -- --check`
clean; `cargo clippy --all-targets -- -D warnings` clean; `cargo clippy --all-targets
--features store-cockroach,store-memory,fixtures -- -D warnings` clean; `cargo clippy
--all-targets --features store-sqlite -- -D warnings` clean; `cargo test
--no-default-features --features store-sqlite --no-run` clean; `cargo test
--no-default-features --features store-cockroach --no-run` clean; `cargo check
--no-default-features` clean.

**`cargo test`:** **636 lib + 5 bin + 3 integration + 1 doctest passing, 3 ignored**
(baseline 620 lib — **+16 lib**, all in `cli::serve_web`; no regressions, nothing removed).

**`cargo test --features store-sqlite`:** **680 lib + 5 bin + 8 integration + 1 doctest
passing, 3 ignored** (baseline 664 lib — **+16 lib**, the same 16).

#### Files touched outside `owns`

| File | Change | Authorization |
|---|---|---|
| `src/main.rs` | `Commands::ServeWeb` variant (+ its clap help), one `name()` arm, one dispatch arm calling `cli::serve_web::run` through the existing `run_async`. `needs_embedder()` **not** edited — the default arm already returns true, which is right: recall embeds when the store claims `VECTOR_SEARCH`. Serve lifecycle, demo stub, every other arm untouched. | `appends-to` (dispatch arm + own flags) |
| `src/cli/mod.rs` | `pub mod serve_web;`, placed alphabetically between `saints` and `stats` | one-line module declaration; nothing else in the file changed |
| `Cargo.toml` / `Cargo.lock` | **not touched** — axum 0.8 was already there, and nothing else was needed | — |
| `src/mcp/*`, `src/store/*`, `src/graph/*` | **not touched** | — |

#### The architecture decision: reader + store poll, not the writer's broadcast

`serve-web` is a **lease-free reader** (spec §2.2). It never constructs a `Memory`, never
takes the T8.6 lease, never spawns GC — the `cli::recall` / `cli::stats` discipline — so it
runs beside a live `lambo serve` on the same session instead of competing for it.

The live feed therefore polls `GraphSnapshot::canonization_events` rather than subscribing
to `Memory::events()`. That broadcast is an in-process `tokio::sync::broadcast` owned by the
writer; a separate reader process cannot subscribe to it at all. Taking it would mean
becoming the writer, which would mean holding the lease, which would mean the page could not
coexist with `serve` — the one thing the demo needs. The durable audit trail is the same
transitions, one flush behind, and a **superset** across writer restarts.

What that costs, said on the page rather than papered over:

- **`flush_lag` / `log_depth` report `n/a`.** They live in the writer's flush task. A reader
  printing `0` would be claiming a durability bound it cannot see — the call `cli::stats`
  already makes, kept identical here.
- **Graph `epoch` is not surfaced at all.** `Graph::from_snapshot` loads at epoch 0, so a
  reader's epoch is *always* 0. (`lambo stats` does print it; that is a pre-existing wart in
  a file T8.5 does not own, noted for whoever does own it.)
- **Two store round-trips per `/api/pulse`** — one raw `load_session` for the event tail,
  one `load_reader_graph` for counts. Deliberate: reusing both existing paths verbatim beats
  reimplementing the snapshot→graph invariant checks to save a SELECT.

What *is* live and does move during a scenario: node / edge / concept / canonical counts,
the canonization feed, and `durable_change_age_ms` (how long this reader has seen the
durable counts stand still).

#### Read-only is enforced, not asserted

The HTTP surface is unauthenticated until T8.7, so a write route reachable from a browser is
a stranger with a pen in the session's memory. Every route is `routing::get`; there is no
derive / record_action / reserve path. Three tests hold the line:

- `read_only_router_has_no_mutating_route` — live 405 sweep, 9 routes × POST/PUT/PATCH/DELETE.
- `the_module_registers_only_get_routes` — source scan of the production half for
  `routing::post|put|patch|delete` **and** for writer constructs (`Memory::builder`,
  `open_writer`, `acquire_lease`, `.spawn()`).
- `routes_constant_covers_every_registered_route` — parses the router body and proves the
  sweep's route list has no gaps, so a new path cannot dodge the sweep.

`--bind` defaults to loopback; a non-loopback bind warns on stderr **and** raises a banner on
the page. **Public exposure still requires T8.7 first** — even read access hands the whole
session to anyone who can reach the port.

#### Endpoints

`GET /` · `/app.css` · `/app.js` · `/healthz` · `/api/session` · `/api/recall` ·
`/api/events?since=N` · `/api/stats` · `/api/pulse?since=N`. The page polls `/api/pulse`
(stats + event tail in one round trip) every 1.5 s.

#### P9 / AWS readiness (deployment is P9; this is what it can rely on)

- **Self-contained binary.** `web/{index.html,app.css,app.js}` are `include_str!`-embedded —
  no CDN, no webfont, no asset directory. `embedded_assets_reference_no_external_origin`
  rejects any external origin in them, so a zero-egress task still renders.
- **`GET /healthz`** answers ALB / ECS checks **without touching the store**, so a slow
  database degrades the page instead of failing the target and pulling the task out of rotation.
- **No secrets in the page.** `/api/session` reports store and embedder *kind* only.
  `session_info_never_leaks_the_dsn_path_or_embedder_url` seeds a DSN with a password, a
  SQLite path and an embedder URL, then asserts none of them appear in the response body.
- **`/api/*` is `Cache-Control: no-store`** — session memory must never be served from a
  CloudFront edge.
- **Polling, not SSE.** There is no `Stream` impl in the dependency set to hand
  `axum::response::Sse` (adding `futures-core`/`tokio-stream` would be a Cargo.toml change
  T8.5 does not need), and a 1.5 s poll survives ALB / CloudFront idle timeouts and
  connection recycling that a long-lived SSE channel does not.
- **`--bind` / `--port`** follow `serve`'s conventions; port default 7710 (serve is 7700), so
  both can run on one host.

#### The bug the tests could not have caught

`serve_bounded` applies the grace window to the **post-signal drain only**. The first cut
wrapped the whole `axum::serve` future in `timeout(SHUTDOWN_GRACE, ..)`, so the server exited
`0` on its own after five seconds. Unit tests could not see it — they call `axum::serve`
directly — and it surfaced only from running the binary.
`the_grace_window_bounds_the_drain_not_the_server` now fails on it in ~240 ms, with
`a_shutdown_signal_stops_the_server_within_the_grace_window` on the other side.

#### What the next agent should NOT re-derive

- **The reader cannot see writer stats.** Do not "fix" `flush_lag` / `log_depth` by printing
  zeros; the only honest fix is a writer-side endpoint, which needs T8.7's auth first.
- **The `memory` store is process-local.** A reader in another process sees an empty session
  through it. `serve-web` warns on stderr and banners the page rather than looking broken;
  the demo needs sqlite or cockroach.
- **`seq` is a position in a `(occurred_at, id)`-sorted list**, sorted in `events_from`
  rather than trusted from the adapter, so the poll cursor means the same thing on
  MemoryStore, SQLite and Cockroach.
- **Canonization thresholds are not reachable in a demo window** (`min_peer_count` 20,
  `canonization_edge_min_age` 60 s, `canonization_eval_interval` 60 s, all from
  `Config::default()`, which `serve` does not read from file — T82-12). Planting the feed for
  a video is **T8.4's** scenario job, not this page's.

#### Verified end to end, two live processes over one sqlite file

`lambo serve-web` reading while separate `lambo derive` / `record-action` writer processes
landed writes: counts moved `0/0/0 → 3/4/2 → 7/12/4` and `durable_change_age_ms` reset to 0
on each; `/api/recall` returned the context block verbatim, canonical marker and `⚑`
load-bearing-pillar line intact, after real transitions were recorded through the store's own
`record_canonization`; `/api/events?since=N` tailed the three hops incrementally; the live
405 sweep matched the test. Browser-verified in the pane.

**UI caveat for personal review:** the recall form is spec-correct (a real `<form>`, one
text input, `<button type="submit">`, no console errors) and submits on click, but a
*synthetic* Return from browser automation did not reach it. Implicit submission should work
for a human pressing Enter — worth one manual keypress before the video.
### T8.4 — task agent (2026-08-15) — two-agent demo scenario

- **Branch:** `task/t8.4-demo`, cut from `phase/p8-surface` @ `f68cfc3`.
- **Owns:** `src/cli/demo.rs`, `demo/README.md`, `demo/LIVE-RUNBOOK.md`,
  `tests/t84_demo.rs`. **Paths changed 2026-08-18** (`bcf75aa` + the docs
  commit that follows it): `demo/` is now gitignored because it holds the
  video-recording assets, `demo/README.md` moved to
  [`docs/demo.md`](../docs/demo.md), and `demo/LIVE-RUNBOOK.md` was dropped from
  tracking rather than moved. It still exists locally under the ignored `demo/`
  and is quoted where this doc needs it. Read `demo/…` below as historical.
- **Appends only:** `src/cli/mod.rs` (`pub mod demo;` — the one line the
  shared-file rule allows) and `src/main.rs` (**one** dispatch arm, plus the
  `demo` subcommand's own two flags `--scenario` / `--session` with help text).
  Nothing else in `main.rs` was touched. This closes **T88-H9**: `lambo demo` is
  no longer a stub printing `lambo demo (stub)`.
- **Entry point:** `lambo demo --scenario rest-api`. Library seam:
  `cli::demo::run_scenario(store, embedder, contract, args, echo) -> DemoRun`.

**Scenario (spec §13).** 12 scripted interactions on one session, static data in
`ACT_I` / `ACT_II` / `ACT_III`: agent A derives `user schema` / `auth
middleware` / `session store`, plants nine `parent_of` children under the pillar
and records six actions that depend on it; agent B joins on a separate feature
and takes an edge to the pillar; agent A returns for the last edit
(`modifies: user schema`). `user schema` then climbs Candidate → Venerable →
Canonical with one `canonization_events` row per hop, and agent B's
`recall("update user schema")` renders the canonical marker, the ⚑ 9-nodes line
and the conflict line. **No code path in `demo.rs` writes a status or an audit
row** — the real `CanonizationTask` does, through the same write gate that
rejects fabricated transitions.

**Knobs (documented in `docs/demo.md`, formerly `demo/README.md`, and the module docs).** Two `Config`s;
**no threshold weakened**, only intervals and one age floor compressed:
`canonization_edge_min_age` 60s → **10ms** (kept non-zero, so the inflation
guard still bites), `canonization_eval_interval` 60s → **1h during the build**
(frozen) then **25ms**, `daemon_tick_interval` 1s → **5ms**,
`backend_flush_interval` 1s → **5ms**, `gc_interval` 10 000 → spec default
during the build then **1**, `match_strategy` → **Canonical** (determinism: no
embedding lookups on the write path). Untouched: `min_peer_count` 20,
`eval_batch_size` 50, `repromotion_cooldown` 300s, `max_canonical_nodes` 1000,
`conflict_recency_window` 30s, scoring/recall weights, and every stage constant.
**No new `Config` key was added.**

**Determinism — the four things that actually bit, and the fix for each.**

| Source | Fix |
|---|---|
| Canonization cycles racing the build | acts I–III run with the eval interval frozen and GC at spec default; the canonization attach **writes nothing**, so no cycle ever sees a half-built graph |
| Stage 1's `gc_survived >= 3` gate | GC bumps only on session mutations, so an idle session's counters stop. `settle_gc_survived` declares one real synonym at a time and **awaits the resulting sweep** until the floor is met for every concept — which is what makes the fixed point unique (a node admitted under the earlier P90 is still admitted under the final one) |
| `recency` measured against real timestamps | `STEP_PACING` (10ms) makes the session's temporal extent a property of the script, not of scheduler jitter. Also makes the narration readable on camera |
| Exact score ties broken by random `NodeId` | structurally identical siblings in one derive carry **distinct concept types**; the audit trail is grouped by concept rather than by node id |

**GC cannot be disabled for the demo** (this was tried first): Stage 1 gates on
`gc_survived >= 3` and GC's survivor bump is the only thing in the system that
raises it, so a GC-free demo has **zero transitions**. GC therefore runs, and
the script is instead a session with nothing collectable: `cli::demo::gc_headroom`
measures every concept's distance from GC's step-2 bar and the run refuses below
`MIN_GC_HEADROOM` 1.25×, naming the concept. Current worst is **1.55×**. Two
script edits were needed to get there (artifacts now carry a real dependency
chain), and one more to separate the two concepts that were within ~0.005 of
each other at Stage 1's P90 cut.

**Normalized in the ×2 comparison, and only these three:** the conflict line's
age (`<n>`, the true age of agent A's write), the rendered composite score
(`<s>`, its `recency` term is a wall-clock measurement) and node ids (`<node>`,
`Uuid::new_v4()`). Hit **ordering**, contents, `[Entity, canonical]`,
`blast radius 9`, the ⚑ line and the conflict sentence are all compared byte for
byte. The demo prints the real values.

**R3-1 honoured:** `lambo demo` mints a fresh session id per run
(`demo-rest-api-<uuid>`); `--session` is documented fresh-only. `seed()` is
never called.

**Tests.** `tests/t84_demo.rs` runs the whole scenario **twice in one process
per backend** against a store that already holds run 1's session, and asserts
the two `DemoOutcome`s are equal plus every spec §13 string:
`scenario_is_identical_twice_on_the_memory_store` and
`sqlite::scenario_is_identical_twice_on_sqlite` (`--features store-sqlite`),
plus an unknown-scenario usage test. 19 unit tests in `cli::demo` cover the
script invariants (nine dependents with the pillar as only parent, no action
target collides with one, ≥6 distinct span sources, agent A writes last, agent B
holds an edge, sibling type distinctness) and the normalizers. Stability
measured: **14 consecutive invocations green** (8 default + 6 sqlite = 28 full
scenario runs, all pairwise identical). One earlier failure was a 61s run that
spanned a host suspend — the waits are wall-clock bounded and the conflict
window is 30s, so a laptop that sleeps mid-run invalidates that run; noted in
the runbook's failure table.

**Gates (full binding block, all green):** `cargo fmt --all -- --check` clean;
all three clippy `-D warnings` lines exit 0; `cargo test` **639 lib + 5 bin + 5
integration + 1 doctest passing, 3 ignored** (T8.3 baseline 620 lib + 5 bin + 3
integration — **+19 lib, +2 integration**); `cargo test --features store-sqlite`
**683 lib + 5 bin + 11 integration + 1 doctest passing, 3 ignored** (baseline
664 lib + 5 bin + 8 integration — **+19 lib, +3 integration**); no regressions.
`cargo test --no-default-features --features store-sqlite --no-run`
and `--features store-cockroach --no-run` both build; `cargo check
--no-default-features` clean. `demo.rs` is not feature-gated — it uses only core
APIs and compiles on every row of the matrix.

**Live-only, not done here.** The T8.4 "done when" needs the scenario ×2 against
the **live cluster**, plus the split-screen `canonization_events` query through
CockroachDB's managed MCP server. Neither can run on this machine (no DSN).
`demo/LIVE-RUNBOOK.md` (local-only since 2026-08-18, see Owns above) carries the
exact commands, the expected transcript, the
`diff` that constitutes the ×2 proof, the session-scoped audit query for the
split screen, a failure-mode table, and the **schema-divergence warning** (the
hand-created `concepts_embedding_nonnull_idx` and the ~2833 seeded concepts on
the cluster; the demo reads only its own fresh session, so seed rows do not
affect its outcome, but table-wide queries must be scoped by `session_id`).

**Residual, for the reviewer.** `src/main.rs`'s `every_subcommand_and_required_arg_has_help`
still skips `demo` with the comment "its flags are not authored here". The flags
now exist and carry help text, so that skip can be dropped — left alone
deliberately, because the shared-file rule limits this task to one dispatch arm
plus its own flags, and another task is appending to the same `match` in
parallel.

### T8.7 — MCP surface hardening (task agent, 2026-08-15) — MINIMAL CUT, not the full block

Scope was deliberately narrowed on 2026-08-15 to the pieces the AWS-bound demo needs:
auth, a session cap, a rate limit if cheap, and the T88-H1 wire-hygiene fix. **Three of the
four T8.7 residuals were not touched** — see "Not done" below. The T8.7 "Done when" is
therefore *not* satisfied; the status line says so.

**Branch:** `task/t8.7-hardening`, cut from `8134a3c`.

#### 1. Bearer-token auth, fail-closed off loopback

`--auth-token` plus `LAMBO_AUTH_TOKEN`, **env wins** — a token in argv is visible in `ps`
and shell history, so the deployment channel takes precedence rather than the reverse. A
*set-but-empty* env var is a usage error (exit 2), not a silent fallback to the flag: that
shape is almost always an unset variable that expanded to nothing.

The rule by transport: **stdio** unaffected (process-local — the client owns the process it
spawned); **HTTP on loopback** keeps today's optional-auth behaviour; **HTTP anywhere else**
requires a token or `serve` refuses to start. The refusal runs as the *first statement* in
`serve()`, before `build_memory` — so a misconfigured start takes no single-writer lease and
the operator's retry is not blocked by their own refused attempt.

Comparison is constant-time: no early exit, and the loop runs over the *presented* input
indexing the expected token modulo its length, so a wrong-length guess does not disclose the
expected length. `std::hint::black_box` stops the optimiser short-circuiting the accumulator.
No new dependency — this is one comparison, not a reason to take `subtle`.

The token lives in a `SecretToken` newtype with a redacting `Debug` and **no `Display`**, so
"never logged" is a property of the type rather than a promise each future caller must keep.
`ServeOptions` and clap's `Commands` both derive `Debug`; this is what makes that safe.

#### 2. Concurrent-session cap (T82-16's unbounded half)

Default 32, `--max-sessions`. Enforced in the same middleware, counted from
`LocalSessionManager`'s own public `sessions` map — rmcp mutates it on create and on
DELETE, so there is **no bookkeeping of ours that can drift from the truth**. Past the cap a
new `initialize` gets 503 naming the live count, `--max-sessions`, and the `DELETE /mcp`
remedy. Requests carrying an `Mcp-Session-Id` are never counted, so the cap bounds *new*
sessions instead of becoming an outage for established ones.

Known and accepted: two concurrent `initialize`s can both pass the check and overshoot by
one. Bounded by in-flight concurrency, harmless at this scale, and closing it would mean
wrapping rmcp's 13-method `SessionManager` trait — not worth it for the minimal cut.

#### 3. Rate limit — BUILT, not deferred, but narrower than the block asked

The block says "a request rate limit"; the brief said build it only if cheap. It was cheap:
a token bucket over `parking_lot::Mutex` + `Instant`, both already in the tree. **No new
dependency.** Default 50 rps sustained with a 2× burst, `--rate-limit-rps 0` disables.

**The honest caveat:** it limits *HTTP requests to `/mcp`*, **not `tools/call` specifically**.
Singling out `tools/call` means buffering and re-injecting every request body to read the
JSON-RPC `method` — real machinery and a correctness risk on a streaming transport — for a
distinction that barely matters here, since on streamable HTTP each `tools/call` is its own
POST. The limit is **global, not per-connection**: per-connection state is trivially defeated
by opening more connections.

**Ordering is load-bearing and tested:** auth → rate → cap. An anonymous flood gets 401 and
never spends rate budget or reads the session count (a 503-vs-401 difference would leak how
loaded the server is).

#### 4. T88-H1 — internal notes were being published to every MCP client

`WireConceptType`'s rustdoc became its JSON-Schema `description` in every `tools/list`
response, carrying a review marker ("Byte-echo note (R4 nit)"), rmcp's `Parameters<T>`
internals, the internal helper name `validate_size`, and a "revisit if…" note. Rewritten as
a user-facing description of the five concept kinds; the rationale moved to a plain `//`
block that opens by saying why it must not become rustdoc again.

Swept the other wire types for the same pattern. `RecallParams.agent_id` published
`"spec §2.2 — see the attribution note in the tool docs"` (**T88-H2**) — both references are
unreachable from the wire. Replaced with the rule the runtime actually applies, and the same
sentence added to the five other `agent_id` fields that carried **no** description at all,
plus `reserve`'s stricter variant which refuses a foreign id rather than warning
(**T88-H3**, which asked for exactly that sentence).

New `published_schemas_carry_no_internal_notes` scans every tool description and every
schema string for marker substrings (`rmcp`, `revisit`, `spec §`, `t82-`, `r1/`, `r4 nit`,
`byte-echo`, `handoff log`, `validate_size`, `todo`, `fixme`, `xxx`). **Mutation-verified**:
re-adding the note shape to one field description fails it. The **F18 golden allowlist is
unaffected** — descriptions are not in the property set — and both F18 tests still pass.

#### Gates (full binding block, all green)

`cargo fmt --all -- --check` clean; all three clippy `-D warnings` lines exit 0;
`cargo test` **703 lib + 5 bin + 5 integration + 1 doctest passing, 3 ignored**
(baseline **685** lib — **+18 lib**, no regressions); `cargo test --features store-sqlite`
**750 lib + 5 bin + 11 integration + 1 doctest passing, 3 ignored** (baseline **732** lib —
**+18 lib**, the same eighteen, no regressions); `cargo test --no-default-features --features
store-sqlite --no-run` and `--features store-cockroach --no-run` both build;
`cargo check --no-default-features` clean.

The +18 breaks down as 17 in `src/mcp/serve.rs` (auth, cap, rate limit, fail-closed start)
and 1 in `src/mcp/server.rs` (`published_schemas_carry_no_internal_notes`).

#### Verified against the real binary, not just tests

`--bind 0.0.0.0` with no token refuses with the full message (exit 1). Empty
`LAMBO_AUTH_TOKEN` is a usage error (exit 2). On loopback with a token: no credential → 401
+ `WWW-Authenticate: Bearer`, wrong token → 401, correct token → 200 with a session id. With
`--max-sessions 3`: sessions 1–3 admitted, 4th and 5th refused 503 with the honest body.
With `--max-sessions 2`: `DELETE /mcp` → 202 and the next `initialize` → 200, proving the
count is live rather than monotonic. Startup log shows `auth_required=true max_sessions=32
rate_limit_rps=50` — **posture visible, token absent**. SIGTERM still logs "session closed,
tail durable", so the middleware did not disturb the durability path.

**Exit-code scheme used** (consistent with T88-H11): **2** = the command line or environment
is malformed and cannot be interpreted (bad `--transport` value, empty token value); **1** =
well-formed but the server will not start (bind-policy refusal, unprovisioned store).

#### Not done — the rest of the T8.7 block

- **R5-verify residual #1** (`concept_type` variant error echoes an escaped control byte).
  Untouched. The rmcp survey done for T88-H1 confirms it is still not interceptable:
  `Parameters<T>` builds the `-32602` inside the framework before any `LamboServer` code
  runs. Needs an rmcp extraction-error hook, or hand-rolled deserialize in all seven tools.
- **R5-verify residual #2** (`redact_urls` misses a bare `host:port`). Untouched; still
  latent, still no live emitter.
- **R5-verify residual #3** (`resolve_focus` O(total-content) `to_lowercase`). Untouched.
  The rate limit now bounds the amplification rate, which is the "defused by the rate limit"
  half of the block's own disjunction — but the **graph-size guard** it pairs that with does
  not exist, and the allocation is unchanged. Do not read this entry as closing #3.
- **Request-size limit.** Not addressed. Tool-layer string caps (`MAX_CONTENT_BYTES`, 16 KiB)
  still apply, but no HTTP body limit was added or audited.
- **Session-cap overshoot** under concurrent `initialize` (above).

#### Files touched

`src/mcp/serve.rs` (all three checks + tests), `src/mcp/server.rs` (T88-H1/H2/H3 + the
hygiene test), `src/main.rs` (three `serve` flags + token resolution — flags only, per the
shared-file rule), `src/mcp/mod.rs` (re-exports for the new public items; additive).
`Cargo.toml` **not touched** — no new dependency was needed for any of it.

#### R1 remediation (2026-08-15) — closed the T8.7 review findings

Follow-up to the review `dev-diary/adversarial-review/adve-review-t8.7-hardening.md`. The
findings are closed here and marked in that file. No new tools, no config knobs, no
dependencies, no `Cargo.toml` change.

- **T87-1 (P1) residual #3 — FIXED via graph-size guard.** `resolve_focus`
  (`src/cli/inspect.rs`) now counts the session's concepts before its O(total-content)
  lowercase pass and refuses (`Focus::Oversized { cap }`) a graph over
  `MAX_INSPECT_SCAN_CONCEPTS` (2_000) instead of paying the pass — the graph-size guard the
  rate limit was missing. The exact and node-id legs are unaffected. The refusal is surfaced
  in MCP `lambo_inspect` (`src/mcp/server.rs`) and CLI `inspect::run`. Tests in
  `src/cli/inspect.rs` pin both the firing (cap+1 graph) and the non-firing (small graph)
  cases.
- **T87-2 (P2) residuals #1/#2 — CLOSED-ACCEPTED (2026-08-15).** Dated accepted-rationales
  recorded in the review file: #1 (concept_type `-32602`) is built inside rmcp's
  `Parameters<T>` before any `LamboServer` code runs — not interceptable, and replacing
  `Parameters<T>` in all seven tools is not worth it; #2 (`redact_urls` bare `host:port`) is
  latent with no emitter, and widening the matcher risks over-redacting readable warnings.
- **T87-3 (P2) HTTP body limit — FIXED.** `guard_request` (`src/mcp/serve.rs`) returns 413 for
  any declared body over `MAX_HTTP_BODY_BYTES` (4 MiB) before streaming to rmcp. Test
  `an_oversized_request_body_is_refused_before_the_service` asserts the 413 and that the inner
  service is never reached.

```text
cargo fmt --all -- --check                                    CLEAN
cargo clippy --all-targets -- -D warnings                    CLEAN
cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings  CLEAN
cargo clippy --all-targets --features store-sqlite -- -D warnings                          CLEAN
cargo test                                                  706 lib + 5 bin + 5 int + 1 doc, 0 failed, 1 ignored
cargo test --features store-sqlite                          753 lib + 5 bin + 11 int + 1 doc, 0 failed, 1 ignored
cargo test --no-default-features --features store-sqlite --no-run    BUILDS
cargo test --no-default-features --features store-cockroach --no-run BUILDS
cargo check --no-default-features                             CLEAN
```

The remediation agent's gate-run output (exact counts) is in its result for this task.

### T8.2 / T8.3 — re-verification at HEAD post-hardening (2026-08-15)

Because the T8.7 hardening, the L82 remediation, and the `596f40f` P3-remediation merge all
reached into `src/mcp/**` and `src/cli/**`, T8.2 and T8.3 were re-run through the full
adversarial review→remediation→reverify loop at current HEAD after those merged. Both came
back **CLEAN**, reverified in this session:

- **T8.2 (MCP server)** — re-verified CLEAN at HEAD `596f40f` (report
  `adve-review-t8.2-mcp-r2.md`). Seven tools intact; T88-H1 off the wire; F18 refuses client
  `created_at` via `deny_unknown_fields`; single-writer/lease intact; the HTTP auth/session-
  cap/rate-limit/413 layers do not regress any tool contract. Zero P1/P2.
- **T8.3 (CLI)** — main review CLEAN (report `adve-review-t8.3-cli-r2.md`) with 3 pre-existing
  P3s (T88-H6/H7/H10). Because phase 8 is being driven to full correctness, those cheap P3s
  were remediated through the agent loop (R2/R3 reverify, both CLEAN):
  - **T88-H7** — doubled `inspect: inspect:` prefix stripped in `src/cli/inspect.rs` (the
    dispatcher already supplies `lambo inspect: `); `cli.mdx` quote updated to match.
  - **T88-H6** — lease-conflict message in `src/memory.rs` dropped the `Spec §2.2` citation and
    the raw `DELETE FROM session_leases` takeover SQL; still names holder+age and fails closed,
    now points at `docs/reference/cli.mdx`. The pinned behavior test was updated to the new
    message contract (memory suite 70 passed). `cli.mdx` quote matched byte-for-byte.
  - **T88-H10** — `release` not-found now explains (CLI-side wrap in `src/cli/reserve.rs`) that
    a CLI reservation is RAM-local to the reserving process.
  This P3 work crosses T8.1 paths (`src/memory.rs`) — authorized and recorded here.

### Cockroach/SQLite minimal-row CI fix (2026-08-15)

CI surfaced an unused-import failure under `-D warnings` on the bare minimal rows
(`--no-default-features --features store-cockroach` and `store-sqlite`): `seed_concept_rows` /
`seed_edge_rows` at `src/store/cockroach.rs:107-110` and `src/store/sqlite.rs:152-155` were
imported unconditionally but their only user is the `#[cfg(feature = "fixtures")]` `seed`
method, so with `fixtures` off they compiled unused. Introduced by the L82 bulk-seed refactor
(commit `635b272e`) which did not gate the two imports; the `fixtures` gate on `seed` predates
it (`0e76a9f9`). **Not a coverage hole** — the conformance tests were already `/ignore`d behind
`fixtures` + a live DSN, and the `seed_*` helpers remain unit-tested in `batch.rs` under every
row. Root-caused by a scout, remediated and reverified by agents.

Fix: split the fixtures-exclusive helpers out of the unconditional `use super::batch::{…}` into
a dedicated `#[cfg(feature = "fixtures")] use super::batch::{seed_concept_rows, seed_edge_rows};`
in BOTH adapters. No `#[allow]`. Proven: `RUSTFLAGS="-D warnings" cargo check
--no-default-features --features store-cockroach --tests` and `... store-sqlite --tests` both
exit 0 (were 101); `cargo check --features store-cockroach,store-memory,fixtures` still exit 0.

**Gate-block gap this exposed:** the phase gate block's two `--no-run` minimal rows do not pass
`-D warnings`, so this landed as a warning locally and only CI (which runs
`RUSTFLAGS=-D warnings`) caught it. Recommend adding `-D warnings` to those two rows (or a
`-D warnings` check of the minimal rows) so feature-mismatch dead imports fail locally, not in
CI.

### T8.4 — two-agent demo (review loop, 2026-08-15)

Adversarial review of T8.4 at current HEAD (report `adve-review-t8.4-demo.md`). The demo is
**REAL, merged, and verified** (not a stub — T88-H9 CLOSED): the scenario runs x2 on the memory
backend with byte-identical OUTCOME blocks (12 interactions / 27 concepts / 93 edges / `user
schema` Canonical + the ⚑ 9-nodes warning + conflict line); a real Candidate→Venerable→Canonical
transition driven through the real engine with a documented 10 ms `canonization_edge_min_age`
knob (no faked transitions); R3-1 honored (fresh sessions per run, never calls `seed()`);
`--scenario bogus` → exit 2 listing valid scenarios; `demo --help` names the flags. Gate block
green.

- **T84-2 (P3) FIXED** — dropped the stale `demo` skip (and its stale comment) from the
  `every_subcommand_and_required_arg_has_help` test in `src/main.rs`; the demo's about +
  `--scenario`/`--session` help already satisfy the invariant, so `demo` is now covered like
  every other subcommand. No help-text additions needed. Reverify CLEAN.
- **T84-1 (P2) DEFERRED-INFRA** — the two live done-when legs (scenario x2 against the LIVE
  Cockroach cluster; split-screen `canonization_events` screenshot into `evidence/`)
  are UNPERFORMED and blocked by live infrastructure (`LAMBO_COCKROACH_DSN` unset, cluster +
  CockroachDB-managed MCP unreachable from the review machine). NOT a code defect and NOT
  fabricated; recorded with `demo/LIVE-RUNBOOK.md` §1-§6 for the cluster holder. This is an
  **open exit-criteria item**: T8.4 is code-CLEAN but not exit-complete until a holder with
  cluster access performs those two legs.

  > **SUPERSEDED (2026-08-15):** T84-1 is now **CLOSED**. The live-cluster verification entry
  > below (this handoff, "T8.4 / T8.6 / T8.5 — live-cluster verification") ran `lambo demo
  > --scenario rest-api` ×2 against the live Cockroach cluster with byte-identical OUTCOME
  > blocks (12 interactions / 27 concepts / 114 edges / 5 canonization_events) and read the
  > split-screen `canonization_events` back via `psql`. Evidence: `demo-live-{1,2,diff,canon-events}.txt`.

### T8.5 — demo web app (review loop, 2026-08-15)

Adversarial review of T8.5 at current HEAD (report `adve-review-t8.5-web.md`), including a
LIVE verification against the running server. The web surface WORKS: a real rest-api demo
scenario (T8.4 writer) ran while `lambo serve-web` polled the same session and confirmed live
updates (nodes 11→39, edges 23→93, concepts 8→27, canonical 0→1, events 0→5 with 5 real
canonization transitions in the feed); `/api/recall` returned the T5.3 context block verbatim
(incl. `[Entity, canonical]` + the ⚑ load-bearing-pillar line); the page is READ-ONLY (405 on
every mutation verb); a headless browser renders all four pieces (session view, recall box,
event feed, stats). Gate block green. Findings closed:

- **T85-1 (P2) FIXED — serve-web now mirrors the T8.7 fail-closed bind auth.** Previously
  `--bind 0.0.0.0` served the whole session unauthenticated with a stale "T8.7 pending" banner.
  Now: non-loopback bind with NO token is a hard startup refusal (exit 2, honest error);
  loopback stays unauthenticated by default (judge's browser works); a token comes from
  `LAMBO_AUTH_TOKEN` or `--auth-token` (env overrides flag; set-but-empty env fails closed);
  every route requires `Authorization: Bearer` when a token is set (constant-time compare);
  stale "T8.7 pending" wording removed from module doc/stderr/app.js/main.rs help. Read-only
  preserved (no write route; mutations still 405). New tests pin the auth (fail if removed).
- **T85-2 (P3) FIXED** — done-when now names `lambo serve-web` (port 7710) beside the MCP
  writer `lambo serve`, not `serve --transport http`.
- **T85-3 (P3) ACCEPTED-by-design** — flush_lag/log_depth stay n/a on the read-only reader
  (a lease-free reader cannot observe the writer's flush task; printing 0 would be a lie);
  already disclosed on-page.
- **T85-4 (P3) informational** — surfaced for the T8.4 video crew, not addressed here.

Reverify CLEAN. Headless-browser confirmation that a browser shows live recall + the event
feed updating (the done-when) came from the fixture/sqlite path.

**SUPERSEDED — the Cockroach live-cluster leg is now performed.** The T8.5 serve-web
Cockroach leg was verified live against the cluster (see the live-cluster entry below:
read-only reader, verbatim recall, real flush stats 39/114/27/1/5, POST `/api/pulse` -> 405).
This supersedes the earlier "infra-blocked like T8.4" statement in this T8.5 entry.

### T8.6 — single-writer lease (re-verification, 2026-08-15)

T8.6 was re-run through an adversarial review at current HEAD (report
`adve-review-t8.6-lease-r2.md`) because its prior CLEAN verdict (2026-08-14) predated the L82
lease-release change and today's lease-message rewrite. **Verdict CLEAN** — no P1/P2.

- Atomic acquire intact on all three backends (memory single map-lock; sqlite/cockroach single
  `INSERT ... ON CONFLICT ... WHERE ... RETURNING` — no read-then-write race).
- Fail-closed `Conflict` naming holder+age intact after the T88-H6 message rewrite (only the
  inline takeover SQL + spec citation were dropped; still points at `docs/reference/cli.mdx`).
- Heartbeat / expiry-after-crash / release-on-close intact. The L82 "lease release" change
  (`060cca1`) is ADDITIVE — `release_lease_after_abandoned_close` + `serve::release_lease_bounded`
  on every abandoned-close path — strictly wider release coverage, not a regression.
- `serve` acquires on start and releases on every exit path (`close_bounded` +
  `release_lease_bounded`); readers never touch the lease.
- Cross-process battle-tests green: `serve_single_writer_lease` +
  `cli_write_lease::derive_succeeds_with_no_serve_and_fails_closed_while_serve_holds`.
- **T86R2-2 (P3) infra-blocked:** the live Cockroach cross-pool done-when leg
  (`single_writer_lease_is_enforced_across_pools`) is compile-only + `#[ignore]d`/`dsn_or_skip`
  with no `LAMBO_COCKROACH_DSN` — same cluster-holder exit item as T8.4/T8.5. The cockroach
  atomic acquire, T86-3 `tx_retry`, and holder-scoped release are present, clippy-clean, and
  reviewable; the live leg awaits a cluster run.
- **T86R2-1 (P3)** recorded, no action: the message rewrite cost is operator convenience only
  (SQL is in `docs/reference/cli.mdx` + the migrations' comments, not inlined in the refusal).

### T8.4 / T8.6 / T8.5 — live-cluster verification (2026-08-15)

The live CockroachDB Cloud cluster legs were exercised end-to-end. Evidence in
`evidence/`: `demo-live-{1,2}.txt`, `demo-live-diff.txt`,
`demo-live-saints.txt`, `demo-live-canon-events.txt`,
`demo-live-conformance.txt`, `demo-live-serveweb-cockroach.txt`.

- **T84-1 (P2) CLOSED — the two live done-when legs are met.** `lambo demo
  --scenario rest-api` ran twice against the live cluster (session
  `demo-rest-api-0d0d5148-…`), each exit 0, OUTCOME blocks byte-identical
  (`diff` prints `IDENTICAL — T8.4 x2 met`): 12 interactions, 27 concepts, 114
  edges, `user schema` Canonical blast radius 9, `canonization_events 5`
  (user schema None→Candidate→Venerable→Canonical, plus two non-canonical
  entries). `saints` returns the canonical memory; the `canonization_events`
  split-screen query read back via `psql` shows the same 5 rows. The
  parallelism: drive the demo ×2 while `serve-web` (reader) shows live
  recall + feed + real stats on the same cockroach session.
- **GC-headroom fix (the live failure the loop caught).** The first live run
  failed the demo's GC-headroom assertion (`'middleware/session.rs'` at 1.03×
  vs floor 1.25); a second weak concept then surfaced (`'users table
  migration'` at 1.08×). Root cause: several concepts rest on the
  timing-sensitive `recency` score term, which real network flush latency
  jitters run to run on live (memory/sqlite replay near-instantly and clear
  it). Fixed by lifting ALL structurally weak concepts at once (GC multi-lift,
  11 depends_on additions + 2 derive-only additions for the user-schema
  dependents + 2 assurance edges); headroom now robust (~1.4×–2.07×).
  Adversarially reviewed CLEAN. `DemoOutcome.edges` 60 -> 114.
- **OUTCOME canonization fixed-point (submit/verify).** The OUTCOME
  `canonization_events` was snapshotted before the quiesce window, showing 4
  while the fixed-point trail + store held 5. Moved the trail capture to the
  fixed point; OUTCOME now reports 5 == "5 events total" == store row count.
- **T86R2-2 CLOSED — the live Cockroach lease leg passed.** The live
  conformance suite (`scripts/run-live-cockroach.sh`, `LAMBO_REQUIRE_LIVE=1`)
  is green 8/8, including `single_writer_lease_is_enforced_across_pools`,
  `cockroach_three_hop_progression_matches_memory`, `saints_and_stats_on_live`,
  and the vector `EXPLAIN` camera proof. This supersedes the earlier
  "T86R2-2 infra-blocked" note above.
- **Serve-web Cockroach leg (T8.5) verified live.** `serve-web` against the
  cockroach session is a read-only reader (`store_is_process_local:false`,
  `vector_search:true`); `/api/recall` returns the T5.3 context block verbatim
  under `user schema [Entity, canonical]`; `/api/stats` shows real numbers
  (39/114/27/1/5, `flush_lag_ms:0`, `log_depth:0` — the T85-3 writer-stats
  fix demonstrated on Cockroach); POST `/api/pulse` -> 405 (read-only holds).
- **T86R2-2 (P3): now CLOSED — superseded by the live-passing conformance 8/8
  above (single_writer_lease_is_enforced_across_pools passed live).**

### T8.9 — release process & binary distribution (task/release, merged 2026-08-16)

Implemented on branch `task/release` (off `main`), adversarially reviewed to
CLEAN, then merged to `main`. Evidence: `adve-review-t8.9-release.md`; the live
`cargo build --release --features ship` + `lambo --version` = `lambo 0.1.0`.

- **One `lambo` binary carries every compiled adapter; the config's
  `[store] kind` / `[embedder] kind` pick it at runtime.** New `ship` Cargo
  feature = store-memory, store-cockroach, store-sqlite, embed-bge,
  embed-fixture. `embed-bedrock` is EXCLUDED (AWS account not authorized —
  bedrock-authorization-blocker.md); it stays an optional gated swap-in.
- **Targets**: GitHub Actions matrix on a `v*` tag over 5 native runners —
  linux-x86_64 + linux-arm64, macos-arm64 + macos-x86_64, windows-x86_64.
  No cross/zigbuild (native runners keep it reproducible).
- **Versioning**: semver; `0.1.0` lives only in `Cargo.toml [package] version` and
  `lambo --version` matches. Release tags `v0.1.0`; the release job asserts
  `GITHUB_REF_NAME == v$VERSION` so a stale tag can never publish mismatched
  artifacts.
- **Distribution**: GitHub Releases primary (binary per platform + SHA-256
  checksum files), plus `scripts/install.sh` (curl | sh, checksum-verified) and
  `cargo install --git` (documented, secondary). Library crate is repo-only for
  v0.1 (crates.io deferred).
- **Pipeline**: `.github/workflows/release.yml` on a `v*` tag builds each
  target with `--features ship`, stages binaries + checksums, attaches
  `scripts/install.sh` so `/releases/latest/download/install.sh` resolves, and
  `gh release create --verify-tag`. All third-party actions SHA-pinned (repo
  convention). Release notes template at `.github/release/release-notes-template.md`.
- **Binary+TOML parity gate (2026-08-16)**: every unix build job now runs the
  parity suite (`tests/binary_parity.rs`, from `main`) against the EXACT staged
  `--features ship` release binary with a real toml — proving the released
  artifact works end-to-end (demo determinism, CLI write + lease, serve-web
  live, MCP stdio) before it can ship. A `ship`-only regression fails the
  release build. Added install path to `docs/reference/installation.mdx`.

**Review loop:** implement → review (found T8.9-P1: install.sh not published as
a release asset, so the primary install URL 404'd; fixed by attaching it; and
T8.9-P3: claimed `--verify-tag` absent, added) → reverify CLEAN. The parity gate
itself went through review → remediate (REL-PARITY-1: a nested `CARGO_TARGET_DIR`
would have tested a subset rebuild, not the ship artifact; fixed to use the same
default dir + `--target` + `--features ship` so `CARGO_BIN_EXE_lambo` resolves to
the exact staged binary) → reverify CLEAN.

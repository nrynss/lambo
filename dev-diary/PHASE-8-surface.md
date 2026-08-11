# P8 — Surface (binary, MCP, demo)

```yaml
id:       P8
branch:   phase/p8-surface
requires: [T2.3, T2.4, T4.3, T5.3, T6.4]   # soft: T3.2 (live store), T7.x (hybrid)
blocks:   P9
parallel: partial   # T8.1 first; then T8.2 ‖ T8.3 ‖ T8.5; T8.4 needs T8.2
```

**Goal:** assemble the library into `lambo`, expose it over MCP, and make the spec §13
two-agent demo scripted and reproducible. This is where the tracks converge; expect
integration friction here, not in the tracks — budget for it.

**Level B:** process start uses **`resolve_from_config_path` / `resolve_backends`** once
(spec §3.4, `notes/level-b-pluggability.md`) and hands **`ResolvedBackends`** into the
command. Serve and CLI never hard-code `CockroachStore::connect`, never rebuild store/
embedder with a second config pass, and stamp/check `EmbeddingContract` on session attach.

---

### T8.1 — `Memory` builder & assembly ★
```yaml
requires:   T2.3, T2.4, T2.5, T3.4, T3.5, T4.1, T4.6, T5.3, T1.5
fixture-ok: yes   # assembles against MemoryStore first
owns:       src/memory.rs
status:     not-started
```
The spec §6.1 surface, exactly: builder (`session`, `agent`, `store`, `embedder`,
`match_strategy`, `flush_interval`, `scoring_weights`) → `build()` wires graph + daemon +
flush task + startup load. Methods: `set_root_goal`, `declare_synonym`, `recall`, `derive`,
`record_action`, `demote`, `retract(_, DryRun)` (dry-run = blast-radius report, no
mutation), `reserve`, `canonical_memories`, `stats` (must expose flush lag + log depth),
`events`, `close` (final flush, clean shutdown of both tasks). Cut list stays cut: no
`correct`, `merge_concepts`, `resume`, `restart_daemon`, `checkpoint`.
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
`MemoryStore` (default features), `close()` flushes the tail, and session attach rejects
embedder kind/model/dim mismatches.

---

### T8.2 — MCP server ★
```yaml
requires:   T8.1
fixture-ok: yes
owns:       src/mcp/, src/main.rs (serve flags)
status:     not-started
```
`lambo serve --session S --transport stdio|http [--port 7700] [--config PATH]` via `rmcp`;
**fallback authorized by spec §6.3: hand-rolled stdio JSON-RPC if rmcp fights — timebox the
fight to half a day.** Tools: `lambo_recall`, `lambo_derive`, `lambo_record_action`,
`lambo_reserve`, `lambo_inspect`, `lambo_saints`, `lambo_stats`. One process owns the
session (spec §2.2); tool calls from multiple MCP clients are tasks inside it, each
carrying `agent_id`.

**rmcp re-add (COH-2, 2026-08-12):** `rmcp` is **not** in Cargo.toml today — removed by
8f9e527 (no MCP server ships yet; `src/mcp/` is an empty stub). T8.2 **owns re-adding it
with a deliberate 0.1.x-vs-v3 choice** (the P8 implementer decides at that point; both
0.1.x and v3 are viable — the hand-rolled JSON-RPC fallback in §6.3 covers either). Do
not assume the crate is already present.

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
owns:       src/cli/
status:     not-started
```
Spec §6.2: `demo --scenario rest-api`, `recall --session --query --top-k`,
`saints --session`, `inspect --session --focus --depth`, `stats --session`, `provision`
(wraps `scripts/provision.sh`). Global/shared `--config` where a store is needed. Read-only
commands go straight to the store as reader processes (spec §2.2) — they must not spin up a
writer against a session another process owns.

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
status:     not-started
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

**Done when:** `cargo run --features demo -- demo --scenario rest-api` (or equivalent)
runs end-to-end against the live cluster twice consecutively with identical outcomes, and
the MCP-server split-screen query is rehearsed and screenshotted into `dev-diary/evidence/`.

---

### T8.5 — Demo app (hosted client)
```yaml
requires:   T8.1        # http transport from T8.2 when it lands
fixture-ok: yes
owns:       web/, src/cli/serve_web.rs (if axum routes live in-binary)
status:     not-started
```
The "functional demo app URL" deliverable (spec §12.4). Minimal axum-served page over the
http transport: session view, live recall box showing the context block verbatim,
canonization event feed, stats (flush lag / log depth). No framework ceremony — this is a
window onto T5.3's text and T6.4's feed, not a product. Deployment target decided in P9
(any public URL satisfies the judges).

**Done when:** a browser against `lambo serve --transport http` shows a live recall and the
event feed updating during the demo scenario.

---

## Exit criteria

- [ ] Spec §6.1 doc-test green (Level B `resolve_backends`); §6.2 commands all exist
- [ ] `serve` / CLI use **one** `ResolvedBackends` (no double construction); fail closed
- [ ] MCP flow proven from a real Claude Code config
- [ ] Demo scenario deterministic ×2 on live infra under `--features demo`, evidence captured
- [ ] Demo app reachable and honest (renders real recall output, not canned text)

---

## Handoff Log

> _Fill on completion._

# P8 — Surface (binary, MCP, demo)

```yaml
id:       P8
requires: [T2.3, T2.4, T4.3, T5.3, T6.4]   # soft: T3.2 (live store), T7.x (hybrid)
blocks:   P9
parallel: partial   # T8.1 first; then T8.2 ‖ T8.3 ‖ T8.5; T8.4 needs T8.2
```

**Goal:** assemble the library into `lambo`, expose it over MCP, and make the spec §13
two-agent demo scripted and reproducible. This is where the tracks converge; expect
integration friction here, not in the tracks — budget for it.

**Level B:** process start loads `LamboFile` → `build_store` / `build_embedder` (spec §3.4,
§6.1–§6.2). Serve and CLI never hard-code `CockroachStore::connect` or a single embedder.

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

**Level B:** builder accepts `Box<dyn GraphStore>` and `Box<dyn Embedder>` produced by the
registries (not concrete adapter types). Doc-test uses `LamboFile` + `build_store` /
`build_embedder` as in spec §6.1.

**Done when:** a doc-test mirroring the spec §6.1 snippet compiles and runs against
`MemoryStore` (default features), and `close()` provably flushes the tail (store contains
everything).

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

**Level B:** on start, `LamboFile::load_resolved(config_path)` → `build_store` +
`build_embedder` → inject into `Memory`. Fail closed if selected kinds are not compiled.
Document required features for the live demo binary (`--features demo` or
`store-cockroach,embed-bge`).

**Done when:** `lambo serve` pasted into a Claude Code MCP config works — recall through a
real client returns the T5.3 context block. That paste-a-path flow is the adoption story;
test it for real, in `dev-diary/evidence/`. Config file + env selection proven in evidence.

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

- [ ] Spec §6.1 doc-test green (Level B registry construction); §6.2 commands all exist
- [ ] `serve` / CLI load `LamboFile` + `build_store` / `build_embedder` (fail closed)
- [ ] MCP flow proven from a real Claude Code config
- [ ] Demo scenario deterministic ×2 on live infra under `--features demo`, evidence captured
- [ ] Demo app reachable and honest (renders real recall output, not canned text)

---

## Handoff Log

> _Fill on completion._

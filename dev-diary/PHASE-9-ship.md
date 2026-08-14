# P9 — Ship

```yaml
id:       P9
branch:   phase/p9-ship
requires: [P8]
blocks:   submission (Tue Aug 18, 5:00 pm ET — 2:30 am IST Aug 19)
parallel: high   # T9.1 ‖ T9.2 ‖ T9.4 ‖ T9.5; T9.3 needs T8.4 rehearsed
```

**Goal:** the spec §12.4 deliverables, all of them, with a buffer in front of the deadline.
Nothing here is optional except where marked; the checklist *is* the phase.

---

### T9.1 — README & repo hygiene
```yaml
requires:   T8.1
fixture-ok: n/a
owns:       README.md, docs/
status:     not-started
```
Setup and run instructions (clone → provision → serve → demo, verified on a clean machine
or container); **the single-writer constraint stated** (spec §12.4); the CockroachDB tools
and AWS services used, written out (that written identification is itself a deliverable);
credit to the v0.6.0 design doc for honesty (spec §12.4 note); MIT license visible in the
GitHub About sidebar (add the license file mapping if GitHub doesn't auto-detect). Repo
public.

**Level B (required in README):**

- Default features vs demo/ship: `cargo build --release --features demo` (or list
  `store-cockroach,embed-bge,…`).
- `lambo.example.toml` / `lambo.toml` + env override rules.
- Link to `dev-diary/notes/level-b-pluggability.md`.
- Never instruct users to mix embedder models mid-session without re-embed.

**Done when:** a cold reader reproduces the demo from README alone (including features +
config), and the About section shows "MIT license".

---

### T9.2 — Architecture diagram
```yaml
requires:   nothing hard (spec §2 is stable) — final polish after T8.1
fixture-ok: n/a
owns:       docs/architecture.*
status:     not-started
```
Spec §12.4: "optional, cheap, do it." One diagram: RAM tier (graph, daemon, recall) over
the write-behind boundary, `GraphStore` / `Embedder` adapters fanning out (Level B:
features → registry → `dyn Trait`), MCP writer path vs read-only reader path (CockroachDB
MCP server), BGE/Bedrock at the edge. Embed in README.

**Done when:** it renders in the README and matches what was actually built (not the spec's
aspirations), including feature-gated adapters.

---

### T9.3 — Video ★★
```yaml
requires:   T8.4 (rehearsed), T8.5
fixture-ok: n/a
owns:       dev-diary/evidence/video/, demo/script.md
status:     not-started
```
Under 3 minutes, memory layer visibly at work (spec §12.4). Script beats (spec §13): the
derive montage → `canonization_events` filling → Agent B's recall with the ⚑ warning →
"Agent B does not make the breaking change" → split-screen CockroachDB MCP query. Show
`provision.sh` for the ccloud story (spec §12.1). Answer "why a graph instead of top-k" in
one screen — the recall context block *is* that screen. Write the script before recording;
record the terminal, not slides.

**Done when:** exported, under 3:00, uploaded, link tested logged-out.

---

### T9.4 — Lambda canonization sweep (optional — first thing cut)
```yaml
requires:   T3.2, T6.4
fixture-ok: no
owns:       lambda/
status:     not-started
```
Spec §12.2 optional: scheduled sweep against the store for sessions with no active writer.
**#1 in the cut order — do not start while anything above is open.** If built: a small
binary re-running T6.x predicates via SQL, EventBridge-scheduled.

**Done when:** a session with no writer gets a canonization transition from the sweep,
logged in `canonization_events`.

---

### T9.6 — Swarm benchmark & showcase (optional — cut order #2, behind T9.4)
```yaml
requires:   T8.2 (R4 CLEAN), P8 exit "surface holds under concurrency"; soft: T8.5
fixture-ok: yes (fixture embedder; store = memory or live)
owns:       dev-diary/evidence/swarm/, bench/
status:     not-started
parallel:   yes — separate hardware, off the submission critical path
```
The headline "lambo beyond coding agents" evidence: a swarm of small local agents
(LiquidAI **LFM2.5-230M** under llama.cpp/vLLM) driven concurrently against one `lambo`
session over MCP, producing the numbers that back the swarm claim — sustained tasks/hour,
canonization dedup rate (duplicate observations collapsed to canonical nodes), and
`reserve` coordination (no double-work). **Target rig: the 12 GB RTX 4070 desktop** (vLLM
or SGLang for continuous batching — check LFM2 support first), not the MBP: this test is
throughput-bound and wants the ~16–32 concurrent headroom. Measured baseline on an 18 GB
M3 Pro for reference: GPU knee at concurrency ~4–8, ~650 tok/s aggregate, memory a
non-issue (~12 KB/token KV).

**Non-blocking by construction.** If it succeeds it strengthens the README/video benchmark
story and can drive the T8.5 swarm view; **if it fails it feeds diagnosis of the swarm
claims** and is cut without touching the submission. Do NOT start while any required
deliverable (T9.1/T9.2/T9.3/T9.5) is open.

**Done when:** a concurrent LFM2.5-230M swarm runs against one session with a dedup-rate
and tasks/hour figure captured, OR it is deliberately cut with the diagnosis logged.

---

### T9.5 — Devpost submission
```yaml
requires:   T9.1, T9.3
fixture-ok: n/a
owns:       dev-diary/notes/devpost.md
status:     not-started
```
The form itself, drafted in-repo first: repo URL (public, MIT About-visible), demo app URL
(T8.5, live), video link, written CockroachDB-tools + AWS-services identification (lift
from T9.1), team/eligibility fields. Submit **hours** before 5:00 pm ET, not minutes —
Devpost under deadline load is a known failure mode. Confirmation screenshot into
`dev-diary/evidence/`.

**Done when:** submission shows as received on Devpost and the screenshot is committed.

---

## Exit criteria — the spec §12.4 checklist, verbatim

- [ ] Public repo, MIT license detectable in About
- [ ] README with setup/run instructions + single-writer constraint stated
- [ ] Functional demo app URL
- [ ] Video under 3 minutes showing the memory layer at work
- [ ] Written identification of CockroachDB tools and AWS services used
- [ ] Architecture diagram
- [ ] **Submitted before Tue Aug 18, 5:00 pm ET**

---

## Handoff Log

> _Fill on completion. Last entry in this diary: what would v0.7.0's first day want to know?_

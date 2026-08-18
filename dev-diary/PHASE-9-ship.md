# P9 — Ship

```yaml
id:       P9
branch:   phase/p9-ship
requires: [P8]
parallel: high   # T9.1 ‖ T9.2
```

**Goal:** Ship Lambo with evidence.

---

### T9.1 — README & repo hygiene
```yaml
requires:   T8.1; soft T8.8   # links into the reference docs T8.8 produces
fixture-ok: n/a
owns:       README.md, docs/  EXCEPT docs/reference/ (owned by T8.8)
status:     DONE. Merged to main 2026-08-15 as 7b0a9f8 (branch task/t9.1-docs, 5 commits)
            and long since pushed; main == origin/main.
            README + site rewritten binary-first; `lambo demo` promoted to a documented
            front door; license corrected to Apache-2.0; the false AWS/Bedrock claim
            removed.
            THE "DO NOT PUSH" BLOCKER IS DISCHARGED (2026-08-17): v0.1.0, v0.2.0 and
            v0.2.1 are published on GitHub Releases, so the install path the docs
            describe resolves.
            GitHub About license CONFIRMED 2026-08-17: `gh repo view` reports
            isPrivate=false and licenseInfo=apache-2.0. Spec §12.4's "MIT" is stale.
            The AWS half of the README is no longer "None yet": D3 replaced it with a
            six-service table (deployment-and-submission.md).
            No residuals. The clean-machine / container reproduction in "Done when" was
            retired 2026-08-17 (owner's call): the install path is exercised by the
            published releases, so re-verifying it in a container tracks nothing. Do not
            re-open it from the "Done when" wording below.
            Crossed into T8.8's docs/reference/ and into site/.
```
Setup and run instructions (clone → provision → serve → demo, verified on a clean machine
or container); **the single-writer constraint stated** (spec §12.4); the CockroachDB tools
and AWS services used, written out (that written identification is itself a deliverable);
credit to the v0.6.0 design doc for honesty (spec §12.4 note); MIT license visible in the
GitHub About sidebar (add the license file mapping if GitHub doesn't auto-detect). Repo
public.  **CORRECTION 2026-08-15: the repo is Apache-2.0, not MIT.**

**Boundary with T8.8:** this is the getting-started / onboarding path. The per-surface
**reference** (MCP tools, CLI verbs, `Memory` API, config keys, end-to-end) lives in
`docs/reference/` and is owned by **T8.8** — link into it, do not restate it here.

**Level B (required in README):**

- Default features vs demo/ship: `cargo build --release --features demo` (or list
  `store-cockroach,embed-bge,…`). **AMENDED 2026-08-15** — `demo` is no longer the release
  profile (T8.9 introduced `ship`), and the deployment story is now prebuilt binary +
  `lambo.toml`. The README states the features/config split in prose and links the build
  command on the site instead of carrying it.
- `lambo.example.toml` / `lambo.toml` + env override rules.
- Link to `dev-diary/notes/level-b-pluggability.md`.
- Never instruct users to mix embedder models mid-session without re-embed.

**Done when:** a cold reader reproduces the demo from README alone (including features +
config), and the About section shows the license.  **The repo is Apache-2.0, not MIT.**

---

### T9.2 — Architecture diagram
```yaml
requires:   nothing hard (spec §2 is stable) — final polish after T8.1
fixture-ok: n/a
owns:       docs/architecture.*
status:     DONE (2026-08-17 reconciliation), but NOT at the path this task reserved.
            The diagram shipped inside T9.1's README rewrite instead: a mermaid
            flowchart in README.md §Architecture (agent surfaces → single-writer
            process → write-behind → memory/sqlite/cockroach, with serve-web's
            read-only path drawn separately), mirrored on the site's End-to-end page.
            `docs/architecture.*` does not exist and is not needed.
            The submission page already claims this condition Met
            (site/src/content/docs/hackathon.mdx).
            Known residual, deliberately not chased: the diagram does not draw the
            Level B feature → registry → `dyn Trait` fan-out. That is carried in prose
            in README §Pluggable backends. If the video wants it visual, that is the
            one edit worth making.
```
Spec §12.4: "optional, cheap, do it." One diagram: RAM tier (graph, daemon, recall) over
the write-behind boundary, `GraphStore` / `Embedder` adapters fanning out (Level B:
features → registry → `dyn Trait`), MCP writer path vs read-only reader path (CockroachDB
MCP server), BGE/Bedrock at the edge. Embed in README.

**Done when:** it renders in the README and matches what was actually built (not the spec's
aspirations), including feature-gated adapters.

---

### T9.3 — Swarm benchmark & showcase (optional)
```yaml
requires:   T8.2 (R4 CLEAN), P8 exit "surface holds under concurrency"; soft: T8.5
fixture-ok: yes (fixture embedder; store = memory or live)
owns:       evidence/swarm/, bench/
status:     SATISFIED 2026-08-18 by the C-series C5 work, not by a separate run.
            Swarms ran concurrently against one session over MCP with tasks/hour and
            dedup captured, on more than one model, and the models that could not
            drive the surface at all were diagnosed rather than left hanging.
            Evidence: evidence/swarm/ (+ probes, runbook, portal visuals).
            Two things this does NOT cover, stated so nobody reads more into it:
            `reserve` coordination / no-double-work was never exercised by any swarm
            run, and the highest tasks/hour figures come from a fallback loop that
            hardcodes the call sequence, so those measure loop throughput rather than
            model-chosen work. The model-chosen run is the agentic one
            (scripts/loadtest/mcp_agentic.py): 1120 completed tasks/hour, dedup 0.857.
parallel:   yes — off the submission critical path
```
The headline "lambo beyond coding agents" evidence: **a swarm of small local agents
driven concurrently against one `lambo` session over MCP**, producing the numbers that
back the swarm claim — sustained tasks/hour and canonization dedup rate (duplicate
observations collapsed to canonical nodes).

The requirement is the swarm, not any particular model or rig. Any small local model
that can drive the MCP surface counts, on whatever hardware is to hand; a model that
turns out to be unable to drive it is a finding, not a blocker, and the next model is
tried. Whatever runs, the machine and the model go in the artifacts, because
throughput and starvation numbers are meaningless without them.

**What ran (2026-08-18, C-series C5):** LFM2-350M and functiongemma-270m were probed
and cannot emit tool calls at all, under either an agent harness or the raw OpenAI
tools API — logged with transcripts. Qwen3-0.6B can, and drove three concurrent agents
against one session: 1120 completed tasks/hour with the model choosing every call,
dedup 0.857, durability exact after SIGTERM. Fallback-loop throughput figures for both
LFM2-350M and Qwen3-0.6B are recorded separately and labeled as loop throughput.

**Non-blocking by construction.** If it succeeds it strengthens the README/video benchmark
story and can drive the T8.5 swarm view; **if it fails it feeds diagnosis of the swarm
claims** and is cut without touching the submission.

**Done when:** a concurrent swarm of small local agents runs against one session with a
dedup-rate and tasks/hour figure captured, OR it is deliberately cut with the diagnosis
logged. **Met** — see status above.

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
requires:   T8.1; soft T8.8   # links into the reference docs T8.8 produces
fixture-ok: n/a
owns:       README.md, docs/  EXCEPT docs/reference/ (owned by T8.8)
status:     drafted 2026-08-15 on branch task/t9.1-docs (6194b55, worktrees/t9.1-docs),
            UNMERGED. README + site rewritten binary-first; `lambo demo` promoted to a
            documented front door. Two "Done when" checks still outstanding: the
            clean-machine/container repro and the GitHub About "MIT license" confirmation.
            Crossed into T8.8's docs/reference/ and into site/ — see Handoff Log.
```
Setup and run instructions (clone → provision → serve → demo, verified on a clean machine
or container); **the single-writer constraint stated** (spec §12.4); the CockroachDB tools
and AWS services used, written out (that written identification is itself a deliverable);
credit to the v0.6.0 design doc for honesty (spec §12.4 note); MIT license visible in the
GitHub About sidebar (add the license file mapping if GitHub doesn't auto-detect). Repo
public.

**Boundary with T8.8:** this is the getting-started / onboarding path. The per-surface
**reference** (MCP tools, CLI verbs, `Memory` API, config keys, end-to-end) lives in
`docs/reference/` and is owned by **T8.8** — link into it, do not restate it here.

**Level B (required in README):**

- Default features vs demo/ship: `cargo build --release --features demo` (or list
  `store-cockroach,embed-bge,…`). **AMENDED 2026-08-15** — `demo` is no longer the release
  profile (T8.9 introduced `ship`), and the deployment story is now prebuilt binary +
  `lambo.toml`. The README states the features/config split in prose and links the build
  command on the site instead of carrying it. See Handoff Log.
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

### T9.1 — README & docs site, binary-first (2026-08-15, branch `task/t9.1-docs`)

**Not merged.** Two commits on `task/t9.1-docs` from `db387fd`: `4f0b149` (README + site
binary-first) and `6194b55` (demo promoted to a documented front door). Eleven files.
Merge with `git merge --no-ff task/t9.1-docs`.

#### The ruling that shaped everything

**Prebuilt binary + `lambo.toml` is the documented deployment path.** Owner's call,
2026-08-15. Cargo is now the path you take to hack on Lambo, not the path you take to run
it. Every page that opened with `git clone && cargo build` was rewritten to open with the
install script. The build-from-source material was not deleted, only demoted to a closing
section of `installation`.

#### What was wrong before this task

- **The live site never got T8.9's prebuilt-binary section.** `docs/reference/installation.mdx`
  had it; `site/src/content/docs/installation.mdx` did not. The site pages are hand-ported
  copies (component imports + `/lambo/` link rewriting), so a content edit to
  `docs/reference/` does **not** reach the site by itself. **Port both, every time.**
- **Starlight edit links pointed at `phase/p8-surface`**, merged and dead since `e0fcc91`.
  Fixed to `main` in `site/astro.config.mjs`. `docs.yml` also still triggered on that
  branch; now `main` only.

#### Ownership crossings — flagged per the repo rule

- **`docs/reference/` is T8.8's**, and T9.1 wrote to `installation.mdx`, `index.mdx`, and
  `end-to-end.mdx` there. Unavoidable: the site copies are generated from those files, and
  leaving them cargo-first would have re-introduced the drift on the next port.
- **`site/` has no owner in any phase doc.** It was built under T8.5/T8.8 without an `owns`
  line. T9.1 treated it as part of the README deliverable. **P9 should assign it.**

#### `lambo demo` is a product surface now, not a video prop

Owner's call, 2026-08-15, reversing an earlier suggestion to feature-gate it out of the
shipped binary. Verified before documenting: `lambo demo` runs with **no config file at
all**, against the in-memory store, in a few seconds, and two runs match.

Consequences the next agent must respect:

- **The golden constants are a public contract.** 12 interactions, 27 concepts, `user
  schema` at blast radius 9 are documented on the demo page as claims a reader checks.
  Changing scoring or canonization tuning now breaks a documented promise, not just a test.
- **`src/cli/demo.rs` is not feature-gated** (`src/cli/mod.rs:9`, no `cfg`), so all ~1876
  lines ship in the `ship` binary and `lambo demo` is a permanent CLI verb. That is now
  intentional. Do not gate it.
- **Banner string updated** (`demo.rs:1466`): "Compressed for the video" → "Compressed for
  a fast run", since the feature outlives the video. `demo/LIVE-RUNBOOK.md`'s sample output
  was updated to match. `dev-diary/evidence/demo-live-{1,2}.txt` were **left alone** as
  historical captures, so they now differ from a fresh run by that one line.
- **Still worth doing post-submission:** lift `wait_until` / `quiesce` / `settle_gc_survived`
  (`demo.rs:1219/1091/1039`) into `src/test_util.rs`. T9.6's swarm benchmark will want that
  fixed-point protocol. No user-visible effect, so it can wait.

#### Deliverable status against spec §12.4

Written identification of CockroachDB tools and AWS services is **done** in the README. One
correction made while writing it: `scripts/provision.sh` applies schema through docker or
psql, **not** ccloud, so the ccloud row credits cluster creation and DSN capture per
`notes/spike-runbook.md` and names provision.sh separately. Do not re-conflate them.

The **demo app URL is still missing** and is the one §12.4 deliverable with no owner making
progress. `lambo serve-web` has only ever run locally. T9.5 cannot be filed without it.

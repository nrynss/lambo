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
status:     MERGED to main 2026-08-15 as 7b0a9f8 (branch task/t9.1-docs, 5 commits).
            README + site rewritten binary-first; `lambo demo` promoted to a documented
            front door; license corrected to Apache-2.0; the false AWS/Bedrock claim
            removed. NOT PUSHED — see the release blocker in the Handoff Log before
            pushing, because the docs deploy fires on push to main.
            Outstanding "Done when": clean-machine/container repro, and the GitHub About
            license confirmation (Apache-2.0, NOT MIT — spec and this doc were stale).
            Crossed into T8.8's docs/reference/ and into site/ — see Handoff Log.
```
Setup and run instructions (clone → provision → serve → demo, verified on a clean machine
or container); **the single-writer constraint stated** (spec §12.4); the CockroachDB tools
and AWS services used, written out (that written identification is itself a deliverable);
credit to the v0.6.0 design doc for honesty (spec §12.4 note); MIT license visible in the
GitHub About sidebar (add the license file mapping if GitHub doesn't auto-detect). Repo
public.  **CORRECTION 2026-08-15: the repo is Apache-2.0, not MIT — see Handoff Log.**

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
config), and the About section shows the license.  **The repo is Apache-2.0, not MIT (Handoff Log).**

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
The form itself, drafted in-repo first: repo URL (public, Apache-2.0 About-visible — NOT MIT, see Handoff Log), demo app URL
(T8.5, live), video link, written CockroachDB-tools + AWS-services identification (lift
from T9.1), team/eligibility fields. Submit **hours** before 5:00 pm ET, not minutes —
Devpost under deadline load is a known failure mode. Confirmation screenshot into
`dev-diary/evidence/`.

**Done when:** submission shows as received on Devpost and the screenshot is committed.

---

## Exit criteria — the spec §12.4 checklist, verbatim

- [ ] Public repo, license detectable in About  ·  **Apache-2.0, not MIT (Handoff Log)**
- [ ] README with setup/run instructions + single-writer constraint stated
- [ ] Functional demo app URL
- [ ] Video under 3 minutes showing the memory layer at work
- [ ] Written identification of CockroachDB tools and AWS services used  ·  **AWS count is currently ZERO — §12.2 requires one (Handoff Log)**
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

#### The getting-started path was broken, and why nobody caught it

**The in-memory store is scoped to one process, and no page said so.** That single
undocumented fact broke the quickstart end to end:

```
$ lambo --config lambo.toml derive --session demo --agent agent-a --content "user schema" --kind entity
derived 1 concept(s): 1 created, 0 matched existing
$ lambo --config lambo.toml recall --session demo --query "user schema"
                                     # empty. exit 0.
```

Each CLI verb is its own process, so with `kind = "memory"` the derive dies with the
process that made it and the next process starts from an empty graph. The page promised
"the recall returns the matching concept with its type and score". It returned nothing, and
exit code 0 made it look like a silent product failure rather than a config mistake.

This predates T9.1 — the original quickstart had the same memory-store flow. The binary-first
rewrite kept it. It survived because every automated test drives one process, and because
`lambo demo` genuinely does work on the memory store (one process, start to finish), so the
store looked fine everywhere anyone was looking.

**Fixed:** quickstart and both installation copies now use `kind = "sqlite"` with a
`provision` step, and carry the real captured output (`user schema [Entity] (score 0.29)`).
The installation first-run also stopped recalling from a session the server holds, which
would have hit the writer lease. Both `config` pages now state the per-process scoping
directly.

**Rule for anyone writing docs here:** if a documented sequence spans more than one `lambo`
invocation, it needs `sqlite` or `cockroach`. `memory` is only correct inside a single
process, which means one `serve` with agents attached to it, or `lambo demo`.

#### BLOCKER — the documented install path does not exist yet

`gh release list` is **empty** and `git tag -l` is **empty**. No release, no tag. So today:

- `curl .../releases/latest/download/install.sh | sh` (README headline, quickstart step 1,
  installation step 1) resolves to a **404**.
- `lambo demo`, now the README's see-it-work step, presupposes that install.
- Every prebuilt-binary claim on the site is written against artifacts nobody has published.

T8.9 built the machinery and it is CLEAN, but nothing has triggered it. The workflow fires
on a version tag and asserts `tag == v$VERSION`, and `Cargo.toml` is at `0.1.0`, so the
trigger is `git tag v0.1.0 && git push origin v0.1.0`.

**This is why the T9.1 merge (7b0a9f8) was left unpushed.** `docs.yml` deploys the site on
push to `main`, so pushing publishes install instructions that fail until the tag lands.
**Push the tag first, or push both together.** Owner decision either way.

#### Two places the spec is now wrong about this repo — READ BEFORE FILING T9.5

Both were caught by the owner on review of the T9.1 draft, after the draft had asserted the
spec's version as fact. The repo is the truth here, not spec §12.

**1. The license is Apache-2.0, not MIT.** `LICENSE` is the Apache 2.0 text, `Cargo.toml`
declares `license = "Apache-2.0"`, and a `NOTICE` file exists. Spec §12.4 and this phase
doc's own exit checklist both say MIT, in four places. Those are **stale**. The README now
says Apache-2.0 and links `LICENSE` + `NOTICE`. The About-sidebar check is still worth doing,
but it is checking for "Apache-2.0".

**2. Zero AWS services are used.** Not "Bedrock pending authorization" — there is **no
Bedrock adapter at all**. `src/embed/` contains only `bge_m3.rs`, `fixture.rs`, `math.rs`,
`mod.rs`. `src/embed/mod.rs:272` returns "BedrockEmbedder is not implemented yet (T7.1)",
and `EmbedderKind::Bedrock::available()` is hardcoded `false` (`mod.rs:101`). The
`embed-bedrock` Cargo feature exists and pulls the SDK deps, which makes the tree *look*
AWS-wired to a reader who does not open `src/embed/`. **It is not.** `ship` excludes it.

The T9.1 draft's "AWS services used" table claimed the adapter shipped. That was false and
is corrected: the README section now says "None yet" and explains the reserved feature.

**This is a submission-eligibility problem, not a docs problem.** Spec §12.2 requires **one**
AWS service. The count is currently zero, and P7's handoff has T7.1 blocked on an external
authorization that has not moved since Aug 11 (`notes/bedrock-authorization-blocker.md`).

**Cheapest path that fixes two deliverables at once:** host `lambo serve-web` on AWS. It is
a read-only single binary against the existing cluster, so App Runner, Lightsail, or a small
EC2 box all work. That produces the **functional demo app URL** (§12.4, still missing, still
unowned) *and* the **one AWS service used** (§12.2) from one afternoon of work. It does not
depend on Bedrock authorization arriving. Recommend doing this before T9.3 records, so the
video can show the hosted URL.

#### Deliverable status against spec §12.4

Written identification of CockroachDB tools is **done** in the README. One correction made
while writing it: `scripts/provision.sh` applies schema through docker or psql, **not**
ccloud, so the ccloud row credits cluster creation and DSN capture per
`notes/spike-runbook.md` and names provision.sh separately. Do not re-conflate them.

The AWS half of that same deliverable is written but currently reads "none", per above.

**Claims audited at merge time**, after two spec-sourced errors got through the first draft:

| Claim | Verdict |
|---|---|
| Seven MCP tools | TRUE — `lambo_{derive,recall,record_action,reserve,inspect,stats,saints}` |
| Distributed vector indexing | TRUE — `CREATE VECTOR INDEX` in `migrations/cockroach/001_init.sql`, camera-proof PASSING in evidence |
| Single-writer lease | TRUE — `serve_single_writer_lease` + `cli_write_lease` green |
| Apache-2.0 license | TRUE — corrected this round |
| Bedrock adapter | **FALSE, removed** — no implementation exists |
| Prebuilt binaries | **NOT YET TRUE** — no release published, see blocker above |
| MCP client interop | TRUE and **broader than first documented** — Claude Code 2.1.226 (handshake + all seven tools over stdio), **OMP v17.3.4 + DeepSeek Flash driving `derive`/`recall`/`stats` autonomously against live Cockroach**, and Pi 0.84.1 once `pi-mcp-adapter` is installed. The model-driven leg is OMP's; Claude Code's own evidence marks it NOT VERIFIED. Docs originally named only Claude Code and understated this. |
| Demo golden numbers | TRUE on a durable store — re-ran against SQLite: 12 interactions, 27 concepts, blast radius 9, score 2.27, identical to the memory-store run. Cockroach untested (no DSN here). |
| Quickstart write-then-read | **WAS BROKEN, FIXED** — see below |
| CLI verbs and flags | TRUE — every subcommand and flag the docs name exists in the binary |
| Release matrix and asset names | TRUE — 5 targets, `lambo-<version>-<name>[.exe]` + `.sha256`, matching the docs |
| Ports 7700 / 7710 | TRUE — clap defaults |
| serve-web is read-only | TRUE and **test-enforced** — `serve_web.rs:1535` greps its own production source for `Memory::builder`, `open_writer`, `acquire_lease`, `.spawn()` |
| Managed MCP server | **UNEVIDENCED** — console-side setup recorded DONE 2026-08-13, but the split-screen `canonization_events` query was never rehearsed and no screenshot reached `dev-diary/evidence/`. It is one of the **two required** §12.1 tools, so the README asserts it while nothing in the repo backs it. Either capture the evidence during the T9.3 recording or soften the claim. |

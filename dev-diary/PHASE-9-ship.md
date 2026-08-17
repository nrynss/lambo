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
status:     DONE. Merged to main 2026-08-15 as 7b0a9f8 (branch task/t9.1-docs, 5 commits)
            and long since pushed; main == origin/main.
            README + site rewritten binary-first; `lambo demo` promoted to a documented
            front door; license corrected to Apache-2.0; the false AWS/Bedrock claim
            removed.
            THE "DO NOT PUSH" BLOCKER IS DISCHARGED (2026-08-17): v0.1.0, v0.2.0 and
            v0.2.1 are published on GitHub Releases, so the install path the docs
            describe resolves. The Handoff Log section that records the blocker is
            history, not current state.
            GitHub About license CONFIRMED 2026-08-17: `gh repo view` reports
            isPrivate=false and licenseInfo=apache-2.0. Spec §12.4's "MIT" is stale.
            The AWS half of the README is no longer "None yet": D3 replaced it with a
            six-service table (deployment-and-submission.md).
            No residuals. The clean-machine / container reproduction in "Done when" was
            retired 2026-08-17 (owner's call): the install path is exercised by the
            published releases, so re-verifying it in a container tracks nothing. Do not
            re-open it from the "Done when" wording below.
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

### T9.3 — Video ★★
```yaml
requires:   T8.4 (rehearsed), T8.5
fixture-ok: n/a
owns:       evidence/video/, demo/script.md
status:     FOOTAGE CAPTURED 2026-08-17, not yet cut. Eight raw takes in
            `evidence/video-raw/` (gitignored); see notes/video-shoot.md for the
            shot list, the three things the narration must not overclaim, and the
            capture rig. Editing, voiceover and upload remain.
            CORRECTION 2026-08-17: "screen capture fails silently on this machine"
            was true of the XWayland/x11grab path only. OBS on the KDE PipeWire
            portal works; the takes above were shot that way. See
            notes/video-shoot.md for the four rules that make it reliable, chiefly
            "pick the window, not the screen" and "verify frames by looking at them".
            `05-guard.mkv` is a real live run of `03_crossover_protect.py` against
            the exhibit, which closes the note that the committed capture
            (evidence/remed-t8-crossover-run.md) was a synthetic recapture.
            Still to do: cut to under 3:00, record voiceover, upload, test the link
            logged out. D1 is not a blocker for editing, but the portal take shows a
            newer build than the deployed site (see the note).
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
status:     not-started, and still cut-order #1, so it stays that way while T9.3 and
            T9.5 are open.
            **Do not read the exhibit's Lambda as this task.** A `python3.12` arm64
            Function URL serving read-only stats over the live session was built under
            remediation T10 and is listed in the README's AWS table. It is a stats
            endpoint. It runs no canonization predicate and writes no
            `canonization_events` row, so T9.4's "Done when" is untouched by it.
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
owns:       evidence/swarm/, bench/
status:     not-started. Cut-order #2, behind T9.4.
            Worth knowing when it is scheduled or cut: P8's one open exit criterion is
            the concurrency leg (T8.2 N1/N2), which has no evidence capture. That is
            the correctness half and T9.6 is the scale half, so cutting T9.6 leaves the
            correctness box open too, so cut it with that stated, not silently.
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
status:     not-started. `dev-diary/notes/devpost.md` does not exist.
            The *content* is largely written and verified live: D3 (2026-08-17) put the
            condition-by-condition table on site/src/content/docs/hackathon.mdx and the
            CockroachDB-tools and six-service AWS identification in README.md. T9.5 is
            now the act of drafting that into the form's own fields and submitting it,
            not fresh writing.
            `requires: T9.1, T9.3` still binds: T9.1 is done, T9.3 is not, so the video
            link is the one field that cannot be filled today.
```
The form itself, drafted in-repo first: repo URL (public, Apache-2.0 About-visible — NOT MIT, see Handoff Log), demo app URL
(T8.5, live), video link, written CockroachDB-tools + AWS-services identification (lift
from T9.1), team/eligibility fields. Submit **hours** before 5:00 pm ET, not minutes —
Devpost under deadline load is a known failure mode. Confirmation screenshot into
`evidence/`.

**Done when:** submission shows as received on Devpost and the screenshot is committed.

---

## Exit criteria — the spec §12.4 checklist, verbatim

Checked against the repo and the running exhibit on **2026-08-17**, not against
the plan. Each box names what was verified, so the next reader can re-check it
rather than trust it.

- [x] Public repo, license detectable in About  ·  `gh repo view` → `isPrivate: false`,
      `licenseInfo: apache-2.0`. **Apache-2.0, not MIT. Spec §12.4 is stale here**
- [x] README with setup/run instructions + single-writer constraint stated  ·  T9.1;
      install → `lambo demo` → `lambo.toml`, and the constraint is stated in
      README §Deployment model and on the site's End-to-end page
- [x] Functional demo app URL  ·  `https://lambo.nryn.dev` → 200, `/healthz` → `ok`,
      `/api/stats` → session `cloudops-exhibit`, 113 nodes / 485 edges / 41 concepts /
      1 canonical / 7 canonization events, `mode: reader`. Re-verified 2026-08-17.
      A Lambda Function URL serves the same session
- [ ] Video under 3 minutes showing the memory layer at work  ·  **the open one.**
      T9.3 / D2, blocked on D1
- [x] Written identification of CockroachDB tools and AWS services used  ·  README
      §CockroachDB tools used, and §AWS services used, which is **six** services
      (EC2, VPC, Secrets Manager, Lambda, RDS for PostgreSQL, IAM), each exercised by
      the running exhibit. The earlier "AWS count is ZERO" note below is history:
      it was true on 2026-08-15 and was closed by the CloudOps build on 2026-08-16
- [x] Architecture diagram  ·  T9.2, mermaid in README §Architecture plus the site
      topology. Not at `docs/architecture.*`; that path was never created
- [ ] **Submitted before Tue Aug 18, 5:00 pm ET**  ·  T9.5, not started; gated on the
      video link

---

## Handoff Log

> _Fill on completion. Last entry in this diary: what would v0.7.0's first day want to know?_

### Reconciliation of this doc against the tree (2026-08-17, at `1dd5b48`)

This phase doc had gone stale in a specific and dangerous way: the parts that
were *wrong* were the parts a reader treats as current state, while the parts
that were *right* were the dated history below. Statuses, the exit checklist and
the T9.1 block are now corrected. **Nothing dated below this entry was
rewritten.** Those entries record what was true when they were written, which
is the whole point of keeping them.

Two sections below are superseded and must not be acted on:

- **"BLOCKER — the documented install path does not exist yet"** is closed.
  `gh release list` shows `v0.1.0` (2026-08-16), `v0.2.0` and `v0.2.1`
  (2026-08-17); `Cargo.toml` is at `0.2.1`; `main` is pushed and equals
  `origin/main`. The install line the docs print resolves.
- **"Zero AWS services are used"** is closed, though not the way that section
  predicted. It recommended hosting `serve-web` on AWS to satisfy §12.2, and
  that is roughly what happened, but larger: two autonomous agents provisioned
  real `us-east-1` infrastructure on 2026-08-16, one moved to delete the
  security group behind the other's RDS instance, and Lambo blocked it. Six
  services are now identified in the README. **Bedrock is still not implemented**:
  `src/embed/` has no adapter and T7.1 remains blocked on the authorization
  request, so the honest boundary both the README and the hackathon page state
  is that AWS runs *around* Lambo, not inside it. Do not soften that.

What changed in the status lines: T9.1 **DONE**, no residuals (pushed; About
license confirmed Apache-2.0 by `gh repo view`; the clean-machine repro was
retired as noise, owner's call). T9.2 **DONE**, but delivered as a mermaid
diagram in `README.md`
rather than at the reserved `docs/architecture.*` path, which does not exist.
T9.3 and T9.5 remain **not started** and are the only two open §12.4 boxes.
T9.4 and T9.6 remain cut-order #1 and #2.

Three things a cold reader would otherwise get wrong:

1. **The exhibit's Lambda is not T9.4.** It is remediation T10's read-only stats
   Function URL. The canonization sweep does not exist.
2. **T9.5's writing is mostly done, filed elsewhere.** D3 put the
   condition-by-condition table on the site's hackathon page and the tool/service
   identification in the README. `dev-diary/notes/devpost.md`, which T9.5 owns,
   has still never been created.
3. **`site/` still has no owner in any phase doc.** T9.1's entry below asked P9
   to assign it and P9 never did. It has since been edited by D3 as well. Anyone
   claiming a task that touches it is crossing into unowned ground and should say
   so in their handoff rather than assume.

The work that closed between P8 and here is not in this doc at all, by design:
remediation T1–T12 plus the E2E review, hardening H1–H7, and deployment D1–D3
live in `notes/`. The dev-diary README's status board now points at all three.

### Evidence promoted to top level, three-client MCP agreement (2026-08-15, `67d5064` on `main`)

**Pushed to `main`, which publishes the docs site.** 79 files. The substance is two new
captures and one structural move.

#### Evidence now lives at `evidence/`, not `dev-diary/evidence/`

Owner's call: evidence is the record behind the claims, and burying it in a working journal
undersells it. Moved to a top-level `evidence/` with an index README.

`dev-diary/evidence` is now a **symlink** to `../evidence` (git mode 120000). This was
deliberate over the two alternatives:

- *Repointing the ~52 references* inside dated handoffs and adversarial reviews would edit
  documents that record what was true at review time.
- *Copying the tree* would leave two copies to drift apart.

The symlink keeps one source of truth and every old path resolving. If you ever flatten it,
those 52 references are the thing to check.

Directories and files carrying task numbers were renamed (`t8.2-mcp-client/` →
`mcp-client-stdio/`, `live-review-t8.2-t8.3/` → `live-review-cockroach/`, and so on).
**Identifiers inside captured transcripts were left alone** — editing a capture so it reads
better stops it being a capture. That rule is now written into `evidence/README.md`.

#### Three clients now agree on the managed-MCP walk

OMP, Claude Code, and Cursor Agent CLI have each driven the CockroachDB managed MCP server
model-first and returned the same five rows for the same session: `user schema`
(`724c92b9`) going Candidate → Venerable → Canonical over 1.29s, `blast_radius` null on the
earlier hops and **9** on the promotion. They agree on every field.

That 9 is also what the demo prints in its final recall block, so the narrated number and
the durably stored number now match from opposite ends. Transcripts in
`evidence/mcp-client-interop/`.

One honest detail preserved: Cursor's node-label query returned rows in a different order
than OMP's. The query has no `ORDER BY`, so no order is owed and the id→content mapping is
identical. Recorded as expected rather than quietly reordered.

#### Two traps worth more than the captures

**Nested `claude -p` cannot authenticate, and re-authenticating does not fix it.** Chased
this properly instead of retrying: it fails on `claude -p "say hi"` too, and still fails
with `CLAUDE_CODE_CHILD_SESSION`, `ANTHROPIC_BASE_URL` and the messaging-socket vars
stripped. Under a host-managed session `~/.claude/.credentials.json` holds an access token
with **no refresh token**, so a standalone CLI process has nothing to refresh with.
Non-model commands like `claude mcp list` are unaffected, which is why the handshake looked
healthy while every model-driven attempt died. The fix was to stop shelling out and use the
session's own tool roster.

**Cursor's per-call MCP approval gate is specific to `-p` print mode.** The earlier note
said `--force` is required, full stop. Re-checked against a re-authenticated account: in
print mode every call is still auto-rejected (`User rejected MCP: lambo-lambo_derive`), but
running `cursor-agent` interactively approves normally and needs no `--force` at all. So
`--force` is the scripting workaround, not a general requirement — and it is the only path
that needs bounding, because the CockroachDB toolset ships `create_database`, `create_table`
and `insert_rows` alongside the read-only tools.

#### A cluster id was one commit away from being public

`PHASE-9-ship.md` had the `mcp-cluster-id` UUID in plaintext in an **uncommitted** edit —
the same id `.mcp.json` is gitignored to protect. Not in `HEAD`, so it was caught before
publication and replaced with a pointer to the gitignored file. Worth a habit: the secret
scan needs to cover the diary, not only `evidence/` and the site.

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
  was updated to match. `evidence/demo-live-{1,2}.txt` were **left alone** as
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

#### DONE 2026-08-15 — managed-MCP evidence captured

Captured via **OMP**, not Claude Code — see `evidence/managed-mcp-canonization-events.md`
for the transcript, the exact re-run command, and the two gotchas (query `database=lambo` or you
hit `defaultdb`; OMP needs `INFERX_API_KEY` from `~/.zshrc` to boot).

#### ALSO DONE 2026-08-15 — Claude Code OAuth completed, second client captured

The OAuth problem described below is **fixed**. `claude mcp list` reports
`cockroachdb-cloud: ✔ Connected`, the `mcp__cockroachdb-cloud__*` tools land in an
interactive session's roster, and a model-driven run through them
(`list_clusters` → `list_databases` → `list_tables` → `select_query`) returned the identical
five rows OMP had returned for the same session. Transcript:
`evidence/mcp-client-interop/claude-code-model-driven-managed-mcp.txt`.

One trap worth keeping: **nested `claude -p` cannot do this and re-authenticating does not
help.** It fails on `claude -p "say hi"` too, and still fails with the child-session env
vars stripped. `~/.claude/.credentials.json` under a host-managed session holds an access
token with no refresh token, so a standalone CLI process has nothing to refresh with.
Non-model commands like `claude mcp list` are unaffected. Use an interactive session's own
tool roster, not a subprocess.

Original instruction retained below for the record.

#### NEXT ACTION — capture the managed-MCP evidence (session restarted 2026-08-15)

`cockroachdb-cloud` is **registered** in `~/.claude.json` and in a gitignored `.mcp.json`
(`https://cockroachlabs.cloud/mcp`, http transport, with an `mcp-cluster-id` header — read the
id from the gitignored `.mcp.json`, do not paste it into a tracked file). Run the query through
**those MCP tools, not psql** —
the point of the evidence is that the managed MCP server answered.

**Restarting the session is NOT sufficient — tried 2026-08-15, tools still absent.** What
`~/.claude.json` holds is the server *registration* (url + cluster-id header), not an access
token. `cockroachlabs.cloud/mcp` is a remote HTTP MCP server behind OAuth, and the harness
reports it under "servers require authentication before their tools can be used" until that
handshake completes. The handshake needs an **interactive** session: run `/mcp` (or
`claude mcp`) in a terminal `claude`, pick `cockroachdb-cloud`, complete the browser consent.
Only after the token is stored will a session pick up the tools. Do not record "authorized"
again on the strength of the config file alone — check that a `cockroach`-named tool actually
appears in the session's roster.

Query one of the three complete demo sessions (5 events each, `user schema` walking
Candidate to Venerable to Canonical):

```sql
SELECT node_id, from_status, to_status, blast_radius, occurred_at
FROM canonization_events
WHERE session_id = 'demo-rest-api-bdd69691-ea92-41b7-ad3a-7506332071dc'
ORDER BY occurred_at;
```

Scope by `session_id` always — the cluster also holds ~2833 seeded concepts and 240 events
across many sessions. Save the transcript to `evidence/` naming the MCP tool that
answered, which closes the one UNEVIDENCED row in the claim audit below and gives T9.3 its
split-screen beat.

**OMP is not a viable driver right now.** It crashes at startup: `Provider inferx: "apiKey"
or "oauth" is required when defining models`. OMP validates every registered provider before
booting, so setting the model role to deepseek does not help. Clear the custom `inferx`
provider or add it to `disabledProviders` first. A local `.mcp.json` for OMP is already
written and gitignored.

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
| MCP client interop | TRUE and **broader again** (2026-08-15) — now **three** clients: Claude Code 2.1.226 (handshake + all seven tools over stdio), **OMP v17.3.4 + DeepSeek Flash** driving `derive`/`recall`/`stats` autonomously against live Cockroach, and **Cursor Agent CLI 2026.08.11** (handshake, 7/7 tool discovery, and a model-driven derive → record_action → recall → stats run). Pi 0.84.1 also works once `pi-mcp-adapter` is installed. The **model-driven leg is now verified twice** (OMP and Cursor), closing the row T8.2 marked NOT VERIFIED. Evidence: `evidence/mcp-client-interop/`. |
| Demo golden numbers | TRUE on a durable store — re-ran against SQLite: 12 interactions, 27 concepts, blast radius 9, score 2.27, identical to the memory-store run. Cockroach untested (no DSN here). |
| Quickstart write-then-read | **WAS BROKEN, FIXED** — see below |
| CLI verbs and flags | TRUE — every subcommand and flag the docs name exists in the binary |
| Release matrix and asset names | TRUE — **4 targets** as of 2026-08-16, `lambo-<version>-<name>[.exe]` + `.sha256`, matching the docs. `x86_64-apple-darwin` was dropped: the macos-13 runner class never picked the job up across four tagged runs, and the release job needs the whole matrix, so one starved runner blocked publication. Apple silicon is covered by macos-arm64. |
| Ports 7700 / 7710 | TRUE — clap defaults |
| serve-web is read-only | TRUE and **test-enforced** — `serve_web.rs:1535` greps its own production source for `Memory::builder`, `open_writer`, `acquire_lease`, `.spawn()` |
| Managed MCP server | **TRUE, now evidenced** (2026-08-15) — `mcp__cockroachdb_cloud_select_query` returned the five-row `canonization_events` walk for the demo session off the live cluster, with `user schema` going Candidate → Venerable → Canonical at blast_radius 9. Transcript and cross-check in `evidence/managed-mcp-canonization-events.md`. **Caveat now lifted (same day):** the `cockroachdb-cloud` OAuth was completed for both Claude Code and Cursor, and each drove the same server model-first, returning the identical five rows for the same session. So **three** independent clients have now driven the managed server — OMP, Claude Code, Cursor — agreeing on every field. Transcripts: `evidence/mcp-client-interop/claude-code-model-driven-managed-mcp.txt` and `.../cursor-model-driven-managed-mcp.txt`. Bonus finding recorded there: Cursor's per-call MCP approval gate is specific to `-p` print mode, and interactive runs need no `--force`. |

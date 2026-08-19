# Dogfooding Lambo while building Lambo

Proposal, 2026-08-19. Not implemented — this document is the design; nothing below exists
until someone runs it.

**The question:** should the agents building this branch record and recall through a live
Lambo session, now that SQLite vector search (F) and a local BGE-M3 embedder both work on
this machine?

**The answer this doc argues:** yes — with one non-negotiable isolation rule, a pinned
binary, and honest expectations about what it does and does not prove.

---

## Why it is worth doing

1. **It is the real workload.** Every measurement so far drove Lambo with harnesses or
   scripted tasks. The development of this branch *is* the intended workload: multiple
   agents (implement, review, remediate, orchestrate) working one codebase across days,
   accumulating decisions that later agents need. If Lambo is not useful here, that is a
   finding worth having early.
2. **The blast-radius claim gets tested in anger.** This branch already produced the exact
   dependency shape the product exists for: F's write-gate property statement is inherited
   by workstream B; the `store.vector_dim` pin semantics are consumed by B4; the 1536 dim
   decision constrains B2's hnsw ceiling handling. Today those dependencies live in doc
   cross-references that an agent must happen to read. Recorded as edges, a B0 agent's
   recall of "quarantine" should surface the F property statement *with a warning that B
   rests on it*. Either it does — evidence — or it does not — a finding.
3. **It generates the calibration corpus G needs next.** G1 measured synthetic pair
   classes. A month of real derives from real development produces the score distribution
   that decides whether `RECENT_SCORE = 0.35` and `semantic_match_threshold = 0.85` hold
   up outside the lab. Same for the duplicate-creation rate under the kept 0.85 threshold —
   the honest miss G2 recorded (`register user` / `create account` at 0.8230) predicts
   duplicates; dogfooding counts them.
4. **It exercises G2's change immediately.** The recalibrated floor ships in any binary
   built from HEAD; dogfooding is the first sustained consumer of the new blend.
5. **It is the docs-pilot topology at miniature.** The Endor docs pilot
   (`~/Documents/work/lambo-at-endor/`) proposes exactly this shape — one serve process,
   several people's agents, recall-before-write. Running it on ourselves first finds the
   friction before proposing it to a team.

## The isolation rule (non-negotiable)

**The serve process runs a *pinned* binary, never the working tree.** Agents on this
branch break the build on purpose (reviewers flip predicates; remediators run red tests).
If the memory server were `target/debug/lambo` from the tree under modification, a
mid-remediation rebuild could take down or corrupt the memory of the very cycle relying
on it. So:

- Build once from a chosen commit: `cargo build --release --features store-sqlite,embed-bge`.
- **Copy the binary out of `target/`** (e.g. `~/lambo-dogfood/bin/lambo-<sha>`) so
  `cargo clean` and rebuilds cannot touch it.
- Upgrading the dogfood server to a newer HEAD is a *deliberate event*: build, copy,
  restart, and note the sha in the session — which is itself dogfooding the upgrade path,
  including the first real test of "same store, newer binary" compatibility every release
  will need.

## Topology

Same law as everywhere else — **one `lambo serve` is the single writer**:

```
orchestrator (Claude Code session) ─┐
implement agent ────────────────────┤  MCP ──► lambo serve  (pinned binary)
review agent ───────────────────────┤            │ sqlite store + bge_m3 embedder
remediation agent ──────────────────┘            ▼
                                     ~/lambo-dogfood/lambo-dev.db
```

- **Store lives outside the repo** (`~/lambo-dogfood/`). This repo is public; the dogfood
  ledger accumulates development chatter that is not curated evidence. If a slice later
  earns a place under `evidence/`, it gets exported deliberately — never by living inside
  the checkout.
- Embedder: the existing rig (llama-server + `bge-m3-q8_0.gguf`, `127.0.0.1:8080`),
  contract `bge_m3 / 1024`. When A lands, switching the dogfood session to Gemini/1536 is
  a *new session*, not a config flip — the dim doc's own law.
- Wiring options, in order of preference: (a) the orchestrator session holds the one MCP
  connection and subagents inherit it — single writer is trivially true; (b) agents use
  read-only CLI verbs (`lambo recall`) plus the orchestrator recording on their behalf;
  (c) HTTP MCP if concurrent direct agent connections prove worth the plumbing. Start
  with (a).

## Protocol (the lambo-cloudops skill, scoped to this branch)

- **Recall before starting a workstream or touching shared design surface** — the dialect
  trait, the width authority, the quarantine semantics, CI rows.
- **Derive what was decided, not what was done**: decisions with a *why* (1536, hnsw from
  init, alias split fails loud), property statements, review findings that survived
  verification, constants and their calibration provenance. Git already records what was
  done; the graph records what the next agent must not re-derive — the same rule
  `dev-diary/README.md` states for handoffs.
- **record_action on merges**: workstream landed, review round closed, binary upgraded.
- Blocking warnings are blocking: an agent recalled into a load-bearing concept stops and
  surfaces it rather than editing through it.

Seed on day one (so recall has something to hit): the A–H workstream decisions already
recorded in this folder, the F property statements, and the G constants with their
evidence pointers.

## What it measures (write these down as they happen, not retrospectively)

> **Mechanism: workstream [I — Observability](I-observability.md)** (added 2026-08-19).
> Metrics 1, 2, 4 and 5 below are unmeasurable without I's serve call ledger; until it
> lands, Agent Governance audit-only logging and manual `lambo_stats` snapshots are the
> interim, and metric 5 (warnings fired) simply cannot be counted.

1. Recall-first compliance per agent cycle (the ledger records it; no self-reporting).
2. Re-derivation savings: times a recalled decision replaced re-reading a workstream doc.
3. Duplicate-creation rate under `semantic_match_threshold = 0.85` on real dev phrasing —
   G's predicted misses, counted.
4. Score distribution of real queries vs G1's synthetic bands — does 0.35 hold?
5. Blast-radius events: did a warning fire before an agent edited something another
   workstream rests on? (The B0 extraction is the first natural test.)
6. Friction, honestly: every time the protocol slowed a cycle down or the serve process
   needed babysitting.

## What this is not

- **Not evidence until exported.** Dogfood data is uncontrolled; claims made from it get
  the same adversarial treatment as everything else before they touch `evidence/` or the
  README.
- **Not a replacement for the review cycle.** Agents still work in worktrees, reviews
  still run, gates still gate. The graph is memory between cycles, not authority over
  them.
- **Not load-bearing.** If the serve process is down, development proceeds; agents fall
  back to reading the dev-diary. A dogfood outage is a logged finding, never a blocker.
- **No Endor-internal content in this store.** This machine hosts both worlds; the
  dogfood session is lambo-development memory only. The Endor pilot gets its own store,
  its own session, its own doc.

## Open questions — answered at standup (2026-08-19)

1. **Pin: `3039b82`** (has F's vector leg and G2's recalibrated floor). Binary at
   `~/lambo-dogfood/bin/lambo-3039b82`; config at `~/lambo-dogfood/lambo.toml`
   (sqlite at `~/lambo-dogfood/lambo-dev.db`, bge_m3 @ `127.0.0.1:8080`, dim 1024).
2. **Wiring: option (a)** — a user-scope stdio MCP registration (`claude mcp add --scope
   user lambo-dogfood -- <pinned binary> serve …`), so the orchestrator session holds the
   one connection and no project `.mcp.json` touches the public repo. Brief-injection
   stays the fallback if subagent recall proves clumsy. One session at a time: each
   session spawns its own serve, and while the write lease fences a stale one safely,
   two live sessions fencing each other is noise.
3. **Retention: persist** across the branch's life and past merge — the long-run state is
   the interesting data.
4. **Export: curated only.** The ledger informs nothing directly; slices earn their way
   into `evidence/` through the normal adversarial treatment.

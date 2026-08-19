# I — Observability: prove Lambo is working as intended

**Goal:** make every claim about Lambo's behaviour in live use *measurable from artifacts*,
not remembered from conversations.

**Why now, before further workstreams (decided 2026-08-19):** the dogfood rig is live and
the DOGFOOD.md measurement list is already half-unmeasurable. Lambo is strong on **state**
observability — `canonization_events` audits every promotion, `lambo_stats` reports
health, the store itself answers any post-hoc SQL question, `serve-web` renders it — and
weak on **flow**: nothing records the calls agents actually make. The JSONL ledgers in
`evidence/` were written by the loadtest *harnesses*; over MCP there is no serve-side
record of "agent X recalled, then derived, and a blast-radius warning rendered." Fired
warnings currently exist only in the conversation that received them — unverifiable
later, which by this repo's standards means unclaimable later. Every implementation cycle
that runs before I lands is dogfood data lost.

---

## What must become measurable (from DOGFOOD.md, mechanism per metric)

| DOGFOOD metric | Mechanism after I |
| --- | --- |
| 1. Recall-first compliance per agent | ledger: call sequence per agent_id (I1) |
| 2. Re-derivation savings | ledger `matched` counts + store SQL (I1, I3) |
| 3. Duplicate-creation rate under 0.85 | post-hoc cosine scan, scripted (I3) |
| 4. Real query scores vs G1 bands | ledger: recall scores persisted (I1, I3) |
| 5. Blast-radius warnings fired | ledger: warning-rendered flag per recall (I1) |
| 6. Friction | human notes (unchanged) |

---

## I1 — The serve call ledger

`lambo serve --ledger <path>`: append-only JSONL, one line per MCP tool call. Default
**off** — no behaviour change for anyone not asking.

Per line: server timestamp, `agent_id`, tool name, outcome (ok / error kind), duration,
and per-tool payload facts — for `derive`: concepts created / matched / demoted counts;
for `recall`: query, top-k hit ids with their **final scores and per-leg provenance**
(vector cosine, BM25, recent floor — the numbers G needs), and whether a canonical marker,
conflict line, or **blast-radius warning rendered**; for `record_action`: edge counts;
for `reserve`: grant/refusal.

Design constraints, decided here rather than discovered:

* **Observability must never take down memory.** The ledger write is a buffered append
  that logs its own failure once and drops lines rather than failing or delaying the tool
  call. A dropped-line counter goes into `lambo_stats` so silence is visible (no silent
  caps — the same rule the evidence culture already enforces).
* **The ledger is not the store.** No replay semantics, no reads from it in the serve
  path, no schema promises beyond "one JSON object per line, `v` field for versioning."
* Content parity with the store: concept text already lives in the database, so the
  ledger may carry it; the file inherits the store's hygiene rules — lives outside the
  repo (`~/lambo-dogfood/`), curated export only, never Endor-internal content.
* Rotation is the operator's problem (`logrotate`/manual); the serve only appends.

**Depends on:** nothing.

## I2 — Heartbeat lines

The serve appends a `stats` line to the same ledger on a fixed interval (default off;
`--ledger-heartbeat <secs>`): the `lambo_stats` payload plus process uptime and the
binary's version/sha. Gives growth-over-time, GC visibility, flush-lag history, and
"which pinned binary produced this stretch of ledger" for free — the upgrade events
DOGFOOD requires become self-documenting.

**Depends on:** I1.

## I3 — The analysis kit

Committed scripts (`scripts/observability/`), duckdb/jq over the ledger + sqlite, one per
question, each emitting a small report:

* `recall_first.py` — per agent, per work-session: fraction of write sequences preceded
  by a recall (metric 1), from ledger ordering alone.
* `dedup_rate.py` — derive created-vs-matched over time (metric 2), ledger + store.
* `score_bands.py` — real recall score distribution against G1's measured bands and the
  0.35 floor (metric 4); flags any true-hit-below-floor recurrence.
* `warnings.py` — every blast-radius warning fired: when, to whom, over which concept,
  and (join with git log by time window) whether the touching commit happened anyway
  (metric 5 — the honest version of it).
* `duplicates.py` — cosine scan for near-duplicate concepts above/below the merge
  threshold (metric 3), reusing the F evidence probe shape.

No dashboards — `serve-web` already renders state; these are report generators whose
outputs can graduate to `evidence/` through the normal curated path.

**Depends on:** I1 (ledger format), I2 (heartbeats, for the time axis).

---

## Interim, and what I does not replace

Until I lands: Agent Governance in audit-only mode logs calls client-side (fragmented per
client, but real), and daily `lambo_stats` snapshots are manual. After I lands these stay
complementary — Governance sees what the *client* did across all tools; the ledger sees
what *lambo* did with per-leg score detail no client can observe.

**Rig consequence:** I changes `serve`, so landing it means a new pinned binary and a
deliberate dogfood upgrade event (DOGFOOD.md rules) — the first real exercise of the
upgrade path, with the heartbeat's sha field proving it happened.

---

## Done when

- [ ] `--ledger` off by default; on, every tool call appends one line and a full
      dogfood day parses with `duckdb` end to end
- [ ] Recall lines carry final + per-leg scores and the warning-rendered flag; derive
      lines carry created/matched counts
- [ ] A ledger-write failure drops lines, logs once, counts in `lambo_stats`, and never
      fails or delays a tool call — tested by making the path unwritable mid-run
- [ ] Heartbeat lines carry stats + binary sha; an upgrade shows as a sha change in the
      same file
- [ ] The five analysis scripts run against a real dogfood ledger and each emits its
      report; their outputs are reproducible from committed inputs when exported
- [ ] The dogfood rig is re-pinned to a ledger-carrying binary and DOGFOOD.md's
      measurement list points here

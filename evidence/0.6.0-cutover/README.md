# 0.6.0 dogfood cut-over: one mixed session to per-project sessions (#33)

On 2026-10-13 the lambo-for-lambo dogfood memory moved from one mixed session
(`lambo-dev`) to eight per-project sessions, with a shared `general` session
layered under each project. This folder records what changed for agents' recall
and what it cost.

Rig: Apple M3 Pro, lambo 0.6.0 (`macos-arm64-metal` release asset), BGE-M3
through candle on Metal. Every measurement used a scratch copy of the cut-over
backup or the verified split, never the live writer.

The files hold aggregates only: no store content, ids, queries from private
projects or ledgers. The `endor` session holds work material, so its figures are
**withheld**: it was measured with `--aggregate-only endor`, and it appears below
only as a name.

| File | What it is |
|---|---|
| `ab-old-vs-new.json` | `mcp_ab.py ab`: recall and write latency, old `/mcp` against new `/mcp/s/lambo` |
| `topk-content-overlap.json` | Top-k overlap per query, measured on hit content (see note) |
| `concurrent-old-w8.json`, `concurrent-new-w8.json` | `mcp_concurrent.py`: 8 agents for 120 s on each side |
| `concurrent-new-multi-<session>.json` | 4 drivers of 2 agents each, in parallel, on four sessions of one new serve |

## The cut-over

Tools: `lambo session classify`, `split` and `verify` (#33 D1–D3), run during a
write freeze of about two minutes. The scripted part took 29 s.

**Split accounting** (source `lambo-dev` to 8 target sessions):
- Concepts: 5,599 → 5,641. 38 shared hubs get 80 placements.
- Interactions: 2,069 → 2,124. 55 origin replicas were added; 54 of them are
  stubs, which keep the interaction's id, agent and times but carry no prompt
  text.
- Source edges: 13,648. Of these, 13,164 were copied within a session, 369
  rebuilt as Temporal links, 52 reattached to a hub, 29 Derives edges
  reattached, and 34 dropped as cross-session.
- A re-run of the split wrote 0 rows.

**Verify** (`lambo session verify`, top 5): plain recall on the old mixed
session, compared with layered recall as each project's agent now sees it.

| Project | recall@5 vs before | Before: hits from other sessions | After: hits from other projects |
|---|---:|---:|---:|
| lambo | 0.865 | 3 | 0 |
| ajilamu | 0.852 | 3 | 0 |
| rustydocs | 0.938 | 14 | 0 |
| lambo-paper | 1.000 | 22 | 0 |
| vimanam | 1.000 | 15 | 0 |
| general (unlayered) | 1.000 | 23 | 0 |
| general-usage | n/a (nothing in scope before) | 25 | 0 |

- **Leakage:** before the split, project answers carried hits from other
  sessions. After it, no project sees another project's hits.
- **The `general` layer:** it contributes at most 2 hits per project's top 5,
  with the #157 absolute relevance floor at 0.50.
- **The only warning change:** "session inactive", because each new session
  starts fresh.

## Performance: before and after

Both sides ran the same binary. The old side was `serve --session lambo-dev` on
`/mcp`; the new side used the live `[serve]` block (8 sessions, `general` at
weight 0.5 and min_similarity 0.50), measured on `/mcp/s/lambo`.

### Recall and write latency (`mcp_ab.py ab`)

12 rounds of 20 queries make 240 recalls per side, plus 30 derives per side,
interleaved.

| ms | Old p50 | Old p90 | Old p99 | New p50 | New p90 | New p99 |
|---|---:|---:|---:|---:|---:|---:|
| recall (top_k 8) | 5.64 | 23.45 | 144.05 | 2.69 | 20.34 | 74.93 |
| derive ack | 0.57 | 5.20 | 14.13 | 0.49 | 0.82 | 7.80 |
| derive applied | 30.65 | 40.16 | 60.23 | 26.18 | 29.27 | 148.25 |

Neither side had errors. The new side's derive-applied p99 is a single sample
out of 30; its p50 and p90 are lower than the old side's.

**Top-k overlap.** The harness reports a Jaccard of 0.0 because the split
gives every node a new id, so the id sets never intersect. Measured on hit
content instead, the top-8 overlap is a mean of 0.70 and a median of 0.78. The
rest is expected: the new side searches only `lambo` plus `general`.

### Concurrent agents (`mcp_concurrent.py`)

Settings: think time 1.0 s, 120 s, seed 1, and a fresh store copy for each run.

| | Old `/mcp`, 8 agents | New `/mcp/s/lambo`, 8 agents | New, 4 sessions × 2 agents |
|---|---:|---:|---:|
| writes acked / applied / failed | 541 / 541 / 0 | 543 / 543 / 0 | 527 / 527 / 0 |
| apply p50 / p90 | 51 / 73 ms | 47 / 67 ms | 39–51 / 53–65 ms per session |
| drain | 0 s | 0 s | 0–0.5 s |
| recall p50 / p90 / p99 | 14.5 / 45.8 / 175 ms | 9.3 / 27.2 / 107 ms | 3.6–13.5 / 23–37 / 40–106 ms |

The multi-session run used `lambo`, `vimanam`, `rustydocs` and `general-usage`,
with one driver each, in parallel against one serve.

### Footprint and startup

| | Old | New |
|---|---:|---:|
| startup to listening port | 3.5–4.3 s | 2.7–3.1 s |
| model on Metal (IOAccelerator footprint) | about 1.08 GB | about 1.08 GB |

- **Memory:** one model serves all eight sessions, so per-session memory is not
  multiplied. RSS is not a useful measure here, because macOS compresses idle
  pages and the weights sit in accelerator memory.
- **Startup:** the model loads lazily at the first embed, so the startup times
  above exclude it.

**Why the new side is faster:** most likely the smaller graph each recall
searches, 2,356 concepts in `lambo` against 5,599 in `lambo-dev`. Merging in the
12-concept `general` layer had no measurable cost.

## Reproducing

The harnesses are in `scripts/loadtest/` (see its README):

```bash
python3 scripts/loadtest/mcp_ab.py ab --a old=http://127.0.0.1:7711/mcp \
  --b new=http://127.0.0.1:7712/mcp/s/lambo --rounds 12 --out ab.json
python3 scripts/loadtest/mcp_concurrent.py --endpoint http://127.0.0.1:7712/mcp/s/lambo \
  --workers 8 --think 1.0 --secs 120 --seed 1 --out conc.json
```

Serve each side from a scratch copy on its own port. Never use the live
writer's port: `ab` writes.

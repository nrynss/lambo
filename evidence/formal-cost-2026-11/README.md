# What the local Lean work cost: tokens and agent time

Token usage and agent time for the formal-methods work done locally between
2026-11-21 12:10 and 2026-11-22 13:26 UTC. It covers every step from picking
up the cloud session's Lean development through #288, the modelling and
fix waves, #325, the four #291 checks, #287, and the 0.15.0 release and gate.
It is the cost data asked for in #292.

- **Source:** the Claude Code transcripts of the one orchestrating session and
  its 127 background agents, all Claude Opus 5.5 (`claude-opus-5-5`). The
  transcripts hold prompts and code, so they stay on the machine; this folder
  has the per-agent totals only.
- **Not covered:** the cloud session that wrote the first 13 models and 252
  theorems before this work; its transcript is not on this machine. Human
  time is not measured either.

## Files

- [`extract.py`](extract.py) reads one session transcript and its
  `subagents/` folder and writes one row per transcript:
  - messages and the four usage fields (fresh input, cache writes, cache
    reads, output), plus thinking tokens;
  - the model, first and last timestamp, span, and active time.

  An assistant message can be written over several lines that repeat its
  usage, so usage is counted once per message id.
- [`categorize.py`](categorize.py) groups the rows into work items and roles
  from each agent's description, and prints [`tables.md`](tables.md). Its
  `ITEMS` list is the whole mapping. With a second argument it writes
  [`agents.jsonl`](agents.jsonl): the same rows with the work item and role
  added, and without the first prompt or the agent id.
- [`tables.md`](tables.md) is three tables:
  - by work item;
  - by role (implementation, review, remediation);
  - per issue for the #293–#302 fix wave.

To reproduce on the machine that holds the transcripts:

```sh
python3 -I extract.py <project>/<session>.jsonl <project>/<session>/subagents rows.jsonl
python3 -I categorize.py rows.jsonl agents.jsonl > tables.md
```

## Definitions

- **Fresh input** is `input_tokens + cache_creation_input_tokens`: tokens
  sent that were not already cached. **Cache read** is
  `cache_read_input_tokens`. Cache reads dominate (about 1.5 billion). They
  are billed at a small fraction of the fresh-input rate, so any dollar
  figure depends on the rates applied; this folder reports tokens only.
- **Agent-hours active** sums the gaps between consecutive transcript records
  and leaves out any gap over 5 minutes (waiting on the user or on a
  background job). For background agents this is close to their wall time.
- **Role** is read from the description: "Review", "Re-review" and "Final
  re-review" count as review; "Remediate …" and "… round-N fixes" as
  remediation; everything else as implementation. Several remediation agents
  were named "Fix …", so remediation is undercounted and implementation
  overcounted.

## Results

| Work item | Agents | Fresh input (M) | Cache read (M) | Output (k) | Agent-hours |
|---|---:|---:|---:|---:|---:|
| #288 model-first | 4 | 0.66 | 24.8 | 148 | 0.6 |
| Modelling wave (13 models) | 27 | 7.91 | 205.6 | 1,793 | 5.3 |
| State-machine models (#273) | 8 | 5.19 | 146.2 | 1,038 | 3.2 |
| Fix wave #293–#302 | 30 | 13.30 | 483.9 | 2,020 | 10.2 |
| Other fixes (#315, #319, #322) | 8 | 3.10 | 98.3 | 493 | 2.1 |
| #325 model-first | 3 | 1.59 | 41.7 | 151 | 1.1 |
| #291 drift check | 9 | 2.64 | 68.2 | 540 | 1.9 |
| #291 constants | 11 | 2.85 | 93.7 | 500 | 3.4 |
| #291 property tests | 5 | 2.25 | 96.5 | 279 | 2.7 |
| #291 differential oracle (found #328) | 3 | 1.72 | 99.8 | 285 | 2.1 |
| #287 discovery race | 2 | 0.51 | 18.1 | 104 | 0.7 |
| Integration of the fix wave | 2 | 2.24 | 52.0 | 159 | 0.7 |
| Comment ID cleanup (not Lean) | 13 | 2.08 | 57.1 | 387 | 1.3 |
| Release 0.15.0 and gate evidence (not Lean) | 2 | 0.47 | 22.9 | 98 | 0.3 |
| **All 127 agents** | 127 | 46.52 | 1,508.6 | 7,994 | 35.5 |
| Orchestrating session | 1 | 5.31 | 499.6 | 872 | 12.1 |

- **The Lean work alone,** without the comment cleanup and the release:
  - 112 agents, 43.97M fresh input, 1,428.6M cache read, 7,509k output and
    33.9 agent-hours;
  - plus the orchestrating session's 12.1 active hours over about 25 hours of
    wall clock.
- **Review is half the agents and a quarter of the output.** 62 review
  agents produced 2,022k output tokens in 8.7 agent-hours, against 5,454k and
  23.0 hours for 59 implementation agents (`tables.md`, role table).
- **Per fix,** most of #293–#302 cost 0.4 to 1.4 agent-hours, counting
  implementation and every review round. The two largest are #294/#295 (2.8
  hours) and #301/#302 (2.4 hours); both took three review rounds
  (`tables.md`, fix-wave table).

## Limits

- The orchestrating session is one transcript over all the work, including
  CI, the release and the gate, so it is not split by work item.
- The work items come from descriptions the orchestrator wrote. An agent
  that worked on two items is counted under the first match in `ITEMS`.
- One machine, one model, one person directing the work; no comparison arm.
  These figures say what this way of working cost here, not that it is
  cheaper or dearer than another.

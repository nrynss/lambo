# G1 - BGE-M3 recall and merge calibration

This is the measurement half of workstream G. It uses the same local BGE-M3
llama.cpp rig as F: `bge-m3-q8_0.gguf`, `http://127.0.0.1:8080`, and 1024-wide
normalized embeddings. `measurement.txt` is the captured run; `measure.py`
replays the corpus against the running server.

## Method

The four score classes are intentionally separate:

| Class | n | Min | Max | Mean | Comparison shape |
|---|---:|---:|---:|---:|---|
| True recall | 6 | 0.4599 | 0.6577 | 0.5826 | bare query to stored `Concept: {content}` vector |
| Paraphrase merge | 10 | 0.6889 | 0.8984 | 0.7992 | production no-origin hybrid framing on both concepts |
| Related but distinct | 10 | 0.6348 | 0.8913 | 0.7683 | same framing |
| Unrelated | 8 | 0.4999 | 0.6249 | 0.5759 | same framing |

The three committed F queries remain the strongest interaction evidence because
they compare bare queries to **durable vectors written by hybrid derive**:

| Query -> intended concept | Durable-vector cosine | Existing 0.50 recent floor |
|---|---:|---|
| database table for people signing up -> user schema | 0.5706 | surfaced |
| login token checking layer -> auth middleware | 0.5823 | surfaced |
| changes that do not break existing clients -> backward compatibility | 0.3991 | masked |

The third line reproduces the failure mode: all three young concepts receive
the old 0.50 recent score, so `max(0.50, 0.3991)` erases the correct ordering.
The fresh corpus's bottom true-recall score is 0.4599; F's durable 0.3991 is
the lower and therefore decisive measurement. Fixture vectors are deliberately
not comparable: their deterministic near/far geometry is about >=0.85 versus
near-orthogonal, so its old 0.50 floor was not evidence for this scale.

## Merge result and precision boundary

The two merge classes overlap heavily. The existing 0.85 threshold already
accepts the related-but-distinct `reset password` / `forgot password` pair
(0.8913), and lowering it to 0.84 would additionally accept three recorded
related-distinct pairs (0.8069, 0.8152, 0.8340). It would rescue only the
near-boundary `user schema` / `user data model` paraphrase (0.8495) from this
corpus. That is an observed precision loss, not a tolerable calibration guess.

Conversely, the 0.85 threshold deliberately creates duplicates for real
paraphrases below it, including `register user` / `create account` (0.8230),
`make an account` / `register user` (0.8152), and `roll out the service` /
`deploy service` (0.8294). G records that duplicate behavior rather than
pretending a score-only threshold can resolve an overlapping corpus.

## G2 decision

* Set `RECENT_SCORE` to **0.35**. It is below the lowest durable true hit
  (0.3991), preserving max-merge while allowing the committed correct vector
  ranking to survive. It also leaves a 0.0491 margin rather than pinning a
  floor to the observed value.
* Keep `semantic_match_threshold` at **0.85**. This is an explicit calibrated
  decision, not a fixture carry-over: the measured overlap means lowering it
  would trade precision for one marginal paraphrase. Under-merging remains the
  intended safety bias until a richer merge signal exists.

Rejected options: additive blending would change all phase-1 ordering without
being needed to expose the recorded hit; per-embedder score machinery cannot
separate the overlapping merge corpus; lowering the merge bar has the measured
false-positive cost above. Fixture behavior is retained and checked as behavior,
not as an assertion that the recent score must be 0.50.

## Interaction checks

`transcript.txt` records both required end-to-end checks. Before G2 and after
G2, the 0.8230 `register user` / `create account` paraphrase creates two
concepts and no `Semantic` edge: preserving that duplicate is the precision
decision. After G2, the original F query that had tied at the old floor returns
`deployment must stay backward compatible` first. The final displayed score is
0.20 because phase-3 applies its default 0.5 query weight; its raw 0.3991
vector score is still above the calibrated phase-1 floor.

## Reproducing the interaction checks

`g-calibration.db` is intentionally ignored because it was a local measurement
scratch database. The interaction proof does not depend on it. Instead,
[`run.sh`](run.sh) copies F's committed `f-bge.db` durable-vector fixture into
a temporary directory and builds both exact revisions: `74febca^` with the
0.50 floor and `74febca` with the 0.35 floor.

With the local llama.cpp BGE-M3 server listening on `127.0.0.1:8080`, run this
from the repository root:

```sh
./evidence/mooshik-g-recall-calibration/run.sh
```

The driver fails unless the before run ranks `user schema stores account
records` first, the after run ranks `deployment must stay backward compatible`
first, and each revision reports exactly two concepts and zero `Semantic` edges
for its `register user` / `create account` session. It cleans up its temporary
worktrees and database copies on exit. Its command log contains complete
arguments, making the captured interaction independently replayable.

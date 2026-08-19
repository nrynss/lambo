# `scripts/observability` — the I3 analysis kit

Report generators over the `lambo serve --ledger` call ledger. One script per
DOGFOOD question, each printing a small human-readable report (`--json` for
piping). No dashboards: `serve-web` already renders *state*, and these answer
questions about *flow*.

See [`dev-diary/lambo-for-mooshik/I-observability.md`](../../dev-diary/lambo-for-mooshik/I-observability.md)
for why the ledger exists, and [`DOGFOOD.md`](../../dev-diary/lambo-for-mooshik/DOGFOOD.md)
for the metrics these close.

---

## Turning the ledger on

```sh
lambo serve --session lambo-dev --agent claude-code \
    --ledger      ~/lambo-dogfood/calls.jsonl \
    --ledger-heartbeat 300
```

Both flags are **off by default**. `--ledger-heartbeat` requires `--ledger` and
is refused at startup without it.

Three things worth knowing before you rely on a report:

1. **The ledger drops rather than delay.** Observability never takes down memory
   (I1). A stalled or unwritable path drops lines, logs one warning, and keeps
   serving; the dropped count is in `lambo_stats` as `ledger_dropped_lines` and
   in every heartbeat line. Every report prints it in its header, and says so
   when it is non-zero — a count computed over a ledger that dropped lines is a
   **lower bound**, and the reports refuse to pretend otherwise.
2. **Heartbeats are what make the counts trustworthy.** Without
   `--ledger-heartbeat` there is no `ledger_dropped_lines` in the file at all,
   so the reports cannot tell a quiet stretch from a dropped one and say
   `dropped: UNKNOWN`. Run with heartbeats.
3. **Hygiene.** The ledger carries recall queries and (truncated to 200
   characters) concept text. It inherits the store's rules: keep it **outside
   the repo** — `~/lambo-dogfood/` — never let Endor-internal content into it,
   and admit it to `evidence/` only through the curated export path. Rotation is
   yours (`logrotate`, or just `mv` the file: the writer reopens the path per
   batch, so it recreates it on the next line with no signal handling).

Set the git sha at build time so the heartbeat can attribute a stretch of ledger
to a binary — an upgrade then shows up as a `git_sha` change in the same file:

```sh
LAMBO_GIT_SHA=$(git rev-parse --short HEAD) cargo build --release
```

Without it the field is `"unknown"`, and two builds of the same crate version are
indistinguishable. The rig's build step is the place that must set it.

---

## The five reports

| Script | DOGFOOD metric | Reads | Answers |
| --- | --- | --- | --- |
| `recall_first.py` | 1 — recall-first compliance | ledger | Did the agent load memory before writing it? |
| `dedup_rate.py` | 2 — re-derivation savings | ledger (+ store) | Is the graph converging or accumulating? |
| `duplicates.py` | 3 — duplicate-creation rate | **store** (+ ledger) | Is the 0.85 merge threshold right for a real embedder? |
| `score_bands.py` | 4 — scores vs G1's bands | ledger | Do the calibrated constants still mean what G1 measured? |
| `warnings.py` | 5 — blast-radius warnings fired | ledger (+ `git log`) | Did the warning change anything? |

Metric 6 (friction) stays human notes, unchanged.

```sh
cd scripts/observability

python3 recall_first.py ~/lambo-dogfood/calls.jsonl
python3 dedup_rate.py   --bucket hour --store ~/lambo-dogfood/lambo.db  ~/lambo-dogfood/calls.jsonl
python3 score_bands.py  ~/lambo-dogfood/calls.jsonl
python3 warnings.py     --repo ~/src/lambo --window-minutes 120  ~/lambo-dogfood/calls.jsonl
python3 duplicates.py   --store ~/lambo-dogfood/lambo.db --ledger ~/lambo-dogfood/calls.jsonl
```

Each takes several ledger files (they are concatenated in argument order), so a
rotated set reads as one history. `_ledger.py` is the shared reader — one module
so all five agree on what a "call", a "write", a "work session" and a
"successful call" are. A metric computed two ways is a metric nobody can quote.

### Definitions that are choices, not facts

Named here rather than buried, because each is a judgement a reader may want to
overrule:

* **Work session** (`recall_first.py --gap-minutes`, default 30). The ledger has
  no agent-session boundary — `serve` holds one lambo session for its whole life
  while an agent's working stretches come and go — so an idle gap is an explicit
  *proxy*. A serve restart is the other boundary and that one is a fact, read off
  the heartbeat's `uptime_secs` resetting.
* **Write sequence.** A maximal run of consecutive successful writes. Compliance
  is scored per sequence, not per write: an agent that recalls once and then
  derives six concepts complied once, and scoring it six times would flatter it
  as much as scoring it once would punish it.
* **`semantic_merged` is not a match** (`dedup_rate.py`). A hybrid similarity
  merge adds a decaying `Semantic` edge and does not re-upsert the target or add
  a `Derives` edge, so counting it as re-derivation savings would overstate them
  with a weaker relationship. Reported in its own column.
* **The git join proves nothing** (`warnings.py --repo`). A commit in the window
  after a warning is not evidence the agent ignored it, and its absence is not
  evidence the agent heeded it. `overlap[...]` is an explicitly-labelled token
  heuristic between the concept text and the commit subject/paths — a starting
  point for a human. This is metric 5's *honest* version; the script never
  concludes.
* **Concepts without an embedding are invisible to `duplicates.py`**, not proven
  distinct. The report says so whenever there are any.

---

## Verifying the kit

`sample/calls.jsonl` is a **fabricated** ledger — no dogfood data, no Endor
content, no real agent — generated deterministically by `make_sample.py`. It has
one planted fact per report (a non-compliant agent, a serve restart and sha
change, a converging dedup rate, a recency floor masking a real cosine, a
blast-radius warning cut by the token budget, a non-zero dropped count, a failed
call, and a torn tail line), so no report is ever exercised against an empty set.

```sh
scripts/observability/verify.sh
```

runs all five, asserts each still finds its planted facts, checks the committed
sample still matches its generator, and checks `--json` parses everywhere. CI
does not execute `scripts/**` (see the path filter in `.github/workflows/ci.yml`),
so **this is a manual gate**: run it after touching anything here, and before
quoting a report in `evidence/`.

`duplicates.py` reads a store rather than the ledger, so `make_sample.py
--store <path>` synthesizes one on demand: four concepts with hand-built unit
vectors at known angles, one pair above the 0.85 threshold and one inside G1's
paraphrase band, plus one concept with no embedding. Generated rather than
committed — a generated binary in the tree is a thing nobody can review.

To regenerate the sample after changing the generator:

```sh
python3 scripts/observability/make_sample.py > scripts/observability/sample/calls.jsonl
```

---

## Why Python and not duckdb

The I doc called for duckdb/jq. The committed generators are **stdlib Python
only**, deliberately: the acceptance criterion is that they *run* against a real
ledger, and a report generator that needs a `pip install` first is a report
generator that does not run on the box where the ledger lives. Nothing here
imports anything outside the standard library.

duckdb and jq remain the right tools for the ad-hoc question the kit does not
answer, and the ledger is shaped for them — one JSON object per line, no nesting
that `read_json` cannot flatten. Recipes:

```sh
# Every tool call, by tool and outcome.
duckdb -c "SELECT tool, outcome, count(*) n, round(avg(duration_us)/1000, 1) ms
           FROM read_json_auto('calls.jsonl')
           WHERE kind = 'call' GROUP BY 1, 2 ORDER BY n DESC"

# Slowest recalls, with the query that caused them.
duckdb -c "SELECT ts, agent_id, duration_us, hit_count, query
           FROM read_json_auto('calls.jsonl')
           WHERE tool = 'lambo_recall' ORDER BY duration_us DESC LIMIT 20"

# Graph growth over the heartbeats.
duckdb -c "SELECT ts, stats.node_count, stats.canonical_count, stats.flush_lag_ms,
                  stats.ledger_dropped_lines
           FROM read_json_auto('calls.jsonl') WHERE kind = 'stats' ORDER BY ts"

# Per-leg scores, flattened one row per hit — the join score_bands.py automates.
duckdb -c "SELECT ts, query, h.content, h.score,
                  h.legs.bm25, h.legs.recent, h.legs.vector_cosine
           FROM read_json_auto('calls.jsonl'), UNNEST(hits) AS t(h)
           WHERE tool = 'lambo_recall'"

# Every dropped-line reading, as a sanity check before quoting any count.
jq -r 'select(.kind=="stats") | [.ts, .git_sha, .stats.ledger_dropped_lines] | @tsv' calls.jsonl

# Did any tool call ever fail, and how?
jq -r 'select(.kind=="call" and .outcome!="ok") | [.ts, .tool, .outcome, .error_kind] | @tsv' calls.jsonl
```

A torn final line (the process was killed mid-write) is where the Python reader
and these tools differ: the reader counts it and carries on, `duckdb` refuses the
file, and `jq` emits every good line and *then* exits non-zero on the tail. Drop
it first if that matters:

```sh
sed '$d' calls.jsonl > whole.jsonl     # not `head -n -1`: BSD head has no negative count
```

---

## Line schema

Authoritative version: the module docs in [`src/ledger.rs`](../../src/ledger.rs)
and `_ledger.py`. Every line carries `v` (currently `1`) and a server-stamped
`ts`; **consumers ignore unknown keys**, so adding a field does not bump `v` —
changing what one means does.

```jsonc
// kind: "call" — one MCP tool call
{"v":1,"ts":"…","kind":"call","tool":"lambo_recall","agent_id":"…",
 "outcome":"ok|error|panic","error_kind":"…",   // error_kind only when not ok
 "duration_us":1234, …per-tool facts merged at the top level }

// per-tool facts
lambo_recall        query, top_k, hit_count, hits[], canonical_marker,
                    blast_radius_warning, conflict_line, hot_warning,
                    reservation_warning, response_annotations[], warning_count
  hits[i]           node_id, content (≤200 chars, then "…[truncated]"), score,
                    legs{}, is_canonical, blast_radius, included_in_context,
                    annotations[]
  legs{}            any of bm25 / recent / vector_cosine → float.
                    A key is present ONLY when that phase-1 leg produced the hit.
                    An EMPTY object means the hit was not a phase-1 candidate at
                    all — it arrived through phase-2 traversal expansion.
lambo_derive        created, matched, semantic_merged, reinforced,
                    concepts_requested
lambo_record_action created, edges
lambo_reserve       op ("reserve"|"release"), granted, ttl_seconds (grants only)
lambo_inspect       depth, fuzzy
lambo_saints        canonical_count
lambo_stats         (no extra facts — the numbers are in the heartbeat)

// kind: "stats" — an I2 heartbeat
{"v":1,"ts":"…","kind":"stats","uptime_secs":900,"version":"0.2.2",
 "git_sha":"abc1234","stats":{ …the lambo_stats payload, including
 ledger_path, ledger_written_lines, ledger_dropped_lines }}
```

`legs` is the field the ledger exists for. Recall's phase-1 merge folds the three
legs by `max`, so a merged score is lossy: a `0.35` is either the recency floor
(`RECENT_SCORE`) or a genuine weak cosine, and the two mean opposite things.
Nothing downstream of the merge could tell them apart, which is why
`score_bands.py` could not have existed before I1.

**`score` and `legs` are different stages and will not agree.** `score` is the
FINAL ranking score — phase-3 assembly applies the daemon's score table and the
configured `RecallWeights` on top of the merged phase-1 score, so `score` is
routinely well above or below `max(legs)`. Band cosines against
`legs.vector_cosine` (which is what `score_bands.py` does); use `score` only for
"what did this rank at".

`derive` lines carry no `demoted` count, and that is not an omission: in this
codebase demotion is `Memory::demote`'s context-overflow split and the
canonization task's `Canonical → None` regression, neither of which `derive` can
perform (`DeriveOutcome` in `src/graph/derive.rs` has no such field). Demotions
are audited in `canonization_events` in the store, which is where to ask about
them. What `derive` does distinguish is `semantic_merged` from `matched`, and
that is the distinction metric 2 turns on.

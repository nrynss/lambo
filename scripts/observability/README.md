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
   in every heartbeat line, with `ledger_dropped_channel_full` /
   `ledger_dropped_write_failed` beside it saying whether the writer fell behind
   or the path is broken. Every report prints the total in its header, and says so
   when it is non-zero — a count computed over a ledger that dropped lines is a
   **lower bound**, and the reports refuse to pretend otherwise.
2. **Heartbeats are what make the counts trustworthy.** Without
   `--ledger-heartbeat` there is no `ledger_dropped_lines` in the file at all,
   so the reports cannot tell a quiet stretch from a dropped one and say
   `dropped: UNKNOWN`. Run with heartbeats.
3. **Hygiene.** The ledger carries recall queries (truncated to 2000 characters)
   and concept text (truncated to 200). It inherits the store's rules: keep it **outside
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
indistinguishable. The rig's build step is the place that must set it —
[`DOGFOOD-SETUP.md`](../../dev-diary/lambo-for-mooshik/DOGFOOD-SETUP.md) §2, which
does.

---

## The five reports

| Script | DOGFOOD metric | Reads | Answers |
| --- | --- | --- | --- |
| `recall_first.py` | 1 — recall-first compliance | ledger | Did the agent load memory before writing it? |
| `dedup_rate.py` | 2 — re-derivation savings | ledger (+ store) | Is the graph converging or accumulating? |
| `duplicates.py` | 3 — duplicate-creation rate | **store** (+ ledger) | Is the 0.85 merge threshold right for a real embedder? |
| `score_bands.py` | 4 — scores vs G1's bands | ledger | Do the calibrated constants still mean what G1 measured? |
| `blast_radius.py` | 5 — blast-radius warnings fired | ledger (+ `git log`) | Did the warning change anything? |

Metric 6 (friction) stays human notes, unchanged.

```sh
cd scripts/observability

python3 recall_first.py ~/lambo-dogfood/calls.jsonl
python3 dedup_rate.py   --bucket hour --store ~/lambo-dogfood/lambo.db  ~/lambo-dogfood/calls.jsonl
python3 score_bands.py  ~/lambo-dogfood/calls.jsonl
python3 blast_radius.py   --repo ~/src/lambo --window-minutes 120  ~/lambo-dogfood/calls.jsonl
python3 duplicates.py   --store ~/lambo-dogfood/lambo.db --ledger ~/lambo-dogfood/calls.jsonl
```

Each takes several ledger files (they are concatenated in argument order), so a
rotated set reads as one history. `_ledger.py` is the shared reader — one module
so all five agree on what a "call", a "write", a "work session" and a
"successful call" are. A metric computed two ways is a metric nobody can quote.

**No Python version floor.** Stdlib only, and `parse_ts` truncates fractional
seconds to six digits before `datetime.fromisoformat`, which is what removes the
floor there would otherwise be: the producer is chrono's `to_rfc3339()`
(`SecondsFormat::AutoSi`), which emits 0, 3, 6 **or 9** fractional digits by clock
resolution, and `fromisoformat` accepts only 3 or 6 before Python 3.11 — while the
Linux half of the rig still ships 3.10 system Pythons and nanosecond stamps are
exactly what it produces. `_ledger.py`'s module docstring records the two remaining
dependencies on timestamp *shape* (string sort is timestamp sort; `BUCKETS` slices
the stamp by prefix), both of which are conditions on the producer rather than
things this reader can normalise away.

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
* **The git join proves nothing** (`blast_radius.py --repo`). A commit in the window
  after a warning is not evidence the agent ignored it, and its absence is not
  evidence the agent heeded it. `overlap[...]` is an explicitly-labelled token
  heuristic between the concept text and the commit subject/paths — a starting
  point for a human. This is metric 5's *honest* version; the script never
  concludes.
* **Concepts without an embedding are invisible to `duplicates.py`**, not proven
  distinct. The report says so whenever there are any.
* **`canonical_marker` counts hits the token budget actually rendered; the four
  warning flags do not.** The spec's word is *rendered*, and the two rendering
  paths differ. `[canonical]` exists only inside a hit's context block, so a
  Canonical hit the budget cut produced no marker and the flag is `false` — a
  `max_tokens: 1` recall of a Canonical concept reports
  `canonical_marker: false` with `is_canonical: true` on the hit, which is the
  honest pair. The four hit-owned warning kinds (`blast_radius_warning`,
  `conflict_line`, `hot_warning`, `reservation_warning`) are **budget-independent**:
  their lines go into the flat `warnings` vector for every returned hit whatever
  the budget did to the block, and arrive as a second text block, so those flags
  are computed over every returned hit. `blast_radius.py` reports both halves — a
  warning whose block was cut was still *delivered*; a cut Canonical hit's marker
  was not. For "was a Canonical concept returned at all", read per-hit
  `is_canonical`, never the set-level flag.
* **Compliance is sticky within a work session** (`recall_first.py`). One
  successful recall marks every later write sequence in that session compliant and
  nothing clears it — stronger than the "recalls once then derives six concepts
  complied once" illustration above, which describes one sequence. A session's
  recalled context does not evaporate after the next write, so stickiness is the
  defensible reading, but it is a choice: `--gap-minutes` is the knob that makes it
  weaker.
* **An unknown `v` warns; it does not refuse** (`_ledger.py`'s `KNOWN_VERSIONS`).
  A line whose schema version this kit does not know is read as the newest version
  it does know, and announced in the header in the same register as dropped lines.
  Refusing would throw away the readable v1 lines in a mixed file — and a mixed
  file is the realistic case, since one ledger spanning an upgrade is exactly what
  `git_sha` exists to make visible. The warning is in `--json` too, under
  `ledger_schema`.
* **The recency floor is read off the ledger, not supplied** (`score_bands.py`).
  Every recency-leg hit carries `legs.recent`, which *is* the `RECENT_SCORE` the
  serving binary was built with, so the report states the floor the traffic
  actually ran under and a file spanning a recalibration shows two. There is no
  `--floor` flag: one existed, changed only a header line, and produced reports
  that contradicted themselves ("recency floor in force: 0.9" above
  "cosine=0.2914 < floor=0.3500").

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
sample still matches its generator, and checks `--json` parses everywhere. Three
cases are generated inside the run rather than committed, so they cannot perturb
the planted facts: a **mixed-version** ledger (a `v:2` line and a line with no `v`,
which must warn loudly and still be read), a ledger of **nanosecond** timestamps
(chrono's nine-digit fractional seconds, which must parse), and a **queued**
ledger whose older heartbeat reports the larger depth (queue depth is a gauge, so
the header must print the newest reading and never the maximum). CI
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
that `read_json` cannot flatten.

**Provenance of the recipes below:** hand-verified against the committed sample,
by hand and not by `verify.sh`, and **not** by running `duckdb` over
`sample/calls.jsonl` — that file ends in a deliberate torn line and `duckdb`
refuses such a file outright (see the note after the recipes). What the kit's own
test suite asserts is the *property* `read_json` needs — one flat JSON object per
line, no nesting it cannot flatten — not a duckdb invocation. Strip the tail first
if you want to run these against the sample.

Recipes:

```sh
# Every tool call, by tool and outcome.
duckdb -c "SELECT tool, outcome, count(*) n, round(avg(duration_us)/1000, 1) ms
           FROM read_json_auto('calls.jsonl')
           WHERE kind = 'call' GROUP BY 1, 2 ORDER BY n DESC"

# Slowest recalls, with the query that caused them.
duckdb -c "SELECT ts, agent_id, duration_us, hit_count, query
           FROM read_json_auto('calls.jsonl')
           WHERE tool = 'lambo_recall' ORDER BY duration_us DESC LIMIT 20"

# Graph growth over the heartbeats, with the drop split.
duckdb -c "SELECT ts, stats.node_count, stats.canonical_count, stats.flush_lag_ms,
                  stats.ledger_dropped_lines, stats.ledger_dropped_channel_full,
                  stats.ledger_dropped_write_failed, stats.ledger_queued_lines
           FROM read_json_auto('calls.jsonl') WHERE kind = 'stats' ORDER BY ts"

# Per-leg scores, flattened one row per hit — the join score_bands.py automates.
duckdb -c "SELECT ts, query, h.content, h.score,
                  h.legs.bm25, h.legs.recent, h.legs.vector_cosine
           FROM read_json_auto('calls.jsonl'), UNNEST(hits) AS t(h)
           WHERE tool = 'lambo_recall'"

# Every dropped-line and queue-depth reading, as a sanity check before quoting
# any count: a non-zero drop makes the file an undercount, and so does a queue
# depth the newest row still reports — those lines were accepted but had not
# reached the disk when the heartbeat was taken (I-R3-2).
jq -r 'select(.kind=="stats") | [.ts, .git_sha, .stats.ledger_dropped_lines, .stats.ledger_queued_lines] | @tsv' calls.jsonl

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
lambo_recall        query (≤2000 chars, then "…[truncated]"), top_k, hit_count,
                    hits[], canonical_marker, blast_radius_warning,
                    conflict_line, hot_warning, reservation_warning,
                    response_annotations[], warning_count
  hits[i]           node_id, content (≤200 chars, then "…[truncated]"), score,
                    legs{}, is_canonical, blast_radius, included_in_context,
                    annotations[]
  legs{}            any of bm25 / recent / vector_cosine → float.
                    A key is present ONLY when that phase-1 leg produced the hit.
                    An EMPTY object means the hit was not a phase-1 candidate at
                    all — it arrived through phase-2 traversal expansion.
lambo_derive        concepts_requested, admitted, receipt
                    (J3: created / matched / semantic_merged / reinforced are
                    NOT here — the ack is issued before the write, so it does
                    not know them. J4 put created/matched back on a `completion`
                    line carrying the same `receipt`, which is the join
                    dedup_rate.py and duplicates.py perform; semantic_merged and
                    reinforced remain receipt-only, reachable with
                    lambo_stats(receipt=...).)
lambo_record_action admitted, receipt
                    (J3/J4: created is on the completion, same reason, same
                    join; edges remain receipt-only.)
lambo_reserve       op ("reserve"|"release"), granted, ttl_seconds (grants only)
lambo_inspect       depth, fuzzy
lambo_saints        canonical_count
lambo_stats         (no extra facts — the numbers are in the heartbeat)

// kind: "stats" — an I2 heartbeat
{"v":1,"ts":"…","kind":"stats","uptime_secs":900,"version":"0.2.2",
 "git_sha":"abc1234","stats":{ …the lambo_stats payload, including
 ledger_path, ledger_written_lines, ledger_dropped_lines,
 ledger_dropped_channel_full, ledger_dropped_write_failed,
 ledger_queued_lines }}

// kind: "completion" (J4) — one background write's durable lifecycle.
// `receipt` joins it back to the derive/record_action `call` line that acked
// it. The two applied states carry the metric-2 fact set; the other two carry
// no counts, because a failed write created nothing and a deferred one has not
// created it yet.
{"v":1,"ts":"…","kind":"completion","agent_id":"…","receipt":"lwr1.…",
 "state":"applied|applied_after_restart|failed|deferred",
 "created_count":3,"matched_count":1}          // applied states only

// kind: "startup" (J4) — a serve's intent to acquire the single-writer lease,
// written BEFORE the acquire, so a serve that LOSES it still leaves an artifact.
{"v":1,"ts":"…","kind":"startup","session":"…","agent_id":"…",
 "transport":"stdio|http","state":"acquiring"}

// kind: "lease" (J4) — one side of a lease conflict, or a proxy's degraded
// state. The other party is named for what it is (JE2E-11): `counterparty` is
// a lease token, `dialled` is a socket path.
{"v":1,"ts":"…","kind":"lease","session":"…","agent_id":"…",
 "event":"refused|refused_takeover","side":"loser|holder",
 "counterparty":"agent-b@host#123"}
{"v":1,"ts":"…","kind":"lease","session":"…","agent_id":"…",
 "event":"proxying|proxying_stopped","side":"loser",
 "dialled":"/run/user/1000/lambo/….sock","lost":0}   // `lost` on stopped only
```

`startup` and `lease` answer operational questions — which serve tried, who
refused whom, how long a client was left without memory — rather than any of the
six metrics, so the report scripts count them in the header's "unknown kind"
figure and read no further. `completion` is the one J4 kind the metric scripts
**do** read, through the join above.

**The five set-level flags on a recall line are not computed alike.**
`canonical_marker` counts only hits with `included_in_context: true` — the
`[canonical]` marker renders inside a hit's block, so a hit the budget cut
rendered none. The four warning flags are computed over every returned hit,
because their lines reach the agent through the flat `warnings` vector whatever
the budget did. Both readings are in the choices list above, and per-hit
`is_canonical` / `included_in_context` are what let a consumer recover either.

**`ledger_dropped_lines` is the total; the two keys beside it are the split.**
`ledger_dropped_channel_full` is backpressure — the writer fell behind and the
bounded channel refused lines, which is the never-block guarantee working.
`ledger_dropped_write_failed` is everything else: a batch whose write or open
failed, a batch abandoned when the 500 ms shutdown budget expired, or an append
after shutdown. The first says "the filesystem is slow"; the second says "the path
is wrong". The total is their sum, so an old consumer reading only
`ledger_dropped_lines` is unaffected.

**`ledger_queued_lines` is not a drop — it is the writer's queue depth**
(accepted, not yet on disk). It exists because the drop counters have a blind
spot about themselves (I-R2-3): on a path whose `open` blocks — a reader-less
FIFO, a hung mount — the writer parks *before its first write*, so `written` and
both drop counters read `0`, which is indistinguishable from an idle server until
1024 lines have piled up. This key moves on the first call. A heartbeat with
`queued` climbing while `written` lags is a writer that is **behind**; both flat
is genuinely no traffic. A **parked** writer is a different case and the file
cannot show it at all — heartbeat lines travel the same channel as call lines, so
they queue too and are abandoned with everything else (measured at the binary:
`written=0`, every line dropped at shutdown), which leaves a **live
`lambo_stats` call** as the only place the parked case can be read (I-R3-2). It
is derived as `accepted - written - write_failed`, so a `channel_full` drop —
which the channel never accepted — does not deflate it. Every report's header
prints a non-zero `queued` from the last heartbeat beside the dropped count, so
no report says "the ledger is complete" over a backlog it can see.

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

**Since J3, `derive` lines carry no `created` / `matched` counts, and since J4
those counts come back through a join.** `lambo_derive` is acknowledged before
the write is applied (writes acknowledged before the embedder,
`dev-diary/lambo-for-mooshik/J-multi-client.md` §J3), so the call line is
written at a moment when no outcome exists. J4 added a `kind:"completion"` line
carrying the receipt id, the settle state, and the true `created_count` /
`matched_count`, and **`dedup_rate.py` and `duplicates.py` join on it** — so an
MCP-driven session's dedup rate is a real number again, not `n/a`. The join
lives once, in `_ledger.joined_facts`, so the two reports cannot disagree about
what a derive created.

Three things are worth knowing about what the join does and does not restore:

* **`semantic_merged` and `reinforced` do not come back.** The completion line
  carries the created/matched pair and no more; those two live only on the
  receipt, fetchable with `lambo_stats(receipt=...)`. Both reports say so where
  the number would otherwise read as a zero — `dedup_rate.py` labels the
  `sem.merged` column an undercount, and `duplicates.py` suppresses its
  "a zero here means the vector merge never fired" reading, which would
  otherwise be a conclusion about the write path drawn from a gap in the reader.
* **Only *applied* completions contribute.** A `failed` write created nothing
  and a `deferred` one has not created it yet (it is owed to the next serve of
  that session), so neither adds facts. A derive whose completion is `deferred`
  is therefore still counted as fact-less, honestly — and the message says the
  replay may be why.
* **A missing completion is still not a zero.** If the completion landed in a
  ledger file the run did not read, the call is reported on its own line rather
  than folded in as `created=0, matched=0`. Pass every file.

CLI-driven sessions (`lambo derive`, `lambo record-action`) use the synchronous
write path and still report every fact on the line, so nothing there changed;
`dedup_rate.py`'s summary prints the split between line-carried and
join-recovered facts, because "this ledger has no completion lines" and "these
writes were synchronous" are different states that used to look identical. And
`duplicates.py`'s store-side half reads the graph rather than the ledger, so its
pairwise scan never depended on any of this.

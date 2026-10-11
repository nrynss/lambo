# Driving `lambo serve` under load and under real models (C-series)

Two families of harness live here, and they answer different questions. The
C-series capture is written up in
[dev-diary/notes/concurrency-capture.md](../../dev-diary/notes/concurrency-capture.md).

**Synthetic (C1–C3)** drives `lambo serve --transport http` with K concurrent
MCP clients under a weighted valid + adversarial mix, pulls SIGTERM while load
is in flight, and proves the tail is durable. Deterministic, fast, and exact
about what it sent, which is what the correctness question needs.

**Model-driven (C5)** puts a real local model in the loop instead. Nothing here
is deterministic; the point is what a model does with the surface when nobody
is scripting its calls.

## What is here

### Synthetic harness (C1–C3)

| Script | Role |
|---|---|
| `mcp_load.py` | The load driver (C1). K worker threads, each an independent streamable-HTTP MCP session, issuing a deterministic seeded mix: valid `lambo_derive` / `lambo_record_action` / `lambo_recall`, plus adversarial calls (record_action over `MAX_ACTION_TARGETS`, content with NUL and U+202E, content over `MAX_CONTENT_BYTES`, an unknown tool, malformed params). Every response — success, tool refusal, 429, 503, transport error — is recorded to a JSONL ledger. |
| `capture_sigterm.sh` | The run harness (C2). Provisions a scratch SQLite store, starts `lambo serve` with a scratch bearer token, runs the driver, sends SIGTERM inside the burst phase, measures signal→exit wall time and exit code, captures full server stderr, then runs the durability check. |
| `check_durability.py` | The C3 check: compares the ledger's accounting against a post-exit readback of the SQLite store. Interactions are 1:1 per acknowledged write call; concept/edge counts come from the write receipts the ledger saw settle as applied (J3 acks a write before applying it), and receipts never seen settled are reported as their own row. Exit codes (non-zero never means durable): 0 checked and durable; 2 shortfall; 3 the ledger's write responses are in a format it does not recognise; 4 nothing to verify (no acknowledged write in the ledger; `--allow-empty` makes that expected and exit 0); 5 concepts unverifiable (store short of settled-applied concepts, GC could explain it, and `--gc-logged` was not given); 6 unreadable input (a ledger line that is not a JSON object, or a missing `--db` is never created: the store is opened read-only); 64 usage error. Concepts are compared by count because receipts carry no concept ids, so the concept check is a lower bound (see the script header). Used by both families. |
| `test_check_durability.py` | Stdlib `unittest` fixtures for the checker: a synthetic ledger and SQLite store exercise refused-at-ack accounting, the empty-ledger and unverifiable-concept exit codes, GC-explained shortfalls, a missing `--db` (never created) and parser drift, with no server. Run `python3 scripts/loadtest/test_check_durability.py`. |

### Model-driven harnesses (C5)

| Script | Role |
|---|---|
| `mcp_agentic.py` | **The one that measures agency.** System prompt = a skill file (`--skill`), toolset = the four lambo MCP tools and nothing else, and the *model* chooses every call via llama.cpp's OpenAI tools API. Records each model turn with the tool calls it proposed, each executed call with the server's response, and per-task protocol accounting (`recall_first`, `derives_without_prior_recall`, the call sequence). |
| `mcp_swarm.py` | Throughput loop, **not** an agency measurement. It hardcodes prompt → `lambo_derive` → `lambo_recall` and gives the model no lambo semantics, so its numbers describe loop throughput and the model's concept-text behavior only. Parameterized by `--llama-model` / `--llama-endpoint`. Do not quote its rates as evidence a model chose anything. |
| `omp_swarm.py` | Runs the same idea through the OMP agent harness rather than a raw chat loop, with the skill in the system prompt. Note that OMP always loads read/write/edit plus any inherited MCP servers, so a lambo-only toolset is not achievable there. |

#### `--tasks`, and why it exists

`mcp_agentic.py` takes `--tasks <file>` (one task string per line). Without it
the built-in list is used, and that list *names the recall-first sequence in
the task text itself*. Measuring whether a model reaches for memory on its own
therefore requires a neutral task file: task text that describes only the work,
with no mention of recall, memory, checking, ordering, or any tool name. The
first control arm missed this and had to be redone; see
`evidence/swarm/experiment2/` for the neutral list, the banned-word grep that
guards it, and what changed when the instruction was removed.

The same rule applies to `--skill`: the file is read verbatim as the system
prompt, so any commentary in it (even an HTML comment) reaches the model.

### Version A/B

| Script | Role |
|---|---|
| `mcp_ab.py` | Latency A/B between two `lambo serve` builds. `ab` drives two HTTP serves, each on its own copy of the same store, with recalls interleaved per query, a `lambo_stats` per round, and synthetic `lambo_derive` writes timed to ack and to applied (via the receipt). It reports p50/p90/p99 per path and the top-k node-id overlap between the two sides as a recall-quality check. `ledger` splits a `serve --ledger` file at a re-pin time and summarizes server-side latency per tool on each side. Output is aggregates only, safe to paste into an issue. Refuses port 7700 unless `--allow-production-port`, since `ab` writes. First run: [#108](https://github.com/nrynss/lambo/issues/108). |
| `mcp_concurrent.py` | Many agents at once against one HTTP serve: N workers, each its own MCP session and agent id, exponential think time, weighted tool mix (`--mix`). After the run it waits for the write queue to drain, then asks `lambo_stats(receipt)` for every acked write and reports the all-inclusive write failure rate and failed-write reasons, plus `drained: true/false`; exits 2 when the run is not a result. Aggregates only. Promoted from `evidence/0.4.0-vs-0.3.0-load/harness/concurrent_mix.py`. See [Concurrent agents](#concurrent-agents-mcp_concurrentpy). |
| `replay_parity.py` | Replays the last N real write calls of a store backup into two builds over stdio and compares every receipt and the final stores. Each difference is classified by reason; reasons a release expects come from a data catalogue, anything else is UNEXPECTED, and the verdict also fails runs where both builds refuse most calls, results never settle, or a serve exits non-zero. Promoted from `evidence/0.4.0-vs-0.3.0-load/harness/replay_parity.py`. See [Replay parity](#replay-parity-replay_paritypy). |
| `test_loadtest_harnesses.py` | Stdlib `unittest` for the pure parts of the two above (mix parsing, seeded payloads, reason normalization and grouping, failure rate, prompt splitting, classification, the catalogue, content stripping, path refusals). No server, GPU or network. Run `python3 -m unittest scripts/loadtest/test_loadtest_harnesses.py`. |

## Concurrent agents (`mcp_concurrent.py`)

N workers, each with its own MCP session and agent id (`conc-<i>`), call one
`lambo serve --transport http`. Between calls a worker waits an exponentially
distributed think time with mean `--think` seconds. The default mix is the live
dogfood ledger's: `recall=40,derive=38,record_action=18,stats=4`; `--mix`
overrides it. Derives write two concepts and record_actions one modifies
target; their text is a pure function of `(--seed, worker, call number)`, so
two runs with the same seed send the same writes in the same per-worker order.
Recall queries come from `--queries FILE` (one per line) or mcp_ab.py's list.

After `--secs` the workers stop and the driver polls `lambo_stats` until the
write queue, log and flush depths are all zero, for at most `--drain-timeout`
seconds. Then, using each worker's own session (receipts are scoped to the
agent that made them), it asks `lambo_stats(receipt)` for every acked write, or
a seeded sample of `--receipt-max` (default 2000) when there were more.

The JSON (`--out`) holds aggregates only, safe to paste into an issue:

- `latency_ms`: per-tool latency of successful calls only; `errors`: per tool,
  the count and latency of calls that came back as a tool error or a
  transport failure;
- `writes.write_failure_rate_all`: the #116 acceptance rate, (failed + dropped
  + refused at ack + transport-failed writes) over (settled receipts + refused
  at ack + transport-failed writes). A transport-failed write may still have
  been admitted (only the reply is known lost), so this is an upper bound;
- `writes.failure_rate`: failed + dropped over settled receipts only (pending,
  expired and query errors are counted in `receipt_states` but excluded);
- `writes.failed_reasons`: receipt reasons, grouped. By default every reason
  is reduced to a known-message id (`hybrid_replan_exhausted`,
  `hybrid_candidate_timeout`, `image_suffix_refused`, `drop_queue_full`, ...,
  each anchored on the exact server text) or the model-safe error class the
  text starts with (`conflict`, `internal error`, ...), else `unrecognised`;
  no server text is copied. `--include-raw-reasons` groups by normalized text
  instead (numbers, paths and URLs folded; read it before pasting). A failed
  receipt carries only the error class, so pass `--server-ledger` (the serve's
  `--ledger` file) to also group the operator-facing `error` of each failed
  `completion` line of this run, as `writes.failed_reasons_server_ledger`.
  A dropped write never reaches the apply path, so it has no `completion`
  line: its reason is only on the receipt (`drop_*`);
- `writes.refused_at_ack` / `writes.transport_failed`: writes that got no
  receipt;
- `drain.drained`: false means the drain timed out and every counter is a
  snapshot with writes still outstanding, never final totals. Drained means
  `write_queue_outstanding`, `log_depth` and `flush_depth` were all present
  and zero;
- `flags.expired_receipts` / `flags.retention_risk`: receipts are kept 300 s
  after they settle (`RECEIPT_RETENTION`), so a long run plus drain loses the
  outcomes of early writes. `retention_risk` is set when run + drain reaches
  240 s, and the script warns on stderr;
- `worker_errors`: workers that died on an unexpected exception;
- `final_stats`: the server's `write_queue_*` counters after the drain.

Exit status: 0 ok; 2 when the drain did not complete, no call succeeded, or a
worker died (the JSON is still written, with `problems` listing why).

It writes synthetic derives: run it against a scratch serve on a store copy.
Port 7700 is refused unless `--allow-production-port`. The token comes from
`--token` or `$LAMBO_AUTH_TOKEN`.

### Acceptance check for #116 (concurrent write failures)

8 workers, think 1.0 s, 120 s, on a scratch copy of a realistic store (the
0.4.0 baseline was 165 failed of 497 writes, all `could not commit after 8
concurrent graph changes`):

```bash
# scratch serve on a store copy, with a ledger for the raw failure reasons
lambo provision --config scratch/lambo.toml
XDG_RUNTIME_DIR=$(mktemp -d /tmp/lbc.XXXX) lambo serve --config scratch/lambo.toml \
    --session conc-116 --agent conc --transport http --port 17701 --bind 127.0.0.1 \
    --ledger scratch/serve-ledger.jsonl 2> scratch/serve.stderr.log &

python3 scripts/loadtest/mcp_concurrent.py \
    --endpoint http://127.0.0.1:17701/mcp --out conc-116-w8.json \
    --workers 8 --think 1.0 --secs 120 --seed 1 \
    --server-ledger scratch/serve-ledger.jsonl

echo "exit $?"   # 2: not drained, nothing succeeded, or a worker died

jq '{drained: .drain.drained, rate: .writes.write_failure_rate_all,
     settled_rate: .writes.failure_rate, acked: .writes.acked,
     refused: .writes.refused_at_ack, transport: .writes.transport_failed,
     reasons: .writes.failed_reasons, ledger: .writes.failed_reasons_server_ledger,
     expired: .flags.expired_receipts, problems: .problems}' conc-116-w8.json
```

Pass means exit 0, `drained` true (otherwise rerun with a longer
`--drain-timeout`; a snapshot is not a result), `expired` 0, and
`write_failure_rate_all` and the reasons meet the bar set in #116. Run the baseline build the same way on a fresh copy of the same store
for the comparison.

## Replay parity (`replay_parity.py`)

Rewinds a copy of a store backup to just before its last `--groups` write calls,
rebuilds those calls from the store (concept text from each interaction's
prompt, types from the concepts it created, `parent_of` and action edges from
the edges stamped with the call's time), replays them in order into each of
two builds over stdio, waiting on every receipt, and compares receipts, final
store counts and canonization. `--variants K` adds K reworded `Recap: ...`
derives to exercise semantic merge. The script header has the full method.

Every differing call gets reason ids from detectors that run independently.
Each id accounts for some compared fields (state, created/matched/merged
counts, reinforced, edges, embedded, and the created/matched/merge content);
a differing field that no fired id accounts for becomes its own UNEXPECTED id,
`unexplained:<field>`, so an expected reason cannot hide another difference.

Which reasons are expected is data: the built-in catalogue, `REASONS_0_4_0`,
is the 0.4.0 CHANGELOG mapping from the 0.4.0 vs 0.3.0 run and applies to
that pair only. For another pair, write a catalogue and pass `--reasons FILE`:

```json
{
  "base": "0.4.0",
  "reasons": {
    "receipt_gains_embedded": {"label": "0.4.0 only", "expected": false},
    "state_differs:some_rule": {"kind": "state_differs", "direction": "b_refuses",
                                "detail_pattern": "exact server refusal text",
                                "label": "...", "expected": true, "ref": "0.4.1 CHANGELOG, #N"}
  }
}
```

With `"base": "0.4.0"` the file's entries are laid over the built-in
catalogue; without a `base` key they replace it. Every entry needs a string
`label` and a boolean `expected` (no defaults). A `state_differs:*` entry needs
`"kind": "state_differs"` and a `detail_pattern`, which should be the exact
server message (refusal text can echo caller content, so a loose keyword
would explain the wrong refusals); `direction` is `b_refuses` (A applied, B
did not; the default), `a_refuses` or `any`. A reason that is not in the
catalogue, is marked unexpected, or comes from no detector is UNEXPECTED.

Verdict: `parity.txt` opens with PASS or FAIL. The run fails (exit 1) when any
differing call is UNEXPECTED, more than `--max-not-applied` (default 0.02) of
the calls applied on neither side (both builds refusing everything is not
parity), any result never settled (pending, transport error, not run), or a
serve exited non-zero. Canonization, unembedded and content-diff differences
are listed as notes.

Safety: the snapshot is opened read-only and may be a backup anywhere. Nothing
is written under `~/lambo-dogfood` or any extra `--protect` directory
(repeatable; added to the default, never replacing it), and a snapshot named
like a live store (`lambo-dev.db*`) is refused unless `--allow-live-name`. The
config template must have a `[store]` with `kind = "sqlite"`; its path is
rewritten to the copy, any `dsn` is dropped, and `LAMBO_CONFIG`,
`LAMBO_STORE*`, `LAMBO_SQLITE_*`, `LAMBO_POSTGRES_*`, `LAMBO_COCKROACH_*` and
`LAMBO_*_DSN` are removed from the environment of `provision` and `serve`.
Each serve gets a fresh `XDG_RUNTIME_DIR` under `--runtime-parent` (default
`/tmp`).

Outputs: `--out` gets `parity.json` and `parity.txt`, aggregates only. Concept
text and raw refusal text are left out (refusals become known-message ids or
error classes, in the log too) unless `--include-content`, so `--out` can be
committed as evidence. Everything that holds store content (store copies,
`calls.json`, `side-*.json`, serve stderr, JSON-RPC transcripts) goes to
`--work`: by default a fresh 0700 temp directory, removed at exit unless
`--keep-work`; a given `--work` is created 0700 if missing and kept. Never
commit `--work`, and keep it out of the checkout.

### Acceptance check: replay parity between two builds

```bash
python3 scripts/loadtest/replay_parity.py \
    --bin-a path/to/lambo-0.4.0 --bin-b target/release/lambo \
    --snapshot path/to/store-backup.db --config-template path/to/lambo.toml \
    --session lambo-dev --groups 300 --variants 30 \
    --reasons path/to/reasons-0.4.1.json \
    --out evidence/<run>/parity --work ~/scratch/parity-work
echo "exit $?"   # 0: PASS; 1: FAIL (see the VERDICT block at the top of parity.txt)
```

Run `--dry-run --keep-work` first: it reconstructs and truncates without
starting a binary, and reports the reconstruction warnings in `parity.json`.

## How the driver is shaped (so refusals don't crowd out the measurement)

The HTTP surface enforces documented limits (T8.7): a sustained rate limit
(`DEFAULT_RATE_LIMIT_RPS` = 50, burst ×2) and a session cap
(`DEFAULT_MAX_SESSIONS` = 32). A 429 or 503 is a *correct* observation — the
driver records it as such — but the phases are shaped so those refusals stay
edge observations:

1. **sessions** — every worker opens its own MCP session.
2. **cap-probe** — a probe mints sessions until the server refuses with 503,
   then releases them via `DELETE /mcp`. Proves the session cap is live.
3. **overdrive** — free-run for the first `--overdrive-calls` calls per
   worker, then paced at ~20 rps per worker for the rest of the phase, so the
   rate limit's 429s are genuinely observed against a fresh, fast server.
4. **main** — paced at `--rate` (default 40 rps aggregate, below the 50 rps
   limit), valid + adversarial mix at `--adversarial-fraction` (default 0.2).
   Expected: zero rate-limit refusals (the 429s that straddle into the main
   window's opening are the overdrive's burst-budget carryover).
5. **burst** — at-cap `record_action` calls paced at `--burst-rate` (default
   45 rps), building a large un-flushed tail. The harness SIGTERMs the
   server during this phase; when the server dies, workers record the
   transport failures and stop after 10 consecutive ones (`server-unreachable`
   phase marker) instead of hammering a dead socket.

The ledger is the ground truth: one JSON line per call with worker, seq, tool,
params as sent, ok/is_error, the response text, HTTP status, and elapsed ms —
plus `phase` markers for cap-probe / main / burst boundaries.

## Running a capture

```bash
# build the binary with the SQLite adapter (default features + store-sqlite)
cargo build --features store-sqlite

# full C2 run into evidence/concurrency/ (K=12, SIGTERM 5s into the burst)
scripts/loadtest/capture_sigterm.sh \
    --out evidence/concurrency --workers 12 --session c-load-20260818 --delay 5
```

The harness listens on port 17700 by default and refuses 7700 (however spelled), the port a
live `lambo serve` writer conventionally holds, unless `--allow-production-port`
is passed. The spawned serve gets a private `XDG_RUNTIME_DIR` (a `mktemp`
directory removed on exit), so its session endpoint never lands in the shared
per-user runtime dir a live writer advertises itself in.
The harness's exit status is the durability check's (0 only when it passed; see
the codes above), and the driver and serve are stopped and reaped on every exit
path, including SIGTERM and SIGINT.

The harness writes into `--out`: `stderr-<run>.log` (the server's full stderr,
containing the exact `lambo serve: session closed, tail durable` line),
`ledger-<run>.jsonl`, `durability-<run>.txt`, `run-<run>.json` (machine +
timing + counts), `lambo.sqlite.toml` and the scratch SQLite store itself.

The auth token is generated into a `mktemp` file and passed via
`LAMBO_AUTH_TOKEN` (the env channel the server documents as taking precedence
over `--auth-token`) — it never appears in argv or in any evidence file; run
metadata records the `<SCRATCH-TOKEN>` placeholder.

## Driving the driver alone

```bash
python3 scripts/loadtest/mcp_load.py \
    --endpoint http://127.0.0.1:17700/mcp \
    --session c-load-20260818 --ledger /tmp/load-ledger.jsonl \
    --workers 12 --seed 0 \
    --main-secs 45 --burst-secs 25
```

`--endpoint` defaults to `http://127.0.0.1:17700/mcp`. An endpoint on port 7700
is refused unless `--allow-production-port` is passed; the port is compared as a
number, so `07700` is refused too and a non-numeric port is an error.

Stdlib only (urllib + threads), mirroring the streamable-HTTP MCP client in
`examples/drive_mcp_soak.py`: `initialize` → `notifications/initialized` →
`tools/call` with the `Mcp-Session-Id` header, accepting both JSON and SSE
replies, and `Authorization: Bearer` when a token is configured (via
`--token` or `LAMBO_AUTH_TOKEN`).

## Running a model-driven capture

Needs `llama-server` serving a chat model, and a `lambo serve` on a scratch
session with a scratch SQLite store. Never point either at `cloudops-exhibit`.

```bash
llama-server -m <Qwen3-0.6B-UD-Q6_K_XL.gguf> --port 8082 --jinja -c 32768 -a qwen3-0.6b

# agency measurement: model chooses every call
python3 scripts/loadtest/mcp_agentic.py \
    --session c-qwen3-agentic-20260818 --ledger <ledger>.jsonl \
    --endpoint http://127.0.0.1:7706/mcp --token "$LAMBO_AUTH_TOKEN" \
    --agents 3 --duration 150 --skill skills/lambo-cloudops/SKILL.md \
    --tasks evidence/swarm/experiment2/tasks-neutral.txt \
    --llama-model qwen3-0.6b --llama-endpoint http://127.0.0.1:8082/v1 \
    --llama-key lambo-swarm-local

# then SIGTERM the server and check what survived
python3 scripts/loadtest/check_durability.py \
    --ledger <ledger>.jsonl --db <store> --session <session> --stderr <serve-stderr>
```

Comparing two system prompts means changing `--skill` and nothing else, running
the arms back to back so ambient machine load is shared, and reporting
recall-first among **acting** tasks (those with at least one tool call). The
unconditional rate mixes protocol adherence with liveness: a model that stalls
looks identical to one that ignores the protocol.

## Determinism

`--seed` drives every worker's RNG (`random.Random(seed * 1000 + worker)`), so
the mix (concept picks, adversarial patterns, call ordering per worker) is
reproducible for a given seed. Timing is not deterministic by design — the run
measures wall clock under concurrency — but the *mix* is.

## Hygiene

The ledger records what was sent and what came back — including the
adversarial payloads (NUL, U+202E as `\u0000` / `\u202e` escapes;
`ensure_ascii=True` keeps the file greppable and isutf8-clean). The
acceptance scan for wire hygiene ("no DSN fragments, no cockroachlabs.cloud,
no sqlx/driver text, no internal URLs") is applied to the response fields and
the server stderr — see the C2 runbook in `evidence/concurrency/README.md`.

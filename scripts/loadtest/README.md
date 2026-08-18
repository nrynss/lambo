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
| `check_durability.py` | The C3 check: compares the ledger's successful-call accounting (interactions 1:1 per write call; concept/edge counts parsed from the server's own response text) against a post-exit readback of the SQLite store. Used by both families. |

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
    --session c-load-20260818 --ledger /tmp/load-ledger.jsonl \
    --workers 12 --seed 0 \
    --main-secs 45 --burst-secs 25 --delay-hint 5
```

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

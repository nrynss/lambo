# Real-model swarm (C5) — LFM2-350M, Qwen3-0.6B, functiongemma-270m against lambo via MCP

Two runs. Run 1 (`20260817-205024`, session `c-swarm-20260818`, 3 agents ×
150 s) drove **LFM2-350M**; Run 2 (`20260818`, sessions `c-swarm-qwen3-20260818`
and `c-swarm-fgemma-20260818`) probed **Qwen3-0.6B** and
**functiongemma-270m** for tool-call capability and swarmed the one that has
it. All numbers on the **Linux box** (cachyos-x8664, Ryzen 5 3600, 12
threads, RTX 4070 SUPER, CachyOS — not the MBP; see the hardware caveat in
`dev-diary/notes/concurrency-capture.md`).

## Per-model results (sourced numbers)

| Model | OMP harness probe | Raw OpenAI tools probe | Swarm run? | Harness | Derive-calls/hour | Dedup rate | Model errors | Unparseable turns |
|---|---|---|---|---|---|---|---|---|
| LFM2-350M | garbled pseudo-tool prose, 0 lambo calls | prose, `finish_reason=stop`, no `tool_calls` | yes (fallback) | `mcp_swarm.py` | 3961 | 0.183 | 0 | 25% (54/218) † |
| **Qwen3-0.6B** | tool call to the **wrong tool** (`lsp`, hallucinated args), 0 lambo calls | **`finish_reason=tool_calls`, one correct `lambo_derive` call** | **yes (fallback)** | `mcp_swarm.py` | **2956** | **0.893** | **0** | **22% (35/159)** † |
| functiongemma-270m | prose refusal, no tool call | native `<start_function_call>` markup returned as prose, no `tool_calls` | **no — finding** | — | — | — | — | — |

† **Model errors** counts HTTP/transport failures on the model call (0 in
both swarm runs). It does **not** count unparseable model replies: turns
whose reply parsed to 0 concepts are recorded as `model_reply` records with
`parsed_concepts: 0` and produce no derive that turn. LFM2: 54 of 218 model
turns; Qwen3-0.6B: 35 of 159 — both disclosed here and traceable to the
ledgers (`kind: "model_reply"`).

Qwen3-0.6B is the only one of the three that emits a protocol-correct
`tool_calls` — and it still did not get OMP as its swarm harness, because
under OMP's harness (dozens of tool schemas in a ~31k-token prompt) it
called `lsp` instead of `lambo_derive` and never drove a lambo tool call
(0 interactions on the probe server). The spec's fallback — the minimal LLM
loop of `scripts/loadtest/mcp_swarm.py` — ran instead, honestly documented
below, exactly as for LFM2.

---

## Run 2 — 2026-08-18: Qwen3-0.6B and functiongemma-270m

### The probes (committed under `probes/`, fresh-run transcripts)

The LFM2 probe was repeated for both new models, same two ways:

1. **OMP harness** (provider registered in `~/.omp/agent/models.yml` —
   `lambolocal-qwen` → `:8082`, `lambolocal-gemma` → `:8083`,
   `api: openai-completions`, apiKey env var `LAMBO_SWARM_KEY`; `omp
   --model <id> -p --no-pty` in a workspace whose `.mcp.json` points at a
   scratch `lambo serve` with `lifecycle: eager`).
2. **Raw `POST /v1/chat/completions`** with a `tools` array for
   `lambo_derive` (`tool_choice: auto`).

Results:

* **Qwen3-0.6B under OMP** — emitted one tool call, but for the wrong tool:
  `lsp` with hallucinated `{"i": "...", "action": "definition"}` arguments.
  The `lsp` call failed; `lambo_derive` was never called; the harness's
  print-mode stdout was empty; probe store read back 0 interactions /
  0 concepts (transcript: `probes/omp-harness-qwen3-0.6b.txt`).
* **Qwen3-0.6B raw** — `finish_reason: "tool_calls"` with a single
  `lambo_derive` tool call carrying valid JSON
  (`{"concepts":[{"content":"auth middleware guards schema integrity",
  "concept_type":"logic"}]}`): the model **can** emit correct OpenAI
  tool_calls when the toolset is small (request + response:
  `probes/raw-tools-probe-qwen3-0.6b.json`).
* **functiongemma-270m under OMP** — a single prose refusal ("I apologize,
  but I cannot assist with generating system reminders or technical
  documentation …"), no tool call of any kind, 0 store interactions
  (transcript: `probes/omp-harness-functiongemma-270m.txt`).
* **functiongemma-270m raw** — emits FunctionGemma-native
  `<start_function_call>call:lambo_derive{…}<end_function_call>` markup, but
  this llama.cpp build (10456) returns it as **content prose**: no
  `tool_calls` field, `finish_reason: "length"` at 512 max_tokens and
  `finish_reason: "stop"` with the full generation (request + both attempts:
  `probes/raw-tools-probe-functiongemma-270m.json`).

**Finding for functiongemma-270m:** like LFM2-350M, as served by this
llama.cpp build it cannot drive an OpenAI-style tool-calling loop — the
native markup never becomes `tool_calls` on the wire — so **no swarm run
was performed**; the failure is the finding, recorded here and in
`dev-diary/notes/concurrency-capture.md`.

### The Qwen3-0.6B swarm (fallback harness, documented choice)

Harness: `scripts/loadtest/mcp_swarm.py` (minimal LLM loop of llama.cpp
`/v1/chat/completions` + the streamable-HTTP MCP client pattern), not OMP —
OMP was probed and the model cannot select the right tool under it (above).
The script was parameterized (`--llama-model`, `--llama-endpoint`) so the
same loop serves any probed model; defaults remain the LFM2 run's. Each
agent thread prompts Qwen3-0.6B with a swarm topic + the previous recall,
the model replies with one JSON `{"concepts": [...]}` object, the loop calls
`lambo_derive` and `lambo_recall` over MCP, and every response lands in the
JSONL ledger. The model supplies the content; the loop supplies the
tool-calling.

Run: 3 agents × 150 s, session `c-swarm-qwen3-20260818`, `llama-server` on
`:8082` (Qwen3-0.6B-UD-Q6_K_XL.gguf, `--jinja -c 32768 -a qwen3-0.6b`),
lambo serve on `:7701` (SQLite store + BGE-M3 embedder on `:8080`, bearer
auth, scratch token env-only).

#### Metrics (from the ledger, `ledger-qwen3-1787019996.jsonl`)

| Metric | Value |
|---|---|
| Swarm agents | 3 concurrent (threads `swarm-0..2`) |
| Window | 151 s of derives |
| Derive calls (tasks) | 124 — **2956 derive-calls/hour** (5913 MCP calls/hour with recalls) |
| Recall calls | 124 |
| Model errors | 0 — no `model_error` ledger records; see footnote † above |
| Concepts created / matched | 27 / 225 |
| **Canonization dedup rate** | **0.893** (225 matched existing of 252 concept references) — Qwen3-0.6B re-derives onto existing canonical keys at a much higher rate than LFM2-350M, whose concept texts drift between turns |
| Unparseable turns | 35 of 159 model turns (22%) parsed to 0 concepts — recorded as `model_reply` records with `parsed_concepts: 0`, disclosed per the footnote † |

Honesty footnote on concept quality: several derives echo fragments of the
recall context verbatim (e.g. `"Rate limit [Resource] (score 1.50)"` appears
as a derived concept — the model re-deriving the text it was handed); the
high dedup rate is partly a consequence of that repetition. Everything above
is traceable to the ledger's `derive`/`model_reply` records and the server's
own created/matched counts.

#### Durability after clean SIGTERM

`store-readback-qwen3-1787019996.txt` (SQLite readback, check_durability.py
accounting pattern): **interactions 124, concepts 27, edges 404, lease rows
0** — the ledger's 124 derives map 1:1 to store interactions and created
(27) == store concepts exactly; derive edges are unreported by the server so
404 is a lower bound. The server exited with the exact lines
`lambo serve: shutdown signal received, winding down` (02:29:23.527356Z) →
`lambo serve: session closed, tail durable` (02:29:23.527618Z), a ~0.3 ms
transcript gap, releasing the single-writer lease. The functiongemma session
closed the same way (`store-readback-fgemma-1787019996.txt`: 0 interactions,
0 concepts, 0 lease rows — nothing was ever written to it).

#### Portal visuals

`portal-qwen3-auth-guard-*.png` and `portal-qwen3-rate-limit-*.png`:
`lambo serve-web` on session `c-swarm-qwen3-20260818` (port 7799), recall
queries whose answers can only exist if the swarm's derives landed. First
cards read **"auth middleware guards the user schema"** and **"Rate limit
protects the public API"** — both strings verbatim in the swarm ledger's
derive concepts (38 and 70 occurrences respectively) — rendered as cards
(3200×1800, captured by `scripts/recording/capture-swarm-portal.mjs`, which
waits for the query-specific content to render, scrolls it into view, and
fails on browser errors; run reports "capture clean: no console/page/http
errors"). The per-card numeric scores shown at capture time were read from
the live DOM and appear in no committed artifact, so they are not quoted
here (same convention as Run 1).

---

## Run 1 — 2026-08-17: LFM2-350M (the original finding, unchanged)

Run `20260817-205024`, session `c-swarm-20260818`, **3 agents × 150 s**, on
the **Linux box** (see the caveat above).

The specified swarm — OMP (Oh My Pi v17.3.5) as harness, `llama-server`
serving **LFM2-350M** (`LFM2-350M-UD-Q8_K_XL.gguf`, second instance on
`:8081`, OpenAI-compatible, `--jinja -c 32768 -a lfm2-350m`), N≥3 OMP agents
MCP-driving `lambo serve --transport http` on `:7701` (auth token, scratch
SQLite store, BGE-M3 embedder) against one scratch session — **is not
feasible with this model, and that failure is the finding**:

* **OMP model switching was configured and tried.** The local endpoint was
  registered as a provider in `~/.omp/agent/models.yml`
  (`lambolocal` → `http://127.0.0.1:8081/v1`, `api: openai-completions`,
  model id `lfm2-350m`) and invoked with `omp --model lfm2-350m -p`.
  The harness loaded the MCP server (`.mcp.json` → `:7701`, `Bearer` header,
  `lifecycle: eager` — the config is gitignored, carries the scratch token).
  **LFM2-350M cannot emit tool calls**: its reply to the derive instruction
  was garbled pseudo-tool text — prose fabricating a `lambo_derive` CLI
  (`lambo_derive schema auth_guards.json`, `lambo_derive audit_schema.yaml`,
  …) instead of the tool-call JSON OMP needs; the probe server logged zero
  `tools/call` requests and the probe store read back 0 interactions.
  Transcript committed under `probes/omp-harness-garbled-tool-call.txt`.
* **The raw OpenAI tools API was probed next** (`/v1/chat/completions` with a
  `tools` array for `lambo_derive`): the model returned prose with
  `finish_reason=stop` and **no `tool_calls`** — response committed under
  `probes/raw-tools-probe.json`. Confirmed at the protocol level: a 350M
  model cannot drive a tool-calling loop.

So the spec's fallback applied: the minimal LLM loop
(`scripts/loadtest/mcp_swarm.py`). All 487 concepts in the session are
LFM2-350M prose, delivered over the real MCP wire.

### Metrics (from the ledger, `ledger-20260817-205024.jsonl`)

| Metric | Value |
|---|---|
| Swarm agents | 3 concurrent (threads `swarm-0..2`) |
| Window | 149 s of derives |
| Derive calls (tasks) | 164 — **3961 derive-calls/hour** (7922 MCP calls/hour with recalls) |
| Recall calls | 164 |
| Model errors | 0 — no `model_error` ledger records (exceptions only); see footnote † |
| Concepts created / matched | 487 / 109 |
| **Canonization dedup rate** | **0.183** (109 matched existing of 596 concept references) — the swarm re-derived overlapping concepts onto existing canonical keys; the rate is low because LFM2-350M's concept texts drift between turns (verbose prose), so most derives create fresh nodes |

Store readback after clean SIGTERM (`store-readback-20260817-205024.txt`):
interactions 164, concepts 487, edges 1522, **lease rows 0** — the ledger's
164 derives map 1:1 to store interactions and created == store concepts
exactly, and the server exited with the exact line
`lambo serve: session closed, tail durable` — `shutdown signal received` at
20:50:24.561330Z → `session closed, tail durable` at 20:50:24.562066Z, a
~0.7 ms transcript gap — releasing the single-writer lease.

## Artifacts

| Artifact | What it shows |
|---|---|
| `ledger-qwen3-1787019996.jsonl` | Run 2 swarm ledger: every Qwen3-0.6B turn — model replies, parsed concepts, `lambo_derive` / `lambo_recall` results with the server's own created/matched counts |
| `stderr-serve-qwen3-1787019996.log` | Run 2 lambo server stderr: session attach, hybrid-degradation warning (SQLite has no VECTOR_SEARCH), the exact shutdown line |
| `store-readback-qwen3-1787019996.txt` | Run 2 post-exit SQLite counts — ledger-exact, lease released |
| `stderr-serve-fgemma-1787019996.log`, `store-readback-fgemma-1787019996.txt` | Run 2 functiongemma session: attach + clean close, 0 interactions/0 concepts/0 lease rows (no swarm ran) |
| `portal-qwen3-auth-guard-*.png`, `portal-qwen3-rate-limit-*.png` | Run 2 visual: recall cards "auth middleware guards the user schema" / "Rate limit protects the public API" — Qwen3-0.6B-derived concepts, rendered by `capture-swarm-portal.mjs` (capture clean) |
| `ledger-20260817-205024.jsonl` | Run 1 swarm ledger (LFM2-350M) |
| `stderr-serve-20260817-205024.log` | Run 1 lambo server stderr, incl. the exact shutdown line |
| `store-readback-20260817-205024.txt` | Run 1 post-exit SQLite counts — ledger-exact, lease released |
| `portal-auth-middleware-*.png`, `portal-billing-retries-*.png` | Run 1 visual: recall cards "auth middleware guards schema integrity by validating the user schema before data access" and "billing service retries failed charges" (both verbatim in the Run 1 ledger) |
| `probes/` | Committed transcripts of all tool-calling probes (see `probes/README.md`): LFM2 (2 files, re-run), Qwen3-0.6B (2 files, fresh), functiongemma-270m (2 files, fresh) |

## What was tried for OMP model configuration (the record)

1. Providers added to `~/.omp/agent/models.yml` — `lambolocal` (`:8081`,
   lfm2-350m), `lambolocal-qwen` (`:8082`, qwen3-0.6b),
   `lambolocal-gemma` (`:8083`, functiongemma-270m), all
   `api: openai-completions`, apiKey the bare env-var name
   `LAMBO_SWARM_KEY`, resolved at runtime.
2. `omp --model <id> -p --no-pty` in the workspace whose `.mcp.json` points
   at the scratch lambo serve: harness booted, MCP tools discovered; LFM2
   garbled the tool call, Qwen3-0.6B called the wrong tool, functiongemma
   refused (transcripts under `probes/`).
3. Raw `POST /v1/chat/completions` with a `tools` array: LFM2 and
   functiongemma-270m emit no `tool_calls`; Qwen3-0.6B emits a correct one.

Steps 1–2 are OMP-config-feasible (the switch itself works; the models
cannot drive it). This is the cut per P9's order: the swarm benchmark is
optional, and each failure is recorded rather than papered over. Only
Qwen3-0.6B met the "emits tool calls" bar (raw probe), and it ran the
spec's fallback harness — both the harness choice and the alternative are
documented above.

## Reproduce

```bash
# llama-server already serving BGE-M3 on :8080; start the chat model(s):
llama-server -m <Qwen3-0.6B-UD-Q6_K_XL.gguf> --port 8082 --jinja -c 32768 -a qwen3-0.6b
# lambo serve on :7701 (sqlite + bge_m3), then:
python3 scripts/loadtest/mcp_swarm.py --session c-swarm-qwen3-20260818 \
    --ledger evidence/swarm/ledger-qwen3-<run>.jsonl --agents 3 --duration 150 \
    --token "$SWARM_TOKEN" --llama-model qwen3-0.6b \
    --llama-endpoint http://127.0.0.1:8082/v1
# portal visuals (session must be the one the swarm wrote):
lambo serve-web --config <toml> --session c-swarm-qwen3-20260818 --port 7799
SWARM_QUERIES='[{"label":"qwen3-auth-guard","query":"auth middleware guards the user schema"},{"label":"qwen3-rate-limit","query":"Rate limit protects the public API"}]' \
PORTAL=http://127.0.0.1:7799 node scripts/recording/capture-swarm-portal.mjs
```

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
`tool_calls`. Under OMP's **default full toolset** it still did not get OMP
as its swarm harness: in that condition it called the built-in `lsp` instead
of `lambo_derive` and never drove a lambo tool call (0 interactions on the
probe server). The round-1 review (C5M-R1-1) flagged that this was probed
only at the full toolset; the remediation ran the narrowed-toolset
counterfactual: with `omp --no-tools` the request-level context drops to 15
tools (read/write/edit + 7 lambo MCP + 5 inherited openaiDeveloperDocs —
captured verbatim in `probes/omp-request-tool-context.jsonl`) and the same
model emits a correct `mcp__lambo_derive` tool call under OMP (probe
transcript `probes/omp-harness-qwen3-narrowed.txt`; the "~31k-token prompt"
figure previously used here appears in no committed artifact and is
dropped). OMP still cannot provide a lambo-only toolset (read/write/edit and
every configured MCP server always load), and in this harness the inherited
`mcp__lambo_*` server shadows workspace-scoped scratch lambos — so the
scratch-isolated, store-verified agentic re-run (`mcp_agentic.py`, below)
is the swarm harness for this model, and the OMP swarm re-run is recorded
with its execution-target caveat in `probes/omp-swarm-qwen3-narrowed/`.

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

* **Qwen3-0.6B under OMP (default full toolset)** — emitted one tool call,
  but for the wrong tool: `lsp` with hallucinated `{"i": "...",
  "action": "definition"}` arguments. The `lsp` call failed;
  `lambo_derive` was never called; the harness's print-mode stdout was
  empty; probe store read back 0 interactions / 0 concepts (transcript:
  `probes/omp-harness-qwen3-0.6b.txt`, verdict scoped per C5M-R1-1). With
  the toolset narrowed (`--no-tools`) the same model selects
  `lambo_derive` correctly under OMP (transcript:
  `probes/omp-harness-qwen3-narrowed.txt`).
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
OMP was probed and the model could not select the right tool under OMP's
**default full toolset** (above; the C5M-R1-1 remediation narrowed this
claim — see the probes and the agentic re-run below).
The script was parameterized (`--llama-model`, `--llama-endpoint`) so the
same loop serves any probed model; defaults remain the LFM2 run's. Each
agent thread prompts Qwen3-0.6B with a swarm topic + the previous recall,
the model replies with one JSON `{"concepts": [...]}` object, the loop calls
`lambo_derive` and `lambo_recall` over MCP, and every response lands in the
JSONL ledger. The model supplies the content; the loop supplies the
tool-calling.

> **C5M-R1-2 disclosure (round-1 finding):** this fallback run gave the
> model **no lambo protocol context** — the SYSTEM prompt was
> content-generation-only ("Respond with EXACTLY one JSON object …"), with
> no lambo tool names, no derive/recall semantics, no blast-radius /
> load-bearing guidance, no skill or AGENTS.md — and the model made **no
> tool decisions**: `agent_loop` in `mcp_swarm.py` hardcodes prompt →
> `model_reply` → extract → `lambo_derive` → `lambo_recall`. The headline
> numbers below (2956 derive-calls/hour, 0.893 dedup, 22% unparseable)
> therefore measure loop throughput plus the model's concept-text
> behavior, **not model-driven tool selection**. The genuine agentic re-run
> (skill as system prompt, minimal lambo-only toolset, model-chosen calls)
> is recorded in the next section.

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
| **Canonization dedup rate** | **0.893** (225 matched existing of 252 concept references — the server's own per-call accounting: 27 created + 225 matched) — Qwen3-0.6B re-derives onto existing canonical keys at a much higher rate than LFM2-350M, whose concept texts drift between turns |
| Unparseable turns | 35 of 159 model turns (22%) parsed to 0 concepts — recorded as `model_reply` records with `parsed_concepts: 0`, disclosed per the footnote † |

Honesty footnote on concept quality: several derives echo fragments of the
recall context verbatim (e.g. `"Rate limit [Resource] (score 1.50)"` appears
as a derived concept — the model re-deriving the text it was handed); the
high dedup rate is partly a consequence of that repetition. There is also a
**placeholder-echo failure mode** (C5M-R1-4): derive `seq 34 worker 1`
shipped four concepts whose content is literally `<concept text>` — the
model echoing the SYSTEM template of `mcp_swarm.py` — and the server
accounted 1 of them as created (so one of the store's 27 concepts may
literally be "<concept text>"); the ledger's derive records carry **255**
concept objects while "252 concept references" (= 27 created + 225 matched)
is the server's own per-call accounting; the 3-reference gap is exactly that
placeholder derive. Everything above is traceable to the ledger's
`derive`/`model_reply` records and the server's own created/matched counts.

### The Qwen3-0.6B agentic re-run (C5M-R1-2 — the genuine agentic run)

Round-1 finding C5M-R1-2 required a real agentic re-run: the model given the
lambo protocol and left to choose the tool calls itself. That run exists
now: `scripts/loadtest/mcp_agentic.py` (agentic mode), fresh scratch store +
session `c-qwen3-agentic-20260818`, **system prompt = the lambo-cloudops
skill text verbatim** (`skills/lambo-cloudops/SKILL.md` — pre-flight recall
protocol, provenance/derivation protocol, blast-radius / load-bearing
semantics, fail-closed rules; sha256 recorded in the ledger `meta` record),
**minimal toolset = the four lambo MCP tools only** (`lambo_derive`,
`lambo_recall`, `lambo_record_action`, `lambo_inspect` — schemas fetched
live from the server's `tools/list`), model-chosen calls via llama.cpp's
OpenAI tools API on `:8082`. The harness executes whatever the model emits
and feeds the server's responses back; it never hardcodes a tool sequence.

Run: **3 agents × 151.0 s** (window = ledger `meta.started_at` → `done`),
ledger `ledger-agentic-qwen3-1787022500.jsonl`, 173 tool calls in 55 task
turns, 0 unparseable tool-call JSON.

| Metric | Value |
|---|---|
| Agents | 3 concurrent (threads `agentic-0..2`) |
| Window | 151.0 s |
| Tasks | 55 (47 completed by the model itself, 8 cut off by llama-server HTTP 500s mid-task — `model_error` records) |
| Tasks/hour | **1120 completed tasks/hour** (47 × 3600/151) |
| Tool calls | 173 — 86 `lambo_recall`, 45 `lambo_derive`, 40 `lambo_record_action`, 2 `lambo_inspect`; 165 ok (95.4%) |
| Calls/hour | 4124 (all tool calls) |
| **Pre-flight protocol adherence** | **43/55 tasks (78%) started with `lambo_recall`**; the other 12 tasks made **zero tool calls** (the model replied without acting — no protocol action taken, recorded as such); **0 of 45 derives were made without a prior recall in the same task** — every derive the model executed was preceded by the pre-flight recall. All 8 error-cutoff tasks were recall-first and mid-protocol (recall→derive→…). |
| Derive / recall calls (ok) | 42 derives / 81 recalls — the model recalls ~1.9× more than it derives, as the protocol demands |
| Dedup rate | **0.857** (36 matched existing of 42 successful derive calls; 6 created) — the model re-derived onto the same resource keys across tasks |
| Unparseable / degenerate turns | 15 of 106 model turns (14.2%) returned neither content nor tool_calls (empty completions) — recorded as `model_turn` records with empty `content` and `tool_calls`; 8 further turns failed on llama-server HTTP 500 |
| Tool-call failures | 8 (5 `lambo_recall` + 3 `lambo_derive`) — all the model emitting empty `{}` arguments; the server fail-closed with `failed to deserialize parameters: missing field agent_id` |
| Model errors | 8 — llama-server `HTTP Error 500` under concurrent load (recorded, not hidden) |

The model's per-task behavior is in the ledger's `model_turn` / `call` /
`task` records: each `task` record carries the exact call sequence the model
chose (`calls: ["lambo_recall","lambo_derive","lambo_record_action",
"lambo_recall", …]`), `recall_first`, and `derives_without_prior_recall`.

#### Durability after clean SIGTERM (agentic run)

`lambo serve` was SIGTERM'd after the window; it exited with the exact lines
`lambo serve: shutdown signal received, winding down` (03:11:10.527900Z) →
`lambo serve: session closed, tail durable` (03:11:10.528188Z), a 0.288 ms
transcript gap. `check_durability.py` (same pattern as C1–C3) reports
**MATCH: interactions 82 == 82 successful write calls (42 derive + 40
record_action), concepts 12 == 12 (6 derive-created + 6
record-action-created), edges 132 (≥ the 3 record_action edges; derive edges
unreported → lower bound), lease rows 0** — verdict "tail durable — no
ledger-successful write is missing from the store" (`durability-agentic-qwen3-1787022500.txt`,
`store-readback-agentic-qwen3-1787022500.txt`).

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
protects the public API"** — rendered as cards (3200×1800, captured by
`scripts/recording/capture-swarm-portal.mjs`, which waits for the
query-specific content to render, scrolls it into view, and fails on browser
errors; run reports "capture clean: no console/page/http errors").
Precise ledger counts (re-derived from `ledger-qwen3-1787019996.jsonl`,
correcting the earlier parenthetical per C5M-R1-4): "auth middleware guards
the user schema" occurs **2× in derive concepts** (its **38 whole-ledger
occurrences** are spread over 38 records: 35 recall texts + 1 `model_reply`
+ 2 derive); "Rate limit protects the public API" occurs **25× in derive
concepts** (case-insensitive) and **132 times across 70 ledger records**
counting the exact case-sensitive phrase "Rate limit protects the public
API" (23 derive + 41 recall + 6 model_reply records; the case-insensitive
whole-ledger count is 136 occurrences in 74 records). The per-card numeric
scores shown at capture time were read from the live DOM and appear in no
committed artifact, so they are not quoted here (same convention as Run 1).

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
| `ledger-agentic-qwen3-1787022500.jsonl` | **C5M-R1-2 agentic re-run ledger**: skill as system prompt (sha256 in `meta`), 3 agents × 151.0 s, every model turn with the tool_calls the model chose, every executed call with the server's response, per-task protocol accounting (`recall_first`, `derives_without_prior_recall`, the chosen call sequence) |
| `stderr-serve-agentic-qwen3-1787022500.log` | Agentic-run server stderr: attach, hybrid-degradation warning, the exact shutdown lines (0.288 ms gap) |
| `durability-agentic-qwen3-1787022500.txt` | Agentic-run `check_durability.py` output: interactions 82 == 82, concepts 12 == 12, edges 132 ≥ 3, lease released — "tail durable" |
| `store-readback-agentic-qwen3-1787022500.txt` | Agentic-run post-SIGTERM SQLite counts |
| `agentic-run-agentic-qwen3-1787022500.out` | Agentic-run driver stderr ("agentic run done: 3 agents x 150.0s") |
| `ledger-20260817-205024.jsonl` | Run 1 swarm ledger (LFM2-350M) |
| `stderr-serve-20260817-205024.log` | Run 1 lambo server stderr, incl. the exact shutdown line |
| `store-readback-20260817-205024.txt` | Run 1 post-exit SQLite counts — ledger-exact, lease released |
| `portal-auth-middleware-*.png`, `portal-billing-retries-*.png` | Run 1 visual: recall cards "auth middleware guards schema integrity by validating the user schema before data access" and "billing service retries failed charges" (both verbatim in the Run 1 ledger) |
| `probes/` | Committed transcripts of all tool-calling probes (see `probes/README.md`): LFM2 (2 files, re-run), Qwen3-0.6B (OMP full-toolset + OMP narrowed + raw + request-level context + the OMP swarm re-run directory), functiongemma-270m (2 files, fresh) |

## What was tried for OMP model configuration (the record)

1. Providers added to `~/.omp/agent/models.yml` — `lambolocal` (`:8081`,
   lfm2-350m), `lambolocal-qwen` (`:8082`, qwen3-0.6b),
   `lambolocal-gemma` (`:8083`, functiongemma-270m), all
   `api: openai-completions`, apiKey the bare env-var name
   `LAMBO_SWARM_KEY`, resolved at runtime.
2. `omp --model <id> -p --no-pty` in the workspace whose `.mcp.json` points
   at the scratch lambo serve: harness booted, MCP tools discovered; LFM2
   garbled the tool call, Qwen3-0.6B called the wrong tool under OMP's
   **default full toolset**, functiongemma refused (transcripts under
   `probes/`).
3. Raw `POST /v1/chat/completions` with a `tools` array: LFM2 and
   functiongemma-270m emit no `tool_calls`; Qwen3-0.6B emits a correct one.
4. **C5M-R1-1 remediation — toolset narrowing under OMP.** `omp --no-tools`
   (or `--tools=<list>`) cuts the built-ins to read/write/edit, verified at
   the request level (`probes/omp-request-tool-context.jsonl`: the exact
   15-tool array OMP sends); OMP cannot drop read/write/edit and cannot
   exclude MCP servers (the parent session's lambo + openaiDeveloperDocs
   servers load for every child process), so a lambo-only toolset is not
   achievable under OMP. Under the narrowed 15-tool context, Qwen3-0.6B
   selects `lambo_derive` correctly (`probes/omp-harness-qwen3-narrowed.txt`).
   The OMP swarm re-run with the skill in the system prompt
   (`probes/omp-swarm-qwen3-narrowed/`) shows the model driving
   protocol-shaped sequences, with the execution-target caveat (the
   inherited `mcp__lambo_*` server shadows the workspace-scoped scratch
   lambo; the OMP leg's calls landed on the harness's live lambo, agent
   'cursor-agent' — recorded there).

Steps 1–2 are OMP-config-feasible (the switch itself works; the models
cannot drive it **at the default full toolset** — and, for LFM2/functiongemma,
at any toolset, since neither emits OpenAI `tool_calls`). The scratch-
isolated, store-verified agentic run is `mcp_agentic.py` (above). This is
the cut per P9's order: the swarm benchmark is optional, and each failure is
recorded rather than papered over.

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
# C5M-R1-2 agentic re-run (skill as system prompt, model-chosen calls):
python3 scripts/loadtest/mcp_agentic.py --session c-qwen3-agentic-20260818 \
    --ledger evidence/swarm/ledger-agentic-qwen3-<run>.jsonl \
    --endpoint http://127.0.0.1:7706/mcp --token "$LAMBO_AUTH_TOKEN" \
    --agents 3 --duration 150 --skill skills/lambo-cloudops/SKILL.md \
    --llama-model qwen3-0.6b --llama-endpoint http://127.0.0.1:8082/v1 \
    --llama-key lambo-swarm-local
# then SIGTERM the lambo serve and check durability:
python3 scripts/loadtest/check_durability.py --ledger <ledger> --db <store> \
    --session c-qwen3-agentic-20260818 --stderr <serve-stderr>

# C5M-R1-1a OMP swarm re-run (narrowed toolset + skill in the system prompt):
python3 scripts/loadtest/omp_swarm.py --cwd <ws-with-mcp.json> \
    --agent-dir <isolated-profile> --skill skills/lambo-cloudops/SKILL.md \
    --out <transcript-dir> --agents 3
```

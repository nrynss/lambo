# C5 — tool-calling probes (committed outputs)

The C5 runbook's finding rests on probes showing which local chat models can
and cannot emit MCP tool-call JSON. Every probe was run the same two ways:
under the OMP harness (`omp --model <id> -p --no-pty` with the workspace
`.mcp.json` pointing at a scratch `lambo serve`) and as a raw
`POST /v1/chat/completions` with a `tools` array. All runs on the **Linux
box** (cachyos x86_64, Ryzen 5 3600, RTX 4070 SUPER — not the MBP), OMP
17.3.5, llama-server build 10456, lambo serve on scratch sessions with
env-only scratch tokens (rendered as `<SCRATCH-TOKEN>` here; nothing is
redacted from the model outputs). The original LFM2 probe observations were
re-run on 2026-08-17 21:05 UTC and marked as re-runs; the Qwen3-0.6B and
functiongemma-270m files below are **fresh-run transcripts** (2026-08-18).

## LFM2-350M — cannot emit tool calls (the original finding)

| File | Result |
|---|---|
| `omp-harness-garbled-tool-call.txt` | Under the harness: long prose reply that fabricates a `lambo_derive` CLI (`lambo_derive schema auth_guards.json`, …) instead of tool-call JSON; probe server logged zero `tools/call`; store read back 0 interactions. Re-run transcript (reproduces the original). |
| `raw-tools-probe.json` | Raw `/v1/chat/completions` with a `tools` array: prose, `finish_reason: "stop"`, **no `tool_calls`**. |

## Qwen3-0.6B — emits tool_calls at the protocol level, not under OMP

| File | Result |
|---|---|
| `omp-harness-qwen3-0.6b.txt` | Under the harness (fresh run): the model **did** emit one tool call — but for the wrong tool (`lsp`, a built-in OMP tool, with hallucinated `{i, action}` arguments); the call failed, `lambo_derive` was never invoked, OMP print-mode stdout was empty, and the store read back 0 interactions. |
| `raw-tools-probe-qwen3-0.6b.json` | Raw `/v1/chat/completions` with a single-tool `tools` array (request + response embedded): `finish_reason: "tool_calls"`, one `lambo_derive` tool call with valid JSON arguments `{"concepts":[{"content":"auth middleware guards schema integrity","concept_type":"logic"}]}`. The model **can** emit correct OpenAI tool_calls when the toolset is small. |

Qwen3-0.6B therefore got a swarm run — via the spec's minimal LLM loop
(`scripts/loadtest/mcp_swarm.py`), because under OMP's harness (dozens of
tool schemas in a ~31k-token prompt) it cannot select the right tool; the
runbook records the numbers.

## functiongemma-270m — cannot emit tool_calls as served (the LFM2 finding stands)

| File | Result |
|---|---|
| `omp-harness-functiongemma-270m.txt` | Under the harness (fresh run): a single prose refusal ("I apologize, but I cannot assist with generating system reminders or technical documentation …"), **no tool call of any kind**, 0 store interactions. |
| `raw-tools-probe-functiongemma-270m.json` | Raw `/v1/chat/completions` with a `tools` array (request + both attempts embedded): the model emits FunctionGemma-native `<start_function_call>call:lambo_derive{…}<end_function_call>` markup, but this llama.cpp build returns it as **content prose** — `finish_reason: "length"` at 512 max_tokens, `finish_reason: "stop"` with the full generation, and **no `tool_calls` field** in either. |

So functiongemma-270m joins LFM2-350M: as served by this llama.cpp build it
cannot drive an OpenAI-style tool-calling loop, and **no swarm run was
performed for it** — the failure is the finding (recorded in the runbook and
concurrency-capture.md).

## What these probes establish

LFM2-350M: pseudo-tool prose under the harness, prose + `finish_reason=stop`
at the raw protocol. Qwen3-0.6B: real `tool_calls` at the raw protocol, but
wrong-tool selection under OMP's harness. functiongemma-270m: native
function-call markup that the serving build never converts to `tool_calls`.
The only harness that works with what these models can do is the minimal LLM
loop of `scripts/loadtest/mcp_swarm.py` (the loop supplies the tool-calling;
the model supplies the content) — which is what ran for Qwen3-0.6B.

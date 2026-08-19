# Evidence

Raw captures behind the claims in the README and the documentation site. Every file here
is a transcript of something that actually ran, kept as captured.

**These are verbatim captures.** Where a transcript contains a session name, a branch, or
an internal identifier that was genuinely on the wire at capture time, it stays — editing
a capture to make it read better would make it stop being evidence. The prose in this
index and in the per-directory READMEs is the curated layer; the `.txt`, `.jsonl` and
`.log` files are not.

No DSN, API key, or cluster id appears anywhere in this directory. Where a capture would
have contained one, it is replaced by a named placeholder such as `<CLUSTER_ID>` or
`<SCRATCH>`.

## MCP client interop

| Path | What it shows |
|---|---|
| [`mcp-client-stdio/`](mcp-client-stdio/) | Claude Code completing the MCP handshake against `lambo serve --transport stdio`, all seven tools driven over the real wire with requests and responses both captured, the HTTP transport serving `initialize`, and the four fail-closed config cases. |
| [`mcp-client-interop/`](mcp-client-interop/) | Two further clients. Cursor Agent CLI: handshake, 7-of-7 tool discovery, and a model-driven `derive → record_action → recall → stats` run. Claude Code **and** Cursor: model-driven calls against the **managed CockroachDB MCP server**, both returning the canonization walk the demo page documents. With OMP below, three independent clients agree on every field. Also holds the re-check of Cursor's two approval gotchas, which found the per-call gate is specific to `-p` print mode. |

## CockroachDB

| Path | What it shows |
|---|---|
| [`managed-mcp-canonization-events.md`](managed-mcp-canonization-events.md) | The managed MCP server answering `select_query` against a live cluster, with the five-event status walk cross-checked against the demo narration. |
| [`live-review-cockroach/`](live-review-cockroach/) | The live-cluster review: lease behaviour, refusal paths, CLI and MCP recall compared on the same session, and the configs used to produce each. |
| `20260811-233251-cockroach-live.txt`, `20260812-025148-…`, `20260812-201338-…` | Timestamped runs of the live conformance suite, produced by [`scripts/capture-cockroach-evidence.sh`](../scripts/capture-cockroach-evidence.sh). |

## Vector search

| Path | What it shows |
|---|---|
| [`vector-spike.txt`](vector-spike.txt) | The original spike proving `sqlx` can round-trip a `VECTOR` column, and the query shape that bypasses the index. |
| `20260812-235945-vector-index.txt` | An honest scan plan — the index was *not* being used at that point, recorded rather than hidden. |
| `20260813-130218-vector-index-predicate-finding.txt` | Why: the session predicate defeated the index. |
| `20260813-131108-vector-index-camera-proof-diagnosis.txt` | The diagnosis that followed. |
| `20260813-134333-vector-index-camera-proof-PASSING.txt` | The plan finally showing `vector search` on `concepts@concepts_embedding_idx`. |
| `20260813-145209-ann-recall-vs-beam.txt` | Recall measured against beam width on the live cluster, which is where the default beam size came from. |

Read those five in order. They are the useful sequence: a claim that did not hold, the
reason, and the fix — rather than only the final green run.

## Demo and end-to-end

| Path | What it shows |
|---|---|
| [`demo-determinism/`](demo-determinism/) | The demo's byte-identical OUTCOME property holding *by construction*: 100 consecutive parity runs with no failures, plus 20 classified run-pairs establishing that only three difference sites exist, all outside the asserted block. Read this before the two captures below. |
| `demo-live-1.txt`, `demo-live-2.txt`, `demo-live-diff.txt` | The demo scenario run twice against the live cluster, and the diff proving the two runs are identical. **Captured before the determinism fix**, when identical output held about nine runs in ten rather than by construction — the result is genuine, but `demo-determinism/` is the stronger claim. |
| `demo-live-canon-events.txt` | The canonization events the same run left behind in the database. |
| `demo-live-saints.txt`, `demo-live-conformance.txt`, `demo-live-serveweb-cockroach.txt` | The canonical-memory listing, the conformance suite, and the read-only web surface, all against that store. |
| [`e2e-gates-fable.txt`](e2e-gates-fable.txt) | The end-to-end gate run from an independent adversarial review. |

## Concurrency capture (C1–C5)

| Path | What it shows |
|---|---|
| [`concurrency/`](concurrency/) | The T8.2 N1/N2 closure capture: K=12 load driver against a scratch SQLite store, SIGTERM mid-burst with the exact `lambo serve: session closed, tail durable` line (signal→exit 1419 ms, exit 0), and the GC-accounted durability comparison (interactions AHEAD by 21; concept shortfall 107 = one daemon GC sweep, collected==gap). Runbook in [`concurrency/README.md`](concurrency/README.md). |
| [`swarm/`](swarm/) | The real-model swarm (C5, T9.6's scale half), two runs. Run 1: LFM2-350M on llama.cpp `:8081` — the finding that a 350M model cannot emit tool calls (probed under OMP and the raw OpenAI tools API), so the spec's fallback LLM loop ran: 3961 derive-calls/hour, dedup 0.183, 0 model errors, portal screenshots of the LFM2-derived concepts. Run 2: Qwen3-0.6B and functiongemma-270m. Qwen3-0.6B emits a correct `lambo_derive` tool_calls at the raw protocol level but calls the wrong tool under OMP — fallback loop ran: 2956 derive-calls/hour, dedup 0.893, 0 model errors, portal screenshots of Qwen-derived concepts. functiongemma-270m joins LFM2 as a no-tool_calls finding (its native `<start_function_call>` markup is returned as prose; no swarm ran for it). All probe transcripts committed under [`swarm/probes/`](swarm/probes/README.md); runbook in [`swarm/README.md`](swarm/README.md). |
| [`mooshik-f-sqlite-bge/`](mooshik-f-sqlite-bge/) | Semantic recall on the local store under a **real** embedder (F-R1-5): SQLite + `kind = "bge_m3"` (1024-d, local llama.cpp), three derived concepts, and recall queries sharing **no vocabulary** with them. Two of three retrieve the right concept on vector similarity alone (cosine 0.5706 and 0.5823, margins +0.16 over second place); the third ranks correctly on cosine (0.3991) but falls below recall's flat `RECENT_SCORE` floor and so does not surface — recorded rather than dropped. Also captures the `store.vector_dim` pin refusing a width disagreement that was unreachable before F remediation. Runbook in [`mooshik-f-sqlite-bge/README.md`](mooshik-f-sqlite-bge/README.md). |
| [`bedrock-blocked.txt`](bedrock-blocked.txt) | The Bedrock authorization refusal — why the AWS embedder path is reserved but unimplemented. |

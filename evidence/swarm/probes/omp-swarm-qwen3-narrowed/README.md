# C5M-R1-1a — OMP swarm re-run with the narrowed toolset + skill in the system prompt

Attempted 2026-08-18 02:58-02:59 UTC (sessions 2026-08-18T02-58-21-*Z), 3
concurrent `omp` agents, `--no-tools --append-system-prompt=<lambo-cloudops
SKILL.md>`, isolated profile, workspace `.mcp.json` pointing at a scratch
`lambo serve` on :7704 (fresh store, session c-qwen3-omp-swarm-20260818).
Driver: `scripts/loadtest/omp_swarm.py`. This is the C5M-R1-1 (a) leg: the
narrowed probe showed Qwen3-0.6B selects `lambo_derive` correctly under
`--no-tools`, so the swarm was re-run under OMP with the skill in the system
prompt, exactly as the round-1 remediation asked.

## Per-agent results (from the OMP session records, extracted verbatim)

| Agent | Topic | Lambo calls (executed) | Sequence | Outcome |
|---|---|---|---|---|
| agent-0 | billing service retries failed charges | 12 | recall→derive→record_action→recall ×4 | rc=0; ended with "DONE" |
| agent-1 | rate limit protects the public API | 1 executed (+2 not executed) | recall, then derive+record_action failed | rc=1: provider stream error ("OpenAI completions stream closed before a finish_reason was received") |
| agent-2 | auth middleware guards the user schema | 5 | recall→derive(2 created,1 matched)→record_action→recall×2 | rc=0; exited after a recall without a final text |

Every derive agent-0 and agent-2 executed was preceded by a `lambo_recall`
in that agent's sequence — the pre-flight protocol shape emerged from the
model itself, with the skill text as the only instruction. agent-2's first
recall returned the harness-live concept "auth middleware guards schema
integrity" and agent-0's recalls returned "billing service" / "WebTier-API
is a service that reaches the database through SG-Base-VPC" — content that
exists in none of the scratch stores committed under `evidence/swarm/`.

## The execution-target caveat (critical for reading this transcript)

None of these calls reached the workspace scratch server on :7704. Its store
read back 0 interactions / 0 concepts / 0 edges after the run and its stderr
logged no daemon event lines; the session records' toolResults instead carry
the attribution warning "this process owns the session as agent
'cursor-agent'; the call from 'omp-swarm' is recorded in the graph as
'cursor-agent'". The lambo tools OMP executed are the harness-INHERITED
`mcp__lambo_*` MCP servers (loaded from the parent omp session's broker for
every child process, independent of the workspace `.mcp.json` — see
`../omp-harness-qwen3-narrowed.txt` and `../omp-request-tool-context.jsonl`),
and Qwen3-0.6B selects those inherited prefixed lambo tools every time, even
when a distinct toolPrefix is configured on the workspace server
(`prefix-test-toolprefix-scratch.session.txt`: the prompt said
`scratch_lambo_derive`, the model still called `mcp__lambo_derive`).

Consequences, stated plainly:

1. **A scratch-isolated swarm under OMP is not achievable in this harness.**
   The workspace-scoped lambo server is shadowed by the inherited one; the
   model's lambo calls land on the harness's live lambo (agent
   'cursor-agent'). The OMP swarm run therefore executed against that live
   server, not a scratch store.
2. **The run wrote to the harness's live store.** The session records show
   server responses "derived 1 concept(s): 1 created" (agent-0: "billing
   service"; agent-2: 2 concepts incl. "auth middleware guard schema
   integrity") and "recorded action 'record action for retry policy': 3
   concept(s) created, 2 edge(s) added". These are real writes to the
   inherited live session's store, made unintentionally (the probe
   workspace's scratch server was the intended target). They are NOT part of
   any committed ledger, and the live store is outside the scratch
   accounting of this branch. If the inherited live store is the exhibit
   environment, those stray concepts ("auth middleware guards schema
   integrity", "billing service", "record action for retry policy", the
   02:53 skill-test derives "database"/"rds_database"/"ec2_instance"/
   "lambda_function"/"delete_database" and their record-action nodes) may
   need manual removal by the harness operator.
3. **The tool-selection result still stands.** What this leg proves — the
   C5M-R1-1 counterfactual — is the SELECTION: with the narrowed toolset and
   the skill in the system prompt, the model drives protocol-shaped lambo
   call sequences (recall→derive→record_action→recall) instead of calling
   `lsp`. What it does not prove is any scratch-store durability or
   ledger-exact accounting for the OMP leg; that evidence comes only from
   the raw-protocol agentic re-run (`mcp_agentic.py`,
   `../ledger-agentic-qwen3-1787022500.jsonl`), which is scratch-isolated
   and store-verified.

## The drift finding

agent-2's session also shows the model drifting into the inherited
`mcp__openaideveloperdocs_*` tools (fetch/search/list calls) and then
repeating the same lambo sequence until OMP's tool-call loop guard started
shaking context ("[shaken ~N tokens — recover: artifact://N]"). The
inherited openaiDeveloperDocs MCP servers are irrelevant to the task and
distract the model; this is another concrete cost of OMP's inability to
exclude inherited MCP servers, and it is recorded as-is.

## Files

| File | What it shows |
|---|---|
| `agent-0-billing-retries.session.txt` | Extracted OMP session: 12 lambo calls in 4 protocol cycles, ends "DONE" |
| `agent-1-rate-limit.session.txt` | Extracted OMP session: recall executed, derive+record_action not executed (provider stream error, rc=1) |
| `agent-2-auth-middleware.session.txt` | Extracted OMP session: recall→derive(2 created)→record_action→recall×2, exit without final text |
| `prefix-test-toolprefix-scratch.session.txt` | Control: with `toolPrefix: "scratch"` on the workspace server the model still called `mcp__lambo_derive` (the inherited one) |
| `agent-*.stdout.txt` / `agent-*.stderr.txt` | Driver-captured process output (print-mode stdout empty when the session ends on a tool call; agent-1's stderr carries the provider-stream error) |
| `omp_swarm.py` (scripts/loadtest/) | The driver: spawns the concurrent omp processes, captures transcripts |

Reproduce: `python3 scripts/loadtest/omp_swarm.py --cwd <ws> --agent-dir
<isolated-profile> --skill skills/lambo-cloudops/SKILL.md --out <out>
--agents 3` with the workspace `.mcp.json` pointing at the scratch serve.

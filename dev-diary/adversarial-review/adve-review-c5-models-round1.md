# Adversarial Review: C5 model re-run (worktree `c5-models`, round 1)

```text
╔════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN — Round 1 of the review/remediate loop           ║
║  Scope:  C5 re-run — Qwen3-0.6B swarm + functiongemma-270m     ║
║          no-tool-call finding (commit 957326e on               ║
║          codex/c5-models, base main e3715e8)                   ║
║  Branch: codex/c5-models (worktree /home/nryn/work/lambo/      ║
║          worktrees/c5-models) — DO NOT MERGE (validation)      ║
║  Date:   2026-08-18                                            ║
║  Reviewer: C5ModelsReviewR1 (fresh, read-only)                 ║
║  Verdict: REQUEST_CHANGES — 2 P2 / 2 P3.                       ║
║          Every probe, ledger, durability, and portal claim     ║
║          verifies against the committed artifacts (incl. the   ║
║          OMP session records under ~/.omp, re-derived ledger   ║
║          arithmetic, and OCR of the portal PNGs); the P2s are  ║
║          evidence-completeness gaps in the OMP-infeasibility   ║
║          claim (no narrowed-toolset attempt) and the           ║
║          "real-model swarm" agency framing (the fallback loop  ║
║          gives the model no lambo protocol context).           ║
╚════════════════════════════════════════════════════════════════╝
```

## Grounding

Reviewed read-only against the committed artifacts (no live swarm re-run, no
`cargo publish`; llama-servers `:8082`/`:8083` were confirmed still listening
and left running). Diff `e3715e8..957326e` is 18 files: `scripts/loadtest/
mcp_swarm.py`, `scripts/recording/capture-swarm-portal.mjs`,
`evidence/swarm/` (README, ledger, stderr ×2, store readbacks ×2, portal PNGs
×2, `probes/` ×4 + README), and the docs (`dev-diary/notes/
concurrency-capture.md` C5, `dev-diary/README.md` board row,
`evidence/README.md`). **No Rust production code in the diff** (verified:
`git diff --name-only` contains no `.rs`/`Cargo` paths). Authority read first:
`dev-diary/notes/concurrency-capture.md` C5 and `evidence/swarm/README.md`.

Checks run, every number re-derived from the artifacts (not from the docs'
claims):

- **Qwen3-0.6B raw probe** (`probes/raw-tools-probe-qwen3-0.6b.json`):
  `finish_reason: "tool_calls"`, one `lambo_derive` tool call, arguments
  `{"concepts":[{"content":"auth middleware guards schema integrity",
  "concept_type":"logic"}]}` — valid JSON, parses. ✓
- **Qwen3-0.6B OMP probe** — verified against the actual OMP session record
  `~/.omp/agent/sessions/-tmp-c-series-scratch-qwen3-ws/
  2026-08-18T02-22-21-162Z_*.jsonl` (not just the committed transcript): the
  model emitted exactly one tool call, `lsp`, id `REhEfyP1117pllTMuYyhSe4JBI7i5lwC`,
  arguments `{"i": "derive concept: auth middleware guards schema integrity",
  "action": "definition"}` — byte-identical to the transcript's "verbatim
  extract"; the call's `details.success` is `false` (the transcript's "the
  call failed"); no `lambo_derive` tool call exists in the session. The
  transcript's honesty claim — stdout empty, extract from the session record —
  is accurate and correctly marked. Probe-phase store 0 interactions/0
  concepts is consistent with `stderr-serve-qwen3-*.log` (no daemon event
  lines between attach 02:22:13Z and the swarm's first derive 02:26:37Z). ✓
- **functiongemma-270m OMP probe** — session record
  `-tmp-c-series-scratch-fgemma-ws/2026-08-18T02-24-04-143Z_*.jsonl` shows a
  single assistant text message, byte-identical to the transcript's quoted
  refusal, and **no tool call** (only `session_exit`). ✓
- **functiongemma-270m raw probe** — `<start_function_call>call:lambo_derive
  {…}<end_function_call>` markup returned as content prose in both attempts;
  attempt 1 `finish_reason: "length"` at max_tokens 512
  (`completion_tokens: 512`), attempt 2 `finish_reason: "stop"` with the full
  generation (`completion_tokens: 1808`, so a >512 budget), **no `tool_calls`
  field** in either. ✓
- **Swarm ledger** (`ledger-qwen3-1787019996.jsonl`, 285 records): 124
  `derive` (all `ok: true`), 124 `recall`, 35 `model_reply`
  (`parsed_concepts: 0`), 0 `model_error`, 1 `meta`, 1 `done`. Model turns =
  124 + 35 = 159; unparseable = 35/159 = 22.01%. Rates re-derived:
  124/151 × 3600 = 2956.3 → **2956**; (124+124)/151 × 3600 = 5912.6 → **5913**.
  Created/matched from the server's own response texts: **27 / 225**; dedup
  225/252 = 0.8929 → **0.893**. (Ledger `concepts` arrays actually hold 255
  objects — see C5M-R1-4.) Derive window: first `t` 1787019997.55 (02:26:37.5Z,
  coincides with the hybrid-degradation WARN at 02:26:37.549Z) → last
  1787020146.84; the stated 151 s window is meta `started_at` → `done`
  (151.0 s), which is what the rates divide by.
- **Durability**: store readback 124 interactions == 124 derives; 27 concepts
  == 27 created; edges 404 (claimed as lower bound — derive edges are not
  reported by the server, so 404 is the store total; correct as phrased);
  `lease_rows` 0. Server exit lines verbatim in `stderr-serve-qwen3-*.log`:
  `shutdown signal received, winding down` (02:29:23.527356Z) →
  `session closed, tail durable` (02:29:23.527618Z), gap **0.262 ms ≈ 0.3 ms**.
  fgemma session: 0/0/0/0, closed the same way (gap 0.337 ms). ✓
- **Portal PNGs**: both 3200×1800 RGB (Pillow). OCR (tesseract): the
  auth-guard capture's first card reads "auth middleware guards the user
  schema … Score 5.78"; the rate-limit capture's first card reads "Rate limit
  protects the public API … Score 2.21" plus the exact ledger variants "Rate
  limit protects the public API [Resource] (score 2.36/2.45/2.64)" — real
  Qwen3-derived concepts rendered as recall cards. The capture script's
  console filter is URL-aware and benign (drops console errors whose
  `location().url` or text contains `favicon`; the response handler drops
  `>=400` only for `/favicon` URLs). ✓
- **Honesty footnote** (recall-context echo): present (README:111-116) and
  accurate — "Rate limit [Resource] (score 1.50)" appears in recall texts (the
  context the model was handed) AND as a derived concept; the model re-derived
  the text it was given. ✓
- **Scope/hygiene**: only `scripts/loadtest`, `scripts/recording`, `evidence/`,
  `dev-diary` docs changed; `python3 -m py_compile scripts/loadtest/mcp_swarm.py`
  clean; `node --check scripts/recording/capture-swarm-portal.mjs` clean;
  `git diff --check e3715e8..HEAD` clean. No secrets: no bearer tokens, keys,
  or 32+ char secrets in the diff (scratch token appears only as
  `<SCRATCH-TOKEN>` in committed files; the real token is env-only and in the
  gitignored `/tmp` workspace `.mcp.json`). Scratch sessions only
  (`c-swarm-qwen3-20260818`, `c-swarm-fgemma-20260818`, both `existing=false`
  in stderr).
- **Regression**: `evidence/concurrency/`, the C1–C4 sections, and the
  round-1/round-2 C-series review records are untouched by the diff; Run 1's
  table row (3961 / 0.183 / 25% (54/218)) still matches the original ledger
  claims; nothing in this branch contradicts `evidence/concurrency/` or the
  merged capture.

## Findings

### C5M-R1-1 (P2) — Qwen3-0.6B "cannot drive OMP" is under-evidenced: only the default full toolset was probed; no narrowed-toolset attempt exists

- **Where:** `evidence/swarm/README.md:27-32,84` ("the model cannot select the
  right tool under it"); `probes/omp-harness-qwen3-0.6b.txt:73-78` (verdict:
  "Qwen3-0.6B does not drive the OMP harness"); `dev-diary/notes/
  concurrency-capture.md:159-162`; the "~31k-token prompt" figure in
  README:28 and probes/README:31.
- **Evidence:** The raw probe (single tool) yields a correct `lambo_derive`
  `tool_calls` (`raw-tools-probe-qwen3-0.6b.json:64-79`), so tool-call
  emission works for this model at a minimal toolset. The only OMP attempt
  used OMP's full default toolset: the probe workspace `.mcp.json` registers
  only the lambo MCP server, but OMP's built-in tools stayed in the model's
  context — the session record shows the model chose the built-in `lsp`. No
  narrowed-toolset OMP attempt (lambo tools only, or OMP's tool-restriction
  configuration) is recorded anywhere in the repo or the scratch workspaces.
  The "~31k-token" prompt-size figure appears in no committed artifact (only
  the llama-server context flag `-c 32768` is documented). The conclusion
  that the model "cannot select the right tool under [OMP]" — and hence the
  decision to run the fallback harness — rests on a single full-toolset
  condition; the counterfactual the model demonstrably satisfies (correct
  selection at a minimal toolset) was never exercised under OMP.
- **Required remediation:** (a) run a narrowed-toolset OMP probe for
  Qwen3-0.6B (lambo tools only; if OMP cannot drop its built-in tools, record
  that) and commit the transcript; or (b) explicitly scope the claim —
  "with OMP's default full toolset it selected the wrong tool; no
  narrowed-toolset attempt was made" — in the README, the probe transcript
  verdict, and the capture note, and attribute the prompt-size figure or drop
  it.

### C5M-R1-2 (P2) — The fallback swarm loop gives the model no lambo protocol context; the "real-model swarm / 3 agents" framing overstates agency

- **Where:** `evidence/swarm/README.md:80-91` (run description) and the
  metrics table `:102-109`; `dev-diary/README.md:180` (board row: "C5
  (real-model swarm): DONE-with-findings … 3 agents × 150 s, 2956
  derive-calls/hour"); `dev-diary/notes/concurrency-capture.md:159-165`.
- **Evidence:** `scripts/loadtest/mcp_swarm.py:52-58` — the SYSTEM prompt is
  content-generation-only ("Respond with EXACTLY one JSON object … List 2-4
  concrete concepts about the subject … Never wrap in markdown"): no lambo
  tool names, no derive/recall semantics, no blast-radius/load-bearing
  guidance, no AGENTS.md/skill. `agent_loop` (`:200-246`) hardcodes
  prompt → `model_reply` → `extract_concepts` → `lambo_derive` →
  `lambo_recall`; the model never chooses a tool, never triggers a recall, and
  never controls cadence — the loop supplies all agency. The runbook does
  disclose "The model supplies the content; the loop supplies the
  tool-calling" (README:90-91), but the C5 status summary and the board row
  present "3 agents × 150 s, 2956 derive-calls/hour" as the swarm outcome
  without stating that the model received no protocol/agentic context — the
  headline numbers measure loop throughput plus the model's concept-text
  canonization behavior, not model-driven tool selection.
- **Required remediation:** add the explicit disclosure (verified above: the
  model was given no lambo protocol and made no tool decisions) to
  `evidence/swarm/README.md` and `concurrency-capture.md` C5; and/or run a
  genuine agentic re-run (system prompt = the lambo-cloudops skill or a swarm
  AGENTS.md, minimal lambo-only toolset, Qwen3-0.6B choosing the calls),
  committing its ledger and transcripts.

### C5M-R1-3 (P3) — Probe transcripts label IST wall-clock time as UTC

- **Where:** `probes/omp-harness-qwen3-0.6b.txt:4` ("~07:52 UTC"),
  `raw-tools-probe-qwen3-0.6b.json:3` ("~07:54 UTC"),
  `probes/omp-harness-functiongemma-270m.txt:4` ("~07:57 UTC"),
  `raw-tools-probe-functiongemma-270m.json:3` ("~07:57-07:58 UTC").
- **Evidence:** The box is `Asia/Kolkata` (IST, +0530; `timedatectl`). The
  embedded UTC timestamps are unambiguous: OMP session files
  `2026-08-18T02-22-21-162Z` and `2026-08-18T02-24-04-143Z`; raw response
  `created` epochs 1787019793 = 02:23:13Z, 1787019855 = 02:24:15Z,
  1787019904 = 02:25:04Z. All four transcripts' "~07:5x UTC" values are the
  box's IST wall-clock mislabeled as UTC; the actual UTC run times are
  02:22–02:25. Dates and run ordering (probes before swarm derives) are
  correct; only the time-of-day labels are wrong.
- **Required remediation:** correct the four timestamps to UTC
  (02:22–02:25Z) or relabel them IST.

### C5M-R1-4 (P3) — Portal-string occurrence counts in the swarm README don't match the ledger as stated; "252 concept references" is the server's accounting, not the ledger's 255

- **Where:** `evidence/swarm/README.md:133-144` ("both strings verbatim in
  the swarm ledger's derive concepts (38 and 70 occurrences respectively)")
  and `:107-108` ("225 matched existing of 252 concept references").
- **Evidence:** Re-derived from the ledger: "auth middleware guards the user
  schema" occurs **2×** in derive concepts (38 is the whole-ledger
  occurrence count — 35 of those are in recall texts, 1 in a `model_reply`);
  "Rate limit protects the public API" occurs **25×** in derive concepts
  (case-insensitive); 70 is the count of ledger **records** containing it
  (total occurrences 132). The card texts themselves are verified (OCR), so
  only the supporting counts are mischaracterized. Separately, the ledger's
  derive records carry **255** concept objects while "252 concept references"
  (= 27 created + 225 matched, the server's own per-call counts) is the dedup
  denominator; the 3-reference gap is derive `seq 34 worker 1`, which shipped
  four schema-placeholder concepts — content literally `<concept text>`, the
  model echoing the SYSTEM template (`mcp_swarm.py:55`) — of which the server
  accounted 1 created (one of the 27 store concepts may literally be
  "<concept text>"). The honesty footnote discloses recall-context echo but
  not this placeholder-echo failure mode.
- **Required remediation:** restate the counts precisely (derive-concept
  occurrences 2 and 25; whole-ledger occurrences 38 and 132; "70" = records
  containing the string) and disclose the placeholder-echo derive in the
  footnote, or drop the parenthetical counts.

## Verdict

**REQUEST_CHANGES — 2 P2 / 2 P3.** The run's core evidence is sound and fully
traceable: both probes verify at the byte level against the OMP session
records and embedded response JSON; the ledger arithmetic (2956/5913,
27/225/252, 0.893, 22%, 0 model errors), the durability accounting
(124==124, 27==27, 404 lower bound, lease 0, 0.262 ms gap, exact shutdown
lines), the portal captures (OCR-verified card text matching ledger
concepts), the scope hygiene (no Rust, no secrets, clean compile/diff checks,
main checkout restored to its 3 pre-existing untracked files), and the
regression sweep (prior C-series artifacts and records untouched) all check
out. The P2s are evidence-completeness gaps in how the branch's headline
claims are worded: the OMP-infeasibility conclusion for Qwen3-0.6B was
tested only under the default full toolset (C5M-R1-1), and the fallback
"swarm" gave the model no protocol context at all, which the summary framing
does not state (C5M-R1-2). Both are cheap to remediate (scoped wording and
explicit disclosure, or a narrowed-toolset/agentic probe). Branch stays
**unmerged** per the validation-only instruction.

- **Status: AWAITING independent re-review (round 2).**

## Remediation disposition (C5M-R1-1..4)

```text
Agent:      C5ModelsRemediationR1
Date:       2026-08-18
Remediation: decdc74 (fix(c-series): C5M round-1 remediation)
Disposition: this document (docs(review): C5M round-1 remediation disposition)
Status:     AWAITING independent re-review (round 2) — branch stays unmerged
            per the validation-only instruction.
```

### C5M-R1-1 (P2) — narrowed-toolset OMP probe — RESOLVED (both arms, honest)

The counterfactual was run and it satisfies the finding's test: with OMP's
toolset narrowed (`omp --no-tools` — the request-level context is captured
verbatim in `evidence/swarm/probes/omp-request-tool-context.jsonl`: 15 tools
= read/write/edit + 7 lambo MCP + 5 inherited openaiDeveloperDocs),
Qwen3-0.6B emits a correct `mcp__lambo_derive` tool call under OMP (session
2026-08-18T02-51-32-500Z, transcript `omp-harness-qwen3-narrowed.txt`) —
the original `lsp` selection is a default-full-toolset result, and every OMP
claim in the repo is now scoped to that. OMP **cannot** provide a lambo-only
toolset (read/write/edit and all configured MCP servers always load; no flag
excludes them) — recorded with the request-level evidence. New honest
disclosure: in this harness the inherited `mcp__lambo_*` server shadows the
workspace-scoped scratch lambo, so the OMP leg's lambo calls executed
against the harness's live lambo (agent 'cursor-agent'), not a scratch
store — scratch stores read back 0 rows. The OMP swarm re-run with the skill
in the system prompt was attempted (3 agents; the model drove
recall→derive→record_action→recall sequences, one agent ended "DONE", one
hit a provider-stream error, one drifted into the inherited
openaiDeveloperDocs tools) and is recorded with the same caveat
(`omp-swarm-qwen3-narrowed/`). The "~31k-token prompt" figure is dropped
(no committed artifact supports it; the measured context is the 15-tool
request).

### C5M-R1-2 (P2) — the genuine agentic re-run — RESOLVED

`scripts/loadtest/mcp_agentic.py` ran the real thing: system prompt = the
lambo-cloudops skill text verbatim (`skills/lambo-cloudops/SKILL.md`, sha256
`fb9462e5…` in the ledger `meta`), minimal toolset = the four lambo MCP
tools only, model-chosen calls via llama.cpp's OpenAI tools API on :8082.
Numbers (ledger `evidence/swarm/ledger-agentic-qwen3-1787022500.jsonl`,
3 agents × 151.0 s, fresh scratch store + session):

| Metric | Value |
|---|---|
| Tasks | 55 (47 model-completed, 8 cut off by llama-server HTTP 500s) |
| Tasks/hour | 1120 completed |
| Tool calls | 173 (86 recall / 45 derive / 40 record_action / 2 inspect; 165 ok) |
| **Protocol adherence** | **43/55 tasks (78%) recall-first; 0 of 45 derives without a prior recall in the same task**; the 12 non-recall-first tasks made zero tool calls (no protocol action taken — recorded) |
| Derive / recall | 42 ok derives / 81 ok recalls (~1.9:1, as the protocol demands) |
| Dedup rate | 0.857 (36 matched of 42 successful derives) |
| Unparseable turns | 15/106 (14.2%) empty (no content, no tool_calls); 8 HTTP 500 failures — all recorded, none hidden |
| Durability (clean SIGTERM) | interactions 82 == 82, concepts 12 == 12, edges 132 ≥ 3, lease 0 — "tail durable" (`durability-agentic-qwen3-1787022500.txt`); server exit lines verbatim, 0.288 ms gap |

The fallback swarm's no-protocol/no-agency limitation is now explicitly
disclosed in `evidence/swarm/README.md` and `concurrency-capture.md` C5, and
the headline C5 numbers are labeled as loop-throughput + concept-text
measurement, not model-driven tool selection.

### C5M-R1-3 (P3) — probe timestamps — RESOLVED

All four transcripts corrected to the actual UTC run times, citing the
embedded timestamps: OMP session files 2026-08-18T02-22-21-162Z
(`omp-harness-qwen3-0.6b.txt` → ~02:22 UTC) and 2026-08-18T02-24-04-143Z
(`omp-harness-functiongemma-270m.txt` → ~02:24 UTC); response `created`
epochs 1787019793 = 02:23:13Z (`raw-tools-probe-qwen3-0.6b.json` → ~02:23
UTC) and 1787019855 / 1787019904 = 02:24:15Z / 02:25:04Z
(`raw-tools-probe-functiongemma-270m.json` → ~02:24-02:25 UTC). Each notes
that the earlier "~07:5x UTC" label was the box's IST wall-clock (+0530)
mislabeled as UTC.

### C5M-R1-4 (P3) — portal-string counts + placeholder echo — RESOLVED

README:133-144 restated precisely (re-derived from the ledger): "auth
middleware guards the user schema" — 2× in derive concepts, 38 whole-ledger
records containing it (35 recall + 1 model_reply + 2 derive); "Rate limit
protects the public API" — 25× in derive concepts (case-insensitive), 132
occurrences across 70 ledger records counting the exact case-sensitive
phrase (23 derive + 41 recall + 6 model_reply; case-insensitive whole-ledger
136 in 74). "252 concept references" labeled as the server's own per-call
accounting (27 created + 225 matched) vs the ledger's 255 concept objects;
the 3-reference gap is derive `seq 34 worker 1`, which shipped four
`<concept text>` placeholder concepts (the model echoing the SYSTEM
template), server-accounted 1 created — disclosed in the honesty footnote.

### Scope and hygiene

Only `scripts/loadtest/`, `evidence/swarm/`, and the three diary docs
changed; no Rust production code; `python3 -m py_compile` clean on the
changed scripts; the two edited probe JSONs parse; `node --check
capture-swarm-portal.mjs` clean (untouched); `git diff --check` clean; the
two probe JSON files re-validated after the timestamp edits. Scratch
sessions only for the new runs (agentic store on /tmp, WAL-verified);
llama-servers left running. The OMP leg's unintended writes to the
harness-inherited live lambo (agent 'cursor-agent') are disclosed in
`omp-harness-qwen3-narrowed.txt` / `omp-swarm-qwen3-narrowed/README.md` for
operator action if that store matters.

**Status: AWAITING independent re-review (round 2).** The user's
validation-only instruction stands: this branch is not merged.

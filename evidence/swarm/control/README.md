# Control arm — Qwen3-0.6B agentic run, no protocol in the system prompt

This directory holds a control for the treatment run described in the
"[The Qwen3-0.6B agentic re-run](../README.md#the-qwen3-06b-agentic-re-run)"
section of `evidence/swarm/README.md` (`ledger-agentic-qwen3-1787022500.jsonl`).
The treatment gave Qwen3-0.6B the `lambo-cloudops` skill as its system
prompt plus the four Lambo MCP tools, and measured recall-before-write
behavior. Nobody had run the identical harness *without* the skill, so the
treatment numbers alone could not separate "the skill taught the protocol"
from "the tasks happened to invite lookups." This is that missing control.

**The one variable changed is the `--skill` file.** No code changes were
made to `scripts/loadtest/mcp_agentic.py`. Everything else — model,
quantization, endpoint shapes, agent count, duration, task set, tool
schemas fetched live from `tools/list` — is identical between the two runs.

## Headline

**The control's recall-first rate (35.0%) is well below the treatment's
(78.2%) — a 43-point gap. This is not a null result.** Under the same
tasks, same model, same four tools, with a system prompt that explains what
each tool does but never says to check memory before writing, Qwen3-0.6B
recalled first on barely a third of tasks; with the `lambo-cloudops`
skill's explicit pre-flight protocol in the system prompt, it did so on
over three-quarters. The skill measurably moves the behavior — tool
awareness alone does not produce the treatment's recall-first rate.

See "What this does and does not establish" below for the one important
caveat: the task text itself (baked into `mcp_agentic.py`, identical in
both arms, out of scope to edit) says "run the pre-flight recall protocol"
and "Halt if recall shows a load-bearing pillar" — so even the control arm
was not reading protocol-free instructions end to end. The comparison is
still valid and the gap still real, but it is a comparison between "skill
protocol reinforced by both the system prompt and the task text" and "the
same task-text hint alone, with no system-prompt reinforcement," not
between "protocol" and "zero protocol language anywhere."

## Side-by-side: treatment vs control

Both columns computed by the same code path,
`evidence/swarm/control/compute_metrics.py`, run against the treatment's
committed ledger and the control's ledger (not quoted from prose on either
side).

| Metric | Treatment (skill) | Control (baseline) |
|---|---|---|
| Ledger | `evidence/swarm/ledger-agentic-qwen3-1787022500.jsonl` | `evidence/swarm/control/ledger-control-1787024872.jsonl` |
| System prompt | `skills/lambo-cloudops/SKILL.md` (sha256 `fb9462e5…eedec7`) | `evidence/swarm/control/baseline-prompt.md` (sha256 `a9103b28…6fc26da64`) |
| Agents × duration | 3 × 150.0s (151.0s wall) | 3 × 150.0s |
| Tasks total | 55 | 60 |
| Tasks recall-first | **43 (78.2%)** | **21 (35.0%)** |
| Derive calls (total, incl. errors) | 45 | 19 |
| Derives without a prior recall (same task) | **0** | **1** |
| Tool calls total | 173 | 67 |
| — `lambo_recall` | 86 (81 ok, 5 err) | 23 (19 ok, 4 err) |
| — `lambo_derive` | 45 (42 ok, 3 err) | 19 (17 ok, 2 err) |
| — `lambo_record_action` | 40 (40 ok, 0 err) | 16 (10 ok, 6 err) |
| — `lambo_inspect` | 2 (2 ok, 0 err) | 9 (3 ok, 6 err) |
| Calls ok / err | 165 / 8 (95.4% ok) | 49 / 18 (73.1% ok) |
| Successful derive calls | 42 | 17 |
| Dedup rate (matched / successful derives) | **0.857** (36/42) | **0.588** (10/17) |
| Model turns total | 106 | 73 |
| Unparseable/empty turns | 15 (14.2%) | 38 (52.1%) |
| Model errors (llama-server HTTP 500) | 8 | 17 |
| Durability after SIGTERM | tail durable, MATCH | tail durable, MATCH |

Raw computed JSON for both ledgers is reproducible with:

```bash
python3 evidence/swarm/control/compute_metrics.py \
    evidence/swarm/ledger-agentic-qwen3-1787022500.jsonl \
    evidence/swarm/control/ledger-control-1787024872.jsonl
```

### A secondary, honest observation: infra noise was higher in the control run

The control run's tool-call error rate (26.9%) and unparseable-turn rate
(52.1%) are both markedly higher than the treatment's (4.6%, 14.2%). This
machine was running four concurrent `llama-server` processes (BGE-M3 on
`:8080`, LFM2-350M on `:8081`, Qwen3-0.6B on `:8082`, functiongemma-270m on
`:8083`) plus an unrelated pre-existing `lambo serve` on `:7705` throughout
this session (`ps aux` at run time, load average ~3.8 on a 12-core box —
none of these were started by this control run and none were touched or
killed). The elevated Qwen3 HTTP 500 rate (17 `model_error` records vs the
treatment's 8) is the same known failure mode documented for the treatment
("llama-server HTTP Error 500 under concurrent load") — it happened more
often here, most plausibly because of the extra concurrent load, not
because of a different failure mode. This is reported rather than smoothed
over: it means the control's absolute call counts are lower than the
treatment's (67 vs 173 executed calls in the same 150s window), but the
recall-first and dedup-rate metrics above are fractions computed only over
executed/successful calls, so they are not mechanically inflated or
deflated by the error-rate difference.

## What this comparison does and does not establish

**Does establish:**
- Under identical tooling, task set, model, and window, the presence of the
  `lambo-cloudops` skill's explicit pre-flight recall protocol in the
  system prompt corresponds to a much higher recall-first rate (78.2% vs
  35.0%) and a much higher dedup rate (0.857 vs 0.588) than a baseline
  prompt that explains the same four tools mechanically but gives no
  recall-before-write instruction.
- The treatment's "0 of 45 derives without a prior recall" result is not
  simply what any tool-aware Qwen3-0.6B run produces on these tasks: the
  control, run under the same task set, produced 1 derive without a prior
  recall out of 19.

**Does not establish:**
- A clean "protocol vs. zero protocol" comparison. `scripts/loadtest/
  mcp_agentic.py`'s hardcoded `AGENTIC_TASKS` user-turn text — identical in
  both arms, out of scope to edit per this task's instructions — already
  says "run the pre-flight recall protocol for it... Halt if recall shows a
  load-bearing pillar" for every task. The control's 35.0% recall-first
  rate is the rate produced by that task-level hint *alone*, without
  system-prompt reinforcement — not the rate for a fully protocol-free
  prompt. If anything this makes the observed 43-point gap a conservative
  estimate of the skill's effect, since the control still carried a partial
  hint.
- Causal mechanism. This is one 150-second window per arm on a shared,
  noisy machine (four LLM servers + other processes). It is not a
  statistically powered study, has no replicate runs per arm, and the
  control run's higher tool-error / model-error rate (see above) is a
  measured confound in absolute call volume, even though it does not
  appear to bias the recall-first/dedup fractions computed above.
- Anything about `lambo_inspect`: the treatment used it only twice (never
  described in the skill's protocol text); the control used it 9 times
  (its plain mechanical description is only in the control prompt, since
  the skill never itemizes it outside protocol sections that were
  removed). This asymmetry is a direct, expected consequence of how the
  two prompts were built and is not a finding about the tool itself.

## Baseline prompt construction

`evidence/swarm/control/baseline-prompt.md` was derived from
`skills/lambo-cloudops/SKILL.md` **by removal** — matching tone and tool
coverage as closely as removal allows (53 lines / 354 words vs the skill's
167 lines / 1031 words; the skill is longer because most of what was
removed was the protocol prose itself, which the control cannot contain).

**Kept** (trimmed of protocol language):
- The opening role framing — "an agent working in a multi-agent cloud-ops
  environment... working against one Lambo session" — with every clause
  about protection, blast radius, canonical status, or checking removed.
- Section 0 "Session and surfaces": the session-id mechanic, the tool list
  (trimmed to the four tools this run actually exposes), the
  single-writer-lease / sequence-your-writes bullet, and the "never send a
  timestamp" bullet. These are operating mechanics, not guidance on when or
  why to call a tool.

**Removed entirely:**
- Section 1 "Pre-flight recall protocol" — the check-memory-before-writing
  instruction itself, the exact variable under test.
- Section 2 "Provenance and derivation protocol" — the
  record-your-dependencies-after-provisioning protocol.
- Section 3 "CockroachDB direct inspection" — canonical-status /
  blast-radius interpretation guidance.
- Section 4 "Verifying this skill" — a self-check built on the pre-flight
  protocol and the blocking-warning line.
- "Honest boundaries" — fail-closed semantics, blast-radius caveats,
  load-bearing-pillar language.

**Added** (not a removal — SKILL.md never itemizes the four tools'
arguments outside the protocol prose it embeds them in): a plain "The four
tools" section. Three of the four descriptions paraphrase argument syntax
that appeared inside the removed protocol sections (`lambo_recall`'s
`--query`, `lambo_derive`'s `--content`/`--kind`, `lambo_record_action`'s
`--produces`/`--modifies`/`--depends-on`), stripped of the surrounding
"MUST run this before a destructive command" / halt-on-pillar language.
`lambo_inspect` has no protocol-section source to strip from (SKILL.md only
ever names it); its description was written fresh from the tool's MCP
schema (`focus`, `depth`), with no canonization/blast-radius framing.

No output/formatting expectations were found in SKILL.md to keep or drop
beyond the removed halt-on-warning language, so the baseline sets none.

**Deviation worth recording:** the first draft of this file carried its
removal/kept documentation as an HTML comment at the top of the same file
used as `--skill`. Since `mcp_agentic.py` reads the entire `--skill` file
verbatim as the system prompt (confirmed by the ledger's `skill_sha256`
matching a hash of the whole file, comment included), that comment —
which named "pre-flight recall protocol," "blocking-warning," "blast
radius," "load-bearing-pillar" to describe what had been removed — was
itself fed to the model as part of its system prompt. That is exactly the
leakage this control is required to avoid, so the run made under that
prompt (sha256 `6e558259…6fc26da64`) was discarded and archived under
`attempt1-contaminated-prompt/` rather than reported as the control. The
file was rewritten with zero commentary — pure tool-reference prompt only
— re-hashed (sha256 `a9103b28…6fc26da64`), and the run was redone end to
end against a fresh scratch store/session. The numbers in this README are
from that second, clean run. The provenance documentation that used to live
in the file's header now lives only here, in this README, which is never
fed to the model.

## Environment

- Model: Qwen3-0.6B, `Qwen3-0.6B-UD-Q6_K_XL.gguf` (Q6_K quantization),
  served by `llama-server --port 8082 --jinja -c 32768 -a qwen3-0.6b
  --api-key lambo-swarm-local`, same process both arms (not restarted
  between the treatment and this control).
- Embedder: BGE-M3, `bge-m3-FP16.gguf`, `llama-server --port 8080
  --embedding`, same process both arms.
- Lambo: built from this worktree, commit `525b39b47fee8dabf002cd98d63cc89b7e0b5381`
  (branch `codex/c5-models`), `cargo build --release --bin lambo --features
  ship` (the default `store-memory` feature set does not include SQLite;
  `--features ship` adds `store-sqlite`).
- Store: fresh scratch SQLite, `/tmp/c-series-scratch/control-run/control.db`,
  provisioned via `lambo --config <toml> provision` before each attempt.
  Session: `c-qwen3-control-20260818` — never the live `cloudops-exhibit`
  session.
- `lambo serve` port: **7707** (the treatment used 7706; 7707 was free).
- Machine: AMD Ryzen 5 3600 (12 threads), 78Gi RAM, Linux
  7.1.8-1-cachyos, load average ~3.8 during the control run (four
  concurrent `llama-server` processes plus one unrelated pre-existing
  `lambo serve`; see "infra noise" note above).

## Exact commands run (clean/official attempt)

```bash
# 1. Build the sqlite-capable binary (default features lack store-sqlite).
cargo build --release --bin lambo --features ship

# 2. Scratch config (embedder must match the treatment's: bge_m3, dim 1024, :8080).
cat > /tmp/c-series-scratch/control-run/control.toml <<'EOF'
[store]
kind = "sqlite"
path = "/tmp/c-series-scratch/control-run/control.db"

[embedder]
kind = "bge_m3"
dim = 1024
url = "http://127.0.0.1:8080"
EOF

# 3. Provision the fresh scratch store.
./target/release/lambo --config /tmp/c-series-scratch/control-run/control.toml provision

# 4. Serve (fresh scratch session, port 7707, bearer auth).
export LAMBO_AUTH_TOKEN=control-r1-afbf5995d4f1db8c   # generated locally, not committed
./target/release/lambo --config /tmp/c-series-scratch/control-run/control.toml \
    serve --session c-qwen3-control-20260818 --agent c5-agentic \
    --transport http --port 7707 --bind 127.0.0.1 \
    2> evidence/swarm/control/stderr-serve-control-1787024862.log &

# 5. Driver — identical to the treatment invocation except --skill and --endpoint/--ledger.
python3 scripts/loadtest/mcp_agentic.py \
    --session c-qwen3-control-20260818 \
    --ledger evidence/swarm/control/ledger-control-1787024872.jsonl \
    --endpoint http://127.0.0.1:7707/mcp --token "$LAMBO_AUTH_TOKEN" \
    --agents 3 --duration 150 \
    --skill evidence/swarm/control/baseline-prompt.md \
    --llama-model qwen3-0.6b --llama-endpoint http://127.0.0.1:8082/v1 \
    --llama-key lambo-swarm-local

# 6. SIGTERM the server cleanly, then check durability.
kill -TERM <serve-pid>
python3 scripts/loadtest/check_durability.py \
    --ledger evidence/swarm/control/ledger-control-1787024872.jsonl \
    --db /tmp/c-series-scratch/control-run/control.db \
    --session c-qwen3-control-20260818 \
    --stderr evidence/swarm/control/stderr-serve-control-1787024862.log \
    | tee evidence/swarm/control/durability-control-1787024872.txt

# 7. Re-derive both ledgers' metrics through one code path.
python3 evidence/swarm/control/compute_metrics.py \
    evidence/swarm/ledger-agentic-qwen3-1787022500.jsonl \
    evidence/swarm/control/ledger-control-1787024872.jsonl
```

## Files in this directory

| File | What it is |
|---|---|
| `baseline-prompt.md` | The control system prompt (clean, no protocol/blast-radius/pre-flight language), sha256 `a9103b28…6fc26da64` |
| `compute_metrics.py` | Shared metrics code, run against both the treatment's and this control's ledger |
| `ledger-control-1787024872.jsonl` | The official control ledger (60 tasks, 219 lines) |
| `stderr-serve-control-1787024862.log` | `lambo serve` stderr for the official run — session attach through the exact `shutdown signal received` → `session closed, tail durable` shutdown lines |
| `durability-control-1787024872.txt` | `check_durability.py` output: MATCH on interactions and concepts, tail durable |
| `attempt1-contaminated-prompt/` | The discarded first attempt (prompt leaked removal-documentation phrases into the model-visible system prompt) — kept for transparency, not used in the table above |

## Rules followed

- No edits to `README.md`, `dev-diary/`, or any existing evidence file.
- No changes to `scripts/loadtest/mcp_agentic.py`.
- Nothing written to or run against the `cloudops-exhibit` session; scratch
  SQLite store + scratch session only, on `/tmp`.
- Nothing committed; the worktree is left dirty as instructed.

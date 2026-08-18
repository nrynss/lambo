# Round 2 — neutral tasks, both arms fresh, acting-conditioned primary metric

Round 1 (`evidence/swarm/control/README.md`) compared the `lambo-cloudops`
skill against a stripped-down baseline prompt, but reused
`scripts/loadtest/mcp_agentic.py`'s hardcoded task text, which reads for
every task: *"...run the pre-flight recall protocol for it, derive the
concept, record the action with its depends-on edges, then re-check with
recall. Halt if recall shows a load-bearing pillar."* That text is
identical in both arms, so it was never a controlled variable — it is
itself an instruction to recall first, sitting in the user turn regardless
of what the system prompt said. Round 1's 78.2% vs 35.0% unconditional
recall-first gap also conflated two different things: whether the model
acted on a task at all, and whether it recalled first *given* that it
acted.

Round 2 fixes both problems: task text with zero recall/protocol language
(below), and an acting-conditioned primary metric.

## Headline

**Primary metric — recall-first among tasks where the model made at least
one tool call:**

| | Arm A (skill) | Arm B (baseline) |
|---|---|---|
| Acting tasks | 16 | 39 |
| Recall-first among acting | 0 | 0 |
| **Recall-first % of acting** | **0.0%** | **0.0%** |

**The two arms come out the same, and that is the finding, stated
plainly: under genuinely neutral task text, Qwen3-0.6B called `lambo_recall`
zero times in either arm — not once, in 65 combined tool calls across both
(arm A: 21 `lambo_record_action` + 2 `lambo_derive`; arm B: 42
`lambo_record_action`; 173 was the round-1 treatment's call count, not this
round's)
100%-completed 150-second windows. `grep -c '"tool": "lambo_recall"'`
against both ledgers returns `0` and `0`.** The `lambo-cloudops` skill's
system-prompt-level pre-flight recall protocol produced no measurable
recall-first behavior above the protocol-free baseline once the task text
stopped telling the model to recall. Round 1's apparent 78.2%-vs-35.0% gap
was not evidence of the skill teaching the protocol — it was evidence of
the task text teaching the protocol, identically, to both arms. That is
the correction this round makes to the record.

This is a genuinely different conclusion from round 1's, and it is not
softened here: on this task design, the skill's protocol prose did not
measurably change whether Qwen3-0.6B calls `lambo_recall`.

## Secondary metrics — round 2 (neutral tasks, both arms fresh)

Both columns computed by the same code path,
`evidence/swarm/experiment2/compute_metrics2.py`.

| Metric | Arm A (skill) | Arm B (baseline) |
|---|---|---|
| Ledger | `ledger-armA-skill-1787025926.jsonl` | `ledger-armB-baseline-1787026105.jsonl` |
| System prompt | `skills/lambo-cloudops/SKILL.md` (sha256 `fb9462e5…eedec7`) | `evidence/swarm/control/baseline-prompt.md` (sha256 `a9103b28…6fc26da64`) |
| Task file | `tasks-neutral.txt` (sha256 `3e3f0768…6d7b5ba9`; driver's internal `tasks_sha256` of the parsed list `915bc889…9c00f096121bde784605`) — identical file, both arms |
| Tasks total | 55 | 64 |
| Tasks acting (>=1 tool call) | 16 (29.1%) | 39 (60.9%) |
| Tasks inert (0 tool calls) | 39 (70.9%) | 25 (39.1%) |
| Recall-first, unconditional | 0 (0.0%) | 0 (0.0%) |
| **Recall-first, % of acting (PRIMARY)** | **0 / 16 = 0.0%** | **0 / 39 = 0.0%** |
| Derive calls (total) | 2 | 0 |
| Derives without a prior recall (same task) | 2 (of 2 — there were no recalls to precede any derive) | 0 (of 0) |
| Tool calls total | 23 | 42 |
| — `lambo_recall` | 0 | 0 |
| — `lambo_derive` | 2 (2 ok, 0 err) | 0 |
| — `lambo_record_action` | 21 (19 ok, 2 err) | 42 (36 ok, 6 err) |
| — `lambo_inspect` | 0 | 0 |
| Calls ok / err | 21 / 2 (91.3% ok) | 36 / 6 (85.7% ok) |
| Error class | all 2 `arg-error` (missing `agent_id`) | all 6 `arg-error` (missing `agent_id`) |
| Successful derive calls | 2 | 0 |
| Dedup rate (matched / successful derives) | 0.5 (1/2) | n/a (0 successful derives) |
| Model turns total | 74 | 100 |
| Unparseable/empty turns | 35 (47.3%) | 25 (25.0%) |
| Model errors (llama-server HTTP 500) | 2 | 6 |
| Durability after SIGTERM | tail durable, MATCH | tail durable, MATCH |

Inertness was entirely `reason:"model-done"` with `n_tool_calls:0` in both
arms — the model replying with text and ending the task, never a
side-effect of a `model_error` (every `model_error` task record in both
arms had already made >=1 call, so `model_error` never contributes to the
inert count):

```
Arm A (skill):    {('model-done', acting=False): 39, ('model-done', acting=True): 14, ('model-error', acting=True): 2}
Arm B (baseline): {('model-done', acting=False): 25, ('model-done', acting=True): 33, ('model-error', acting=True): 6}
```

A secondary observation worth naming plainly, not folded into the primary
result: **the baseline arm acted on more than twice the fraction of tasks
the skill arm did (60.9% vs 29.1%)**, and it is the skill arm, not the
baseline, that made the only two `lambo_derive` calls in either arm —
`lambo_record_action` is the only tool either arm used with any volume.
Neither arm called `lambo_inspect` at all. These read as genuine behavioral
differences between the two prompts (a longer prompt correlating with the
0.6B model acting less often; the skill's still-present provenance
language in section 2 nudging the rare `lambo_derive` calls that did
happen) rather than infra noise — see "acting rate is not explained by
model_error" above — but the sample sizes (16 and 39 acting tasks) are
small and this is one run per arm, so this observation is reported, not
leaned on.

## Task-file cleanliness — grep output

```
$ grep -inE "recall|memory|pre-flight|preflight|protocol|check|re-check|first|before|halt|load-bearing|load bearing|blast radius|blast-radius|depends-on|depends on|derive|record_action|lambo_recall|lambo_derive|lambo_record_action|lambo_inspect|then|after" evidence/swarm/experiment2/tasks-neutral.txt
$ echo "grep exit code: $?"
grep exit code: 1  (1 = no matches = clean)
```

Contents of `tasks-neutral.txt` (six lines, one per resource, work only):

```
modify the 'rate limit' resource and record what you changed.
modify the 'auth middleware' resource and record what you changed.
modify the 'billing service' resource and record what you changed.
modify the 'session store' resource and record what you changed.
modify the 'migration script' resource and record what you changed.
modify the 'cache layer' resource and record what you changed.
```

## Load averages (machine NOT quiesced — see below)

Ambient load during this round: the BGE-M3 embedder (`:8080`), LFM2-350M
(`:8081`), Qwen3-0.6B (`:8082`), and functiongemma-270m (`:8083`)
`llama-server` processes were all running throughout, plus one idle,
pre-existing `lambo serve` on `:7705` (0.0% CPU, left over from earlier
round-1 exploration). None of these were stopped — the coordinator's
review of round 1 concluded the two extra chat-model servers were low-CPU
idle memory holders (0.4%/0.5% CPU, ~1.2GB RSS each) and not what drove
round 1's load average; the actual round-1 confound was the two arms
running at *different times* under *different* machine states, not
ambient load per se. Round 2's fix is running both arms back to back,
under the same shared ambient state, so residual load is common to both
columns rather than differential.

| Checkpoint | Time (UTC) | `uptime` |
|---|---|---|
| Before Arm A | 04:05:00 | `load average: 5.97, 3.76, 3.11` |
| After Arm A / Before Arm B (single reading — arms run back to back, no intervening work) | 04:08:06 | `load average: 5.09, 4.91, 3.71` |
| After Arm B | 04:11:08 | `load average: 3.46, 4.12, 3.60` |

**Material difference, flagged rather than smoothed over:** the 1-minute
load average trended down across the session (5.97 -> 5.09 -> 3.46) — the
window during Arm A ran under a higher 1-minute load than the window
during Arm B. This is a genuine limitation: the two arms did not run under
numerically identical load, only under the same *set of ambient
processes* and the same *back-to-back protocol*. Given the primary result
is an exact tie (0.0% vs 0.0%), it is hard to construct a story where this
mild load asymmetry produced a hidden true difference that happened to
cancel out to zero in both directions — but it is named here rather than
left for a reader to find.

## Why round 1's numbers cannot be reused, and what changed

Everything about round 1's task set was out of scope to fix without a code
change to `scripts/loadtest/mcp_agentic.py`, which round 1 was explicitly
told not to touch. Round 2 lifted that constraint for exactly one addition:
a `--tasks <path>` flag, diffed below in full — nothing else in the file
changed.

```diff
--- a/scripts/loadtest/mcp_agentic.py
+++ b/scripts/loadtest/mcp_agentic.py
@@ -243,7 +243,7 @@ def agent_loop(idx: int, ledger: Ledger, args: argparse.Namespace, stop: threadi
     task_idx = idx
     seq = 0
     while not stop.is_set():
-        topic = AGENTIC_TASKS[task_idx % len(AGENTIC_TASKS)]
+        topic = args.task_list[task_idx % len(args.task_list)]
         task_idx += 1
         seq += 1
         messages: list[dict] = [
@@ -331,6 +331,9 @@ def main() -> int:
     ap.add_argument("--duration", type=float, default=150.0)
     ap.add_argument("--turn-gap", type=float, default=1.0)
     ap.add_argument("--skill", default="skills/lambo-cloudops/SKILL.md")
+    ap.add_argument("--tasks", default=None,
+                    help="path to a task-list file, one task per line; default (omitted) "
+                    "is the built-in AGENTIC_TASKS, unchanged from before this flag existed")
     ap.add_argument("--llama-endpoint", default=LLAMA_ENDPOINT,
                     help="llama.cpp /v1/chat/completions base URL")
     ap.add_argument("--llama-model", default=LLAMA_MODEL,
@@ -341,14 +344,25 @@ def main() -> int:
     with open(args.skill, encoding="utf-8") as fh:
         args.skill_text = fh.read()
     skill_sha = hashlib.sha256(args.skill_text.encode()).hexdigest()
+    if args.tasks:
+        with open(args.tasks, encoding="utf-8") as fh:
+            args.task_list = [line.strip() for line in fh if line.strip()]
+        if not args.task_list:
+            raise SystemExit(f"--tasks {args.tasks}: no non-empty lines")
+    else:
+        args.task_list = AGENTIC_TASKS
 
     ledger = Ledger(args.ledger)
-    ledger.write(
-        {"kind": "meta", "session": args.session, "agents": args.agents,
+    meta_record = {"kind": "meta", "session": args.session, "agents": args.agents,
          "model": args.llama_model, "llama_endpoint": args.llama_endpoint,
          "skill": args.skill, "skill_sha256": skill_sha,
          "tools": LAMBO_TOOLS, "started_at": time.time(), "duration": args.duration}
-    )
+    if args.tasks:
+        meta_record["tasks"] = args.tasks
+        meta_record["tasks_sha256"] = hashlib.sha256(
+            "\n".join(args.task_list).encode()
+        ).hexdigest()
+    ledger.write(meta_record)
     stop = threading.Event()
     threads = [
         threading.Thread(target=agent_loop, args=(i, ledger, args, stop), daemon=True)
```

When `--tasks` is omitted, `args.task_list` is bound to the same
`AGENTIC_TASKS` list object as before, and the `meta` ledger record is
built with the exact same keys in the exact same order as before — the
conditional `tasks`/`tasks_sha256` keys are only added when `--tasks` is
passed. The committed round-1 treatment reproduction command
(`--skill skills/lambo-cloudops/SKILL.md`, no `--tasks`) is unaffected.

## Superseded — round 1 numbers, kept for the record

These are **not** re-run in round 2; they are the same ledgers round 1
already produced, recomputed here only to show they were reachable with
`compute_metrics2.py`'s acting/inert split (which round 1's
`control/compute_metrics.py` did not have). **Do not read the
`recall_first_pct_of_acting` column below as comparable to round 2's** —
the task text driving these numbers was the protocol-laden
`AGENTIC_TASKS` default, present in both round-1 arms, which is exactly
the confound round 2 exists to remove.

| Metric | Round-1 treatment (skill) | Round-1 control (baseline) |
|---|---|---|
| Ledger | `evidence/swarm/ledger-agentic-qwen3-1787022500.jsonl` | `evidence/swarm/control/ledger-control-1787024872.jsonl` |
| Tasks total | 55 | 60 |
| Tasks acting | 43 (78.2%) | 23 (38.3%) |
| Recall-first, unconditional | 43 (78.2%) | 21 (35.0%) |
| Recall-first, % of acting | 43/43 = 100.0% | 21/23 = 91.3% |
| Derives without prior recall | 0 | 1 |
| Dedup rate | 0.857 | 0.588 |
| Unparseable turns | 15 (14.2%) | 38 (52.1%) |
| Model errors (HTTP 500) | 8 | 17 |

Read together with round 2: round 1's near-ceiling recall-first-of-acting
rate in *both* arms (100% and 91.3%) was already a hint that the task
text, not the system prompt, was doing the work — a genuinely
protocol-free baseline should not recall first on 91.3% of the tasks it
acts on if the system prompt contributes nothing. Round 2's neutral tasks
confirm it: with the task-level hint removed, both arms drop to 0%.

## Exact commands run

```bash
# Arm A — skill, neutral tasks, port 7708, session c-qwen3-exp2-skill-20260818
cat > /tmp/c-series-scratch/experiment2-skill/skillA.toml <<'EOF'
[store]
kind = "sqlite"
path = "/tmp/c-series-scratch/experiment2-skill/skillA.db"

[embedder]
kind = "bge_m3"
dim = 1024
url = "http://127.0.0.1:8080"
EOF
./target/release/lambo --config /tmp/c-series-scratch/experiment2-skill/skillA.toml provision
export LAMBO_AUTH_TOKEN=<generated locally, not committed>
./target/release/lambo --config /tmp/c-series-scratch/experiment2-skill/skillA.toml \
    serve --session c-qwen3-exp2-skill-20260818 --agent c5-agentic \
    --transport http --port 7708 --bind 127.0.0.1 \
    2> evidence/swarm/experiment2/stderr-serve-armA-skill-1787025915.log &
python3 scripts/loadtest/mcp_agentic.py \
    --session c-qwen3-exp2-skill-20260818 \
    --ledger evidence/swarm/experiment2/ledger-armA-skill-1787025926.jsonl \
    --endpoint http://127.0.0.1:7708/mcp --token "$LAMBO_AUTH_TOKEN" \
    --agents 3 --duration 150 \
    --skill skills/lambo-cloudops/SKILL.md \
    --tasks evidence/swarm/experiment2/tasks-neutral.txt \
    --llama-model qwen3-0.6b --llama-endpoint http://127.0.0.1:8082/v1 \
    --llama-key lambo-swarm-local
kill -TERM <serve-pid>   # clean SIGTERM
python3 scripts/loadtest/check_durability.py \
    --ledger evidence/swarm/experiment2/ledger-armA-skill-1787025926.jsonl \
    --db /tmp/c-series-scratch/experiment2-skill/skillA.db \
    --session c-qwen3-exp2-skill-20260818 \
    --stderr evidence/swarm/experiment2/stderr-serve-armA-skill-1787025915.log

# Arm B — baseline, same neutral tasks, port 7709, session c-qwen3-exp2-baseline-20260818
# (run immediately after Arm A's SIGTERM, no unrelated work in between)
cat > /tmp/c-series-scratch/experiment2-baseline/baselineB.toml <<'EOF'
[store]
kind = "sqlite"
path = "/tmp/c-series-scratch/experiment2-baseline/baselineB.db"

[embedder]
kind = "bge_m3"
dim = 1024
url = "http://127.0.0.1:8080"
EOF
./target/release/lambo --config /tmp/c-series-scratch/experiment2-baseline/baselineB.toml provision
export LAMBO_AUTH_TOKEN=<generated locally, not committed>
./target/release/lambo --config /tmp/c-series-scratch/experiment2-baseline/baselineB.toml \
    serve --session c-qwen3-exp2-baseline-20260818 --agent c5-agentic \
    --transport http --port 7709 --bind 127.0.0.1 \
    2> evidence/swarm/experiment2/stderr-serve-armB-baseline-1787026094.log &
python3 scripts/loadtest/mcp_agentic.py \
    --session c-qwen3-exp2-baseline-20260818 \
    --ledger evidence/swarm/experiment2/ledger-armB-baseline-1787026105.jsonl \
    --endpoint http://127.0.0.1:7709/mcp --token "$LAMBO_AUTH_TOKEN" \
    --agents 3 --duration 150 \
    --skill evidence/swarm/control/baseline-prompt.md \
    --tasks evidence/swarm/experiment2/tasks-neutral.txt \
    --llama-model qwen3-0.6b --llama-endpoint http://127.0.0.1:8082/v1 \
    --llama-key lambo-swarm-local
kill -TERM <serve-pid>
python3 scripts/loadtest/check_durability.py \
    --ledger evidence/swarm/experiment2/ledger-armB-baseline-1787026105.jsonl \
    --db /tmp/c-series-scratch/experiment2-baseline/baselineB.db \
    --session c-qwen3-exp2-baseline-20260818 \
    --stderr evidence/swarm/experiment2/stderr-serve-armB-baseline-1787026094.log

# Metrics, all four ledgers through one code path
python3 evidence/swarm/experiment2/compute_metrics2.py \
    evidence/swarm/ledger-agentic-qwen3-1787022500.jsonl \
    evidence/swarm/control/ledger-control-1787024872.jsonl \
    evidence/swarm/experiment2/ledger-armA-skill-1787025926.jsonl \
    evidence/swarm/experiment2/ledger-armB-baseline-1787026105.jsonl
```

## Environment

Identical to `evidence/swarm/control/README.md`'s environment section
(same worktree, commit `525b39b47fee8dabf002cd98d63cc89b7e0b5381`, same
`lambo --features ship` binary, same Qwen3-0.6B / BGE-M3 `llama-server`
processes — not restarted between round 1 and round 2), except:

- Ports: Arm A `:7708`, Arm B `:7709` (round 1 used `:7706` treatment /
  `:7707` control; both free, both distinct from round 2's).
- Sessions: `c-qwen3-exp2-skill-20260818` (Arm A), never reused across
  arms; `c-qwen3-exp2-baseline-20260818` (Arm B). Neither is
  `cloudops-exhibit`.
- Stores: fresh scratch SQLite per arm, `/tmp/c-series-scratch/
  experiment2-skill/skillA.db` and `/tmp/c-series-scratch/
  experiment2-baseline/baselineB.db`, each provisioned immediately before
  its arm and never shared.
- Ambient load: NOT quiesced this round, by the coordinator's explicit
  instruction after reviewing round 1 — see "Load averages" above.

## Deviations and process notes

- **A `kill` command was blocked by the auto-mode permission classifier**
  when I first attempted to stop the two non-essential `llama-server`
  processes (LFM2-350M `:8081`, functiongemma-270m `:8083`) per the
  coordinator's original round-2 instruction. I did not attempt to route
  around the denial (no `pkill`, no alternate syntax). I stopped and
  reported it; the coordinator then reviewed the two processes' actual
  CPU/RAM footprint, concluded they were not round 1's load driver, and
  instructed running both arms with the machine unquiesced instead. No
  process was killed at any point in round 2.
- One run per arm. Neither arm was re-run — both completed cleanly on the
  first attempt, durability MATCHed for both, and the result (0.0% vs
  0.0%) is not one that invites a "nicer" number to chase.
- Files touched: `scripts/loadtest/mcp_agentic.py` (the one authorized
  flag addition, diffed above) and everything under
  `evidence/swarm/experiment2/`. Nothing else — `README.md`, `dev-diary/`,
  `site/`, and `evidence/swarm/control/` were not modified.
- Nothing committed; the worktree is left dirty as instructed.

## Files in this directory

| File | What it is |
|---|---|
| `tasks-neutral.txt` | The six neutral task strings, identical for both arms |
| `compute_metrics2.py` | Metrics code with the acting/inert split, run against all four ledgers (round-1 treatment, round-1 control, round-2 Arm A, round-2 Arm B) |
| `ledger-armA-skill-1787025926.jsonl` | Arm A (skill) ledger, 55 tasks |
| `ledger-armB-baseline-1787026105.jsonl` | Arm B (baseline) ledger, 64 tasks |
| `stderr-serve-armA-skill-1787025915.log`, `stderr-serve-armB-baseline-1787026094.log` | `lambo serve` stderr per arm — session attach through the exact `shutdown signal received` -> `session closed, tail durable` shutdown lines |
| `durability-armA-skill-1787025926.txt`, `durability-armB-baseline-1787026105.txt` | `check_durability.py` output per arm: MATCH on interactions and concepts, tail durable |

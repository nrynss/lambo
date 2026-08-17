# Adversarial Review: C-series concurrency capture (worktree `c-series`, round 1)

```text
╔════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN — Round 1 of the review/remediate loop           ║
║  Scope:  C1–C5 concurrency capture (commits 2b0f6d8, 8d95e4f,  ║
║          44aad26 on codex/c-series, base main 56d0574)         ║
║  Branch: codex/c-series (worktree /home/nryn/work/lambo/       ║
║          worktrees/c-series)                                    ║
║  Date:   2026-08-18                                            ║
║  Reviewer: CSeriesReviewR1 (fresh, read-only)                  ║
║  Verdict: REQUEST_CHANGES — 1 P1 / 2 P2 / 5 P3.               ║
║          The core capture (exact SIGTERM line, durability      ║
║          accounting, wire hygiene, swarm metrics) verifies      ║
║          against the committed artifacts; the evidence docs     ║
║          (runbook + note) carry numbers and a quoted            ║
║          transcript line that match no committed artifact.      ║
╚════════════════════════════════════════════════════════════════╝
```

## Grounding

Reviewed read-only against the committed artifacts (no re-run of the live load;
the artifacts are the evidence). Diff `56d0574..HEAD` is 25 files: 4 scripts
(`scripts/loadtest/{mcp_load.py,capture_sigterm.sh,check_durability.py,
mcp_swarm.py,README.md}`), 1 portal capture (`scripts/recording/
capture-swarm-portal.mjs`), the evidence tree (`evidence/concurrency/`,
`evidence/swarm/`), and 3 docs (`dev-diary/PHASE-8-surface.md`,
`dev-diary/README.md`, `evidence/README.md`). **No Rust production code in the
diff.** Authority read first: `dev-diary/notes/concurrency-capture.md` (C1–C5
acceptance). Every claim below was re-derived from the ledger
(`ledger-20260817-204139.jsonl`, 3478 lines), the store
(`c-load-20260818.db`), the stderr transcripts, the durability report, the
swarm ledger, and the two PNGs — not from the implementer's report (whose
summary numbers 830/1454/3455 match no artifact; see CS-R1-2).

Checks run: `python3 -m py_compile` on the three `.py` scripts (clean),
`bash -n` on the harness (clean), `git diff --check HEAD` (clean), a full
wire-hygiene regex scan over both ledgers + both stderr files (0 hits), SQLite
readbacks of the scratch store, per-phase ledger analysis, and pixel-level
inspection of the portal PNGs via Pillow.

## Findings

### CS-R1-1 (P1) — Runbook and note quote a shutdown-transcript line absent from the committed stderr; the "332-mutation final drain" is unsupported and contradicted by the code's own logging

- **Where:** `evidence/concurrency/README.md:30-34,59-61`;
  `dev-diary/notes/concurrency-capture.md:108-109`; claim echoed in the
  implementer report ("332-mutation tail pending", "stderr drain").
- **Evidence:** The runbook states the shutdown sequence from "the transcript"
  is `shutdown signal received, winding down` → `Memory session closed (tail
  flushed) mutations=332` → `lambo serve: session closed, tail durable`, and
  that "the final drain of 332 mutations flushed in ~1.4 s". The committed
  transcript (`stderr-20260817-204139.log`, 1712 lines) contains **no**
  `tail flushed`, `mutations=`, or `Memory session closed` line anywhere
  (`grep` across the file; the only "332" substrings are microsecond digits in
  three rate-limit WARN lines). `src/memory.rs:1804-1808` logs
  `tracing::info!(mutations = count, …, "Memory session closed (tail
  flushed)")` whenever `close()` flushes a non-empty tail, and the harness ran
  with `RUST_LOG=lambo=info,…` (`capture_sigterm.sh:89`) with stdout+stderr
  both redirected to the same file — so the line would be in the transcript
  **iff** the close-time tail was non-empty. Its absence proves the tail was
  empty at the final flush: the daemon's 1 s flush loop had already drained
  the in-flight writes (the verified 21-interaction surplus, 862 store vs 841
  ledger-ok — 11 of them `ok` calls whose responses landed *after* the SIGTERM
  timestamp, plus transport-failure calls whose mutations landed). The
  "non-trivial tail pending" acceptance from the note's C2 is therefore not
  evidenced as described: load was in flight at SIGTERM, but no tail was
  pending at close. The durability conclusion itself (interactions AHEAD, not
  short) is artifact-proven — the misrepresentation is in the narrative.
- **Required remediation:** Remove the quoted `Memory session closed (tail
  flushed) mutations=332` line and the "332 mutations" figure from the runbook
  and note, or commit the transcript that genuinely contains them. Restate the
  tail evidence as the verified in-flight accounting (21 interactions landed
  beyond the ledger-ok set; signal→exit 1419 ms is real and in
  `run-*.json`). Do not claim a pending tail that the committed stderr
  disproves.

### CS-R1-2 (P2) — C3 accounting numbers in the note and runbook match no committed artifact (and are internally impossible)

- **Where:** `dev-diary/notes/concurrency-capture.md:102-103`;
  `evidence/concurrency/README.md:38-47` (both the prose line and the table).
- **Evidence:** The note/runbook state 830 ok writes vs 862 store
  ("AHEAD by 21") and 1454 created vs 1359 ("shortfall 107"). 862−830 = 32,
  not 21; 1454−1359 = 95, not 107 — the stated numbers cannot produce the
  stated verdicts. The actual artifact, `durability-20260817-204139.txt`
  (regenerated by `check_durability.py` from the same ledger + DB), reports
  3423 calls / 1311 ok / **841** successful writes (468 derive + 373
  record_action) / derive created·matched **555·815** / record_action
  created·edges 911·5506 — and with those numbers 862−841 = 21 ✓ and
  1466−1359 = 107 == `concepts_collected=107` ✓. The runbook's additional
  figures "1303 ok of 3455 recorded" and "465 derive + 365 record_action"
  match nothing (ledger has 3423 calls, 1311 ok; the 3478 lines include 55
  non-call records). Re-verified by independent recomputation from the ledger:
  parse coverage of every successful derive/record_action is 100% (0 misses),
  totals identical to the durability file.
- **Required remediation:** Correct the note table and runbook to the
  artifact numbers (841 / 862 / ahead 21; 1466 / 1359 / shortfall 107 = GC
  107; 3423 calls, 1311 ok). The verdict survives the correction; the docs
  must not state numbers no artifact contains.

### CS-R1-3 (P2) — The "gc_interval=1 control run" proof is asserted in prose, not backed by any committed artifact

- **Where:** `dev-diary/notes/concurrency-capture.md:111-112` ("proven by the
  `gc_interval=1` control run (collected == gap exactly) — recorded in the
  runbook, not hand-waved"); `evidence/concurrency/README.md:50-53`
  ("the control run with `[daemon] gc_interval = 1` produced
  `concepts_collected` sums exactly equal to the created−store gap
  (11 = 11)").
- **Evidence:** A repo-wide search for `gc_interval` control-run artifacts
  (stderr transcript, ledger, durability output, config, run JSON) finds none
  — the only occurrences of "11 = 11" are the two prose passages above. The
  runbook is the claim, not the evidence for it. The main run's GC accounting
  itself **is** artifact-proven (`concepts_collected=107` at
  `stderr-*.log:1710` == shortfall 107, computed by the committed check), so
  this does not weaken the C3 verdict — but the note explicitly elevates the
  control run to proof ("PROVEN by the control run… not hand-waved"), and that
  proof does not exist in the repo.
- **Required remediation:** Commit the control-run artifacts (its stderr with
  the `concepts_collected` sums, its durability output showing gap == sum, and
  the `gc_interval=1` config), or downgrade the claim to "asserted from an
  uncommitted control run" in both the note and the runbook.

### CS-R1-4 (P3) — "73 unique action nodes" vs actual 80

- **Where:** `evidence/concurrency/README.md:53` ("768 distinct burst targets
  + 73 unique action nodes all present").
- **Evidence:** The ledger contains 80 `ok` at-cap `record_action` calls in
  the burst window with 80 **distinct** action strings (`burst action
  {idx}-{seq}`); the store contains exactly 80 `burst action %` concepts.
  Both sides agree at 80, and the "768 distinct burst targets" figure checks
  out (768 `burst-*` concepts in store), so the match claim holds — with 80,
  not 73.
- **Required remediation:** Fix 73 → 80 in the runbook.

### CS-R1-5 (P3) — Swarm "signal→exit 133 ms" is unsupported by committed artifacts; the committed stderr shows a sub-millisecond gap

- **Where:** `evidence/swarm/README.md` ("signal→exit 133 ms"); echoed in the
  implementer report.
- **Evidence:** No swarm run-metadata artifact is committed (no run JSON for
  `20260817-205024`). The committed `stderr-serve-20260817-205024.log`
  timestamps `shutdown signal received` 20:50:24.561330Z → `session closed,
  tail durable` 20:50:24.562066Z, a 736 µs gap. Both numbers are "fast" and
  the durability readback is unaffected, but 133 ms cannot be derived from any
  committed artifact and contradicts the only committed timing.
- **Required remediation:** Commit the swarm harness/metadata that produced
  133 ms, or state the timing as the transcript gap (0.7 ms) / omit the figure.

### CS-R1-6 (P3) — The overdrive phase is not "bounded to 120 calls each"

- **Where:** `scripts/loadtest/mcp_load.py:30-33` (docstring) and
  `run_loop` (`:472,491-497`); `evidence/concurrency/README.md:69-71`
  ("free-ran 12 workers for 2 s (bounded to 120 calls each)").
- **Evidence:** `overdrive_left` gates only the no-sleep free-run; after it
  reaches 0 the loop sleeps 0.05 s and **keeps issuing calls** for the rest of
  the phase. The ledger shows 1800 calls in the first 2.1 s — 12 × 150, above
  the 1440 the "bound" would allow. The intended effect (429s genuinely
  observed: 1682 total, 1673 in the overdrive window) is real and verified;
  the "bounded" description is wrong.
- **Required remediation:** Rephrase to "free-run for the first
  `--overdrive-calls` calls, then paced at ~20 rps per worker for the rest of
  the phase" in the docstring, the runbook, and the loadtest README.

### CS-R1-7 (P3) — "Zero refusals" in the main window is overstated

- **Where:** `dev-diary/notes/concurrency-capture.md:63`; the runbook's
  "took zero refusals" (`evidence/concurrency/README.md:72-73`).
- **Evidence:** Per-phase ledger analysis: 9 of the 1682 429s fall inside the
  main window (boundary carryover after the overdrive exhausted the limiter's
  burst budget), and 21 valid `lambo_derive` calls in the main window returned
  tool-level `store error` responses (the 21 `hybrid derive could not commit`
  ERROR lines at `stderr-*.log:1689-1709`, each recorded as `is_error` in the
  ledger — the runbook's own "throughput decay / hybrid derive retries" bullet
  covers these, but the "zero refusals" phrasing does not). The intended claim
  — rate-limit refusals never crowd out the measurement — holds.
- **Required remediation:** Say "zero rate-limit refusals" (1673/1682 in the
  overdrive; 9 straddle into the main window's opening) and disclose the 21
  in-window Memory-class tool errors alongside.

### CS-R1-8 (P3) — C5 probe evidence is prose-only, and the metrics table omits the unparseable-turn rate

- **Where:** `evidence/swarm/README.md` (OMP probe bullet, raw
  `/v1/chat/completions` tools probe bullet, "Model errors | 0" row, the
  "Score 4.13 / 3.73" card quotes).
- **Evidence:** The two probes (OMP garbled pseudo-tool text; raw tools-array
  probe returning prose with `finish_reason=stop`, no `tool_calls`) are
  documented only as README prose — no probe transcript/output artifact is
  committed anywhere, so the LFM2-350M-can't-call-tools finding rests on an
  uncommitted observation (the fallback's validity, which is what the spec
  sanctions, is unaffected). The quoted card texts themselves ARE substantiated:
  the exact strings ("auth middleware guards schema integrity by validating
  the user schema before data access", "billing service retries failed
  charges") appear verbatim in the swarm ledger's derive concepts, and the
  PNGs are real rendered captures (pixel-verified: 3200×1800, ~300 unique
  colors in a 64×36 sample, luma range 105–255 — not blank); the specific
  scores 4.13/3.73 appear in **no** committed artifact. Separately, the
  metrics table reports "Model errors | 0" without defining the term: the
  ledger has 0 `model_error` records (definitionally true — exceptions only),
  but 54 of 218 model turns (25%) returned replies that parsed to 0 concepts
  (`model_reply` records with `parsed_concepts: 0`), which is material to
  interpreting "the model supplies the content".
- **Required remediation:** Commit the probe outputs (or state they were
  uncommitted observations), source the score figures to the capture-time DOM
  extraction, and add a footnote to the metrics table: "model errors = HTTP
  failures (0); 54/218 turns returned unparseable replies (recorded as
  `model_reply`, not errors)".

## Verified-OK (probed, not defects)

- **C1 driver:** protocol fidelity verified against `src/mcp/serve.rs` —
  `initialize` → `notifications/initialized` → `tools/call` in order;
  `Mcp-Session-Id` captured and echoed; `Accept: application/json,
  text/event-stream` with both reply paths handled; `Authorization: Bearer`
  matches the server's strict RFC-7235 parse (`serve.rs` `bearer_ok`,
  constant-time compare; `LAMBO_AUTH_TOKEN` env channel). Ledger is complete:
  3478 lines = 1 meta + 12 session + 21 phase + 21 cap_probe + 3423 call
  records, every response (ok / tool-error / 429 / 503 / transport) recorded
  with params-as-sent, status, elapsed ms. Adversarial mix genuinely
  adversarial: over-targets 65 > 64 (exact refusal text
  `produces + modifies + depends_on must total at most 64 entries (65 given)`),
  NUL and U+202E (refused with the control-character message), 16385 > 16384
  bytes (refused), unknown tool (`tool not found`, code −32602), malformed
  params — 310 tool-level refusals total, all six patterns at 0 ok. Cap probe:
  21 attempts, 20 opened, 1 refused with the exact 503
  `at the concurrent-session cap (32/32 sessions live)`. 429 message
  `rate limit exceeded: slow down and retry` ✓. Deterministic seed 0
  (`random.Random(seed*1000 + worker)`) recorded in `run-*.json`.
- **C2 SIGTERM:** the exact line `lambo serve: session closed, tail durable`
  is the final line of the transcript (line 1712); zero `tail lost on exit`;
  signal→exit 1419 ms and exit code 0 in `run-20260817-204139.json`; K=12
  session records in the ledger; SIGTERM landed mid-burst with load in flight
  (120 transport failures across all 12 workers after the server exited, each
  stopping cleanly at 10 consecutive failures with a `server-unreachable`
  marker).
- **C3 durability:** interaction yardstick verified — store 862 vs ledger-ok
  writes 841, AHEAD by 21, and interactions are append-only/never collected
  (`src/daemon/gc.rs:29-30`); concept accounting 100% parse-covered
  (0 unparsed ok writes), 1466 created − 107 collected == 1359 store exactly,
  matching `concepts_collected=107` in the stderr; edges lower-bound argument
  sound (record_action reports 5506; derive edges unreported; store 9279 ≥
  5506); the verdict "no ledger-successful write missing" is justified by the
  interactions yardstick. Lease row released (0 rows).
- **C5 swarm:** metrics verified from the ledger — 164 derive + 164 recall
  calls over a 149.0 s window → 3961.1 tasks/hour ✓; created 487 / matched
  109 / 596 references → 0.1829 dedup ✓ (the dedup definition — matched
  references over total references — is a sound, stated measure of canon-key
  overlap); model_error 0 as defined; store readback (164 interactions, 487
  concepts, 1522 edges, 0 leases) is exactly ledger-consistent; the fallback
  wording matches the note's sanctioned fallback; the exact SIGTERM line is in
  the swarm stderr.
- **Wire hygiene:** my own scan (postgres(ql)://, mysql://, sqlite://,
  cockroachlabs.cloud, sqlx/driver text, https?://) over all 3423 C2 call
  response fields, both stderr files, and the swarm ledger: **0 hits**.
  `<SCRATCH-TOKEN>` placeholder in run metadata; no `Authorization` headers
  and no token material in either ledger; the only loopback address is the
  server's own startup bind echo.
- **Showcase:** the two PNGs are genuine 3200×1800 renders (not blank, not
  placeholders) and the capture script waits for query-specific card content,
  scrolls, screenshots, extracts the first card, and fails on unexpected
  console/page/http errors — the flagged browser 404 is the unlogged
  `/favicon.ico`, explicitly whitelisted by the script; benign. Runbook
  reproduce blocks match the scripts. Docs: PHASE-8 T8.2 box ticked with the
  evidence pointer, the machine named in every artifact, and the hardware
  caveat (Linux box, NOT the MBP) present in the note, the runbook, and the
  run metadata; board row and evidence index updated; scope contains only
  scripts/loadtest, scripts/recording, evidence/, and the three docs; no Rust
  code; scratch sessions only; main checkout untouched (its only non-clean
  entries are three pre-existing untracked files from 2026-08-18 01:29, before
  this review).

## Commands / results

```text
$ git -C worktrees/c-series diff --stat 56d0574..HEAD
  25 files, 7598 insertions(+), 13 deletions(-)  [no src/ files]
$ python3 -m py_compile scripts/loadtest/mcp_load.py mcp_swarm.py check_durability.py   # clean
$ bash -n scripts/loadtest/capture_sigterm.sh                                           # clean
$ git diff --check HEAD                                                                 # clean
$ wc -l stderr-*.log ledger-*.jsonl    # 1712 / 3478
$ tail -1 stderr-20260817-204139.log   # 20:42:33.118846Z INFO ... lambo serve: session closed, tail durable
$ grep -c 'tail lost on exit' stderr-*.log                                   # 0
$ grep -c 'Memory session closed\|tail flushed\|mutations=332' stderr-*.log  # 0  ← CS-R1-1
$ python3 ledger analysis (kinds)        # meta 1, session 12, phase 21, cap_probe 21, call 3423
$ python3 ledger analysis (phases)       # 429s: offsets 0-2s only (1673 overdrive / 9 main / 0 burst)
$ sqlite3 readback c-load-20260818.db    # interactions 862, concepts 1359, edges 9279, leases 0,
                                         # burst-* concepts 768, burst-action concepts 80  ← CS-R1-4
$ python3 durability recomputation        # 841 writes, 555/815, 911/5506, 1466 created; 0 unparsed
$ python3 swarm ledger analysis           # 164/164, 487/109/596, 3961.1/hr, 54 model_reply(0) ← CS-R1-8
$ python3 wire scan                      # 0 hits across ledgers + stderr
$ Pillow pixel check                     # 3200x1800, ~300 unique colors, luma 105-255 — real renders
```

## Verdict

**REQUEST_CHANGES** — the capture's core claims are artifact-verified (exact
SIGTERM line, exit 0 / 1419 ms, K=12, durability ahead-by-21 with the
concept shortfall exactly equal to the GC sweep count, wire hygiene clean,
swarm metrics all ledger-exact, real portal renders). What fails the review
is evidence-document integrity: the showcase runbook quotes a shutdown line
that is not in the committed transcript and asserts a 332-mutation drain the
code's own info-level logging disproves (P1); the C3 tables state numbers
that match no artifact and are internally impossible (P2); and the GC control
run is presented as proof without a committed artifact (P2). The P3s are
count/wording/evidence-gap fixes. None of this touches Rust production code —
the remediations are confined to the docs and the runbook claims.

## Remediation disposition

*Empty — round 1 of the review/remediate loop. Findings CS-R1-1..CS-R1-8 are
for the remediation agent; disposition to be appended after remediation.

# Adversarial Review: C-series concurrency capture (worktree `c-series`, round 2)

```text
╔════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — Round 2 independent re-review of the round-1  ║
║  remediation (commits aa3406f + 42679ae on codex/c-series,      ║
║  base ac31252).                                                 ║
║  Branch: codex/c-series (worktree /home/nryn/work/lambo/        ║
║          worktrees/c-series)                                    ║
║  Date:   2026-08-18                                            ║
║  Reviewer: CSeriesReviewR2 (fresh, read-only)                  ║
║  Verdict: CLEAN / APPROVE — all 8 round-1 findings (1 P1 / 2    ║
║          P2 / 5 P3) verified closed against the committed       ║
║          artifacts; regression sweep green; zero new findings.  ║
╚════════════════════════════════════════════════════════════════╝
```

## Grounding

Reviewed read-only against the committed artifacts (no live-load re-run, no
live Cockroach). Authority read first: the round-1 record's findings AND its
remediation-disposition section (`adve-review-c-series-round1.md:307-389`),
plus `dev-diary/notes/concurrency-capture.md`. The remediation diff
`ac31252..HEAD` is 15 files: the two evidence docs, the note, the new
`evidence/concurrency/control-gc/` artifact set, the new `evidence/swarm/
probes/` artifact set, the swarm runbook, the loadtest README, and a
docstring-only change to `scripts/loadtest/mcp_load.py` — **no Rust
production code** and no changes to the original capture artifacts
(ledger/stderr/store/durability/swarm ledger/PNGs are untouched from the
implementation chain; verified via the diff stat).

Checks run, every number re-derived independently from the artifacts (not
from the disposition's claims):

- `python3 -m py_compile` on `mcp_load.py`, `mcp_swarm.py`,
  `check_durability.py` — clean.
- `check_durability.py` re-run from HEAD on the committed ledger + DB +
  stderr for BOTH the main run and the control run; output diffed against
  the committed `durability-20260817-204139.txt` and
  `durability-gcproof.txt` — **byte-identical, exit 0** (so every number
  the docs quote is exactly what the committed script computes from the
  committed artifacts).
- Independent ledger/store/stderr recomputation (python): line-anchored
  stderr checks, per-phase 429 analysis from the `phase` markers,
  post-SIGTERM response timing, burst action-set analysis, swarm ledger
  metrics, wire-hygiene regex scan, Pillow pixel check of the PNGs.
- `git diff --check HEAD` clean; worktree clean before and after review
  (the two SQLite `-shm`/`-wal` sidecars my readback created were removed;
  the committed DBs diff empty against HEAD).

## Round-1 finding closure (each verified with evidence)

### CS-R1-1 (P1) — FIXED, verified

`grep` across both docs (`evidence/concurrency/README.md`,
`dev-diary/notes/concurrency-capture.md`): zero occurrences of
`mutations=332`, `332-mutation`, or any claim that a tail was flushed. The
runbook now quotes the real shutdown sequence — `shutdown signal received,
winding down` (stderr line 1711, `20:42:31.788025Z`) → `lambo serve:
session closed, tail durable` (line 1712) — and names the absent
`Memory session closed (tail flushed)` line only to state it is **not** in
the transcript (the honest form of the fix: `src/memory.rs` logs that line
whenever `close()` flushes a non-empty tail, so its absence proves the
close-time tail was empty). Re-verified against the stderr: `tail lost on
exit` 0 hits, `Memory session closed` 0, `tail flushed` 0, `mutations=` 0
over all 1712 lines. Both docs state the artifact reality: surplus 21
interactions (store 862 − ledger-ok writes 841; **11 `ok` responses landed
after the SIGTERM timestamp** — recomputed: exactly 11, all
`record_action`, `t > 1786999351.788025`), and that `CLOSE_GRACE` was
**not** tested to its limit (close drain a no-op on SQLite).

### CS-R1-2 (P2) — FIXED, verified

The docs carry only artifact numbers. The runbook's accounting paragraph
and table match the regenerated durability file exactly: 3423 calls / 1311
ok / 841 successful writes (468 derive + 373 record_action) / derive
created·matched 555·815 / record_action created·edges 911·5506 / refused
310 / 429s 1682 / transport 120 / store interactions 862 (862−841 = 21) /
concepts 1359 vs ledger-created 1466 (555+911; 1466−1359 = 107 ==
`concepts_collected=107` at stderr line 1710). Scoped remnant scan over the
six C-series docs for `830`, `1454`, `3455`, `1303`, `465 derive`,
`365 record`, `585`, `870`: **zero matches** (the only file carrying those
old figures is the round-1 record itself, which documents them as the
finding).

### CS-R1-3 (P2) — FIXED, verified (preferred path)

`evidence/concurrency/control-gc/` is committed and internally consistent:
`gcproof.toml` has `[daemon] gc_interval = 1`; `gcproof.log` has 55 GC
sweep lines whose `concepts_collected` values (1 + 1 + 2 + 7) **sum to
11** (recomputed); `gcproof-ledger.jsonl` has 198 calls, 2 workers, full
phase set (cap-probe/overdrive/main/burst); `gcproof.db` reads 76
interactions / 206 concepts / 2036 edges; `run-gcproof.json` records
`gap_equals_collected: true`. The committed `check_durability.py` run on
the control artifacts reproduces `durability-gcproof.txt`
**byte-identical**: 217 ledger-created (37 derive + 180 record_action) −
206 store = 11 == GC sum; interactions 76 = 76 **MATCH**; exit 0. The
main-run 107 accounting stays artifact-proven (stderr:1710).

### CS-R1-4 (P3) — FIXED, verified

Ledger: 80 `ok` at-cap `record_action` calls inside the burst window
(`burst-start`..`burst-end`) with **80 distinct** action strings
(`burst action {worker}-{seq}`). Store readback: exactly 80 `burst action %`
concepts, plus 768 `burst-%` concepts. Runbook says 80.

### CS-R1-5 (P3) — FIXED, verified

`133 ms` appears nowhere in the C-series docs. The swarm runbook states the
committed transcript gap: `shutdown signal received` 20:50:24.561330Z →
`session closed, tail durable` 20:50:24.562066Z (~0.7 ms) — both
timestamps verified verbatim in `stderr-serve-20260817-205024.log`.

### CS-R1-6 (P3) — FIXED, verified

`mcp_load.py` docstring and `run_loop` docstring, the runbook, and
`scripts/loadtest/README.md` all say: overdrive = "free-run for the first
`--overdrive-calls` calls per worker, then paced at ~20 rps per worker
(0.05 s sleep) for the rest of the phase". The code matches the words
(`overdrive_left` gates only the no-sleep path; after 0 the loop sleeps
0.05 s and keeps issuing). The loadtest README phase list now includes the
overdrive phase as item 3; "zero transport refusals" is gone, replaced by
"zero rate-limit refusals (the 429s that straddle into the main window's
opening are the overdrive's burst-budget carryover)".

### CS-R1-7 (P3) — FIXED, verified

Both docs state the precise truth, re-derived: 1682 rate-limit 429s total,
**1673 inside the overdrive window** and **9 in the main window's opening**
(recomputed from the phase markers: all 9 land 3–9 ms after `main-start`,
boundary carryover), and **21 valid in-window `lambo_derive` calls
returned tool-level `store error` responses** — the ledger has exactly 21
`pattern=valid` is_error derive calls with
`lambo_derive: store error (the detail was logged server-side)`, matching
the 21 `hybrid derive could not commit` ERROR lines at
`stderr-20260817-204139.log:1689-1709`.

### CS-R1-8 (P3) — FIXED, verified

`evidence/swarm/probes/` exists: `omp-harness-garbled-tool-call.txt`
(prose fabricating a `lambo_derive` CLI — `lambo_derive schema
auth_guards.json`, … — spot-checked in the file), `raw-tools-probe.json`
(`choices[0].message` has no `tool_calls` key, `finish_reason: "stop"`),
and `probes/README.md` explicitly recording that these are **re-run
transcripts (2026-08-17 21:05 UTC)** of the original uncommitted
observations. The card scores 4.13/3.73 are dropped from the runbook,
which now says the per-card numeric scores "appear in no committed
artifact, so they are not quoted here". The quoted card texts remain
substantiated: both strings ("auth middleware guards schema integrity by
validating the user schema before data access", "billing service retries
failed charges") appear verbatim in the swarm ledger's derive concepts.
The metrics table footnotes the unparseable-turn rate — 54 of 218 model
turns (25%), all 54 recorded as `model_reply` with `parsed_concepts: 0`
(recomputed) — and defines "Model errors 0" as 0 `model_error` records
(HTTP/transport failures only; recomputed 0).

## Regression sweep (nothing re-broken)

- Exact SIGTERM line at stderr line 1712 (`lambo serve: session closed,
  tail durable`), `tail lost on exit` 0, exit code 0 and signal→exit 1419
  ms in `run-20260817-204139.json` — all unchanged and re-verified.
- Wire hygiene: 0 matches for DSN/driver/URL/token patterns over all 3423
  C2 call response fields and both stderr transcripts (re-run scan). One
  benign note: the **swarm** ledger's `meta` record (committed in 44aad26,
  i.e. pre-existing, not introduced by this remediation) carries
  `"llama_endpoint":"http://127.0.0.1:8081/..."` — a loopback config echo
  of the local llama.cpp server, same class as the whitelisted bind echo,
  no token material; it is not a call response field and the runbook's
  wire-hygiene claim (scoped to call response fields + stderr) holds as
  written. Recorded for transparency only.
- Swarm metrics all ledger-exact, recomputed: 164 derive + 164 recall over
  a 149.049 s window → 3961.1 derive-calls/hour (7922.2 MCP calls/hour with
  recalls); created 487 / matched 109 / 596 references → dedup 0.183;
  store readback 164 interactions / 487 concepts / 1522 edges / 0 lease
  rows; 0 `model_error` records; 54/218 unparseable (footnoted).
- PNGs are real renders: 3200×1800, 300 / 263 unique colors in the 64×36
  sample, luma 105–255 / 109–255 — not blank, not placeholders.
- No new numbers introduced anywhere that contradict the artifacts: the
  only figures new to the docs (21, 11, 217, 206, 55, 198, 76, 80, 768,
  1673, 9, 54/218, 0.7 ms, 3961.1) were each recomputed above and match.
- Disposition section present in the round-1 record (lines 307-389),
  accurately describing the remediation; working tree clean; main checkout
  still shows only the 3 pre-existing untracked files.

## Commands / results

```text
$ git -C worktrees/c-series diff --stat ac31252..HEAD
  15 files, +859 -62  [no src/ files; capture artifacts untouched]
$ python3 -m py_compile scripts/loadtest/mcp_load.py mcp_swarm.py check_durability.py  # clean
$ python3 scripts/loadtest/check_durability.py --ledger ... --db ... --stderr ...      # both runs
  → diff vs committed durability files: IDENTICAL, exit 0
$ grep 'mutations=332|830|1454|3455|1303|585|870|bounded to 120|133 ms|73 unique|zero refusals|4.13/3.73'  # C-series docs: 0
$ python3 stderr analysis   # 1712 lines; 1711 shutdown / 1712 assertion / 1710 GC 107; 21 ERROR at 1689-1709;
                            # tail lost 0, Memory session closed 0, tail flushed 0, mutations= 0
$ python3 ledger analysis   # 3423 calls; 1311 ok; 429s 1673 overdrive / 9 main (3-9ms after main-start);
                            # 11 ok record_action after SIGTERM; 80 burst actions / 80 distinct;
                            # 21 valid in-window store-error derives; 120 transport (10 per worker)
$ python3 store readback   # 862 interactions, 1359 concepts, 9279 edges, 768 burst-*, 80 burst-action, 0 leases
$ python3 gcproof analysis # 55 sweeps sum 11; 198 calls / 2 workers / full phase set; 217-206=11; 76=76
$ python3 swarm analysis   # 164/164, 487/109/596, 3961.105/hr, 54/218, 0 model_error, 1522 edges, 0 leases
$ Pillow pixel check       # 3200x1800, 300/263 colors, luma 105-255 / 109-255 — real renders
```

## Verdict

**CLEAN / APPROVE.** Every round-1 finding — the P1 shutdown-line
fabrication, the P2 unmatched accounting numbers, the P2 prose-only GC
control proof, and the five P3 count/wording/evidence-gap fixes — is
genuinely closed, each re-derived from the committed artifacts rather than
taken from the disposition's claims. The regression sweep confirms the
core capture claims (exact SIGTERM line, zero tail loss, exit 0 / 1419 ms,
wire hygiene, ledger-exact swarm metrics, real portal renders) still hold
and that no new numbers contradict any artifact. No findings in this
round; the only observation is a pre-existing, benign loopback config echo
in the swarm ledger's meta record, noted for transparency and outside this
patch's scope.

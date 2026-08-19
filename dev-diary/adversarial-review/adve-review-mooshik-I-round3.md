# Adversarial review — lambo-for-mooshik workstream I, round 3

- **Reviewer:** `i_review_r3` (independent; source read-only apart from four declared verification-only flips, all reverted and reported below; nothing committed. Tree clean at `1f86792` at close, `git diff --check def9709..1f86792` clean.)
- **Scope:** remediation commit `1f86792` ("fix(serve): I round-2 remediation — arm shutdown before the I-inserted startup block") against its parent `def9709`. 11 files, +160/−29. Authorities: `dev-diary/adversarial-review/adve-review-mooshik-I-round2.md` — its three findings (I-R2-1 P1 blocking, I-R2-2 P3, I-R2-3 P3) and their required remediations are the checklist — with round 1 read for the pattern history, plus `dev-diary/lambo-for-mooshik/I-observability.md`, `dev-diary/README.md` "Conventions for agents", and the binding constraint from project memory.
- **CI note (orchestrator-attached):** the post-push run on `1f86792` is **green** — run 32283530988, 2m09s, all jobs pass — with the two red runs (32262805901, 32275330759) bracketing the regression window exactly at the I implementation. The verdict below stands on local artifacts; CI corroborates it on the environment that originally caught the defect.
- **Verdict:** **CLEAN** — three P3 advisory findings (**I-R3-1**, **I-R3-2**, **I-R3-3**), none blocking. Per orchestrator decision 2026-08-20, the advisories are carried as **J0** rather than remediated in a fourth round.

All three round-2 findings are remediated at the artifact, and the P1 is remediated at the mechanism rather than at the test. I reproduced round 2's negative control in both directions myself and it discriminates exactly as claimed: a 400 ms widener inserted **after** the arming leaves `serve_pre_handshake_durability` green (1.17 s → 4.59 s, so the widener is demonstrably live and the SIGTERM is being buffered and honoured); the same widener moved **before** the arming — the pre-fix ordering — fails with `ExitStatus(unix_wait_status(15))`, the CI message byte-for-byte. The arming placement is what fixes it, and the fix is structural: `serve()` now arms as the first statement after `build_memory` returns, with `Ledger::open`, `LamboServer::new`, the heartbeat spawn, the event pump and the serve-level attach log all below it.

The self-reported arithmetic deviation on I-R2-3 is the most interesting thing in the commit, and it holds up better than "reasonable". `accepted − written − write_failed` is correct against the code line by line — `try_send`'s `Full` arm increments `channel_full` and never touches `accepted`, so the prescribed `accepted − written − dropped` would subtract lines the channel never took. And it is not merely argued: the channel-full test's new assertion **fails under the prescribed formula** — I substituted it (flip C) and got `left: 961, right: 1025`, understated by exactly `overflow` = 64, precisely the magnitude the comment predicts. A deviation pinned by a test that the alternative cannot pass is a better artifact than the finding asked for.

What I found is three P3s, all doc-precision, all in the same family the previous two rounds named — and one of them is the fourth consecutive instance of that family: the load-bearing claim about the parked-writer case keeps being written from intent. `Ledger::open`'s own docstring still asserts the ordering this commit inverted, ~540 lines above the docstring the commit *did* fix in the same file.

## Method

Read the full diff hunk by hunk, then read the ordering in `serve()` and the counter arithmetic in `ledger.rs` at the source rather than through the commit message's description of them. Traced every `write_failed` increment site and every `accepted` increment site to classify the arithmetic exactly. Built `--features store-sqlite,fixtures` and drove six real MCP stdio sessions against a real provisioned SQLite store: ledger-off and ledger-on payload captures at `1f86792`; the same two at the **parent** `def9709` (checked out `src/` only, rebuilt, restored) so the ledger-OFF byte-identity claim is verified by diffing two real binaries' `structuredContent` rather than by trusting the prefix-based test; and two reader-less-FIFO sessions, one without and one with `--ledger-heartbeat 1`. Ran the full gate suite sequentially with `RUSTFLAGS="-D warnings"`, recording real counts. Hammered `ledger::tests` ×8 and `serve_pre_handshake_durability` ×12, then ×15 more under ten busy loops. Swept the whole tree with `rg` for every claim about the arming order, to find stale siblings rather than assume the two the commit message names are all of them.

Four verification-only flips, each run then reverted with `git checkout --`:

- **A** — a 400 ms `tokio::time::sleep` immediately after the arming in `serve()`.
- **B** — the same sleep moved above the arming, reconstructing the pre-fix ordering.
- **C** — `queued()` rewritten to the literally-prescribed `accepted − written − dropped`.
- **D** — a review-only unit test opening a ledger with a panicking `BatchSink`, to drive the `Disconnected` class of `write_failed` (lines never `accepted`) against a genuinely non-empty queue and read what the gauge reports.

## Round-2 findings: verification at the artifact

| # | Required remediation | Verified how | Verdict |
|---|---|---|---|
| **I-R2-1** (P1, was blocking) | Option 1: arm immediately after `build_memory` returns, before `Ledger::open` / `LamboServer::new` / heartbeat / event pump; correct the invariant comment | **Ordering read at the source** (`serve.rs:763-800`): `authorize_bind` → `authorize_ledger` → `build_memory` → **`shutdown_signal()` + `tokio::pin!`** → `Ledger::open` → `LamboServer::{new,with_ledger}` → heartbeat spawn → `mem.events()` + event pump → serve-level attach log → transport. The arming is the first statement after `build_memory`; the moved lines are byte-identical; everything else added is comment. **Negative control reproduced in both directions** (flips A and B): widener after the arming → **pass** (4.59 s vs a 1.17 s baseline, widener demonstrably live); widener before → **FAIL, `ExitStatus(unix_wait_status(15))`**, the CI failure byte-for-byte. Test green 12/12 unhammered and **15/15 under ten busy loops** | **Remediated, at the mechanism** |
| I-R2-1, comment content | Six required elements | **All present and accurate:** guard begins when `build_memory` returns; `build_memory` itself unguarded "exactly as it was pre-I"; serve-level line "fully guarded"; the ~6 µs→~1.1 ms history (a fair transcription of round 2's measurements); option 2 deferred with the hung-build reason; tagged `(I-R2-1)` twice. The comment now states a property true of **both** attach lines, and carries its own history, which is what stops a future refactor from re-tightening it | **Remediated** |
| I-R2-1, adjacent stale claims | `shutdown_signal` docstring; FIFO test docstring | Both corrected as reported — the first now names the mechanism (eagerness makes the arming *point* effective, it does not move it), the second re-grounds the test's justification on its surviving half. A **third** instance in the same file was missed → **I-R3-1** | **Remediated** (one sibling missed) |
| **I-R2-2** (P3) | Keep the loose matcher, comment the trap | **Matcher unchanged, verified mechanically:** the diff filtered to non-comment lines yields exactly one removed `//!` line; executable code byte-identical. The comment names the trap in the imperative: "This matcher is LOOSE ON PURPOSE — do not tighten it (I-R2-2) … **Anchoring on the serve-level line would green CI while leaving the product hole open. The looseness IS the coverage**." Module doc's singular corrected with an explicit "'A' line, not 'the'" paragraph | **Remediated, precisely** |
| **I-R2-3** (P3), row wording | Queue-then-drop-then-`write_failed` | The row now reads all three stages in order, with the queue "visible immediately as `ledger_queued_lines`". **Confirmed at the real binary** (below) | **Remediated** |
| I-R2-3, the key | Only when on; OFF payload byte-identical | **Verified independently at two real binaries.** OFF: 17 keys each at `1f86792` and `def9709`, symmetric difference NONE, no non-timing value differs. ON: 22 → 23, added exactly `['ledger_queued_lines']` | **Remediated** |
| I-R2-3, the arithmetic deviation | `− write_failed` rather than the prescribed `− dropped` | **Verified line by line:** `append`'s counter sites show `Err(Full)` → `channel_full`, **no `accepted`**, so the prescribed formula double-subtracts backpressure. **Pinned by test:** flip C fails `left: 961, right: 1025` — short by exactly `overflow` = 64. Both `queued()` and `shutdown` share one subtraction, so gauge and exit count cannot drift apart | **Remediated; the deviation improves on the prescription** |
| I-R2-3, the new assertion | `queued == 1 + CHANNEL_CAPACITY` while `channel_full == 64` | Present with inline reasoning plus a drained-queue-reads-0 assertion. The construction is deterministic, not lucky: `entered >= 1` is only set after the coalescing loop returned `Empty`, so the channel is provably empty at that instant. 8/8 runs | **Remediated** |

## New findings (carried as J0, per orchestrator decision 2026-08-20)

### I-R3-1 (P3) — `Ledger::open`'s docstring still asserts the ordering this commit inverted; the third instance in the file, the fourth of the pattern

- **Evidence:** `src/ledger.rs:250-254`, present tense: *"`serve` calls this **after** the single-writer lease is taken and **before** the SIGTERM handler is armed, so a blocking `open` on this path would take down memory through the flag that turns observability on."* Since this commit, `serve` arms **before** `Ledger::open` (`serve.rs:797` vs `:806`). The commit fixed the two adjacent stale claims; this one is ~540 lines away, on the same subject, findable by one `rg` query.
- **Impact:** doc-only; the paragraph's probe-placement conclusion survives on independent grounds. But it is the sentence a maintainer would read to decide whether the probe may be moved back, and it overstates the stake by describing a hazard the arming closed.
- **Remediation (J0):** reword to the current ordering — a blocking `open` here wedges a server that never serves (availability), not one that dies unflushed (durability). Keep the conclusion.
- **Two siblings checked and deliberately not flagged** (past-tense narrative, accurate as history): `ledger.rs:423-425` and `I-observability.md:236-238`.

### I-R3-2 (P3) — The kit README's parked-writer reading recipe describes something a ledger file can never show

- **Evidence:** `scripts/observability/README.md:320-327`: *"A heartbeat with `written` flat and `queued` climbing is a parked writer."* Measured at the real binary: a parked writer writes **nothing, heartbeat lines included** — they travel the same channel and are abandoned with everything else (`abandoned=7 dropped=7 written=0`, seven being six calls plus the one beat). The parked case is visible only through **live `lambo_stats`**, never through the file; the heartbeat-trend reading is valid only for a writer that is *behind*.
- **Impact:** small, consumer-side, but affirmatively misleading in the document that defines how the file is read — the reader is told to look in the file for the artifact of a condition that keeps itself out of the file.
- **Remediation (J0):** name the transport in the sentence. Optionally have `header()` mention a non-zero last-heartbeat `queued` beside the dropped line, so "the ledger is complete" is never printed over a known backlog.

### I-R3-3 (P3) — The phase doc's Handoff Log was not updated for this commit's two behavioural changes

- **Evidence:** convention 7; `I-observability.md`'s Handoff Log last touched at `49a9b09`. This commit's signal-arming move and the new public `ledger_queued_lines` key are recorded only in the commit message.
- **Remediation (J0):** one Handoff entry — the arming move (option 2 deferred and why) and the new key, folding in the arithmetic deviation's reasoning, which is the part most likely to be re-derived.

## Attacks that did not land

- **`queued()` under-reporting via never-`accepted` `write_failed` entries.** Flip D confirms the class is real (a panicking sink: `queued` 1 → 0 under 21 `Disconnected` appends, saturating away a real backlog) but all three sub-classes are unreachable in the shipped serve path: a `serde_json::Value` cannot fail serialization (Number holds no NaN/Inf); post-shutdown appends cannot be observed by `lambo_stats` (shutdown runs after `run_and_close`); `Disconnected` needs a writer-thread death that the production sink has no panic path to. Where reachable, drift is toward zero with `dropped_write_failed` climbing loudly beside it. Advisory: the code documents the `channel_full` exclusion but not that the subtrahend carries never-accepted classes — one sentence would spare a future reader flip D.
- **Over-reporting:** the identity closes; the only over-report needs a permanently dead writer with no further traffic.
- **Flakiness in the new assertion:** the sync point is a genuine barrier; 8/8.
- **The prefix-based byte-identity test:** bypassed by verifying at two real binaries; identical.
- **Drop-order side effect of the move:** the pinned signal future now outlives the ledger drain — direction is *more* coverage; `close_bounded` re-arms regardless. Advisory only.
- **"queued moves on the first call":** off by one without a heartbeat (ledger line appended after the response is built); exact with `--ledger-heartbeat` armed, which is the shipped configuration. Advisory only.

## Positive observations

- **The P1 is fixed structurally, and the placement is demonstrably what does the work** — flips A and B differ only in which side of `shutdown_signal()` a sleep sits on, and differ in outcome between exit 0 and `unix_wait_status(15)`. As clean a causal demonstration as this class of bug allows.
- **The invariant comment now carries its own history**, names the residual window, and says why the stronger option was refused.
- **I-R2-2 is the rare finding whose correct remediation is to change nothing executable, and it was executed that way** — verified mechanically. A test comment that will survive contact with someone trying to green CI.
- **The arithmetic deviation is held down by a test the prescribed formula fails** — the strongest form a deviation can take. The `1 +` is the in-flight batch, guaranteed by the test's own construction.
- **The failure-table row is now what the binary does, stage for stage** — measured, every line accounted for, the C-series ordering guarantee still ahead of observability.
- **Both derivations share one subtraction**, so the live gauge and the exit count cannot disagree.
- **The doc surface moved as a set** (docs + site mirrors + README semantics + duckdb recipe + regenerated sample, drift-checked by `verify.sh`).
- **Test counts unchanged and honestly explained** — the new coverage is assertions inside existing tests.
- **The FIFO test's justification was re-grounded rather than deleted** when the fix removed half its motivation.

## Gate results

Run sequentially on `1f86792` with `RUSTFLAGS="-D warnings"`.

| Command / check | Result |
|---|---|
| `cargo fmt --all -- --check` | **pass** |
| clippy ×6 | **pass ×6** |
| `cargo test --all --features fixtures` | **pass** — lib 793 / 0 / 1; all binaries green |
| `cargo test --features store-sqlite,fixtures` | **pass** — lib 858 / 0 / 1; every integration binary green |
| `cargo test --no-default-features --features store-sqlite` | **pass** — 515 / 0 |
| `cargo test --no-default-features --features store-cockroach` | **pass** — 497 / 0 |
| `cargo test --features ship,fixtures --lib` | **pass** — 884 / 0 / 8 |
| `cargo check --no-default-features` / `--features demo` | **pass** |
| `sqlite-vectors` CI row, verbatim | **pass** — 15 / 0 / 0; all three guards |
| `scripts/observability/verify.sh` | **pass** — `ALL CHECKS PASSED`, incl. the sample-drift diff |
| Every count in the commit message | **exact**, and identical to round 2's, as claimed |
| Baseline pre-handshake test | pass, 1.17 s |
| Flip A (widener after arming) | **pass**, 4.59 s |
| Flip B (widener before arming) | **FAIL, `unix_wait_status(15)`** — CI failure byte-for-byte |
| Flip C (prescribed formula) | **FAIL** — `left: 961, right: 1025`, short by exactly 64 |
| Flip D (`Disconnected` vs real queue) | `queued` 1 → 0; class real, path unreachable |
| Pre-handshake test ×12, then ×15 under 10 busy loops | **27 pass, 0 fail** |
| `ledger::tests` ×8 | deterministic, 0 failures |
| Real FIFO serve, 8 calls, no heartbeat | `queued` 0→7, all other counters 0; `tail durable` before ledger close; `abandoned=8 dropped=8`; exit 0 |
| Real FIFO serve, 6 calls, `--ledger-heartbeat 1` | `queued` 1→6; `abandoned=7 dropped=7`; exit 0 |
| `lambo_stats` OFF, `1f86792` vs `def9709` binaries | 17 keys each, identical, no non-timing value differs |
| `lambo_stats` ON, same two binaries | 22 → 23, added exactly `['ledger_queued_lines']` |
| `git diff --check def9709..1f86792` | clean |
| Verification-only flips | 4; all reverted; tree clean |

## Verdict

**CLEAN** — advisory findings **I-R3-1**, **I-R3-2**, **I-R3-3**, all P3, none blocking, carried as **J0**.

The P1 is genuinely closed, and closed at the mechanism. The invariant comment that round 2 identified as part of why the hole survived review has been replaced by one that states a property true of both attach lines, discloses the residual window, and records why the stronger option was refused. I-R2-2 was remediated in the only way that could have been right — nothing executable changed. I-R2-3's row matches the binary stage for stage, the new key's off-payload byte-identity is confirmed against the parent binary, and the arithmetic deviation is pinned by an assertion the prescribed formula cannot pass. Gates green on all sixteen invocations, every number exact, and CI green on the environment that caught the original defect.

One pattern is worth handing forward rather than grading. Round 1's blockers were claims written from intent; round 2's blocker was a comment falsified by a refactor four lines above it; I-R3-1 is a third docstring in the same file falsified by the same reordering — which the remediation went looking for and found two of. The remaining one is not carelessness (it is 540 lines from the change); it is evidence that this module's prose has more load-bearing claims about `serve()`'s startup ordering than any one of them signals, and that the cheap defence is an `rg` sweep for the ordering claim rather than a read of the neighbourhood. That sweep takes one query and would have found it.

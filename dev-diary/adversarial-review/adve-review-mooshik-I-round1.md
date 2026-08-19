# Adversarial review — lambo-for-mooshik workstream I, round 1

- **Reviewer:** `i_review_r1` (independent; source read-only apart from four declared verification-only flips, all restored and reported below; nothing committed)
- **Scope:** implementation commit `a9ec7f2` ("feat(serve): I1-I3 observability — call ledger, heartbeats, analysis kit") against its parent `e8b4790`. 27 files, +4853/−126. **Sha note:** after this review ran, the branch was rebased over the docs-only J commit (`2fd18b9`); `a9ec7f2` is content-identical to `ed674a8`, the sha now on the branch — same diff, same tree apart from J's two doc files. Authorities: `dev-diary/lambo-for-mooshik/I-observability.md` (spec + Handoff Log; the "Done when" list is the acceptance bar); `DOGFOOD.md` and `DOGFOOD-SETUP.md`; `dev-diary/README.md` "Conventions for agents"; the H3 wire contract in `src/recall/detail.rs`; the binding design constraint from project memory ("the serve call ledger must never take down memory … off by default; it is not the store").
- **Provenance:** implementation by an Opus agent (terminated twice by API errors), gates+commit by a Sonnet closer — the review deliberately probed that seam.
- **Verdict:** **REQUEST_CHANGES** — 5 P2, 7 P3. Blocking: **I-R1-1, I-R1-2, I-R1-3, I-R1-4**

The mechanism is the strong part and I could not break the thing the constraint is really about: a tool call is never failed or delayed by the ledger. I drove a real `lambo serve --ledger --ledger-heartbeat 1` over stdio against a real SQLite store, and the shutdown ordering holds exactly as the Handoff claims — `session closed, tail durable` lands **before** `call ledger closed written=14 dropped=0`, so the C-series durability guarantee is not moved behind observability. The provenance refactor, which was the risk I expected to sink this, is honest: `candidates()` really is the projection, the H3 wire shape is byte-for-byte what it was (I dumped the serialized keys), and the `lambo_stats` payload is genuinely byte-identical with the ledger off. All fifteen gate invocations pass, and every number in the commit message is exact — including "22 new tests", which is 22 on the nose.

What blocks is four places where the artifact does not support the claim attached to it. The ledger tells a consumer a canonical marker rendered when the token budget rendered nothing at all. The headline row of the never-block failure table — channel full — has zero coverage, and the test named for it drops 3072 lines without the channel ever filling once (measured, not inferred). A `--ledger` path whose `open` blocks wedges startup *after* the lease is taken and *before* the signal handler is armed, so SIGTERM kills the process with the tail unflushed — the exact failure "observability must never take down memory" exists to forbid, in a module whose own table says "serve still starts". And `DOGFOOD-SETUP.md`, the runbook that builds the pinned binary and wires all seven clients, sets neither `LAMBO_GIT_SHA` nor `--ledger` — while `src/ledger.rs` says it does.

## Method

Read the full diff hunk by hunk, then traced each claim to its producer rather than its comment. Ran all fifteen gate invocations sequentially (one script, no concurrent cargo, to avoid the build race the commit message discloses) and recorded the real counts. Built the binary and drove three real MCP stdio sessions: a full recall-first cycle with heartbeats and a clean SIGTERM; a probe for `reserve`'s early exits and a 15.4 KiB query; and a startup probe against a FIFO. Ran `verify.sh`, then ran all five scripts against (a) the committed sample, (b) a **real** ledger and a **real** provisioned store produced by the driven session, and (c) a hand-constructed adversarial ledger (out-of-order timestamps, a derive with no facts, a `v:2` line, a line with no `v`, a partial stats payload, an unknown `kind`, a torn tail). Four verification-only source flips, each run then reverted with `git checkout --`: a `LegScores` duplicate-input probe in `candidates.rs`; a token-budget probe plus a `DetailedRecall` wire-shape dump in `server.rs`; and a split counter in `ledger.rs` that distinguishes a `try_send` rejection from a failed write, because the shipped code folds both into `dropped` and no black-box test can tell them apart.

## Verified claims

| Claim | Where | Verdict |
|---|---|---|
| Shutdown ordering: the ledger closes **after** `Memory::close`, so a hung writer cannot precede the durable tail | `serve.rs:810-835`; real run | **True, empirically.** `14:00:14.663978 session closed, tail durable` → `14:00:14.669955 call ledger closed written=14 dropped=0`. Heartbeat aborted first. `written` equals the 14 lines actually in the file; exit 0. |
| The writer is a dedicated OS thread, not a Tokio task | `ledger.rs:171-177` | **True.** `std::thread::Builder::new().name("lambo-ledger")`. The stated reason (a hung FS must not occupy the worker that runs `Memory::close`) holds — but see I-R1-3 for the same hazard on the *startup* path, which is not on that thread. |
| A tool call is never failed or delayed by the ledger | `ledger.rs:218-248`, `server.rs:641-687`; real run | **True.** `append` is serialize + `try_send`, no await, no I/O. Every tool call succeeded with the ledger directory deleted mid-run (the committed test) and in my own runs. |
| `candidates()` is exactly the projection of `candidates_with_legs()` | `candidates.rs:186-189` | **True** for every input the suite covers (789 + 854 + 513 + 495 + 880 tests all exercise the new path, since `candidates` now delegates). One constructed input diverges from the *parent commit* — I-R1-6. |
| Recall behaviour is byte-identical to the parent: same candidates, scores, ordering, tie-breaks | `rank()` at `candidates.rs:262-273` | **True as far as I could test.** `rank` reproduces the parent's `total_cmp` desc → UUID asc → no truncation, verbatim. The keyword and recent legs are arithmetically identical (both were last-write / max-idempotent before). The vector leg is not, on duplicate ids only (I-R1-6). |
| `DetailedRecall.legs` is `#[serde(skip)]` and the H3 wire contract is unchanged | `detail.rs:126-148` | **True, empirically.** Serialized a `DetailedRecall` with a populated `legs` map: top-level keys `["hits","response_annotations"]`; hit keys exactly `["annotations","blast_radius","concept_type","content","included_in_context","score","status"]` — the pinned H3 set. `legs` absent. `src/cli/serve_web.rs` is not in the 27 changed files and its exact-key H3 tests are green. |
| `lambo_stats` payload byte-identical with `--ledger` off | `server.rs:1520-1535` | **True.** Same 17 keys, same values. The source reordering (`summary` moved) is invisible on the wire: `serde_json` without `preserve_order` sorts keys — confirmed by the real run's payload emerging alphabetical. |
| "22 new I1/I2 unit tests (ledger, serve, server, candidates)" | commit message | **True, exactly.** 7 + 3 + 9 + 3 = 22. |
| Gate suite green on all listed configurations, with the listed counts | commit message | **True, every number.** 789 / 854 / 513 / 495 / 880 / 15 vector tests. See the gate table below. |
| `--ledger-heartbeat` without `--ledger` refused with a message naming the fix | `serve.rs:604-618` | **True, at the binary.** Refused before the lease is taken, exit 1. |
| `--ledger-heartbeat 0` refused rather than panicking `tokio::time::interval` | `main.rs:500-509` | **True at the CLI** — clear message, exit 2. Not true at the library boundary: `authorize_ledger` accepts `Some(ZERO)`, so a `serve()` caller still gets a silently-panicked heartbeat task. I-R1-12. |
| The heartbeat test does not accidentally require a real sha | `server.rs:3596+`, `serve.rs:1320+` | **True.** Both compare against `crate::ledger::GIT_SHA`, so `"unknown"` passes. No test pins a sha value. |
| `truncate_for_ledger` cuts on a char boundary, bounded at 200 chars | `server.rs:444-449` | **True.** `char_indices().nth(200)` cannot land mid-codepoint. Worst-case line reasoning checks out against `MAX_TOP_K = 100`, `MAX_CONTENT_BYTES = 16_384`. The `query` field is exempt from it — I-R1-12. |
| Per-tool facts are lazy, so "off" builds no JSON | `server.rs:466-470`, `641-646` | **True.** With no ledger `observed` short-circuits to `contain_panic`. One residual per-call cost remains — I-R1-12. |
| The HTTP transport no longer rebuilds `ToolRouter` per request | `serve.rs:1119-1139` | **True.** A straight improvement, correctly credited as independent of I1. |
| G1's measured bands are transcribed verbatim into `score_bands.py` | `score_bands.py:56-62` vs `evidence/mooshik-g-recall-calibration/measurement.txt` | **True, all four bands**, digit for digit. |
| `duplicates.py` reads the committed dogfood-shaped store schema | `duplicates.py:82-96` vs `migrations/sqlite/001_init.sql:72-89` | **True, and verified against a real store** — decoded real dim-1024 vectors from the driven session's `lambo.db`. |
| `verify.sh` passes; all five reports find their planted facts | run | **True.** `ALL CHECKS PASSED`. |
| The scripts fail loud rather than reporting an empty set as a pass | run | **True, and it earned its keep** — `NO VECTOR-LEG SCORES IN THIS LEDGER … that is the finding` on a real ledger whose writes had not flushed. |
| Out-of-order timestamps are handled | `_ledger.py:100-107` | **True.** `sorted_calls()` fixed a deliberately reordered pair; string sort is safe for chrono's fixed-offset output. |
| CLI / `serve-web` verbs unaffected | `git diff --name-only -- src/cli/` | **True.** Zero files under `src/cli/` changed. |
| Done-when 1 | tests + real run | **Met** for "off by default" and "one line per call"; the `duckdb`-end-to-end half is not — I-R1-7. |
| Done-when 2 | real run | **Met on real traffic** — per-leg maps `{"bm25":1.0137,"recent":0.35}`, `{"bm25":0.6213}`, `{"recent":0.35}`, `{}` (traversal expansion). |
| Done-when 3 | `server.rs:3361+` | **Met for the unwritable-path arm**; **not met for the channel-full arm** — I-R1-2. |
| Done-when 4 | tests + real run | **Met in the file format**, unmet as an operable property — I-R1-4. Real heartbeats: `sha=unknown`. |
| Done-when 5 | — | **Half met.** All five scripts run (proved against a real ledger and store); no such artifact committed — I-R1-7. |
| Done-when 6 | diff | **Honestly marked half-done**, but the stated remainder understates it — I-R1-4. |

## Findings

### I-R1-1 (P2) — The ledger reports that a canonical marker *rendered* when the token budget rendered nothing at all; the whole flag family is budget-blind and the definition is written down nowhere

- **Evidence:** the spec's wording is exact — "whether a canonical marker, conflict line, or **blast-radius warning rendered**" — *rendered*. `recall_facts` computes all five flags over every returned hit with no reference to `included_in_context`: `canonical_marker |= hit.is_canonical` (`server.rs:353`) and the four annotation flags inside the same loop. Probe (verification-only, restored): promote a concept to `Canonical` through the audited path, then recall it with `max_tokens: 1` so no block fits. Result: `context = ""`, both hits `included_in_context: false`, the string `[canonical]` appearing nowhere in the response — and the ledger says `canonical_marker: true`, `blast_radius_warning: true`. The distinction is real in the code: the `⚑`/conflict/hot/reservation lines are pushed into the flat `warnings` vector for every hit regardless of budget (`assemble.rs:310` — *"a block truncated from the context still reports its conditions"*) and delivered as a second text block, so those four flags are defensible as "the line reached the agent". The `[canonical]` marker exists **only** inside the hit's budget-gated block (`format.rs:143`, `assemble.rs:334-346`). Nothing puts it in `warnings`.
- **Impact:** (a) `canonical_marker` is unsupported by anything the agent received, under either reading of "rendered", and it is the field a reader would use to answer "did canonization reach the agent". (b) The set-level semantics are documented nowhere — the README's "Definitions that are choices" section does not name it. (c) The implementer understood the distinction: `warnings.py` compensates for exactly one of the five (`of which 1 were attached to a hit the TOKEN BUDGET CUT`) and `make_sample.py` plants that case — so the kit is honest about `load_bearing` and silent about `canonical_marker`, and that `warnings.py` line is itself slightly wrong the other way (the model *did* see the `⚑` warning line, just not the block).
- **Repro:** promote via `apply_canonization_transition` (`None → Venerable → Canonical`), then `lambo_recall` with `max_tokens: 1`.
- **Required remediation:** pick a definition and make the artifact match. Either (a) compute the flags over `included_in_context` hits only, naming the four warning kinds' budget-independence separately — preferred, it is what the spec says; or (b) keep set-level semantics and rename (`canonical_hit_returned`, …) with the choice stated in the README's choices list. Either way, cover the `canonical_marker` budget case in `warnings.py` or say why it is exempt, and correct "the model never saw the block" to distinguish the block from the warning line.

### I-R1-2 (P2) — The channel-full arm of the never-block guarantee has no coverage, and the test named for it measures a different mechanism on a premise that is false

- **Evidence:** the module's failure table (`ledger.rs:23-28`) lists `channel full (writer behind) | drop the line, dropped += 1` first. The test `a_full_channel_drops_rather_than_blocking_the_caller` (`ledger.rs:516-537`) claims *"No writer thread can drain a channel whose receiver is parked on a path it cannot open"* — false: `writer_loop` calls `rx.recv()` **first** and fails on the *write* (`ledger.rs:296-310`); the receiver is never parked. Instrumented the `try_send` rejection arm separately (verification-only, restored): `unopenable-path burst of 3072: total dropped=3072, of which CHANNEL-FULL=0`. The test duplicates `an_unopenable_path_drops_every_line_and_never_panics` under a name claiming the opposite mechanism, and its assertion cannot distinguish them even in principle — both drop sources increment one `AtomicU64` (`ledger.rs:245`, `:316`), so no black-box test can separate them.
- **Impact:** the design constraint's first named failure mode — the one a genuinely stalled filesystem produces — is untested and *untestable through the current surface*. The `CHANNEL_CAPACITY = 1024` sizing rationale rests on behaviour nothing verifies. Could not be closed by experiment: the natural way to park the writer (a blocking `open`) dead-ends in I-R1-3.
- **Required remediation:** make the arm observable, then test it. Split the counter (`dropped_channel_full` vs `dropped_write_failed` — also better operational information, surfaced separately in `lambo_stats`), inject the writer's sink (test-only trait or closure) so a test can hold the writer inside `write_batch`, push past 1024, assert `dropped_channel_full > 0` and that `append` returned. At minimum, rename the test to what it tests and mark the channel-full row as uncovered.

### I-R1-3 (P2) — A `--ledger` path whose `open` blocks wedges serve startup with the lease already held, and SIGTERM then kills it with the tail unflushed

- **Evidence:** `Ledger::open`'s doc says **"Never fails."**; the table says `path unopenable at startup | one WARN, every line drops, serve still starts`. The startup probe is a synchronous, unbounded `open_for_append` on the runtime's main task (`ledger.rs:159`), called from `serve()` *after* `build_memory` has taken the lease and started the daemon/flush/canonization loops, and *before* the transport and SIGTERM handler exist (`serve.rs:746-751`). Product-level repro: `mkfifo $S/fifo.jsonl; lambo serve --ledger $S/fifo.jsonl` → one log line (session attached, lease taken), no handshake, still running after 6s; SIGTERM ends it with **no** `session closed, tail durable` — the default signal disposition killed it before the handler was armed.
- **Impact:** observability taking down memory, reached through the flag that turns observability on: the server never serves, the lease is held by a process that will never release it, and the C-series durability invariant is bypassed. P2 not P1 because the trigger is narrow (a typo yields `ENOENT`, handled loudly and correctly); P2 not P3 because the binding constraint is contradicted by a doc that claims the contradiction cannot happen, and the fix is small. Also blocks the only route to testing I-R1-2.
- **Required remediation:** move the probe into the writer thread's first iteration (it already reopens per batch; a blocking `open` then parks the writer, which the OS-thread design exists to tolerate) — or bound the startup probe (`O_NONBLOCK` / short-lived thread with a join budget). Drop "Never fails." for "never fails *the server*", and correct the table: "cannot open" and "open blocks" are different classes.

### I-R1-4 (P2) — `DOGFOOD-SETUP.md` sets neither `LAMBO_GIT_SHA` nor `--ledger`, so an operator following it gets a sha-less binary and no ledger — and `src/ledger.rs` says that file already handles it

- **Evidence:** `src/ledger.rs:101`: *"`DOGFOOD-SETUP.md`'s build step is the place that must do it."* That file is not among the 27 touched; its build step is a bare `cargo build --release --features store-sqlite,embed-bge`, and its §4 gives seven per-client registration blocks, none carrying `--ledger` — under prose that already reads "so the ledger attributes writes per client". `grep -rn LAMBO_GIT_SHA` hits six files; not that one. Real run confirms: every heartbeat `sha=unknown`, every report header `binaries: 0.2.2 @ unknown`.
- **Impact:** follow the replication runbook and I2's acceptance property ("an upgrade shows as a sha change") is unobtainable — two builds at different commits both stamp `"unknown"` — and there is no ledger at all, so metrics 1/2/4/5 stay unmeasurable. The Done-when box is honestly half-done but its stated remainder ("re-pinning is an operator action") is not the whole gap: writing the operator's instructions was in scope, one of the two docs was updated, and the code comment points at the one that was not. Doc/code drift at exactly the two-agent seam.
- **Required remediation:** `DOGFOOD-SETUP.md` §2 build step gains `LAMBO_GIT_SHA=$(git rev-parse --short HEAD)`; "The server command, everywhere" (and the client blocks, or one inheritance statement) gains `--ledger ~/lambo-dogfood/calls.jsonl --ledger-heartbeat 300`; `src/ledger.rs:101` points at whichever file owns the build step.

### I-R1-5 (P2, non-blocking) — The `v` field is the ledger's only schema promise, and no consumer acts on it: a `v:2` line is analysed as `v:1`, silently

- **Evidence:** the promise is stated three times (`ledger.rs:33-35`, `:57-61`; `_ledger.py:31-33`). `_ledger.load` records the version and never acts on it (`_ledger.py:181-182`), dispatching on `kind` alone; `header` prints `schema v: [1, 2]` as one provenance line. An adversarial `v:2` recall line was consumed in full by every script — no warning, no exit code — while a dropped-line count gets capitals and a torn line gets `UNPARSEABLE`.
- **Impact:** the failure mode is a confidently wrong number. Compounds with absent-vs-zero: a derive line with no facts reads as `created=0, matched=0` (`dedup_rate.py:74-77`), so a hypothetical `v:2` renaming `matched` yields `dedup 0.000` everywhere — the opposite of the truth. Latent (nothing emits `v:2`), hence non-blocking.
- **Required remediation:** `KNOWN_VERSIONS = {1}` in `_ledger.load`; refuse or warn in the dropped-lines register on anything outside it; state the choice in the README. Distinguish absent facts from zero in `dedup_rate.py` and count fact-less derive lines.

### I-R1-6 (P3) — Duplicate vector-leg entries now rank differently than at the parent commit: last-write instead of max-merge

- **Evidence:** parent max-merged within the vector leg; new code assigns (`legs.entry(s.item).or_default().vector = Some(s.score)`, `candidates.rs:219`, `:296`), so a duplicate id's second entry overwrites. Probe: `[(dup, 0.90), (dup, 0.10)]` → new merged 0.1, parent 0.9. Keyword leg was already last-write; recent is idempotent. No shipped adapter returns duplicate ids — but the `vector_candidates` trait contract does not forbid them, and B2's new adapter is being written against it now. Also noted: the legs map is retained on `RecallPipeline` and cloned on every recall-cache interaction (48 bytes/entry vs the parent's dropped 8-byte map) — bounded, almost certainly immaterial, unmeasured, unmentioned.
- **Required remediation:** restore max-merge for the vector leg and add the duplicate-input case to `i1_per_leg_scores_survive_the_max_merge`; or state the distinctness requirement in the trait doc. Mention the retained/cloned map in the Handoff.

### I-R1-7 (P3) — Two acceptance boxes are checked on evidence that does not exist: no duckdb end-to-end, and no real dogfood ledger

- **Evidence:** Done-when 1's "a full dogfood day parses with `duckdb` end to end" — duckdb is never invoked in the kit; the nearest test asserts the property "duckdb's `read_json` actually needs" without shelling out, and the README itself states duckdb *refuses* a torn final line — which the committed sample ends in. Done-when 5's "a real dogfood ledger" — the only committed ledger is the disclosed-fabricated sample. Both boxes `[x]`.
- **Impact:** box-ticking against the wrong artifact. Both boxes were closed by the review itself in an hour (real driven session, real ledger, real store, all five reports + real duckdb run) — the argument they should have been closed rather than checked.
- **Required remediation:** uncheck with reasons, or capture the evidence through the curated path; reword box 1 to the property the test asserts.

### I-R1-8 (P3) — `score_bands.py --floor` changes only a header line, and produces a self-contradictory report

- **Evidence:** `--floor 0.9` prints "recency floor in force: 0.9" then, four lines later, masking rows computed from the hit's own `recent` value: `cosine=0.2914 < floor=0.3500`. The flag threads to exactly one place: the header dict.
- **Required remediation:** delete the flag and print the observed floor, or make a supplied floor a stated override. Reading the floor off the ledger is the right design; say so.

### I-R1-9 (P3) — Two claims in `ledger.rs`'s headline paragraph are not true as written: there *is* a lock on the calling path, and the abandoned-writer path loses lines counted neither written nor dropped

- **Evidence:** "no lock" — `append` takes a `parking_lot::Mutex` across `try_send` (`ledger.rs:236`); bounded, but a lock. "No silent caps" — when `SHUTDOWN_DRAIN` expires, `shutdown` returns without joining (`:262-272`); the writer's in-flight batch (≤1 MiB) plus channel contents are neither `written` nor `dropped`, and the stated bound omits the batch.
- **Required remediation:** count abandoned lines as drops (writer bumps `dropped` on the way out, or `shutdown` drains and counts); state the bound as capacity + one batch; reword to "no lock held across anything that can block".

### I-R1-10 (P3) — The kit states no Python floor, and chrono's nine-digit fractional seconds break `datetime.fromisoformat` before 3.11 — on the Linux half of the rig

- **Evidence:** `parse_ts` is a bare `fromisoformat` (`_ledger.py:75-77`); the producer's `to_rfc3339()` (`SecondsFormat::AutoSi`) emits 0/3/6/**9** fractional digits by clock resolution; pre-3.11 Python accepts only 3 or 6. No `python_requires`, no version check, no README note; the rig runs on Linux where 3.10 system Pythons are still common.
- **Required remediation:** normalise in `parse_ts` (truncate to six digits) or state the floor with a version check; consider pinning `SecondsFormat::Micros` on the Rust side; write down the two quiet dependencies on timestamp shape (`BUCKETS` prefix slicing, string sort).

### I-R1-11 (P3) — `recall_first.py`'s compliance is stickier than the README's list of "definitions that are choices" admits

- **Evidence:** `recalled` is set on the first successful recall in a session and never cleared (`recall_first.py:57-72`), so one recall makes every later write sequence in the session compliant — stronger than the README's "recalls once then derives six concepts complied once" illustration. Also `opened_with_recall` lacks a `succeeded()` check (`:84`), unlike every other predicate. The per-call `derives_without_prior_recall` figure — the one comparable to C5's — is present and labelled.
- **Required remediation:** one sentence in the choices list; add `succeeded()`; note in the docstring which figure compares to `evidence/swarm/`.

### I-R1-12 (P3) — Four smaller surface inaccuracies

- **The recall `query` is written untruncated** (bounded at 16 KiB by `check_size`) while the truncation constant's worst-case reasoning never mentions it; a real 15.4 KiB query produced a 15,752-byte line. Truncate it or document the exemption.
- **Two exit codes for one configuration error** (heartbeat-without-ledger exits 1; heartbeat 0 exits 2), and the zero-guard lives only in `main.rs` — `authorize_ledger` accepts `Some(ZERO)`, giving a library caller a silently-panicked heartbeat task. Move the check into `authorize_ledger`.
- **`reserve`'s "grant/refusal for EVERY exit" comment is false for one exit**: an empty `agent_id` returns from `attribution` before `note_facts` runs — `op=None granted=None` measured. Hoist `note_facts` or narrow the comment.
- **"Off costs nothing" costs one `String` clone per call** (`p.agent_id.clone()` before `observed`). Take `&str` and clone inside the `Some(ledger)` arm, or amend the sentence.

## Positive observations

- **The shutdown ordering is right, verified at the binary.** OS-thread writer so a hung FS cannot starve `Memory::close`; `ledger.shutdown()` strictly after `run_and_close`; heartbeat aborted before the drain; `written` matches the file. The part most expected to be broken, and it held.
- **The H3 wire contract is provably untouched** — serialized keys dumped and compared; `serve-web` not in the diff; `#[serde(skip)]` with a doc comment saying why.
- **"Off means off" is real, including the payload** — byte-identical `lambo_stats`, asserted in both directions by a test that can actually fail.
- **The `demoted` finding is better than the spec it contradicts:** derive performs no demotion; `semantic_merged` added as a separate column with the reason it must not be folded into `matched` (a similarity merge adds a decaying `Semantic` edge, no `Derives` edge). Carried into `dedup_rate.py` and the README's choices list. This is what a Handoff Log is for.
- **Per-leg provenance works on real traffic and the empty case is honest** — `{}` genuinely means traversal expansion; the floor-vs-weak-cosine distinction metric 4 rests on is now recoverable.
- **The reports fail loud on an empty set, confirmed by accident** — the driven session's unflushed writes produced `NO VECTOR-LEG SCORES … that is the finding` instead of a hollow pass.
- **The adversarial ledger did not break the shared reader** — everything absorbed and *counted in the header*; the torn tail became `UNPARSEABLE` with a line number; `restart_times()` reads restarts off `uptime_secs` regressing, labelled as the one boundary that is a fact.
- **`duplicates.py` works against a real store**, decoding the real codec at dim 1024, with the fixture's stand-in disclosed and the `O(n²)` refusal shaped right.
- **`_ledger.py`'s single-reader discipline earns its justification** — one definition each of call/write/session/success ("a metric computed two ways is a metric nobody can quote"); every header leads with the dropped count and says `dropped: UNKNOWN` without heartbeats.
- **G1's bands transcribed digit-for-digit; the git join refuses to conclude** (`CORRELATION ONLY`) — metric 5's honest version, as the spec asked.
- **The HTTP router fix is a real improvement, correctly attributed** as independent of I1.
- **Repo conventions honoured** — no graph lock across an `.await` (legs built inside the sync closure that already held the lock), fmt/diff clean, docs/site mirrors updated together, Handoff Log filled with four numbered surprises.

## Gate results

Run sequentially in one script on `a9ec7f2` with `RUSTFLAGS="-D warnings"`, deliberately not in parallel (the commit message discloses a concurrent-build race).

| Command / check | Result |
|---|---|
| `cargo fmt --all -- --check` | **pass** |
| `cargo clippy --all-targets -- -D warnings` (default) | **pass** |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | **pass** |
| `cargo clippy --all-targets --no-default-features --features store-sqlite -- -D warnings` | **pass** |
| `cargo clippy --all-targets --no-default-features --features store-cockroach -- -D warnings` | **pass** |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | **pass** |
| `cargo clippy --all-targets --features demo -- -D warnings` | **pass** |
| `cargo test --all --features fixtures` | **pass** — lib 789 / 0 / 1; all binaries green |
| `cargo test --features store-sqlite,fixtures` | **pass** — lib 854 / 0 / 1 |
| `cargo test --no-default-features --features store-sqlite` | **pass** — 513 / 0 |
| `cargo test --no-default-features --features store-cockroach` | **pass** — 495 / 0 |
| `cargo test --features ship,fixtures --lib` | **pass** — 880 / 0 / 8 |
| `cargo check --no-default-features` | **pass** |
| `cargo check --features demo` | **pass** |
| `sqlite-vectors` CI row, verbatim | **pass** — 15 / 0 / 0; all three guards exit 0 |
| `scripts/observability/verify.sh` | **pass** — `ALL CHECKS PASSED` |
| Real `serve --ledger --ledger-heartbeat 1`, 11 calls + SIGTERM | **14-line ledger, written=14 dropped=0, exit 0**; `tail durable` precedes `ledger closed` |
| All five reports vs that real ledger + real store | **all ran**; absent vector leg reported as a finding |
| Adversarial ledger (7 defect classes) | **no crash in any of five**; torn tail counted; `v:2` **silently consumed** (I-R1-5) |
| `--ledger-heartbeat 60` without `--ledger` | refused, exit 1 |
| `--ledger --ledger-heartbeat 0` | refused, exit 2 (I-R1-12) |
| `--ledger <fifo>` | **serve wedges** — lease taken, no transport, SIGTERM kills without `tail durable` (I-R1-3) |
| Instrumented channel-full test | **CHANNEL-FULL = 0** of 3072 drops (I-R1-2) |
| `git diff --check e8b4790..a9ec7f2` | clean |
| Verification-only flips | 4 (`candidates.rs`, `server.rs` ×2, `ledger.rs`); all reverted; tree clean at `a9ec7f2` |

## Verdict

**REQUEST_CHANGES** — blocking **I-R1-1**, **I-R1-2**, **I-R1-3**, **I-R1-4**.

The mechanism is good and the hardest parts of it are right: the never-block posture held under everything thrown at it, the shutdown sequence puts the durable tail ahead of observability (confirmed at the binary), the provenance refactor is one implementation with two views and a provably unchanged wire contract, and every number in the commit message is exact. What blocks is four claims the artifacts do not support: a canonical-marker flag that says "rendered" when the budget rendered nothing; a never-block failure table whose headline arm has zero coverage and cannot be covered through the current counter; a `--ledger` path whose blocking `open` wedges startup with the lease held and the SIGTERM handler unarmed; and a replication runbook that produces a sha-less, ledger-less rig while the code comment says it does the opposite. The pattern across all four is the seam the provenance note predicted: the mechanism was built carefully and the claims about it were written from intent rather than from artifacts. Three of the four are doc-matches-code fixes; only I-R1-2 and I-R1-3 need code, and both fixes are small.

I-R1-5 is a non-blocking P2 to fix in the same pass (six lines; the difference between a wrong number and a refusal). I-R1-6 through I-R1-12 are P3s.

Re-review must verify: `canonical_marker` counts only rendered hits or is renamed and documented, with the choice in the README's choices list; the channel-full arm separately counted and covered by a test that genuinely creates backpressure; `Ledger::open` unable to block the startup path, with the table distinguishing "cannot open" from "open blocks"; `DOGFOOD-SETUP.md` carrying `LAMBO_GIT_SHA` and `--ledger`, with `src/ledger.rs`'s pointer corrected; and the two duckdb/real-ledger boxes unchecked with reasons or closed with an exported artifact.

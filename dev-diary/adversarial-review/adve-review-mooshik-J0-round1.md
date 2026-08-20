# Adversarial review — lambo-for-mooshik workstream J, task J0, round 1

- **Reviewer:** `j0_review_r1` (independent — I did not write any of this code; no implementor report and no `rg`-sweep table existed to check against, so every claim below is derived from the artifacts. Source read-only: **zero** verification-only flips were needed, the tree is clean at `0c81419`, and `git diff --check 0bcb275..0c81419` is clean. Nothing committed but this file.)
- **Scope:** commit `0c81419` ("docs(ledger): J0 — I round-3 advisories (I-R3-1..3)") against its parent `0bcb275`. 4 files, +171/−14: `src/ledger.rs` (+22/−9), `dev-diary/lambo-for-mooshik/I-observability.md` (+102), `scripts/observability/_ledger.py` (+37/−2), `scripts/observability/README.md` (+10/−3). Authorities: `dev-diary/adversarial-review/adve-review-mooshik-I-round3.md` — its "New findings (carried as J0…)" section is the checklist, with "Attacks that did not land" and "Positive observations" read for the pattern history — plus `dev-diary/lambo-for-mooshik/J-multi-client.md` §J0, `dev-diary/README.md` "Conventions for agents" (7 especially), `adve-review-mooshik-I-round2.md` (to check one quoted measurement), and `adve-review-mooshik-I-round1.md` §I-R1-3 (to check whether the new docstring's counterfactual is grounded).
- **Verdict:** **CLEAN** — six P3 advisory findings (**J0-R1-1 … J0-R1-6**), none blocking. All three specified remediations are at the artifact; the strong new mechanical claim the brief singled out is **correct**, and I could not break it.

The interesting result is a negative one. The claim this commit adds beyond its specification — that with the handler armed above the call, a blocking `Ledger::open` hangs a lease-holding process *and cannot be closed either, because nothing polls the pinned shutdown future while the main task sits in the syscall* — is the fourth-consecutive-instance candidate the round-3 reviewer warned about, and it is **not** written from intent. It is a real property of the shipped structure, checkable at three call sites, and it survived every attack I could mount on it: eager registration is real (`shutdown_signal` is a sync `fn`), the pinned future has exactly two poll sites and both are inside the transport, the two spawned tasks poll other futures, and `Memory::close` is reachable only downstream of the transport. What I did find, on the same paragraph, is one clause in the wrong grammatical mood (**J0-R1-1**), which is a much smaller thing than the pattern predicted.

The second-most-valuable result is also negative: my independent `rg` sweep found **no remaining stale serve-startup-ordering claim anywhere in the tree**, and — the harder half — **no over-fix**: both siblings the round-3 reviewer deliberately cleared are byte-untouched. The one prose claim I found falsified by the I work is a *key count*, not an ordering claim (**J0-R1-5**), which the sweep as specified would not have caught.

The four remaining findings are all one-or-two-line fixes on the consumer side: the kit's own "these functions depend on string-sort-is-timestamp-sort" register was not extended to the new function that depends on it (**J0-R1-2**), the new `header()` register has no `verify.sh` fixture in a kit that has one for every comparable behaviour (**J0-R1-3**), the README recipe labelled "sanity check before quoting any count" still reads only the drop counter (**J0-R1-4**), and `J-multi-client.md` §J0 still reads as an open instruction list (**J0-R1-6**).

## Method

Read the four authorities in the order the brief gave, then the diff hunk by hunk, then every claim at the source rather than through the commit message.

For the hang claim: read `shutdown_signal`'s body (not its docstring) to establish whether registration is eager or lazy — it is a sync `fn` returning `impl Future`, so the two `signal()` calls run at the call site; then traced the pinned `shutdown` binding to **every** consumer via `rg`, to rule out a concurrent poller; then traced the only paths to `Memory::close`; then read `Ledger::open` → `open_with_sink` to determine whether the hypothetical describes a reachable path, and `adve-review-mooshik-I-round1.md` §I-R1-3 to determine whether it describes a *historical* one.

For the sweep: `rg -n` (not `-rn`; `-r` is `--replace` and mangles output) over `src/ tests/ docs/ site/ scripts/ dev-diary/ README.md` for `SIGTERM`, `shutdown_signal`, `armed`, `arming`, `pre-handshake`, `default disposition`, `(before|after) the (signal|shutdown|transport|handler)`, `parked|parks`, `queued`, `ledger_schema`, `ledger is complete`. Every hit classified in the table below; both reviewer-cleared siblings read in full to confirm they were not "fixed".

For the Python half: probed `queued_lines()` directly in the interpreter against four hand-built heartbeat sets (newest-wins, absent key, absent `ts`, `bool` value); re-derived the committed sample's five heartbeats with an independent parser; predicted which `header()` branch the sample takes, then confirmed it by running a real report; read `verify.sh` end to end to establish what its `--json` cases actually assert; and `rg`ed for every consumer of `ledger_schema` in the tree.

For the numbers: ran the gates myself; re-derived the `lambo_stats` key count from `stats_json` + the `lambo_stats` handler rather than trusting round 3's measurement; and checked the round-2 quotation against `adve-review-mooshik-I-round2.md:60`.

**No verification-only flips were needed.** The one experiment that would have required a build — inserting a blocking FIFO `open` into `Ledger::open` and SIGTERMing a real serve — would only have confirmed a property already settled by three unambiguous call sites, and would have measured a design the commit is arguing *against* shipping. I say so explicitly rather than claim a binary-level proof I did not run: the hang claim below is verified **structurally, at the source**, not at a process.

## Specified remediations: verification at the artifact

| # | Required remediation | Verified how | Verdict |
|---|---|---|---|
| **I-R3-1** | Reword `Ledger::open`'s docstring (~244-262) to `1f86792`'s ordering, keep the probe-placement conclusion | `src/ledger.rs:250-262` now reads "**after** the single-writer lease is taken and — since I-R2-1 — **after** the SIGTERM handler is armed (`shutdown_signal()` is the first statement once `build_memory` returns; this call is the next one)". Checked against `serve.rs`: arming at `:795`, `Ledger::open` at `:806` — **correct**. The conclusion ("That distinction is the whole reason the probe is not here", and the closing "observability taking down memory through the flag that turns observability on") is intact, and the closing sentence was generalised to "Either way", so it now covers both orderings | **Remediated**, and the hazard class re-derived rather than just re-ordered. One clause's mood → **J0-R1-1** |
| I-R3-1, the two cleared siblings | Must **not** be "fixed" | `ledger.rs:432-434` (was `:423-425`; shifted +9 by this commit's own insertion) still past tense: "the same call **wedged** `serve` between the lease and the SIGTERM handler". `I-observability.md:236-244` still past tense: "**was** … therefore **wedged** startup". Both byte-untouched; `git show --stat` confirms only four files moved | **Correctly left alone** |
| I-R3-1, bonus | not required | The FIFO test's docstring (`ledger.rs:798-806`) said such a process "would at least still die flushing" — falsified by the new analysis, and corrected in the same commit, keeping the test's surviving justification rather than deleting the paragraph | **Beyond spec, correct** |
| **I-R3-2** | The kit README's parked-writer recipe must name the right transport | `scripts/observability/README.md:326-332` now separates the two readings explicitly: "`queued` climbing while `written` lags is a writer that is **behind**" vs "A **parked** writer is a different case and the file cannot show it at all … which leaves a **live `lambo_stats` call** as the only place the parked case can be read (I-R3-2)", with the mechanism (heartbeats share the channel) and the measurement (`written=0`, every line dropped at shutdown) | **Remediated**, and complete for the parked/behind distinction — the `docs/` and `site/` mirrors already named the live transport and needed no edit (verified). Two adjacent consumer sites not carried → **J0-R1-4** |
| I-R3-2, optional half | `header()` may mention a non-zero last-heartbeat `queued` | Taken. `_ledger.py:213-230` (`queued_lines()`, newest-not-maximum, with its own blind-spot paragraph), `:381,392-402` (`header()`: a `queued:` line, plus an `elif queued:` that softens the completeness line), `:433` (`emit()`: `ledger_schema.queued_lines`), `:29-30` (module schema comment). Semantics correct — gauge → newest, counter → maximum, the two functions adjacent and deliberately different | **Remediated**. No gate coverage → **J0-R1-3**; undocumented sort dependency → **J0-R1-2** |
| **I-R3-3** | One Handoff Log entry in `I-observability.md` for `1f86792`'s two behavioural changes, folding in the arithmetic deviation's reasoning | Two entries (`I-observability.md:285-357` for the round-2 remediation, `:359-385` for the J0 closure). The arming move carries the option-2 deferral *and* the eager-registration reason it is non-obvious; the key carries the `channel_full`-never-`accepted` argument, the "both derivations share one subtraction" property, the name of the test the prescribed formula fails with the exact `left: 961, right: 1025`, and a pointer to round 3's flip D so it is not re-derived. Convention 7's "what the next agent should not re-derive", literally | **Remediated**. Every checkable number in it is exact (see gate table) |
| **Guidance** | An `rg` sweep for serve-startup-ordering claims, not a read of the neighbourhood | Cannot be verified from the commit (no implementor report survived), so I ran it independently. **Result: nothing stale left, and nothing over-fixed** — see the sweep table. Whatever the implementor did, the outcome matches what the sweep would have produced for the ordering claim. It did **not** extend to the adjacent key-count claim → **J0-R1-5** |
| **Scope** | J0 is doc-precision; nothing behavioural beyond the sanctioned `header()` half | `src/` diff is **comment-only** — verified: the two `src/ledger.rs` hunks touch only `///` lines, no executable line moved, and the test count is unchanged at 793/0/1. The only behaviour change is in `_ledger.py`, which is the sanctioned half | **Clean** |
| **Missing** | Should `J-multi-client.md` record J0 as done? | It does not → **J0-R1-6**. No Done-when box was needed (all fourteen belong to J1–J5, verified), and `dev-diary/lambo-for-mooshik/README.md`'s index has no status column, so nothing else needed a touch | **Gap** |

## Sweep results — serve-startup-ordering claims

Patterns: `SIGTERM`, `shutdown_signal`, `armed`, `arming`, `pre-handshake`, `default
disposition`, `before the (signal|shutdown|transport|handler)`, `after the …`, `parked|parks`,
`queued`, over `src/ tests/ docs/ site/ scripts/ dev-diary/ README.md`.

Serve-startup-ordering claim sites, classified:

| Site | Claim | Class |
|---|---|---|
| `src/ledger.rs:250-262` | armed **after** lease, **before** this call | fixed by this commit |
| `src/ledger.rs:432-434` | "On the runtime's main task the same call **wedged** `serve` between the lease and the SIGTERM handler" | past tense; correct history; deliberately untouched (the reviewer-cleared sibling) |
| `src/ledger.rs:798-806` (FIFO test) | armed *before* this call | fixed by this commit |
| `src/mcp/serve.rs:749-753` | "armed **before** the transport handoff" | true, and now understated (it is armed far earlier) — not stale |
| `src/mcp/serve.rs:766-793` | the I-R2-1 invariant comment | accurate |
| `src/mcp/serve.rs:1258-1274` | `shutdown_signal` docstring | accurate |
| `src/memory.rs:2881` | "`shutdown_signal()` installs process-wide handlers" | accurate, no ordering claim |
| `tests/serve_pre_handshake_durability.rs:9-19,146-150` | arming before the transport handoff; loose matcher rationale | accurate |
| `dev-diary/.../I-observability.md:236-244` | past-tense round-1 narrative | correct history; deliberately untouched (2nd cleared sibling) |
| `dev-diary/.../I-observability.md:293-311` | new Handoff entry | see findings |
| `docs/reference/{cli,mcp}.mdx`, `site/src/content/docs/{cli,mcp}.mdx` | `ledger_queued_lines` described as a **live `lambo_stats`** key ("Read it when the drop counters are all zero but the file is not growing") | correct transport already; no parked-writer file-reading claim; nothing stale |

**No stale serve-startup-ordering claim remains anywhere in the tree.** The two cleared
siblings are untouched, as the commit message claims (verified by `git show 0c81419 --stat`
touching only 4 files, and by reading both sites).

## The Python half (`scripts/observability/_ledger.py`)

**Q1 — truthiness vs `is not None` on a real `0`.** Behaves as documented, and the
`None`/`0` distinction is preserved exactly where it matters. Probed directly:

```
newest wins -> 0        # two heartbeats, older queued=7, newer queued=0
absent key  -> None
```

`header()`'s `if queued:` / `elif queued:` collapse `None` and `0` to the same
(no-op) outcome, which is what both the docstring ("when the last heartbeat
reported one") and the README ("prints a **non-zero** `queued`") promise — a `0`
must print nothing, and it prints nothing. The distinction that does matter is
kept in `emit()`: `"queued_lines": ledger.queued_lines()` serialises `null` for
"no heartbeat carried the key" and `0` for "the writer was empty", so a duckdb
consumer can tell "unknown" from "clean". Correctly split: truthiness for the
human register, identity for the machine one. **No finding.**

**Q2 — the `ts` sort.** `ts` is **not** guaranteed by the parser. `load()`
classifies on `kind` alone and never requires `ts`, so a heartbeat without it
raises `KeyError: 'ts'` inside `queued_lines()` — confirmed:

```
NO ts  ->  KeyError 'ts'
```

This is *pre-existing* exposure, not new: `sorted_calls()` (`:175`) and
`restart_times()` (`:190`) index `r["ts"]` the same way, and `header()` already
calls `restart_times()`, so a `ts`-less heartbeat already killed every report
before this commit. Not a finding.

What **is** new is the lexical-ordering dependency — see **J0-R1-2** below.
`dropped_lines()` takes a `max()` over values and so is offset-insensitive;
`queued_lines()` is the first *completeness* claim in the kit whose answer depends
on which stamp sorts last.

`isinstance(value, int)` accepts `True` (`bool` ⊂ `int`), so a producer emitting
`"ledger_queued_lines": true` yields `queued = True` and a header line reading
`queued:   True LINE(S) STILL QUEUED`. Identical to the pre-existing
`dropped_lines()` pattern, cosmetic, unreachable from the Rust producer
(`json!(ledger.counters().queued())`, a `usize`). Not a finding.

**Q3 — does `elif queued:` change any committed-sample or `verify.sh` output? No.**
Predicted, then confirmed. Re-derived the sample independently:

| line | ts | `ledger_queued_lines` | `ledger_dropped_lines` |
|---|---|---|---|
| 1 | 2026-08-18T09:00:00+00:00 | 0 | 0 |
| 14 | 2026-08-18T09:25:00+00:00 | 0 | 0 |
| 17 | 2026-08-18T11:00:00+00:00 | 0 | 0 |
| 18 | 2026-08-19T09:25:00+00:00 | 0 | 0 |
| 25 | 2026-08-19T09:45:00+00:00 | 0 | **4** |

Five heartbeats, all `ledger_queued_lines: 0` — the commit message's claim is
**exact**. The sample cannot reach either new branch twice over: `dropped_lines()`
is `max(...) = 4`, so `elif dropped:` fires first and `elif queued:` is
unreachable; and `queued = 0` is falsy, so the `queued:` line is suppressed.
Confirmed at the artifact — `recall_first.py sample/calls.jsonl` still prints
`dropped:  4 LINES DROPPED …` with no `queued:` line, and `verify.sh` is
`ALL CHECKS PASSED` including the `make_sample.py` drift diff.

**Q4 — can `emit()`'s new key break a consumer? No.** The module docstring's
forward-compatibility claim is about *ledger lines*, not report JSON, but the
report side is safe on inspection: the only strict read of that object anywhere in
the tree is `verify.sh:122`, `d["ledger_schema"]["unknown_version_lines"]`, which
is a lookup of a different key on a dict that gained one. The README's duckdb
recipes all read `calls.jsonl` (`read_json_auto`), never a report's `--json`. The
five `--json` gate cases assert only `json.load(...)` succeeds. Additive key on an
object with no schema pin and no positional consumer. **No finding.**

**Q5 — no test for `queued_lines()`, and `verify.sh` cannot catch a semantic
regression in it.** See **J0-R1-3** below. A *crash* regression is caught
transitively (every report's `header()` calls it, so all five `--json` cases and
every text case go red). A *semantic* regression is not caught by anything: every
committed heartbeat carries `0`, `make_sample.py` hardcodes `"ledger_queued_lines": 0`,
and `verify.sh` has no non-zero-queued fixture — so swapping `reverse=True` for
`reverse=False`, or `sorted(...)[0]` for the newest, or `max()` for last, all stay
green. Both new `header()` branches and the whole `queued:` line are dead code with
respect to the gate.

## New findings

### J0-R1-1 (P3) — `Ledger::open`'s new paragraph states a counterfactual hazard in the indicative

- **Evidence:** `src/ledger.rs:255-259`: *"What that ordering **leaves** is an **availability** hazard rather than the durability one it used to be: a blocking `open` here **would** wedge `serve` …"* The first clause is present indicative and reads as a residual hazard in the shipped code. It is not one: `Ledger::open` → `open_with_sink` (`ledger.rs:272-285`) makes **no file syscall on the caller's thread at all** — `sync_channel` + `thread::Builder::spawn(writer_loop)` — and the paragraph's own first sentence says so ("performs no I/O of its own"). The ordering leaves *no* hazard here; what it changes is the outcome of a design (the probe inside `open`) that I-R1-3 removed. Every other clause in the paragraph is correctly subjunctive.
- **Impact:** doc-only, and mild — the sentence sits two lines below "performs no I/O of its own", so a careful reader reconciles it. But it is the same defect class the previous three rounds kept finding: a hazard stated one notch stronger than the artifact supports. The paragraph's job is to stop the probe coming back; it does that job better if the reader cannot mistake the hazard for a live one.
- **Remediation (J0/J-next):** one clause. E.g. *"What that ordering would change, were the probe ever moved back here, is the hazard class: availability rather than the durability one it used to be — a blocking `open` here would wedge …"* Nothing else in the paragraph needs to move.

### J0-R1-2 (P3) — `queued_lines()` joins the kit's documented "string sort is timestamp sort" dependency list without being added to it

- **Evidence:** `_ledger.py:41-45` names the members explicitly: *"**String sort is timestamp sort** (`Ledger.sorted_calls`, `restart_times`, `binaries`). True only while every stamp is the same fixed-offset RFC3339 form … A producer that switched to `Z`, to local offsets, or to a variable number of fractional digits would reorder lines lexically without erroring."* `queued_lines()` (`:227`) does `sorted(self.heartbeats, key=lambda r: r["ts"], reverse=True)` and is **not** in that list. The register exists precisely because these dependencies are "invisible at their use site and both break silently" — and this one is newly load-bearing: `dropped_lines()` uses `max()` over *values*, so it never cared which stamp was newest, whereas `queued_lines()` is the kit's first **completeness** verdict decided by lexical stamp order. `load()` accepts multiple paths, so a mixed-offset multi-file read is the realistic trigger.
- **Impact:** small but real. A wrong "newest" heartbeat makes `header()` print `dropped: 0 — the ledger is complete` over a known backlog, which is the exact failure the optional half of I-R3-2 was added to prevent — silently, with no error.
- **Remediation:** add `queued_lines` to the parenthetical at `_ledger.py:41`. (Two words. Optionally also `restart_times`-style: key on `parse_ts(r["ts"])` instead of the raw string, which would make the dependency real rather than documented — but that is a behaviour change and belongs with the other three call sites, not alone.)

### J0-R1-3 (P3) — the new `header()` register has no gate coverage, in a kit whose gate covers every comparable behaviour

- **Evidence:** `verify.sh` has purpose-built fixtures for each of the kit's other header-register behaviours — a mixed-`v` file for the unknown-version warning (`:107-129`), a fact-less derive, a nine-digit-fractional-seconds file (`:132-141`). It has **none** for a non-zero `queued`. Every committed heartbeat carries `ledger_queued_lines: 0` (re-derived above) and `make_sample.py` hardcodes `"ledger_queued_lines": 0`, so `header()`'s new `elif queued:` branch and its `queued:` line never execute under the gate, and `queued_lines()`'s "newest, not maximum" semantics are unpinned. Verified by inspection of all five `--json` cases: they assert only that `json.load` succeeds.
- **Impact:** the commit's own thesis is that a backlog is the second way this file can be an undercount. That claim now has strictly less regression protection than the drop claim it sits beside. `reverse=True` → `reverse=False`, or last → `max()`, both stay green.
- **Remediation:** one `verify.sh` block, ~10 lines, in the style of the existing mixed-`v` case: a two-heartbeat fixture whose **older** beat carries `ledger_queued_lines: 9` and whose **newer** carries `3`, with `ledger_dropped_lines: 0` on both; assert the text header contains `queued:   3` (proving newest-not-maximum in one assertion) and `see \`queued\``, and that `--json`'s `ledger_schema.queued_lines == 3`.

### J0-R1-4 (P3) — the README's "sanity check before quoting any count" recipe still reads only the drop counter

- **Evidence:** `scripts/observability/README.md:245`: *"# Every dropped-line reading, as a sanity check before quoting any count."* followed by `jq -r 'select(.kind=="stats") | [.ts, .git_sha, .stats.ledger_dropped_lines] | @tsv' calls.jsonl`. Eighty lines below, the same commit establishes that a non-zero `queued` also makes every count a lower bound, and adds a header line saying so. The recipe named for exactly that job — the pre-quote sanity check — was not extended, while the *other* duckdb recipe on the page (`:233-236`, graph growth) already lists `stats.ledger_queued_lines`.
- **Impact:** small, consumer-side. An operator following the recipe the page labels as the completeness check gets the pre-commit answer.
- **Remediation:** add `.stats.ledger_queued_lines` to the `jq` array at `README.md:245` and widen the comment to "every dropped-line and queue-depth reading".

### J0-R1-5 (P3) — `lambo_stats`'s own comment still says "the three `ledger_*` keys"; there are six

- **Evidence:** `src/mcp/server.rs:1626-1630`, the comment directly above the payload build: *"With `--ledger` off this is exactly the payload it always was; with it on, **the three** `ledger_*` keys are appended (I1: …)"*. The builder it introduces inserts **six** (`server.rs:805-834`): `ledger_path`, `ledger_written_lines`, `ledger_dropped_lines`, `ledger_dropped_channel_full`, `ledger_dropped_write_failed`, `ledger_queued_lines`. I-R2-2/I-R2-3 took the count from 3 → 5 → 6 and the comment never moved. Everything else in the tree already says six (`docs/reference/mcp.mdx:210`, `site/src/content/docs/mcp.mdx:212`: "all six keys are absent"), so this is the last site with the old number.
- **Impact:** doc-only and small, but it is the *same class* as I-R3-1 — prose in this module falsified by a later change to the thing it describes — and it is inside `src/`, three lines above the code it miscounts. It was reachable by the `rg` sweep the guidance asked for, had the sweep covered the key-set claim as well as the ordering claim.
- **Remediation:** change "the three `ledger_*` keys" to "the six `ledger_*` keys" at `src/mcp/server.rs:1628`. Nothing else.


### J0-R1-6 (P3) — `J-multi-client.md` §J0 still reads as an open instruction list

- **Evidence:** convention 7. `J-multi-client.md:26` still carries the bare row `| J0 | Carryover from workstream I, round 3 (CLEAN) | nothing |`, and `:38-68` still reads in the imperative — "**Reword** to the current ordering", "**One clause.** Optionally: `header()` mentions a non-zero last-heartbeat `queued`", "`I-observability.md`'s Handoff Log **lacks** an entry". All three are now done. The closure *is* recorded — but in `I-observability.md:359-385`, which is the right home for I's remediation history and the wrong home for J's task board. Nothing in the J doc says J0 landed, and there is no J Handoff Log to carry it.
- **Impact:** the next agent opening `J-multi-client.md` to claim J1 reads three undone items above it. Low risk of real rework (the artifacts are obviously already changed), but it is the exact failure convention 7 exists to prevent — "an undocumented finish is an unfinished task" — and it is the *only* specified-scope item this commit did not touch.
- **Remediation:** two edits in `J-multi-client.md`. (1) `:26` → `| J0 | Carryover from workstream I, round 3 (CLEAN) | nothing | **DONE `0c81419`** |` (or a `— DONE` suffix in the Task cell, since the table has three columns). (2) A one-line status under the §J0 heading, in the form the other docs use: **Status: done, `0c81419`**, linking this review, and a half-sentence saying the three numbered items below are kept as the spec rather than as open work. **Keep the "Guidance handed forward" paragraph unchanged** — "J2 moves this ordering again; apply the sweep then" is still live, and J2 has not run. No Done-when box is needed: all fourteen belong to J1–J5.


## Attacks that did not land

- **The headline mechanical claim — "the pinned shutdown future is never polled while the main task sits in the blocking syscall, so `Memory::close` never runs" — holds structurally, and I could not break it.** Traced at the source: `shutdown_signal()` (`serve.rs:1275`) is a **sync** `fn`, so `signal(SignalKind::interrupt())` and `signal(SignalKind::terminate())` run at the call site (`serve.rs:795`) and only `recv()` is inside the returned `async move` — registration is genuinely eager, so the "handler is installed, SIGTERM no longer takes the default disposition" half is true at `Ledger::open` (`serve.rs:806`). The future is `tokio::pin!`ed in `serve()` and handed out by `as_mut()` at exactly two places, `serve_stdio` / `serve_http` inside the `transport` async block (`serve.rs:850-853`), first polled at `serve.rs:855`. Everything between the arming and there — `Ledger::open`, `LamboServer::{new,with_ledger}`, the heartbeat `tokio::spawn`, `mem.events()`, the event-pump `tokio::spawn`, the attach log — is straight-line non-`await` work on the same task, so **no concurrent poller exists**: the two spawned tasks poll `heartbeat_loop` and `log_events`, neither of which touches `shutdown`, and the only other `shutdown_signal()` call site is `close_bounded` (`serve.rs:939`), downstream of the transport and unreachable while `open` blocks. `Memory::close` is reached only through `run_and_close` → `close_bounded` (`serve.rs:855-856, 894-895`), i.e. only after the transport future returns. There is no `Drop` path to `close`. So a blocking call on that task hangs the process with the lease held and no close — as claimed, including "until something kills it harder".
- **Whether the hazard the docstring describes is reachable at all.** It is not, and this is the one clause worth grading (**J0-R1-1**) — but the counterfactual itself is legitimate and well-grounded, not invented: the probe *was* a synchronous unbounded `open_for_append` on the runtime's main task (I-R1-3; `I-observability.md:236-244` records it), and the paragraph exists to say why it must not go back. The reviewer-cleared sibling at `ledger.rs:432-434` states the same thing in the past tense for the same reason. So the finding is the indicative *mood* of one bridging clause, not the argument.
- **The two deliberately-not-flagged siblings.** Both untouched, as claimed: `ledger.rs:432-434` (was `:423-425`, shifted by the +9 lines this commit added above it) still reads "On the runtime's main task the same call **wedged** `serve` between the lease and the SIGTERM handler", and `I-observability.md:236-244` still reads "**was** … therefore **wedged** startup". `git show --stat` touches four files, neither of which could have hidden an edit to those lines. **No over-fix.**
- **A stale serve-startup-ordering claim the commit missed.** None exists. The classified sweep table above enumerates every site; the two nearest candidates (`serve.rs:749`, `tests/serve_pre_handshake_durability.rs:9-19`) are both still true — `serve.rs:749`'s "armed **before** the transport handoff" is now merely *understated*, not wrong. The `docs/` and `site/` mirrors describe `ledger_queued_lines` as a **live `lambo_stats`** key ("Read it when the drop counters are all zero but the file is not growing"), i.e. they already named the right transport and needed no I-R3-2 edit. The one stale prose claim I did find in the same family is a *key-count*, not an ordering claim (**J0-R1-5**).
- **`elif queued:` changing a committed report.** Cannot: `dropped_lines()` is `max(...) = 4` on the sample, so `elif dropped:` fires first, and `queued = 0` is falsy anyway. Confirmed by running `recall_first.py` on the sample and by `verify.sh`'s byte-exact `make_sample.py` drift diff.
- **`emit()`'s new key breaking a consumer.** Only strict read of `ledger_schema` in the tree is `verify.sh:122` on a different key; the duckdb recipes read the ledger file, not report JSON; the five `--json` gate cases assert only that the document parses.
- **`is not None` vs truthiness on a real `0`.** Correct as written — truthiness for the human register (a `0` must print nothing, and does), identity preserved in `emit()` (`null` vs `0`), so the "unknown" / "clean" distinction survives for machines.
- **`ts` absent or non-comparable in `queued_lines()`.** Raises `KeyError`, but this is pre-existing kit-wide exposure (`sorted_calls` `:175`, `restart_times` `:190`, both already called from `header()`), not new. Graded only the *documentation* gap it creates (**J0-R1-2**).
- **`isinstance(value, int)` accepting `True`.** Real (`bool` ⊂ `int`), identical to `dropped_lines()`, and unreachable from the Rust producer, which emits `json!(usize)`.
- **The round-2 citation in the Handoff Log.** Quoted as "measured an 8 s simulated startup take a signal at 2 s and run the remaining 6 s before acting on it". `adve-review-mooshik-I-round2.md:60`: *"measured with an 8 s simulated startup, a SIGTERM at 2 s ran the remaining 6 s before being taken."* **Exact.**
- **"17 keys either side."** Re-derived from source rather than taken on trust: `stats_json` builds 15 base keys (`server.rs:773-790`) and the `lambo_stats` handler adds `summary` + `warnings` (`server.rs:1634-1635`) = **17** with the ledger off; the `if let Some(ledger)` arm adds 6 = **23** on, and 5 = **22** at the parent. Round 3's "17 / 17" and "22 → 23" are consistent with the source to the key.

## Positive observations

- **The commit did the `rg` sweep's job, not just the finding's.** The specification named one stale site; the interesting question was whether a fourth existed. My independent sweep across `src/ tests/ docs/ site/ scripts/ dev-diary/` found no remaining stale serve-startup-ordering claim, and — the harder test — found **no over-fix**: both siblings the round-3 reviewer deliberately cleared are byte-untouched. Getting the boundary right in both directions is the part that usually fails.
- **The new mechanical claim is stronger than it had to be and it is correct.** The specification only asked for the ordering to be reworded. The commit instead re-derived the hazard *class* (availability, not durability) and then went one step further to the non-obvious consequence — that arming the handler does not rescue a process whose task is blocked, because nothing polls the pinned future. That is a real property of the shipped structure, verifiable at three call sites, and it is exactly the kind of claim the previous three rounds found written from intent. This one is not.
- **The FIFO test's docstring was corrected in the same breath rather than left to contradict the module.** It previously said such a process "would at least still die flushing" — which the new analysis falsifies — and the fix keeps the test's surviving justification ("a `serve` that hangs before it serves is a failure however its eventual death arrives") instead of deleting the paragraph.
- **The optional half of I-R3-2 was taken, and taken with the right semantics.** Queue depth is a gauge, so `queued_lines()` reads the **newest** heartbeat while `dropped_lines()` reads the **maximum** — the two functions sit adjacent in the file and differ deliberately, with the reason in the docstring. Getting gauge-vs-counter right here is the difference between a useful line and a misleading one.
- **The new header line is honest about what it does *not* cover.** `queued_lines()`'s docstring names its own blind spot in the same paragraph as its purpose: a fully parked writer emits no heartbeats, so this function cannot see the case that motivated the key. A reader cannot mistake the file reading for the live one.
- **`None` / `0` are kept distinct on the machine path** while collapsed on the human path — deliberate, and the right way round.
- **The Handoff Log entries are the ones worth writing.** Both record *why* rather than *what*: the option-2 deferral carries the eager-registration reasoning that makes it non-obvious, and the arithmetic entry carries the `channel_full`-never-`accepted` argument plus the name of the test the prescribed formula fails. Convention 7's "what the next agent should not re-derive", literally satisfied.
- **Every number in the commit message re-derived exact** — 793/0/1, five sample heartbeats all `ledger_queued_lines: 0`, byte-identical sample reports, `verify.sh` green, and the round-2 8 s/2 s citation verbatim.
## Gate results

Run on `0c81419` in the `wt/j0` worktree with `CARGO_TARGET_DIR=/Users/narayan/Documents/work/lambo/target`.

| Command / check | Result |
|---|---|
| `cargo fmt --all -- --check` | **pass** |
| `cargo clippy --all-targets --features fixtures -- -D warnings` | **pass**, no warnings |
| `cargo test --all --features fixtures` | **pass** — lib **793 passed / 0 failed / 1 ignored**, every binary green. Commit message **exact** |
| `bash scripts/observability/verify.sh` | **pass** — `ALL CHECKS PASSED`, including the `make_sample.py` byte-drift diff and all five `--json` cases |
| `recall_first.py sample/calls.jsonl` header | `dropped:  4 LINES DROPPED …`, **no `queued:` line** — the new branches are not reached, so sample reports are byte-identical, as claimed |
| Sample heartbeats, re-derived independently | **5**, all `ledger_queued_lines: 0`; `dropped_lines()` max = 4. Commit message **exact** |
| "17 keys either side", re-derived from source | `stats_json` 15 base keys + `summary` + `warnings` = **17** off; `+6` ledger keys = **23** on (**22** at the parent, 5 keys). Round 3's figures consistent to the key |
| Round-2 citation "8 s simulated startup, signal at 2 s, remaining 6 s" | `adve-review-mooshik-I-round2.md:60` — **verbatim** |
| `src/` diff is comment-only | **confirmed mechanically** — filtering the `src/ledger.rs` hunks to non-`///` changed lines yields **zero** lines |
| `git diff --check 0bcb275..0c81419` | clean |
| `git show --numstat 0c81419` | 4 files: 102/0, 10/3, 37/2, 22/9 |
| Verification-only flips | **0**; tree clean throughout; nothing but this review file committed |
| `queued_lines()` probes (newest-wins / absent key / absent `ts` / `bool`) | `0` / `None` / `KeyError 'ts'` / `True` — recorded above |

## Verdict

**CLEAN** — advisory findings **J0-R1-1 … J0-R1-6**, all P3, none blocking.

All three specified remediations are at the artifact, the boundary the round-3 reviewer drew was respected in **both** directions, and the scope discipline holds — the `src/` change is provably comment-only and the only behavioural change is the sanctioned `header()` half.

On the thing the brief asked me to attack hardest: the new mechanical claim is right, and it is right for a reason that is not visible from the commit message. It rests on three independent facts, each of which had to hold: `shutdown_signal` is a sync `fn`, so registration is genuinely eager at `serve.rs:795` and SIGTERM is genuinely no longer at its default disposition by the time `Ledger::open` runs; the pinned future is handed to exactly two poll sites, both inside the transport future at `serve.rs:850-853`, with no spawned task and no `select!` in between; and `Memory::close` is reachable only through `run_and_close` → `close_bounded`, downstream of that transport. Remove any one and the claim fails. All three hold. The commit went past its specification to derive a consequence — that arming a handler does not rescue a task that never polls — that is genuinely non-obvious, and it got it right. Against a pattern history of three consecutive rounds finding load-bearing claims written from intent, that is the result worth recording.

What I found instead is six one-or-two-line precision gaps, four of them on the consumer side of the sanctioned `header()` change and none of them capable of misleading anyone about behaviour. The two that would most repay fixing are **J0-R1-3** (the new completeness register has strictly less regression protection than the drop register it sits beside, in a kit whose gate covers every comparable behaviour) and **J0-R1-6** (J0's own workstream doc does not record it as done, which is the only specified-scope item untouched). **J0-R1-2** and **J0-R1-5** are the same defect class as I-R3-1 — a written-down register that a later change did not get added to — and both are two-word fixes; carrying them into J1/J2 rather than a J0 round 2 is the same call the orchestrator made for I's advisories, and I would make it again.

One thing to hand forward rather than grade. The round-3 guidance said the cheap defence is an `rg` sweep for *the ordering claim*. My sweep for that claim came back empty — but the same sweep widened by one term (the `ledger_*` **key set**) immediately found `src/mcp/server.rs:1628` still saying "the three `ledger_*` keys" over a builder that inserts six. The lesson is not "sweep for the ordering claim" but "sweep for whatever the change is a claim *about*" — and for J2, which moves both the startup ordering **and** the lease/endpoint schema, that means two sweeps, not one.

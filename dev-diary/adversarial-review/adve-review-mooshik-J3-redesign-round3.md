# Adversarial review — Mooshik J3 durability redesign, round 3

**Reviewer:** independent adversarial reviewer, agent id `J3Round3Reviewer`. Wrote none of the
code or docs under review; REVIEW-ONLY (no implementation, no fix, no compare-and-adopt).

**Scope:** the six remediation commits `160858b..9b4c456` on `wt/j3` (pushed to `origin/wt/j3`, HEAD
`9b4c456` — R-1 rule table, R-1 bound+R-2+R-4+R-5+R-8, R-3 column preflight, R-5/R-7 source docs,
R-2/R-6/R-7 surfaces + dispositions, R-9 evidence driver), against the round-2 checklist (1 P1 / 3 P2 /
5 P3) and the prescribed design in `J3-durability-redesign.md` §"Prescribed design for J3-R2R-1".

**Verdict:** **APPROVE.** All nine round-2 findings are closed at source, with every priority
verification the operator named satisfied: the suggested sequential-decision termination measure is
genuinely implemented with a stated threshold and error posture (not a bare constant); the rule
table has no wildcard to `Backend` and decides the class at the adapter; the R-3 column preflight is
verified against the live Cockroach cluster; R-8's `write_queue_replay_blocked` is set in the
liveness return and both breaking arms. Two new P3 findings (below) are documentation-only and do
not block integration. Gate reconciliation: the sqlite gate is **1006/0/3, not 996/3** — the
remediation's reported `serve_proxy_multi_client` trio does **not** reproduce on this machine; it
passes in isolation and in the full suite. clippy ×4, fmt, and the Cockroach gates are clean.

---

## Method

Read-only at source. All checks done in the main checkout at `9b4c456` (this review's assigned
environment); a detached read-only worktree at `bc28ac8` was used solely to enumerate the round-2
fixtures test list for the by-name delta, then removed. Nothing in the repo was modified.

1. **Checklist at source, not trust.** Every one of the nine findings was re-derived at the file:line
   the remediation names (and at the product's own path, not its test), at HEAD.
2. **The algorithm question adjudicated by reading the loop**, not the commit message or the doc's
   as-built claims — the operator's explicit concern.
3. **Gates re-run, not taken at face value:** full sqlite gate, full fixtures gate, fmt, clippy ×4,
   the narrow and broad `store-cockroach` gates, and the live `--ignored` Cockroach runs against the
   real cluster (DSN sourced from `.env`, `LAMBO_LIVE=1`, never printed). The `serve_proxy_multi_client`
   trio was exercised in isolation and within the full suite.
4. **verify.sh probed for a repos-free pass**, since the remediation declared it blocked on Python 3.14.

Verification-only edits: none. `git status` shows only the pre-existing untracked `local:/` file.

---

## The nine round-2 findings (J3-R2R-1 .. R-9)

| # | Grade | Verdict | Evidence at `9b4c456` |
|---|---|---|---|
| J3-R2R-1 | P1 | **CLOSED** | Rule table `src/embed/bge_m3.rs:48-80` (`EmbedStatusClass` + `classify_status`), applied at the adapter that knows the status `:202-250`; `is_transient` reads the variant only, `src/embed/mod.rs:77-82` (no re-derivation from a message). 500/502/503/504/429/408/425/509/529 + un-named 5xx + unclassified → `Unavailable` (leave durable); content 400/413/415/422 → `Backend`; permanent-config 401/403/404 → `Backend` + loud warn. Sequential decision rule `src/writeq.rs:180-214` (`EMBEDDER_SICK_THRESHOLD = 3`) implemented in the replay loop `:3066-3180` — see Algorithm box. Self-bounding sentence restated at its true magnitude `src/embed/mod.rs:62-76`. Tests: `the_status_rule_table_classifies_every_named_status` `:387-407`, `status_class_drives_the_error_variant` `:425-446`, `rejects_500_as_transient` `:371-379`. |
| J3-R2R-2 | P2 | **CLOSED** | In-session worker leaves `LamboError::EmbedUnavailable` intents unconsumed (consumes only non-`EmbedUnavailable` failures), `src/writeq.rs:2582-2609`; test `a_write_reached_during_an_in_session_embedder_outage_is_not_consumed` `src/memory.rs:4894`; both `mcp.mdx` mirrors restate the honest asymmetry `docs/reference/mcp.mdx:169`, `site/src/content/docs/mcp.mdx:171`; Done-when (6) corrected `dev-diary/lambo-for-mooshik/J-multi-client.md:2329,2712`; the remaining asymmetry declared as a deviation with the J4 seam `J3-durability-redesign.md:534-545`. |
| J3-R2R-3 | P2 | **CLOSED (live)** | `columns_in_ddl` `src/store/mod.rs:133-191` + `unprovisioned_column_err` `:197-211`; both adapters diff columns (`sqlite.rs:726-747` via `pragma_table_info`, `cockroach.rs:1999-2020` via `information_schema.columns`). **Verified against the live cluster** — `column_preflight_refuses_a_missing_column_live` `src/store/cockroach.rs:3750-3784` passed (see Cockroach gate below). |
| J3-R2R-4 | P2 | **CLOSED** | `ReceiptAnswer::ordinal` with no `_` arm `writeq.rs:1005-1020`, asserted eleven distinct `:3665-3679`; `PendingReplay` in the array `:3641-3664`; test renamed `only_the_two_pendings_are_unsettled` with `!PendingReplay.is_settled()` `:3703-3725`; `is_settled` `:998-999`. |
| J3-R2R-5 | P3 | **CLOSED** | `RECEIPT_WAIT_MAX` restated on the surviving reason, "projected" dropped `writeq.rs:670-685`; the three `PendingReplay`-falsified sentences fixed (`:895-908` docstring "Two of the eleven are unsettled", `:2656` lookup "`pending_replay` while its replay is owed", `:2683-2685` wait "either is honest"); `consumed_at` purge restated as lazy `types/mod.rs:552-572`; `match_strategy` setter names all three consequences `memory.rs:550`. |
| J3-R2R-6 | P3 | **CLOSED (as scoped)** | Measured table dropped, shape kept, grounded in round-2 re-measurement (refusal 2048–3072 B not 1536 B): `writeq.rs:486-504, 516-521`. The live BGE-M3 at `127.0.0.1:8080` is **not** running (checked: `/health` empty), so "re-measure-and-stamp" was genuinely unavailable — the allowed drop-numbers option was the only honest one. Not a PARTIAL-if-live. |
| J3-R2R-7 | P3 | **CLOSED** | `MatchStrategy` authority names all three consequences `types/mod.rs:200-232`; `config.rs:132-133` says "all three"; both `api.mdx` mirrors updated `docs/reference/api.mdx:74`, `site/src/content/docs/api.mdx:76`. |
| J3-R2R-8 | P3 | **CLOSED** | `ReplayBlockReason` `writeq.rs:1314-1321`, counter `:1419-1430`; set in the liveness-gate return `:3045-3047`, the embedder-sick break `:3104-3106`, and the non-embedder break `:3165-3166`; exposed as `write_queue_replay_blocked` (`null`/`"embedder"`/`"other"`) `mcp/server.rs:1136-1143`; test `replay_blocked_names_the_reason_or_is_none` `:3630-3636`. |
| J3-R2R-9 | P3 | **CLOSED** | `j3_n1_outage_demo.py` iterates **every** receipt partitioned by state in session 2 (`pending_replay==backlog`, `applied_after_restart==drained`, `failed==0`) `:175-197` and session 3 (all `applied_after_restart`) `:211-215` — deterministic. |

---

## The algorithm question (the operator's explicit concern) — **YES, implemented**

The migration from a hard-coded `break after k` to the sequential decision rule is real, at
`src/writeq.rs`, not a bare unexplained constant:

| Criterion | Implemented? | Evidence (`src/writeq.rs`) |
|---|---|---|
| (a) Content rejection is the **absorbing** consume-and-break case, **never** counted as embedder-sickness evidence | **YES** | `Err(LamboError::Embed(_))` arm `:3138-3160` consumes the intent as `failed`, `transient_streak = 0`, continues the loop over the rest — never increments the streak. |
| (b) A transient streak is the sickness evidence, **reset on any success** | **YES** | `Err(LamboError::EmbedUnavailable(_))` `:3102-3131` increments `transient_streak`; `Ok(summary)` `:3088-3091` and the content arm `:3139` both set `transient_streak = 0` (observed health). |
| (c) The loop **terminates leaving the remaining backlog DURABLE** (not consumed) once the threshold crosses | **YES** | `if transient_streak >= EMBEDDER_SICK_THRESHOLD { …; break; }` `:3104-3120` — breaks with `THIS intent and the rest of the backlog stay DURABLE and unconsumed`, plus a warn naming `remaining = backlog - applied - failed`. |
| (d) Threshold AND its error posture (burn bound / false-alarm tolerance) are **stated** in code/design, not unexplained | **YES** | `EMBEDDER_SICK_THRESHOLD = 3` `:214`; docstring `:180-213` states **Burn bound** ("intents at risk before the decision … cost is time, ≤ this × `HYBRID_IO_TIMEOUT`, never durability") and **False-alarm tolerance** ("stop only after this many *consecutive* transients with no applied success or content refusal between them"). The design doc's as-built records the same posture `J3-durability-redesign.md:516-532`. `EMBEDDER_SICK_THRESHOLD` is *used* by the loop that consumes the backlog, and the streak lives in the loop, not on the write — the exact "bound the loop's measure, never the write's survival" the prescription demanded. |

The Bernoulli-transient-vs-content framing (Wald's SPRT, deferred by the design doc until the rule
table existed) is faithfully the simplest SPRT-family form — a run-of-3 sequential test with stated
controls — and the design doc says so explicitly. This is genuinely the suggested measure, not a
bare `k=3`.

---

## The rule table (operator's concern, part 2)

- **No `_ =>` arm produces `Backend`/content.** `:78` maps the catch-all to `Unclassified`; named
  `Content`/`PermanentConfig` are explicit statuses only (`:68`, `:70`). The only wildcard (`_`) —
  and every un-named 5xx (`:76`) — lands transient/unclassified, i.e. durability-preserving.
- **`unclassified` is conservative and logged** — `:206-220`: treated as transient, `tracing::warn!`
  names the unrecognised status.
- **Class decided at the adapter** where the status is known, and carried as an `EmbedError` variant;
  `is_transient` (`src/embed/mod.rs:77-82`) needs no re-derivation from a message string. ✓
- **Exact status→class mapping matches the prescribed table** (`J3-durability-redesign.md:374-390`):
  transient 408/425/429/500/502/503/504/509/529 + un-named 5xx; content 400/413/415/422; permanent-config
  401/403/404; unclassified everything else. One deviation worth naming: the prescription's table listed
  `500` under transient (`:375,380`) which is what shipped (`bge_m3.rs:74`) — consistent.

---

## Gate reconciliation

| Gate | Round-2 claimed (@bc28ac8) | Remediation reported | **This review measured (@9b4c456)** |
|---|---|---|---|
| sqlite (`store-sqlite,embed-fixture,fixtures`) | 1000 / 0 / 3 | 996 / 3 / 3 (trio "pre-existing") | **1006 / 0 / 3** — the trio passes |
| fixtures (`--all fixtures`) | 908 / 0 / 3 | 912 / 0 / 3 | **912 / 0 / 3** |
| cockroach narrow (`--no-default features store-cockroach`) | 565 / 0 / 0 | 557 + 5 + 2 (+2 doctests) | **557 lib + 5 + 2 + 2 doctests = 566 / 0 / 0** (exact match) |
| fmt (`cargo fmt --all -- --check`) | clean | clean | **clean** |
| clippy ×4 (default; `store-sqlite,fixtures`; `ship,fixtures`; `--no-default store-cockroach,embed-fixture`) | clean | **not run** | **clean ×4, 0 warnings** |
| verify.sh | 46 ok | blocked (Python 3.14) | **blocked, genuinely** (see below) |
| cockroach **live** `--ignored` (`LAMBO_COCKROACH_DSN` + `LAMBO_REQUIRE_LIVE=1`) | — | 6/6 incl. column-preflight | **7 / 7 passed / 0 failed**, including `column_preflight_refuses_a_missing_column_live` |

### The 3 `serve_proxy_multi_client` failures — **do NOT reproduce; not a J3 regression, and not a
stable pre-existing failure either**

The three tests the remediation reported failing are a J2 proxy suite (`tests/serve_proxy_multi_client.rs`,
8 tests: `two_clients_over_stdio_both_work_through_one_hub`, `a_dead_holder_leaves_the_proxy_honest_and_the_lease_unclaimed`,
`a_call_in_flight_when_the_holder_dies_is_answered_rather_than_lost`, `a_live_endpoint_with_no_lease_row_is_refused_rather_than_dialled`,
`a_base_directory_too_long_for_a_socket_still_serves_its_own_client`, `a_holder_reachable_only_at_its_own_directory_is_still_forwarded_to`,
`a_holder_whose_lease_outlasts_the_client_budget_is_refused_at_once`, `a_holder_whose_endpoint_refuses_is_not_described_as_still_refreshing`).
They bind no fixed TCP port (stdio transport), use unique per-test temp dirs
(`scratch()` embeds `process::id()` + a nanosecond timestamp, `tests/serve_proxy_multi_client.rs:175-199`), and a
fixture embedder — so the dogfood serve on `127.0.0.1:7700` (pid 118400, live during my run) and the other
serves **cannot** occupy any resource these tests need.

Measured on this machine at HEAD, working tree clean:
- **All 8 pass in isolation** (`cargo test --features store-sqlite,embed-fixture,fixtures --test serve_proxy_multi_client` → `8 passed; 0 failed`).
- **All 8 pass inside the full sqlite suite** (`8 passed` line in the 1006/0/3 run).

So the remediation's "996/3, verified failing on clean HEAD with changes stashed" does **not** reproduce
here: the suite is green and the trio passes. The failures were a transient/environment artifact of the
remediation's own run (residual socket in `/run/user/1000/lambo`, machine load, or a test-parallelism
hazard), not a stable property of the branch and not a J3 regression (the tests are J2-scoped and pass).
The remediation's reported gate numbers (996/3) under-state the branch; the honest figure is 1006/0/3.

### clippy ×4 — now clean (the remediation never ran it)

Ran all four configs (`--all-targets`): default, `store-sqlite,fixtures`, `ship,fixtures`,
`--no-default-features store-cockroach,embed-fixture` — **0 warnings, 0 errors on each**. The
remediation's budget skip is now discharged.

### verify.sh — genuinely blocked, pre-existing, environment-caused; cannot pass without a repo change

`scripts/observability/verify.sh` fails at step 1 (`make_sample.py`): Python 3.14.7's `sqlite3` →
`from warnings import warn` resolves the **local** `scripts/observability/warnings.py` (which shadows
stdlib `warnings`), which does `import subprocess`, whose `warnings.warn` then resolves to the local module
⇒ `AttributeError: module 'warnings' has no attribute 'warn'`. The files in the failing chain
(`warnings.py`, `make_sample.py`, `_ledger.py`) are untouched by any J3 commit
(`git log bc28ac8..HEAD -- scripts/observability/` is empty) — **J3-independent, pre-existing**.
I tried to make it pass without touching the repo: `PYTHONSAFEPATH=1` alone still breaks on sibling import
`_ledger`; `PYTHONSAFEPATH=1 + PYTHONPATH=<dir>` still shadows stdlib `warnings` because the directory must
stay importable for `_ledger` yet its `warnings.py` hijacks the stdlib name. There is **no clean env-only
pass on Python 3.14** — it requires renaming `scripts/observability/warnings.py` (a repo change, out of
this review's scope and correctly out of the remediation's). The branch has indeed never passed verify.sh
on this rig (Python 3.14). `warnings.py` is dead weight with no consumer in `verify.sh`'s report list, so
the fix is a one-line removal — appropriate for a later remediation, not a J3 blocker.

### Fixtures +4 — reconciled **by name** (net +4 distinct new tests)

`—--list` delta `bc28ac8..HEAD` (excluded the two `memory::Memory (line …)` doc junk rows that cancel):
added 6, removed 2; the two removals are renames/replacements (net 0): `rejects_non_success_status` →
`rejects_500_as_transient` (R-1 rewrite) and `only_pending_is_unsettled` → `only_the_two_pendings_are_unsettled`
(R-4 rename). The four **net-new** fixtures tests are therefore:
1. `embed::bge_m3::tests::status_class_drives_the_error_variant` (R-1),
2. `embed::bge_m3::tests::the_status_rule_table_classifies_every_named_status` (R-1),
3. `memory::tests::a_write_reached_during_an_in_session_embedder_outage_is_not_consumed` (R-2),
4. `writeq::tests::replay_blocked_names_the_reason_or_is_none` (R-8).

`912 = 908 + 4` reconciles exactly.

### store-cockroach + live Cockroach — verified

Narrow `--no-default-features --features store-cockroach` with the `.env` DSN loaded and
`LAMBO_REQUIRE_LIVE=1`: **lib 557 / 0 failed**, plus 5 + 2 + 2 doctests = **566 / 0 / 0**, an exact
match to the remediation's cited `557+5+2(+2)`. The live `--ignored` lib run against the real cluster:
**7 / 7 passed / 0 failed / 55 s**, including `column_preflight_refuses_a_missing_column_live` which, read
at source (`cockroach.rs:3750-3784`), does exactly what the remediation claimed and what the operator
asked to reproduce live: `init_schema` → passing preflight → rename `concepts.chunk_group_id` away →
preflight refuses by **table** (`concepts`) + **column** (`chunk_group_id`) + actionable (`lambo provision`)
→ rename back → passes. DSN never printed or committed.

### Live embedder note (R-6)

BGE-M3 at `127.0.0.1:8080` is **down** (`curl /health` empty); only the chat llama-server on `:8082`
(`{"status":"ok"}`) is up. The remediation's drop-numbers choice was the only honest option (re-measure
was unavailable), so this is **not** a PARTIAL-if-live — it is the permitted fallback correctly chosen.

---

## New findings

| # | Grade | Finding | Evidence |
|---|---|---|---|
| R3-N1 | P3 | **Stale trait docstring contradicts the shipped column preflight.** `GraphStore::preflight_schema`'s trait docstring still reads "**Tables only.** … a missing column is not covered here" (`src/store/mod.rs:265-267`), but both SQL adapters now *do* diff columns. A reader of the trait is told the very gap J3-R2R-3 closed is still open. Direction is safe (it under-states protection, not over-claims it), so P3 — one sentence to update at the trait. | `src/store/mod.rs:265-267` vs `src/store/sqlite.rs:726-747`, `src/store/cockroach.rs:1999-2020` |
| R3-N2 | P3 | **`scripts/observability/warnings.py` is unreferenced dead weight** that makes `verify.sh` unrunnable on Python 3.14 (see Gate reconciliation). Not J3-caused, but worth naming as the one-line fix a later remediation should make so the observability gate can run at all. | `scripts/observability/warnings.py`, verified shadowing / no consumer in `verify.sh` |

Neither blocks integration.

---

## Verification-only edits

None. `git status` clean apart from the pre-existing untracked `local:/` brief file. The detached
`bc28ac8` worktree used for the by-name fixture delta was removed. A temporary
`/tmp/j3_r2_wt` was removed (see Method). No commit is attributed to this review (the operator handles
the J3 doc commit for the round).

## Verdict

**APPROVE.**

The nine round-2 findings are closed at source; the prescribed P1 fix is implemented as designed — the
rule table with no wildcard to `Backend`, class decided at the adapter, and the sequential decision rule
genuinely wired in with its threshold and both error-posture controls *stated* in the code and in the
design doc's as-built section, not a bare constant. The R-3 column preflight is verified against the
real Cockroach cluster, and R-8's block-reason stat is set in the liveness return and both breaking arms.

Every gate the remediation skipped or blocked has now been run or definitively characterised: clippy ×4
clean, fmt clean, fixtures 912/0/3 with the +4 reconciled by name, store-cockroach 566/0/0 plus 7/7 live,
and verify.sh confirmed as a genuine pre-existing Python-3.14 environmental failure that cannot pass
without a one-line repo change. The one material correction to the remediation's report is that its
sqlite gate (996/3) **under-states the branch**: independently measured, the suite is **1006/0/3** and
the `serve_proxy_multi_client` trio it flagged as "pre-existing failing" passes in isolation and in the
full run; those failures do not reproduce and are not a J3 regression.

Under the zero-residue rule: the two new P3 findings are documentation/maintenance items (a stale trait
sentence; a dead `warnings.py`), explicitly non-blocking, and a later cleanup round can take both plus
the previously-scheduled F4 close-time-latency measurement against live Cockroach.

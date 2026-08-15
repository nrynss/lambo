# Adversarial Review R2: T8.3 — CLI subcommands (read + write parity)

```text
╔══════════════════════════════════════════════════════════════════════╗
║  STATUS: CLEAN — zero P1, zero P2 (re-verification at 596f40f)       ║
║  Verdict: CLEAN — T8.3 CLI surface holds at current HEAD             ║
║  Findings: 3 × P3 (all pre-existing T88-H6/H7/H10, still pending)    ║
║  Gates: fmt [x] clippy x3 [x] test 706 [x] test-sqlite 753 [x]       ║
║          no-default x2 --no-run [x] check --no-default-features [x]   ║
║  Live:   all 9 subcommands on SQLite; lease fail-closed; readers      ║
║          lease-free; differential + guard non-spurious; r2 file       ║
║  Opened: 2026-08-15 · Reviewed: 2026-08-15 · HEAD: 596f40f            ║
╚══════════════════════════════════════════════════════════════════════╝
```

**Task:** T8.3 — CLI subcommands, read + write parity with the MCP surface
(`dev-diary/PHASE-8-surface.md` §T8.3; spec §6.2, §2.2).
**Tree:** `phase/p8-surface` @ `596f40f`, clean tree (one untracked doc only — a sibling
review's report, untouched). Since the R3-CLEAN verdict against the older tree, three things
changed that a fresh review must own: the L82 remediation (invisible-char validator in
`src/cli/caps.rs`), the T8.8 comment-only rustdoc pass (incl. the `MAX_INSPECT_NODES`
total-vs-level comment fix in `src/cli/caps.rs`), and T8.7's release `596f40f` (the
`MAX_INSPECT_SCAN_CONCEPTS` graph-size guard on `resolve_focus` in the T8.3-owned
`src/cli/inspect.rs`).
**Method:** find-only (no `src/`/`Cargo.*` edited). Full binding gate block re-run
independently; every subcommand exercised against a live `cargo build --features store-sqlite`
binary on a SQLite session; the write/read lease story re-probed both ways; the new
`resolve_focus` guard checked for spurious gating; differential + every prior T83 pin +
all three T88-H6/H7/H10 pending items confirmed against current source and the live binary.

---

## Gates (full binding block, run independently — all green)

```text
cargo fmt --all -- --check                                    CLEAN
cargo clippy --all-targets -- -D warnings                    exit 0
cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings   exit 0
cargo clippy --all-targets --features store-sqlite -- -D warnings                           exit 0
cargo test                                                  706 lib + 5 bin + int, 0 failed, 1 ignored
cargo test --features store-sqlite                          753 lib + 5 bin + 11 int + 1 doc, 0 failed, 1 ignored
cargo test --no-default-features --features store-sqlite --no-run    BUILDS
cargo test --no-default-features --features store-cockroach --no-run BUILDS
cargo check --no-default-features                             CLEAN
```

Matches the T8.7 R1 remediation counts exactly (706 / 753) and every gate row is green. The
62-test `cli::` suite ran clean (`cargo test --features store-sqlite --lib -- cli::`).

---

## Findings

### T83R2-1 (P3) — T88-H7 not addressed: every `inspect` error message doubles the `inspect:` prefix — CONFIRMED (live)

- **Where:** `src/cli/inspect.rs:284` (Ambiguous), `:299-300` (Oversized), `:303-305`
  (Missing). The dispatcher (`src/main.rs:350`) already prefixes `lambo inspect: `, and each
  `run` message begins with `inspect: ` again.
- **Evidence (live, SQLite):**
  - Ambiguous (exit 2):
    `lambo inspect: inspect: 'a' matches 2 concepts — name one exactly, or pass its node_id:`
  - Missing (exit 1):
    `lambo inspect: inspect: no concept matching 'x' in session 's2'`
  - The new Oversized message shipped by T8.7 has the same doubled form
    (`inspect: this session's graph has more than {cap} …`), extending the H7 class.
- **Status:** PENDING since the T8.8 audit; still present. Cosmetic — the *success* path and
  all *help text* are unaffected, so it does **not** contradict the T8.3 done-when or
  help-text requirement. Recorded so the loop knows it remains open. Cheap fix (drop the
  leading `inspect: ` from the three messages), in T8.3's own file, no new scope.

### T83R2-2 (P3) — T88-H6 not addressed: user-facing lease error still carries a `Spec §2.2` citation and raw SQL — CONFIRMED (live)

- **Where:** `src/memory.rs:559-567` (the `Conflict` on lease refusal); the spec citation at
  `:561`, `OPERATOR_OVERRIDE` (raw `DELETE FROM session_leases …`) appended at `:566`.
- **Evidence (live):** CLI `derive` / `record-action` against a serve-held session exit 1 with:
  `… it acquired the single-writer lease 3s ago and is still refreshing it. Spec §2.2 is one
  writer per session; refusing to open a second. If that holder is wedged, an operator can
  force a takeover: DELETE FROM session_leases WHERE session_id = '<session>';`
- **Status:** PENDING. Crucially, the T8.3 done-when half **holds** — the error is honest and
  names the holder, its age, and the takeover option (the T8.3 requirement is "fails closed
  naming the holder", which is met). Only the polish half (spec citation + raw SQL in a
  routine message) is open. Note this string lives in **`src/memory.rs`** (T8.1-owned), so a
  T8.3 agent cannot fix it without a cross-path authorization — that is a constraint worth
  recording, not a defect of the CLI surface. P3; the audit itself rated the refusal
  "excellent" and only the citation/SQL as the issue.

### T83R2-3 (P3) — T88-H10 not addressed: `release` after a CLI `reserve` reports a generic not-found — CONFIRMED (live)

- **Where:** `src/graph/reserve.rs:140-141` (`StoreError::NotFound("no reservation on node
  {node}")`), surfaced by `src/cli/reserve.rs:84-87` as `lambo release: not found: …`.
- **Evidence (live):** `reserve` succeeds (corrected T83-10 message: "…this reservation ends
  when this process exits (now)…"), then the very next `release` in a fresh process fails with
  `lambo release: not found: no reservation on node <uuid>` — correct behaviour, but the error
  does not connect back to the RAM-local lifetime. `release --help` does explain it ("On the
  CLI a prior reserve already ended when that process exited"); only the runtime message is
  terse.
- **Status:** PENDING, low severity. The message originates in `graph/reserve.rs` (not
  T8.3-owned); a CLI-side wrap in `src/cli/reserve.rs` could add the context. P3.

---

## Verified holds at HEAD (attacked, did not break)

1. **All 9 T8.3 subcommands** present in `src/main.rs` dispatch, with exact flag sets and clap
   help (`every_subcommand_and_required_arg_has_help` passes). `needs_embedder()` excludes
   `saints`/`inspect`/`stats`/`provision` (T83-7 pin `saints_stats_inspect_provision_resolve_store_only`
   passes), so non-embedding readers carry no embedder.
2. **Read verbs are lease-free.** Live: `recall`/`saints`/`stats`/`inspect` all exit 0 **while
   a `serve` holds the session lease**. Pin `reader_recall_does_not_spawn_gc_or_mutate_epoch`
   passes.
3. **Write verbs fail closed naming the holder; lease released after a successful write.**
   Live: `derive` + `record-action` against a serve-held `s2` each exit 1 naming
   `srv@cachyos-x8664#<pid>` + age + takeover; after `serve` stopped, `derive` →
   `record-action` → `reserve`/`release` all acquire and release. No second writer.
4. **Single construction site / shared validators.** MCP `src/mcp/server.rs:42-46` imports
   `check_size as validate_size` and every `MAX_*` cap, plus `clamp_cfg_default`, from
   `crate::cli::caps` — no duplication. Pins `shared_validators_are_the_caps_module` and
   `cli_refuses_oversized_and_control_char_like_mcp` pass. Same caps on both surfaces
   (`MAX_TOP_K=100`, `MAX_CONCEPTS_PER_DERIVE=64`, `MAX_ACTION_TARGETS=64`,
   `MAX_RESERVE_TTL_SECS=3600`, `MAX_INSPECT_DEPTH=5`, `MAX_CONTENT_BYTES=16384`).
5. **CLI↔MCP differential.** `cli_mcp_differential_derive_record_recall` passes (compares
   recall content, not scores — the documented caveat).
6. **Every prior T83 finding's pin present and green:** T83-1 (`parent_of_writes_hierarchical_edge_parent_to_child`),
   T83-2 (`parent_of_with_more_than_one_colon_is_usage_naming_ambiguity`),
   T83-3 (F18 guard walks `get_id`/`get_long`/`get_all_aliases`, substring match),
   T83-5 (`recall_prints_skipped_vector_leg_when_embed_fails`),
   T83-6 (`cargo_toml_marker_requires_package_name_lambo`, `provision_script_without_lambo_marker_is_ignored`),
   T83-11 (`canonical_memories_from_graph_agrees_with_memory`),
   T83-10 (reserve text "ends when this process exits", pinned in the sqlite e2e).
7. **L82-2 (invisible-char) fix intact** in `check_size` via `INVISIBLE_RANGES`/`TEXT_REQUIRED_INVISIBLE`
   (`src/cli/caps.rs`), pinned by `invisible_format_characters_are_refused_by_codepoint` and the
   allow-list test.
8. **New `resolve_focus` graph-size guard (T8.7 T87-1, in the T8.3-owned `inspect.rs`) is sound and
   non-spurious.** The exact (case-insensitive, no-alloc `eq_ignore_ascii_case`) and node-id legs run
   *before* the guard (`inspect.rs:64-85`); the bound `g.concepts().count() > MAX_INSPECT_SCAN_CONCEPTS`
   (`:94`) gates only the O(total-content) lowercase fuzzy leg. Live on a small graph: the fuzzy leg
   resolves normally (`resolved 'billing' → 'billing retries' (substring match, single candidate)`),
   so the guard did **not** regress correct CLI `inspect`. Both pin tests pass
   (`a_graph_past_the_scan_cap_refuses_only_the_fuzzy_leg`, `a_graph_within_the_scan_cap_still_resolves_the_fuzzy_leg`).
   Exit-code contract holds: ambiguity/usage = 2, missing/oversized/lease/close = 1.
9. **T88-H8 comment fix present** (`caps.rs:31-37`: `MAX_INSPECT_NODES` is a total, verified against
   `render_neighbourhood`'s single pre-loop budget at `inspect.rs:176`). **T88-H11** (all help present
   and consistent with MCP wording) holds — spot-checked `inspect`/`release` help live.
10. **`provision` is honest.** On SQLite it performs the real `init_schema` ("sqlite schema
    provisioned (init_schema, idempotent)"); the Cockroach arm wraps `scripts/provision.sh` with the
    repo-marker guard and echoes the resolved path (T83-6). No false "real provision" claim.

---

## Disposition

The CLI surface of T8.3 holds at `596f40f`. All nine subcommands build, run, carry correct
help, keep readers lease-free, keep writers single-writer via the T8.6 lease (fail-closed
naming the holder), share validators/caps with MCP, and pass the differential test. The T8.7
hardening's new graph-size guard on the shared `resolve_focus` does **not** regress CLI
`inspect` — it fires only on oversized graphs and resolves normally on a correct small graph.
Every prior T83 finding's pin is present and green. The tree is clean and every gate row is
green.

The three P3 items (T83R2-1/2/3) are the pre-existing T88-H6/H7/H10 open items from the T8.8
audit, all still pending and live-confirmed. None contradicts the T8.3 done-when or
help-text requirement (each is cosmetic/operability; the required honest lease-failure and
help-text behaviours all hold), so none is a P1/P2.

**Verdict: CLEAN — zero P1, zero P2.** The 3× P3 records stay open for whoever takes the
T88-H6/H7/H10 polish (note the ownership caveat: H6's string is in T8.1's `memory.rs`).

— T83ReviewR2, 2026-08-15

---

## R2 Verdict (reverify of the P3 remediation)

Re-reviewed the uncommitted 4-file remediation (inspect.rs, memory.rs, cli/reserve.rs, cli.mdx)
against T83R2-1/-2/-3 at `phase/p8-surface`. **Verdict: FINDINGS — the remediation is NOT sound.**

- **T83R2-1 (dup `inspect:` prefix)** — FIXED, sound. All three messages in `src/cli/inspect.rs`
  (Ambiguous/`Focus::Oversized`/`Focus::Missing`) now render a single `lambo inspect: …` via the
  `src/main.rs:350` dispatcher prefix. `cli.mdx:141` quote matches. MCP `lambo_inspect:` prefix
  untouched (tool name — correct, not a duplicate).
- **T83R2-2 (lease error: spec citation + raw SQL)** — FIXED in code, but **regressed a pinned
  behavior test (P2)**. `src/memory.rs:559-566` now names holder + age and points at
  `docs/reference/cli.mdx`; raw SQL and `Spec §2.2` gone; still `LamboError::Conflict`; still
  `OPERATOR_OVERRIDE`-referenced at `memory.rs:1910` (not dead). BUT the test
  `memory::tests::a_second_writer_sharing_a_store_is_refused_by_the_lease` (`memory.rs:3331`,
  assert at `:3351`) pins `msg.contains("session_leases")`, which the new message drops →
  **3rd-party gate is RED**: `cargo test --features store-sqlite --lib -- memory::` = 69 passed,
  1 failed. The remediation needed to update this T8.1-owned test to the new message contract.
- **T83R2-3 (release not-found)** — FIXED, sound, reachable. `src/cli/reserve.rs:86-94` matches
  `LamboError::Store(StoreError::NotFound(_))` (graph-side "no reservation on node"), re-explains
  RAM-local lifetime; non-NotFound passthrough preserved; the genuine reserved-by-other-agent
  `Conflict` path (graph/reserve.rs:147) is NOT swallowed (falls through to `Err(e) → from(e)`).
  `src/graph/reserve.rs` unchanged, as required.
- **Doc quote accuracy (P3)** — `docs/reference/cli.mdx:169` ends `…docs/reference/cli.mdx);`
  with a stray trailing `;`, but the emitted string ends `…docs/reference/cli.mdx)` (no `;`). The
  remediation's own doc edit is not verbatim — breaks the audit's quoting rule.

**Gates run (remediation tree):** `cargo fmt --all -- --check` CLEAN · `cargo clippy --all-targets
--features store-sqlite -- -D warnings` exit 0 · `cargo check --no-default-features` CLEAN ·
`cargo test --features store-sqlite --lib -- cli::` 62 passed · `cargo test --features
store-sqlite --lib -- memory::` **69 passed, 1 FAILED** (P2 above). No source was modified during
this review (only this verdict note).

— T83P3Reverify, 2026-08-15

---

## R2 Remediation (P8) — dispositions

Remediation agent `T83P3Remediate` addressed the two findings raised by the R2 reverify
against the P3 remediation. `T83R2-1` and `T83R2-3` were already verified sound by that
reverify and were left untouched.

### T83R2-2-REGRESS — FIXED

- **Where:** `src/memory.rs` test `memory::tests::a_second_writer_sharing_a_store_is_refused_by_the_lease`
  (was `:3331`, final assertion `:3351-3354`), the pinned behavior test for the lease
  fail-closed contract. The message body at `memory.rs:559-566` was already correct and was
  **not** modified.
- **What changed:** The test's final assertions now pin the new message contract instead of
  the dropped raw-SQL literal. It still verifies the fail-closed `LamboError::Conflict`
  variant (via `let LamboError::Conflict(msg) = err else { panic!(…) }`, so a non-Conflict
  error fails the test), still asserts the lease is enforced (`single-writer`), still names
  the current holder (`agent-a`) and its age (the `s ago` surface — value-agnostic, since the
  age is nondeterministic in-test), and still asserts the operator-takeover pointer surfaces
  (`operator can force a takeover`, `docs/reference/cli.mdx`). The `session_leases` literal
  assertion was removed because that string is intentionally no longer emitted. The test
  remains meaningful: it still fails if the fail-closed/naming/takeover-pointing behavior
  regresses.
- **Full `memory::` gate now green:** `cargo test --features store-sqlite --lib -- memory::`
  passes (see gates below).

### T83R2-2-DOC — FIXED

- **Where:** `docs/reference/cli.mdx:169`.
- **What changed:** Removed the stray trailing `;` (leftover from the old SQL constant
  terminator) so the quoted CLI output now ends `»(see the single-writer lease note in
  docs/reference/cli.mdx)` and matches the emitted string byte-for-byte (the message body at
  `memory.rs:559-566` ends `…docs/reference/cli.mdx)` with no semicolon). The T8.8 verbatim
  quote rule is restored.

### Gates (full binding block — all green on the remediation tree)

```text
cargo fmt --all -- --check                                    CLEAN
cargo clippy --all-targets -- -D warnings                    exit 0
cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings   exit 0
cargo clippy --all-targets --features store-sqlite -- -D warnings                           exit 0
cargo test                                                   PASS
cargo test --features store-sqlite                           PASS (incl. memory:: green)
cargo test --no-default-features --features store-sqlite --no-run    BUILDS
cargo test --no-default-features --features store-cockroach --no-run BUILDS
cargo check --no-default-features                             CLEAN
```

— T83P3Remediate, 2026-08-15

---

## R3 — Final review verdict (T83P3FinalReview, 2026-08-15)

**VERDICT: CLEAN** — both P3 remediation fixes verified real, meaningful, and regress-free; the other two P3 fixes intact; gates green.

### 1. T83R2-2-REGRESS (memory.rs test rewrite)
- Rewritten test `memory::tests::a_second_writer_sharing_a_store_is_refused_by_the_lease` genuinely pins fail-closed: `let LamboError::Conflict(msg) = err else { panic!(...) }` fails if the store does NOT return the Conflict variant. Asserts: `single-writer` (lease enforced), `agent-a` (holder named), `s ago` (holder's age, value-agnostic), `operator can force a takeover` + `docs/reference/cli.mdx` (takeover pointer). No `#[allow]`, no tautology — regresses to red if the message loses any of these or the variant changes.
- Message BODY at memory.rs:559-566 unchanged except the doc/comment alignment (verified same emitted strings). Raw `session_leases` SQL literal intentionally dropped from the user-facing message; `OPERATOR_OVERRIDE` still used at memory.rs:1910, so not dead code.
- No other test weakened: suite green (70 passed under memory:: filter).
- Properly routed: the removed "inspect:" internal prefix is re-added by `run_async` in main.rs (lines 330-350) as `lambo {name}: `, so `lambo inspect: ...` is preserved.

### 2. T83R2-2-DOC (cli.mdx ';' / quote fidelity)
- docs/reference/cli.mdx:169 now matches the emitted memory.rs string byte-for-byte after the CLI prefix (`lambo derive: conflict: `); no stray `;`. Verified programmatically (prefix-strip equality holds).
- docs/reference/cli.mdx:141 (`lambo inspect: 'a' matches 5 concepts …`) matches the updated inspect.rs message under the same `lambo {name}: ` prefix.

### 3. Other two P3 fixes intact
- src/cli/inspect.rs: internal `inspect:` prefix stripped (Ambiguous / Oversized / Missing) — correct, outer prefix supplies it; MCP variant (server.rs:979) separately and correctly keeps `lambo_inspect:`.
- src/cli/reserve.rs: `StoreError::NotFound` on `release` wrapped in a diagnostic Runtime error — intact and correct.

### Gates run (this review)
- `cargo test --features store-sqlite --lib -- memory::` → 70 passed
- `cargo test --features store-sqlite --lib` → 753 passed, 0 failed, 1 ignored
- `cargo clippy --all-targets --features store-sqlite -- -D warnings` → exit 0 (no warnings)
- targeted test `memory::tests::a_second_writer_sharing_a_store_is_refused_by_the_lease … ok`
- Byte-for-byte doc-vs-emitted checks (python) for cli.mdx:141 & :169

— T83P3FinalReview, 2026-08-15

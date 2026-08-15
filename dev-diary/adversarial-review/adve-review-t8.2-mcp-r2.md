# Adversarial Review (R2): T8.2 — MCP server re-verify at current HEAD

```text
╔══════════════════════════════════════════════════════════════════════════╗
║  STATUS: CLEAN (R2 re-verify) — T8.2 holds at 596f40f after hardening    ║
║  Verdict: CLEAN  (0 P1 / 0 P2 / 0 P3)                                    ║
║  Scope:   src/mcp/{server,serve,mod}.rs, src/main.rs, src/cli/inspect.rs ║
║  Tree:    phase/p8-surface @ 596f40f, clean working tree                 ║
║  Gates:   fmt [x] clippy x3 [x] test 706 [x] test-sqlite 753 [x]        ║
║           no-default x2 --no-run [x] check --no-default-features [x]     ║
║  Opened:  2026-08-15 · Closed: 2026-08-15                                ║
╚══════════════════════════════════════════════════════════════════════════╝
```

**Task:** T8.2 — MCP server (PHASE-8-surface.md §T8.2; spec §6.2/§6.3/§2.2; F18 carryover).
**Tree:** `phase/p8-surface` @ `596f40f` (`fix(P8): close T8.7 review findings to CLEAN (R2)`), clean.
Since the prior CLEAN (R5-verify) the surface changed materially: `task/t8.7-hardening`
(bearer auth, session cap, global rate limit, T88-H1 wire-hygiene) merged at `5d765c9`,
`task/live-l82-remediation` merged, `task/t8.8-delta` (docs + comment-only rustdoc, no
`src/mcp/**`), and the `596f40f` remediation (graph-size guard on `resolve_focus` fuzzy leg,
HTTP 413 body limit in `guard_request`, dated accepted-rationales).
**Method:** full binding gate block run independently; clause-by-clause re-read of
`server.rs` (seven tools) + `serve.rs` (serve/stdio/http/guard) + `main.rs` (single resolve);
then **three live probes** against a `cargo build --features store-sqlite` binary — a full
stdio MCP session on a fresh fixture session, a supplemental stdio attribution/determinism
probe, and an HTTP probe through the new auth/guard layers. **Findings only** — no `src/` or
`Cargo.*` file touched; only this report was created.

---

## Gates (full binding block — all green, counts match T8.7 R2 exactly)

```text
cargo fmt --all -- --check                                   CLEAN
cargo clippy --all-targets -- -D warnings                   CLEAN
cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings  CLEAN
cargo clippy --all-targets --features store-sqlite -- -D warnings                          CLEAN
cargo test                                                706 lib + 5 bin + 5 int + 1 doc, 0 failed, 1 ignored
cargo test --features store-sqlite                        753 lib + 5 bin + 11 int + 1 doc, 0 failed, 1 ignored
cargo test --no-default-features --features store-sqlite --no-run    BUILDS
cargo test --no-default-features --features store-cockroach --no-run BUILDS
cargo check --no-default-features                           CLEAN
```

Counts (706 / 753 lib) match the T8.7 R2 reverify exactly. `tests/serve_single_writer_lease.rs`
and `tests/serve_sigterm_durability.rs` are compiled and green in both test rows.

---

## What the hardening did NOT regress (each verified against code AND a live binary)

- **Exactly seven tools, correct names** — live `tools/list` returned exactly
  `lambo_derive, lambo_inspect, lambo_recall, lambo_record_action, lambo_reserve,
  lambo_saints, lambo_stats`; pinned by `the_router_publishes_exactly_the_seven_spec_tools`.
- **T88-H1 wire hygiene still fixed** — full `tools/list` blob marker-scanned for
  `byte-echo`, `r4 nit`, `handoff log`, `validate_size`, `rmcp`, `revisit`, `spec §`,
  `todo`, `fixme`, `not interceptable`, `rebuilds the whole router`: **zero leaks**. The
  Rustdoc-→-schema `description` on `WireConceptType` and every params field is
  user-facing; all review rationale lives in `//` blocks (`server.rs:58-82`).
- **F18 holds, refused not ignored.** A client `created_at` at top level
  (`lambo_derive {…, "created_at": …, "concepts": […]}`) and nested on a `WireConcept`
  (`concepts:[{…, "created_at": …}]`) are both **refused**: `unknown field `created_at`,
  expected one of `agent_id`, `concepts`, `parent_of`` / `…expected `content` or
  `concept_type``. `deny_unknown_fields` on all nine wire structs. The F18 golden-allowlist
  + full-schema-walk tests (`f18_tool_schemas_match_the_golden_property_set`,
  `f18_no_tool_schema_accepts_a_client_timestamp`) are present and pass.
- **`lambo_reserve`/`release` fail closed on a foreign `agent_id`.** Live:
  `lambo_reserve` as `agent-b` on a process owned by `agent-a` → `isError: true`,
  `…NOTHING WAS RESERVED OR RELEASED…` (T82-3 still holds through `require_session_agent`).
- **Warnings reach the text content** (T82-9). Foreign-agent `lambo_recall` and
  `lambo_derive` each return a **second** text block `warnings:\n- attribution: this process
  owns the session as agent 'agent-a' …` after the verbatim `content[0]`.
- **`lambo_inspect` deterministic, and the new fuzzy-leg guard does NOT spuriously gate a
  small graph.** Two identical fuzzy inspect calls returned byte-identical text
  (`resolved 'alpha' → 'alpha subsystem cert rotation' (substring match, single candidate)`).
  The O(totalscan) fuzzy leg ran normally on a 3-concept graph — `MAX_INSPECT_SCAN_CONCEPTS`
  (2_000, `src/cli/inspect.rs:32`) fires only past the cap, exact and node-id legs ungated
  (`resolve_focus`, `:63-130`; refusal wired to MCP at `server.rs:973-984`).
- **Server timestamps / no client clock reach.** `begin_interaction` stamps `Utc::now()`
  server-side ([INFERENCE] unchanged — no timestamp field in any published schema, and the
  schema walk is the same one F18 enforces); instructions still carry "Never send a
  timestamp".
- **Single-writer / one-process ownership intact post-hardening.** `serve()` still
  constructs **one** `Memory` (`serve.rs:689`), `authorize_bind` runs first
  (`serve.rs:687`) before the lease is taken, `events()` exactly once (`:696`), and
  `run_and_close` closes on **every** exit path (`:735-755`). The concurrent-session cap
  bounds only HTTP `LocalSessionManager` session *minting*; the writer *lease* (one writer
  per session) is untouched and green via `tests/serve_single_writer_lease.rs`.
- **Stdio shutdown durability (T82-1 fix) holds.** stdio EOF → `exit rc 0` with
  `mcp stdio: client disconnected reason=Closed` → `Memory session closed (tail flushed)` →
  `lambo serve: session closed, tail durable`. Tokio blocking-task teardown handled by
  `runtime.shutdown_background()` in `main.rs:445`.
- **HTTP availability not blocked by auth/guard.** Missing/wrong token → `401`; valid token
  → `initialize 200` + `mcp-session-id`; `tools/list` and `lambo_stats` both `200` through
  the guarded router. Oversized body → server returns `413` without streaming the body (the
  client's `sendall` hit a closed socket — the T87-3 "refuse before body" behavior, already
  pinned by `an_oversized_request_body_is_refused_before_the_service`).
- **Level B single construction site.** Exactly one `resolve_from_config_path` on the serve
  path, inside `resolve_for_command` (`main.rs:306-312`); serve reuses the resulting
  `*backends` with no re-resolve (`main.rs:426-427`).

---

## Findings

**None.** No new P1/P2/P3. All items in scope hold at current HEAD; every T8.7/L82/596f40f
change was checked specifically for a T8.2 regression and none was found.

## Observations (non-findings, recorded for completeness)

- A `lambo_derive` call whose `parent_of` endpoints are also members of `concepts`
  reports `"derived 2 concept(s): 2 created, 2 matched existing"` on a **fresh** session:
  `matched` counts nodes re-referenced later in the same call (documented at
  `src/graph/derive.rs:150-156`), so created+matched can exceed the arg count. Pre-existing
  wording unchanged since the original 5-round-CLEAN review; relay-faithful to
  `DeriveOutcome`; not introduced by the hardening. Purely cosmetic; no action requested.
- The prior accepted residuals (T82-12 → T8.4, T82-16 remainder → T8.5/P9) and T8.7's
  dated accepted-rationales remain correctly routed; nothing reopened.

---

## Disposition

**CLEAN — zero P1, zero P2.** The T8.2 surface has not regressed under the T8.7 hardening,
the L82 remediation, or the `596f40f` changes. Exactly seven tools and their contracts are
intact over stdio and HTTP; F18 is enforced, `lambo_reserve` still fails closed on a foreign
agent, warnings reach the model, inspect is deterministic with the new graph-size guard not
firing spuriously, the writer lease and shutdown-durability guarantees hold, and the auth /
session-cap / rate-limit / 413-body layers gate without breaking any T8.2 tool call. Full
binding gate block green (706/753 lib). No loop continuation required.

— T82ReviewR2, 2026-08-15

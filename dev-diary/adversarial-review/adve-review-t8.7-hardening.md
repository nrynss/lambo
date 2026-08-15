# Adversarial Review: T8.7 — MCP surface hardening (minimal cut)

```text
╔══════════════════════════════════════════════════════════════════════╗
║  STATUS: FINDINGS — T8.7 "Done when" NOT met; loop must continue     ║
║  Verdict: FINDINGS  (1 P1 / 2 P2 / 1 P3)                             ║
║  Scope:   merged minimal cut @ 5d765c9 (task/t8.7-hardening)         ║
║           + prior 8134a3c (task/live-l82-remediation)                ║
║  Gates:   fmt [x] clippy x3 [x] test 703 [x] test-sqlite 750 [x]    ║
║           no-default x2 --no-run [x] check --no-default-features [x] ║
║  Opened:  2026-08-15 · Reviewed: 2026-08-15                          ║
╚══════════════════════════════════════════════════════════════════════╝
```

> **FINAL VERDICT (this file's R2 reverify, 2026-08-15): CLEAN.** The STATUS banner above is the
> R1 / minimal-cut snapshot; the appended "R2 reverify verdict" at the end of this file confirms
> "Done when (a)-(d): all met" and "Verdict: CLEAN. Zero P1/P2." All T8.7 findings (T87-1..4)
> are fixed or closed with dated accepted-rationales; the loop is complete.

**Task:** T8.7 — MCP surface hardening (PHASE-8-surface.md §T8.7).
**Tree:** `phase/p8-surface` @ `5d765c9`, clean working tree (confirmed `git status`).
**Method:** clause-by-clause read of the merge's own status line and Handoff Log entry
against the live code and live binary; full binding gate block run independently; HTTP
transport exercised end-to-end against a provisioned SQLite store with a real
`cargo build --features store-sqlite` binary (loopback auth, rate limit, session cap,
fail-closed start); stdio `tools/list` captured live and marker-scanned for wire hygiene.
**Findings only** — no `src/` or `Cargo.*` file was touched. The single artifact created is
this report.

The hardening commits are present: `5d765c9` (T8.7 minimal cut) and the prior
`8134a3c` (L82 remediation). The tree is a clean serial checkout at HEAD.

---

## Gates (full binding block — run independently, all green)

```text
cargo fmt --all -- --check                                    CLEAN
cargo clippy --all-targets -- -D warnings                    CLEAN
cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings  CLEAN
cargo clippy --all-targets --features store-sqlite -- -D warnings                          CLEAN
cargo test                                                  703 lib + 5 bin + 5 int + 1 doc, 0 failed, 1 ignored
cargo test --features store-sqlite                          750 lib + 5 bin + 11 int + 1 doc, 0 failed, 1 ignored
cargo test --no-default-features --features store-sqlite --no-run    BUILDS
cargo test --no-default-features --features store-cockroach --no-run BUILDS
cargo check --no-default-features                             CLEAN
```

Matches the claimed numbers exactly (703 / 750 lib, +18 over baseline). No clippy/fmt
failures on any feature row.

---

## What actually works (verified against code and the live binary)

The hardening the minimal cut *was* scoped to is real, correct, and pinned by tests that
are not no-ops:

- **Fail-closed start.** `serve()` calls `authorize_bind` as its *first* statement, before
  `build_memory` takes the single-writer lease (`src/mcp/serve.rs:656`) — so a refused
  start costs no lease and no retry-blocking. Live: `--bind 0.0.0.0` with no token → full
  refusal message, exit `1`. Loopback (`127.0.0.1`, `::1`, `127/8`) stays optional-auth;
  stdio is untouched (`serve.rs:1482-1494`).
- **Empty env is a usage error, env beats flag.** `resolve_auth_token` treats a
  *set-but-empty* `LAMBO_AUTH_TOKEN` as an error, not a silent fallback to `--auth-token`
  (`serve.rs:303-315`; fixed in `main.rs:408-412`). Live: `LAMBO_AUTH_TOKEN=""` → usage
  error, exit `2`. `SecretToken` has a redacting `Debug` and no `Display`, and the compare
  is constant-time (`serve.rs:261-271`).
- **HTTP auth.** Loopback with a token configured: no credential / wrong token / wrong
  scheme → `401` with `WWW-Authenticate: Bearer`; correct token → served. The 401 is built
  before the rate limiter and session count are read, so an anonymous flood neither spends
  rate budget nor leaks load (`guard_request`, `serve.rs:458-485`; pinned by
  `auth_is_checked_before_the_rate_limit_and_the_cap`, `serve.rs:1761`). Live: `401 / 401`,
  valid token reaches rmcp (406 only because curl omitted the streamable-HTTP `Accept`
  header — a client detail, not a hardening failure).
- **Rate limit.** Global token bucket over `parking_lot::Mutex` + `Instant`, default 50 rps
  with 2× burst, `--rate-limit-rps 0` disables, refill clamps to capacity with no idle
  credit accumulation (`serve.rs:362-408`). Refused → honest `429` + `Retry-After`, not a
  hang. Live (`--rate-limit-rps 1`): `200, 200, 429, 429` with body
  `rate limit exceeded: slow down and retry`.
- **Concurrent-session cap.** Counted from `LocalSessionManager`'s own public `sessions`
  map via the `LiveSessions` trait, so no bookkeeping of ours can drift; only a session-less
  POST counts; established sessions are never starved; past-cap refused with an honest `503`
  naming `live/max`, `--max-sessions`, and the `DELETE /mcp` remedy. Live
  (`--max-sessions 3`, rate limit off): `initialize ×3 → 200, 200, 200`, 4th → `503` with
  the truthful `(3/3 sessions live)` body.
- **T88-H1 wire hygiene.** The published `tools/list` descriptions are user-facing. Live
  stdio capture: exactly **7 tools** (`lambo_derive`, `lambo_inspect`, `lambo_recall`,
  `lambo_record_action`, `lambo_reserve`, `lambo_saints`, `lambo_stats`); a marker scan of
  the full wire dump for `byte-echo`, `r4 nit`, `handoff log`, `validate_size`, `rmcp`,
  `revisit`, `spec §`, `todo`, `fixme` returned **zero hits**. `WireConceptType` now
  publishes a clean user-facing description; the review rationale lives in a `//` block
  (`server.rs:59-82`). Guard test `published_schemas_carry_no_internal_notes`
  (`server.rs:1398`) pins this against regression.
- **No new tools; the study 7-tool surface still works over stdio** (captured live, first
  `initialize` -> 200, then `tools/list` correct).

Each hardening behaviour has a real, discriminating test that fails on a plausible edit
(auth refusal asserts the inner route is *never reached*; the cap test asserts an
established session still gets through; the bucket test drives `Instant` deterministically;
the fail-closed rule is additionally pinned through the real `serve()` entry point in
`serve.rs:1788-1829`, not just the helper).

---

## Findings

### T87-1 (P1) — Residual #3 not fixed and not provably defused: no graph-size guard; T8.7 "Done when" clause (c) unmet

- **Where:** `src/cli/inspect.rs:70-77` (fuzzy leg of `resolve_focus`), called from
  `lambo_inspect` at `src/mcp/server.rs:935-936`; no graph-size guard anywhere in the
  inspect path (`src/cli/caps.rs:43-45` exposes `MAX_INSPECT_CANDIDATES/DEPTH/NODES` — none
  bounds the total concept set the fuzzy leg iterates).
- **Detail:** `resolve_focus` still allocates **O(total-content) per call**: line 73 runs
  `c.content.to_lowercase()` for *every* concept in the graph on every fuzzy `lambo_inspect`,
  and lines 71-78 additionally clone the content of every match into a `Vec`. The block's
  disjunct for #3 is *"fix the allocation **or** rely on the rate limit **plus** a
  graph-size guard"*. The rate limit is present, but the **graph-size guard does not exist** —
  confirmed by reading the inspect path and the Handoff Log ("the graph-size guard it pairs
  that with does not exist"). With an unattended graph and 50 rps, an attacker (or a large
  legitimate session graph) can still trigger 50 × total-content of `to_lowercase` work per
  second. Only one of the two required defuse legs is shipped, so #3 is neither fixed nor
  closed.
- **Evidence:** code read; the Handoff Log states the allocation is unchanged and the guard
  is absent. My scan found no total-graph or total-content bound feeding the fuzzy leg.
- **Reproduction:** structural (no live exploit attempted); the rate-limit bounds the *rate*
  but the per-request cost is unbounded by graph size, which is the exact shape the block
  calls an amplification vector.
- **Gap vs acceptance:** clause (c) of §T8.7 "Done when" is not satisfied.

### T87-2 (P2) — Residuals #1/#2 not closed with a *dated accepted-rationale in the review file*; T8.7 "Done when" clause (d) unmet

- **Where:** residual #1 rationale at `src/mcp/server.rs:71-81` (undated `//` comment),
  residual #2 rationale at `src/mcp/server.rs:284-290` (undated `//` comment), function
  `redact_urls` at `server.rs:291-302`.
- **Detail:** Neither residual is *fixed* — `concept_type`'s variant error is still built as
  `-32602` inside rmcp's `Parameters<T>` extractor before any `LamboServer` code runs
  (`server.rs:71-81`), and `redact_urls` still redacts only `://` tokens, missing a bare
  `host:port` (`server.rs:291-302`). The rationales for accepting both exist *only* as
  **undated** source comments; clause (d) of §T8.7 "Done when" requires each residual to be
  "fixed **or closed with a dated accepted-rationale in the review file**." No such dated
  record exists yet (this is the first T8.7 review file; the prior T8.2 review explicitly
  defers both to T8.7).
- **Evidence:** code read; live wire capture confirms no byte-echo is currently reachable
  through a *valid* instrument but the -32602 path is unchanged and undocumented-in-file.
- **Gap vs acceptance:** clause (d) is not satisfied. Closure is a documentation action for
  #1/#2 unless remediation elects to fix; it is trivially closable but currently open.

### T87-3 (P2) — HTTP request-size limit not added (T82-16 remainder); transport body size is the one unbounded axis

- **Where:** `src/mcp/serve.rs` `serve_http`/`HttpGuard` (no body-size limit on the router
  or middleware); tool-layer caps at `src/cli/caps.rs:43` (`MAX_CONTENT_BYTES = 16_384`) and
  `:23` (`MAX_CONCEPTS_PER_DERIVE = 64`) do bound per-field content and per-call concept
  count, and `deny_unknown_fields` rejects stray keys.
- **Detail:** The T82-16 remainder the block lists ("a request-size limit *if not already
  bounded*") was not implemented and, per the Handoff Log, not even audited: "no HTTP body
  limit was added or audited." The tool layer bounds any *one* string (16 KiB) and the
  concept count (64 ≈ ~1 MiB of content per call), and the rate limit bounds request *count*,
  so the practical amplification through an oversized body is largely contained — that is why
  this is P2, not P1 for remote impact. But the transport itself imposes no ceiling, and a
  body padded with rejected/oversized fields still incurs parse + validation cost before the
  tool layer refuses it.
- **Evidence:** code read (no `Content-Length`/`DefaultBodyLimit`-style bound in
  `serve_http` or `guard_request`); Handoff Log confirms no body limit was added or audited.
- **Gap vs acceptance:** the request-size bound is not a formal "Done when" clause, but it is
  an explicit T82-16 content item. Needs a disposition: either add a body limit or record a
  dated rationale that the tool-layer caps suffice (with the rate limit bounding request
  count).

### T87-4 (P3) — Two concurrent `initialize`s can both pass the cap check and overshoot by one (acknowledged race); rate limit is global / per-request, not per-`tools/call`

- **Where:** `src/mcp/serve.rs:500-518` (`live()` read then mint, a check-then-act gap);
  `src/mcp/serve.rs:349-361` (documented global, not per-`tools/call`).
- **Detail:** The overshoot-by-one race is bounded by in-flight concurrency, harmful only at
  the boundary, and the Handoff Log already documents it as an accepted trade-off (closing it
  would mean wrapping rmcp's 13-method `SessionManager` trait). The rate limit is honestly
  documented as bounding *requests to `/mcp`* rather than `tools/call` specifically, which is
  the correct and safe cut on streamable HTTP. Recorded for completeness; no action requested.

---

## Residuals' actual state in code (as reviewed)

- **#3 `resolve_focus` to_lowercase:** NOT fixed; rate-limit-defused **in part only** — the
  graph-size guard is absent (see T87-1).
- **#1 `concept_type` byte-echo:** NOT fixed; rationale present as an **undated** source
  comment only, not in a review file (see T87-2).
- **#2 `redact_urls` bare host:port:** NOT fixed; rationale present as an **undated** source
  comment only (see T87-2).
- **Request-size limit:** NOT added and NOT audited (see T87-3).

---

## Disposition

The merged minimal cut is **sound for what it implements**: the fail-closed bearer gate,
global rate limit, concurrent-session cap, and the T88-H1/H2/H3 wire-schema hygiene are
correct, honest, live-verified, and pinned by discriminating tests; all nine gate rows pass;
the tree is clean.

It does **not**, however, satisfy T8.7's own "Done when": clause (c) (residual #3 fixed **or**
rate-limit + graph-size guard) and clause (d) (residuals #1/#2 fixed **or** closed with a
dated accepted-rationale *in the review file*) are both unmet, and the request-size limit is
unaddressed. The status line and Handoff Log already state this honestly; this review
confirms it against the code and binary.

**Loop must continue:** a remediation pass is required to close T87-1 (add the graph-size
guard or fix `resolve_focus`), T87-2 (record the dated accepted-rationales in this file, or
fix), and T87-3 (add a request-size limit or record a dated rationale) before a re-review can
return CLEAN.

— T87Review, 2026-08-15

---

# R1 remediation (2026-08-15)

Remediation agent. The reviewer's text above is untouched; this section appends the
disposition of every finding. Fixes land in the T8.7 surface (`src/mcp/serve.rs`) and the
shared `resolve_focus` residual (`src/cli/inspect.rs`) — no new tools, no config knobs, no
dependencies. The full binding gate block was re-run after these changes and is green (see
below).

**Disposition summary: 2 FIXED, 1 CLOSED-ACCEPTED, 1 no-action (P3).**

| # | P | Disposition |
|---|---|---|
| T87-1 | P1 | **FIXED** — graph-size guard on the `resolve_focus` fuzzy leg; test pinned |
| T87-2 | P2 | **CLOSED-ACCEPTED** (2026-08-15) — dated rationales for residuals #1 and #2, below |
| T87-3 | P2 | **FIXED** — HTTP request-body size limit (413); test pinned |
| T87-4 | P3 | — recorded as accepted by the review; no action taken |

---

### T87-1 — FIXED. The `resolve_focus` fuzzy leg is now bounded by a graph-size guard

`resolve_focus` (`src/cli/inspect.rs`) still does an O(total-content) `to_lowercase()` pass
for a fuzzy (substring) focus, so per-request work tracked graph growth. The block's other
disjunct for residual #3 is "rely on the rate limit **plus** a graph-size guard"; the rate
limit was shipped in the minimal cut but the guard was missing. It is now present:

- **`MAX_INSPECT_SCAN_CONCEPTS`** (`src/cli/inspect.rs`, default **2_000**): before the fuzzy
  pass runs, `resolve_focus` counts the session's concepts (an allocation-free linear pass)
  and **refuses** (`Focus::Oversized { cap }`) a graph over the cap rather than paying the
  lowercase+alloc pass. Refusing, not trimming — a trim would silently search a subset and
  could miss the real match. The exact (case-insensitive content) and node-id legs still
  resolve on an oversized graph; only the substring leg is gated.
- Per-second worst case is now `rate_limit × MAX_INSPECT_SCAN_CONCEPTS`, independent of graph
  growth — the paired leg the review said was missing.
- **Handled at both call sites:** CLI `inspect::run` returns a `Runtime` error naming the cap;
  MCP `lambo_inspect` (`src/mcp/server.rs`) returns a tool-level error directing to an exact
  focus / node_id.
- **Pinned by tests** (`src/cli/inspect.rs`): `a_graph_past_the_scan_cap_refuses_only_the_fuzzy_leg`
  asserts `Focus::Oversized { cap }` fires on a `cap+1` graph while exact and node-id still
  resolve; `a_graph_within_the_scan_cap_still_resolves_the_fuzzy_leg` asserts the guard does
  not fire spuriously on a small graph. Both pass.

### T87-2 — CLOSED-ACCEPTED (2026-08-15). Dated accepted-rationales for residuals #1 and #2

Neither residual is fixed by code; each is accepted with a dated rationale recorded here (the
block's clause (d) — "fixed **or closed with a dated accepted-rationale** in the review file").

- **Residual #1 — `concept_type`'s `-32602` variant error.** ACCEPTED (2026-08-15). The
  error is built inside rmcp's `Parameters<T>` extractor before any `LamboServer` code runs,
  so it is not interceptable at our layer — there is no rmcp extraction-error hook to attach
  to. Sanitising it would mean replacing `Parameters<T>` with a hand-rolled deserialize in all
  seven tools, a large and error-prone change for a field whose only reachable "byte" is an
  escaped control char in an enum slot. Not shuffled back — intentionally left as-is, and this
  record stands in place of the previous undated source comment (`src/mcp/server.rs:71-82`) as
  the authoritative accepted-rationale.
- **Residual #2 — `redact_urls` misses a bare `host:port`.** ACCEPTED (2026-08-15). The
  matcher handles `scheme://…` tokens only. Widening it to `host:port` would over-redact
  ordinary warning text that happens to be word:number (`ratio 3:4`, `line 42:10`,
  SQLSTATE-style codes), corrupting the very messages it exists to keep readable; and there is
  currently no live emitter of a schemeless endpoint — every store/embedder endpoint the
  warnings log is a full URL. Latent with no reachable path today; should a future warning emit
  a bare `host:port`, it should be redacted at that source where the shape is known rather than
  by widening this heuristic (`src/mcp/server.rs:284-302`). This record is the dated
  accepted-rationale the clause requires.

### T87-3 — FIXED. The HTTP transport now enforces a request-body size ceiling

`guard_request` (`src/mcp/serve.rs`) now checks the request's `Content-Length` and returns
**413 Payload Too Large** (naming the limit) for any declared body over a new
`MAX_HTTP_BODY_BYTES = 4 MiB` constant, before the body is streamed to rmcp — so a body
padded with rejected/oversized fields no longer pays parse + validation first. Tool-layer caps
(16 KiB per string, 64 concepts/call ≈ ~1 MiB) and the rate limit already contained most
amplification; this closes the transport's "no ceiling" gap for the declared-length bodies
every streamable-HTTP POST carries. A chunked body sent without `Content-Length` keeps the
tool-layer caps + rate limit as its bound (documented on the constant). **Pinned by
`an_oversized_request_body_is_refused_before_the_service`** (`src/mcp/serve.rs`), which sends
`MAX_HTTP_BODY_BYTES + 1` and asserts 413, `too large` in the body, and that the inner MCP
service is never reached — then asserts a normal-size body still gets through (200).

### T87-4 — no action (P3, accepted by review)

Left as recorded: the acknowledged session-cap overshoot-by-one and the global (not
per-`tools/call`) rate limit are accepted trade-offs already documented in the Handoff Log.
No change.

**Gate block (full binding, re-run after remediation — all green):**

```text
cargo fmt --all -- --check                                    CLEAN
cargo clippy --all-targets -- -D warnings                    CLEAN
cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings  CLEAN
cargo clippy --all-targets --features store-sqlite -- -D warnings                          CLEAN
cargo test                                                  706 lib + 5 bin + 5 int + 1 doc, 0 failed, 1 ignored
cargo test --features store-sqlite                          753 lib + 5 bin + 11 int + 1 doc, 0 failed, 1 ignored
cargo test --no-default-features --features store-sqlite --no-run    BUILDS
cargo test --no-default-features --features store-cockroach --no-run BUILDS
cargo check --no-default-features                             CLEAN
```

The +3 lib over the prior 703/750 are the three tests added by this remediation: two in
`src/cli/inspect.rs` (the graph-size guard firing + the non-firing case) and one in
`src/mcp/serve.rs` (the body-size limit).

The exact per-row counts are captured in the remediation agent's handoff note in
`dev-diary/PHASE-8-surface.md`.

— T87Remediation, 2026-08-15

# R2 reverify verdict (2026-08-15)

**Verification performed (T87Reverify):** reviewed the full uncommitted diff of
`src/cli/inspect.rs`, `src/mcp/serve.rs`, `src/mcp/server.rs`, reread every new guard/test and
the R1 dispositions against live source; re-ran the gates independently:

```text
cargo fmt --all -- --check                                    CLEAN
cargo clippy --all-targets -- -D warnings                    exit 0
cargo clippy --all-targets --features store-sqlite -- -D warnings   exit 0
cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings   exit 0
cargo test --lib (default)                                   706 passed / 1 ignored
cargo test --lib -- cli::inspect::tests                     2 passed (cap+1 fires-only-fuzzy; within-cap resolves)
cargo test --lib -- oversized_request_body                   1 passed (413 + inner not reached + normal 200)
```

**Per-finding check:**
- **T87-1 (P1) — FIXED, real and sound.** `resolve_focus` (`src/cli/inspect.rs:32`, `:87-98`)
  counts `g.concepts().count()` (the exact set the fuzzy leg iterates — not a bypassable
  proxy) and returns `Focus::Oversized { cap }` **before** the `to_lowercase` pass at
  `:100-108`. The exact (case-insensitive, `eq_ignore_ascii_case` — no lowercase alloc) and
  node-id legs at `:64-85` run before the guard and are NOT gated. Refusal is honest at both
  call sites: CLI `inspect::run` (`:299-302`) returns a `Runtime` error naming the cap; MCP
  `inspect_impl` (`server.rs:973-984`) returns a tool error directing to an exact focus /
  node_id. Both tests fire discriminately: `a_graph_past_the_scan_cap_refuses_only_the_fuzzy_leg`
  would panic (get `Missing`, not `Oversized`) if the guard were removed; the within-cap test
  pins non-spurious firing. No lock held across `.await` (the whole read-lock block is sync),
  no new panic path, `count()` adds no allocation.
- **T87-2 (P2) — CLOSED-ACCEPTED. Met.** Dated **2026-08-15** accepted-rationales for #1
  (rmcp `Parameters<T>` `-32602`, not interceptable at our layer) and #2 (`redact_urls` bare
  `host:port`, latent no-emitter, widening risks over-redacting) are recorded in this review
  file (R1 `:258-274`), reasoning sound with no false claims. Clause (d) satisfied.
- **T87-3 (P2) — FIXED.** `guard_request` (`serve.rs:536-551`) checks declared
  `Content-Length` and returns **413 Payload Too Large naming `MAX_HTTP_BODY_BYTES` (4 MiB)**
  before `next.run(req)` streams the body to rmcp. Test `an_oversized_request_body_is_refused_before_the_service`
  asserts 413 + body names `too large` + inner service never reached (`reached == 0`) + a
  normal body still 200. Removing the check fails the test. Chunked bodies without
  Content-Length are honestly documented on the constant as bounded by tool-layer caps +
  rate limit.
- **T87-4 (P3) — no action taken; correct (accepted by the review).**

**No new defect / no scope creep:** diff touches only T8.7-owned paths plus the shared
`resolve_focus` residual (`src/cli/inspect.rs`, explicitly owned by this task) and the
phase/review docs. **No `Cargo.toml` change, no new dependency.** Surface unchanged: still
7 tools (`lambo_derive/inspect/recall/record_action/reserve/saints/stats`), stdio untouched
(the new body check lives only in the HTTP `guard_request` path). No unbounded allocation is
introduced by any guard. Exhaustive match on the new `Focus::Oversized`.

**"Done when" (a)-(d): all met** — unauthenticated non-loopback refused, rate limit +
concurrent-session cap (with tests) intact from the minimal cut, residual #3 fixed by the
paired graph-size guard, residuals #1/#2 closed with dated accepted-rationales in the review
file.

**Verdict: CLEAN. Zero P1/P2.**

— T87Reverify, 2026-08-15

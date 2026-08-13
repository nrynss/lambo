# Adversarial Review: T8.2 — MCP server (opus, deep)

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN — round R1                                         ║
║  Verdict: REQUEST CHANGES                                        ║
║  Findings: 3 P1 / 6 P2 / 7 P3                                    ║
║  Opened: 2026-08-14                                              ║
╚══════════════════════════════════════════════════════════════════╝
```

**Task:** T8.2 — MCP server (spec §6.2/§6.3/§2.2; PHASE-8-surface.md)
**Branch reviewed:** `phase/p8-surface` @ `fafb739` (implementing commit)
**Scope:** `src/mcp/server.rs`, `src/mcp/serve.rs`, `src/mcp/mod.rs`, `src/main.rs`,
`Cargo.toml`, `dev-diary/evidence/t8.2-mcp-client/`
**Method:** clause-by-clause read against the T8.2 spec block, the T8.1 review's three "For
T8.2" constraints and the F18 carryover; then **13 live experiments** against the real
binary over the real MCP wire protocol (stdio + streamable HTTP), plus **4 source mutations**
to measure what the test suite actually pins. All mutations reverted; `git status` clean and
`cargo test` re-run green before writing this.

**Gates verified independently on the clean tree:**
`cargo fmt --all -- --check` clean · `cargo clippy --all-targets -- -D warnings` clean ·
same with `--features store-cockroach,store-memory,fixtures` clean ·
`cargo check --no-default-features` clean ·
`cargo test` **562 lib + 2 bin + 3 integration + 1 doc, 0 failed, 3 ignored** — matches the
claimed baseline exactly. `Cargo.lock` carries **one** `reqwest`, `0.12.28` — the duplication
trap the rmcp ruling exists to avoid is genuinely avoided.

---

## Findings

### T82-1 (P1) — `close()` does **not** run on SIGINT/SIGTERM under `--transport stdio`; the tail is dropped — CONFIRMED (demonstrated)

`src/mcp/serve.rs:173-186` (`serve_stdio`) vs `:107-112` (its own rustdoc) and
`:221-244` (`shutdown_signal`, wired to HTTP only).

`serve_stdio` awaits `service.waiting()` and nothing else. There is no signal handling
anywhere on the stdio path — not in `serve_stdio`, not in `serve()`, not in `main.rs`. The
default disposition for SIGINT and SIGTERM therefore terminates the process outright:
`event_pump.abort()` never runs, `mem.close()` never runs, `Memory`'s `Drop` never runs. The
whole write-behind tail (`log_depth` + the flush task's `pending`) dies with the process.

This directly falsifies the module's own guarantee, `src/mcp/serve.rs:109-111`:
> `Memory::close` runs on **every** exit path — clean client disconnect, Ctrl-C, or a
> transport error

Ctrl-C is exactly the case that does not hold.

**Reproduction** (`/tmp/.../t82/sig2.py`, spawning with `preexec_fn=os.setsid` so signal
dispositions are the real defaults, not a background-job artifact):

```
[stdio-int]  exited rc=-2  after 0.00s on SIGINT    close() ran: False
[stdio-term] exited rc=-15 after 0.00s on SIGTERM   close() ran: False
[http-int]   exited rc=0   after 0.00s on SIGINT    close() ran: True
[http-term]  exited rc=0   after 0.00s on SIGTERM   close() ran: True
```

Last stderr line in both stdio cases is `lambo serve: session attached`; the
`session closed, tail durable` line that the evidence README quotes as proof of durability is
absent. HTTP, which *is* wired to `shutdown_signal`, is correct.

**Why it bites in practice, not only in theory.** The MCP stdio shutdown sequence is
close-stdin → wait → SIGTERM → SIGKILL. The stdin-EOF path does close correctly (verified —
see Verified holds), so the happy path is fine; but any server slower than the client's grace
window, any `kill` from a supervisor, and every human who runs `lambo serve` in a terminal and
presses Ctrl-C loses the tail silently. T8.1 spent three remediation rounds making `close()`
cancellation-safe; that work is bypassed entirely here.

**Fix shape:** `tokio::select!` `service.waiting()` against `shutdown_signal()` in
`serve_stdio`; on the signal arm, cancel the service (or simply drop it) and return `Ok(())`
so the existing `mem.close()` in `serve()` runs. Pin it with a test.

---

### T82-2 (P1) — HTTP graceful shutdown blocks forever on an open SSE stream, so `close()` never runs there either — CONFIRMED (demonstrated)

`src/mcp/serve.rs:192-218` (`serve_http`), specifically
`axum::serve(listener, app).with_graceful_shutdown(shutdown_signal())` combined with
`cfg.sse_keep_alive = Some(Duration::from_secs(15))` at `:201`.

`with_graceful_shutdown` stops accepting and then **waits for every in-flight connection to
finish**. A streamable-HTTP MCP client opens `GET /mcp` as its server→client SSE channel and
holds it open for the life of the session; `sse_keep_alive` guarantees it never idles out.
That connection never finishes, so `axum::serve` never returns, so `serve()` never reaches
`mem.close()`. There is no shutdown timeout and no forced-close fallback.

**Reproduction** (`/tmp/.../t82/sse.py`): POST `initialize` → capture `mcp-session-id` → open
a raw-socket `GET /mcp` with `Accept: text/event-stream` and that session id (server answers
`HTTP/1.1 200 OK … content-type: text/event-stream`) → SIGTERM:

```
--- sending SIGTERM with the SSE stream still open ---
DID NOT EXIT within 20s of SIGTERM (graceful shutdown blocked by the open SSE stream)
close() ran: False
last: INFO lambo::mcp::serve: mcp http: listening on /mcp addr=127.0.0.1:7801
```

Note this is the *same* guarantee as T82-1 failing by a second, independent mechanism — the
earlier `[http-term] close() ran: True` result only held because that probe had no SSE stream
open, i.e. no real MCP client attached. With a real client attached, **neither transport
closes the session on a signal.** T8.5 will serve exactly this shape.

**Fix shape:** bound the graceful phase — `tokio::time::timeout(GRACE, axum::serve(..))`, or
select the serve future against a deadline armed by `shutdown_signal()` — and fall through to
`mem.close()` when it expires. Log the forced close. Pin with a test that holds an SSE stream
open.

---

### T82-3 (P1) — `lambo_reserve` grants and releases **other agents'** locks and reports success; the §11 soft lock provides no mutual exclusion across MCP clients — CONFIRMED (demonstrated)

`src/mcp/server.rs:465-515` (`lambo_reserve`), `:231-246` (`attribution`).

The task agent flagged the underlying cause (per-call `agent_id` cannot reach the graph) and
judged an `attribution:` warning to be adequate disclosure. **It is not.** The warning is
attached to a result whose text says the operation *succeeded*, and it is placed where the
model never reads it. The tool does not merely fail to detect contention — it affirmatively
asserts a safety property it does not provide.

**Reproduction** (`/tmp/.../t82/reserve.py`, one server, `--agent agent-a`, real stdio wire):

```
A reserves          -> reserved 7a893720-… until … for agent 'agent-a' | isError False
B reserves SAME node-> isError: False
   text: reserved 7a893720-… until … for agent 'agent-a'
   TEXT CONTAINS 'attribution': False
C releases A's lock -> isError: False   released 7a893720-…
```

Three distinct consequences, each independently a defect:

1. **`agent-b` "reserves" a node `agent-a` already holds, and is told it succeeded.** The
   §11 conflict cannot fire because `graph::reserve` sees one `AgentId`. A model that follows
   the tool description ("Take a soft lock on a memory node before editing it") will edit
   concurrently, believing it holds the lock.
2. **`agent-c` releases `agent-a`'s lock and is told it succeeded.** `Memory::release`'s
   non-owner `Conflict` guard is unreachable through MCP for the same reason — any client can
   free any other client's lock.
3. **The `attribution:` warning is in `structuredContent.warnings` only, never in the text
   content.** MCP clients feed `content` to the model; `structuredContent` is optional and
   commonly not surfaced. The one thing `content` *does* say is `for agent 'agent-a'` — which
   reads to agent-b as confirmation, not as a caveat.

The disclosure therefore papers over the hole in the place where it matters. The full fix is
the `Memory::reserve_as` / `release_as` change the Handoff Log proposes (a T8.1 re-open, and
the right long-term answer). **But T8.2 can and must fail closed today without touching
`src/memory.rs`:** when `agent_id != self.mem.agent()`, `lambo_reserve` should return
`CallToolResult::error` explaining that per-call agent identity is not yet supported and that
the reservation was *not* taken — never a success. Same for `release`. And every
`attribution:` warning belongs in the **text** content, not only in the structured block
(see T82-9).

**T8.4 owns the downstream consequence** — the spec §13 two-agent conflict line — and should
treat this as a blocker, as the Handoff Log says. This finding is about T8.2 reporting
success for an operation that did not do what it claims.

---

### T82-4 (P2) — the F18 test walks only **top-level** schema properties: a nested `created_at` survives 562/562 — CONFIRMED (mutation)

`src/mcp/server.rs:913-945` (`f18_no_tool_schema_accepts_a_client_timestamp`), specifically
`schema["properties"].as_object()` at `:929-932`.

The Handoff Log claims the test "walks every *published* tool schema and fails on a property
named …". It does enumerate the published schemas (`tool_router.list_all()`, not a hand-written
list) — that half of the claim is true and good. But it inspects only the **root**
`properties` map. `lambo_derive`'s real published schema (captured in
`stdio-tools-list.jsonl`) puts `WireConcept` and `WireParentOf` under `$defs`, reachable only
via `properties.concepts.items.$ref`. Nothing under `$defs` is ever examined.

**Mutation A** — add `pub created_at: Option<String>` to `WireConcept`
(`src/mcp/server.rs:103`):

```
cargo test --lib mcp::server::tests::f18  ->  ok (1 passed)
cargo test --lib                          ->  562 passed; 0 failed
```

A client-supplied timestamp on every derived concept passes the entire suite.

**Mutation B** (control) — the same field on `RecallParams` (top level) → the test FAILS with
`F18: tool lambo_recall accepts 'created_at'`. So the test discriminates correctly *within*
the surface it inspects; it just inspects one level.

**Mutation C** — `client_clock_ms`, `as_of`, `ts` on `DeriveParams` (top level, names outside
the denylist) → **12/12 mcp tests pass**. The check is a denylist of nine spellings; F18 is a
statement about *any* client-supplied logical time.

**Fix shape:** walk the whole schema document recursively (root `properties` **and** every
`$defs.*.properties`, and any `items`/`additionalProperties` sub-schema), and switch from a
name denylist to a **golden allowlist**: assert the exact set of property paths each tool
publishes. Any new field on any tool then trips the test and forces a human decision, which is
what F18 actually needs.

**Note:** F18 itself *holds at the value level* — independently verified. `Memory::begin_interaction`
(`src/memory.rs:1470-1484`) stamps `created_at = Utc::now()` under the graph write lock, all
three write paths route through it, and `prompt_text` is never parsed for a date anywhere in
`src/`. I could find no route by which a client influences `created_at` today. This finding is
that the *guard* is much weaker than advertised, not that the guard is currently breached.

---

### T82-5 (P2) — no panic containment at the MCP boundary: a panicking handler silently drops the JSON-RPC response and the caller hangs forever — CONFIRMED (mutation)

`src/mcp/server.rs:249-681` (every `#[tool]` body), `src/mcp/serve.rs:173-186`.

No handler is wrapped in `catch_unwind`. T8.1's flush path armours every store attempt with
`CatchUnwindPoll` + `panic_message` (and T81-2 escalated the *absence* of that armour to a
finding); the MCP boundary — which is fed arbitrary client input — has no equivalent.

**Mutation** — `panic!("MUTATION-PANIC internal detail: dsn=postgres://user:SECRET@host/db")`
at the top of `lambo_stats`, driven over the real stdio wire (`/tmp/.../t82/panic.py`):

```
panicking call response: TIMEOUT        (no response of any kind, 8s)
subsequent call:         {"id":3, … "0 canonical memories in session 's-panic'"}
exit rc: 0   close ran: True
panic leaked to stderr: True
```

So: the session and `close()` survive (good), and the panic payload does **not** cross the
protocol to the client (good). But the request id is answered by nothing at all — no result,
no JSON-RPC error, no cancellation. The calling model blocks until its own timeout, with no
signal about what happened. The panic text, including anything a future panic message
interpolates, lands on stderr, which under stdio is the stream the MCP client collects into
its logs.

**Confidence:** the containment gap is proven. **Reachability is not** — I probed malformed
params, wrong types, unknown enum variants, missing required fields, `top_k`/`depth`
boundaries, unknown UUIDs, reflexive `parent_of`, a 2 MiB payload and a 1 MiB query, and found
no naturally reachable panic (see Verified holds). Rank it as defence-in-depth on an
attacker-facing boundary, not as a live crash.

**Fix shape:** one shared helper wrapping each handler body in
`futures::FutureExt::catch_unwind` (or the existing `CatchUnwindPoll`), converting a panic
into `CallToolResult::error("internal error")` with the detail logged at ERROR to stderr only.

---

### T82-6 (P2) — `lambo_record_action` accepts unbounded input while `lambo_derive` is bounded at 16 KiB — CONFIRMED (demonstrated)

`src/mcp/server.rs:403-453` vs `:330-394`; the bound that does exist lives at
`src/graph/hybrid.rs:271` (`MAX_HYBRID_CONTEXT_BYTES`), i.e. in the hybrid *derive* path only.

`MAX_CONCEPTS_PER_DERIVE` caps the *count* of concepts, never the size of any string. Derive
survives that only because the hybrid path independently refuses inputs over 16384 bytes.
`record_action` is the sync/canonical path and has no such guard, and neither does
`lambo_recall`'s `query`.

**Reproduction** (`/tmp/.../t82/wire.py`, `burst.py`, real stdio wire):

```
2 MiB concept via lambo_derive        -> isError True  "hybrid derive input exceeds 16384 bytes"
2 MiB action  via lambo_record_action -> isError False "recorded action 'AAAA…'"
1 MiB query   via lambo_recall        -> isError False (embedded and served)
```

A 2 MiB `Resource` concept is now in the RAM graph, in the mutation log, and headed for the
store. One client can grow the process the whole session shares, without limit, through the
tool that has no guard. It is also an inconsistent contract: the same string is refused by one
write tool and accepted by another.

**Fix shape:** a `MAX_CONTENT_BYTES` in `src/mcp/server.rs` applied uniformly to every
client-supplied string (`concepts[].content`, `parent_of.*`, `action`, `produces`/`modifies`/
`depends_on` entries, `query`, `focus`), sized at or below `MAX_HYBRID_CONTEXT_BYTES` so the
MCP surface refuses before the graph does. This is the same class as the existing `MAX_*`
constants (self-flag #3) — those are reasonable; the gap is that they cover counts and not
sizes.

---

### T82-7 (P2) — `lambo_inspect`'s substring resolution silently picks an arbitrary concept, and the choice changes across and within runs — CONFIRMED (demonstrated)

`src/mcp/server.rs:542-555`. Self-flag #2, escalated: the agent called this "non-deterministic
across runs"; it is worse, and the consequence is not confined to `inspect`.

`g.concepts()` iterates `Graph::nodes`, a `std::collections::HashMap`
(`src/graph/graph.rs:49`, `:898-903`). Both fallback legs use `.find(..)` over that iterator,
so the exact-content leg picks arbitrarily among case-insensitive duplicates and the substring
leg picks arbitrarily among *all* substring matches. `HashMap` order also reshuffles on
resize, so a `derive` between two `inspect` calls can change the answer inside one process.

**Reproduction** (`/tmp/.../t82/insp.py`: derive `auth middleware`, `auth middleware rewrite`,
`legacy auth middleware shim`; then `lambo_inspect(focus="auth")` three times with a derive
between each):

```
run 0: ['legacy auth middleware shim', 'legacy auth middleware shim', 'legacy auth middleware shim']
run 1: ['auth middleware',             'auth middleware',             'auth middleware']
run 2: ['legacy auth middleware shim', 'legacy auth middleware shim', 'auth middleware rewrite']
```

Nothing in the response tells the caller a fuzzy match happened: the block just reads
`focus: legacy auth middleware shim [Resource]`. An agent that asked about `auth`, got the
shim, and then fed the returned `node_id` into `lambo_reserve` or an edit is now operating on
a concept it never named. That is how a resolution nit becomes a write bug.

**Fix shape:** make the substring leg deterministic **and** loud — collect all matches, sort
by a total order (shortest content, then `node_id`), and either (a) refuse when there is more
than one match, listing the candidates so the model can disambiguate, or (b) return the best
match with an explicit `resolved '<focus>' → '<content>' (N other matches)` line in the *text*
content. Refusing is the safer default given the reserve path.

---

### T82-8 (P2) — the evidence does not support "all seven tools driven end-to-end"; three tools were never driven over the wire — CONFIRMED

`dev-diary/PHASE-8-surface.md:779-782` (Handoff Log) vs
`dev-diary/evidence/t8.2-mcp-client/stdio-jsonrpc-session.jsonl`.

The Handoff Log states: *"**All seven tools were driven end-to-end over the real MCP wire
protocol** (`initialize` → `lambo_derive` → `lambo_record_action` → `lambo_recall` →
`lambo_stats`)"* — a sentence whose own parenthetical names four tools. Parsing the transcript
confirms the parenthetical, not the claim:

```
stdio-jsonrpc-session.jsonl: 5 frames — initialize, then 4 tool results:
  lambo_derive, lambo_record_action, lambo_recall, lambo_stats
never driven over the wire: lambo_reserve, lambo_inspect, lambo_saints
```

Two secondary honesty problems in the same artifact:

- **Both JSONL files contain responses only, no requests.** Nothing in the evidence records
  what was *sent*, so the transcripts cannot be independently checked against the arguments
  claimed (the README quotes an `agent_id: "agent-b"` call whose request frame is absent).
- The T8.2 spec block requires fail-closed on **"store×embedder dims disagree"**; the
  evidence's fail-closed table covers unknown key, uncompiled kind, bad transport and missing
  `--session`, but not the dim mismatch. (The mechanism is unit-tested in `resolve.rs` and is
  unenforceable under default features anyway, since `MemoryStore::vector_dimensions()` is
  `None` — so this is an evidence-coverage gap, not a hole in the code.)

Everything else the evidence claims, I re-verified and it holds — see Verified holds. The
"NOT verified: a model-driven tool call" disclosure is exemplary and is **not** overstated
anywhere I could find; the README's "Read this honestly" paragraph about the missing
canonical/⚑/conflict lines is likewise accurate.

**Fix shape:** drive `lambo_reserve`, `lambo_inspect` and `lambo_saints` over the wire and
append the frames (or correct the sentence to name the four). Capture requests alongside
responses.

---

### T82-9 (P2) — every warning is `structuredContent`-only; the model reading `content` never sees any of them

`src/mcp/server.rs:312-318`, `:386-392`, `:444-451`, `:506-513`, `:567-572`, `:617-622`,
`:659-678` — in all seven tools, `warnings` is written into `structured_content` and never
into the text `ContentBlock`.

This is the mechanism behind T82-3, but its blast radius is wider: `lambo_recall` folds
`result.warnings` into the same structured-only channel (`:295`), and those include
`Memory::recall`'s **embed-failure degradation warning** — the signal that a recall silently
dropped its vector leg and returned keyword-only results. A model reading the context block
sees a normal-looking block and cannot tell that recall degraded.

**Reproduction:** `/tmp/.../t82/reserve.py` — `recall text (agent-b) contains 'attribution':
False`, with the warning present in `structuredContent.warnings`.

**Fix shape:** append warnings to the text content as well (for `lambo_recall`, as a trailing
block after the verbatim T5.3 text, clearly delimited so the block itself stays verbatim —
or as a second `ContentBlock::text`, which keeps `content[0]` exactly the context block and
preserves the existing test's contract).

---

### T82-10 (P2) — `serve` against SQLite/Cockroach dies with a raw driver error and no remedy — CONFIRMED (reproduced)

The task agent's second flagged finding, confirmed and partially escalated.

```
$ lambo --config sqlite.toml serve --session s-sqlite --transport stdio
lambo serve: backend: lookup session: error returned from database: (code: 1) no such table: sessions
```

The *ownership* reasoning is right and I would not overturn it: `lambo provision` (T8.3) owns
schema bootstrap, `GraphStore::init_schema()` has no non-test caller in `src/`, and adding
auto-init to `serve` would preempt T8.3 and duplicate the provision path. The disclosure is
adequate as an ownership statement.

What is *not* adequate is the user-facing failure. The primary artifact of the whole project,
pointed at either of the two durable stores, exits with an SQLite error string and no
indication that a bootstrap step exists. That is a T8.2-owned message, fixable without
touching T8.3's code: detect the missing-schema shape at attach and emit
`session store is not provisioned — run 'lambo provision' (or scripts/provision.sh) first`.
**If deferred, T8.3 inherits it** — but T8.3 landing does not fix the message.

---

### T82-11 (P3) — params structs lack `deny_unknown_fields`: a client-sent `created_at` is silently accepted and ignored — CONFIRMED (demonstrated)

`src/mcp/server.rs:88-168` (all seven params structs). schemars emits no
`additionalProperties: false`, and serde ignores unknown fields by default.

```
lambo_derive {..., "created_at": "1999-01-01T00:00:00Z"}                  -> isError False
lambo_derive {concepts:[{..., "created_at": "1999-01-01T00:00:00Z"}]}     -> isError False
```

Harmless today — the value is discarded, F18 holds. But the F18 posture is "ignored" rather
than "refused", and a client that believes it backdated an interaction is told it succeeded.
`#[serde(deny_unknown_fields)]` on all seven structs converts this into a loud refusal for one
line each, and makes the surface fail closed the way the rest of Level B does. Cheap; take it.

### T82-12 (P3) — `--config` cannot reach any product knob

`src/config.rs:206-213`: `LamboFile` carries `store` + `embedder` only. `MemoryBuilder::config()`
exists but `mcp::serve::build_memory` (`src/mcp/serve.rs:85-95`) never calls it, so every
`lambo serve` runs on `Config::default()`. `--config` therefore selects backends and nothing
else — `default_top_k`, `match_strategy`, `canonization_edge_min_age`, the scoring weights and
every timing knob are unreachable from the command line.

It fails *closed* (`deny_unknown_fields` rejects `[config]`-shaped keys with a parse error), so
it is not silently misleading — hence P3, not P2. But T8.4's phase-doc requirement to use a
"config-shortened `canonization_edge_min_age`" is not expressible through `serve`; T8.4's
`src/cli/demo.rs` will have to build its own `Memory` to get there. Worth a Handoff Log line so
T8.4 does not discover it late.

### T82-13 (P3) — `--agent` defaults to `lambo-serve`, so the attribution warning fires on the default configuration

`src/main.rs:29`. Any real client sending its own `agent_id` mismatches the process owner, so
the `attribution:` warning is the *normal* case rather than the exception, which is exactly how
a warning stops being read. Not a defect on its own; it compounds T82-3.

### T82-14 (P3) — read tools answer normally after `close()`

`lambo_stats` / `lambo_saints` / `lambo_inspect` call `Memory::stats()` /
`canonical_memories()` / `graph()`, none of which check `closed` (`src/memory.rs:1107`,
`:1134`). Only `recall` gates (`:1055`, `ensure_open`). The existing test
`a_closed_session_refuses_writes_through_the_tools` covers `lambo_derive` only. Answering from
a closed session's RAM graph is defensible, but it is undocumented and untested at this
surface — one doc line, or a `session is closed` note on the result.

### T82-15 (P3) — `lambo_inspect`'s node budget marks neighbours `seen` after exhausting it

`src/mcp/server.rs:740-746`: `seen.insert(other)` runs **before** the `budget == 0` check, and
the `break` exits only the inner edge loop, so the outer frontier loop keeps consuming (and
permanently excluding) one neighbour per remaining frontier node. Output impact is nil — the
hop loop breaks immediately after and the `(truncated at 200 neighbours)` note is emitted
correctly — so this is a latent trap for whoever changes the loop next, not a live bug. Move
the budget check above the `seen.insert`.

### T82-16 (P3) — HTTP transport is unauthenticated, unrate-limited, and creates MCP sessions unboundedly

`src/mcp/serve.rs:192-218`. Self-flag #5, confirmed and slightly widened: beyond "no auth",
there is no request-size limit, no rate limit, and `LocalSessionManager` mints a session per
`initialize` with no cap. Combined with T82-6 (unbounded `record_action` content) that is a
remote memory-exhaustion path on a session *writer*. The loopback default and the flag's
rustdoc are real mitigations and this is a hackathon artifact, so P3 — **but T8.5 inherits it
the moment the demo app is served on a public URL**, and P9 should not ship `--bind 0.0.0.0`
without at least a shared-secret header and a body-size layer.

### T82-17 (P3) — `event_pump.abort()` precedes `close()`, so shutdown-time daemon events are never logged

`src/mcp/serve.rs:136-139`. Self-flag #4, confirmed as harmless: the task only logs, and the
`Drop` of the receiver is clean. Worth 30 seconds to move the abort after `close()` (or to
`select!` the pump against the close future) so canonization/conflict events emitted during
the final drain still reach stderr — that is exactly the window an operator debugging a failed
close wants to see.

---

## Verified holds (attacked, did not break)

- **F18 at the value level.** `begin_interaction` (`src/memory.rs:1470-1484`) stamps
  `Utc::now()` under the graph write lock; `derive`, `record_action` and `demote` all route
  through it; `hybrid.rs:289` reads `created_at` from the interaction node, never from input.
  `prompt_text` is stored, never parsed. No tool exposes a timestamp in any published schema
  (checked against the captured `stdio-tools-list.jsonl`, all seven). No route found by which
  a client influences `created_at` — including unknown extra fields, which are discarded. The
  `initialize` instructions do tell the model not to send one. **The guard is weak (T82-4);
  the property currently holds.**
- **Level B, single construction site.** Exactly one `resolve_from_config_path`, in
  `main.rs:111`; `build_memory` takes `ResolvedBackends`, so a second resolve is not
  expressible through the API. One `Memory::builder()` in the serve path; `LamboServer` holds
  `Arc<Memory>` and the HTTP factory clones the `Arc` — no second `Memory`, honouring the T8.1
  constraint.
- **`--config` precedence is exactly as specified.** Empirically: flag beats `LAMBO_CONFIG`
  beats `./lambo.toml` beats defaults; a nonexistent explicit path fails closed
  (`read /nope/nothing.toml: No such file or directory`) rather than silently falling back to
  defaults. Unknown TOML key → parse error, exit 1. Uncompiled `store.kind = "cockroach"` →
  the named remedy, exit 1. Bad `--transport` → exit 2. Missing `--session` → clap, exit 2.
  All exit before a session is attached.
- **`events()` called exactly once**, at `src/mcp/serve.rs:121`, before either transport runs;
  no other call site exists outside `memory.rs`/`canon` internals and tests. The T8.1
  constraint is met.
- **No synonym tool is exposed at all**, and `lambo_reserve` carries an explicit
  non-durability warning. The T8.1 "do not assume synonym/reservation durability" constraint is
  honoured (the warning's *placement* is T82-9's problem, not this one's).
- **Lock discipline (spec §6.4).** `src/mcp/` takes exactly one lock directly —
  `self.mem.graph().read()` at `server.rs:538` — inside a block with no `.await`;
  `render_neighbourhood` is a sync free function. No graph guard is held across any await
  anywhere in the new code.
- **Concurrency.** 30 pipelined `tools/call` requests (derive/recall/inspect/stats/saints,
  mixed `agent_id`s) over one stdio session: 30/30 answered in 0.09s, responses interleaved
  out of order (so genuinely concurrent), no deadlock, no panic, no lost id.
- **Error paths are civil.** Unknown tool → `-32602 tool not found`. Wrong types, missing
  required fields and unknown enum variants → readable `isError: true` results naming the
  field. `top_k: 0`, `top_k: 10000`, `depth: 99` → the documented bounds. Reflexive
  `parent_of` → the graph's own invariant error. Unknown UUID → `not found`. Zero panics
  across every probe.
- **stdin-EOF shutdown is correct** on stdio: `mcp stdio: client disconnected reason=Closed`
  followed by `session closed, tail durable`. This is the path the evidence exercised, and it
  works — T82-1/T82-2 are about the paths it did not.
- **rmcp rung.** `rmcp 3.1.2`, `default-features = false`, the four ruled features; `Cargo.lock`
  resolves a single `reqwest 0.12.28`. `#[tool_handler(router = self.tool_router)]` is used, so
  the router is built once in `new()` — no `field 'tool_router' is never read` warning under
  `-D warnings`. `get_info` is overridden as the ruling requires.
- **stdout is clean under stdio.** `init_tracing` pins the subscriber to stderr and is called
  before anything can log; every probe's stdout carried only JSON-RPC frames.
- **The seven-tool set and its schemas.** Exactly the spec §6.2 names; every tool requires
  `agent_id` and carries a description; pinned by tests that would fail on a rename.

## Rulings on the seven self-flags

| Self-flag | Ruling |
|---|---|
| #1 protocol dispatch untested in-process | **Accepted as a documented trade-off.** The registration + harness-arm guard pair is a reasonable substitute, and the manual JSONL session plus my own 13 wire experiments cover the dispatch path. Not a finding. |
| #2 `lambo_inspect` fallback resolution | **Escalated → T82-7 (P2).** Worse than flagged, and it feeds node ids into the reserve path. |
| #3 `MAX_*` limits are invented | **Accepted** as reasonable defence for a shared process — but they cover counts, not sizes → **T82-6 (P2)**. |
| #4 `event_pump.abort()` | **Confirmed, trivial → T82-17 (P3).** |
| #5 HTTP unauthenticated | **Confirmed → T82-16 (P3)**, and it is also the vehicle for **T82-2 (P1)**. |
| #6 failed `close()` not retried | **Accepted.** Correct for a process that is exiting; the error is surfaced and logged. The real durability problem is that `close()` does not run at all on signals — T82-1/T82-2. |
| #7 `lambo_reserve` carries a `release` boolean | **Accepted.** Keeping the published set at exactly seven names is the right call; the boolean is a mild smell, not a defect. |

## Rulings on the two raised cross-ownership findings

**(a) Per-call `agent_id` cannot reach the graph.** Independently **CONFIRMED** — `Memory`
binds one `AgentId` at `build()` and every write passes `self.agent`. The root-cause analysis
and the proposed `derive_as`/`reserve_as` fix are correct and belong to a T8.1 re-open. **The
chosen disclosure is NOT adequate** (T82-3): a structured-only warning attached to a
success result lets `lambo_reserve` grant and release other agents' locks while reporting
success. T8.2 must fail closed on a foreign `agent_id` for the reserve/release path and move
the warning into the text content, regardless of when the `Memory` change lands.

**(b) No schema bootstrap for a SQLite/Cockroach serve.** Independently **CONFIRMED** and
reproduced. **The ownership disclosure is adequate** — T8.3 owns `lambo provision`, and
auto-init in `serve` would rightly be scope theft. **The error message is not** → T82-10 (P2).

---

## Disposition

**REQUEST CHANGES.** Three P1s, all in the same theme the phase exists to protect: the
durability guarantee the module documents in its own rustdoc does not hold on either transport
under a signal (T82-1, T82-2), and a tool reports success for a mutual-exclusion guarantee it
does not provide (T82-3). The P2 set is dominated by tests and evidence claiming more coverage
than they carry (T82-4, T82-8) and by unbounded/ambiguous client input (T82-6, T82-7).

The work underneath is strong: the rmcp rung landed at the top of the ladder with a single
`reqwest`, Level B is genuinely single-construction and fail-closed, lock discipline is clean,
the concurrency path holds under a burst, and the "NOT verified" disclosure about the
model-driven call is honest to the letter. Fixing the three P1s is a small amount of code in
`serve.rs` and `server.rs`; none of them force new scope.

**Round R1 opened 2026-08-14.** Remediation should mark each finding in this file and record
which of T82-12/T82-16 (if deferred) is inherited by T8.4/T8.5 respectively.

**Tree state at review end:** all four mutations reverted, `git status` clean, gates re-run
green (562 lib + 2 bin + 3 integration + 1 doc, 0 failed, 3 ignored).

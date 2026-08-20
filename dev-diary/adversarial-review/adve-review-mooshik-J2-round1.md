# Adversarial review — mooshik J2, round 1

**Reviewer**: independent adversarial reviewer (Opus 5), agent_id `j2-reviewer-r1`. Did not
write the code under review.
**Scope**: all four commits `19f51d3..HEAD` on `wt/j2` —
`7f51bb6` (stage 1, `session_leases.endpoint`), `8e64fc8` (stage 2, every holder binds a
session endpoint), `275b418` (stages 3+4, the proxy and `resolve_role`), `40791e2`
(stage 5, the §J2 status note and doc mirrors). 23 files, +3278/−102.
**Worktree**: `/Users/narayan/Documents/work/lambo/.claude/worktrees/j2`, branch `wt/j2`,
based on `19f51d3`. No commit amended.
**Verdict**: **REQUEST_CHANGES** — one P1 (an in-flight forwarded request is never answered
when the holder's connection closes, falsifying the "never hangs" promise J2 exists to
deliver), seven P2, thirteen P3. The design is sound and the declared deviation was
necessary — I reproduced the hang it exists to prevent. The blockers are gaps between what
the design argues and what the code does, not errors in the argument.

## Method

1. `lambo_recall` as `j2-reviewer-r1` on "J2 proxy wedge invariant handshake replay" —
   30 hits carrying the orchestrator's guidance (wedge invariant, scope cut, socket-path
   scheme, ordering decision, honest-failure text) and the implementor's own derived
   corrections (the measured handshake-replay deviation, the reconnect rule). Graph read as
   context; adjudication is against spec → phase doc → source, in that order.
2. Read `§J2` spec, the folded J0 catches, the J1 residual handover, the `J2 Status —
   landed` note with its three sweep tables, `Done when`, and `What J does not change`.
3. `git show` each of the four commits, non-test hunks before test hunks.
4. Independent re-derivation of all three sweeps with `rg` over the whole tree.
5. Targeted attacks on the handshake replay, the byte pipe's framing and cancellation
   safety, socket lifecycle/TOCTOU, the acquire→publish→bind ordering across all three
   stores, and the wedge invariant by inspection.
6. Three verification-only mutations, run and reverted (declared under Gate results): the
   handshake replay disabled, `dial()`'s row re-read short-circuited, and the proxy's
   shutdown future replaced with `pending()`.
7. Gates re-run from scratch in the worktree: fmt, clippy on four feature sets, four test
   profiles, `verify.sh`, and the declared negative control.

**Order of authority.** Spec § → phase-doc status note → source → graph. Where the graph and
the code disagree the code is what ships and the disagreement is itself a finding: one such
drift is J2-R1-3 (the endpoint directory's per-uid discriminator, present in the recorded
decision, absent from the code and from the note written after it).

## Per-item verification

### §J2 design items (spec bullets + the status note's "What shipped" / "Design decisions")

| # | Item | Verdict | Evidence |
|---|---|---|---|
| 1 | `session_leases` gains a nullable `endpoint`; both `001_init.sql`, both SQL adapters, `MemoryStore` for parity | **PASS** | `migrations/sqlite/001_init.sql`, `migrations/cockroach/001_init.sql`; `src/store/sqlite.rs:1450-1468` (`ACQUIRE_SQL`), `:1512` (`LEASE_ROW_SQL`), `:1509` (`LeaseRowText`); `src/store/cockroach.rs:1792` (`ADD COLUMN IF NOT EXISTS endpoint STRING`); `src/store/memory.rs:407` (`read_lease`) |
| 2 | `endpoint` rides `LeaseHolder`, **not** `token()` | **PASS** | `src/store/lease.rs:127-172`; `reachable_at` is opt-in; `lease.rs:300-305` asserts `h.reachable_at(..).token() == h.token()`. Fencing-token code paths byte-unchanged — the only `current_token` edits in the diff are the two `RETURNING`/`SELECT` column lists |
| 3 | Endpoint **string** published by the very acquire that takes the lease, all three stores | **PASS** | sqlite `src/store/sqlite.rs:1450-1468` — one upsert, `endpoint = excluded.endpoint`, `RETURNING … endpoint`; cockroach `src/store/cockroach.rs` `ACQUIRE_SQL` same shape; memory `src/store/memory.rs`. `refresh_lease` routes through the same `acquire_or_refresh`, and the heartbeat clones the endpoint-carrying `LeaseHolder` (`src/memory.rs:692-701`, `:851`), so a refresh cannot NULL the column |
| 4 | Socket bound only after the lease, only by the winner | **PASS** | `src/mcp/serve.rs:952-989` (`resolve_role` fork) then `:1058-1105` (`endpoint.bind()` below the arming); the `Role::Proxy` arm returns before reaching the bind |
| 5 | `authorize_bind`'s "refusing here means no lease is taken" still literally true | **PASS** | `src/mcp/serve.rs:935-950` — `authorize_bind`, `authorize_ledger`, `SessionEndpoint::for_store` all run before `serve_builder`/`resolve_role`; `SessionEndpoint::resolve` does no I/O (`src/mcp/endpoint.rs:107-139`). The "What J2 changed" docstring section is at `serve.rs:336-353` |
| 6 | Stale-socket unlink licensed by the lease | **PASS on ordering, FAILS on the licence's premise** | Ordering is right (`bind()` is only reachable from the post-`resolve_role` holder path). The premise "a socket file at this path cannot belong to a live holder" rests on the derived path being unique per *store*, which `store_identity` does not deliver for a relative `store.path` — see **J2-R1-1** |
| 7 | `Attach`, not a new `LamboError` variant; `build` byte-identical | **PASS** | `src/memory.rs:626` `build_attach`; `build` is now a three-line delegate returning `LamboError::Conflict(held.message)` from the same `format!`. **No duplication to drift** — the attack looked for two copies of the build and there is one |
| 8 | Migration additive and idempotent | **PASS** | `ensure_column` (`src/store/sqlite.rs:1532-1553`) checks `pragma_table_info` before ALTER, so re-init over an already-converged table is a no-op — which every fresh sqlite DB exercises on every `init_schema`, in addition to `a_pre_j2_lease_table_gains_the_endpoint_column_on_init`. Cockroach uses `ADD COLUMN IF NOT EXISTS`, compile-checked under `--features store-cockroach` |
| 9 | The proxy is a byte pipe; no `Cargo.toml` change | **PASS** | `git diff 19f51d3..HEAD -- Cargo.toml Cargo.lock` is empty; `src/mcp/proxy.rs` never deserializes a forwarded frame (the only `serde_json::from_str` calls are `Handshake::observe` and `unreachable_reply`, both on frames it does not rewrite) |
| 10 | `--transport http` unchanged; a refused http serve still exits 1 | **PASS** | `src/mcp/serve.rs:867-873` (`opts.transport != Transport::Stdio` → `Err(Conflict(held.message))`, the pre-J2 message). `serve_http`'s body is untouched in the diff |
| 11 | The wedge invariant: exactly one `read_lease`, no acquire reachable post-client-byte | **PASS** | `rg -n 'acquire_lease\|read_lease' src/mcp/proxy.rs` → one `read_lease` at `src/mcp/proxy.rs:391`, zero `acquire_lease`. `resolve_role` is the only acquire site on the serve path and returns before `HubProxy::run` is entered |
| 12 | 50s worst-case startup | **PASS (number re-derived)** | `LEASE_TTL = 45s` (`src/store/lease.rs:92`) + `ELECTION_SLACK = 5s` (`src/mcp/serve.rs:772-780`) = 50s, retry cadence 1s. SIGTERM during the wait is honoured by the OS default disposition — the arming is deliberately still below `resolve_role`, and no lease is held during the wait |
| 13 | Proxy branch armed for liveness, not durability; negative control | **PASS** | `src/mcp/serve.rs:956-989`; `HubProxy::run`'s `select!` is `biased` with the shutdown arm first. Negative control reproduced independently — see Gate results |
| 14 | A released lease leaves no stale endpoint | **PASS** | `release_lease` DELETEs the row (`src/store/sqlite.rs:734-748`), then `endpoint.unlink()` runs after `run_and_close` returns (`serve.rs:1139-1145`). A live proxy then reads `None` → `"has no lease holder to forward to"` → honest per-call error |

### The traps §J2 folded in

| Trap | Verdict | Evidence |
|---|---|---|
| J0-1: the proxy branch reopens I-R2-1's hole | **PASS** | Answered rather than routed around: `src/mcp/serve.rs:956-989` argues the hole is not there (no lease, no tail, no graph) and installs the registration for **liveness**, polled first in a `biased` `select!`. `serve_pre_handshake_durability` gained a case with its own sync point `"proxying to the session holder"` — a line only the proxy emits. Negative control re-run independently: `proxy.run(std::future::pending())` turns exactly that case red (`tests/serve_pre_handshake_durability.rs:368`) while the holder case stays green |
| J0-2: unconditional binding collides with `authorize_bind` | **PASS** | Dissolved by construction, not argued around — see item 5 above. `authorize_bind`'s docstring gained a "What J2 changed" section (`serve.rs:336-353`) instead of being quietly falsified |
| J1-R2-3 / J1-R2-4 residual exposure | **PASS on honesty** | §J2 states plainly that neither residual's *attacker set* widens but both become **reachable in practice for the first time**, and that the neutralise-on-render half is now the load-bearing one. That is the answer the handover asked for, not a deferral. Not fixed here, correctly — fixing it is a rendering change, not a J2 change |
| Sweep discipline (2 planned + 1 unplanned) | **PASS with one miss** | Sweeps 2 and 3 re-derived clean (below). Sweep 1 has a real miss — **J2-R1-7** |

### The declared deviation — the recorded-initialize replay

| Question | Answer |
|---|---|
| Was the deviation necessary? | **Yes, reproduced.** Verification-only mutation: an env-gated early `return Ok(())` at the top of `Handshake::replay`, then `cargo test --test serve_proxy_multi_client a_dead_holder`. The new holder's rmcp server answers the forwarded `tools/call` with `expect initialized request, but received: Some(Request(… CallToolRequest …))` and **closes the connection**; the client's `id 4` is never answered and the test fails at its 30s bound. The orchestrator's instruction and the requirement it accompanied are genuinely mutually exclusive, and the requirement is the one that should have won |
| Is the implementation safe? | **Mostly, with two seams** — the unbounded swallow (**J2-R1-8**) and the single-line assumption (**J2-R1-12**). The `BufReader`-reuse argument is correct and load-bearing: `split_hub` takes the already-split, already-replayed-into halves precisely so the pump owns the reader that buffered the response |
| Is the staleness residual stated where a maintainer will find it? | **Yes** — at the type (`src/mcp/proxy.rs:230-238`), in §J2's status note, and in the commit message. Three places, one of them the code |

## New findings

### J2-R1-1 (P1) — an in-flight forwarded request is never answered when the holder's connection closes

`src/mcp/proxy.rs:525-533`:

```rust
FromHub::Closed => {
    tracing::warn!(generation = gen, "lambo serve: the session holder closed the connection — the next call will re-read the lease and try the current holder");
    writer = None;
}
```

Nothing answers the requests **already forwarded and still unanswered**. The
`unreachable_reply` path at `:501-506` only fires when `Self::send` *fails*; a frame that
was written successfully and then lost with the holder gets no reply and no error. The
generation filter at `:519-520` (`if gen != generation { continue; }`) is the second half of
the same hole: a response that arrives from a connection already replaced is dropped
silently, so the id it answers is never answered at all.

This directly falsifies the claim `HubProxy::run`'s own docstring makes at
`src/mcp/proxy.rs:431-432` — *"every forwarded call fails honestly and immediately (never
hangs)"* — and the §J2 status note repeats it. Worse, recovery is **client-driven**: the
reconnect only happens inside the `client_rx` arm, so a client politely waiting for its
response sends nothing, and the proxy never reconnects either. A client with no per-call
timeout wedges permanently; a client with one wedges for its full timeout.

**Reproduced.** The mutation run above produced exactly this sequence in the proxy's own
logs, from unmutated pump code:

```
INFO  lambo serve: proxy reconnected to the current session holder generation=1
WARN  lambo serve: the session holder closed the connection — the next call will re-read …
   (nothing sent to the client; the test's 30s bound expires on id 4)
```

The mutation changed only *why* the holder closed the connection, not what the pump does
about it. In production the trigger is the ordinary one: a holder that dies (or is SIGTERMed)
while a `lambo_derive` is embedding — a window of hundreds of milliseconds on every write.

**Remediation.** Track outstanding client request ids and drain them on connection loss:

* add `let mut inflight: std::collections::HashMap<serde_json::Value, ()> = …` (or a
  `HashSet<String>` of the serialized id) to `HubProxy::run`;
* in the `client_rx` arm, after a successful `send`, insert the frame's `id` when it has a
  non-null one (reuse the parse `unreachable_reply` already does — expose it as
  `fn request_id(frame: &str) -> Option<serde_json::Value>`);
* in the `hub_rx` `Frame` arm, remove the id the frame answers before forwarding;
* on `FromHub::Closed` **and** on a `gen != generation` drop, emit one
  `HUB_UNREACHABLE_CODE` error per remaining id, then clear the map.

Pin it with an integration case that forwards a call and kills the holder before it answers —
the `--max-sessions 1` ceiling (`src/mcp/serve.rs:1415-1425`, `drop(stream)`) gives a
deterministic "accepted then closed" holder without needing a race.

### J2-R1-2 (P2) — `store_identity` is the store's *spelling*, not its identity; two graphs can derive one socket

`src/mcp/endpoint.rs:314-320`:

```rust
fn store_identity(store: &StoreConfig) -> String {
    format!("{:?}\u{1f}{}\u{1f}{}", store.kind, store.dsn.as_deref().unwrap_or(""), store.path.as_deref().unwrap_or(""))
}
```

`store.path` is used verbatim, and nothing in the tree canonicalizes it (`rg -n canonicalize
src/` finds only `graph/canonical.rs`). `SqliteStore` opens it through
`SqliteConnectOptions::from_str` (`src/store/sqlite.rs:383`), so a **relative** path resolves
against each process's own cwd. Every published example uses one:
`path = "./lambo.db"` in `docs/reference/config.mdx:15`, `installation.mdx:44`,
`end-to-end.mdx:29`, `lambo.example.toml:15`, and all four site mirrors.

So two agent clients launched with different cwds and the documented config are **two
different SQLite files with one derived socket path**. Both win their own lease (separate
databases, separate rows), and the second holder's `bind` takes the `AddrInUse` branch at
`src/mcp/endpoint.rs:207-220` and **unlinks the first holder's live socket** — the licence
argued in that function's docs ("while we hold the lease, a socket file at this path cannot
belong to a live holder") does not hold, because the two holders hold two different leases.
A proxy belonging to graph A then re-dials the path, reaches holder B, and its writes land in
the wrong graph. That is verbatim the outcome `SessionEndpoint`'s module docstring
(`endpoint.rs:27-35`) says the store discriminator exists to prevent, and the same failure
the implementor already found and fixed for `MemoryStore` (`a_process_private_store_
advertises_no_endpoint`) — the fix stopped one step short.

The mirror case is milder but visible: one store reached by two spellings (`./lambo.db` from
the right cwd vs an absolute path) derives two addresses, so the loser's `proxyable` check
returns `EndpointIsNotOurs` and it refuses with a message blaming "a different lambo version,
or a different XDG_RUNTIME_DIR" — none of which is true.

**Remediation.** Canonicalize the file half of the identity before hashing, and fall back to
an absolute-but-unresolved path when the file does not exist yet:

```rust
StoreKind::Sqlite => {
    let p = std::path::Path::new(store.path.as_deref().unwrap_or(""));
    std::fs::canonicalize(p)
        .or_else(|_| std::env::current_dir().map(|d| d.join(p)))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string_lossy().into_owned())
}
```

Keep the DSN half as-is (hashing already keeps a password out of the path). Pin it with a
unit test asserting that `"./lambo.db"` resolved from two cwds is **one** endpoint and that
`"a/lambo.db"` and `"b/lambo.db"` are two. Then the docstring's licence claim becomes true
rather than aspirational.

### J2-R1-3 (P2) — the endpoint directory lost the per-uid discriminator, and the mode check ignores ownership

The decision recorded in the graph is *"dir is `$XDG_RUNTIME_DIR/lambo` if set, else
`$TMPDIR/lambo-<uid>`, else `/tmp/lambo-<uid>`, created 0700"*. The code
(`src/mcp/endpoint.rs:276-287`) drops the `-<uid>` suffix on both fallbacks, and §J2's status
note and the stage-2 commit message were written to match the code rather than the decision.
That is graph↔code drift, and the dropped half is exactly the part that made the shared
fallback safe by construction rather than by check.

Consequences on the `/tmp/lambo` path (reached whenever neither `XDG_RUNTIME_DIR` nor
`TMPDIR` is set — bare containers, `ssh` without `pam_systemd`, cron):

* **Cross-user DoS, and it is a regression.** The first uid to run `lambo serve` creates
  `/tmp/lambo` mode 0700 owned by itself. Every other uid's `bind` then fails `EACCES` — and
  since every holder binds, *and* the proxy path needs the holder to have bound, that is a
  new hard failure for a case that worked fine before J2. `/tmp`'s sticky bit means the
  second user cannot even clear it.
* **The mode check never checks the owner, and follows symlinks.**
  `std::fs::metadata(dir)` at `endpoint.rs:188-201` resolves a symlink, so an attacker-placed
  `/tmp/lambo → /tmp/theirs` with `/tmp/theirs` at mode 0700 passes the `mode & 0o077 != 0`
  gate. Same-uid is out of the threat model, but a *different* uid owning a 0700 directory we
  can nonetheless write into (an ACL grant on macOS, a group-writable ancestor) is not, and
  the docstring's claim that the check "is what makes the shared `/tmp` fallback safe rather
  than assumed safe" over-promises for that case.

**Remediation.** Restore the uid suffix (`format!("lambo-{}", unsafe { libc::geteuid() })`,
or read it from `nix`/`rustix` if already vendored — otherwise `std::os::unix::fs::
MetadataExt::uid()` of a known-own file works), and add an ownership assertion beside the
mode check using `symlink_metadata` on the final component so a symlinked directory is
refused rather than followed. Pin with a test that a directory owned by another uid is
refused (feasible only as an ignored/root test) and, cheaply, that `endpoint_dir()` for the
`/tmp` fallback contains the current euid.

### J2-R1-4 (P2) — a torn final frame at holder death is forwarded to the client as a complete one

`src/mcp/proxy.rs:556-567` reads the hub with `read.lines()` and forwards every
`next_line()`. `tokio::io::Lines` yields a trailing unterminated remainder as a line — the
final line ending is optional. So when a holder dies mid-write, the half of the JSON object
that made it into the socket is delivered to the client's stdout **as a frame**, followed by
`FromHub::Closed`. The client receives truncated JSON on its MCP wire.

The same shape exists on the client→hub direction (`:449-456`), where it is milder: the
holder's rmcp parser rejects it and the client is already gone.

The design's justification for a byte pipe is that it copies frames without interpreting
them; that is exactly why it must not *invent* a frame boundary the peer never wrote.

**Remediation.** Read with `read_until(b'\n')` and forward only when the buffer ends in
`\n`; on EOF with a non-empty, unterminated remainder, log at WARN and drop it, then send
`FromHub::Closed`. Pin with a unit test over a `tokio::io::duplex` pair that writes
`{"jsonrpc":"2.0","id":1,"resu` and closes, asserting the reader emits `Closed` and no frame.

### J2-R1-5 (P2) — an over-long `sun_path` refuses the whole serve; a *failed bind* does not

Two paths to the same operator situation ("this process cannot have an endpoint") end in
opposite outcomes:

* `SessionEndpoint::for_store(...)?` at `src/mcp/serve.rs:950` propagates the length refusal,
  so `serve` **exits** — and pre-J2 the same machine served fine. A long
  `TMPDIR`/`XDG_RUNTIME_DIR` (a deep per-user path, a container mount, a long username) is
  now a hard startup failure for a feature the operator did not ask for.
* A *failed bind* at `serve.rs:1063-1075` deliberately does **not** stop the process: "a bind
  failure does not stop this process serving memory — the same posture `Ledger::open` takes".

The harsher outcome is attached to the cheaper problem. The pre-lease-group argument (the
check is pure, so refusing costs nothing) explains *where* the check may live, not *why* it
must be fatal.

**Remediation.** Keep the check pre-lease and keep its message, but treat the refusal the
way a bind failure is treated: log at ERROR, run with `endpoint = None`, and let the process
serve its own client. Then `for_store` returns `Ok(None)` on an over-long path rather than
`Err`, and the one behaviour that changes is that a losing serve on such a machine refuses as
it did before J2 — which is the correct degradation. Adjust
`an_over_long_base_directory_is_refused_before_any_lease` to assert the `None`, and add a
subprocess case asserting a serve with a pathologically long `TMPDIR` still attaches.

### J2-R1-6 (P2) — "the row is re-read on every retry, never a cached address" is unpinned, and its stated reason is false

`HubProxy::reconnect`'s docstring (`src/mcp/proxy.rs:360-366`) says the address is never
cached "**because a new holder is a new endpoint**". It is not: the address is a pure function
of `(session, store identity)` (`endpoint.rs:107-139`), so *every* holder of a given session
and store binds the **same** path — which is precisely why `bind` needs its stale-socket
branch at all. The two claims cannot both be true, and the false one is load-bearing in the
test's own doc comment (`tests/serve_proxy_multi_client.rs:337-339`: "a brand-new holder at a
brand-new endpoint is picked up").

**Mutation-proven unpinned.** With `dial()` short-circuited to `connect(&self.endpoint)` —
no `read_lease`, none of the three `proxyable` checks — both integration tests pass:

```
test two_clients_over_stdio_both_work_through_one_hub ... ok
test a_dead_holder_leaves_the_proxy_honest_and_the_lease_unclaimed ... ok
test result: ok. 2 passed; 0 failed
```

So no test exercises the `proxyable` checks **on the reconnect path** (the unit tests cover
the pure function; `resolve_role` covers the startup path). The re-read does earn its keep —
it is what turns "the row is gone" into an honest per-call error — but that is a different
property from the one claimed, and it is the one that should be pinned.

**Remediation.** Two small edits. (1) Correct the docstring and the test comment: the reason
to re-read is that the *row* changes — holder identity, host, and presence — not the address.
(2) Add the case that fails under the mutation: after killing the holder, `release_lease` the
row and make a call **before** any new holder exists, asserting the reply names the honest
"no lease holder / not responding" text. That is one `b.call` inserted at
`tests/serve_proxy_multi_client.rs:418`, in the window the test already creates.

### J2-R1-7 (P2) — sweep 1 missed that `serve()` no longer calls `build_memory` at all

Sweep 1's table adjudicates `ledger.rs:253` as *"still TRUE — the bind sits below
`LamboServer`, so `Ledger::open` is still next"*. It verified the second clause and not the
first. The claim reads:

> `shutdown_signal()` is the first statement once `build_memory` returns; this call is the next one

`serve()` does not call `build_memory` any more — stage 3+4 replaced it with
`serve_builder` + `resolve_role` (`src/mcp/serve.rs:950-956`). `rg -n build_memory` finds
**zero** call sites in the whole tree: it survives only because it is `pub` and re-exported at
`src/mcp/mod.rs:19`. Nine sites still describe the serve startup in its terms —
`ledger.rs:253`, `serve.rs:341`, `:916`, `:918`, `:936`, `:977`, `:1002-1016`, `:1614`, and
`tests/serve_pre_handshake_durability.rs:164` — and this is exactly the claim family
("serve-startup ordering") sweep 1 declared itself over.

One of them is more than a rename. `serve.rs:1013-1016` argues that arming *before*
`build_memory` would trade a durability hazard for an availability one "needs `build_memory`
raced". Under J2 the thing that would be made SIGTERM-immune is `resolve_role` — a loop that
can legitimately run for **50 seconds**. That is a much stronger reason for the current
placement than the one written down, and a maintainer reading the old text will not find it.

**Remediation.** Either delete `build_memory` and its `mcp::` re-export (it has no callers)
or docstring it as a library entry point that the serve path no longer uses; then rewrite the
nine sites in terms of `resolve_role`, and add the 50-second sentence at `serve.rs:1013`.

### J2-R1-8 (P2) — the handshake-replay swallow is an unbounded read, in an arm body the shutdown future cannot pre-empt

`Handshake::replay` (`src/mcp/proxy.rs:279-303`) awaits `read.read_line(&mut swallowed)`
with no deadline, and it is reached from `reconnect_and_replay(...).await` **inside** the
`client_rx` arm body at `src/mcp/proxy.rs:490` — not as a `select!` branch. So a holder that
accepts the connection and never answers the replayed `initialize` parks the pump forever, and
the `biased` shutdown arm cannot be polled: the process becomes deaf to SIGTERM as well.

A `UnixStream::connect` succeeds as soon as the connection lands in the listener's backlog,
so "accepted but never answered" does not require a malicious peer — a holder whose accept
loop is starved or stopped is enough. The `--max-sessions` ceiling is *not* such a case, to the
implementor's credit: `serve.rs:1423` does `drop(stream)`, which the swallow correctly sees as
`UnexpectedEof`.

**Remediation.** Wrap the replay in the budget it already has a constant for:
`tokio::time::timeout(CONNECT_BUDGET, handshake.replay(&mut read, &mut write))` in
`reconnect_and_replay`, mapping elapsed to the same `LamboError::Conflict("holder rejected the
session handshake: …")`. Better still, race the shutdown future: hoist the reconnect out of
the arm body by storing a `pending_frame` and looping, so `select!` keeps its shutdown branch
live across the reconnect. Pin with a unit test over a `UnixListener` that accepts and never
writes, asserting the call returns inside the budget.

## P3 findings

| # | Finding | Evidence | Fix |
|---|---|---|---|
| J2-R1-9 | Four operator-facing messages contain 18–26-space runs — a `\`-continuation collapsed onto one line, leaving the continuation indent inside the literal. `cargo fmt` cannot see it and no test asserts the text | `src/mcp/endpoint.rs:199-203` ("reachable by other␣×18users"), `:212` (stale-socket WARN), `:217`, `src/mcp/serve.rs:1421` ("refusing a␣×18connection") | re-break the literals with `\` continuations |
| J2-R1-10 | `unreachable_reply` answers a **client response** frame (an `id`, no `method` — the client's answer to a server-initiated `sampling/createMessage` or `roots/list`) with an error response keyed to the server's request id. Reachable whenever a holder dies with a server→client request outstanding | `src/mcp/proxy.rs:197-210` keys only on `id` | require `value.get("method").is_some()` before synthesizing; add the case to `a_notification_and_a_broken_frame_are_not_answered` |
| J2-R1-11 | `Handshake` records `initialize` and `notifications/initialized` only. Other session-scoped client state — `logging/setLevel`, `notifications/roots/list_changed`, subscriptions — is silently lost on reconnect and is **not** among the stated residuals, which name only `serverInfo`/`capabilities`/`protocolVersion` | `src/mcp/proxy.rs:257-266` | either record the small set of idempotent session-configuring frames, or add the sentence to the residual paragraph at `:230-238` |
| J2-R1-12 | The swallow assumes the **first** line back is the `initialize` response. A holder that emits any notification first has its response forwarded to the client as a duplicate answer to an id the client already holds | `src/mcp/proxy.rs:287-294` | loop, bounded, until a line whose `id` equals the recorded `initialize`'s id; forward the others |
| J2-R1-13 | `GraphStore::read_lease`'s default `Ok(None)` keeps the five test doubles untouched — but it also means any *future* adapter that forgets it silently disables proxying, and the failure looks like a missing endpoint | `src/store/mod.rs:335-337` | keep the default, and say in its docstring that a real adapter must override it; consider a store-conformance test that asserts a non-`None` result after an acquire |
| J2-R1-14 | `HUB_UNREACHABLE_MESSAGE` hardcodes "within 45 seconds" with no link to `lease::LEASE_TTL`. A TTL change leaves model-facing text lying | `src/mcp/proxy.rs:77-83` | build the sentence with `LEASE_TTL.as_secs()`, or add a `const _: () = assert!(LEASE_TTL.as_secs() == 45)` beside it |
| J2-R1-15 | `a_handshake_that_never_happened_replays_nothing` never calls `replay` — it asserts that `Handshake::default()` has two `None`s. The comment is honest about this; the name is not | `src/mcp/proxy.rs:706-712` | drive it over a `tokio::io::duplex` pair and assert no bytes were written |
| J2-R1-16 | `two_clients_over_stdio_both_work_through_one_hub`'s comment claims the test is "what would go red if the proxy ever started re-serializing arguments". It would not: `agent-b` survives any normalisation | `tests/serve_proxy_multi_client.rs:272-274` | use an id J1 deliberately keeps untrimmed, e.g. `"agent-b "`, so a trimming forwarder breaks the lock-holder match |
| J2-R1-17 | A single non-UTF-8 byte from either peer ends the pump. `while let Ok(Some(line))` (`:452`, `:558`) treats an `Err` as end-of-stream, so the client is told "proxy client disconnected" for what is a decode failure | `src/mcp/proxy.rs:452`, `:558` | match the `Err` arm and log it before breaking |
| J2-R1-18 | Neither direction caps line length; `read_line` grows without bound. A broken peer can OOM the proxy | `src/mcp/proxy.rs:452`, `:558`, `:287` | `take(MAX_FRAME)` on the reader, with an honest error past the cap |
| J2-R1-19 | The endpoint's operational surface — the socket, the 0700 directory, `XDG_RUNTIME_DIR`/`TMPDIR` — appears in **no** user-facing doc. `rg -n -i 'XDG_RUNTIME_DIR\|unix socket\|\.sock' docs/ site/ skills/` returns nothing, yet `bind` can refuse with "chmod 700 that directory, or set XDG_RUNTIME_DIR". The DOGFOOD-SETUP deferral is declared and correct, but `docs/reference` is not the runbook | — | one paragraph in `mcp.mdx`'s new section (both mirrors) naming the directory rule and the two env vars |
| J2-R1-20 | The gate line "524/0/0 cockroach" is not reproducible. I got 515/0/0 (`--no-default-features --features store-cockroach --lib`), 542/0/1 (`+embed-fixture --lib`), 794/0/2 (`--features store-cockroach --lib`), 551/0/1 (full suite). The other three numbers matched **exactly**, which is what makes this one worth naming | `dev-diary/lambo-for-mooshik/J-multi-client.md` §J2 gate lines | record the invocation beside each count |
| J2-R1-21 | `select! { biased; shutdown, client_rx, hub_rx }` polls the client before the hub, so a client that streams notifications continuously starves the holder's responses. Self-limiting for request/response clients; not for a streaming one | `src/mcp/proxy.rs:469-473` | drop `biased` below the shutdown arm, or alternate |

## Attacks that did not land

* **`build`/`build_attach` duplication drift.** There is nothing to drift: `build` is a
  three-line delegate over `build_attach` (`src/memory.rs:615-620`), so the "two copies of
  the startup sequence" this was hunting for does not exist.
* **A `Cargo.toml` change smuggled in.** `git diff 19f51d3..HEAD -- Cargo.toml Cargo.lock` is
  empty. The `transport-io` → `transport-async-rw` argument holds.
* **The fencing token.** Every `current_token` line in the source diff is a column list or a
  tuple destructuring; no guard, no increment rule, no comparison moved.
  `two_clients_over_stdio_both_work_through_one_hub` asserts `row.token == 1` at the end of a
  fully-working two-client session.
* **A refresh NULLing the endpoint.** `refresh_lease` routes through the same
  `acquire_or_refresh` with `endpoint = excluded.endpoint`, and the heartbeat clones the
  endpoint-carrying `LeaseHolder` (`src/memory.rs:692-701`, `:851`), so the column survives
  every 15s refresh. Pinned by `the_lease_endpoint_round_trips_and_a_refresh_republishes_it`
  on both SQL adapters.
* **A released lease leaving a dangling endpoint.** `release_lease` DELETEs the row, so a
  clean holder exit leaves nothing for the next proxy to dial; `endpoint.unlink()` then
  removes the file. Both orderings of the two are honest.
* **A torn write from `select!` cancellation.** Every `send` is awaited in an *arm body*, not
  as a branch, so no partially-completed write is ever dropped. Exactly one task writes to
  stdout and exactly one to the hub. (The cost is J2-R1-8's un-pre-emptable awaits — the same
  property, seen from the other side.)
* **`SUN_PATH_MAX` off-by-one.** `len + 1 > SUN_PATH_MAX` accepts a 103-byte path and refuses
  a 104-byte one, which is right: `sun_path` holds 104 bytes *including* the NUL.
* **A hostile session name escaping the directory.** `sanitize_prefix` maps every non
  `[A-Za-z0-9_-]` char to `_` before truncating, so `../../etc` cannot traverse, and identity
  lives in the hash rather than the prefix.
* **A `DefaultHasher` that moves across toolchains.** FNV-1a is written out and pinned by
  `the_hash_is_pinned_so_a_compiler_upgrade_cannot_move_an_endpoint`.
* **Two serves racing acquire→publish→bind, the loser unlinking the winner's socket.** The
  loser never reaches `bind` — `resolve_role` returns `Role::Proxy` before it — and the
  winner's `AddrInUse` unlink is genuinely licensed *for one store*. The way to break this is
  J2-R1-2, which breaks the "one store" premise rather than the ordering.
* **TOCTOU between the directory mode check and `bind`.** `/tmp` is sticky, so another uid
  cannot rename or unlink our 0700 directory between the two. The reachable weakness is the
  missing ownership check, not the window (J2-R1-3).
* **A pipelining client.** `initialize` + `initialized` + first call sent before any read all
  flow through in order and are `observe`d individually; the first connection replays nothing.
* **A recorded `initialize` id colliding with a later client request id.** The swallow happens
  immediately after the replay write, on a connection with nothing else in flight, so a later
  reuse of that id cannot be caught by it.
* **A proxy leaving something stale behind.** It binds nothing, opens no ledger, spawns no
  heartbeat and builds no `LamboServer`; `rg` confirms zero `acquire_lease` and one
  `read_lease` in `proxy.rs`.
* **`resolve_role`'s election being wedged by SIGTERM.** The arming is deliberately still
  below it, so a signal during the 50s wait hits the default disposition and kills a process
  that holds no lease. Correct, and the placement is the same one I-R2-1 chose.
* **`resolve_role`'s retry loop being expensive.** `build_attach` takes the lease *before* the
  startup load (`src/memory.rs:670-686`), and the builder's backends are all `Arc`s, so 50
  attempts are 50 upserts against an already-open pool — no second connection, no replay.
* **The HTTP transport path.** `resolve_role` refuses on `opts.transport != Transport::Stdio`
  before any endpoint logic, and `serve_http`'s body is untouched in the diff.

## Sweeps, re-derived independently

**Sweep 1 (serve-startup ordering, 11 sites).** Nine verdicts confirmed. `PHASE-8-surface.md`
gained the third-listener annotation, the `src/mcp/endpoint.rs` surface entry and the
`build_memory` extra-parameter note, all correct; `serve()`'s arming comment now names the
endpoint bind and its accept loop; `authorize_bind` gained the restating section rather than
being falsified. **One miss, and it is inside the declared family:** J2-R1-7 — the family's
central noun, `build_memory`, no longer runs on the serve path, and `ledger.rs:253` was
adjudicated "still TRUE" on its second clause while the first went stale.

**Sweep 2 (lease/endpoint schema, 11 sites).** Re-derived clean, all 11. The
`LeaseRowText`/`LeaseRowTs` + `LEASE_ROW_SQL` collapse genuinely removes the drift shape:
`rg -n 'session_leases \(' src/store/` finds one column list per adapter, both including
`endpoint`, both matched by a `RETURNING` built from the same constant. `provision.sh:34`
(tables, not columns) and `check_durability.py:136` (named-column `SELECT`) confirmed clean.

**Sweep 3 (what happens to a second writer, 5 pairs).** All five verdicts confirmed and both
corrections are right and complete. My own independent sweep
(`rg -i 'exactly one writer|second (serve|writer)|one client at a time|two writer|exits 1'`
over the tree, excluding `dev-diary/` and `evidence/`) turns up **no further falsified site**:
`api.mdx:125`/`:127` and `cli.mdx:167`/`:169` stay true because the lease is untouched and a
CLI verb still refuses, and `end-to-end.mdx:10`/`:14` ("exactly one writer, which is a
`lambo serve` process that holds the session's lease") stays true because a proxy holds none.
The new `mcp.mdx` section is byte-identical in both mirrors — `diff` of the two files shows
only the pre-existing Astro imports, the `/lambo/` link prefixes and the site-only
`## Verified clients` section — and both anchors (`/mcp#more-than-one-client-on-one-machine`
and `/lambo/mcp/#...`) resolve against the `### More than one client on one machine` heading.
Register rule satisfied on every touched file. The §J5 correction about the mirrors *not*
being byte-identical is independently confirmed and is a genuinely useful catch.

## Positive observations

* **The deviation was handled the way a deviation should be.** Instruction, contradicting
  requirement, the measurement that decided it, and the residual — all four written down, in
  the code and in the phase doc, before anyone asked. My reproduction agreed with theirs on
  the first attempt, down to the rmcp error text.
* **The wedge invariant is genuinely enforced, not just asserted.** One `read_lease`, zero
  `acquire_lease` in `proxy.rs`; the invariant stated at the function a future author would
  edit; and pinned by a subprocess test that reads the row back from a real store.
* **`Attach` instead of a new error variant** is the right answer to J1-R2-2, and making
  `build` a delegate rather than a parallel implementation removes the drift risk entirely
  rather than documenting it.
* **The J0 catches were answered, not routed around.** In particular the arming decision is
  argued from what the arming *protects*, and the "its own sync point" fix for the loose
  matcher is exactly right — I reproduced its negative control and it is a real control, not a
  ceremony.
* **Two self-found defects are pinned as tests** (`sessions_sharing_a_truncated_prefix_do_not_
  collide`, `a_process_private_store_advertises_no_endpoint`). The second is the same class of
  bug as J2-R1-2 — the reasoning was right and stopped one case short.
* **FNV-1a written out with the reason** (`DefaultHasher` is not stable across releases) and
  pinned by a golden-value test is the kind of care that is usually skipped.
* **N4 discipline in `HUB_UNREACHABLE_MESSAGE`** is real and asserted: no path, no URL, no
  errno, and the integration test checks `!refused.contains(".sock")`.
* **`serve_single_writer_lease.rs`'s move to `--transport http`** is argued, not quietly
  retargeted, and it gained an assertion that B was refused *by the lease* rather than by
  anything J2 added — the right guard against the retarget hiding a regression.

## Gate results

All re-run from scratch in the worktree at `40791e2`, `CARGO_TARGET_DIR` shared:

| Gate | Claimed | Re-derived | |
|---|---|---|---|
| `cargo fmt --check` | clean | **clean** | ✓ |
| `cargo clippy --all-targets -D warnings` (default) | clean | **clean** | ✓ |
| …`--features store-sqlite,embed-fixture,fixtures` | clean | **clean** | ✓ |
| …`--features ship` | clean | **clean** | ✓ |
| …`--features store-cockroach,embed-fixture` | clean | **clean** | ✓ |
| `cargo test --features fixtures` | 832/0/3 | **832/0/3** | ✓ exact |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | 914/0/3 | **914/0/3** | ✓ exact |
| …`--lib` | 887/0/1 | **887/0/1** | ✓ exact |
| cockroach | 524/0/0 | **not reproducible** — 515/0/0, 542/0/1, 794/0/2, 551/0/1 depending on invocation | J2-R1-20 |
| `scripts/observability/verify.sh` | ALL CHECKS PASSED | **ALL CHECKS PASSED** | ✓ |
| Negative control: proxy shutdown future → `pending()` | red | **red** — `a_pre_handshake_sigterm_to_a_proxy_exits_cleanly_and_leaves_the_holder_intact` FAILED at `tests/serve_pre_handshake_durability.rs:368`, holder case still green | ✓ |
| New tests | 18 unit + 3 integration | **23** new `#[test]`/`#[tokio::test]` in `src/`, **3** in `tests/` | ✓ (undercount in the note) |

**Verification-only edits, all reverted; tree clean but for this file.**

1. `src/mcp/proxy.rs` — env-gated early `return Ok(())` at the top of `Handshake::replay`, to
   adjudicate the deviation. Reverted from a byte copy taken before the edit.
2. `src/mcp/proxy.rs` — env-gated short-circuit in `dial()` that skips `read_lease` and
   `proxyable`, to mutation-test the "never a cached address" claim. Reverted the same way.
3. `src/mcp/serve.rs` — `proxy.run(std::future::pending())` for the negative control.
   Reverted from a byte copy.

`git status --short` after all three: only the untracked review file.

## Verdict

**REQUEST_CHANGES.** One P1, seven P2, thirteen P3.

This is the strongest single change of the workstream and most of it is right: the byte-pipe
choice is well argued and its consequences are real, the wedge invariant is enforced rather
than described, both J0 catches were answered at the level they were raised, and the one
deviation from the plan was measured, declared, and — I confirmed — necessary. The blockers
are not design errors. They are three specific gaps between what the design claims and what
the code does:

* **J2-R1-1** falsifies the central promise ("fails honestly and immediately, never hangs")
  for the one call that matters most — the one in flight when the holder dies. J2 exists to
  remove a hang; this leaves a narrower one.
* **J2-R1-2** and **J2-R1-3** are both the same shape: an identity that is a *string* where
  the argument requires an *identity*. The store-discriminator reasoning is right and the
  implementation is one `canonicalize` short of delivering it; the uid discriminator was
  reasoned correctly in the graph and lost on the way to the code.
* **J2-R1-6** and **J2-R1-7** are register-rule failures inside families J2 declared itself
  over — a claim whose stated reason is false and unpinned, and a claim family whose central
  noun went stale while the sweep passed one of its members.

None of these needs a redesign; every one has a bounded, executable fix, and four of the
seven P2s are edits under twenty lines.

**Integration readiness.** With J2-R1-1 through J2-R1-3 fixed, J2 is ready to integrate
pending the operator's live two-client verification — which no agent can run, and which §J2's
own `Done when` box correctly leaves unticked: the committed test drives two subprocesses of
one binary, which is exactly the "two sessions of one product" that box excludes. The
mechanism is pinned; the two-product claim is not, and cannot be from inside `cargo test`.

## Residuals handed forward

* **J3** — the async-ack path will make J2-R1-1's window *wider*, not narrower: a receipt
  outstanding across a holder death is the same unanswered-id problem with a longer lifetime.
  Whatever J3 does about receipts should assume the fix for J2-R1-1 exists, or inherit it.
* **J4** — §J2 hands over the proxy's ledger silence deliberately and names the two new states
  worth recording (*proxying*, and *proxying to a holder that stopped answering*). Add a
  third from this review: *an in-flight call lost with the holder*, which is currently
  invisible from both sides.
* **J5** — the §J5 correction in §J2 is right and should be honoured: a `diff` gate over the
  four mirror pairs would be red on the day it landed. The real gate is over shared prose with
  link prefixes normalised and site-only sections excluded. J2's own new section is a good
  test case for it, being byte-identical in both copies by construction.
* **J5 / config layering** — J2-R1-19's undocumented endpoint surface (the socket, the 0700
  directory rule, `XDG_RUNTIME_DIR`/`TMPDIR`) belongs in J5's "docs state the multi-client
  default" box.
* **Beyond J** — in-process promotion inherits the wedge invariant (acquisition may only be
  unlocked *together* with promotion) and the self-loop shape §J2 records. Add: it also
  inherits J2-R1-2, because a promoted proxy binds the socket, and binding the wrong graph's
  socket is worse than dialling it.
* **The DOGFOOD-SETUP re-pin** stays one act with the runbook edit, as §J2 says. The list §J2
  wrote for that edit is complete as far as I can check it; add the endpoint directory rule
  to §6's smoke test, since "the socket exists" is already on that list and "the directory is
  0700 and yours" is the check that actually fails on a shared box.

---

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>

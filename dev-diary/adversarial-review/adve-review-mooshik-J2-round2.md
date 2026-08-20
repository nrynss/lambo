# Adversarial review — mooshik J2, round 2

**Reviewer**: independent adversarial reviewer (Opus 5), agent_id `j2-reviewer-r2`. Wrote
nothing under review.
**Scope**: the five remediation commits `bbac803..a573e64` on `wt/j2` — `58faeac`
(J2-R1-1, the P1), `fdb3225` (J2-R1-4/6/8/11/12/14/15/17/18/21), `8daf389`
(J2-R1-2/3/5/9), `3b0d02a` (J2-R1-7/13/19/20, register-only), `a573e64` (J2-L1/J2-L2, the
live probe's two P2s). Against the 21-finding checklist of
`adve-review-mooshik-J2-round1.md` plus the orchestrator's two live-probe findings.
**Worktree**: `/Users/narayan/Documents/work/lambo/.claude/worktrees/j2`, branch `wt/j2`.
No commit amended; tree left clean.
**Verdict**: **REQUEST_CHANGES** — all 23 findings verified closed, every gate exact, all
five declared mutations reproduced, and the live two-product probe flips the P1 from a 151.7s
hang to a 2.6 ms honest answer. Two new **P2** and five **P3**. Both P2s are the same shape
and neither needs a behaviour change: a load-bearing *number* that is quoted as an exact
bound and is wrong, in a docstring that uses it to justify a shipped decision.

## Method

1. `lambo_recall` as `j2-reviewer-r2` on "J2 round 1 remediation in-flight endpoint TMPDIR" —
   30 hits carrying the live-probe constraints, the remediation's derived decisions
   (the −32001/−32002 split, the graph↔code re-convergence on the per-uid directory), and
   round 1's own "attacks that did not land" list, which I used to avoid re-running cleared
   ground. Graph read as context; adjudication is against the round-1 checklist → §J2 →
   source, in that order.
2. Read the round-1 review's 21 findings and its gate table, then each of the five
   remediation commit messages in full, then the code each one claims to change.
3. Independent verification of every finding at the artifact, not at the commit message.
4. Five verification-only mutations, run and reverted (below), all env-gated so one build
   exercises both directions.
5. Gates re-run from scratch: fmt, clippy on four feature sets, three test profiles,
   `verify.sh`. Every count re-derived and every delta reconciled to named tests.
6. **Part B, the centrepiece**: the live two-product harness rebuilt against the remediated
   binary and re-run, with the pre-remediation run as the control.

**Order of authority.** The round-1 checklist is the contract; §J2's corrected status note is
the claim; the source is what ships. Where a commit message and the code disagree the code
wins and the disagreement is a finding. Both new P2s are of that kind — the code is right and
the sentence describing it is not.

## Part A — the 23 at the artifact

### The P1

| # | Finding | Verdict | Evidence |
|---|---|---|---|
| **J2-R1-1** | in-flight forwarded request never answered when the holder's connection closes | **CLOSED** | `src/mcp/proxy.rs:968-973` (`inflight: Vec<(u64, serde_json::Value)>`), recorded after a successful send at `:1035-1041`, retired by `response_id` at `:1058-1067`, drained by `answer_lost` at `:1139-1166`. `request_id` (`:410`) requires a `method` and a non-null `id`; `response_id` (`:428`) requires *no* `method` plus a `result` or `error`. Mutation 1 below: red at the 30s bound (31.20s), green in 1.02s. **Confirmed live**: 2.6 ms to answer, `lost=1` — Part B run 3c |

### The seven P2

| # | Finding | Verdict | Evidence |
|---|---|---|---|
| **J2-R1-2** | `store_identity` is the store's *spelling*, not its identity | **CLOSED** | `canonical_store_path` (`src/mcp/endpoint.rs:570-593`) with the four branches documented at `:528-569`. Both directions pinned: `one_store_reached_two_ways_is_one_endpoint` (symlinked dir + redundant `.` spelling → one address) and `two_stores_with_one_file_name_in_two_directories_are_two_endpoints`. URI spellings left verbatim (`looks_like_uri`, `:601`) — the correct call, since `cwd.join` on a URI would reintroduce the bug from the other side. One residual window: **J2-R2-5** |
| **J2-R1-3** | per-uid discriminator lost; mode check ignores ownership | **CLOSED** | `endpoint_dir_from(xdg, uid)` (`:408-414`) — `$XDG_RUNTIME_DIR/lambo`, else `/tmp/lambo-{uid}`; graph and code agree again. `assert_private_dir` (`:446-486`) is **lstat-correct**: `symlink_metadata` (not `metadata`), then `meta.uid() != geteuid()`, then `mode & 0o077`. Three checks, three distinct messages. Verified live: `/tmp/lambo-501 drwx------ narayan` |
| **J2-R1-4** | torn final frame forwarded as a complete one | **CLOSED** | `Framed`/`read_frame` (`:193-260`) replaces `Lines()`. `Torn` is dropped with a WARN and followed by `Closed`, never forwarded — `src/mcp/proxy.rs:1181-1196` in the hub reader, `:927-937` on the client side. Pinned by `a_torn_final_frame_is_dropped_not_forwarded` and `a_complete_frame_is_read_without_its_newline` |
| **J2-R1-5** | over-long `sun_path` refuses the whole serve | **CLOSED** | `for_store` returns `Option<Self>` (`:138`), no `Result` — the refusal became an ERROR log and `endpoint = None`. Pinned at the binary by `a_base_directory_too_long_for_a_socket_still_serves_its_own_client`, which asserts both halves (the client's session works **and** the lease row's `endpoint` is NULL). The test now drives `XDG_RUNTIME_DIR` rather than `TMPDIR`, which is *why* it still fails without the fix |
| **J2-R1-6** | "never a cached address" unpinned, and its stated reason false | **CLOSED** | Reason rewritten around liveness and honest errors (`:786-805` region). Pinned by `a_live_endpoint_with_no_lease_row_is_refused_rather_than_dialled`. **Mutation 2 is the proof**: at round 1 the `dial()` short-circuit left every test green; it now turns **two** red. The remediation's note that my prescribed fix would *not* have been red (the writer is live from the preceding call, so `dial` is never entered) is correct — I checked, and the new test builds the discriminating window instead |
| **J2-R1-7** | sweep 1 missed that `serve()` no longer calls `build_memory` | **CLOSED** | All nine sites renamed to `resolve_role` (`src/ledger.rs:254`, `src/mcp/serve.rs:344-345`, and the rest). `build_memory` **kept** with a library-only docstring at `serve.rs:711-720` saying no call site remains — the right call; deleting a `pub`, re-exported API in passing is wider than a remediation. The stronger placement argument (arming above `resolve_role` would make a *legitimate* 50s wait unkillable to close a ~1.1 ms window in a process holding no lease) is now written at `serve.rs:1152` |
| **J2-R1-8** | replay swallow is an unbounded read in an arm body | **CLOSED, with the bound misstated** | `replay` wraps `swallow_response` in `tokio::time::timeout(CONNECT_BUDGET, …)` (`:585-603`) and `MAX_REPLAY_FRAMES = 64` caps the count. Pinned by `a_holder_that_never_answers_the_replay_is_given_up_on`. The declared deviation (no reconnect hoist) is argued, not assumed, and I accept it. **But the 2 × CONNECT_BUDGET bound it states is wrong — J2-R2-1** |

### The thirteen P3

| # | Verdict | Evidence |
|---|---|---|
| J2-R1-9 | **CLOSED** | Four literals re-broken; and the class now fails a test — `src/mcp/endpoint.rs:979`, `:1006` assert `!err.to_string().contains("  ")` alongside a phrase spanning a continuation |
| J2-R1-10 | **CLOSED** | Subsumed by `request_id`'s `method` requirement (`:410-424`); a client *response* frame carries a holder-minted id and is not answered. Cases added to `a_notification_and_a_broken_frame_are_not_answered` |
| J2-R1-11 | **CLOSED (documented, not built)** | `src/mcp/proxy.rs:501` names `logging/setLevel`, `notifications/roots/list_changed` and subscriptions in the residual paragraph. Documenting rather than enumerating the protocol is the right call for a byte pipe, and the commit says so |
| J2-R1-12 | **CLOSED** | `swallow_response` (`:612-655`) reads until the frame whose id matches the recorded `initialize`, returns everything before it, and `run` forwards those (`:1015-1021`). Positional fallback only when the recorded `initialize` has no id. Pinned by `the_replay_swallows_the_initialize_response_and_forwards_what_came_before_it` |
| J2-R1-13 | **CLOSED** | `src/store/mod.rs:333-345`: "**A real adapter MUST override this**", plus what the silent failure looks like (`HolderPublishedNoEndpoint`, indistinguishable from a CLI verb holding the lease) and the check an adapter author can run |
| J2-R1-14 | **CLOSED** | `const _: () = assert!(LEASE_TTL.as_secs() == 45, …)` at `src/mcp/proxy.rs:164-170`, beside the sentence that quotes it |
| J2-R1-15 | **CLOSED** | Now `async`, drives `replay` over a duplex and asserts no bytes written |
| J2-R1-16 | **CLOSED** | `tests/serve_proxy_multi_client.rs:333` takes the lock as `"agent-b "`; `:354` asserts `contended.contains("agent-b  until")` — two spaces. A trimming forwarder gives one |
| J2-R1-17 | **CLOSED** | `Framed::NotUtf8` dropped through the newline, stream survives; `a_non_utf8_frame_is_dropped_and_the_stream_survives` |
| J2-R1-18 | **CLOSED** | `MAX_FRAME_BYTES = 8 MiB` (`:147`), resynchronising at the next newline; `an_oversize_frame_is_dropped_and_the_stream_resynchronises`. One place still unbounded: **J2-R2-7** |
| J2-R1-19 | **CLOSED** | `XDG_RUNTIME_DIR` now appears 3× in each of `docs/reference/mcp.mdx` and `site/src/content/docs/mcp.mdx` (was zero across `docs/ site/ skills/`) |
| J2-R1-20 | **CLOSED, and the original count vindicated** | §J2 `J-multi-client.md:1050-1065` carries a gate table with the exact invocation beside each count, plus the line that counts are sums over every test binary. `524/0/0` reproduces at `bbac803` with `cargo test --no-default-features --features store-cockroach` — my round-1 "not reproducible" was four wrong invocations, and the remediation is right that the recording, not the number, was the defect |
| J2-R1-21 | **CLOSED** | Nested `select!`: outer `biased` keeps shutdown first and unconditional, inner unbiased gives the two directions equal footing (`:975-990`). Inner arms only *receive*, so no cancellation safety is spent — I checked that every await which could be cut short is still in an outer arm body |

### The two live-probe P2

| # | Verdict | Evidence |
|---|---|---|
| **J2-L1** | **CLOSED, both halves, one live-confirmed** | Half (1): `TMPDIR` is gone from the derivation — `endpoint_dir_from` takes only `(xdg, uid)`. Pinned by `what_a_client_does_to_tmpdir_cannot_move_the_endpoint`. Half (2): `proxyable` compares `file_name()` and **returns the path to dial** (`:350-375`); `dial` runs the published directory through the same `assert_private_dir` and logs both paths at INFO (`:808-843`). Mutation 3 turns the unit test *and* the integration test red, exactly as claimed. **Proven live** in run 1b with a forced directory divergence — Part B |
| **J2-L2** | **CLOSED in behaviour; two claims about it are false** | `ELECTION_BUDGET = 20s` (`src/mcp/serve.rs:842`) with the client-tolerance argument; the blind wait replaced by `expires_at` arithmetic at `:983-1002`, with the old fixed deadline kept as a backstop at `:1007-1018`. Mutation 4 reproduces red at **45.97s** (claimed 45.1s). The refusal is immediate and names a number — measured live at **2.12s**. But **J2-R2-2**: the docstring's "majority of real cases" and the phase doc's "the client's next start succeeds" are both false |

### Attacks on the P1 fix's seams

My brief named five. All five hold; the reasoning is worth recording because three of them are
load-bearing and non-obvious.

| Attack | Result |
|---|---|
| id retirement "from **any** generation" — can a stale generation's response reach the CLIENT and collide with a retried call's id? | **Does not land, and `Closed` being terminal is why.** `split_hub`'s reader task sends `Closed` as its last act and then returns (`:1220`), so no frame can arrive from generation *N* after its `Closed`. Since an id leaves `inflight` only by a real response or by `answer_lost` on that generation's `Closed`, the window "id retired, client re-uses it, stale frame still coming for it" requires a holder that answers the same id **twice** *and* a client that re-uses an id — two protocol violations at once. Within the byte pipe's declared posture (the store is the trust boundary, the holder is trusted) this is not reachable |
| does the `Closed` drain answer with the right **per-id** error — gen *N* vs gen *N+1*? | **Yes.** `answer_lost` retains on `*gen == generation` (`:1147-1155`), so a `Closed` for a superseded connection drains only its own ids and leaves the current generation's untouched. Interleaving with a reconnect is safe for the same reason: the reconnect happens in the `client_rx` arm body, so a queued `Closed(gen N)` is processed *after* `generation` has advanced, and the new id is recorded against *N+1*. `lost_calls_are_answered_per_connection_and_only_once` pins it |
| is the −32001/−32002 boundary exactly "left the process"? What about buffered-but-not-flushed writes? | **Yes, and the partial-write case is safe.** `send` is `write_all` + `write_all(b"\n")` + `flush`, and `sent` is true only if all three return `Ok` (`:1230-1235`). A partial `write_all` failure does put bytes on the wire while the client is told "NOTHING WAS READ OR WRITTEN" — but those bytes cannot form a complete frame (no newline), the holder's line-framed reader therefore never delivers them to `rmcp`, no tool executes, and `writer = None` means the fragment can never be completed. The claim is about the graph write, and for the graph write it is true |
| `Handshake` replay interacting with the drain — reconnect swallow window while synthesized errors are being written | **Safe.** The synthesized errors are written by `answer_lost` from the `hub_rx` arm; the replay runs in the `client_rx` arm. They cannot interleave — one pump, one `select!`, and both are arm-body awaits. The preamble frames the new holder sent before answering the replayed `initialize` are forwarded before the triggering frame is sent (`:1015-1021`), so client-visible ordering is preserved |
| the bounded replay's 2 × `CONNECT_BUDGET` SIGTERM-deafness claim — arithmetic, and stated where claimed | **Stated where claimed** (`src/mcp/proxy.rs:130-134, CONNECT_BUDGET`'s docstring) but **the arithmetic is wrong — J2-R2-1** |

### Attacks on J2-L1's fix

| Attack | Result |
|---|---|
| can a row publish a path whose hash matches but which points at a different-uid dir or a symlink? | **No.** `dial_dir` → `assert_private_dir` on the published path's parent, with `symlink_metadata` (so a symlinked directory is refused, not followed) and `meta.uid() != geteuid()` (so a different-uid directory is refused even at 0700). Verified by construction: the socket *file* cannot be a hostile symlink either, because only we (or root) can write into a 0700 directory we own. Same-uid is explicitly out of the threat model, correctly — that account can already read the store |
| does the canonicalized store identity make two paths to one store agree **in both directions**? | **Yes for the realistic cases**, and both directions are pinned by named tests (see J2-R1-2). I checked the cross-branch case specifically: a process deriving *before* the file exists takes `canonicalize(parent).join(name)` and one deriving *after* takes `canonicalize(file)`, and these agree because `canonicalize(parent)` resolves the same symlinks the full path would — I measured macOS `realpath(3)` to confirm it also case-folds the final component, which closes the case-insensitive-filesystem variant (a file created through a given spelling *has* that spelling on disk, so both branches return it). One genuine window remains: **J2-R2-5** |
| the per-uid dir — are the ownership and symlink checks lstat-correct? | **Yes.** `symlink_metadata` first, `is_symlink()` refused, then uid, then mode. The commit is honest that the ownership refusal is not reachable in-process and the test says so |

### Attacks on J2-L2's fix

| Attack | Result |
|---|---|
| the mutation's 45.1s — why 45.1? | **Arithmetic confirmed.** Under the mutation the process waits blindly; the CLI-shaped holder's full `LEASE_TTL` (45s) has to lapse before `build_attach` succeeds, and the `ELECTION_RETRY` cadence is 1s, so the outcome lands at 45s + up to one poll + setup. I measured **45.97s**; the remediation recorded 45.1s. Same phenomenon, machine load explains the delta |
| the declared residual — is the refusal honest about "try again"? | **The number is honest; two sentences around it are not.** "Retry in 39s" is correct and actionable (I measured it against a 38s remaining lease). But the docstring's "majority of real cases … the wait still succeeds" and the phase doc's "The client's next start succeeds" are both false, and the residual is scoped as an edge case when it is the general case — **J2-R2-2**. A third defect rides along: the composed message asserts the dead holder "is still refreshing it" — **J2-R2-3** |

## Part B — the live harness on the remediated tree

**Build.** `CARGO_TARGET_DIR=<scratchpad>/j2-r2-target`, `LAMBO_GIT_SHA=a573e64`,
`--release --no-default-features --features store-sqlite,embed-bge`. Provenance verified by
embedded strings rather than by trust: `"the holder's endpoint directory differs from this
process"`, `"outcome is UNKNOWN"` and `"will not block the client that spawned it"` are all
**present** in the new binary and all **absent** from the `bbac803` baseline binary beside it.
Embedder rig `127.0.0.1:8080` (bge-m3-q8_0) up and healthy throughout, untouched.
Clients: `cursor-agent` (holder side, `--trust --force -p --approve-mcps`, project
`.cursor/mcp.json`) and `opencode` 1.18.18 (proxy side, project `opencode.json`). Scratch
sqlite stores only; `~/lambo-dogfood` never referenced.

### 1. Unaligned-env run — the J2-L1 reproduction

| | `bbac803` (baseline run 2) | `a573e64` (run 1) |
|---|---|---|
| outcome | **refused**; "server unavailable … status=failed" | **proxied** |
| time | 31.96s to client give-up | opencode completed in **8s** |
| cross-product read-your-writes | absent (model: "I don't have any MCP tools") | **PASS** |
| lease row | holder cursor-agent, loser never forwarded | holder unchanged, socket present |
| endpoint dir | `/tmp/lambo` vs `$TMPDIR/lambo` (two addresses) | `/tmp/lambo-501` for both, `drwx------ narayan` |

Verbatim, the line that used to be a refusal:

```
INFO lambo::mcp::proxy: lambo serve: proxying to the session holder (this process takes no
lease and holds no graph; every write happens in the holder, under the holder's fencing
token) session=j2-r2-probe endpoint=/tmp/lambo-501/j2-r2-probe-d1a4740af0566b03.sock
```

**One honest caveat about this run, which changes what it proves.** I wrapped each server in
`/bin/sh -c` to capture stderr and the child env — and a shell wrapper **reintroduces** the
macOS per-user `TMPDIR`. Both children therefore saw the same `TMPDIR`, the two derivations
agreed, and half (2) was never exercised. A separate direct-spawn probe (MCP server as the
bare command, no wrapper) confirms §J2's measurement stands: cursor-agent's child sees
`TMPDIR=<UNSET>`, `XDG_RUNTIME_DIR=<UNSET>`. So run 1 proves half (1) — the derivation no
longer moves when a client scrubs `TMPDIR` — and I had to force the divergence to test the
class removal. That is run 1b, and it is the stronger test.

### 1b. Forced directory divergence — J2-L1 half (2), the class removal

Holder's serve given `XDG_RUNTIME_DIR=/tmp/j2r2xdg`; the proxy's serve given none.

```
lease row:  endpoint=/tmp/j2r2xdg/lambo/j2-r2-probeb-ab5a3737f49cb9e1.sock
proxy derives:       /tmp/lambo-501/j2-r2-probeb-ab5a3737f49cb9e1.sock
                     ^ different directory, IDENTICAL name

INFO lambo serve: the holder's endpoint directory differs from this process's — forwarding
to the published path, because the address name (a hash of the session and the store)
matches, so this is the same session on the same store reached through a different
environment published=/tmp/j2r2xdg/lambo/… derived=/tmp/lambo-501/…
```

Proxied; cross-product read-your-writes **PASS**; opencode's write landed in the shared
store. This is the claim half (2) makes — a matching name in an unexpected directory is
benign and is dialled — demonstrated against two real client products. It also surfaced
**J2-R2-4**: the headline proxy line's `endpoint=` field reports the process's *own*
derivation, which in this run is a socket that does not exist.

### 2. Phases 1–2

Covered by runs 1 and 1b above: proxy line present, cross-product read-your-writes,
lease row unchanged with the holder still cursor-agent, endpoint socket present, endpoint
directory 0700 and owned by this uid.

### 3c. The P1 flip — holder killed mid-call

Embedder behind the 20s `slow-embedder.py` shim so the holder is provably inside an embed
(shim `REQ` at `1787219352.474`, kill at `1787219352.638`).

| | `bbac803` (baseline run 5) | `a573e64` (run 3c) |
|---|---|---|
| detection | 18 ms — **and nothing sent to the client** | **2.6 ms**, and *answered*: `lost=1` |
| client outcome | `MCP error -32001: Request timed out` | `MCP error -32002` + outcome-UNKNOWN wording |
| time to that outcome | **121.7s** (the client's full per-call timeout) | **immediate** — inside the same 2.6 ms |
| opencode exit after kill | **151.7s** | **2.4s** |

The proxy's own line, and then what the model actually received:

```
09:49:12.641277  WARN lambo serve: the session holder closed the connection with calls still
in flight — each was answered with an honest 'outcome unknown' error, because this process
cannot know whether the holder applied them before it died generation=0 lost=1
```

```
✗ lambo_lambo_recall {"agent_id":"opencode-agent","query":"anything at all"} failed
Error: MCP error -32002: lambo: this call had already been handed to the process that holds
this session when that process stopped answering, so its outcome is UNKNOWN. It may have been
applied or it may not — if it was a write, treat it as neither done nor undone; if it was a
read, you received nothing. … When it answers, recall before re-deriving: repeating a write
that did land duplicates it, and repeating one that did not is the fix. …
```

Every property the fix promised, at the binary, across two products: the answer carries the
in-flight id, the code is `-32002` and not `-32001`, the wording is *unknown* and not
*nothing*, and it arrives sub-second after detection rather than at a client timeout.

### 4. Fresh-start election and idle-death honesty

**Idle death — unchanged and correct.** Kill at `09:53:45.19`; the proxy noticed the close in
**2.4 ms**; the client's next call got `-32001` with the "NOTHING WAS READ OR WRITTEN"
wording. Baseline: 18 ms detection, same `-32001`. So the −32001/−32002 split is now
demonstrated **end-to-end across two real client products**: the call that never left gets
"nothing", the call that was in flight gets "unknown". That distinction is the thing J3's
receipts inherit, and it is real.

**Fresh-start election — and this is where the new P2 lives.** The baseline's 43.6s and
phase 3's own 3a both started their fresh client *31–41s after* the kill, by which time the
dead holder's lease had nearly lapsed. Re-run as written, 3a reports `budget_secs=20
lapses_in_secs=0`, waits **1.0s** and attaches — 9s end to end. That is a real improvement
and it is not the interesting case.

So I measured the case the docstring's claim is actually about — a client starting
*immediately* after an abrupt death, with the lease freshly heartbeated:

```
lease before the kill:  expires_at 09:57:07.437,  remaining 40s
kill -9 holder, fresh serve started immediately
fresh serve exited rc=1 after 2.12s  (kill+2.17s)

lambo serve: conflict: session j2-r2-elect is already held by another writer
(cursor-agent@…#52914) — it acquired the single-writer lease 4s ago and is still refreshing
it. … the holder's endpoint is not accepting connections (Connection refused (os error 61))
That holder's lease does not lapse for 38s, and this process will not block the client that
spawned it for longer than 20s waiting … Retry in 39s, or stop the other holder.
```

| | `bbac803` | `a573e64` |
|---|---|---|
| prompt start after abrupt death | waits, **succeeds** at ~40s | **refuses at 2.12s**, client gets no tools |
| late start (≥ ~32s after death) | waits ~10s, succeeds | waits ~1s, succeeds (9s end to end) |

The behaviour is the declared trade and I do not dispute it — for `opencode`, whose gate is
31.96s, a fast honest refusal is strictly better than a slow failure. What I dispute is the
two sentences that describe it (**J2-R2-2**) and the falsehood inside the refusal
(**J2-R2-3**).

**Cleanup.** No `lambo serve` process anywhere; the slow-embedder shim stopped; my
`j2-r2-*.sock` files and `/tmp/j2r2xdg` removed; the user's llama-server on `:8080` healthy
and untouched; only project-scope client configs written; `~/lambo-dogfood` never referenced.
Artifacts beside the baseline in the scratchpad: `r2run1/`, `r2run1b/`, `r2run3/`,
`r2run3c/`, `r2elect/`, `envprobe/`, `J2-R2-NOTES.md`, and the harnesses `j2-r2-run1.sh`,
`j2-r2-run1b.sh`, `j2-r2-prompt-election.sh`.

## New findings

### J2-R2-1 (P2) — the 2 × `CONNECT_BUDGET` SIGTERM-deafness bound omits the store read that shares the arm body

`src/mcp/proxy.rs:130-134`, in `CONNECT_BUDGET`'s docstring:

> The two together bound how long the pump can be deaf to SIGTERM inside one `client_rx` arm
> body at 2 × `CONNECT_BUDGET`.

The two operations named — `connect` and `replay` — are each budgeted, so the sentence is true
*about them*. It is not true about the arm body, which is what it claims. The arm body calls
`reconnect_and_replay` → `dial()`, and `dial()` **begins** with a store round trip
(`src/mcp/proxy.rs:809-813`):

```rust
let row = self.store.read_lease(&self.session).await.map_err(LamboError::Store)?
```

Nothing puts a `CONNECT_BUDGET` over that. What bounds it is the store's own configuration:
`busy_timeout(8s)` on sqlite (`src/store/sqlite.rs:397`), `statement_timeout = 20s` on
cockroach (`src/store/cockroach.rs:641`), plus sqlx pool acquisition, which is not overridden
anywhere. A second, smaller error sits beside it: `connect` checks its deadline only *after* a
failed attempt (`:717-728`), so its own bound is `CONNECT_BUDGET + CONNECT_RETRY` plus one
attempt — about 2.1s, not 2.0s.

Real worst case for arm-body deafness: **≈12s on sqlite, ≈24s on cockroach**, against a
stated 4s.

This matters because the 4s figure is not decoration — it is the load-bearing number in a
**declared deviation**. `fdb3225` declines my round-1 suggestion to hoist the reconnect out of
the arm body on the grounds that it "restructures the pump's control flow for four seconds of
shutdown latency". At 12–24s the trade is a different trade. I still think the deviation is
defensible — a proxy holds no lease, no tail and no graph, so even 24s of SIGTERM deafness
costs durability nothing — but that argument has to be made against the real number.

**Remediation** (doc-precision, no behaviour change). Rewrite the bound to name all three
terms and the store's timeouts, e.g.: *"inside one `client_rx` arm body the pump is deaf to
SIGTERM for the row read (bounded by the store: sqlite's `busy_timeout`, cockroach's
`statement_timeout`, plus pool acquisition), then `CONNECT_BUDGET + CONNECT_RETRY` for the
connect, then `CONNECT_BUDGET` for the replay"* — and restate the deviation argument in
`fdb3225`'s terms at the same place, since "four seconds" appears there too. If a hard bound
is wanted instead, one `tokio::time::timeout(CONNECT_BUDGET, self.store.read_lease(…))` in
`dial()` delivers the sentence as originally written; that is a behaviour change and belongs
to whoever owns the decision, not to a doc fix.

### J2-R2-2 (P2) — "the majority of real cases … the wait still succeeds" is false, and so is "the client's next start succeeds"

`src/mcp/serve.rs:838-841`, in `ELECTION_BUDGET`'s docstring:

> A fast, actionable refusal beats spending a client's entire startup gate to arrive at the
> same place — and in the majority of real cases (a lease expires uniformly somewhere inside
> its TTL) the wait still succeeds.

The parenthesis is the error, and the tree's own constants refute it. `LEASE_TTL = 45s` and
`LEASE_HEARTBEAT_INTERVAL = 15s` (`src/store/lease.rs:92`, `:100`), and every refresh sets
`expires_at = now + 45s`. So at an **abrupt** death the remaining lease is `45 − u` for
`u ∈ [0,15]` — always in **[30s, 45s]**, never uniform on [0,45]. The first-pass refusal
condition (`serve.rs:987`) is `lapses_in + ELECTION_SLACK(5) > left(≈18 after the attach
attempt)`, i.e. refuse whenever `lapses_in > ≈13s`. Every value in [30,45] exceeds it.

Therefore a client that starts promptly after an abrupt holder death is refused **always**,
not in a minority of cases. Waiting succeeds only in the window where the client starts
roughly 17–32s after the death; later than that the lease has lapsed and the process simply
attaches.

**Measured live**, and the numbers are unambiguous: lease freshly refreshed with 40s
remaining, holder `kill -9`'d, fresh serve started immediately → refused `rc=1` at **2.12s**,
message "does not lapse for 38s … Retry in 39s". At `bbac803` that same start waited and
succeeded at ~40s.

The phase doc understates the same thing twice (`dev-diary/lambo-for-mooshik/J-multi-client.md:1020-1022`):

> a holder that dies immediately after a heartbeat leaves ~45s of lease, which no 20s budget
> can outlast, so that start refuses instead of recovering. The client's next start succeeds.

"Dies immediately after a heartbeat" frames the general case as an edge case — any abrupt
death lands in [30,45]. And "The client's next start succeeds" is false for an immediate
retry: at kill+5s the remaining lease is still ~33s, so the next start is refused too. The
refusal's own "Retry in 39s" is the honest number, and it contradicts the doc.

None of this is an argument for changing the budget. The trade is defensible and was declared;
`opencode`'s measured 31.96s gate means the pre-remediation 50s wait failed that client
anyway, so a 2.12s honest refusal is a real improvement for it. The defect is that the
justification written down is not the justification that survives contact with the constants —
which is precisely the register failure J2-R1-7 was.

**Remediation** (doc-precision, no behaviour change). Three edits. (1) Replace the parenthesis
at `serve.rs:840-841` with the actual distribution and the actual window: a lease is refreshed
every `LEASE_HEARTBEAT_INTERVAL`, so an abrupt death leaves `[LEASE_TTL −
LEASE_HEARTBEAT_INTERVAL, LEASE_TTL]`, and the wait succeeds for clients starting roughly
`LEASE_TTL − LEASE_HEARTBEAT_INTERVAL − 13s` to `LEASE_TTL − 13s` after the death — a prompt
start is refused by design. (2) Correct §J2's residual to say *any* abrupt death, not "dies
immediately after a heartbeat", and delete or qualify "The client's next start succeeds"
(it succeeds once the lease lapses, which the message already quantifies). (3) Consider
pinning it, since it is now a claim about arithmetic: a unit test over the
`lapses_in + ELECTION_SLACK > left` predicate asserting that `LEASE_TTL −
LEASE_HEARTBEAT_INTERVAL` refuses and that a small `lapses_in` waits would make this
falsifiable rather than prose.

### J2-R2-3 (P3) — the election refusal tells an operator a dead holder "is still refreshing it"

Verbatim from the live run above, two seconds after the holder was `kill -9`'d:

> session j2-r2-elect is already held by another writer (cursor-agent@…#52914) — it acquired
> the single-writer lease 4s ago and **is still refreshing it**. Refusing to open a second
> writer. … **the holder's endpoint is not accepting connections (Connection refused (os error
> 61))** That holder's lease does not lapse for 38s …

The message contradicts itself inside one paragraph, and the false half comes first. The
holder is not refreshing anything; it is dead, and the very next clause says so. The
`"is still refreshing it"` text is `held.message` from `build_attach` and is pre-existing, but
J2-L2 is what newly **composes** it with `probe_holder`'s reason (`serve.rs:989-1002`), and
that composition is what creates the contradiction. An operator who reads the first sentence
goes looking for a live process that no longer exists.

**Remediation.** At the composition site, suppress or soften the "is still refreshing it"
clause when `outcome` is the endpoint-refused reason — that reason is strong evidence the
holder is gone. Cheapest form: since the refusal already has both strings in hand, lead with
the probe outcome and demote `held.message`, or add one clause — "(its lease has not lapsed
yet, but its endpoint is not answering, so it has most likely died)". Assert the absence of
"still refreshing" in the endpoint-refused case in
`a_holder_whose_lease_outlasts_the_client_budget_is_refused_at_once`, which is already at the
binary and already has the message.

### J2-R2-4 (P3) — the "proxying to the session holder" line names the address this process derived, not the one it dialled

`src/mcp/proxy.rs:905-913` logs `endpoint = %self.endpoint.path().display()` — the process's
own derivation. Under J2-L1 half (2) that is by construction *not* necessarily the path the
proxy connected to. Observed live in run 1b:

```
INFO … proxying to the session holder … endpoint=/tmp/lambo-501/j2-r2-probeb-ab5a…sock
```

while the socket actually in use was `/tmp/j2r2xdg/lambo/j2-r2-probeb-ab5a…sock`. The named
file does not exist. The preceding directory-differs INFO line carries the truth, so nothing
is hidden — but the headline line is the one an operator greps for, and J2-R1-19's new doc
paragraph tells them to go look at the socket.

**Remediation.** Have `reconnect_and_replay` (or `dial`) return the dialled address and log
that in the proxy line, or add a second field: `derived=… dialled=…`. One field, and it makes
the line true in the configuration J2-L1 exists to support.

### J2-R2-5 (P3) — a dangling-symlink store path gives one store two identities across the file's creation

`canonical_store_path`'s docstring claims *"the same store reached by a symlink and reached
directly derives **one** address"*. There is a window where that is false. `canonicalize`
requires the whole path to exist, and I measured that macOS `realpath(3)` fails with ENOENT on
a **dangling** symlink and succeeds — resolving to the target — once the target exists. So for
`path = "./link.db"` where `link.db` → a not-yet-created `real.db`:

* the first serve derives on the not-exists branch → `<cwd>/link.db`;
* sqlite's `create_if_missing` writes through the link and creates `real.db`;
* the next serve derives on the exists branch → `<cwd>/real.db`.

Two identities, two address names, so the second client's `proxyable` refuses with
`EndpointIsNotOurs` — whose message blames "a different session, a different store, or a
lambo whose endpoint scheme is not this one", none of which is true. The degradation is safe
(the lease still serialises writes, so no graph is corrupted) and the configuration is exotic,
which is why this is P3 rather than a repeat of J2-R1-2. But it is the same *shape* as J2-L1 —
an address that is not stable across two processes — and the docstring states the stronger
claim.

**Remediation.** Either narrow the claim (one sentence: symlinks are resolved once the target
exists; a dangling symlink resolves to the link's own name until then) or close it by
resolving the parent chain and keeping the final component's link status in mind — e.g. try
`canonicalize(p)`, then `canonicalize(read_link(p))` relative to the parent, before falling
back. The one-sentence version is proportionate.

### J2-R2-6 (P3) — `proxyable` accepts a relative or bare published path

`proxyable` compares only `file_name()`, so a row publishing the bare name
`sess-<hash>.sock`, or `./sess-<hash>.sock`, matches and is returned as the path to dial.
`dial_dir` then takes `address.parent()`, which is `Some("")` for a bare name — and
`assert_private_dir` reports `endpoint directory  could not be inspected: No such file or
directory`, with an empty path in an operator-facing message. For `./sess-…sock` the parent is
`.`, which is *this process's* cwd, and if that happens to be 0700 and self-owned the check
passes and the connect is attempted relative to the cwd.

The store is the trust boundary and a writer who can forge the row can already write graph
content, so this is robustness rather than a vulnerability — which is why the round-1
reasoning about the trust boundary still stands. But "the directory only decides
reachability" leans on the directory check, and the directory check should not be handed a
relative path.

**Remediation.** In `proxyable`, require `published.is_absolute()` before the name comparison,
returning `EndpointIsNotOurs` otherwise. One line, and it makes the empty-path message
unreachable. Add the case to
`a_holder_publishing_a_different_address_name_is_not_proxyable`, which already covers a
nameless path.

### J2-R2-7 (P3) — the pump's `inflight` list is unbounded and linearly scanned

`inflight: Vec<(u64, serde_json::Value)>` grows one entry per forwarded request and shrinks
only on a response or a `Closed` drain. A holder that accepts frames and never answers lets it
grow without bound, and every holder response costs an O(n) `position` scan
(`src/mcp/proxy.rs:1064-1066`). For real MCP traffic n is a handful and neither cost is
visible; `MAX_FRAME_BYTES` was added for the analogous unbounded-growth case (J2-R1-18), so
the asymmetry is worth naming rather than a defect worth fixing urgently. The client is also
the local, trusted party here.

**Remediation.** Either state at the declaration that the list is bounded in practice by the
client's own in-flight window and why that is acceptable, or add a cap that answers the oldest
id with `HUB_LOST_MESSAGE` past some generous ceiling. The comment is the proportionate fix.

## Attacks that did not land

Recorded so they are not re-run. Round 1's own list still holds and I did not repeat it.

* **Every one of the five P1-seam attacks my brief named** — see the table in Part A. The
  `Closed`-is-terminal property is what makes the "any generation" retirement safe, and it is
  a real property of `split_hub`, not an accident.
* **The −32001/−32002 boundary under a partial write.** Bytes can leave the process while the
  client is told nothing was written, but they cannot form a frame, the holder never parses
  them, and the connection is abandoned. The claim is about the graph write and holds.
* **Case-insensitive-filesystem divergence in `canonical_store_path`.** I measured that macOS
  `realpath(3)` case-folds the final component to the on-disk spelling, and that a file
  created through a given spelling carries that spelling — so the exists and not-exists
  branches cannot disagree on case. (Python's `os.path.realpath` does *not* case-fold, which
  is a trap for anyone probing this without `ctypes`.)
* **Hardlinked / bind-mounted store files.** Two hardlinks to one sqlite file canonicalize to
  two paths and so derive two addresses. Not a defect: the lease lives *in* the database, so
  only one holder wins, and the loser refuses rather than corrupting anything. No
  canonicalization can resolve a hardlink, and the outcome is safe.
* **Cross-uid name collision.** The address name carries no uid (the directory does), so two
  uids on one box derive the same *name* for the same session and store. Harmless:
  `assert_private_dir`'s ownership check refuses the other user's directory on the dial side,
  and `/tmp/lambo-<uid>` keeps the bind side apart.
* **The reconnect/drain interleave.** One pump, one `select!`; the replay and the drain are in
  different arms and cannot overlap. Preamble frames are forwarded before the triggering
  frame, so client-visible ordering is preserved.
* **`build_memory` drift.** Kept deliberately as a library entry point with a docstring saying
  no call site remains; `rg` confirms the serve path does not use it.

## Positive observations

* **The P1 fix is the real thing, and the live probe is what proves it.** 151.7s of client
  hang became 2.4s, and the mechanism is visible in one log line with `lost=1`. The
  `-32001`/`-32002` split — which could easily have been a paper distinction — showed up
  *correctly on both sides* in two different live scenarios against two different client
  products: "nothing" for the call that never left, "unknown" for the call that was inside the
  holder. That is the distinction J3's receipts inherit, and it now exists in measured form.
* **Mutation 2 is the cleanest evidence in this round.** The same `dial()` short-circuit that
  left every test green at round 1 now turns two red. That is a register rule enforced, not
  described — and the remediation's note that my *prescribed* fix would not have been red
  (correct: the writer is live from the preceding call, so `dial` is never entered) is a
  reviewer being corrected on the merits, which is the right outcome.
* **`read_frame` closed three findings with one abstraction** without becoming a parser. Torn,
  Oversize and NotUtf8 each get the behaviour its failure mode deserves, and the two lossy
  cases resynchronise at the newline instead of splitting one bad frame into several.
* **J2-L1's two halves are genuinely different arguments** — incidence and class — and both
  are needed. Half (2) held up under a divergence I constructed specifically to break it.
* **J2-R1-20 was answered by disproving the reviewer.** The count was right and the recording
  was incomplete; the remediation stashed itself to verify that at `bbac803` and then recorded
  the invocation beside every count. That is the correct response to a bad finding.
* **`assert_private_dir` factored out for symmetry** is the kind of hardening that follows from
  taking a fix seriously: half (2) lets a proxy dial a directory it did not derive, so the
  check that governs binding now governs dialling.
* **Both new P2s are honesty defects in the remediation's own favour-free direction** — the
  code is more conservative than the prose claims, not less. Nothing ships broken.

## Gate results

All re-run from scratch in the worktree at `a573e64`,
`CARGO_TARGET_DIR=/Users/narayan/Documents/work/lambo/target`.

| Gate | Claimed | Re-derived | |
|---|---|---|---|
| `cargo fmt --check` | clean | **clean** | ✓ |
| `cargo clippy --all-targets -D warnings` (default) | clean | **clean** | ✓ |
| …`--features store-sqlite,embed-fixture,fixtures` | clean | **clean** | ✓ |
| …`--features ship,fixtures` | clean | **clean** | ✓ |
| …`--no-default-features --features store-cockroach,embed-fixture` | clean | **clean** | ✓ |
| `cargo test --all --features fixtures` | 853/0/3 | **853/0/3** | ✓ exact |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | 940/0/3 | **940/0/3** | ✓ exact |
| `cargo test --no-default-features --features store-cockroach` | 545/0/0 | **545/0/0** | ✓ exact |
| `scripts/observability/verify.sh` | ALL CHECKS PASSED | **ALL CHECKS PASSED** | ✓ |

**Every delta accounted for by named test.** Test-name sets extracted at both revisions
(including the `#[tokio::test(…)]` parameterized forms, which a naive sweep misses): **27
added, 1 removed**, net **+26**. The one removal is the J2-R1-5 rename
(`an_over_long_base_directory_is_refused_before_any_lease` →
`…_is_reported_by_the_derivation`). Of the 27, **11** are in `src/mcp/endpoint.rs`, **11** in
`src/mcp/proxy.rs`, **5** in `tests/serve_proxy_multi_client.rs`. Reconciliation:

* `src/` net = 21 → fixtures 832 + 21 = **853** ✓; cockroach 524 + 21 = **545** ✓
* `src/` + `tests/` = 26 → sqlite 914 + 26 = **940** ✓

The five integration tests are absent from the fixtures and cockroach profiles because they
need `store-sqlite`, which is exactly the 5-test gap between the deltas.

### Mutations — all five reproduced, run and reverted

| Mutation | Expected | Observed |
|---|---|---|
| 1. `answer_lost` returns `Ok(0)` (the pre-fix wedge) | P1 test red at the bound | **red at 31.20s** — `no frame with id 2 (a call in flight when the holder died): timed out waiting on channel`. Same build with the gate off: **green in 1.02s** (the commit claimed 1.2s) |
| 2. `dial()` short-circuited to a bare `connect(&self.endpoint)` | the J2-R1-6 pin red | **two red**: `a_live_endpoint_with_no_lease_row_is_refused_rather_than_dialled` and `a_holder_reachable_only_at_its_own_directory_is_still_forwarded_to`; other 5 green. At round 1 this mutation left everything green |
| 3. `proxyable` restored to whole-path comparison | unit + integration red | **both red**: `a_holder_publishing_the_same_address_in_another_directory_is_proxyable` (5 other proxyable unit tests green) and `a_holder_reachable_only_at_its_own_directory_is_still_forwarded_to` (6 others green) |
| 4. `ELECTION_BUDGET` → `LEASE_TTL + ELECTION_SLACK`, arithmetic removed | red at ~45.1s | **red at 45.97s** — "the refusal must not spend the client's startup budget to reach the same answer: 45.973868583s". Arithmetic confirmed: `LEASE_TTL` 45s + one `ELECTION_RETRY` poll + setup |
| 5. `proxy.run(std::future::pending())` (the negative control) | proxy case red, holder case green | **red** — `a_pre_handshake_sigterm_to_a_proxy_exits_cleanly_and_leaves_the_holder_intact` at `tests/serve_pre_handshake_durability.rs:370`; `a_pre_handshake_sigterm_still_flushes_the_session_row` green |

**Verification-only edits, all reverted from byte copies taken before each edit.**
`src/mcp/proxy.rs` restored to `cbf691878606724413dbcbacdeb2d390d0980265` and
`src/mcp/serve.rs` to `1ec531882aaffc5090939a3b097a021251220035`; `git status --short` empty
but for this review file.

## Verdict

**REQUEST_CHANGES.** Two P2, five P3. All 23 prior findings closed.

This is a thorough remediation and most of it is better than the round-1 review asked for.
The P1 fix is correct at the mechanism and — the part that matters — correct at the binary
against two real client products, where a 151.7s hang became a 2.6 ms honest answer carrying
the right error code and the right wording. J2-L1's two halves are separately argued and both
survive attack, including a divergence I built specifically to break half (2). Three findings
were answered by *disagreeing* with me on the merits (the reconnect hoist, the discriminating
test window, the cockroach count), and in all three the remediation is right. Mutation 2 is
the cleanest single piece of evidence in this round: the exact short-circuit that pinned
nothing at round 1 now turns two tests red.

The two blockers are not defects in the code. They are both a **number quoted as an exact
bound, used to justify a shipped decision, and wrong**:

* **J2-R2-1** — the arm body's SIGTERM deafness is bounded at 4s only if you ignore the store
  read that opens `dial()`. The real figure is ~12s on sqlite and ~24s on cockroach, and the
  4s figure is what `fdb3225` uses to decline the reconnect hoist.
* **J2-R2-2** — "in the majority of real cases the wait still succeeds" is refuted by the
  tree's own `LEASE_TTL`/`LEASE_HEARTBEAT_INTERVAL` pair and by a live measurement: a prompt
  start after an abrupt death is refused **always**, not rarely, and the phase doc's "the
  client's next start succeeds" is false for an immediate retry.

I am grading these P2 rather than P3 deliberately. Both fixes are pure doc-precision — no
behaviour change is required, and I do not think either decision should be reversed. But this
is the third time in J2 that a *stated reason* has been the defect rather than the code
(J2-R1-6's "a new holder is a new endpoint", J2-R1-7's nine `build_memory` sites, and now
these), and in both earlier cases the false sentence was what let the gap survive a sweep.
Carrying a false justification forward is the mechanism by which J2-R1-7 happened.

**Do they meet the carryover bar?** Mechanically, yes: both are doc-precision with no
behaviour, which is exactly the carryover shape. My recommendation is nonetheless a **short
round 3** rather than carryover, for one reason — J2-R2-2's remediation is the kind that
benefits from being pinned (it is now a claim about arithmetic, and a five-line unit test over
the `lapses_in + ELECTION_SLACK > left` predicate would make it falsifiable), and a carried
doc item does not usually acquire a test. If the operator prefers carryover, the two P2s plus
the five P3s are ~40 lines of prose and three one-line code edits, and none of them blocks
integration on correctness grounds.

**Integration readiness — what can now be claimed.** §J2's two-product `Done when` box can be
ticked, and this is the first round in which that is true. Concretely, verified at the binary
with `cursor-agent` and `opencode` on one machine, one store, one session:

* two **different client products** attach to one session, the loser proxies rather than
  exiting 1, and cross-product read-your-writes works in both directions;
* it works on **unmodified default wiring** — no env alignment, which is the outage J2-L1
  found and the thing that was false at `bbac803`;
* it works even when the two products derive **different endpoint directories**, provided the
  address name matches (forced and verified);
* a holder death with a call **in flight** is answered in 2.6 ms with `-32002` and
  outcome-unknown wording, rather than hanging for the client's timeout;
* a holder death with **no** call in flight is answered with `-32001` and
  nothing-was-written wording — the split is real and demonstrated end to end;
* the endpoint directory is 0700 and self-owned, and a stale socket is unlinked under lease.

What may **not** be claimed: that a client starting promptly after an abrupt holder death
recovers within that start. It does not — it is refused in ~2s with an actionable retry
interval, by design (J2-L2), and the next start recovers once the lease lapses. That belongs
in the `Done when` box's wording, not only in a residual paragraph.

## Residuals handed forward

* **J3** — the `-32001`/`-32002` split is now load-bearing and *measured*, so receipts should
  inherit it rather than reinvent it: a receipt outstanding across a holder death is the
  unknown-outcome case, and the wording that resolves it safely ("recall before re-deriving")
  already exists. J2-R2-7's unbounded `inflight` list becomes materially more interesting under
  J3, because a receipt's lifetime is longer than a call's — whatever J3 does about
  outstanding receipts should decide the cap that J2 currently does not need.
* **J4** — round 1 asked for a third proxy state in the ledger ("an in-flight call lost with
  the holder"); it now exists as a WARN with a `lost` count and is the obvious thing to record.
  Add a fourth from this round: *an election refused because the lease outlasts the client
  budget*, which is now a common, deliberate outcome (measured 2.12s) and is currently visible
  only on the refused process's stderr.
* **J5** — J2-R2-1 and J2-R2-2 are both doc-precision inside `src/`, so they are not J5's; but
  J2-R2-2's second half lands in `J-multi-client.md`, and if it is carried rather than fixed it
  should ride the same act as the DOGFOOD-SETUP re-pin so the runbook's "(up to ~20s)" and the
  residual agree. J2-R1-19's new `mcp.mdx` paragraphs are a good test case for the mirror gate
  §J2 specified (shared prose, link prefixes normalised, site-only sections excluded).
* **The DOGFOOD-SETUP re-pin** — add one line to §6's smoke test from this round: the endpoint
  the lease publishes may legitimately not be the path a given serve derives, so the check is
  "the published socket exists and its directory is 0700 and yours", not "the socket is where I
  expected". J2-R2-4 is the log line that currently makes that confusing.
* **Beyond J** — in-process promotion is the only thing that removes J2-L2's residual
  (a wait that does not block the MCP `initialize` response), and §J2 scoped it out with an
  argument this remediation correctly declined to reopen. When it is reopened, it inherits the
  wedge invariant, J2-R1-2 (a promoted proxy *binds*, and binding the wrong graph's socket is
  worse than dialling it), and now J2-R2-1's real arm-body bound.
* **Harness note for the next live probe** — a `/bin/sh -c` wrapper around an MCP server
  reintroduces `TMPDIR` and therefore *hides* the J2-L1 divergence. Any future two-product
  probe that wraps the command for logging is testing an aligned environment without saying so;
  force the divergence with `XDG_RUNTIME_DIR` on one side, as run 1b does.

---

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>

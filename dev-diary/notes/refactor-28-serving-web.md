# #28: MCP serving and the web portal split (decisions)

Refactor 5/5, the last of the series. Base: #27's head `cef1046` (main with
#24 to #27, Rust 1.99, edition 2024), fast-forwarded to main `1729485` before
the first commit. Decisions and why; the commits carry the mechanics.

## Same pattern as #27: the root keeps the composition, children hold steps

Each file became a module root that keeps its composition (the function
whose body is the order of things), with child modules holding the steps by
responsibility. Items private to the old file became `pub(super)`. Every
`pub` item and every `pub(crate)` item something outside the module names is
re-exported at its old path (`crate::mcp::serve::{EarlyShutdown,
run_and_close, close_bounded_until, CLOSE_FLUSH_GRACE}`,
`crate::mcp::proxy::{connect, dial_dir, INFLIGHT_DEPTH_WARN}`, and every
public item of the three modules). `pub(crate)` items nothing outside named
(`HttpGuard`, `RateLimiter`, `LiveSessions`) are reached at their new paths.

| module | holds | lines |
|---|---|---|
| `mcp/serve.rs` | lifecycle map, `Transport`, `ServeOptions`, `serve()` | 570 |
| `mcp/serve/builder.rs` | `resolve_serve_backends`, `serve_builder`, `build_memory`, `explain_startup_failure` | 140 |
| `mcp/serve/roles.rs` | election constants, `Role`, `resolve_role`, refusal-message repair, `record_refused_loser` | 410 |
| `mcp/serve/hub.rs` | every Unix-socket touch: `derive_endpoint`, `probe_holder`, `bind_hub`/`Hub::release`, `serve_endpoint` and its tracked sessions | 335 |
| `mcp/serve/heartbeat.rs` | `authorize_ledger`, heartbeat, startup line, refusal poller + `RefusalCursor`, `log_events` | 287 |
| `mcp/serve/http_guards.rs` | `SecretToken`, `authorize_bind`, `RateLimiter`, session cap, body cap, `guard_request` | 425 |
| `mcp/serve/transport.rs` | `Exit`, `run_until_shutdown`, `setup_or_shutdown`, stdio, HTTP | 313 |
| `mcp/serve/signals.rs` | `shutdown_signal`, `shutdown_signal_counter`, `EarlyShutdown` + `AttachShutdown` impl | 459 |
| `mcp/serve/shutdown.rs` | grace budgets + asserts, `HolderShutdown`/`wind_down`, `run_and_close`, `close_bounded*`, `HolderTasks`, `close_ledger`, the stage table | 574 |
| `mcp/proxy.rs` | `HubProxy`, `new`, the pump `run`, interface with serve | 617 |
| `mcp/proxy/dialing.rs` | budgets + assert, `NotProxyable`, `proxyable`, `dial_dir`, `connect`, the raced dial | 488 |
| `mcp/proxy/handshake.rs` | `Handshake` and its replay | 217 |
| `mcp/proxy/forwarding.rs` | request/response ids, holder reader task, framed write, in-flight warning + receipt assert | 186 |
| `mcp/proxy/disconnect.rs` | unreachable/lost codes and messages, `answer_lost`, `client_gone` | 154 |
| `mcp/proxy/framing.rs` | `Framed`, `read_frame` | 105 |
| `cli/serve_web.rs` | assets, `Args`, `run`, `serve_bounded`, signal registration | 361 |
| `cli/serve_web/auth.rs` | `AuthToken`, resolution, bind refusal, bearer gate | 134 |
| `cli/serve_web/state.rs` | `AppState`, `Freshness` | 49 |
| `cli/serve_web/dto.rs` | every response and query type | 303 |
| `cli/serve_web/projections.rs` | hop-1 structural dependents, event feed order, stats | 248 |
| `cli/serve_web/routes.rs` | handlers, `router`, `/api/graph` caps | 425 |
| `mcp/endpoint.rs` | unchanged production (the #39 seam) | 830 |
| `mcp/endpoint/tests.rs` | its former inline tests, same module path | 740 |
| `surface/bearer.rs` | the shared bearer check (fix below) and its unit tests | 212 |

Before: `serve.rs` 3,117, `proxy.rs` 1,667, `serve_web.rs` 1,469,
`endpoint.rs` 1,571.

**`serve()` stays whole, in the root.** It is 132 lines of code in 368
with its comments, and its body *is* the lifecycle order the issue asks to
keep visible. Splitting it would scatter the arming argument (why each
startup step sits where it does) across files. The root's module doc lists
the five phases and which module holds each step.

Verified by script (`bodycmp`, every fn, struct, enum, const, static, type
and const-assert, base `42cf536` after the two fixes, against head): 259 of
264 items identical once whitespace and rustfmt's trailing commas are
normalised (nine moved items were re-wrapped by rustfmt because `pub(super)`
lengthened their signatures). The five that differ are the two deliberate
refactors below (`Role`, `resolve_role`, `run_and_close`, `serve`) and the
endpoint source-scan anchor.

## The #39 platform seam: `serve::hub`

`endpoint.rs` and `proxy.rs` are Unix by nature (a Unix socket, its file
identity, the directory-ownership checks). What made a gate hard was that
`serve()` bound the endpoint inline, held the `SocketIdentity` in a local and
unlinked it at exit, and `roles`/`builder` named the endpoint and proxy types
from their own modules. Now `serve::hub` is the only serving module that
reaches `mcp::endpoint` or `mcp::proxy`: `derive_endpoint` (pre-lease),
`probe_holder`, `bind_hub` (the former inline block, same logic and log
lines), `Hub::release` (accept-loop abort, then `unlink_if_ours` with the
captured identity, at the same point after the close), and re-exports of
`SessionEndpoint` and `HubProxy` for the rest of `serve`. Behaviour
unchanged.

That makes `hub` the only serve file that *reaches* the Unix modules, not
the only one that *names* their types. A #39 gate therefore covers:

- `mcp/mod.rs`: the `pub mod endpoint` / `pub mod proxy` declarations and
  the `pub use endpoint::SessionEndpoint` re-export;
- `endpoint.rs`, `proxy.rs` and `serve/hub.rs` themselves;
- the type names that still flow through `hub`'s re-exports:
  `SessionEndpoint` in `builder.rs` (`serve_builder` and the public
  `build_memory` signature) and `roles.rs` (`resolve_role`), and `HubProxy`
  in `roles.rs` (`Role::Proxy`) and `serve()` (`HubProxy::run`). Either
  `hub` supplies non-Unix stand-ins for those two types, or #39 keeps
  `SessionEndpoint` compiled everywhere (its derivation is
  platform-neutral) and gates only the socket operations.

`heartbeat.rs` named `SessionEndpoint` only for `serve_startup_line`'s unused
parameter; the remediation dropped it, so it no longer appears in the
list.

## The #40 shutdown stages

`serve/shutdown.rs` documents the holder's shutdown as seven named stages:
transport drain, keep-warm abort, session close (`close_bounded` around
`Memory::close` and its own numbered stages in `memory/shutdown.rs`), event
pump abort, background tasks (`HolderTasks::stop`: heartbeat, keep-warm
again, refusal poller), endpoint release (`Hub::release`), ledger close
(`close_ledger`). Stages 1 to 4 stay inside `run_and_close`, the seam the
"close always runs" tests drive, marked by stage comments; 5 to 7 are named
functions `serve()` calls in the old order. Spawn points, abort order, the
idempotent second keep-warm abort (#13) and the ledger's close line are
unchanged. No logging added (that is #40).

**Stage 6 now does what the table says (review L2, own `fix(serve)`
commit).** The endpoint ran each proxy connection in a detached task, and
`Hub::release` aborted only the accept loop, so proxy sessions outlived
stage 6 until the runtime dropped: a call in that window reached a closed
`Memory` and could append to the ledger during stage 7. The hub now owns the
sessions (a `JoinSet`) and `release` (now async) signals each to stop; each
cancels its rmcp service and awaits it (rmcp drains in-flight responses for
up to 2 s, then closes the connection), bounded by
`hub::ENDPOINT_RELEASE_GRACE` (3 s), after which stragglers are aborted with
a WARN. The proxy sees what a holder exit already gives it, at stage 6
instead of at runtime drop. The wait is after the lease release and the
final flush, outside `SHUTDOWN_BUDGET` like the ledger's 0.5 s drain, so a
supervisor timeout (#40's `ExitTimeOut`) should cover 15 + 3 + 0.5 s. The
residue the stage table now names: rmcp runs each tool call in a task of its
own, which a cancelled service waits on for its drain but does not join, so
a call still running past that can append after stage 7 begins, where the
ledger counts it as `write_failed`. Pinned by `serve::tests::hub_release`
(fails on the old release). No overlap with #40's stall itself: that one
logged no `session closed` line, so it stalled at stage 3 or earlier.

## Defect fixed on the way (own commit, `36fd498`)

**The portal's token comparator.** T1-P3-1 (full-stack sweep, 2026-08-16)
flagged the comparators in both HTTP surfaces. Its remediation (`5a6f633`)
rewrote only the portal's, and reversed its loop to run over the *expected*
token so that "the input cannot leak its length". The input's length is the
caller's own; what that made observable is an iteration count fixed by the
secret's length, which is the property `mcp::serve`'s comparator documents it
protects. One rule, two copies, opposite guarantees. Both surfaces now call
`crate::surface::bearer` (serve's loop direction); each keeps its token type
and messages, and results are identical for every input. This deviates from
the remediation's chosen direction deliberately; the argument is above and in
the module doc. Pinned by `cli::serve_web::tests::auth::the_portal_uses_the_shared_bearer_check`
(fails on the previous tree). Boolean results cannot distinguish the two
loops, so that pin is structural, and on its own it did not guard the
direction: flipping the shared loop back kept every test green (review L1).
The remediation factored the loop into `surface::bearer::fold_diff`, which
takes a per-iteration callback (a no-op in production), and
`surface::bearer::tests::the_loop_count_follows_the_presented_length_not_the_secret`
counts iterations over a grid of presented and secret lengths; flipping the
loop fails that test and only that test. The doc also now says the check is
stronger on length than `subtle`'s slice `ct_eq` (which returns early on a
length mismatch) and names the one secret-dependent operation left, the
`%` by the secret's length.

Also fixed (comment-only, `42cf536`): two comments in `serve.rs` truncated
mid-sentence by earlier edits, and `wind_down`'s whole doc block sitting on
`HolderShutdown` (rustdoc attached both to the struct; `wind_down` had none),
the failure JE2E-R2-1 once recorded for `shutdown_signal`.

## Source-scan hazards

- **Portal scans.** `routes::{the_module_registers_only_get_routes,
  routes_constant_covers_every_registered_route}` and the new bearer scan read
  `PRODUCTION_SOURCES` in `cli/serve_web/tests/mod.rs`, every portal
  production file; the router scan finds the one file holding `fn router(`.
  `routes::the_source_scans_cover_every_production_file` (new) walks
  `src/cli/serve_web/` at test time, recursively and skipping only `tests/`
  (review L3: it first read the top level only), and fails if a file is
  missing from that list, so the next split, into a subdirectory or not,
  cannot silently narrow the scans. Checked by
  mutation: a POST route in `routes.rs` fails both route scans, an unlisted
  file fails the coverage test, a gate bypassing `surface::bearer` in
  `auth.rs` fails the bearer scan.
- **Asset anchors.** `include_str!("../../web/...")` stays in `serve_web.rs`;
  nothing moved, nothing to anchor.
- **Endpoint scan.** `store_is_shareable_is_ruled_not_defaulted` read
  `include_str!("endpoint.rs")` relative to its file; moving the tests to
  `endpoint/tests.rs` would have broken the path, so it now names
  `src/mcp/endpoint.rs` from `CARGO_MANIFEST_DIR`.

## Placed, not moved

- The portal's hop-1 `structural_dependents` and `is_structural` (#25 left
  them for this phase) are web read projections in
  `serve_web/projections.rs`. They return a web DTO and are structural-only at
  hop 1, so they are not `surface::neighbourhood`'s projection.
- The portal's request bounds already come from `crate::surface` through
  `crate::cli::caps` (`MAX_INSPECT_NODES`, `check_size_cli` over
  `surface::validate::check_size`). `/api/graph`'s node and edge caps are
  response caps of that one handler and stay beside it.
- The portal's `shutdown_signal` duplicates serve's eager registration. Left
  as is: it is a reader with no tail to protect, its own copy needs no
  pre-arm, and sharing it would make `cli` depend on serve's signal module
  for twelve lines.

## For #31 and #32

- #31 (defer the embedder load until the role is known) touches the
  pre-lease group in `serve()` and `builder::serve_builder`; `resolve_role`
  still takes the builder by value, so the proxy releases the model.
- #32 (multi-session serving): the per-session lifecycle pieces are now
  separable: attach (`builder` + `roles`), the holder tasks (`HolderTasks`),
  the endpoint (`Hub`), and the staged shutdown. A shared runtime would own
  one `Hub` and the transports, and one `HolderTasks` plus one `Memory` per
  session; the stage table is where a per-session detach stops short of
  stages 6 and 7.

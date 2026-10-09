# #32 PR 5: credentials and authority (decisions)

Base: main `2244134` (PRs 1 to 4 merged). Design of record: the approved #32
design, sections 6.1, 6.2, 6.4 and 6.5, 2.4 and 7, and the PR 5 row of
section 8, which is the acceptance list. Decisions and why; the commits
carry the mechanics.

PR 5 makes an HTTP `lambo serve` enforce `[[serve.credential]]`. It still
serves **pinned** sessions only: on-demand attach is PR 6, the admin surface
and in-serve erase PR 7.

## Module map

| module | holds |
|---|---|
| `surface/bearer.rs` | `bearer_credential` (the header parse on its own) and `match_any`, the constant-time scan over several tokens |
| `surface/session.rs` | `SessionAuthority<T: BearerSecret>` (a surface's credential set), `BearerSecret`, `SessionScope::pinned`, `SessionGrant::legacy_default` / `implicit_local`, the reserved names |
| `mcp/serve/authority.rs` (new) | `serve_authority` (the serve's set from `ServeOptions`), `check_serve_credentials`, `any_credential`, `authorize_default`, `Authenticated` |
| `mcp/serve/http_guards.rs` | the guard resolves the bearer to a grant and attaches it to the request |
| `mcp/serve/transport.rs` | `http_app` (the router behind the guards, what `serve_http` serves and the tests serve); the routes authorize before the registry lookup |
| `mcp/serve/openers.rs` (second review) | `Openers` (which credential opened each MCP session) and `AttributingSessions`, the session manager that records the opener at the mint |
| `main.rs` | the preflight reads `LAMBO_AUTH_TOKEN` and, over HTTP, the credentials' variables, before any backend |

## Decisions

**Who has which credential.** Design §6.1, as a table in `authority.rs`:
nothing configured on loopback is the implicit `local` grant (every request,
no header checked: today's loopback behaviour, byte for byte); nothing
configured beyond loopback is refused by `authorize_bind`; the legacy token
is the grant `default`; `[[serve.credential]]` entries are their own grants,
beside `default` when both exist. `local` is gone as soon as anything is
configured, so a loopback serve with `[[serve.credential]]` now requires a
token on every request (changelog Breaking). `authorize_bind` counts any
credential, legacy or configured.

**`default` and `local` cover "every pinned session", a fourth scope part.**
`"*"` also covers every name inside any credential's prefix, which is wider
than the design's "the pinned sessions". `SessionScope::pinned()` is that
scope; no configured credential can express it (the parser has no syntax for
it), so it cannot be confused with `"*"`.

**The credential set lives in `surface::session`, generic over the secret.**
The brief: the authorization types are the ones #4's portal shares.
`SessionAuthority<T: BearerSecret>` holds `(secret, grant)` pairs or one
implicit grant, never both; `BearerSecret` is implemented by the serve's
`SecretToken` (and can be by the portal's `AuthToken`), so the secret never
leaves the redacting type as a string. The serve's set is
`SessionAuthority<SecretToken>`; nothing serve-specific is in the shared
type except what the design names for both surfaces.

**The scan is constant-time across credentials.** `match_any` compares the
presented token with every configured one through the shared #28 comparator
(`tokens_match`), never returns early, and folds the winning index in with a
mask, so the time depends on the number of credentials and the presented
length only. A request with no header is scanned as an empty token, so "no
header" and "wrong token" cost the same. A unit test counts comparisons per
position (an early return fails it).

**The order is §6.2's, and the registry comes last (PR 4 review L6).** The
guard runs step 1 alone (401, today's body) and attaches the grant as
`Authenticated`; the router runs steps 2 and 3 (`parse_addressed`, then the
grant's scope) in memory, and only then calls `registry.lookup`. So an
out-of-scope caller gets the uniform 404 for a live, held, detaching or
failed session alike; the 503s are visible only inside scope. A request that
reaches the router without `Authenticated` (a router served without the
guard) is refused, never served.

**`/mcp` is authorized like its own route.** An addressable default is
authorized exactly as `/mcp/s/{default}`. A one-session serve keeps
`--session`'s loose rule, so its default may not be addressable; no exact
name or prefix can spell such a session, so only a scope over every pinned
session reaches it (`"*"`, `default`, `local`). The dogfood rig
(`--session lambo-dev` + `LAMBO_AUTH_TOKEN`) is the `default` grant over
its one pinned session: unchanged.

**Absent in scope is the 404, `create` or not, until PR 6.** The acceptance
row: a `create`-less credential is refused on an absent session (404) and
served on an existing one. With pinned sessions only, "existing" means
pinned; a credential with `create` also gets the 404 for an unpinned id,
because nothing attaches on demand yet. The refusal is logged with reason
`Absent` (never the id). PR 6 turns that arm into the on-demand attach for
a credential with `create`, after the scope check.

**What the guard answers is id-independent.** 401, 429, the MCP-session
cap's 503 and 413 come from the guard before routing and depend on nothing
about the addressed session, so they are no oracle. On a 401 the server
orders `content-length` and `connection` differently for an unrouted path
than for a routed one (pre-existing): that says a path is routed, which the
routes are public anyway; the test pins full-byte identity across every id
on the session routes and status-and-body identity on unrouted paths.

**Credentials are resolved in the CLI's preflight, HTTP only.** An unset,
empty or non-UTF-8 variable is exit 2 before any backend is built (PR 4's
preflight rule). A stdio serve authenticates nobody, so it never reads the
credentials' variables: a stdio client sharing one `lambo.toml` with an
HTTP hub needs none of its secrets. `LAMBO_AUTH_TOKEN` moved into the
preflight too, so an empty one no longer waits for a model load.

**The legacy-token collision (PR 1 review).** `check_serve_credentials`
refuses a configured token equal to the legacy one, two credentials with one
name or one token, and a configured credential named `default` or `local`,
naming the credential and never a value. The CLI runs it in the preflight;
`serve_authority` runs it again pre-lease, so a library caller building
`ServeOptions` without the `[serve]` parser meets it too.

**The notice.** `[[serve.credential]]` leaves `unenforced_keys` for both
transports (stdio: not applicable, as `--auth-token` is ignored there), and
the notice drops its "authenticates only with --auth-token" clause.

## Tests

- Unit: `surface::bearer` (the scan's answers and its comparison count),
  `surface::session` (the pinned scope, the authority's resolve and
  authorize), `mcp::serve::tests::authority` (which credentials exist for
  which configuration, the startup refusals, the bind rule, `/mcp`'s
  authorization including a loose default).
- Wire (`mcp::serve::tests::registry::authority`), through `http_app`: an
  out-of-scope caller's responses for live, detaching, held and failed
  sessions and for unknown, malformed, percent-encoded and oversized ids are
  byte-identical to an unrouted path on every method, and the recording
  store (`Shared` now records every call) sees no call in that window except
  a live session's lease heartbeat; in scope the 503s appear; the
  `create`-less rule; one 401 for every id. Mutation: dropping the scope
  check fails the slot-state test.
- Real serve (`pinned_serve`): a session held by another writer at startup
  is the unrouted 404 for an out-of-scope credential and 503 in scope.
- Spawned (`tests/serve_credentials.rs`): each credential reaches only its
  session; the rig's legacy shape is unchanged; an unset variable and a
  shared legacy token are exit 2 before the backends, never quoting a token.

## Not in this PR

- The per-credential `vectors` capability (#22 recommendation 14): #22 PR 4
  adds `accept_client_vectors` as process config in parallel. Making it a
  credential capability is a follow-up once both have landed.
- On-demand attach and `create` (PR 6), `erase` / `admin` and the admin
  routes (PR 7). Both flags are parsed and carried on the grant; neither
  changes an answer yet.

## For PR 6 and 7

- PR 6: `Lookup::NotHosted` in `serve_session` is reached only after the
  grant is authorized for the id; the on-demand attach goes there, gated on
  `grant.capabilities().create` (or an existing lease row for a credential
  without it, decision 3). Keep the attach after the scope check, so an
  out-of-scope request still makes no store call.
- PR 7: route the admin surface through `SessionAuthority::authorize` with
  `SessionNeed::Erase` / `Admin`; scope is checked before capability, so a
  missing capability is the same 404.

## Review remediation (2026-10-09)

The Opus review of PR 5 (M1, M2, L1-L5, I1-I6) was remediated on the same
branch, one commit per finding.

- **M1, Host.** Each session's streamable-HTTP service takes a `HostCheck`
  (`session.rs`). The implicit `local` credential and stdio keep rmcp's
  loopback allow-list (DNS-rebinding protection); a serve whose authority
  `requires_bearer()` disables it. The design has no allowed-hosts list
  (§6, §7), so none was added: the bearer token is the protection, since a
  rebound page cannot present one.
- **M2, fairness.** §3.6 keeps `max_sessions` process-wide and gives no
  per-credential numbers, so these were chosen: one rate bucket per
  credential at `--rate-limit-rps` (`CredentialRates`), and a per-credential
  share of the cap, `max(1, floor(max_sessions / credentials))`, checked
  after the process cap. Rounding down keeps the shares inside the cap, so
  every credential, the operator's included, can always open its share.
  Idle shares are not lent: an operator who needs more raises
  `--max-sessions`. One credential means the whole cap and one bucket at the
  old rate, so the rig and every single-token serve are unchanged.
  `LiveSessions::live_opened_by` counts by opener, using L1's map.
- **L1, MCP-session binding.** `AttachedSession.openers` maps each MCP
  session id to the credential whose `initialize` minted it (pruned against
  rmcp's map on every insert; recorded at the mint since the second review,
  below). `transport::serve_live` replaces a foreign id
  with a value rmcp never mints before rmcp sees the request, so the answer
  is rmcp's own unknown-id answer, byte for byte by construction.
- **L2.** A presented credential over 4 KiB is refused before the scan;
  `SecretToken` refuses a longer token; the 401 WARN is throttled to once
  per 10 s with a `held_back` count.
- **L3.** `SecretToken::new` refuses surrounding whitespace and bytes outside
  0x20-0x7E (an inner space is presentable and stays allowed). `--auth-token`
  has its own clap parser so the usage error never repeats the value.
- **L4.** `--bind` and `--auth-token` help name `[[serve.credential]]`.
- **L5.** The zero-store-calls window exempts only the live session's own
  `refresh_lease`, and probes `/mcp`, a JSON-RPC body and a live MCP session
  id.
- **I1** doc wording on `match_any`; **I2** two `Authorization` headers are
  401 (chosen over reading the first: a proxy that keeps the last would
  disagree about the caller); **I3/I5** startup warnings
  (`authority::startup_warnings`); **I4** the per-user Unix socket is the
  same-user boundary, documented; **I6** an additive `credential` field (the
  name) on ledger `call` lines for configured credentials only, through a
  hand-written `call_tool` that scopes the name around the macro's dispatch.

Not done: the web portal (`serve-web`) keeps its single-token `bearer_ok`
without the 4 KiB cap; it compares one secret, so the multiplier L2 is about
does not exist there.

## Second review remediation, L1 and L2 (2026-10-09)

The Sonnet security review of the S1-S4 fixes left two Lows, one commit
each.

- **L1, attribution at the mint; `handle` inline again.** S1 ran rmcp's
  whole `handle` on a spawned task so a client disconnect could not leave a
  minted MCP session unattributed. That also stopped the disconnect from
  cancelling a sessionless request rmcp answers directly (a
  per-request-protocol call or `server/discover`,
  `serve_negotiated_request_directly`, whose drop guard cancels the
  request's token, rmcp #857): the call ran on after its client left.
  Now the opener is recorded where the id is minted. `openers.rs` wraps
  rmcp's `LocalSessionManager` in `AttributingSessions`, the manager each
  session's streamable-HTTP service mints through, and its
  `create_session` records the opener in the same poll in which the local
  manager put the id in its map (it does not await after the insert). The
  credential reaches it through a task-local (`as_credential`) set around
  `handle`, because rmcp's `create_session` takes no arguments and is
  awaited inline in the request's task. `transport::serve_attributed` runs
  `handle` inline for every request. So a mint is never unattributed, the
  id-bearing response cannot leave before the binding, and dropping the
  handler cancels what rmcp cancels.
  Rejected: spawning only `initialize` requests (needs our own parse of
  the body that must agree with rmcp's deserializer, or it mints inline
  again); recording after `handle` returns on a spawned bookkeeping task
  (a drop between `create_session` and `initialize_session` still leaves
  the session unowned). `restore_session` keeps the trait default
  (`NotSupported`): the serve configures no session store, and a restore
  would mint a session no credential opened.
  Caveat: lambo's own tools do not watch `RequestContext::ct`, so the
  cancel ends rmcp's per-request loop and signals the token, but a lambo
  tool already running still finishes. That was so before S1 as well.
- **L2, the body is read before the cap reserves, within 30 s.** S4's
  reservation was taken in the guard, before rmcp read the body, so a
  client dribbling the body of a sessionless POST held a slot of the cap
  (and of its share) for as long as it liked; the serve had no request
  timeout at all. Of the two fixes the review offered, both halves were
  taken, because one alone does not close it: `guard_request` now reads the
  whole body itself (`read_body`, to `MAX_HTTP_BODY_BYTES`) **before** the
  cap check, so a body still arriving holds no slot, and does so within
  `REQUEST_BODY_TIMEOUT` (30 s, `408` with `Connection: close` after), so
  it holds its connection for a bounded time. 30 s: MCP bodies are a few
  KiB, the serve had no other request timeout to match, and it is the
  same order as the serve's other operator-facing waits. rmcp reads the
  whole body before acting anyway, so buffering it in the guard costs no
  more; it also holds a chunked body to the 4 MiB the declared-length
  check applies (`413`, as rmcp answered it before). The declared-length
  `413` now comes before the cap's `503`. Releasing the reservation while
  rmcp reads the body was not possible: rmcp reads it inside `handle`,
  past the point the reservation must already be held.

## Third review remediation, L1-L3, N2, N3 (2026-10-09)

A third (Sonnet) security review of the second round found it clean at
Medium and above and left three Lows and three notes. Each fix is its own
commit with its test.

- **L1, only an opener's body is buffered.** The second round's guard read
  the body of every authenticated request before routing, so a request the
  router refuses (an unknown path, a session out of scope or not hosted) or
  one inside an MCP session held up to 4 MiB and its connection for up to
  30 s. Only a request that opens an MCP session (`opens_a_new_session`)
  needs its body read before the cap reserves, so only it is buffered now.
  Every other request keeps the declared-length `413` and streams to rmcp
  after routing, as before the second round.
- **L2, a sessionless call holds no opening.** Every POST without a usable
  session id took a reservation, including a per-request-protocol
  `tools/call` or `server/discover` that rmcp answers directly and that
  mints nothing, for the whole call; parallel stateless calls from one
  credential reached its share and got `503`. The guard now classifies an
  opener's buffered body with `can_mint_a_session`, which is rmcp's own
  reading, `serde_json::from_slice::<ClientJsonRpcMessage>` (what rmcp's
  `expect_json` calls), on the very bytes rmcp is handed: rmcp 3.1.2
  mints for a sessionless POST only when the message is an `initialize`
  request, so only that reserves. A body that does not parse still
  reserves: a disagreement can only over-count, the safe side for S1/S4.
  Rejected: releasing the reservation when rmcp returns without having
  called `create_session` (the call still holds it for its duration).
- **L3, `Connection: close` on the read's `413` and `400`.** Like the `408`,
  they answer with the rest of the body unread.
- **N3, the disconnect cancel behind the guard.** `serve_live`'s body is now
  `transport::serve_owned`, generic over the MCP server, so a test serves a
  probe through `guard_request` and `serve_owned` and asserts the
  disconnect still cancels the call (`AttachedSession` is fixed to
  `LamboServer`, whose tools ignore cancellation).
- **N2, rmcp facts the code rests on.** The attribution's atomicity (no
  await between the local manager's insert and its return, `create_session`
  awaited inline in `handle_post`), "only an `initialize` mints", the
  reading of `Mcp-Session-Id` and the 4 MiB ceiling are rmcp 3.1.2 facts;
  `Cargo.toml` asks for `3.1.2` with a caret, so `Cargo.lock` is what pins
  it today. `rmcp_still_mints_through_create_session_with_the_same_ceiling`
  (in `mcp/serve/tests/transport.rs`) fails loudly, naming what to re-check,
  if an upgrade mints outside `create_session`, keeps sessions elsewhere,
  or moves the default ceiling. An exact `=3.1.2` pin would make the
  upgrade a deliberate act; it is a dependency change and needs the
  owner's approval, so it is not made here.
- **N1, left as is.** For an opener the guard now reads the body before
  rmcp's Host (DNS-rebinding) and `Accept` checks run. The bound is L1's
  (an authenticated caller, one body of at most 4 MiB within 30 s, and
  now only for a session-opening POST); repeating rmcp's Host check in the
  guard would be a second copy that could drift from rmcp's.

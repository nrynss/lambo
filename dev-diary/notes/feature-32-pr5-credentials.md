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

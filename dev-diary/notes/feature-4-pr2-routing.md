# #4 PR 2: allowlist, `/s/{session}` routing, one grant, the Host check (decisions)

Base: main `5773e4a` (#4 PR 1 reader views and #32 PR 5 credentials merged).
Design of record: the approved #4 design (`4-DESIGN.md`, owner-approved
2026-10-09), sections 3, 4.1, 4.4, 4.5, 8, the PR 2 row of section 9,
section 10 and Q10, Q11, Q12 and Q18. Decisions and why; the commits carry
the mechanics.

PR 2 serves an allowlist through one grant. Per-credential scope
(`[[web.credential]]`) and the opt-in listing are PR 3; the page's base path
and picker are PR 4.

## What changed

| piece | where |
|---|---|
| `SessionAuthority::authorize_default` (moved from serve) and `is_pinned` | `src/surface/session.rs` |
| `[web] sessions`, `[web] allowed_hosts`, `AllowedHost` | `src/config/web.rs` |
| `plan_sessions`, `plan_allowed_hosts`, `check_served`, `Args.sessions` / `Args.allowed_hosts`, startup lines | `src/cli/serve_web.rs` |
| `SessionAuthority<AuthToken>`, `HostCheck`, `host_guard`, `gate`, the 4 KiB token refusal | `src/cli/serve_web/auth.rs` |
| `resolve_session`, `SessionCtx` (new) | `src/cli/serve_web/scope.rs` |
| `AppState` without a current session; handlers take `SessionCtx` | `src/cli/serve_web/{state,routes,projections}.rs` |
| repeatable `--session`, `--allowed-host`, planning before the backends | `src/main.rs` |
| the exhibit's `--allowed-host` and self-signed `Host` rewrite | `scripts/aws-infra/launch_exhibit_ec2.py`, its README |
| tests | `src/cli/serve_web/tests/{routing,host}.rs` (new), `auth.rs`, `mod.rs` |

## Decisions

**The session is resolved before routing, by layers over a fallback
service, not by an `any` route (deviation from design 3.3's mechanism, same
semantics).** The design's `any` dispatcher hands the request to an inner
`GET`-only router, which means calling a `Router` as a tower `Service`.
`tower` and `tower-service` are not direct dependencies and `Cargo.toml` is
frozen, and axum's `route_service` refuses a `Router`. `Router::layer` does
wrap a router's fallback, so `router()` serves the `GET`-only routes as the
fallback service of an outer router and puts the resolution in a layer over
it: every request, any method, any path, passes through it before routing.
What the design asks for holds: one handler set for the aliases and the
scoped paths, a refused scope is the uniform 404 for every method, in scope
a non-`GET` is the route's own 405 (Q11) and any other path under a session
is axum's own 404. There is no `any(` in the portal at all; the source pin
(`the_session_is_resolved_by_one_layer_before_routing`) counts one
`.fallback_service(`, three `.layer(`s in their order, no `routing::any`,
no `route_service(` and no `Path<` extractor (which would percent-decode).
`ROUTES` and the read-only sweep are unchanged, because no route was added.

**The order (design 3.3).** `host_guard` (outermost, before routing, every
request) → for `/s/...` in `scope::resolve_session`: bearer
(`SessionAuthority::authenticate`), shape (`parse_addressed` on the raw
segment), scope (`authorize`), allowlist (`is_pinned`), then rewrite to the
unscoped path and route. All of it is in memory; the recording store in
`a_refused_session_is_byte_identical_to_an_unrouted_path_with_no_store_call`
sees zero calls across 7 methods x 8 id kinds x 6 suffixes.

**The unscoped routes keep their bearer check where it was (a `gate` over
the routes).** The first cut checked the bearer before routing for every
path. A parity run against main showed axum orders `content-length` and
`connection` differently for a routed response and adds `Allow` to one on
a non-`GET` route, so the 401 on every alias had changed bytes. The gate
over the routes is where `require_auth` always sat, so the unscoped 401 is
byte for byte main's; only a scoped path is authenticated before routing,
and its 401 equals the unrouted path's 401 for every id. Its own commit
(`fix(serve-web): keep the unscoped routes' 401 byte-identical`), because
the regression was introduced earlier on this branch.

**Aliases are authorized like serve's `/mcp`.** `authorize_default` moved
from `mcp::serve::authority` onto `SessionAuthority` (serve keeps a one-line
delegate), so a strict default is authorized as `/s/{default}` would be and
a loose single default only under a scope over every pinned session. In
PR 2 both grants (`local`, `default`) have that scope; PR 3's narrower
credentials inherit the rule.

**One grant (design 4.1, PR 2).** `portal_authority`: no token is the
implicit `local` grant, a token is the legacy `default` grant, both
`SessionScope::pinned()` over the allowlist's addressable names (no
prefixes). The startup count line names it: `lambo serve-web: credential
'local' reads 2 sessions`.

**Q12: the strict charset only with more than one session.** `[web]
sessions` itself is not held to it (unlike `[serve] sessions`): the rule
depends on the union with `--session`, so `check_served` applies it at
startup, naming the session. A loose single session is served at the
aliases only.

**Planning before the backends.** `plan_sessions` and `plan_allowed_hosts`
run in `main` right after `lambo.toml` is read, before the embedder or
store is built, so a bad set is exit 2 at once; `run` re-checks
(`check_served`, the host parse) for library callers. Missing `--session`
with no `[web] sessions` is still exit 2, now with Lambo's message after
reading `lambo.toml` instead of clap's.

**Host check (design 4.5, Q18), its own `fix` commit.** Under the implicit
grant only: `localhost`, `127.0.0.1`, `::1` (any port, case-insensitive,
brackets ignored) plus `--allowed-host` / `[web] allowed_hosts` (`host` any
port, `host:port` that port, rmcp's matching rule). The `Host` header, else
the request target's authority; missing or malformed is refused. One fixed
403 (text, names neither the Host nor any session), the first logged at
`warn` with the fix, later ones at `debug`. With a token configured any
Host is accepted, as `lambo serve` does. Allowed hosts given beside a token
are noted at startup as unchecked.

**The exhibit (design 8).** `--hostname` mode: Caddy forwards the visitor's
Host, so the systemd unit carries `LAMBO_ALLOWED_HOST=<hostname>` and the
wrapper passes `--allowed-host`, but only when `lambo serve-web --help`
lists the flag: the launcher's default `--lambo-version` is 0.3.0, which
has no Host check and would refuse the unknown argument. `--self-signed`
mode: the public IP is not known when the user data is rendered (the EIP is
allocated after launch), so the Caddyfile sets `header_up Host
{upstream_hostport}` and the loopback rule accepts it. Not verified against
a live Caddy or instance. What an operator of an already-running exhibit
must do is in the CHANGELOG (Breaking) and `scripts/aws-infra/README.md`.

**Single-session behaviour (the owner's byte-identity requirement).**
Checked with both binaries (main `5773e4a` and this branch, one sqlite
session, 7 methods x 15 paths x 2 Hosts): without a token 196 of 210
responses identical (date aside, JSON compared without the two per-request
timing fields), the 14 others the new `/s/` route; with a token 210 of 210
identical (every 401 included) except the new route once authorized. The
deliberate differences: the Host check; one more startup line (the count
line); a token over 4 KiB is refused at startup (it could never be
presented, since the shared scan refuses a longer header); `--session` may
be omitted when `[web] sessions` names one.

**`surface::bearer::bearer_ok` moved into its test module.** The portal was
its last caller; serve resolves through `match_any`. Its test keeps its name.

## Tests

New, `tests/routing.rs`: the served-set union and its exit 2s, Q12, `run`'s
re-check (zero store calls), the 404 matrix against an unrouted path with a
recording store, the scoped 401 equal to the unrouted 401 for every id and
method, the in-scope sweep (405, and the uniform 404 for `/s/x/app.css`,
`/s/x/api/nope` and the like), alias equality (the scoped page adds exactly
`no-store` and `Referrer-Policy`), the query string through the rewrite,
two sessions under 16 concurrent clients (counts, pulse, graph, events,
inspect both ways, session, recall, the mismatch banner and the fail-closed
recall on the other-model session), per-session freshness, LRU reload over
HTTP, an allowlisted empty session, the loose single session, and the source
pin. `tests/host.rs`: the loopback names, nine refused Host shapes on eight
paths and two methods with no store read, allowed hosts with and without a
port, any Host under a token, the check chosen from the credential set, and
the host planning. Unit: `[web]` parse and refusals, `AllowedHost` matching.

Changed, named: `auth::bearer_header_is_parsed_strictly` drives the portal's
authority instead of `bearer_ok`; `auth::the_portal_uses_the_shared_bearer_check`
pins `SessionAuthority<AuthToken>` and `.authenticate(presented)` and bans
local copies of the comparator and the scan; `feeds::durable_change_age_...`
and `views::until_queued` name the session (`AppState` has no current one);
the `Args` literals gain the two fields.

Mutation checks run: authorizing by shape alone (no scope, no allowlist)
fails the 404 matrix, the empty-session test and isolation; inserting the
default session for every scoped request fails five routing tests; disabling
the Host check fails the two Host tests.

## Not done here

PR 3: `[[web.credential]]`, prefix scopes, `inherit_serve_credentials`,
`list_sessions`. PR 4: the page's base path and picker (the page at
`/s/b/` still polls `/api/...`, which is the default session; that is PR 4's
first acceptance item). PR 5: H7 closure and the remaining docs.

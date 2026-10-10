# #4 PR 3: per-credential read scope and the opt-in listing (decisions)

Base: main `4a6d935b` (#4 PR 2 merged as #85). Design of record: the
approved #4 design (`4-DESIGN.md`, owner-approved 2026-10-09), sections
4.1, 4.2, 4.5, 6.2, the PR 3 row of section 9, section 10, and Q4 to Q8,
Q14 and Q18. Decisions and why; the commits carry the mechanics.

PR 3 turns serve-web's one grant into a credential set: `[[web.credential]]`
entries, optionally every `[[serve.credential]]` as a read grant, beside the
legacy token, plus the opt-in `GET /api/sessions`. The page's picker is PR 4.

## What changed

| piece | where |
|---|---|
| the credential grammar, shared by `[serve]` and `[web]` (`CredentialTable`, `CredentialEntry`, `validate_set`, `resolve`, `check_credential_set`) | `src/config/credential.rs` (new) |
| `[serve]` on the shared grammar (messages unchanged) | `src/config/serve.rs`, `src/mcp/serve/authority.rs` |
| `[[web.credential]]` (`WebCredentialConfig`), `list_sessions`, `inherit_serve_credentials`, `validate_with_serve` | `src/config/web.rs` |
| `[embedder] api_key_env` may not name a web `token_env` | `src/config.rs`, `src/config/secret_env.rs` |
| `SessionAuthority::grants` | `src/surface/session.rs` |
| `WebCredential`, `resolve_web_credentials`, `check_web_credentials`, `portal_authority` over the set, `Caller`, `credential_reach`, `unserved_names`, `listable` | `src/cli/serve_web/auth.rs` |
| `plan_credentials`, `Args.credentials`, startup lines and warnings | `src/cli/serve_web.rs`, `src/main.rs` |
| `/api/sessions` (`api_sessions`, `SessionList`), registered only when on | `src/cli/serve_web/{routes,dto,state}.rs` |
| `/s/{id}/api/sessions` refused | `src/cli/serve_web/scope.rs` |
| tests | `src/cli/serve_web/tests/credentials.rs` (new), `src/config/web.rs` |

## Decisions

**One grammar, two tables.** The design says `[[web.credential]]` has "the
same grammar as `[[serve.credential]]` minus the write capabilities". The
checks moved out of `config::serve` into `config::credential`,
parameterized by the table a refusal names, rather than being copied:
reserved names, the inline token, `token_env` (the shared `secret_env`
rule, so `LAMBO_AUTH_TOKEN` is refused), the strict charset for scope
entries, duplicates within an entry and across entries, and the
same-token refusal at resolution. `[serve]`'s messages are byte for byte
what they were (its tests pass unchanged except one added import). The
startup set check (`check_serve_credentials`: reserved names, a name or
token used twice, a token equal to the legacy one) became
`config::check_credential_set`, generic over the secret type; serve keeps
its wrapper and messages. Two refactor commits, no behaviour change.

**`create`, `erase`, `admin` refused by key presence, whatever the value.**
The design: "refused by name at parse time ... an accepted-but-ignored
`erase = true` is a lie". `erase = false` is not a lie, but the key does
not exist in this table, and one rule ("the key is not accepted here") is
simpler to state and test than "accepted when false". The value is parsed
into the discarding `InlineToken` marker, so a string value is never held
or echoed. With `deny_unknown_fields` alone the refusal would have been a
bare "unknown field"; this one says the portal is read-only and where the
keys belong.

**Scan order and the set.** `default` (the legacy token) first, then the
`[[web.credential]]` entries in file order, then the inherited
`[[serve.credential]]` entries. All go into the one `SessionAuthority`
(one construction site, pinned by a source test), so the scan is
`surface::bearer::match_any`, constant-time over all of them; the portal
has no comparator of its own (source scan). The implicit `local` grant
exists only when the set is empty.

**The Host rule with several credentials (design 4.5, Q18).** "Only when no
token" is defined as "only under the implicit grant": the Host check runs
while no credential of any kind (legacy, web, inherited) is configured,
and is off as soon as one is, however many. Why: the check defends the
unauthenticated loopback portal against DNS rebinding; once every request
needs a bearer, a rebound page has nothing to present, whichever credential
it would need. That is PR 2's `HostCheck::for_authority` unchanged
(`requires_bearer`), now with more inputs, and the mutation that keys it on
the legacy token alone fails two tests.

**Scope enforcement is PR 2's order, per grant.** Bearer, shape, the grant's
scope, then the allowlist (`is_pinned`), all in memory before any store
call; every refusal is the uniform 404. The portal's `HostedSessions` holds
the allowlist and no prefixes, so `"*"` is the allowlist and a prefix grant
reaches only allowlisted ids under it (Q8): an id under the prefix that has
data in the store but is not served is the 404 with no load. The aliases
use `authorize_default`, so they serve the default only to a grant that may
read it; others get the uniform 404 on the data routes (the page, assets
and `/healthz` name no session and stay 200).

**The listing (design 6.2, Q7).**

- *Absent means unrouted.* With `list_sessions` off the route is not
  registered, so every method is the uniform 404, byte-identical to any
  unknown path. Registering it always and answering 404 from the handler
  would leave `POST /api/sessions` a 405 with `Allow`, which says the route
  exists. The cost: the read-only sweep (`read_only_router_has_no_mutating_route`)
  now runs with the listing on, so every `ROUTES` entry is registered; a
  separate test pins the off case.
- *What is listed.* Served sessions, in allowlist order, that the grant
  names exactly, or every served session when the grant's scope covers
  every pinned session: `local`, `default`, and `"*"`. `"*"` counts as
  "every served session" because design 4.1 defines it as the allowlist on
  the portal; it is not a prefix expansion. A prefix grant lists nothing
  (it reads the allowlisted ids under it, but the design forbids the
  expansion). A loose single default (one session, a name outside the
  charset) is listed for `local`/`default`/`"*"`, since that is what they
  read.
- *Unscoped only.* `/s/{id}/api/sessions` is the uniform 404 for every id:
  the listing is the caller's, not a session's (design 3.2 lists seven
  data routes under a session).
- *No store call*, `no-store`, names only (`{"sessions": [...]}`).
- Under the implicit grant it names every served session to anyone who
  reaches the port; startup warns (R5). Refusing that combination was the
  rejected alternative of Q7.

**`inherit_serve_credentials` (Q5).** Imports each `[[serve.credential]]`
with its scope and `SessionCapabilities::default()`. Its tokens resolve
under serve's table wording (they are `[serve]` entries) through the
portal's token rule (`AuthToken::new`, which shares
`check_configured_token` with `SecretToken`). A name or `token_env` in both
tables is refused at parse time (`validate_with_serve`); a token shared
across the tables is refused by the set check. Off, serve's variables are
never read, so a portal sharing a `lambo.toml` with an HTTP serve needs
none of its secrets.

**When credentials are read.** `plan_credentials` runs in `main` with the
served-set planning, before any backend or model load (exit 2, naming the
credential and the variable, never a value), as `lambo serve` does since
#32 PR 5. It reads `LAMBO_AUTH_TOKEN` only when a credential is configured
(for the legacy-collision check), so a single-token portal resolves its
token exactly where and how it did. `run` re-checks the whole set
(`check_web_credentials`) for library callers. `authorize_bind_web` counts
any credential, legacy or configured (serve's `any_credential` rule).

**Startup output.** One count line per grant, in scan order (`credential
'viewers' reads 1 session`), so a single token or the implicit grant prints
the one line PR 2 printed. "Authentication is ON" keeps its exact text
when only the legacy token exists, and names the credential count
otherwise. Warnings, one line each, names only: the legacy token beside
configured credentials (it still reads everything), a credential naming
sessions that are not served, a credential reading none, and the listing
under the implicit grant.

**No per-credential fair shares.** #32 PR 5 split serve's rate limit and
MCP-session cap per credential (review M2). The portal has neither: Q13
kept the load and recall semaphores only, process-wide, bounding work
rather than requests. The design asks for nothing per credential, so
nothing was added. Open question below.

**Single-session, single-token behaviour.** Checked with both binaries (main
`4a6d935b` and this branch, `store-sqlite,fixtures`, one sqlite session,
7 methods x 17 paths x 2 Hosts, `/api/sessions` and
`/s/t4-parity/api/sessions` among them): without a token 238 of 238
responses identical, with a token 0 differences with no header, a wrong
token and the right one; stdout and stderr identical. Every existing
portal test passes; the two changed are named below.

## Tests

`tests/credentials.rs`, one per PR 3 acceptance item: resolution refusals
(unset, two credentials and one token, across tables, legacy collision,
reserved names from a library caller, an unpresentable token; no token in
any message or `Debug`); inheritance imports scope and drops capabilities,
and stays read-only over the wire; `local` is gone with a web credential
(401 without a bearer, on loopback); the Host rule over the five credential
mixes, and a foreign Host served with a token; each credential reaches only
its sessions, every other id (other served, unserved, malformed,
percent-encoded, oversized) byte-identical to the unrouted 404 on every
method with zero store calls; the prefix grant and an unserved id under it
(no load); the aliases follow the scope; the listing absent by default
(every method, both with credentials and implicit, no store call); the
listing's content per credential (exact, none for the prefix, all for
`"*"`/`default`/`local`), 401 without a bearer, 404 under a session, 405 on
mutating methods; a sweep over 5 credentials x ~60 paths finding no
response naming an out-of-scope session by name or content; the count
lines; the warnings; the shared-scan source pin. `config::web::tests`: the
grammar refusals worded for `[web]`, the write keys, the inline token, the
cross-table collisions, `api_key_env`.

Changed, named: `routes::read_only_router_has_no_mutating_route` runs with
`list_sessions` on; `routing::in_scope_mutating_methods_are_405_...`
excludes `/api/sessions` from the scoped routes and adds it to the
uniform-404 rests; `routing.rs` helpers became `pub(super)`;
`config::serve::tests` imports `MAX_ADDRESSED_LEN` directly.

Mutations, each caught (see the test commit): scope ignored, aliases for
any grant, list everything, list a prefix expansion, Host check keyed on
the legacy token, configured credentials ignored without a legacy token,
allowlist not checked, inherited capabilities kept, listing always
registered.

## Open questions

- **Recall fairness.** `recall_concurrency` is process-wide, so one
  credential's script can hold every recall permit and make the others wait
  into the 503. Per-credential permit shares (serve's `floor(n / k)` rule)
  would fix it; the design keeps semaphores only (Q13), so it is not done.
- **`inherit_serve_credentials` with no `[[serve.credential]]`** is accepted
  silently (it imports nothing). A startup note would be cheap.

## Not done here

PR 4: the page's picker (it can use `/api/sessions`). PR 5: H7 closure and
the remaining docs.

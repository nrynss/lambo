# #4 multi-session `serve-web`: the consolidated record

`lambo serve-web` now shows several sessions in one read-only window. It is
still a lease-free reader of the store, and a window that serves one session
is reached and answers as it always did. This note is the single record of
#4. It covers the design that was approved, every decision made while
building it in PRs 1 to 4, and each place where the shipped code differs from
the design, with the reason. The per-PR notes keep the full arguments, test
lists and mutation checks. This note says what is true on main at
`4e02592e` (PR 4 plus #32 PR 9) and points to them.

- Issue: [#4](https://github.com/nrynss/lambo/issues/4). It began as
  hardening task H7 ([hardening-tasks.md](hardening-tasks.md), now DONE).
  The owner's six decisions of 2026-10-06 are
  [issue comment 6014402698](https://github.com/nrynss/lambo/issues/4#issuecomment-6014402698).
  The design of record is
  [issue comment 6084141497](https://github.com/nrynss/lambo/issues/4#issuecomment-6084141497)
  (`4-DESIGN.md`), approved by the owner on 2026-10-09 with the answers in
  its status banner: Q1 a separate reader process, Q2 reads from the store,
  Q4 a separate `[[web.credential]]` table, Q6 decision 3 amended to
  per-credential read scope, Q7 listing opt-in, Q9 an erased or never-written
  allowlisted session is an empty page with a "no memory in this session
  yet" state, and every other question on its recommendation.
- Operator docs:
  - [`docs/reference/cli.mdx` § Watch a session in a browser](../../docs/reference/cli.mdx),
    which opens with the overview set against § Several sessions in one
    serve (#32);
  - [`docs/reference/config.mdx` § Read-only window views and § Portal credentials](../../docs/reference/config.mdx);
  - `lambo.example.toml`, the `[web]` block (a tested contract:
    `config::web::tests::the_example_files_web_block_parses_when_uncommented`);
  - the exhibit launcher, [`scripts/aws-infra/README.md`](../../scripts/aws-infra/README.md)
    (the `Host` check).
- The #32 side: [feature-32-multi-session.md](feature-32-multi-session.md),
  § "The inherited-credential rules", lists the serve rules the portal's
  `inherit_serve_credentials` relies on.

## The PRs

| PR | what | merged | merge | note |
|---|---|---|---|---|
| 1 | per-session reader views: one load per poll, TTL, single-flight, load and recall bounds (still one session) | [#78](https://github.com/nrynss/lambo/pull/78) | `d1aeb60b` | [pr1](feature-4-pr1-reader-views.md) |
| 2 | the allowlist, `/s/{session}` routing, `SessionAuthority` with one grant, the `Host` check | [#85](https://github.com/nrynss/lambo/pull/85) | `4a6d935b` | [pr2](feature-4-pr2-routing.md) |
| 3 | `[[web.credential]]`, `inherit_serve_credentials`, the opt-in listing | [#90](https://github.com/nrynss/lambo/pull/90) | `9c63b060` | [pr3](feature-4-pr3-credentials.md) |
| 4 | the page's session picker, `switchable`, the empty-session state | [#94](https://github.com/nrynss/lambo/pull/94) | `61f3ada0` | [pr4](feature-4-pr4-frontend.md) |
| 5 | this note, the reference docs, the example config, H7 closed, the CHANGELOG | branch `docs/4-pr5-docs` | | |

They landed in order. PR 2 waited for #32 PR 5 (#80), which brought
`SessionAuthority`, `BearerSecret` and `HostedSessions::is_pinned`. Each PR
had an Opus review and an Opus remediation; the findings and their fixes are
in each PR note's remediation section.

## What the design set out (in brief)

- **A separate reader process (Q1, Q2).** The portal reads the store, never
  `lambo serve`'s session holders. A holder's recall notes accesses (#30),
  which would let a human browsing change canonization inputs; a holder
  read takes the graph lock against agent writes; SQLite's single
  connection would be shared; and the exhibit runs with no writer at all.
  Mounting the portal in serve is a follow-up with three prerequisites: a
  side-effect-free `Memory` read API, a lock-hold budget for whole-graph
  projections, and a `SessionNeed::Read` that serve refuses on `/mcp`.
- **An explicit allowlist (decision 1).** The ordered union of the
  repeatable `--session` and `[web] sessions`; the first is the default.
  No store discovery and no `GraphStore` method.
- **`/s/{session}/...` per request (decision 2).** No current session
  anywhere. The unscoped routes alias the default (Q10).
- **One authority model with serve (decision 3, amended, Q4 to Q6).**
  `SessionAuthority<AuthToken>` from `surface::session`, the uniform 404,
  and a read-only `[[web.credential]]` table. The legacy token and the
  implicit loopback grant keep "one bearer for every configured session".
- **Bounded reads (decision 6, Q3, Q13, Q15, Q17).** A per-session view
  with a short TTL, single-flight loads, an LRU of views, load and recall
  semaphores, lazy loading, count-bounded defaults.
- **Two pre-existing defects, fixed in their own commits:** the double
  session load per `/api/pulse` (PR 1), and no `Host` check on the
  unauthenticated portal, a DNS-rebinding hole (PR 2).

## Decisions as shipped, by area

### Reader views (PR 1)

- **One load per session per TTL.** `ViewCache` in
  `src/cli/serve_web/views.rs`. Every request and tab shares one
  `SessionView` until it is older than `view_ttl_ms` (default 1500, the
  page's poll interval). Measured with a counting store: `/api/pulse` went
  from 2 loads to 1, 8 concurrent cold pulses from 16 to 1, a page open from
  11 to 1.
- **The feed comes from the loaded graph.** The design asked for a
  `load_reader_graph` variant returning the snapshot's events. Not needed:
  `Graph::from_snapshot` keeps every canonization event, so one load builds
  graph, index, counts and feed.
- **Single-flight without `futures::Shared`.** `Cargo.toml` was frozen.
  Each slot has a tokio `Mutex<()>` gate and a `completed` counter; a
  request that queued while a load finished takes that load's outcome, the
  view or the error. A dead store is hit once per burst, and a failed load
  is not cached.
- **Eviction drops the view only.** The slot keeps the freshness tracker and
  #14's per-session query-embedding cache, so a reloaded session does not
  report "just changed" or re-embed. The bound is a count, not bytes; views
  held by requests in flight are outside it (PR 1 note, "Eviction").
- **`view_ttl_ms` is capped at 60000.** Not in the design: a view older than
  a minute is a frozen page. `0` reloads on every request, still
  single-flight. `load_concurrency` is forced to 1 on SQLite.
- **Recall order: validate, view, permit, run.** A bad query is a 400 with
  no store call. The recall permit (2 s wait, then `503`, `Retry-After: 1`,
  `no-store`) is taken after the view, so a slow load cannot hold a recall
  permit (PR 1 review L5). `run_detailed_on` gives byte-identical `context`,
  `hits` and annotations to `lambo recall`.
- **Startup is lazy (Q15).** Schema preflight only; the embedding-mismatch
  warning moves to each session's first load, naming it.

### Selection, routing and the uniform 404 (PR 2)

- **Resolution by layers over a fallback service, not an `any` route.**
  Deviation from design 3.3's mechanism, same semantics. Calling an inner
  `Router` as a service needs `tower`, which is not a direct dependency, and
  axum's `route_service` refuses a `Router`. So `router()` serves the
  `GET`-only routes as an outer router's fallback, with `host_guard` and
  `resolve_session` as layers over it: every request, any method and path,
  passes them before routing. There is no `any(` in the portal; a source pin
  counts the layers.
- **The order (design 3.3).** Host check, then for `/s/...`: bearer, shape
  (`parse_addressed` on the raw, never percent-decoded segment), the grant's
  scope, the allowlist (`is_pinned`), then rewrite to the unscoped path and
  route. All in memory: a recording store sees zero calls across 7 methods,
  8 id kinds and 6 suffixes. Out of scope is the uniform 404 for every
  method; in scope a non-`GET` is the route's own 405 (Q11).
- **The unscoped 401 stayed where it was.** The first cut authenticated
  every path before routing; a parity run against main showed the alias
  401s had changed bytes (header order, `Allow`). The `gate` over the routes
  keeps main's 401 byte for byte; only scoped paths authenticate first, and
  their 401 equals the unrouted path's for every id. Its own `fix` commit.
- **Q12.** The strict addressed charset applies only when more than one
  session is served, checked at startup on the union; a loose single
  session is served at the aliases only.
- **Relative URLs and the 308 (PR 2 review M1).** The page at `/s/{b}/`
  first showed the default session, because the script fetched absolute
  `/api/...`. The script now fetches relative `api/...`, and `GET` or `HEAD`
  of `/s/{b}` is a `308` to `/s/{b}/` (query kept, `no-store`,
  `Referrer-Policy: same-origin`), issued only after bearer, scope and
  allowlist, so it says no more than the page would. Design 3.2 said "no
  redirects": that was about a pre-authorization oracle, which this is not.
  This moved the base path from PR 4 into PR 2.
- **The scoped page** adds exactly `no-store` and `Referrer-Policy:
  same-origin` to the alias's bytes.

### The Host check (PR 2, generalized in PR 3)

- **Only under the implicit `local` grant (design 4.5, Q18).** While no
  credential of any kind is configured, the portal answers only `Host`
  `localhost`, `127.0.0.1` or `[::1]` (any port, case-insensitive) plus
  `--allowed-host` / `[web] allowed_hosts` (a bare host matches any port,
  `host:port` that port). Anything else, a missing or malformed `Host`
  (PR 2 review L1), and two `Host` headers (L2) get one fixed `403` that
  names neither the Host nor any session. The first refusal is logged at
  `warn`, later ones at `debug`.
- **PR 3 defined "no token" as "no credential".** With a legacy token, a
  `[[web.credential]]` or an inherited `[[serve.credential]]`, any `Host`
  is accepted: a rebound page has no token to present, whichever credential
  it would need. This is `HostCheck::for_authority` keyed on
  `requires_bearer`, and matches serve (#32 PR 5 M1).
- **Breaking for proxies.** A proxy that forwards a foreign `Host` to an
  unauthenticated window now gets `403`. The exhibit launcher passes
  `--allowed-host <hostname>` in `--hostname` mode (only when the binary's
  `--help` lists the flag, since 0.3.0 does not), and in `--self-signed`
  mode Caddy rewrites `Host` to the upstream address. Not verified against a
  live Caddy or instance.

### Credentials (PR 2 one grant, PR 3 the set)

- **One grammar, two tables.** `[[web.credential]]` shares
  `config::credential` with `[[serve.credential]]`: reserved names, the
  inline-token refusal, `token_env` (never `LAMBO_AUTH_TOKEN`), the scope
  charset, duplicates, and the startup set check. Serve's messages were kept
  byte for byte.
- **`create`, `erase`, `admin` refused by key presence**, whatever the value,
  with a message saying the portal is read-only. One rule is simpler than
  "accepted when false".
- **The shared bearer validator (PR 2 review M2).** `AuthToken` lacked
  serve's refusals of whitespace and non-printable bytes, so such a token
  answered every request `401` with no hint. One validator,
  `surface::bearer::check_configured_token`, now backs `SecretToken::new`
  and `AuthToken::new`; a token over 4 KiB is refused at startup too, since
  the shared scan refuses a longer presented credential before comparing.
- **The two-Authorization 401 (PR 2 review L3).** The first header used to
  decide. Serve's extraction moved to
  `surface::bearer::presented_authorization`, called by both surfaces, so
  two headers are a `401` in either order.
- **One authority, one scan.** The legacy `default` first, then
  `[[web.credential]]` in file order, then inherited entries, all in one
  `SessionAuthority`, scanned by `surface::bearer::match_any` in constant
  time. The portal has no comparator of its own (source scan). `local`
  exists only when the set is empty.
- **An inherited `"*"` is held to serve's set (PR 3 review M1).** The first
  cut judged an inherited `"*"` against the portal's allowlist, so an agent
  token read (and listed) sessions it could never use through MCP.
  Decision: expand, not refuse. `SessionScope::star_within` pins it to
  `ServeConfig::hosted_sessions()` (the `[serve] sessions` plus every
  `[[serve.credential]]` prefix), still intersected with the allowlist. On-
  demand attach (#32 PR 6) changed when serve holds a session, not which
  sessions `"*"` may address, so the static set is right. A session pinned
  only by `lambo serve --session` is not in the file, so the import narrows
  and never widens. A pinned `"*"` never reaches a loose single session and
  lists only `[serve] sessions` names. A web `"*"` still means the
  allowlist.
- **Prefix grants (Q8).** The portal's `HostedSessions` holds the allowlist
  and no prefixes, so a `session_prefix` reaches only served ids under it.
  An id under the prefix that is in the store but not served is the 404,
  with no load.
- **Credentials are read before any backend**, in `main`, with the served
  set. A single-token portal resolves its token exactly as before.
- **No per-credential recall shares (PR 3 review L4).** `recall_concurrency`
  is process-wide (Q13), so one credential's script can hold every recall
  permit. Left as a follow-up; it needs its own design. Serve's share,
  `floor(n / k)` at least 1, lets the shares add up to more than the pool
  when credentials outnumber permits, so the portal would need its own
  oversubscription rule.

### The listing (PR 3)

- **The route is unregistered when off.** Registering it always and
  answering 404 from the handler would leave `POST /api/sessions` a `405`
  with `Allow`, which says the route exists. Off, every method is the
  uniform 404, byte-identical to any unknown path. The read-only sweep now
  runs with the listing on, and a separate test pins the off case.
- **What it lists.** The served sessions the grant names exactly, in
  allowlist order, or every served session for `local`, `default` and a web
  `"*"`. Never a prefix expansion or another grant's names. No store call,
  `no-store`.
- **Unscoped only.** `/s/{id}/api/sessions` is the uniform 404, refused by
  the path check and again by the listing itself through the
  `ScopedRequest` marker (PR 4 review I3).
- **Under the implicit grant** it names every served session to anyone who
  reaches the port; startup warns (design R5). Refusing that combination was
  Q7's rejected alternative.

### The page (PR 4)

- **`switchable` per caller, and the accepted one bit.** The design said
  single-session mode shows no picker but gave the page no signal: the
  listing is opt-in and unrouted when off. `/api/session` gains
  `switchable`, true when the presenting grant reads more than one served
  session, counted as the startup count lines count it. Per caller, not the
  allowlist size, so a credential scoped to one session learns nothing about
  others. A prefix grant over two served ids gets `true`: it learns one
  bit, "you can read another session", never a name. The review (I1) raised
  it; the owner kept it. The field is absent when false, so a one-session
  window's JSON and headers are unchanged (the HTML gained the picker's
  hidden markup, PR 4 review I4).
- **Listing first, history as the fallback.** With `switchable`, the page
  asks for the listing; two or more names make a `<select>` ending in
  "Other session…" (PR 4 review L2, so a prefix-reached session can still be
  opened). Otherwise a text field suggests this browser profile's history
  (`localStorage` `lambo-sessions`, at most 20 names). With a listing the
  browser keeps no history.
- **Every URL stays relative.** `rootPath()` is `""` at `/` and `"../../"`
  at `/s/{id}/`, the only two places the page lives.
- **The HEAD probe.** Switching sends `HEAD` to the target page before
  navigating. The page route reads no store; in scope it is 200, out of
  scope the uniform 404. Only that 404 drops the name and says it cannot be
  read; a `401`, `403`, `5xx` or no answer keeps it with a retryable message
  (PR 4 review L1), since a transient proxy or store failure must not erase
  history. Navigating blindly would land on an empty 404 with no way back.
- **The client name rule is the server's.** The regex is
  `parse_addressed`'s charset, length and leading-dot rule; the test parses
  it out of `app.js` and checks 17 names. Names go into the path unencoded
  (`encodeURIComponent` would turn `:` into `%3A`, which the server
  refuses).
- **No markup sinks.** Names reach the DOM through `textContent` and
  `<option>.value`; a source test bans `innerHTML` and its relatives.
- **Q9.** An empty session says "No memory in this session yet", following
  the poll's count, so a first write or an erase shows within one poll.
- **Q14, the browser bearer (design 4.4).** Unchanged: the page sends no
  `Authorization` header, so with any credential a plain browser cannot open
  it. It works behind a proxy that adds the header per user, and from
  scripts; behind such a proxy, the picker follows the injected token's
  scope.
- **CSP deferred to #92.** A `Content-Security-Policy` (and `nosniff`) would
  change every single-session response's headers, against the
  byte-identity rule the earlier PRs held, so it needs its own decision.
  The owner deferred it to [#92](https://github.com/nrynss/lambo/issues/92).

## Single-session compatibility, as checked

Each PR compared both binaries on one SQLite session:

- PR 2: 7 methods x 15 paths x 2 Hosts. Without a token, 196 of 210
  responses identical, the 14 others the new `/s/` route; with a token, 210
  of 210 identical except the new route once authorized.
- PR 3: 7 x 17 x 2. Without a token, 238 of 238 identical; with one, no
  difference.
- PR 4: the one-session page made the same six requests as before, and no
  listing request.

The deliberate differences for a one-session window: the page can be up to
`view_ttl_ms` older than the store; a store error other than an
unprovisioned schema shows at the first request, not at startup; the
mismatch warning moves to the first load; the `Host` check; one more startup
line (the count line); a token over 4 KiB, or with whitespace or
non-printable bytes, refuses the start; two `Authorization` headers are a
`401`; `--session` may be omitted when `[web] sessions` names one.

## Deviations from the design, in one table

| design | shipped | why | PR |
|---|---|---|---|
| 5.1: a `load_reader_graph` variant returning the snapshot's events | the feed is built from the loaded graph | `Graph::from_snapshot` keeps every event; one load suffices | 1 |
| 5.1: `futures::Shared` single-flight | a gate mutex and a `completed` counter | no new crate | 1 |
| `view_ttl_ms` unbounded | at most 60000 | a minute-old view is a frozen page | 1 |
| 5.3: recall permit before the load | after the view | a slow load held a recall permit for its whole wait (review L5) | 1 |
| 3.3: one `any` dispatcher handing off to an inner router | layers over a fallback service | no `tower` dependency; `route_service` refuses a `Router` | 2 |
| 3.2: "no redirects" | `GET`/`HEAD` `/s/{b}` is a `308` after authorization | the page's relative URLs need the slash; post-authorization it is no oracle (review M1) | 2 |
| 4.5: Host check "only under the implicit loopback grant" | the same rule, made precise: no credential of any kind | a rebound page can present no token, whichever | 2, 3 |
| 4.1: inherited scope "the same" | an inherited `"*"` is serve's hosted set, not the allowlist | the import must never widen (review M1) | 3 |
| 6.2: listing refusal is the uniform 404 | the route is not registered when off | a handler 404 leaves `POST` a 405 that names the route | 3 |
| 6.2: single-session mode shows no picker (no signal named) | `switchable` per caller on `/api/session` | the listing is opt-in, so the page needed a signal; per caller says nothing beyond its scope | 4 |
| 6.2: a name that 404s is dropped | only the probe's 404 drops it; other failures keep it | a transient failure must not erase history (review L1) | 4 |
| 8: PR 4 owns the base path | PR 2 shipped it | the scoped page showed the default session without it | 2 |

## Not done, and where it goes

- **CSP and `nosniff` for the page:** [#92](https://github.com/nrynss/lambo/issues/92).
- **Per-credential recall fairness** (PR 3 review L4): not filed. Needs a
  design: Q13 settled process-wide semaphores, and serve's share,
  `floor(n / k)` at least 1, adds up to more than the pool when credentials
  outnumber permits, so the portal would need its own oversubscription rule.
- **The holder-backed portal mounted in `lambo serve`** (design 2.4): not
  filed. Its three prerequisites are above.
- **A request-rate limit on the portal** (Q13's alternative), a byte budget
  for views (Q17's), and a durable "session version" probe to skip
  unchanged reloads (design R2): follow-ups if measured.
- **A login flow for browsers** (Q14's alternative): out of scope.
- **Unverified:** load latency on Postgres and Cockroach (only SQLite's
  282 ms at 3,600 concepts, from #8); the exhibit's Caddy `Host` forwarding
  on a live instance.

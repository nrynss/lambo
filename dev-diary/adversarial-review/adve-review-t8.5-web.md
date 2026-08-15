# Adversarial Review: T8.5 — Demo app (hosted client)

```text
╔══════════════════════════════════════════════════════════════════════╗
║  STATUS: FINDINGS — T8.5 "Done when" MET; web surface run live        ║
║  Verdict: FINDINGS  (0 P1 / 1 P2 / 3 P3)                              ║
║  Scope:   HEAD of phase/p8-surface @ 186fb5b (clean tree)             ║
║  Gates:   fmt [x] clippy x3 [x] test 718 [x] test-sqlite 771 [x]     ║
║           no-default x2 --no-run [x] check --no-default-features [x]  ║
║  Opened:  2026-08-15 · Reviewed: 2026-08-15                            ║
╚══════════════════════════════════════════════════════════════════════╝
```

**Task:** T8.5 — Demo app (hosted client), PHASE-8-surface.md §T8.5.
**Tree:** `phase/p8-surface` @ `186fb5b`, clean working tree (confirmed `git status`).
**Method:** clause-by-clause read of §T8.5 + the "T8.5 — Demo app / read-only session window"
Handoff Log entry against `src/cli/serve_web.rs`, `web/{index.html,app.css,app.js}` and the
`serve-web` arm of `src/main.rs`; full binding gate block run independently; the **live
server** exercised end-to-end: `cargo build --features store-sqlite,fixtures`, provisioned
a fresh SQLite file, ran the real `rest-api` demo scenario (T8.4 writer) against a fixed
session, served `lambo serve-web` as the reader on the same file, and probed every endpoint
with `curl` **during and after** the scenario plus a real browser render. Findings only —
no `src/`, `web/`, `Cargo.*` or `demo/` file was touched; the only artifact created is this
report. All servers were killed and the temp store removed on completion.

The two live-cluster legs (against the Cockroach demo cluster) were **not** run: no
`LAMBO_COCKROACH_DSN` is available in this environment. The live-update behaviour was
verified over the fixture/sqlite path (which IS the demo's actual dependency — spec §2.2,
demo/README: sqlite is the shared-store minute). This shadows T8.4's own infra-blocked live
cluster leg and is recorded as such, not claimed.

---

## Gates (full binding block — run independently, all green)

```text
cargo fmt --all -- --check                                    CLEAN
cargo clippy --all-targets -- -D warnings                    CLEAN
cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings  CLEAN
cargo clippy --all-targets --features store-sqlite -- -D warnings                          CLEAN
cargo test                                                   718 pass, 0 failed, 3 ignored
cargo test --features store-sqlite                           771 pass, 0 failed, 3 ignored
cargo test --no-default-features --features store-sqlite --no-run    BUILDS
cargo test --no-default-features --features store-cockroach --no-run BUILDS
cargo check --no-default-features                             CLEAN
```

Matches the claimed numbers (718 / 771 ≥ handoff's 636+ / 680+ lib; no regressions). All
three read-only tests named in the handoff are present in `serve_web.rs::tests`.
No clippy/fmt failures on any feature row.

---

## Live verification (what I ran and what the endpoints returned)

Built `lambo` with `--features store-sqlite,fixtures`; wrote `/tmp/t85test/lambo.toml`
(`[store] kind="sqlite" path="./demo.db"`, `[embedder] kind="fixture" dim=1024`); ran
`lambo provision` (schema bootstrap, exit 0). Then **two live processes on one sqlite file**:

1. `lambo serve-web --session demo-web-review --port 7710` (loopback, default) — reader.
2. `lambo demo --scenario rest-api --session demo-web-review` (T8.4 writer) — took the
   single-writer lease, drove the real script, ran the real canonization daemon, released.

**Empty-session probes** (serve-web up, writer not yet started): `/api/session` →
`{store:"sqlite", embedder:"fixture", vector_search:false, mode:"reader", read_only:true,
store_is_process_local:false, exposed_beyond_loopback:false}`; `/api/stats` → all-zero
counts, `flush_lag_ms:null`, `log_depth:null`; `/api/events?since=0` → `{total:0,events:[]}`
; `/api/recall?q=user` → empty context; `/healthz` → `ok`. No DSN/path/embedder-url ever
leaked into `/api/session` (kind-only), matching `session_info_never_leaks_the_dsn_path_or_embedder_url`.

**During the scenario** (rapid `/api/pulse?since=0` sampling): counts moved
`nodes 11→39, edges 23→93, concepts 8→27, canonical 0→1, events 0→5`, and the event tail
filled from empty to 5 real transitions — i.e. the feed and stats **update live** and reflect
the writer's durable snapshot. The demo settles in ~1s so the page's 1.5s poll lands on the
fixed point; the update-on-poll mechanism is proven by the mid-run samples and by the
incremental cursor below.

**Post-scenario probes:**
- `/api/recall?q=update user schema` returned the T5.3 context block **verbatim**, canonical
  marker and ⚑ intact: `user schema [Entity, canonical] (score 1.92, blast radius 9)\n⚑
  Load-bearing pillar — 9 nodes depend on this. Modify with caution.\n…` — identical to the
  `lambo recall` reader output (reuses `cli::recall::run`).
- `/api/events?since=0` → 5 transitions incl. `user schema None→Candidate→Venerable→Canonical
  (blast radius 9)`, plus `add oauth_id…` and `wire login endpoint` — the real record order,
  matching the demo's OUTCOME block.
- **Cursor works**: `?since=5` → `{total:5,since:5,events:[]}` (exhausted); `?since=3` → tail
  `seq 3,4`. The client's `seen` cursor is a first-class, correct incremental poll.
- `/api/stats` final → `39/93/27/1/5`, `durable_change_age_ms` climbing (reset to ~0 on each
  durable change).

**Read-only (live 405 sweep):** POST/PUT/PATCH/DELETE on all 9 routes (`/`, `/app.css`,
`/app.js`, `/healthz`, `/api/{session,recall,events,stats,pulse}`) → **405 every time**,
including a POST body to `/api/recall`. Confirmed no mutating route is reachable; the recall
form is a plain GET.

**Browser render** (headless Chromium at `http://127.0.0.1:7710/`): title
`lambo — demo-web-review`; session view chips (`store: sqlite`, `embedder: fixture / 1024d`,
`keyword recall`); stat tiles `nodes 39 / edges 93 / concepts 27 / canonical 1 / transitions 5
/ flush lag n/a / log depth n/a`; canonization feed rendered the 5 transitions in
reverse-chronological order with the `blast radius 9` note; `live · polling every 1.5s`; no
banners (loopback). Clicking **Recall** filled `#recall-out` with the full verbatim context
block (`308 chars, verbatim`). All four required pieces render with real session data.

**Loopback vs non-loopback auth:** a fresh `serve-web --bind 0.0.0.0` **started and served
unauthenticated** (HTTP 200, no token) with only a stderr warning + page banner — it does
**not** fail closed off-loopback. This is the P2 below.

Every value above came from the live binary and store, not from the source. The one leg not
run live is the Cockroach cluster demo (no DSN); this is **infra-blocked like T8.4**, and the
fixture/sqlite path covers the done-when.

---

## Findings

### T85-1 — P2 — `serve-web` stays unauthenticated off-loopback, and its "T8.7 pending" banner is now false

**file:** `src/cli/serve_web.rs:12-26` (module doc + `run()` warning), `web/app.js:94-99`
(off-loopback banner), stderr banner at `serve_web.rs:617-624`.

**description:** The brief's premise — "the HTTP surface was UNAUTHENTICATED until T8.7" —
has now flipped: **T8.7 is in** (`mcp/serve.rs::authorize_bind` fails closed off-loopback
with bearer auth; `serve` refuses a non-loopback, no-token HTTP bind). But `serve-web` never
connected to that auth: its `Args` are `session/port/bind` only (no token), there is no
`authorize_bind` equivalent, and a `--bind 0.0.0.0` starts and serves **the whole configured
session unauthenticated** — only a warning is printed and a banner raised. Worse, the
banner/stderr text still says **"UNAUTHENTICATED (T8.7 pending)"**, which is stale: T8.7 has
landed fail-closed, and **no task owns serve-web auth** (T8.5 owns `web/`+`serve_web.rs`;
T8.7 owns `mcp/server.rs`+`mcp/serve.rs`). So the promise "will be secured by T8.7" that the
banner implies will never be delivered by any in-flight task — the surface is read-only
(verified, so risk is full-session **read** disclosure, not mutation) but it is the one
HTTP surface outside the phase's own fail-closed-off-loopback posture, with messaging that
misstates why.

**evidence (live):** `serve-web --bind 0.0.0.0 --port 7711` on a session holding the entire
demo graph returned `200` for `/api/session` with no `Authorization` header
(`exposed_beyond_loopback:true`), while stderr printed `⚑ … UNAUTHENTICATED (T8.7 pending)`.
Contrast `serve`'s post-T8.7 `authorize_bind` (serve.rs:319-347), which is a hard startup
refusal.

**reproduction:** `LAMBO_CONFIG=<sqlite config> lambo serve-web --session S --bind 0.0.0.0`
then `curl http://<addr>:<port>/api/session` with no token → `200` + full session kind/counts.

**inheritor / fix:** this is an ownership gap forced by T8.7 landing — neither T8.5 nor T8.7
owns it. Name the **phase orchestrator / P9 deployment owner**. Minimal in-scope fix:
reword the module doc, stderr warning and `web/app.js` banner to stop claiming "T8.7
pending" and to state plainly that `serve-web` remains intentionally unauthenticated and
**read-only** (so exposure is read-leak only) and must sit behind a private network /
authenticating proxy (already the deployment stance). The fuller option — fail-closed bearer
auth off-loopback mirroring `authorize_bind` — is real new scope for that owner. Because the
surface is read-only and the demo runs on unauthenticated loopback by design, this does not
block the done-when (no P1), but leaving the stale "T8.7 pending" message is a false promise.

### T85-2 — P3 — Done-when names `lambo serve --transport http`; the page is `serve-web`

**file:** `dev-diary/PHASE-8-surface.md:552` (T8.5 done-when).

**description:** The done-when literally reads "a browser against `lambo serve --transport
http` shows a live recall and the event feed updating…", but the deliverable is served by the
separate **`lambo serve-web`** command (port 7710), a reader beside `lambo serve` (the MCP
writer). `serve --transport http` is the MCP surface and serves no page. Verified behavior is
fine — this is a doc-reference mismatch only, worth correcting so an operator/reviewer knows
to run `serve-web`, and worth stating that `serve-web` is a reader that must run *beside* (not
instead of) a writer that produces the data.

**evidence:** `src/main.rs` `ServeWeb` arm dispatches `cli::serve_web::run` under
`run_async("serve-web", …)`; no web page exists on the `serve` MCP HTTP transport. Live: the
page was served on 7710 by `serve-web` while `demo` (writer) filled the same store.

**inheritor / fix:** doc-edit by the phase owner; no code change.

### T85-3 — P3 — `flush_lag` / `log_depth` are permanently `n/a` (writer-only), by design

**file:** `src/cli/serve_web.rs:270-271, 364-366`; rendered at `web/app.js:126-132`.

**description:** The spec's stats list "flush lag / log depth", and the page renders both
tiles — but hardcoded to `n/a` with an explanatory tooltip/note, because a lease-free reader
cannot observe the writer process's flush task, and printing `0` would be a lie shaped like a
measurement (same call `cli::stats` makes). This is the honest choice and is clearly disclosed
on-page; recorded so a judge reading "stats (flush lag / log depth)" literally sees `n/a` and
knows it is intentional. Real numbers would require a writer-side endpoint (which, per T8.5's
own note, needs auth — see T85-1's owner).

**evidence:** live `/api/stats` → `flush_lag_ms:null, log_depth:null`; page tiles
`flush lag n/a`, `log depth n/a` with `writer_only` tooltip. Consistent with `lambo stats`.

**inheritor / fix:** informational — recorded against whoever owns a writer-side stats
endpoint (bundle with T85-1 / P9).

### T85-4 — P3 — Recall requires clicking **Recall**; synthetic Return may not submit

**file:** `web/index.html:36-39`, `web/app.js:201-221`.

**description:** The recall control is a spec-correct real `<form>` (hidden input + `<button
type=submit>`), and implicit submission works for a human pressing Enter. A *synthetic*
Return from browser automation does not trigger it (handoff's own caveat). I confirmed the
**button click** path works end-to-end live (rendered the full verbatim context block), so a
human demo is unaffected; recorded so the video rehearsal does one manual keypress rather than
relying on automation.

**evidence:** live browser session — `tab.click('#recall-go')` filled `#recall-out` with the
full `308 chars, verbatim` context block.

**inheritor / fix:** none required for the demo; informational for the T8.4 video crew.

---

## Verdict

**FINDINGS — 0 P1 / 1 P2 / 3 P3.** The T8.5 done-when is **met and verified live**: a browser
against the page on loopback shows the session view, the T5.3 recall context block verbatim,
the T6.4 canonization feed and stats (flush lag/log depth honestly `n/a`), and they **update
live** during the real `rest-api` demo scenario (counts 0→39/93/27/1/5, feed 0→5 transitions;
incremental cursor verified). The surface is genuinely read-only: every route is `GET`, a live
POST/PUT/PATCH/DELETE sweep returned 405 on all nine routes, and three source+router tests
pin that. Loopback default is unauthenticated, so a judge's browser works out of the box. The
one open **P2** is the honesty interaction with T8.7: serve-web remains unauthenticated
off-loopback and its banner still claims "T8.7 pending" when T8.7 has landed fail-closed and
no task owns serve-web auth — a stale promise on a full-session read surface. Since the
surface is read-only and the demo is on unauthenticated loopback, this is not a P1 blocker,
but it should be corrected/reworded (and served-web auth owned) by the named inheritor.

**Closure bar:** CLEAN requires zero P1/P2 — therefore T8.5 requires one remediation round on
T85-1 before reverify can be CLEAN.

---

## Remediation disposition (T8.5 remediation round — 2026-08-15)

| Finding | Severity | Disposition |
|---------|----------|-------------|
| **T85-1** | P2 | **FIXED** — serve-web now mirrors T8.7's fail-closed bind auth (see below) |
| **T85-2** | P3 | **FIXED** — done-when corrected to name `lambo serve-web` beside `lambo serve` |
| **T85-3** | P3 | **ACCEPTED** — `flush_lag` / `log_depth` are `n/a` by design; a lease-free reader cannot observe the writer's flush task, and printing `0` would be a lie shaped like a measurement (same call `cli::stats` makes). The page already discloses this on the tiles + tooltip. A writer-side stat endpoint is out of scope (that is P9 / authorized-writer scope) and was deliberately **not** added. No code change. |
| **T85-4** | P3 | not owned by this remediation; informational for the T8.4 video crew (manual keypress). Unchanged. |

**T85-1 fix (mirrors `mcp::serve`'s T8.7 fail-closed posture):**

* `src/cli/serve_web.rs` gains a serve-web-local bearer-auth set mirroring T8.7:
  `AuthToken` (redacting `Debug`, rejects empty tokens), `tokens_match`
  (constant-time), `bearer_ok` (RFC 7235 §2.1), `resolve_auth_token`
  (env `LAMBO_AUTH_TOKEN` wins over the `--auth-token` flag; a set-but-empty
  env fails closed), and `authorize_bind_web` (a non-loopback bind with **no**
  token is a hard startup refusal; loopback stays unauthenticated by default so
  a judge's browser works).
* `main.rs` `ServeWeb` arm accepts `--auth-token` (typed `AuthToken`) and wires
  it into `Args`.
* A `require_auth` axum middleware sits over the **whole** router and requires
  `Authorization: Bearer <token>` on every route whenever a token is
  configured; with no token (the loopback default) it is a pass-through.
  Surface stays read-only — no write route, no mutation path added.
* Stale "T8.7 pending" wording removed from the module doc, the startup stderr,
  `web/app.js`'s off-loopback banner, and `main.rs`'s help/docs, replaced with
  the actual behavior (loopback unauthenticated default; non-loopback requires
  the token; read-only surface).
* New tests: `authorize_bind_web_fails_closed_off_loopback` (mirrors T8.7's
  bind-auth unit test), `an_empty_auth_token_is_refused`, `bearer_header_is_parsed_strictly`,
  and `a_configured_token_is_required_on_every_route` (e2e: every route 401
  without / wrong token, 200 with the correct token).

**T85-2 fix:** `dev-diary/PHASE-8-surface.md` done-when now reads "a browser
against `lambo serve-web` (the separate read-only demo command, port 7710) …
with `lambo serve-web` running **beside** the MCP writer `lambo serve` on the
same session."

**Verification:** full binding gate block run green; live-verified on
`--bind 127.0.0.1` (serves unauthenticated, no token needed), `--bind 0.0.0.0`
with no token (refuses to start with an honest error), and `--bind 0.0.0.0`
with `LAMBO_AUTH_TOKEN` set (starts and requires the token on every request).
No mutation route; read-only holds.

---

## P8 reverify verdict (independent, T85Reverify — 2026-08-15)

**Verdict: CLEAN.** Re-ran the full investigation independently; every claimed
fix is real and no regression or out-of-scope edit was found. I did not modify
any source.

**Findings resolution.**
* **T85-1 FIXED (verified):** `authorize_bind_web` fails closed for a
  non-loopback bind with no token (hard startup refusal, exit 2); loopback
  stays unauthenticated. `resolve_auth_token` has `LAMBO_AUTH_TOKEN` (env) win
  over `--auth-token`, and a set-but-empty env var is a usage error. The
  `require_auth` middleware layers over the whole router, so with a token set
  every route returns 401 (identical for missing/wrong header) and 200 for the
  correct token; with no token it is pass-through. `tokens_match` is genuinely
  constant-time: it iterates all presented bytes, accumulates a single XOR
  diff, applies `std::hint::black_box`, and has no data-dependent early exit
  (the only early return guards an empty *expected*, which `AuthToken::new`
  rejects). `bearer_ok` parses strictly per RFC 7235 (scheme
  case-insensitive, credential exact). No `#[allow]`/`#[expect]`/shortcut.
  Read-only preserved: all routes are `get`; POST on a route returns 405 even
  with the correct token; `read_only_router_has_no_mutating_route` and
  `the_module_registers_only_get_routes` still gate the build.
* **T85-2 FIXED (verified):** done-when in PHASE-8-surface.md names
  `lambo serve-web` (port 7710) running beside the MCP writer `lambo serve`.
* **T85-3 / T85-4:** disposition (accept-by-design / informational) is
  reasonable; no code change required, unchanged.

**Scope (no out-of-scope edits):** `git status` touches only
`src/cli/serve_web.rs`, `src/main.rs`, `web/app.js`,
`dev-diary/PHASE-8-surface.md`, plus the new review file. `src/mcp/*` and
`src/store/*` are untouched.

**Independently re-run gates (this reverify):**
* `cargo build --features store-sqlite,fixtures` — CLEAN.
* `cargo test --features store-sqlite,fixtures --lib cli::serve_web` — 20/20 ok,
  incl. the four new tests and both read-only guards (not weakened; only
  assertion-message wording changed).
* Live smoke (`cargo build`d binary, memory store, servers killed after):
  - `--bind 127.0.0.1` → starts, unauth `GET /` = 200, corrected banner (no
    "T8.7 pending").
  - `--bind 0.0.0.0`, no token → refuses to start, exit 2, honest error naming
    `LAMBO_AUTH_TOKEN`/`--auth-token`.
  - `LAMBO_AUTH_TOKEN=s3cret --bind 0.0.0.0` → no header 401, wrong token 401,
    `Basic` scheme 401, correct token 200 (API + page), `WWW-Authenticate:
    Bearer` on 401, POST (mutation) still 405 with the correct token.
  - Empty `LAMBO_AUTH_TOKEN` (loopback) → usage error, exit 2.
  - Env overrides flag: env token accepted, flag token rejected (401).

The new tests pin the auth (they fail if it is removed: the e2e asserts 401 on
every route without the token), and no existing test was weakened.

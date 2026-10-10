# #4 PR 4: the page's session picker (decisions)

Base: main `9c63b060` (#4 PR 3 merged as #90). Design of record: the
approved #4 design (`4-DESIGN.md`, owner-approved 2026-10-09), sections
6.1 and 6.2, the PR 4 row of section 9, section 10, and Q9 and Q14.
Decisions and why; the commits carry the mechanics.

PR 2 already moved the page to relative `api/...` URLs and added the
`/s/{id}` to `/s/{id}/` redirect (its review M1), so base-path derivation
and deep links were in place. PR 4 adds the picker, the signal that tells
the page whether to show it, and the empty-session state.

## What changed

| piece | where |
|---|---|
| `switchable` on `/api/session`, serialized only when `true` | `src/cli/serve_web/dto.rs`, `routes.rs` (`api_session`) |
| `reach` (shared with the startup count lines) and `switchable` | `src/cli/serve_web/auth.rs` |
| the scoped resolution attaches `Caller`, as the gate does | `src/cli/serve_web/scope.rs` |
| the picker form (hidden by default), its styles | `web/index.html`, `web/app.css` |
| `send` (the one network call), `rootPath`, the picker, the history, the empty-session state | `web/app.js` |
| tests | `src/cli/serve_web/tests/page.rs` (new), `tests/credentials.rs` |

## Decisions

**How the page knows it is in single-session mode: a per-caller
`switchable` field (the design left this open).** Design 6.2 says "single-
session mode shows no picker", but the page had no signal: the listing is
opt-in, and off it is unrouted, so it cannot be the signal; and the page
cannot probe names it does not know. `/api/session` therefore gains
`switchable`, `true` when the presenting grant reads more than one served
session, counted exactly as the startup count lines count it
(`authorize_default` over the allowlist). Three choices inside that:

- *Per caller, not the allowlist size.* "More than one served" would tell
  a credential scoped to one session that others exist. The caller's own
  reach says nothing outside its scope, and it matches the owner's wording
  for PR 4: no picker noise when only one session is *readable*.
- *A prefix grant counts what it reads.* A `session_prefix` credential
  over two allowlisted ids gets `true` (it learns one bit: "you can read
  another session", never a name). The listing still never expands a
  prefix, so such a caller gets the text field, not a list. (Review L2:
  that held only when the listing named fewer than two sessions; a grant
  with exact names plus a prefix got a select of the exact names alone.
  The select now ends with "Other session…", which reveals the field.)
- *Absent when false* (`skip_serializing_if`). The single-session payload
  is byte for byte what it was, which is what the earlier PRs' parity
  checks held to; the page treats an absent field as `false`. (Review I4:
  the JSON and the headers are unchanged; the page's HTML is not, it
  carries the picker's hidden markup, so the docs say "looks the same".)

No new route.

**Listing first, history as the fallback.** With `switchable`, the page
asks for the listing at the site root. A list of two or more names (the
current session added if the caller's grant reads it but the listing does
not name it, e.g. through a prefix) becomes a `<select>`. A 404 (listing
off), an error, or fewer than two names becomes a text field with a
`<datalist>` of this browser's history (`localStorage` `lambo-sessions`,
most recent first, at most 20, names only, every access in a `try`, and
what it holds filtered through the name rule before use). With a listing
the browser keeps no history (design 6.2, "one source of truth").

**Every URL stays relative, including the listing.** The PR 2 source pin
forbids an absolute `/api` literal. The listing (unscoped, refused under
`/s/{id}/`) and other sessions' pages are built on `rootPath()`: `""` at
`/`, `"../../"` at `/s/{id}/`. So `../../api/sessions` from a scoped page
and `api/sessions` from `/` both resolve to `/api/sessions`. The pages are
only ever at `/` and `/s/{id}/` (the bare `/s/{id}` redirects), so the two
cases are exhaustive. Static assets stay absolute, as PR 2 decided.

**Switching is a navigation, and it asks first.** Design 6.1:
`location.assign` to `/s/{name}/`, never an in-page swap, so the poll
cursor, the graph, the focus and the freshness start over. Before
navigating, the picker sends `HEAD` to the target page. The page route
reads no store, so this costs nothing, and in scope it is 200; out of
scope it is the uniform 404. A 404 is reported in the picker's live region
and the name is dropped from the history (design 6.2, "a name that 404s is
dropped from the list on that answer"). The alternative, navigating
blindly, lands the user on an empty-bodied 404 with no way back to the
picker and no chance to drop the name. The probe is no oracle beyond what
navigating would show.

**The client name rule is the server's.** `/^[A-Za-z0-9_:-][A-Za-z0-9._:-]{0,127}$/`,
`parse_addressed`'s charset, length and leading-dot rule; a test checks
the two agree on 17 names. A name outside it is refused in the picker
rather than sent. Names are concatenated into the path unencoded: the
charset needs no encoding, and `encodeURIComponent` would turn `:` into
`%3A`, which the server refuses as malformed.

**No markup sinks.** Names and messages reach the DOM through `textContent`
(the existing `el()` helper) and `<option>.value`. The script contains no
`innerHTML`, `outerHTML`, `insertAdjacentHTML`, `document.write`, `eval(`
or `new Function` (source test). There is no CSP header today; adding one
would change every single-session response's bytes, so it is left as an
open question rather than slipped in.

**Accessibility.** A `<form>` with a submit button, so Enter opens the
choice; a visually hidden `<label>` whose `for` follows whichever control
is shown; focus rings on both controls; messages in a `role="status"`
`aria-live="polite"` region. The `<select>` changes nothing until
submitted (no navigate-on-change, which fires on arrow keys).

**Q9: "No memory in this session yet".** The owner's Q9 answer says the
frontend shows this state for an empty (never written or erased)
allowlisted session. The page said "Concepts are being recorded; none has
reached Canonical", which claims memory that is not there. Empty is known
from the structure (no concept nodes) or, before it answers, from the
poll's concept count. Its own commit. Only an empty session's page
changes.

**Q14: the browser bearer.** Unchanged, as the design says: the page
sends no `Authorization` header. Behind a proxy that adds one per user,
the picker follows that token's scope (`switchable` and the listing are
per caller). The docs say so.

**The `<title>` names the session** when the picker is shown
(`proj-b · Lambo, session memory`), so tabs on several sessions can be
told apart. A single-session page keeps its title.

## Checks

**Browser.** Three `lambo serve-web` processes on ports 7741 to 7743 over
a scratch SQLite store (`proj-a`, `proj-b` with a concept named
`<img src=x onerror=alert(1)>`, and the never-written `proj-empty`),
driven in the desktop app's browser pane:

- listing off (7741): the text field at `/`; an unknown name gives "No
  session called no-such can be read here." and the URL stays; typing
  `proj-b` and Enter navigates to `/s/proj-b/`, the title and name follow,
  the history becomes `["proj-b","proj-a"]`, the suggestions offer
  `proj-a`; the script-shaped concept renders as text (no `<img>` in the
  page); `/s/proj-a?focus=login%20handler` redirects and restores the
  focus; `/s/proj-empty/` shows "No memory in this session yet" in the
  hero and the history;
- listing on (7742): a select of the three names with the current one
  selected, the label bound to it; choosing `proj-a` and pressing Enter
  on Open navigates;
- one session (7743): no picker, the original title, and the same six
  requests as before (`/app.css`, `/app.js`, `/api/session`, `/api/graph`,
  `/api/pulse`, `/api/inspect`): no listing request;
- 375 px wide: the picker wrapped and broke the session name mid-word;
  fixed (`fix(web): wrap the picker, ...`), the rule applying only while
  the picker is shown.

Arrow keys on the focused closed `<select>` did not change its value in
the pane (macOS opens the native popup instead), so the select was set by
the form-input tool and submitted from the keyboard.

**Mutations, each caught:** `switchable` from the allowlist size (the
credential test), always `true` (the single-session and credential tests),
no `Caller` on scoped requests (three tests, including alias equality),
the field always serialized (the single-session and credential tests).

## Open questions

- A Content-Security-Policy for the page (`default-src 'self'`) would
  harden it further; it changes every response's headers, so it needs the
  owner's call on the single-session byte-identity rule.
- With a long allowlist and the listing off, the text field relies on the
  user knowing names (decision 3). Nothing more is planned.

## Not done here

PR 5: H7 closure, `lambo.example.toml` `[web]`, the full
`feature-4-multi-session-portal.md` note.

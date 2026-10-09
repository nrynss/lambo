# #32 PR 1: the session surface, `[serve]` config and the ledger `session` field (decisions)

Base: main `b0e8207`. Design of record: the approved #32 design (issue #32
comment, approved by the repo owner on 2026-10-09 with all 19 recommendations),
section 8's PR 1 row. Decisions and why; the commits carry the mechanics.

PR 1 adds no routing and changes no serve behaviour. A `lambo serve` with or
without a `[serve]` table behaves exactly as before; the one runtime difference
is the additive `session` key on ledger lines.

## What changed

| piece | where |
|---|---|
| addressed-id validation, grants, the uniform 404 | `src/surface/session.rs` |
| `[serve]` parse, validation, credential resolve | `src/config/serve.rs` |
| `LamboFile.serve`, validation at the file boundary | `src/config.rs` |
| `Ledger::for_session` and the stamp in `append` | `src/ledger.rs` |
| scoping sites | `src/mcp/serve.rs` (open), `src/mcp/server.rs` (`with_ledger`), `src/memory/builder.rs` (write pipeline) |
| pre-existing fix: no source line in config errors | `src/config.rs` (`toml_error`) |

## Decisions

**`AddressedSessionId` is its own type.** `crate::types::SessionId` already
exists and means "any name the store accepts" (the CLI's looser
`check_size` rule). The addressed type is proof that decision 16's strict rule
ran, so PR 4's router cannot hand an unchecked path segment to an attach.

**The uniform 404 is axum's own unrouted 404.** Status 404, no headers of
its own, empty body: exactly `StatusCode::NOT_FOUND.into_response()`, which is
what axum 0.8's router answers for an unmatched path (`routing/not_found.rs`).
On the wire the server adds `content-length: 0` to both. A refused session is
then indistinguishable from a path that was never routed, which is stronger
than "identical across refusal reasons". A wire-level test
(`a_refused_session_and_an_unrouted_path_are_identical_on_the_wire`) serves a
real router on loopback and compares the full responses, minus `date`. The
claim holds only while every layer is added with `.layer` (a `.route_layer`
skips unrouted paths) and the session route answers every method: **PR 4 must
route `/mcp/s/{session}` with `any`, returning the uniform 404 for a refused
id on every method**, because an unrouted method otherwise gets 405 plus
`Allow`. That would not reveal whether a session exists, but it would break
the "indistinguishable from an unrouted path" claim. The test pins both
failure modes. `SessionRefusal` keeps a
`RefusalReason` for the operator's log only; its `Display` never includes the
probed id.

**Missing capability is the uniform 404 too, and scope is checked first.**
Design §8 PR 7 says a credential without `erase`, or out of scope, gets the
uniform 404. Checking scope before capability means a probe for an
out-of-scope id never learns whether a capability would have mattered.

**A prefix never covers itself.** `dc-u-` names no user. An id must be
strictly longer than the prefix to be in its scope.

**Every session name in `[serve]` uses the strict charset**, not only the
pinned list: `default_session`, `[[serve.projects]] session` and credential
`sessions` entries too. `[serve]` is new, so nothing deployed can break, and
each of those names can end up in a URL. `--session` keeps its looser rule
(decision 16), and the refusal message says so.

**Structural validation runs in `LamboFile::from_toml_str`.** That is the
file boundary every command crosses, so a malformed `[serve]` fails closed
everywhere, like an unknown key. Token resolution does **not** run there or
in `resolve_backends`: store-only and CLI commands must not need serve's
secrets, and the variable names are configured, so `RESOLVE_ENV_VARS` cannot
list them. `ServeConfig::resolve_credentials` is the serve-time entry point
(PR 5 calls it).

**An inline `token` key is parsed, then refused.** The parser accepts it as
`InlineToken`, which discards the value whatever its type, so `validate` can
refuse it with "use `token_env`" rather than a bare unknown-field error. The
value is never held, logged or serialized.

**Reserved names and the legacy variable.** `default` (what `--auth-token`
becomes, §6.1) and `local` (the implicit loopback credential) are reserved,
and `token_env = "LAMBO_AUTH_TOKEN"` is refused, so a log line naming a
credential is never ambiguous. Two credentials with one `token_env`, or whose
variables resolve to the same token, are refused: a request presenting it
could not be attributed.

**`token_env` is never echoed unless it is a safe name** (review M1). It is
the key a token is most likely to be pasted into. It must be
`[A-Z_][A-Z0-9_]*`, at most 64 bytes, and must not look like a token (an AWS
key id, or a 20-plus character run of letters and digits with no `_`); a
lower-case value is refused because tokens are mixed or lower case and
variable names conventionally are not. A refused value is never quoted
("(value not shown)"). Refusing at parse time means a token-shaped value can
never reach the duplicate check or PR 5's "environment variable X is not set"
path; those messages also go through `shown_env` as a second line. `[serve]`
is new, so tightening the rule from `[A-Za-z_][A-Za-z0-9_]*` breaks nothing
deployed.

**`[serve]` is not serialized when empty**, so `toml::to_string` of a
`LamboFile` without it is still readable by a binary that predates it.

**The ledger stamps the session through a scoped handle, not new builder
arguments.** `Ledger::for_session` returns a handle onto the same file,
writer thread and counters that inserts `session` into every object line it
appends that lacks one; a line that names its own session (startup, lease)
keeps it. The line builders and their callers in `writeq` and the tool
wrappers are untouched, which keeps this PR off #11's files. PR 4 gives each
attached session `ledger.for_session(id)`. `LINE_VERSION` stays 1: the field
is additive and the analysis kit ignores unknown keys.

## Pre-existing defect fixed

`LamboFile::from_toml_str` formatted `toml::de::Error` with `Display`, which
quotes the offending source line. A misspelled key beside a secret (a `dssn`
typo next to a DSN with a password, or a token under a typo of `token_env`)
printed the secret into the startup error and from there into launchd or
systemd logs. The error now carries the parser's message and a line and
column. Its own `fix(config)` commit, failing test first.

Residual, corrected after review (L1): the first version of this note said no
secret-bearing key could reach the value-echo path. That was wrong. `[store]
kind` and `[embedder] kind` deserialize through `FromStr`, which quoted the
value (`unknown store kind "postgresql://u:pw@h/db"`), and serde's `invalid
type: string "...", expected usize` quotes a string under a numeric key such
as `dim`. A DSN or token pasted under one of those keys reached the startup
log. Fixed in its own `fix(config)` commit: the two kind parsers no longer
echo (they list the accepted kinds), `toml_error` replaces serde's quoted
`string "..."` and `unknown variant `...`` values with `(value not shown)`
while keeping field names, and `promotion_policy` quotes only a short word
(its tests pin that a typo is shown). `token_env` is covered separately (M1).

## Wire compatibility

- Ledger: `session` on `call`, `completion` and `stats` lines. Additive; no
  key changes meaning.
- Config errors: the text of a `lambo.toml` parse error changes shape (no
  snippet; `(line N, column M)` suffix). Documented in the config reference and
  the changelog.
- Rust API: `LamboFile` gains a public field, so an external struct literal
  breaks (changelog "Breaking"). In-tree, four literals add
  `serve: Default::default()` (one in `config.rs`, three in `resolve.rs`
  tests); no assertion changed.
- No MCP tool, `get_info`, route or CLI flag changes.

## For PR 4, 5 and 8 (forward constraints from the PR 1 review)

Not defects in PR 1; each is a decision a later PR must make or a check it
must add.

- **PR 4/8: a non-hosted default.** `validate()` does not require
  `default_session` or a `[[serve.projects]] session` to be pinned or covered
  by any credential's prefix. PR 4/8 must decide what `/mcp` (and a cwd-mapped
  stdio serve) does when that session is not hosted: attach it on demand, or
  refuse at startup.
- **PR 4: the pinned cap after the union.** `pinned.len() > max_attached`
  counts `[serve] sessions` only. The design's pinned set is the union with
  repeatable `--session` (§7.2), so PR 4 must re-check the cap after the
  union.
- **PR 4: every method gets the 404.** Route `/mcp/s/{session}` with `any`
  and answer a refused id with the uniform 404 on every method; keep every
  guard on `.layer`, not `.route_layer`. Pinned by
  `a_refused_session_and_an_unrouted_path_are_identical_on_the_wire`.
- **PR 5: the legacy token.** `resolve_credentials` refuses two
  `[[serve.credential]]` entries with one token, but cannot see the legacy
  `LAMBO_AUTH_TOKEN` / `--auth-token` credential (`default`). PR 5 must also
  refuse a configured token equal to the legacy one, without quoting either.
- **PR 5: drop the unenforced warning.** `ServeConfig::warn_if_unenforced`
  says `[serve]` is not yet enforced; PR 5 (credentials) must narrow or remove
  it as each part starts being enforced, and the config reference's "will be
  read" goes back to the present tense.
- **PR 8: canonical project paths.** `[[serve.projects]]` duplicate-path
  detection is textual (`~/a` vs `~/a/`, symlinks). PR 8 canonicalises before
  comparing.
- **Timing (no action).** A refusal for an out-of-scope id makes no store
  call whether or not the id exists, so there is no existence oracle. The only
  store-dependent timing is in scope (absent vs present), which the design
  allows.

## Not in this PR

The `serve()` split (PR 2), calibration (PR 3), the registry and routing (PR
4), credential enforcement (PR 5), on-demand scope (PR 6), the admin surface
(PR 7) and the cwd map's resolution (PR 8).

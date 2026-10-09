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

**The uniform 404 is axum's own unrouted 404.** Status 404, no headers, empty
body: exactly `StatusCode::NOT_FOUND.into_response()`, which is what axum 0.8's
router answers for an unmatched path (`routing/not_found.rs`). A refused
session is then indistinguishable from a path that was never routed, which is
stronger than "identical across refusal reasons". `SessionRefusal` keeps a
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
column. Its own `fix(config)` commit, failing test first. Residual: the
parser's message for a wrong-typed value can still include that value (for
example `invalid type: string "...", expected usize`); no secret-bearing key
in the file is typed so that a secret would reach that path.

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

## Not in this PR

The `serve()` split (PR 2), calibration (PR 3), the registry and routing (PR
4), credential enforcement (PR 5), on-demand scope (PR 6), the admin surface
(PR 7) and the cwd map's resolution (PR 8).

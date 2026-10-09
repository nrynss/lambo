# #32 PR 8: the stdio cwd map, `[[serve.projects]]` (decisions)

Base: main `42a5ead`. Design of record: the approved #32 design, sections 2.2
(stdio), 2.3 (agents: mapping project to session), 7.1 and 7.2, and section 8's
PR 8 row: longest prefix wins; `~` expansion; canonical cwd; falls back to
`default_session`, else refuses; `--session` always wins. Decisions and why; the
commits carry the mechanics.

PR 8 is session *selection* for a stdio serve and nothing else. It runs in
parallel with PR 4 (the session registry) and touches none of
`src/mcp/serve/{registry,shutdown,session,process}.rs`, nor `src/mcp/serve.rs`:
the selected id reaches `ServeOptions.session` exactly as a `--session` value
always has, so the serve, the J2 proxy and the lease see no difference.

## What changed

| piece | where |
|---|---|
| the resolver, `SelectedSession`, `SessionSource`, `SessionSelectionError`, `MissingSession`, `SESSION_REQUIRED` | `src/config/serve/projects.rs` |
| resolver tests (21, one of them macOS-only) | `src/config/serve/projects/tests.rs` |
| entry path check in `validate`; narrowed unenforced notice (`has_unenforced_keys`) | `src/config/serve.rs` |
| `--session` optional; selection before backends; refusal; startup log line | `src/main.rs` |
| end-to-end: mapped, default, flag wins, refusal, http, HOME unset, ambiguous map exits 1 | `tests/serve_stdio_cwd_map.rs` |
| docs | `docs/reference/{cli,config}.mdx` and site mirrors, `lambo.example.toml`, `CHANGELOG.md` |

## Decisions

**Order is the design's, with `--session` short-circuiting everything.** A
serve given `--session` reads neither the working directory nor the map (the
resolver never calls its cwd closure; a test panics if it does), and the flag
keeps its looser id rule. Mapped and default ids go through
`surface::session::parse_addressed` again at selection, although `validate`
already ran it, so a `ServeConfig` built in code is held to the same rule.

**Prefixes compare by path component.** `Path::starts_with`, not a string
prefix: `~/work/lambo` covers `~/work/lambo/src` but not `~/work/lambo-wt-32h`.
"Longest" is the count of normal components of the canonical entry path.

**Both sides are canonicalized; symlinks are followed on both.** The cwd and
every entry go through `std::fs::canonicalize`, which resolves every symlink,
`.` and `..`. So a project entered through a symlink selects the entry for the
link's target, an entry written through a symlink (`/tmp` on macOS) still
matches, and a symlink inside a project that points out of it leaves the
project (the cwd is where the link leads). The alternative, matching the
lexical `$PWD`, was rejected: it is client-controlled, `..` could make a
directory look like it is inside a project it is not, and `current_dir()` on
unix already returns a resolved path, so lexical matching would not even be
consistent. Canonicalizing only stats; nothing is opened or read.

**An entry that does not exist is skipped, not refused.** An existing
canonical cwd cannot be inside a directory that does not exist, so skipping is
exact, not a guess, and a map listing a checkout that is not on this machine
keeps working everywhere else.

**A cwd that cannot be canonicalized falls back to `default_session`**, as the
design says, with its own `SessionSource` so the startup line is a WARN that
says the map was skipped. Without a default, it is the plain refusal.

**Ambiguity is refused, not resolved by file order.** Two entries that
canonicalize to the same directory (one via a symlink, say) and name different
sessions would otherwise choose by position in the file; memory landing in a
session by accident is the failure this map must not have. Only the longest
level matters: a clash above a deeper match does not block the deeper entry.
Entries that agree are fine. `validate` already refuses the same `path` string
twice.

**`~` handling: `~` and `~/...` expand to `$HOME`; `~user` and relative paths
are refused at file read.** Both are new `validate` refusals (in
`check_project_path`). A relative path has nothing stable to be relative to
(the client picks the cwd); `~user` would make the process resolve another
account's home implicitly.

**A relative `$HOME` counts as unset** (review L1). `fs::canonicalize`
resolves a relative path against the process cwd, which for a stdio serve is
the project the client chose; with `HOME=.` a `~` catch-all canonicalized to
the cwd itself and beat the real entry. `select_stdio_session_with` filters a
non-absolute home, so the injected and real paths behave alike.

**With `HOME` unset, `~` entries are skipped, not refused** (review L2; this
reverses the first cut, which refused the whole selection). The rest of the
map and `default_session` still apply; `SelectedSession::tilde_entries_skipped`
makes `main.rs` log one WARN, and a refusal's hint says the `~` entries were
skipped. Neither quotes an entry, the cwd or `$HOME`. Why: it is the same
degrade-and-say-so the design already chose for an unresolvable cwd, which
skips the *whole* map; refusing over `HOME` was stricter than that for a
smaller unknown, and it cost every absolute entry that did cover the cwd. The
cost, accepted: a cwd under a skipped `~` entry can land in a shallower
absolute entry or in `default_session`, which the WARN names. An operator who
wants fail-closed writes the path out in full. The alternatives the review
raised were not taken: the passwd home (`std::env::home_dir` falling back to
`getpwuid_r`) silently disagrees with an environment that deliberately cleared
`HOME`, and "refuse only if a `~` entry could be deeper" cannot be decided
without knowing where `~` is.

**HTTP still requires `--session` in this PR.** The design's selection order is
for stdio. What `[serve] sessions` / `default_session` mean for HTTP is PR 4's
registry, and an HTTP serve that quietly served `default_session` alone would
pre-empt it. So `--transport http` without `--session` gets the same refusal.

**The refusal is clap's own error.** `--session` is now `Option<String>`, and
`main.rs` renders `SESSION_REQUIRED` through `Command::error(
MissingRequiredArgument, ..)` on the built `serve` subcommand: the same
`error: the following required arguments were not provided: --session
<SESSION>` text and exit code 2 as before; only the usage line now reads
`lambo serve [OPTIONS]`. A stdio refusal adds one line naming the two keys that
could have supplied a session. Selection runs before `resolve_for_command`, so
a refusal still builds no backend (no embedder load), as clap's did.

**The refusal hint says why** (review L4). `Missing` carries a
`MissingSession`: when the cwd could not be resolved the hint says the map
was not checked and there is no `default_session`, instead of "neither applies
here", which would claim the map was consulted.

**Exit codes and ordering, against the clap-required flag** (review, clap
compatibility). Missing session: exit 2, same first lines. A map that cannot
be applied (two entries for one directory, different sessions; a session name
that is not addressable) is a configuration error: exit 1, before any backend
or ledger (tested end to end). Two orderings changed when `--session` is
absent, because selection now reads `lambo.toml` and parses `--transport`
before clap's old check would have fired: a bogus `--transport` reports the
transport error (still exit 2) instead of the missing flag, and a malformed
`lambo.toml` exits 1 with its load error instead of 2 with the missing-flag
error. With `--session` given nothing changes. `--session` with no value is
still clap's own error, exit 2.

**Refusals and logs never quote the working directory or `$HOME`.** They name
configured values only: an entry's `path` as written and session names, which
are configuration, not secrets. Tests assert a marker in the cwd never appears.

**The unenforced notice narrows, keeping its prefix.** A table that sets only
`default_session` and `[[serve.projects]]` is fully enforced for stdio, so
`warn_if_unenforced` now asks `has_unenforced_keys`. The text keeps
"[serve] is parsed but not yet enforced" (the PR 1 integration test greps it)
and names the exception. PR 4/5 still own removing it.

**One global map needs `--config` or `LAMBO_CONFIG`** (review L6). Discovery
is `--config`, `LAMBO_CONFIG`, then `./lambo.toml`, and a stdio serve's cwd is
the project, so without either the map read is the project's own file. The
docs say so, with the note that a project's `lambo.toml` can then choose the
session through `default_session`, the same trust it already has over the
store.

**Tests the review asked for:** `/` as a catch-all losing to a deeper entry;
macOS-only (`cfg(target_os = "macos")`) case and NFC/NFD variants matching and
two such variants with different sessions refused, which holds because
Darwin's realpath returns the on-disk spelling (the test skips itself on a
case-sensitive volume); relative `HOME`; HOME unset end to end; an ambiguous
map exiting 1 end to end.

## For PR 4 (merge notes)

- `main.rs`'s serve arm now unwraps `session: Option<String>`; when PR 4 makes
  `--session` repeatable for HTTP, `select_serve_session`'s `Transport::Http`
  arm is the place that changes.
- `SERVE_UNENFORCED_NOTICE` and `has_unenforced_keys` will need PR 4/5's
  narrowing on top of this one.

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
| the resolver, `SelectedSession`, `SessionSource`, `SessionSelectionError`, `SESSION_REQUIRED` | `src/config/serve/projects.rs` |
| resolver tests (17) | `src/config/serve/projects/tests.rs` |
| entry path check in `validate`; narrowed unenforced notice (`has_unenforced_keys`) | `src/config/serve.rs` |
| `--session` optional; selection before backends; refusal; startup log line | `src/main.rs` |
| end-to-end: mapped, default, flag wins, refusal, http | `tests/serve_stdio_cwd_map.rs` |
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
account's home implicitly. A `~` entry with `HOME` unset is refused at
selection rather than skipped: the serve cannot tell whether the cwd is inside
it, and guessing would be the accidental-session failure again.

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

**Refusals and logs never quote the working directory or `$HOME`.** They name
configured values only: an entry's `path` as written and session names, which
are configuration, not secrets. Tests assert a marker in the cwd never appears.

**The unenforced notice narrows, keeping its prefix.** A table that sets only
`default_session` and `[[serve.projects]]` is fully enforced for stdio, so
`warn_if_unenforced` now asks `has_unenforced_keys`. The text keeps
"[serve] is parsed but not yet enforced" (the PR 1 integration test greps it)
and names the exception. PR 4/5 still own removing it.

## For PR 4 (merge notes)

- `main.rs`'s serve arm now unwraps `session: Option<String>`; when PR 4 makes
  `--session` repeatable for HTTP, `select_serve_session`'s `Transport::Http`
  arm is the place that changes.
- `SERVE_UNENFORCED_NOTICE` and `has_unenforced_keys` will need PR 4/5's
  narrowing on top of this one.

# #33 PR 7: repository-scoped Resource identity (#129 part 2)

Status: implemented and locally gated, 2026-10-13 (Codex design and first
implementation; finished by Claude, see Takeover). Not yet reviewed.

The owner selected option (a): a client-supplied repository becomes part of a
path-like Resource's canonical identity. Migration cost does not constrain the
choice (dogfood ruling `de12fd0e`; owner comment on #129). PR #149 supplies the
write-destination `scope` and relative-path refusal seam (`7658f250`).

## Public contract (as implemented)

- Optional call-level `repo_scope` on the `lambo_derive` and
  `lambo_record_action` wire parameters and `--repo-scope` on `lambo derive`
  and `lambo record-action`. In the library, `Action::repo_scope` scopes an
  action and `ParentOf::with_repo_scope` scopes a derive. `ParentOf` is the
  derive context every entry point already threads (canonical, Hybrid, async,
  cited), so the scope covers all of a derive's Resources, not only its
  hierarchy pairs, without new entry-point signatures; the existing
  constructors stay unscoped. `scope` still selects the destination session;
  `repo_scope` identifies a repository inside that session.
- A queued scoped job is durably one `WriteIntentPayload::RepoScoped
  { repo_scope, payload }` wrapper around the unscoped `Derive`, `DeriveCited`
  or `Action` payload; the scope is stored nowhere else. A build that predates
  the variant refuses an unconsumed row by name instead of replaying it under
  the legacy key. Replay refuses an invalid scope, a nested wrapper and a
  wrapped image derive, and settles that row `failed` without stopping the
  replay. On consume, every adapter (MemoryStore through `settled()`, SQLite
  and Postgres through `REPO_SCOPED_PAYLOAD_LIKE`) rewrites the row to the
  empty `derive` a 0.5.0 build decodes, as for image and cited intents.
- Validate a supplied scope as nonempty after trimming, within the normal string
  cap and text safety checks. Treat it as an opaque NFC string. The server never
  reads a filesystem or remote and never claims the scope is verified.
- Scoped Resource keys use `RepoResource:v1:` followed by the JSON tuple
  `[repo_scope, path]`. The uppercase reserved prefix cannot be produced by the
  lowercase legacy canonicalizer; the JSON tuple separates arbitrary components
  without delimiter collisions. Path identity is trimmed NFC text, case-sensitive
  and order-preserving. No stemming, token sorting, synonym rewrite, filesystem
  resolution, symlink resolution, or absolute/relative rebasing applies.
- Missing scope keeps today's key and all existing lookup behavior. Supplied
  scope never falls back to an unscoped path match. Paths cannot merge by vector
  similarity across repository identities, including into an unscoped old hub.
- Scope only Resource identity. A scoped path used in an action's produces,
  modifies, or textual depends_on is a Resource, so producing and depending on
  the same file resolve identically. Explicit id references keep their existing
  reference semantics. Prose and explicitly non-Resource derive concepts retain
  their existing keys.
- Scope-aware hierarchy/citation/text references must find the same scoped
  Resource key. Parent-only path references are Resource under a supplied scope;
  absent scope retains the existing Entity default. Text lookup has no unscoped
  fallback; explicit ids remain the precise way to name an existing old record.

## Path predicate and client guidance

`graph::resource::is_path_like` is syntactic and platform independent:

| Input | Path? | Why |
|---|---|---|
| `README.md`, `Makefile`, `.env` | yes | PR #149's lone-file rule (`is_bare_relative_path`) |
| `src/main.rs`, `docs/reference`, `a\b`, `and/or` | yes | separated, first segment not domain-like |
| `./file name`, `../a`, `/tmp/file name`, `C:\a b`, `~/.config/a` | yes | explicit relative or root prefix, spaces allowed |
| `./src.old/main.rs` | yes | the `./` makes a dotted first segment local |
| `file name.rs` | no | a space without an explicit prefix |
| `src.old/a.rs`, `github.com/a/b` | no | dotted first segment reads as a domain |
| `https://host/a.rs`, `git@host:a.rs` | no | URL, scp remote |
| `Node.js`, `example.com`, `a decision` | no | not a file name, prose |

Every entry PR #149's refusal catches (`is_bare_relative_path`) is also
path-like, so a Resource entry under a valid scope is always exempt; the
domain-led and whitespace exclusions match on both sides. Prose with a slash
(`and/or`) is path-like, but only matters on a Resource, where an exact
per-repository key is harmless.

The general-layer refusal is relaxed only for relative Resource paths actually
covered by a validated repo_scope. A destination session does not infer a repo.

Recommend a stable sanitized remote identifier (host/owner/repository), removing
scheme, user information, query/fragment and trailing `.git`; clients must agree
on the spelling across HTTPS/SSH remotes and worktrees. A stable agreed root
basename is a fallback for a local-only repository, with explicit disambiguation
when basenames collide. Never send credentials or a private machine path as the
scope merely to identify a checkout. The server treats identifiers literally;
it cannot infer whether two spellings name the same remote.

## Existing data and migration

Do not assign a conflated old hub to a single repository automatically. Its
incident records can belong to multiple repositories; its origin interaction is
immutable provenance, not proof of exclusive file ownership. A useful offline
rescope plan needs per-edge/record repository attribution, new identity copies,
and an explicit report of unresolved attribution. Stream D's planner may propose
that work on copies. PR 7 implements and tests new identity semantics with generic
synthetic data; it does not operate on the live rig or forge provenance.

The reason is attribution correctness, not avoiding migration cost. Explicit ids
continue to address old records, while new scoped writes create new identities.

## Compatibility and validation plan

Wire additions are optional and additive. Public struct fields and affected
library write signatures are 0.x library API breaks and will be listed under
CHANGELOG's Breaking section. Mirror CLI/MCP reference docs and schema goldens.
Exercise canonical and Hybrid derive, queued replay, actions and dependency
references, same repo reuse, distinct repos, adversarial tuple delimiters, path
case/order distinctions, unscoped compatibility, empty scope refusal, general
layer exemption, and SQLite snapshot persistence. Run the full lane gate.

## Review and measurements

Pending: the orchestrator's adversarial review runs on the committed head.

## Takeover

Start point (2026-10-13): Codex's uncommitted work, snapshotted with
`git stash create` as `d19e30b7810d0ed74089c61ffeeac0c94273795e` on base
`6add6f12` (branch `codex/129-repo-resource-scope`).

Codex had implemented the whole design (about 50 files plus
`src/graph/resource.rs` and its tests) but had not formatted, gated,
documented or committed it. It compiled. It is committed unchanged as
`d5a6bcce`, then `origin/main` at `57f79386` (#36 PR 5) was merged
(`1cf82dc2`; #19(b) deliberately not included). The merge needed one fix:
canon's alias preview calls `resolve_reference`, which PR 7 had gated to
tests (`f3bf0fc7`). The merge commit itself does not compile without it.

What review of the inherited diff changed, each in its own commit:

- **Scope stored once** (`2f65a336`). A derive's scope rode inside the
  public, durable `PinnedCitations`, so a scoped cited derive stored it
  twice and a public type grew a field unrelated to citations. It now
  lives on the internal `JobPayload::Derive`; the `RepoScoped` wrapper is
  its only durable home, and citations carrying one fail to decode.
- **Replay no longer stalls** (`2f65a336`). A stored `repo_scoped` row
  that replay refuses (invalid scope, nested wrapper, wrapped image derive)
  was a `Config` error, which the replay loop treats as session-wide: it
  stopped the replay and left every later intent `pending_replay`. It now
  settles that row `failed` and goes on, like a cycle refusal. Found by the
  new replay test.
- **No-merge set read under the plan lock** (`54e8020d`). The Hybrid
  derive took a second, unguarded graph read per derive to collect scoped
  nodes; they now join the image-node set read under the plan guard, so the
  write-window replan covers a scoped node written after the read.
  Short-id-shaped action `depends_on` text is pinned under the scoped key.
- **Parameter errors** (`812b164e`). A bad `repo_scope` reached MCP and CLI
  callers as `config: ...`; it is now a parameter or usage error. Wire and
  CLI help text rewritten; goldens and F18 property sets pinned.
- **Doctest** (`ceb11f19`): the `Memory` example's `Action` literal.
- **Coverage** (`2f65a336`, `161dea87`). The inherited Hybrid isolation
  test passed with the no-merge guard removed: the store only ranked flushed
  vectors and the fixture vectors were dissimilar anyway. A constant-vector
  embedder over a graph-ranked store now fails when either guard (scoped
  item never merges, nothing merges into a scoped node) is removed; both
  mutations were run. Also added: layer exemption and refusals over MCP,
  replay of deferred scoped writes and of a malformed row, SQLite
  persistence through the CLI, and scoped intents in the shared
  settled-intent testkit every adapter runs.

Gate on the committed head (fresh `XDG_RUNTIME_DIR` per cargo call):
`cargo fmt --check` pass; clippy `-D warnings --all-targets` pass on
store-sqlite,fixtures / ship,fixtures / no-default store-postgres /
store-postgres,store-sqlite,fixtures (with and without default features) /
embed-eg2 / recall-elastic / embed-candle; `cargo test --features
store-sqlite,fixtures` 2405 passed, 0 failed, 7 ignored (lib 2322/0/5),
against 2390/0/7 at `57f79386`: +15 new tests, none lost, the same
ignored set; no-default store-sqlite `--no-run`, `cargo check
--no-default-features`, Postgres and Postgres+SQLite all-targets
`--no-run` pass; Postgres non-live `cargo test --no-default-features
--features store-postgres` 1233/0/18 (live Postgres and Cockroach rows not
run: manual and `#[ignore]`d); schema goldens pass; mirror drift check
pass; `git diff --check` clean. Each intermediate commit after the merge
fix compiles on its own.

Open after round one (both closed in round two, below):

- `origin/main` moved to `1c5a6bc2` with #19(b) (#151), #152 and #153.
- Recall rendered a scoped Resource by content only, and inspect and
  retract by text could pick one of several same-content concepts.
  (Round one also listed demote here; demote resolves no target, so it
  never had the problem.)

Existing conflated hubs stay untouched by design (provenance, not cost).

### Round two (2026-10-13)

- **Merged `origin/main` at `1c5a6bc2`** (`d5cb0b5e`, a merge, not a
  rebase): #19(b) `against` (#151), its docs (#152) and session
  classification D1 (#153). Conflicts were in `graph::cite`, the intents,
  the three store adapters, the settle testkit and the docs. Resolution
  keeps main's `validate`/`recheck_all`/`write_all_edges` shape and threads
  the repo scope through them: `validate_scoped` pins `depends_on` and
  `against` with the scoped resolver, `recheck_all` takes the scope, and an
  action's `against` is pinned and rechecked under `action.repo_scope`.
  `RepoScoped` may wrap `ActionCited`; `settled()` and every adapter's
  consume UPDATE settle it beside `ActionCited`; the shared settle testkit
  stores a scoped `ActionCited`.
- **Scoped `against` tests** (`8b75bd97`): a scoped derive and a scoped
  action with `against` contradict only their repository's `README.md`
  (both strategies); a scope without the file is refused, never the legacy
  concept; unscoped still names the legacy one; deferred at close they are
  `repo_scoped` wrappers around `derive_cited` and `action_cited` and
  replay under their scope.
- **Retract and inspect** (`f12e725c`). Text that reaches a
  repository-scoped path and any other concept is refused as ambiguous:
  `Memory::retract` with the shared typed refusal
  (`ReferenceField::RetractTarget`, `ReferenceRefusalKind::Ambiguous`,
  candidates with short ids and repositories) and now also accepts a full
  node id; `lambo_inspect`'s focus with its existing `Focus::Ambiguous`,
  candidates listed with repositories and full ids. No wire parameter was
  added: neither surface has a natural repo field, and an id is the exact
  reference the design already names. Without scoped concepts, and for a
  single match, both behave as before.
- **Recall labels** (`94beacd7`). `RecallHit` and the presentation hit
  behind `structuredContent.hits[]` carry `repo_scope` for a scoped path,
  omitted when absent; the label reads `README.md [Resource, repo
  "github.com/acme/widgets"]`. The recall goldens are unchanged.
- Tests for both (`22074de4`).

Gate on the merged head (fresh `XDG_RUNTIME_DIR` per cargo call): fmt
pass; the 8 clippy rows pass; `cargo test --features store-sqlite,fixtures`
2469 passed, 0 failed, 7 ignored (lib 2386/0/5, final head after the
inspect-label commit) against 2449/0/7 on `1c5a6bc2`: +20 tests, none
lost, the same ignored set; SQLite-only
`--no-run`, `cargo check --no-default-features`, Postgres and
Postgres+SQLite all-targets `--no-run` pass; Postgres non-live 1272/0/18
(live Postgres and Cockroach not run); goldens and mirror drift pass;
`git diff --check` clean. Each new intermediate commit compiles.

- **Inspect labels** (own commit after the docs): `lambo_inspect`'s
  neighbourhood label names a scoped node's repository the same way.

Still open: root goals naming a path match every same-content concept (all
of them, not one of them quietly), unchanged; nothing else known.

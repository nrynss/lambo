# #33 PR6: layered serving documentation and cut-over runbook

Base829397b0 (#142 merged), branch `codex/33-layered-docs`.

## Decisions and source verification

- Document implemented HTTP serving layers and MCP recall/inspect read scope.
  `src/config/serve/layers.rs` enforces one layer, pinning, no self/transitive
  layer; `src/mcp/serve/layers.rs` checks grants/contracts and stdio holder vs
  forwarded behavior. `tools/scope.rs` resolves only own session/configured
  layer and looks up a direct layer late. Unlayered and scoped results keep
  plain wire shape. Existing source/schema goldens remain untouched.
- CLI recall/inspect read one explicit session, not the layered MCP stack;
  main.rs exposes --query/--focus/--session and no --scope. Examples use those
  actual flags. No HTTP header routing or new client folder inference.
- Preserve PR142's existing reference content; add project/default/read examples
  and replace the older runbook Section7. `general` holds operator-wide rulings;
  default `general-usage` receives unmatched-folder writes and also reads general.
  A first serve --session overrides the configured default, so the planned
  unit must stop passing the old lambo-dev override at cut-over.
- Write scope remains explicitly TODO for Claude's #33 PR4. Every current write
  uses its addressed endpoint; a scoped read does not move later writes. No
  cut-over/client/hook/config changes were performed.
- Keep C5's stdio cwd/pattern area unchanged. If its PR lands first, integrate
  through a normal main merge and preserve its new pattern documentation.

## Client references (fetched 2026-10-13)

- Local `codex mcp add --help`: HTTP --url, no project-scope flag. Official
  [OpenAI MCP](https://developers.openai.com/codex/mcp/) and
  [config precedence](https://developers.openai.com/codex/config-basic/)
  confirm trusted-project .codex/config.toml, user fallback and shared local
  clients. This corrects the old runbook's global-only Codex assumption.
- Local `claude mcp add --help` plus official
  [MCP scope docs](https://code.claude.com/docs/en/mcp) confirm local/user options
  and same-name local override. Keep public repo free of private registrations.
- Official [Cursor config](https://prod.cursor.com/help/customization/mcp) and
  [CLI MCP](https://prod.cursor.com/docs/cli/mcp) confirm project/global file
  precedence and shared editor/CLI config.
- Official [Claude worktrees](https://code.claude.com/docs/en/worktrees) establish
  the default directory and desktop main-checkout project config behavior,
  but not universal private local-scope HTTP inheritance for CLI launches.
  Require operator checks for fresh/entered/nested/sibling worktrees, rather
  than claim inheritance or edit hooks. No settings/credential files read.

## Cut-over and rollback design

Freeze clients/writer, take fresh completed backup, classify/split/verify a copy,
review exact accounting and ambiguities, stage verified targets, then validate
every URL/client/worktree before resuming. Retain source read-only. After target
writes start, rollback needs explicit reconciliation/replay; silently routing
back to archived source would discard new history. The owner performs cut-over.

## Validation

Reference/site mirrors and git diff --check passed after edits. No code, schema,
golden, dependency or test definition changed; the gate ran the unchanged source
at829397b0, so this PR adds no tests or ignores. Existing layered
scope/merge/degradation/golden tests are the behavior authority.

Every cargo invocation used a fresh XDG_RUNTIME_DIR under /tmp/lbX.*:

| Prescribed gate | Result |
|---|---|
| cargo fmt --check | PASS |
| clippy --all-targets --features store-sqlite,fixtures -- -D warnings | PASS |
| clippy --all-targets --features ship,fixtures -- -D warnings | PASS |
| clippy --all-targets --no-default-features --features store-postgres -- -D warnings | PASS |
| clippy --all-targets --features store-postgres,store-sqlite,fixtures -- -D warnings | PASS |
| clippy --all-targets --features embed-eg2 -- -D warnings | PASS |
| clippy --all-targets --features recall-elastic -- -D warnings | PASS |
| clippy --all-targets --features embed-candle -- -D warnings | PASS |
| cargo test --features store-sqlite,fixtures | PASS:2169unit +80integration/doc =2249passed,0failed,7ignored (5unit,2integration) |
| cargo test --no-run --no-default-features --features store-sqlite | PASS (compile only) |
| cargo check --no-default-features | PASS |
| scripts/docs/check-mirror-drift.sh | PASS |

Both public/runbook TOML examples passed the compiled LamboFile parser and
ServeConfig::resolve_layers with their actual pinned/default lists. Three MCP
JSON read examples matched golden required/property/type declarations; every
non-read tool's schema has no scope. Binary recall/inspect help confirms flags.
Local Codex/Claude MCP add help and fetched primary references confirm setup
syntax; no registrations were executed. Live inheritance, client connectivity,
credentials, Postgres/Cockroach and cut-over were not tested or changed.

## Review trail

Implementation handoff: ready for root's fresh adversarial review and focused
review. Findings/remediation are recorded here as they arrive.

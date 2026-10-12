# #33 PR 3: serve layers and layered `lambo_recall`

Design of record: the #33 layered-recall design, revision 2026-10-12b,
sections 1, 1.1, 2.3, 2.6, 4, 5, 6 and the PR 3 row of section 10. Built
on PR 1 (#133, `format::fit_blocks`), PR 2 (#134, `Memory::recall_layered`)
and #139 (a layer's traversal anchor must agree with the caller's, which
the merge already enforces; PR 3 adds nothing there). Branch
`feat/33-pr3-serve-layers` from main 92c0f4af. Agent id `claude-opus-5-5`.

## What changed

- `src/config/serve/layers.rs` (new): `[serve] layers`,
  `[serve.session_layers]`, `[[serve.layer]]` (`LayerConfig`), the
  resolved `ServeLayers` / `LayerSpec`, crate-private `MAX_LAYERS = 1`,
  the static checks (run by `ServeConfig::validate`) and
  `ServeConfig::resolve_layers` / `layer_stack_of`.
- `src/memory/layered.rs`: `Memory::recall_layered_slots` over
  `LayerSlot::{Live, Unavailable}`; `recall_layered` is that call with every
  slot live.
- `src/mcp/serve/layers.rs` (new): the start checks (pinned stacks,
  credential coverage, embedding contracts), `plan_layers` and
  `stdio_layer_warning`.
- `src/mcp/server/layers.rs` (new): `LayerSource`, `LiveLayer`,
  `LayerHandle` (over `Weak<dyn LayerSource>`), `ServerLayers` (handles
  plus the credential set for the per-call check).
- `src/mcp/serve/registry.rs`: `SessionAttacher.{layers, authority}`;
  `build_session` gives a layered pinned session's server its handles;
  `impl LayerSource for SessionRegistry` answers only a live slot, with
  the layer's in-flight guard.
- `src/mcp/serve.rs`, `src/main.rs`: `ServeOptions.layers`; the CLI plans
  layers before any backend is built; `serve` checks them before any
  lease, and the contracts once the pinned sessions are acquired.
- `src/mcp/serve/http_guards.rs`, `src/mcp/server/trace.rs`: the guard puts
  the request's grant in its extensions (`CallGrant`); `call_tool` scopes
  it task-locally beside the credential name.
- `src/mcp/server/tools/recall.rs`: targets (own, stack, one layer),
  `scope`, `hits[].session`, the merged context verbatim, the ledger
  facts, the image/vector note.
- `src/mcp/server/trace.rs`: `layered_recall_facts` (per-hit `session`,
  `session_hits`, legs from `hit_legs` by position) beside an unchanged
  `recall_facts`.
- `src/mcp/server.rs`: `get_info` appends the layer paragraph for a
  layered session.
- `src/mcp/server/params.rs`: `RecallParams.scope`; `Default` on the six
  parameter structs that lacked it (design section 5).
- Docs: config.mdx "Layers", cli.mdx "Layered recall" plus a one-session
  note on `lambo recall` and `lambo serve-web`, mcp.mdx `scope`, "Layered
  sessions" and the get_info paragraph, each with its site mirror;
  CHANGELOG (Breaking (library API) and Added).

## Decisions, with reasons

- **Pinning is checked at resolve, not in `validate`.** A layer may be
  pinned by `--session` instead of `[serve] sessions`, so only the serve
  that knows its pinned union can check it. `validate` (every command)
  keeps the checks that need no CLI: names, duplicates, `MAX_LAYERS`,
  self, transitive, knob ranges, knobs naming a configured layer. An
  empty override (`x = []`) may name an unpinned session, since it layers
  nothing (first draft refused it; that stopped an HTTP serve for an
  opt-out of an on-demand session, fixed in 7a698e1d).
- **`MAX_LAYERS` guards both list forms** (`layers` and each override),
  through one `check_stack`, the one line to relax for N tiers.
- **`ServeLayers` is opaque and built only by `resolve_layers`.** A
  library caller cannot hand `serve` an unchecked stack; `serve` still
  re-checks that every stack names its own pinned sessions, because a
  caller may resolve against one set and pin another.
- **The handle holds `Weak<dyn LayerSource>`, not `Weak<SessionRegistry>`.**
  The registry is serve-private; the trait keeps `mcp::server` free of it
  and lets the server tests use a fixed source. The substance of design
  section 6 holds: the lookup is per call, no `Arc<Memory>` is cached, no
  cycle forms, and the lookup takes the layer's in-flight guard.
  Consequence measured in a test: a layered call holding a layer's
  `Memory` delays that layer's re-attach until the call ends (the retry
  waits for the old handle to go), which is bounded by the call.
- **An unavailable layer is a slot, not an error.** The merge already
  answers a missing tier (one `layer_unavailable` annotation and warning);
  `recall_layered_slots` lets the serve hand it a configured layer with no
  live `Memory`. Rejected: answering an unavailable layer by dropping it
  from the stack, which would give no annotation and no warning.
- **Credential coverage at start, plus a per-call check.** Start: for
  every grant (implicit `local`, legacy `default`, each configured one),
  reaching a layered session implies reaching each of its layers. Per
  call: the request's grant is checked for every layer the call reads; a
  refusal is a generic `configuration error` that names no session (it is
  a config bug the start prevents), logged by name. A call with no grant
  (the J2 endpoint) is allowed, as design section 1 says.
- **Contracts are compared exactly at start, never normalised.** The
  runtime check in `recall_as_layer` is exact, so normalising `model:
  None` vs `Some("")` at start would leave every layered recall degraded;
  the start refuses with its own message for that case (the #134 note's
  L6). Within one serve all sessions come from one builder template, so a
  mismatch at start is not reachable by configuration today; the check is
  there for the shape the design asks and is unit-tested directly.
- **CLI `lambo recall` and the portal stay single-session.** Both are
  lease-free readers that take no `[serve]` configuration and hold no lease
  on a layer. The merge is the one renderer of `from <session>` and
  `also <id> in <session>`; the MCP wire returns `LayeredRecall.context`
  verbatim and never re-renders through `render_detailed_block`, so the
  #134 note's re-render hazard cannot arise. Rejected: teaching
  `render_detailed_block` the meta (a second renderer of the same text,
  only needed by a caller that does not exist), and loading a layer's
  graph into the CLI reader (a lease-free merge path the design does not
  ask for). Documented: read a layer with `--session general`.
- **`scope` = this session reads it alone**, as its plain recall; on a
  layered session its hits still carry `session` (every hit of a layered
  session does, design 11.2). `scope` = a layer reads that layer's plain
  recall (its own route, recent leg and cache included), which is what
  `/mcp/s/general` answers. Unknown values are refused listing this
  session's scopes, never quoting the value.
- **Recall by image or vector on a layered session reads one session** and
  adds a warning that the layers were not searched. `Memory::recall_layered`
  is text-only (PR 2's API); widening it is a library change outside PR 3.
  With `scope` it reads the named session.
- **`get_info` names layers and `scope` for recall only.** Write scope
  arrives in PR 4; telling agents now to write with `scope = "general"`
  would invite calls this server refuses. PR 4 extends the paragraph.
- **Ledger.** The unlayered line is built by the same code path with
  legs from `detail.legs` and no sessions, so its keys are unchanged.
  The layered line takes legs from `LayeredRecall.hit_legs` by position
  (node ids are unique only within a session) and adds per-hit `session`
  and `session_hits` (a map keyed by session, N-ready). `scope` is added
  only when the call sent one.
- **Stdio** logs one warning when its session would be layered and serves
  unlayered; a library caller's stacks on a stdio `ServeOptions` are
  dropped with a warning.

## Tests (evidence)

New, 29 in the unit binary:

- `config::serve::layers::tests` (12): design example with defaults and a
  knob; no keys; list shapes (scalars refused); unknown keys hard errors;
  `two_layers_are_refused_naming_the_limit` (both forms); self layer;
  transitive layering (two ways) and an empty override on a layer;
  names and duplicates; knobs (no layer, seven bad values); pinning at
  resolve (CLI-pinned accepted, unpinned override refused); override
  then default; serde round trip.
- `mcp::serve::tests::layers` (4): coverage (narrow refused naming the
  credential and both sessions, never a token; layer-only, `*`, implicit
  `local` and legacy `default` pass); pinned stacks; contracts (equal,
  held layer skipped, width mismatch, unset vs empty model named);
  stdio unlayered with one warning, HTTP resolve and refusal.
- `mcp::serve::tests::refuses_to_start::serve_refuses_a_credential_that_reaches_a_session_but_not_its_layer`:
  through `serve`, and no lease row is written for either session.
- `mcp::serve::tests::registry::layered` (3), two pinned sessions in one
  serve, the fixture embedder, the real router and guards:
  end to end (merged hits with `session` on every one, `from general` in
  the context, `general` unlayered with no `session`, `scope`, get_info);
  `general`'s lease lost detaches only `general`, the project degrades
  with `layer_unavailable` and refuses `scope = general` retryably;
  the handle follows a re-attached layer (`Arc::ptr_eq` on the new
  `Memory`, the old one gone) and the registry is freed while a server
  holding a handle lives (no cycle).
- `mcp::server::tests::layered` (9): merge and meta, content[0] equals
  context; unlayered 0.5.0 shape and get_info prefix-only; scope
  (layer, own, unknown not quoted, oversized); a source not serving and
  its return; a dropped source freed (no strong reference); the per-call
  grant check (narrow refused generically for stack and scope, wide and
  no-grant served); the ledger (per-hit session, `session_hits`, a layer
  hit's bm25 leg from `hit_legs`, `scope`, plain line unchanged); a
  recall by image on a layered session;
  `an_unlayered_recall_is_byte_identical_to_the_0_5_0_body`, which
  compares the serialized result against the 0.5.0 body reproduced as a
  reference, at a fitting and a truncating budget. Mutation check:
  forcing `session` onto every hit fails it, the shape test and the
  ledger test.

Goldens: `tool_schemas.golden.json` gains recall's `scope` property only
(and the F18 property set its name); every other golden, the H3 portal
parity tests and the CLI/MCP context parity test pass unchanged.

## Gate (local, each with its own XDG_RUNTIME_DIR)

- `cargo fmt --check`: pass.
- `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings`: pass.
- `cargo clippy --all-targets --features ship,fixtures -- -D warnings`: pass.
- `cargo clippy --all-targets --no-default-features --features store-postgres -- -D warnings`: pass.
- `cargo clippy --all-targets --features store-postgres,store-sqlite,fixtures -- -D warnings`: pass.
- `cargo clippy --all-targets --features embed-eg2 -- -D warnings`: pass.
- `cargo clippy --all-targets --features recall-elastic -- -D warnings`: pass.
- `cargo clippy --all-targets --features embed-candle -- -D warnings`: pass.
- `cargo test --features store-sqlite,fixtures`: 2,218 passed, 0 failed,
  7 ignored (unit binary 2,138 passed, 5 ignored). Before: 2,189 passed,
  7 ignored (unit 2,109). +29, all named above; none removed or ignored.
- `cargo test --no-run --no-default-features --features store-sqlite`: pass.
- `cargo check --no-default-features`: pass.
- `bash scripts/docs/check-mirror-drift.sh`: pass.
- Extra: `cargo test --no-default-features --features store-postgres --lib layer`: 37 passed.

No live database was used. No Cargo.toml, Cargo.lock or workflow change;
`src/store/pg/postgres/tests.rs` not read.

## For PR 4

- `ServerLayers::handle` / `authorize` and `LayerHandle::live` are the
  lookup PR 4's write `scope` needs; `recall_target`'s refusals (unknown
  scope listing this session's scopes, retryable `layer_unavailable`,
  generic grant refusal) are the pattern to share.
- Extend the get_info paragraph with the write guidance once `scope` is
  accepted on writes.
- Piggyback drain over all pipelines and epoch-routed receipts are not
  needed by PR 3 (recall writes nothing), and are untouched.

# #33 PR 4: `scope` on writes and the remaining tools

Design of record: the #33 layered-recall design, revision 2026-10-12b,
sections 1.1, 3, 5, 6, 7 and the PR 4 row of section 10. Owner decisions
it carries: agents may write to `general` at their own discretion, using
`scope`; relative paths are refused in `general`; cross-session edges are
refused in v1 (their ids are reported). Built on #142 (#33 PR 3: the read
side's `scope`, `tools/scope.rs`) and #147 (#36 PR 4: the shared
reference resolver and the `ForeignScopes` / `LayerProbe` seam). Branch
`feat/33-pr4-write-scope` from main e333bc7c. Agent id `claude-opus-5-5`.

## What changed

- `src/mcp/server/params.rs`: `scope: Option<String>` on `DeriveParams`,
  `RecordActionParams`, `DeriveImageParams`, `ReserveParams` and
  `SaintsParams` (all already `Default`, from PR 3).
- `src/mcp/server/tools/scope.rs`: `LamboServer::target` (resolve,
  authorize, look the layer up live), `Target` (the session acted on, its
  relative-path rule, its probe), `target_err` (the lease-loss refusal),
  `take_receipts` and `receipt_pipeline` (the multi-pipeline drain and
  the epoch routing), and `LayerProbe` generalized to "every session but
  the target".
- `tools/write.rs`, `derive_image.rs`, `reserve.rs`, `saints.rs`: act on
  `target.mem(..)`; the scoped ack's line and `session` key; `scope` in
  the ledger facts.
- `src/mcp/server.rs`: `answered` drains through `take_receipts`; the
  five tool descriptions; the layered `get_info` paragraph now says how
  to write to a layer (PR 3's "every write acts on this session only"
  sentence is gone).
- `tools/stats.rs`: the receipt goes to `receipt_pipeline(id)`.
- `src/mcp/server/layers.rs`: `LayerRead::Receipts`.
- `src/mcp/server/response.rs`: `layer_lost`.
- `src/memory/gate.rs`: `Memory::is_closed` (crate-private).
- `src/writeq/receipts.rs`: `WritePipeline::knows_receipt`,
  `take_piggyback_at_most` (public, additive).
- `src/surface/validate.rs`: `is_bare_relative_path` (crate-private).
- Goldens: `tool_schemas.golden.json` and the F18 property set.
- Docs: mcp.mdx (*Writing to a layer* under `lambo_derive`, the five
  tables, `lambo_stats`'s receipt row, the get_info example), config.mdx
  *Layers*, cli.mdx *Layered recall*, each with its site mirror;
  CHANGELOG (Breaking (library API) and Added).

## Decisions, with reasons

- **One resolver, unchanged.** `target` is `resolve_scope` (size, own
  name, then a layer, authorized for the call's grant) plus the live
  lookup. Nothing in the resolver or the grant check changed, so reads
  and writes refuse a scope identically.
- **The layer is looked up once, at the start of the call, and held to
  its end.** A write needs the target's `Memory` for its validation (its
  `derive_embeds`, `match_strategy`, vector search, embedder) before the
  ack, so the lookup comes first. This is also where the rate token is
  drawn (`LayerRead::Direct`, as a request to `/mcp/s/general` draws its
  token at admission, before validation). Held for the ack only: the
  apply runs on the layer's pipeline, so the call is bounded well inside
  `PREVIOUS_HANDLE_WAIT`.
- **Saints and reserve draw the token too.** A scoped `lambo_saints` is a
  direct read of the layer and a scoped `lambo_reserve` a write to it,
  exactly what `/mcp/s/general` would charge. A merged recall, a
  reference probe and a receipt drain draw nothing.
- **`LayerRead::Receipts`, not `Probe`, for the drain and the routing.**
  Same cost rule (no token), different reason, so the variant documents
  it: a layer over its rate must not withhold an outcome the agent is
  owed.
- **The drain and the routing hold the pipeline, never the `Memory`.**
  `lambo_stats` may wait up to 34 s (`RECEIPT_WAIT_MAX`), longer than
  `PREVIOUS_HANDLE_WAIT` (30 s). Holding an `Arc<WritePipeline>` and
  dropping the `LiveLayer` before the wait means a stats wait never
  delays a re-attach of the layer. A receipt of a layer that has since
  been re-attached is answered by the new pipeline if its durable
  intents know it, else `restart_lost` from this session's pipeline,
  which is the truth.
- **One piggyback budget across pipelines.** `MAX_PIGGYBACK_RECEIPTS`
  bounds one response; taking 8 per pipeline would make a response's
  size grow with the number of tiers (N-ready: it loops over the stack).
  This session's receipts come first; the rest wait for the next call,
  and the count says so. For an unlayered session the drain is exactly
  `take_piggyback`, so its output is unchanged.
- **Routing by "knows", not only by epoch.** `knows_receipt` is the
  epoch match or the pipeline's restart map (a previous process's
  durable intents), so a `pending_replay` / `applied_after_restart`
  receipt of a layer is found too. An id no pipeline knows goes to this
  session's pipeline, which answers `restart_lost` or `never_issued` as
  before.
- **Cross-session edges are cross-session references.** Within one write
  every concept is created or matched in the target session (concepts,
  `parent_of` ends, an action's targets by text), so the only way an
  edge could cross sessions is a reference that resolves elsewhere: a
  `depends_on` citation or a hyphenated id. `LayerProbe` now covers every
  session the call can read except the target, nearest first: for a
  write scoped to `general`, this session and then the other layers; for
  an unscoped write, the layers. The refusal names the reference (the id
  that would have been an edge's endpoint) and the session that holds
  it. Each graph is read under its own read lock as a temporary of one
  statement, after the core dropped the target's guard: one session's
  lock at a time, none across an await (#36 PR 4's discipline, kept).
- **The relative-path rule is narrow and lives behind one predicate.**
  `is_bare_relative_path` refuses `README.md`, `src/main.rs`, `./x`,
  `../x`, `dir/`, and lets through prose, ids, branch names
  (`feat/33-pr4`), repo slugs (`nrynss/lambo#147`), domains
  (`api.example.com`), absolute paths and URLs. It misses a directory
  named without a trailing separator (`docs/reference`); a false refusal
  of a branch or a slug in `general` would cost more than that miss.
  Checked on `record_action`'s `produces`, `modifies` and `depends_on` and
  on a derived `resource` concept's content. Not checked: the action
  text (prose by convention), `parent_of` ends and image captions (which
  hold whitespace and a suffix).
- **The seam for PR 7 (#129 part 2).** `Target::refuses_relative_paths`
  is the one predicate, and `Target::check_paths` the one refusal site.
  PR 7 adds a client-supplied repo scope; a call that carries one makes
  `refuses_relative_paths` false (or narrows `check_paths` to entries the
  repo scope does not cover). No other code needs to change.
- **The rule applies to scoped writes, not to writes at
  `/mcp/s/general`.** The design refuses relative paths for
  `scope = "general"`. A server addressed as `general` directly does not
  know it is someone's layer (only layered sessions hold the stack), and
  that path is the operator's (curation, the split tool). Making
  `general`'s own server refuse would need a "this session is a layer"
  flag from the registry; it is a small follow-up if the owner wants it.
- **Lease loss mid-call.** `begin_write` already refuses a fenced or
  closed handle before anything is written. `target_err` turns that
  refusal, when the target is a layer whose `Memory` is now closed or
  fenced, into `layer_lost`: "layer general stopped serving during this
  call (it closed or lost its lease), so nothing was applied to it; retry
  shortly, or call without scope to act on <session>". Never the store's
  lease error, which names operator-only SQL (J1-R2-2). An erased layer
  answers as erased (permanent). Per-session write queues stay separate
  (the layer's own `Memory::pipeline`), and no lock is held across an
  await (the target holds an `Arc` and an in-flight guard, no lock).
- **The ack.** A scoped write's ack keeps the receipt format and adds one
  text line, `written to session 'general'; writes to different sessions
  have no ordering between them`, and `session` in `structuredContent`.
  Scoped `lambo_reserve` and `lambo_saints` name the session the same
  way. Absent for this session, so unscoped answers are unchanged.
- **Ledger.** `scope` is added to a write's, reserve's and saints' facts
  when the call sent one, as on recall and inspect.

## Tests (evidence)

`src/mcp/server/tests/scoped_writes.rs` (13), on the server with a test
`LayerSource`:

| Requirement | Test |
|---|---|
| Each tool scoped to the layer and unscoped | `each_tool_acts_on_its_scope_and_on_this_session_without_one` (derive, record_action, reserve and release with contention, saints), `an_image_derive_is_written_to_its_scope` |
| Unknown scope refused before the ack | `an_unknown_scope_is_refused_before_the_ack_on_every_tool` (all five; no ack, no receipt, both graphs unchanged; unlayered session) |
| Grant refused for an uncovered layer | `a_scoped_write_is_refused_for_a_grant_that_does_not_reach_the_layer` (generic refusal names no session; unscoped write served; drain and routing skip the layer) |
| Receipts from both pipelines drain | `receipts_from_both_pipelines_drain_on_the_next_response` (take-once), `the_piggyback_budget_is_shared_across_pipelines` |
| Stats epoch routing | `lambo_stats_routes_a_receipt_to_its_pipeline_by_epoch` (layer's receipt applied with the layer's ids; own; a foreign id answers `restart_lost`) |
| Rate token | `a_scoped_write_spends_one_layer_token` (one direct read per scoped write, none unscoped; over its rate refused, nothing written) |
| Relative path refused in general | `a_relative_path_is_refused_in_a_layer` (three record_action fields, a resource derive; absolute, prose, slug accepted; unscoped accepted) |
| Cross-session citation refused when scoped, both directions | `a_cross_session_citation_or_edge_is_refused_with_its_id` (project concept cited into general by short id, full id, content; record_action id; general concept cited unscoped; a citation inside general lands) |
| Cross-session edge refused, ids reported | same test: the refusal carries the reference |
| Lease loss mid-call | `losing_the_layers_lease_mid_call_is_a_clear_refusal` (derive, record_action, reserve; no operator SQL; nothing applied) |
| Byte identity, unscoped on an unlayered session | `unscoped_calls_on_an_unlayered_session_are_byte_identical`, plus PR 3's `an_unlayered_recall_matches_the_frozen_v0_5_0_projection` and every pre-existing server test, unchanged |
| Ledger `scope` | `the_ledger_records_a_writes_scope` |

`src/mcp/serve/tests/registry/layered.rs` (2), through the real registry:
`a_scoped_write_lands_in_the_layer_end_to_end` (HTTP: write at the
project's path lands in general, stats routes its receipt, get_info, an
unknown scope) and `a_scoped_write_draws_the_layers_rate` (the real
bucket: one token drawn; receipts, probes and merged reads draw none).

`src/surface/validate.rs`:
`bare_relative_paths_are_told_from_prose_ids_and_absolute_paths`.

## Not done, and why

- `lambo_stats` takes no `scope`: the receipt routes itself, and the
  session figures are the addressed session's (design section 3 lists
  seven tools).
- `general`'s own endpoint does not refuse relative paths (above).
- No change to `Cargo.toml`, `Cargo.lock`, workflows or
  `src/store/pg/postgres/tests.rs`.

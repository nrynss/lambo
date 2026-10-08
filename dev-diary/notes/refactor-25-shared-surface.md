# #25: shared request rules and the MCP tool split (decisions)

Refactor 2/5. Base: #24's head `7e1644f`. Decisions and why; the commits
carry the mechanics.

## A neutral `crate::surface`, not a new CLI submodule

The write queue imported `MAX_CONCEPTS_PER_DERIVE` / `MAX_CONTENT_BYTES` from
`cli::caps`, and both writeq and MCP reached into the CLI or MCP for shared
rules. The rules now live in `crate::surface`, which no surface owns:

| module | holds | visibility |
|---|---|---|
| `surface::limits` | request caps, `clamp_cfg_default` | `pub` |
| `surface::validate` | `check_size`, `require_nonempty`, `check_in_range` (String errors) | `pub` |
| `surface::focus` | `resolve_focus`, `Focus`, scan caps, the four refusal/note builders | crate |
| `surface::neighbourhood` | `render_neighbourhood` | crate |
| `surface::error` | `err_class` (N4), shared by MCP `tool_err` and writeq receipts | crate |

Each surface keeps only its adaptation: `cli::caps` keeps `CliError`, the clap
`ConceptKind` and the `*_cli` wrappers and re-exports the rest (old paths
valid); MCP maps to `bad_param`; the web portal answers with its own status.
"Surface" is the codebase's existing word for the request boundary. Future
session-addressing validation (#32, #4) and the resolve-or-refuse citation
resolver (#36, #19) belong here.

Not moved: the web portal's hop-1 `structural_dependents` (a web DTO, structural
edges only; #28 owns web read projections), and MCP's single-line `agent_id`
rule (MCP-door-only by design, J1).

## Recall no longer imports the daemon

`ScoreTable` and `HotListPayload` are read-side data and moved to
`crate::types` (re-exported at `daemon::ScoreTable`,
`daemon::hotlist::HotListPayload`, `lambo::ScoreTable`). The hot-list
re-validation recall used to run inside `assemble` is daemon maintenance (it
runs detector callbacks and evicts), so it is now
`HotList::revalidate_members`, called from `Daemon::recall_detailed` under the
guards it already held (graph read, then hot write) with the same `now` that
assembly renders with. `assemble` takes the payload map. The T5.3 eviction
assertions moved to `daemon::hotlist::tests::revalidate_members_keeps_live_and_drops_lapsed`.

## MCP server facade

`mcp/server.rs` keeps the handle, constructors, `answered` (trace, panic
containment, receipt delivery) and the `#[tool]` / `#[tool_handler]` blocks
together, so rmcp's macros see exactly what they saw. Parameters,
responses/errors, trace, stats and the tool bodies (one file per tool) moved to
`mcp/server/`. Done as one mechanical commit, verified by a sorted-line diff
(only visibility, imports, and re-pathed doc links differ), rather than five
intermediate states that would each need temporary cross-imports.

## Defects fixed on the way (own commits)

- `scripts/cloudops/_lambo.py`'s empty-session self-test had been failing since
  #9 reshaped the `Focus::Missing` arm; it now pins to `missing_refusal`.
- `INVISIBLE_RANGES` skipped unassigned Default_Ignorable codepoints and the
  Mongolian free variation selectors (key forking). This is a **re-land**, not
  a new fix. It is finding V1 of the t8.2-t8.3 review, fixed on 2026-08-15 by
  `c95a014` (table) and `aac5cd5` (tests). The next commit, `9686b40`
  ("docs(L82): final verify"), silently reverted both: its src tree is
  byte-identical to the pre-R3 base `2ac39f0`, and its src diff is exactly the
  inverse of those two commits (nothing else in it was lost; its only other
  change is the appended verify note). `9ce776e` re-landed the table, and a
  follow-up commit (`0e55e3d`) restored every `aac5cd5` test row.
  `9ce776e`'s message calls the gap "advisory J1-R3-3, left open"; that is
  wrong twice. J1-R3-3 is the `U+2028`/`U+2029` argument, and V1 had been
  closed, then reverted.

  **The key change is one-way for stored data.** Stored concepts keep the keys
  they were written with, and nothing re-keys them on load, so every store
  still loads and the uniqueness check cannot newly collide. But:
  (a) a concept stored with a Mongolian free variation selector keeps its old,
  forked key, so a new derive of the same text computes the stripped key and
  no longer matches it: it creates a new concept or merges into a plain-text
  twin, and the old row is orphaned from matching. This applies to rows
  written before 2026-08-15 and to rows written between `9686b40` and #25.
  (b) Text containing the unassigned codepoints (`U+2065`, `U+FFF0-FFF8`,
  `U+E0080-E00FF`, `U+E01F0-E0FFF`) still loads and renders, but resubmitting
  it is now refused wherever a surface size-checks text (concept and action
  content, inspect focus, recall query, agent and session ids). Measured
  exposure: the orchestrator checked the live dogfood store read-only on
  2026-10-08, and 0 of 3,966 concepts contain any affected codepoint, in
  content or in keys. A re-key migration is needed only if a
  store turns up such rows. Recorded in CHANGELOG.md under Unreleased.
- The `Clock` doc comment was attached to `RecallPipeline`.

## Left for later phases

writeq still pins two budgets against MCP constants in `const` asserts
(`mcp::serve::CLOSE_FLUSH_GRACE`, `mcp::proxy::INFLIGHT_DEPTH_WARN`), and
`memory.rs` holds `mcp::serve::EarlyShutdown`. Compile-time couplings, not
runtime calls; #27/#28 decide which side owns them.

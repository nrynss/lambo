# #22 PR 4: `lambo_derive_image`, `lambo derive-image`, the contract in stats (decisions)

Base: main `932caea` (PR 1 #62, PR 2 #67, PR 3 #71) plus the orchestrator's
`base64 = "0.22"` commit. Design of record:
`lambo-handoff-2026-10-08/design/22-DESIGN.md` (approved 2026-10-09)
sections 3.3 and 6, recommendations 3, 4, 10 and 14, and the PR 4 row of
section 12. The PR 5 amendment (`22-AMENDMENT-PR5.md`, llama.cpp) changes
nothing in this surface. This PR adds the wire surface over PR 3's core:
the MCP tool, the CLI verb, the operator's client-vector opt-in, the
contract in `lambo_stats`, and the three constraints PR 3 carried over.

## What changed

| piece | where |
|---|---|
| `[embedder] accept_client_vectors`, `LAMBO_ACCEPT_CLIENT_VECTORS`, `Config::accept_client_vectors` | `src/embed/mod.rs`, `src/config.rs`, `src/resolve.rs`, `lambo.example.toml` |
| `LamboError::ImageIdTaken`, `surface::error::model_safe_message` | `src/types/mod.rs`, `src/graph/hybrid.rs`, `src/surface/error.rs`, `src/mcp/server/response.rs`, `src/writeq/{receipts,replay}.rs` |
| `decode_base64`, `MAX_IMAGE_B64_LEN`, `check_submitted_vector`, `MAX_VECTOR_VALUES`, `sniff_mime` | `src/surface/image.rs` |
| `check_no_image_suffix`, at `lambo_derive`, `lambo_record_action`, `lambo derive`, `lambo record-action` | `src/surface/validate.rs` and the four callers |
| `embedding_contract`, `embedding_modalities` | `src/mcp/server/stats.rs`, `src/mcp/server/tools/stats.rs` |
| `lambo_derive_image` (params, conditional listing, body) | `src/mcp/server/{params.rs,server.rs}`, `src/mcp/server/tools/derive_image.rs` |
| `lambo derive-image` | `src/cli/derive_image.rs`, `src/main.rs` |
| the tool-list golden | `src/mcp/server/tests/tool_schemas.golden.json` |

## Decisions

**Conditional listing, not the fallback** (design 6.1). rmcp 3.1's
`ToolRouter::remove_route` makes it a one-liner in `LamboServer::new`: the
route is dropped unless the embedder's `modalities()` contain `IMAGE` or
`Config::accept_client_vectors` is on (`image_tool_listed`). Both are
**process** facts: one embedder is resolved per process, and the flag is
process config copied into every session's `Config` at resolve. So every
session one serve holds (#32's registry included) lists the same tools, and
nothing about a session or a caller changes the list. A text-only deployment
lists the seven spec tools, and a call to the unlisted name is rmcp's "tool
not found". `the_router_publishes_exactly_the_seven_spec_tools` now runs
over a text-only embedder, which is what it always meant; the fixture
embedder embeds images since PR 3, so a fixture server lists eight.

**The seven schemas are byte-identical, and pinned.**
`published_tool_schemas_are_pinned_to_the_golden` compares each listed
tool's description and input schema, through `serde_json::to_string`, with
`tool_schemas.golden.json`. Its seven entries were captured from the tree at
`932caea` before any change; the eighth is the new tool. A text-only server
must match exactly the seven, a fixture server all eight.

**`accept_client_vectors` lives in `[embedder]` and reaches the surfaces
through `Config`.** Resolve copies it into `crate::Config`, which every
`Memory` carries, so `LamboServer` and the CLI read
`mem.config().accept_client_vectors` with no plumbing through serve's
session or registry code (`src/mcp/serve/{registry,pinned}.rs`, in review
for #32 PR 4, are untouched). The core never reads it (PR 3 decision). It is
process-wide by design (recommendation 14); a per-credential `vectors = true`
belongs to #32 PR 5's credentials. The overlay accepts exactly `true` or
`false` and its error never quotes the value. It joins `RESOLVE_ENV_VARS`.

**The image-id collision is its own variant** (PR 3 constraint).
`LamboError::ImageIdTaken(image_id)` replaces PR 3's `LamboError::Embed`
for "a text concept already holds this image's canonical key", produced
only by `supplied_match_repair` under the commit lock. Following J1-R2-2, the
N4 exception is a type, so it opens for one producer:
`surface::error::model_safe_message` renders it as `image id taken: ... with
image id "r17" ...; choose another image id`, and every other error keeps
the class sentence byte for byte. `tool_err` (the synchronous path) and
`model_safe_failure` (the receipt) both call it, and the replay arm settles
it `failed` with the same sentence (a fact about the input, like `Embed`).
The id is shown only when it has the published `[a-z0-9]{1,64}` shape. The
old `Embed` message also named the text concept's node id; that now goes
only to an `info` log line. `err_class` gives `"image id taken"`, a new
ledger `error_kind`.

**`[image:` is refused in a concept's own text, not in references**
(PR 3 constraint, scoped). `surface::validate::check_no_image_suffix`
(raw text and canonical tokens, the same rule as an image caption) runs on
`lambo_derive`'s `concepts[].content`, `lambo_record_action`'s `action`,
`lambo derive`'s `--content` and `--concept`, and `lambo record-action
--action`. It does **not** run on `parent_of` ends or on `produces`,
`modifies`, `depends_on`: design 5.2 makes naming an image concept's content
the way a text write links to it, and PR 3 already resolves a `parent_of`
child that names the image. A reference that arrives before its image does
create a text concept with that key, and the image derive is then refused
with `ImageIdTaken`: loud, never a silent image without a vector. The
orchestrator's wording ("other text inputs that become concept content")
would have covered references too; this deviation is argued here and
recorded in the graph.

**The caps run before anything is decoded or embedded.** In order: the
one-of check; caption (non-empty, `check_size`, no `[image:`), image id
(`validate_image_id`, the published pattern `^[a-z0-9]{1,64}$`), `parent_of`
ends; the `hybrid` strategy; then for `image`, an image embedder, the base64
length (`MAX_IMAGE_B64_LEN = 2_796_204`, the padded encoding of the 2 MiB
byte cap, checked **before** decoding because stdio has no transport cap)
and `surface::image::validate`; for `vector`, `accept_client_vectors` and
`check_submitted_vector`. Configuration refusals (`config_refusal`) show
the setting by name: they are Lambo's own text about Lambo's own keys,
unlike a core `Config` error, which can quote a path and still goes through
`tool_err`. A store without vector search is still refused by the core,
through `tool_err`, as a class.

**AC4 at the wire.** `check_submitted_vector` is the model-safe twin of the
core's `check_supplied_values`: the same rules, a message that names which
of `kind`, `model`, `dim` differ and shows the live contract (which
`lambo_stats` publishes), and never the declared strings or a value. MCP
and the CLI both call it before the core, so the core's own message (which
does quote the declared contract) is reached only by library callers.

**Schema choices.** The image concept type is its own enum without
`observation`, since the core refuses an observation image and a published
value the tool always refuses would be a schema that lies. `image.mime` is
a `String` with a published `enum`: a serde enum's unknown-variant error is
built inside rmcp's extractor and quotes the caller's string, and a client
could put its base64 in that slot. `image.data` and `image_id` publish their
own `maxLength` (2,796,204 and 64); `vector.values` publishes `maxItems`
4096 and `contract.dim` a maximum of 4096; a test pins all four to their
constants. Every struct is `deny_unknown_fields`, so a `url` or `path` key
is refused (design 6.4).

**Never on the wire, in a log, a receipt or the ledger.** The bytes live in
the call and are dropped when it returns; the queue and the durable intent
carry the vector (PR 3). The tool's ledger call line carries only
`payload` (`"image"` or `"vector"`), `admitted` and `receipt`: no caption,
no id, no base64, no component (`the_ledger_carries_no_image_payload`).
Every refusal names the field and the rule.

**`lambo_stats`.** `embedding_contract` is the session's stamp, else the
live contract, with `model` whole (the PR 5 amendment's
`artifact;prompts=profile` string, tested with that shape) or `null`;
`embedding_modalities` is the embedder's. Both are in the payload builder
the I2 heartbeat shares, and the text summary ends with an `embedding:`
line. Per session, so nothing leaks across #32's sessions.

**CLI.** `lambo derive-image --session --agent --caption --kind
[--image-id] (--image PATH [--mime M] | --vector-json PATH) [--parent-of]`.
`--kind`, not the design's `--type`, to match `lambo derive`. Files are
size-checked before they are read (2 MiB image, 1 MiB vector file);
`--mime` defaults to the type the magic bytes name; a malformed vector file
is reported by line and column only (serde quotes the offending value). The
output names the image concept's content, so a default digest id is
learned.

## Fixes made on the way

- The schema-maxima guard's leaf walker did not follow `anyOf`, so an
  optional nested object's fields (here `image` and `vector`) were never
  checked. It now does.
- Stale MCP and CLI reference prose, in its own commit (see the commit
  list).

## Tests added

| test | runs in |
|---|---|
| `surface::error::tests::an_image_id_collision_names_the_fix_and_only_the_callers_id` | every row |
| `surface::image::tests::{the_base64_cap_is_the_encoding_of_the_byte_cap, base64_refusals_never_quote_the_input, a_submitted_vector_is_checked_without_echoing_it}` | every row |
| `surface::validate::tests::the_image_suffix_is_refused_in_concept_text_without_quoting_it` | every row |
| `embed::tests::{accept_client_vectors_toml_key, accept_client_vectors_env_overlay}`, `resolve::tests::resolve_carries_accept_client_vectors_into_the_config` | every row (resolve: `store-memory` + `embed-fixture`) |
| `memory::tests::image::{an_image_id_held_by_a_text_concept_is_refused_with_the_fix_on_both_paths, a_replayed_image_intent_whose_id_is_taken_settles_failed_with_the_fix}` | rows with `store-memory` + `embed-fixture` (+ `fixtures` for the replay) |
| `mcp::server::tests::derive_image::*` (10) | rows with `store-memory` + `embed-fixture` |
| `mcp::server::tests::schemas::{the_image_tool_is_listed_only_where_it_can_work, published_tool_schemas_are_pinned_to_the_golden, the_image_tool_schema_publishes_the_runtime_caps}` | same |
| `mcp::server::tests::stats::stats_reports_the_embedding_contract_and_modalities` | same |
| `mcp::server::tests::tools::the_image_suffix_is_refused_in_concept_text_but_not_in_references`, `cli::tests::cli_refuses_an_image_suffix_in_concept_text_like_mcp` | same |
| `cli::derive_image::tests::*` (4) | same |

Changed tests: `graph::hybrid::tests::supplied`'s two collision tests expect
`ImageIdTaken`; the F18 golden property set, the maxima table, the harness
arm list and `unknown_fields_are_refused_by_every_params_struct` cover the
new tool; `i1_every_tool_call_appends_exactly_one_parseable_ledger_line`
calls it; `the_router_publishes_exactly_the_seven_spec_tools` runs over a
text-only embedder. In `tests/binary_parity.rs`,
`mcp_stdio_publishes_exactly_seven_tools_and_refuses_a_client_timestamp` is
renamed
`mcp_stdio_publishes_the_spec_tools_and_the_image_tool_and_refuses_a_client_timestamp`
(the fixture embeds images, so the real binary lists eight) and now drives
an image derive and the base64 cap over real stdio. No CI-grepped name
changed. `VectorSearchable` moved from `memory::tests::replay` to
`crate::test_util`, unchanged.

## Not in this PR

- The EmbeddingGemma 2 adapter over llama.cpp, the live test and the
  ranking-parity evidence: PR 5.
- Recall by image (`lambo_recall` with an image or a query vector): PR 6.
- Per-credential client vectors: #32 PR 5.

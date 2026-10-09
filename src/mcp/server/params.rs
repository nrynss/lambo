//! The tools' parameter schemas (the seven spec tools and, when the deployment
//! can serve it, `lambo_derive_image`), and the `agent_id` door check.
//!
//! Rustdoc on the params types is published verbatim as JSON-Schema
//! `description`s in every `tools/list` (see the internal notes below), so
//! moving them here changed no schema: schemars reads the same doc comments.

use chrono::{DateTime, Utc};
use rmcp::model::CallToolResult;
use serde::Deserialize;

use super::response::bad_param;
use super::LamboServer;
use crate::surface::validate::{check_size as validate_size, require_nonempty};
use crate::types::{AgentId, ConceptType};

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

// ===========================================================================
// INTERNAL NOTES — deliberately `//` and not `///`.
//
// Everything in this module's rustdoc on a params struct, field, or enum is
// published VERBATIM as the JSON-Schema `description` in every `tools/list`
// response, so it is read by every MCP client and every model. Review markers,
// dependency internals and "revisit if…" notes are not wire copy (T88-H1).
// Keep engineering rationale here; keep the rustdoc user-facing.
//
// Why this mirrors `ConceptType` instead of deriving `JsonSchema` on the core
// type: the MCP schema is owned here, so a core rename cannot silently change
// a published tool schema.
//
// Byte-echo note (R4 nit): an invalid value here yields serde's `unknown
// variant \`…\`` error, which repeats the caller's decoded string — potentially
// a decoded control char such as `U+0001` — back to the model, unlike
// `validate_size`, which names control codepoints instead of echoing them. This
// is **not** interceptable at our layer: every tool takes its params through
// rmcp's `Parameters<T>` extractor, so the variant error is built and returned
// (as a `-32602`) inside the rmcp framework, before any `LamboServer` code runs.
// Sanitising it would mean abandoning `Parameters<T>` for a hand-rolled
// deserialize in all seven tools — a large, error-prone change for a field whose
// only reachable "byte" is an escaped control char in an enum slot. Left as-is;
// revisit if rmcp grows an extraction-error hook.
// ===========================================================================

/// What kind of thing a concept is. Pick the one that fits the content best:
///
/// - `entity` — a named thing: a person, service, file, table, or component.
/// - `logic` — a rule, decision, or piece of reasoning about how things work.
/// - `constraint` — a requirement or limit that must keep holding.
/// - `resource` — something produced, consumed, or acted on by the work.
/// - `observation` — something noticed in passing; the weakest kind, and
///   the only one that can later be demoted.
#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WireConceptType {
    Entity,
    Logic,
    Constraint,
    Resource,
    Observation,
}

impl From<WireConceptType> for ConceptType {
    fn from(w: WireConceptType) -> Self {
        match w {
            WireConceptType::Entity => ConceptType::Entity,
            WireConceptType::Logic => ConceptType::Logic,
            WireConceptType::Constraint => ConceptType::Constraint,
            WireConceptType::Resource => ConceptType::Resource,
            WireConceptType::Observation => ConceptType::Observation,
        }
    }
}

/// `Default` is for library callers that build one with a struct literal:
/// `RecallParams { agent_id, query, ..Default::default() }` keeps compiling
/// when an optional field is added (as `image` and `query_vector` were, #22
/// PR 6). It is not a wire default: `agent_id` is still required, and an
/// empty `query` alone is still refused.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecallParams {
    /// Id of the agent making this call. Caller-asserted and unverified: work
    /// is recorded under exactly the id you send. Use one stable id per agent —
    /// callers sharing an id share its memory attribution and its soft locks.
    #[schemars(length(max = 16_384))]
    pub agent_id: String,
    /// Natural-language query. Required, unless you send `image` or
    /// `query_vector`; beside either it is optional and its words still
    /// match concept text.
    #[schemars(length(max = 16_384))]
    #[serde(default)]
    pub query: String,
    /// Hits to return. Defaults to the session config's `default_top_k`.
    #[schemars(range(min = 1, max = 100))]
    pub top_k: Option<usize>,
    /// Token budget for the rendered context block.
    #[schemars(range(min = 1, max = 100_000))]
    pub max_tokens: Option<usize>,
    /// Graph traversal depth for phase 2 expansion.
    #[schemars(range(min = 0, max = 5))]
    pub traversal_depth: Option<usize>,
    /// Optional: recall what is close to this image, for this server to
    /// embed (when its embedder embeds images). Send at most one of `image`
    /// and `query_vector`. Not stored.
    pub image: Option<WireImage>,
    /// Optional: recall what is close to a vector you computed, in this
    /// session's embedding space (accepted only when the operator enabled
    /// client vectors). Send at most one of `image` and `query_vector`. Not
    /// stored.
    pub query_vector: Option<WireQueryVector>,
}

/// A query vector you computed, in this session's embedding space.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireQueryVector {
    /// The components, `dim` of them. Lambo normalizes the vector to unit
    /// length.
    #[schemars(length(max = 4_096))]
    pub values: Vec<f32>,
    /// The embedding space the vector was computed in. It must equal this
    /// session's `embedding_contract` exactly.
    pub contract: WireEmbeddingContract,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireConcept {
    /// The concept text.
    #[schemars(length(max = 16_384))]
    pub content: String,
    /// One of `entity`, `logic`, `constraint`, `resource`, `observation`.
    pub concept_type: WireConceptType,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireParentOf {
    #[schemars(length(max = 16_384))]
    pub parent: String,
    #[schemars(length(max = 16_384))]
    pub child: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeriveParams {
    /// Id of the agent making this call. Caller-asserted and unverified: work
    /// is recorded under exactly the id you send. Use one stable id per agent —
    /// callers sharing an id share its memory attribution and its soft locks.
    #[schemars(length(max = 16_384))]
    pub agent_id: String,
    /// Concepts to derive from this interaction.
    pub concepts: Vec<WireConcept>,
    /// Optional RFC3339 historical about-time for this evidence, such as a
    /// commit or document date. Omit it for a live fact, which is about now.
    /// No additional date-range bounds are applied.
    #[schemars(length(max = 16_384))]
    pub event_time: Option<DateTime<Utc>>,
    /// Optional `(parent, child)` hierarchy pairs. Both ends resolve (and may
    /// be created) as concepts.
    pub parent_of: Option<Vec<WireParentOf>>,
}
/// One entry in a `lambo_record_action` resource list (`produces`,
/// `modifies`, `depends_on`). A plain string on the wire, with the same
/// per-string size cap the runtime enforces, so a client can pre-validate an
/// entry without a round trip.
#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
pub struct WireResource(#[schemars(length(max = 16_384))] pub String);

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordActionParams {
    /// Id of the agent making this call. Caller-asserted and unverified: work
    /// is recorded under exactly the id you send. Use one stable id per agent —
    /// callers sharing an id share its memory attribution and its soft locks.
    #[schemars(length(max = 16_384))]
    pub agent_id: String,
    /// The action taken — becomes a `Resource` concept.
    #[schemars(length(max = 16_384))]
    pub action: String,
    /// Optional RFC3339 historical about-time for this evidence, such as a
    /// commit or document date. Omit it for a live fact, which is about now.
    /// No additional date-range bounds are applied.
    #[schemars(length(max = 16_384))]
    pub event_time: Option<DateTime<Utc>>,
    /// Resources this action creates (`Causal` edges).
    pub produces: Option<Vec<WireResource>>,
    /// Resources this action mutates (`Causal` edges).
    pub modifies: Option<Vec<WireResource>>,
    /// Things this action depends on (`Dependency` edges).
    pub depends_on: Option<Vec<WireResource>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReserveParams {
    /// Id of the agent making this call — the identity the lock is held under.
    /// Caller-asserted and unverified: locks are cooperative. A distinct id
    /// gets a distinct lock; two callers sending the SAME id share one lock and
    /// can release each other's. Use one stable id per agent.
    #[schemars(length(max = 16_384))]
    pub agent_id: String,
    /// Node to reserve, as a UUID string (from `lambo_recall` or
    /// `lambo_inspect`).
    #[schemars(length(max = 16_384))]
    pub node_id: String,
    /// Soft-lock lifetime in seconds (default 30, max 3600).
    #[schemars(range(min = 1, max = 3_600))]
    pub ttl_seconds: Option<u64>,
    /// Release this agent's existing soft lock instead of taking one.
    pub release: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectParams {
    /// Id of the agent making this call. Caller-asserted and unverified: work
    /// is recorded under exactly the id you send. Use one stable id per agent —
    /// callers sharing an id share its memory attribution and its soft locks.
    #[schemars(length(max = 16_384))]
    pub agent_id: String,
    /// Concept content, a node UUID, or the short-form id rendered in recall
    /// blocks, to centre the neighbourhood on.
    #[schemars(length(max = 16_384))]
    pub focus: String,
    /// Hops out from the focus (default 2, max 5).
    #[schemars(range(min = 0, max = 5))]
    pub depth: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SaintsParams {
    /// Id of the agent making this call. Caller-asserted and unverified: work
    /// is recorded under exactly the id you send. Use one stable id per agent —
    /// callers sharing an id share its memory attribution and its soft locks.
    #[schemars(length(max = 16_384))]
    pub agent_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatsParams {
    /// Id of the agent making this call. Caller-asserted and unverified: work
    /// is recorded under exactly the id you send. Use one stable id per agent —
    /// callers sharing an id share its memory attribution and its soft locks.
    #[schemars(length(max = 16_384))]
    pub agent_id: String,
    /// A write receipt id from a `lambo_derive` or `lambo_record_action` ack.
    /// Answers what happened to that one write: applied, failed, dropped,
    /// pending, expired, restart-lost or never-issued. Receipts are scoped to
    /// the agent that created them.
    #[schemars(length(max = 16_384))]
    pub receipt: Option<String>,
    /// With `receipt`, wait up to this many milliseconds for the write to be
    /// applied before answering — the opt-in synchrony that restores
    /// read-your-writes when you need it. Clamped to the server's own maximum.
    /// Ignored without `receipt`.
    ///
    /// The published maximum is [`crate::writeq::RECEIPT_WAIT_MAX`] in
    /// milliseconds — a client that sends more is clamped to it rather than
    /// refused, and `the_published_wait_maximum_is_the_real_one` pins the
    /// literal below to the constant (T88-H4 requires a *published* maximum,
    /// and `schemars` takes a literal).
    #[schemars(range(min = 0, max = 34_000))]
    pub wait_ms: Option<u64>,
}

// ===========================================================================
// INTERNAL NOTES for `lambo_derive_image` (#22 PR 4) — `//`, not wire copy.
//
// The image concept's type omits `observation` (WireImageConceptType): the
// core refuses an observation image (observations never canonical-match, so
// the same image would duplicate), and publishing a value the tool always
// refuses would be a schema that lies.
//
// `mime` is a `String` with a published enum rather than a serde enum: a
// serde enum's unknown-variant error is built inside rmcp's extractor and
// quotes the caller's string back (see the byte-echo note above). As a
// `String` the value reaches `surface::image::validate`, whose refusal never
// quotes it.
//
// That closes one slot, not the class (review L3). Every refusal *our* code
// builds for this tool names the field and the rule and never quotes a
// value, but serde's type and variant errors, built inside rmcp's
// extractor before any Lambo code runs, still quote what the caller sent
// in a wrongly typed slot: `concept_type: "<anything>"` (unknown variant),
// `image: "<base64>"` or `vector: "<text>"` (invalid type: string), a
// string in `vector.values` or `contract.dim`. The echo goes back only to
// the caller that sent it, so nothing crosses sessions or reaches the
// ledger; it is the same known residual as the byte-echo note above, with
// the same revisit trigger (an rmcp extraction-error hook).
//
// `image.data`, `image_id` and `caption` publish their own `maxLength` (the
// base64 form of the 2 MiB byte cap, 64, and `surface::image::
// MAX_CAPTION_BYTES`: the uniform 16384 less the shortest suffix), not the
// uniform 16384; the maxima test lists all three as named exceptions.
// `vector.values`' `maxItems` and `contract.dim`'s maximum are
// `surface::image::MAX_VECTOR_VALUES`; schemars takes literals, so
// `the_image_tool_schema_publishes_the_runtime_caps` pins them to the
// constants.
// ===========================================================================

/// What kind of thing the image is. One of `entity`, `logic`, `constraint`,
/// `resource` (an image cannot be an `observation`).
#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WireImageConceptType {
    Entity,
    Logic,
    Constraint,
    Resource,
}

impl From<WireImageConceptType> for ConceptType {
    fn from(w: WireImageConceptType) -> Self {
        match w {
            WireImageConceptType::Entity => ConceptType::Entity,
            WireImageConceptType::Logic => ConceptType::Logic,
            WireImageConceptType::Constraint => ConceptType::Constraint,
            WireImageConceptType::Resource => ConceptType::Resource,
        }
    }
}

/// One image, inline.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireImage {
    /// The image's media type, which must match its bytes: `image/png`,
    /// `image/jpeg` or `image/webp`.
    #[schemars(extend("enum" = ["image/png", "image/jpeg", "image/webp"]))]
    pub mime: String,
    /// The image bytes in base64 (standard alphabet, padded), on one line,
    /// with no `data:` prefix. At most 2 MiB decoded, and at most 4096 pixels
    /// a side.
    #[schemars(length(max = 2_796_204))]
    pub data: String,
}

/// An embedding space: copy it from `lambo_stats`' `embedding_contract`.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireEmbeddingContract {
    /// Embedder kind, e.g. `embeddinggemma2`.
    #[schemars(length(max = 16_384))]
    pub kind: String,
    /// Model string, exactly as `lambo_stats` reports it (omit or null for
    /// the server default).
    #[schemars(length(max = 16_384))]
    pub model: Option<String>,
    /// Vector width.
    #[schemars(range(min = 1, max = 4_096))]
    pub dim: usize,
}

/// A vector you computed for the image, in this session's embedding space.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireVector {
    /// The components, `dim` of them. Lambo normalizes the vector to unit
    /// length.
    #[schemars(length(max = 4_096))]
    pub values: Vec<f32>,
    /// The embedding space the vector was computed in. It must equal this
    /// session's `embedding_contract` exactly.
    pub contract: WireEmbeddingContract,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeriveImageParams {
    /// Id of the agent making this call. Caller-asserted and unverified: work
    /// is recorded under exactly the id you send. Use one stable id per agent —
    /// callers sharing an id share its memory attribution and its soft locks.
    #[schemars(length(max = 16_384))]
    pub agent_id: String,
    /// What the image is, in words. It is the concept's text: keyword recall
    /// and the recall display read it. It may not contain an image suffix
    /// of its own (`[image:<id>]`). With the suffix Lambo appends, the
    /// content must fit in 16384 bytes: a caption of at most 16375 bytes less
    /// the image id's length (16359 with the default 16-character id).
    #[schemars(length(max = 16_374))]
    pub caption: String,
    /// One of `entity`, `logic`, `constraint`, `resource`.
    pub concept_type: WireImageConceptType,
    /// Your id for this image: 1 to 64 lowercase letters and digits. The
    /// concept's content is the caption plus `[image:<image_id>]`; the same
    /// caption and id derived again is the same concept. Omit it for an id
    /// made from a digest of what you send: the image's bytes, or the
    /// normalized vector.
    #[schemars(length(max = 64), regex(pattern = r"^[a-z0-9]{1,64}$"))]
    pub image_id: Option<String>,
    /// The image itself, for this server to embed. Send exactly one of
    /// `image` and `vector`.
    pub image: Option<WireImage>,
    /// A vector you computed for the image instead (accepted only when the
    /// operator enabled client vectors). Send exactly one of `image` and
    /// `vector`.
    pub vector: Option<WireVector>,
    /// Optional RFC3339 historical about-time for this evidence, such as a
    /// commit or document date. Omit it for a live fact, which is about now.
    /// No additional date-range bounds are applied.
    #[schemars(length(max = 16_384))]
    pub event_time: Option<DateTime<Utc>>,
    /// Optional `(parent, child)` hierarchy pairs. Both ends resolve (and may
    /// be created) as concepts; name the image concept by its content,
    /// `caption [image:<image_id>]`.
    pub parent_of: Option<Vec<WireParentOf>>,
}

// Redacting `Debug` for the image tool's params (review L4): the base64,
// the vector, the caption and the client's `mime`, `image_id` and contract
// strings are
// user data, so a future `?p` in a log line shows their sizes only, as PR
// 3's `ImagePayload` does. Not wire copy (no `///`): a `Debug` impl is not
// in the schema either way.
impl std::fmt::Debug for WireImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireImage")
            .field("mime_len", &self.mime.len())
            .field("data_len", &self.data.len())
            .finish()
    }
}

impl std::fmt::Debug for WireEmbeddingContract {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireEmbeddingContract")
            .field("kind_len", &self.kind.len())
            .field("model_len", &self.model.as_ref().map(String::len))
            .field("dim", &self.dim)
            .finish()
    }
}

impl std::fmt::Debug for WireVector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireVector")
            .field("len", &self.values.len())
            .field("contract", &self.contract)
            .finish()
    }
}

impl std::fmt::Debug for WireQueryVector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireQueryVector")
            .field("len", &self.values.len())
            .field("contract", &self.contract)
            .finish()
    }
}

impl std::fmt::Debug for DeriveImageParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeriveImageParams")
            .field("agent_id", &self.agent_id)
            .field("caption_len", &self.caption.len())
            .field("concept_type", &self.concept_type)
            .field("image_id_len", &self.image_id.as_ref().map(String::len))
            .field("image", &self.image)
            .field("vector", &self.vector)
            .field("event_time", &self.event_time)
            .field("parent_of", &self.parent_of.as_ref().map(Vec::len))
            .finish()
    }
}

/// Door-side cap on a caller-asserted `agent_id`, in characters (J1, operator
/// ruling 2026-08-20). Deliberately far below the uniform `MAX_CONTENT_BYTES`:
/// an id is a name other agents read, and the recall budget drops whole
/// blocks, so an id near the uniform cap can evict the block it annotates
/// from another agent's context. 256 is generous for any real client id, and
/// bounds — but does not eliminate — that eviction: see the measurement in
/// [`LamboServer::check_agent_id`]. Applies only at this door — `--agent` and
/// `AgentId` itself stay uncapped (trusted, process-side).
pub(super) const MAX_AGENT_ID_CHARS: usize = 256;

/// `true` for a character that would break the promise that a string is **one
/// field on one line** (J1-R2-1).
///
/// Stated as a *class* rather than as the literal characters a review happened
/// to name, because a list of literals rots and a class does not:
///
/// * [`char::is_control`] is exactly the `Cc` general category — every C0/C1
///   control, so `\n`, `\r`, `\t`, `U+000B`, `U+000C` and `U+0085` all land
///   here. Round 1 prescribed the three literals `\n`/`\r`/`\t`; naming the
///   category instead means this rule stays complete if `check_size`'s
///   exception table (which passes `\n` and `\t`, both legitimate inside a
///   concept's `content`) is ever widened again.
/// * `U+2028` and `U+2029` are the *only* members of `Zl` and `Zp` — the two
///   general categories whose entire semantic is "line break". They are not
///   `Cc`, so `is_control` misses them; they are absent from
///   `graph::canonical::INVISIBLE_RANGES`, so `check_size` misses them too. In
///   CSS text layout they are *forced* line and paragraph breaks, and
///   `cli::serve_web` serves the recall context block verbatim into a page —
///   so there the forged break becomes a real one, while a terminal shows
///   nothing at all. They are written out rather than tested by property
///   because this crate has no Unicode-category dependency and will not grow
///   one for two codepoints; the honest spelling of this whole predicate, if
///   one ever arrives, is `general_category(c) ∈ {Cc, Zl, Zp}`.
///
/// Two neighbouring rules were considered and rejected. **Any `White_Space`
/// character** is too wide: an ordinary space must stay legal, since ids are
/// taken untrimmed and `"a"` and `"a "` are deliberately two agents
/// ([`LamboServer::caller_agent`]). **Unicode line-break classes
/// `BK`/`CR`/`LF`/`NL`** — the review's alternative — is too narrow *and* needs
/// a table: that set is `{U+000B, U+000C, U+2028, U+2029} ∪ {CR, LF, U+0085}`,
/// a strict subset of what the two arms below already give, and it would also
/// drop `\t`, which forges a column rather than a line but is refused for the
/// same reason. Anything merely *invisible* stays `check_size`'s business
/// (`INVISIBLE_RANGES`, which runs first and names the codepoint it refuses);
/// this predicate answers one question only — does this character forge a line
/// or a column — so widening it would duplicate a table that already exists
/// and then drift from it.
pub(super) fn breaks_one_line(c: char) -> bool {
    c.is_control() || c == '\u{2028}' || c == '\u{2029}'
}

/// Shared [`validate_size`] mapped into a tool-level error. The check itself
/// lives in [`crate::surface::validate`] so CLI and MCP cannot drift.
pub(super) fn check_size(field: &str, value: &str) -> Result<(), CallToolResult> {
    validate_size(field, value).map_err(bad_param)
}

impl LamboServer {
    /// Validate the caller-asserted `agent_id` (J1).
    ///
    /// Every tool carries `agent_id` because spec §6.2/§2.2 says calls from
    /// several MCP clients are tasks in one process, each identifying itself.
    /// Since J1 that id is **honoured**: write tools stamp it on the
    /// interaction and contend on it for soft locks, via `Memory`'s `_as`
    /// surface. There is no attribution gap left to warn about, so this checks
    /// shape only — non-empty, within the uniform size cap, and **renderable
    /// as one field on one line** (J1-R1-1, below).
    ///
    /// **The id is caller-asserted and unauthenticated.** Over stdio the client
    /// owns the process; over HTTP one bearer token authenticates the server,
    /// not each agent. So identity here is a cooperative declaration, exactly
    /// like the soft locks it drives (spec §11: advisory, RAM-only). Distinct
    /// ids get distinct locks; callers sharing an id share locks knowingly. The
    /// compensating control is that this is *said out loud* — in every
    /// `agent_id` param description, in `lambo_reserve`'s tool doc, and in the
    /// server instructions — not silently assumed.
    pub(super) fn check_agent_id(&self, agent_id: &str) -> Result<(), CallToolResult> {
        require_nonempty("agent_id", agent_id).map_err(bad_param)?;
        check_size("agent_id", agent_id)?;
        // J1-R1-1. `check_size` allows `\n` and `\t` on purpose, because both
        // are legitimate inside a concept's `content` — but this id is not
        // content. Since J1 it is rendered **verbatim into the T5.3 context
        // block another agent reads**, by two renderers that do not sanitise:
        // as the soft-lock holder (`recall::format::reservation_warning`, via
        // `recall::assemble`) and as the §13 conflict sentence's writer
        // (`recall::format::conflict_warning`, whose `agent_display` only
        // strips a prefix and capitalises — and which needs no lock at all,
        // just one `lambo_derive`). So a line break lets one client write whole
        // lines into every *other* agent's context in Lambo's own `⚑ CANONICAL`
        // vocabulary, and a tab lets it distort how that block renders.
        // Refusing them here means an id that reaches the graph is always
        // renderable as one field on one line — and the rule is stated as a
        // character *class* ([`breaks_one_line`]), not as the three literals
        // round 1 prescribed, because that list was incomplete the day it was
        // written: U+2028/U+2029 are Zl/Zp, so they are neither controls nor
        // members of `INVISIBLE_RANGES`, and they slipped every layer (J1-R2-1).
        // The same predicate folds `conflict_err`'s message, so the two cannot
        // drift apart again. (`\r` and the other C0/C1 controls are already
        // refused upstream by `check_size`; the class covers them here anyway so
        // this rule reads complete and survives a change to that exception
        // table.)
        //
        // **Why the door and not `AgentId::new`.** The type is also constructed
        // from the operator's own `--agent` by the CLI and by library callers —
        // trusted input on the same side of the boundary as the process itself —
        // so tightening the *type* would change its semantics for every caller,
        // which is not J1's to do. This function is the single place where an
        // unauthenticated, remote string becomes a write identity and a lock
        // name, which makes it the place the renderability requirement belongs.
        //
        // Length IS tightened here, by operator ruling (2026-08-20, closing
        // the question round-1 remediation declared): an id is a *name*, and
        // because the recall budget drops whole blocks, a holder id at the
        // uniform 16 KiB cap can evict the very block it annotates from
        // another agent's context — denial-of-context rather than injection.
        // 256 chars is generous for any real client id and reduces that vector
        // by ~64× at the same door as the single-line guard. It does not close
        // it: measured (J1-R2-3), a 256-char holder still evicts the block it
        // annotates below ~160 `max_tokens`, and the reservation line renders
        // outside the budget entirely. The remainder is a rendering-side
        // question, carried as a §J2 residual in
        // `dev-diary/lambo-for-mooshik/J-multi-client.md` — not closed here.
        // The divergence from
        // `--agent` and from `AgentId` (both uncapped) is deliberate: this
        // door is where unauthenticated remote identity is policed; trusted
        // process-side callers keep the type's semantics.
        if let Some(c) = agent_id.chars().find(|c| breaks_one_line(*c)) {
            return Err(bad_param(format!(
                "agent_id must be a single line with no tabs, control or line-separator \
                 characters (found U+{:04X}); it is rendered into other agents' recall \
                 context as the holder of your soft locks — send a one-line id such as \
                 'agent-b'",
                c as u32
            )));
        }
        if agent_id.chars().count() > MAX_AGENT_ID_CHARS {
            return Err(bad_param(format!(
                "agent_id must be at most {MAX_AGENT_ID_CHARS} characters (got {}); it is a \
                 name other agents read, not content — send a short id such as 'agent-b'",
                agent_id.chars().count()
            )));
        }
        Ok(())
    }

    /// [`LamboServer::check_agent_id`], returning the acting [`AgentId`] for the
    /// write path to stamp.
    ///
    /// The id is taken untrimmed and verbatim, so `"a"` and `"a "` are two
    /// agents holding two locks. Normalising here would silently merge two
    /// callers' locks — the one failure mode J1's whole design is arranged to
    /// avoid — so the mismatch is left visible to the caller instead.
    pub(super) fn caller_agent(&self, agent_id: &str) -> Result<AgentId, CallToolResult> {
        self.check_agent_id(agent_id)?;
        Ok(AgentId::new(agent_id))
    }
}

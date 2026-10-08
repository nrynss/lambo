//! The seven MCP tools of `lambo serve` (spec §6.2).
//!
//! One process owns the session (spec §2.2); every tool call is a task inside
//! it. [`LamboServer`] is a cheap handle — cloning it clones an `Arc<Memory>`,
//! never a second [`Memory`] (a second one would spawn a rival task trio
//! against a divergent RAM copy of the same session).
//!
//! # F18 — flush timestamps are server-side
//!
//! `created_at` remains server-stamped for every tool call. `lambo_derive` and
//! `lambo_record_action` additionally accept an optional `event_time`: the
//! historical about-time of a fact, such as its commit or document date. It is
//! not an observed-at claim and therefore does not weaken the server's flush
//! timestamp authority. No other client-supplied time surface is accepted.
//!
//! # Error convention
//!
//! Per rmcp's own guidance: `Err(ErrorData)` is for requests the server cannot
//! route (the client renders those opaquely, so the message never reaches the
//! user); `Ok(CallToolResult::error(..))` is for "the tool ran and it did not
//! work", whose content the caller actually sees. Memory-level failures —
//! conflicts, unknown nodes, a closed session — are the latter.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde::Deserialize;
use serde_json::json;

use crate::cli::caps::{
    check_size as validate_size, clamp_cfg_default, MAX_ACTION_TARGETS, MAX_CONCEPTS_PER_DERIVE,
    MAX_INSPECT_CANDIDATES, MAX_INSPECT_DEPTH, MAX_MAX_TOKENS, MAX_RESERVE_TTL_SECS, MAX_TOP_K,
    MAX_TRAVERSAL_DEPTH,
};
use crate::cli::inspect::{
    render_neighbourhood, resolve_focus, Focus, MAX_INSPECT_BOUNDED_SCAN, MAX_INSPECT_SCAN_CONCEPTS,
};
use crate::graph::action::Action;
use crate::graph::derive::ParentOf;
use crate::ledger::Ledger;
use crate::memory::Memory;
use crate::recall::detail::AnnotationKind;
use crate::store::flush::{panic_message, CatchUnwindPoll};
use crate::types::{AgentId, ConceptType, LamboError, NodeId, RecallQuery, RecallResult};
use crate::writeq::{ReceiptAnswer, ReceiptId};

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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecallParams {
    /// Id of the agent making this call. Caller-asserted and unverified: work
    /// is recorded under exactly the id you send. Use one stable id per agent —
    /// callers sharing an id share its memory attribution and its soft locks.
    #[schemars(length(max = 16_384))]
    pub agent_id: String,
    /// Natural-language query.
    #[schemars(length(max = 16_384))]
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
    #[schemars(range(min = 0, max = 4_000))]
    pub wait_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// MCP surface over one [`Memory`].
#[derive(Clone)]
pub struct LamboServer {
    mem: Arc<Memory>,
    tool_router: ToolRouter<Self>,
    /// I1 call ledger, `None` unless `serve --ledger` named a path.
    ///
    /// `None` is the whole of "off": no scope is established, so no facts are
    /// built, no timestamps are taken beyond the one `Instant` every call
    /// already affords, and `lambo_stats` emits exactly the payload it emitted
    /// before this field existed.
    ledger: Option<Arc<Ledger>>,
    /// When this process's server handle was created — the heartbeat's uptime.
    started_at: Instant,
}

impl std::fmt::Debug for LamboServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LamboServer")
            .field("session", self.mem.session())
            .field("agent", self.mem.agent())
            .field("ledger", &self.ledger.as_ref().map(|l| l.path()))
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// I1 — the per-call trace slot
// ---------------------------------------------------------------------------

/// What a tool body tells the ledger about the call it just served.
///
/// The alternative was changing every `*_impl` signature to return facts
/// alongside its [`CallToolResult`], which would have rippled into the tool
/// bodies, the wrapper block, and every test that calls an `_impl` directly —
/// a lot of churn for a feature that is off by default. A task-local slot keeps
/// the whole mechanism in this file and, more importantly, makes "off" cost
/// **nothing**: with no ledger the scope is never established, so
/// [`note_facts`]'s closure never runs and no per-tool JSON is ever built.
#[derive(Default)]
struct CallTrace {
    /// Set by [`bad_param`] / [`tool_err`] / [`contain_panic`] on their way out.
    error_kind: Option<&'static str>,
    /// Per-tool payload facts, merged into the ledger line at the top level.
    facts: Option<serde_json::Value>,
}

tokio::task_local! {
    /// Established by [`LamboServer::observed`] for the duration of one tool
    /// call, and only when a ledger is listening.
    static TRACE: std::cell::RefCell<CallTrace>;
}

/// The I1 `lambo_recall` payload facts: the query, the top-k hits with **final
/// and per-leg** scores, and which typed warnings actually rendered.
///
/// Every flag here is derived from a **typed producer**, never from matching
/// text against the rendered context — the H3 annotation kinds
/// ([`AnnotationKind`]) exist precisely so a consumer never has to parse
/// `⚑`. That matters for DOGFOOD metric 5: "a blast-radius warning fired" has
/// to be a fact, not a grep.
///
/// `legs` carries only the legs that produced the hit, so a `0.35` from the
/// recency floor is distinguishable from a genuine `0.35` cosine — the
/// distinction G1's score bands are meaningless without. An **empty** `legs`
/// object means the hit was not a phase-1 candidate at all: it arrived through
/// phase-2 traversal expansion (or the query was answered by structural
/// dispatch, which skips the blend).
///
/// `score` and `legs` describe different stages and are not expected to agree:
/// `score` is the FINAL ranking score (phase-3 assembly, daemon score table and
/// `RecallWeights` applied), while `legs` are the raw phase-1 retrieval inputs.
/// A consumer banding cosines wants `legs.vector_cosine`; one asking "what did
/// this rank at" wants `score`.
///
/// Concept text is carried (truncated to [`LEDGER_CONTENT_PREFIX`]) because the
/// I1 hygiene rule allows it — the text already lives in the store — and because
/// it is what makes `warnings.py` able to say *which concept* a blast-radius
/// warning fired over without a store join. It is truncated rather than whole
/// so one recall of `MAX_TOP_K` long concepts cannot turn into a megabyte line.
///
/// # What the five set-level flags mean, and why they are not all computed alike
///
/// The spec's word is **rendered**, and the honest answer differs by kind
/// because the two rendering paths differ:
///
/// * **`canonical_marker` is budget-gated.** `[canonical]` exists *only* inside a
///   hit's context block (`format::render_block`), and the block is emitted only
///   while the token budget lasts. So this flag counts hits with
///   `included_in_context == true` and nothing else. A `max_tokens: 1` recall of
///   a Canonical concept therefore reports `canonical_marker: false` — correctly:
///   the agent received an empty context and the string `[canonical]` appeared
///   nowhere in the response. (Per-hit `is_canonical` is still on every hit, so
///   "was a Canonical concept *returned*" remains answerable — from the hits,
///   which is where a set-level flag cannot honestly answer it.)
/// * **The four warning flags are budget-independent** —
///   `blast_radius_warning`, `conflict_line`, `hot_warning`,
///   `reservation_warning`. Their lines are pushed into the flat `warnings`
///   vector for *every* returned hit regardless of the budget
///   (`assemble.rs`: "a block truncated from the context still reports its
///   conditions") and delivered to the agent as a second text block. The line
///   reached the agent even when the block did not, so these are computed over
///   every returned hit. Per-hit `included_in_context` is what tells a consumer
///   whether the *block* the warning was about was also there — which is the
///   distinction `warnings.py` reports.
fn recall_facts(
    query: &str,
    top_k: usize,
    detailed: &crate::recall::detail::DetailedRecall,
) -> serde_json::Value {
    let mut canonical_marker = false;
    let mut blast_radius_warning = false;
    let mut conflict_line = false;
    let mut hot_warning = false;
    let mut reservation_warning = false;

    let hits: Vec<serde_json::Value> = detailed
        .hits
        .iter()
        .zip(detailed.detailed.iter())
        .map(|(hit, d)| {
            // Budget-gated: `[canonical]` renders inside the hit's block, so a
            // hit the budget cut rendered no marker. See this function's docs.
            canonical_marker |= hit.is_canonical && d.included_in_context;
            let mut legs = serde_json::Map::new();
            if let Some(l) = detailed.legs.get(&hit.node_id) {
                if let Some(s) = l.keyword {
                    legs.insert("bm25".into(), json!(s));
                }
                if let Some(s) = l.recent {
                    legs.insert("recent".into(), json!(s));
                }
                if let Some(s) = l.vector {
                    legs.insert("vector_cosine".into(), json!(s));
                }
            }
            let mut kinds: Vec<&'static str> = Vec::new();
            // Deliberately NOT gated on `included_in_context`: these four lines
            // go into the flat `warnings` vector for every returned hit and
            // reach the agent as a second text block whatever the budget did to
            // the block itself. See this function's docs.
            for a in &d.annotations {
                match a.kind {
                    AnnotationKind::LoadBearing => {
                        blast_radius_warning = true;
                        kinds.push("load_bearing");
                    }
                    AnnotationKind::Conflict => {
                        conflict_line = true;
                        kinds.push("conflict");
                    }
                    AnnotationKind::Hot => {
                        hot_warning = true;
                        kinds.push("hot");
                    }
                    AnnotationKind::Reservation => {
                        reservation_warning = true;
                        kinds.push("reservation");
                    }
                    // Response-global kinds are never hit-owned (H3 contract),
                    // so they are reported once, below.
                    AnnotationKind::Traversal | AnnotationKind::VectorDegraded => {}
                }
            }
            json!({
                "node_id": hit.node_id.0.to_string(),
                "content": truncate_for_ledger(&hit.content),
                "score": hit.score,
                "legs": legs,
                "is_canonical": hit.is_canonical,
                "blast_radius": hit.blast_radius,
                "included_in_context": d.included_in_context,
                "annotations": kinds,
            })
        })
        .collect();

    let response_kinds: Vec<&'static str> = detailed
        .response_annotations
        .iter()
        .map(|a| match a.kind {
            AnnotationKind::Traversal => "traversal",
            AnnotationKind::VectorDegraded => "vector_degraded",
            AnnotationKind::LoadBearing => "load_bearing",
            AnnotationKind::Conflict => "conflict",
            AnnotationKind::Hot => "hot",
            AnnotationKind::Reservation => "reservation",
        })
        .collect();

    json!({
        "query": truncate_to(query, LEDGER_QUERY_PREFIX),
        "top_k": top_k,
        "hit_count": detailed.hits.len(),
        "hits": hits,
        "canonical_marker": canonical_marker,
        "blast_radius_warning": blast_radius_warning,
        "conflict_line": conflict_line,
        "hot_warning": hot_warning,
        "reservation_warning": reservation_warning,
        "response_annotations": response_kinds,
        "warning_count": detailed.warnings.len(),
    })
}

/// Longest concept-text prefix a ledger line carries.
///
/// A concept may be `MAX_CONTENT_BYTES` (16 KiB) and a recall may return
/// [`MAX_TOP_K`] hits, so untruncated text makes a one-megabyte worst-case line.
/// 200 characters names a concept unambiguously in a report and keeps a heavy
/// dogfood day's ledger in the low megabytes.
const LEDGER_CONTENT_PREFIX: usize = 200;

/// Longest recall-`query` prefix a ledger line carries.
///
/// The query is the other client string on a recall line, and it is `check_size`d
/// at 16 KiB like everything else — so it belongs in the worst-case reasoning
/// above, which an earlier revision omitted: a real 15.4 KiB query produced a
/// 15,752-byte line, ten times what [`LEDGER_CONTENT_PREFIX`] budgets for all
/// `MAX_TOP_K` hits together.
///
/// Cut generously rather than at 200, because the two strings are read for
/// different things. Concept text only has to *name* the concept in a report;
/// a query is the input under study — `score_bands.py` and `warnings.py` both
/// print it verbatim, and a query cut at 200 characters stops being reproducible.
/// 2000 characters keeps one line's query an order of magnitude below the hit
/// budget while covering every query a human or an agent actually writes.
const LEDGER_QUERY_PREFIX: usize = 2000;

/// Compile-time pin on the relationship the two caps' reasoning rests on: the
/// query cap is deliberately the wider of the two, for the reason above. A future
/// edit that narrowed it below the content cap would not fail a test — it would
/// fail the build, here.
const _: () = if LEDGER_QUERY_PREFIX <= LEDGER_CONTENT_PREFIX {
    panic!("the ledger's recall-query cap must stay wider than its concept-text cap");
};

/// `content` truncated to [`LEDGER_CONTENT_PREFIX`] **characters**, with an
/// explicit marker so a consumer never mistakes a cut for the whole text.
fn truncate_for_ledger(content: &str) -> String {
    truncate_to(content, LEDGER_CONTENT_PREFIX)
}

/// Longest inspect-focus prefix a ledger line carries.
///
/// The focus is the other client string an inspect line carries (issue #9):
/// recorded on the failure paths so a failed inspect is classifiable from the
/// ledger alone. 200 characters identifies the miss without carrying a 16 KiB
/// paste.
const LEDGER_FOCUS_PREFIX: usize = 200;

/// `focus` truncated to [`LEDGER_FOCUS_PREFIX`] **characters**, with an
/// explicit marker so a consumer never mistakes a cut for the whole focus.
fn focus_for_ledger(focus: &str) -> String {
    truncate_to(focus, LEDGER_FOCUS_PREFIX)
}

/// `text` truncated to `max` **characters**, with an explicit marker so a
/// consumer never mistakes a cut for the whole string.
///
/// Cut on a `char` boundary, not a byte one: a byte slice through a multi-byte
/// codepoint would panic, and the ledger is not allowed to panic a tool call.
fn truncate_to(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((cut, _)) => format!("{}…[truncated]", &text[..cut]),
    }
}

/// Classify the error this call is returning. Outside a ledgered call, a no-op.
fn note_error(kind: &'static str) {
    let _ = TRACE.try_with(|t| t.borrow_mut().error_kind = Some(kind));
}

/// Record per-tool payload facts — **lazily**, so the JSON is built only when a
/// ledger will actually consume it.
fn note_facts(facts: impl FnOnce() -> serde_json::Value) {
    let _ = TRACE.try_with(|t| t.borrow_mut().facts = Some(facts()));
}

/// A short, detail-free class for a `Memory` failure (N4).
///
/// The full error can interpolate a DSN, a store URL, a file path or a driver
/// message — none of which the model needs and any of which is worth keeping
/// out of a model-facing string. Return the class; the detail is logged.
///
/// `pub(crate)` since JE2E-12, so `writeq`'s async write path can render the
/// same class the synchronous path does. The alternative was a second match in
/// `writeq.rs`, which is how a new `LamboError` variant would come to have one
/// class on the sync path and another on the async one — the drift this
/// function exists to prevent, reintroduced one module over.
pub(crate) fn err_class(err: &LamboError) -> &'static str {
    match err {
        LamboError::Store(_) => "store error",
        // J3 round-1 N1: same pairing as the Conflict/SoftLock one below. The
        // split lets the durable-intent replay tell "not reached" from "refused
        // this input"; it must not give the model, the operator or the ledger a
        // new error class to learn.
        LamboError::Embed(_) | LamboError::EmbedUnavailable(_) => "embedding error",
        LamboError::Config(_) => "configuration error",
        // J1-R2-2: two variants, one class. The split exists so N4 can tell a
        // model-safe §11 refusal from a lease-lost one; an operator and the
        // ledger see the same `error_kind` either way, so nothing downstream of
        // this function moved.
        LamboError::Conflict(_) => "conflict",
        LamboError::SoftLock(_) => "conflict",
        LamboError::Other(_) => "internal error",
    }
}

/// Render a `Memory` failure as a caller-visible tool error (N4).
///
/// Matches the [`contain_panic`] policy: the full detail goes to the log, the
/// client gets a class and a pointer to the log — never the raw error, which
/// can carry a store URL or driver message.
///
/// **One documented exception**, added by J1-R1-2 and narrowed by J1-R2-2:
/// [`conflict_err`] renders a [`LamboError::SoftLock`] with its message intact
/// on the reserve path, because that message is a node id, a holder and an
/// expiry — nothing N4 exists to hide, and the only way a caller can learn who
/// to wait for. Every other error on every path comes through here, including
/// every [`LamboError::Conflict`]: the lease-lost fence is one, and its message
/// is exactly the operator-only detail N4 exists for.
fn tool_err(what: &str, err: LamboError) -> CallToolResult {
    tracing::error!(
        tool = what,
        error = %err,
        "mcp: tool returned a Memory error — full detail logged, class returned to the caller"
    );
    // I1: the same class the caller is told, in the ledger's `error_kind`.
    note_error(err_class(&err));
    CallToolResult::error(vec![ContentBlock::text(format!(
        "{what}: {} (the detail was logged server-side)",
        err_class(&err)
    ))])
}

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
fn breaks_one_line(c: char) -> bool {
    c.is_control() || c == '\u{2028}' || c == '\u{2029}'
}

/// Render a §11 soft-lock refusal from the reserve path as a model-facing
/// refusal that still carries its detail (J1-R1-2).
///
/// [`tool_err`]'s N4 policy discards a `Memory` error's message because it can
/// interpolate a DSN, a store URL, a file path or a driver string — none of
/// which the model needs. `graph::reserve`'s two messages carry none of that:
/// they are built from a node id the caller just sent, the holder's `agent_id`,
/// and an expiry — and the last two are *already* model-facing, since `recall`
/// renders that same holder and expiry into the context block. They are also
/// precisely what the loser of a race needs, because the whole
/// cooperative-identity design is "coordinate by ids"; a bare `conflict` leaves
/// a caller unable to tell a lock it should wait for from one it should work
/// around.
///
/// **What selects this function is the producer, not the class (J1-R2-2).** The
/// first version of this exception matched [`LamboError::Conflict`], and that
/// was wrong: `Memory::reserve_as`/`release_as` enter `begin_write_sync()`
/// *before* the graph, and a fenced handle's `lease_lost_error` was a `Conflict`
/// too — one interpolating `store::lease::OPERATOR_OVERRIDE`, a raw
/// `DELETE FROM session_leases …`. So `lambo_reserve` handed a model an
/// operator-only statement against an internal table, on a path where the
/// parent returned a class. Matching a *variant* opens the door for every
/// producer of that variant, not for the one this docstring reasons about, and
/// no amount of care in this function could have narrowed it — which is why
/// `graph::reserve` now returns its own [`LamboError::SoftLock`] and this
/// exception is spelled against that. The default is closed: a new `Conflict`
/// producer anywhere under `reserve_as` flattens through [`tool_err`] without
/// anyone having to remember this paragraph. `redact_urls` was never the
/// missing piece — the leaked string had no `://`.
///
/// The exception stays as narrow as it reads: everything else on this path
/// still goes through [`tool_err`], and the ledger books the same `error_kind`
/// (`"conflict"`) either way, so the split is invisible downstream of
/// [`err_class`]. The appended "wait for the expiry or work elsewhere" advice is
/// true again for the same reason: a §11 soft lock does expire, whereas the
/// fenced handle this function used to reach never will.
///
/// The message is folded to one line on the way out, by the same
/// [`breaks_one_line`] class [`LamboServer::check_agent_id`] refuses at the door
/// — one predicate, so the guard and the fold cannot disagree about what "one
/// line" means (they did: J1-R2-1). The fold is defence in depth, for a holder
/// that entered by another path — a library caller, or an operator's `--agent`.
///
/// It does **not** [`redact_urls`]. N3's redaction exists for Lambo's own
/// endpoints appearing in Lambo's own warnings; the holder here is a
/// caller-chosen name, which `recall` already renders verbatim into the context
/// block through `format::reservation_warning`. Redacting this one path would
/// advertise a neutralisation the read side does not have. Whether a
/// caller-chosen id should be neutralised on render at all is still open —
/// recorded as a §J2 residual in `dev-diary/lambo-for-mooshik/J-multi-client.md`
/// rather than only here, since J2 is where it comes due.
fn conflict_err(what: &str, msg: &str, nothing: &str) -> CallToolResult {
    tracing::warn!(
        tool = what,
        conflict = %msg,
        "mcp: soft-lock conflict returned to the caller"
    );
    // I1: the same class the caller is told, unchanged from the `tool_err` path.
    note_error("conflict");
    CallToolResult::error(vec![ContentBlock::text(format!(
        "{what}: {}; {nothing}. Wait for the expiry or work elsewhere.",
        msg.chars()
            .map(|c| if breaks_one_line(c) { ' ' } else { c })
            .collect::<String>()
    ))])
}

/// Replace any whitespace-delimited token that looks like a URL with a
/// placeholder (N3), so a warning that surfaced a store/embedder endpoint does
/// not carry it into a model-facing string. Idempotent — a redacted token has
/// no `://` left to match.
///
/// Scope (R4 nit): this matches only `scheme://…` tokens, not a bare
/// `host:port`. That is deliberate, not an oversight — no current warning path
/// emits a schemeless `host:port` (every endpoint the store/embedder logs is a
/// full URL), and a `host:port` matcher trained on a colon would over-redact
/// ordinary warning text (`ratio 3:4`, `line 42:10`, SQLSTATE-style codes),
/// corrupting the very message it is meant to keep readable. If a future warning
/// starts emitting bare `host:port`, redact it at that source (where the shape is
/// known) rather than widening this heuristic.
fn redact_urls(s: &str) -> String {
    s.split(' ')
        .map(|tok| {
            if tok.contains("://") {
                "<redacted-url>"
            } else {
                tok
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reject a parameter the server will not act on.
///
/// A bad parameter is the *client's* problem and it is worth surfacing where
/// the model can read and correct it, so this is a tool-level error rather than
/// a `-32602` the client renders opaquely.
fn bad_param(msg: impl Into<String>) -> CallToolResult {
    note_error("invalid params");
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

/// Shared [`validate_size`] mapped into a tool-level error. The check itself
/// lives in [`crate::cli::caps`] so CLI and MCP cannot drift.
fn check_size(field: &str, value: &str) -> Result<(), CallToolResult> {
    validate_size(field, value).map_err(bad_param)
}

/// Attach warnings to a result **where the model will actually read them**.
///
/// R1/T82-9: warnings used to live only in `structuredContent`, which MCP
/// clients treat as optional and commonly do not surface — so
/// `lambo_reserve`'s advisory-and-RAM-local warning, and `Memory::recall`'s
/// embed-failure degradation warning, reached nobody. They are now a second
/// text block. `content[0]` is deliberately left alone: for `lambo_recall` it is
/// the T5.3 context block verbatim, and that is the artifact the calling agent
/// reads.
///
/// URLs are redacted from the model-facing text (N3): a degradation warning can
/// surface a store or embedder endpoint, which the model does not need. The raw
/// warning is logged for the operator.
fn attach_warnings(out: &mut CallToolResult, warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    let mut text = String::from("warnings:");
    for w in warnings {
        tracing::debug!(warning = %w, "mcp: warning attached to a result (raw, pre-redaction)");
        text.push_str("\n- ");
        text.push_str(&redact_urls(w));
    }
    out.content.push(ContentBlock::text(text));
}

/// Piggyback settled write receipts onto a tool result (J3 shape part 3).
///
/// **Why this and not an MCP notification.** A notification lands in the
/// client's log, not in the model's context — the exact failure workstream J
/// exists to fix, where a serve refusal reached nobody. A tagged text block on
/// the next response the agent reads is the one delivery channel the model is
/// guaranteed to see.
///
/// **Per-caller through a shared hub.** The agent is the caller-asserted
/// `agent_id` of *this* call, so several clients through one hub — or through a
/// J2 proxy, which forwards the response bytes untouched — each get only their
/// own receipts. Receipts are per-agent scoped in the store as well (J1), so
/// this is a lookup, not a filter.
///
/// **Take-once**, so a settled receipt is not re-announced forever. A response
/// that never reaches its client loses its piggyback, which is why the
/// fetch-by-id surface on `lambo_stats` exists as well.
///
/// URLs are redacted (N3), and that is now the **second** layer rather than the
/// only one. Since JE2E-12 a `failed` answer carries the N4 *class* —
/// `err_class` plus "the detail was logged server-side", the same sentence
/// `tool_err` renders on the synchronous path — instead of the `LamboError`'s
/// own message. `redact_urls` stays because it is not the same guard: it matches
/// `scheme://` tokens, and the thing that reached a model before this was a
/// store **file path** ("unable to open database file: /path"), which has no
/// `://` for it to catch. Neither layer is load-bearing alone.
fn attach_receipts(
    out: &mut CallToolResult,
    taken: &[(ReceiptId, ReceiptAnswer)],
    remaining: usize,
) {
    if taken.is_empty() {
        return;
    }
    let mut text = String::from("write receipts (your earlier writes, now settled):");
    for (id, answer) in taken {
        text.push_str("\n- ");
        text.push_str(&id.to_string());
        text.push_str(": ");
        text.push_str(&redact_urls(&answer.describe()));
    }
    if remaining > 0 {
        text.push_str(&format!(
            "\n({remaining} more settled receipt(s) will arrive on your next call)"
        ));
    }
    out.content.push(ContentBlock::text(text));
}

/// Run a tool body with panic containment (R1/T82-5).
///
/// The MCP boundary is fed arbitrary client input, and a panicking handler used
/// to drop the JSON-RPC response entirely: no result, no error, no
/// cancellation, and the caller blocked until its own timeout. T8.1 armours
/// every store attempt this way for the same reason; the same two helpers are
/// reused here so there is one panic-containment behaviour in the tree.
///
/// The panic detail goes to the log, never to the client — a payload can
/// interpolate anything, including a DSN.
async fn contain_panic(
    tool: &'static str,
    fut: impl Future<Output = CallToolResult>,
) -> CallToolResult {
    match CatchUnwindPoll(fut).await {
        Ok(out) => out,
        Err(payload) => {
            tracing::error!(
                tool,
                panic = %panic_message(&payload),
                "mcp: tool handler panicked — contained and reported as a tool error"
            );
            // I1: a contained panic is its own outcome, not just "error".
            note_error("panic");
            CallToolResult::error(vec![ContentBlock::text(format!(
                "{tool}: internal error (the failure was logged server-side); \
                 the call had no effect beyond anything already written"
            ))])
        }
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
const MAX_AGENT_ID_CHARS: usize = 256;

/// The text half of the `gc` object, one line on the `lambo_stats` summary.
fn gc_summary_line(g: &crate::memory::GcStats) -> String {
    let at = g
        .last_gc_at
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "never".into());
    match &g.last_sweep {
        None => format!(
            "gc: last_gc_at={at} last_gc_epoch={} last_sweep=none (this process)",
            g.last_gc_epoch
        ),
        Some(s) => format!(
            "gc: last_gc_at={at} last_gc_epoch={} last_sweep trigger={} collected={} \
             deferred={} cap={} cap_bound={}",
            g.last_gc_epoch,
            s.trigger.map(|t| t.as_str()).unwrap_or("direct"),
            s.collected,
            s.deferred,
            s.collection_cap,
            s.cap_bound
        ),
    }
}

/// `lambo_stats`' `gc` object (issue #29) — one builder for the tool and the
/// I2 heartbeat. `last_gc_at` is RFC 3339 or null; `last_sweep` is null until
/// this process has swept.
fn gc_stats_json(g: &crate::memory::GcStats) -> serde_json::Value {
    json!({
        "last_gc_at": g.last_gc_at.map(|t| t.to_rfc3339()),
        "last_gc_epoch": g.last_gc_epoch,
        "last_sweep": g.last_sweep.as_ref().map(|s| json!({
            "trigger": s.trigger.map(|t| t.as_str()),
            "collected": s.collected,
            "deferred": s.deferred,
            "collection_cap": s.collection_cap,
            "cap_bound": s.cap_bound,
            "resources_spared_by_dependents": s.resources_spared_by_dependents,
            "survivors_deferred": s.survivors_deferred,
        })),
    })
}

impl LamboServer {
    /// Wrap a live [`Memory`]. The `Arc` is the point: every clone of this
    /// server — one per HTTP request, in the streamable-http transport — shares
    /// the single session owner.
    pub fn new(mem: Arc<Memory>) -> Self {
        Self {
            mem,
            tool_router: Self::tool_router(),
            ledger: None,
            started_at: Instant::now(),
        }
    }

    /// The same handle, recording every tool call to the I1 ledger.
    ///
    /// `serve --ledger` is the only caller. Clones share the ledger, as they
    /// share the `Memory`: the streamable-http transport clones this handle per
    /// request and all of them must append to one file.
    pub fn with_ledger(mem: Arc<Memory>, ledger: Arc<Ledger>) -> Self {
        Self {
            ledger: Some(ledger),
            ..Self::new(mem)
        }
    }

    /// The session this process owns.
    pub fn memory(&self) -> &Arc<Memory> {
        &self.mem
    }

    /// The call ledger, when one is configured.
    pub fn ledger(&self) -> Option<&Arc<Ledger>> {
        self.ledger.as_ref()
    }

    /// The agent id [`LamboServer::observed`] should stamp on the line — `None`,
    /// and no allocation at all, when there is no ledger to stamp it onto.
    ///
    /// This exists so that "off costs nothing" is true of the *string* too. The
    /// obvious shape (`observed(tool, &p.agent_id, self.foo_impl(p))`) does not
    /// compile: `p` moves into the impl future, so a borrow of `p.agent_id`
    /// cannot outlive the call expression. Deciding here keeps the one clone on
    /// the ledger-on path where it belongs, without duplicating the check across
    /// seven tool wrappers.
    fn ledger_agent(&self, agent_id: &str) -> Option<String> {
        self.ledger.as_ref().map(|_| agent_id.to_string())
    }

    /// Run one tool body and, when a ledger is listening, append its line.
    ///
    /// **The ledger never changes what the caller gets.** The result is
    /// returned unmodified; the line is built from it afterwards. The append
    /// itself cannot block or fail (see [`Ledger::append`]), so a ledger that
    /// is behind, unwritable, or gone costs a tool call nothing but the
    /// microseconds of one `serde_json::to_vec`.
    ///
    /// With no ledger this is `contain_panic` and nothing else — no task-local
    /// scope, no `Instant`, no facts, and (see [`LamboServer::ledger_agent`]) no
    /// copy of the agent id either.
    async fn observed(
        &self,
        tool: &'static str,
        agent_id: Option<String>,
        fut: impl Future<Output = CallToolResult>,
    ) -> CallToolResult {
        let Some(ledger) = self.ledger.clone() else {
            return contain_panic(tool, fut).await;
        };
        // `Some` whenever a ledger is attached: `ledger_agent` and `self.ledger`
        // read the same field. An empty id would be refused by `check_agent_id`
        // anyway, and a line is still owed for that refusal.
        let agent_id = agent_id.unwrap_or_default();
        let started = Instant::now();
        TRACE
            .scope(std::cell::RefCell::new(CallTrace::default()), async move {
                let out = contain_panic(tool, fut).await;
                let duration_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
                // Read the slot from INSIDE the scope: `task_local::scope`
                // drops its value when the future completes, so there is no
                // "after" in which to read it.
                let (error_kind, facts) = TRACE.with(|t| {
                    let mut t = t.borrow_mut();
                    (t.error_kind.take(), t.facts.take())
                });
                let failed = out.is_error.unwrap_or(false);
                let outcome = match (failed, error_kind) {
                    (_, Some("panic")) => "panic",
                    (true, _) => "error",
                    (false, _) => "ok",
                };
                ledger.append(&crate::ledger::call_line(
                    tool,
                    &agent_id,
                    outcome,
                    // A failure with no class is a path that returned
                    // `CallToolResult::error` without going through
                    // `bad_param` / `tool_err`; say so rather than guessing.
                    if failed {
                        Some(error_kind.unwrap_or("unclassified"))
                    } else {
                        None
                    },
                    duration_us,
                    facts,
                ));
                out
            })
            .await
    }

    /// The `lambo_stats` numbers, as the JSON both `lambo_stats` and the I2
    /// heartbeat report.
    ///
    /// One builder so the two can never drift: a heartbeat that disagreed with
    /// the tool would make the whole time axis in `scripts/observability`
    /// unreadable.
    fn stats_json(&self) -> serde_json::Value {
        self.stats_json_with_gc(&self.mem.gc_stats())
    }

    /// [`Self::stats_json`] over a `gc` reading the caller already took, so
    /// `lambo_stats` can render its structured `gc` object and its text `gc:`
    /// line from one value (two reads straddling the daemon's first anchor or a
    /// sweep could otherwise disagree within one answer).
    fn stats_json_with_gc(&self, gc: &crate::memory::GcStats) -> serde_json::Value {
        let s = self.mem.stats();
        let mut payload = json!({
            "session": s.session.0,
            "agent": s.agent.0,
            "flush_lag_ms": s.flush_lag.as_millis() as u64,
            "log_depth": s.log_depth,
            "flush_depth": s.flush_depth,
            "dead_lettered": s.dead_lettered,
            "degraded": s.degraded,
            "node_count": s.node_count,
            "edge_count": s.edge_count,
            "concept_count": s.concept_count,
            "total_concepts": s.concept_count,
            "embedded_concepts": s.embedded_concepts,
            "canonical_count": s.canonical_count,
            "epoch": s.epoch,
            "daemon_cycles": s.daemon_cycles,
            "canonization_cycles": s.canonization_cycles,
            "canonization_failures": s.canonization_failures,
            // Which promotion policy the cycles above are actually running.
            // The counters cannot answer it: `canonization_cycles` climbs
            // identically under both policies and `canonical_count` staying 0
            // is the normal reading for a young swarm session AND the whole
            // symptom of a `Solo` selection that did not take. Read from the
            // live `Config`, so it reports the value that WON — file, env, or
            // default — rather than any one of the three inputs.
            "promotion_policy": self.mem.config().promotion_policy.as_str(),
            // Issue #29: GC's sweep accounting, read-side only. An additive
            // key: `last_gc_at`/`last_gc_epoch` are the durable mark,
            // `last_sweep` the last sweep THIS process ran (null after a
            // restart until the next one).
            "gc": gc_stats_json(gc),
        });
        // I1: dropped lines are reported next to written ones so a gap in the
        // ledger is never mistaken for a gap in the traffic. Emitted ONLY when
        // a ledger exists — with `--ledger` off the payload is byte-identical
        // to what it was before I1, which is what "off by default means no
        // behaviour change" has to mean for a payload.
        //
        // `ledger_dropped_lines` stays the headline total ("is this ledger
        // complete?"); the two `_channel_full` / `_write_failed` keys beside it
        // answer "why", which the total cannot: backpressure means the writer is
        // behind, a failed write means the path is broken, and an operator
        // reading one number cannot tell those apart. Additive keys on a payload
        // that only exists when the ledger is on.
        if let Some(ledger) = &self.ledger {
            let obj = payload.as_object_mut().expect("json! built an object");
            obj.insert(
                "ledger_path".into(),
                json!(ledger.path().display().to_string()),
            );
            obj.insert(
                "ledger_written_lines".into(),
                json!(ledger.counters().written()),
            );
            obj.insert(
                "ledger_dropped_lines".into(),
                json!(ledger.counters().dropped()),
            );
            obj.insert(
                "ledger_dropped_channel_full".into(),
                json!(ledger.counters().dropped_channel_full()),
            );
            obj.insert(
                "ledger_dropped_write_failed".into(),
                json!(ledger.counters().dropped_write_failed()),
            );
            // I-R2-3. Queue depth, because the drop counters have a blind spot
            // about themselves: on a path whose `open` blocks (reader-less FIFO,
            // hung mount) the writer parks before its first write, so `written`
            // and both drop counters read `0` — indistinguishable from an idle
            // server — until CHANNEL_CAPACITY lines have piled up. This key moves
            // on the first call, so "writer parked" is visible immediately.
            obj.insert(
                "ledger_queued_lines".into(),
                json!(ledger.counters().queued()),
            );
        }
        // J3. Unconditional, unlike the `ledger_*` keys above, and the
        // difference is not an inconsistency: the ledger is an optional
        // subsystem, so "off by default means no behaviour change" is a promise
        // that can be kept for it byte-for-byte. The write queue has no off
        // switch — every `lambo_derive` goes through it — so there is no
        // baseline payload left to preserve, and hiding the keys behind a
        // condition that is always true would only make them look optional.
        //
        // `write_queue_bound` / `write_queue_lane_bound` are the static
        // fairness/memory caps in force (the J3 redesign — no rate sizes a
        // bound any more); `write_queue_measured` and the rate keys are the
        // embedder telemetry that used to size them and now only describes
        // them. `write_queue_accepted` is here so the gauge is re-derivable
        // from the payload — `outstanding = accepted − applied − failed −
        // deferred` — which is the property I-R2-3 asked `ledger_queued_lines`
        // for and the reason `dropped` sits beside them rather than inside
        // them.
        {
            let queue = self.mem.pipeline();
            let c = queue.counters();
            let calibration = queue.calibration();
            let obj = payload.as_object_mut().expect("json! built an object");
            obj.insert(
                "write_queue_bound".into(),
                json!(calibration.map_or(crate::writeq::WRITE_QUEUE_MAX, |c| c.bound)),
            );
            // The bound that actually refuses one agent's burst, reported
            // beside the aggregate one (J3-R1-1): a lane drains 1-wide however
            // wide the deployment's embedder is, so this is the number that
            // explains a drop a single-agent session sees. Since the J3
            // redesign it is the per-agent fair share, not a measurement.
            obj.insert(
                "write_queue_lane_bound".into(),
                json!(calibration.map_or(crate::writeq::WRITE_QUEUE_LANE_MAX, |c| c.lane_bound)),
            );
            obj.insert(
                "write_queue_measured".into(),
                json!(calibration.is_some_and(|c| c.measured())),
            );
            // `probe`, `observed` or `unmeasured`. The probe fires at the
            // coldest moment of the process's life and measured a 7x spread
            // across repeats on one host (J3-R1-2), so "measured" is not enough
            // on its own: an operator needs to know whether the number is a
            // startup estimate or this deployment's own observed writes.
            obj.insert(
                "write_queue_bound_source".into(),
                json!(calibration.map_or("unmeasured", |c| c.source.tag())),
            );
            obj.insert(
                "write_queue_items_per_sec".into(),
                json!(calibration.and_then(|c| c.items_per_sec)),
            );
            obj.insert(
                "write_queue_serial_items_per_sec".into(),
                json!(calibration.and_then(|c| c.serial_items_per_sec)),
            );
            // The probe's own serial figure, kept beside whichever rate is in
            // force (J3-R2-4). Replacing a number is not a reason to destroy
            // it: `serial_items_per_sec` alone tells an operator what the
            // deployment retires at now, and the GAP between the pair is the
            // self-diagnosing comparison — how far the startup estimate sat
            // from the work the agents actually send, the fact two review
            // rounds had to measure at a release binary because nothing
            // published it. Equal to `write_queue_serial_items_per_sec` while
            // `bound_source` is `probe`, and frozen at the probe's reading
            // after that.
            obj.insert(
                "write_queue_probe_serial_items_per_sec".into(),
                json!(calibration.and_then(|c| c.probe_serial_items_per_sec)),
            );
            obj.insert("write_queue_outstanding".into(), json!(c.outstanding()));
            obj.insert("write_queue_accepted".into(), json!(c.accepted()));
            obj.insert("write_queue_applied".into(), json!(c.applied()));
            obj.insert("write_queue_failed".into(), json!(c.failed()));
            obj.insert("write_queue_abandoned".into(), json!(c.abandoned()));
            obj.insert("write_queue_dropped".into(), json!(c.dropped()));
            // Split out of `write_queue_dropped`'s total, not subtracted from
            // it (J3-R1-8): "the embedder is the bottleneck" and "the session
            // is shutting down and refused a tail" are the same count but
            // opposite diagnoses, and `dropped` remains their sum so no count
            // vanishes.
            obj.insert(
                "write_queue_dropped_closed".into(),
                json!(c.dropped_closed()),
            );
            // J3 durable intents: `deferred` counts this session's acked
            // writes a clean close handed to the NEXT serve as durable
            // intents (a fourth settle class — neither applied nor failed);
            // `replayed` counts a PREVIOUS process's intents this session
            // applied at attach (not summed into `applied`, which counts only
            // this session's own accepted jobs, so `outstanding` stays exact).
            obj.insert("write_queue_deferred".into(), json!(c.deferred()));
            obj.insert("write_queue_replayed".into(), json!(c.replayed()));
            // J3 round-1 N1: the replay DEBT, not a total — durable intents
            // this session found owed and has not yet paid. Non-zero with
            // `replayed` not advancing is the visible form of "the embedder was
            // not answering at attach, so nothing was consumed".
            obj.insert("write_queue_replay_owed".into(), json!(c.replay_owed()));
            // J3 round-2 R-8: a *level* (`replay_owed`) cannot tell "draining"
            // from "wedged"; this names the class of the error that ended the
            // last replay. `null` = draining/idle, "embedder" = sick/wedged,
            // "other" = store/lease/config.
            obj.insert(
                "write_queue_replay_blocked".into(),
                match c.replay_blocked() {
                    crate::writeq::ReplayBlockReason::None => json!(null),
                    crate::writeq::ReplayBlockReason::Embedder => json!("embedder"),
                    crate::writeq::ReplayBlockReason::Other => json!("other"),
                },
            );
            obj.insert("receipts_retained".into(), json!(queue.receipts_retained()));
        }
        payload
    }

    /// Build one I2 heartbeat line: the `lambo_stats` payload, this process's
    /// uptime, and the binary's version + git sha.
    pub fn heartbeat_line(&self) -> serde_json::Value {
        crate::ledger::stats_line(self.stats_json(), self.started_at.elapsed())
    }

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
    fn check_agent_id(&self, agent_id: &str) -> Result<(), CallToolResult> {
        if agent_id.trim().is_empty() {
            return Err(bad_param("agent_id must be a non-empty string"));
        }
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
    fn caller_agent(&self, agent_id: &str) -> Result<AgentId, CallToolResult> {
        self.check_agent_id(agent_id)?;
        Ok(AgentId::new(agent_id))
    }

    /// Run a tool through [`LamboServer::observed`] and then piggyback this
    /// caller's settled write receipts onto the result (J3).
    ///
    /// **One wrapper, so it cannot be forgotten for one tool.** The same
    /// reasoning that keeps every `*_impl` behind [`contain_panic`] applies
    /// here: the piggyback is the delivery channel for outcomes that no longer
    /// arrive on the write's own response, and a tool that skipped it would be
    /// a tool after which an agent silently never hears about its writes. It
    /// deliberately does not live inside `observed`, which returns early when
    /// no ledger is attached — that would have made receipt delivery depend on
    /// `--ledger`.
    ///
    /// Every tool carries it, `lambo_derive` included: a derive whose own ack
    /// is a receipt is exactly the call after which an *earlier* write is most
    /// likely to have settled.
    async fn answered(
        &self,
        tool: &'static str,
        agent_id: String,
        fut: impl Future<Output = CallToolResult>,
    ) -> CallToolResult {
        let mut out = self.observed(tool, self.ledger_agent(&agent_id), fut).await;
        // Only for an id the door would accept. A refused id owns no receipts
        // by construction (nothing could have been written under it), so this
        // is about not doing a lookup on an unvalidated string rather than
        // about hiding anything.
        if self.check_agent_id(&agent_id).is_ok() {
            let acting = AgentId::new(&agent_id);
            let (taken, remaining) = self.mem.pipeline().take_piggyback(&acting);
            attach_receipts(&mut out, &taken, remaining);
        }
        out
    }
}

// Each `#[tool]` handler is a thin, panic-contained wrapper (R1/T82-5) around a
// `*_impl` body in the plain `impl` block below. Keeping the bodies out of the
// macro'd block means the containment cannot be forgotten for one tool: the
// wrapper is the only thing the router can reach.
#[tool_router]
impl LamboServer {
    /// Three-phase recall (spec §8) rendered as the T5.3 context block.
    ///
    /// The block is returned verbatim as text content — it is the artifact the
    /// calling agent is meant to read — with warnings appended as a *second*
    /// text block and the hits alongside as structured content.
    #[tool(
        name = "lambo_recall",
        description = "Recall relevant memory for a query and return the Lambo context block \
                       (canonical markers, blast-radius warnings, conflict lines)."
    )]
    async fn lambo_recall(&self, Parameters(p): Parameters<RecallParams>) -> CallToolResult {
        let agent_id = p.agent_id.clone();
        self.answered("lambo_recall", agent_id, self.recall_impl(p))
            .await
    }

    /// Derive concepts from a fresh interaction (spec §7).
    ///
    /// The interaction's `created_at` is stamped server-side (F18). Callers may
    /// optionally supply the evidence's historical `event_time`.
    #[tool(
        name = "lambo_derive",
        description = "Derive concepts from the current interaction into session memory. \
                       created_at is stamped server-side. event_time is optional RFC3339 \
                       historical about-time, such as a commit or document date; omit it for \
                       a live fact, which is about now. Returns as soon \
                       as the input is validated and ordered — the write is applied in the \
                       background and the ack carries a receipt id. The outcome is \
                       piggybacked on your next tool response; to wait for it, call \
                       lambo_stats with that receipt and a wait_ms."
    )]
    async fn lambo_derive(&self, Parameters(p): Parameters<DeriveParams>) -> CallToolResult {
        let agent_id = p.agent_id.clone();
        self.answered("lambo_derive", agent_id, self.derive_impl(p))
            .await
    }

    /// Record an agent action (spec §7) — a `Resource` concept plus `Causal` /
    /// `Dependency` edges, on a fresh server-stamped interaction (F18).
    #[tool(
        name = "lambo_record_action",
        description = "Record an action the agent took, with what it produces, modifies and \
                       depends on. created_at is stamped server-side. event_time is optional \
                       RFC3339 historical about-time, such as a commit or document date; omit \
                       it for a live fact, which is about now. \
                       Returns as soon as the input is validated and ordered — the write is \
                       applied in the background and the ack carries a receipt id, resolved \
                       the same way as lambo_derive's."
    )]
    async fn lambo_record_action(
        &self,
        Parameters(p): Parameters<RecordActionParams>,
    ) -> CallToolResult {
        let agent_id = p.agent_id.clone();
        self.answered("lambo_record_action", agent_id, self.record_action_impl(p))
            .await
    }

    /// Take (or release) a soft lock on a node — spec §11.
    ///
    /// Not durable: reservations are RAM-local to this process (pinned contract
    /// S5). "No reservation" after a restart does **not** mean nobody else is
    /// working on the node.
    ///
    /// **Cooperative, and said so out loud (J1).** The lock is held under the
    /// caller-asserted `agent_id`, which nothing here authenticates — over stdio
    /// the client owns the process, over HTTP one token authenticates the server
    /// rather than each agent. So: distinct ids get distinct locks and contend
    /// honestly; callers that send the same id share one lock and can release
    /// each other's. That is the same trust level §11 soft locks always had
    /// (advisory, RAM-only, never blocking a write); what J1 removed was the
    /// blanket refusal of foreign ids, which left every client but one of a
    /// shared serve with no mutual-exclusion primitive at all.
    #[tool(
        name = "lambo_reserve",
        description = "Take a soft lock on a memory node before editing it (or release one). \
                       Reservations are advisory and do not survive a server restart. The \
                       lock is held under the agent_id you send, which is caller-asserted \
                       and unverified: a distinct id gets a distinct lock, and callers \
                       sharing an id share the lock."
    )]
    async fn lambo_reserve(&self, Parameters(p): Parameters<ReserveParams>) -> CallToolResult {
        let agent_id = p.agent_id.clone();
        self.answered("lambo_reserve", agent_id, self.reserve_impl(p))
            .await
    }

    /// Neighbourhood around a focus concept — the read-only graph view.
    #[tool(
        name = "lambo_inspect",
        description = "Inspect the neighbourhood around a concept: its type, canonization \
                       status, blast radius and typed edges out to a depth."
    )]
    async fn lambo_inspect(&self, Parameters(p): Parameters<InspectParams>) -> CallToolResult {
        let agent_id = p.agent_id.clone();
        self.answered("lambo_inspect", agent_id, self.inspect_impl(p))
            .await
    }

    /// The canonical ("saints") memories — spec §10.
    #[tool(
        name = "lambo_saints",
        description = "List the session's canonical memories — concepts that earned Canonical \
                       status through the audited transition path."
    )]
    async fn lambo_saints(&self, Parameters(p): Parameters<SaintsParams>) -> CallToolResult {
        let agent_id = p.agent_id.clone();
        self.answered("lambo_saints", agent_id, self.saints_impl(p))
            .await
    }

    /// Session health — the spec §2.4 observable durability bound.
    #[tool(
        name = "lambo_stats",
        description = "Session health: flush lag, write-behind log depth, background write \
                       queue depth, node/edge/concept counts, canonization progress and \
                       degraded state. Pass a receipt from a lambo_derive or \
                       lambo_record_action ack to ask what happened to that one write, and \
                       wait_ms to wait for it to be applied first."
    )]
    async fn lambo_stats(&self, Parameters(p): Parameters<StatsParams>) -> CallToolResult {
        let agent_id = p.agent_id.clone();
        self.answered("lambo_stats", agent_id, self.stats_impl(p))
            .await
    }
}

/// The tool bodies. Everything the router can reach goes through
/// [`contain_panic`] above; these are the parts that do the work.
///
/// **Read tools answer from the RAM graph even after `close()`** (R1/T82-14):
/// `lambo_stats`, `lambo_saints` and `lambo_inspect` read state that is still
/// valid — a closed session's graph does not change — while every write tool
/// and `lambo_recall` refuse. That is deliberate: an operator inspecting why a
/// close failed still needs `lambo_stats` to answer.
impl LamboServer {
    pub(crate) async fn recall_impl(&self, p: RecallParams) -> CallToolResult {
        if let Err(e) = self.check_agent_id(&p.agent_id) {
            return e;
        }
        let mut warnings: Vec<String> = Vec::new();
        if p.query.trim().is_empty() {
            return bad_param("query must be a non-empty string");
        }
        if let Err(e) = check_size("query", &p.query) {
            return e;
        }
        let cfg = self.mem.config();
        // N6: when the client omits a knob it inherits the session config's
        // default, which is not bound by the MCP maxima. A config wider than the
        // MCP cap (e.g. `default_top_k` above `MAX_TOP_K`) would otherwise make
        // the tool refuse a request that named nothing wrong. Clamp the
        // config-derived default into range with a logged warning; an *explicit*
        // out-of-range value from the client is still a client error and is
        // rejected below.
        let top_k = match p.top_k {
            Some(v) => v,
            None => clamp_cfg_default("default_top_k", cfg.default_top_k, 1, MAX_TOP_K),
        };
        let max_tokens = match p.max_tokens {
            Some(v) => v,
            None => clamp_cfg_default(
                "default_max_tokens",
                cfg.default_max_tokens,
                1,
                MAX_MAX_TOKENS,
            ),
        };
        let traversal_depth = match p.traversal_depth {
            Some(v) => v,
            None => clamp_cfg_default(
                "default_traversal_depth",
                cfg.default_traversal_depth,
                0,
                MAX_TRAVERSAL_DEPTH,
            ),
        };
        if top_k == 0 || top_k > MAX_TOP_K {
            return bad_param(format!("top_k must be in 1..={MAX_TOP_K}"));
        }
        if traversal_depth > MAX_TRAVERSAL_DEPTH {
            return bad_param(format!(
                "traversal_depth must be in 0..={MAX_TRAVERSAL_DEPTH}"
            ));
        }
        if max_tokens == 0 || max_tokens > MAX_MAX_TOKENS {
            return bad_param(format!("max_tokens must be in 1..={MAX_MAX_TOKENS}"));
        }

        let query_text = p.query.clone();
        let query = RecallQuery {
            query: p.query,
            top_k,
            max_tokens,
            traversal_depth,
        };
        // `recall_detailed` is the SAME execution `recall` projects from (it is
        // what `recall` calls); taking the detailed view here is what lets the
        // I1 ledger record per-leg scores and typed warning kinds. The response
        // below is built from the projection and is unchanged by this.
        let detailed = match self.mem.recall_detailed(query).await {
            Ok(r) => r,
            Err(e) => return tool_err("lambo_recall", e),
        };
        note_facts(|| recall_facts(&query_text, top_k, &detailed));
        let result: RecallResult = detailed.into();
        // These include `Memory::recall`'s embed-failure degradation warning —
        // the signal that a recall dropped its vector leg and returned
        // keyword-only hits. `attach_warnings` is what puts it where the model
        // can see it (R1/T82-9). Redact URLs as they enter the vec (N3) so both
        // the text content and the structured `warnings` are clean; the raw
        // detail is logged for the operator.
        for w in &result.warnings {
            tracing::debug!(warning = %w, "mcp: recall degradation warning (raw, pre-redaction)");
        }
        warnings.extend(result.warnings.iter().map(|w| redact_urls(w)));

        let hits: Vec<_> = result
            .hits
            .iter()
            .map(|h| {
                json!({
                    "node_id": h.node_id.0.to_string(),
                    "content": h.content,
                    "concept_type": h.concept_type,
                    "score": h.score,
                    "is_canonical": h.is_canonical,
                    "blast_radius": h.blast_radius,
                })
            })
            .collect();

        // `content[0]` stays the context block verbatim; warnings follow it.
        let mut out = CallToolResult::success(vec![ContentBlock::text(result.context.clone())]);
        attach_warnings(&mut out, &warnings);
        out.structured_content = Some(json!({
            "context": result.context,
            "hits": hits,
            "warnings": warnings,
        }));
        out
    }

    pub(crate) async fn derive_impl(&self, p: DeriveParams) -> CallToolResult {
        let acting = match self.caller_agent(&p.agent_id) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let event_time = p.event_time;
        // J1-R1-3: this path can no longer emit a warning. The attribution
        // warning was the only one it ever had, and J1 deleted it rather than
        // rewording it, so a `Vec` here would be a shape that says otherwise to
        // the next reader. The `warnings` key stays in `structuredContent`
        // because it is part of the response shape consumers read; if a later
        // phase (J3's write receipts) gives this tool something to say, it must
        // also go through `attach_warnings`, which is what puts a warning where
        // the model actually reads it (R1/T82-9).
        if p.concepts.is_empty() {
            return bad_param("concepts must contain at least one entry");
        }
        if p.concepts.len() > MAX_CONCEPTS_PER_DERIVE {
            return bad_param(format!(
                "concepts must contain at most {MAX_CONCEPTS_PER_DERIVE} entries"
            ));
        }
        if let Some(bad) = p.concepts.iter().find(|c| c.content.trim().is_empty()) {
            let _ = bad;
            return bad_param("every concept.content must be a non-empty string");
        }
        for c in &p.concepts {
            if let Err(e) = check_size("concept.content", &c.content) {
                return e;
            }
        }

        // `derive` borrows `&[(&str, ConceptType)]` and `ParentOf<'_>` borrows
        // `&[(&str, &str)]`; both owners must outlive the await.
        let concepts: Vec<(&str, ConceptType)> = p
            .concepts
            .iter()
            .map(|c| (c.content.as_str(), ConceptType::from(c.concept_type)))
            .collect();
        let pairs: Vec<(&str, &str)> = p
            .parent_of
            .iter()
            .flatten()
            .map(|r| (r.parent.as_str(), r.child.as_str()))
            .collect();
        if pairs
            .iter()
            .any(|(a, b)| a.trim().is_empty() || b.trim().is_empty())
        {
            return bad_param("parent_of entries must have non-empty parent and child");
        }
        for (a, b) in &pairs {
            if let Err(e) = check_size("parent_of.parent", a) {
                return e;
            }
            if let Err(e) = check_size("parent_of.child", b) {
                return e;
            }
        }
        let parent_of = if pairs.is_empty() {
            ParentOf::none()
        } else {
            ParentOf::from_pairs(&pairs)
        };

        // J3: acknowledged after validation, before the embedder. The
        // validation pre-pass and the interaction that pins this write's place
        // in the `Temporal` chain both happen inside this call; the embed,
        // canonicalize and insert happen in the background.
        let submitted = match self
            .mem
            .derive_async_as(&acting, &concepts, &parent_of, event_time)
            .await
        {
            Ok(s) => s,
            Err(e) => return tool_err("lambo_derive", e),
        };

        // I1 (DOGFOOD metric 2, re-derivation savings) — **changed by J3, and
        // this is the honest statement of what changed.** `created`, `matched`,
        // `semantic_merged` and `reinforced` are no longer knowable when this
        // line is written: the whole point of the async ack is that the write
        // has not happened yet. They are not lost — they are on the receipt,
        // and `receipt` is emitted here so a reader can join the two — but
        // `scripts/observability/dedup_rate.py` and `duplicates.py` read them
        // off the ledger line, so for MCP-driven sessions those two tools now
        // see zero derive facts. CLI-driven sessions are unaffected: they use
        // the synchronous `Memory::derive`, which still reports everything.
        //
        // The fix is a background-completion ledger line, which is a ledger
        // *schema* change and therefore not J3's (see §J3's handoff note). What
        // J3 owes is that the loss is visible rather than silent, which is what
        // `admitted` and `receipt` on this line are for.
        note_facts(|| {
            json!({
                "concepts_requested": concepts.len(),
                "admitted": !submitted.dropped(),
                "receipt": submitted.receipt.to_string(),
            })
        });

        let summary = if submitted.dropped() {
            format!(
                "{} concept(s) were NOT written: {}",
                concepts.len(),
                submitted.answer.describe()
            )
        } else {
            format!(
                "accepted {} concept(s) for background write; validated and ordered, not yet \
                 applied",
                concepts.len()
            )
        };
        let mut out = CallToolResult::success(vec![ContentBlock::text(format!(
            "{summary}\nreceipt {}: {}\nThe outcome arrives on your next tool response. To wait \
             for it, call lambo_stats with receipt={} (add wait_ms to block).",
            submitted.receipt,
            redact_urls(&submitted.answer.describe()),
            submitted.receipt,
        ))]);
        // `created` / `matched` are deliberately ABSENT rather than empty: an
        // empty list here would claim nothing was created, which is not what
        // this ack knows. They live on the receipt.
        out.structured_content = Some(json!({
            "summary": summary,
            "receipt": submitted.receipt.to_string(),
            "receipt_state": submitted.answer.tag(),
            "warnings": [],
        }));
        out
    }

    pub(crate) async fn record_action_impl(&self, p: RecordActionParams) -> CallToolResult {
        let acting = match self.caller_agent(&p.agent_id) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let event_time = p.event_time;
        // J1-R1-3: no warning is reachable here — see `derive_impl`.
        if p.action.trim().is_empty() {
            return bad_param("action must be a non-empty string");
        }
        if let Err(e) = check_size("action", &p.action) {
            return e;
        }
        let produces: Vec<String> = p
            .produces
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.0)
            .collect();
        let modifies: Vec<String> = p
            .modifies
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.0)
            .collect();
        let depends_on: Vec<String> = p
            .depends_on
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.0)
            .collect();
        // N1: cap the combined fan-out. Without this bound one client could hand
        // `record_action` an arbitrarily long target list and hold the single
        // process's graph write lock for as long as it takes to fan every entry
        // out into a concept and an edge — the stall vector `lambo_derive` is
        // already guarded against, on the tool that had no guard.
        let total = produces.len() + modifies.len() + depends_on.len();
        if total > MAX_ACTION_TARGETS {
            return bad_param(format!(
                "produces + modifies + depends_on must total at most {MAX_ACTION_TARGETS} \
                 entries ({total} given)"
            ));
        }
        if produces
            .iter()
            .chain(&modifies)
            .chain(&depends_on)
            .any(|s| s.trim().is_empty())
        {
            return bad_param("produces / modifies / depends_on entries must be non-empty");
        }
        for s in produces.iter().chain(&modifies).chain(&depends_on) {
            if let Err(e) = check_size("produces / modifies / depends_on entry", s) {
                return e;
            }
        }

        // N1, superseded by J3 and worth saying why rather than deleting.
        // `Memory::record_action` is synchronous and takes the graph write lock
        // for its whole body, so calling it inline occupied a Tokio *worker*
        // thread until it returned and a burst of large calls could starve the
        // runtime — including the worker that would run `Memory::close` on
        // SIGTERM. N1 moved it to the blocking pool with `spawn_blocking`.
        //
        // J3 removes the shape instead of offloading it: the graph write now
        // happens on a background pipeline worker and this call does validation
        // plus one brief `begin_interaction_full` lock, neither of which can
        // occupy a worker for long. So the `spawn_blocking` hop is gone, and
        // what it was defending is defended better. The load-bearing anti-hang
        // guarantee is unchanged and still `serve`'s `CLOSE_GRACE` bound
        // (src/mcp/serve.rs), which force-exits a stalled shutdown regardless.
        let produces: Vec<&str> = produces.iter().map(String::as_str).collect();
        let modifies: Vec<&str> = modifies.iter().map(String::as_str).collect();
        let depends_on: Vec<&str> = depends_on.iter().map(String::as_str).collect();
        let action = Action {
            event_time,
            action: p.action.as_str(),
            produces: &produces,
            modifies: &modifies,
            depends_on: &depends_on,
        };
        // J3: acknowledged after validation, before the graph write. Unlike
        // `derive` this path has no embedder hop, so what asynchrony buys here
        // is ORDERING with derive — see `Memory::record_action_async_as`.
        let submitted = match self.mem.record_action_async_as(&acting, &action).await {
            Ok(s) => s,
            Err(e) => return tool_err("lambo_record_action", e),
        };

        // I1: `created` and `edges` are not knowable at ack time — see the
        // matching note in `derive_impl` for what that costs and where the
        // numbers went.
        note_facts(|| {
            json!({
                "admitted": !submitted.dropped(),
                "receipt": submitted.receipt.to_string(),
            })
        });
        let summary = if submitted.dropped() {
            format!(
                "action '{}' was NOT recorded: {}",
                p.action,
                submitted.answer.describe()
            )
        } else {
            format!(
                "accepted action '{}' for background write; validated and ordered, not yet \
                 applied",
                p.action
            )
        };
        let mut out = CallToolResult::success(vec![ContentBlock::text(format!(
            "{summary}\nreceipt {}: {}\nThe outcome arrives on your next tool response. To wait \
             for it, call lambo_stats with receipt={} (add wait_ms to block).",
            submitted.receipt,
            redact_urls(&submitted.answer.describe()),
            submitted.receipt,
        ))]);
        // `action_node`, `created` and `edges` are ABSENT rather than zeroed:
        // this ack does not know them. They are on the receipt.
        out.structured_content = Some(json!({
            "summary": summary,
            "receipt": submitted.receipt.to_string(),
            "receipt_state": submitted.answer.tag(),
            "warnings": [],
        }));
        out
    }

    async fn reserve_impl(&self, p: ReserveParams) -> CallToolResult {
        let releasing = p.release.unwrap_or(false);
        // I1: grant/refusal for EVERY exit of this tool, set before the first
        // one can be taken — which means before the `agent_id` check, not after.
        // Each success path overwrites it with `granted: true`; anything that
        // returns early — an empty or oversized `agent_id`, a bad node_id, a
        // `Conflict` from a lock another agent holds — leaves this standing, so a
        // refusal can never be recorded as a grant by a path somebody forgot to
        // annotate. The id check used to run first, which left its own two exits
        // reporting `op=None granted=None`.
        let op = if releasing { "release" } else { "reserve" };
        note_facts(|| json!({ "op": op, "granted": false }));
        // J1: the caller's id IS the lock identity. No refusal here any more —
        // two clients through one serve contend for real, each under its own
        // name, which is the whole point. The id is unauthenticated, and the
        // tool description says so rather than this code pretending otherwise.
        let acting = match self.caller_agent(&p.agent_id) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let mut warnings: Vec<String> = Vec::new();
        // N5: node_id is a client string too — size- and control-checked before
        // it is parsed, so the same uniform guard covers every field.
        if let Err(e) = check_size("node_id", &p.node_id) {
            return e;
        }
        let node_id = match uuid::Uuid::parse_str(p.node_id.trim()) {
            Ok(u) => NodeId(u),
            Err(e) => return bad_param(format!("node_id must be a UUID: {e}")),
        };

        if releasing {
            return match self.mem.release_as(&acting, node_id) {
                Ok(()) => {
                    note_facts(|| json!({ "op": "release", "granted": true }));
                    let msg = format!("released {}", node_id.0);
                    let mut out = CallToolResult::success(vec![ContentBlock::text(msg.clone())]);
                    attach_warnings(&mut out, &warnings);
                    out.structured_content =
                        Some(json!({ "released": true, "node_id": node_id.0.to_string(),
                                     "summary": msg, "warnings": warnings }));
                    out
                }
                Err(LamboError::SoftLock(msg)) => {
                    conflict_err("lambo_reserve (release)", &msg, "nothing was released")
                }
                Err(e) => tool_err("lambo_reserve (release)", e),
            };
        }

        let ttl_secs = p.ttl_seconds.unwrap_or(30);
        if ttl_secs == 0 || ttl_secs > MAX_RESERVE_TTL_SECS {
            return bad_param(format!("ttl_seconds must be in 1..={MAX_RESERVE_TTL_SECS}"));
        }
        let reservation = match self
            .mem
            .reserve_as(&acting, node_id, Duration::from_secs(ttl_secs))
        {
            Ok(r) => r,
            Err(LamboError::SoftLock(msg)) => {
                return conflict_err("lambo_reserve", &msg, "nothing was reserved")
            }
            Err(e) => return tool_err("lambo_reserve", e),
        };
        note_facts(|| json!({ "op": "reserve", "granted": true, "ttl_seconds": ttl_secs }));
        warnings.push(
            "reservations are advisory and RAM-local: they are lost on server restart".into(),
        );
        let summary = format!(
            "reserved {} until {} for agent '{}'",
            node_id.0,
            reservation.expires_at.to_rfc3339(),
            reservation.agent_id.0
        );
        let mut out = CallToolResult::success(vec![ContentBlock::text(summary.clone())]);
        attach_warnings(&mut out, &warnings);
        out.structured_content = Some(json!({
            "summary": summary,
            "node_id": node_id.0.to_string(),
            "agent_id": reservation.agent_id.0,
            "expires_at": reservation.expires_at.to_rfc3339(),
            "warnings": warnings,
        }));
        out
    }

    async fn inspect_impl(&self, p: InspectParams) -> CallToolResult {
        if let Err(e) = self.check_agent_id(&p.agent_id) {
            return e;
        }
        let mut warnings: Vec<String> = Vec::new();
        if p.focus.trim().is_empty() {
            return bad_param("focus must be a non-empty string");
        }
        if let Err(e) = check_size("focus", &p.focus) {
            return e;
        }
        let depth = p.depth.unwrap_or(2);
        if depth > MAX_INSPECT_DEPTH {
            return bad_param(format!("depth must be in 0..={MAX_INSPECT_DEPTH}"));
        }

        // One short read section, no `.await` inside (spec §6.4).
        let resolved = {
            let g = self.mem.graph().read();
            match resolve_focus(&g, p.focus.trim()) {
                Focus::Exact(id) => Ok((id, None, render_neighbourhood(&g, id, depth))),
                Focus::Fuzzy {
                    id,
                    content,
                    bounded,
                } => {
                    // A fuzzy resolution is stated in the *text*, not only in a
                    // warning: "focus: <something the caller did not ask for>"
                    // is exactly the line a model reads straight past. A
                    // bounded one also names the bound (issue #9), so the
                    // caller knows what was not scanned.
                    let note = match bounded {
                        None => format!(
                            "resolved '{}' → '{}' (substring match, single candidate)",
                            p.focus.trim(),
                            content
                        ),
                        Some(b) => format!(
                            "resolved '{}' → '{}' (substring match within the bounded scan \
                             of {} concepts; this graph is past the {}-concept full-scan cap, \
                             so concepts outside the subset were not scanned)",
                            p.focus.trim(),
                            content,
                            b.scanned,
                            MAX_INSPECT_SCAN_CONCEPTS
                        ),
                    };
                    Ok((id, Some(note), render_neighbourhood(&g, id, depth)))
                }
                other => Err(other),
            }
        };

        let (focus_id, note, (text, structured)) = match resolved {
            Ok(v) => v,
            Err(Focus::Ambiguous {
                candidates,
                bounded,
            }) => {
                // Refuse rather than pick (R1/T82-7): an arbitrary pick fed a
                // node_id the caller never named into `lambo_reserve` and into
                // edits, and the pick changed between calls.
                let mut msg = match bounded {
                    None => format!(
                        "lambo_inspect: '{}' matches {} concepts — name one exactly, or pass \
                         its node_id:",
                        p.focus.trim(),
                        candidates.len()
                    ),
                    Some(b) => format!(
                        "lambo_inspect: '{}' matches {} concepts within the bounded scan of {} \
                         concepts (this graph is past the {}-concept full-scan cap); name one \
                         exactly, or pass its node_id:",
                        p.focus.trim(),
                        candidates.len(),
                        b.scanned,
                        MAX_INSPECT_SCAN_CONCEPTS
                    ),
                };
                for c in candidates.iter().take(MAX_INSPECT_CANDIDATES) {
                    msg.push_str(&format!("\n  {} [{}]", c.content, c.id.0));
                }
                if candidates.len() > MAX_INSPECT_CANDIDATES {
                    msg.push_str(&format!(
                        "\n  … and {} more",
                        candidates.len() - MAX_INSPECT_CANDIDATES
                    ));
                }
                // Issue #9: every inspect failure names its mode and its focus
                // in the ledger facts BEFORE the error returns, so telemetry
                // can classify the refusal without reprobing.
                note_facts(|| {
                    json!({
                        "failure": "ambiguous",
                        "focus": focus_for_ledger(&p.focus),
                    })
                });
                return CallToolResult::error(vec![ContentBlock::text(msg)]);
            }
            Err(Focus::Oversized { cap, near }) => {
                // T8.7 residual #3 graph-size guard, relaxed by issue #9: the
                // O(total-content) pass is still refused, but the bounded
                // subset was scanned and matched nothing. The refusal says so
                // and names the bound, because the concept the caller meant
                // may exist outside the subset; exact / node-id focus still
                // works. The near-match remediation survives past the cap,
                // ranked within the subset only and announced as such.
                note_facts(|| {
                    json!({
                        "failure": "oversized",
                        "focus": focus_for_ledger(&p.focus),
                    })
                });
                let mut msg = format!(
                    "lambo_inspect: this session's graph has more than {cap} concepts; the \
                     fuzzy pass scanned only the bounded subset (the \
                     {MAX_INSPECT_BOUNDED_SCAN} most recently created plus the \
                     {MAX_INSPECT_BOUNDED_SCAN} highest blast-radius concepts) and matched \
                     nothing; pass a node_id or an exact concept instead"
                );
                if !near.is_empty() {
                    msg.push_str(
                        "\nnearest within the bounded subset (suggestions, not matches; \
                         pass a node_id or name one exactly):",
                    );
                    for c in near.iter().take(MAX_INSPECT_CANDIDATES) {
                        msg.push_str(&format!("\n  {} [{}]", c.content, c.id.0));
                    }
                }
                return CallToolResult::error(vec![ContentBlock::text(msg)]);
            }
            Err(Focus::Missing { near }) => {
                // Issue #9: the bare "no concept matching" refusal is how 22
                // of 78 dogfood-rig inspects failed. Do what Ambiguous does:
                // refuse, explain, and offer the closest concepts with their
                // node ids, announced as suggestions.
                let mut msg = format!(
                    "lambo_inspect: no concept matching '{}' in session '{}'",
                    p.focus,
                    self.mem.session().0
                );
                if !near.is_empty() {
                    msg.push_str(
                        "\nsuggestions (not matches; pass a node_id or name one exactly):",
                    );
                    for c in near.iter().take(MAX_INSPECT_CANDIDATES) {
                        msg.push_str(&format!("\n  {} [{}]", c.content, c.id.0));
                    }
                }
                note_facts(|| {
                    json!({
                        "failure": "missing",
                        "focus": focus_for_ledger(&p.focus),
                    })
                });
                return CallToolResult::error(vec![ContentBlock::text(msg)]);
            }
            // Unreachable: Exact and Fuzzy are resolved in the read section
            // above, and `Focus` has no further variants.
            Err(Focus::Exact(_) | Focus::Fuzzy { .. }) => {
                unreachable!("Exact and Fuzzy resolve in the read section")
            }
        };

        // A fuzzy resolution is stated in the *text*, not only in a warning:
        // "focus: <something the caller did not ask for>" is exactly the line a
        // model reads straight past.
        let text = match &note {
            Some(n) => {
                warnings.push(n.clone());
                format!("{n}\n{text}")
            }
            None => text,
        };

        // I1: enough to place an inspect in a call sequence without carrying the
        // whole neighbourhood into the ledger. `fuzzy` is worth a field — a
        // resolution the caller did not ask for is exactly the friction
        // DOGFOOD metric 6 is looking for.
        note_facts(|| json!({ "depth": depth, "fuzzy": note.is_some() }));

        // Issue #30: an inspect that resolved returned its focus concept to the
        // caller — an access. The focus only: the neighbourhood is context
        // around what was asked for (depth up to 5 can reach hundreds of
        // concepts), and a refusal (ambiguous / missing / oversized) returned
        // no concept at all.
        self.mem.note_accesses([focus_id]);

        let mut out = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
        attach_warnings(&mut out, &warnings);
        out.structured_content = Some(json!({
            "view": text,
            "focus": structured,
            "resolution": note,
            "warnings": warnings,
        }));
        out
    }

    async fn saints_impl(&self, p: SaintsParams) -> CallToolResult {
        if let Err(e) = self.check_agent_id(&p.agent_id) {
            return e;
        }
        // J1-R1-3: no warning is reachable here — see `derive_impl`.
        let saints = self.mem.canonical_memories();
        note_facts(|| json!({ "canonical_count": saints.len() }));
        let mut text = format!(
            "{} canonical memor{} in session '{}'\n",
            saints.len(),
            if saints.len() == 1 { "y" } else { "ies" },
            self.mem.session().0
        );
        for s in &saints {
            text.push_str(&format!(
                "  {} [{:?}, canonical]  blast_radius={}  accesses={}  since {}\n",
                s.content,
                s.concept_type,
                s.blast_radius,
                s.access_count,
                s.created_at.to_rfc3339()
            ));
        }
        let rows: Vec<_> = saints
            .iter()
            .map(|s| {
                json!({
                    "node_id": s.node_id.0.to_string(),
                    "content": s.content,
                    "concept_type": s.concept_type,
                    "blast_radius": s.blast_radius,
                    "access_count": s.access_count,
                    "created_at": s.created_at.to_rfc3339(),
                })
            })
            .collect();
        let mut out = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
        out.structured_content = Some(json!({
            "summary": text,
            "saints": rows,
            "warnings": [],
        }));
        out
    }

    async fn stats_impl(&self, p: StatsParams) -> CallToolResult {
        if let Err(e) = self.check_agent_id(&p.agent_id) {
            return e;
        }
        // J1-R1-3: no warning is reachable here — see `derive_impl`.
        //
        // **The receipt is resolved FIRST, and the ordering is load-bearing**
        // (found by measuring the shipped binary, not by a test): a `wait_ms`
        // blocks for up to `RECEIPT_WAIT_MAX`, so a snapshot taken before it
        // describes the session as it was before the write the caller was
        // waiting for. That reported `write_queue_applied: 0` and
        // `concept_count: 0` beside a receipt that said `applied` — a payload
        // contradicting itself. Everything below reads the session after the
        // wait.
        let receipt = match &p.receipt {
            None => None,
            Some(raw) => {
                let id = match raw.trim().parse::<crate::writeq::ReceiptId>() {
                    Ok(id) => id,
                    Err(_) => {
                        return bad_param(
                            "receipt must be a receipt id from a lambo_derive or \
                             lambo_record_action ack",
                        )
                    }
                };
                let acting = match self.caller_agent(&p.agent_id) {
                    Ok(a) => a,
                    Err(e) => return e,
                };
                let queue = self.mem.pipeline();
                let answer = match p.wait_ms {
                    // A wait of 0 is a fetch, not a wait; both go through the
                    // same clamp so the difference is only the budget.
                    Some(ms) if ms > 0 => {
                        queue
                            .wait(&acting, id, std::time::Duration::from_millis(ms))
                            .await
                    }
                    _ => queue.lookup(&acting, id),
                };
                // Delivered here, explicitly, so `answered`'s piggyback does
                // not state the same outcome a second time in the same response
                // (J3-R1-9). Take-once is unaffected — this *is* the take.
                if answer.is_settled() {
                    queue.mark_delivered(&acting, id);
                }
                Some((id, answer))
            }
        };

        let s = self.mem.stats();
        // One GC reading for both halves of the answer (see `stats_json_with_gc`).
        let gc = self.mem.gc_stats();
        let text = format!(
            "session '{}' (owner agent '{}')\n\
             nodes={} edges={} concepts={} canonical={}\n\
             embedded={}/{}\n\
             flush_lag={:?} log_depth={} flush_depth={} dead_lettered={} degraded={}\n\
             epoch={} daemon_cycles={} canonization_cycles={} canonization_failures={}\n\
             promotion_policy={}\n\
             {}",
            s.session.0,
            s.agent.0,
            s.node_count,
            s.edge_count,
            s.concept_count,
            s.canonical_count,
            s.embedded_concepts,
            s.concept_count,
            s.flush_lag,
            s.log_depth,
            s.flush_depth,
            s.dead_lettered,
            s.degraded,
            s.epoch,
            s.daemon_cycles,
            s.canonization_cycles,
            s.canonization_failures,
            // The text half of the same answer, on its own line: an operator
            // reading the tool output should not have to open the structured
            // payload to learn which policy the cycle counts above belong to.
            self.mem.config().promotion_policy.as_str(),
            gc_summary_line(&gc),
        );
        // One payload builder shared with the I2 heartbeat, so a heartbeat can
        // never report different numbers than the tool. With `--ledger` off
        // this is exactly the payload it always was; with it on, the six
        // `ledger_*` keys are appended (I1: the dropped-line counter has to be
        // reachable from `lambo_stats`, or silence is invisible).
        let mut payload = self.stats_json_with_gc(&gc);

        let mut lines = vec![text.clone()];
        if let Some((id, answer)) = &receipt {
            // Redacted like every other model-facing string (N3): a `failed`
            // answer carries a `LamboError`, and a store error can name a DSN.
            lines.push(format!("receipt {id}: {}", redact_urls(&answer.describe())));
        }
        let text = lines.join("\n");

        {
            let obj = payload.as_object_mut().expect("stats_json built an object");
            obj.insert("summary".into(), json!(text));
            obj.insert("warnings".into(), json!([]));
            if let Some((id, answer)) = &receipt {
                let mut r = json!({
                    "id": id.to_string(),
                    "state": answer.tag(),
                    "detail": redact_urls(&answer.describe()),
                });
                // The node ids the ack could not report. Present only on
                // `applied`, and absent — not empty — otherwise, for the same
                // reason the ack omits them: an empty list would be a claim.
                // An agent that needs a node id to reserve it gets it here.
                if let ReceiptAnswer::Applied(s) = answer {
                    let obj = r.as_object_mut().expect("json! built an object");
                    obj.insert("kind".into(), json!(s.kind.tool()));
                    obj.insert("created".into(), json!(s.created));
                    obj.insert("matched".into(), json!(s.matched));
                    // I1's DOGFOOD metric 2 fact set, relocated here from the
                    // ledger call line — see `AppliedSummary`. The `_count`
                    // pair is the TRUE count beside a list truncated at
                    // MAX_RECEIPT_IDS; the other three are emitted only for the
                    // write kind that has one, so an absent key never reads as
                    // a zero.
                    for (key, value) in [
                        ("created_count", Some(s.created_count)),
                        ("matched_count", Some(s.matched_count)),
                        ("semantic_merged", s.semantic_merged),
                        ("reinforced", s.reinforced),
                        ("edges", s.edges),
                        // Applied ≠ embedded (J3-R3-1): present only for the
                        // hybrid strategy (derive and record_action), so an
                        // agent can see a write that applied without its
                        // vector instead of reading `applied` as embedded.
                        ("embedded", s.embedded),
                    ] {
                        if let Some(v) = value {
                            obj.insert(key.into(), json!(v));
                        }
                    }
                }
                obj.insert("receipt".into(), r);
            }
        }

        let mut out = CallToolResult::success(vec![ContentBlock::text(text)]);
        out.structured_content = Some(payload);
        out
    }
}

// `router = self.tool_router` on purpose: the macro's default is
// `Self::tool_router()`, which **rebuilds the whole router — every tool's JSON
// schema included — on every `tools/list` and every `tools/call`**. Pointing it
// at the field built once in `new()` keeps per-call work to a map lookup.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for LamboServer {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` / `Implementation` are `#[non_exhaustive]`: start from
        // the SDK's default (which carries the negotiated protocol version) and
        // set only what is ours.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new("lambo", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(format!(
            "Lambo agentic graph memory for session '{}'. Call lambo_recall before \
                 acting on a task to load relevant prior memory, lambo_derive and \
                 lambo_record_action to write what you learned and did, lambo_reserve \
                 before editing a shared concept, and lambo_inspect / lambo_saints / \
                 lambo_stats to look around. Every tool takes your agent_id: it is \
                 caller-asserted and unverified, so send one stable id of your own — \
                 work is recorded under it, soft locks are held under it, distinct ids \
                 get distinct locks, and callers sharing an id share locks. created_at is \
                 server-stamped: do not send a client flush timestamp. lambo_derive and \
                 lambo_record_action may take optional RFC3339 event_time for historical \
                 about-time, such as a commit or document date; omit it for a live fact, \
                 which is about now. Ordering is yours to manage, and \
                 writes are applied in the BACKGROUND: lambo_derive and \
                 lambo_record_action return once your input is validated and ordered, \
                 and their ack carries a receipt id. Their outcome arrives on your next \
                 tool response. If you need a write visible to the very next read, call \
                 lambo_stats with that receipt and a wait_ms first; otherwise carry on \
                 — writes you send one after another are applied in that order (two you \
                 fire at once have no order to keep).",
            self.mem.session().0
        ));
        info
    }
}

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod tests;

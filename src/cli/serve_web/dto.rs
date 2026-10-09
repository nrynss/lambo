//! The portal's wire types: what each route returns and the query strings it
//! accepts. Serialization shapes only; the reads that fill them are in
//! `projections` and the handlers in `routes`.

use serde::{Deserialize, Serialize};

use crate::canon::{GateProgress, PromotionPolicy};
use crate::resolve::{
    embedding_mismatch_error, session_embedding_compatibility, SessionEmbeddingCompatibility,
};
use crate::types::{ConceptType, EmbeddingContract};

/// Session identity, backend kinds, and embedding-space compatibility.
///
/// Backend connectivity remains deliberately hidden: `StoreConfig::dsn`,
/// `StoreConfig::path` and `EmbedderConfig::llama_url` are credentials or
/// internal topology and never appear here. H1 intentionally includes the
/// stored and configured model identifiers because a mismatch warning that
/// cannot name the two spaces is not actionable. `StoreConfig`'s own `Debug`
/// redacts the DSN for the same reason.
#[derive(Clone, Debug, Serialize)]
pub(super) struct SessionInfo {
    pub(super) session: String,
    pub(super) store: String,
    pub(super) embedder: String,
    pub(super) embedding_dim: usize,
    pub(super) vector_search: bool,
    /// Stored-vs-configured embedding identity. A mismatch leaves structural
    /// routes available but makes vector recall fail closed.
    pub(super) embedding_contract: EmbeddingStatus,
    /// Always `"reader"` — this process holds no writer lease.
    pub(super) mode: &'static str,
    /// Always `true`. The router registers `GET` routes only.
    pub(super) read_only: bool,
    /// The in-RAM store is per-process: a reader cannot see another process's
    /// writes through it. Surfaced so the page can say so instead of looking broken.
    pub(super) store_is_process_local: bool,
    /// `--bind` reaches beyond loopback. Such a bind always requires a bearer
    /// token; the page can only reach this surface through an authenticated
    /// proxy or a client that sends the token.
    pub(super) exposed_beyond_loopback: bool,
    pub(super) poll_interval_ms: u64,
    pub(super) version: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct EmbeddingStatus {
    /// `unrecorded`, `compatible`, or `mismatch`.
    pub(super) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) stored: Option<EmbeddingContract>,
    pub(super) configured: EmbeddingContract,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) message: Option<String>,
}

impl EmbeddingStatus {
    pub(super) fn inspect(
        stored: Option<&EmbeddingContract>,
        configured: &EmbeddingContract,
    ) -> Self {
        match session_embedding_compatibility(stored, configured) {
            SessionEmbeddingCompatibility::Unrecorded => Self {
                status: "unrecorded",
                stored: None,
                configured: configured.clone(),
                message: None,
            },
            SessionEmbeddingCompatibility::Compatible => Self {
                status: "compatible",
                stored: stored.cloned(),
                configured: configured.clone(),
                message: None,
            },
            SessionEmbeddingCompatibility::Mismatch { stored, live } => Self {
                status: "mismatch",
                message: Some(embedding_mismatch_error(&stored, &live).to_string()),
                stored: Some(stored),
                configured: live,
            },
        }
    }
    /// E2E-8: only a `compatible` contract means the vector leg can actually
    /// return candidates. `unrecorded` (legacy) sessions had their vectors
    /// quarantined at load and the checked read returns an empty pool for an
    /// unstamped durable contract — the flag must not say the leg is on when
    /// it returns nothing. `mismatch` is refused by the checked read. The
    /// `status` field itself keeps its `unrecorded|compatible|mismatch`
    /// semantics (H1's banner logic depends on it); only this derived flag
    /// tightens.
    pub(super) fn vector_search_trusted(&self) -> bool {
        self.status == "compatible"
    }
}

/// One canonization transition, as the writer durably recorded it.
#[derive(Clone, Debug, Serialize)]
pub(super) struct WebEvent {
    /// Position in the session's ordered event list — the poll cursor.
    pub(super) seq: usize,
    pub(super) occurred_at: String,
    pub(super) node_id: String,
    /// `None` when the concept is no longer in the snapshot (GC'd since).
    pub(super) content: Option<String>,
    pub(super) from_status: &'static str,
    pub(super) to_status: &'static str,
    pub(super) blast_radius: Option<i32>,
}

#[derive(Debug, Serialize)]
pub(super) struct EventsPayload {
    /// Every transition recorded for the session.
    pub(super) total: usize,
    /// The cursor this response answered.
    pub(super) since: usize,
    pub(super) events: Vec<WebEvent>,
}

#[derive(Debug, Serialize)]
pub(super) struct WebStats {
    pub(super) session: String,
    pub(super) nodes: usize,
    pub(super) edges: usize,
    pub(super) concepts: usize,
    pub(super) canonical: usize,
    pub(super) canonization_events: usize,
    /// `Some` when a writer has published flush stats into the shared store
    /// (T85-3); `null` (rendered `n/a`) when no writer has yet, or the store
    /// doesn't support it — never a fabricated `0`.
    pub(super) flush_lag_ms: Option<u64>,
    /// Same: writer-published log depth, or `null`/`n/a` when absent.
    pub(super) log_depth: Option<usize>,
    /// How long the durable counts above have been unchanged, as seen here.
    pub(super) durable_change_age_ms: u64,
    pub(super) mode: &'static str,
    pub(super) writer_only: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct Pulse {
    pub(super) stats: WebStats,
    pub(super) events: EventsPayload,
    pub(super) embedding_contract: EmbeddingStatus,
    pub(super) vector_search: bool,
}

pub(super) struct StatsRead {
    pub(super) stats: WebStats,
    pub(super) embedding_status: EmbeddingStatus,
}

#[derive(Debug, Serialize)]
pub(super) struct RecallResponse {
    pub(super) session: String,
    pub(super) query: String,
    /// The T5.3 context block **verbatim** — canonical markers, `⚑` warnings
    /// and conflict lines exactly as an agent would receive them. Byte-equal
    /// to `lambo recall` for the same execution (H3: both project from the
    /// same `run_detailed` call).
    pub(super) context: String,
    pub(super) elapsed_ms: u64,
    /// H3: every ranked hit, with full status, `included_in_context` and the
    /// hit's typed annotations.
    pub(super) hits: Vec<crate::recall::detail::DetailedHit>,
    /// H3: response-global explanations (`traversal`, `vector_degraded`) in
    /// producer order.
    pub(super) response_annotations: Vec<crate::recall::detail::Annotation>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RecallParams {
    pub(super) q: Option<String>,
    pub(super) top_k: Option<usize>,
    pub(super) max_tokens: Option<usize>,
    pub(super) traversal_depth: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(super) struct SinceParams {
    pub(super) since: Option<usize>,
}

pub(super) const WRITER_ONLY: &str =
    "flush_lag / log_depth / daemon_cycles live in the writer process; \
                           this is a lease-free reader and cannot observe them";

#[derive(Debug, Deserialize)]
pub(super) struct InspectParams {
    pub(super) focus: String,
    /// Accepted for CLI parity but deliberately ignored (treated as 1): the
    /// page needs hop 1 only, per the /api/inspect contract.
    #[allow(dead_code)]
    pub(super) depth: Option<usize>,
}

/// One hop-1 structural neighbour of the focus — a thing the focus stands
/// behind, or a thing behind it. Structural edges only
/// (`Dependency`/`Causal`/`Hierarchical`), which is what keeps the false
/// `CoOccurrence` edge off the page (T7).
#[derive(Debug, Serialize)]
pub(super) struct InspectDependent {
    pub(super) content: String,
    pub(super) concept_type: ConceptType,
    pub(super) edge: String,
}

#[derive(Debug, Serialize)]
pub(super) struct InspectResponse {
    pub(super) focus: String,
    pub(super) found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) status: Option<&'static str>,
    pub(super) blast_radius: u64,
    pub(super) dependents: Vec<InspectDependent>,
    /// `true` when the dependents array hit the bound; always present (a miss
    /// already says so via `found: false`).
    pub(super) truncated: bool,
    /// C2: the promotion policy **this reader process resolved**, always
    /// present, including on a miss (it is a property of the process, not of
    /// the concept).
    ///
    /// It is here because `serve-web` is a lease-free reader that runs its own
    /// `resolve_for_command`, and there is no channel — no lease-row column, no
    /// session-endpoint field — carrying the writer's policy to it. So a
    /// `lambo serve` started from a systemd unit with
    /// `Environment=LAMBO_PROMOTION_POLICY=Solo` and a `lambo serve-web` opened
    /// by hand in a shell that does not export it are two processes with two
    /// answers, and the page would silently render the reader's. Naming the
    /// resolution the numbers came from is what makes that mismatch *visible*
    /// instead of a wrong gate count; closing it needs the two processes to
    /// resolve the same config, which is an operator requirement the docs state
    /// rather than something this payload can enforce.
    pub(super) promotion_policy: PromotionPolicy,
    /// T11: how close this concept is to canonization under
    /// `promotion_policy`, additive beside status/radius.
    ///
    /// Absent in exactly two situations, and `gate_progress_omitted` says which
    /// — a client must never have to guess. Under `Solo` it is **present**,
    /// carrying the policy label and the cooldown with swarm's four gates left
    /// out (see [`GateProgress`]); an absent block is never a policy statement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) gate_progress: Option<GateProgress>,
    /// Why `gate_progress` is absent, when it is and the focus was found.
    ///
    /// `"already_canonical"` — H2: a promoted fact has no promotion gates left
    /// to explain, and the block's aged-basis figures contradicted the live
    /// status/blast_radius. `"unavailable"` — the store read behind the gates
    /// failed; this additive payload degrades to a labelled absence rather than
    /// failing the endpoint the page loads on.
    ///
    /// The label exists because those two used to be one indistinguishable
    /// null, and the page rendered "no gates" identically for a canonized
    /// concept and for a broken store query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) gate_progress_omitted: Option<&'static str>,
}

impl InspectResponse {
    /// A miss is a 200 with `found: false` — never a non-2xx (the page says
    /// "nothing depends on this" without rendering an error).
    ///
    /// `gate_progress_omitted` stays `None` here: `found: false` already
    /// explains the absence, and there is no concept whose gates went missing.
    pub(super) fn missing(focus: String, promotion_policy: PromotionPolicy) -> Self {
        Self {
            focus,
            found: false,
            status: None,
            blast_radius: 0,
            dependents: Vec::new(),
            truncated: false,
            promotion_policy,
            gate_progress: None,
            gate_progress_omitted: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct GraphEdge {
    pub(super) parent: String,
    pub(super) child: String,
    pub(super) edge: String,
}

#[derive(Debug, Serialize)]
pub(super) struct GraphNode {
    pub(super) content: String,
    pub(super) concept_type: ConceptType,
    pub(super) status: &'static str,
    /// The live dependent count (same helper `/api/inspect` uses), so the
    /// tree marks load-bearing Candidates/Venerables — not just promoted
    /// Canonicals whose frozen `blast_radius` column happens to be `Some`.
    pub(super) blast_radius: u64,
}

#[derive(Debug, Serialize)]
pub(super) struct GraphResponse {
    pub(super) session: String,
    pub(super) nodes: Vec<GraphNode>,
    pub(super) edges: Vec<GraphEdge>,
    pub(super) truncated: bool,
}

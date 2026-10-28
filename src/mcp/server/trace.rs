//! The I1 call ledger's per-call trace: the task-local slot a tool body
//! writes its facts and error class into, the recall facts builder, the
//! ledger's truncation caps, and [`LamboServer::observed`], which appends one
//! line per call when a ledger is attached.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use rmcp::model::CallToolResult;
use serde_json::json;

use super::response::contain_panic;
use super::LamboServer;
use crate::recall::detail::AnnotationKind;

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
    /// Set by [`super::response::bad_param`] / [`super::response::tool_err`] /
    /// [`contain_panic`] on their way out.
    error_kind: Option<&'static str>,
    /// Per-tool payload facts, merged into the ledger line at the top level.
    facts: Option<serde_json::Value>,
}

tokio::task_local! {
    /// Established by [`LamboServer::observed`] for the duration of one tool
    /// call, and only when a ledger is listening.
    static TRACE: std::cell::RefCell<CallTrace>;

    /// The configured credential the current tool call arrived as, set by
    /// `call_tool` for the whole call (#32 PR 5 review I6).
    static CALLER: Option<Arc<str>>;

    /// How the current tool call arrived (#33 PR 3), set by `call_tool`
    /// for the whole call; a layered call checks it against each layer.
    static AUTH: Option<CallAuth>;
}

/// The configured `[[serve.credential]]` an HTTP request authenticated as,
/// by name, never by token (#32 PR 5 review I6). The serve's guard puts it
/// in the request's extensions, rmcp hands the request parts to the tool
/// call, and a ledgered call line carries it as `credential`.
///
/// Set only for a configured credential: the legacy `default` and the
/// implicit `local` leave no mark, so a serve that uses only those (a
/// one-session serve, the dogfood rig) writes byte-for-byte the lines it
/// always wrote, and a line without the field means one of those two (or
/// stdio).
#[derive(Clone, Debug)]
pub(crate) struct CallCredential(pub(crate) Arc<str>);

/// The grant an HTTP request authenticated as, whatever its name (#33 PR
/// 3): the serve's guard puts it in the request's extensions beside
/// [`CallCredential`], for a layered recall's per-layer authorization.
#[derive(Clone, Debug)]
pub(crate) struct CallGrant(pub(crate) Arc<crate::surface::session::SessionGrant>);

/// How a tool call arrived, for a layered call's per-layer check (#33 PR
/// 3 review L2). A call with no request parts at all (the J2 session
/// endpoint, stdio, a library caller) has none.
#[derive(Clone, Debug)]
pub(crate) enum CallAuth {
    /// An HTTP request, with the grant the guard resolved for it. `None`
    /// means the parts carried no grant (a router served without the
    /// guard), which a layered call refuses rather than trusts.
    Http(Option<Arc<crate::surface::session::SessionGrant>>),
}

/// Run one tool call with `caller` as its credential and `auth` as how it
/// arrived.
pub(super) async fn with_caller<F: Future>(
    caller: Option<Arc<str>>,
    auth: Option<CallAuth>,
    call: F,
) -> F::Output {
    CALLER.scope(caller, AUTH.scope(auth, call)).await
}

/// The current call's configured credential, if any.
fn caller() -> Option<Arc<str>> {
    CALLER.try_with(Clone::clone).ok().flatten()
}

/// How the current call arrived; `None` without request parts.
pub(super) fn call_auth() -> Option<CallAuth> {
    AUTH.try_with(Clone::clone).ok().flatten()
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
/// `fusion` (#173, additive) carries the hit's 1-based rank in each leg it
/// came from (`bm25_rank`, `recent_rank`, `vector_rank`) and both fused
/// values, `max` and `rrf` (see [`crate::recall::fusion`]), whichever of the
/// two ranked the recall, so one kind can be shadow-scored while the other
/// runs. It is absent where `legs` is empty.
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
/// `skipped_redundant` (#181, additive) is on a hit that `[selection]
/// skip_redundant` skipped members for: each skipped member's `node_id`
/// and the `signal` that found it redundant (`same_content`,
/// `semantic_edge`, `cosine`). It is absent everywhere else.
///
/// `cold_start` (#79) says the recall was ranked in cold mode: a fresh concept
/// the daemon had not scored yet would be shown, so every hit's `score` is
/// `w_query × q` with no daemon share. It is always present on a recall line
/// (an additive key; `v` stays 1), so a ledger written by a binary with it
/// tells `false` apart from "not recorded", which a line without the key is.
///
/// # What the five set-level flags mean, and why they are not all computed alike
///
/// The spec's word is **rendered**, and the honest answer differs by kind
/// because the two rendering paths differ:
///
/// * **`canonical_marker` is budget-gated.** `[canonical]` exists *only* inside a
///   hit's context block (`format::render_block`), and the block is emitted only
///   while the token budget lasts. So this flag counts hits with
///   `included_in_context == true`, plus a truncated first block (#130)
///   whose cut kept its label line, and nothing else. A `max_tokens: 1`
///   recall of a Canonical concept therefore reports `canonical_marker:
///   false` — correctly:
///   the agent received an empty context and the string `[canonical]` appeared
///   nowhere in the response. (Per-hit `is_canonical` is still on every hit, so
///   "was a Canonical concept *returned*" remains answerable — from the hits,
///   which is where a set-level flag cannot honestly answer it.)
/// * **The warning flags are budget-independent** —
///   `blast_radius_warning`, `conflict_line`, `hot_warning`,
///   `reservation_warning` and `contradiction_warning` (#19(c), additive). Their lines are pushed into the flat `warnings`
///   vector for *every* returned hit regardless of the budget
///   (`assemble.rs`: "a block truncated from the context still reports its
///   conditions") and delivered to the agent as a second text block. The line
///   reached the agent even when the block did not, so these are computed over
///   every returned hit. Per-hit `included_in_context` is what tells a consumer
///   whether the *block* the warning was about was also there — which is the
///   distinction `warnings.py` reports.
pub(super) fn recall_facts(
    query: &str,
    top_k: usize,
    detailed: &crate::recall::detail::DetailedRecall,
) -> serde_json::Value {
    recall_facts_with(
        query,
        top_k,
        detailed,
        |_, hit| detailed.legs.get(&hit.node_id).copied(),
        None,
    )
}

/// [`recall_facts`] for a layered session's recall (#33 PR 3): every hit
/// line also names its `session`, and the line carries `session_hits`, the
/// hit count per session (a map keyed by session, so N tiers fit). Legs
/// come from [`LayeredRecall`](crate::memory::LayeredRecall)'s per-hit
/// list, by position: node ids are unique within a session, not across
/// sessions, so the merged `legs` map holds the caller's session only.
/// Additive keys; `v` stays 1.
pub(super) fn layered_recall_facts(
    query: &str,
    top_k: usize,
    recalled: &crate::memory::LayeredRecall,
) -> serde_json::Value {
    let sessions: Vec<&str> = recalled.hits.iter().map(|h| h.session.as_str()).collect();
    let mut facts = recall_facts_with(
        query,
        top_k,
        &recalled.detail,
        |i, _| recalled.hit_legs.get(i).copied().flatten(),
        Some(&sessions),
    );
    // #181: additive, on a line whose hit's merged copies had members
    // skipped as redundant with them (session-qualified).
    if let Some(lines) = facts.get_mut("hits").and_then(|h| h.as_array_mut()) {
        for (line, hit) in lines.iter_mut().zip(&recalled.hits) {
            if let (Some(skips), Some(map)) = (
                crate::recall::redundancy::copy_skips_json(&hit.also),
                line.as_object_mut(),
            ) {
                map.insert("also_skipped_redundant".into(), skips);
            }
        }
    }
    facts
}

/// The recall line's facts, with each hit's legs from `legs_of(position,
/// hit)` and, for a layered session, each hit's session by position.
fn recall_facts_with(
    query: &str,
    top_k: usize,
    detailed: &crate::recall::detail::DetailedRecall,
    legs_of: impl Fn(usize, &crate::types::RecallHit) -> Option<crate::recall::candidates::LegScores>,
    sessions: Option<&[&str]>,
) -> serde_json::Value {
    let mut canonical_marker = false;
    let mut blast_radius_warning = false;
    let mut conflict_line = false;
    let mut hot_warning = false;
    let mut reservation_warning = false;
    let mut contradiction_warning = false;
    let mut possibly_superseded = false;

    // #130: the truncated first hit, if any, and whether its cut kept the
    // label line (and so `[…, canonical]`).
    let truncated_label = detailed
        .truncated_hit()
        .is_some_and(|(_, form)| form.keeps_label());
    let hits: Vec<serde_json::Value> = detailed
        .hits
        .iter()
        .zip(detailed.detailed.iter())
        .enumerate()
        .map(|(i, (hit, d))| {
            // Budget-gated: `[canonical]` renders inside the hit's block, so a
            // hit the budget cut rendered no marker. See this function's docs.
            // A truncated first block (#130) renders it when the cut kept the
            // label line, which is every form but content-only.
            canonical_marker |=
                hit.is_canonical && (d.included_in_context || (i == 0 && truncated_label));
            let mut legs = serde_json::Map::new();
            let mut fusion = serde_json::Map::new();
            if let Some(l) = legs_of(i, hit) {
                // #173: the leg ranks and both fused values, whichever ranked,
                // so a ledger can shadow-score the kind that did not.
                for (name, rank) in [
                    ("bm25_rank", l.keyword_rank),
                    ("recent_rank", l.recent_rank),
                    ("vector_rank", l.vector_rank),
                ] {
                    if let Some(r) = rank {
                        fusion.insert(name.into(), json!(r));
                    }
                }
                if l.keyword.is_some() || l.recent.is_some() || l.vector.is_some() {
                    fusion.insert("max".into(), json!(l.merged()));
                }
                if let Some(r) = l.rrf {
                    fusion.insert("rrf".into(), json!(r));
                }
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
            // Deliberately NOT gated on `included_in_context`: these lines
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
                    AnnotationKind::Contradiction => {
                        contradiction_warning = true;
                        kinds.push("contradiction");
                    }
                    AnnotationKind::Contradicts => kinds.push("contradicts"),
                    AnnotationKind::Superseded => kinds.push("superseded"),
                    AnnotationKind::RestsOnContradicted => kinds.push("rests_on_contradicted"),
                    AnnotationKind::PossiblySuperseded => kinds.push("possibly_superseded"),
                    // Response-global kinds are never hit-owned (H3 contract),
                    // so they are reported once, below.
                    AnnotationKind::Traversal
                    | AnnotationKind::VectorDegraded
                    | AnnotationKind::LayerUnavailable => {}
                }
            }
            let mut line = json!({
                "node_id": hit.node_id.0.to_string(),
                "content": truncate_for_ledger(&hit.content),
                "score": hit.score,
                "legs": legs,
                "is_canonical": hit.is_canonical,
                "blast_radius": hit.blast_radius,
                "included_in_context": d.included_in_context,
                "annotations": kinds,
            });
            // #130: additive, and only on the truncated hit, so a line
            // without it reads as before (`v` stays 1).
            if d.truncated_in_context
                && let Some(map) = line.as_object_mut()
            {
                map.insert("truncated_in_context".into(), json!(true));
            }
            // #198: additive, and only on a hit a shown newer concept is
            // suspected of replacing, so a line without it reads as before
            // (`v` stays 1). Present whether or not the prose line is shown.
            if let Some(by) = d.possibly_superseded_by
                && let Some(map) = line.as_object_mut()
            {
                possibly_superseded = true;
                map.insert("possibly_superseded_by".into(), json!(by.0.to_string()));
            }
            // #173: additive, and only on a phase-1 hit, so a line without
            // it reads as before (`v` stays 1).
            if !fusion.is_empty()
                && let Some(map) = line.as_object_mut()
            {
                map.insert("fusion".into(), serde_json::Value::Object(fusion));
            }
            // #33: additive, on a layered session's lines only.
            if let Some(session) = sessions.and_then(|s| s.get(i))
                && let Some(map) = line.as_object_mut()
            {
                map.insert("session".into(), json!(session));
            }
            // #181: additive, and only on a hit that members were skipped
            // as redundant with, so a line without it reads as before.
            if !d.skipped_redundant.is_empty()
                && let Some(map) = line.as_object_mut()
            {
                map.insert(
                    "skipped_redundant".into(),
                    crate::recall::redundancy::skips_json(&d.skipped_redundant),
                );
            }
            line
        })
        .collect();

    let response_kinds: Vec<&'static str> = detailed
        .response_annotations
        .iter()
        .map(|a| match a.kind {
            AnnotationKind::Traversal => "traversal",
            AnnotationKind::VectorDegraded => "vector_degraded",
            AnnotationKind::LayerUnavailable => "layer_unavailable",
            AnnotationKind::LoadBearing => "load_bearing",
            AnnotationKind::Conflict => "conflict",
            AnnotationKind::Hot => "hot",
            AnnotationKind::Reservation => "reservation",
            AnnotationKind::Contradiction => "contradiction",
            AnnotationKind::Contradicts => "contradicts",
            AnnotationKind::Superseded => "superseded",
            AnnotationKind::RestsOnContradicted => "rests_on_contradicted",
            AnnotationKind::PossiblySuperseded => "possibly_superseded",
        })
        .collect();

    let mut facts = json!({
        "query": truncate_to(query, LEDGER_QUERY_PREFIX),
        "top_k": top_k,
        "hit_count": detailed.hits.len(),
        "hits": hits,
        "canonical_marker": canonical_marker,
        "blast_radius_warning": blast_radius_warning,
        "conflict_line": conflict_line,
        "hot_warning": hot_warning,
        "reservation_warning": reservation_warning,
        "contradiction_warning": contradiction_warning,
        "response_annotations": response_kinds,
        "warning_count": detailed.warnings.len(),
        "cold_start": detailed.cold_start,
    });
    // #198: additive, only when some hit carries a suspected pair, so a
    // line without one reads as before (`v` stays 1).
    if possibly_superseded && let Some(map) = facts.as_object_mut() {
        map.insert("possibly_superseded".into(), json!(true));
    }
    // #33: per-session hit counts, on a layered session's lines only.
    if let Some(sessions) = sessions
        && let Some(map) = facts.as_object_mut()
    {
        let mut counts = std::collections::BTreeMap::<&str, usize>::new();
        for s in sessions {
            *counts.entry(s).or_default() += 1;
        }
        map.insert("session_hits".into(), json!(counts));
    }
    facts
}

/// Longest concept-text prefix a ledger line carries.
///
/// A concept may be `MAX_CONTENT_BYTES` (16 KiB) and a recall may return
/// [`crate::surface::limits::MAX_TOP_K`] hits, so untruncated text makes a
/// one-megabyte worst-case line.
/// 200 characters names a concept unambiguously in a report and keeps a heavy
/// dogfood day's ledger in the low megabytes.
pub(super) const LEDGER_CONTENT_PREFIX: usize = 200;

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
pub(super) const LEDGER_QUERY_PREFIX: usize = 2000;

/// Compile-time pin on the relationship the two caps' reasoning rests on: the
/// query cap is deliberately the wider of the two, for the reason above. A future
/// edit that narrowed it below the content cap would not fail a test — it would
/// fail the build, here.
const _: () = if LEDGER_QUERY_PREFIX <= LEDGER_CONTENT_PREFIX {
    panic!("the ledger's recall-query cap must stay wider than its concept-text cap");
};

/// `content` truncated to [`LEDGER_CONTENT_PREFIX`] **characters**, with an
/// explicit marker so a consumer never mistakes a cut for the whole text.
pub(super) fn truncate_for_ledger(content: &str) -> String {
    truncate_to(content, LEDGER_CONTENT_PREFIX)
}

/// Longest inspect-focus prefix a ledger line carries.
///
/// The focus is the other client string an inspect line carries (issue #9):
/// recorded on the failure paths so a failed inspect is classifiable from the
/// ledger alone. 200 characters identifies the miss without carrying a 16 KiB
/// paste.
pub(super) const LEDGER_FOCUS_PREFIX: usize = 200;

/// `focus` truncated to [`LEDGER_FOCUS_PREFIX`] **characters**, with an
/// explicit marker so a consumer never mistakes a cut for the whole focus.
pub(super) fn focus_for_ledger(focus: &str) -> String {
    truncate_to(focus, LEDGER_FOCUS_PREFIX)
}

/// `text` truncated to `max` **characters**, with an explicit marker so a
/// consumer never mistakes a cut for the whole string.
///
/// Cut on a `char` boundary, not a byte one: a byte slice through a multi-byte
/// codepoint would panic, and the ledger is not allowed to panic a tool call.
pub(super) fn truncate_to(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((cut, _)) => format!("{}…[truncated]", &text[..cut]),
    }
}

/// Classify the error this call is returning. Outside a ledgered call, a no-op.
pub(super) fn note_error(kind: &'static str) {
    let _ = TRACE.try_with(|t| t.borrow_mut().error_kind = Some(kind));
}

/// Record per-tool payload facts — **lazily**, so the JSON is built only when a
/// ledger will actually consume it.
pub(super) fn note_facts(facts: impl FnOnce() -> serde_json::Value) {
    let _ = TRACE.try_with(|t| t.borrow_mut().facts = Some(facts()));
}

impl LamboServer {
    /// The agent id [`LamboServer::observed`] should stamp on the line — `None`,
    /// and no allocation at all, when there is no ledger to stamp it onto.
    ///
    /// This exists so that "off costs nothing" is true of the *string* too. The
    /// obvious shape (`observed(tool, &p.agent_id, self.foo_impl(p))`) does not
    /// compile: `p` moves into the impl future, so a borrow of `p.agent_id`
    /// cannot outlive the call expression. Deciding here keeps the one clone on
    /// the ledger-on path where it belongs, without duplicating the check across
    /// seven tool wrappers.
    pub(super) fn ledger_agent(&self, agent_id: &str) -> Option<String> {
        self.ledger.as_ref().map(|_| agent_id.to_string())
    }

    /// Run one tool body and, when a ledger is listening, append its line.
    ///
    /// **The ledger never changes what the caller gets.** The result is
    /// returned unmodified; the line is built from it afterwards. The append
    /// itself cannot block or fail (see [`crate::ledger::Ledger::append`]), so a ledger that
    /// is behind, unwritable, or gone costs a tool call nothing but the
    /// microseconds of one `serde_json::to_vec`.
    ///
    /// With no ledger this is `contain_panic` and nothing else — no task-local
    /// scope, no `Instant`, no facts, and (see [`LamboServer::ledger_agent`]) no
    /// copy of the agent id either.
    pub(super) async fn observed(
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
                let mut line = crate::ledger::call_line(
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
                );
                // Additive, and only for a configured credential (see
                // `CallCredential`).
                if let Some(credential) = caller()
                    && let Some(obj) = line.as_object_mut()
                {
                    obj.insert("credential".into(), json!(&*credential));
                }
                ledger.append(&line);
                out
            })
            .await
    }
}

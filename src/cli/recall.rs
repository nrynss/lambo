//! `lambo recall` — lease-free reader process (spec §2.2).
//!
//! Loads the session, builds a [`Daemon`] **without spawning** (spawn would run
//! GC = writer), embeds the query only when the store claims `VECTOR_SEARCH`,
//! and prints the T5.3 context block.
//!
//! H3: the public [`run`] stays a thin wrapper over [`run_detailed`], the
//! single-execution seam that produces BOTH the operator-visible string and
//! the structured presentation model the HTTP `/api/recall` payload is
//! serialized from. The CLI string and the HTTP `context` are the same
//! execution's output by construction.

use super::caps::{
    check_size_cli, clamp_cfg_default, require_nonempty, CliError, MAX_MAX_TOKENS, MAX_TOP_K,
    MAX_TRAVERSAL_DEPTH,
};
use super::load_reader_graph_with_contract;
use crate::config::Config;
use crate::daemon::{Daemon, RecallPipeline};
use crate::recall::cache::RecallCache;
use crate::recall::detail::{Annotation, AnnotationKind, DetailedHit, DetailedRecall};
use crate::resolve::ResolvedBackends;
use crate::store::Capabilities;
use crate::types::RecallQuery;

/// The result of one detailed recall: the full `lambo recall` string plus the
/// H3 presentation model, all from the same execution.
pub(crate) struct CliRecall {
    /// The complete `lambo recall` output — the ⚑ header (if any) above the
    /// rendered context blocks. This is byte-identical to what [`run`]
    /// returns and what `/api/recall` puts in `context`.
    pub(crate) context: String,
    /// The presentation hits, serialized on the wire as `hits`.
    pub(crate) hits: Vec<DetailedHit>,
    /// Response-global annotations in producer order: `vector_degraded`
    /// (query embedding failure, CLI side) first, then the daemon's own
    /// (`traversal` for a dispatched structural query).
    pub(crate) response_annotations: Vec<Annotation>,
}

/// Recall relevant memory for a query.
pub async fn run(
    backends: &ResolvedBackends,
    session: &str,
    query: &str,
    top_k: Option<usize>,
    max_tokens: Option<usize>,
    traversal_depth: Option<usize>,
) -> Result<String, CliError> {
    Ok(
        run_detailed(backends, session, query, top_k, max_tokens, traversal_depth)
            .await?
            .context,
    )
}

/// One recall execution producing the CLI string AND the H3 presentation
/// model. The HTTP endpoint calls this instead of [`run`], so the page's
/// `context` can never drift from `lambo recall` — both project from the
/// same execution's data.
pub(crate) async fn run_detailed(
    backends: &ResolvedBackends,
    session: &str,
    query: &str,
    top_k: Option<usize>,
    max_tokens: Option<usize>,
    traversal_depth: Option<usize>,
) -> Result<CliRecall, CliError> {
    require_nonempty("session", session)?;
    check_size_cli("session", session)?;
    require_nonempty("query", query)?;
    check_size_cli("query", query)?;

    let cfg = Config::default();
    let top_k = match top_k {
        Some(v) => v,
        None => clamp_cfg_default("default_top_k", cfg.default_top_k, 1, MAX_TOP_K),
    };
    let max_tokens = match max_tokens {
        Some(v) => v,
        None => clamp_cfg_default(
            "default_max_tokens",
            cfg.default_max_tokens,
            1,
            MAX_MAX_TOKENS,
        ),
    };
    let traversal_depth = match traversal_depth {
        Some(v) => v,
        None => clamp_cfg_default(
            "default_traversal_depth",
            cfg.default_traversal_depth,
            0,
            MAX_TRAVERSAL_DEPTH,
        ),
    };
    if top_k == 0 || top_k > MAX_TOP_K {
        return Err(CliError::Usage(format!("top-k must be in 1..={MAX_TOP_K}")));
    }
    if traversal_depth > MAX_TRAVERSAL_DEPTH {
        return Err(CliError::Usage(format!(
            "traversal-depth must be in 0..={MAX_TRAVERSAL_DEPTH}"
        )));
    }
    if max_tokens == 0 || max_tokens > MAX_MAX_TOKENS {
        return Err(CliError::Usage(format!(
            "max-tokens must be in 1..={MAX_MAX_TOKENS}"
        )));
    }

    let loaded = load_reader_graph_with_contract(
        backends.store.as_ref(),
        session,
        Some(&backends.embedding),
    )
    .await?;
    // Do NOT spawn: spawn would run GC, which is a writer. Config::default()
    // for knobs — same as `lambo serve` today (T82-12 is not T8.3's to fix).
    let daemon = Daemon::from_config(loaded.graph, &cfg).with_index(loaded.index);

    let mut extra_warnings = Vec::new();
    // H3: the embed-failure line is a typed, response-global annotation
    // (`vector_degraded`) captured at its producer — never text-parsed later.
    let mut extra_annotations: Vec<Annotation> = Vec::new();
    let embedding = if backends
        .store
        .capabilities()
        .contains(Capabilities::VECTOR_SEARCH)
    {
        match backends.embedder.embed(query).await {
            Ok(vector) => Some(vector),
            Err(err) => {
                let text = format!("recall: query embedding failed ({err}); vector leg skipped");
                extra_warnings.push(text.clone());
                extra_annotations.push(Annotation::new(AnnotationKind::VectorDegraded, text));
                None
            }
        }
    } else {
        None
    };

    let mut cache = RecallCache::<RecallPipeline>::new();
    let rq = RecallQuery {
        query: query.to_string(),
        top_k,
        max_tokens,
        traversal_depth,
    };
    let mut detail = daemon
        .recall_detailed(
            &loaded.session,
            rq,
            backends.store.as_ref(),
            embedding
                .as_deref()
                .map(|vector| (vector, &backends.embedding)),
            cfg.recall_weights,
            &mut cache,
        )
        .await;
    // Response-global annotations preserve producer order: the CLI-side
    // `vector_degraded` (embedded before recall) precedes the daemon's
    // (`traversal`, produced during recall).
    extra_annotations.append(&mut detail.response_annotations);
    detail.response_annotations = extra_annotations;

    let context = render_cli_text(&detail);
    Ok(CliRecall {
        context,
        hits: detail.detailed,
        response_annotations: detail.response_annotations,
    })
}

/// Render the operator-visible `lambo recall` string from a detailed recall —
/// the single renderer the CLI and the HTTP payload share. The context blocks
/// are the included hits' own rendered blocks (the pipeline's exact block
/// format, see [`crate::recall::format::render_detailed_block`]); the header
/// carries every warning whose owning block is outside the token budget,
/// preserving producer order: `vector_degraded` first, then each hit's
/// annotations in rank order, then the remaining response-global annotations
/// (`traversal`). A warning line whose block IS in the context is not
/// duplicated in the header.
///
/// The parity this enforces is the H3 losslessness property: every annotation
/// text appears in the output exactly once — inside its included block, or as
/// a header line when the block was excluded.
pub(crate) fn render_cli_text(detail: &DetailedRecall) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for h in &detail.detailed {
        if h.included_in_context {
            blocks.push(crate::recall::format::render_detailed_block(h));
        }
    }
    let block_context = blocks.join("\n\n");

    let mut header = String::new();
    let mut push_header = |w: &str| {
        if w.contains('⚑') {
            header.push_str(w);
        } else {
            header.push('⚑');
            header.push(' ');
            header.push_str(w);
        }
        header.push('\n');
    };
    for a in &detail.response_annotations {
        if a.kind == AnnotationKind::VectorDegraded {
            push_header(&a.text);
        }
    }
    for h in &detail.detailed {
        if h.included_in_context {
            continue;
        }
        for a in &h.annotations {
            push_header(&a.text);
        }
    }
    for a in &detail.response_annotations {
        if a.kind != AnnotationKind::VectorDegraded {
            push_header(&a.text);
        }
    }

    if header.is_empty() {
        block_context
    } else if block_context.is_empty() {
        header
    } else {
        format!("{header}{block_context}")
    }
}

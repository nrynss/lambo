//! `lambo recall` — lease-free reader process (spec §2.2).
//!
//! Loads the session, builds a [`Daemon`] **without spawning** (spawn would run
//! GC = writer), embeds the query only when the store claims `VECTOR_SEARCH`,
//! and prints the T5.3 context block.

use super::caps::{
    check_size_cli, clamp_cfg_default, require_nonempty, CliError, MAX_MAX_TOKENS, MAX_TOP_K,
    MAX_TRAVERSAL_DEPTH,
};
use super::load_reader_graph_with_contract;
use crate::config::Config;
use crate::daemon::{Daemon, RecallPipeline};
use crate::recall::cache::RecallCache;
use crate::resolve::ResolvedBackends;
use crate::store::Capabilities;
use crate::types::RecallQuery;

/// Recall relevant memory for a query.
pub async fn run(
    backends: &ResolvedBackends,
    session: &str,
    query: &str,
    top_k: Option<usize>,
    max_tokens: Option<usize>,
    traversal_depth: Option<usize>,
) -> Result<String, CliError> {
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
    let embedding = if backends
        .store
        .capabilities()
        .contains(Capabilities::VECTOR_SEARCH)
    {
        match backends.embedder.embed(query).await {
            Ok(vector) => Some(vector),
            Err(err) => {
                extra_warnings.push(format!(
                    "recall: query embedding failed ({err}); vector leg skipped"
                ));
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
    let result = daemon
        .recall(
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
    Ok(render_recall_text(result, extra_warnings))
}

/// Print daemon / embed warnings the operator can see. `tracing::warn!` is a
/// no-op on the CLI path (no subscriber), so skipped vector legs must land in
/// the returned text — same `⚑` channel the context block already uses.
fn render_recall_text(result: crate::types::RecallResult, extra_warnings: Vec<String>) -> String {
    let mut header = String::new();
    for w in extra_warnings.iter().chain(result.warnings.iter()) {
        if result.context.contains(w.as_str()) {
            continue;
        }
        if w.contains('⚑') {
            header.push_str(w);
        } else {
            header.push('⚑');
            header.push(' ');
            header.push_str(w);
        }
        header.push('\n');
    }
    if header.is_empty() {
        result.context
    } else if result.context.is_empty() {
        header
    } else {
        format!("{header}{}", result.context)
    }
}

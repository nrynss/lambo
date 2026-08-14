//! `lambo recall` — lease-free reader process (spec §2.2).
//!
//! Loads the session, builds a [`Daemon`] **without spawning** (spawn would run
//! GC = writer), embeds the query only when the store claims `VECTOR_SEARCH`,
//! and prints the T5.3 context block.

use super::caps::{
    check_size_cli, clamp_cfg_default, require_nonempty, CliError, MAX_MAX_TOKENS, MAX_TOP_K,
    MAX_TRAVERSAL_DEPTH,
};
use super::load_reader_graph;
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

    let loaded = load_reader_graph(backends.store.as_ref(), session).await?;
    // Do NOT spawn: spawn would run GC, which is a writer. Config::default()
    // for knobs — same as `lambo serve` today (T82-12 is not T8.3's to fix).
    let daemon = Daemon::from_config(loaded.graph, &cfg).with_index(loaded.index);

    let embedding = if backends
        .store
        .capabilities()
        .contains(Capabilities::VECTOR_SEARCH)
    {
        match backends.embedder.embed(query).await {
            Ok(vector) => Some(vector),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "recall: query embedding failed; vector leg skipped"
                );
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
            embedding.as_deref(),
            cfg.recall_weights,
            &mut cache,
        )
        .await;
    Ok(result.context)
}

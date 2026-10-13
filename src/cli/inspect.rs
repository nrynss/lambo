//! `lambo inspect` — neighbourhood around a focus concept (reader process).
//!
//! Focus resolution and the neighbourhood projection are shared with
//! `lambo_inspect` and the web portal and live in [`crate::surface`]
//! (`surface::focus`, `surface::neighbourhood`); this module is the CLI
//! adapter: argument checks, the reader load, and the mapping of the shared
//! refusal text onto [`CliError`]. Caps: [`MAX_INSPECT_DEPTH`],
//! [`crate::surface::limits::MAX_INSPECT_NODES`],
//! [`crate::surface::limits::MAX_INSPECT_CANDIDATES`],
//! `surface::focus::MAX_INSPECT_SCAN_CONCEPTS`,
//! `surface::focus::MAX_INSPECT_BOUNDED_SCAN`.
//!
//! The focus items that used to live here were crate-private and moved to
//! `crate::surface` without aliases; every caller imports them from there.

use super::caps::{
    check_in_range_cli, check_size_cli, require_nonempty, CliError, MAX_INSPECT_DEPTH,
};
use super::load_reader_graph;
use crate::canon::{explain_node, render_admission, EvalParams, PromotionPolicy};
use crate::store::GraphStore;
use crate::surface::focus::{
    ambiguous_refusal, fuzzy_note, missing_refusal, oversized_refusal, resolve_focus, Focus,
};
use crate::surface::neighbourhood::render_neighbourhood_parts;

/// Inspect the neighbourhood around `focus` (lease-free reader), explaining
/// its next canonization hop under the default policy (`Swarm`). The binary
/// calls [`run_with_policy`] with the policy `lambo.toml` and
/// `LAMBO_PROMOTION_POLICY` select.
pub async fn run(
    store: &dyn GraphStore,
    session: &str,
    focus: &str,
    depth: usize,
) -> Result<String, CliError> {
    run_with_policy(store, session, focus, depth, PromotionPolicy::default()).await
}

/// [`run`], explaining the focus's next canonization hop under `policy`
/// (#36 PR 3). A reader resolves its own config, like `serve-web`, so the
/// output names the policy it explained. The other canonization knobs are not
/// settable from `lambo.toml`, so their defaults are what a writer runs.
pub async fn run_with_policy(
    store: &dyn GraphStore,
    session: &str,
    focus: &str,
    depth: usize,
    policy: PromotionPolicy,
) -> Result<String, CliError> {
    require_nonempty("session", session)?;
    check_size_cli("session", session)?;
    require_nonempty("focus", focus)?;
    check_size_cli("focus", focus)?;
    check_in_range_cli("depth", depth, 0, MAX_INSPECT_DEPTH)?;

    let loaded = load_reader_graph(store, session).await?;
    // The read guard is scoped so it never spans the explanation's store
    // queries below.
    let (id, note, view) = {
        let g = loaded.graph.read();
        match resolve_focus(&g, focus.trim()) {
            Focus::Exact(id) => (id, None, render_neighbourhood_parts(&g, id, depth)),
            Focus::Fuzzy {
                id,
                content: matched,
                bounded,
            } => {
                // A fuzzy resolution is stated, never silent, and a bounded
                // one names the bound, so the caller knows what was not
                // scanned.
                let note = fuzzy_note(focus.trim(), &matched, bounded.as_ref());
                (id, Some(note), render_neighbourhood_parts(&g, id, depth))
            }
            other => return Err(refusal(other, focus, session)),
        }
    };
    let params = EvalParams::from_config(&crate::Config {
        promotion_policy: policy,
        ..crate::Config::default()
    });
    // A store fault degrades this additive section to one line; the
    // neighbourhood still prints.
    let section = match explain_node(&loaded.graph, store, id, &params, chrono::Utc::now()).await {
        Ok(admission) => admission.as_ref().map(render_admission),
        Err(e) => Some(format!("canonization: explanation unavailable: {e}\n")),
    };
    let section = crate::canon::with_last_demotion(section, &loaded.graph.read(), id);
    let text = view.text(section.as_deref());
    Ok(match note {
        Some(note) => format!("{note}\n{text}"),
        None => text,
    })
}

/// The refusal for a focus that resolved to no single concept.
fn refusal(resolved: Focus, focus: &str, session: &str) -> CliError {
    match resolved {
        Focus::Exact(_) | Focus::Fuzzy { .. } => {
            unreachable!("exact and fuzzy foci resolve before a refusal is built")
        }
        Focus::Ambiguous {
            candidates,
            bounded,
        } => CliError::Usage(ambiguous_refusal(
            focus.trim(),
            &candidates,
            bounded.as_ref(),
        )),
        // Subset-scoped suggestions (issue #9): the ranking never left the
        // bounded subset, so the refusal says so.
        Focus::Oversized { cap, near } => CliError::Runtime(oversized_refusal(cap, &near)),
        // Suggestions, never a silent match (issue #9): the caller gets a
        // nameable target with its node id.
        Focus::Missing { near } => CliError::Runtime(missing_refusal(focus, session, &near)),
    }
}

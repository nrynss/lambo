//! `lambo inspect` — neighbourhood around a focus concept (reader process).
//!
//! Focus resolution and the neighbourhood projection are shared with
//! `lambo_inspect` and the web portal and live in [`crate::surface`]
//! ([`crate::surface::focus`], [`crate::surface::neighbourhood`]); this module
//! is the CLI adapter: argument checks, the reader load, and the mapping of
//! the shared refusal text onto [`CliError`]. Caps: [`MAX_INSPECT_DEPTH`], [`crate::surface::limits::MAX_INSPECT_NODES`],
//! [`crate::surface::limits::MAX_INSPECT_CANDIDATES`],
//! [`crate::surface::focus::MAX_INSPECT_SCAN_CONCEPTS`],
//! [`crate::surface::focus::MAX_INSPECT_BOUNDED_SCAN`].

use super::caps::{
    check_in_range_cli, check_size_cli, require_nonempty, CliError, MAX_INSPECT_DEPTH,
};
use super::load_reader_graph;
use crate::store::GraphStore;
use crate::surface::focus::{
    ambiguous_refusal, fuzzy_note, missing_refusal, oversized_refusal, resolve_focus, Focus,
};
use crate::surface::neighbourhood::render_neighbourhood;

/// Inspect the neighbourhood around `focus` (lease-free reader).
pub async fn run(
    store: &dyn GraphStore,
    session: &str,
    focus: &str,
    depth: usize,
) -> Result<String, CliError> {
    require_nonempty("session", session)?;
    check_size_cli("session", session)?;
    require_nonempty("focus", focus)?;
    check_size_cli("focus", focus)?;
    check_in_range_cli("depth", depth, 0, MAX_INSPECT_DEPTH)?;

    let loaded = load_reader_graph(store, session).await?;
    let g = loaded.graph.read();
    match resolve_focus(&g, focus.trim()) {
        Focus::Exact(id) => {
            let (text, _) = render_neighbourhood(&g, id, depth);
            Ok(text)
        }
        Focus::Fuzzy {
            id,
            content: matched,
            bounded,
        } => {
            // A fuzzy resolution is stated, never silent, and a bounded one
            // names the bound, so the caller knows what was not scanned.
            let note = fuzzy_note(focus.trim(), &matched, bounded.as_ref());
            let (text, _) = render_neighbourhood(&g, id, depth);
            Ok(format!("{note}\n{text}"))
        }
        Focus::Ambiguous {
            candidates,
            bounded,
        } => Err(CliError::Usage(ambiguous_refusal(
            focus.trim(),
            &candidates,
            bounded.as_ref(),
        ))),
        // Subset-scoped suggestions (issue #9): the ranking never left the
        // bounded subset, so the refusal says so.
        Focus::Oversized { cap, near } => Err(CliError::Runtime(oversized_refusal(cap, &near))),
        // Suggestions, never a silent match (issue #9): the caller gets a
        // nameable target with its node id.
        Focus::Missing { near } => Err(CliError::Runtime(missing_refusal(focus, session, &near))),
    }
}

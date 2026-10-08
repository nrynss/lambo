//! `lambo inspect` — neighbourhood around a focus concept (reader process).
//!
//! Focus resolution and the neighbourhood projection are shared with
//! `lambo_inspect` and the web portal and live in [`crate::surface`]
//! ([`crate::surface::focus`], [`crate::surface::neighbourhood`]); this module
//! is the CLI adapter: argument checks, the reader load, and the CLI's refusal
//! text. Caps: [`MAX_INSPECT_DEPTH`], [`MAX_INSPECT_CANDIDATES`],
//! [`crate::surface::limits::MAX_INSPECT_NODES`], [`MAX_INSPECT_SCAN_CONCEPTS`],
//! [`MAX_INSPECT_BOUNDED_SCAN`].

use super::caps::{
    check_size_cli, require_nonempty, CliError, MAX_INSPECT_CANDIDATES, MAX_INSPECT_DEPTH,
};
use super::load_reader_graph;
use crate::store::GraphStore;
use crate::surface::focus::{
    resolve_focus, Focus, MAX_INSPECT_BOUNDED_SCAN, MAX_INSPECT_SCAN_CONCEPTS,
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
    if depth > MAX_INSPECT_DEPTH {
        return Err(CliError::Usage(format!(
            "depth must be in 0..={MAX_INSPECT_DEPTH}"
        )));
    }

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
            let note = match bounded {
                None => format!(
                    "resolved '{}' → '{}' (substring match, single candidate)",
                    focus.trim(),
                    matched
                ),
                Some(b) => format!(
                    "resolved '{}' → '{}' (substring match within the bounded scan of {} \
                     concepts; this graph is past the {}-concept full-scan cap, so concepts \
                     outside the subset were not scanned)",
                    focus.trim(),
                    matched,
                    b.scanned,
                    MAX_INSPECT_SCAN_CONCEPTS
                ),
            };
            let (text, _) = render_neighbourhood(&g, id, depth);
            Ok(format!("{note}\n{text}"))
        }
        Focus::Ambiguous {
            candidates,
            bounded,
        } => {
            let mut msg = match bounded {
                None => format!(
                    "'{}' matches {} concepts — name one exactly, or pass its node_id:",
                    focus.trim(),
                    candidates.len()
                ),
                Some(b) => format!(
                    "'{}' matches {} concepts within the bounded scan of {} concepts (this \
                     graph is past the {}-concept full-scan cap); name one exactly, or pass \
                     its node_id:",
                    focus.trim(),
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
            Err(CliError::Usage(msg))
        }
        Focus::Oversized { cap, near } => {
            let mut msg = format!(
                "this session's graph has more than {cap} concepts; the fuzzy pass scanned only \
                 the bounded subset (the {MAX_INSPECT_BOUNDED_SCAN} most recently created plus \
                 the {MAX_INSPECT_BOUNDED_SCAN} highest blast-radius concepts) and matched \
                 nothing; pass a node_id or an exact concept instead"
            );
            if !near.is_empty() {
                // Subset-scoped suggestions (issue #9): the ranking never left
                // the bounded subset, so the renderer must say so.
                msg.push_str(
                    "\nnearest within the bounded subset (suggestions, not matches; \
                     pass a node_id or name one exactly):",
                );
                for c in near.iter().take(MAX_INSPECT_CANDIDATES) {
                    msg.push_str(&format!("\n  {} [{}]", c.content, c.id.0));
                }
            }
            Err(CliError::Runtime(msg))
        }
        Focus::Missing { near } => {
            let mut msg = format!("no concept matching '{}' in session '{}'", focus, session);
            if !near.is_empty() {
                // Suggestions, never a silent match (issue #9): the caller
                // gets a nameable target with its node id.
                msg.push_str("\nsuggestions (not matches; pass a node_id or name one exactly):");
                for c in near.iter().take(MAX_INSPECT_CANDIDATES) {
                    msg.push_str(&format!("\n  {} [{}]", c.content, c.id.0));
                }
            }
            Err(CliError::Runtime(msg))
        }
    }
}

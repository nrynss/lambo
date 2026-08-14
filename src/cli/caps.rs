//! Shared surface caps and validators (CLI + MCP).
//!
//! These bounds started on the MCP tools (T8.2). T8.3 owns them so both
//! surfaces refuse the same oversized / control-character input and cannot
//! drift. MCP wraps [`check_size`]'s `String` error in a tool-level error;
//! the CLI maps it to a usage exit.

use clap::ValueEnum;

use crate::types::ConceptType;

/// Upper bound on `top_k` a client may ask for. Recall assembles and renders
/// every hit, so an unbounded `top_k` from one client is a cheap way to stall
/// the single process every other client shares.
pub const MAX_TOP_K: usize = 100;
/// Upper bound on `traversal_depth` (spec §8 phase 2 is a BFS — depth is an
/// exponent, not a linear cost).
pub const MAX_TRAVERSAL_DEPTH: usize = 5;
/// Upper bound on `max_tokens` for one context block.
pub const MAX_MAX_TOKENS: usize = 100_000;
/// Upper bound on concepts in a single `derive` call.
pub const MAX_CONCEPTS_PER_DERIVE: usize = 64;
/// Upper bound on the combined `produces` + `modifies` + `depends_on` target
/// count in a single `record-action` call.
pub const MAX_ACTION_TARGETS: usize = 64;
/// Upper bound on `reserve` TTL — a soft lock (spec §11), not a lease.
pub const MAX_RESERVE_TTL_SECS: u64 = 3600;
/// Upper bound on `inspect` depth.
pub const MAX_INSPECT_DEPTH: usize = 5;
/// Cap on neighbours rendered per `inspect` frontier level.
pub const MAX_INSPECT_NODES: usize = 200;
/// Upper bound on **every** client-supplied string this surface accepts.
///
/// Sized to match `graph::hybrid::MAX_HYBRID_CONTEXT_BYTES` so the surface
/// refuses before the graph does.
pub const MAX_CONTENT_BYTES: usize = 16_384;
/// Candidate concepts listed when `inspect`'s focus is ambiguous.
pub const MAX_INSPECT_CANDIDATES: usize = 10;

/// Concept type as it crosses the CLI (`--kind` / `--concept CONTENT:KIND`).
///
/// Snake_case to match MCP [`crate::mcp::server::WireConceptType`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum ConceptKind {
    Entity,
    Logic,
    Constraint,
    Resource,
    Observation,
}

impl From<ConceptKind> for ConceptType {
    fn from(k: ConceptKind) -> Self {
        match k {
            ConceptKind::Entity => ConceptType::Entity,
            ConceptKind::Logic => ConceptType::Logic,
            ConceptKind::Constraint => ConceptType::Constraint,
            ConceptKind::Resource => ConceptType::Resource,
            ConceptKind::Observation => ConceptType::Observation,
        }
    }
}

impl ConceptKind {
    /// Parse a `entity|logic|constraint|resource|observation` token.
    pub fn parse_token(s: &str) -> Result<Self, CliError> {
        match s.to_ascii_lowercase().as_str() {
            "entity" => Ok(Self::Entity),
            "logic" => Ok(Self::Logic),
            "constraint" => Ok(Self::Constraint),
            "resource" => Ok(Self::Resource),
            "observation" => Ok(Self::Observation),
            _ => Err(CliError::Usage(format!(
                "kind must be entity|logic|constraint|resource|observation, got '{s}'"
            ))),
        }
    }
}

/// CLI command failure. Usage (bad flags/values) exits 2; runtime (store,
/// lease, close) exits 1. Never a panic on bad input.
#[derive(Debug)]
pub enum CliError {
    Usage(String),
    Runtime(String),
}

impl CliError {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => 2,
            Self::Runtime(_) => 1,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(m) | Self::Runtime(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CliError {}

impl From<crate::types::LamboError> for CliError {
    fn from(err: crate::types::LamboError) -> Self {
        Self::Runtime(err.to_string())
    }
}

/// Clamp a config-derived default into the surface-enforced range.
///
/// A session config can set a `default_top_k` (etc.) wider than the surface
/// maximum; a caller that omits the knob would then inherit a value the
/// surface refuses. Clamp it into `lo..=hi` and log when that changes it.
pub fn clamp_cfg_default(name: &str, value: usize, lo: usize, hi: usize) -> usize {
    let clamped = value.clamp(lo, hi);
    if clamped != value {
        tracing::warn!(
            config_key = name,
            configured = value,
            clamped_to = clamped,
            "session config default is outside the surface bound — using the clamped value"
        );
    }
    clamped
}

/// Validate one client string before it reaches the store: refuse it if it is
/// over [`MAX_CONTENT_BYTES`] **or** carries a control character other than
/// tab/newline.
///
/// The size cap is the single-process fairness guard. The control-character
/// check is a data-hygiene one: a NUL or other C0 control ends up verbatim in a
/// concept's `content`, its canonical key, and every downstream rendering,
/// where it can corrupt terminals, truncate at the NUL, or smuggle ANSI
/// escapes. Tab and newline are the only controls a legitimate multi-line
/// concept needs, so everything else is refused here rather than sanitised
/// silently.
///
/// Names the offending codepoint; never echoes the raw byte.
pub fn check_size(field: &str, value: &str) -> Result<(), String> {
    if value.len() > MAX_CONTENT_BYTES {
        return Err(format!(
            "{field} exceeds {MAX_CONTENT_BYTES} bytes ({} given)",
            value.len()
        ));
    }
    if let Some(c) = value
        .chars()
        .find(|c| c.is_control() && *c != '\n' && *c != '\t')
    {
        return Err(format!(
            "{field} contains a disallowed control character (U+{:04X}); only tab and newline \
             are allowed",
            c as u32
        ));
    }
    Ok(())
}

/// [`check_size`] mapped to a CLI usage error.
pub fn check_size_cli(field: &str, value: &str) -> Result<(), CliError> {
    check_size(field, value).map_err(CliError::Usage)
}

/// Refuse an empty (after trim) required string.
pub fn require_nonempty(field: &str, value: &str) -> Result<(), CliError> {
    if value.trim().is_empty() {
        return Err(CliError::Usage(format!(
            "{field} must be a non-empty string"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_the_t8_2_values() {
        assert_eq!(MAX_TOP_K, 100);
        assert_eq!(MAX_TRAVERSAL_DEPTH, 5);
        assert_eq!(MAX_MAX_TOKENS, 100_000);
        assert_eq!(MAX_CONCEPTS_PER_DERIVE, 64);
        assert_eq!(MAX_ACTION_TARGETS, 64);
        assert_eq!(MAX_RESERVE_TTL_SECS, 3600);
        assert_eq!(MAX_INSPECT_DEPTH, 5);
        assert_eq!(MAX_INSPECT_NODES, 200);
        assert_eq!(MAX_CONTENT_BYTES, 16_384);
        assert_eq!(MAX_INSPECT_CANDIDATES, 10);
    }

    #[test]
    fn oversized_string_is_refused() {
        let big = "A".repeat(MAX_CONTENT_BYTES + 1);
        let err = check_size("query", &big).unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
        assert!(err.contains(&MAX_CONTENT_BYTES.to_string()), "{err}");
        assert!(!err.contains(&big), "must not echo the payload");
    }

    #[test]
    fn control_char_is_refused_by_codepoint() {
        let err = check_size("query", "ok\u{0001}no").unwrap_err();
        assert!(
            err.contains("U+0001"),
            "must name the codepoint, never echo the raw byte: {err}"
        );
        assert!(!err.contains('\u{0001}'), "must not echo U+0001: {err}");
    }

    #[test]
    fn tab_and_newline_are_allowed() {
        check_size("query", "ok\tline\nnext").unwrap();
        check_size("query", "A".repeat(MAX_CONTENT_BYTES).as_str()).unwrap();
    }

    #[test]
    fn clamp_cfg_default_pins_the_bounds() {
        assert_eq!(
            clamp_cfg_default("default_top_k", MAX_TOP_K + 500, 1, MAX_TOP_K),
            MAX_TOP_K
        );
        assert_eq!(clamp_cfg_default("default_top_k", 0, 1, MAX_TOP_K), 1);
        assert_eq!(clamp_cfg_default("default_top_k", 7, 1, MAX_TOP_K), 7);
    }

    #[test]
    fn concept_kind_parses_snake_case() {
        assert_eq!(
            ConceptKind::parse_token("entity").unwrap(),
            ConceptKind::Entity
        );
        assert_eq!(
            ConceptKind::parse_token("Observation").unwrap(),
            ConceptKind::Observation
        );
        assert!(ConceptKind::parse_token("nope").is_err());
    }
}

//! CLI adaptation of the shared request rules.
//!
//! The limits and validators themselves live in [`crate::surface`], shared by
//! the CLI, MCP, the web portal and the write queue. This module keeps what is
//! CLI-specific, [`CliError`] (usage exit 2, runtime exit 1), [`ConceptKind`]
//! (the clap value enum) and the `CliError`-returning wrappers, and
//! re-exports the shared items so `crate::cli::caps::*` paths stay valid.

use clap::ValueEnum;

use crate::types::ConceptType;

pub use crate::surface::limits::{
    clamp_cfg_default, MAX_ACTION_TARGETS, MAX_CONCEPTS_PER_DERIVE, MAX_CONTENT_BYTES,
    MAX_INSPECT_CANDIDATES, MAX_INSPECT_DEPTH, MAX_INSPECT_NODES, MAX_MAX_TOKENS,
    MAX_RESERVE_TTL_SECS, MAX_TOP_K, MAX_TRAVERSAL_DEPTH,
};
pub use crate::surface::validate::check_size;

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

/// [`check_size`] mapped to a CLI usage error.
///
/// **Known residual, deliberately not guarded here (J1-R2-4).** This is what
/// gates the CLI's `--agent` (`derive`, `record-action`, `reserve`, `release`),
/// and [`check_size`] passes `\n` on purpose, so `lambo derive --agent
/// $'x\ninjected'` writes a genuinely multi-line interaction author — which,
/// unlike an MCP soft-lock holder, is **durable**, and which a later `serve`'s
/// recall renders verbatim through `recall::format::conflict_warning`.
/// `mcp::server::LamboServer::check_agent_id`'s single-line rule guards the MCP
/// door only, by design: that door is where an *unauthenticated remote* string
/// becomes an identity, while `--agent` is the trusted local operator naming
/// themselves, and tightening it would change `AgentId`'s semantics for every
/// library caller. So this is an operator poisoning their own graph — P3 — but
/// it is the one residual that outlives the process, and it is recorded under
/// §J2 in `dev-diary/lambo-for-mooshik/J-multi-client.md` rather than only here,
/// because J2 is where clients stop being local.
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

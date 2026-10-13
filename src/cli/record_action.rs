//! `lambo record-action` — lease-held thin adapter over [`crate::memory::Memory::record_action`].

use super::caps::{check_size_cli, require_nonempty, CliError};
use super::{close_writer, open_writer};
use crate::graph::action::Action;
use crate::graph::cite::MAX_AGAINST_PER_CONCEPT;
use crate::resolve::ResolvedBackends;
use crate::surface::validate::{check_action_targets, check_no_image_suffix};

/// Parsed `record-action` flags.
pub struct Args {
    pub repo_scope: Option<String>,
    pub session: String,
    pub agent: String,
    pub action: String,
    pub produces: Vec<String>,
    pub modifies: Vec<String>,
    pub depends_on: Vec<String>,
    /// `--against` (#19(b)): existing concepts the action contradicts.
    pub against: Vec<String>,
}

/// Record an action the agent took.
pub async fn run(backends: ResolvedBackends, args: Args) -> Result<String, CliError> {
    if let Some(scope) = args.repo_scope.as_deref() {
        crate::graph::resource::check_repo_scope(scope).map_err(CliError::Usage)?;
    }
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;
    require_nonempty("agent", &args.agent)?;
    check_size_cli("agent", &args.agent)?;
    require_nonempty("action", &args.action)?;
    check_size_cli("action", &args.action)?;
    // #22 PR 4: the action is its own Resource concept's content.
    check_no_image_suffix("action", &args.action).map_err(CliError::Usage)?;

    let total = args.produces.len() + args.modifies.len() + args.depends_on.len();
    check_action_targets(total).map_err(CliError::Usage)?;
    for s in args
        .produces
        .iter()
        .chain(&args.modifies)
        .chain(&args.depends_on)
    {
        require_nonempty("produces / modifies / depends_on entry", s)?;
        check_size_cli("produces / modifies / depends_on entry", s)?;
    }

    // #19(b): shape-checked here; whether each resolves is the core's
    // question.
    if args.against.len() > MAX_AGAINST_PER_CONCEPT {
        return Err(CliError::Usage(format!(
            "against may be given at most {MAX_AGAINST_PER_CONCEPT} times"
        )));
    }
    for r in &args.against {
        require_nonempty("against", r)?;
        check_size_cli("against", r)?;
    }

    let mem = open_writer(backends, &args.session, &args.agent).await?;
    let produces: Vec<&str> = args.produces.iter().map(String::as_str).collect();
    let modifies: Vec<&str> = args.modifies.iter().map(String::as_str).collect();
    let depends_on: Vec<&str> = args.depends_on.iter().map(String::as_str).collect();
    let action = Action {
        repo_scope: args.repo_scope.as_deref(),
        event_time: None,
        action: args.action.as_str(),
        produces: &produces,
        modifies: &modifies,
        depends_on: &depends_on,
    };
    // The embedding variant, not the sync one: a CLI-recorded action that
    // stored no vector was findable by keyword only, exactly like the MCP path
    // before 2026-09-01.
    let out = match mem
        .record_action_against_embedded_as(mem.agent(), &action, &args.against)
        .await
    {
        Ok(outcome) => Ok(format!(
            "recorded action '{}': {} concept(s) created ({} embedded), {} edge(s) added",
            args.action,
            outcome.created.len(),
            outcome.embedded,
            outcome.edges
        )),
        Err(e) => Err(CliError::from(e)),
    };
    close_writer(mem, out).await
}

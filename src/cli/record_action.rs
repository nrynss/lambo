//! `lambo record-action` — lease-held thin adapter over [`Memory::record_action`].

use super::caps::{check_size_cli, require_nonempty, CliError, MAX_ACTION_TARGETS};
use super::{close_writer, open_writer};
use crate::graph::action::Action;
use crate::resolve::ResolvedBackends;

/// Parsed `record-action` flags.
pub struct Args {
    pub session: String,
    pub agent: String,
    pub action: String,
    pub produces: Vec<String>,
    pub modifies: Vec<String>,
    pub depends_on: Vec<String>,
}

/// Record an action the agent took.
pub async fn run(backends: ResolvedBackends, args: Args) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;
    require_nonempty("agent", &args.agent)?;
    check_size_cli("agent", &args.agent)?;
    require_nonempty("action", &args.action)?;
    check_size_cli("action", &args.action)?;

    let total = args.produces.len() + args.modifies.len() + args.depends_on.len();
    if total > MAX_ACTION_TARGETS {
        return Err(CliError::Usage(format!(
            "produces + modifies + depends_on must total at most {MAX_ACTION_TARGETS} \
             entries ({total} given)"
        )));
    }
    for s in args
        .produces
        .iter()
        .chain(&args.modifies)
        .chain(&args.depends_on)
    {
        require_nonempty("produces / modifies / depends_on entry", s)?;
        check_size_cli("produces / modifies / depends_on entry", s)?;
    }

    let mem = open_writer(backends, &args.session, &args.agent).await?;
    let produces: Vec<&str> = args.produces.iter().map(String::as_str).collect();
    let modifies: Vec<&str> = args.modifies.iter().map(String::as_str).collect();
    let depends_on: Vec<&str> = args.depends_on.iter().map(String::as_str).collect();
    let action = Action {
        action: args.action.as_str(),
        produces: &produces,
        modifies: &modifies,
        depends_on: &depends_on,
    };
    let out = match mem.record_action(&action) {
        Ok(outcome) => Ok(format!(
            "recorded action '{}': {} concept(s) created, {} edge(s) added",
            args.action,
            outcome.created.len(),
            outcome.edges
        )),
        Err(e) => Err(CliError::from(e)),
    };
    close_writer(mem, out).await
}

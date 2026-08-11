use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use lambo::{build_embedder, build_store, LamboFile};

/// Lambo — agentic graph memory (MCP server + CLI).
#[derive(Debug, Parser)]
#[command(name = "lambo", version, about, long_about = None)]
struct Cli {
    /// Process config (`lambo.toml`). Overrides: `LAMBO_CONFIG`, then `./lambo.toml`.
    /// Env always wins over file for set keys (Level B).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run the MCP server for a session (primary artifact).
    Serve {
        /// Session id this process owns (single-writer model).
        #[arg(long)]
        session: Option<String>,
        /// Transport: stdio | http
        #[arg(long, default_value = "stdio")]
        transport: String,
        /// HTTP port when transport=http
        #[arg(long, default_value_t = 7700)]
        port: u16,
    },
    /// Scripted two-agent demo scenario.
    Demo {
        #[arg(long, default_value = "rest-api")]
        scenario: String,
    },
    /// Recall against a session.
    Recall {
        #[arg(long)]
        session: String,
        #[arg(long)]
        query: String,
        #[arg(long, default_value_t = 5)]
        top_k: usize,
    },
    /// List canonical ("saints") memories.
    Saints {
        #[arg(long)]
        session: String,
    },
    /// Inspect a focus node neighborhood.
    Inspect {
        #[arg(long)]
        session: String,
        #[arg(long)]
        focus: String,
        #[arg(long, default_value_t = 2)]
        depth: usize,
    },
    /// Session stats (flush lag, log depth, etc.).
    Stats {
        #[arg(long)]
        session: String,
    },
    /// Provision / migrate durable store (ccloud + schema).
    ///
    /// Does **not** construct the embedder (ops-only path). Still validates Level B
    /// store selection so a misconfigured `kind` fails closed early.
    Provision,
}

impl Commands {
    fn name(&self) -> &'static str {
        match self {
            Self::Serve { .. } => "serve",
            Self::Demo { .. } => "demo",
            Self::Recall { .. } => "recall",
            Self::Saints { .. } => "saints",
            Self::Inspect { .. } => "inspect",
            Self::Stats { .. } => "stats",
            Self::Provision => "provision",
        }
    }

    /// Whether this command needs store + embedder (vs store-only ops).
    fn needs_embedder(&self) -> bool {
        !matches!(self, Self::Provision)
    }
}

/// Load Level B config and construct store + embedder (fail closed).
fn resolve_backends(config: Option<&std::path::Path>) -> Result<(), String> {
    let file = LamboFile::load_resolved(config).map_err(|e| e.to_string())?;
    let _store = build_store(file.store).map_err(|e| e.to_string())?;
    let _embed = build_embedder(file.embedder).map_err(|e| e.to_string())?;
    Ok(())
}

/// Validate store selection only (provision / schema tooling does not need an embedder).
fn resolve_store_only(config: Option<&std::path::Path>) -> Result<(), String> {
    let file = LamboFile::load_resolved(config).map_err(|e| e.to_string())?;
    let _store = build_store(file.store).map_err(|e| e.to_string())?;
    Ok(())
}

fn resolve_for_command(cmd: &Commands, config: Option<&std::path::Path>) -> Result<(), String> {
    if cmd.needs_embedder() {
        resolve_backends(config)
    } else {
        resolve_store_only(config)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = cli.config.as_deref();

    let Some(cmd) = cli.command else {
        eprintln!("lambo: pass a subcommand (try --help)");
        return ExitCode::from(2);
    };

    // Single Level B guard — fail closed before any stub work.
    if let Err(e) = resolve_for_command(&cmd, config) {
        let what = if cmd.needs_embedder() {
            "failed to build backends"
        } else {
            "failed to resolve store"
        };
        eprintln!("lambo {}: {what}: {e}", cmd.name());
        return ExitCode::FAILURE;
    }

    match cmd {
        Commands::Serve {
            session,
            transport,
            port,
        } => {
            println!(
                "lambo serve (stub): session={session:?} transport={transport} port={port} \
                 (store+embedder resolved via Level B config)"
            );
            ExitCode::SUCCESS
        }
        Commands::Demo { scenario } => {
            println!("lambo demo (stub): scenario={scenario}");
            ExitCode::SUCCESS
        }
        Commands::Recall {
            session,
            query,
            top_k,
        } => {
            println!("lambo recall (stub): session={session} query={query} top_k={top_k}");
            ExitCode::SUCCESS
        }
        Commands::Saints { session } => {
            println!("lambo saints (stub): session={session}");
            ExitCode::SUCCESS
        }
        Commands::Inspect {
            session,
            focus,
            depth,
        } => {
            println!("lambo inspect (stub): session={session} focus={focus} depth={depth}");
            ExitCode::SUCCESS
        }
        Commands::Stats { session } => {
            println!("lambo stats (stub): session={session}");
            ExitCode::SUCCESS
        }
        Commands::Provision => {
            println!("lambo provision (stub): use scripts/provision.sh for now");
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_help_structure() {
        Cli::command().debug_assert();
    }

    /// Only runs when both default adapters are compiled; uses an explicit temp
    /// config path so a developer-local `./lambo.toml` cannot poison discovery.
    #[test]
    #[cfg(all(feature = "store-memory", feature = "embed-bge"))]
    fn resolve_backends_default_memory_and_bge() {
        use std::io::Write;
        use std::sync::{Mutex, OnceLock};

        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _g = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        for k in [
            "LAMBO_STORE",
            "LAMBO_EMBEDDER",
            "LAMBO_CONFIG",
            "LAMBO_COCKROACH_DSN",
            "DATABASE_URL",
            "LAMBO_SQLITE_PATH",
            "LAMBO_EMBED_DIM",
            "LAMBO_LLAMA_EMBED_URL",
            "LAMBO_LLAMA_MODEL",
        ] {
            std::env::remove_var(k);
        }

        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "lambo-cli-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lambo-test.toml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(
                f,
                r#"[store]
kind = "memory"
[embedder]
kind = "bge_m3"
dim = 1024
"#
            )
            .unwrap();
        }

        resolve_backends(Some(&path)).expect("default Level B backends");
        resolve_store_only(Some(&path)).expect("default Level B store");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

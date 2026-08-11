use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use lambo::{resolve_from_config_path, resolve_store_only, ResolvedBackends};

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

    fn needs_embedder(&self) -> bool {
        !matches!(self, Self::Provision)
    }
}

/// Single construction site for store + embedder (T8.x must reuse this, not rebuild).
enum Resolved {
    Full(Box<ResolvedBackends>),
    StoreOnly,
}

fn resolve_for_command(
    cmd: &Commands,
    config: Option<&std::path::Path>,
) -> Result<Resolved, String> {
    if cmd.needs_embedder() {
        let r = resolve_from_config_path(config).map_err(|e| e.to_string())?;
        Ok(Resolved::Full(Box::new(r)))
    } else {
        let _store = resolve_store_only(config).map_err(|e| e.to_string())?;
        Ok(Resolved::StoreOnly)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = cli.config.as_deref();

    let Some(cmd) = cli.command else {
        eprintln!("lambo: pass a subcommand (try --help)");
        return ExitCode::from(2);
    };

    // Construct once; when Memory/serve land, pass `Resolved` into the command body.
    let resolved = match resolve_for_command(&cmd, config) {
        Ok(r) => r,
        Err(e) => {
            let what = if cmd.needs_embedder() {
                "failed to build backends"
            } else {
                "failed to resolve store"
            };
            eprintln!("lambo {}: {what}: {e}", cmd.name());
            return ExitCode::FAILURE;
        }
    };

    match (cmd, resolved) {
        (
            Commands::Serve {
                session,
                transport,
                port,
            },
            Resolved::Full(backends),
        ) => {
            // Hold backends so they are not dead-code; T8.2 will own them for the process life.
            let _ = (&backends.store, &backends.embedder, &backends.embedding);
            println!(
                "lambo serve (stub): session={session:?} transport={transport} port={port} \
                 (store+embedder resolved once: {} dim={})",
                backends.embedding.kind, backends.embedding.dim
            );
            ExitCode::SUCCESS
        }
        (Commands::Demo { scenario }, Resolved::Full(backends)) => {
            let _ = backends;
            println!("lambo demo (stub): scenario={scenario}");
            ExitCode::SUCCESS
        }
        (
            Commands::Recall {
                session,
                query,
                top_k,
            },
            Resolved::Full(backends),
        ) => {
            let _ = backends;
            println!("lambo recall (stub): session={session} query={query} top_k={top_k}");
            ExitCode::SUCCESS
        }
        (Commands::Saints { session }, Resolved::Full(backends)) => {
            let _ = backends;
            println!("lambo saints (stub): session={session}");
            ExitCode::SUCCESS
        }
        (
            Commands::Inspect {
                session,
                focus,
                depth,
            },
            Resolved::Full(backends),
        ) => {
            let _ = backends;
            println!("lambo inspect (stub): session={session} focus={focus} depth={depth}");
            ExitCode::SUCCESS
        }
        (Commands::Stats { session }, Resolved::Full(backends)) => {
            let _ = backends;
            println!("lambo stats (stub): session={session}");
            ExitCode::SUCCESS
        }
        (Commands::Provision, Resolved::StoreOnly) => {
            println!("lambo provision (stub): use scripts/provision.sh for now");
            ExitCode::SUCCESS
        }
        _ => {
            // needs_embedder / resolve_for_command pairing is exhaustive in practice.
            eprintln!("lambo: internal resolve mismatch");
            ExitCode::FAILURE
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

    #[test]
    #[cfg(all(feature = "store-memory", feature = "embed-bge"))]
    fn resolve_backends_default_memory_and_bge() {
        use std::io::Write;
        use std::sync::{Mutex, OnceLock};

        // Bin tests link the lib without cfg(test), so use a local lock (same discipline
        // as lib `test_util::env_lock` — do not mutate env without holding a mutex).
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

        let full = resolve_from_config_path(Some(&path)).expect("full resolve");
        assert_eq!(full.embedding.dim, 1024);
        assert!(full.store.vector_dimensions().is_none());
        let _ = resolve_store_only(Some(&path)).expect("store only");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

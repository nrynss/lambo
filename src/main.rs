use clap::{Parser, Subcommand};

/// Lambo — agentic graph memory (MCP server + CLI).
#[derive(Debug, Parser)]
#[command(name = "lambo", version, about, long_about = None)]
struct Cli {
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
    Provision,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        None => {
            // clap prints help only with --help; give a short nudge for bare `lambo`.
            eprintln!("lambo: pass a subcommand (try --help)");
            std::process::exit(2);
        }
        Some(Commands::Serve {
            session,
            transport,
            port,
        }) => {
            println!("lambo serve (stub): session={session:?} transport={transport} port={port}");
        }
        Some(Commands::Demo { scenario }) => {
            println!("lambo demo (stub): scenario={scenario}");
        }
        Some(Commands::Recall {
            session,
            query,
            top_k,
        }) => {
            println!("lambo recall (stub): session={session} query={query} top_k={top_k}");
        }
        Some(Commands::Saints { session }) => {
            println!("lambo saints (stub): session={session}");
        }
        Some(Commands::Inspect {
            session,
            focus,
            depth,
        }) => {
            println!("lambo inspect (stub): session={session} focus={focus} depth={depth}");
        }
        Some(Commands::Stats { session }) => {
            println!("lambo stats (stub): session={session}");
        }
        Some(Commands::Provision) => {
            println!("lambo provision (stub): use scripts/provision.sh for now");
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
}

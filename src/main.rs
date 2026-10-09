use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use lambo::cli::{CliError, ConceptKind};
use lambo::mcp::{PinnedSessions, ServeOptions, Transport};
use lambo::store::{GraphStore, StoreKind};
use lambo::{
    resolve_backends, resolve_from_config_path, resolve_store_only, LamboFile, ResolvedBackends,
};

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
        /// Session id this process owns (single-writer model, spec §2.2).
        /// Repeatable with --transport http: every session given is pinned
        /// (attached at startup) and served at /mcp/s/<session>, beside
        /// lambo.toml [serve] sessions; the first is the default, served at
        /// /mcp. A stdio serve takes at most one; without it, it uses the
        /// lambo.toml [[serve.projects]] entry covering its working directory
        /// (longest path wins), then [serve] default_session, and refuses if
        /// neither applies.
        #[arg(long)]
        session: Vec<String>,
        /// Agent identity this process writes as.
        #[arg(long, default_value = "lambo-serve")]
        agent: String,
        /// Transport: stdio | http
        #[arg(long, default_value = "stdio")]
        transport: String,
        /// HTTP port when transport=http
        #[arg(long, default_value_t = 7700)]
        port: u16,
        /// Bind address when transport=http. Loopback by default. Binding
        /// anywhere else REQUIRES --auth-token (or LAMBO_AUTH_TOKEN): this
        /// process is a session *writer* and serve refuses to start otherwise.
        #[arg(long, default_value = "127.0.0.1")]
        bind: std::net::IpAddr,
        /// Bearer token required on every HTTP request. Prefer the
        /// LAMBO_AUTH_TOKEN env var, which overrides this flag — a token in
        /// argv is visible in `ps` and shell history. Optional on loopback,
        /// mandatory on any other bind. Ignored by --transport stdio.
        #[arg(long, value_name = "TOKEN")]
        auth_token: Option<lambo::mcp::SecretToken>,
        /// Maximum concurrently live MCP sessions on the HTTP transport;
        /// further `initialize` requests are refused with 503.
        #[arg(long, default_value_t = lambo::mcp::DEFAULT_MAX_SESSIONS)]
        max_sessions: usize,
        /// Sustained HTTP request/second ceiling (burst allowance is 2x). Set
        /// 0 to disable the limit.
        #[arg(long, default_value_t = lambo::mcp::DEFAULT_RATE_LIMIT_RPS)]
        rate_limit_rps: u32,
        /// DANGEROUS: attach despite a same-width stored/configured model-id mismatch.
        #[arg(long)]
        allow_embedding_mismatch: bool,
        /// I1 — append one JSON line per MCP tool call to this path
        /// (append-only JSONL, created if absent). Off by default.
        ///
        /// Never affects a tool call: the write is buffered onto a dedicated
        /// writer thread and a stalled or unwritable path DROPS lines rather
        /// than failing or delaying anything, counting them in `lambo_stats`
        /// as `ledger_dropped_lines`. Rotation is yours (`logrotate`, or just
        /// `mv` it — the writer recreates the path on the next line).
        ///
        /// The file inherits the store's hygiene rules: keep it outside the
        /// repo (`~/lambo-dogfood/`), export only through the curated path.
        #[arg(long, value_name = "PATH")]
        ledger: Option<std::path::PathBuf>,
        /// I2 — also append a `stats` heartbeat line to the ledger every N
        /// seconds: the `lambo_stats` payload plus process uptime and the
        /// binary's version and git sha. Requires --ledger. Off by default.
        #[arg(long, value_name = "SECS")]
        ledger_heartbeat: Option<u64>,
    },
    /// Serve the read-only demo page for a session: live recall, the canonization feed, and durable counts.
    ///
    /// A reader process: it never takes the writer lease and exposes no mutating
    /// route, so it runs safely beside `lambo serve` on the same session.
    /// Loopback is unauthenticated by default; a non-loopback bind requires a
    /// bearer token (LAMBO_AUTH_TOKEN or --auth-token) and fails closed without one.
    ServeWeb {
        /// Session to open a read-only window onto (reader process; does not take the writer lease).
        #[arg(
            long,
            help = "Session to open a read-only window onto (reader process; does not take the writer lease)."
        )]
        session: String,
        /// HTTP port to listen on.
        #[arg(long, default_value_t = 7710, help = "HTTP port to listen on.")]
        port: u16,
        /// Bind address. Loopback by default — unauthenticated. A non-loopback
        /// bind REQUIRES --auth-token (or LAMBO_AUTH_TOKEN) and refuses to start
        /// without one.
        #[arg(
            long,
            default_value = "127.0.0.1",
            help = "Bind address. Loopback by default. A non-loopback bind requires a bearer token (--auth-token or LAMBO_AUTH_TOKEN)."
        )]
        bind: std::net::IpAddr,
        /// Bearer token required on every request. Prefer the LAMBO_AUTH_TOKEN
        /// env var, which overrides this flag — a token in argv is visible in
        /// `ps` and shell history. Optional on loopback, mandatory on any other
        /// bind.
        #[arg(long, value_name = "TOKEN")]
        auth_token: Option<lambo::cli::serve_web::AuthToken>,
    },
    /// Scripted two-agent demo scenario (spec §13): two agents build one REST API, `user schema` earns Canonical, and the second agent's recall carries the blast-radius and conflict warnings.
    Demo {
        /// Scenario to run. Only `rest-api` exists in v0.1.
        #[arg(
            long,
            default_value = "rest-api",
            help = "Scenario to run. Only `rest-api` exists in v0.1."
        )]
        scenario: String,
        /// Session to write. Defaults to a fresh id per run; the scenario is not re-runnable into a used session (canonization state is not restored over one).
        #[arg(
            long,
            help = "Session to write. Defaults to a fresh id per run; the scenario is not re-runnable into a used session (canonization state is not restored over one)."
        )]
        session: Option<String>,
        /// DANGEROUS: attach despite a same-width stored/configured model-id mismatch.
        #[arg(long)]
        allow_embedding_mismatch: bool,
    },
    /// Recall relevant memory for a query and return the Lambo context block (canonical markers, blast-radius warnings, conflict lines).
    Recall {
        /// Session to recall against (reader process; does not take the writer lease).
        #[arg(
            long,
            help = "Session to recall against (reader process; does not take the writer lease)."
        )]
        session: String,
        /// Natural-language query. Optional with --image or --query-vector-json.
        #[arg(
            long,
            required_unless_present_any = ["image", "query_vector_json"],
            help = "Natural-language query. Optional with --image or --query-vector-json."
        )]
        query: Option<String>,
        /// Hits to return. Defaults to the session config's default_top_k.
        #[arg(
            long,
            help = "Hits to return. Defaults to the session config's default_top_k."
        )]
        top_k: Option<usize>,
        /// Token budget for the rendered context block.
        #[arg(long, help = "Token budget for the rendered context block.")]
        max_tokens: Option<usize>,
        /// Graph traversal depth for phase 2 expansion.
        #[arg(long, help = "Graph traversal depth for phase 2 expansion.")]
        traversal_depth: Option<usize>,
        /// Recall what is close to this PNG, JPEG or WebP file (at most 2 MiB, 4096 px a side), embedded by the configured embedder (#22). Not stored.
        #[arg(
            long,
            value_name = "PATH",
            conflicts_with = "query_vector_json",
            help = "Recall what is close to this PNG, JPEG or WebP file (at most 2 MiB, 4096 px a side), embedded by the configured embedder. Not stored."
        )]
        image: Option<PathBuf>,
        /// The --image file's MIME type (image/png, image/jpeg, image/webp). Default: read from the file; a mismatch is refused.
        #[arg(
            long,
            requires = "image",
            help = "The --image file's MIME type (image/png, image/jpeg, image/webp). Default: read from the file; a mismatch is refused."
        )]
        mime: Option<String>,
        /// Recall what is close to a vector: a JSON file {"values": [...], "contract": {"kind", "model", "dim"}} in this session's space. Needs [embedder] accept_client_vectors = true.
        #[arg(
            long = "query-vector-json",
            value_name = "PATH",
            help = "Recall what is close to a vector: a JSON file {\"values\": [...], \"contract\": {\"kind\", \"model\", \"dim\"}} in this session's space. Needs [embedder] accept_client_vectors = true."
        )]
        query_vector_json: Option<PathBuf>,
    },
    /// List the session's canonical memories — concepts that earned Canonical status through the audited transition path.
    Saints {
        /// Session to list canonical memories from (reader process; does not take the writer lease).
        #[arg(
            long,
            help = "Session to list canonical memories from (reader process; does not take the writer lease)."
        )]
        session: String,
    },
    /// Inspect the neighbourhood around a concept: its type, canonization status, blast radius and typed edges out to a depth.
    Inspect {
        /// Session to inspect (reader process; does not take the writer lease).
        #[arg(
            long,
            help = "Session to inspect (reader process; does not take the writer lease)."
        )]
        session: String,
        /// Concept content (or a node UUID) to centre the neighbourhood on.
        #[arg(
            long,
            help = "Concept content (or a node UUID) to centre the neighbourhood on."
        )]
        focus: String,
        /// Hops out from the focus (default 2, max 5).
        #[arg(
            long,
            default_value_t = 2,
            help = "Hops out from the focus (default 2, max 5)."
        )]
        depth: usize,
    },
    /// Session health: node/edge/concept counts and canonization progress. Writer-only flush lag is not visible to a reader process.
    Stats {
        /// Session to report (reader process; does not take the writer lease).
        #[arg(
            long,
            help = "Session to report (reader process; does not take the writer lease)."
        )]
        session: String,
    },
    /// Provision / migrate the durable store schema (SQLite and Postgres via init_schema, Cockroach via scripts/provision.sh).
    ///
    /// Does **not** construct the embedder (ops-only path). Still validates Level B
    /// store selection so a misconfigured `kind` fails closed early.
    Provision,
    /// Erase a whole session for account deletion: every node, edge, vector, canonization record and write intent, plus the session row with its embedding contract. A tombstone keeps the id from being written again. Safe to repeat. Refused while a live writer holds the session.
    ///
    /// Operator-only: the authority is access to the store, never an agent id.
    /// Does not construct the embedder. Prints one JSON report after the store
    /// committed; `already_absent: true` means nothing was left to remove.
    EraseSession {
        /// Session to erase.
        #[arg(long, help = "Session to erase.")]
        session: String,
        /// The session id again, exactly, to guard against typos. Nothing is erased when it differs.
        #[arg(
            long,
            help = "The session id again, exactly, to guard against typos. Nothing is erased when it differs."
        )]
        confirm: String,
    },
    /// Maintain the recall tier's index ([recall] in lambo.toml).
    ///
    /// Operator-only. Does not construct the embedder.
    RecallIndex {
        #[command(subcommand)]
        action: RecallIndexAction,
    },
    /// Derive concepts from the current interaction into session memory. Timestamps are stamped server-side; do not send one.
    Derive {
        /// Session this process writes (acquires the single-writer lease).
        #[arg(
            long,
            help = "Session this process writes (acquires the single-writer lease)."
        )]
        session: String,
        /// Agent identity stamped on the interaction and concepts.
        #[arg(long, help = "Agent identity stamped on the interaction and concepts.")]
        agent: String,
        /// The first concept's text.
        #[arg(long, help = "The first concept's text.")]
        content: String,
        /// The first concept's type: entity, logic, constraint, resource, or observation.
        #[arg(
            long,
            value_enum,
            help = "The first concept's type: entity, logic, constraint, resource, or observation."
        )]
        kind: ConceptKind,
        /// Hierarchy pair CHILD:PARENT (repeatable). Parent is right of the FIRST colon, child left — matching MCP WireParentOf. Only the first colon is the separator, so the parent may itself contain colons (e.g. an IPv6 CIDR like 2001:db8::/32).
        #[arg(
            long = "parent-of",
            value_name = "CHILD:PARENT",
            action = ArgAction::Append,
            help = "Hierarchy pair CHILD:PARENT (repeatable). Parent is right of the FIRST colon, child left — matching MCP WireParentOf. Only the first colon is the separator, so the parent may itself contain colons (e.g. an IPv6 CIDR like 2001:db8::/32)."
        )]
        parent_of: Vec<String>,
        /// Extra concept CONTENT:KIND (repeatable) so one invocation can match a multi-concept MCP lambo_derive.
        #[arg(
            long,
            value_name = "CONTENT:KIND",
            action = ArgAction::Append,
            help = "Extra concept CONTENT:KIND (repeatable) so one invocation can match a multi-concept MCP lambo_derive."
        )]
        concept: Vec<String>,
        /// DANGEROUS: attach despite a same-width stored/configured model-id mismatch.
        #[arg(long)]
        allow_embedding_mismatch: bool,
    },
    /// Derive one image concept from a local image file or a client-computed vector file (#22). Its content is the caption plus [image:<id>] and its vector is the image's.
    DeriveImage {
        /// Session this process writes (acquires the single-writer lease).
        #[arg(
            long,
            help = "Session this process writes (acquires the single-writer lease)."
        )]
        session: String,
        /// Agent identity stamped on the interaction and concept.
        #[arg(long, help = "Agent identity stamped on the interaction and concept.")]
        agent: String,
        /// What the image is, in words: the concept's text. May not contain [image:.
        #[arg(
            long,
            help = "What the image is, in words: the concept's text. May not contain [image:."
        )]
        caption: String,
        /// The concept's type: entity, logic, constraint or resource (not observation).
        #[arg(
            long,
            value_enum,
            help = "The concept's type: entity, logic, constraint or resource (not observation)."
        )]
        kind: ConceptKind,
        /// Your id for the image, 1 to 64 of [a-z0-9]. Default: from the image's digest.
        #[arg(
            long = "image-id",
            value_name = "ID",
            help = "Your id for the image, 1 to 64 of [a-z0-9]. Default: from the image's digest."
        )]
        image_id: Option<String>,
        /// A PNG, JPEG or WebP file (at most 2 MiB, 4096 px a side) for the configured embedder to embed.
        #[arg(
            long,
            value_name = "PATH",
            conflicts_with = "vector_json",
            required_unless_present = "vector_json",
            help = "A PNG, JPEG or WebP file (at most 2 MiB, 4096 px a side) for the configured embedder to embed."
        )]
        image: Option<PathBuf>,
        /// The image's MIME type (image/png, image/jpeg, image/webp). Default: read from the file; a mismatch is refused.
        #[arg(
            long,
            requires = "image",
            help = "The image's MIME type (image/png, image/jpeg, image/webp). Default: read from the file; a mismatch is refused."
        )]
        mime: Option<String>,
        /// A JSON file {"values": [...], "contract": {"kind", "model", "dim"}} with a vector computed in this session's space. Needs [embedder] accept_client_vectors = true.
        #[arg(
            long = "vector-json",
            value_name = "PATH",
            help = "A JSON file {\"values\": [...], \"contract\": {\"kind\", \"model\", \"dim\"}} with a vector computed in this session's space. Needs [embedder] accept_client_vectors = true."
        )]
        vector_json: Option<PathBuf>,
        /// Hierarchy pair CHILD:PARENT (repeatable), as for derive. Name the image concept by its content, caption [image:<id>].
        #[arg(
            long = "parent-of",
            value_name = "CHILD:PARENT",
            action = ArgAction::Append,
            help = "Hierarchy pair CHILD:PARENT (repeatable), as for derive. Name the image concept by its content, caption [image:<id>]."
        )]
        parent_of: Vec<String>,
        /// DANGEROUS: attach despite a same-width stored/configured model-id mismatch.
        #[arg(long)]
        allow_embedding_mismatch: bool,
    },
    /// Record an action the agent took, with what it produces, modifies and depends on. Timestamps are stamped server-side; do not send one.
    RecordAction {
        /// Session this process writes (acquires the single-writer lease).
        #[arg(
            long,
            help = "Session this process writes (acquires the single-writer lease)."
        )]
        session: String,
        /// Agent identity stamped on the action.
        #[arg(long, help = "Agent identity stamped on the action.")]
        agent: String,
        /// The action taken — becomes a Resource concept.
        #[arg(long, help = "The action taken — becomes a Resource concept.")]
        action: String,
        /// Resources this action creates (Causal edges). Repeatable.
        #[arg(long, action = ArgAction::Append, help = "Resources this action creates (Causal edges). Repeatable.")]
        produces: Vec<String>,
        /// Resources this action mutates (Causal edges). Repeatable.
        #[arg(long, action = ArgAction::Append, help = "Resources this action mutates (Causal edges). Repeatable.")]
        modifies: Vec<String>,
        /// Things this action depends on (Dependency edges). Repeatable.
        #[arg(long = "depends-on", action = ArgAction::Append, help = "Things this action depends on (Dependency edges). Repeatable.")]
        depends_on: Vec<String>,
        /// DANGEROUS: attach despite a same-width stored/configured model-id mismatch.
        #[arg(long)]
        allow_embedding_mismatch: bool,
    },
    /// Take a soft lock on a memory node before editing it. On the CLI the reservation ends when this process exits; it is not durable.
    Reserve {
        /// Session this process writes (acquires the single-writer lease).
        #[arg(
            long,
            help = "Session this process writes (acquires the single-writer lease)."
        )]
        session: String,
        /// Agent identity the soft lock is taken as.
        #[arg(long, help = "Agent identity the soft lock is taken as.")]
        agent: String,
        /// Node to reserve, as a UUID string (from recall or inspect).
        #[arg(
            long,
            help = "Node to reserve, as a UUID string (from recall or inspect)."
        )]
        node: String,
        /// Soft-lock lifetime in seconds (default 30, max 3600). On the CLI the reservation still ends when this process exits.
        #[arg(
            long = "ttl-seconds",
            help = "Soft-lock lifetime in seconds (default 30, max 3600). On the CLI the reservation still ends when this process exits."
        )]
        ttl_seconds: Option<u64>,
        /// DANGEROUS: attach despite a same-width stored/configured model-id mismatch.
        #[arg(long)]
        allow_embedding_mismatch: bool,
    },
    /// Release a soft lock previously taken with reserve. On the CLI a prior reserve already ended when that process exited.
    Release {
        /// Session this process writes (acquires the single-writer lease).
        #[arg(
            long,
            help = "Session this process writes (acquires the single-writer lease)."
        )]
        session: String,
        /// Agent identity the soft lock was taken as.
        #[arg(long, help = "Agent identity the soft lock was taken as.")]
        agent: String,
        /// Node to release, as a UUID string.
        #[arg(long, help = "Node to release, as a UUID string.")]
        node: String,
        /// DANGEROUS: attach despite a same-width stored/configured model-id mismatch.
        #[arg(long)]
        allow_embedding_mismatch: bool,
    },
    /// Migrate a session to the configured embedder: re-embed EVERY concept into the live space and swap the session's contract atomically (K2).
    ///
    /// Run with the NEW embedder configured (e.g. LAMBO_EMBEDDER=candle) after
    /// stopping the serve process that holds the session. Every concept is
    /// embedded BEFORE the graph is touched, and the vector rewrite plus
    /// contract swap flush as ONE transaction with the lease release — a crash
    /// mid-migration leaves the old contract and old vectors together, which
    /// is consistent. Refuses --allow-embedding-mismatch: this verb IS the
    /// migration; the relabel override would leave old-space vectors behind.
    ReEmbed {
        /// Session this process writes (acquires the single-writer lease).
        #[arg(
            long,
            help = "Session this process writes (acquires the single-writer lease)."
        )]
        session: String,
        /// Agent identity this migration writes as.
        #[arg(long, help = "Agent identity this migration writes as.")]
        agent: String,
        /// REFUSED: re-embed is itself the migration; pairing it with the relabel override is contradictory and the flag exists only so the clap shape mirrors the other writers.
        #[arg(long)]
        allow_embedding_mismatch: bool,
        /// Backfill only: embed the concepts that have NO vector, leave every existing vector and the session contract untouched. This is the repair path for a session that accumulated NULL vectors inside its own current space, which the default migration refuses to touch.
        #[arg(long)]
        missing_only: bool,
        /// Full migration only: null the vector of every image concept instead of refusing on it. An image vector cannot be recomputed from its caption; each image concept keeps its caption and source, and deriving the same image again under the new embedder restores its vector.
        #[arg(long)]
        drop_image_vectors: bool,
    },
}

/// `lambo recall-index` actions.
#[derive(Debug, Subcommand)]
enum RecallIndexAction {
    /// Rebuild one session's recall index from the durable store: re-index every stored vector and drop documents the store no longer has. Takes the session's lease, so it is refused while a writer holds the session. Prints one JSON report.
    Backfill {
        /// Session to rebuild.
        #[arg(long, help = "Session to rebuild.")]
        session: String,
    },
}

impl Commands {
    fn name(&self) -> &'static str {
        match self {
            Self::Serve { .. } => "serve",
            Self::ServeWeb { .. } => "serve-web",
            Self::Demo { .. } => "demo",
            Self::Recall { .. } => "recall",
            Self::Saints { .. } => "saints",
            Self::Inspect { .. } => "inspect",
            Self::Stats { .. } => "stats",
            Self::Provision => "provision",
            Self::EraseSession { .. } => "erase-session",
            Self::RecallIndex { .. } => "recall-index",
            Self::Derive { .. } => "derive",
            Self::DeriveImage { .. } => "derive-image",
            Self::RecordAction { .. } => "record-action",
            Self::Reserve { .. } => "reserve",
            Self::Release { .. } => "release",
            Self::ReEmbed { .. } => "re-embed",
        }
    }

    fn needs_embedder(&self) -> bool {
        !matches!(
            self,
            Self::Provision
                | Self::EraseSession { .. }
                | Self::RecallIndex { .. }
                | Self::Saints { .. }
                | Self::Inspect { .. }
                | Self::Stats { .. }
        )
    }

    fn allow_embedding_mismatch(&self) -> bool {
        match self {
            Self::Serve {
                allow_embedding_mismatch,
                ..
            }
            | Self::Demo {
                allow_embedding_mismatch,
                ..
            }
            | Self::Derive {
                allow_embedding_mismatch,
                ..
            }
            | Self::DeriveImage {
                allow_embedding_mismatch,
                ..
            }
            | Self::RecordAction {
                allow_embedding_mismatch,
                ..
            }
            | Self::Reserve {
                allow_embedding_mismatch,
                ..
            }
            | Self::Release {
                allow_embedding_mismatch,
                ..
            }
            | Self::ReEmbed {
                allow_embedding_mismatch,
                ..
            } => *allow_embedding_mismatch,
            _ => false,
        }
    }
}

/// Single construction site for store + embedder (T8.x must reuse this, not rebuild).
enum Resolved {
    Full(Box<ResolvedBackends>),
    StoreOnly {
        store: Box<dyn GraphStore>,
        kind: StoreKind,
        /// The resolved `store.dsn` (file overlaid with the kind's DSN
        /// environment variable). `provision`'s Cockroach arm hands it to
        /// `scripts/provision.sh` so the config file, not the ambient
        /// environment, names the cluster that receives the DDL (E2E-1).
        dsn: Option<String>,
    },
}

fn resolve_for_command(
    cmd: &Commands,
    config: Option<&std::path::Path>,
    loaded: Option<LamboFile>,
) -> Result<Resolved, String> {
    if cmd.needs_embedder() {
        // `serve` reads `lambo.toml` once, in `serve_preflight`, and hands
        // the file down; every other writer loads it here.
        let r = match loaded {
            Some(file) => resolve_backends(file),
            None => resolve_from_config_path(config),
        }
        .map_err(|e| e.to_string())?;
        Ok(Resolved::Full(Box::new(r)))
    } else {
        // Kind comes from the same file resolve_store_only reads; the store is
        // still constructed exactly once.
        let file = LamboFile::load_resolved(config).map_err(|e| e.to_string())?;
        let kind = file.store.kind;
        let dsn = file.store.dsn.clone();
        let store = resolve_store_only(config).map_err(|e| e.to_string())?;
        Ok(Resolved::StoreOnly { store, kind, dsn })
    }
}

/// What `lambo serve` decided before any backend or embedder is built: the
/// transport, the pinned sessions, and the one read of `lambo.toml`, which
/// the backend resolve then consumes.
struct ServePlan {
    transport: Transport,
    pinned: PinnedSessions,
    file: LamboFile,
}

/// `lambo serve`'s checks that need no backend (#32 PR 4), run before the
/// resolve so a usage error costs no model load: the transport, the stdio
/// session rule, the pinned sessions over HTTP, and the `[serve]` notice.
/// `Err` carries the exit code; the message is already printed.
fn serve_preflight(
    session: &[String],
    transport: &str,
    config: Option<&std::path::Path>,
) -> Result<ServePlan, ExitCode> {
    // Diagnostics to stderr, before anything can log: under
    // `--transport stdio`, stdout is the JSON-RPC channel and one stray
    // line on it corrupts the framing.
    lambo::mcp::init_tracing();
    let transport = match transport.parse::<Transport>() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("lambo serve: {e}");
            return Err(ExitCode::from(2));
        }
    };
    // A usage error is reported before the file is read, so a stdio serve
    // given several `--session`s exits 2 whatever state the file is in
    // (#32 PR 8 Sonnet review L1).
    if transport == Transport::Stdio && session.len() > 1 {
        return Err(too_many_stdio_sessions(session.len()));
    }
    let file = match LamboFile::load_resolved(config) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("lambo serve: failed to build backends: {e}");
            return Err(ExitCode::FAILURE);
        }
    };
    // Stdio: the one session, from `--session` or the file (#32 PR 8).
    let stdio_selected;
    let session = if transport == Transport::Stdio {
        stdio_selected = stdio_session(session, &file.serve)?;
        std::slice::from_ref(&stdio_selected)
    } else {
        session
    };
    // Stdio: the one session chosen above. HTTP: the ordered union of
    // `--session` and `[serve] sessions`. Either way checked against `serve`'s own
    // rules (every name addressable by URL once more than one is pinned),
    // here, before any backend is built (#32 review L3).
    let pinned = match lambo::mcp::pin_sessions(session, &file.serve, transport) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("lambo serve: {e}");
            return Err(ExitCode::from(2));
        }
    };
    // `[serve]`: the keys this serve does not enforce yet are named once
    // (#32 PR 1 review L3); never a value.
    file.serve.warn_if_unenforced(transport == Transport::Stdio);
    Ok(ServePlan {
        transport,
        pinned,
        file,
    })
}

/// The one session a stdio serve owns (#32 PR 8): its `--session`, else
/// the `[[serve.projects]]` entry covering the working directory, else
/// `[serve] default_session`, chosen from the file `serve_preflight` already
/// read, before any backend is built.
///
/// More than one `--session` is a usage error (exit 2). No session at all
/// is the clap missing-`--session` text with the resolver's hint (exit 2); a
/// map that cannot decide (two entries for one directory naming different
/// sessions) is a configuration error (exit 1). With `--session` given the
/// file and the working directory are not consulted and nothing is logged.
fn stdio_session(
    session: &[String],
    serve: &lambo::config::ServeConfig,
) -> Result<String, ExitCode> {
    match session {
        [one] => Ok(one.clone()),
        [] => {
            let selected = serve.select_stdio_session(None).map_err(|e| match e {
                lambo::config::SessionSelectionError::Missing(missing) => {
                    session_refusal(Some(&missing.hint()))
                }
                lambo::config::SessionSelectionError::Config(e) => {
                    eprintln!("lambo serve: {e}");
                    ExitCode::FAILURE
                }
            })?;
            log_session_source(&selected);
            Ok(selected.session)
        }
        many => Err(too_many_stdio_sessions(many.len())),
    }
}

/// The usage refusal for a stdio serve given `count` (more than one)
/// `--session`s. Printed here; exit 2.
fn too_many_stdio_sessions(count: usize) -> ExitCode {
    eprintln!(
        "lambo serve: --session was given {count} times, but a stdio serve owns exactly one \
         session; serve several sessions with --transport http"
    );
    ExitCode::from(2)
}

fn emit_stdout(out: &str) {
    print!("{out}");
    if !out.ends_with('\n') {
        println!();
    }
}

fn run_async(
    name: &str,
    fut: impl std::future::Future<Output = Result<String, CliError>>,
) -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lambo {name}: failed to start tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(fut);
    // Write path spawns Memory tasks; readers may have sqlx pool tasks.
    runtime.shutdown_background();
    match result {
        Ok(out) => {
            emit_stdout(&out);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("lambo {name}: {e}");
            ExitCode::from(e.exit_code())
        }
    }
}

/// The missing-session refusal, rendered the way clap rendered a missing
/// required `--session` before the flag became optional (#32 PR 8), with the
/// resolver's hint (or none) under the clap text. Returns clap's usage exit
/// code.
fn session_refusal(hint: Option<&str>) -> ExitCode {
    let mut command = Cli::command();
    command.build();
    let mut message = lambo::config::SESSION_REQUIRED.to_owned();
    if let Some(hint) = hint {
        message.push_str("\n\n");
        message.push_str(hint);
    }
    let err = match command.find_subcommand_mut("serve") {
        Some(serve) => serve.error(clap::error::ErrorKind::MissingRequiredArgument, message),
        None => command.error(clap::error::ErrorKind::MissingRequiredArgument, message),
    };
    let _ = err.print();
    ExitCode::from(2)
}

/// One startup line saying where a stdio serve's session came from, plus one
/// WARN when `~` entries were skipped. Names the configured entry, never the
/// working directory or `$HOME`.
fn log_session_source(selected: &lambo::config::SelectedSession) {
    use lambo::config::SessionSource;
    let session = selected.session.as_str();
    if selected.tilde_entries_skipped {
        tracing::warn!(
            "HOME is not set to an absolute path, so [[serve.projects]] entries starting with `~` \
             were skipped"
        );
    }
    match &selected.source {
        SessionSource::Flag => {}
        SessionSource::Project { path } => tracing::info!(
            session,
            project_path = %path,
            "session selected by the lambo.toml [[serve.projects]] entry covering the working directory"
        ),
        SessionSource::DefaultSession => tracing::info!(
            session,
            "session is [serve] default_session: no [[serve.projects]] entry covers the working directory"
        ),
        SessionSource::DefaultSessionCwdUnavailable => tracing::warn!(
            session,
            "the working directory could not be canonicalized, so [[serve.projects]] was skipped; \
             using [serve] default_session"
        ),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = cli.config.as_deref();

    let Some(cmd) = cli.command else {
        eprintln!("lambo: pass a subcommand (try --help)");
        return ExitCode::from(2);
    };

    let allow_embedding_mismatch = cmd.allow_embedding_mismatch();

    // `lambo serve`'s usage checks, before any backend is built (#32 PR 4).
    let mut serve_plan = match &cmd {
        Commands::Serve {
            session, transport, ..
        } => match serve_preflight(session, transport, config) {
            Ok(plan) => Some(plan),
            Err(code) => return code,
        },
        _ => None,
    };
    let loaded = serve_plan
        .as_mut()
        .map(|plan| std::mem::take(&mut plan.file));

    // Construct once; when Memory/serve land, pass `Resolved` into the command body.
    let mut resolved = match resolve_for_command(&cmd, config, loaded) {
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
    if allow_embedding_mismatch {
        match &mut resolved {
            Resolved::Full(backends) => backends.allow_embedding_mismatch = true,
            Resolved::StoreOnly { .. } => unreachable!("writer override requires full backends"),
        }
    }

    match (cmd, resolved) {
        (
            Commands::Serve {
                session: _,
                agent,
                transport: _,
                port,
                bind,
                auth_token,
                max_sessions,
                rate_limit_rps,
                allow_embedding_mismatch: _,
                ledger,
                ledger_heartbeat,
            },
            Resolved::Full(backends),
        ) => {
            // Tracing is up, the transport parsed and the sessions pinned:
            // `serve_preflight`, before the backends were built.
            let ServePlan {
                transport, pinned, ..
            } = serve_plan.expect("serve_preflight ran for serve");
            // Env beats flag (T8.7) — resolved here, before any of it reaches a
            // log line. A set-but-empty LAMBO_AUTH_TOKEN is a usage error, not
            // a silent fallback to the flag.
            let auth_token = match lambo::mcp::resolve_auth_token(auth_token) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("lambo serve: {e}");
                    return ExitCode::from(2);
                }
            };
            // A zero `--ledger-heartbeat` would spin the heartbeat loop as fast
            // as the executor allows, which is a flood, not a heartbeat. The
            // refusal used to live here and now lives in
            // `mcp::serve::authorize_ledger`, which keeps this message verbatim:
            // a library caller gets the same refusal instead of a panicking
            // heartbeat task, and both ledger configuration errors leave through
            // the one `Err` arm at the end of this block rather than with two
            // different exit codes for the same class of mistake.
            let opts = ServeOptions {
                session: pinned.default,
                sessions: pinned.sessions,
                agent,
                transport,
                port,
                bind,
                auth_token,
                max_sessions,
                rate_limit_rps,
                ledger,
                ledger_heartbeat: ledger_heartbeat.map(std::time::Duration::from_secs),
            };

            // `backends` is the single resolve from `resolve_for_command` above
            // — serve does not resolve again (Level B single construction site).
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("lambo serve: failed to start tokio runtime: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let result = runtime.block_on(lambo::mcp::serve(opts, *backends));

            // Do not let the runtime's `Drop` wait for blocking tasks (R1/T82-1).
            // The stdio transport parks a **blocking** read on stdin, and
            // `Runtime::drop` waits for blocking tasks to return — so after a
            // SIGINT/SIGTERM the process sat there with its session already
            // closed until the client happened to close stdin, and a supervisor
            // escalated to SIGKILL. `serve` has already awaited `Memory::close`
            // by this point, so the tail is durable and the parked read holds
            // nothing but a file descriptor.
            runtime.shutdown_background();

            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("lambo serve: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // T8.5 — the read-only demo window. Reader path: `run_async` owns the
        // runtime exactly as it does for `recall`, and the command body takes
        // the single `ResolvedBackends` from `resolve_for_command` above.
        (
            Commands::ServeWeb {
                session,
                port,
                bind,
                auth_token,
            },
            Resolved::Full(backends),
        ) => run_async(
            "serve-web",
            lambo::cli::serve_web::run(
                *backends,
                lambo::cli::serve_web::Args {
                    session,
                    port,
                    bind,
                    auth_token,
                },
            ),
        ),
        (
            Commands::Demo {
                scenario,
                session,
                allow_embedding_mismatch: _,
            },
            Resolved::Full(backends),
        ) => run_async(
            "demo",
            lambo::cli::demo::run(*backends, lambo::cli::demo::Args { scenario, session }),
        ),
        (
            Commands::Recall {
                session,
                query,
                top_k,
                max_tokens,
                traversal_depth,
                image,
                mime,
                query_vector_json,
            },
            Resolved::Full(backends),
        ) => {
            let query = query.unwrap_or_default();
            if image.is_none() && query_vector_json.is_none() {
                run_async(
                    "recall",
                    lambo::cli::recall::run(
                        &backends,
                        &session,
                        &query,
                        top_k,
                        max_tokens,
                        traversal_depth,
                    ),
                )
            } else {
                let by = lambo::cli::recall::RecallBy {
                    image,
                    mime,
                    query_vector_json,
                };
                run_async(
                    "recall",
                    lambo::cli::recall::run_by(
                        &backends,
                        &session,
                        &query,
                        &by,
                        top_k,
                        max_tokens,
                        traversal_depth,
                    ),
                )
            }
        }
        (Commands::Saints { session }, Resolved::StoreOnly { store, .. }) => {
            run_async("saints", lambo::cli::saints::run(store.as_ref(), &session))
        }
        (
            Commands::Inspect {
                session,
                focus,
                depth,
            },
            Resolved::StoreOnly { store, .. },
        ) => run_async(
            "inspect",
            lambo::cli::inspect::run(store.as_ref(), &session, &focus, depth),
        ),
        (Commands::Stats { session }, Resolved::StoreOnly { store, .. }) => {
            run_async("stats", lambo::cli::stats::run(store.as_ref(), &session))
        }
        (Commands::Provision, Resolved::StoreOnly { store, kind, dsn }) => run_async(
            "provision",
            lambo::cli::provision::run(store, kind, dsn.as_deref()),
        ),
        (
            Commands::RecallIndex {
                action: RecallIndexAction::Backfill { session },
            },
            Resolved::StoreOnly { store, .. },
        ) => run_async(
            "recall-index backfill",
            lambo::cli::recall_index::backfill(
                store.as_ref(),
                lambo::cli::recall_index::Args { session },
            ),
        ),
        (Commands::EraseSession { session, confirm }, Resolved::StoreOnly { store, .. }) => {
            run_async(
                "erase-session",
                lambo::cli::erase_session::run(
                    store.as_ref(),
                    lambo::cli::erase_session::Args { session, confirm },
                ),
            )
        }
        (
            Commands::Derive {
                session,
                agent,
                content,
                kind,
                parent_of,
                concept,
                allow_embedding_mismatch: _,
            },
            Resolved::Full(backends),
        ) => run_async(
            "derive",
            lambo::cli::derive::run(
                *backends,
                lambo::cli::derive::Args {
                    session,
                    agent,
                    content,
                    kind,
                    parent_of,
                    concept,
                },
            ),
        ),
        (
            Commands::DeriveImage {
                session,
                agent,
                caption,
                kind,
                image_id,
                image,
                mime,
                vector_json,
                parent_of,
                allow_embedding_mismatch: _,
            },
            Resolved::Full(backends),
        ) => run_async(
            "derive-image",
            lambo::cli::derive_image::run(
                *backends,
                lambo::cli::derive_image::Args {
                    session,
                    agent,
                    caption,
                    kind,
                    image_id,
                    image,
                    mime,
                    vector_json,
                    parent_of,
                },
            ),
        ),
        (
            Commands::RecordAction {
                session,
                agent,
                action,
                produces,
                modifies,
                depends_on,
                allow_embedding_mismatch: _,
            },
            Resolved::Full(backends),
        ) => run_async(
            "record-action",
            lambo::cli::record_action::run(
                *backends,
                lambo::cli::record_action::Args {
                    session,
                    agent,
                    action,
                    produces,
                    modifies,
                    depends_on,
                },
            ),
        ),
        (
            Commands::Reserve {
                session,
                agent,
                node,
                ttl_seconds,
                allow_embedding_mismatch: _,
            },
            Resolved::Full(backends),
        ) => run_async(
            "reserve",
            lambo::cli::reserve::reserve(
                *backends,
                lambo::cli::reserve::ReserveArgs {
                    session,
                    agent,
                    node,
                    ttl_seconds,
                },
            ),
        ),
        (
            Commands::Release {
                session,
                agent,
                node,
                allow_embedding_mismatch: _,
            },
            Resolved::Full(backends),
        ) => run_async(
            "release",
            lambo::cli::reserve::release(
                *backends,
                lambo::cli::reserve::ReleaseArgs {
                    session,
                    agent,
                    node,
                },
            ),
        ),
        (
            Commands::ReEmbed {
                session,
                agent,
                allow_embedding_mismatch: _,
                missing_only,
                drop_image_vectors,
            },
            Resolved::Full(backends),
        ) => run_async(
            "re-embed",
            lambo::cli::re_embed::run(
                *backends,
                lambo::cli::re_embed::Args {
                    session,
                    agent,

                    // The verb refuses it outright; the resolve-level flip
                    // below still lands on ResolvedBackends and is ignored.
                    allow_embedding_mismatch,
                    missing_only,
                    drop_image_vectors,
                },
            ),
        ),
        _ => {
            // needs_embedder / resolve_for_command pairing is exhaustive in practice.
            eprintln!("lambo: internal resolve mismatch");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    /// A unique scratch directory removed on drop, so a failing test leaves
    /// nothing under `$TMPDIR`. (`main.rs` is its own crate, so it cannot reach
    /// the library's `test_util::ScratchDir`.) Gated like its only user.
    #[cfg(all(feature = "store-memory", feature = "embed-bge"))]
    struct ScratchDir(std::path::PathBuf);

    #[cfg(all(feature = "store-memory", feature = "embed-bge"))]
    impl ScratchDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "lambo-cli-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    #[cfg(all(feature = "store-memory", feature = "embed-bge"))]
    impl std::ops::Deref for ScratchDir {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }

    #[cfg(all(feature = "store-memory", feature = "embed-bge"))]
    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_help_structure() {
        Cli::command().debug_assert();
    }

    #[test]
    fn h1_embedding_mismatch_override_is_explicit_and_writer_only() {
        let parsed = Cli::try_parse_from([
            "lambo",
            "serve",
            "--session",
            "s",
            "--allow-embedding-mismatch",
        ])
        .unwrap();
        assert!(parsed.command.as_ref().unwrap().allow_embedding_mismatch());

        let reader = Cli::try_parse_from([
            "lambo",
            "recall",
            "--session",
            "s",
            "--query",
            "q",
            "--allow-embedding-mismatch",
        ]);
        assert!(
            reader.is_err(),
            "clap must reject the dangerous override on readers"
        );

        let mut writer_help = Vec::new();
        Cli::command()
            .find_subcommand_mut("serve")
            .unwrap()
            .write_long_help(&mut writer_help)
            .unwrap();
        assert!(String::from_utf8(writer_help)
            .unwrap()
            .contains("--allow-embedding-mismatch"));

        for reader in ["recall", "serve-web", "saints", "inspect", "stats"] {
            let mut help = Vec::new();
            Cli::command()
                .find_subcommand_mut(reader)
                .unwrap()
                .write_long_help(&mut help)
                .unwrap();
            assert!(
                !String::from_utf8(help)
                    .unwrap()
                    .contains("--allow-embedding-mismatch"),
                "{reader} help must not advertise a writer-only override"
            );
        }
    }

    #[test]
    fn serve_session_is_optional_at_parse_time() {
        // #32 PR 8: lambo.toml can supply a stdio serve's session, and PR 4
        // an HTTP serve's; the refusals live in `serve_preflight`, after the
        // file is read.
        let parsed = Cli::try_parse_from(["lambo", "serve"]).unwrap();
        assert!(matches!(
            parsed.command,
            Some(Commands::Serve { ref session, .. }) if session.is_empty()
        ));
        let flagged = Cli::try_parse_from(["lambo", "serve", "--session", "s"]).unwrap();
        assert!(matches!(
            flagged.command,
            Some(Commands::Serve { ref session, .. }) if session == &["s"]
        ));
    }

    #[test]
    fn every_subcommand_and_required_arg_has_help() {
        let cmd = Cli::command();
        walk_help(&cmd, "lambo");
    }

    fn walk_help(cmd: &clap::Command, path: &str) {
        for sub in cmd.get_subcommands() {
            if sub.get_name() == "help" {
                continue;
            }
            let here = format!("{path} {}", sub.get_name());
            assert!(
                sub.get_about().is_some() || sub.get_long_about().is_some(),
                "{here} must have about/long_about"
            );
            for arg in sub.get_arguments() {
                let id = arg.get_id().as_str();
                if matches!(id, "help" | "version") {
                    continue;
                }
                assert!(
                    arg.get_help().is_some() || arg.get_long_help().is_some(),
                    "{here} --{id} must have help"
                );
            }
            walk_help(sub, &here);
        }
    }

    #[test]
    fn f18_no_cli_flag_accepts_a_client_timestamp() {
        const BANNED: &[&str] = &[
            "timestamp",
            "created_at",
            "createdat",
            "now",
            "time",
            "when",
            "date",
            "occurred_at",
            "logical_time",
        ];
        fn normalize(raw: &str) -> String {
            raw.to_lowercase().replace('-', "_")
        }
        fn looks_banned(raw: &str) -> Option<&'static str> {
            let n = normalize(raw);
            BANNED.iter().copied().find(|b| n.contains(b))
        }
        fn walk(cmd: &clap::Command) {
            for arg in cmd.get_arguments() {
                let mut tokens = vec![arg.get_id().as_str().to_string()];
                if let Some(long) = arg.get_long() {
                    tokens.push(long.to_string());
                }
                if let Some(aliases) = arg.get_all_aliases() {
                    for alias in aliases {
                        tokens.push(alias.to_string());
                    }
                }
                for token in &tokens {
                    if let Some(hit) = looks_banned(token) {
                        panic!(
                            "F18: CLI flag '{token}' contains banned client-timestamp token '{hit}'"
                        );
                    }
                }
            }
            for sub in cmd.get_subcommands() {
                walk(sub);
            }
        }
        walk(&Cli::command());
    }

    /// E2E-F6: `provision --help` names every kind it provisions. `postgres`
    /// has been a shipped, tested provision path since B1/B2, and it is exactly
    /// the verb the DSN-precedence findings are about, so the operator most in
    /// need of the sentence is the one reading this line.
    #[test]
    fn provision_help_names_every_store_kind_it_provisions() {
        let provision = Cli::command()
            .get_subcommands()
            .find(|c| c.get_name() == "provision")
            .expect("provision subcommand")
            .clone();
        let about = provision
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default();
        for kind in ["SQLite", "Postgres", "Cockroach"] {
            assert!(
                about.contains(kind),
                "provision --help must name {kind}: {about}"
            );
        }
    }

    /// #22 PR 6: `--query` is required unless `--image` or
    /// `--query-vector-json` is given; the two are exclusive, and `--mime`
    /// needs `--image`.
    #[test]
    fn recall_query_is_optional_only_beside_an_image_or_a_vector() {
        let parse = |args: &[&str]| {
            let mut all = vec!["lambo", "recall", "--session", "s"];
            all.extend_from_slice(args);
            Cli::try_parse_from(all)
        };
        assert!(parse(&[]).is_err(), "no query, no image, no vector");
        assert!(parse(&["--query", "q"]).is_ok());
        assert!(parse(&["--image", "a.png"]).is_ok());
        assert!(parse(&["--image", "a.png", "--mime", "image/png", "--query", "q"]).is_ok());
        assert!(parse(&["--query-vector-json", "v.json"]).is_ok());
        assert!(parse(&["--image", "a.png", "--query-vector-json", "v.json"]).is_err());
        assert!(parse(&["--query", "q", "--mime", "image/png"]).is_err());
        assert!(
            parse(&["--image", "a.png"])
                .unwrap()
                .command
                .unwrap()
                .needs_embedder(),
            "a recall by image embeds"
        );
    }

    #[test]
    fn saints_stats_inspect_provision_resolve_store_only() {
        let saints = Cli::try_parse_from(["lambo", "saints", "--session", "s"]).unwrap();
        assert!(
            !saints.command.unwrap().needs_embedder(),
            "saints must not construct an embedder"
        );
        let stats = Cli::try_parse_from(["lambo", "stats", "--session", "s"]).unwrap();
        assert!(
            !stats.command.unwrap().needs_embedder(),
            "stats must not construct an embedder"
        );
        let inspect = Cli::try_parse_from([
            "lambo",
            "inspect",
            "--session",
            "s",
            "--focus",
            "user schema",
        ])
        .unwrap();
        assert!(
            !inspect.command.unwrap().needs_embedder(),
            "inspect must not construct an embedder"
        );
        let provision = Cli::try_parse_from(["lambo", "provision"]).unwrap();
        assert!(!provision.command.unwrap().needs_embedder());
        let recall =
            Cli::try_parse_from(["lambo", "recall", "--session", "s", "--query", "q"]).unwrap();
        assert!(
            recall.command.unwrap().needs_embedder(),
            "recall still embeds when the store claims VECTOR_SEARCH"
        );
    }

    /// #18: the recall-index verb is an operator, store-only path.
    #[test]
    fn recall_index_backfill_parses_and_needs_no_embedder() {
        let cli =
            Cli::try_parse_from(["lambo", "recall-index", "backfill", "--session", "s"]).unwrap();
        let cmd = cli.command.unwrap();
        assert_eq!(cmd.name(), "recall-index");
        assert!(!cmd.needs_embedder());
        assert!(matches!(
            cmd,
            Commands::RecallIndex {
                action: RecallIndexAction::Backfill { ref session }
            } if session == "s"
        ));
        assert!(Cli::try_parse_from(["lambo", "recall-index", "backfill"]).is_err());
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
        // Every variable the resolve reads, from the one list the library
        // owns. `LAMBO_PROMOTION_POLICY` is on it not because a stray value
        // would be *invalid* — an empty one is unset and a bogus one fails
        // closed by design — but because a `LAMBO_PROMOTION_POLICY=Solo` left
        // in the ambient shell would silently change what this test resolves.
        for k in lambo::RESOLVE_ENV_VARS {
            // SAFETY: the libtest harness runs this binary's tests on
            // parallel threads, so "single-threaded" is not the argument.
            // The argument is that this is the only test in the bin target
            // that mutates the environment (`LOCK` only guards against a
            // future second one), and every Rust reader on other threads goes
            // through std, whose environment lock synchronises it with
            // `remove_var`. The residual exposure is a C library reading the
            // environment via libc while this runs, the same one the lib's
            // `test_util::EnvGuard` documents.
            unsafe { std::env::remove_var(k) };
        }

        let dir = ScratchDir::new();
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
    }
}

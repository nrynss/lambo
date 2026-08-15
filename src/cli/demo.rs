//! `lambo demo` — the spec §13 two-agent scenario, scripted and deterministic
//! (T8.4). This module **is** the video's script.
//!
//! Two agents build one REST API against one session. Agent A lays down
//! `user schema` / `auth middleware` / `session store` and records the actions
//! that depend on them; agent B joins on a separate feature; agent A comes back
//! for one last edit; agent B then asks `recall("update user schema")` and is
//! told, by memory rather than by a colleague, that the thing it is about to
//! change is load-bearing and was touched seconds ago.
//!
//! Nothing here is staged. Every concept, edge and canonization transition is
//! produced by the same [`Memory`] surface an MCP client drives, and the three
//! `user schema` transitions are committed by the real
//! [`CanonizationTask`](crate::canon::CanonizationTask) against the real store
//! predicates. There is no code path in this file that writes a
//! [`CanonizationStatus`] or a `canonization_events` row.
//!
//! # Determinism (the T8.4 bar: identical outcomes, every run)
//!
//! "Works three times in five" is not done, so the scenario is built so that
//! its outcome is a **fixed point**, not a snapshot taken at a lucky instant:
//!
//! 1. **The script is fixed.** [`ACT_I`] / [`ACT_II`] / [`ACT_III`] are static
//!    data: the same twelve interactions, in the same order, with the same
//!    contents, every run. Matching is pinned to
//!    [`MatchStrategy::Canonical`] so the write path resolves concepts by
//!    canonical key alone — no embedding lookups on the write path, therefore
//!    no dependence on an embedder's weights, network, or backend.
//! 2. **No wall-clock waits decide anything.** Every wait is a bounded poll on
//!    an observable condition (`wait_until`) — a status in the graph, an
//!    audit-trail length, a `gc_survived` floor, a daemon event, a completed
//!    canonization cycle. Nothing in this file sleeps for a fixed duration and
//!    then assumes progress happened.
//! 3. **The canonization state machine is driven to a unique fixed point.**
//!    See "Why the fixed point is unique" below — this is the part that a
//!    naive scripted demo gets wrong, and it is why the demo settles
//!    `gc_survived` and then quiesces instead of stopping the moment
//!    `user schema` turns Canonical.
//! 4. **The one genuinely time-derived value is normalized, not faked.** The
//!    conflict line renders the true age of agent A's write. On a laptop the
//!    whole session replays in well under a second, so that age reads `0` or
//!    `1`; spec §13's "eleven seconds" is the age at the instant the video's
//!    agent B asks. [`DemoOutcome`] therefore carries the conflict line with
//!    the integer replaced by `<n>` ([`normalize_conflict_age`]) — the rest of
//!    the context block is compared byte for byte.
//!
//! ## Why the fixed point is unique
//!
//! Canonization Stage 1 admits a concept only when `gc_survived >= 3`, and
//! `gc_survived` is bumped by GC, which runs on **session mutations**, not on
//! a timer. That makes "how many sweeps has this concept seen?" a function of
//! how daemon ticks interleaved with writes — a race, and the one that would
//! otherwise make the *other* concepts' statuses wobble between runs.
//!
//! So the demo does two things after the last write:
//!
//! * **Settle** (`settle_gc_survived`): declare one session synonym at a
//!   time — a real spec §7.1 call, agent A teaching the session its aliases —
//!   and wait for the resulting GC sweep, until **every** concept clears the
//!   `gc_survived >= 3` floor. Stage 1's survival gate is then uniformly open
//!   and admission is decided by score alone.
//! * **Quiesce** (`quiesce`): keep polling until the audit trail stops
//!   growing across [`QUIESCE_STABLE_CYCLES`] consecutive completed
//!   canonization cycles.
//!
//! With the graph frozen, every scoring dimension is session-relative
//! (`src/daemon/score.rs`: recency, session activity and density are all
//! measured against the session, never against the wall clock), so the score
//! table is a pure function of the graph. `user schema` is the only concept in
//! the session with a Stage-3 blast radius above the floor, so it is the only
//! concept that can leave the non-Canonical peer set; removing the single
//! highest scorer can only lower the P90 cut, so any concept admitted earlier
//! is still admitted at the end. The fixed point is therefore exactly
//! "every concept whose composite exceeds the final P90", independent of the
//! order in which the cycles happened to run.
//!
//! # The knobs (documented, per the T8.4 brief)
//!
//! Two [`Config`]s are used, and **no threshold is weakened by either** — only
//! intervals and one age floor are compressed, because the demo has to fit in
//! a three-minute video rather than an afternoon.
//!
//! | Knob | Spec default | Build phase (acts I–III) | Canonization phase |
//! |---|---|---|---|
//! | `canonization_edge_min_age` | 60s | 60s (unused: no cycles run) | **10ms** |
//! | `canonization_eval_interval` | 60s | **1h** (frozen) | **25ms** |
//! | `daemon_tick_interval` | 1s | **5ms** | **5ms** |
//! | `gc_interval` | 10 000 mutations | 10 000 (no sweep runs) | **1 mutation** |
//! | `backend_flush_interval` | 1s | **5ms** | **5ms** |
//! | `match_strategy` | Hybrid | **Canonical** | **Canonical** |
//!
//! `canonization_edge_min_age` is the knob the T8.4 brief names. It is the age
//! floor Stage 2 applies to inbound structural edges (`interaction_span`) and
//! Stage 3 applies to the blast-radius query — the guard that stops a burst of
//! same-tick edges from inflating either measure. Compressing it from 60s to
//! 10ms keeps the guard **live** (an edge written in this cycle still does not
//! count; the engine genuinely waits for it to age) while letting a session
//! that is minutes old in demo time behave like one that is an hour old in
//! spec time.
//!
//! Freezing `canonization_eval_interval` during the build is not cosmetic
//! either: it guarantees no cycle ever evaluates a half-built graph, so the
//! state machine starts from one deterministic state. `gc_interval` is left at
//! its spec default for the same span, so no GC sweep can evict a concept out
//! of a partially-written session.
//!
//! Left at spec defaults, deliberately, because they are the thresholds the
//! demo is claiming to satisfy: `canonization_min_peer_count` (20),
//! `canonization_eval_batch_size` (50), `canonization_repromotion_cooldown`
//! (300s), `max_canonical_nodes` (1000), `conflict_recency_window` (30s), the
//! scoring and recall weights, and every stage constant in `src/canon`
//! (`gc_survived >= 3`, strictly above P90, `distinct >= 3`,
//! `coverage >= 0.3`, `blast_radius > 5`).
//!
//! # Fresh sessions (P6 review R3-1)
//!
//! On SQLite and CockroachDB, canonization state is **not** restored over an
//! existing session by the seed path, so re-running the scenario into a live
//! session silently produces a demo that does not transition. The demo
//! therefore mints a fresh session id per run by default
//! ([`fresh_session_id`]); `--session` exists for the live runbook's
//! "inspect what the last run wrote" step and is documented as
//! fresh-only.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;

use super::caps::{check_size_cli, require_nonempty, CliError};
use crate::config::Config;
use crate::embed::Embedder;
use crate::graph::action::Action;
use crate::graph::derive::ParentOf;
use crate::graph::Graph;
use crate::memory::Memory;
use crate::resolve::ResolvedBackends;
use crate::store::GraphStore;
use crate::types::{
    CanonizationStatus, ConceptType, DaemonEvent, EmbeddingContract, MatchStrategy, NodeId,
    RecallQuery,
};

// ---------------------------------------------------------------------------
// Scenario identity
// ---------------------------------------------------------------------------

/// The only scenario v0.1 ships — spec §13's two agents on a REST API.
pub const SCENARIO_REST_API: &str = "rest-api";

/// Every scenario `--scenario` accepts, for help text and the usage error.
pub const SCENARIOS: &[&str] = &[SCENARIO_REST_API];

/// Agent A: builds the API, and makes the last edit agent B is warned about.
pub const AGENT_A: &str = "agent-a";
/// Agent B: works a separate feature, and is the one that calls `recall`.
pub const AGENT_B: &str = "agent-b";

// ---------------------------------------------------------------------------
// Compressed knobs (see the module docs for the full table + rationale)
// ---------------------------------------------------------------------------

/// Stage 2 / Stage 3 structural-edge age floor. Spec default 60s.
pub const DEMO_EDGE_MIN_AGE: Duration = Duration::from_millis(10);
/// Canonization cycle period during the canonization phase. Spec default 60s.
pub const DEMO_EVAL_INTERVAL: Duration = Duration::from_millis(25);
/// Canonization cycle period while the graph is being built: long enough that
/// no cycle can evaluate a half-written session.
pub const BUILD_EVAL_INTERVAL: Duration = Duration::from_secs(3600);
/// Daemon poll period. Spec default 1s.
pub const DEMO_TICK_INTERVAL: Duration = Duration::from_millis(5);
/// Write-behind flush period. Spec default 1s.
pub const DEMO_FLUSH_INTERVAL: Duration = Duration::from_millis(5);
/// GC runs every this many session mutations during the canonization phase.
/// Spec default 10 000 — left at the default during the build.
pub const DEMO_GC_INTERVAL: u64 = 1;

/// Canonization Stage 1's survival floor (`src/canon/stage1.rs`) — mirrored
/// here as the settle target, not redefined: the demo waits for the real gate.
pub const STAGE1_MIN_GC_SURVIVED: i32 = 3;
/// Consecutive completed canonization cycles with an unchanged audit trail
/// before the state machine is called settled.
pub const QUIESCE_STABLE_CYCLES: u64 = 3;

/// Every concept in the scripted graph must sit at least this multiple of GC's
/// step-2 eviction bar. See [`gc_headroom`] for why the demo cannot simply
/// disable GC, and why a collectable concept would break determinism.
pub const MIN_GC_HEADROOM: f64 = 1.25;

/// Ceiling on any single bounded wait. Generous: it exists to turn a hang into
/// a diagnosis, not to pace the demo (every wait returns as soon as its
/// condition holds — typically in single-digit milliseconds).
pub const STEP_DEADLINE: Duration = Duration::from_secs(60);
/// Poll period inside `wait_until`.
pub const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Spacing between scripted interactions.
///
/// This is the one deliberate delay in the file, and it is **not** a wait for
/// progress — it paces the script. Two reasons, both load-bearing:
///
/// * **Determinism.** Interactions are server-stamped from the process clock
///   (`Memory`: deliberately no API to supply one), and the `recency` scoring
///   dimension is each concept's position within the session's temporal
///   extent. Twelve writes issued back to back land microseconds apart, so
///   their *interior spacing* is scheduler jitter — which moves every
///   `recency` value, and with it GC's eviction margins and Stage 1's
///   ordering, run to run. Pacing the script makes the extent a property of
///   the script instead: 10ms between writes dominates the jitter by three
///   orders of magnitude, so the same interaction lands at the same relative
///   position every run.
/// * **It is a video.** Twelve interactions in 300µs is a flicker; a human has
///   to be able to read the narration as it scrolls.
///
/// The whole script costs [`EXPECT_INTERACTIONS`] × this — about a tenth of a
/// second, comfortably inside the 30s conflict-recency window agent B's
/// warning depends on.
pub const STEP_PACING: Duration = Duration::from_millis(10);

/// Session aliases agent A declares while settling `gc_survived`. Each is a
/// real spec §7.1 synonym for `user schema`; each also advances the mutation
/// epoch by one, which is what funds the next GC sweep. Sized with slack —
/// three sweeps are needed, the extras are never reached on a healthy run.
pub const SETTLE_ALIASES: &[&str] = &[
    "user_schema",
    "users table",
    "user model",
    "user record",
    "user entity",
    "user row",
    "user document",
    "user object",
];

// ---------------------------------------------------------------------------
// The strings the demo exists to produce (spec §13, asserted verbatim)
// ---------------------------------------------------------------------------

/// The pillar the whole scenario is about.
pub const USER_SCHEMA: &str = "user schema";
/// The canonical marker recall must render for it (spec §13 step 3).
pub const EXPECT_CANONICAL_LABEL: &str = "user schema [Entity, canonical]";
/// The load-bearing-pillar warning, verbatim including the count (spec §13).
pub const EXPECT_BLAST_WARNING: &str =
    "⚑ Load-bearing pillar — 9 nodes depend on this. Modify with caution.";
/// The conflict line with its one time-derived integer normalized.
pub const EXPECT_CONFLICT_LINE: &str = "Agent A wrote to it <n> seconds ago";
/// Spec §13's ⚑ count, and therefore the number of dependents the script
/// plants under `user schema`.
pub const EXPECT_BLAST_RADIUS: u64 = 9;
/// Interactions the script opens (one per write call, server-stamped).
pub const EXPECT_INTERACTIONS: usize = 12;
/// Concepts the script creates. Asserted so a GC eviction or a canonicalizer
/// collision is a loud failure rather than a quietly different demo.
pub const EXPECT_CONCEPTS: usize = 27;

/// The nine concepts whose only structural inbound source is `user schema`,
/// i.e. exactly the nine the ⚑ warning counts. Nothing else in the script may
/// point at them, or they stop being dependents.
pub const USER_SCHEMA_DEPENDENTS: &[&str] = &[
    "email column",
    "password hash column",
    "user id column",
    "created at column",
    "role column",
    "users table migration",
    "user serializer",
    "user validation rules",
    "user fixtures",
];

/// The three pillars spec §13 step 1 names.
pub const PILLARS: &[&str] = &[USER_SCHEMA, "auth middleware", "session store"];

// ---------------------------------------------------------------------------
// The script
// ---------------------------------------------------------------------------

/// One scripted interaction. Each variant maps 1:1 onto a [`Memory`] write,
/// and each write opens exactly one server-stamped interaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// [`Memory::derive`] — concepts, plus `(parent, child)` hierarchy pairs.
    Derive {
        concepts: &'static [(&'static str, ConceptType)],
        parent_of: &'static [(&'static str, &'static str)],
        narration: &'static str,
    },
    /// [`Memory::record_action`] — a `Resource` concept plus its edges.
    Action {
        action: &'static str,
        produces: &'static [&'static str],
        modifies: &'static [&'static str],
        depends_on: &'static [&'static str],
        narration: &'static str,
    },
}

impl Step {
    /// The one-line label the narration prints for this step.
    pub fn label(&self) -> &'static str {
        match self {
            Step::Derive { narration, .. } | Step::Action { narration, .. } => narration,
        }
    }

    /// `derive` / `record-action` — the verb column of the narration.
    pub fn verb(&self) -> &'static str {
        match self {
            Step::Derive { .. } => "derive",
            Step::Action { .. } => "record-action",
        }
    }
}

/// Act I — agent A builds the API. Nine interactions: three derives that plant
/// the pillars and their nine dependents, six actions that make the rest of
/// the codebase depend on them.
pub const ACT_I: &[Step] = &[
    Step::Derive {
        concepts: &[
            (USER_SCHEMA, ConceptType::Entity),
            ("auth middleware", ConceptType::Entity),
            ("session store", ConceptType::Entity),
        ],
        parent_of: &[],
        narration: "user schema, auth middleware, session store",
    },
    Step::Derive {
        concepts: &[
            ("email column", ConceptType::Entity),
            ("password hash column", ConceptType::Constraint),
            ("user id column", ConceptType::Logic),
        ],
        parent_of: &[
            (USER_SCHEMA, "email column"),
            (USER_SCHEMA, "password hash column"),
            (USER_SCHEMA, "user id column"),
        ],
        narration: "email / password hash / user id columns  (children of user schema)",
    },
    Step::Action {
        action: "write POST /users handler",
        produces: &["handlers/users.rs"],
        modifies: &[],
        depends_on: &[USER_SCHEMA, "auth middleware"],
        narration: "write POST /users handler            depends on user schema",
    },
    Step::Derive {
        concepts: &[
            ("created at column", ConceptType::Entity),
            ("role column", ConceptType::Constraint),
            ("users table migration", ConceptType::Resource),
        ],
        parent_of: &[
            (USER_SCHEMA, "created at column"),
            (USER_SCHEMA, "role column"),
            (USER_SCHEMA, "users table migration"),
        ],
        narration: "created at / role columns, users table migration  (children)",
    },
    Step::Action {
        action: "write session middleware",
        produces: &["middleware/session.rs"],
        modifies: &[],
        depends_on: &["session store", USER_SCHEMA, "handlers/users.rs"],
        narration: "write session middleware             depends on session store, user schema",
    },
    Step::Derive {
        concepts: &[
            ("user serializer", ConceptType::Logic),
            ("user validation rules", ConceptType::Constraint),
            ("user fixtures", ConceptType::Resource),
        ],
        parent_of: &[
            (USER_SCHEMA, "user serializer"),
            (USER_SCHEMA, "user validation rules"),
            (USER_SCHEMA, "user fixtures"),
        ],
        narration: "user serializer, validation rules, fixtures  (children)",
    },
    Step::Action {
        action: "add JWT verification",
        produces: &["middleware/jwt.rs"],
        modifies: &[],
        depends_on: &["auth middleware", USER_SCHEMA, "middleware/session.rs"],
        narration: "add JWT verification                 depends on auth middleware, user schema",
    },
    Step::Action {
        action: "write user repository",
        produces: &["repo/users.rs"],
        modifies: &[],
        depends_on: &[USER_SCHEMA, "handlers/users.rs"],
        narration: "write user repository                depends on user schema",
    },
    Step::Action {
        action: "wire login endpoint",
        produces: &["handlers/login.rs"],
        modifies: &[],
        depends_on: &[
            "auth middleware",
            "session store",
            USER_SCHEMA,
            "handlers/users.rs",
            "middleware/jwt.rs",
            "repo/users.rs",
        ],
        narration: "wire login endpoint                  depends on all three pillars",
    },
];

/// Act II — agent B joins on a separate feature. Two interactions. The second
/// is what puts agent B into `user schema`'s contesting-agent set, which is
/// half of what the spec §13 conflict line needs.
pub const ACT_II: &[Step] = &[
    Step::Derive {
        concepts: &[
            ("rate limiter", ConceptType::Entity),
            ("redis backend", ConceptType::Resource),
        ],
        parent_of: &[],
        narration: "rate limiter, redis backend          (agent B's own feature)",
    },
    Step::Action {
        action: "add rate limiting middleware",
        produces: &["middleware/ratelimit.rs"],
        modifies: &[],
        depends_on: &["auth middleware", USER_SCHEMA, "handlers/login.rs"],
        narration: "add rate limiting middleware         depends on auth middleware, user schema",
    },
];

/// Act III — agent A returns for one last edit on `user schema`. This is the
/// write the conflict line reports, and it is deliberately the newest
/// `Causal`/`Dependency` write incident to the pillar.
pub const ACT_III: &[Step] = &[Step::Action {
    action: "add oauth_id to user schema",
    produces: &[],
    modifies: &[USER_SCHEMA],
    // Four dependencies, not one, and deliberately so: this action and
    // `wire login endpoint` are the two concepts that compete for the
    // non-`user schema` places above canonization Stage 1's P90 cut, and with
    // a near-tie between them the P90 boundary was decided by the last digit
    // of the recency dimension — i.e. by scheduling. Giving the last edit the
    // structure it would really have (the oauth column reaches the repo, the
    // handler and the JWT middleware) separates the two by an order of
    // magnitude more than the jitter, so the Candidate set is the same set
    // every run.
    depends_on: &[
        "auth middleware",
        "repo/users.rs",
        "handlers/users.rs",
        "middleware/jwt.rs",
    ],
    narration: "add oauth_id to user schema          MODIFIES user schema",
}];

// ---------------------------------------------------------------------------
// Arguments and outcome
// ---------------------------------------------------------------------------

/// Parsed `demo` flags.
#[derive(Clone, Debug, Default)]
pub struct Args {
    /// Scenario name. Only [`SCENARIO_REST_API`] exists in v0.1.
    pub scenario: String,
    /// Session id. Defaults to a fresh one per run — see R3-1 in the module
    /// docs; re-running into a used session is not supported.
    pub session: Option<String>,
}

/// One `user schema` promotion, as the audit trail recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    pub content: String,
    pub from: String,
    pub to: String,
    pub blast_radius: Option<i32>,
}

impl std::fmt::Display for Transition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} -> {}", self.content, self.from, self.to)?;
        if let Some(b) = self.blast_radius {
            write!(f, "  (blast radius {b})")?;
        }
        Ok(())
    }
}

/// The deterministic outcome summary — **the ×2 bar**. Two runs of the same
/// scenario on the same backend must produce equal values here, and the demo
/// prints [`DemoOutcome::render`] so a human can diff two runs by eye.
///
/// Everything volatile is deliberately excluded: the session id (fresh per
/// run, R3-1), wall-clock timings, node ids, mutation epochs, `gc_survived`
/// counters, flush lag. The one time-derived string that must be reported —
/// the conflict line's age — is normalized by [`normalize_conflict_age`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DemoOutcome {
    pub scenario: String,
    pub interactions: usize,
    pub concepts: usize,
    pub edges: usize,
    /// Every concept's canonization status, content-sorted.
    pub statuses: Vec<(String, String)>,
    /// The `canonization_events` audit trail, grouped by concept (content
    /// order), hops in commit order within each concept.
    ///
    /// Grouped rather than raw because two concepts promoted in the **same**
    /// cycle are committed in `NodeId`-ascending order and node ids are
    /// `Uuid::new_v4()`: their interleaving is a property of the random ids,
    /// not of the script. Each concept's own hop sequence — the thing spec §13
    /// step 2 asks for — is preserved exactly.
    pub transitions: Vec<Transition>,
    /// Canonical memories in `lambo saints` order: `(content, blast radius)`.
    pub canonical: Vec<(String, u64)>,
    /// Agent B's context block, normalized.
    pub recall_context: String,
    /// Agent B's recall warnings, in order, normalized.
    pub recall_warnings: Vec<String>,
}

impl DemoOutcome {
    /// Stable text rendering — what the CLI prints and what a live ×2 run is
    /// diffed on.
    pub fn render(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "scenario            {}", self.scenario);
        let _ = writeln!(s, "interactions        {}", self.interactions);
        let _ = writeln!(s, "concepts            {}", self.concepts);
        let _ = writeln!(s, "edges               {}", self.edges);
        let _ = writeln!(s, "canonization_events {}", self.transitions.len());
        for t in &self.transitions {
            let _ = writeln!(s, "  {t}");
        }
        let _ = writeln!(s, "canonical           {}", self.canonical.len());
        for (content, radius) in &self.canonical {
            let _ = writeln!(s, "  {content}  blast_radius={radius}");
        }
        let _ = writeln!(s, "statuses");
        for (content, status) in &self.statuses {
            let _ = writeln!(s, "  {status:<10} {content}");
        }
        let _ = writeln!(s, "recall_warnings     {}", self.recall_warnings.len());
        for w in &self.recall_warnings {
            let _ = writeln!(s, "  {w}");
        }
        let _ = writeln!(s, "recall_context");
        for line in self.recall_context.lines() {
            let _ = writeln!(s, "  {line}");
        }
        s
    }
}

/// A completed run: the narration (the video's script, in order) and the
/// outcome summary.
#[derive(Clone, Debug)]
pub struct DemoRun {
    pub transcript: Vec<String>,
    pub outcome: DemoOutcome,
}

// ---------------------------------------------------------------------------
// Narration
// ---------------------------------------------------------------------------

/// Collects the script and, when `echo` is set, streams it to stdout as it
/// happens (the demo is watched live; a transcript printed at the end would
/// be a log, not a demo).
struct Narrator {
    echo: bool,
    lines: Vec<String>,
}

impl Narrator {
    fn new(echo: bool) -> Self {
        Self {
            echo,
            lines: Vec::new(),
        }
    }

    fn say(&mut self, line: impl Into<String>) {
        let line = line.into();
        if self.echo {
            println!("{line}");
        }
        self.lines.push(line);
    }

    fn blank(&mut self) {
        self.say(String::new());
    }

    fn banner(&mut self, title: &str) {
        self.blank();
        let bar = "─".repeat(72usize.saturating_sub(title.chars().count() + 4));
        self.say(format!("── {title} {bar}"));
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// `lambo demo` — resolve-once backends in, printed transcript out.
pub async fn run(backends: ResolvedBackends, args: Args) -> Result<String, CliError> {
    let ResolvedBackends {
        store,
        embedder,
        embedding,
        ..
    } = backends;
    let store: Arc<dyn GraphStore> = Arc::from(store);
    let embedder: Arc<dyn Embedder> = Arc::from(embedder);
    let run = run_scenario(store, embedder, embedding, args, true).await?;
    Ok(run.outcome.render())
}

/// The scenario itself, over already-shared backends.
///
/// `echo` streams the narration to stdout; tests pass `false` and read
/// [`DemoRun::transcript`].
pub async fn run_scenario(
    store: Arc<dyn GraphStore>,
    embedder: Arc<dyn Embedder>,
    embedding: EmbeddingContract,
    args: Args,
    echo: bool,
) -> Result<DemoRun, CliError> {
    let scenario = if args.scenario.is_empty() {
        SCENARIO_REST_API.to_string()
    } else {
        args.scenario.clone()
    };
    if scenario != SCENARIO_REST_API {
        return Err(CliError::Usage(format!(
            "unknown scenario '{scenario}' — valid scenarios: {}",
            SCENARIOS.join(", ")
        )));
    }
    let session = match args.session {
        Some(s) => {
            require_nonempty("session", &s)?;
            check_size_cli("session", &s)?;
            s
        }
        None => fresh_session_id(),
    };

    let mut n = Narrator::new(echo);
    header(&mut n, &scenario, &session, store.as_ref());

    store.init_schema().await.map_err(|e| {
        CliError::Runtime(format!(
            "init_schema: {e}; provision the store first (`lambo provision`)"
        ))
    })?;

    // ---- Acts I–III: build the session -----------------------------------
    n.banner("ACT I — agent-a builds the REST API (9 interactions)");
    let mem = open(
        &store,
        &embedder,
        &embedding,
        &session,
        AGENT_A,
        build_config(),
    )
    .await?;
    play(&mem, ACT_I, &mut n, 1).await?;
    mem.close()
        .await
        .map_err(|e| CliError::Runtime(format!("agent-a close: {e}")))?;
    n.say("  agent-a released the single-writer lease".to_string());

    n.banner("ACT II — agent-b joins on a separate feature (2 interactions)");
    let mem = open(
        &store,
        &embedder,
        &embedding,
        &session,
        AGENT_B,
        build_config(),
    )
    .await?;
    play(&mem, ACT_II, &mut n, ACT_I.len() + 1).await?;
    mem.close()
        .await
        .map_err(|e| CliError::Runtime(format!("agent-b close: {e}")))?;
    n.say("  agent-b released the single-writer lease".to_string());

    n.banner("ACT III — agent-a comes back for one last edit");
    let mem = open(
        &store,
        &embedder,
        &embedding,
        &session,
        AGENT_A,
        build_config(),
    )
    .await?;
    play(&mem, ACT_III, &mut n, ACT_I.len() + ACT_II.len() + 1).await?;
    mem.close()
        .await
        .map_err(|e| CliError::Runtime(format!("agent-a close: {e}")))?;
    n.say("  agent-a released the single-writer lease".to_string());

    // ---- Canonization ----------------------------------------------------
    // A separate attach, and deliberately one that writes nothing: the graph
    // is complete and frozen before the first daemon cycle, so no GC sweep and
    // no canonization cycle can ever see a half-written session. Every sweep
    // from here is funded by exactly one settle synonym and awaited, which is
    // what makes the sequence identical run to run.
    let mem = open(
        &store,
        &embedder,
        &embedding,
        &session,
        AGENT_A,
        canonization_config(),
    )
    .await?;

    // Check the facts the whole demo rests on before waiting on anything, so a
    // broken script fails here with a diagnosis instead of timing out.
    assert_shape(mem.graph(), &mut n)?;

    n.banner("CANONIZATION — the engine, not the script, promotes user schema");
    settle_gc_survived(&mem, &mut n).await?;
    let transitions = await_progression(&mem, &mut n).await?;
    quiesce(&mem, &mut n).await?;

    let canonical: Vec<(String, u64)> = mem
        .canonical_memories()
        .into_iter()
        .map(|c| (c.content, c.blast_radius))
        .collect();
    mem.close()
        .await
        .map_err(|e| CliError::Runtime(format!("agent-a close: {e}")))?;
    n.say("  agent-a released the single-writer lease".to_string());

    // ---- Act IV: agent B recalls ------------------------------------------
    n.banner("ACT IV — agent-b: recall(\"update user schema\")");
    let mem = open(
        &store,
        &embedder,
        &embedding,
        &session,
        AGENT_B,
        canonization_config(),
    )
    .await?;
    await_conflict(&mem, &mut n).await?;
    let result = recall_until_complete(&mem, &mut n).await?;

    let (interactions, concepts, edges, statuses) = {
        let g = mem.graph().read();
        let mut statuses: Vec<(String, String)> = g
            .concepts()
            .map(|c| (c.content.clone(), format!("{:?}", c.canonization_status)))
            .collect();
        statuses.sort();
        (
            g.interactions().count(),
            g.concepts().count(),
            g.edge_count(),
            statuses,
        )
    };
    mem.close()
        .await
        .map_err(|e| CliError::Runtime(format!("agent-b close: {e}")))?;

    let outcome = DemoOutcome {
        scenario,
        interactions,
        concepts,
        edges,
        statuses,
        transitions,
        canonical,
        recall_context: normalize_volatile(&result.context),
        recall_warnings: result
            .warnings
            .iter()
            .map(|w| normalize_volatile(w))
            .collect(),
    };

    n.banner("OUTCOME — the ×2 determinism bar");
    for line in outcome.render().lines() {
        n.say(format!("  {line}"));
    }
    n.blank();

    Ok(DemoRun {
        transcript: n.lines,
        outcome,
    })
}

// ---------------------------------------------------------------------------
// Configs
// ---------------------------------------------------------------------------

/// Acts I–III: canonization frozen, GC at its spec default (no sweep can run),
/// flush and daemon compressed so the build is not paced by 1s timers.
pub fn build_config() -> Config {
    Config {
        match_strategy: MatchStrategy::Canonical,
        daemon_tick_interval: DEMO_TICK_INTERVAL,
        backend_flush_interval: DEMO_FLUSH_INTERVAL,
        canonization_eval_interval: BUILD_EVAL_INTERVAL,
        ..Config::default()
    }
}

/// The canonization phase and agent B's read: the compressed knobs from the
/// module docs. Every stage threshold is untouched.
pub fn canonization_config() -> Config {
    let mut cfg = build_config();
    cfg.canonization_eval_interval = DEMO_EVAL_INTERVAL;
    cfg.canonization_edge_min_age = DEMO_EDGE_MIN_AGE;
    cfg.gc_interval = DEMO_GC_INTERVAL;
    cfg
}

/// A fresh session id, per the R3-1 carveout.
pub fn fresh_session_id() -> String {
    format!("demo-{SCENARIO_REST_API}-{}", uuid::Uuid::new_v4())
}

// ---------------------------------------------------------------------------
// Playing the script
// ---------------------------------------------------------------------------

async fn open(
    store: &Arc<dyn GraphStore>,
    embedder: &Arc<dyn Embedder>,
    embedding: &EmbeddingContract,
    session: &str,
    agent: &str,
    config: Config,
) -> Result<Memory, CliError> {
    Memory::builder()
        .session(session)
        .agent(agent)
        .store(store.clone())
        .embedder(embedder.clone())
        .embedding_contract(embedding.clone())
        .config(config)
        .build()
        .await
        .map_err(|e| CliError::Runtime(format!("{agent}: {e}")))
}

/// Apply one act, narrating each interaction as it lands.
async fn play(
    mem: &Memory,
    steps: &[Step],
    n: &mut Narrator,
    first_index: usize,
) -> Result<(), CliError> {
    for (offset, step) in steps.iter().enumerate() {
        let index = first_index + offset;
        // Pace the script (see `STEP_PACING`) before the write, so the very
        // first interaction of an act is spaced from the last one of the act
        // before it too.
        tokio::time::sleep(STEP_PACING).await;
        match step {
            Step::Derive {
                concepts,
                parent_of,
                ..
            } => {
                let outcome = mem
                    .derive(concepts, &ParentOf::from_pairs(parent_of))
                    .await
                    .map_err(|e| CliError::Runtime(format!("derive: {e}")))?;
                n.say(format!(
                    "  [{index:>2}] {:<14} {}  → {} created, {} matched",
                    step.verb(),
                    step.label(),
                    outcome.created.len(),
                    outcome.matched.len()
                ));
            }
            Step::Action {
                action,
                produces,
                modifies,
                depends_on,
                ..
            } => {
                let outcome = mem
                    .record_action(&Action {
                        action,
                        produces,
                        modifies,
                        depends_on,
                    })
                    .map_err(|e| CliError::Runtime(format!("record-action: {e}")))?;
                n.say(format!(
                    "  [{index:>2}] {:<14} {}  → {} created, {} edges",
                    step.verb(),
                    step.label(),
                    outcome.created.len(),
                    outcome.edges
                ));
            }
        }
    }
    Ok(())
}

/// The two structural facts the demo claims, checked the moment the graph is
/// complete: exactly [`EXPECT_CONCEPTS`] concepts, and exactly
/// [`EXPECT_BLAST_RADIUS`] dependents under `user schema`.
fn assert_shape(graph: &Arc<RwLock<Graph>>, n: &mut Narrator) -> Result<(), CliError> {
    let g = graph.read();
    let concepts = g.concepts().count();
    let interactions = g.interactions().count();
    let node = concept_id(&g, USER_SCHEMA).ok_or_else(|| {
        CliError::Runtime(format!("demo: '{USER_SCHEMA}' is missing from the graph"))
    })?;
    let radius = crate::recall::format::blast_radius(&g, node);
    let missing: Vec<&str> = USER_SCHEMA_DEPENDENTS
        .iter()
        .copied()
        .filter(|d| concept_id(&g, d).is_none())
        .collect();
    drop(g);

    if !missing.is_empty() {
        return Err(CliError::Runtime(format!(
            "demo: dependents missing from the graph: {}",
            missing.join(", ")
        )));
    }
    if concepts != EXPECT_CONCEPTS {
        return Err(CliError::Runtime(format!(
            "demo: expected {EXPECT_CONCEPTS} concepts after the script, found {concepts} \
             (a GC eviction or a canonicalizer collision changed the scripted graph)"
        )));
    }
    if interactions != EXPECT_INTERACTIONS {
        return Err(CliError::Runtime(format!(
            "demo: expected {EXPECT_INTERACTIONS} interactions, found {interactions}"
        )));
    }
    if radius != EXPECT_BLAST_RADIUS {
        return Err(CliError::Runtime(format!(
            "demo: '{USER_SCHEMA}' has blast radius {radius}, expected {EXPECT_BLAST_RADIUS} \
             — the ⚑ warning spec §13 quotes would not match"
        )));
    }
    n.say(format!(
        "  graph complete: {interactions} interactions, {concepts} concepts, \
         '{USER_SCHEMA}' blast radius {radius}"
    ));

    let headroom = gc_headroom(&graph.read());
    let (weakest, ratio) = headroom
        .first()
        .cloned()
        .unwrap_or_else(|| (String::new(), f64::INFINITY));
    if ratio < MIN_GC_HEADROOM {
        return Err(CliError::Runtime(format!(
            "demo: '{weakest}' sits at {ratio:.2}× GC's eviction bar (floor \
             {MIN_GC_HEADROOM:.2}×). GC has to run — Stage 1's `gc_survived >= 3` gate has \
             no other source — so a collectable concept would make the concept count and \
             the ⚑ count depend on how many sweeps ran. Give it more structure in the \
             script rather than turning GC off."
        )));
    }
    n.say(format!(
        "  GC headroom: closest to the eviction bar is '{weakest}' at {ratio:.2}× \
         — nothing in this session is collectable"
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Driving canonization (never faking it)
// ---------------------------------------------------------------------------

/// Lift every concept over Stage 1's `gc_survived >= 3` floor.
///
/// GC runs on session mutations, so an idle session's counters stop climbing
/// (`src/daemon/mod.rs`, NEW-2). Each synonym advances the epoch by one, which
/// funds exactly one sweep; the wait is on the observed counter, never on a
/// duration. See the module docs for why this is what makes the fixed point
/// unique.
async fn settle_gc_survived(mem: &Memory, n: &mut Narrator) -> Result<(), CliError> {
    let mut used = 0usize;
    loop {
        let floor = min_gc_survived(mem.graph());
        if floor >= STAGE1_MIN_GC_SURVIVED {
            n.say(format!(
                "  gc_survived floor {floor} ≥ {STAGE1_MIN_GC_SURVIVED} — Stage 1's survival \
                 gate is open for every concept"
            ));
            return Ok(());
        }
        let alias = SETTLE_ALIASES.get(used).copied().ok_or_else(|| {
            CliError::Runtime(format!(
                "demo: ran out of settle aliases with gc_survived floor {floor} \
                 (< {STAGE1_MIN_GC_SURVIVED}); GC is not sweeping"
            ))
        })?;
        mem.declare_synonym(alias, USER_SCHEMA)
            .map_err(|e| CliError::Runtime(format!("declare_synonym: {e}")))?;
        used += 1;
        let graph = mem.graph().clone();
        wait_until(&format!("gc sweep after synonym '{alias}'"), move || {
            min_gc_survived(&graph) > floor
        })
        .await?;
    }
}

/// Wait for `user schema`'s three hops, narrating each as the audit trail
/// records it. Returns the whole trail in commit order.
async fn await_progression(mem: &Memory, n: &mut Narrator) -> Result<Vec<Transition>, CliError> {
    let target = [
        CanonizationStatus::Candidate,
        CanonizationStatus::Venerable,
        CanonizationStatus::Canonical,
    ];
    for want in target {
        let graph = mem.graph().clone();
        wait_until(&format!("user schema → {want:?}"), move || {
            status_of(&graph, USER_SCHEMA) == Some(want)
        })
        .await?;
        let cycles = mem.stats().canonization_cycles;
        n.say(format!(
            "  cycle {cycles:>3}   {USER_SCHEMA:<24} → {want:?}   (canonization_events row written)"
        ));
    }
    Ok(trail(mem.graph()))
}

/// Wait until the audit trail stops growing across [`QUIESCE_STABLE_CYCLES`]
/// completed canonization cycles: the state machine's fixed point.
async fn quiesce(mem: &Memory, n: &mut Narrator) -> Result<(), CliError> {
    loop {
        let events = trail(mem.graph()).len();
        let from = mem.stats().canonization_cycles;
        wait_until("canonization cycles", || {
            mem.stats().canonization_cycles >= from + QUIESCE_STABLE_CYCLES
        })
        .await?;
        if trail(mem.graph()).len() == events {
            n.say(format!(
                "  no transitions for {QUIESCE_STABLE_CYCLES} consecutive cycles — \
                 the state machine is at its fixed point ({events} events total)"
            ));
            return Ok(());
        }
    }
}

/// Wait for the daemon to publish the `Conflict` on `user schema`.
///
/// [`Memory::events`]'s first receiver was subscribed before the daemon was
/// spawned, so the warm-up cycle's condition set cannot be missed.
async fn await_conflict(mem: &Memory, n: &mut Narrator) -> Result<(), CliError> {
    let node = concept_id(&mem.graph().read(), USER_SCHEMA)
        .ok_or_else(|| CliError::Runtime(format!("demo: '{USER_SCHEMA}' missing on reattach")))?;
    let mut rx = mem.events();
    let deadline = tokio::time::Instant::now() + STEP_DEADLINE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(CliError::Runtime(format!(
                "demo: timed out after {STEP_DEADLINE:?} waiting for the Conflict event on \
                 '{USER_SCHEMA}'"
            )));
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(DaemonEvent::Conflict {
                node_id, agents, ..
            })) if node_id == node => {
                let names: Vec<String> = agents.iter().map(|a| a.as_str().to_string()).collect();
                n.say(format!(
                    "  daemon event: Conflict on '{USER_SCHEMA}' — contesting agents: {}",
                    names.join(", ")
                ));
                return Ok(());
            }
            Ok(Ok(_)) => continue,
            // A lagging receiver re-syncs; the recall retry below is the
            // acceptance gate either way.
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                return Err(CliError::Runtime(
                    "demo: daemon event channel closed before the Conflict event".into(),
                ))
            }
            Err(_) => {
                return Err(CliError::Runtime(format!(
                    "demo: timed out after {STEP_DEADLINE:?} waiting for the Conflict event on \
                     '{USER_SCHEMA}'"
                )))
            }
        }
    }
}

/// Recall until the context block carries all three spec §13 strings, then
/// narrate the block verbatim. Bounded; the retry exists because the hot list
/// is refreshed by the daemon loop, not by the reader.
async fn recall_until_complete(
    mem: &Memory,
    n: &mut Narrator,
) -> Result<crate::types::RecallResult, CliError> {
    let deadline = tokio::time::Instant::now() + STEP_DEADLINE;
    loop {
        let result = mem
            .recall(RecallQuery {
                query: "update user schema".into(),
                top_k: 5,
                max_tokens: 500,
                traversal_depth: 2,
            })
            .await
            .map_err(|e| CliError::Runtime(format!("recall: {e}")))?;
        if let Some(missing) = missing_strings(&result.context) {
            if tokio::time::Instant::now() >= deadline {
                return Err(CliError::Runtime(format!(
                    "demo: context block never carried {} within {STEP_DEADLINE:?}\n\
                     ---- last context ----\n{}\n----------------------",
                    missing.join(", "),
                    result.context
                )));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }
        n.blank();
        for line in result.context.lines() {
            n.say(format!("  {line}"));
        }
        n.blank();
        n.say("  agent-b does not make the breaking change.".to_string());
        return Ok(result);
    }
}

/// Which of the three spec §13 strings the context is still missing, or `None`
/// when it carries all of them.
pub fn missing_strings(context: &str) -> Option<Vec<&'static str>> {
    let mut missing = Vec::new();
    if !context.contains(EXPECT_CANONICAL_LABEL) {
        missing.push(EXPECT_CANONICAL_LABEL);
    }
    if !context.contains(EXPECT_BLAST_WARNING) {
        missing.push(EXPECT_BLAST_WARNING);
    }
    if !normalize_conflict_age(context).contains(EXPECT_CONFLICT_LINE) {
        missing.push(EXPECT_CONFLICT_LINE);
    }
    (!missing.is_empty()).then_some(missing)
}

// ---------------------------------------------------------------------------
// Bounded waiting
// ---------------------------------------------------------------------------

/// Poll `cond` every [`POLL_INTERVAL`] until it holds, or fail with a named
/// diagnosis after [`STEP_DEADLINE`]. The only waiting primitive in this
/// module: nothing here sleeps for a fixed duration and assumes progress.
async fn wait_until<F: FnMut() -> bool>(label: &str, mut cond: F) -> Result<(), CliError> {
    let deadline = tokio::time::Instant::now() + STEP_DEADLINE;
    loop {
        if cond() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(CliError::Runtime(format!(
                "demo: timed out after {STEP_DEADLINE:?} waiting for {label}"
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// Graph readers
// ---------------------------------------------------------------------------

/// Every concept's distance from GC's step-2 eviction bar, closest first:
/// `(content, eviction_score / bar)`. A ratio at or below `1.0` means GC will
/// collect that concept on its next sweep.
///
/// This is a **determinism precondition**, not a nicety. GC cannot simply be
/// switched off for the demo: canonization Stage 1 gates on `gc_survived >= 3`
/// and GC's survivor bump is the only thing in the system that raises it, so a
/// demo with GC disabled has no transitions at all — the exact fakery the task
/// forbids. GC therefore runs, and the script instead has to be a *healthy*
/// session: one where the sub-threshold clause has nothing to collect. If it
/// did collect something, the ⚑ count and the concept total would depend on
/// how many sweeps happened to run.
///
/// `min_concept_score` (0.12) is not a [`Config`] key — the daemon passes
/// `GcParams::default()` for it — so this is measured against the same
/// constant GC uses, and mirrors GC's own two adjustments: the live-dimension
/// score while `access_count` is dead session-wide (ALGO-1) and the per-type
/// bar (ALGO-11).
pub fn gc_headroom(graph: &Graph) -> Vec<(String, f64)> {
    use crate::daemon::gc::MIN_CONCEPT_SCORE;
    use crate::daemon::score::{score, score_concept, score_over_live_dimensions, SessionContext};

    let ctx = SessionContext::compute(graph);
    let weights = Config::default().scoring;
    let frequency_is_live = graph.concepts().any(|c| c.access_count > 0);
    let mut out: Vec<(String, f64)> = graph
        .concepts()
        .map(|c| {
            let dims = score_concept(graph, c, &ctx);
            let value = if frequency_is_live {
                score(dims, &weights)
            } else {
                score_over_live_dimensions(dims, &weights)
            };
            let bar = MIN_CONCEPT_SCORE / c.concept_type.eviction_resistance();
            (c.content.clone(), value / bar)
        })
        .collect();
    out.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    out
}

fn concept_id(graph: &Graph, content: &str) -> Option<NodeId> {
    graph
        .concepts()
        .find(|c| c.content == content)
        .map(|c| c.id)
}

fn status_of(graph: &Arc<RwLock<Graph>>, content: &str) -> Option<CanonizationStatus> {
    graph
        .read()
        .concepts()
        .find(|c| c.content == content)
        .map(|c| c.canonization_status)
}

fn min_gc_survived(graph: &Arc<RwLock<Graph>>) -> i32 {
    graph
        .read()
        .concepts()
        .map(|c| c.gc_survived)
        .min()
        .unwrap_or(0)
}

/// The `canonization_events` audit trail, resolved to concept contents and
/// grouped by concept (see [`DemoOutcome::transitions`]). A stable sort keeps
/// each concept's hops in commit order.
fn trail(graph: &Arc<RwLock<Graph>>) -> Vec<Transition> {
    let g = graph.read();
    let contents: HashMap<NodeId, String> =
        g.concepts().map(|c| (c.id, c.content.clone())).collect();
    let mut out: Vec<Transition> = g
        .canonization_events()
        .iter()
        .map(|e| Transition {
            content: contents
                .get(&e.node_id)
                .cloned()
                .unwrap_or_else(|| e.node_id.0.to_string()),
            from: format!("{:?}", e.from_status),
            to: format!("{:?}", e.to_status),
            blast_radius: e.blast_radius,
        })
        .collect();
    out.sort_by(|a, b| a.content.cmp(&b.content));
    out
}

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

/// Everything the ×2 comparison must not see: the conflict line's age
/// ([`normalize_conflict_age`]) and the rendered composite score
/// ([`normalize_score`]).
///
/// The hit *ordering*, every concept's content, the `[Entity, canonical]`
/// marker, `blast radius 9`, the ⚑ line and the conflict sentence all survive
/// this untouched and are compared byte for byte.
pub fn normalize_volatile(text: &str) -> String {
    normalize_node_ids(&normalize_score(&normalize_conflict_age(text)))
}

/// Replace every UUID-shaped token with `<node>`.
///
/// Node ids are `Uuid::new_v4()` — random by construction, and one reaches the
/// rendered surface: the high-risk warning names the node it fired on. The
/// warning's text, condition and the fact that it fired on the pillar are all
/// still compared; only the identifier is masked.
pub fn normalize_node_ids(text: &str) -> String {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if is_uuid_at(bytes, i) {
            out.push_str("<node>");
            i += GROUPS.iter().sum::<usize>() + 4;
        } else {
            // Advance one char, not one byte: the context block is UTF-8 and
            // carries `⚑`.
            let ch = text[i..].chars().next().expect("char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Whether a canonical 8-4-4-4-12 hex UUID starts at byte `i`.
fn is_uuid_at(bytes: &[u8], i: usize) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut at = i;
    for (g, len) in GROUPS.iter().enumerate() {
        if g > 0 {
            if bytes.get(at) != Some(&b'-') {
                return false;
            }
            at += 1;
        }
        for _ in 0..*len {
            match bytes.get(at) {
                Some(b) if b.is_ascii_hexdigit() => at += 1,
                _ => return false,
            }
        }
    }
    true
}

/// Replace `(score X.XX` with `(score <s>`.
///
/// The composite's `recency` dimension is each concept's position inside the
/// session's real temporal extent, and interactions are server-stamped from
/// the process clock. [`STEP_PACING`] makes that extent the script's rather
/// than the scheduler's, which pins the *ordering* — the meaningful part, and
/// the part the context block's line order still asserts — but the second
/// decimal of a score is a wall-clock measurement and replaying it bit for bit
/// would be a claim the system does not make.
pub fn normalize_score(text: &str) -> String {
    const HEAD: &str = "(score ";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(h) = rest.find(HEAD) {
        let split = h + HEAD.len();
        let after = &rest[split..];
        let digits = after
            .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
            .unwrap_or(after.len());
        out.push_str(&rest[..split]);
        if digits > 0 {
            out.push_str("<s>");
        }
        rest = &after[digits..];
    }
    out.push_str(rest);
    out
}

/// Replace the age in every `... wrote to it <N> seconds ago` with `<n>`.
///
/// That integer is the **true** age of agent A's write at read time (T5.3
/// re-validates the hot-list payload against the caller's clock), so it is the
/// one value in the context block a replay cannot reproduce bit for bit. It is
/// normalized rather than frozen: the demo prints the real line, and only the
/// determinism comparison sees `<n>`.
pub fn normalize_conflict_age(text: &str) -> String {
    const HEAD: &str = " wrote to it ";
    const TAIL: &str = " seconds ago";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(h) = rest.find(HEAD) {
        let split = h + HEAD.len();
        let after = &rest[split..];
        let Some(t) = after.find(TAIL) else { break };
        let digits = &after[..t];
        out.push_str(&rest[..split]);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            out.push_str("<n>");
            rest = &after[t..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn header(n: &mut Narrator, scenario: &str, session: &str, store: &dyn GraphStore) {
    n.say("═".repeat(72));
    n.say(format!(
        "  lambo demo — scenario {scenario}   (spec §13: two agents, one REST API)"
    ));
    n.say("═".repeat(72));
    n.say(format!(
        "  session      {session}   (fresh per run — P6 R3-1)"
    ));
    n.say(format!("  capabilities {:?}", store.capabilities()));
    n.say(format!(
        "  agents       {AGENT_A} (builds the API) · {AGENT_B} (separate feature)"
    ));
    n.blank();
    n.say("  Compressed for the video — intervals only, no threshold weakened:".to_string());
    n.say(format!(
        "    canonization_edge_min_age   60s     → {DEMO_EDGE_MIN_AGE:?}"
    ));
    n.say(format!(
        "    canonization_eval_interval  60s     → {DEMO_EVAL_INTERVAL:?}  \
         (frozen during the build)"
    ));
    n.say(format!(
        "    daemon_tick_interval        1s      → {DEMO_TICK_INTERVAL:?}"
    ));
    n.say(format!(
        "    backend_flush_interval      1s      → {DEMO_FLUSH_INTERVAL:?}"
    ));
    n.say(format!(
        "    gc_interval                 10000   → {DEMO_GC_INTERVAL} mutation \
         (spec default during the build)"
    ));
    n.say(
        "  Untouched: min_peer_count 20, gc_survived ≥ 3, strictly > P90, distinct ≥ 3,"
            .to_string(),
    );
    n.say("  coverage ≥ 0.3, blast radius > 5, conflict window 30s.".to_string());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_the_conflict_age_and_nothing_else() {
        let ctx = "user schema [Entity, canonical] (score 0.71, blast radius 9)\n\
                   ⚑ Load-bearing pillar — 9 nodes depend on this. Modify with caution.\n\
                   Agent A wrote to it 11 seconds ago";
        let got = normalize_conflict_age(ctx);
        assert!(got.contains(EXPECT_CONFLICT_LINE), "{got}");
        assert!(got.contains(EXPECT_CANONICAL_LABEL), "{got}");
        assert!(got.contains(EXPECT_BLAST_WARNING), "{got}");
        assert!(!got.contains("11 seconds ago"), "{got}");
    }

    #[test]
    fn normalization_is_idempotent_and_total_on_multiple_lines() {
        let two = "Agent A wrote to it 0 seconds ago\nAgent B wrote to it 250 seconds ago";
        let once = normalize_conflict_age(two);
        assert_eq!(once, normalize_conflict_age(&once));
        assert_eq!(
            once,
            "Agent A wrote to it <n> seconds ago\nAgent B wrote to it <n> seconds ago"
        );
    }

    #[test]
    fn normalization_leaves_non_numeric_matches_alone() {
        let s = "Agent A wrote to it many seconds ago";
        assert_eq!(normalize_conflict_age(s), s);
        assert_eq!(normalize_conflict_age("nothing to see"), "nothing to see");
        assert_eq!(normalize_conflict_age(""), "");
    }

    #[test]
    fn normalize_score_masks_the_number_and_keeps_the_blast_radius() {
        let line = "user schema [Entity, canonical] (score 2.27, blast radius 9)";
        assert_eq!(
            normalize_score(line),
            "user schema [Entity, canonical] (score <s>, blast radius 9)"
        );
        assert_eq!(
            normalize_score("a [Resource] (score 0.13)\nb [Logic] (score 12.5)"),
            "a [Resource] (score <s>)\nb [Logic] (score <s>)"
        );
        // Idempotent, and a total function on text that has no score at all.
        let once = normalize_score(line);
        assert_eq!(once, normalize_score(&once));
        assert_eq!(normalize_score("no score here"), "no score here");
    }

    #[test]
    fn normalize_node_ids_masks_uuids_and_preserves_multibyte_text() {
        let line = format!(
            "{EXPECT_BLAST_WARNING}\nHigh-risk modification: high-value node \
             445370df-69b2-47ac-94ab-b18b52b8b100 (Canonical, blast radius 9)"
        );
        let got = normalize_node_ids(&line);
        assert!(got.contains("high-value node <node> (Canonical"), "{got}");
        // The ⚑ line is multibyte and must survive byte-wise scanning intact.
        assert!(got.contains(EXPECT_BLAST_WARNING), "{got}");
        assert!(!got.contains("445370df"), "{got}");
        assert_eq!(got, normalize_node_ids(&got));
        // Not a UUID: too short, and a hex-looking word.
        assert_eq!(normalize_node_ids("deadbeef-1234"), "deadbeef-1234");
    }

    #[test]
    fn normalize_volatile_masks_all_three_and_keeps_every_spec_string() {
        let ctx = "user schema [Entity, canonical] (score 2.27, blast radius 9)\n\
                   ⚑ Load-bearing pillar — 9 nodes depend on this. Modify with caution.\n\
                   Agent A wrote to it 11 seconds ago\n\
                   High-risk modification: high-value node \
                   445370df-69b2-47ac-94ab-b18b52b8b100 (Canonical, blast radius 9)";
        let got = normalize_volatile(ctx);
        assert!(got.contains(EXPECT_CANONICAL_LABEL), "{got}");
        assert!(got.contains(EXPECT_BLAST_WARNING), "{got}");
        assert!(got.contains(EXPECT_CONFLICT_LINE), "{got}");
        assert!(got.contains("blast radius 9"), "{got}");
        assert!(got.contains("(score <s>"), "{got}");
        assert!(got.contains("<node>"), "{got}");
        assert_eq!(got, normalize_volatile(&got), "must be idempotent");
    }

    /// How many scripted actions name `content` — the coarse structural
    /// signature that separates otherwise-identical siblings.
    fn action_mentions(content: &str) -> usize {
        ACT_I
            .iter()
            .chain(ACT_II)
            .chain(ACT_III)
            .filter(|s| match s {
                Step::Action {
                    produces,
                    modifies,
                    depends_on,
                    ..
                } => {
                    produces.contains(&content)
                        || modifies.contains(&content)
                        || depends_on.contains(&content)
                }
                Step::Derive { .. } => false,
            })
            .count()
    }

    /// Two concepts derived in one interaction with the same structure score
    /// **exactly** equal, and an exact tie is broken by `NodeId` — a random
    /// UUID, so the demo's rank order would change run to run. The tie-break
    /// is distinct concept types.
    ///
    /// Siblings that differ structurally (the three pillars: 8 / 5 / 2 actions
    /// name them) cannot tie in the first place, so the rule is scoped to
    /// siblings with the same signature.
    #[test]
    fn structurally_identical_siblings_carry_distinct_concept_types() {
        for step in ACT_I.iter().chain(ACT_II).chain(ACT_III) {
            let Step::Derive { concepts, .. } = step else {
                continue;
            };
            let mut seen: Vec<(usize, ConceptType)> = Vec::new();
            for (content, kind) in concepts.iter() {
                let signature = (action_mentions(content), *kind);
                assert!(
                    !seen.contains(&signature),
                    "'{content}' is structurally identical to an earlier sibling in the same \
                     derive AND shares its concept type — their scores tie exactly and the \
                     order becomes NodeId (random) order"
                );
                seen.push(signature);
            }
        }
        // The pillars are the exemption this rule relies on: same type, but
        // separated by how much of the codebase depends on them.
        let mentions: Vec<usize> = PILLARS.iter().map(|p| action_mentions(p)).collect();
        let mut unique = mentions.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            PILLARS.len(),
            "the three pillars share ConceptType::Entity, so they must be separated \
             structurally instead: {mentions:?}"
        );
    }

    #[test]
    fn missing_strings_names_every_absent_requirement() {
        let missing = missing_strings("").expect("empty context is missing all three");
        assert_eq!(missing.len(), 3, "{missing:?}");
        let full = format!(
            "{EXPECT_CANONICAL_LABEL} (score 0.71, blast radius 9)\n{EXPECT_BLAST_WARNING}\n\
             Agent A wrote to it 3 seconds ago"
        );
        assert!(missing_strings(&full).is_none());
    }

    #[test]
    fn the_script_opens_exactly_the_advertised_interactions() {
        assert_eq!(
            ACT_I.len() + ACT_II.len() + ACT_III.len(),
            EXPECT_INTERACTIONS
        );
    }

    /// The nine dependents must be planted by `parent_of` and touched by
    /// nothing else: any `produces` / `modifies` / `depends_on` naming one
    /// gives it a second structural inbound source and silently drops the ⚑
    /// count below nine.
    #[test]
    fn no_action_target_collides_with_a_user_schema_dependent() {
        for step in ACT_I.iter().chain(ACT_II).chain(ACT_III) {
            let Step::Action {
                action,
                produces,
                modifies,
                depends_on,
                ..
            } = step
            else {
                continue;
            };
            for target in produces
                .iter()
                .chain(*modifies)
                .chain(*depends_on)
                .chain(std::iter::once(action))
            {
                assert!(
                    !USER_SCHEMA_DEPENDENTS.contains(target),
                    "'{target}' is a user schema dependent; an action edge would cost it \
                     its place in the ⚑ {EXPECT_BLAST_RADIUS} count"
                );
            }
        }
    }

    /// Every dependent is declared exactly once, with `user schema` as its
    /// only parent.
    #[test]
    fn every_dependent_has_user_schema_as_its_only_parent() {
        let mut seen: Vec<&str> = Vec::new();
        for step in ACT_I.iter().chain(ACT_II).chain(ACT_III) {
            let Step::Derive { parent_of, .. } = step else {
                continue;
            };
            for (parent, child) in parent_of.iter() {
                assert_eq!(
                    *parent, USER_SCHEMA,
                    "'{child}' is parented by '{parent}', not by the pillar"
                );
                assert!(
                    USER_SCHEMA_DEPENDENTS.contains(child),
                    "'{child}' is parented by the pillar but not counted as a dependent"
                );
                assert!(!seen.contains(child), "'{child}' is declared twice");
                seen.push(child);
            }
        }
        assert_eq!(seen.len(), USER_SCHEMA_DEPENDENTS.len());
        assert_eq!(seen.len() as u64, EXPECT_BLAST_RADIUS);
    }

    /// Spec §13 step 2 wants inbound evidence from several distinct
    /// interactions; Stage 2 refuses below three.
    #[test]
    fn user_schema_gets_inbound_structural_edges_from_at_least_three_interactions() {
        let sources = ACT_I
            .iter()
            .chain(ACT_II)
            .chain(ACT_III)
            .filter(|s| match s {
                Step::Action {
                    modifies,
                    depends_on,
                    ..
                } => modifies.contains(&USER_SCHEMA) || depends_on.contains(&USER_SCHEMA),
                Step::Derive { .. } => false,
            })
            .count();
        assert!(
            sources >= 6,
            "spec §13 narrates six distinct interactions; the script has {sources}"
        );
    }

    /// Act III must be the last word on `user schema`, or the conflict line
    /// names agent B instead of agent A.
    #[test]
    fn agent_a_makes_the_newest_write_to_the_pillar() {
        let Some(Step::Action { modifies, .. }) = ACT_III.last() else {
            panic!("act III must end with an action");
        };
        assert!(
            modifies.contains(&USER_SCHEMA),
            "act III's last write must touch the pillar"
        );
    }

    /// Agent B must hold at least one edge to the pillar, or the conflict has
    /// only one contesting agent and never fires.
    #[test]
    fn agent_b_holds_an_edge_to_the_pillar() {
        let touches = ACT_II.iter().any(|s| match s {
            Step::Action {
                depends_on,
                modifies,
                ..
            } => depends_on.contains(&USER_SCHEMA) || modifies.contains(&USER_SCHEMA),
            Step::Derive { .. } => false,
        });
        assert!(touches, "agent B never touches '{USER_SCHEMA}'");
    }

    #[test]
    fn the_three_pillars_are_derived_in_the_first_interaction() {
        let Some(Step::Derive { concepts, .. }) = ACT_I.first() else {
            panic!("act I must open with a derive");
        };
        for pillar in PILLARS {
            assert!(
                concepts.iter().any(|(c, _)| c == pillar),
                "'{pillar}' is not derived in the opening interaction"
            );
        }
    }

    #[test]
    fn configs_compress_intervals_without_weakening_thresholds() {
        let spec = Config::default();
        let build = build_config();
        let canon = canonization_config();

        // Frozen during the build, compressed after.
        assert_eq!(build.canonization_eval_interval, BUILD_EVAL_INTERVAL);
        assert_eq!(canon.canonization_eval_interval, DEMO_EVAL_INTERVAL);
        assert_eq!(
            build.canonization_edge_min_age,
            spec.canonization_edge_min_age
        );
        assert_eq!(canon.canonization_edge_min_age, DEMO_EDGE_MIN_AGE);
        assert_eq!(build.gc_interval, spec.gc_interval);
        assert_eq!(canon.gc_interval, DEMO_GC_INTERVAL);

        // The age floor is compressed, never disabled: an edge written in this
        // cycle still has to age before Stage 2 or Stage 3 will count it.
        assert!(!DEMO_EDGE_MIN_AGE.is_zero());

        // Thresholds are the thing being demonstrated — untouched in both.
        for cfg in [&build, &canon] {
            assert_eq!(
                cfg.canonization_min_peer_count,
                spec.canonization_min_peer_count
            );
            assert_eq!(
                cfg.canonization_eval_batch_size,
                spec.canonization_eval_batch_size
            );
            assert_eq!(
                cfg.canonization_repromotion_cooldown,
                spec.canonization_repromotion_cooldown
            );
            assert_eq!(cfg.max_canonical_nodes, spec.max_canonical_nodes);
            assert_eq!(cfg.conflict_recency_window, spec.conflict_recency_window);
            assert_eq!(cfg.scoring, spec.scoring);
            assert_eq!(cfg.recall_weights, spec.recall_weights);
            // Determinism: the write path must resolve concepts identically on
            // every backend and every run.
            assert_eq!(cfg.match_strategy, MatchStrategy::Canonical);
        }
    }

    #[test]
    fn fresh_session_ids_do_not_repeat() {
        let a = fresh_session_id();
        let b = fresh_session_id();
        assert_ne!(a, b);
        assert!(a.starts_with("demo-rest-api-"), "{a}");
    }

    #[test]
    fn settle_aliases_are_distinct_and_leave_headroom() {
        let mut sorted = SETTLE_ALIASES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), SETTLE_ALIASES.len(), "aliases must be unique");
        assert!(
            SETTLE_ALIASES.len() as i32 > STAGE1_MIN_GC_SURVIVED,
            "one alias funds one sweep; the list must outlast the floor"
        );
        assert!(!SETTLE_ALIASES.contains(&USER_SCHEMA));
    }

    #[test]
    fn outcome_renders_stably() {
        let outcome = DemoOutcome {
            scenario: SCENARIO_REST_API.into(),
            interactions: EXPECT_INTERACTIONS,
            concepts: EXPECT_CONCEPTS,
            edges: 60,
            statuses: vec![(USER_SCHEMA.into(), "Canonical".into())],
            transitions: vec![Transition {
                content: USER_SCHEMA.into(),
                from: "Venerable".into(),
                to: "Canonical".into(),
                blast_radius: Some(9),
            }],
            canonical: vec![(USER_SCHEMA.into(), EXPECT_BLAST_RADIUS)],
            recall_context: format!("{EXPECT_CANONICAL_LABEL}\n{EXPECT_CONFLICT_LINE}"),
            recall_warnings: vec![EXPECT_BLAST_WARNING.into()],
        };
        let once = outcome.render();
        assert_eq!(once, outcome.clone().render());
        assert!(
            once.contains("user schema: Venerable -> Canonical"),
            "{once}"
        );
        assert!(once.contains("blast_radius=9"), "{once}");
    }
}

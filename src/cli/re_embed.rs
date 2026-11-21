//! `lambo re-embed` — the K2 migration verb: rewrite a session's EVERY concept
//! vector into the configured live embedding space and swap the session's
//! [`EmbeddingContract`] in the same atomic flush.
//!
//! Why this exists: Level B refuses to attach a writer whose stored contract
//! disagrees with the configured embedder, because two model spaces in one
//! session is the corruption it guards against. That refusal is correct for
//! every ordinary attach — and exactly wrong for the one operation that ENDS a
//! mixed-space risk instead of creating one. `re-embed` attaches through
//! `crate::memory::MemoryBuilder::reembed_mode`, embeds every concept
//! content outside any lock, then appends the whole rewrite as one ordered
//! batch via [`crate::graph::Graph::reembed_all`]: every `UpsertNode` first,
//! the trailing `SetEmbedding` last (on a session with no contract the
//! `SetEmbedding` comes first, so the store's first-stamp legacy quarantine
//! runs before the new vectors land, #322). The final flush in
//! `super::close_writer` drains that batch transactionally with the lease
//! token (one store transaction per batch), so the durable session goes
//! old-consistent → new-consistent with no observable window in between —
//! and on a crash mid-migration the old contract and old vectors survive
//! together, which is consistent.
//!
//! This is the migration path; it deliberately REFUSES
//! `--allow-embedding-mismatch`, which relabels vectors without rewriting them.
//!
//! # Image concepts (#22, design Q19)
//!
//! An image concept's vector (`embedding_source` set) is not a function of
//! its text, so re-embed never embeds its caption:
//!
//! * a full migration **refuses** while any image concept still carries a
//!   vector, because that vector would stay in the old space; with
//!   `--drop-image-vectors` it nulls them in the same batch (keeping each
//!   source, so the concept stays an image concept whose vector is missing)
//!   and reports the count. Re-deriving the same image under the new
//!   embedder restores the vector.
//! * `--missing-only` skips image concepts and reports how many it skipped.
//!
//! # A session already under the live contract (#296)
//!
//! A plain re-embed refuses a session that already carries the live
//! contract: there is nothing to migrate. Two modes act on that state:
//!
//! * `--missing-only` fills the vectors that are missing. It writes vectors
//!   from the live embedder into the session's own space, so it runs the
//!   embedder canary like any writer attach: under `canary = "verify"` a
//!   drift refuses it before it writes.
//! * `--rewrite-same-contract` rewrites every text concept vector in place
//!   ([`crate::graph::Graph::reembed_in_place`]) and keeps the contract. It
//!   is the repair for a canary drift (the same contract name, different
//!   vectors), which is only ever detected on a session in this state.
//!
//! # The canary pin
//!
//! A full rewrite pins the store's canary for the live contract again once
//! the rewrite is durable. The pin is per store and contract, and the
//! store's other sessions under that contract rest on it, so before writing
//! the rewrite reads what the pin says about the live embedder
//! ([`crate::embed::canary::compare_pin`]). A pin it cannot confirm (the
//! live embedder drifted from it, or could not be checked) is replaced only
//! with `--repin-canary`, and the run then says that the other sessions are
//! no longer flagged.

use std::sync::Arc;

use super::caps::{check_size_cli, require_nonempty, CliError};
use super::{close_writer, map_writer_err};
use crate::embed::Embedder;
use crate::memory::Memory;
use crate::resolve::ResolvedBackends;
use crate::types::{EmbeddingContract, NodeId};

/// Parsed `re-embed` flags.
pub struct Args {
    pub session: String,
    pub agent: String,
    /// Accepted on the wire only so the clap shape mirrors the other writers;
    /// always refused. Re-embed IS the mismatch migration.
    pub allow_embedding_mismatch: bool,
    /// Backfill mode: embed only the concepts that currently have NO vector,
    /// leaving every existing vector and the session contract untouched.
    ///
    /// The default (full migration) refuses to run when the live contract is
    /// already the stored one, which is correct for a migration and useless for
    /// a repair: a session that accumulated NULL vectors inside its own current
    /// space then has no path back to full coverage. That is the state the
    /// dogfood session was found in on 2026-09-01, with 555 of 946 concepts
    /// unembedded and `re-embed` refusing by design.
    pub missing_only: bool,
    /// Full migration only: null the vector of every image concept (#22)
    /// instead of refusing on it. An image vector cannot be recomputed from
    /// its caption; the image concept stays, findable by its caption, until
    /// the image is derived again under the new embedder.
    pub drop_image_vectors: bool,
    /// Full rewrite of a session that already carries the live contract
    /// (#296): re-embed every text concept in place and keep the contract.
    /// The repair for an embedder canary drift. Refused unless the session
    /// carries the live contract and holds a vector; without it, such a
    /// session is refused as before.
    pub rewrite_same_contract: bool,
    /// Full rewrite only (#296): replace the store's canary pin for the live
    /// contract even when the live embedder does not reproduce it (it
    /// drifted, or could not be checked). Without it such a rewrite is
    /// refused before it writes, because the store's other sessions under
    /// that contract rest on the pin.
    pub repin_canary: bool,
}

/// Run the K2 re-embed migration for one session.
pub async fn run(backends: ResolvedBackends, args: Args) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;
    require_nonempty("agent", &args.agent)?;
    check_size_cli("agent", &args.agent)?;
    if args.allow_embedding_mismatch && args.missing_only {
        return Err(CliError::Usage(
            "--missing-only and --allow-embedding-mismatch are contradictory: the backfill \
             writes vectors into the space the session already declares, so there is no \
             mismatch for the override to permit."
                .to_string(),
        ));
    }
    if args.missing_only && args.drop_image_vectors {
        return Err(CliError::Usage(
            "--missing-only and --drop-image-vectors are contradictory: the backfill never \
             touches an existing vector, and it already skips image concepts."
                .to_string(),
        ));
    }
    if args.missing_only && (args.rewrite_same_contract || args.repin_canary) {
        return Err(CliError::Usage(
            "--missing-only fills missing vectors in the session's own space and never pins \
             the canary; --rewrite-same-contract and --repin-canary belong to a full rewrite."
                .to_string(),
        ));
    }
    if args.allow_embedding_mismatch {
        return Err(CliError::Usage(
            "--allow-embedding-mismatch is refused by re-embed: re-embed IS the embedding \
             migration. It attaches past the contract check itself, embeds every concept into \
             the configured live space, and swaps the contract atomically; pairing it with the \
             relabel override (which leaves old-space vectors mislabelled) would be \
             contradictory. Drop the flag."
                .to_string(),
        ));
    }

    // The builder is constructed directly because `open_writer` does not expose
    // `reembed_mode`. Backends pass through UNCHANGED:
    // `allow_embedding_mismatch` stays false (the override field is simply
    // unused here), and `reembed_mode(true)` bypasses the contract check by its
    // own mechanism, never by flipping the override.
    let config = backends.config.clone();
    let canary_mode = backends.embedder_cfg.canary;
    if args.repin_canary && canary_mode == crate::embed::canary::CanaryMode::Off {
        return Err(CliError::Usage(
            "--repin-canary has nothing to replace: with [embedder] canary = \"off\" a \
             re-embed neither checks nor pins the canary, and leaves any pin the store holds \
             as it is. Drop the flag, or turn the canary on."
                .to_string(),
        ));
    }
    // #296: the backfill runs the configured canary like any writer attach;
    // a full rewrite attaches with it off and checks the pin itself below.
    let canary = if args.missing_only {
        backends.canary_options()
    } else {
        crate::embed::canary::CanaryOptions::off()
    };
    let store: std::sync::Arc<dyn crate::store::GraphStore> = Arc::from(backends.store);
    let embedder: std::sync::Arc<dyn Embedder> = Arc::from(backends.embedder);
    let live_contract: EmbeddingContract = backends.embedding.clone();

    let mem = Memory::builder()
        .session(&args.session)
        .agent(&args.agent)
        .config(config)
        .store(store.clone())
        .embedder(embedder.clone())
        .embedding_contract(live_contract.clone())
        .canary(canary)
        .reembed_mode(true)
        .build()
        .await
        .map_err(map_writer_err)?;

    // Snapshot concept ids + contents under the graph READ lock, sorted by id
    // for deterministic embed order, then DROP the lock: the embed calls below
    // are real model inference and must never run under any lock.
    //
    // #22: only TEXT concepts (no `embedding_source`) are embedded from
    // their content. Image concepts are counted, never embedded: one that
    // still carries a vector blocks a full migration unless
    // --drop-image-vectors, and one whose vector is missing is skipped by
    // both modes (re-deriving the image restores it).
    let (concepts, images, total, prior_contract, has_vectors) = {
        let g = mem.graph().read();
        let prior_contract = g.embedding().cloned();
        let has_vectors = g.concepts().any(|c| c.embedding.is_some());
        let mut images = ImageCounts::default();
        let mut snapshot: Vec<(NodeId, String)> = Vec::new();
        for c in g.concepts() {
            match (&c.embedding_source, &c.embedding) {
                (Some(_), Some(_)) => images.with_vector += 1,
                (Some(_), None) => images.without_vector += 1,
                (None, embedding) => {
                    if !args.missing_only || embedding.is_none() {
                        snapshot.push((c.id, c.content.clone()));
                    }
                }
            }
        }
        snapshot.sort_by_key(|(id, _)| id.0);
        (
            snapshot,
            images,
            g.concepts().count(),
            prior_contract,
            has_vectors,
        )
    };

    // Every path below — success or any mid-run abort — funnels into
    // close_writer at the end of this function: a failed migration
    // must release the writer lease immediately, not hold it to TTL.
    //
    // #296: whether the re-pin after a full rewrite replaces a pin the live
    // embedder does not reproduce (allowed by --repin-canary).
    let mut replaces_unconfirmed_pin = false;
    let out = if !args.missing_only && images.with_vector > 0 && !args.drop_image_vectors {
        let n = images.with_vector;
        Err(CliError::Runtime(format!(
            "session '{}': {n} image concept{} still carr{} an image vector, which re-embed \
             cannot recompute from a caption: rewriting the session's space would leave \
             {} in the old one. Refused; nothing was written. Rerun with \
             --drop-image-vectors to null them (the image concepts stay, findable by \
             caption, and deriving the same image again under the new embedder restores \
             each vector), or keep the current embedder.",
            args.session,
            if n == 1 { "" } else { "s" },
            if n == 1 { "ies" } else { "y" },
            if n == 1 { "it" } else { "them" },
        )))
    } else if total == 0 {
        Ok(format!(
            "session '{}' has no concepts; nothing to re-embed",
            args.session
        ))
    } else if args.missing_only && prior_contract.is_none() {
        // #322: a backfill fills vectors into the contract the session
        // already has, and never stamps one (`Graph::embed_missing`). Refuse
        // before embedding anything, and name the full re-embed.
        Err(CliError::Runtime(format!(
            "session '{}' has no recorded contract (for example, one written before \
             embedding contracts), and its stored vectors are ignored at load. --missing-only fills \
             vectors into the contract a session already has. Refused; nothing was written. \
             Run `lambo re-embed` without --missing-only to embed every text concept and \
             record the live contract.",
            args.session
        )))
    } else if args.missing_only && concepts.is_empty() {
        Ok(format!(
            "session '{}': every text concept already carries a vector; nothing to \
             backfill{}",
            args.session,
            images.skipped_note()
        ))
    } else if args.missing_only {
        backfill_missing(
            &mem,
            &args.session,
            &args.agent,
            &embedder,
            &live_contract,
            &concepts,
        )
        .await
        .map(|out| out + &images.skipped_note())
    } else if let Some(refusal) =
        contract_refusal(&args, prior_contract.as_ref(), &live_contract, has_vectors)
    {
        Err(refusal)
    } else {
        match repin_guard(
            store.as_ref(),
            embedder.as_ref(),
            &live_contract,
            canary_mode,
            args.repin_canary,
            &args.session,
        )
        .await
        {
            Err(refusal) => Err(refusal),
            Ok(unconfirmed) => {
                replaces_unconfirmed_pin = unconfirmed;
                rewrite_all(
                    &mem,
                    &args.session,
                    &args.agent,
                    &embedder,
                    &live_contract,
                    &concepts,
                    args.drop_image_vectors,
                    args.rewrite_same_contract,
                )
                .await
                .map(|out| out + &images.skipped_note())
            }
        }
    };
    // #176: a full rewrite moved this session into the live space (or
    // rewrote it in place), so the store's canary is pinned again from the
    // live embedder — after the rewrite is durable, never before. A backfill
    // changes no space.
    let full_rewrite = out.is_ok() && !args.missing_only && total > 0;
    let out = close_writer(mem, out).await?;
    if !full_rewrite || canary_mode == crate::embed::canary::CanaryMode::Off {
        return Ok(out);
    }
    Ok(
        match crate::embed::canary::pin(store.as_ref(), embedder.as_ref(), &live_contract).await {
            // The canary is store-wide per contract. A re-pin over a pin the
            // live embedder did not reproduce (--repin-canary; the drift the
            // canary exists for) leaves the store's other sessions under that
            // contract in the pinned embedder's space, and after this pin
            // nothing flags them any more: say so. #296: this used to key on
            // `prior == live`, which a successful rewrite never met.
            Ok(()) if replaces_unconfirmed_pin => format!(
                "{out}\nembedder canary pinned again for the live contract, replacing a pin the \
                 live embedder did not reproduce (--repin-canary). Any other session in this \
                 store under the same contract still holds vectors from the embedder that pin \
                 recorded, and the canary no longer flags it: re-embed each of them with \
                 --rewrite-same-contract"
            ),
            Ok(()) => format!("{out}\nembedder canary pinned again for the live contract"),
            // #296: the rewrite is durable but the store still holds the
            // old pin. If that pin was another build's, every writer attach
            // under canary = "verify" is refused until it is replaced, so
            // name the exact recovery.
            Err(e) => format!(
                "{out}\nwarning: the embedder canary could not be pinned again ({e}). The \
                 rewrite is durable, but the store still holds the old pin for this contract, \
                 and the next writer attach compares against it: if it was taken from another \
                 build, every writer attach under canary = \"verify\" is refused. Once the \
                 embedder answers, rerun `lambo re-embed --rewrite-same-contract \
                 --repin-canary --session {} --agent {}` to rewrite this session again and pin \
                 the canary.",
                args.session, args.agent,
            ),
        },
    )
}

/// #296: the full rewrite's contract precondition, checked before anything
/// is embedded or written. `--rewrite-same-contract` needs a session already
/// under the live contract that holds a vector; without it, a session
/// already under the live contract is refused, as `Graph::reembed_all`
/// refuses it, but here, naming the two modes that act on that state.
fn contract_refusal(
    args: &Args,
    prior: Option<&EmbeddingContract>,
    live: &EmbeddingContract,
    has_vectors: bool,
) -> Option<CliError> {
    let describe = |c: &EmbeddingContract| {
        format!(
            "kind={} model={} dim={}",
            c.kind,
            c.model.as_deref().unwrap_or("<unset>"),
            c.dim
        )
    };
    let session = &args.session;
    let under_live = prior == Some(live);
    let message = if args.rewrite_same_contract && !under_live {
        format!(
            "session '{session}': --rewrite-same-contract rewrites a session that already \
             carries the live contract ({}), and this one carries {}: re-embedding it into the \
             live contract is a migration. Refused; nothing was written. Drop the flag.",
            describe(live),
            prior.map_or_else(|| "no recorded contract".to_string(), describe),
        )
    } else if args.rewrite_same_contract && !has_vectors {
        format!(
            "session '{session}' carries the live contract but holds no vector, so there is \
             nothing to rewrite in place. Refused; nothing was written. Fill its missing \
             vectors with --missing-only."
        )
    } else if !args.rewrite_same_contract && under_live {
        format!(
            "session '{session}' already carries the live contract ({}), so there is nothing to \
             migrate. Refused; nothing was written. To fill missing vectors, run with \
             --missing-only; to rewrite every vector in place (the repair for an embedder \
             canary drift), run with --rewrite-same-contract.",
            describe(live),
        )
    } else {
        return None;
    };
    Some(CliError::Runtime(message))
}

/// #296: before a full rewrite writes anything, read what the store's canary
/// pin for the live contract says about the live embedder. The rewrite pins
/// the canary again afterwards, and the store's other sessions under that
/// contract rest on the pin, so a pin the live embedder does not reproduce
/// (it drifted, or could not be checked) is replaced only with
/// `--repin-canary`. Returns whether the re-pin will replace such a pin.
async fn repin_guard(
    store: &dyn crate::store::GraphStore,
    embedder: &dyn Embedder,
    live: &EmbeddingContract,
    mode: crate::embed::canary::CanaryMode,
    repin: bool,
    session: &str,
) -> Result<bool, CliError> {
    use crate::embed::canary::{self, CanaryMode, CanaryVerdict};
    if mode == CanaryMode::Off {
        return Ok(false);
    }
    let contract = format!(
        "kind={} model={} dim={}",
        live.kind,
        live.model.as_deref().unwrap_or("<unset>"),
        live.dim
    );
    let why = match canary::compare_pin(store, embedder, live)
        .await
        .map_err(CliError::from)?
    {
        Some(CanaryVerdict::Drifted { min_cosine, worst }) => format!(
            "the live embedder no longer reproduces that pin (canary {worst} has cosine \
             {min_cosine:.5}, below {})",
            canary::CANARY_MIN_COSINE
        ),
        // The check could not run (the embedder errored, timed out, or
        // answered the wrong width or too few vectors). That is usually
        // transient, so the advice is to retry; the opt-in is only for an
        // operator who knows the pin is another build's.
        Some(CanaryVerdict::Unverified(reason)) => {
            if repin {
                return Ok(true);
            }
            return Err(CliError::Runtime(format!(
                "session '{session}': the store holds an embedder canary pin for {contract}, \
                 which this re-embed pins again after its rewrite, but the check of the live \
                 embedder against that pin could not run ({reason}). Refused; nothing was \
                 written. Retry once the embedder answers. Pass --repin-canary only to replace \
                 that pin deliberately, knowing it was taken from another build of the model: \
                 the store's other sessions under that contract keep vectors from that build, \
                 and after the re-pin the canary would no longer flag them."
            )));
        }
        // No usable pin, or one the live embedder reproduces: the re-pin
        // takes nothing from any other session. `compare_pin` returns no
        // other verdict.
        None
        | Some(
            CanaryVerdict::Verified { .. }
            | CanaryVerdict::Off
            | CanaryVerdict::Pinned
            | CanaryVerdict::RecentlyVerified,
        ) => return Ok(false),
    };
    if repin {
        return Ok(true);
    }
    Err(CliError::Runtime(format!(
        "session '{session}': this re-embed would pin the embedder canary again for \
         {contract}, replacing the store's pin for that contract, and {why}. Any other session \
         in this store under that contract holds vectors from the pinned embedder, and after \
         the re-pin the canary would no longer flag it. Refused; nothing was written. Rerun \
         with --repin-canary to replace the pin anyway (then re-embed each of those sessions \
         with --rewrite-same-contract), or restore the embedder the pin was taken from."
    )))
}

/// The image concepts (#22) a re-embed found, which it never embeds.
#[derive(Default)]
struct ImageCounts {
    /// Image concepts that carry a vector.
    with_vector: usize,
    /// Image concepts whose vector is missing (quarantined or dropped).
    without_vector: usize,
}

impl ImageCounts {
    /// The report line for the image concepts left without a vector, or
    /// nothing when there are none.
    fn skipped_note(&self) -> String {
        match self.without_vector {
            0 => String::new(),
            n => format!(
                "\nskipped {n} image concept(s) whose vector is missing: an image vector is \
                 never recomputed from a caption; derive the image again to restore it"
            ),
        }
    }
}

/// The backfill body: embed only the concepts handed in (already filtered to
/// those with no vector) and append them through [`Graph::embed_missing`](crate::graph::Graph::embed_missing),
/// which leaves the session contract exactly as it found it.
///
/// Same all-or-nothing discipline as [`rewrite_all`]: every embed happens
/// before any mutation, so an embedder failure part-way leaves the graph
/// untouched rather than half-backfilled, and `close_writer` still releases
/// the lease on that path.
async fn backfill_missing(
    mem: &Memory,
    session: &str,
    agent: &str,
    embedder: &Arc<dyn Embedder>,
    live_contract: &EmbeddingContract,
    missing: &[(NodeId, String)],
) -> Result<String, CliError> {
    let mut updates: Vec<(NodeId, Vec<f32>)> = Vec::with_capacity(missing.len());
    for (id, content) in missing {
        let vector = embedder.embed(content).await.map_err(|e| {
            CliError::Runtime(format!(
                "backfill aborted BEFORE mutating the graph: embedding concept {id} \
                 ({content:?}) failed: {e}"
            ))
        })?;
        updates.push((*id, vector));
    }

    debug_assert_eq!(mem.session().as_str(), session);
    let (filled, total, after) = {
        let mut g = mem.graph().write();
        let total = g.concepts().count();
        let filled = g
            .embed_missing(updates, live_contract)
            .map_err(CliError::from)?;
        let after = g.concepts().filter(|c| c.embedding.is_some()).count();
        (filled, total, after)
    };

    Ok(format!(
        "backfilled session '{session}' as agent '{agent}': {filled} concept(s) that had no \
         vector embedded into the session's existing space\n\
         coverage: embedded {}/{total} -> {after}/{total}\n\
         contract unchanged: kind={} model={} dim={}\n\
         existing vectors were not touched; this is a repair, not a migration",
        after - filled,
        live_contract.kind,
        live_contract.model.as_deref().unwrap_or("<unset>"),
        live_contract.dim,
    ))
}

/// The all-or-nothing rewrite body: embed EVERY concept BEFORE mutating the
/// graph. Any embed failure aborts here — no mutation appended, no partial
/// rewrite, no mixed-space state — and the caller's `close_writer` still
/// releases the lease on this error path. `in_place`
/// (`--rewrite-same-contract`, #296) keeps the contract
/// ([`crate::graph::Graph::reembed_in_place`]).
#[allow(clippy::too_many_arguments)]
async fn rewrite_all(
    mem: &Memory,
    session: &str,
    agent: &str,
    embedder: &Arc<dyn Embedder>,
    live_contract: &EmbeddingContract,
    concepts: &[(NodeId, String)],
    drop_image_vectors: bool,
    in_place: bool,
) -> Result<String, CliError> {
    let mut updates: Vec<(NodeId, Vec<f32>)> = Vec::with_capacity(concepts.len());
    for (id, content) in concepts {
        let vector = embedder.embed(content).await.map_err(|e| {
            CliError::Runtime(format!(
                "re-embed aborted BEFORE mutating the graph: embedding concept {id} \
                 ({content:?}) failed: {e}"
            ))
        })?;
        updates.push((*id, vector));
    }

    // Under the graph WRITE lock: capture before-coverage, append the whole
    // ordered batch, capture after-coverage. Deliberately NO manual drain:
    // close_writer's Memory::close final flush drains this exact log —
    // UpsertNodes then SetEmbedding, or on a session with no contract
    // SetEmbedding then UpsertNodes (#322) — and flushes it transactionally with
    // the lease token before releasing the lease. The write lock makes the
    // append sequence atomic against the flush task's drain_log, so no
    // partial batch can ever be flushed between the two coverages.
    let rewritten = concepts.len();
    debug_assert_eq!(mem.session().as_str(), session);
    let (before, after, total, dropped) = {
        let mut g = mem.graph().write();
        let total = g.concepts().count();
        let before = g.concepts().filter(|c| c.embedding.is_some()).count();
        let dropped = match (in_place, drop_image_vectors) {
            (false, true) => g
                .reembed_all_dropping_image_vectors(updates, live_contract.clone())
                .map_err(CliError::from)?,
            (false, false) => {
                g.reembed_all(updates, live_contract.clone())
                    .map_err(CliError::from)?;
                0
            }
            (true, true) => g
                .reembed_in_place_dropping_image_vectors(updates, live_contract)
                .map_err(CliError::from)?,
            (true, false) => {
                g.reembed_in_place(updates, live_contract)
                    .map_err(CliError::from)?;
                0
            }
        };
        let after = g.concepts().filter(|c| c.embedding.is_some()).count();
        (before, after, total, dropped)
    };
    let dropped_line = if drop_image_vectors {
        format!(
            "\ndropped {dropped} image vector(s) (--drop-image-vectors): those image concepts \
             keep their caption and source; derive each image again to restore its vector"
        )
    } else {
        String::new()
    };

    let (contract_line, flush_line) = if in_place {
        (
            "contract unchanged (rewritten in place)",
            "the vector rewrite flushes as ONE transaction with this writer's lease release",
        )
    } else {
        (
            "contract now",
            "the vector rewrite and contract swap flush as ONE transaction with this \
             writer's lease release",
        )
    };
    Ok(format!(
        "re-embedded session '{session}' as agent '{agent}': {rewritten} concept(s) rewritten \
         into the live space\n\
         coverage: embedded {before}/{total} -> {after}/{total}\n\
         {contract_line}: kind={} model={} dim={}\n\
         {flush_line}{dropped_line}",
        live_contract.kind,
        live_contract.model.as_deref().unwrap_or("<unset>"),
        live_contract.dim,
    ))
}

/// Test seeding shared by the store tests below: no store or embedder
/// feature, so the Postgres live test (`--no-default-features --features
/// store-postgres`) uses it too.
#[cfg(test)]
mod seed {
    use std::time::Duration;

    use crate::store::{
        lease::{LeaseHolder, LeaseOutcome},
        GraphStore,
    };
    use crate::types::{
        AgentId, CanonizationStatus, Concept, ConceptType, EmbeddingContract, Interaction,
        Mutation, MutationBatch, NodeId, SessionId,
    };

    /// Plant the pre-K2 damage shape directly in the durable store under
    /// `contract`: one interaction, two concepts, "user schema" WITH a
    /// `dim`-wide vector and "auth middleware" with a NULL embedding (the
    /// row an interrupted or degraded writer leaves behind). With no
    /// contract it is a pre-contract (legacy) session (#322): the vectored
    /// concept's row keeps a vector no contract describes. `source` and
    /// `unvectored` are the two concepts' `embedding_source` (#22).
    pub(super) async fn seed_session<S: GraphStore + ?Sized>(
        store: &S,
        session: &str,
        dim: usize,
        contract: Option<EmbeddingContract>,
        source: Option<crate::types::EmbeddingSource>,
        unvectored: Option<crate::types::EmbeddingSource>,
    ) {
        let sid = SessionId::from(session);
        let seeder = LeaseHolder::for_this_process(&AgentId::from("seeder"));
        let LeaseOutcome::Acquired(info) = store
            .acquire_lease(&sid, &seeder, Duration::from_secs(30))
            .await
            .unwrap()
        else {
            panic!("test setup must acquire the seed lease");
        };
        let ts = chrono::Utc::now();
        let i1 = NodeId::new();
        let concept =
            |id: NodeId,
             content: &str,
             emb: Option<Vec<f32>>,
             embedding_source: Option<crate::types::EmbeddingSource>| {
                Mutation::UpsertNode {
                    node: crate::types::Node::Concept(Concept {
                        id,
                        session_id: sid.clone(),
                        content: content.into(),
                        canonical_key: content.to_lowercase(),
                        concept_type: ConceptType::Entity,
                        origin_interaction: i1,
                        origin_agent: AgentId::from("seeder"),
                        created_at: ts,
                        access_count: 0,
                        last_accessed: None,
                        gc_survived: 0,
                        canonization_status: CanonizationStatus::None,
                        blast_radius: None,
                        last_demotion_time: None,
                        embedding: emb,
                        human_confirmed: 0,
                        embedding_source,
                        chunk_group_id: None,
                    }),
                }
            };
        let c1 = NodeId::new();
        let c2 = NodeId::new();
        let derives_edge = |target: NodeId| Mutation::UpsertEdge {
            edge: crate::types::Edge {
                event_time: None,
                id: NodeId::new(),
                session_id: sid.clone(),
                source: i1,
                target,
                edge_type: crate::types::EdgeType::Derives,
                weight: 1.0,
                reinforcements: 1,
                created_at: ts,
                last_reinforced: ts,
            },
        };
        // Contract FIRST: the store treats a contract stamped over already
        // stored vectors as a legacy upgrade and strips them (untrusted
        // provenance) — which would manufacture damage instead of modeling it.
        let set_contract = contract.map(|embedding| Mutation::SetEmbedding {
            session_id: sid.clone(),
            embedding: Some(embedding),
        });
        let interaction = Mutation::UpsertNode {
            node: crate::types::Node::Interaction(Interaction {
                event_time: None,
                id: i1,
                session_id: sid.clone(),
                agent_id: AgentId::from("seeder"),
                prompt_text: Some("seed".into()),
                previous_id: None,
                created_at: ts,
            }),
        };
        let batch = MutationBatch {
            mutation_epoch: 0,
            gc_mark: Default::default(),
            mutations: [interaction]
                .into_iter()
                .chain(set_contract)
                .chain([
                    concept(c1, "user schema", Some(vec![0.25_f32; dim]), source),
                    derives_edge(c1),
                    concept(c2, "auth middleware", None, unvectored),
                    derives_edge(c2),
                ])
                .collect(),
        };
        store.flush(&batch, Some(info.token)).await.unwrap();
        store.release_lease(&sid, &seeder).await.unwrap();
    }

    /// The raw durable session, read straight from the store (no load
    /// quarantine): its contract, and each concept's content and vector,
    /// sorted by content.
    pub(super) async fn raw_session<S: GraphStore + ?Sized>(
        store: &S,
        session: &str,
    ) -> (Option<EmbeddingContract>, Vec<(String, Option<Vec<f32>>)>) {
        let snap = store.load_session(&SessionId::from(session)).await.unwrap();
        let mut concepts: Vec<_> = snap
            .concepts
            .iter()
            .map(|c| (c.content.clone(), c.embedding.clone()))
            .collect();
        concepts.sort_by(|a, b| a.0.cmp(&b.0));
        (snap.embedding, concepts)
    }

    /// Whether a new writer can take `session`'s lease now. The probe
    /// releases what it took, so a test can go on writing.
    pub(super) async fn lease_is_free<S: GraphStore + ?Sized>(store: &S, session: &str) -> bool {
        let probe = LeaseHolder::for_this_process(&AgentId::from("probe"));
        let sid = SessionId::from(session);
        let free = matches!(
            store
                .acquire_lease(&sid, &probe, Duration::from_secs(5))
                .await
                .unwrap(),
            LeaseOutcome::Acquired(_)
        );
        if free {
            store.release_lease(&sid, &probe).await.unwrap();
        }
        free
    }
}

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod tests {
    use super::*;
    use crate::store::{
        lease::{LeaseHolder, LeaseOutcome},
        GraphStore,
    };
    use crate::types::{AgentId, GraphSnapshot, MutationBatch, NodeId, SessionId, StoreError};
    use crate::{embed::FixtureEmbedder, MemoryStore};
    use crate::{
        embed::{EmbedderConfig, EmbedderKind},
        store::{StoreConfig, StoreKind},
    };
    use std::time::Duration;

    const SESSION: &str = "k2-reembed";
    const AGENT: &str = "operator";

    /// `Arc<S>` as a `GraphStore` so the seeded store is shared by identity
    /// between the seed phase and the verb under test (the same
    /// two-processes-one-file shape the sibling CLI tests model).
    struct ArcStore<S>(Arc<S>);

    #[async_trait::async_trait]
    impl<S: GraphStore + 'static> GraphStore for ArcStore<S> {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.0.init_schema().await
        }
        fn capabilities(&self) -> crate::store::Capabilities {
            self.0.capabilities()
        }
        fn vector_dimensions(&self) -> Option<usize> {
            self.0.vector_dimensions()
        }
        async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
            self.0.flush(batch, token).await
        }
        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.0.load_session(session).await
        }
        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<crate::types::Scored<NodeId>>, StoreError> {
            self.0.keyword_candidates(session, tokens, limit).await
        }
        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: chrono::DateTime<chrono::Utc>,
        ) -> Result<u64, StoreError> {
            self.0.blast_radius(session, node, min_edge_age, now).await
        }
        async fn interaction_span(
            &self,
            session: &SessionId,
            node: NodeId,
            min_age: Duration,
            now: chrono::DateTime<chrono::Utc>,
        ) -> Result<crate::types::InteractionSpan, StoreError> {
            self.0.interaction_span(session, node, min_age, now).await
        }
        async fn interaction_span_with_window(
            &self,
            session: &SessionId,
            node: NodeId,
            min_age: Duration,
            now: chrono::DateTime<chrono::Utc>,
            window: Duration,
        ) -> Result<crate::types::InteractionSpan, StoreError> {
            self.0
                .interaction_span_with_window(session, node, min_age, now, window)
                .await
        }
        async fn acquire_lease(
            &self,
            session: &SessionId,
            holder: &LeaseHolder,
            ttl: Duration,
        ) -> Result<LeaseOutcome, StoreError> {
            self.0.acquire_lease(session, holder, ttl).await
        }
        async fn refresh_lease(
            &self,
            session: &SessionId,
            holder: &LeaseHolder,
            ttl: Duration,
        ) -> Result<LeaseOutcome, StoreError> {
            self.0.refresh_lease(session, holder, ttl).await
        }
        async fn release_lease(
            &self,
            session: &SessionId,
            holder: &LeaseHolder,
        ) -> Result<(), StoreError> {
            self.0.release_lease(session, holder).await
        }
        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<crate::types::Scored<NodeId>>, StoreError> {
            self.0.vector_candidates(session, embedding, limit).await
        }
        async fn record_canonization(
            &self,
            event: &crate::types::CanonizationEvent,
            token: Option<u64>,
        ) -> Result<(), StoreError> {
            self.0.record_canonization(event, token).await
        }
        // #176/#296: the canary pin lives in `store_meta`; without these the
        // double would read every pin as absent.
        async fn read_store_meta(&self, key: &str) -> Result<Option<String>, StoreError> {
            self.0.read_store_meta(key).await
        }
        async fn write_store_meta(&self, key: &str, value: &str) -> Result<(), StoreError> {
            self.0.write_store_meta(key, value).await
        }
        async fn write_store_meta_if_absent(
            &self,
            key: &str,
            value: &str,
        ) -> Result<bool, StoreError> {
            self.0.write_store_meta_if_absent(key, value).await
        }
    }

    fn backends_on<S: GraphStore + 'static>(
        store: Arc<S>,
        kind: &str,
        model: &str,
    ) -> ResolvedBackends {
        ResolvedBackends {
            store: Box::new(ArcStore(store)),
            embedder: Box::new(FixtureEmbedder::new()),
            store_cfg: StoreConfig {
                kind: StoreKind::Memory,
                dsn: None,
                path: None,
                vector_dim: None,
            },
            embedder_cfg: EmbedderConfig {
                kind: EmbedderKind::Fixture,
                dim: 1024,
                llama_url: None,
                llama_model: None,
                ..Default::default()
            },
            embedding: EmbeddingContract {
                kind: kind.into(),
                model: Some(model.into()),
                dim: 1024,
            },
            allow_embedding_mismatch: false,
            config: crate::Config::default(),
        }
    }

    /// Plant the pre-K2 damage shape directly in the durable store: one
    /// interaction, two concepts under a LEGACY contract — one WITH a legacy
    /// vector, one with a NULL embedding (the row an interrupted or degraded
    /// writer leaves behind).
    async fn seed_damaged_session(store: &Arc<MemoryStore>) {
        seed_session_with_source(store, None).await;
    }

    /// [`seed_damaged_session`], with `source` as the vectored concept's
    /// `embedding_source` (#22).
    async fn seed_session_with_source(
        store: &Arc<MemoryStore>,
        source: Option<crate::types::EmbeddingSource>,
    ) {
        seed_session_with_sources(store, source, None).await;
    }

    /// [`seed_damaged_session`], with `vectored` and `unvectored` as the
    /// `embedding_source` of its vectored and its NULL-vector concept (#22:
    /// a sourced NULL-vector concept is an image whose vector is missing).
    async fn seed_session_with_sources(
        store: &Arc<MemoryStore>,
        source: Option<crate::types::EmbeddingSource>,
        unvectored: Option<crate::types::EmbeddingSource>,
    ) {
        seed_session_under(
            store.as_ref(),
            Some(EmbeddingContract {
                kind: "legacy".into(),
                model: Some("v1".into()),
                dim: 1024,
            }),
            source,
            unvectored,
        )
        .await;
    }

    /// [`seed_damaged_session`] under `contract`, into any store. With no
    /// contract it is a pre-contract (legacy) session (#322): the vectored
    /// concept's row keeps a vector no contract describes.
    async fn seed_session_under<S: GraphStore>(
        store: &S,
        contract: Option<EmbeddingContract>,
        source: Option<crate::types::EmbeddingSource>,
        unvectored: Option<crate::types::EmbeddingSource>,
    ) {
        super::seed::seed_session(store, SESSION, 1024, contract, source, unvectored).await;
    }

    /// Durable reader-side assertions: stamped contract kind plus per-concept
    /// vector widths (None == NULL row).
    async fn durable_state(store: &Arc<MemoryStore>) -> (Option<String>, Vec<Option<usize>>) {
        let loaded = crate::cli::load_reader_graph(store.as_ref(), SESSION)
            .await
            .unwrap();
        let g = loaded.graph.read();
        let mut widths = g
            .concepts()
            .map(|c| c.embedding.as_ref().map(|v| v.len()))
            .collect::<Vec<_>>();
        widths.sort();
        (g.embedding().map(|c| c.kind.clone()), widths)
    }

    #[tokio::test]
    async fn re_embed_fills_null_rows_and_stamps_the_new_contract() {
        let store = Arc::new(MemoryStore::new());
        seed_damaged_session(&store).await;
        // Pre-state sanity: the durable session is genuinely damaged.
        let (kind, widths) = durable_state(&store).await;
        assert_eq!(kind.as_deref(), Some("legacy"));
        assert_eq!(widths, vec![None, Some(1024)]);

        let out = run(
            backends_on(store.clone(), "fixture", "fixture-model"),
            Args {
                session: SESSION.into(),
                agent: AGENT.into(),
                allow_embedding_mismatch: false,
                missing_only: false,
                drop_image_vectors: false,
                rewrite_same_contract: false,
                repin_canary: false,
            },
        )
        .await
        .expect("re-embed");

        // The NULL row is filled and the live contract is stamped.
        assert!(out.contains("coverage: embedded 1/2 -> 2/2"), "{out}");
        let (kind, widths) = durable_state(&store).await;
        assert_eq!(kind.as_deref(), Some("fixture"), "contract must migrate");
        assert_eq!(widths, vec![Some(1024), Some(1024)]);
    }

    /// [`super::seed::raw_session`] of this module's session.
    async fn raw_session<S: GraphStore>(
        store: &S,
    ) -> (Option<EmbeddingContract>, Vec<(String, Option<Vec<f32>>)>) {
        super::seed::raw_session(store, SESSION).await
    }

    /// #322: `lambo re-embed` on a pre-contract session (no durable
    /// contract; one legacy vector, one NULL row) must leave the new vectors
    /// in the store, under the live contract. Before the fix the batch wrote
    /// the vectors, then stamped the contract, and the store's first-stamp
    /// legacy quarantine nulled every vector in the same transaction, while
    /// the report said 2/2.
    ///
    /// Mirrors `reembedCmd322_contractless_durable` (every text concept
    /// holds a live vector) and `reembedOrdered_contractless_matches_graph`
    /// (the store is the graph, so the reported coverage is the store's) in
    /// `formal/LamboProofs/Contract.lean`; the old behaviour is
    /// `reembed_contractless_durable_loses_vectors`.
    async fn assert_contractless_re_embed_keeps_its_vectors<S: GraphStore + 'static>(
        store: Arc<S>,
    ) {
        let (contract, rows) = raw_session(store.as_ref()).await;
        assert_eq!(contract, None, "the seed is a pre-contract session");
        assert_eq!(
            rows.iter()
                .map(|(c, v)| (c.as_str(), v.as_ref().map(Vec::len)))
                .collect::<Vec<_>>(),
            vec![("auth middleware", None), ("user schema", Some(1024))],
            "the legacy row keeps its vector until a contract is stamped"
        );

        let out = run(
            backends_on(store.clone(), "fixture", "fixture-model"),
            args(false, false),
        )
        .await
        .expect("re-embed");
        // The graph loads through the legacy quarantine, so the legacy
        // vector is not counted before; both concepts are after.
        assert!(out.contains("coverage: embedded 0/2 -> 2/2"), "{out}");
        assert!(out.contains("contract now: kind=fixture"), "{out}");

        let (contract, rows) = raw_session(store.as_ref()).await;
        assert_eq!(
            contract.map(|c| c.kind).as_deref(),
            Some("fixture"),
            "the live contract is stamped"
        );
        let widths: Vec<_> = rows
            .iter()
            .map(|(c, v)| (c.as_str(), v.as_ref().map(Vec::len)))
            .collect();
        let fixture = FixtureEmbedder::new();
        for (content, vector) in &rows {
            assert!(
                vector.as_ref() == Some(&fixture.embed(content).await.unwrap()),
                "the store holds the new vector of {content:?}; stored widths: {widths:?}"
            );
        }
        assert_eq!(rows.len(), 2);
        assert!(
            lease_is_free_on(store.as_ref()).await,
            "the lease was released"
        );
    }

    #[tokio::test]
    async fn re_embed_of_a_contractless_session_keeps_its_new_vectors() {
        let store = Arc::new(MemoryStore::new());
        seed_session_under(store.as_ref(), None, None, None).await;
        assert_contractless_re_embed_keeps_its_vectors(store).await;
    }

    /// [`re_embed_of_a_contractless_session_keeps_its_new_vectors`] on
    /// SQLite, whose `set_embedding` quarantine is its own SQL. Multi-thread
    /// flavor: the sync session load joins a worker runtime, and SQLite's
    /// single pooled connection must be returnable meanwhile (the
    /// cross-runtime caveat in `store::sqlite`).
    #[cfg(feature = "store-sqlite")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn re_embed_of_a_contractless_sqlite_session_keeps_its_new_vectors() {
        let store = Arc::new(crate::store::SqliteStore::connect("sqlite::memory:").unwrap());
        store.init_schema().await.unwrap();
        seed_session_under(store.as_ref(), None, None, None).await;
        assert_contractless_re_embed_keeps_its_vectors(store).await;
    }

    /// #322: `--missing-only` fills vectors into the space a session already
    /// declares, so on a pre-contract session it is refused before it embeds
    /// or writes anything, and names the full re-embed instead
    /// (`missingOnly_contractless_writes_nothing`).
    #[tokio::test]
    async fn re_embed_missing_only_refuses_a_contractless_session() {
        let store = Arc::new(MemoryStore::new());
        seed_session_under(store.as_ref(), None, None, None).await;
        let before = raw_session(store.as_ref()).await;
        let err = run(
            backends_on(store.clone(), "fixture", "fixture-model"),
            args(true, false),
        )
        .await
        .expect_err("a backfill never stamps a contract");
        assert!(matches!(err, CliError::Runtime(_)), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("no recorded contract"), "{msg}");
        assert!(msg.contains("without --missing-only"), "{msg}");
        assert!(msg.contains("nothing was written"), "{msg}");
        assert_eq!(raw_session(store.as_ref()).await, before, "no writes");
        assert!(
            lease_is_free(&store).await,
            "the refusal released the lease"
        );
    }

    fn image_source() -> crate::types::EmbeddingSource {
        crate::types::EmbeddingSource {
            modality: crate::types::SourceModality::Image,
            origin: crate::types::VectorOrigin::Client,
            sha256: None,
            mime: None,
        }
    }

    /// Every durable concept as (content, vector width, has a source).
    async fn durable_concepts(store: &Arc<MemoryStore>) -> Vec<(String, Option<usize>, bool)> {
        let snap = store.load_session(&SessionId::from(SESSION)).await.unwrap();
        let mut out: Vec<_> = snap
            .concepts
            .iter()
            .map(|c| {
                (
                    c.content.clone(),
                    c.embedding.as_ref().map(Vec::len),
                    c.embedding_source.is_some(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn args(missing_only: bool, drop_image_vectors: bool) -> Args {
        Args {
            session: SESSION.into(),
            agent: AGENT.into(),
            allow_embedding_mismatch: false,
            missing_only,
            drop_image_vectors,
            rewrite_same_contract: false,
            repin_canary: false,
        }
    }

    /// Whether a new writer can take the session's lease now. The probe
    /// releases what it took, so a test can go on writing.
    async fn lease_is_free(store: &Arc<MemoryStore>) -> bool {
        lease_is_free_on(store.as_ref()).await
    }

    /// [`lease_is_free`] on any store.
    async fn lease_is_free_on<S: GraphStore>(store: &S) -> bool {
        super::seed::lease_is_free(store, SESSION).await
    }

    /// #22 design Q19: a full migration must not leave an image vector in
    /// the old space, and cannot recompute one from a caption, so it refuses
    /// before any embed or write, releases the lease, and names the flag.
    #[tokio::test]
    async fn re_embed_refuses_image_vectors_unless_told_to_drop_them() {
        let store = Arc::new(MemoryStore::new());
        seed_session_with_source(&store, Some(image_source())).await;
        let before = durable_state(&store).await;
        let err = run(
            backends_on(store.clone(), "fixture", "fixture-model"),
            args(false, false),
        )
        .await
        .expect_err("image vectors block a migration");
        assert!(matches!(err, CliError::Runtime(_)), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("1 image concept still carries"), "{msg}");
        assert!(msg.contains("--drop-image-vectors"), "{msg}");
        assert!(msg.contains("nothing was written"), "{msg}");
        assert_eq!(durable_state(&store).await, before, "no writes");
        assert!(
            lease_is_free(&store).await,
            "the refusal released the lease"
        );
    }

    /// With `--drop-image-vectors` the migration nulls the image vectors in
    /// the same batch (keeping each source), rewrites every text concept,
    /// swaps the contract, and reports the count.
    #[tokio::test]
    async fn re_embed_drop_image_vectors_nulls_them_and_reports_the_count() {
        let store = Arc::new(MemoryStore::new());
        seed_session_with_source(&store, Some(image_source())).await;
        let out = run(
            backends_on(store.clone(), "fixture", "fixture-model"),
            args(false, true),
        )
        .await
        .expect("re-embed");
        assert!(out.contains("1 concept(s) rewritten"), "{out}");
        assert!(out.contains("dropped 1 image vector(s)"), "{out}");
        assert!(out.contains("coverage: embedded 1/2 -> 1/2"), "{out}");
        let (kind, _) = durable_state(&store).await;
        assert_eq!(kind.as_deref(), Some("fixture"), "contract migrated");
        assert_eq!(
            durable_concepts(&store).await,
            vec![
                ("auth middleware".into(), Some(1024), false),
                ("user schema".into(), None, true),
            ],
            "the text concept is embedded; the image keeps its source, not a caption vector"
        );
    }

    /// `--missing-only` fills text concepts only and says how many image
    /// concepts it skipped; a full migration likewise leaves an image concept
    /// whose vector is already missing alone.
    #[tokio::test]
    async fn re_embed_never_gives_an_image_concept_a_caption_vector() {
        for missing_only in [true, false] {
            let store = Arc::new(MemoryStore::new());
            // The vectored concept is text; the NULL one is an image whose
            // vector is missing.
            seed_session_with_sources(&store, None, Some(image_source())).await;
            let out = run(
                backends_on(
                    store.clone(),
                    if missing_only { "legacy" } else { "fixture" },
                    if missing_only { "v1" } else { "fixture-model" },
                ),
                args(missing_only, false),
            )
            .await
            .expect("re-embed");
            assert!(
                out.contains("skipped 1 image concept(s) whose vector is missing"),
                "{missing_only}: {out}"
            );
            let concepts = durable_concepts(&store).await;
            assert!(
                concepts.contains(&("auth middleware".into(), None, true)),
                "{missing_only}: the image stays without a vector: {concepts:?}"
            );
            assert!(
                concepts.contains(&("user schema".into(), Some(1024), false)),
                "{missing_only}: {concepts:?}"
            );
        }
    }

    #[tokio::test]
    async fn re_embed_refuses_missing_only_with_drop_image_vectors() {
        let err = run(
            backends_on(Arc::new(MemoryStore::new()), "fixture", "m"),
            args(true, true),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CliError::Usage(_)), "{err}");
    }

    #[tokio::test]
    async fn re_embed_refuses_the_allow_embedding_mismatch_override() {
        let err = run(
            backends_on(Arc::new(MemoryStore::new()), "fixture", "m"),
            Args {
                session: SESSION.into(),
                agent: AGENT.into(),
                allow_embedding_mismatch: true,
                missing_only: false,
                drop_image_vectors: false,
                rewrite_same_contract: false,
                repin_canary: false,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CliError::Usage(_)), "{err}");
        assert!(
            err.to_string().contains("re-embed IS the embedding"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn re_embed_on_an_empty_session_is_an_honest_no_op() {
        let store = Arc::new(MemoryStore::new());
        let out = run(
            backends_on(store, "fixture", "fixture-model"),
            Args {
                session: SESSION.into(),
                agent: AGENT.into(),
                allow_embedding_mismatch: false,
                missing_only: false,
                drop_image_vectors: false,
                rewrite_same_contract: false,
                repin_canary: false,
            },
        )
        .await
        .expect("an empty session is Ok, not an error");
        assert!(out.contains("no concepts; nothing to re-embed"), "{out}");
    }

    #[tokio::test]
    async fn re_embed_reports_a_held_lease_as_a_conflict() {
        let store = Arc::new(MemoryStore::new());
        seed_damaged_session(&store).await;

        // Another writer owns the session: the honest, named refusal.
        let other = LeaseHolder::for_this_process(&AgentId::from("another-writer"));
        match store
            .acquire_lease(&SessionId::from(SESSION), &other, Duration::from_secs(30))
            .await
            .unwrap()
        {
            LeaseOutcome::Acquired(_) => {}
            other => panic!("test setup must hold the lease, got {other:?}"),
        }

        let err = run(
            backends_on(store, "fixture", "fixture-model"),
            Args {
                session: SESSION.into(),
                agent: AGENT.into(),
                allow_embedding_mismatch: false,
                missing_only: false,
                drop_image_vectors: false,
                rewrite_same_contract: false,
                repin_canary: false,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("already held"), "{err}");
    }

    /// The embedder a drifted pin was taken from: the fixture's vectors of
    /// other texts, so no canary phrase is near the live fixture's.
    struct OldBuild(FixtureEmbedder);

    #[async_trait::async_trait]
    impl Embedder for OldBuild {
        fn dimensions(&self) -> usize {
            1024
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
            Ok(self
                .0
                .embed_sync(&format!("an older build of the model: {text}")))
        }
    }

    /// The contract [`seed_damaged_session`] stamps.
    fn legacy() -> EmbeddingContract {
        EmbeddingContract {
            kind: "legacy".into(),
            model: Some("v1".into()),
            dim: 1024,
        }
    }

    fn with_canary(
        mut backends: ResolvedBackends,
        mode: crate::embed::canary::CanaryMode,
    ) -> ResolvedBackends {
        backends.embedder_cfg.canary = mode;
        backends
    }

    /// Pin `contract`'s canary in `store` from `embedder`.
    async fn pin_from(
        store: &Arc<MemoryStore>,
        embedder: &dyn Embedder,
        contract: &EmbeddingContract,
    ) {
        crate::embed::canary::pin(store.as_ref(), embedder, contract)
            .await
            .unwrap();
    }

    /// What the live fixture embedder measures against `contract`'s pin.
    async fn verdict(
        store: &Arc<MemoryStore>,
        contract: &EmbeddingContract,
    ) -> crate::embed::canary::CanaryVerdict {
        crate::embed::canary::check(
            store.as_ref(),
            &FixtureEmbedder::new(),
            contract,
            &crate::embed::canary::CanaryOptions {
                mode: crate::embed::canary::CanaryMode::Verify,
                related_pairs: None,
            },
            0.85,
        )
        .await
        .unwrap()
        .verdict
    }

    /// #296, finding 1: the drift repair can be followed. A canary drift is
    /// only ever seen on a session under the live contract; the plain verb
    /// still refuses that session, now naming the flag, and
    /// `--rewrite-same-contract` rewrites every vector in place, keeps the
    /// contract and re-pins the canary. Over a drifted pin the re-pin also
    /// needs `--repin-canary`, and the run then says the store's other
    /// sessions under the contract are no longer flagged.
    ///
    /// Mirrors `Contract.reembedCmdFixed_repairs_drift`,
    /// `Contract.reembedCmdFixed_full_spec`,
    /// `Contract.reembedCmdFixed_plain_refuses_same_contract` and
    /// `Contract.example_drift_repaired_in_place`.
    #[tokio::test]
    async fn re_embed_rewrite_same_contract_repairs_a_canary_drift() {
        use crate::embed::canary::{CanaryMode, CanaryVerdict};
        let store = Arc::new(MemoryStore::new());
        seed_damaged_session(&store).await;
        pin_from(&store, &OldBuild(FixtureEmbedder::new()), &legacy()).await;
        assert!(
            matches!(
                verdict(&store, &legacy()).await,
                CanaryVerdict::Drifted { .. }
            ),
            "test setup: the live embedder drifted from the pin"
        );
        let live = || {
            with_canary(
                backends_on(store.clone(), "legacy", "v1"),
                CanaryMode::Verify,
            )
        };
        let before = durable_concepts(&store).await;

        let err = run(live(), args(false, false))
            .await
            .expect_err("the plain verb still refuses a same-contract session");
        assert!(err.to_string().contains("--rewrite-same-contract"), "{err}");
        assert_eq!(durable_concepts(&store).await, before, "no writes");

        let err = run(
            live(),
            Args {
                rewrite_same_contract: true,
                ..args(false, false)
            },
        )
        .await
        .expect_err("a drifted pin is not replaced without --repin-canary");
        let msg = err.to_string();
        assert!(msg.contains("--repin-canary"), "{msg}");
        assert!(msg.contains("nothing was written"), "{msg}");
        assert_eq!(durable_concepts(&store).await, before, "no writes");
        assert!(
            lease_is_free(&store).await,
            "the refusal released the lease"
        );

        let out = run(
            live(),
            Args {
                rewrite_same_contract: true,
                repin_canary: true,
                ..args(false, false)
            },
        )
        .await
        .expect("the in-place repair");
        assert!(out.contains("coverage: embedded 1/2 -> 2/2"), "{out}");
        assert!(out.contains("no longer flags"), "{out}");

        let snap = store.load_session(&SessionId::from(SESSION)).await.unwrap();
        assert_eq!(snap.embedding, Some(legacy()), "the contract did not move");
        let fixture = FixtureEmbedder::new();
        for c in &snap.concepts {
            assert_eq!(
                c.embedding.as_deref(),
                Some(fixture.embed_sync(&c.content).as_slice()),
                "{}: every vector is the live embedder's",
                c.content
            );
        }
        assert!(
            matches!(
                verdict(&store, &legacy()).await,
                CanaryVerdict::Verified { .. }
            ),
            "the canary is pinned again from the live embedder"
        );
        assert!(lease_is_free(&store).await);
    }

    /// `--rewrite-same-contract` is refused on a session not under the live
    /// contract (a migration) and on a session that holds no vector (a
    /// backfill), before anything is written.
    ///
    /// Mirrors `Contract.example_same_contract_preconditions`.
    #[tokio::test]
    async fn re_embed_rewrite_same_contract_refuses_a_migration_or_an_empty_space() {
        let store = Arc::new(MemoryStore::new());
        seed_damaged_session(&store).await;
        let before = durable_concepts(&store).await;
        let err = run(
            backends_on(store.clone(), "fixture", "fixture-model"),
            Args {
                rewrite_same_contract: true,
                repin_canary: true,
                ..args(false, false)
            },
        )
        .await
        .expect_err("a migration is not an in-place rewrite");
        assert!(err.to_string().contains("--rewrite-same-contract"), "{err}");
        assert_eq!(durable_concepts(&store).await, before, "no writes");

        // Two image concepts and no text concept: once a migration with
        // --drop-image-vectors nulls the one image vector, the session
        // holds none.
        let store = Arc::new(MemoryStore::new());
        seed_session_with_sources(&store, Some(image_source()), Some(image_source())).await;
        run(
            backends_on(store.clone(), "legacy", "v2"),
            args(false, true),
        )
        .await
        .expect("migration dropping the image vector");
        let err = run(
            backends_on(store.clone(), "legacy", "v2"),
            Args {
                rewrite_same_contract: true,
                ..args(false, false)
            },
        )
        .await
        .expect_err("a session with no vector has nothing to rewrite in place");
        assert!(err.to_string().contains("--missing-only"), "{err}");
    }

    /// #296, finding 3: `--missing-only` runs the canary. Under `verify` a
    /// drift refuses the backfill like any writer attach, before it writes;
    /// under `warn` the backfill runs as before.
    ///
    /// Mirrors `Contract.missingOnly_fixed_refuses_drift`,
    /// `Contract.missingOnly_fixed_agrees` and
    /// `Contract.example_drifted_backfill_fixed`.
    #[tokio::test]
    async fn re_embed_missing_only_runs_the_canary() {
        use crate::embed::canary::CanaryMode;
        let store = Arc::new(MemoryStore::new());
        seed_damaged_session(&store).await;
        pin_from(&store, &OldBuild(FixtureEmbedder::new()), &legacy()).await;
        let before = durable_concepts(&store).await;

        let err = run(
            with_canary(
                backends_on(store.clone(), "legacy", "v1"),
                CanaryMode::Verify,
            ),
            args(true, false),
        )
        .await
        .expect_err("a drifted backfill under verify is refused");
        let msg = err.to_string();
        assert!(msg.contains("canary drifted"), "{msg}");
        // Advice re-embed can follow: it refuses --allow-embedding-mismatch.
        assert!(msg.contains("--rewrite-same-contract"), "{msg}");
        assert!(!msg.contains("--allow-embedding-mismatch"), "{msg}");
        assert_eq!(durable_concepts(&store).await, before, "no writes");
        assert!(
            lease_is_free(&store).await,
            "the refusal released the lease"
        );

        let out = run(
            with_canary(backends_on(store.clone(), "legacy", "v1"), CanaryMode::Warn),
            args(true, false),
        )
        .await
        .expect("warn logs the drift and backfills");
        assert!(out.contains("coverage: embedded 1/2 -> 2/2"), "{out}");
    }

    /// #296, finding 2: no silent re-pin. A migration into a contract whose
    /// store pin the live embedder does not reproduce (another session's
    /// vectors rest on it) is refused before it writes unless
    /// `--repin-canary` is given, and then says the other sessions are no
    /// longer flagged. Over a pin the live embedder reproduces, the
    /// migration re-pins as before, without the opt-in or the warning.
    ///
    /// Mirrors `Contract.fixed_repin_unsafe_needs_optin`,
    /// `Contract.reembedCmdFixed_migration_agrees` and
    /// `Contract.example_repin_guard`.
    #[tokio::test]
    async fn re_embed_never_silently_replaces_a_pin_it_cannot_confirm() {
        use crate::embed::canary::CanaryMode;
        let target = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-model".into()),
            dim: 1024,
        };
        let store = Arc::new(MemoryStore::new());
        seed_damaged_session(&store).await;
        pin_from(&store, &OldBuild(FixtureEmbedder::new()), &target).await;
        let before = durable_state(&store).await;

        let err = run(
            with_canary(
                backends_on(store.clone(), "fixture", "fixture-model"),
                CanaryMode::Warn,
            ),
            args(false, false),
        )
        .await
        .expect_err("a drifted pin is not replaced silently");
        let msg = err.to_string();
        assert!(msg.contains("--repin-canary"), "{msg}");
        assert!(msg.contains("nothing was written"), "{msg}");
        assert_eq!(durable_state(&store).await, before, "no writes");
        assert!(
            lease_is_free(&store).await,
            "the refusal released the lease"
        );

        let out = run(
            with_canary(
                backends_on(store.clone(), "fixture", "fixture-model"),
                CanaryMode::Warn,
            ),
            Args {
                repin_canary: true,
                ..args(false, false)
            },
        )
        .await
        .expect("the opt-in replaces the pin");
        assert!(out.contains("no longer flags"), "{out}");
        assert_eq!(durable_state(&store).await.0.as_deref(), Some("fixture"));

        // A pin the live embedder reproduces: re-pinned with no opt-in and
        // no warning.
        let store = Arc::new(MemoryStore::new());
        seed_damaged_session(&store).await;
        pin_from(&store, &FixtureEmbedder::new(), &target).await;
        let out = run(
            with_canary(
                backends_on(store.clone(), "fixture", "fixture-model"),
                CanaryMode::Warn,
            ),
            args(false, false),
        )
        .await
        .expect("an agreeing pin needs no opt-in");
        assert!(out.contains("embedder canary pinned again"), "{out}");
        assert!(!out.contains("no longer flags"), "{out}");
    }

    /// Embeds one text at a time, but fails every batch. The canary check
    /// embeds its 20 phrases through `embed_batch`, so it cannot run, while
    /// the rewrite's own one-at-a-time embeds succeed: a provider throttling
    /// the canary burst.
    struct CanaryBatchFails(FixtureEmbedder);

    #[async_trait::async_trait]
    impl Embedder for CanaryBatchFails {
        fn dimensions(&self) -> usize {
            1024
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
            Ok(self.0.embed_sync(text))
        }
        async fn embed_batch(
            &self,
            texts: &[&str],
            _deadline: tokio::time::Instant,
        ) -> Vec<Result<Vec<f32>, crate::embed::EmbedError>> {
            texts
                .iter()
                .map(|_| Err(crate::embed::EmbedError::Backend("throttled".into())))
                .collect()
        }
    }

    /// #296 review: a pin check that could not run is not a drift. A
    /// migration over an existing pin whose check fails (here only the
    /// canary batch fails) is refused before it writes, with advice to retry
    /// once the embedder answers and `--repin-canary` offered only as the
    /// deliberate override; the retry with a working embedder then needs no
    /// opt-in. With the opt-in the rewrite runs, and when the pin after it
    /// fails the output names the exact recovery, which then works.
    ///
    /// Mirrors `Contract.fixed_repin_unsafe_needs_optin` and
    /// `Contract.pinAfterFixed_no_silent_replace` (an `unchecked` pin is
    /// replaced only with the opt-in).
    #[tokio::test]
    async fn re_embed_retries_rather_than_replaces_a_pin_it_could_not_check() {
        use crate::embed::canary::CanaryMode;
        let target = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-model".into()),
            dim: 1024,
        };
        let store = Arc::new(MemoryStore::new());
        seed_damaged_session(&store).await;
        pin_from(&store, &FixtureEmbedder::new(), &target).await;
        let before = durable_state(&store).await;
        let throttled = |store: &Arc<MemoryStore>| {
            let mut b = with_canary(
                backends_on(store.clone(), "fixture", "fixture-model"),
                CanaryMode::Warn,
            );
            b.embedder = Box::new(CanaryBatchFails(FixtureEmbedder::new()));
            b
        };

        let err = run(throttled(&store), args(false, false))
            .await
            .expect_err("an unchecked pin is not replaced without the opt-in");
        let msg = err.to_string();
        assert!(msg.contains("could not run"), "{msg}");
        assert!(msg.contains("Retry once the embedder answers"), "{msg}");
        assert!(
            msg.contains("knowing it was taken from another build"),
            "{msg}"
        );
        assert!(msg.contains("nothing was written"), "{msg}");
        assert_eq!(durable_state(&store).await, before, "no writes");
        assert!(
            lease_is_free(&store).await,
            "the refusal released the lease"
        );

        // The retry, once the embedder answers: the pin agrees, no opt-in.
        let out = run(
            with_canary(
                backends_on(store.clone(), "fixture", "fixture-model"),
                CanaryMode::Warn,
            ),
            args(false, false),
        )
        .await
        .expect("the retry migrates");
        assert!(!out.contains("no longer flags"), "{out}");

        // The deliberate override: the rewrite runs, the pin after it fails,
        // and the output names the recovery.
        let store = Arc::new(MemoryStore::new());
        seed_damaged_session(&store).await;
        pin_from(&store, &OldBuild(FixtureEmbedder::new()), &target).await;
        let out = run(
            throttled(&store),
            Args {
                repin_canary: true,
                ..args(false, false)
            },
        )
        .await
        .expect("the opt-in rewrites");
        assert!(out.contains("could not be pinned again"), "{out}");
        assert!(
            out.contains(&format!(
                "lambo re-embed --rewrite-same-contract --repin-canary --session {SESSION} \
                 --agent {AGENT}"
            )),
            "{out}"
        );
        assert_eq!(durable_state(&store).await.0.as_deref(), Some("fixture"));

        // The named recovery works once the embedder answers.
        let out = run(
            with_canary(
                backends_on(store.clone(), "fixture", "fixture-model"),
                CanaryMode::Verify,
            ),
            Args {
                rewrite_same_contract: true,
                repin_canary: true,
                ..args(false, false)
            },
        )
        .await
        .expect("the recovery");
        assert!(out.contains("embedder canary pinned again"), "{out}");
        assert!(
            matches!(
                verdict(&store, &target).await,
                crate::embed::canary::CanaryVerdict::Verified { .. }
            ),
            "the pin is the live embedder's"
        );
    }

    /// #296 review: `--repin-canary` under `canary = "off"` is a usage
    /// error, not silently ignored: with the canary off nothing is checked
    /// or pinned, so there is no pin for it to replace.
    ///
    /// Mirrors the usage refusal in `Contract.reembedCmdFixed`.
    #[tokio::test]
    async fn re_embed_refuses_repin_canary_with_the_canary_off() {
        let err = run(
            with_canary(
                backends_on(Arc::new(MemoryStore::new()), "fixture", "m"),
                crate::embed::canary::CanaryMode::Off,
            ),
            Args {
                repin_canary: true,
                ..args(false, false)
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CliError::Usage(_)), "{err}");
        assert!(err.to_string().contains("canary = \"off\""), "{err}");
    }

    /// An embedder whose every embed fails — drives the mid-run abort path.
    struct FailingEmbedder;

    #[async_trait::async_trait]
    impl Embedder for FailingEmbedder {
        fn dimensions(&self) -> usize {
            1024
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
            Err(crate::embed::EmbedError::Backend("failing embedder".into()))
        }
    }

    #[tokio::test]
    async fn re_embed_embed_failure_still_releases_the_lease() {
        // The old code returned from run() on an embed failure
        // WITHOUT close_writer, so the session stayed unwritable for the rest
        // of the TTL. The abort message is honest AND the lease goes back.
        let store = Arc::new(MemoryStore::new());
        seed_damaged_session(&store).await;
        let mut backends = backends_on(store.clone(), "fixture", "m");
        backends.embedder = Box::new(FailingEmbedder);

        let err = run(
            backends,
            Args {
                session: SESSION.into(),
                agent: AGENT.into(),
                allow_embedding_mismatch: false,
                missing_only: false,
                drop_image_vectors: false,
                rewrite_same_contract: false,
                repin_canary: false,
            },
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("re-embed aborted BEFORE mutating the graph"),
            "{err}"
        );

        // Durable state untouched: old contract and old vectors survive.
        let (kind, widths) = durable_state(&store).await;
        assert_eq!(kind.as_deref(), Some("legacy"));
        assert_eq!(widths, vec![None, Some(1024)]);

        // The point of the fix: a DIFFERENT writer can take the lease right
        // now, not after the TTL.
        let verifier = LeaseHolder::for_this_process(&AgentId::from("lease-verifier"));
        match store
            .acquire_lease(
                &SessionId::from(SESSION),
                &verifier,
                Duration::from_secs(30),
            )
            .await
            .unwrap()
        {
            LeaseOutcome::Acquired(_) => {}
            other => panic!("lease must be released after a failed migration, got {other:?}"),
        }
    }
}

/// #322 on the live Postgres family: the store's first-stamp quarantine there
/// is `QUARANTINE_LEGACY_EMBEDDINGS_SQL`, run before the `SetEmbedding`'s
/// session write. Live only, `#[ignore]`d like every live-database test:
///
/// ```text
/// LAMBO_POSTGRES_DSN=<dsn> cargo test --no-default-features --features store-postgres \
///   --lib cli::re_embed::postgres_live -- --ignored
/// ```
///
/// No embedder feature in that build, so the test carries its own
/// deterministic embedder.
#[cfg(all(test, feature = "store-postgres"))]
mod postgres_live {
    use super::{run, seed, Args};
    use crate::embed::{EmbedError, Embedder, EmbedderConfig};
    use crate::resolve::ResolvedBackends;
    use crate::store::pg::postgres::{postgres_dsn_or_skip, PostgresStore};
    use crate::store::{GraphStore, StoreConfig, StoreKind};
    use crate::types::EmbeddingContract;

    /// A deterministic embedder of the store's width: the vector of a text
    /// is a function of its bytes, so the test can recompute it.
    struct ByteEmbedder(usize);

    #[async_trait::async_trait]
    impl Embedder for ByteEmbedder {
        fn dimensions(&self) -> usize {
            self.0
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
            let bytes = text.as_bytes();
            Ok((0..self.0)
                .map(|i| f32::from(bytes[i % bytes.len()]) / 255.0 + 0.01)
                .collect())
        }
    }

    fn store_config(dsn: &str) -> StoreConfig {
        StoreConfig {
            kind: StoreKind::Postgres,
            dsn: Some(dsn.to_string()),
            path: None,
            vector_dim: None,
        }
    }

    /// Mirrors `reembedCmd322_contractless_durable` and
    /// `reembedOrdered_contractless_matches_graph` in
    /// `formal/LamboProofs/Contract.lean` on Postgres: after `lambo re-embed`
    /// of a pre-contract session, the raw rows hold the live contract and
    /// every new vector. Before the fix the trailing `SetEmbedding`'s
    /// quarantine nulled them (`reembed_contractless_durable_loses_vectors`).
    #[tokio::test]
    #[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
    async fn postgres_re_embed_of_a_contractless_session_keeps_its_new_vectors() {
        let Some(dsn) = postgres_dsn_or_skip(
            "postgres_re_embed_of_a_contractless_session_keeps_its_new_vectors",
        ) else {
            return;
        };
        let store = PostgresStore::new(store_config(&dsn)).expect("construct");
        store.init_schema().await.expect("init_schema");
        let dim = store.vector_dimensions().expect("pg stores carry vectors");
        let session = format!("reembed-contractless-{}", uuid::Uuid::new_v4());
        seed::seed_session(&store, &session, dim, None, None, None).await;
        let (contract, rows) = seed::raw_session(&store, &session).await;
        assert_eq!(contract, None, "the seed is a pre-contract session");
        assert_eq!(
            rows.iter()
                .map(|(c, v)| (c.as_str(), v.as_ref().map(Vec::len)))
                .collect::<Vec<_>>(),
            vec![("auth middleware", None), ("user schema", Some(dim))],
            "the legacy row keeps its vector until a contract is stamped"
        );

        let live = EmbeddingContract {
            kind: "byte-test".into(),
            model: Some("v1".into()),
            dim,
        };
        let backends = ResolvedBackends {
            store: Box::new(PostgresStore::new(store_config(&dsn)).expect("construct")),
            embedder: Box::new(ByteEmbedder(dim)),
            store_cfg: store_config(&dsn),
            embedder_cfg: EmbedderConfig {
                dim,
                // No store-wide canary pin in a shared live database.
                canary: crate::embed::canary::CanaryMode::Off,
                ..Default::default()
            },
            embedding: live.clone(),
            allow_embedding_mismatch: false,
            config: crate::Config::default(),
        };
        let out = run(
            backends,
            Args {
                session: session.clone(),
                agent: "operator".into(),
                allow_embedding_mismatch: false,
                missing_only: false,
                drop_image_vectors: false,
                rewrite_same_contract: false,
                repin_canary: false,
            },
        )
        .await
        .expect("re-embed");
        assert!(out.contains("coverage: embedded 0/2 -> 2/2"), "{out}");

        let (contract, rows) = seed::raw_session(&store, &session).await;
        assert_eq!(contract, Some(live), "the live contract is stamped");
        let widths: Vec<_> = rows
            .iter()
            .map(|(c, v)| (c.as_str(), v.as_ref().map(Vec::len)))
            .collect();
        let embedder = ByteEmbedder(dim);
        for (content, vector) in &rows {
            assert!(
                vector.as_ref() == Some(&embedder.embed(content).await.unwrap()),
                "the store holds the new vector of {content:?}; stored widths: {widths:?}"
            );
        }
        assert_eq!(rows.len(), 2);
        assert!(
            seed::lease_is_free(&store, &session).await,
            "the lease was released"
        );
    }
}

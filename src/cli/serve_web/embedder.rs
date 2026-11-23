//! The portal's embedder: loaded on the first recall that embeds a query,
//! or never (#247).
//!
//! The embedder is the portal's only heavy resource, and only recall's
//! vector leg uses it. The resolve built it the way it builds it for every
//! command ([`crate::resolve::resolve_backends`], the single construction
//! site): an in-process adapter (candle) has its identity and width but no
//! weights on the device until [`Embedder::prepare`] runs (issue #31). The
//! portal used to run that at startup. It no longer does:
//!
//! * **Lazy (the default).** Nothing loads at startup or for browsing:
//!   the page, counts, feeds, inspect, the graph and `/api/session` read the
//!   resolved [`crate::types::EmbeddingContract`], which needs no model. The
//!   first recall that actually embeds a query (the store can search by
//!   vector, the session's contract matches, and the query is not in the
//!   session's query cache) runs `prepare` through [`EmbedderGate`], and
//!   keeps the loaded model for the life of the process. Concurrent recalls
//!   share that one load. The adapter is the resolved one, so the kind,
//!   model and width are exactly what they were when startup loaded it.
//! * **Off (`[web] embedder = false` or `--no-embedder`).** `prepare` and
//!   every embed are never called. Recall runs on its keyword and recent
//!   legs, the path a failed embed already degrades to. On a store that can
//!   search by vector it carries [`OFF_BY_CONFIGURATION`] as its
//!   `vector_degraded` line, so the page says the vector leg is off rather
//!   than failed; on one that cannot there is no vector leg to mention.
//!
//! **A failed load is not remembered** (the view cache's rule): the recalls
//! that waited on the failed attempt get its error and degrade as a failed
//! embed does (`recall: query embedding failed (...); vector leg skipped`),
//! and the next recall tries again. Nothing waits on a load another recall
//! started and finished, and nothing panics.
//!
//! This file is the only portal source that calls `prepare`
//! (`embedder::only_the_gate_loads_the_embedder` pins it).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use crate::cli::caps::CliError;
use crate::cli::recall::{run_detailed_on_leg, CliRecall, QueryLeg, RecallRequest};
use crate::cli::LoadedReader;
use crate::embed::{same_error, EmbedError, Embedder};
use crate::recall::query_cache::QueryEmbeddingCache;
use crate::resolve::ResolvedBackends;

/// The `vector_degraded` line of every recall while the embedder is off by
/// configuration, on a store that could otherwise search by vector.
pub(super) const OFF_BY_CONFIGURATION: &str = "recall: vector leg off by configuration \
     ([web] embedder = false or --no-embedder); keyword and recent legs only";

/// Whether, and when, the portal loads its embedder. One per process, in
/// `AppState`.
pub(super) struct EmbedderGate {
    /// `[web] embedder`, with `--no-embedder` applied.
    enabled: bool,
    /// A `prepare` succeeded: every later embed goes straight through.
    loaded: AtomicBool,
    /// Load attempts finished, successful or not. A recall that saw this
    /// change while it waited takes that attempt's outcome.
    attempts: AtomicU64,
    /// Held for the length of one attempt: the single flight. Holds the
    /// last attempt's error, `None` after a success.
    turn: AsyncMutex<Option<EmbedError>>,
    /// Every `prepare` this gate ran.
    #[cfg(test)]
    prepares: AtomicU64,
    /// Recalls that reached the single flight (not the loaded fast path),
    /// so a test knows when every one of a burst is waiting on it.
    #[cfg(test)]
    arrivals: AtomicU64,
}

impl EmbedderGate {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            loaded: AtomicBool::new(false),
            attempts: AtomicU64::new(0),
            turn: AsyncMutex::new(None),
            #[cfg(test)]
            prepares: AtomicU64::new(0),
            #[cfg(test)]
            arrivals: AtomicU64::new(0),
        }
    }

    /// Whether recall may use the embedder at all.
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    /// How many times this gate has run `prepare`.
    #[cfg(test)]
    #[cfg_attr(
        not(all(feature = "store-memory", feature = "embed-fixture")),
        allow(dead_code)
    )]
    pub(super) fn prepares(&self) -> u64 {
        self.prepares.load(Ordering::SeqCst)
    }

    /// How many recalls have reached the single flight.
    #[cfg(test)]
    #[cfg_attr(
        not(all(feature = "store-memory", feature = "embed-fixture")),
        allow(dead_code)
    )]
    pub(super) fn arrivals(&self) -> u64 {
        self.arrivals.load(Ordering::SeqCst)
    }

    /// Recall `request` on `loaded`, the same pipeline `lambo recall` runs,
    /// with the vector leg's query embedded through this gate: loaded on
    /// first use, or off.
    pub(super) async fn recall(
        &self,
        backends: &ResolvedBackends,
        loaded: &LoadedReader,
        request: &RecallRequest,
        queries: Option<&Mutex<QueryEmbeddingCache>>,
    ) -> Result<CliRecall, CliError> {
        if !self.enabled {
            let leg = QueryLeg::Off(OFF_BY_CONFIGURATION);
            return run_detailed_on_leg(backends, loaded, request, queries, leg).await;
        }
        let gated = Gated {
            gate: self,
            inner: backends.embedder.as_ref(),
        };
        run_detailed_on_leg(backends, loaded, request, queries, QueryLeg::Embed(&gated)).await
    }

    /// Make `embedder` ready to embed: one `prepare` at a time, and a recall
    /// that waited on another's attempt takes its outcome instead of trying
    /// again. A failure is answered, not kept: a recall arriving after it
    /// tries again.
    ///
    /// A recall dropped mid-load (its client went away) releases the turn
    /// without recording an outcome, so the next one calls `prepare` again.
    /// That costs no second load: `prepare` is idempotent and shared by the
    /// adapter's clones, and candle's keeps loading on its blocking thread
    /// and makes a later call wait for that same load (#31).
    async fn load(&self, embedder: &dyn Embedder) -> Result<(), EmbedError> {
        if self.loaded.load(Ordering::Acquire) {
            return Ok(());
        }
        let arrived = self.attempts.load(Ordering::Acquire);
        #[cfg(test)]
        self.arrivals.fetch_add(1, Ordering::SeqCst);
        let mut last = self.turn.lock().await;
        if self.loaded.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.attempts.load(Ordering::Acquire) != arrived
            && let Some(err) = last.as_ref()
        {
            return Err(same_error(err));
        }
        // Only a model that is not on the device yet is worth a log line;
        // an HTTP adapter's `prepare` does nothing.
        let deferred = !embedder.is_prepared();
        if deferred {
            eprintln!("lambo serve-web: loading the embedder for the first recall that needs it");
        }
        let started = Instant::now();
        #[cfg(test)]
        self.prepares.fetch_add(1, Ordering::SeqCst);
        let outcome = embedder.prepare().await;
        match &outcome {
            Ok(()) => {
                *last = None;
                self.loaded.store(true, Ordering::Release);
                if deferred {
                    eprintln!(
                        "lambo serve-web: embedder loaded in {} ms; kept for the life of this \
                         process",
                        started.elapsed().as_millis()
                    );
                }
            }
            Err(err) => {
                eprintln!(
                    "lambo serve-web: the embedder could not be loaded ({err}); recall runs \
                     without its vector leg, and the next recall tries again"
                );
                *last = Some(same_error(err));
            }
        }
        self.attempts.fetch_add(1, Ordering::AcqRel);
        outcome
    }
}

/// The resolved embedder behind the gate: every embed loads it first. It
/// embeds with the resolved adapter itself, so its space is exactly
/// `ResolvedBackends::embedding`.
struct Gated<'a> {
    gate: &'a EmbedderGate,
    inner: &'a dyn Embedder,
}

#[async_trait]
impl<'a> Embedder for Gated<'a> {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.gate.load(self.inner).await?;
        self.inner.embed(text).await
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.gate.load(self.inner).await?;
        self.inner.embed_query(text).await
    }

    fn is_prepared(&self) -> bool {
        self.inner.is_prepared()
    }
}

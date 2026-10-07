//! Embedder keep-warm: periodic tiny embeds that make model weights more likely
//! to be resident in a long-lived, mostly idle `lambo serve` (issue #13).
//!
//! **The problem.** On Apple silicon the candle adapter holds BGE-M3's f16
//! weights (~1.1 GB) in Metal buffers, and Metal buffers on unified memory are
//! ordinary pageable memory to the macOS pager. Between calls nothing in the
//! process touches them — the ledger heartbeat reads graph counters only, and
//! the daemon tick and flush loop touch the graph only — so under memory
//! pressure macOS compresses (and eventually swaps) them, and the next call's
//! forward pays to bring them back. The live rig showed the cost growing with
//! the idle gap: recall p50 186 ms at < 2 min to 780 ms at > 30 min, p90 9 s,
//! against 71 ms for an isolated probe on the same store and binary.
//!
//! **What it promises, and what it does not.** A touch keeps *recently
//! touched* weights resident and so reduces how often a call pays the
//! swap-in; it does not guarantee residency. Under heavy memory pressure the
//! pager has been seen to take the whole weight set within 10-21 s of last
//! use, so a call that lands late in an interval can still pay the full
//! swap-in. The interval is the lever (see [`DEFAULT_KEEP_WARM_INTERVAL`]).
//!
//! **The policy.** One short, fixed probe text is embedded every interval and
//! the vector discarded. Why one tiny forward is enough on Metal: a forward
//! runs every encoder layer, and Metal makes residency decisions per buffer
//! per command buffer, so each weight buffer a kernel binds is made resident
//! whole, however few of its bytes the kernel reads. That covers the large
//! word-embedding table too (~250k x 1024 f16, about 512 MB) even though the
//! probe only gathers a handful of its rows. On CPU candle or llama.cpp the
//! same touch reads only the pages it uses, so most of that table stays cold
//! there; the encoder layers, which every forward reads in full, are what
//! a touch keeps warm on those backends. The touch writes nothing: no store
//! I/O, no graph mutation, no ledger line, no recall cache.
//!
//! * `keep_warm_secs` absent ⇒ **auto**: on (every
//!   [`DEFAULT_KEEP_WARM_INTERVAL`]) only when the resolved embedder holds its
//!   weights in host-pageable unified memory — today, the candle adapter on a
//!   Metal device. Off everywhere else: CUDA weights live in VRAM (the CUDA
//!   rig shows no tail), the fixture embedder has no weights, and the remote
//!   adapters (llama.cpp, Gemini) hold theirs in another process.
//! * `keep_warm_secs = 0` ⇒ off.
//! * `keep_warm_secs = N` ⇒ every `N` seconds, for any embedder kind (an
//!   operator running llama-server on the same Mac can opt in; each touch is
//!   then one HTTP embed request).
//!
//! The loop's first touch lands one interval after it is armed, never at arm
//! time: the model has just been loaded (and the attach has just used it), and
//! serve startup must not gain work before its handshake.

use std::sync::Arc;
use std::time::Duration;

use super::Embedder;

/// Auto interval for an embedder whose weights sit in pageable unified memory.
///
/// The live rig already paid 2.6x at its shortest bucket (< 2 min since the
/// previous call), so the touch period has to sit well inside that; 30 s gives
/// a 4x margin. The cost is one short forward per interval — on the order of
/// 10-20 ms of GPU time on an M3 Pro, i.e. well under 0.1% duty.
pub const DEFAULT_KEEP_WARM_INTERVAL: Duration = Duration::from_secs(30);

/// The text each touch embeds. Short (a handful of tokens) so a touch costs
/// one minimal forward; non-empty so it clears every adapter's CON-7 guard.
pub const KEEP_WARM_PROBE: &str = "lambo keep-warm probe";

/// Resolve the effective keep-warm interval from the configured setting and
/// the resolved embedder's memory placement. Pure, so the table is testable
/// without a device.
///
/// * `Some(0)` → `None` (off, explicitly)
/// * `Some(n)` → every `n` seconds
/// * `None` → [`DEFAULT_KEEP_WARM_INTERVAL`] when `weights_pageable` is true,
///   else `None`
pub fn resolve_keep_warm(setting: Option<u64>, weights_pageable: bool) -> Option<Duration> {
    match setting {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None if weights_pageable => Some(DEFAULT_KEEP_WARM_INTERVAL),
        None => None,
    }
}

/// Logging state for touch outcomes, so a persistently failing embedder warns
/// once per outage rather than once per interval.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum TouchState {
    #[default]
    Ok,
    Failing,
}

/// What to log after a touch, given the previous state. Pure for testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TouchLog {
    /// Nothing worth an operator's attention.
    Quiet,
    /// First failure after a success (or the first touch failed).
    Warn,
    /// First success after one or more failures.
    Recovered,
}

fn next_touch_state(prev: TouchState, ok: bool) -> (TouchState, TouchLog) {
    match (prev, ok) {
        (TouchState::Ok, true) => (TouchState::Ok, TouchLog::Quiet),
        (TouchState::Ok, false) => (TouchState::Failing, TouchLog::Warn),
        (TouchState::Failing, false) => (TouchState::Failing, TouchLog::Quiet),
        (TouchState::Failing, true) => (TouchState::Ok, TouchLog::Recovered),
    }
}

/// Embed [`KEEP_WARM_PROBE`] every `every`, discarding the vector. Runs until
/// the task is aborted (the serve holder path aborts it as soon as the
/// transport returns, before the close, and again beside the ledger heartbeat
/// after it).
///
/// Holds no lock across an await: the only await is the embed itself, and
/// what an adapter does inside it (the candle coalescer's queue mutex, an
/// HTTP request) is the same thing a recall does. A touch that coincides with
/// a real call either delays it by at most one minimal forward or is
/// coalesced into the candle adapter's batch. Coalesced, the batch is padded
/// to its longest member (`PaddingStrategy::BatchLongest`), so the forward is
/// two rows at the query's length rather than one, and batching can move the
/// query's vector by f16 rounding noise. That is no new effect: any two
/// concurrent real calls already batch the same way.
///
/// The period is measured from the end of one touch to the start of the next
/// (a sleep, not an interval timer): a touch that stalls (a cold swap-in, a
/// slow remote) is followed by the next one a full interval later, never by a
/// catch-up tick or a backlog.
pub async fn keep_warm_loop(embedder: Arc<dyn Embedder>, every: Duration) {
    let mut state = TouchState::default();
    loop {
        tokio::time::sleep(every).await;
        let started = tokio::time::Instant::now();
        let outcome = embedder.embed(KEEP_WARM_PROBE).await;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;
        let (next, log) = next_touch_state(state, outcome.is_ok());
        state = next;
        match (log, &outcome) {
            (TouchLog::Warn, Err(err)) => tracing::warn!(
                error = %err,
                "embedder keep-warm: touch failed; will keep trying every {}s (logged once \
                 until it recovers)",
                every.as_secs()
            ),
            (TouchLog::Recovered, _) => {
                tracing::info!(elapsed_ms, "embedder keep-warm: touch recovered")
            }
            _ => tracing::debug!(
                elapsed_ms,
                ok = outcome.is_ok(),
                "embedder keep-warm: touch"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::EmbedError;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[test]
    fn resolution_table() {
        // Auto follows memory placement.
        assert_eq!(
            resolve_keep_warm(None, true),
            Some(DEFAULT_KEEP_WARM_INTERVAL)
        );
        assert_eq!(resolve_keep_warm(None, false), None);
        // Explicit zero is off regardless of placement.
        assert_eq!(resolve_keep_warm(Some(0), true), None);
        assert_eq!(resolve_keep_warm(Some(0), false), None);
        // Explicit N is honoured regardless of placement.
        assert_eq!(
            resolve_keep_warm(Some(45), false),
            Some(Duration::from_secs(45))
        );
        assert_eq!(
            resolve_keep_warm(Some(1), true),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn probe_is_short_and_non_empty() {
        assert!(!KEEP_WARM_PROBE.trim().is_empty());
        assert!(KEEP_WARM_PROBE.len() < 64);
    }

    #[test]
    fn touch_logging_warns_once_per_outage() {
        use TouchLog::*;
        let outcomes = [true, false, false, false, true, true, false, true];
        let mut state = TouchState::default();
        let mut logs = Vec::new();
        for ok in outcomes {
            let (next, log) = next_touch_state(state, ok);
            state = next;
            logs.push(log);
        }
        assert_eq!(
            logs,
            vec![Quiet, Warn, Quiet, Quiet, Recovered, Quiet, Warn, Recovered]
        );
    }

    /// Counts touches and records the texts it was asked to embed; can be
    /// switched to fail.
    struct Recording {
        calls: AtomicUsize,
        texts: Mutex<Vec<String>>,
        fail: AtomicBool,
    }

    impl Recording {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                texts: Mutex::new(Vec::new()),
                fail: AtomicBool::new(false),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Embedder for Recording {
        fn dimensions(&self) -> usize {
            4
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.texts.lock().unwrap().push(text.to_string());
            if self.fail.load(Ordering::SeqCst) {
                Err(EmbedError::Unavailable("down".into()))
            } else {
                Ok(vec![1.0, 0.0, 0.0, 0.0])
            }
        }
    }

    /// Let spawned tasks run until they block, advance the paused clock by
    /// `d`, then let them run again. The leading yields matter: a task that has
    /// not yet been polled has not registered its sleep, and advancing before
    /// it does would silently shift its whole schedule.
    async fn advance(d: Duration) {
        settle().await;
        tokio::time::advance(d).await;
        settle().await;
    }

    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn first_touch_is_one_interval_after_arming_then_periodic() {
        let rec = Recording::new();
        let every = Duration::from_secs(30);
        let task = tokio::spawn(keep_warm_loop(rec.clone() as Arc<dyn Embedder>, every));

        // Nothing at arm time: serve startup must not gain a forward.
        advance(Duration::ZERO).await;
        assert_eq!(rec.calls(), 0, "no touch at arm time");
        advance(Duration::from_secs(29)).await;
        assert_eq!(rec.calls(), 0, "no touch before one interval");
        advance(Duration::from_secs(1)).await;
        assert_eq!(rec.calls(), 1, "first touch at one interval");
        advance(every).await;
        advance(every).await;
        assert_eq!(rec.calls(), 3, "one touch per interval thereafter");
        assert!(rec
            .texts
            .lock()
            .unwrap()
            .iter()
            .all(|t| t == KEEP_WARM_PROBE));

        task.abort();
        let _ = task.await;
        let after_abort = rec.calls();
        advance(every * 3).await;
        assert_eq!(rec.calls(), after_abort, "abort stops the loop");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_embedder_does_not_stop_the_loop() {
        let rec = Recording::new();
        rec.fail.store(true, Ordering::SeqCst);
        let every = Duration::from_secs(10);
        let task = tokio::spawn(keep_warm_loop(rec.clone() as Arc<dyn Embedder>, every));
        for _ in 0..3 {
            advance(every).await;
        }
        assert_eq!(rec.calls(), 3, "failures are logged, not fatal");
        rec.fail.store(false, Ordering::SeqCst);
        advance(every).await;
        assert_eq!(rec.calls(), 4, "keeps touching after recovery");
        task.abort();
    }

    /// An embed slower than the interval is followed by a full interval of
    /// quiet, not a catch-up touch or a burst.
    #[tokio::test(start_paused = true)]
    async fn a_slow_touch_does_not_cause_a_burst() {
        struct Slow {
            calls: AtomicUsize,
            each: Duration,
        }
        #[async_trait]
        impl Embedder for Slow {
            fn dimensions(&self) -> usize {
                4
            }
            async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(self.each).await;
                Ok(vec![1.0, 0.0, 0.0, 0.0])
            }
        }
        let every = Duration::from_secs(10);
        let slow = Arc::new(Slow {
            calls: AtomicUsize::new(0),
            each: Duration::from_secs(35),
        });
        let task = tokio::spawn(keep_warm_loop(slow.clone() as Arc<dyn Embedder>, every));
        // t=10: touch 1 starts, finishes at t=45; the next sleep ends at
        // t=55 (45 + 10), so by t=50 exactly one touch has started.
        advance(Duration::from_secs(10)).await;
        assert_eq!(slow.calls.load(Ordering::SeqCst), 1);
        advance(Duration::from_secs(35)).await; // t=45: touch 1 completes
        advance(Duration::from_secs(5)).await; // t=50
        assert_eq!(
            slow.calls.load(Ordering::SeqCst),
            1,
            "no catch-up burst after a slow touch"
        );
        advance(Duration::from_secs(5)).await; // t=55
        assert_eq!(slow.calls.load(Ordering::SeqCst), 2);
        task.abort();
    }
}

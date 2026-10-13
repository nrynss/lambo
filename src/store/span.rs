//! Shared coverage denominator for Stage 2 (#36).

use std::time::Duration;

use crate::types::StoreError;

/// Default denominator cap: fourteen days, not a rolling evidence expiry.
pub const DEFAULT_COVERAGE_WINDOW: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Smallest accepted cap: one day (#36 PR 2 review L3).
///
/// The cap exists so that a long session asks for days of spread, not a
/// share of its whole life. A cap of minutes would turn that around: in any
/// session longer than the cap, a burst of three citations a few minutes
/// apart would cover the whole denominator, which is exactly the burst
/// Stage 2 is there to refuse. One day also sits well above the default
/// 60-second `canonization_edge_min_age`, so the cap never measures spread
/// finer than the age floor that admits it.
pub const MIN_COVERAGE_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// Keep the cap at least [`MIN_COVERAGE_WINDOW`], exactly representable in
/// the adapters' millisecond arithmetic, and small enough for signed
/// timestamp-span arithmetic.
pub fn validate_coverage_window(window: Duration) -> Result<(), StoreError> {
    if window < MIN_COVERAGE_WINDOW
        || !window.subsec_nanos().is_multiple_of(1_000_000)
        || window.as_millis() > i64::MAX as u128
    {
        return Err(StoreError::Invariant(
            "canonization_coverage_window must be at least one day, in whole milliseconds, \
             within i64::MAX milliseconds"
                .into(),
        ));
    }
    Ok(())
}

pub(crate) fn coverage(span_ms: i64, session_ms: i64, window: Duration) -> f64 {
    if session_ms <= 0 {
        // Called only for nonempty support. F1: a single-point session.
        1.0
    } else {
        let denominator = (session_ms as f64).min(window.as_millis() as f64);
        (span_ms.max(0) as f64 / denominator).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_preserves_short_sessions_and_accepts_four_point_two_days() {
        assert_eq!(coverage(30, 55, DEFAULT_COVERAGE_WINDOW), 30.0 / 55.0);
        assert_eq!(
            coverage(362_880_000, 49 * 86_400_000, DEFAULT_COVERAGE_WINDOW),
            0.3
        );
        assert_eq!(coverage(0, 0, DEFAULT_COVERAGE_WINDOW), 1.0);
        assert_eq!(coverage(0, 100, DEFAULT_COVERAGE_WINDOW), 0.0);
        assert_eq!(
            coverage(30 * 86_400_000, 49 * 86_400_000, DEFAULT_COVERAGE_WINDOW),
            1.0
        );
    }

    #[test]
    fn reject_below_a_day_fractional_milliseconds_and_overflow() {
        for invalid in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_millis(1),
            Duration::from_secs(60),
            MIN_COVERAGE_WINDOW - Duration::from_millis(1),
            MIN_COVERAGE_WINDOW + Duration::from_nanos(1),
            Duration::MAX,
        ] {
            assert!(validate_coverage_window(invalid).is_err(), "{invalid:?}");
        }
        // The floor itself is accepted, as is anything whole above it.
        assert!(validate_coverage_window(MIN_COVERAGE_WINDOW).is_ok());
        assert!(validate_coverage_window(MIN_COVERAGE_WINDOW + Duration::from_millis(1)).is_ok());
        assert!(validate_coverage_window(DEFAULT_COVERAGE_WINDOW).is_ok());
    }

    /// A third-party store that predates #36 PR 2: it implements only the
    /// required methods, so `interaction_span_with_window` is the trait
    /// default. Every other method is unreachable; the default must not
    /// fall back to any of them.
    struct Legacy;

    #[async_trait::async_trait]
    impl crate::store::GraphStore for Legacy {
        fn capabilities(&self) -> crate::store::Capabilities {
            crate::store::Capabilities::empty()
        }
        async fn init_schema(&self) -> Result<(), StoreError> {
            unreachable!()
        }
        async fn flush(
            &self,
            _: &crate::types::MutationBatch,
            _: Option<u64>,
        ) -> Result<(), StoreError> {
            unreachable!()
        }
        async fn load_session(
            &self,
            _: &crate::types::SessionId,
        ) -> Result<crate::types::GraphSnapshot, StoreError> {
            unreachable!()
        }
        async fn keyword_candidates(
            &self,
            _: &crate::types::SessionId,
            _: &[String],
            _: usize,
        ) -> Result<Vec<crate::types::Scored<crate::types::NodeId>>, StoreError> {
            unreachable!()
        }
        async fn vector_candidates(
            &self,
            _: &crate::types::SessionId,
            _: &[f32],
            _: usize,
        ) -> Result<Vec<crate::types::Scored<crate::types::NodeId>>, StoreError> {
            unreachable!()
        }
        async fn blast_radius(
            &self,
            _: &crate::types::SessionId,
            _: crate::types::NodeId,
            _: Duration,
            _: chrono::DateTime<chrono::Utc>,
        ) -> Result<u64, StoreError> {
            unreachable!()
        }
        async fn interaction_span(
            &self,
            _: &crate::types::SessionId,
            _: crate::types::NodeId,
            _: Duration,
            _: chrono::DateTime<chrono::Utc>,
        ) -> Result<crate::types::InteractionSpan, StoreError> {
            unreachable!("the default must not substitute the raw span")
        }
        async fn record_canonization(
            &self,
            _: &crate::types::CanonizationEvent,
            _: Option<u64>,
        ) -> Result<(), StoreError> {
            unreachable!()
        }
    }

    /// The trait default fails closed with `Capability` for every valid cap,
    /// the default included, and still refuses an invalid cap as invalid.
    #[tokio::test]
    async fn the_trait_default_fails_closed_without_the_raw_span() {
        use crate::store::GraphStore;
        let session = crate::types::SessionId::from("legacy");
        let node = crate::types::NodeId::new();
        for window in [DEFAULT_COVERAGE_WINDOW, Duration::from_secs(28 * 86_400)] {
            let err = Legacy
                .interaction_span_with_window(
                    &session,
                    node,
                    Duration::ZERO,
                    chrono::Utc::now(),
                    window,
                )
                .await
                .unwrap_err();
            assert!(matches!(err, StoreError::Capability(_)), "{err:?}");
        }
        let err = Legacy
            .interaction_span_with_window(
                &session,
                node,
                Duration::ZERO,
                chrono::Utc::now(),
                Duration::from_secs(60),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
    }

    /// #20: both supersession accessors default to `Capability` (the
    /// evaluator then skips supersession for the cycle), and an invalid cap
    /// is still refused as invalid first.
    #[tokio::test]
    async fn the_supersession_defaults_fail_closed() {
        use crate::store::GraphStore;
        let session = crate::types::SessionId::from("legacy");
        let node = crate::types::NodeId::new();
        let now = chrono::Utc::now();
        let err = Legacy
            .latest_structural_evidence_at(&session, node, Duration::ZERO, now, &[])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Capability(_)), "{err:?}");
        let err = Legacy
            .interaction_span_since(
                &session,
                node,
                Duration::ZERO,
                now,
                DEFAULT_COVERAGE_WINDOW,
                now,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Capability(_)), "{err:?}");
        let err = Legacy
            .interaction_span_since(
                &session,
                node,
                Duration::ZERO,
                now,
                Duration::from_secs(60),
                now,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
    }

    /// #36 PR 2 review L4: after M1 a store that does not forward the
    /// window-aware span degrades quietly (no Stage 2 evidence, one warning),
    /// so a test wrapper that forgot to forward it would make every cycle it
    /// drives refuse Stage 2 without failing. Every in-tree `GraphStore` impl
    /// that defines `interaction_span` must therefore define
    /// `interaction_span_with_window` too. `Legacy` above is the one
    /// deliberate exception.
    ///
    /// Only lines that start (after indentation) with `impl` and name
    /// `GraphStore for ` are impl headers, so the literals in this test never
    /// count toward `checked`.
    ///
    /// Blind spots: `src/store/pg/postgres/tests.rs` is skipped (it holds
    /// fixture credentials this scan has no business reading), and only
    /// `src/` is walked, so integration tests under `tests/` and programs
    /// under `examples/` are not checked. A wrapper there that drives
    /// canonization needs the same forward by hand.
    #[test]
    fn every_in_tree_store_forwards_the_window_aware_span() {
        fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rust_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        fn block_end(s: &str, open: usize) -> usize {
            let mut depth = 0usize;
            for (i, c) in s[open..].char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            return open + i;
                        }
                    }
                    _ => {}
                }
            }
            s.len()
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let skipped = root.join("store/pg/postgres/tests.rs");
        let mut files = Vec::new();
        rust_files(&root, &mut files);
        let mut checked = 0;
        let mut missing = Vec::new();
        for file in files.iter().filter(|f| **f != skipped) {
            let text = std::fs::read_to_string(file).unwrap();
            let headers = text.split_inclusive('\n').scan(0, |offset, line| {
                let start = *offset;
                *offset += line.len();
                let trimmed = line.trim_start();
                let header = trimmed.starts_with("impl")
                    && trimmed.contains("GraphStore for ")
                    && !trimmed.starts_with("//");
                Some(header.then(|| start + line.find("GraphStore for ").unwrap()))
            });
            for at in headers.flatten().collect::<Vec<_>>() {
                let open = at + text[at..].find('{').unwrap();
                let block = &text[open..block_end(&text, open)];
                if !block.contains("fn interaction_span(") {
                    continue;
                }
                let name: String = text[at + "GraphStore for ".len()..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if name == "Legacy" && file.ends_with("store/span.rs") {
                    continue;
                }
                checked += 1;
                if !block.contains("fn interaction_span_with_window(") {
                    missing.push(format!("{}: {name}", file.display()));
                }
            }
        }
        assert!(checked > 40, "the scan found only {checked} impls");
        assert!(missing.is_empty(), "no window-aware forward: {missing:#?}");
    }
}

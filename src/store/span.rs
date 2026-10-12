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

#[cfg(any(feature = "store-memory", feature = "store-sqlite", test))]
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
}

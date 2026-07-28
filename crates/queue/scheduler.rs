//! Retry scheduling.
//!
//! The ladder is explicit rather than a formula, because the exact intervals
//! matter to the learning experience: a merchant watching the delivery log
//! should see recognizable, realistic spacing, and the docs quote these numbers.
//!
//!   attempt 1 fails → retry in 1 minute
//!   attempt 2 fails → 5 minutes
//!   attempt 3 fails → 30 minutes
//!   attempt 4 fails → 2 hours
//!   attempt 5 fails → 6 hours
//!   attempt 6 fails → 24 hours
//!   attempt 7 fails → dead (no further retries)

use time::{Duration, OffsetDateTime};

/// Backoff intervals in seconds, indexed by attempt number (1-based).
const LADDER_SECS: &[i64] = &[
    60,     // after attempt 1
    300,    // after attempt 2
    1_800,  // after attempt 3
    7_200,  // after attempt 4
    21_600, // after attempt 5
    86_400, // after attempt 6
];

/// Maximum attempts before a job is declared dead.
pub const MAX_ATTEMPTS: i32 = 7;

/// When should the next attempt run, given how many have already been made?
///
/// `attempts` is the count INCLUDING the one that just failed (the store's
/// `claim_batch` increments before handing the job out, so the value read off
/// a claimed job is already correct).
///
/// Returns `None` when the ladder is exhausted — the caller then marks the job
/// dead rather than rescheduling.
pub fn next_retry_at(attempts: i32, now: OffsetDateTime) -> Option<OffsetDateTime> {
    if attempts < 1 {
        return Some(now);
    }
    // attempts >= 1 is checked above, so attempts - 1 >= 0; fall back to an
    // out-of-range index (rather than panicking) if that invariant ever breaks.
    let idx = usize::try_from(attempts - 1).unwrap_or(usize::MAX);
    LADDER_SECS
        .get(idx)
        .map(|&secs| now + Duration::seconds(secs))
}

/// Human-readable ladder for the docs and dashboard tooltip.
pub fn ladder_description() -> Vec<&'static str> {
    vec!["1m", "5m", "30m", "2h", "6h", "24h", "dead"]
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    #[test]
    fn first_failure_retries_in_one_minute() {
        assert_eq!(
            next_retry_at(1, now()).unwrap(),
            now() + Duration::minutes(1)
        );
    }

    #[test]
    fn ladder_progresses_correctly() {
        let expected = [
            Duration::minutes(1),
            Duration::minutes(5),
            Duration::minutes(30),
            Duration::hours(2),
            Duration::hours(6),
            Duration::hours(24),
        ];
        for (i, exp) in expected.iter().enumerate() {
            let attempt = i32::try_from(i + 1).unwrap();
            assert_eq!(
                next_retry_at(attempt, now()).unwrap(),
                now() + *exp,
                "attempt {attempt} spacing wrong"
            );
        }
    }

    #[test]
    fn ladder_exhausts_after_six_retries() {
        assert!(next_retry_at(6, now()).is_some()); // last retry
        assert!(next_retry_at(7, now()).is_none()); // dead
        assert!(next_retry_at(100, now()).is_none());
    }

    #[test]
    fn ladder_length_matches_max_attempts() {
        // MAX_ATTEMPTS counts the initial try plus every retry in the ladder.
        assert_eq!(i32::try_from(LADDER_SECS.len()).unwrap() + 1, MAX_ATTEMPTS);
    }

    #[test]
    fn total_retry_window_is_about_33_hours() {
        let total: i64 = LADDER_SECS.iter().sum();
        assert!(total > 32 * 3600 && total < 34 * 3600);
    }
}

use std::sync::{Arc, RwLock};

use time::OffsetDateTime;

/// The source of "now" for the entire system.
///
/// # Why this exists
///
/// No code in `domain`, `store`, or `engine` may call `OffsetDateTime::now_utc()`
/// directly. Every timestamp comes from an injected `Clock`. This buys three
/// things that are impossible otherwise:
///
/// 1. **Time travel.** The sandbox exposes `POST /v1/test/clock/advance`, which
///    lets a learner watch T+2 settlement resolve in ten seconds instead of two
///    days. That endpoint is only implementable if nothing reads the wall clock
///    behind our back.
/// 2. **Deterministic tests.** Retry ladders, evidence deadlines and expiry
///    windows are testable without `sleep`.
/// 3. **Reproducible scenarios.** A graded exercise replays identically every run.
///
/// Enforce this with a CI check:
///
/// ```bash
/// ! grep -rn "now_utc()" crates/domain crates/store crates/engine crates/queue
/// ```
///
/// The bounds matter: `Send + Sync + 'static` so a `Clock` can live inside an
/// `Arc` shared across tokio tasks; `Debug` so `AppState` can derive `Debug`.
pub trait Clock: Send + Sync + std::fmt::Debug + 'static {
    fn now(&self) -> OffsetDateTime;

    /// Unix timestamp in seconds. Used for webhook signature timestamps.
    fn unix_timestamp(&self) -> i64 {
        self.now().unix_timestamp()
    }
}

/// Production clock. Reads the operating system time.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// A clock frozen at a fixed instant, movable on demand.
///
/// Used in unit and integration tests. The advanceable `VirtualClock` exposed
/// through the test API lives in the `simulator` crate and builds on this.
#[derive(Debug, Clone)]
pub struct FixedClock {
    inner: Arc<RwLock<OffsetDateTime>>,
}

impl FixedClock {
    pub fn new(at: OffsetDateTime) -> Self {
        Self {
            inner: Arc::new(RwLock::new(at)),
        }
    }

    /// Freeze at the current wall-clock instant.
    pub fn frozen_now() -> Self {
        Self::new(OffsetDateTime::now_utc())
    }

    /// Move the clock forward. Negative durations move it back.
    pub fn advance(&self, by: time::Duration) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard += by;
    }

    /// Jump to a specific instant.
    pub fn set(&self, to: OffsetDateTime) {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = to;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        *self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Blanket impl so `Arc<dyn Clock>` and `Arc<SystemClock>` both satisfy `Clock`.
/// Lets services hold `Arc<dyn Clock>` without wrapper types.
impl<T: Clock + ?Sized> Clock for Arc<T> {
    fn now(&self) -> OffsetDateTime {
        (**self).now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn system_clock_advances_on_its_own() {
        let c = SystemClock;
        let a = c.now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = c.now();
        assert!(b > a);
    }

    #[test]
    fn fixed_clock_does_not_move_by_itself() {
        let c = FixedClock::new(datetime!(2026-01-01 00:00:00 UTC));
        let a = c.now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(c.now(), a);
    }

    #[test]
    fn advance_moves_the_clock_forward() {
        let c = FixedClock::new(datetime!(2026-01-01 00:00:00 UTC));
        c.advance(time::Duration::hours(48));
        assert_eq!(c.now(), datetime!(2026-01-03 00:00:00 UTC));
    }

    #[test]
    fn advance_accepts_negative_durations() {
        let c = FixedClock::new(datetime!(2026-01-03 00:00:00 UTC));
        c.advance(time::Duration::hours(-48));
        assert_eq!(c.now(), datetime!(2026-01-01 00:00:00 UTC));
    }

    #[test]
    fn clones_share_the_same_underlying_instant() {
        let a = FixedClock::new(datetime!(2026-01-01 00:00:00 UTC));
        let b = a.clone();
        a.advance(time::Duration::days(1));
        // b sees a's advance because the inner Arc is shared.
        assert_eq!(b.now(), datetime!(2026-01-02 00:00:00 UTC));
    }

    #[test]
    fn works_behind_a_trait_object() {
        let c: Arc<dyn Clock> = Arc::new(FixedClock::new(datetime!(2026-06-01 12:00:00 UTC)));
        assert_eq!(c.now(), datetime!(2026-06-01 12:00:00 UTC));
        assert_eq!(c.unix_timestamp(), 1_780_315_200);
    }
}

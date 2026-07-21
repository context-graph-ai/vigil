use chrono::{DateTime, Utc};
use contextdb_core::Wallclock;
use std::sync::Arc;

/// Cloneable persisted-time authority that can be carried into worker threads.
///
/// Production uses ContextDB's wall clock. Tests that need exact ordering inject
/// one shared source at worker construction, so correctness does not depend on a
/// thread-local override being inherited or on sleeping long enough for time to move.
#[derive(Clone)]
pub struct PersistedClock {
    millis: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl PersistedClock {
    pub fn contextdb() -> Self {
        Self::from_millis_source(|| Wallclock::now().0)
    }

    pub fn from_millis_source(source: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self {
            millis: Arc::new(source),
        }
    }

    pub fn unix_millis(&self) -> u64 {
        (self.millis)()
    }

    pub fn now_utc(&self) -> DateTime<Utc> {
        utc_from_millis(self.unix_millis())
    }
}

impl Default for PersistedClock {
    fn default() -> Self {
        Self::contextdb()
    }
}

/// The one Vigil adapter for persisted wall time.
///
/// Duration and timeout measurement stays on `Instant`. Values written into
/// Context Graph observations, work envelopes, and receipts come through the
/// substrate clock so tests can advance them without sleeping.
pub(crate) fn now_utc() -> DateTime<Utc> {
    utc_from_millis(Wallclock::now().0)
}

fn utc_from_millis(millis: u64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(
        i64::try_from(millis).expect("wall-clock milliseconds fit chrono's signed range"),
    )
    .expect("wall-clock milliseconds form a valid UTC timestamp")
}

pub(crate) fn unix_seconds() -> u64 {
    Wallclock::now().0 / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_time_uses_the_contextdb_wallclock_seam() {
        let _clock = Wallclock::test_clock_guard(|| 1_700_000_000_123);
        assert_eq!(now_utc().timestamp_millis(), 1_700_000_000_123);
        assert_eq!(unix_seconds(), 1_700_000_000);
    }
}

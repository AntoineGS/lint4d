//! The store's clock: what leases, hints and the health backoff measure
//! (spec §12.1 amendment 2026-11-03: tests move it, never sleep).

use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct Clock {
    #[cfg(feature = "test-support")]
    offset_millis: std::sync::atomic::AtomicU64,
}

impl Clock {
    fn offset(&self) -> Duration {
        #[cfg(feature = "test-support")]
        let offset = Duration::from_millis(
            self.offset_millis
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        #[cfg(not(feature = "test-support"))]
        let offset = Duration::ZERO;
        offset
    }

    pub(crate) fn now(&self) -> Instant {
        Instant::now() + self.offset()
    }

    #[allow(dead_code)] // first used by the lease renewal
    pub(crate) fn unix_seconds(&self) -> u64 {
        super::unix_seconds() + self.offset().as_secs()
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn advance(&self, by: Duration) {
        self.offset_millis.fetch_add(
            u64::try_from(by.as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

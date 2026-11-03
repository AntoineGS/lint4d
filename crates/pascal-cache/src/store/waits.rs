//! Process-wide lock-wait counters (spec §10 amendment 2026-11-03, D28):
//! waits for a pooled connection, transaction starts that waited for
//! SQLite's lock, and `Busy` outcomes. One admitted store per process, so
//! process-wide is per store in the LSP.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A `BEGIN IMMEDIATE` faster than this did not wait for a lock.
pub const LOCK_WAIT_THRESHOLD: Duration = Duration::from_millis(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LockWaits {
    pub pool_waits: u64,
    pub pool_wait_micros: u64,
    pub lock_waits: u64,
    pub lock_wait_micros: u64,
    pub busy: u64,
}

static POOL_WAITS: AtomicU64 = AtomicU64::new(0);
static POOL_WAIT_MICROS: AtomicU64 = AtomicU64::new(0);
static LOCK_WAITS: AtomicU64 = AtomicU64::new(0);
static LOCK_WAIT_MICROS: AtomicU64 = AtomicU64::new(0);
static BUSY: AtomicU64 = AtomicU64::new(0);

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

pub fn lock_waits() -> LockWaits {
    LockWaits {
        pool_waits: POOL_WAITS.load(Ordering::Relaxed),
        pool_wait_micros: POOL_WAIT_MICROS.load(Ordering::Relaxed),
        lock_waits: LOCK_WAITS.load(Ordering::Relaxed),
        lock_wait_micros: LOCK_WAIT_MICROS.load(Ordering::Relaxed),
        busy: BUSY.load(Ordering::Relaxed),
    }
}

pub(crate) fn record_pool_wait(waited: Duration) {
    POOL_WAITS.fetch_add(1, Ordering::Relaxed);
    POOL_WAIT_MICROS.fetch_add(micros(waited), Ordering::Relaxed);
}

pub(crate) fn record_lock_wait(waited: Duration) {
    if waited >= LOCK_WAIT_THRESHOLD {
        LOCK_WAITS.fetch_add(1, Ordering::Relaxed);
        LOCK_WAIT_MICROS.fetch_add(micros(waited), Ordering::Relaxed);
    }
}

pub(crate) fn record_busy() {
    BUSY.fetch_add(1, Ordering::Relaxed);
}

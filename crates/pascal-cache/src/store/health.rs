//! The process-level health of a store (spec §10 amendment 2026-11-03,
//! D15): after a full, read-only, recovering or corrupt store, operations
//! it blocks bypass without touching SQLite until a bounded backoff ends;
//! the next operation then probes.

use super::outcome::Bypass;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

pub const MIN_HEALTH_BACKOFF: Duration = Duration::from_secs(1);
pub const MAX_HEALTH_BACKOFF: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unhealthy {
    Full,
    ReadOnly,
    Recovering,
    Corrupt,
}

impl Unhealthy {
    fn of(bypass: &Bypass) -> Option<Self> {
        match bypass {
            Bypass::Full => Some(Self::Full),
            Bypass::ReadOnly => Some(Self::ReadOnly),
            Bypass::Recovering => Some(Self::Recovering),
            Bypass::Corrupt => Some(Self::Corrupt),
            _ => None,
        }
    }

    pub fn bypass(self) -> Bypass {
        match self {
            Self::Full => Bypass::Full,
            Self::ReadOnly => Bypass::ReadOnly,
            Self::Recovering => Bypass::Recovering,
            Self::Corrupt => Bypass::Corrupt,
        }
    }

    /// A full store still serves reads and the maintenance that frees space.
    fn blocks(self, access: Access) -> bool {
        self != Self::Full || access == Access::Write
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Maintenance,
}

#[derive(Debug, Clone, Copy)]
struct State {
    unhealthy: Unhealthy,
    retry_at: Instant,
    backoff: Duration,
}

#[derive(Default)]
pub(crate) struct Health(Mutex<Option<State>>);

impl Health {
    pub(crate) fn state(&self) -> Option<Unhealthy> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map(|state| state.unhealthy)
    }

    pub(crate) fn blocked(&self, access: Access, now: Instant) -> Option<Bypass> {
        let state = (*self.0.lock().unwrap_or_else(PoisonError::into_inner))?;
        (now < state.retry_at && state.unhealthy.blocks(access)).then(|| state.unhealthy.bypass())
    }

    pub(crate) fn record<T>(&self, access: Access, result: &Result<T, Bypass>, now: Instant) {
        let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match result {
            Err(bypass) => {
                let Some(unhealthy) = Unhealthy::of(bypass) else {
                    return;
                };
                let backoff = match *slot {
                    Some(state) if state.unhealthy == unhealthy => {
                        (state.backoff * 2).min(MAX_HEALTH_BACKOFF)
                    }
                    _ => MIN_HEALTH_BACKOFF,
                };
                *slot = Some(State {
                    unhealthy,
                    retry_at: now + backoff,
                    backoff,
                });
            }
            // Only an operation the state blocks proves it is over.
            Ok(_) if slot.is_some_and(|state| state.unhealthy.blocks(access)) => *slot = None,
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_store_blocks_writes_but_not_reads_or_maintenance() {
        let health = Health::default();
        let t0 = Instant::now();
        health.record(Access::Write, &Err::<(), _>(Bypass::Full), t0);
        assert_eq!(health.blocked(Access::Write, t0), Some(Bypass::Full));
        assert_eq!(health.blocked(Access::Read, t0), None);
        assert_eq!(health.blocked(Access::Maintenance, t0), None);
        health.record(Access::Read, &Ok(()), t0);
        assert_eq!(
            health.state(),
            Some(Unhealthy::Full),
            "a read proves nothing about space"
        );
    }

    #[test]
    fn corruption_blocks_everything_until_the_backoff_then_one_probe() {
        let health = Health::default();
        let t0 = Instant::now();
        health.record(Access::Read, &Err::<(), _>(Bypass::Corrupt), t0);
        for access in [Access::Read, Access::Write, Access::Maintenance] {
            assert_eq!(health.blocked(access, t0), Some(Bypass::Corrupt));
        }
        let probe = t0 + MIN_HEALTH_BACKOFF;
        assert_eq!(
            health.blocked(Access::Read, probe),
            None,
            "the backoff expired: probe"
        );
        health.record(Access::Read, &Err::<(), _>(Bypass::Corrupt), probe);
        assert_eq!(
            health.blocked(Access::Read, probe + MIN_HEALTH_BACKOFF),
            Some(Bypass::Corrupt),
            "doubled"
        );
        let second = probe + 2 * MIN_HEALTH_BACKOFF;
        assert_eq!(health.blocked(Access::Read, second), None);
        health.record(Access::Read, &Ok(()), second);
        assert_eq!(health.state(), None, "a successful probe clears the state");
    }

    #[test]
    fn the_backoff_is_bounded() {
        let health = Health::default();
        let mut now = Instant::now();
        for _ in 0..20 {
            health.record(Access::Write, &Err::<(), _>(Bypass::Recovering), now);
            now += MAX_HEALTH_BACKOFF;
        }
        assert_eq!(
            health.blocked(Access::Read, now),
            None,
            "never longer than the maximum"
        );
    }

    #[test]
    fn ordinary_bypasses_do_not_mark_the_store_unhealthy() {
        let health = Health::default();
        let t0 = Instant::now();
        for bypass in [
            Bypass::Busy,
            Bypass::Fenced,
            Bypass::ScratchExhausted,
            Bypass::Retiring,
        ] {
            health.record(Access::Write, &Err::<(), _>(bypass), t0);
        }
        assert_eq!(health.state(), None);
    }
}

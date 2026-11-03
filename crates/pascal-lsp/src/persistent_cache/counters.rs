//! Process-wide diagnostic counters (spec §11.2 amendment 2026-11-03, D27).
//! Fixed-size, so the status document stays bounded.

use super::names::{self, BYPASS_KINDS};
use crate::workspace::persist::{PersistError, Reuse};
use pascal_cache::{Bypass, StepReport};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissCause {
    Input,
    Version,
    Freshness,
    Budget,
    Authorization,
    Integrity,
}

pub(crate) const MISS_CAUSES: usize = 6;
pub(crate) const MISS_NAMES: [&str; MISS_CAUSES] = [
    "input",
    "version",
    "freshness",
    "budget",
    "authorization",
    "integrity",
];

pub(crate) struct Counters {
    hits: AtomicU64,
    decoded_bytes: AtomicU64,
    misses: [AtomicU64; MISS_CAUSES],
    bypasses: [AtomicU64; BYPASS_KINDS],
    steps: AtomicU64,
    cycles: AtomicU64,
    rows: AtomicU64,
    skipped: AtomicU64,
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            hits: AtomicU64::new(0),
            decoded_bytes: AtomicU64::new(0),
            misses: std::array::from_fn(|_| AtomicU64::new(0)),
            bypasses: std::array::from_fn(|_| AtomicU64::new(0)),
            steps: AtomicU64::new(0),
            cycles: AtomicU64::new(0),
            rows: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CounterSnapshot {
    pub(crate) hits: u64,
    pub(crate) decoded_bytes: u64,
    pub(crate) misses: [u64; MISS_CAUSES],
    pub(crate) bypasses: [u64; BYPASS_KINDS],
    pub(crate) steps: u64,
    pub(crate) cycles: u64,
    pub(crate) rows: u64,
    pub(crate) skipped: u64,
}

impl Default for CounterSnapshot {
    fn default() -> Self {
        Self {
            hits: 0,
            decoded_bytes: 0,
            misses: [0; MISS_CAUSES],
            bypasses: [0; BYPASS_KINDS],
            steps: 0,
            cycles: 0,
            rows: 0,
            skipped: 0,
        }
    }
}

impl Counters {
    pub(crate) fn record_hit(&self, decoded_bytes: u64) {
        self.hits.fetch_add(1, Relaxed);
        self.decoded_bytes.fetch_add(decoded_bytes, Relaxed);
    }

    pub(crate) fn record_miss(&self, cause: MissCause) {
        self.misses[cause as usize].fetch_add(1, Relaxed);
    }

    pub(crate) fn record_bypass(&self, bypass: &Bypass) {
        self.bypasses[names::bypass_index(bypass)].fetch_add(1, Relaxed);
    }

    /// Record one `reuse_persisted_facts` outcome (`l4d-zf1b.8` calls this).
    pub(crate) fn record_reuse(&self, result: &Result<Reuse, PersistError>) {
        match result {
            Ok(Reuse::Hit { facts, .. }) => self.record_hit(facts.charge.bytes() as u64),
            Ok(Reuse::Miss(miss)) => self.record_miss(miss.cause()),
            Err(error) => match error.cause() {
                Ok(cause) => self.record_miss(cause),
                Err(bypass) => self.record_bypass(bypass),
            },
        }
    }

    pub(crate) fn record_step(&self, report: &StepReport) {
        self.steps.fetch_add(1, Relaxed);
        self.cycles
            .fetch_add(u64::from(report.cycle_complete), Relaxed);
        self.rows.fetch_add(report.rows as u64, Relaxed);
        self.skipped.fetch_add(u64::from(report.skipped), Relaxed);
    }

    pub(crate) fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            hits: self.hits.load(Relaxed),
            decoded_bytes: self.decoded_bytes.load(Relaxed),
            misses: std::array::from_fn(|index| self.misses[index].load(Relaxed)),
            bypasses: std::array::from_fn(|index| self.bypasses[index].load(Relaxed)),
            steps: self.steps.load(Relaxed),
            cycles: self.cycles.load(Relaxed),
            rows: self.rows.load(Relaxed),
            skipped: self.skipped.load(Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_add_up_and_snapshot() {
        let counters = Counters::default();
        counters.record_hit(100);
        counters.record_hit(28);
        counters.record_miss(MissCause::Freshness);
        counters.record_bypass(&Bypass::Busy);
        counters.record_bypass(&Bypass::Busy);
        let snapshot = counters.snapshot();
        assert_eq!((snapshot.hits, snapshot.decoded_bytes), (2, 128));
        assert_eq!(snapshot.misses[MissCause::Freshness as usize], 1);
        assert_eq!(snapshot.bypasses[names::bypass_index(&Bypass::Busy)], 2);
    }
}

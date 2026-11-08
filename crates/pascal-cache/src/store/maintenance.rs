//! The maintenance driver (spec §10 amendment 2026-11-03, D11-D13): one
//! cycle of bounded transactions, stepped by the caller with a time and
//! row budget off interactive paths, cancellable between transactions and
//! resumable from the database. One step at a time per store directory,
//! under `maintenance.lock`.

use super::Store;
use super::budget::Pressure;
use super::gc::{MaintenanceLimits, MaintenanceLimitsError, RetireStep};
use super::health::Access;
use super::location::StoreLocation;
use super::namespaces::open_lock_file;
use super::outcome::Bypass;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// What one [`Maintenance::step`] may spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepBudget {
    /// Real elapsed time; checked before every transaction.
    pub time: Duration,
    /// Rows the step's transactions change at most (vacuum pages count as
    /// rows; lease renewals and hint flushes do not).
    pub rows: usize,
}

/// The phases of one maintenance cycle, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// Flush pending recency hints.
    Hints,
    /// Renew this process's lease when due, then delete expired leases.
    Leases,
    /// A passive checkpoint (and truncation when complete).
    Checkpoint,
    /// Retire the namespaces retention does not keep and no live process
    /// holds (`Store::retire_obsolete`).
    ObsoleteNamespaces,
    AbandonedBuilds,
    UnusedSnapshots,
    Records,
    Evidence,
    Artifacts,
    /// `Store::reclaim`: retired scopes, retired live-scope rows, orphan
    /// manifests.
    Delete,
    /// Bounded `incremental_vacuum`.
    Vacuum,
    /// The cycle is over; the next step starts a new one.
    Done,
}

impl Phase {
    fn next(self) -> Self {
        match self {
            Self::Hints => Self::Leases,
            Self::Leases => Self::Checkpoint,
            Self::Checkpoint => Self::ObsoleteNamespaces,
            Self::ObsoleteNamespaces => Self::AbandonedBuilds,
            Self::AbandonedBuilds => Self::UnusedSnapshots,
            Self::UnusedSnapshots => Self::Records,
            Self::Records => Self::Evidence,
            Self::Evidence => Self::Artifacts,
            Self::Artifacts => Self::Delete,
            Self::Delete => Self::Vacuum,
            Self::Vacuum | Self::Done => Self::Done,
        }
    }
}

/// What one step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepReport {
    /// Another holder had `maintenance.lock`: nothing ran but a due lease
    /// renewal.
    pub skipped: bool,
    /// `cancel` was set; the step stopped between transactions.
    pub cancelled: bool,
    /// The phase the next step resumes at.
    pub phase: Phase,
    /// Rows changed (see [`StepBudget::rows`]).
    pub rows: usize,
    /// The step finished the cycle.
    pub cycle_complete: bool,
    /// The store's pressure as last read by the step.
    pub pressure: Pressure,
}

/// The cycle's position. A new `Maintenance` (after a crash, say) starts at
/// [`Phase::Hints`]: every phase finds its work from the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Maintenance {
    limits: MaintenanceLimits,
    phase: Phase,
}

/// What one phase transaction did.
enum Ran {
    /// `rows` changed; `finished` when the phase has nothing left.
    Batch { rows: usize, finished: bool },
    /// The WAL is at its threshold: nothing but checkpoints may write.
    WalPressure,
}

impl Ran {
    fn of(result: Result<(usize, bool), Bypass>) -> Result<Self, Bypass> {
        match result {
            Ok((rows, finished)) => Ok(Self::Batch { rows, finished }),
            Err(Bypass::WalPressure) => Ok(Self::WalPressure),
            Err(bypass) => Err(bypass),
        }
    }
}

impl Maintenance {
    pub fn new(limits: MaintenanceLimits) -> Result<Self, MaintenanceLimitsError> {
        limits.validate()?;
        Ok(Self {
            limits,
            phase: Phase::Hints,
        })
    }

    pub fn limits(&self) -> MaintenanceLimits {
        self.limits
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Run phases from [`Self::phase`], one bounded transaction at a time,
    /// until the cycle is done, `budget` is spent or `cancel` is set
    /// (checked before every transaction). Takes `maintenance.lock`
    /// without waiting and skips the step when another holder has it. At
    /// the WAL threshold only a checkpoint runs. A bypass leaves the phase
    /// where it was, so the next step resumes there.
    ///
    /// Below the WAL threshold every step renews this store's lease when
    /// due, a skipped one too. A process must renew at least every
    /// `LEASE_EXPIRY - LEASE_RENEWAL`, through a step, an admitted write or
    /// [`Store::renew_lease`], to keep its lease and its snapshot holds.
    pub fn step(
        &mut self,
        store: &Store,
        budget: StepBudget,
        cancel: &AtomicBool,
    ) -> Result<StepReport, Bypass> {
        store.guarded(Access::Maintenance, || self.run(store, budget, cancel))
    }

    fn run(
        &mut self,
        store: &Store,
        budget: StepBudget,
        cancel: &AtomicBool,
    ) -> Result<StepReport, Bypass> {
        let started = Instant::now();
        let mut report = StepReport {
            skipped: false,
            cancelled: false,
            phase: self.phase,
            rows: 0,
            cycle_complete: false,
            pressure: store.pressure()?,
        };
        let Some(_lock) = MaintenanceLock::try_take(store.location())? else {
            // The holder renews its own lease, not this one (spec §9).
            if report.pressure != Pressure::WalPressure {
                store.renew_lease()?;
            }
            report.skipped = true;
            return Ok(report);
        };
        if report.pressure == Pressure::WalPressure {
            store.checkpoint()?;
            report.pressure = store.pressure()?;
            if report.pressure == Pressure::WalPressure {
                return Ok(report);
            }
        }
        // Wherever the cycle resumes, the lease stays fresh.
        store.renew_lease()?;
        if self.phase == Phase::Done {
            self.phase = Phase::Hints;
        }
        loop {
            report.phase = self.phase;
            if self.phase == Phase::Done {
                report.cycle_complete = true;
                break;
            }
            if cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                break;
            }
            if report.rows >= budget.rows || started.elapsed() >= budget.time {
                break;
            }
            match self.run_phase(store, budget.rows - report.rows)? {
                Ran::Batch { rows, finished } => {
                    report.rows += rows;
                    if finished {
                        self.phase = self.phase.next();
                    }
                }
                Ran::WalPressure => {
                    store.checkpoint()?;
                    report.pressure = store.pressure()?;
                    if report.pressure == Pressure::WalPressure {
                        break;
                    }
                }
            }
        }
        Ok(report)
    }

    /// One transaction of the current phase, changing at most `left` rows
    /// (`left >= 1`).
    fn run_phase(&self, store: &Store, left: usize) -> Result<Ran, Bypass> {
        let limits = MaintenanceLimits {
            batch_rows: self.limits.batch_rows.min(left),
            vacuum_pages: self.limits.vacuum_pages.min(left),
            ..self.limits
        };
        let batch = limits.batch_rows;
        let retire = |step| {
            Ran::of(
                store
                    .retire_step(step, &limits)
                    .map(|retired| (retired, retired < batch)),
            )
        };
        // Phases whose writes do not check the WAL themselves.
        let unpressured =
            || -> Result<bool, Bypass> { Ok(store.pressure()? != Pressure::WalPressure) };
        match self.phase {
            Phase::Hints => Ran::of(if unpressured()? {
                store.flush_hints().map(|_| (0, true))
            } else {
                Err(Bypass::WalPressure)
            }),
            Phase::Leases => Ran::of(if unpressured()? {
                store.renew_lease().and_then(|()| {
                    let expired = store.expire_leases(batch)?;
                    Ok((expired, expired < batch))
                })
            } else {
                Err(Bypass::WalPressure)
            }),
            Phase::Checkpoint => Ran::of(store.checkpoint().map(|_| (0, true))),
            Phase::ObsoleteNamespaces => Ran::of(if unpressured()? {
                store.retire_obsolete().map(|_| (0, true))
            } else {
                Err(Bypass::WalPressure)
            }),
            Phase::Delete => Ran::of(
                store
                    .reclaim(batch)
                    .map(|reclaimed| (reclaimed.rows, reclaimed.complete)),
            ),
            Phase::Vacuum => Ran::of(
                store
                    .vacuum_step(&limits)
                    .map(|pages| (pages, pages < limits.vacuum_pages)),
            ),
            Phase::Done => Ok(Ran::Batch {
                rows: 0,
                finished: true,
            }),
            Phase::AbandonedBuilds => retire(RetireStep::AbandonedBuilds),
            Phase::UnusedSnapshots => retire(RetireStep::UnusedSnapshots),
            Phase::Records => retire(RetireStep::Records),
            Phase::Evidence => retire(RetireStep::Evidence),
            Phase::Artifacts => retire(RetireStep::Artifacts),
        }
    }
}

/// `maintenance.lock`, held for one step and taken without waiting
/// (spec §9 amendment 2026-11-03).
pub(crate) struct MaintenanceLock(#[allow(dead_code)] std::fs::File);

impl MaintenanceLock {
    pub(crate) fn try_take(location: &StoreLocation) -> Result<Option<Self>, Bypass> {
        super::count_operation();
        let file = open_lock_file(&location.maintenance_lock())?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(Bypass::Io(error.kind())),
        }
    }
}

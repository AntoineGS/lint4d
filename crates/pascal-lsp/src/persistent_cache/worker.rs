//! The `pascal-cache` thread (D23): opens and reopens the store, retries
//! opens whose bypass advises it (D24), and runs bounded maintenance steps
//! while the interactive gate is idle.

use super::{CacheState, Inner, PersistentCache, StatusCause, status};
use crate::workspace::admitted::{AdmittedStore, StoreBypass};
use pascal_cache::{Bypass, Maintenance, MaintenanceLimits, Pressure, StepBudget, StepReport};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(super) const STEP_BUDGET: StepBudget = StepBudget {
    time: Duration::from_millis(50),
    rows: 256,
};
pub(super) const IDLE_INTERVAL: Duration = Duration::from_secs(60);
pub(super) const PRESSURE_INTERVAL: Duration = Duration::from_secs(1);
/// A step deferred by interactive work is tried again this soon.
const GATE_RETRY: Duration = Duration::from_millis(25);
/// Past this since the last completed step (or the open), a step runs
/// whatever the gate says, so the lease is renewed (D23); the step budget
/// bounds what it takes from interactive work.
pub(super) const RENEWAL_DEADLINE: Duration = Duration::from_secs(120);
const TRACE_INTERVAL: Duration = Duration::from_secs(600);

pub(super) type StepResult = Result<StepReport, Bypass>;

pub(super) enum Command {
    Open,
    /// Run one step now, gate ignored, and reply (tests).
    StepNow(crossbeam_channel::Sender<Option<StepResult>>),
    Shutdown,
}

/// The delay before the next scheduled step, given the last one.
pub(super) fn next_delay(last: Option<&StepResult>) -> Duration {
    match last {
        None => Duration::ZERO,
        Some(Ok(report)) if report.skipped || report.cancelled => IDLE_INTERVAL,
        Some(Ok(report)) if report.pressure == Pressure::WalPressure => PRESSURE_INTERVAL,
        Some(Ok(report)) if !report.cycle_complete => Duration::ZERO,
        Some(Ok(report)) if report.pressure != Pressure::None => PRESSURE_INTERVAL,
        Some(_) => IDLE_INTERVAL,
    }
}

/// Whether a scheduled step runs now: when the gate is idle, or when the
/// lease renewal is overdue.
pub(super) fn should_step(gate_idle: bool, since_last: Duration) -> bool {
    gate_idle || since_last >= RENEWAL_DEADLINE
}

/// The maintenance cycle's position, kept for one generation.
pub(super) type Cursor = (u64, Maintenance);

pub(super) fn new_cursor() -> Cursor {
    (
        0,
        Maintenance::new(MaintenanceLimits::default()).expect("default limits"),
    )
}

/// One maintenance step under a job, `None` when no job is admitted. The
/// job is held for the whole store access (usage reading included) and
/// dropped on return.
pub(super) fn step(inner: &Arc<Inner>, cursor: &mut Cursor) -> Option<StepResult> {
    let job = inner.job()?;
    if cursor.0 != job.generation() {
        *cursor = (job.generation(), new_cursor().1);
    }
    let result = cursor.1.step(job.store(), STEP_BUDGET, job.cancel_flag());
    match &result {
        Ok(report) => {
            job.counters().record_step(report);
            if !job.is_cancelled() {
                match status::UsageView::read(job.store()) {
                    Ok(usage) => inner.record_usage(job.generation(), usage),
                    Err(bypass) => job.observe(&bypass),
                }
            }
        }
        Err(bypass) => job.observe(bypass),
    }
    Some(result)
}

/// The scheduled path: never waits at the gate. A busy gate defers the
/// step (`None`, no job taken) unless the renewal is overdue.
pub(super) fn scheduled_step(
    inner: &Arc<Inner>,
    cursor: &mut Cursor,
    since_last: Duration,
) -> Option<StepResult> {
    let gate_idle = inner.gate.try_wait_idle(&AtomicBool::new(false));
    if !should_step(gate_idle, since_last) {
        return None;
    }
    step(inner, cursor)
}

/// The worker's maintenance timetable for the current `Enabled`
/// generation.
struct Schedule {
    generation: u64,
    due: Instant,
    /// The last completed step, or the open.
    renewed: Instant,
    traced: Option<(Instant, String)>,
}

impl Schedule {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            generation: 0,
            due: now,
            renewed: now,
            traced: None,
        }
    }

    /// When the next step is due, `None` unless maintenance is scheduled
    /// and the state is `Enabled`. A new generation starts afresh.
    fn due(&mut self, inner: &Inner) -> Option<Instant> {
        if !inner.schedule_maintenance {
            return None;
        }
        let generation = {
            let shared = inner.lock();
            if shared.state != CacheState::Enabled || shared.shut_down {
                return None;
            }
            shared.generation
        };
        if generation != self.generation {
            let now = Instant::now();
            self.generation = generation;
            self.due = now;
            self.renewed = now;
        }
        Some(self.due)
    }

    fn run(&mut self, inner: &Arc<Inner>, cursor: &mut Cursor) {
        let since_last = Instant::now().saturating_duration_since(self.renewed);
        let Some(result) = scheduled_step(inner, cursor, since_last) else {
            // Deferred by the gate, or no job is admitted; a state change
            // re-arms through `due`.
            self.due = Instant::now() + GATE_RETRY;
            return;
        };
        let now = Instant::now();
        if matches!(&result, Ok(report) if !report.cancelled) {
            self.renewed = now;
        }
        self.due = now + next_delay(Some(&result));
        self.trace(inner, now);
    }

    /// At most every [`TRACE_INTERVAL`], and only when the status changed.
    fn trace(&mut self, inner: &Arc<Inner>, now: Instant) {
        if !crate::trace::enabled()
            || self
                .traced
                .as_ref()
                .is_some_and(|(at, _)| now.duration_since(*at) < TRACE_INTERVAL)
        {
            return;
        }
        let handle = PersistentCache {
            inner: Some(Arc::clone(inner)),
        };
        let json = handle.status().to_json().to_string();
        if self.traced.as_ref().is_some_and(|(_, last)| *last == json) {
            return;
        }
        crate::trace::trace!("cache-status {json}");
        self.traced = Some((now, json));
    }
}

/// Holds a `Weak<Inner>`: dropping the last handle drops the sender, which
/// ends the loop.
pub(super) fn spawn(inner: &Arc<Inner>) -> (crossbeam_channel::Sender<Command>, JoinHandle<()>) {
    let (sender, commands) = crossbeam_channel::unbounded();
    let weak = Arc::downgrade(inner);
    let handle = std::thread::Builder::new()
        .name("pascal-cache".into())
        .spawn(move || run(&weak, &commands))
        .expect("spawn the pascal-cache thread");
    (sender, handle)
}

fn run(weak: &Weak<Inner>, commands: &crossbeam_channel::Receiver<Command>) {
    let mut cursor = new_cursor();
    let mut schedule = Schedule::new();
    loop {
        let (timer, step_timer) = match weak.upgrade() {
            Some(inner) => (
                retry_timer(&inner),
                schedule
                    .due(&inner)
                    .map_or_else(crossbeam_channel::never, crossbeam_channel::at),
            ),
            None => return,
        };
        crossbeam_channel::select! {
            recv(commands) -> command => match command {
                Ok(Command::Open) => {}
                Ok(Command::StepNow(reply)) => {
                    let result = weak.upgrade().and_then(|inner| step(&inner, &mut cursor));
                    let _ = reply.send(result);
                }
                Ok(Command::Shutdown) | Err(_) => return,
            },
            recv(timer) -> _ => {
                if let Some(inner) = weak.upgrade() {
                    retry_due(&inner);
                }
            },
            recv(step_timer) -> _ => {
                if let Some(inner) = weak.upgrade() {
                    schedule.run(&inner, &mut cursor);
                }
            },
        }
        let Some(inner) = weak.upgrade() else {
            return;
        };
        // Transitions made on this thread never send to it: open whenever
        // the state asks for it.
        while open(&inner) {}
    }
}

fn retry_timer(inner: &Inner) -> crossbeam_channel::Receiver<Instant> {
    let shared = inner.lock();
    match shared.retry {
        Some((generation, due)) if generation == shared.generation => crossbeam_channel::at(due),
        _ => crossbeam_channel::never(),
    }
}

/// The retry timer fired: leave the bypass for `Opening` unless anything
/// moved the state since it was armed.
fn retry_due(inner: &Arc<Inner>) {
    let mut shared = inner.lock();
    let Some((generation, due)) = shared.retry else {
        return;
    };
    if generation != shared.generation
        || !matches!(shared.state, CacheState::Bypassed(_))
        || shared.shut_down
        || Instant::now() < due
    {
        return;
    }
    shared.retry = None;
    shared.set_state(CacheState::Opening);
    inner.changed.notify_all();
}

/// One open attempt, counted as an admitted job. Returns whether the state
/// is `Opening` again afterwards (a fence finished into a reopen).
fn open(inner: &Arc<Inner>) -> bool {
    let (generation, directory, limits) = {
        let mut shared = inner.lock();
        if shared.state != CacheState::Opening || shared.shut_down {
            return false;
        }
        shared.active += 1;
        (
            shared.generation,
            shared.directory.clone(),
            shared.options.limits(),
        )
    };
    #[cfg(test)]
    if let Some((entered, release)) = super::lock(&inner.open_hook).take() {
        let _ = entered.send(());
        let _ = release.recv();
    }
    let opened = directory
        .ok_or(())
        .or_else(|()| pascal_cache::default_directory())
        .map_err(StoreBypass::Store)
        .and_then(|directory| {
            AdmittedStore::open(&inner.project_cache, &directory, &inner.namespace, limits)
        });
    let mut shared = inner.lock();
    if shared.state != CacheState::Opening || shared.generation != generation {
        // Fenced meanwhile: close what was opened before acknowledging.
        drop(shared);
        drop(opened);
        let mut shared = inner.lock();
        shared.active -= 1;
        inner.finish_fence(&mut shared);
        let reopen = shared.state == CacheState::Opening;
        inner.unlock(shared);
        inner.changed.notify_all();
        return reopen;
    }
    shared.active -= 1;
    match opened {
        Ok(store) => {
            shared.reset_epoch = Some(store.scope().reset_epoch);
            shared.store = Some(Arc::new(store));
            shared.cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            shared.retry_delay = None;
            shared.set_state(CacheState::Enabled);
        }
        Err(StoreBypass::Memory) => shared.set_state(CacheState::Bypassed(StatusCause::Memory)),
        Err(StoreBypass::Store(bypass)) => {
            inner.counters.record_bypass(&bypass);
            let retry = retries(&bypass);
            shared.set_state(CacheState::Bypassed(StatusCause::Store(bypass)));
            if retry {
                let delay = retry_delay(shared.retry_delay);
                shared.retry_delay = Some(delay);
                shared.retry = Some((shared.generation, Instant::now() + delay));
            }
        }
    }
    inner.unlock(shared);
    inner.changed.notify_all();
    false
}

pub(super) fn retries(bypass: &Bypass) -> bool {
    pascal_cache::admin::advice(bypass) == pascal_cache::admin::RecoveryAdvice::Retry
}

pub(super) fn retry_delay(previous: Option<Duration>) -> Duration {
    previous.map_or(pascal_cache::MIN_HEALTH_BACKOFF, |delay| {
        delay
            .saturating_mul(2)
            .min(pascal_cache::MAX_HEALTH_BACKOFF)
    })
}

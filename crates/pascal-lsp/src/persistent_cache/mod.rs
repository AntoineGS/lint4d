//! The process's persistent-cache handle (spec §3, §10, §11 amendments
//! 2026-11-03, `l4d-zf1b.11`).
//!
//! One [`AdmittedStore`] per process, opened by the `pascal-cache` worker
//! thread. Analysis takes a [`CacheJob`] while the state is `Enabled`.
//! Disabling or reopening fences the generation: no new job, admitted jobs
//! are cancelled, and the state moves on only once every admitted job (an
//! in-flight open included) has finished and the store is closed (D21, D22).
//! Reaching `Disabled` is the acknowledgement; no store IO follows it.
//!
//! Locking: `Inner::shared` is never held while a store is opened or
//! dropped, nor while waiting on anything but its own condition variable.
//! Commands go over an unbounded channel. Callers of
//! [`PersistentCache::reconfigure`], [`PersistentCache::readmit`] and
//! [`PersistentCache::shutdown`] (the main loop) never hold a [`CacheJob`]:
//! the wait would wait for itself, which is why every wait is bounded.

pub(crate) mod counters;
pub(crate) mod exclusion;
pub(crate) mod names;
pub(crate) mod options;
pub(crate) mod status;
mod worker;

pub(crate) use counters::Counters;
pub(crate) use exclusion::CacheExclusion;
pub(crate) use options::{
    CacheOptions, CacheOptionsOverride, CacheOptionsUpdate, parse_runtime_cache_options,
};
pub(crate) use status::{CacheStatus, StatusCause};

use crate::project_cache::ProjectCache;
use crate::warmer::InteractiveGate;
use crate::workspace::admitted::AdmittedStore;
use crate::workspace::persist::{PersistError, Reuse};
use pascal_cache::{Bypass, MAX_OPEN_WAIT, Namespace, Store, StoreLimits};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{JoinHandle, ThreadId};
use std::time::{Duration, Instant};

/// A repeated bypass is written to stderr at most this often.
const LOG_REPEAT_INTERVAL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CacheState {
    Disabled,
    Opening,
    Enabled,
    /// No new job; admitted jobs are finishing. Becomes `Shared::after`.
    Fencing,
    Bypassed(StatusCause),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ack {
    /// The transition finished (for a disable: no cache IO follows).
    Done,
    /// An admitted job outlived the wait; its drop finishes the transition.
    Pending,
}

/// The process's handle. `Default` is inert: no store, no thread, no IO.
#[derive(Clone, Default)]
pub(crate) struct PersistentCache {
    inner: Option<Arc<Inner>>,
}

impl std::fmt::Debug for PersistentCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PersistentCache")
            .field("inert", &self.inner.is_none())
            .finish_non_exhaustive()
    }
}

pub(super) struct Inner {
    shared: Mutex<Shared>,
    changed: Condvar,
    counters: Counters,
    project_cache: ProjectCache,
    gate: Arc<InteractiveGate>,
    namespace: Namespace,
    /// `false` only in unit tests, which step maintenance by hand (Task 6
    /// honours it).
    schedule_maintenance: bool,
    /// A wake is on its way to the worker (one at a time).
    wake_pending: AtomicBool,
    /// Kept apart from `Shared` (which holds the command sender) so
    /// `shutdown` joins with no lock held.
    join: Mutex<Option<JoinHandle<()>>>,
    #[cfg(test)]
    open_hook: Mutex<Option<OpenHook>>,
    /// Jobs this handle admitted (a handle-local count of store work).
    #[cfg(test)]
    jobs_issued: std::sync::atomic::AtomicUsize,
    /// Test seams for index batches under store pressure (Task 11).
    #[cfg(test)]
    seams: Mutex<IndexSeams>,
}

#[cfg(test)]
pub(crate) type PublishFault = Box<dyn FnMut(&std::path::Path) -> Option<Bypass> + Send>;

/// What an index batch sees instead of the store, in tests.
#[cfg(test)]
#[derive(Default)]
struct IndexSeams {
    /// The store's pressure, given the waits so far.
    pressure: Option<Box<dyn FnMut(u64) -> pascal_cache::Pressure + Send>>,
    /// A bypass for the publication of an extracted member.
    publish: Option<PublishFault>,
    /// `await_maintenance` calls.
    waits: u64,
}

/// Test seam: the worker sends on the first and blocks on the second after
/// taking the open's admitted count and before `AdmittedStore::open`.
#[cfg(test)]
type OpenHook = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

pub(super) struct Shared {
    state: CacheState,
    /// What `Fencing` becomes: `Disabled`, or `Opening` for a reopen.
    after: CacheState,
    generation: u64,
    options: CacheOptions,
    /// Resolved once at start (environment only, no IO); `None` when the
    /// platform has no default, so the open bypasses with that error.
    directory: Option<PathBuf>,
    /// `directory` while the options are active and it holds no Pascal
    /// sources: what the workspace leaves out (l4d-vrw0).
    exclusion: Option<CacheExclusion>,
    store: Option<Arc<AdmittedStore>>,
    cancel: Arc<AtomicBool>,
    /// Admitted jobs, an in-flight open and a store being closed included.
    active: usize,
    /// Written by the worker after each step (no IO when read).
    usage: Option<status::UsageView>,
    reset_epoch: Option<u64>,
    shut_down: bool,
    commands: Option<crossbeam_channel::Sender<worker::Command>>,
    worker_thread: Option<ThreadId>,
    /// Maintenance steps the worker completed (any generation).
    steps: u64,
    /// Steps the worker began; each step is numbered by this count.
    steps_begun: u64,
    /// The number of the last step that completed.
    last_step_done: u64,
    /// The last retry delay, doubled by each retried bypass.
    retry_delay: Option<Duration>,
    /// A retry due for the `Bypassed` state of this generation.
    retry: Option<(u64, Instant)>,
    logged: [Option<Instant>; names::BYPASS_KINDS],
    /// Transition lines, written by [`Inner::unlock`] once `shared` is
    /// released (a client not draining stderr must not block the lock).
    log: Vec<String>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn new(options: CacheOptions) -> Self {
        let directory = options
            .directory
            .clone()
            .or_else(|| pascal_cache::default_directory().ok());
        let mut exclusion = None;
        let state = if let Some(message) = &options.invalid {
            CacheState::Bypassed(StatusCause::Configuration(message.clone()))
        } else if !options.enabled {
            CacheState::Disabled
        } else {
            match directory.as_deref().map(dedicated).transpose() {
                Ok(dedicated) => {
                    exclusion = dedicated;
                    CacheState::Opening
                }
                Err(message) => CacheState::Bypassed(StatusCause::Configuration(message)),
            }
        };
        let mut shared = Self {
            state,
            after: CacheState::Disabled,
            generation: 0,
            options,
            directory,
            exclusion,
            store: None,
            cancel: Arc::new(AtomicBool::new(false)),
            active: 0,
            usage: None,
            reset_epoch: None,
            shut_down: false,
            commands: None,
            worker_thread: None,
            steps: 0,
            steps_begun: 0,
            last_step_done: 0,
            retry_delay: None,
            retry: None,
            logged: [None; names::BYPASS_KINDS],
            log: Vec::new(),
        };
        if shared.options.invalid.is_none() && matches!(shared.state, CacheState::Bypassed(_)) {
            shared.log_transition();
        }
        shared
    }

    /// The status name: `Fencing` is named by its target.
    fn state_name(&self) -> &'static str {
        match (&self.state, &self.after) {
            (CacheState::Disabled, _) => "disabled",
            (CacheState::Opening, _) => "opening",
            (CacheState::Enabled, _) => "enabled",
            (CacheState::Fencing, CacheState::Opening) => "reopening",
            (CacheState::Fencing, _) => "disabling",
            (CacheState::Bypassed(_), _) => "bypassed",
        }
    }

    /// Every transition: a new generation; leaving `Enabled` drops the
    /// usage reading.
    fn set_state(&mut self, state: CacheState) {
        if self.state == CacheState::Enabled {
            self.usage = None;
        }
        self.state = state;
        self.generation += 1;
        self.log_transition();
    }

    /// Queue one stderr line for each settled state; a repeated bypass at
    /// most once per [`LOG_REPEAT_INTERVAL`].
    fn log_transition(&mut self) {
        let cause = match &self.state {
            CacheState::Opening | CacheState::Fencing => return,
            CacheState::Disabled | CacheState::Enabled => String::new(),
            CacheState::Bypassed(StatusCause::Configuration(message)) => {
                format!(": configuration: {message}")
            }
            CacheState::Bypassed(StatusCause::Memory) => ": memory".to_owned(),
            CacheState::Bypassed(StatusCause::Store(bypass)) => {
                let now = Instant::now();
                let last = &mut self.logged[names::bypass_index(bypass)];
                if last.is_some_and(|last| now.duration_since(last) < LOG_REPEAT_INTERVAL) {
                    return;
                }
                *last = Some(now);
                match bypass {
                    Bypass::Location(issue) => format!(
                        ": {} ({})",
                        names::bypass_name(bypass),
                        names::location_issue_name(issue)
                    ),
                    _ => format!(": {}", names::bypass_name(bypass)),
                }
            }
        };
        let line = format!("pascal-lsp: persistent cache {}{cause}", self.state_name());
        self.log.push(line);
    }
}

/// The exclusion of a store directory that holds no Pascal sources, or why
/// it is refused (walks the directory: never under `Inner::shared`).
///
/// The directory and its missing parents are created here, before the
/// workspace takes the exclusion, so the first open changes nothing a
/// workspace walk can see. A location the store refuses is left for the
/// open to report.
fn dedicated(directory: &std::path::Path) -> Result<CacheExclusion, String> {
    exclusion::check_dedicated(directory)?;
    let _ = pascal_cache::StoreLocation::prepare(directory);
    Ok(CacheExclusion::new(directory))
}

impl Inner {
    fn new(
        options: CacheOptions,
        project_cache: ProjectCache,
        gate: Arc<InteractiveGate>,
        namespace: Namespace,
        schedule_maintenance: bool,
    ) -> Self {
        Self {
            shared: Mutex::new(Shared::new(options)),
            changed: Condvar::new(),
            counters: Counters::default(),
            project_cache,
            gate,
            namespace,
            schedule_maintenance,
            wake_pending: AtomicBool::new(false),
            join: Mutex::new(None),
            #[cfg(test)]
            open_hook: Mutex::new(None),
            #[cfg(test)]
            jobs_issued: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            seams: Mutex::new(IndexSeams::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        lock(&self.shared)
    }

    /// Start the worker unless one runs or the handle shut down.
    fn ensure_worker(self: &Arc<Self>, shared: &mut Shared) {
        if shared.commands.is_some() || shared.shut_down {
            return;
        }
        let (commands, handle) = worker::spawn(self);
        shared.worker_thread = Some(handle.thread().id());
        shared.commands = Some(commands);
        *lock(&self.join) = Some(handle);
    }

    /// Ask the worker to open. The worker itself never sends: it checks
    /// for `Opening` after every event.
    fn request_open(self: &Arc<Self>, shared: &mut Shared) {
        self.ensure_worker(shared);
        if shared.worker_thread == Some(std::thread::current().id()) {
            return;
        }
        if let Some(commands) = &shared.commands {
            // Unbounded: never blocks under the lock.
            let _ = commands.send(worker::Command::Open);
        }
    }

    /// Fence the generation: no new job, admitted work cancelled. Returns
    /// the store for the caller to drop after unlocking, counted in
    /// `active` until [`Inner::release`] (R1: nothing closes after the
    /// acknowledgement).
    fn begin_fence(&self, shared: &mut Shared, after: CacheState) -> Option<Arc<AdmittedStore>> {
        let after = if shared.shut_down {
            CacheState::Disabled
        } else {
            after
        };
        match shared.state {
            CacheState::Enabled | CacheState::Opening => {
                shared.after = after;
                shared.set_state(CacheState::Fencing);
                shared.cancel.store(true, Ordering::Relaxed);
                shared.retry = None;
                let store = shared.store.take();
                if store.is_some() {
                    shared.active += 1;
                }
                self.changed.notify_all();
                store
            }
            CacheState::Fencing => {
                shared.after = after;
                None
            }
            CacheState::Disabled | CacheState::Bypassed(_) => None,
        }
    }

    /// Finish the fence once nothing admitted is left.
    fn finish_fence(self: &Arc<Self>, shared: &mut Shared) {
        if shared.state != CacheState::Fencing || shared.active != 0 || shared.store.is_some() {
            return;
        }
        let after = if shared.shut_down {
            CacheState::Disabled
        } else {
            std::mem::replace(&mut shared.after, CacheState::Disabled)
        };
        shared.cancel = Arc::new(AtomicBool::new(false));
        shared.set_state(after);
        if shared.state == CacheState::Opening {
            self.request_open(shared);
        }
        self.changed.notify_all();
    }

    /// Release `shared`, then write the transition lines it queued.
    fn unlock(&self, mut shared: MutexGuard<'_, Shared>) {
        let lines = std::mem::take(&mut shared.log);
        drop(shared);
        for line in lines {
            eprintln!("{line}");
        }
    }

    /// One admitted operation ended (its store reference already dropped).
    fn release(self: &Arc<Self>) {
        let mut shared = self.lock();
        shared.active -= 1;
        self.finish_fence(&mut shared);
        self.unlock(shared);
        self.changed.notify_all();
    }

    /// After [`Inner::begin_fence`]: drop the store it handed out with no
    /// lock held, then finish the fence. Always called, so a fence that
    /// holds nothing (no job, no store, an open not yet claimed by the
    /// worker) finishes here.
    fn close(self: &Arc<Self>, store: Option<Arc<AdmittedStore>>) {
        if let Some(store) = store {
            drop(store);
            self.release();
        } else {
            let mut shared = self.lock();
            self.finish_fence(&mut shared);
            self.unlock(shared);
        }
    }

    /// Keep the worker's usage reading for the status, unless the
    /// generation moved on or left `Enabled` meanwhile.
    /// Ask the worker for a scheduled step now: writes were refused for
    /// room only maintenance makes (a checkpoint, reclaimed rows). Only
    /// the cache's own schedule steps; without one this does nothing.
    fn wake_maintenance(&self) {
        if !self.schedule_maintenance || self.wake_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let shared = self.lock();
        // The worker's own step schedules its next one itself.
        let sent = shared.worker_thread != Some(std::thread::current().id())
            && shared
                .commands
                .as_ref()
                .is_some_and(|commands| commands.send(worker::Command::Wake).is_ok());
        drop(shared);
        if !sent {
            self.wake_pending.store(false, Ordering::Release);
        }
    }

    /// A maintenance step begins; returns its number.
    fn note_step_begun(&self) -> u64 {
        let mut shared = self.lock();
        shared.steps_begun += 1;
        shared.steps_begun
    }

    /// Step `number` finished.
    fn note_step(&self, number: u64) {
        let mut shared = self.lock();
        shared.steps += 1;
        shared.last_step_done = shared.last_step_done.max(number);
        drop(shared);
        self.changed.notify_all();
    }

    fn record_usage(&self, generation: u64, usage: status::UsageView) {
        let mut shared = self.lock();
        if shared.generation == generation && shared.state == CacheState::Enabled {
            shared.usage = Some(usage);
        }
    }

    pub(super) fn job(self: &Arc<Self>) -> Option<CacheJob> {
        let mut shared = self.lock();
        if shared.state != CacheState::Enabled {
            return None;
        }
        let store = shared.store.clone()?;
        shared.active += 1;
        #[cfg(test)]
        self.jobs_issued.fetch_add(1, Ordering::Relaxed);
        Some(CacheJob {
            inner: Arc::clone(self),
            store: Some(store),
            cancel: Arc::clone(&shared.cancel),
            generation: shared.generation,
        })
    }

    /// Wait up to `timeout` while `waiting` holds; returns the guard.
    fn wait_while<'a>(
        &'a self,
        shared: MutexGuard<'a, Shared>,
        timeout: Duration,
        mut waiting: impl FnMut(&Shared) -> bool,
    ) -> MutexGuard<'a, Shared> {
        let deadline = Instant::now() + timeout;
        let mut shared = shared;
        while waiting(&shared) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            shared = self
                .changed
                .wait_timeout(shared, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        shared
    }

    fn acknowledge(&self, shared: MutexGuard<'_, Shared>, wait: Duration) -> Ack {
        let shared = self.wait_while(shared, wait, |shared| shared.state == CacheState::Fencing);
        if shared.state == CacheState::Fencing {
            Ack::Pending
        } else {
            Ack::Done
        }
    }

    /// Leave `Disabled`/`Bypassed` for `Opening` (never after shutdown).
    fn open_from_rest(self: &Arc<Self>, shared: &mut Shared) {
        if shared.shut_down {
            return;
        }
        shared.retry = None;
        shared.set_state(CacheState::Opening);
        self.request_open(shared);
        self.changed.notify_all();
    }
}

#[cfg(any(test, feature = "test-support"))]
const STEP_NOW_WAIT: Duration = Duration::from_secs(120);

impl PersistentCache {
    pub(crate) fn start(
        options: CacheOptions,
        project_cache: ProjectCache,
        gate: Arc<InteractiveGate>,
    ) -> Self {
        Self::start_with(options, project_cache, gate, Namespace::current().clone())
    }

    pub(crate) fn start_with(
        options: CacheOptions,
        project_cache: ProjectCache,
        gate: Arc<InteractiveGate>,
        namespace: Namespace,
    ) -> Self {
        Self::launch(Inner::new(options, project_cache, gate, namespace, true))
    }

    fn launch(inner: Inner) -> Self {
        let inner = Arc::new(inner);
        let mut shared = inner.lock();
        if shared.state == CacheState::Opening {
            inner.request_open(&mut shared);
        }
        inner.unlock(shared);
        Self { inner: Some(inner) }
    }

    #[cfg(test)]
    pub(crate) fn start_unscheduled_for_test(
        options: CacheOptions,
        project_cache: ProjectCache,
        gate: Arc<InteractiveGate>,
        namespace: Namespace,
    ) -> Self {
        Self::launch(Inner::new(options, project_cache, gate, namespace, false))
    }

    #[cfg(test)]
    pub(crate) fn start_paused_for_test(
        options: CacheOptions,
        project_cache: ProjectCache,
        gate: Arc<InteractiveGate>,
        namespace: Namespace,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> Self {
        let inner = Inner::new(options, project_cache, gate, namespace, false);
        *lock(&inner.open_hook) = Some((entered, release));
        Self::launch(inner)
    }

    #[cfg(test)]
    pub(crate) fn set_pressure_for_test(
        &self,
        pressure: Option<Box<dyn FnMut(u64) -> pascal_cache::Pressure + Send>>,
    ) {
        if let Some(inner) = &self.inner {
            lock(&inner.seams).pressure = pressure;
        }
    }

    /// The pressure the seam reports, if one is set.
    #[cfg(test)]
    pub(crate) fn pressure_for_test(&self) -> Option<pascal_cache::Pressure> {
        let inner = self.inner.as_ref()?;
        let mut seams = lock(&inner.seams);
        let waits = seams.waits;
        seams.pressure.as_mut().map(|pressure| pressure(waits))
    }

    #[cfg(test)]
    pub(crate) fn set_publish_fault_for_test(&self, fault: Option<PublishFault>) {
        if let Some(inner) = &self.inner {
            lock(&inner.seams).publish = fault;
        }
    }

    #[cfg(test)]
    pub(crate) fn publish_fault_for_test(&self, path: &std::path::Path) -> Option<Bypass> {
        let inner = self.inner.as_ref()?;
        lock(&inner.seams)
            .publish
            .as_mut()
            .and_then(|fault| fault(path))
    }

    #[cfg(test)]
    pub(crate) fn room_waits_for_test(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |inner| lock(&inner.seams).waits)
    }

    #[cfg(test)]
    pub(crate) fn has_worker_for_test(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.lock().commands.is_some())
    }

    /// Wait at most `timeout` while the state is `Opening` or `Fencing`,
    /// returning early once `cancel` is set; returns the state.
    pub(crate) fn wait_settled(&self, timeout: Duration, cancel: &AtomicBool) -> CacheState {
        const SLICE: Duration = Duration::from_millis(25);
        let Some(inner) = &self.inner else {
            return CacheState::Disabled;
        };
        let unsettled =
            |shared: &Shared| matches!(shared.state, CacheState::Opening | CacheState::Fencing);
        let deadline = Instant::now() + timeout;
        let mut shared = inner.lock();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if !unsettled(&shared) || left.is_zero() || cancel.load(Ordering::Relaxed) {
                return shared.state.clone();
            }
            shared = inner.wait_while(shared, left.min(SLICE), unsettled);
        }
    }

    /// A job while `Enabled`; `None` otherwise (and always when inert).
    pub(crate) fn job(&self) -> Option<CacheJob> {
        self.inner.as_ref()?.job()
    }

    /// Whether `job` would admit one now; no job is taken.
    pub(crate) fn is_enabled(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.lock().state == CacheState::Enabled)
    }

    /// The jobs this handle has admitted since it started.
    #[cfg(test)]
    pub(crate) fn jobs_issued_for_test(&self) -> usize {
        self.inner
            .as_ref()
            .map_or(0, |inner| inner.jobs_issued.load(Ordering::Relaxed))
    }

    /// Apply runtime options, waiting at most [`MAX_OPEN_WAIT`] for a
    /// fence to finish.
    pub(crate) fn reconfigure(&self, options: CacheOptions) -> Ack {
        self.reconfigure_within(options, MAX_OPEN_WAIT)
    }

    /// Apply runtime options. The directory and an invalid initialization
    /// value come from the start options and never change; while that value
    /// is invalid this is a no-op. Must not be called while holding a job.
    pub(crate) fn reconfigure_within(&self, options: CacheOptions, wait: Duration) -> Ack {
        let Some(inner) = &self.inner else {
            return Ack::Done;
        };
        // Enabling without an exclusion checks the directory first, with no
        // lock held, whatever the state (a disable may still be fencing).
        let (directory, unchecked) = {
            let shared = inner.lock();
            (shared.directory.clone(), shared.exclusion.is_none())
        };
        let checked = (unchecked && options.enabled)
            .then(|| directory.as_deref().map(dedicated))
            .flatten();
        let mut shared = inner.lock();
        if shared.options.invalid.is_some() {
            return Ack::Done;
        }
        let options = CacheOptions {
            directory: shared.options.directory.clone(),
            invalid: None,
            ..options
        };
        let old = std::mem::replace(&mut shared.options, options);
        let new = &shared.options;
        let (active, limits_changed) = (new.active(), new.limits() != old.limits());
        let mut store = None;
        let state = shared.state.clone();
        let mut refused = None;
        if !active {
            shared.exclusion = None;
        } else if shared.exclusion.is_none() {
            match checked {
                Some(Ok(exclusion)) => shared.exclusion = Some(exclusion),
                Some(Err(message)) => refused = Some(message),
                None => {}
            }
        }
        let refusal =
            refused.map(|message| CacheState::Bypassed(StatusCause::Configuration(message)));
        match (&state, active) {
            (CacheState::Enabled | CacheState::Opening, false) => {
                store = inner.begin_fence(&mut shared, CacheState::Disabled);
            }
            (CacheState::Fencing, false) => shared.after = CacheState::Disabled,
            (CacheState::Bypassed(_), false) => {
                shared.retry = None;
                shared.set_state(CacheState::Disabled);
                inner.changed.notify_all();
            }
            (CacheState::Disabled, false) => {}
            (CacheState::Disabled | CacheState::Bypassed(_), true) => match refusal {
                Some(refusal) => {
                    shared.retry = None;
                    shared.set_state(refusal);
                    inner.changed.notify_all();
                }
                None => inner.open_from_rest(&mut shared),
            },
            (CacheState::Enabled | CacheState::Opening, true) if refusal.is_some() => {
                store = inner.begin_fence(&mut shared, refusal.expect("guarded"));
            }
            (CacheState::Enabled | CacheState::Opening, true) if limits_changed => {
                store = inner.begin_fence(&mut shared, CacheState::Opening);
            }
            (CacheState::Enabled | CacheState::Opening, true) => {}
            (CacheState::Fencing, true) => {
                if !shared.shut_down {
                    shared.after = refusal.unwrap_or(CacheState::Opening);
                }
            }
        }
        inner.unlock(shared);
        inner.close(store);
        inner.acknowledge(inner.lock(), wait)
    }

    /// Admit the store again against the current `maxCacheBytes` (spec
    /// §11.1 amendment): reopen if open, retry a memory or store bypass.
    /// Must not be called while holding a job.
    pub(crate) fn readmit(&self) -> Ack {
        let Some(inner) = &self.inner else {
            return Ack::Done;
        };
        let mut shared = inner.lock();
        let mut store = None;
        let state = shared.state.clone();
        match &state {
            CacheState::Enabled | CacheState::Opening => {
                store = inner.begin_fence(&mut shared, CacheState::Opening);
            }
            CacheState::Bypassed(StatusCause::Memory | StatusCause::Store(_))
                if shared.options.active() =>
            {
                inner.open_from_rest(&mut shared);
            }
            CacheState::Fencing | CacheState::Disabled | CacheState::Bypassed(_) => {}
        }
        inner.unlock(shared);
        inner.close(store);
        inner.acknowledge(inner.lock(), MAX_OPEN_WAIT)
    }

    /// Close the store (waiting at most [`MAX_OPEN_WAIT`] for admitted
    /// jobs) and stop the worker. Idempotent; nothing reopens afterwards.
    pub(crate) fn shutdown(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut shared = inner.lock();
        shared.shut_down = true;
        shared.retry = None;
        let store = inner.begin_fence(&mut shared, CacheState::Disabled);
        inner.unlock(shared);
        inner.close(store);
        let mut shared = inner.wait_while(inner.lock(), MAX_OPEN_WAIT, |shared| {
            shared.state == CacheState::Fencing
        });
        let commands = shared.commands.take();
        inner.unlock(shared);
        if let Some(commands) = commands {
            let _ = commands.send(worker::Command::Shutdown);
        }
        let handle = lock(&inner.join).take();
        if let Some(handle) = handle
            && handle.thread().id() != std::thread::current().id()
        {
            let _ = handle.join();
        }
    }

    /// A guard that shuts the handle down when dropped.
    pub(crate) fn close_on_drop(&self) -> CloseOnDrop {
        CloseOnDrop(self.clone())
    }

    /// The store directory the workspace leaves out: `Some` while the
    /// options are active and the directory holds no Pascal sources.
    /// A Pascal source or project file turned up under the excluded
    /// directory: refuse it rather than hide the file. The store is fenced
    /// into the configuration bypass; a later enable checks again.
    pub(crate) fn refuse_sources(&self, path: &std::path::Path) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut shared = inner.lock();
        let Some(directory) = shared.directory.clone() else {
            return;
        };
        shared.exclusion = None;
        let refusal = CacheState::Bypassed(StatusCause::Configuration(exclusion::sources_message(
            &directory, path,
        )));
        let mut store = None;
        match shared.state {
            CacheState::Enabled | CacheState::Opening => {
                store = inner.begin_fence(&mut shared, refusal);
            }
            CacheState::Fencing => shared.after = refusal,
            CacheState::Disabled | CacheState::Bypassed(_) => {
                if shared.options.active() {
                    shared.retry = None;
                    shared.set_state(refusal);
                    inner.changed.notify_all();
                }
            }
        }
        inner.unlock(shared);
        inner.close(store);
    }

    pub(crate) fn exclusion(&self) -> Option<CacheExclusion> {
        self.inner.as_ref()?.lock().exclusion.clone()
    }

    pub(crate) fn state(&self) -> CacheState {
        self.inner
            .as_ref()
            .map_or(CacheState::Disabled, |inner| inner.lock().state.clone())
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |inner| inner.lock().generation)
    }

    #[cfg(test)]
    pub(crate) fn counters(&self) -> Option<&Counters> {
        self.inner.as_ref().map(|inner| &inner.counters)
    }

    /// The bounded status document's fields. No store call.
    pub(crate) fn status(&self) -> CacheStatus {
        let lock_waits = pascal_cache::lock_waits();
        let Some(inner) = &self.inner else {
            return CacheStatus {
                state: "disabled",
                generation: 0,
                enabled: false,
                cause: None,
                directory: None,
                limits: StoreLimits::default(),
                reset_epoch: None,
                reserved: status::Reserved {
                    pages: 0,
                    scratch: 0,
                    facts: 0,
                    publication: 0,
                },
                usage: None,
                lock_waits,
                counters: counters::CounterSnapshot::default(),
            };
        };
        let stats = inner.project_cache.stats();
        let shared = inner.lock();
        let enabled = shared.state == CacheState::Enabled;
        CacheStatus {
            state: shared.state_name(),
            generation: shared.generation,
            enabled: shared.options.active(),
            cause: match &shared.state {
                CacheState::Bypassed(cause) => Some(cause.clone()),
                _ => None,
            },
            directory: shared.directory.clone(),
            limits: shared.options.limits(),
            reset_epoch: shared.reset_epoch.filter(|_| enabled),
            reserved: status::Reserved {
                pages: stats.reserved_pages_bytes,
                scratch: stats.reserved_scratch_bytes,
                facts: stats.reserved_facts_bytes,
                publication: stats.reserved_publication_bytes,
            },
            usage: shared.usage.filter(|_| enabled),
            lock_waits,
            counters: inner.counters.snapshot(),
        }
    }

    /// Run one maintenance step on the worker, gate ignored. `None` without
    /// a worker or without a job. The wait is a backstop against a dead
    /// worker, long enough that a loaded machine never reaches it.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn step_now(&self) -> Option<worker::StepResult> {
        let inner = self.inner.as_ref()?;
        let (reply, result) = crossbeam_channel::bounded(1);
        inner
            .lock()
            .commands
            .as_ref()?
            .send(worker::Command::StepNow(reply))
            .ok()?;
        result.recv_timeout(STEP_NOW_WAIT).ok().flatten()
    }

    /// The scheduled step's path (gate check included, never waiting) on
    /// the calling thread, with a fresh cycle and no renewal overdue.
    #[cfg(test)]
    pub(crate) fn scheduled_step_for_test(&self) -> Option<worker::StepResult> {
        let inner = self.inner.as_ref()?;
        worker::scheduled_step(inner, &mut worker::new_cursor(), Duration::ZERO)
    }

    /// Wake the scheduled maintenance and wait at most `timeout` for its
    /// next step to finish, calling `check` between waits (an `Err` ends
    /// the wait with it). `Ok(false)` without a schedule or when no step
    /// finished in time. The caller then reads the store's pressure.
    pub(crate) fn await_maintenance(
        &self,
        timeout: Duration,
        check: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<bool, String> {
        const SLICE: Duration = Duration::from_millis(25);
        #[cfg(test)]
        if let Some(inner) = &self.inner {
            lock(&inner.seams).waits += 1;
        }
        let Some(inner) = self
            .inner
            .as_ref()
            .filter(|inner| inner.schedule_maintenance)
        else {
            return Ok(false);
        };
        // A step already running may have read the pressure before the
        // refusal: only one that begins after the wake counts.
        let before = inner.lock().steps_begun;
        inner.wake_maintenance();
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let shared = inner.wait_while(inner.lock(), left.min(SLICE), |shared| {
                shared.last_step_done <= before && !shared.shut_down
            });
            if shared.last_step_done > before {
                return Ok(true);
            }
            if shared.shut_down || left.is_zero() {
                return Ok(false);
            }
            drop(shared);
            check()?;
        }
    }

    /// Wait while the state is `Opening` or `Fencing`; returns the state.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn settle(&self, timeout: Duration) -> CacheState {
        let Some(inner) = &self.inner else {
            return CacheState::Disabled;
        };
        let shared = inner.wait_while(inner.lock(), timeout, |shared| {
            matches!(shared.state, CacheState::Opening | CacheState::Fencing)
        });
        shared.state.clone()
    }

    /// Wait up to `timeout` for `predicate` to hold for the state.
    #[cfg(test)]
    pub(crate) fn wait_for(
        &self,
        mut predicate: impl FnMut(&CacheState) -> bool,
        timeout: Duration,
    ) -> bool {
        let Some(inner) = &self.inner else {
            return predicate(&CacheState::Disabled);
        };
        let shared = inner.wait_while(inner.lock(), timeout, |shared| !predicate(&shared.state));
        predicate(&shared.state)
    }
}

/// Shuts the handle down when dropped (the server's exit paths).
pub(crate) struct CloseOnDrop(PersistentCache);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

/// One admitted operation on the store. While it lives the generation's
/// fence cannot finish; dropping it releases the store reference first,
/// then the admitted count.
///
/// Analysis (`l4d-zf1b.8`) takes one with `workspace.persistent_cache().job()`,
/// polls [`Self::cancel_flag`], calls [`Self::observe`] for a store bypass it
/// gets outside reuse, and [`Self::record_reuse`] after every
/// `reuse_persisted_facts` (which counts that outcome's bypass itself).
pub(crate) struct CacheJob {
    inner: Arc<Inner>,
    store: Option<Arc<AdmittedStore>>,
    cancel: Arc<AtomicBool>,
    generation: u64,
}

impl std::fmt::Debug for CacheJob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CacheJob")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl CacheJob {
    pub(crate) fn store(&self) -> &Store {
        self.store.as_deref().expect("held until drop")
    }

    /// Set when the generation is fenced: stop and roll back.
    pub(crate) fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn counters(&self) -> &Counters {
        &self.inner.counters
    }

    /// Report a bypass the store returned. `Fenced` (D24) fences this
    /// generation and reopens once the last job drops; it is counted only
    /// when it does so, so jobs racing on one reset count it once.
    pub(crate) fn observe(&self, bypass: &Bypass) {
        if *bypass != Bypass::Fenced {
            self.inner.counters.record_bypass(bypass);
            // Only maintenance makes this room: step soon, not at the next
            // idle interval.
            if matches!(bypass, Bypass::WalPressure | Bypass::OverBudget) {
                self.inner.wake_maintenance();
            }
            return;
        }
        let mut shared = self.inner.lock();
        if shared.generation != self.generation || shared.state != CacheState::Enabled {
            return;
        }
        self.inner.counters.record_bypass(bypass);
        let store = self.inner.begin_fence(&mut shared, CacheState::Opening);
        self.inner.unlock(shared);
        self.inner.close(store);
    }

    /// Record one `reuse_persisted_facts` outcome: hits and misses are
    /// counted here, a store bypass goes through [`Self::observe`] (so it
    /// is counted once and `Fenced` reopens). Call it after every reuse.
    pub(crate) fn record_reuse(&self, result: &Result<Reuse, PersistError>) {
        let counters = &self.inner.counters;
        match result {
            Ok(Reuse::Hit { facts, .. }) => counters.record_hit(facts.charge.bytes() as u64),
            Ok(Reuse::Miss(miss)) => counters.record_miss(miss.cause()),
            Err(error) => match error.cause() {
                Ok(cause) => counters.record_miss(cause),
                Err(bypass) => self.observe(bypass),
            },
        }
    }
}

impl Drop for CacheJob {
    fn drop(&mut self) {
        drop(self.store.take());
        self.inner.release();
    }
}

#[cfg(all(test, unix))]
mod tests;

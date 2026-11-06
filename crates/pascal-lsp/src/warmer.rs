use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use lsp_server::{Message, Request, RequestId, Response};
use lsp_types::{ProgressToken, Url};

use crate::persistent_cache::CacheState;
use crate::project_cache::IndexAnchor;
use crate::workspace::Workspace;
use crate::workspace::baseline::SealOutcome;
use crate::workspace::indexing::{IndexBound, IndexMode, IndexProgress};
use crate::workspace::rename::WorkspaceInput;

/// Pending members one warmer extraction batch publishes before it yields.
#[cfg(test)]
const INDEX_BATCH_UNITS: usize = crate::workspace::indexing::EXTRACTION_BATCH_UNITS;
/// Index anchors the warmer holds; further requests are dropped.
const MAX_INDEX_ANCHORS: usize = 16;
/// Seals a store bypass retired that an anchor tries again in a row.
const MAX_SEAL_RETRIES: usize = 3;
/// Batches an anchor is asked for again, in a row, for members refused for
/// room while the store's pressure outlasts their waits (each waits for a
/// maintenance step, which paces them).
const MAX_ROOM_RETRIES: usize = 3;
/// Reuse batches an anchor runs again, in a row, that met only retryable
/// misses (a busy store, a refused decode reservation) before its pass ends.
const MAX_REUSE_RETRIES: usize = 3;

#[derive(Default)]
pub(crate) struct InteractiveGate {
    busy: Mutex<usize>,
    idle: Condvar,
    /// Queued Interactive and Bulk client requests, which wait for a worker
    /// and could take a lent slot (queued pull diagnostics are not counted).
    waiting_clients: std::sync::atomic::AtomicUsize,
    /// Background jobs paused in `yield_to_interactive_work`. Their workers
    /// use no CPU meanwhile, so the dispatcher lends their slots to client
    /// requests.
    paused: std::sync::atomic::AtomicUsize,
    /// Running Bulk client requests; index batches wait for them (D33).
    running_bulk: std::sync::atomic::AtomicUsize,
    /// Test seam: after this many passing `try_wait_idle` or
    /// `index_may_run` checks, mark the gate busy so a pause lands at an
    /// exact unit boundary.
    #[cfg(test)]
    busy_after_checks: Mutex<Option<usize>>,
    /// Test seam: as `busy_after_checks`, but a client request queues.
    #[cfg(test)]
    waiting_after_checks: Mutex<Option<usize>>,
    /// Test seam: as `busy_after_checks`, but running Bulk work ends.
    #[cfg(test)]
    bulk_ends_after_checks: Mutex<Option<usize>>,
    /// Test seam: the next seal after a complete batch finds the gate busy.
    #[cfg(test)]
    busy_before_seal: AtomicBool,
    /// Test seam: the next seal after a complete batch finds the store
    /// `Busy`.
    #[cfg(test)]
    busy_store_before_seal: AtomicBool,
    #[cfg(test)]
    index_turn_waiters: std::sync::atomic::AtomicUsize,
    /// Test seam: `wait_index_turn` waits as if the gate did not pass.
    #[cfg(test)]
    hold_index_turns: AtomicBool,
    /// Test seam: every crawl slice is past `CRAWL_SLICE`.
    #[cfg(test)]
    pub(crate) crawl_slice_over: AtomicBool,
}

/// How long a crawl that cannot publish analyses units past its last yield
/// before it yields again to one queued extraction batch (R8-2/D42). Units a
/// re-dispatched crawl replays as cache hits do not count.
const CRAWL_SLICE: Duration = Duration::from_secs(2);

/// What `wait_index_turn` decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexTurn {
    Go,
    /// A crawl is queued: the batch stops, its anchor requeued.
    Yield,
    Cancelled,
}

impl InteractiveGate {
    pub(crate) fn set(&self, busy: usize) {
        *self.busy.lock().unwrap_or_else(PoisonError::into_inner) = busy;
        if busy == 0 {
            self.idle.notify_all();
        }
    }

    /// Returns `false` if cancelled while interactive work was pending.
    pub(crate) fn wait_idle(&self, cancel: &AtomicBool) -> bool {
        let mut busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        while *busy > 0 {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            busy = self
                .idle
                .wait_timeout(busy, Duration::from_millis(25))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        !cancel.load(Ordering::Relaxed)
    }

    /// Records how many client requests wait for a worker. Background work
    /// pauses for them as for interactive work, and lends them its slot.
    pub(crate) fn set_waiting_clients(&self, waiting: usize) {
        self.waiting_clients.store(waiting, Ordering::Relaxed);
        if waiting == 0 {
            self.idle.notify_all();
        }
    }

    /// Records how many Bulk client requests run. Index batches wait for
    /// them, the crawl does not.
    pub(crate) fn set_running_bulk(&self, running: usize) {
        self.running_bulk.store(running, Ordering::Relaxed);
        if running == 0 {
            // Under the lock, so a waiter between its check and its wait
            // cannot miss the notification.
            let _busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
            self.idle.notify_all();
        }
    }

    /// Whether a Bulk client request runs: index batches wait for it, and
    /// the crawl goes first meanwhile.
    pub(crate) fn bulk_runs(&self) -> bool {
        self.running_bulk.load(Ordering::Relaxed) > 0
    }

    fn index_may_run_with(&self, busy: usize) -> bool {
        self.background_may_run(busy) && self.running_bulk.load(Ordering::Relaxed) == 0
    }

    /// Whether an index batch may take its next step: no interactive request
    /// runs or is queued, no client request waits for a worker and no Bulk
    /// request runs (D33).
    pub(crate) fn index_may_run(&self, cancel: &AtomicBool) -> bool {
        self.tick_test_seams();
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let busy = *self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        self.index_may_run_with(busy) && !cancel.load(Ordering::Relaxed)
    }

    /// `index_may_run` without cancellation and without counting a test
    /// seam check: whether the warmer may dispatch an index batch now.
    pub(crate) fn index_may_run_now(&self) -> bool {
        let busy = *self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        self.index_may_run_with(busy)
    }

    /// Waits, without a time limit, until `index_may_run` would pass. The
    /// warmer has one thread, so a batch does not wait while a crawl is
    /// queued: it yields instead, and the crawl runs under its own gate. A
    /// `Full` batch yields to a queued crawl at once, a `Reuse` one only
    /// while Bulk work holds it back (D33). Interactive work pauses the crawl
    /// too, so a reuse batch waits for it in front of the crawl.
    pub(crate) fn wait_index_turn(
        &self,
        cancel: &AtomicBool,
        crawl_pending: &AtomicBool,
        mode: IndexMode,
    ) -> IndexTurn {
        let mut busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        #[cfg(test)]
        self.index_turn_waiters.fetch_add(1, Ordering::Relaxed);
        let turn = loop {
            if cancel.load(Ordering::Relaxed) {
                break IndexTurn::Cancelled;
            }
            let crawl_queued = crawl_pending.load(Ordering::Relaxed);
            if mode == IndexMode::Full && crawl_queued {
                break IndexTurn::Yield;
            }
            #[cfg(test)]
            let held = self.hold_index_turns.load(Ordering::Relaxed);
            #[cfg(not(test))]
            let held = false;
            if !held && self.index_may_run_with(*busy) {
                break IndexTurn::Go;
            }
            if crawl_queued && self.bulk_runs() {
                break IndexTurn::Yield;
            }
            busy = self
                .idle
                .wait_timeout(busy, Duration::from_millis(25))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        #[cfg(test)]
        self.index_turn_waiters.fetch_sub(1, Ordering::Relaxed);
        turn
    }

    #[cfg(test)]
    pub(crate) fn index_turn_waiters(&self) -> usize {
        self.index_turn_waiters.load(Ordering::Relaxed)
    }

    /// Counts one passing check against the test seams.
    fn tick_test_seams(&self) {
        #[cfg(test)]
        {
            let tick = |slot: &Mutex<Option<usize>>| {
                let mut remaining = slot.lock().unwrap_or_else(PoisonError::into_inner);
                match remaining.as_mut() {
                    Some(0) => {
                        *remaining = None;
                        true
                    }
                    Some(checks) => {
                        *checks -= 1;
                        false
                    }
                    None => false,
                }
            };
            if tick(&self.busy_after_checks) {
                self.set(1);
            }
            if tick(&self.waiting_after_checks) {
                self.set_waiting_clients(1);
            }
            if tick(&self.bulk_ends_after_checks) {
                self.set_running_bulk(0);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn set_paused_background_for_tests(&self, paused: usize) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn waiting_clients_for_tests(&self) -> usize {
        self.waiting_clients.load(Ordering::Relaxed)
    }

    /// The number of background jobs paused for interactive work.
    pub(crate) fn paused_background(&self) -> usize {
        self.paused.load(Ordering::Relaxed)
    }

    fn background_may_run(&self, busy: usize) -> bool {
        busy == 0 && self.waiting_clients.load(Ordering::Relaxed) == 0
    }

    /// Waits until no interactive request runs or waits and no client request
    /// waits for a worker, for at most `limit`: true then or after `limit`,
    /// false if cancelled.
    fn wait_background_turn(&self, cancel: &AtomicBool, limit: Duration) -> bool {
        let deadline = Instant::now().checked_add(limit);
        let mut busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        while !self.background_may_run(*busy) {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            let slice = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    remaining.min(Duration::from_millis(25))
                }
                None => Duration::from_millis(25),
            };
            busy = self
                .idle
                .wait_timeout(busy, slice)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        !cancel.load(Ordering::Relaxed)
    }

    /// Checks whether warming may proceed without waiting while it owns a cache claim.
    pub(crate) fn try_wait_idle(&self, cancel: &AtomicBool) -> bool {
        self.tick_test_seams();
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        *self.busy.lock().unwrap_or_else(PoisonError::into_inner) == 0
            && !cancel.load(Ordering::Relaxed)
    }
}

thread_local! {
    static BACKGROUND_GATE: std::cell::RefCell<Option<(Arc<InteractiveGate>, Duration)>> =
        const { std::cell::RefCell::new(None) };
}

/// The longest background work pauses at once for interactive work. It
/// then does one more step and pauses again, so that it slows down to a
/// fraction of its speed while interactive work runs, but a long or held
/// interactive request does not stall it.
pub(crate) const BACKGROUND_PAUSE_LIMIT: Duration = Duration::from_millis(500);

/// Makes the background work on the calling thread pause for interactive
/// work at `yield_to_interactive_work` points, each time for at most the
/// given duration (`None`: it never pauses).
pub(crate) fn set_background_gate_on_this_thread(gate: Option<(Arc<InteractiveGate>, Duration)>) {
    BACKGROUND_GATE.with(|cell| *cell.borrow_mut() = gate);
}

#[cfg(feature = "test-support")]
pub(crate) fn has_background_gate_on_this_thread() -> bool {
    BACKGROUND_GATE.with(|cell| cell.borrow().is_some())
}

/// Waits while interactive requests run or are queued, or other client
/// requests wait for a worker, up to the thread's pause limit, when the
/// calling thread does background work and holds no cache claim (a lookup
/// waiting for one would wait out the pause). False when cancelled.
pub(crate) fn yield_to_interactive_work(cancel: &AtomicBool) -> bool {
    let Some((gate, limit)) = BACKGROUND_GATE.with(|cell| cell.borrow().clone()) else {
        return true;
    };
    if cancel.load(Ordering::Relaxed) {
        return false;
    }
    if crate::project_cache::claims_held_on_this_thread() > 0
        || gate.background_may_run(*gate.busy.lock().unwrap_or_else(PoisonError::into_inner))
    {
        return true;
    }
    let _span = crate::trace::Span::new(|| "background work paused for interactive work".into());
    gate.paused.fetch_add(1, Ordering::Relaxed);
    let resumed = gate.wait_background_turn(cancel, limit);
    gate.paused.fetch_sub(1, Ordering::Relaxed);
    resumed
}

#[derive(Debug, Default)]
pub(crate) struct WarmQueue {
    order: VecDeque<Url>,
    queued: HashSet<Url>,
}

impl WarmQueue {
    pub(crate) fn push_front(&mut self, uri: Url) {
        if !self.queued.insert(uri.clone()) {
            self.order.retain(|queued| queued != &uri);
        }
        self.order.push_front(uri);
    }

    pub(crate) fn remove(&mut self, uri: &Url) {
        if self.queued.remove(uri) {
            self.order.retain(|queued| queued != uri);
        }
    }

    pub(crate) fn pop(&mut self) -> Option<Url> {
        let uri = self.order.pop_front()?;
        self.queued.remove(&uri);
        Some(uri)
    }

    pub(crate) fn clear(&mut self) {
        self.order.clear();
        self.queued.clear();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WarmEvent {
    Begin {
        uri: Url,
        total: usize,
        generation: u64,
        open_epoch: u64,
    },
    Progress {
        uri: Url,
        done: usize,
        total: usize,
        generation: u64,
        open_epoch: u64,
    },
    End {
        uri: Url,
        fingerprint: Option<u64>,
        pins: Vec<(Url, u64)>,
        generation: u64,
        open_epoch: u64,
    },
    Paused {
        uri: Url,
        generation: u64,
        open_epoch: u64,
    },
    /// A crawl stopped at a unit boundary so that other warmer work runs
    /// before it is dispatched again. No client preemption.
    Yielded {
        uri: Url,
        /// The crawl's progress count at the boundary where it yielded.
        done: usize,
        /// To a due reuse pass, which the index gate lets run (D33).
        /// Otherwise the crawl cannot publish (its context came from unsaved
        /// project input) and one queued extraction batch runs after its
        /// `CRAWL_SLICE` (R8-2/D42).
        to_reuse: bool,
        generation: u64,
        open_epoch: u64,
    },
    /// One workspace-index batch finished (its mode is `progress.mode`).
    IndexProgress {
        anchor: IndexAnchor,
        /// Boxed: it carries the context key.
        progress: Box<IndexProgress>,
        /// What sealing the baseline after a complete batch gave.
        seal: Option<SealOutcome>,
        generation: u64,
    },
    /// A workspace-index batch ended without a result: `paused` when
    /// interactive work or cancellation interrupted it (published members
    /// are kept), otherwise it failed.
    IndexStopped {
        anchor: IndexAnchor,
        mode: IndexMode,
        paused: bool,
        generation: u64,
    },
    /// The crawl stored new or changed interface entries, so semantic tokens
    /// computed earlier may now resolve more members.
    ClosureStored {
        uri: Url,
        generation: u64,
        open_epoch: u64,
    },
}

pub(crate) const WARM_CREATE_PREFIX: &str = "pascal-lsp-warm-create-";
const WARM_TOKEN_PREFIX: &str = "pascal-lsp-warm-";

struct PendingWarmCreate {
    uri: Url,
    token: ProgressToken,
    title: String,
    total: usize,
    latest_message: Option<String>,
    report_on_ack: bool,
    completed: bool,
}

pub(crate) struct WarmProgress {
    supported: bool,
    next: u64,
    active: HashMap<Url, ProgressToken>,
    latest_messages: HashMap<Url, String>,
    pending_creates: HashMap<RequestId, PendingWarmCreate>,
}

impl WarmProgress {
    pub(crate) fn new(supported: bool) -> Self {
        Self {
            supported,
            next: 0,
            active: HashMap::new(),
            latest_messages: HashMap::new(),
            pending_creates: HashMap::new(),
        }
    }

    fn title(uri: &Url) -> String {
        let stem = uri
            .to_file_path()
            .ok()
            .and_then(|path| {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| uri.to_string());
        format!("Indexing {stem}")
    }

    #[cfg(test)]
    pub(crate) fn handle(
        &mut self,
        connection: &dyn crate::server::ProtocolSender,
        event: &WarmEvent,
    ) -> Result<(), String> {
        self.handle_with_retry(connection, event, true)
    }

    pub(crate) fn handle_with_retry(
        &mut self,
        connection: &dyn crate::server::ProtocolSender,
        event: &WarmEvent,
        paused_will_retry: bool,
    ) -> Result<(), String> {
        if !self.supported {
            return Ok(());
        }
        match event {
            WarmEvent::Begin { uri, total, .. } => {
                if let Some(token) = self.active.get(uri) {
                    let message = self
                        .latest_messages
                        .get(uri)
                        .cloned()
                        .unwrap_or_else(|| format!("0/{total} units"));
                    self.latest_messages.insert(uri.clone(), message.clone());
                    return crate::server::send_progress_report(connection, token, &message);
                }
                if let Some(pending) = self
                    .pending_creates
                    .values_mut()
                    .find(|pending| pending.uri == *uri)
                {
                    // The create response gates progress notifications. Retain
                    // the retry report here and emit it immediately after the
                    // original begin once the client accepts the token.
                    pending
                        .latest_message
                        .get_or_insert_with(|| format!("0/{total} units"));
                    pending.report_on_ack = true;
                    return Ok(());
                }

                self.next = self
                    .next
                    .checked_add(1)
                    .ok_or_else(|| "warm progress token space exhausted".to_string())?;
                let token = ProgressToken::String(format!("{WARM_TOKEN_PREFIX}{}", self.next));
                let id = RequestId::from(format!("{WARM_CREATE_PREFIX}{}", self.next));
                let pending = PendingWarmCreate {
                    uri: uri.clone(),
                    token: token.clone(),
                    title: Self::title(uri),
                    total: *total,
                    latest_message: None,
                    report_on_ack: false,
                    completed: false,
                };
                self.pending_creates.insert(id.clone(), pending);
                let request = Request::new(
                    id.clone(),
                    "window/workDoneProgress/create".to_string(),
                    serde_json::json!({"token": token}),
                );
                if let Err(error) = connection.send_control(Message::Request(request)) {
                    self.pending_creates.remove(&id);
                    return Err(error.to_string());
                }
                Ok(())
            }
            WarmEvent::Progress {
                uri, done, total, ..
            } => {
                let message = format!("{done}/{total} units");
                if let Some(token) = self.active.get(uri) {
                    self.latest_messages.insert(uri.clone(), message.clone());
                    crate::server::send_progress_report(connection, token, &message)
                } else if let Some(pending) = self
                    .pending_creates
                    .values_mut()
                    .find(|pending| pending.uri == *uri)
                {
                    pending.latest_message = Some(message);
                    Ok(())
                } else {
                    Ok(())
                }
            }
            WarmEvent::End { uri, .. } => {
                if let Some(pending) = self
                    .pending_creates
                    .values_mut()
                    .find(|pending| pending.uri == *uri)
                {
                    // The create request may still be awaiting its client
                    // response. Preserve the terminal state so its eventual
                    // acknowledgement can receive a balanced begin/end pair.
                    pending.completed = true;
                    return Ok(());
                }
                self.latest_messages.remove(uri);
                match self.active.remove(uri) {
                    Some(token) => crate::server::send_progress_end(connection, &token, None),
                    None => Ok(()),
                }
            }
            WarmEvent::ClosureStored { .. }
            | WarmEvent::Yielded { .. }
            | WarmEvent::IndexProgress { .. }
            | WarmEvent::IndexStopped { .. } => Ok(()),
            WarmEvent::Paused { .. } if paused_will_retry => Ok(()),
            WarmEvent::Paused { uri, .. } => self.close(connection, uri),
        }
    }

    /// Returns `true` when `response` answered one of our create requests.
    pub(crate) fn handle_response(
        &mut self,
        connection: &dyn crate::server::ProtocolSender,
        response: &Response,
    ) -> Result<bool, String> {
        let Some(pending) = self.pending_creates.remove(&response.id) else {
            return Ok(is_warm_create_id(&response.id));
        };
        if response.error.is_some() {
            return Ok(true);
        }

        let initial_message = format!("0/{} units", pending.total);
        crate::server::send_progress_begin(
            connection,
            &pending.token,
            &pending.title,
            &initial_message,
        )?;
        let latest_message = pending.latest_message.unwrap_or(initial_message);
        if pending.report_on_ack || latest_message != format!("0/{} units", pending.total) {
            crate::server::send_progress_report(connection, &pending.token, &latest_message)?;
        }
        if pending.completed {
            crate::server::send_progress_end(connection, &pending.token, None)?;
        } else {
            self.active
                .insert(pending.uri.clone(), pending.token.clone());
            self.latest_messages.insert(pending.uri, latest_message);
        }
        Ok(true)
    }

    /// Ends every active warm progress token and forgets outstanding creates.
    pub(crate) fn end_all(
        &mut self,
        connection: &dyn crate::server::ProtocolSender,
    ) -> Result<(), String> {
        let tokens = self
            .active
            .drain()
            .map(|(_, token)| token)
            .collect::<Vec<_>>();
        self.pending_creates.clear();
        self.latest_messages.clear();
        let mut first_error = None;
        for token in tokens {
            if let Err(error) = crate::server::send_progress_end(connection, &token, None) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub(crate) fn close(
        &mut self,
        connection: &dyn crate::server::ProtocolSender,
        uri: &Url,
    ) -> Result<(), String> {
        self.pending_creates
            .retain(|_, pending| pending.uri != *uri);
        self.latest_messages.remove(uri);
        match self.active.remove(uri) {
            Some(token) => crate::server::send_progress_end(connection, &token, None),
            None => Ok(()),
        }
    }
}

fn is_warm_create_id(id: &RequestId) -> bool {
    serde_json::to_value(id)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .is_some_and(|id| id.starts_with(WARM_CREATE_PREFIX))
}

enum WarmJobKind {
    Crawl {
        uri: Url,
        open_epoch: u64,
        /// Set while a `Full` batch is queued: a crawl that cannot publish
        /// yields to it after `CRAWL_SLICE`.
        extraction_queued: Arc<AtomicBool>,
        /// Set while a reuse pass is due: the crawl yields to it at a unit
        /// boundary where the index gate passes (it was dispatched first
        /// because Bulk work held the pass back).
        reuse_due: Arc<AtomicBool>,
        /// The progress count at the crawl's last yield: it yields again only
        /// past it, so every slice analyses new units.
        resume_after: usize,
    },
    /// The flag is set while an open-file crawl is queued; a `Full` batch
    /// yields to it, a `Reuse` one runs before it by design (D33).
    Index(IndexAnchor, Arc<AtomicBool>, IndexMode),
}

struct WarmJob {
    kind: WarmJobKind,
    /// The session's input; `job_input` derives what the job analyses.
    input: WorkspaceInput,
    /// The store was enabled at dispatch.
    persisting: bool,
    cancel: Arc<AtomicBool>,
    generation: u64,
}

impl WarmJob {
    /// What the job analyses: an index batch reads the disk-only baseline
    /// view while the store is enabled (`persisting`, D31), so it may
    /// publish. Built on the warmer thread: comparing open buffers with
    /// their files reads them.
    fn job_input(&self, persisting: bool) -> std::borrow::Cow<'_, WorkspaceInput> {
        match self.kind {
            WarmJobKind::Index(..) if persisting => {
                std::borrow::Cow::Owned(self.input.disk_baseline())
            }
            WarmJobKind::Index(..) | WarmJobKind::Crawl { .. } => {
                std::borrow::Cow::Borrowed(&self.input)
            }
        }
    }
}

pub(crate) struct Warmer {
    /// The jobs' gate: an index batch is dispatched only when it passes, so
    /// a waiting batch never holds the one warmer thread.
    gate: Arc<InteractiveGate>,
    queue: WarmQueue,
    /// Workspace-index anchors, project anchors first, deduplicated.
    index_queue: VecDeque<IndexAnchor>,
    /// Anchors whose reuse pass (D33) is due, project anchors first; only
    /// filled while the store is enabled, opening or fencing.
    reuse_queue: VecDeque<IndexAnchor>,
    /// Set while `reuse_queue` holds a pass and the store is not off.
    reuse_due: Arc<AtomicBool>,
    /// Since when the store has been opening or fencing, as of the last
    /// poll: a reuse pass waits for it ahead of the crawl for at most
    /// `MAX_OPEN_WAIT` from then.
    unsettled_since: Option<Instant>,
    /// Anchors whose reuse pass ended; extraction batches take them on.
    reuse_done: HashSet<IndexAnchor>,
    /// Seals of each anchor a store bypass retired in a row.
    seal_retries: HashMap<IndexAnchor, usize>,
    /// Batches of each anchor asked for again in a row for members refused
    /// for room.
    room_retries: HashMap<IndexAnchor, usize>,
    /// Reuse batches of each anchor run again in a row that met only
    /// retryable misses.
    reuse_retries: HashMap<IndexAnchor, usize>,
    /// The index batch in flight.
    index_in_flight: Option<(IndexAnchor, IndexMode)>,
    /// The configured project's absolute path, as of the last poll.
    configured_project: Option<std::path::PathBuf>,
    /// Set while a crawl is queued; a running index batch yields to it.
    crawl_pending: Arc<AtomicBool>,
    /// Set while the store is enabled and a `Full` batch is queued.
    extraction_queued: Arc<AtomicBool>,
    /// A crawl yielded (`WarmEvent::Yielded`): one `Full` batch runs before
    /// it is dispatched again.
    interleave: bool,
    /// Each yielded crawl's progress count at its last yield.
    crawl_resume: HashMap<Url, usize>,
    open_order: VecDeque<Url>,
    jobs: Option<Sender<WarmJob>>,
    events: Receiver<WarmEvent>,
    cancel: Arc<AtomicBool>,
    generation: u64,
    next_open_epoch: u64,
    open_epochs: HashMap<Url, u64>,
    busy: bool,
    busy_attempt: Option<(Url, u64)>,
    /// Cache invalidation epoch at which each in-flight crawl started.
    dispatched_epochs: HashMap<Url, u64>,
    /// Invalidation epoch of each open file's last completed crawl.
    crawled_epochs: HashMap<Url, u64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Warmer {
    pub(crate) fn start(gate: Arc<InteractiveGate>) -> Self {
        let (jobs, job_receiver) = crossbeam_channel::unbounded::<WarmJob>();
        let (event_sender, events) = crossbeam_channel::unbounded();
        let worker_gate = gate.clone();
        let thread = crate::server::analysis_thread("PascalLspWarmer")
            .spawn(move || {
                lower_thread_priority();
                for job in job_receiver {
                    run_job(job, worker_gate.clone(), &event_sender);
                }
            })
            .expect("spawn warmer thread");
        Self {
            gate,
            queue: WarmQueue::default(),
            index_queue: VecDeque::new(),
            reuse_queue: VecDeque::new(),
            reuse_due: Arc::new(AtomicBool::new(false)),
            unsettled_since: None,
            reuse_done: HashSet::new(),
            seal_retries: HashMap::new(),
            room_retries: HashMap::new(),
            reuse_retries: HashMap::new(),
            index_in_flight: None,
            configured_project: None,
            crawl_pending: Arc::new(AtomicBool::new(false)),
            extraction_queued: Arc::new(AtomicBool::new(false)),
            interleave: false,
            crawl_resume: HashMap::new(),
            open_order: VecDeque::new(),
            jobs: Some(jobs),
            events,
            cancel: Arc::new(AtomicBool::new(false)),
            generation: 0,
            next_open_epoch: 0,
            open_epochs: HashMap::new(),
            busy: false,
            busy_attempt: None,
            dispatched_epochs: HashMap::new(),
            crawled_epochs: HashMap::new(),
            thread: Some(thread),
        }
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn open(&mut self, uri: Url) {
        // Keep an identity for the full open lifetime so duplicate didOpen
        // notifications cannot stale an active attempt or its progress token.
        let open_epoch = if let Some(open_epoch) = self.open_epochs.get(&uri).copied() {
            open_epoch
        } else {
            // A missing entry follows a didClose or reset boundary; the server
            // retires progress in those same lifecycle paths before reopening.
            self.next_open_epoch = self
                .next_open_epoch
                .checked_add(1)
                .expect("warm open identity space exhausted");
            self.open_epochs.insert(uri.clone(), self.next_open_epoch);
            self.next_open_epoch
        };
        crate::trace::trace!("warm open {uri} (epoch {open_epoch})");
        self.open_order.retain(|opened| opened != &uri);
        self.open_order.push_front(uri.clone());
        let already_queued = self.queue.queued.contains(&uri);
        let already_in_flight = self
            .busy_attempt
            .as_ref()
            .is_some_and(|(busy_uri, busy_epoch)| busy_uri == &uri && *busy_epoch == open_epoch);
        if already_queued || !already_in_flight {
            self.queue.push_front(uri);
            self.crawl_pending.store(true, Ordering::Relaxed);
        }
    }

    /// Schedules another attempt for an already-open file without changing
    /// its identity. Watcher invalidations are not document reopenings.
    pub(crate) fn rewarm(&mut self, uri: Url) {
        crate::trace::trace!("warm rewarm {uri}");
        self.queue.push_front(uri);
        self.crawl_pending.store(true, Ordering::Relaxed);
    }

    /// Re-crawls an open file whose snapshot found part of its interface
    /// closure uncached, unless it was already crawled, or a crawl is in
    /// flight, since the cache was last invalidated. Otherwise a unit that can
    /// never be cached would trigger a crawl on every request.
    pub(crate) fn request_closure_crawl(
        &mut self,
        uri: Url,
        cache: &crate::project_cache::ProjectCache,
    ) {
        let missed = cache.closure_misses(&uri);
        self.request_closure_crawl_at(uri, cache.invalidation_epoch(), |dispatched| {
            cache.units_invalidated_since(dispatched, &missed)
        });
    }

    /// A crawl in flight covers the request while none of the missed units
    /// was invalidated since it was dispatched, whatever else changed.
    fn request_closure_crawl_at(
        &mut self,
        uri: Url,
        invalidation_epoch: u64,
        invalidated_since: impl FnOnce(u64) -> bool,
    ) {
        let in_flight = self
            .busy_attempt
            .as_ref()
            .is_some_and(|(busy_uri, _)| busy_uri == &uri)
            && self.dispatched_epochs.get(&uri).is_some_and(|&dispatched| {
                dispatched == invalidation_epoch || !invalidated_since(dispatched)
            });
        if in_flight || self.crawled_epochs.get(&uri) == Some(&invalidation_epoch) {
            crate::trace::trace!(
                "warm closure crawl for {uri} skipped (in flight: {in_flight}, epoch {invalidation_epoch})"
            );
            return;
        }
        crate::trace::trace!(
            "warm closure crawl for {uri} requested at epoch {invalidation_epoch} (last crawled at {:?})",
            self.crawled_epochs.get(&uri)
        );
        self.rewarm(uri);
    }

    /// Queues a workspace-index build for `anchor`. Index jobs only run while
    /// no open-file crawl is queued or running.
    pub(crate) fn request_index(&mut self, anchor: IndexAnchor) {
        if self.index_queue.contains(&anchor) {
            return;
        }
        match anchor {
            IndexAnchor::Project(_) => {
                let at = self
                    .index_queue
                    .iter()
                    .take_while(|queued| matches!(queued, IndexAnchor::Project(_)))
                    .count();
                self.index_queue.insert(at, anchor);
            }
            IndexAnchor::Document(_) => self.index_queue.push_back(anchor),
        }
        self.index_queue.truncate(MAX_INDEX_ANCHORS);
    }

    pub(crate) fn close(&mut self, uri: &Url, cache: &crate::project_cache::ProjectCache) {
        let anchor = IndexAnchor::Document(uri.clone());
        self.reuse_queue.retain(|queued| *queued != anchor);
        self.reuse_done.remove(&anchor);
        self.queue.remove(uri);
        self.dispatched_epochs.remove(uri);
        self.crawled_epochs.remove(uri);
        self.crawl_resume.remove(uri);
        self.open_order.retain(|opened| opened != uri);
        self.open_epochs.remove(uri);
        cache.unpin(uri);
        cache.forget_closure_misses(uri);
    }

    /// Project switch: cancel in-flight work and crawl `open` again.
    pub(crate) fn reset(
        &mut self,
        open: impl IntoIterator<Item = Url>,
        cache: &crate::project_cache::ProjectCache,
    ) {
        crate::trace::trace!("warm reset (configuration changed)");
        self.cancel.store(true, Ordering::Relaxed);
        cache.clear_pins();
        self.cancel = Arc::new(AtomicBool::new(false));
        self.generation = self.generation.wrapping_add(1);
        self.queue.clear();
        self.index_queue.clear();
        self.reuse_queue.clear();
        self.reuse_due.store(false, Ordering::Relaxed);
        self.reuse_done.clear();
        self.seal_retries.clear();
        self.room_retries.clear();
        self.reuse_retries.clear();
        self.interleave = false;
        self.crawl_resume.clear();
        self.dispatched_epochs.clear();
        self.crawled_epochs.clear();
        let open = open.into_iter().collect::<HashSet<_>>();
        self.open_order.retain(|uri| open.contains(uri));
        self.open_epochs.retain(|uri, _| open.contains(uri));
        for uri in self.open_order.iter().rev() {
            self.queue.push_front(uri.clone());
            self.crawl_pending.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn is_idle(&self) -> bool {
        !self.busy
            && self.queue.is_empty()
            && self.index_queue.is_empty()
            && self.reuse_queue.is_empty()
    }

    /// The receiver the main loop waits on with the client's lanes, so an
    /// event is handled as soon as it is ready (l4d-axsn).
    pub(crate) fn events(&self) -> &Receiver<WarmEvent> {
        &self.events
    }

    /// Whether the warmer thread still runs (its event channel then stays
    /// connected).
    pub(crate) fn is_running(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }

    /// Whether `uri` is open as far as the warmer knows.
    pub(crate) fn is_open(&self, uri: &Url) -> bool {
        self.open_epochs.contains_key(uri)
    }

    /// Pins a finished crawl's closure within half of `maxCacheBytes`,
    /// unpinning the least recently opened files' closures first
    /// (l4d-axsn). Returns the owners unpinned.
    pub(crate) fn pin_crawl(
        &self,
        cache: &crate::project_cache::ProjectCache,
        uri: &Url,
        pins: Vec<(Url, u64)>,
    ) -> Vec<Url> {
        let oldest_first = self.open_order.iter().rev().cloned().collect::<Vec<_>>();
        let unpinned = cache.pin_capped(uri, pins, &oldest_first);
        for owner in &unpinned {
            crate::trace::trace!("warm pins of {owner} released for {uri} (pin cap)");
        }
        unpinned
    }

    #[cfg(test)]
    pub(crate) fn replace_events_for_test(&mut self, events: Receiver<WarmEvent>) {
        self.events = events;
    }

    #[cfg(test)]
    pub(crate) fn open_epoch_for_test(&self, uri: &Url) -> u64 {
        self.open_epochs[uri]
    }

    #[cfg(test)]
    pub(crate) fn has_index_request_for_test(&self, anchor: &IndexAnchor) -> bool {
        self.index_queue.contains(anchor)
    }

    /// Takes and accepts the finished jobs' events without dispatching; the
    /// main loop calls it while the workspace is with a mutation worker.
    pub(crate) fn drain_events(
        &mut self,
        cache: &crate::project_cache::ProjectCache,
    ) -> Vec<WarmEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            match &event {
                WarmEvent::Progress { .. } => {}
                WarmEvent::End { uri, pins, .. } => crate::trace::trace!(
                    "warm end {uri}: {} pins, cache epoch {}",
                    pins.len(),
                    cache.invalidation_epoch()
                ),
                other => crate::trace::trace!("warm event {other:?}"),
            }
            if self.accept_event(&event, cache) {
                if let WarmEvent::End { uri, .. } = &event {
                    self.request_index(IndexAnchor::Document(uri.clone()));
                    if let Some(project) = self.configured_project.clone() {
                        self.request_index(IndexAnchor::Project(project));
                    }
                }
                events.push(event);
            }
        }
        events
    }

    /// The configured project's anchor, then each open file's, that has not
    /// had its reuse pass, join `reuse_queue` (D33).
    fn queue_reuse_passes(&mut self) {
        let anchors = self
            .configured_project
            .clone()
            .map(IndexAnchor::Project)
            .into_iter()
            .chain(self.open_order.iter().cloned().map(IndexAnchor::Document))
            .collect::<Vec<_>>();
        for anchor in anchors {
            let in_flight = self
                .index_in_flight
                .as_ref()
                .is_some_and(|(running, mode)| *mode == IndexMode::Reuse && *running == anchor);
            if in_flight || self.reuse_done.contains(&anchor) || self.reuse_queue.contains(&anchor)
            {
                continue;
            }
            match anchor {
                IndexAnchor::Project(_) => {
                    let at = self
                        .reuse_queue
                        .iter()
                        .take_while(|queued| matches!(queued, IndexAnchor::Project(_)))
                        .count();
                    self.reuse_queue.insert(at, anchor);
                }
                IndexAnchor::Document(_) => self.reuse_queue.push_back(anchor),
            }
        }
        self.reuse_queue.truncate(MAX_INDEX_ANCHORS);
    }

    fn dispatch_index(
        &mut self,
        workspace: &Workspace,
        anchor: IndexAnchor,
        mode: IndexMode,
        persisting: bool,
        crawl_pending: Arc<AtomicBool>,
    ) {
        let kind = WarmJobKind::Index(anchor.clone(), crawl_pending, mode);
        let job = WarmJob {
            kind,
            input: workspace.analysis_input(),
            persisting,
            cancel: self.cancel.clone(),
            generation: self.generation,
        };
        if self
            .jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(job).is_ok())
        {
            crate::trace::trace!("warm index dispatch {anchor:?} ({mode:?})");
            self.busy = true;
            self.index_in_flight = Some((anchor, mode));
        }
    }

    pub(crate) fn poll(&mut self, workspace: &Workspace) -> Vec<WarmEvent> {
        self.configured_project = workspace.configured_project_path();
        let events = self.drain_events(workspace.project_cache());
        let state = workspace.persistent_cache().state();
        let persisting = state == CacheState::Enabled;
        let settling = matches!(state, CacheState::Opening | CacheState::Fencing);
        if settling {
            self.unsettled_since.get_or_insert_with(Instant::now);
        } else {
            self.unsettled_since = None;
        }
        // A second start usually polls first while the store still opens:
        // the reuse pass is queued anyway, and its batch waits for the store
        // ahead of the crawl, within the store's open bound (D33).
        let awaiting_store = self
            .unsettled_since
            .is_some_and(|since| since.elapsed() < pascal_cache::MAX_OPEN_WAIT);
        if persisting || settling {
            self.queue_reuse_passes();
        } else {
            self.reuse_queue.clear();
        }
        // Index batches publish no parses (A13): they need room for their
        // own charges, not `has_room`, which pins can fill (l4d-axsn).
        // An extraction batch is dispatched only when it can run now; one
        // waiting for its turn would hold the crawl back (one warmer
        // thread).
        let gate = self.gate.clone();
        let index_fits =
            || gate.index_may_run_now() && workspace.project_cache().index_batch_fits();
        // 1. A reuse pass, even while a crawl is queued (D33). Only Bulk work
        // lets the crawl go first: interactive work would pause the crawl as
        // well, so the pass waits for it in `wait_index_turn`.
        if !self.busy
            && (persisting || awaiting_store)
            && !self.reuse_queue.is_empty()
            && !gate.bulk_runs()
            && workspace.project_cache().index_batch_fits()
            && let Some(anchor) = self.reuse_queue.pop_front()
        {
            let pending = self.crawl_pending.clone();
            self.dispatch_index(workspace, anchor, IndexMode::Reuse, persisting, pending);
        }
        self.reuse_due.store(
            (persisting || settling) && !self.reuse_queue.is_empty(),
            Ordering::Relaxed,
        );
        // 1b. One extraction batch after a crawl yielded to it (R8-2/D42),
        // which does not yield to the queued crawl in turn.
        // An interleave the gate holds back stays due; the crawl still runs.
        if !persisting || self.index_queue.is_empty() {
            self.interleave = false;
        }
        if !self.busy
            && self.interleave
            && index_fits()
            && let Some(anchor) = self.index_queue.pop_front()
        {
            self.interleave = false;
            let pending = Arc::new(AtomicBool::new(false));
            self.dispatch_index(workspace, anchor, IndexMode::Full, persisting, pending);
        }
        self.extraction_queued.store(
            persisting && !self.index_queue.is_empty(),
            Ordering::Relaxed,
        );
        // 2. The crawl.
        if !self.busy && workspace.project_cache().has_room() {
            while let Some(uri) = self.queue.pop() {
                let Some(open_epoch) = self.open_epochs.get(&uri).copied() else {
                    continue;
                };
                let busy_uri = uri.clone();
                self.dispatched_epochs.insert(
                    busy_uri.clone(),
                    workspace.project_cache().invalidation_epoch(),
                );
                let kind = WarmJobKind::Crawl {
                    uri,
                    open_epoch,
                    extraction_queued: self.extraction_queued.clone(),
                    reuse_due: self.reuse_due.clone(),
                    resume_after: self.crawl_resume.get(&busy_uri).copied().unwrap_or(0),
                };
                let job = WarmJob {
                    kind,
                    input: workspace.analysis_input(),
                    persisting,
                    cancel: self.cancel.clone(),
                    generation: self.generation,
                };
                if self
                    .jobs
                    .as_ref()
                    .is_some_and(|jobs| jobs.send(job).is_ok())
                {
                    crate::trace::trace!(
                        "warm dispatch {busy_uri} at cache epoch {}",
                        workspace.project_cache().invalidation_epoch()
                    );
                    self.busy = true;
                    self.busy_attempt = Some((busy_uri, open_epoch));
                }
                break;
            }
        }
        if self.queue.is_empty() {
            self.crawl_pending.store(false, Ordering::Relaxed);
        }
        // 3. Extraction, once no crawl is queued.
        if !self.busy
            && self.queue.is_empty()
            && index_fits()
            && let Some(anchor) = self.index_queue.pop_front()
        {
            let pending = self.crawl_pending.clone();
            self.dispatch_index(workspace, anchor, IndexMode::Full, persisting, pending);
        }
        self.extraction_queued.store(
            persisting && !self.index_queue.is_empty(),
            Ordering::Relaxed,
        );
        events
    }

    /// Where a batch of `mode` for `anchor` goes back to: the front of
    /// `reuse_queue` until its pass is done, else the index queue.
    fn requeue_index(&mut self, anchor: &IndexAnchor, mode: IndexMode) {
        match mode {
            IndexMode::Reuse => {
                self.reuse_queue.retain(|queued| queued != anchor);
                self.reuse_queue.push_front(anchor.clone());
                self.reuse_queue.truncate(MAX_INDEX_ANCHORS);
            }
            IndexMode::Full => self.request_index(anchor.clone()),
        }
    }

    fn accept_event(
        &mut self,
        event: &WarmEvent,
        cache: &crate::project_cache::ProjectCache,
    ) -> bool {
        match event {
            WarmEvent::End {
                uri,
                generation,
                open_epoch,
                ..
            } => {
                self.busy = false;
                self.busy_attempt = None;
                let accepted = *generation == self.generation
                    && self.open_epochs.get(uri).copied() == Some(*open_epoch);
                if accepted {
                    self.crawl_resume.remove(uri);
                }
                if accepted && let Some(epoch) = self.dispatched_epochs.remove(uri) {
                    if epoch == cache.invalidation_epoch() {
                        self.crawled_epochs.insert(uri.clone(), epoch);
                    } else {
                        // The cache rejected this crawl's stores once it
                        // was invalidated, so the closure is not settled.
                        cache.forget_closure_misses(uri);
                    }
                }
                accepted
            }
            WarmEvent::Paused {
                uri,
                generation,
                open_epoch,
            } => {
                self.busy = false;
                self.busy_attempt = None;
                if *generation != self.generation
                    || self.open_epochs.get(uri).copied() != Some(*open_epoch)
                {
                    return false;
                }
                self.queue.push_front(uri.clone());
                self.crawl_pending.store(true, Ordering::Relaxed);
                true
            }
            WarmEvent::Yielded {
                uri,
                done,
                to_reuse,
                generation,
                open_epoch,
            } => {
                self.busy = false;
                self.busy_attempt = None;
                if *generation != self.generation
                    || self.open_epochs.get(uri).copied() != Some(*open_epoch)
                {
                    return false;
                }
                self.queue.push_front(uri.clone());
                self.crawl_pending.store(true, Ordering::Relaxed);
                // A yield to the reuse pass owes no extraction batch.
                self.interleave |= !*to_reuse;
                self.crawl_resume.insert(uri.clone(), *done);
                true
            }
            WarmEvent::IndexProgress {
                anchor,
                progress,
                seal,
                generation,
            } => {
                self.busy = false;
                self.index_in_flight = None;
                if *generation != self.generation {
                    return false;
                }
                // A batch that settled nothing and left no untried member
                // (deferred or open-only members), or hit the memory limit,
                // cannot gain from a retry.
                // A reuse pass goes on over a cold or partly filled store
                // until every member was tried (A5): published records are
                // spread over the path order, and an absent miss is cheap.
                let mut requeued = progress.worth_requeue();
                // A reuse batch that met only retryable misses (a busy
                // store, a refused decode reservation) runs again up to
                // `MAX_REUSE_RETRIES` times in a row before its pass ends.
                if progress.mode == IndexMode::Reuse {
                    if requeued {
                        self.reuse_retries.remove(anchor);
                    } else if progress.reuse_deferred > 0
                        && progress.reuse_pending > 0
                        && !progress.memory
                    {
                        let retries = self.reuse_retries.entry(anchor.clone()).or_default();
                        if *retries < MAX_REUSE_RETRIES {
                            *retries += 1;
                            requeued = true;
                        }
                    }
                }
                let settled = progress.indexed
                    + progress.refreshed
                    + progress.refused
                    + progress.limited
                    + progress.reused
                    > 0;
                // Members refused for room hold `complete` back; a batch
                // that found no room settled nothing and is not requeued,
                // so nothing else would ask for the anchor again.
                if progress.complete || settled {
                    self.room_retries.remove(anchor);
                } else if !requeued && progress.awaiting_room > 0 {
                    let retries = self.room_retries.entry(anchor.clone()).or_default();
                    if *retries < MAX_ROOM_RETRIES {
                        *retries += 1;
                        self.request_index(anchor.clone());
                    }
                }
                if requeued {
                    self.requeue_index(anchor, progress.mode);
                } else if progress.mode == IndexMode::Reuse {
                    // The pass is done: extraction follows the crawl.
                    self.reuse_done.insert(anchor.clone());
                    self.reuse_retries.remove(anchor);
                    if !progress.complete {
                        self.request_index(anchor.clone());
                    }
                }
                // A seal stopped by interactive work is tried again, and one
                // a store bypass retired (another process writing: `Busy`)
                // up to `MAX_SEAL_RETRIES` times in a row.
                match seal {
                    Some(SealOutcome::Interrupted) => self.request_index(anchor.clone()),
                    Some(SealOutcome::Bypassed) => {
                        let retries = self.seal_retries.entry(anchor.clone()).or_default();
                        if *retries < MAX_SEAL_RETRIES {
                            *retries += 1;
                            self.request_index(anchor.clone());
                        }
                    }
                    Some(SealOutcome::Sealed | SealOutcome::Unchanged) => {
                        self.seal_retries.remove(anchor);
                    }
                    _ => {}
                }
                true
            }
            WarmEvent::IndexStopped {
                anchor,
                mode,
                paused,
                generation,
            } => {
                self.busy = false;
                self.index_in_flight = None;
                if *generation != self.generation {
                    return false;
                }
                if *paused {
                    self.requeue_index(anchor, *mode);
                } else if *mode == IndexMode::Reuse {
                    // A failed pass is not retried, as a failed Full batch.
                    self.reuse_done.insert(anchor.clone());
                }
                true
            }
            WarmEvent::Begin {
                uri,
                generation,
                open_epoch,
                ..
            }
            | WarmEvent::Progress {
                uri,
                generation,
                open_epoch,
                ..
            }
            | WarmEvent::ClosureStored {
                uri,
                generation,
                open_epoch,
            } => {
                *generation == self.generation
                    && self.open_epochs.get(uri).copied() == Some(*open_epoch)
            }
        }
    }

    /// Stops accepting work, cancels the active job, and joins within two seconds.
    /// Returns `true` when the worker thread was joined successfully.
    pub(crate) fn shutdown(&mut self) -> bool {
        self.cancel.store(true, Ordering::Relaxed);
        drop(self.jobs.take());
        self.queue.clear();
        self.index_queue.clear();
        self.reuse_queue.clear();
        self.reuse_due.store(false, Ordering::Relaxed);
        self.busy = false;
        self.busy_attempt = None;
        self.index_in_flight = None;

        let Some(thread) = self.thread.take() else {
            return true;
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while !thread.is_finished() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
        if thread.is_finished() {
            if thread.join().is_ok() {
                true
            } else {
                eprintln!("pascal-lsp: warning: warmer thread panicked during shutdown");
                false
            }
        } else {
            eprintln!(
                "pascal-lsp: warning: warmer thread did not stop within 2 seconds; detaching it"
            );
            drop(thread);
            false
        }
    }
}

impl Drop for Warmer {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn run_index_job(
    anchor: IndexAnchor,
    crawl_pending: &AtomicBool,
    mode: IndexMode,
    job: &WarmJob,
    gate: &InteractiveGate,
    events: &Sender<WarmEvent>,
) {
    let stopped = |paused| {
        let _ = events.send(WarmEvent::IndexStopped {
            anchor: anchor.clone(),
            mode,
            paused,
            generation: job.generation,
        });
    };
    match gate.wait_index_turn(&job.cancel, crawl_pending, mode) {
        IndexTurn::Go => {}
        IndexTurn::Yield => return stopped(true),
        IndexTurn::Cancelled => return stopped(false),
    }
    // A batch dispatched while the store was opening or reopening would
    // index memory-only, and its members would keep no record for the rest
    // of the process: no seal, extracted again on the next start. Once the
    // store is enabled an extraction batch goes back, and the next poll
    // schedules it as persisting, reuse pass first (D33). A reuse pass,
    // dispatched to wait for the store ahead of the crawl, runs as
    // persisting at once; without an enabled store it goes back, and the
    // next poll drops it or lets the crawl go first.
    let mut persisting = job.persisting;
    if !persisting {
        let settled = job
            .input
            .persistent_cache
            .wait_settled(pascal_cache::MAX_OPEN_WAIT, &job.cancel);
        match mode {
            IndexMode::Full if settled == CacheState::Enabled => return stopped(true),
            IndexMode::Full => {}
            IndexMode::Reuse if settled == CacheState::Enabled => persisting = true,
            IndexMode::Reuse => return stopped(!job.cancel.load(Ordering::Relaxed)),
        }
    }
    let _span = crate::trace::Span::new(|| format!("warm index {anchor:?} ({mode:?})"));
    let started = Instant::now();
    let mut workspace = Workspace::from_analysis_input(&job.job_input(persisting));
    // A reuse pass runs before the crawl by design (D33); extraction yields
    // to a queued crawl.
    let mut before_io = || {
        if (mode == IndexMode::Reuse || !crawl_pending.load(Ordering::Relaxed))
            && gate.index_may_run(&job.cancel)
        {
            Ok(())
        } else {
            Err(crate::workspace::rename::CANCELLATION_MESSAGE.to_string())
        }
    };
    let bound = match mode {
        IndexMode::Reuse => IndexBound::reuse_pass(),
        IndexMode::Full => IndexBound::extraction(),
    };
    let result = workspace.advance_workspace_index(&anchor, bound, &mut before_io, &job.cancel);
    match result {
        Ok(progress) => {
            crate::trace::trace!(
                "index batch {anchor:?} mode={mode:?} indexed={} refreshed={} refused={} limited={} deferred={} skipped_open={} pending={} reused={} reuse_pending={} persisted={} complete={} memory={} elapsed_ms={:.1} reserved_index_bytes={}",
                progress.indexed,
                progress.refreshed,
                progress.refused,
                progress.limited,
                progress.deferred,
                progress.skipped_open,
                progress.pending,
                progress.reused,
                progress.reuse_pending,
                progress.persisted,
                progress.complete,
                progress.memory,
                started.elapsed().as_secs_f64() * 1000.0,
                workspace.project_cache().stats().reserved_index_bytes,
            );
            let seal = progress
                .key
                .as_ref()
                .filter(|_| progress.complete && workspace.persistent_cache().is_enabled())
                .map(|key| {
                    #[cfg(test)]
                    if gate.busy_before_seal.swap(false, Ordering::Relaxed) {
                        gate.set(1);
                    }
                    #[cfg(test)]
                    if gate.busy_store_before_seal.swap(false, Ordering::Relaxed)
                        && let Some(job) = workspace.persistent_cache().job()
                    {
                        job.store().fail_next_for_test(pascal_cache::Bypass::Busy);
                    }
                    let outcome = workspace.seal_baseline_if_due(key, &mut before_io);
                    crate::trace::trace!("index seal {anchor:?}: {outcome:?}");
                    outcome
                });
            let _ = events.send(WarmEvent::IndexProgress {
                anchor,
                progress: Box::new(progress),
                seal,
                generation: job.generation,
            });
        }
        Err(message) if message == crate::workspace::rename::CANCELLATION_MESSAGE => {
            crate::trace::trace!(
                "index batch {anchor:?} mode={mode:?} stopped paused={} elapsed_ms={:.1} reserved_index_bytes={}",
                !job.cancel.load(Ordering::Relaxed),
                started.elapsed().as_secs_f64() * 1000.0,
                workspace.project_cache().stats().reserved_index_bytes,
            );
            stopped(!job.cancel.load(Ordering::Relaxed));
        }
        Err(message) => {
            crate::trace::trace!(
                "index batch {anchor:?} mode={mode:?} stopped error={message} elapsed_ms={:.1} reserved_index_bytes={}",
                started.elapsed().as_secs_f64() * 1000.0,
                workspace.project_cache().stats().reserved_index_bytes,
            );
            stopped(false);
        }
    }
}

fn run_job(job: WarmJob, gate: Arc<InteractiveGate>, events: &Sender<WarmEvent>) {
    let (uri, open_epoch, extraction_queued, reuse_due, resume_after) = match &job.kind {
        WarmJobKind::Crawl {
            uri,
            open_epoch,
            extraction_queued,
            reuse_due,
            resume_after,
        } => (
            uri.clone(),
            *open_epoch,
            extraction_queued.clone(),
            reuse_due.clone(),
            *resume_after,
        ),
        WarmJobKind::Index(anchor, pending, mode) => {
            return run_index_job(anchor.clone(), pending, *mode, &job, &gate, events);
        }
    };
    let end = |fingerprint, pins| {
        let _ = events.send(WarmEvent::End {
            uri: uri.clone(),
            fingerprint,
            pins,
            generation: job.generation,
            open_epoch,
        });
    };
    if !gate.wait_idle(&job.cancel) {
        return end(None, Vec::new());
    }
    crate::project_cache::set_claim_role_on_this_thread(crate::project_cache::ClaimRole::yielding(
        job.cancel.clone(),
    ));
    let _span = crate::trace::Span::new(|| format!("warm crawl {}", uri));
    let mut workspace = Workspace::from_analysis_input(&job.job_input(false));
    // The crawl publishes the disk-proven units it analyses (R8-2/D42). A
    // session's first crawl is usually dispatched while the store still
    // opens: it waits for the store to settle rather than publish nothing.
    let persists = workspace
        .persistent_cache()
        .wait_settled(pascal_cache::MAX_OPEN_WAIT, &job.cancel)
        == CacheState::Enabled;
    workspace.set_crawl_persists(persists);
    let private = workspace.crawl_private_signal();
    // The slice starts at the first unit past the last yield: units replayed
    // as cache hits do not count.
    let slice_started = Mutex::new(None::<Instant>);
    // The progress count where the crawl yielded, and whether to a reuse
    // pass.
    let yielded = Arc::new(Mutex::new(None::<(usize, bool)>));
    let hook_cache = workspace.project_cache().clone();
    let hook_yielded = yielded.clone();
    #[cfg(test)]
    let slice_gate = gate.clone();
    let slice_over = move || {
        let started = *slice_started
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(Instant::now);
        #[cfg(test)]
        if slice_gate.crawl_slice_over.load(Ordering::Relaxed) {
            return true;
        }
        started.elapsed() >= CRAWL_SLICE
    };
    let hook_events = events.clone();
    let hook_uri = uri.clone();
    let hook_generation = job.generation;
    let hook_open_epoch = open_epoch;
    let hook_cancel = job.cancel.clone();
    let hook_gate = gate.clone();
    let paused = Arc::new(AtomicBool::new(false));
    let hook_paused = paused.clone();
    workspace.set_dependency_hook(Arc::new(move |unit, done, total| {
        if done == 0 {
            let _ = hook_events.send(WarmEvent::Begin {
                uri: hook_uri.clone(),
                total,
                generation: hook_generation,
                open_epoch: hook_open_epoch,
            });
        } else {
            let _ = hook_events.send(WarmEvent::Progress {
                uri: hook_uri.clone(),
                done,
                total,
                generation: hook_generation,
                open_epoch: hook_open_epoch,
            });
        }
        crate::trace::trace!("  warm unit {done}/{total} {}", unit_name(unit));
        // Only past the last yield, so every dispatch analyses new units.
        let boundary = persists && done > resume_after && done < total;
        // A reuse pass the crawl went ahead of while Bulk work held it back
        // runs at the first unit boundary where the index gate passes (D33).
        if boundary
            && reuse_due.load(Ordering::Relaxed)
            && hook_gate.index_may_run_now()
            && hook_cache.index_batch_fits()
        {
            *hook_yielded.lock().unwrap_or_else(PoisonError::into_inner) = Some((done, true));
            return Err(crate::workspace::rename::CANCELLATION_MESSAGE.to_string());
        }
        // A crawl that cannot publish lets one queued extraction batch run
        // at the first unit boundary past its slice (R8-2/D42).
        if boundary
            && private.load(Ordering::Relaxed)
            && extraction_queued.load(Ordering::Relaxed)
            && slice_over()
        {
            *hook_yielded.lock().unwrap_or_else(PoisonError::into_inner) = Some((done, false));
            return Err(crate::workspace::rename::CANCELLATION_MESSAGE.to_string());
        }
        // Pause between units while interactive requests run.
        if done == total || hook_gate.try_wait_idle(&hook_cancel) {
            Ok(())
        } else {
            if !hook_cancel.load(Ordering::Relaxed) {
                hook_paused.store(true, Ordering::Relaxed);
            }
            Err(crate::workspace::rename::CANCELLATION_MESSAGE.to_string())
        }
    }));
    let result = workspace.warm_with_cancel(&uri, &job.cancel);
    // Entries stored before a pause or error are already in the shared cache,
    // and a retry will not store them again, so report them now.
    if workspace.interface_entries_stored() > 0 {
        let _ = events.send(WarmEvent::ClosureStored {
            uri: uri.clone(),
            generation: job.generation,
            open_epoch,
        });
    }
    match result {
        Ok(outcome) => {
            let pins = outcome
                .fingerprint
                .map(|fingerprint| {
                    std::iter::once(uri.clone())
                        .chain(outcome.dependencies)
                        .chain(outcome.closure)
                        .map(|uri| (uri, fingerprint))
                        .collect()
                })
                .unwrap_or_default();
            end(outcome.fingerprint, pins)
        }
        Err(_)
            if let Some((done, to_reuse)) =
                *yielded.lock().unwrap_or_else(PoisonError::into_inner)
                && !job.cancel.load(Ordering::Relaxed) =>
        {
            crate::trace::trace!(
                "warm crawl {uri} yields to {} after {done} units",
                if to_reuse {
                    "a reuse pass"
                } else {
                    "extraction"
                }
            );
            let _ = events.send(WarmEvent::Yielded {
                uri,
                done,
                to_reuse,
                generation: job.generation,
                open_epoch,
            });
        }
        Err(_) if paused.load(Ordering::Relaxed) => {
            let _ = events.send(WarmEvent::Paused {
                uri,
                generation: job.generation,
                open_epoch,
            });
        }
        Err(_) => end(None, Vec::new()),
    }
}

fn unit_name(uri: &Url) -> &str {
    uri.path().rsplit('/').next().unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn lower_thread_priority() {
    // SAFETY: gettid and setpriority only affect the calling thread.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
    }
}

#[cfg(not(target_os = "linux"))]
fn lower_thread_priority() {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    fn uri(name: &str) -> Url {
        Url::parse(&format!("file:///ws/{name}")).unwrap()
    }

    fn fixture() -> (tempfile::TempDir, crate::workspace::Workspace, Url) {
        let temp = tempfile::tempdir().unwrap();
        let main_text = "unit Main;\ninterface\nuses Provider;\nimplementation\nend.\n".to_string();
        std::fs::write(temp.path().join("Main.pas"), &main_text).unwrap();
        std::fs::write(
            temp.path().join("Provider.pas"),
            "unit Provider;\ninterface\nprocedure Hello;\nimplementation\nprocedure Hello; begin end;\nend.\n",
        )
        .unwrap();
        let mut workspace = crate::workspace::Workspace::with_override_session(
            vec![temp.path().to_path_buf()],
            Default::default(),
            pascal_project::delphi_overrides::OverrideSession::new(None),
        );
        let main = Url::from_file_path(temp.path().join("Main.pas")).unwrap();
        workspace
            .open_document(main.clone(), main_text, 1)
            .expect("open main unit");
        (temp, workspace, main)
    }

    fn run_until_end(
        warmer: &mut Warmer,
        workspace: &crate::workspace::Workspace,
    ) -> Vec<WarmEvent> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut events = Vec::new();
        while std::time::Instant::now() < deadline {
            events.extend(warmer.poll(workspace));
            if events
                .iter()
                .any(|event| matches!(event, WarmEvent::End { .. }))
            {
                return events;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("warmer did not finish: {events:?}");
    }

    /// `Main` uses `Provider`, whose interface uses `Base`.
    fn closure_fixture() -> (tempfile::TempDir, crate::workspace::Workspace, Url, Url) {
        let temp = tempfile::tempdir().unwrap();
        let main_text = "unit Main;\ninterface\nuses Provider;\nimplementation\nend.\n".to_string();
        std::fs::write(temp.path().join("Main.pas"), &main_text).unwrap();
        std::fs::write(
            temp.path().join("Provider.pas"),
            "unit Provider;\ninterface\nuses Base;\nimplementation\nend.\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("Base.pas"),
            "unit Base;\ninterface\nimplementation\nend.\n",
        )
        .unwrap();
        let mut workspace = crate::workspace::Workspace::with_override_session(
            vec![temp.path().to_path_buf()],
            Default::default(),
            pascal_project::delphi_overrides::OverrideSession::new(None),
        );
        let main = Url::from_file_path(temp.path().join("Main.pas")).unwrap();
        let base = Url::from_file_path(temp.path().join("Base.pas")).unwrap();
        workspace
            .open_document(main.clone(), main_text, 1)
            .expect("open main unit");
        (temp, workspace, main, base)
    }

    #[test]
    fn a_crawl_that_stores_new_interface_entries_reports_them_once() {
        let (_temp, workspace, main, base) = closure_fixture();
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.open(main.clone());

        let first = run_until_end(&mut warmer, &workspace);
        assert!(
            first
                .iter()
                .any(|event| matches!(event, WarmEvent::ClosureStored { .. })),
            "{first:?}"
        );
        let Some(WarmEvent::End { pins, .. }) = first.last() else {
            panic!("{first:?}")
        };
        assert!(
            pins.iter().any(|(uri, _)| uri == &base),
            "closure units are pinned: {pins:?}"
        );

        warmer.rewarm(main);
        let second = run_until_end(&mut warmer, &workspace);
        assert!(
            !second
                .iter()
                .any(|event| matches!(event, WarmEvent::ClosureStored { .. })),
            "{second:?}"
        );
    }

    #[test]
    fn a_paused_crawl_reports_the_interface_entries_it_stored() {
        let (_temp, workspace, main, _base) = closure_fixture();
        let gate = Arc::new(InteractiveGate::default());
        // The first check (before Provider loads) passes; the closure crawl's
        // check, after Main's interface imports were stored, pauses.
        *gate.busy_after_checks.lock().unwrap() = Some(1);
        let mut warmer = Warmer::start(gate.clone());
        warmer.open(main.clone());

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut events = Vec::new();
        while std::time::Instant::now() < deadline
            && !events
                .iter()
                .any(|event| matches!(event, WarmEvent::Paused { .. }))
        {
            events.extend(warmer.poll(&workspace));
            std::thread::sleep(Duration::from_millis(10));
        }

        let stored = events
            .iter()
            .filter(|event| matches!(event, WarmEvent::ClosureStored { .. }))
            .count();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, WarmEvent::Paused { .. })),
            "the crawl never paused: {events:?}"
        );
        assert_eq!(stored, 1, "{events:?}");
        gate.set(0);
    }

    #[test]
    fn closure_crawl_requests_are_ignored_until_the_cache_is_invalidated() {
        let (_temp, workspace, main) = fixture();
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.open(main.clone());
        run_until_end(&mut warmer, &workspace);
        let epoch = workspace.project_cache().invalidation_epoch();

        warmer.request_closure_crawl_at(main.clone(), epoch, |_| true);
        assert!(warmer.queue.is_empty(), "this epoch was already crawled");

        warmer.request_closure_crawl_at(main, epoch + 1, |_| true);
        assert!(
            !warmer.queue.is_empty(),
            "a newer epoch allows another crawl"
        );
    }

    #[test]
    fn closure_crawl_requests_are_ignored_while_that_crawl_is_in_flight() {
        let (_temp, workspace, main) = fixture();
        let gate = Arc::new(InteractiveGate::default());
        gate.set(1);
        let mut warmer = Warmer::start(gate.clone());
        warmer.open(main.clone());
        warmer.poll(&workspace);
        assert!(warmer.queue.is_empty(), "the crawl was dispatched");
        let epoch = workspace.project_cache().invalidation_epoch();

        warmer.request_closure_crawl_at(main.clone(), epoch, |_| true);
        assert!(
            warmer.queue.is_empty(),
            "the in-flight crawl already covers this epoch"
        );

        warmer.request_closure_crawl_at(main, epoch + 1, |_| true);
        assert!(
            !warmer.queue.is_empty(),
            "a newer epoch allows another crawl"
        );
        gate.set(0);
    }

    #[test]
    fn a_crawl_in_flight_survives_invalidations_of_unrelated_files() {
        let (temp, workspace, main) = fixture();
        let cache = workspace.project_cache();
        let gate = Arc::new(InteractiveGate::default());
        gate.set(1);
        let mut warmer = Warmer::start(gate.clone());
        warmer.open(main.clone());
        cache.report_closure_misses(&main, HashSet::from([uri("Sync.pas")]));
        assert_eq!(cache.take_closure_crawl_requests(), vec![main.clone()]);
        warmer.poll(&workspace);
        assert!(warmer.queue.is_empty(), "the crawl was dispatched");
        let dispatched = cache.invalidation_epoch();

        for index in 0..5 {
            cache.invalidate_file_contents(&temp.path().join(format!("Unrelated{index}.pas")));
        }
        assert_ne!(cache.invalidation_epoch(), dispatched);
        warmer.request_closure_crawl(main.clone(), cache);
        assert!(
            warmer.queue.is_empty(),
            "no missed unit was invalidated, so the crawl in flight still covers them"
        );

        cache.invalidate_file_contents(&uri("Sync.pas").to_file_path().unwrap());
        warmer.request_closure_crawl(main, cache);
        assert!(
            !warmer.queue.is_empty(),
            "a missed unit was invalidated since the dispatch"
        );
        gate.set(0);
    }

    #[test]
    fn a_crawl_invalidated_while_in_flight_is_requested_again() {
        let (temp, workspace, main) = fixture();
        let cache = workspace.project_cache();
        let misses = || HashSet::from([uri("Sync.pas")]);
        let gate = Arc::new(InteractiveGate::default());
        gate.set(1);
        let mut warmer = Warmer::start(gate.clone());
        warmer.open(main.clone());
        cache.report_closure_misses(&main, misses());
        assert_eq!(cache.take_closure_crawl_requests(), vec![main.clone()]);
        warmer.poll(&workspace);
        assert!(warmer.queue.is_empty(), "the crawl was dispatched");

        // An edit while the crawl waits makes the cache drop its stores.
        cache.invalidate_file_contents(&temp.path().join("Provider.pas"));
        cache.report_closure_misses(&main, misses());
        gate.set(0);
        run_until_end(&mut warmer, &workspace);
        cache.report_closure_misses(&main, misses());

        assert_eq!(
            cache.take_closure_crawl_requests(),
            vec![main.clone()],
            "the invalidated crawl did not settle the closure"
        );
        warmer.request_closure_crawl(main.clone(), cache);
        assert!(!warmer.queue.is_empty(), "the crawl is queued again");

        run_until_end(&mut warmer, &workspace);
        for _ in 0..3 {
            cache.report_closure_misses(&main, misses());
        }
        assert!(
            cache.take_closure_crawl_requests().is_empty(),
            "a crawl at the current epoch settles a persistent miss"
        );
    }

    #[test]
    fn closing_a_file_forgets_its_closure_misses() {
        let (_temp, workspace, main) = fixture();
        let cache = workspace.project_cache();
        let misses = || HashSet::from([uri("Sync.pas")]);
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.open(main.clone());
        cache.report_closure_misses(&main, misses());
        assert_eq!(cache.take_closure_crawl_requests(), vec![main.clone()]);

        warmer.close(&main, cache);
        warmer.open(main.clone());
        cache.report_closure_misses(&main, misses());

        assert_eq!(cache.take_closure_crawl_requests(), vec![main]);
    }

    #[test]
    fn most_recently_opened_file_is_warmed_first_and_deduplicated() {
        let mut queue = WarmQueue::default();
        queue.push_front(uri("A.pas"));
        queue.push_front(uri("B.pas"));
        queue.push_front(uri("A.pas"));
        assert_eq!(queue.pop(), Some(uri("A.pas")));
        assert_eq!(queue.pop(), Some(uri("B.pas")));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn closing_removes_pending_work() {
        let mut queue = WarmQueue::default();
        queue.push_front(uri("A.pas"));
        queue.remove(&uri("A.pas"));
        assert!(queue.is_empty());
    }

    #[test]
    fn gate_blocks_until_interactive_work_finishes() {
        let gate = Arc::new(InteractiveGate::default());
        gate.set(1);
        let waiter = {
            let gate = gate.clone();
            std::thread::spawn(move || gate.wait_idle(&AtomicBool::new(false)))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !waiter.is_finished(),
            "the warmer must pause while interactive work is pending"
        );
        gate.set(0);
        assert!(waiter.join().unwrap());
    }

    #[test]
    fn cancelled_gate_wait_returns_false() {
        let gate = InteractiveGate::default();
        gate.set(1);
        assert!(!gate.wait_idle(&AtomicBool::new(true)));
    }

    #[test]
    fn opened_file_is_warmed_with_progress_and_pins() {
        let (_temp, workspace, main) = fixture();
        let gate = Arc::new(InteractiveGate::default());
        let mut warmer = Warmer::start(gate);
        warmer.open(main.clone());
        let events = run_until_end(&mut warmer, &workspace);
        assert!(
            matches!(events.first(), Some(WarmEvent::Begin { total: 1, .. })),
            "{events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                WarmEvent::Progress {
                    done: 1,
                    total: 1,
                    ..
                }
            )),
            "the final report must reach 1/1: {events:?}"
        );
        let Some(WarmEvent::End { pins, .. }) = events.last() else {
            panic!("{events:?}")
        };
        assert_eq!(
            pins.len(),
            2,
            "the file and its one direct import are pinned"
        );
        assert_eq!(workspace.project_cache().stats().units, 2);
    }

    #[test]
    fn zero_dependency_warm_emits_no_progress() {
        let temp = tempfile::tempdir().expect("workspace");
        let main_text = "unit Main;\ninterface\nimplementation\nend.\n".to_string();
        let path = temp.path().join("Main.pas");
        std::fs::write(&path, &main_text).unwrap();
        let main = Url::from_file_path(path).unwrap();
        let mut workspace = crate::workspace::Workspace::with_override_session(
            vec![temp.path().to_path_buf()],
            Default::default(),
            pascal_project::delphi_overrides::OverrideSession::new(None),
        );
        workspace
            .open_document(main.clone(), main_text, 1)
            .expect("open main unit");
        let gate = Arc::new(InteractiveGate::default());
        let mut warmer = Warmer::start(gate);
        warmer.open(main);

        let events = run_until_end(&mut warmer, &workspace);

        assert!(
            !events
                .iter()
                .any(|event| matches!(event, WarmEvent::Begin { .. } | WarmEvent::Progress { .. })),
            "zero-dependency crawls emit no progress lifecycle: {events:?}"
        );
        assert!(matches!(events.last(), Some(WarmEvent::End { .. })));
    }

    #[test]
    fn warmer_waits_for_interactive_work() {
        let (_temp, workspace, main) = fixture();
        let gate = Arc::new(InteractiveGate::default());
        gate.set(1);
        let mut warmer = Warmer::start(gate.clone());
        warmer.open(main);
        std::thread::sleep(Duration::from_millis(200));
        let early = warmer.poll(&workspace);
        assert!(
            !early
                .iter()
                .any(|event| matches!(event, WarmEvent::End { .. })),
            "{early:?}"
        );
        gate.set(0);
        run_until_end(&mut warmer, &workspace);
    }

    #[test]
    fn project_switch_cancels_crawl_and_discards_results() {
        let (_temp, workspace, main) = fixture();
        let cache_before = workspace.project_cache().stats();
        let gate = Arc::new(InteractiveGate::default());
        gate.set(1); // Hold the crawl before its first unit.
        let mut warmer = Warmer::start(gate.clone());
        warmer.open(main.clone());
        warmer.poll(&workspace);
        warmer.reset(Vec::<Url>::new(), workspace.project_cache());
        gate.set(0);
        std::thread::sleep(Duration::from_millis(300));
        let events = warmer.poll(&workspace);
        assert!(
            events.iter().all(|event| !matches!(
                event,
                WarmEvent::End { generation, .. } if *generation == warmer.generation()
            )),
            "{events:?}"
        );
        assert_eq!(workspace.project_cache().stats(), cache_before);
    }

    #[test]
    fn project_switch_clears_old_pins_and_recrawls_full_cache() {
        let (_temp, workspace, main) = fixture();
        let cache = workspace.project_cache();
        let gate = Arc::new(InteractiveGate::default());
        let mut warmer = Warmer::start(gate);
        warmer.open(main.clone());
        let first_events = run_until_end(&mut warmer, &workspace);
        let Some(WarmEvent::End { pins, .. }) = first_events
            .iter()
            .find(|event| matches!(event, WarmEvent::End { .. }))
        else {
            panic!("{first_events:?}");
        };
        cache.pin(&main, pins.clone());
        let budget = cache.stats().bytes;
        assert!(budget > 0);
        cache.set_max_bytes(budget);
        assert!(!cache.has_room(), "old crawl pins use the full budget");

        warmer.reset([main.clone()], cache);
        assert!(cache.has_room(), "reset releases old crawl pins");
        assert_eq!(cache.stats().bytes, budget, "reset retains cache entries");

        let second_events = run_until_end(&mut warmer, &workspace);
        assert!(
            second_events.iter().any(|event| matches!(
                event,
                WarmEvent::End { generation, .. } if *generation == warmer.generation()
            )),
            "{second_events:?}"
        );
    }

    #[test]
    fn project_switch_recrawl_dispatches_most_recently_opened_file_first() {
        let (temp, mut workspace, _main) = fixture();
        let mut uris = Vec::new();
        for name in ["A", "B", "C"] {
            let source = format!("unit {name};\ninterface\nimplementation\nend.\n");
            let path = temp.path().join(format!("{name}.pas"));
            std::fs::write(&path, &source).unwrap();
            let uri = Url::from_file_path(path).unwrap();
            workspace
                .open_document(uri.clone(), source, 1)
                .expect("open recrawl fixture unit");
            uris.push(uri);
        }
        let [a, b, c] = uris.as_slice() else {
            unreachable!();
        };
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.open(a.clone());
        warmer.open(b.clone());
        warmer.open(c.clone());
        let a_open_epoch = warmer.open_epochs[a];
        warmer.rewarm(a.clone());
        assert_eq!(warmer.open_epochs[a], a_open_epoch);

        // Workspace enumeration is unordered and may supply the reverse of
        // the warmer's recency order.
        warmer.reset([c.clone(), b.clone(), a.clone()], workspace.project_cache());
        assert_eq!(warmer.open_epochs[a], a_open_epoch);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut dispatched = Vec::new();
        while std::time::Instant::now() < deadline && dispatched.len() < 3 {
            for event in warmer.poll(&workspace) {
                if let WarmEvent::End { uri, .. } = event {
                    dispatched.push(uri);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(dispatched, vec![c.clone(), b.clone(), a.clone()]);
    }

    #[test]
    fn background_work_pauses_for_interactive_work_only_without_claims() {
        let gate = Arc::new(InteractiveGate::default());
        let cancel = AtomicBool::new(false);
        assert!(
            yield_to_interactive_work(&cancel),
            "work without a background gate never pauses"
        );
        gate.set(1);
        set_background_gate_on_this_thread(Some((gate.clone(), Duration::MAX)));

        let cache = crate::project_cache::ProjectCache::new(1 << 20);
        let context = pascal_project::ProjectContext::from_parts(
            pascal_project::ProjectContextParts::default(),
        );
        let uri = Url::parse("file:///ws/Main.pas").unwrap();
        let crate::project_cache::Lookup::Compute(claim) =
            cache.imports(&uri, &context, 1, &HashMap::new(), &cancel)
        else {
            panic!("expected a claim")
        };
        assert!(
            yield_to_interactive_work(&cancel),
            "work holding a claim does not pause: a waiter would wait out the pause"
        );
        drop(claim);

        let (paused_tx, paused_rx) = std::sync::mpsc::channel();
        let paused_gate = gate.clone();
        let paused = std::thread::spawn(move || {
            set_background_gate_on_this_thread(Some((paused_gate, Duration::MAX)));
            paused_tx
                .send(yield_to_interactive_work(&AtomicBool::new(false)))
                .unwrap();
        });
        // The pause is reported to the dispatcher, which lends its slot.
        let deadline = Instant::now() + Duration::from_secs(5);
        while gate.paused_background() == 0 {
            assert!(Instant::now() < deadline, "the work pauses");
            std::thread::yield_now();
        }
        assert!(paused_rx.try_recv().is_err(), "the work is still paused");
        gate.set(0);
        assert!(
            paused_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the work resumes once interactive work is done")
        );
        paused.join().unwrap();
        assert_eq!(gate.paused_background(), 0);

        // A client request waiting for a worker pauses background work too.
        gate.set_waiting_clients(1);
        let (paused_tx, paused_rx) = std::sync::mpsc::channel();
        let paused_gate = gate.clone();
        let paused = std::thread::spawn(move || {
            set_background_gate_on_this_thread(Some((paused_gate, Duration::MAX)));
            paused_tx
                .send(yield_to_interactive_work(&AtomicBool::new(false)))
                .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while gate.paused_background() == 0 {
            assert!(
                Instant::now() < deadline,
                "the work pauses for a waiting request"
            );
            std::thread::yield_now();
        }
        gate.set_waiting_clients(0);
        assert!(
            paused_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the work resumes once the request has a worker")
        );
        paused.join().unwrap();

        gate.set(1);
        set_background_gate_on_this_thread(Some((gate.clone(), Duration::ZERO)));
        assert!(
            yield_to_interactive_work(&cancel),
            "a pause ends at its limit while interactive work still runs"
        );
        cancel.store(true, Ordering::Relaxed);
        assert!(
            !yield_to_interactive_work(&cancel),
            "a cancelled pause fails"
        );
        set_background_gate_on_this_thread(None);
    }

    #[test]
    fn shutdown_cancels_and_joins_worker_waiting_for_interactive_gate() {
        let (_temp, workspace, main) = fixture();
        let gate = Arc::new(InteractiveGate::default());
        gate.set(1);
        let mut warmer = Warmer::start(gate);
        warmer.open(main);
        warmer.poll(&workspace);
        std::thread::sleep(Duration::from_millis(50));

        let started = std::time::Instant::now();
        let joined = warmer.shutdown();

        assert!(joined, "the canceled worker should finish and be joined");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "warmer teardown must respect its bounded join deadline"
        );
    }

    #[test]
    fn interactive_lookup_does_not_wait_for_a_paused_warm_walk() {
        let (_temp, workspace, main) = fixture();
        let input = workspace.analysis_input();
        let gate = Arc::new(InteractiveGate::default());
        let warm_cancel = Arc::new(AtomicBool::new(false));
        let hook_gate = gate.clone();
        let hook_cancel = warm_cancel.clone();
        let (claim_acquired_tx, claim_acquired_rx) = std::sync::mpsc::channel();
        let (resume_hook_tx, resume_hook_rx) = crossbeam_channel::bounded::<()>(1);
        let warm_uri = main.clone();
        let mut warming_workspace = Workspace::from_analysis_input(&input);
        let first_unit = Arc::new(AtomicBool::new(true));
        warming_workspace.set_dependency_hook(Arc::new(move |_, _, _| {
            // Only the first unit boundary holds the walk; the test releases
            // it (or drops the sender) whatever the interactive request does.
            if first_unit.swap(false, Ordering::Relaxed) {
                hook_gate.set(1);
                claim_acquired_tx.send(()).unwrap();
                let _ = resume_hook_rx.recv();
            }
            if hook_gate.try_wait_idle(&hook_cancel) {
                Ok(())
            } else {
                Err(crate::workspace::rename::CANCELLATION_MESSAGE.to_string())
            }
        }));
        let warm_thread = std::thread::spawn(move || {
            crate::project_cache::set_claim_role_on_this_thread(
                crate::project_cache::ClaimRole::yielding(warm_cancel.clone()),
            );
            warming_workspace.warm_with_cancel(&warm_uri, &warm_cancel)
        });
        claim_acquired_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("warmer reached its dependency walk");

        let interactive_input = workspace.analysis_input();
        let interactive_uri = main.clone();
        let (interactive_started_tx, interactive_started_rx) = std::sync::mpsc::channel();
        let (interactive_done_tx, interactive_done_rx) = std::sync::mpsc::channel();
        let interactive_thread = std::thread::spawn(move || {
            crate::project_cache::set_claim_role_on_this_thread(
                crate::project_cache::ClaimRole::interactive(
                    Arc::new(AtomicBool::new(false)),
                    Duration::ZERO,
                ),
            );
            let mut interactive_workspace = Workspace::from_analysis_input(&interactive_input);
            interactive_started_tx.send(()).unwrap();
            let result = interactive_workspace
                .warm_with_cancel(&interactive_uri, &AtomicBool::new(false))
                .map(|_| ());
            interactive_done_tx.send(result).unwrap();
        });
        interactive_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("interactive cache consumer started");

        // The warm stays paused in its dependency walk until the interactive
        // request is done or has had its chance to finish.
        let completed_while_gate_busy = interactive_done_rx.recv_timeout(Duration::from_secs(10));
        let completed_before_release = completed_while_gate_busy.is_ok();
        drop(resume_hook_tx);
        let warm_result = warm_thread.join().unwrap();
        let interactive_result = match completed_while_gate_busy {
            Ok(result) => result,
            Err(_) => interactive_done_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("interactive request finishes after releasing the walk"),
        };
        interactive_thread.join().unwrap();

        assert!(
            completed_before_release,
            "interactive cache lookup must not wait for the warmer's paused claim"
        );
        assert!(
            warm_result.is_err(),
            "the paused warm attempt must yield its claim"
        );
        assert!(interactive_result.is_ok());
    }

    #[test]
    fn paused_event_requeues_open_file_for_retry() {
        let (_temp, workspace, main) = fixture();
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.open(main.clone());
        warmer.queue.clear();
        warmer.busy = true;
        let event = WarmEvent::Paused {
            uri: main.clone(),
            generation: warmer.generation,
            open_epoch: warmer.open_epochs[&main],
        };

        assert!(warmer.accept_event(&event, workspace.project_cache()));
        assert!(!warmer.busy);
        assert_eq!(warmer.queue.pop(), Some(main));
    }

    #[test]
    fn repeated_open_preserves_inflight_attempt_and_finishes_progress() {
        let (_temp, workspace, main) = fixture();
        let gate = Arc::new(InteractiveGate::default());
        gate.set(1);
        let mut warmer = Warmer::start(gate);
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        warmer.events = events_rx;
        warmer.open(main.clone());

        assert!(warmer.poll(&workspace).is_empty());
        assert!(warmer.busy, "poll should dispatch the first attempt");
        let open_epoch = warmer.open_epochs[&main];

        let (connection, client) = lsp_server::Connection::memory();
        let mut progress = WarmProgress::new(true);
        let begin = WarmEvent::Begin {
            uri: main.clone(),
            total: 1,
            generation: warmer.generation,
            open_epoch,
        };
        events_tx.send(begin.clone()).expect("queue begin event");
        assert_eq!(warmer.poll(&workspace), vec![begin.clone()]);
        progress
            .handle_with_retry(&connection, &begin, true)
            .expect("create progress for accepted begin");
        let Message::Request(create) = client.receiver.recv().expect("progress create") else {
            panic!("expected progress creation request");
        };
        progress
            .handle_response(
                &connection,
                &Response::new_ok(create.id, serde_json::Value::Null),
            )
            .expect("acknowledge progress create");
        let Message::Notification(progress_begin) = client.receiver.recv().expect("progress begin")
        else {
            panic!("expected progress begin notification");
        };

        // Repeated didOpen for this already-open URI must neither rotate the
        // in-flight attempt's identity nor queue a duplicate crawl.
        warmer.open(main.clone());
        workspace.project_cache().set_max_bytes(0);
        let end = WarmEvent::End {
            uri: main.clone(),
            fingerprint: None,
            pins: Vec::new(),
            generation: warmer.generation,
            open_epoch,
        };
        events_tx.send(end.clone()).expect("queue end event");
        let accepted = warmer.poll(&workspace);
        assert_eq!(
            accepted,
            vec![end.clone()],
            "the in-flight End remains current"
        );
        for event in &accepted {
            progress
                .handle_with_retry(&connection, event, true)
                .expect("finish accepted warm progress");
        }

        let Message::Notification(progress_end) = client.receiver.recv().expect("progress end")
        else {
            panic!("expected progress end notification");
        };
        assert_eq!(progress_end.params["token"], progress_begin.params["token"]);
        assert_eq!(progress_end.params["value"]["kind"], "end");
        assert!(progress.active.is_empty());
        assert!(progress.pending_creates.is_empty());
        assert!(
            warmer.queue.is_empty(),
            "repeated open must not queue an in-flight URI"
        );
        assert!(client.receiver.try_recv().is_err());
    }

    #[test]
    fn poll_drops_stale_generation_progress_events() {
        let (_temp, workspace, main) = fixture();
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.open(main.clone());
        warmer.queue.clear();
        warmer.generation = 4;
        let open_epoch = warmer.open_epochs[&main];
        let (sender, events) = crossbeam_channel::unbounded();
        warmer.events = events;
        for event in [
            WarmEvent::Begin {
                uri: main.clone(),
                total: 1,
                generation: 3,
                open_epoch,
            },
            WarmEvent::Progress {
                uri: main.clone(),
                done: 1,
                total: 1,
                generation: 3,
                open_epoch,
            },
            WarmEvent::Paused {
                uri: main.clone(),
                generation: 3,
                open_epoch,
            },
            WarmEvent::Begin {
                uri: main.clone(),
                total: 1,
                generation: 4,
                open_epoch,
            },
            WarmEvent::Progress {
                uri: main,
                done: 1,
                total: 1,
                generation: 4,
                open_epoch,
            },
        ] {
            sender.send(event).expect("queue warmer event");
        }

        let events = warmer.poll(&workspace);
        assert_eq!(events.len(), 2, "only current-generation progress survives");
        assert!(events.iter().all(|event| match event {
            WarmEvent::Begin { generation, .. } | WarmEvent::Progress { generation, .. } => {
                *generation == 4
            }
            _ => false,
        }));
        assert!(
            warmer.queue.is_empty(),
            "stale pause must not requeue the file"
        );
    }

    #[test]
    fn close_drops_late_same_generation_events_without_resurrecting_progress() {
        let (_temp, mut workspace, main) = fixture();
        let (connection, client) = lsp_server::Connection::memory();
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        let mut progress = WarmProgress::new(true);
        warmer.open(main.clone());
        let open_epoch = warmer.open_epochs[&main];
        warmer.busy = true;

        assert!(workspace.close_document(&main));
        warmer.close(&main, workspace.project_cache());
        progress
            .close(&connection, &main)
            .expect("close progress for the document");

        let late_begin = WarmEvent::Begin {
            uri: main.clone(),
            total: 1,
            generation: warmer.generation,
            open_epoch,
        };
        let begin_accepted = warmer.accept_event(&late_begin, workspace.project_cache());
        if begin_accepted {
            progress
                .handle_with_retry(&connection, &late_begin, true)
                .expect("handle late begin");
        }
        let late_paused = WarmEvent::Paused {
            uri: main.clone(),
            generation: warmer.generation,
            open_epoch,
        };
        let paused_accepted = warmer.accept_event(&late_paused, workspace.project_cache());
        if paused_accepted {
            progress
                .handle_with_retry(&connection, &late_paused, workspace.is_open(&main))
                .expect("handle late pause");
        }

        assert!(!begin_accepted, "closed URI must drop a late Begin");
        assert!(!warmer.busy, "late Paused still releases the worker slot");
        assert!(progress.active.is_empty());
        assert!(progress.pending_creates.is_empty());
        assert!(
            client.receiver.try_recv().is_err(),
            "late events for a closed URI must not create or begin progress"
        );
    }

    #[test]
    fn close_reopen_drops_events_from_the_first_open() {
        let (temp, mut workspace, main) = fixture();
        let other_text = "unit Other;\ninterface\nimplementation\nend.\n";
        let other_path = temp.path().join("Other.pas");
        std::fs::write(&other_path, other_text).expect("write other unit");
        let other = Url::from_file_path(other_path).expect("other URI");
        workspace
            .open_document(other.clone(), other_text.to_string(), 1)
            .expect("open other unit");

        let (connection, client) = lsp_server::Connection::memory();
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        let mut progress = WarmProgress::new(true);
        warmer.open(main.clone());
        let first_open_epoch = warmer.open_epochs[&main];
        warmer.busy = true;

        assert!(workspace.close_document(&main));
        warmer.close(&main, workspace.project_cache());
        progress
            .close(&connection, &main)
            .expect("close first-open progress");
        let main_text = std::fs::read_to_string(temp.path().join("Main.pas"))
            .expect("read main unit for reopen");
        workspace
            .open_document(main.clone(), main_text, 2)
            .expect("reopen main unit");
        warmer.open(main.clone());
        let reopened_epoch = warmer.open_epochs[&main];
        assert_ne!(reopened_epoch, first_open_epoch);
        warmer.open(other);
        let queue_before_old_events = warmer.queue.order.clone();
        workspace.project_cache().set_max_bytes(0);

        let (sender, old_events) = crossbeam_channel::unbounded();
        warmer.events = old_events;
        let generation = warmer.generation;
        for event in [
            WarmEvent::Begin {
                uri: main.clone(),
                total: 1,
                generation,
                open_epoch: first_open_epoch,
            },
            WarmEvent::Progress {
                uri: main.clone(),
                done: 1,
                total: 1,
                generation,
                open_epoch: first_open_epoch,
            },
            WarmEvent::Paused {
                uri: main.clone(),
                generation,
                open_epoch: first_open_epoch,
            },
            WarmEvent::End {
                uri: main.clone(),
                fingerprint: Some(7),
                pins: vec![(main.clone(), 7)],
                generation,
                open_epoch: first_open_epoch,
            },
        ] {
            sender.send(event).expect("queue old-open event");
        }

        let accepted = warmer.poll(&workspace);
        for event in &accepted {
            let paused_will_retry = match event {
                WarmEvent::Paused { uri, .. } => workspace.is_open(uri),
                _ => true,
            };
            progress
                .handle_with_retry(&connection, event, paused_will_retry)
                .expect("handle accepted event");
        }

        assert!(
            accepted
                .iter()
                .all(|event| !matches!(event, WarmEvent::End { .. })),
            "a stale End must not reach the caller's cache pinning path"
        );
        assert!(accepted.is_empty(), "old-open events must all be fenced");
        assert_eq!(warmer.queue.order, queue_before_old_events);
        assert!(!warmer.busy, "stale End still releases the old worker slot");
        assert!(progress.active.is_empty());
        assert!(progress.pending_creates.is_empty());
        assert!(
            client.receiver.try_recv().is_err(),
            "a stale Begin must not create progress for the reopened URI"
        );
    }

    #[test]
    fn paused_closed_uri_ends_active_progress_when_no_retry_is_planned() {
        let (connection, client) = lsp_server::Connection::memory();
        let uri = uri("Closed.pas");
        let mut progress = WarmProgress::new(true);
        let begin = WarmEvent::Begin {
            uri: uri.clone(),
            total: 1,
            generation: 0,
            open_epoch: 0,
        };
        progress
            .handle_with_retry(&connection, &begin, true)
            .expect("request progress token");
        let Message::Request(create) = client.receiver.recv().expect("create request") else {
            panic!("expected progress creation request");
        };
        progress
            .handle_response(
                &connection,
                &Response::new_ok(create.id, serde_json::Value::Null),
            )
            .expect("acknowledge progress token");
        let Message::Notification(begin) = client.receiver.recv().expect("progress begin") else {
            panic!("expected progress begin");
        };

        progress
            .handle_with_retry(
                &connection,
                &WarmEvent::Paused {
                    uri: uri.clone(),
                    generation: 0,
                    open_epoch: 0,
                },
                false,
            )
            .expect("end a token for a closed file");
        let Message::Notification(end) = client.receiver.recv().expect("progress end") else {
            panic!("expected progress end");
        };

        assert_eq!(end.params["token"], begin.params["token"]);
        assert_eq!(end.params["value"]["kind"], "end");
        assert!(!progress.active.contains_key(&uri));
        assert!(client.receiver.try_recv().is_err());
    }

    /// `count` units `U<n>.pas` in a fresh root, none open in the workspace.
    fn index_fixture(
        count: usize,
        options: crate::workspace::WorkspaceOptions,
    ) -> (tempfile::TempDir, crate::workspace::Workspace, IndexAnchor) {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..count {
            std::fs::write(
                temp.path().join(format!("U{index}.pas")),
                format!(
                    "unit U{index};\ninterface\nprocedure P{index};\nimplementation\nprocedure P{index};\nbegin\nend;\nend.\n"
                ),
            )
            .unwrap();
        }
        let workspace = crate::workspace::Workspace::with_override_session(
            vec![temp.path().to_path_buf()],
            options,
            pascal_project::delphi_overrides::OverrideSession::new(None),
        );
        let anchor =
            IndexAnchor::Document(Url::from_file_path(temp.path().join("U0.pas")).unwrap());
        (temp, workspace, anchor)
    }

    fn poll_until(
        warmer: &mut Warmer,
        workspace: &crate::workspace::Workspace,
        mut done: impl FnMut(&[WarmEvent]) -> bool,
    ) -> Vec<WarmEvent> {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut events = Vec::new();
        while std::time::Instant::now() < deadline {
            events.extend(warmer.poll(workspace));
            if done(&events) {
                return events;
            }
            std::thread::yield_now();
        }
        panic!("warmer did not reach the expected state: {events:?}");
    }

    fn is_index_event(event: &WarmEvent) -> bool {
        matches!(
            event,
            WarmEvent::IndexProgress { .. } | WarmEvent::IndexStopped { .. }
        )
    }

    #[test]
    fn index_jobs_run_only_when_no_open_file_is_queued() {
        let (_temp, workspace, main) = fixture();
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.open(main.clone());
        warmer.request_index(IndexAnchor::Document(main.clone()));

        warmer.poll(&workspace);
        assert!(warmer.busy_attempt.is_some(), "the crawl goes first");
        assert_eq!(warmer.index_queue.len(), 1, "the index job waits");

        let events = poll_until(&mut warmer, &workspace, |events| {
            events.iter().any(is_index_event)
        });
        let end = events
            .iter()
            .position(|event| matches!(event, WarmEvent::End { .. }))
            .expect("the crawl ended");
        let index = events.iter().position(is_index_event).unwrap();
        assert!(end < index, "{events:?}");
    }

    #[test]
    fn an_index_batch_pauses_for_interactive_work_and_requeues() {
        let (_temp, workspace, anchor) = index_fixture(6, Default::default());
        let (_other_temp, mut reference, reference_anchor) = index_fixture(6, Default::default());
        let total = reference
            .advance_workspace_index(
                &reference_anchor,
                IndexBound::all(),
                &mut || Ok(()),
                &AtomicBool::new(false),
            )
            .expect("reference batch")
            .indexed;
        assert!(total > 2, "{total}");

        let gate = Arc::new(InteractiveGate::default());
        *gate.busy_after_checks.lock().unwrap() = Some(2);
        let mut warmer = Warmer::start(gate.clone());
        warmer.request_index(anchor.clone());
        let events = poll_until(&mut warmer, &workspace, is_stopped);
        fn is_stopped(events: &[WarmEvent]) -> bool {
            events.iter().any(is_index_event)
        }
        assert!(
            matches!(
                events.last(),
                Some(WarmEvent::IndexStopped { paused: true, .. })
            ),
            "{events:?}"
        );
        warmer.poll(&workspace);
        assert!(
            !warmer.busy && warmer.index_queue.contains(&anchor),
            "the paused anchor waits in the queue until the gate passes"
        );

        gate.set(0);
        let events = poll_until(&mut warmer, &workspace, is_stopped);
        let Some(WarmEvent::IndexProgress { progress, .. }) = events.last() else {
            panic!("{events:?}")
        };
        assert_eq!(progress.indexed, total - 2, "the published members stayed");
        assert!(progress.complete);
    }

    #[test]
    fn a_complete_index_is_not_requeued() {
        let (_temp, workspace, anchor) = index_fixture(3, Default::default());
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.request_index(anchor);
        let events = poll_until(&mut warmer, &workspace, |events| {
            events.iter().any(is_index_event)
        });
        assert!(matches!(
            events.last(),
            Some(WarmEvent::IndexProgress { progress, .. }) if progress.complete
        ));
        assert!(warmer.index_queue.is_empty());
        assert!(warmer.is_idle());
    }

    #[test]
    fn an_incomplete_batch_is_requeued_until_complete() {
        let (_temp, workspace, anchor) =
            index_fixture(INDEX_BATCH_UNITS * 2 + 2, Default::default());
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.request_index(anchor);
        let events = poll_until(&mut warmer, &workspace, |events| {
            events
                .iter()
                .any(|event| matches!(event, WarmEvent::IndexProgress { progress, .. } if progress.complete))
        });
        let complete: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                WarmEvent::IndexProgress { progress, .. } => Some(progress.complete),
                _ => None,
            })
            .collect();
        assert_eq!(complete, [false, false, true], "{events:?}");
        assert!(warmer.index_queue.is_empty());
    }

    #[test]
    fn a_warm_end_requests_the_context_and_the_configured_project() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("Missing.dproj");
        let main_text = "unit Main;\ninterface\nimplementation\nend.\n".to_string();
        std::fs::write(temp.path().join("Main.pas"), &main_text).unwrap();
        let mut workspace = crate::workspace::Workspace::with_override_session(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions {
                project_file: Some(project.clone()),
                ..Default::default()
            },
            pascal_project::delphi_overrides::OverrideSession::new(None),
        );
        let main = Url::from_file_path(temp.path().join("Main.pas")).unwrap();
        workspace
            .open_document(main.clone(), main_text, 1)
            .expect("open main unit");
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.open(main.clone());
        let events = poll_until(&mut warmer, &workspace, |events| {
            events.iter().filter(|event| is_index_event(event)).count() >= 2
        });
        let anchors: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                WarmEvent::IndexProgress { anchor, .. }
                | WarmEvent::IndexStopped { anchor, .. } => Some(anchor.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            anchors,
            [IndexAnchor::Project(project), IndexAnchor::Document(main)]
        );
    }

    /// Task 10b follow-up (13b-6): a relative `projectFile` is looked up
    /// under the workspace roots and gets its Project anchor.
    #[test]
    fn a_relative_configured_project_gets_its_project_anchor() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("App.dproj");
        std::fs::write(&project, "<Project></Project>").unwrap();
        let workspace = crate::workspace::Workspace::with_override_session(
            vec![temp.path().to_path_buf()],
            crate::workspace::WorkspaceOptions {
                project_file: Some("App.dproj".into()),
                ..Default::default()
            },
            pascal_project::delphi_overrides::OverrideSession::new(None),
        );
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.poll(&workspace);
        assert_eq!(warmer.configured_project, Some(project));
    }

    #[test]
    fn the_index_queue_is_deduplicated_project_first_and_capped() {
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        let document = |name: &str| IndexAnchor::Document(uri(name));
        warmer.request_index(document("A.pas"));
        warmer.request_index(document("A.pas"));
        warmer.request_index(IndexAnchor::Project("/p/One.dproj".into()));
        warmer.request_index(IndexAnchor::Project("/p/Two.dproj".into()));
        assert_eq!(
            warmer.index_queue,
            [
                IndexAnchor::Project("/p/One.dproj".into()),
                IndexAnchor::Project("/p/Two.dproj".into()),
                document("A.pas"),
            ]
        );
        for index in 0..40 {
            warmer.request_index(document(&format!("D{index}.pas")));
        }
        assert_eq!(warmer.index_queue.len(), MAX_INDEX_ANCHORS);
        assert!(matches!(warmer.index_queue[0], IndexAnchor::Project(_)));
    }

    #[test]
    fn reset_drops_index_requests() {
        let (_temp, workspace, main) = fixture();
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.request_index(IndexAnchor::Document(main));
        warmer.reset(std::iter::empty(), workspace.project_cache());
        assert!(warmer.index_queue.is_empty());
    }

    #[test]
    fn opening_a_file_mid_batch_pauses_the_index_for_its_crawl() {
        let (_temp, workspace, main) = fixture();
        let gate = Arc::new(InteractiveGate::default());
        // The batch is dispatched, then waits for its turn.
        gate.hold_index_turns.store(true, Ordering::Relaxed);
        let mut warmer = Warmer::start(gate.clone());
        warmer.request_index(IndexAnchor::Document(main.clone()));
        warmer.poll(&workspace);
        assert!(warmer.busy, "the index job was dispatched");
        while gate.index_turn_waiters() == 0 {
            std::thread::yield_now();
        }

        warmer.open(main.clone());
        let events = poll_until(&mut warmer, &workspace, |events| {
            events.iter().any(is_index_event)
        });
        assert!(
            matches!(
                events.first(),
                Some(WarmEvent::IndexStopped {
                    paused: true,
                    mode: IndexMode::Full,
                    ..
                })
            ),
            "the waiting batch yields to the opened file: {events:?}"
        );

        gate.hold_index_turns.store(false, Ordering::Relaxed);
        let events = poll_until(&mut warmer, &workspace, |events| {
            events
                .iter()
                .any(|event| matches!(event, WarmEvent::IndexProgress { .. }))
        });
        let end = events
            .iter()
            .position(|event| matches!(event, WarmEvent::End { .. }))
            .expect("the opened file was crawled");
        let resumed = events
            .iter()
            .position(|event| matches!(event, WarmEvent::IndexProgress { .. }))
            .unwrap();
        assert!(end < resumed, "{events:?}");
    }

    fn index_mode(event: &WarmEvent) -> Option<IndexMode> {
        match event {
            WarmEvent::IndexProgress { progress, .. } => Some(progress.mode),
            WarmEvent::IndexStopped { mode, .. } => Some(*mode),
            _ => None,
        }
    }

    #[test]
    fn index_batches_wait_for_running_bulk_requests() {
        let gate = Arc::new(InteractiveGate::default());
        gate.set_running_bulk(1);
        assert!(!gate.index_may_run(&AtomicBool::new(false)));
        assert!(
            gate.try_wait_idle(&AtomicBool::new(false)),
            "the crawl gate is unchanged"
        );
        gate.set_running_bulk(0);
        assert!(gate.index_may_run(&AtomicBool::new(false)));
        gate.set_waiting_clients(1);
        assert!(!gate.index_may_run(&AtomicBool::new(false)));
        gate.set_waiting_clients(0);
        assert!(!gate.index_may_run(&AtomicBool::new(true)), "cancelled");
    }

    #[test]
    fn an_index_turn_waits_without_a_time_limit_until_bulk_work_ends() {
        let gate = Arc::new(InteractiveGate::default());
        let no_crawl = Arc::new(AtomicBool::new(false));
        gate.set_running_bulk(1);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = {
            let gate = gate.clone();
            let no_crawl = no_crawl.clone();
            std::thread::spawn(move || {
                done_tx
                    .send(gate.wait_index_turn(
                        &AtomicBool::new(false),
                        &no_crawl,
                        IndexMode::Reuse,
                    ))
                    .unwrap();
            })
        };
        // Wait until the waiter is parked on the condvar, then check it has
        // not returned (a returned turn would be in the channel already).
        while gate.index_turn_waiters() == 0 {
            std::thread::yield_now();
        }
        assert!(done_rx.try_recv().is_err(), "the turn waits for bulk work");
        gate.set_running_bulk(0);
        assert_eq!(done_rx.recv().unwrap(), IndexTurn::Go);
        waiter.join().unwrap();
        gate.set_running_bulk(1);
        assert_eq!(
            gate.wait_index_turn(&AtomicBool::new(true), &no_crawl, IndexMode::Reuse),
            IndexTurn::Cancelled
        );
    }

    #[test]
    fn an_index_turn_yields_to_a_queued_crawl_instead_of_waiting() {
        let gate = InteractiveGate::default();
        let crawl = AtomicBool::new(true);
        let cancel = AtomicBool::new(false);
        assert_eq!(
            gate.wait_index_turn(&cancel, &crawl, IndexMode::Full),
            IndexTurn::Yield,
            "extraction always yields to a queued crawl"
        );
        assert_eq!(
            gate.wait_index_turn(&cancel, &crawl, IndexMode::Reuse),
            IndexTurn::Go,
            "a reuse batch runs ahead of the crawl while it can run now"
        );
        gate.set_running_bulk(1);
        assert_eq!(
            gate.wait_index_turn(&cancel, &crawl, IndexMode::Reuse),
            IndexTurn::Yield,
            "but does not wait in front of it for Bulk work"
        );
    }

    /// Returns once an index batch waits in `wait_index_turn`.
    fn wait_for_turn_waiter(gate: &InteractiveGate) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while gate.index_turn_waiters() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "no index batch waits for its turn"
            );
            std::thread::yield_now();
        }
    }

    /// I-1 (b): interactive work pauses the crawl as well, so a reuse batch
    /// waits for it in front of a queued crawl instead of yielding.
    #[test]
    fn a_reuse_turn_waits_for_interactive_work_in_front_of_a_queued_crawl() {
        let gate = Arc::new(InteractiveGate::default());
        let crawl = Arc::new(AtomicBool::new(true));
        gate.set(1);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = {
            let (gate, crawl) = (gate.clone(), crawl.clone());
            std::thread::spawn(move || {
                done_tx
                    .send(gate.wait_index_turn(&AtomicBool::new(false), &crawl, IndexMode::Reuse))
                    .unwrap();
            })
        };
        wait_for_turn_waiter(&gate);
        // Taking the gate's lock waits until the waiter's first check is
        // done and it waits on the condvar (or has returned).
        gate.set(1);
        assert!(done_rx.try_recv().is_err(), "the turn waits");
        gate.set(0);
        assert_eq!(done_rx.recv().unwrap(), IndexTurn::Go);
        waiter.join().unwrap();
        gate.set(1);
        assert_eq!(
            gate.wait_index_turn(&AtomicBool::new(false), &crawl, IndexMode::Full),
            IndexTurn::Yield,
            "extraction still yields"
        );
    }

    #[test]
    fn an_index_batch_stops_between_members_when_a_client_request_queues() {
        let (_temp, workspace, anchor) = index_fixture(6, Default::default());
        let (_other_temp, mut reference, reference_anchor) = index_fixture(6, Default::default());
        let total = reference
            .advance_workspace_index(
                &reference_anchor,
                IndexBound::all(),
                &mut || Ok(()),
                &AtomicBool::new(false),
            )
            .expect("reference batch")
            .indexed;
        assert!(total > 2, "{total}");

        let gate = Arc::new(InteractiveGate::default());
        // Two members pass their check; a client request then queues.
        *gate.waiting_after_checks.lock().unwrap() = Some(2);
        let mut warmer = Warmer::start(gate.clone());
        warmer.request_index(anchor.clone());
        let events = poll_until(&mut warmer, &workspace, |events| {
            events.iter().any(is_index_event)
        });
        assert!(
            matches!(
                events.last(),
                Some(WarmEvent::IndexStopped {
                    paused: true,
                    mode: IndexMode::Full,
                    ..
                })
            ),
            "{events:?}"
        );
        assert_eq!(gate.waiting_clients_for_tests(), 1);
        assert!(!gate.index_may_run_now());
        warmer.poll(&workspace);
        assert!(
            !warmer.busy && warmer.index_queue.contains(&anchor),
            "the paused anchor waits in the queue until the gate passes"
        );

        gate.set_waiting_clients(0);
        let events = poll_until(&mut warmer, &workspace, |events| {
            events.iter().any(is_index_event)
        });
        let Some(WarmEvent::IndexProgress { progress, .. }) = events.last() else {
            panic!("{events:?}")
        };
        assert_eq!(progress.indexed, total - 2, "the published members stayed");
        assert!(progress.complete);
    }

    #[test]
    fn a_disabled_cache_runs_no_reuse_pass_and_extracts_after_the_crawl() {
        let (_temp, workspace, main) = fixture();
        assert_disabled_sequence(&workspace, &main, Arc::new(InteractiveGate::default()));
    }

    fn assert_disabled_sequence(
        workspace: &crate::workspace::Workspace,
        main: &Url,
        gate: Arc<InteractiveGate>,
    ) {
        assert!(!workspace.persistent_cache().is_enabled());
        let mut warmer = Warmer::start(gate);
        warmer.open(main.clone());
        warmer.request_index(IndexAnchor::Document(main.clone()));

        warmer.poll(workspace);
        assert!(warmer.busy_attempt.is_some(), "the crawl goes first");
        assert!(
            warmer.reuse_queue.is_empty(),
            "no reuse pass without a store"
        );
        assert_eq!(warmer.index_queue.len(), 1, "the index job waits");

        let events = poll_until(&mut warmer, workspace, |events| {
            events.iter().any(is_index_event)
        });
        let end = events
            .iter()
            .position(|event| matches!(event, WarmEvent::End { .. }))
            .expect("the crawl ended");
        let index = events.iter().position(is_index_event).unwrap();
        assert!(end < index, "{events:?}");
        let sequence = events.iter().map(event_kind).collect::<Vec<_>>();
        assert_eq!(
            sequence,
            [
                "Begin",
                "Progress 1/1",
                "Progress 1/2",
                "ClosureStored",
                "End",
                "IndexProgress Full Document seal=None",
            ],
            "{events:?}"
        );
        assert!(warmer.reuse_queue.is_empty() && warmer.reuse_done.is_empty());
    }

    fn event_kind(event: &WarmEvent) -> String {
        let anchor_kind = |anchor: &IndexAnchor| match anchor {
            IndexAnchor::Project(_) => "Project",
            IndexAnchor::Document(_) => "Document",
        };
        match event {
            WarmEvent::Begin { .. } => "Begin".into(),
            WarmEvent::Progress { done, total, .. } => format!("Progress {done}/{total}"),
            WarmEvent::End { .. } => "End".into(),
            WarmEvent::Paused { .. } => "Paused".into(),
            WarmEvent::Yielded { to_reuse: true, .. } => "Yielded to reuse".into(),
            WarmEvent::Yielded { .. } => "Yielded".into(),
            WarmEvent::ClosureStored { .. } => "ClosureStored".into(),
            WarmEvent::IndexProgress {
                anchor,
                progress,
                seal,
                ..
            } => format!(
                "IndexProgress {:?} {} seal={seal:?}",
                progress.mode,
                anchor_kind(anchor)
            ),
            WarmEvent::IndexStopped {
                anchor,
                mode,
                paused,
                ..
            } => format!(
                "IndexStopped {mode:?} {} paused={paused}",
                anchor_kind(anchor)
            ),
        }
    }

    #[test]
    fn a_closure_that_fills_the_cache_is_capped_so_an_index_batch_dispatches() {
        let (_temp, workspace, main, _base) = closure_fixture();
        let cache = workspace.project_cache();
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.open(main.clone());
        let events = run_until_end(&mut warmer, &workspace);
        let Some(WarmEvent::End { pins, .. }) = events
            .iter()
            .find(|event| matches!(event, WarmEvent::End { .. }))
        else {
            panic!("{events:?}");
        };
        // The open file's closure alone fills `maxCacheBytes` (multidev:
        // 2 GiB of pins, no index batch for 1,500 s).
        cache.set_max_bytes(cache.stats().bytes);
        cache.pin(&main, pins.clone());
        assert!(!cache.has_room(), "uncapped pins fill the budget");

        warmer.pin_crawl(cache, &main, pins.clone());
        let stats = cache.stats();
        assert!(
            stats.pinned_bytes <= cache.max_bytes() / 2,
            "crawl pins are capped at half the budget: {stats:?}"
        );
        assert!(stats.pinned_bytes > 0, "the closure keeps what fits");
        let events = poll_until(&mut warmer, &workspace, |events| {
            events.iter().any(is_index_event)
        });
        assert!(
            events.iter().any(is_index_event),
            "an index batch was dispatched: {events:?}"
        );
    }

    #[test]
    fn crawl_pins_unpin_the_least_recently_opened_closure_first() {
        let (temp, mut workspace, main, base) = closure_fixture();
        let other_text = "unit Other;\ninterface\nuses Base;\nimplementation\nend.\n".to_string();
        std::fs::write(temp.path().join("Other.pas"), &other_text).unwrap();
        let other = Url::from_file_path(temp.path().join("Other.pas")).unwrap();
        workspace
            .open_document(other.clone(), other_text, 1)
            .unwrap();
        let cache = workspace.project_cache();
        let mut warmer = Warmer::start(Arc::new(InteractiveGate::default()));
        warmer.open(main.clone());
        let main_pins = match run_until_end(&mut warmer, &workspace).last() {
            Some(WarmEvent::End { pins, .. }) => pins.clone(),
            other => panic!("{other:?}"),
        };
        warmer.open(other.clone());
        let other_pins = match run_until_end(&mut warmer, &workspace)
            .iter()
            .rev()
            .find(|event| matches!(event, WarmEvent::End { uri, .. } if *uri == other))
        {
            Some(WarmEvent::End { pins, .. }) => pins.clone(),
            other => panic!("{other:?}"),
        };
        assert!(other_pins.iter().any(|(uri, _)| *uri == base));
        cache.set_max_bytes(cache.stats().bytes);
        assert!(warmer.pin_crawl(cache, &main, main_pins).is_empty());
        let unpinned = warmer.pin_crawl(cache, &other, other_pins);
        assert_eq!(unpinned, vec![main], "Main was opened first");
        assert!(cache.stats().pinned_bytes <= cache.max_bytes() / 2);
    }

    /// Index scheduling with an enabled store (D33). Every test opens one.
    #[cfg(unix)]
    mod persisting {
        use super::*;
        use crate::persistent_cache::{CacheOptions, CacheState, PersistentCache};
        use crate::workspace::baseline::SealOutcome;

        fn write(path: &std::path::Path, text: &str) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        const MAIN_TEXT: &str = "unit Main;\ninterface\nuses Provider;\nimplementation\nend.\n";

        /// `App.dproj` searching `lib`, where `Main` uses `Provider`, plus
        /// `extra` units `U<n>`.
        fn write_project(root: &std::path::Path, extra: usize) {
            write(
                &root.join("App.dproj"),
                "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_UnitSearchPath>lib</DCC_UnitSearchPath></PropertyGroup><ItemGroup></ItemGroup></Project>",
            );
            write(&root.join("App.dpr"), "program App; begin end.\n");
            write(&root.join("lib/Main.pas"), MAIN_TEXT);
            write(
                &root.join("lib/Provider.pas"),
                "unit Provider;\ninterface\nprocedure Hello;\nimplementation\nprocedure Hello; begin end;\nend.\n",
            );
            for index in 0..extra {
                write(
                    &root.join(format!("lib/U{index}.pas")),
                    &format!(
                        "unit U{index};\ninterface\nprocedure P{index};\nimplementation\nprocedure P{index};\nbegin\nend;\nend.\n"
                    ),
                );
            }
            pascal_project::backdate_tree_for_test(root);
        }

        /// A workspace over `root` with the project configured, whose cache
        /// handle has an open store in `store`.
        fn persisting_workspace(
            root: &std::path::Path,
            store: &std::path::Path,
            gate: &Arc<InteractiveGate>,
        ) -> crate::workspace::Workspace {
            persisting_workspace_with(root, store, gate, Default::default())
        }

        /// `persisting_workspace` under `limits`.
        fn persisting_workspace_with(
            root: &std::path::Path,
            store: &std::path::Path,
            gate: &Arc<InteractiveGate>,
            limits: crate::workspace::ResourceLimits,
        ) -> crate::workspace::Workspace {
            let mut workspace = crate::workspace::Workspace::with_override_session(
                vec![root.to_path_buf()],
                crate::workspace::WorkspaceOptions {
                    project_file: Some(root.join("App.dproj")),
                    limits,
                    ..Default::default()
                },
                pascal_project::delphi_overrides::OverrideSession::new(None),
            );
            let cache = PersistentCache::start_unscheduled_for_test(
                CacheOptions {
                    enabled: true,
                    directory: Some(store.to_path_buf()),
                    ..CacheOptions::default()
                },
                workspace.project_cache().clone(),
                gate.clone(),
                pascal_cache::Namespace::new(pascal_cache::Digest::from_bytes([11; 32])),
            );
            assert_eq!(cache.settle(Duration::from_secs(5)), CacheState::Enabled);
            workspace.set_persistent_cache(cache);
            workspace
        }

        /// `write_project` in a fresh root, a cold store, `Main` open.
        fn fixture_persisting_with(
            extra: usize,
            gate: &Arc<InteractiveGate>,
        ) -> (
            tempfile::TempDir,
            tempfile::TempDir,
            crate::workspace::Workspace,
            Url,
        ) {
            let temp = tempfile::tempdir().unwrap();
            write_project(temp.path(), extra);
            let store = crate::workspace::persist::tests::private_dir();
            let mut workspace = persisting_workspace(temp.path(), store.path(), gate);
            let main = Url::from_file_path(temp.path().join("lib/Main.pas")).unwrap();
            workspace
                .open_document(main.clone(), MAIN_TEXT.to_string(), 1)
                .expect("open main unit");
            (temp, store, workspace, main)
        }

        fn fixture_persisting(
            gate: &Arc<InteractiveGate>,
        ) -> (
            tempfile::TempDir,
            tempfile::TempDir,
            crate::workspace::Workspace,
            Url,
        ) {
            fixture_persisting_with(2, gate)
        }

        fn is_full_progress(event: &WarmEvent) -> bool {
            matches!(event, WarmEvent::IndexProgress { progress, .. } if progress.mode == IndexMode::Full)
        }

        /// `write_project(2)` with the project configured and `Main` open,
        /// over a store whose open waits for the returned sender.
        fn opening_workspace(
            gate: &Arc<InteractiveGate>,
        ) -> (
            tempfile::TempDir,
            tempfile::TempDir,
            crate::workspace::Workspace,
            PersistentCache,
            std::sync::mpsc::Sender<()>,
            Url,
        ) {
            let temp = tempfile::tempdir().unwrap();
            write_project(temp.path(), 2);
            let store = crate::workspace::persist::tests::private_dir();
            let mut workspace = crate::workspace::Workspace::with_override_session(
                vec![temp.path().to_path_buf()],
                crate::workspace::WorkspaceOptions {
                    project_file: Some(temp.path().join("App.dproj")),
                    ..Default::default()
                },
                pascal_project::delphi_overrides::OverrideSession::new(None),
            );
            let (entered, release) = (std::sync::mpsc::channel(), std::sync::mpsc::channel());
            let cache = PersistentCache::start_paused_for_test(
                CacheOptions {
                    enabled: true,
                    directory: Some(store.path().to_path_buf()),
                    ..CacheOptions::default()
                },
                workspace.project_cache().clone(),
                gate.clone(),
                pascal_cache::Namespace::new(pascal_cache::Digest::from_bytes([11; 32])),
                entered.0,
                release.1,
            );
            entered.1.recv().unwrap();
            assert_eq!(cache.state(), CacheState::Opening);
            workspace.set_persistent_cache(cache.clone());
            let main = Url::from_file_path(temp.path().join("lib/Main.pas")).unwrap();
            workspace
                .open_document(main.clone(), MAIN_TEXT.to_string(), 1)
                .expect("open main unit");
            (temp, store, workspace, cache, release.0, main)
        }

        /// A batch dispatched while the store is still opening must not
        /// index memory-only: its members would keep no record for the rest
        /// of the process, so no snapshot is sealed and a restart extracts
        /// them again (protocol test `persistent_index`, l4d-zf1b.8). The
        /// store stays opening past `MAX_OPEN_WAIT`, so the reuse pass that
        /// waits for it goes back and the crawl, then extraction, go ahead
        /// (Task 10b: the wait ahead of the crawl is bounded).
        #[test]
        fn an_index_batch_dispatched_while_the_store_opens_persists_its_members() {
            let gate = Arc::new(InteractiveGate::default());
            let (_temp, _store, workspace, cache, release, main) = opening_workspace(&gate);
            let mut warmer = Warmer::start(gate);
            warmer.open(main.clone());
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            let mut events = Vec::new();
            while !warmer
                .index_in_flight
                .as_ref()
                .is_some_and(|(_, mode)| *mode == IndexMode::Full)
            {
                assert!(std::time::Instant::now() < deadline, "{events:?}");
                events.extend(warmer.poll(&workspace));
                std::thread::yield_now();
            }
            assert_eq!(
                cache.state(),
                CacheState::Opening,
                "dispatched while opening"
            );
            release.send(()).unwrap();
            events.extend(poll_until(&mut warmer, &workspace, |events| {
                events.iter().any(|event| {
                    matches!(event, WarmEvent::IndexProgress { progress, .. }
                        if progress.mode == IndexMode::Full && progress.complete)
                })
            }));
            let status = workspace.project_cache().index_status(&|_| false);
            let counts = status[0].counts;
            // The open, unedited `Main` is indexed and persisted from disk.
            assert_eq!(counts.fresh, counts.members, "{counts:?}");
            assert_eq!(counts.persisted, counts.members, "{counts:?}: {events:?}");
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    WarmEvent::IndexProgress {
                        seal: Some(SealOutcome::Sealed),
                        ..
                    }
                )),
                "{events:?}"
            );
            assert_eq!(cache.state(), CacheState::Enabled);
            assert!(
                events
                    .iter()
                    .filter_map(index_mode)
                    .any(|mode| mode == IndexMode::Reuse),
                "the store's reuse pass ran once it was enabled: {events:?}"
            );
            cache.shutdown();
        }

        /// The first crawl of a session is usually dispatched while the
        /// store still opens; it publishes once the store is enabled
        /// (R8-2/D42, protocol test
        /// `a_server_killed_during_the_crawl_reuses_what_the_crawl_published`).
        #[test]
        fn a_crawl_dispatched_while_the_store_opens_publishes_its_units() {
            let gate = Arc::new(InteractiveGate::default());
            let (temp, _store, workspace, cache, release, main) = opening_workspace(&gate);
            // Bulk work holds every index batch back: only the crawl runs.
            gate.set_running_bulk(1);
            let mut warmer = Warmer::start(gate.clone());
            warmer.open(main);
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            let mut events = Vec::new();
            while warmer.busy_attempt.is_none() {
                assert!(std::time::Instant::now() < deadline, "{events:?}");
                events.extend(warmer.poll(&workspace));
                std::thread::yield_now();
            }
            assert_eq!(
                cache.state(),
                CacheState::Opening,
                "dispatched while opening"
            );
            release.send(()).unwrap();
            events.extend(run_until_end_within(
                &mut warmer,
                &workspace,
                Duration::from_secs(20),
            ));
            assert!(!events.iter().any(is_index_event), "{events:?}");
            warmer.shutdown();
            assert_eq!(
                workspace.project_cache().published_by_crawl_for_test(),
                vec![temp.path().join("lib/Provider.pas")]
            );
            gate.set_running_bulk(0);
            cache.shutdown();
        }

        #[test]
        fn a_reuse_pass_runs_before_the_crawl_and_extraction_after() {
            let gate = Arc::new(InteractiveGate::default());
            let (_temp, _store, workspace, main) = fixture_persisting(&gate);
            let mut warmer = Warmer::start(gate);
            warmer.open(main.clone());
            let events = poll_until(&mut warmer, &workspace, |events| {
                events
                    .iter()
                    .position(|event| matches!(event, WarmEvent::End { .. }))
                    .is_some_and(|end| events[end..].iter().any(is_full_progress))
            });
            let reuse = events
                .iter()
                .position(is_index_event)
                .expect("a reuse batch ran");
            let end = events
                .iter()
                .position(|event| matches!(event, WarmEvent::End { .. }))
                .unwrap();
            assert!(reuse < end, "the reuse pass precedes the crawl: {events:?}");
            assert_eq!(index_mode(&events[reuse]), Some(IndexMode::Reuse));
            assert!(
                events[..end]
                    .iter()
                    .filter_map(index_mode)
                    .all(|mode| mode == IndexMode::Reuse),
                "nothing is extracted before the crawl: {events:?}"
            );
            let Some(WarmEvent::IndexProgress { progress, .. }) =
                events[end..].iter().find(|event| is_full_progress(event))
            else {
                unreachable!()
            };
            assert!(
                progress.indexed > 0,
                "extraction follows the crawl: {progress:?}"
            );
            assert!(warmer.reuse_done.contains(&IndexAnchor::Document(main)));
            workspace.persistent_cache().shutdown();
        }

        fn reuse_progress(event: &WarmEvent) -> bool {
            matches!(event, WarmEvent::IndexProgress { progress, .. } if progress.mode == IndexMode::Reuse)
        }

        fn position(events: &[WarmEvent], matching: impl Fn(&WarmEvent) -> bool) -> usize {
            events
                .iter()
                .position(matching)
                .unwrap_or_else(|| panic!("{events:?}"))
        }

        fn is_begin(event: &WarmEvent) -> bool {
            matches!(event, WarmEvent::Begin { .. })
        }

        fn is_end(event: &WarmEvent) -> bool {
            matches!(event, WarmEvent::End { .. })
        }

        /// I-1 (Task 10b): at a second start the store is usually still
        /// opening at the first poll after `didOpen`. The reuse pass is
        /// dispatched anyway and waits for the store; the crawl follows it.
        #[test]
        fn a_reuse_pass_due_while_the_store_opens_runs_before_the_crawl() {
            let gate = Arc::new(InteractiveGate::default());
            let (_temp, _store, workspace, cache, release, main) = opening_workspace(&gate);
            let mut warmer = Warmer::start(gate);
            warmer.open(main);
            let mut events = warmer.poll(&workspace);
            assert!(
                matches!(warmer.index_in_flight, Some((_, IndexMode::Reuse)))
                    && warmer.busy_attempt.is_none(),
                "the first job is a reuse pass, not the crawl: {:?} {:?}",
                warmer.index_in_flight,
                warmer.busy_attempt
            );
            assert_eq!(cache.state(), CacheState::Opening);
            release.send(()).unwrap();
            events.extend(run_until_end_within(
                &mut warmer,
                &workspace,
                Duration::from_secs(20),
            ));
            let reuse = position(&events, reuse_progress);
            assert!(reuse < position(&events, is_begin), "{events:?}");
            assert!(
                events[..reuse].iter().all(|event| !is_index_event(event)),
                "the pass ran as persisting, without a requeue: {events:?}"
            );
            cache.shutdown();
        }

        /// I-1 (b): diagnostics queued at `didOpen` hold the index gate. The
        /// reuse pass is dispatched anyway and waits for them, ahead of the
        /// crawl, which they would pause too.
        #[test]
        fn a_reuse_pass_held_by_interactive_work_still_runs_before_the_crawl() {
            let gate = Arc::new(InteractiveGate::default());
            let (_temp, _store, workspace, cache, release, main) = opening_workspace(&gate);
            gate.set(1);
            let mut warmer = Warmer::start(gate.clone());
            warmer.open(main);
            let mut events = warmer.poll(&workspace);
            assert!(
                matches!(warmer.index_in_flight, Some((_, IndexMode::Reuse)))
                    && warmer.busy_attempt.is_none(),
                "{:?} {:?}",
                warmer.index_in_flight,
                warmer.busy_attempt
            );
            wait_for_turn_waiter(&gate);
            assert!(
                warmer.events.try_recv().is_err(),
                "the pass waits for interactive work and does not yield to the crawl"
            );
            release.send(()).unwrap();
            gate.set(0);
            events.extend(run_until_end_within(
                &mut warmer,
                &workspace,
                Duration::from_secs(20),
            ));
            assert!(
                position(&events, reuse_progress) < position(&events, is_begin),
                "{events:?}"
            );
            cache.shutdown();
        }

        /// I-1 (c): Bulk work lets the crawl go first; once it ends, the crawl
        /// yields to the due reuse pass at its next unit boundary, then
        /// resumes and reaches its end.
        #[test]
        fn a_crawl_yields_to_a_due_reuse_pass_once_bulk_work_ends() {
            let gate = Arc::new(InteractiveGate::default());
            let (_temp, _store, workspace, main) = wide_fixture(&gate);
            gate.set_running_bulk(1);
            // Bulk work ends at the crawl's first unit boundary check.
            *gate.bulk_ends_after_checks.lock().unwrap() = Some(0);
            let mut warmer = Warmer::start(gate.clone());
            warmer.open(main);
            let events = run_until_end_within(&mut warmer, &workspace, Duration::from_secs(20));
            let kinds = events.iter().map(event_kind).collect::<Vec<_>>();
            let begin = position(&events, is_begin);
            let yielded = kinds
                .iter()
                .position(|kind| kind == "Yielded to reuse")
                .unwrap_or_else(|| panic!("the crawl yielded to the reuse pass: {kinds:?}"));
            let reuse = position(&events, reuse_progress);
            let end = position(&events, is_end);
            assert!(
                begin < yielded && yielded < reuse && reuse < end,
                "{kinds:?}"
            );
            assert!(
                matches!(events[yielded], WarmEvent::Yielded { done: 1, .. }),
                "at the first unit boundary: {kinds:?}"
            );
            assert!(
                events[reuse..end].iter().any(is_begin),
                "the crawl is dispatched again after the pass: {kinds:?}"
            );
            assert!(
                !kinds[..end]
                    .iter()
                    .any(|kind| kind.starts_with("IndexProgress Full")),
                "no extraction before the crawl's end: {kinds:?}"
            );
            assert!(warmer.reuse_queue.is_empty(), "{kinds:?}");
            warmer.shutdown();
            workspace.persistent_cache().shutdown();
        }

        /// I-1: a disabled store keeps the sequence of a workspace without
        /// one (`a_disabled_cache_runs_no_reuse_pass_and_extracts_after_the_crawl`).
        #[test]
        fn a_disabled_store_keeps_the_crawl_first_sequence() {
            let gate = Arc::new(InteractiveGate::default());
            let (_temp, mut workspace, main) = fixture();
            let store = crate::workspace::persist::tests::private_dir();
            let cache = PersistentCache::start_unscheduled_for_test(
                CacheOptions {
                    enabled: false,
                    directory: Some(store.path().to_path_buf()),
                    ..CacheOptions::default()
                },
                workspace.project_cache().clone(),
                gate.clone(),
                pascal_cache::Namespace::new(pascal_cache::Digest::from_bytes([11; 32])),
            );
            assert_eq!(cache.state(), CacheState::Disabled);
            workspace.set_persistent_cache(cache.clone());
            assert_disabled_sequence(&workspace, &main, gate);
            assert_eq!(cache.jobs_issued_for_test(), 0);
            cache.shutdown();
        }

        fn project_reuse_batches(events: &[WarmEvent]) -> Vec<&IndexProgress> {
            events
                .iter()
                .filter_map(|event| match event {
                    WarmEvent::IndexProgress {
                        anchor: IndexAnchor::Project(_),
                        progress,
                        ..
                    } if progress.mode == IndexMode::Reuse => Some(&**progress),
                    _ => None,
                })
                .collect()
        }

        #[test]
        fn a_large_warm_reuse_pass_runs_in_several_batches_before_the_crawl() {
            let gate = Arc::new(InteractiveGate::default());
            let temp = tempfile::tempdir().unwrap();
            write_project(temp.path(), 600);
            let store = crate::workspace::persist::tests::private_dir();
            // A first process indexes and persists every member.
            let first = persisting_workspace(temp.path(), store.path(), &gate);
            let mut view = Workspace::from_analysis_input(&first.analysis_input().disk_baseline());
            let project = IndexAnchor::Project(temp.path().join("App.dproj"));
            let progress = view
                .advance_workspace_index(
                    &project,
                    IndexBound::all(),
                    &mut || Ok(()),
                    &AtomicBool::new(false),
                )
                .expect("first session");
            assert!(
                progress.complete && progress.persisted > 512,
                "{progress:?}"
            );
            first.persistent_cache().shutdown();
            drop((view, first));

            let mut workspace = persisting_workspace(temp.path(), store.path(), &gate);
            let main = Url::from_file_path(temp.path().join("lib/Main.pas")).unwrap();
            workspace
                .open_document(main.clone(), MAIN_TEXT.to_string(), 1)
                .unwrap();
            let mut warmer = Warmer::start(gate);
            warmer.open(main);
            let events = run_until_end_within(&mut warmer, &workspace, Duration::from_secs(60));
            let end = events
                .iter()
                .position(|event| matches!(event, WarmEvent::End { .. }))
                .unwrap();
            let batches = project_reuse_batches(&events[..end]);
            assert!(
                batches.len() >= 2,
                "{} reuse batches: {events:?}",
                batches.len()
            );
            let reused = batches
                .iter()
                .map(|progress| progress.reused)
                .sum::<usize>();
            assert!(reused > 512, "the store's facts were reused: {reused}");
            workspace.persistent_cache().shutdown();
        }

        /// Every reuse batch before the crawl's `End`, of any anchor.
        fn reuse_batches_before_end(events: &[WarmEvent]) -> Vec<&IndexProgress> {
            let end = position(events, is_end);
            events[..end]
                .iter()
                .filter_map(|event| match event {
                    WarmEvent::IndexProgress { progress, .. }
                        if progress.mode == IndexMode::Reuse =>
                    {
                        Some(&**progress)
                    }
                    _ => None,
                })
                .collect()
        }

        /// The members of `write_project(root, _)` in membership (path) order.
        fn members_in_path_order(root: &std::path::Path) -> Vec<std::path::PathBuf> {
            let mut members = std::fs::read_dir(root.join("lib"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .chain(std::iter::once(root.join("App.dpr")))
                .collect::<Vec<_>>();
            members.sort_by_cached_key(|path| crate::workspace::rename::path_key(path));
            members
        }

        /// A5 (Task 13b): a cold store no longer ends the pass after one
        /// batch: every member is tried before the crawl, in bounded
        /// batches, and extraction follows the crawl.
        #[test]
        fn a_cold_store_reuse_pass_tries_every_member_before_the_crawl() {
            let gate = Arc::new(InteractiveGate::default());
            let (temp, _store, workspace, main) = fixture_persisting_with(600, &gate);
            let mut warmer = Warmer::start(gate);
            warmer.open(main);
            let events = run_until_end_within(&mut warmer, &workspace, Duration::from_secs(60));
            let batches = reuse_batches_before_end(&events);
            assert!(batches.len() >= 2, "{events:?}");
            assert_eq!(batches.iter().map(|batch| batch.reused).sum::<usize>(), 0);
            assert_eq!(
                batches.last().unwrap().reuse_pending,
                0,
                "every member was tried: {batches:?}"
            );
            let project = IndexAnchor::Project(temp.path().join("App.dproj"));
            assert!(warmer.reuse_done.contains(&project));
            assert!(
                warmer.index_queue.contains(&project)
                    || warmer.index_in_flight == Some((project, IndexMode::Full)),
                "extraction follows"
            );
            workspace.persistent_cache().shutdown();
        }

        /// A5 (Task 13b, R2 shape): the store holds records for members
        /// spread over the end of the path order only, so the first reuse
        /// batch misses every member it tries (all absent). The pass goes
        /// on and reuses every published member before the crawl's `End`.
        #[test]
        fn a_partly_filled_store_is_reused_past_an_absent_first_batch() {
            const ABSENT_PREFIX: usize = 530;
            let gate = Arc::new(InteractiveGate::default());
            let temp = tempfile::tempdir().unwrap();
            write_project(temp.path(), 600);
            let store = crate::workspace::persist::tests::private_dir();
            let first = persisting_workspace(temp.path(), store.path(), &gate);
            let mut view = Workspace::from_analysis_input(&first.analysis_input().disk_baseline());
            let project = IndexAnchor::Project(temp.path().join("App.dproj"));
            let progress = view
                .advance_workspace_index(
                    &project,
                    IndexBound::all(),
                    &mut || Ok(()),
                    &AtomicBool::new(false),
                )
                .expect("first session");
            assert!(progress.complete, "{progress:?}");
            first.persistent_cache().shutdown();
            drop((view, first));
            // New bytes have no record: every member of the prefix, and
            // every other one after it, is absent from the store.
            let main_path = temp.path().join("lib/Main.pas");
            let members = members_in_path_order(temp.path());
            assert!(members.len() > ABSENT_PREFIX + 40, "{}", members.len());
            // `Main` too: the index batches read the disk, open or not.
            let mut published = 0;
            for (at, path) in members.iter().enumerate() {
                if at < ABSENT_PREFIX || at % 2 == 0 {
                    let text = std::fs::read_to_string(path).unwrap();
                    write(path, &format!("{text}{{ edited }}\n"));
                } else {
                    published += 1;
                }
            }
            pascal_project::backdate_tree_for_test(temp.path());

            let mut workspace = persisting_workspace(temp.path(), store.path(), &gate);
            let main = Url::from_file_path(&main_path).unwrap();
            workspace
                .open_document(
                    main.clone(),
                    std::fs::read_to_string(&main_path).unwrap(),
                    1,
                )
                .unwrap();
            let mut warmer = Warmer::start(gate);
            warmer.open(main);
            let events = run_until_end_within(&mut warmer, &workspace, Duration::from_secs(60));
            let batches = reuse_batches_before_end(&events);
            let first_batch = batches.first().expect("a reuse batch ran");
            assert_eq!(first_batch.reused, 0, "{first_batch:?}");
            assert_eq!(
                first_batch.reuse_absent, first_batch.reuse_missed,
                "{first_batch:?}"
            );
            let reused = batches.iter().map(|batch| batch.reused).sum::<usize>();
            assert_eq!(reused, published, "{batches:?}");
            workspace.persistent_cache().shutdown();
        }

        /// A5 fix round 1 (Important 1): a sealed snapshot lists the first
        /// members in path order artifact-less (refused for the per-file
        /// limit), more than one reuse batch of them. A batch that only met
        /// such members still goes on, and every published member after them
        /// is reused before the crawl's `End`.
        #[test]
        fn a_reuse_pass_goes_on_past_a_batch_of_retrying_refusals() {
            const EXTRA: usize = 1200;
            const TAIL: usize = 60;
            let gate = Arc::new(InteractiveGate::default());
            let temp = tempfile::tempdir().unwrap();
            write_project(temp.path(), EXTRA);
            let members = members_in_path_order(temp.path());
            let refused_from = 3;
            let refused_to = members.len() - TAIL;
            assert!(refused_to - refused_from > 2 * 512, "{}", members.len());
            for path in &members[refused_from..refused_to] {
                assert!(path.file_stem().unwrap().to_str().unwrap().starts_with('U'));
                let text = std::fs::read_to_string(path).unwrap();
                write(path, &format!("{text}{{ {} }}\n", "x".repeat(400)));
            }
            pascal_project::backdate_tree_for_test(temp.path());
            let limits = || crate::workspace::ResourceLimits {
                max_file_bytes: 300,
                ..Default::default()
            };
            let store = crate::workspace::persist::tests::private_dir();
            let main = Url::from_file_path(temp.path().join("lib/Main.pas")).unwrap();

            let mut first = persisting_workspace_with(temp.path(), store.path(), &gate, limits());
            first
                .open_document(main.clone(), MAIN_TEXT.to_string(), 1)
                .unwrap();
            let mut warmer = Warmer::start(gate.clone());
            warmer.open(main.clone());
            let deadline = std::time::Instant::now() + Duration::from_secs(180);
            let mut events = Vec::new();
            while !events
                .iter()
                .any(|event| sealed(event, &SealOutcome::Sealed))
            {
                assert!(std::time::Instant::now() < deadline, "not sealed");
                events.extend(warmer.poll(&first));
                std::thread::yield_now();
            }
            warmer.shutdown();
            first.persistent_cache().shutdown();
            drop(first);

            let mut second = persisting_workspace_with(temp.path(), store.path(), &gate, limits());
            second
                .open_document(main.clone(), MAIN_TEXT.to_string(), 1)
                .unwrap();
            let mut warmer = Warmer::start(gate);
            warmer.open(main);
            let events = run_until_end_within(&mut warmer, &second, Duration::from_secs(120));
            let batches = reuse_batches_before_end(&events);
            let reused = batches.iter().map(|batch| batch.reused).sum::<usize>();
            assert_eq!(
                reused,
                members.len() - (refused_to - refused_from),
                "{batches:?}"
            );
            second.persistent_cache().shutdown();
        }

        /// A5 fix round 1 (Minor 2): a reuse batch that only met retryable
        /// misses is tried again, at most `MAX_REUSE_RETRIES` times in a row,
        /// before the pass ends; a batch that progressed resets the count.
        #[test]
        fn a_reuse_batch_of_retryable_misses_is_retried_a_bounded_number_of_times() {
            let gate = Arc::new(InteractiveGate::default());
            let (_temp, _store, workspace, main) = fixture_persisting(&gate);
            let mut warmer = Warmer::start(gate);
            let anchor = IndexAnchor::Document(main);
            let key = crate::workspace::ContextKey::for_test("/retry");
            let batch = |deferred: usize, missed: usize| {
                let mut progress = IndexProgress::new(key.clone(), IndexMode::Reuse);
                progress.reuse_deferred = deferred;
                progress.reuse_missed = missed;
                progress.reuse_pending = 10;
                progress.pending = 10;
                WarmEvent::IndexProgress {
                    anchor: anchor.clone(),
                    progress: Box::new(progress),
                    seal: None,
                    generation: 0,
                }
            };
            let accept = |warmer: &mut Warmer, event: &WarmEvent| {
                warmer.busy = true;
                assert!(warmer.accept_event(event, workspace.project_cache()));
                let requeued = warmer.reuse_queue.front() == Some(&anchor);
                warmer.reuse_queue.clear();
                requeued
            };
            for _ in 0..MAX_REUSE_RETRIES {
                assert!(accept(&mut warmer, &batch(5, 0)), "retried");
            }
            // Progress resets the count.
            assert!(accept(&mut warmer, &batch(5, 1)));
            for _ in 0..MAX_REUSE_RETRIES {
                assert!(accept(&mut warmer, &batch(5, 0)), "retried again");
            }
            assert!(!accept(&mut warmer, &batch(5, 0)), "the pass ends");
            assert!(warmer.reuse_done.contains(&anchor));
            workspace.persistent_cache().shutdown();
        }

        /// A5 (Task 13b, R4 shape): a first server's crawl publishes units
        /// spread over the end of the path order, and the server stops
        /// before any index batch (no seal, no snapshot to hint). The next
        /// start reuses all of them in its reuse pass, before its crawl.
        #[test]
        fn a_restart_reuses_every_unit_a_stopped_crawl_published() {
            let gate = Arc::new(InteractiveGate::default());
            let temp = tempfile::tempdir().unwrap();
            write_project(temp.path(), 600);
            let members = members_in_path_order(temp.path());
            let crawled = members[540..]
                .iter()
                .step_by(5)
                .filter_map(|path| {
                    let name = path.file_stem()?.to_str()?;
                    name.starts_with('U').then(|| name.to_owned())
                })
                .collect::<Vec<_>>();
            assert!(crawled.len() >= 10, "{crawled:?}");
            let main_text = format!(
                "unit Main;\ninterface\nuses {};\nimplementation\nend.\n",
                crawled.join(", ")
            );
            let main_path = temp.path().join("lib/Main.pas");
            write(&main_path, &main_text);
            pascal_project::backdate_tree_for_test(temp.path());
            let store = crate::workspace::persist::tests::private_dir();
            let main = Url::from_file_path(&main_path).unwrap();

            let mut first = persisting_workspace(temp.path(), store.path(), &gate);
            first
                .open_document(main.clone(), main_text.clone(), 1)
                .unwrap();
            // Bulk work holds every index batch back: only the crawl runs.
            gate.set_running_bulk(1);
            let mut warmer = Warmer::start(gate.clone());
            warmer.open(main.clone());
            let events = run_until_end_within(&mut warmer, &first, Duration::from_secs(60));
            assert!(!events.iter().any(is_index_event), "{events:?}");
            warmer.shutdown();
            let published = first.project_cache().published_by_crawl_for_test();
            assert_eq!(published.len(), crawled.len(), "{published:?}");
            first.persistent_cache().shutdown();
            drop(first);
            gate.set_running_bulk(0);

            let mut second = persisting_workspace(temp.path(), store.path(), &gate);
            second.open_document(main.clone(), main_text, 1).unwrap();
            let mut warmer = Warmer::start(gate);
            warmer.open(main);
            let events = run_until_end_within(&mut warmer, &second, Duration::from_secs(60));
            let batches = reuse_batches_before_end(&events);
            let reused = batches.iter().map(|batch| batch.reused).sum::<usize>();
            assert_eq!(reused, published.len(), "{batches:?}");
            second.persistent_cache().shutdown();
        }

        #[test]
        fn a_crawl_runs_while_bulk_work_holds_the_reuse_pass_back() {
            let gate = Arc::new(InteractiveGate::default());
            let (_temp, _store, workspace, main) = fixture_persisting(&gate);
            gate.set_running_bulk(1);
            let mut warmer = Warmer::start(gate.clone());
            warmer.open(main);
            let events = run_until_end_within(&mut warmer, &workspace, Duration::from_secs(20));
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, WarmEvent::Begin { .. })),
                "{events:?}"
            );
            assert!(
                !events.iter().any(is_index_event),
                "no index batch ran or waited ahead of the crawl: {events:?}"
            );
            assert!(!warmer.reuse_queue.is_empty(), "the reuse pass still waits");

            gate.set_running_bulk(0);
            let events = poll_until(&mut warmer, &workspace, |events| {
                events
                    .iter()
                    .any(|event| index_mode(event) == Some(IndexMode::Reuse))
            });
            assert!(events.iter().any(is_index_event));
            workspace.persistent_cache().shutdown();
        }

        fn run_until_end_within(
            warmer: &mut Warmer,
            workspace: &crate::workspace::Workspace,
            limit: Duration,
        ) -> Vec<WarmEvent> {
            let deadline = std::time::Instant::now() + limit;
            let mut events = Vec::new();
            while std::time::Instant::now() < deadline {
                events.extend(warmer.poll(workspace));
                if events
                    .iter()
                    .any(|event| matches!(event, WarmEvent::End { .. }))
                {
                    return events;
                }
                std::thread::yield_now();
            }
            panic!("warmer did not finish: {events:?}");
        }

        fn sealed(event: &WarmEvent, outcome: &SealOutcome) -> bool {
            matches!(
                event,
                WarmEvent::IndexProgress { anchor: IndexAnchor::Project(_), seal: Some(seal), .. }
                    if seal == outcome
            )
        }

        #[test]
        fn a_complete_batch_seals_and_an_interrupted_seal_requeues() {
            let gate = Arc::new(InteractiveGate::default());
            let (temp, _store, workspace, main) = fixture_persisting(&gate);
            // The first seal finds interactive work queued and stops.
            gate.busy_before_seal.store(true, Ordering::Relaxed);
            let mut warmer = Warmer::start(gate.clone());
            warmer.open(main);
            let events = poll_until(&mut warmer, &workspace, |events| {
                events
                    .iter()
                    .any(|event| sealed(event, &SealOutcome::Interrupted))
            });
            assert!(
                !events
                    .iter()
                    .any(|event| sealed(event, &SealOutcome::Sealed)),
                "{events:?}"
            );
            let project = IndexAnchor::Project(temp.path().join("App.dproj"));
            assert!(
                warmer.index_queue.contains(&project) && !warmer.busy,
                "the interrupted anchor was requeued, and waits for the gate"
            );
            let key = events
                .iter()
                .find_map(|event| match event {
                    WarmEvent::IndexProgress {
                        progress,
                        seal: Some(_),
                        ..
                    } => progress.key.clone(),
                    _ => None,
                })
                .unwrap();
            assert!(workspace.project_cache().baseline_hint(&key).is_none());

            gate.set(0);
            let events = poll_until(&mut warmer, &workspace, |events| {
                events
                    .iter()
                    .any(|event| sealed(event, &SealOutcome::Sealed))
            });
            assert!(
                events
                    .iter()
                    .any(|event| sealed(event, &SealOutcome::Sealed))
            );
            let hint = workspace
                .project_cache()
                .baseline_hint(&key)
                .expect("sealed");
            assert!(hint.sealed() && hint.held());
            workspace.persistent_cache().shutdown();
        }

        /// Task 10b: two servers sealing at once can find the store `Busy`
        /// (protocol test `two_servers_index_the_same_workspace`). The
        /// bypassed seal is tried again; without it the context would never
        /// seal in this process.
        #[test]
        fn a_seal_the_store_bypassed_is_tried_again() {
            let gate = Arc::new(InteractiveGate::default());
            let (_temp, _store, workspace, main) = fixture_persisting(&gate);
            gate.busy_store_before_seal.store(true, Ordering::Relaxed);
            let mut warmer = Warmer::start(gate.clone());
            warmer.open(main);
            let events = poll_until(&mut warmer, &workspace, |events| {
                events
                    .iter()
                    .any(|event| sealed(event, &SealOutcome::Sealed))
            });
            let bypassed = position(&events, |event| sealed(event, &SealOutcome::Bypassed));
            assert!(
                bypassed < position(&events, |event| sealed(event, &SealOutcome::Sealed)),
                "{events:?}"
            );
            workspace.persistent_cache().shutdown();
        }

        /// Task 11 fix round 2 (C): a publication refused for room while
        /// the pressure outlasts the batches' waits. The batch that settles
        /// nothing is not requeued, so the warmer asks for the anchor again
        /// itself; once the pressure clears, the member is proven and the
        /// context seals with no request from outside.
        #[test]
        fn members_refused_for_room_are_proven_once_the_pressure_outlasts_a_wait() {
            let gate = Arc::new(InteractiveGate::default());
            let (_temp, _store, workspace, main) = fixture_persisting(&gate);
            let cache = workspace.persistent_cache().clone();
            let mut refused = false;
            cache.set_publish_fault_for_test(Some(Box::new(move |path| {
                (path.ends_with("App.dpr") && !std::mem::replace(&mut refused, true))
                    .then_some(pascal_cache::Bypass::WalPressure)
            })));
            // Two waits find no room; the next look finds it cleared.
            cache.set_pressure_for_test(Some(Box::new(|waits| {
                if waits >= 2 {
                    pascal_cache::Pressure::None
                } else {
                    pascal_cache::Pressure::WalPressure
                }
            })));
            let mut warmer = Warmer::start(gate.clone());
            warmer.open(main);
            let events = poll_until(&mut warmer, &workspace, |events| {
                events
                    .iter()
                    .any(|event| sealed(event, &SealOutcome::Sealed))
            });
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    WarmEvent::IndexProgress { progress, .. } if progress.awaiting_room > 0
                )),
                "{events:?}"
            );
            assert!(cache.room_waits_for_test() >= 2);
            workspace.persistent_cache().shutdown();
        }

        const WIDE_MAIN: &str =
            "unit Main;\ninterface\nuses Provider, U0, U1;\nimplementation\nend.\n";

        /// `fixture_persisting_with(2)`, where `Main` uses `Provider`, `U0`
        /// and `U1`.
        fn wide_fixture(
            gate: &Arc<InteractiveGate>,
        ) -> (
            tempfile::TempDir,
            tempfile::TempDir,
            crate::workspace::Workspace,
            Url,
        ) {
            let temp = tempfile::tempdir().unwrap();
            write_project(temp.path(), 2);
            write(&temp.path().join("lib/Main.pas"), WIDE_MAIN);
            pascal_project::backdate_tree_for_test(temp.path());
            let store = crate::workspace::persist::tests::private_dir();
            let mut workspace = persisting_workspace(temp.path(), store.path(), gate);
            let main = Url::from_file_path(temp.path().join("lib/Main.pas")).unwrap();
            workspace
                .open_document(main.clone(), WIDE_MAIN.to_string(), 1)
                .expect("open main unit");
            (temp, store, workspace, main)
        }

        fn sorted(mut paths: Vec<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
            paths.sort();
            paths
        }

        #[test]
        fn a_crawl_publishes_the_disk_units_it_analyses() {
            let gate = Arc::new(InteractiveGate::default());
            let (temp, store, workspace, main) = wide_fixture(&gate);
            // Bulk work holds every index batch back: only the crawl runs.
            gate.set_running_bulk(1);
            let mut warmer = Warmer::start(gate.clone());
            warmer.open(main);
            let events = run_until_end_within(&mut warmer, &workspace, Duration::from_secs(20));
            assert!(!events.iter().any(is_index_event), "{events:?}");
            warmer.shutdown();
            let lib = temp.path().join("lib");
            let closure = vec![
                lib.join("Provider.pas"),
                lib.join("U0.pas"),
                lib.join("U1.pas"),
            ];
            assert_eq!(
                sorted(workspace.project_cache().published_by_crawl_for_test()),
                closure
            );
            workspace.persistent_cache().shutdown();

            gate.set_running_bulk(0);
            let restarted = persisting_workspace(temp.path(), store.path(), &gate);
            let mut view =
                Workspace::from_analysis_input(&restarted.analysis_input().disk_baseline());
            let project = IndexAnchor::Project(temp.path().join("App.dproj"));
            crate::navigation::test_reset_document_parse_count();
            let progress = view
                .advance_workspace_index(
                    &project,
                    IndexBound::all().with_mode(IndexMode::Reuse),
                    &mut || Ok(()),
                    &AtomicBool::new(false),
                )
                .expect("restarted reuse pass");
            assert_eq!(crate::navigation::test_document_parse_count(), 0);
            assert_eq!(progress.reused, closure.len(), "{progress:?}");
            let key = progress.key.clone().expect("a context");
            for path in &closure {
                assert!(
                    restarted
                        .project_cache()
                        .index_member_has_record(&key, path),
                    "{} was reused and proven",
                    path.display()
                );
            }
            restarted.persistent_cache().shutdown();
        }

        #[test]
        fn a_crawl_under_an_overlaid_project_file_publishes_nothing_and_interleaves() {
            let gate = Arc::new(InteractiveGate::default());
            let (temp, _store, mut workspace, main) = wide_fixture(&gate);
            // `.dproj` buffers are not held; the main source is metadata.
            workspace
                .open_document(
                    Url::from_file_path(temp.path().join("App.dpr")).unwrap(),
                    "program App; { unsaved } begin end.\n".to_owned(),
                    1,
                )
                .unwrap();
            // Every crawl slice is past its 2 s.
            gate.crawl_slice_over.store(true, Ordering::Relaxed);
            let mut warmer = Warmer::start(gate.clone());
            warmer.open(main);
            let events = run_until_end_within(&mut warmer, &workspace, Duration::from_secs(30));
            let kinds = events.iter().map(event_kind).collect::<Vec<_>>();
            let yielded = kinds
                .iter()
                .position(|kind| kind == "Yielded")
                .unwrap_or_else(|| panic!("the crawl yielded: {kinds:?}"));
            let full = kinds
                .iter()
                .position(|kind| kind.starts_with("IndexProgress Full"))
                .unwrap_or_else(|| panic!("a Full batch ran: {kinds:?}"));
            let end = kinds.iter().position(|kind| kind == "End").unwrap();
            assert!(yielded < full && full < end, "{kinds:?}");
            assert!(
                kinds[..yielded]
                    .iter()
                    .all(|kind| !kind.starts_with("IndexProgress Full")),
                "no extraction before the crawl yields: {kinds:?}"
            );
            assert!(
                kinds[full..end].iter().any(|kind| kind == "Begin"),
                "the crawl is dispatched again after the batch: {kinds:?}"
            );
            assert!(
                !kinds.iter().any(|kind| kind == "Paused"),
                "no client preemption is reported: {kinds:?}"
            );
            assert!(
                workspace
                    .project_cache()
                    .published_by_crawl_for_test()
                    .is_empty()
            );
            warmer.shutdown();
            workspace.persistent_cache().shutdown();
        }

        /// Fix round 1: a crawl dispatched again replays its done units as
        /// cache hits; it yields only after new units, so it reaches `End`
        /// even while extraction stays queued.
        #[test]
        fn a_yielding_crawl_still_reaches_its_end_while_extraction_stays_queued() {
            let gate = Arc::new(InteractiveGate::default());
            let (temp, _store, mut workspace, main) = wide_fixture(&gate);
            workspace
                .open_document(
                    Url::from_file_path(temp.path().join("App.dpr")).unwrap(),
                    "program App; { unsaved } begin end.\n".to_owned(),
                    1,
                )
                .unwrap();
            gate.crawl_slice_over.store(true, Ordering::Relaxed);
            // Bulk work holds every index batch back: the queued extraction
            // never runs, so it stays queued.
            gate.set_running_bulk(1);
            let project = IndexAnchor::Project(temp.path().join("App.dproj"));
            let mut warmer = Warmer::start(gate.clone());
            warmer.request_index(project.clone());
            warmer.open(main);
            let events = run_until_end_within(&mut warmer, &workspace, Duration::from_secs(20));
            let yields = events
                .iter()
                .filter_map(|event| match event {
                    WarmEvent::Yielded { done, .. } => Some(*done),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert!(
                yields.len() >= 2 && yields.windows(2).all(|pair| pair[1] == pair[0] + 1),
                "every slice analyses a new unit: {yields:?}"
            );
            assert_eq!(yields[0], 1, "{yields:?}");
            assert!(!events.iter().any(is_index_event), "{events:?}");
            assert!(warmer.index_queue.contains(&project), "still queued");
            assert!(
                warmer.interleave,
                "the interleave the gate held back is still due"
            );
            warmer.shutdown();
            gate.set_running_bulk(0);
            workspace.persistent_cache().shutdown();
        }

        #[test]
        fn a_disk_only_crawl_does_not_yield_to_queued_extraction() {
            let gate = Arc::new(InteractiveGate::default());
            let (_temp, _store, workspace, main) = wide_fixture(&gate);
            gate.crawl_slice_over.store(true, Ordering::Relaxed);
            let mut warmer = Warmer::start(gate.clone());
            warmer.open(main);
            let events = run_until_end_within(&mut warmer, &workspace, Duration::from_secs(20));
            let kinds = events.iter().map(event_kind).collect::<Vec<_>>();
            assert!(!kinds.iter().any(|kind| kind == "Yielded"), "{kinds:?}");
            assert_eq!(
                workspace
                    .project_cache()
                    .published_by_crawl_for_test()
                    .len(),
                3
            );
            warmer.shutdown();
            workspace.persistent_cache().shutdown();
        }
    }
}

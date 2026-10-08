use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use lsp_server::{Message, Request, RequestId, Response};
use lsp_types::{ProgressToken, Url};

use crate::workspace::Workspace;
use crate::workspace::rename::WorkspaceInput;

#[derive(Default)]
pub(crate) struct InteractiveGate {
    busy: Mutex<usize>,
    idle: Condvar,
    /// Test seam: after this many passing `try_wait_idle` checks, mark the
    /// gate busy so a pause lands at an exact unit boundary.
    #[cfg(test)]
    busy_after_checks: Mutex<Option<usize>>,
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

    /// Checks whether warming may proceed without waiting while it owns a cache claim.
    pub(crate) fn try_wait_idle(&self, cancel: &AtomicBool) -> bool {
        #[cfg(test)]
        {
            let mut remaining = self
                .busy_after_checks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match remaining.as_mut() {
                Some(0) => {
                    *remaining = None;
                    drop(remaining);
                    self.set(1);
                }
                Some(checks) => *checks -= 1,
                None => {}
            }
        }
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        *self.busy.lock().unwrap_or_else(PoisonError::into_inner) == 0
            && !cancel.load(Ordering::Relaxed)
    }
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
            WarmEvent::ClosureStored { .. } => Ok(()),
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

struct WarmJob {
    uri: Url,
    input: WorkspaceInput,
    cancel: Arc<AtomicBool>,
    generation: u64,
    open_epoch: u64,
}

pub(crate) struct Warmer {
    queue: WarmQueue,
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
        let thread = std::thread::Builder::new()
            .name("PascalLspWarmer".to_string())
            .spawn(move || {
                lower_thread_priority();
                for job in job_receiver {
                    run_job(job, gate.clone(), &event_sender);
                }
            })
            .expect("spawn warmer thread");
        Self {
            queue: WarmQueue::default(),
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
        }
    }

    /// Schedules another attempt for an already-open file without changing
    /// its identity. Watcher invalidations are not document reopenings.
    pub(crate) fn rewarm(&mut self, uri: Url) {
        crate::trace::trace!("warm rewarm {uri}");
        self.queue.push_front(uri);
    }

    /// Re-crawls an open file whose snapshot found part of its interface
    /// closure uncached, unless it was already crawled, or a crawl is in
    /// flight, since the cache was last invalidated. Otherwise a unit that can
    /// never be cached would trigger a crawl on every request.
    pub(crate) fn request_closure_crawl(&mut self, uri: Url, invalidation_epoch: u64) {
        let in_flight = self
            .busy_attempt
            .as_ref()
            .is_some_and(|(busy_uri, _)| busy_uri == &uri)
            && self.dispatched_epochs.get(&uri) == Some(&invalidation_epoch);
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

    pub(crate) fn close(&mut self, uri: &Url, cache: &crate::project_cache::ProjectCache) {
        self.queue.remove(uri);
        self.dispatched_epochs.remove(uri);
        self.crawled_epochs.remove(uri);
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
        self.dispatched_epochs.clear();
        self.crawled_epochs.clear();
        let open = open.into_iter().collect::<HashSet<_>>();
        self.open_order.retain(|uri| open.contains(uri));
        self.open_epochs.retain(|uri, _| open.contains(uri));
        for uri in self.open_order.iter().rev() {
            self.queue.push_front(uri.clone());
        }
    }

    pub(crate) fn is_idle(&self) -> bool {
        !self.busy && self.queue.is_empty()
    }

    pub(crate) fn poll(&mut self, workspace: &Workspace) -> Vec<WarmEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            match &event {
                WarmEvent::Progress { .. } => {}
                WarmEvent::End { uri, pins, .. } => crate::trace::trace!(
                    "warm end {uri}: {} pins, cache epoch {}",
                    pins.len(),
                    workspace.project_cache().invalidation_epoch()
                ),
                other => crate::trace::trace!("warm event {other:?}"),
            }
            if self.accept_event(&event, workspace.project_cache()) {
                events.push(event);
            }
        }
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
                let job = WarmJob {
                    uri,
                    input: workspace.analysis_input(),
                    cancel: self.cancel.clone(),
                    generation: self.generation,
                    open_epoch,
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
        events
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
        self.busy = false;
        self.busy_attempt = None;

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

fn run_job(job: WarmJob, gate: Arc<InteractiveGate>, events: &Sender<WarmEvent>) {
    let end = |fingerprint, pins| {
        let _ = events.send(WarmEvent::End {
            uri: job.uri.clone(),
            fingerprint,
            pins,
            generation: job.generation,
            open_epoch: job.open_epoch,
        });
    };
    if !gate.wait_idle(&job.cancel) {
        return end(None, Vec::new());
    }
    let _span = crate::trace::Span::new(|| format!("warm crawl {}", job.uri));
    let mut workspace = Workspace::from_analysis_input(&job.input);
    let hook_events = events.clone();
    let hook_uri = job.uri.clone();
    let hook_generation = job.generation;
    let hook_open_epoch = job.open_epoch;
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
    let result = workspace.warm_with_cancel(&job.uri, &job.cancel);
    // Entries stored before a pause or error are already in the shared cache,
    // and a retry will not store them again, so report them now.
    if workspace.interface_entries_stored() > 0 {
        let _ = events.send(WarmEvent::ClosureStored {
            uri: job.uri.clone(),
            generation: job.generation,
            open_epoch: job.open_epoch,
        });
    }
    match result {
        Ok(outcome) => {
            let pins = outcome
                .fingerprint
                .map(|fingerprint| {
                    std::iter::once(job.uri.clone())
                        .chain(outcome.dependencies)
                        .chain(outcome.closure)
                        .map(|uri| (uri, fingerprint))
                        .collect()
                })
                .unwrap_or_default();
            end(outcome.fingerprint, pins)
        }
        Err(_) if paused.load(Ordering::Relaxed) => {
            let _ = events.send(WarmEvent::Paused {
                uri: job.uri,
                generation: job.generation,
                open_epoch: job.open_epoch,
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

        warmer.request_closure_crawl(main.clone(), epoch);
        assert!(warmer.queue.is_empty(), "this epoch was already crawled");

        warmer.request_closure_crawl(main, epoch + 1);
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

        warmer.request_closure_crawl(main.clone(), epoch);
        assert!(
            warmer.queue.is_empty(),
            "the in-flight crawl already covers this epoch"
        );

        warmer.request_closure_crawl(main, epoch + 1);
        assert!(
            !warmer.queue.is_empty(),
            "a newer epoch allows another crawl"
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
        warmer.request_closure_crawl(main.clone(), cache.invalidation_epoch());
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
    fn interactive_lookup_can_proceed_after_warm_releases_import_claim() {
        let (_temp, workspace, main) = fixture();
        let input = workspace.analysis_input();
        let gate = Arc::new(InteractiveGate::default());
        let warm_cancel = Arc::new(AtomicBool::new(false));
        let hook_gate = gate.clone();
        let hook_cancel = warm_cancel.clone();
        let (claim_acquired_tx, claim_acquired_rx) = std::sync::mpsc::channel();
        let (resume_hook_tx, resume_hook_rx) = crossbeam_channel::bounded(1);
        let warm_uri = main.clone();
        let mut warming_workspace = Workspace::from_analysis_input(&input);
        warming_workspace.set_dependency_hook(Arc::new(move |_, _, _| {
            hook_gate.set(1);
            claim_acquired_tx.send(()).unwrap();
            resume_hook_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("test releases the dependency hook");
            if hook_gate.try_wait_idle(&hook_cancel) {
                Ok(())
            } else {
                Err(crate::workspace::rename::CANCELLATION_MESSAGE.to_string())
            }
        }));
        let warm_thread =
            std::thread::spawn(move || warming_workspace.warm_with_cancel(&warm_uri, &warm_cancel));
        claim_acquired_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("warmer reached the dependency hook while holding its imports claim");

        let interactive_input = workspace.analysis_input();
        let interactive_uri = main.clone();
        let (interactive_started_tx, interactive_started_rx) = std::sync::mpsc::channel();
        let (interactive_done_tx, interactive_done_rx) = std::sync::mpsc::channel();
        let interactive_thread = std::thread::spawn(move || {
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
        std::thread::sleep(Duration::from_millis(50));
        resume_hook_tx.send(()).unwrap();

        let completed_while_gate_busy = interactive_done_rx.recv_timeout(Duration::from_secs(2));
        let completed_before_release = completed_while_gate_busy.is_ok();
        gate.set(0);
        let warm_result = warm_thread.join().unwrap();
        let interactive_result = match completed_while_gate_busy {
            Ok(result) => result,
            Err(_) => interactive_done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("interactive request finishes after releasing the gate"),
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
}

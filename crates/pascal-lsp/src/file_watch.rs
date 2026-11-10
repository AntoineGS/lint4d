use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lsp_types::Url;
use notify::event::ModifyKind;
use notify::{RecursiveMode, Watcher};

use crate::project_cache::ProjectCache;

/// Distinct changed paths held between two drains. A burst of more (a
/// branch switch over the whole tree) is an overflow; a build or a pull
/// usually changes far fewer distinct paths than it reports events, each
/// write reporting one or two (l4d-zh7i: Delphi builds of the multidev
/// tree wrote 1,200-3,350 compiled units beside the sources and overflowed
/// a queue of 1,024 events, staling every index member).
pub(crate) const MAX_PENDING_WATCH_PATHS: usize = 32 * 1024;
pub(crate) const MAX_WATCH_EVENTS_PER_TURN: usize = 256;

/// Non-recursive directory watch registration. Returns `false` when the
/// directory cannot be watched; callers then rely on stamp probes alone.
pub(crate) trait DirectoryWatch: Send {
    fn watch(&mut self, directory: &Path) -> bool;
    fn unwatch(&mut self, directory: &Path);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatchEvent {
    /// Data or metadata changed without changing directory membership.
    Modified(Vec<PathBuf>),
    /// A path was created, removed, renamed, or changed in an unknown way.
    Changed(Vec<PathBuf>),
    Overflow,
}

/// How a pending path changed: `Changed` covers what `Modified` does and
/// also the parent's listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Modified,
    Changed,
}

/// The paths changed since the last drain, each once, in the order they
/// first changed (l4d-zh7i: a burst is applied once per path).
#[derive(Default)]
struct Pending {
    order: VecDeque<PathBuf>,
    changes: HashMap<PathBuf, Change>,
    overflowed: bool,
}

#[derive(Default)]
struct Queue {
    pending: Mutex<Pending>,
    #[cfg(test)]
    arrived: Condvar,
}

impl Queue {
    fn pending(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, event: WatchEvent) {
        let mut pending = self.pending();
        let (paths, change) = match event {
            WatchEvent::Overflow => {
                overflow(&mut pending);
                drop(pending);
                self.notify();
                return;
            }
            WatchEvent::Modified(paths) => (paths, Change::Modified),
            WatchEvent::Changed(paths) => (paths, Change::Changed),
        };
        for path in paths {
            if let Some(known) = pending.changes.get_mut(&path) {
                if change == Change::Changed {
                    *known = Change::Changed;
                }
                continue;
            }
            if pending.changes.len() >= MAX_PENDING_WATCH_PATHS {
                overflow(&mut pending);
                break;
            }
            pending.changes.insert(path.clone(), change);
            pending.order.push_back(path);
        }
        drop(pending);
        self.notify();
    }

    fn notify(&self) {
        #[cfg(test)]
        self.arrived.notify_all();
    }
}

/// An overflow loses which paths changed; what is pending is covered by it.
fn overflow(pending: &mut Pending) {
    pending.overflowed = true;
    pending.order.clear();
    pending.changes.clear();
}

pub(crate) struct WatchEventSender {
    queue: Arc<Queue>,
}

impl WatchEventSender {
    fn send(&self, event: WatchEvent) {
        self.queue.push(event);
    }
}

pub(crate) struct WatchEvents {
    queue: Arc<Queue>,
    /// Paths applied and overflows applied, for `pascal/testBackgroundIdle`.
    applied: AtomicU64,
    overflows: AtomicU64,
    /// Set when a drain applied an overflow, until `take_overflowed`.
    overflowed: AtomicBool,
    /// Every path applied, for `pascal/testBackgroundIdle`.
    #[cfg(feature = "test-support")]
    applied_paths: Mutex<HashSet<PathBuf>>,
}

impl WatchEvents {
    pub(crate) fn channel() -> (WatchEventSender, Self) {
        let queue = Arc::new(Queue::default());
        (
            WatchEventSender {
                queue: Arc::clone(&queue),
            },
            Self {
                queue,
                applied: AtomicU64::new(0),
                overflows: AtomicU64::new(0),
                overflowed: AtomicBool::new(false),
                #[cfg(feature = "test-support")]
                applied_paths: Mutex::default(),
            },
        )
    }

    /// Apply at most `limit` changed paths (an overflow counts as one), each
    /// path once however many events reported it. Paths `ignored` holds
    /// (the store's own files, l4d-vrw0) are dropped before anything is
    /// invalidated.
    pub(crate) fn drain(
        &self,
        cache: &ProjectCache,
        limit: usize,
        ignored: &dyn Fn(&Path) -> bool,
    ) -> Vec<Url> {
        let mut remaining = limit;
        let (overflowed, taken) = {
            let mut pending = self.queue.pending();
            let overflowed = remaining > 0 && std::mem::take(&mut pending.overflowed);
            if overflowed {
                remaining -= 1;
            }
            let mut taken = Vec::new();
            while taken.len() < remaining {
                let Some(path) = pending.order.pop_front() else {
                    break;
                };
                let change = pending
                    .changes
                    .remove(&path)
                    .expect("a pending path has its change");
                taken.push((path, change));
            }
            (overflowed, taken)
        };
        if overflowed {
            apply_watch_event(cache, WatchEvent::Overflow);
            self.overflows.fetch_add(1, Ordering::Relaxed);
            self.overflowed.store(true, Ordering::Relaxed);
        }
        let mut affected = Vec::new();
        let mut seen = HashSet::new();
        for (path, change) in taken {
            if ignored(&path) {
                continue;
            }
            self.applied.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "test-support")]
            self.applied_paths
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(path.clone());
            let event = match change {
                Change::Modified => WatchEvent::Modified(vec![path]),
                Change::Changed => WatchEvent::Changed(vec![path]),
            };
            for uri in apply_watch_event(cache, event) {
                if seen.insert(uri.clone()) {
                    affected.push(uri);
                }
            }
        }
        affected
    }

    /// Whether a drain applied an overflow since the last call. The paths
    /// pending at the overflow are not known any more, so the caller rewarms
    /// every open document, as their events would have.
    pub(crate) fn take_overflowed(&self) -> bool {
        self.overflowed.swap(false, Ordering::Relaxed)
    }

    /// Whether changes wait for a drain.
    #[cfg(feature = "test-support")]
    pub(crate) fn pending(&self) -> bool {
        let pending = self.queue.pending();
        pending.overflowed || !pending.order.is_empty()
    }

    /// Whether `path` was applied since the last call that returned true
    /// for it.
    #[cfg(feature = "test-support")]
    pub(crate) fn take_applied(&self, path: &Path) -> bool {
        self.applied_paths
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(path)
    }

    /// How many paths and how many overflows were applied so far.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn applied(&self) -> (u64, u64) {
        (
            self.applied.load(Ordering::Relaxed),
            self.overflows.load(Ordering::Relaxed),
        )
    }

    /// The next pending change as one event, without applying it, waiting
    /// at most `timeout` for one.
    #[cfg(test)]
    fn recv_timeout(&self, timeout: std::time::Duration) -> Option<WatchEvent> {
        let deadline = std::time::Instant::now() + timeout;
        let mut pending = self.queue.pending();
        loop {
            if std::mem::take(&mut pending.overflowed) {
                return Some(WatchEvent::Overflow);
            }
            if let Some(path) = pending.order.pop_front() {
                return Some(match pending.changes.remove(&path) {
                    Some(Change::Modified) => WatchEvent::Modified(vec![path]),
                    _ => WatchEvent::Changed(vec![path]),
                });
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            pending = self
                .queue
                .arrived
                .wait_timeout(pending, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

pub(crate) struct NotifyWatcher {
    inner: notify::RecommendedWatcher,
}

impl NotifyWatcher {
    pub(crate) fn start(events: WatchEventSender) -> Result<Self, String> {
        let inner = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            let event = match result {
                Ok(event) if event.need_rescan() => WatchEvent::Overflow,
                Ok(event) if event.kind.is_access() => return,
                Ok(event)
                    if matches!(
                        event.kind,
                        notify::EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Metadata(_))
                    ) =>
                {
                    WatchEvent::Modified(event.paths)
                }
                Ok(event) => WatchEvent::Changed(event.paths),
                Err(_) => WatchEvent::Overflow,
            };
            events.send(event);
        })
        .map_err(|error| format!("directory watcher unavailable: {error}"))?;
        Ok(Self { inner })
    }
}

impl DirectoryWatch for NotifyWatcher {
    fn watch(&mut self, directory: &Path) -> bool {
        self.inner
            .watch(directory, RecursiveMode::NonRecursive)
            .is_ok()
    }

    fn unwatch(&mut self, directory: &Path) {
        let _ = self.inner.unwatch(directory);
    }
}

/// Applies one watcher event and returns the URIs whose entries were evicted.
pub(crate) fn apply_watch_event(cache: &ProjectCache, event: WatchEvent) -> Vec<Url> {
    let include_parent = !matches!(event, WatchEvent::Modified(_));
    match event {
        WatchEvent::Modified(paths) | WatchEvent::Changed(paths) => {
            let mut affected = Vec::new();
            let mut seen = HashSet::new();
            for path in paths {
                let invalidated = if include_parent {
                    cache.invalidate_path(&path)
                } else {
                    cache.invalidate_file_contents(&path)
                };
                for uri in invalidated {
                    if seen.insert(uri.clone()) {
                        affected.push(uri);
                    }
                }
            }
            affected
        }
        WatchEvent::Overflow => {
            cache.invalidate_after_overflow();
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_cache::{ImportValue, Lookup, Probe, ProjectCache};
    use std::time::Duration;

    fn store_import(cache: &ProjectCache, main: &Url, probes: Vec<Probe>) {
        let context = pascal_project::ProjectContext::default();
        let no = std::sync::atomic::AtomicBool::new(false);
        let Lookup::Compute(claim) = cache.imports(main, &context, 1, &Default::default(), &no)
        else {
            panic!()
        };
        cache.store_imports(
            claim,
            ImportValue {
                resolved: std::sync::Arc::new(pascal_core::ResolvedImports {
                    bindings: vec![],
                    dependencies: vec![],
                    complete: true,
                    settled: true,
                }),
                report: std::sync::Arc::new(pascal_core::ResolutionReport {
                    observations: vec![],
                    warnings: vec![],
                    complete: true,
                    incomplete_reasons: vec![],
                }),
                probes,
                watch_dirs: vec![],
            },
            1,
            &no,
        );
    }

    #[test]
    fn watcher_reports_file_created_in_watched_directory() {
        let temp = tempfile::tempdir().unwrap();
        let (sender, events) = WatchEvents::channel();
        let mut watcher = NotifyWatcher::start(sender).expect("watcher");
        assert!(watcher.watch(temp.path()));
        std::fs::write(temp.path().join("New.pas"), "unit New;").unwrap();
        // Other events may come first: FSEvents on macOS can still report the
        // temporary directory's own creation.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match events.recv_timeout(remaining) {
                Some(WatchEvent::Modified(paths) | WatchEvent::Changed(paths)) => {
                    if paths.iter().any(|path| path.ends_with("New.pas")) {
                        return;
                    }
                    seen.extend(paths);
                }
                Some(WatchEvent::Overflow) => return,
                None => panic!("no event for New.pas; saw {seen:?}"),
            }
        }
    }

    #[test]
    fn unwatched_directory_reports_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let (sender, events) = WatchEvents::channel();
        let mut watcher = NotifyWatcher::start(sender).expect("watcher");
        assert!(watcher.watch(temp.path()));
        watcher.unwatch(temp.path());
        std::fs::write(temp.path().join("New.pas"), "unit New;").unwrap();
        assert!(events.recv_timeout(Duration::from_millis(500)).is_none());
    }

    // Linux reports an in-place write as Modify(Data). Backends that report
    // only an unknown change deliberately keep conservative invalidation.
    #[cfg(target_os = "linux")]
    #[test]
    fn unrelated_content_write_does_not_requeue_directory_consumer() {
        let temp = tempfile::tempdir().unwrap();
        let unrelated = temp.path().join("Unrelated.pas");
        std::fs::write(&unrelated, "unit Unrelated;").unwrap();
        let main = Url::from_file_path(temp.path().join("Main.pas")).unwrap();
        let cache = ProjectCache::new(1 << 20);
        let context = pascal_project::ProjectContext::default();
        let no = AtomicBool::new(false);
        store_import(
            &cache,
            &main,
            vec![Probe::Stamp {
                path: temp.path().to_path_buf(),
                expected: pascal_project::path_stamp_result(temp.path()).unwrap(),
            }],
        );
        let (sender, events) = WatchEvents::channel();
        let mut watcher = NotifyWatcher::start(sender).unwrap();
        assert!(watcher.watch(temp.path()));

        std::fs::write(&unrelated, "unit Unrelated; interface implementation end.").unwrap();
        let event = events
            .recv_timeout(Duration::from_secs(5))
            .expect("unrelated file write must reach the watcher");
        let mut affected = apply_watch_event(&cache, event);
        affected.extend(events.drain(&cache, MAX_WATCH_EVENTS_PER_TURN, &|_| false));

        assert!(
            affected.is_empty(),
            "writing an unrelated sibling must not schedule indexing: {affected:?}"
        );
        assert!(matches!(
            cache.imports(&main, &context, 1, &Default::default(), &no),
            Lookup::Hit(_)
        ));
    }

    #[test]
    fn modified_event_requeues_exact_dependencies_but_not_directory_only_consumers() {
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("include.custom");
        std::fs::write(&provider, "{$DEFINE BEFORE}").unwrap();
        let provider_uri = Url::from_file_path(&provider).unwrap();
        let consumer = Url::from_file_path(temp.path().join("Consumer.pas")).unwrap();
        let unrelated = Url::from_file_path(temp.path().join("Unrelated.pas")).unwrap();
        let directory_probe = Probe::Stamp {
            path: temp.path().to_path_buf(),
            expected: pascal_project::path_stamp_result(temp.path()).unwrap(),
        };
        let cache = ProjectCache::new(1 << 20);
        store_import(&cache, &provider_uri, vec![]);
        store_import(
            &cache,
            &consumer,
            vec![
                directory_probe.clone(),
                Probe::Content {
                    path: provider.clone(),
                    stamp: pascal_project::path_stamp_result(&provider).unwrap(),
                    content_hash: pascal_project::content_hash_bytes(b"{$DEFINE BEFORE}"),
                },
            ],
        );
        store_import(&cache, &unrelated, vec![directory_probe]);

        std::fs::write(&provider, "{$DEFINE AFTER}").unwrap();
        let mut affected = apply_watch_event(&cache, WatchEvent::Modified(vec![provider]));
        affected.sort();
        let mut expected = vec![consumer, provider_uri];
        expected.sort();
        assert_eq!(affected, expected);
        assert!(matches!(
            cache.imports(
                &unrelated,
                &pascal_project::ProjectContext::default(),
                1,
                &Default::default(),
                &AtomicBool::new(false),
            ),
            Lookup::Hit(_)
        ));
    }

    #[test]
    fn directory_membership_events_still_requeue_consumers() {
        for operation in ["create", "delete", "rename"] {
            let temp = tempfile::tempdir().unwrap();
            let old = temp.path().join("Old.pas");
            let new = temp.path().join("New.pas");
            std::fs::write(&old, "unit Old;").unwrap();
            let main = Url::from_file_path(temp.path().join("Main.pas")).unwrap();
            let cache = ProjectCache::new(1 << 20);
            store_import(
                &cache,
                &main,
                vec![Probe::Stamp {
                    path: temp.path().to_path_buf(),
                    expected: pascal_project::path_stamp_result(temp.path()).unwrap(),
                }],
            );
            let (sender, events) = WatchEvents::channel();
            let mut watcher = NotifyWatcher::start(sender).unwrap();
            assert!(watcher.watch(temp.path()));

            match operation {
                "create" => std::fs::write(&new, "unit New;").unwrap(),
                "delete" => std::fs::remove_file(&old).unwrap(),
                "rename" => std::fs::rename(&old, &new).unwrap(),
                _ => unreachable!(),
            }
            let event = events
                .recv_timeout(Duration::from_secs(5))
                .expect("directory membership event");
            assert_eq!(apply_watch_event(&cache, event), vec![main], "{operation}");
        }
    }

    #[test]
    fn change_event_evicts_cache_entries_and_overflow_drops_imports() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ProjectCache::new(1 << 20);
        let context = pascal_project::ProjectContext::default();
        let main = lsp_types::Url::from_file_path(temp.path().join("Main.pas")).unwrap();
        let empty = || ImportValue {
            resolved: std::sync::Arc::new(pascal_core::ResolvedImports {
                bindings: vec![],
                dependencies: vec![],
                complete: true,
                settled: true,
            }),
            report: std::sync::Arc::new(pascal_core::ResolutionReport {
                observations: vec![],
                warnings: vec![],
                complete: true,
                incomplete_reasons: vec![],
            }),
            probes: vec![Probe::Stamp {
                path: temp.path().to_path_buf(),
                expected: pascal_project::path_stamp_result(temp.path())
                    .ok()
                    .flatten(),
            }],
            watch_dirs: vec![],
        };
        let no = std::sync::atomic::AtomicBool::new(false);
        let Lookup::Compute(claim) = cache.imports(&main, &context, 1, &Default::default(), &no)
        else {
            panic!()
        };
        cache.store_imports(claim, empty(), 1, &no);
        let affected =
            apply_watch_event(&cache, WatchEvent::Changed(vec![temp.path().join("X.pas")]));
        assert_eq!(affected, vec![main.clone()]);

        let Lookup::Compute(claim) = cache.imports(&main, &context, 1, &Default::default(), &no)
        else {
            panic!()
        };
        cache.store_imports(claim, empty(), 1, &no);
        apply_watch_event(&cache, WatchEvent::Overflow);
        assert_eq!(cache.stats().imports, 0);
    }

    fn overflowed(events: &WatchEvents) -> bool {
        events.queue.pending().overflowed
    }

    #[test]
    fn full_watch_event_queue_flags_overflow_and_drops_imports() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ProjectCache::new(1 << 20);
        let main = Url::from_file_path(temp.path().join("Main.pas")).unwrap();
        store_import(&cache, &main, vec![]);
        let (sender, events) = WatchEvents::channel();

        for index in 0..MAX_PENDING_WATCH_PATHS {
            sender.send(WatchEvent::Changed(vec![
                temp.path().join(format!("{index}")),
            ]));
        }
        assert!(!overflowed(&events));
        sender.send(WatchEvent::Changed(vec![temp.path().join("one more")]));
        assert!(overflowed(&events));

        events.drain(&cache, MAX_WATCH_EVENTS_PER_TURN, &|_| false);
        assert!(!overflowed(&events));
        assert_eq!(
            events.applied(),
            (0, 1),
            "the overflow covers what was pending"
        );
        // The paths it covered are unknown: the caller rewarms open documents.
        assert!(events.take_overflowed());
        assert!(!events.take_overflowed());
        assert_eq!(cache.stats().imports, 0);
    }

    /// l4d-zh7i: a build reports each compiled unit it writes once or
    /// twice; however many events name a path, it is pending once, and
    /// many more events than distinct paths do not overflow.
    #[test]
    fn events_for_one_path_are_applied_once() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ProjectCache::new(1 << 20);
        let main = Url::from_file_path(temp.path().join("Main.pas")).unwrap();
        store_import(&cache, &main, vec![]);
        let (sender, events) = WatchEvents::channel();
        let built = (0..1024)
            .map(|index| temp.path().join(format!("Unit{index}.dcu")))
            .collect::<Vec<_>>();
        for _ in 0..3 {
            for path in &built {
                sender.send(WatchEvent::Changed(vec![path.clone()]));
                sender.send(WatchEvent::Modified(vec![path.clone()]));
            }
        }
        assert!(!overflowed(&events));
        for _ in 0..built.len().div_ceil(MAX_WATCH_EVENTS_PER_TURN) {
            events.drain(&cache, MAX_WATCH_EVENTS_PER_TURN, &|_| false);
        }
        assert_eq!(events.applied(), (built.len() as u64, 0));
        assert_eq!(cache.stats().imports, 1, "nothing observed the units");

        // A Modified then Changed path is applied as Changed, once.
        let main_path = main.to_file_path().unwrap();
        sender.send(WatchEvent::Modified(vec![main_path.clone()]));
        sender.send(WatchEvent::Changed(vec![main_path]));
        assert_eq!(
            events.recv_timeout(Duration::ZERO),
            Some(WatchEvent::Changed(vec![temp.path().join("Main.pas")]))
        );
        assert_eq!(events.recv_timeout(Duration::ZERO), None);
    }

    /// l4d-vrw0: events for the store's own files invalidate nothing.
    #[test]
    fn ignored_paths_are_dropped_before_any_invalidation() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("cache/lint4d/analysis");
        std::fs::create_dir_all(&store).unwrap();
        let main = Url::from_file_path(temp.path().join("Main.pas")).unwrap();
        let cache = ProjectCache::new(1 << 20);
        let probe = |path: &Path| Probe::Stamp {
            path: path.to_path_buf(),
            expected: pascal_project::path_stamp_result(path).unwrap(),
        };
        store_import(&cache, &main, vec![probe(temp.path()), probe(&store)]);
        let (sender, events) = WatchEvents::channel();
        for event in [
            WatchEvent::Changed(vec![store.join("analysis-v7.sqlite3-wal")]),
            WatchEvent::Modified(vec![store.join("analysis-v7.sqlite3")]),
            WatchEvent::Changed(vec![store.clone(), store.join("namespaces")]),
        ] {
            sender.send(event);
        }
        let ignored = |path: &Path| path.starts_with(&store);
        assert!(
            events
                .drain(&cache, MAX_WATCH_EVENTS_PER_TURN, &ignored)
                .is_empty()
        );
        assert_eq!(cache.stats().imports, 1);

        // The same events, not ignored, invalidate the import.
        sender.send(WatchEvent::Changed(vec![
            store.join("analysis-v7.sqlite3-wal"),
        ]));
        assert_eq!(
            events.drain(&cache, MAX_WATCH_EVENTS_PER_TURN, &|_| false),
            vec![main]
        );
    }

    #[test]
    fn watch_event_drain_respects_limit_and_leaves_remaining_events() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ProjectCache::new(1 << 20);
        let uris = (0..3)
            .map(|index| Url::from_file_path(temp.path().join(format!("Main{index}.pas"))).unwrap())
            .collect::<Vec<_>>();
        for uri in &uris {
            store_import(&cache, uri, vec![]);
        }
        let (sender, events) = WatchEvents::channel();
        for uri in &uris {
            sender.send(WatchEvent::Changed(vec![uri.to_file_path().unwrap()]));
        }

        assert_eq!(events.drain(&cache, 2, &|_| false), uris[..2]);
        assert_eq!(cache.stats().imports, 1);
        assert_eq!(events.drain(&cache, 2, &|_| false), vec![uris[2].clone()]);
        assert_eq!(cache.stats().imports, 0);
    }

    #[cfg(unix)]
    #[test]
    fn overflow_drops_every_witness() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("A.inc");
        std::fs::write(&file, b"{A}").unwrap();
        pascal_project::backdate_for_test(&file);
        let stamp = pascal_project::path_stamp_result(&file).unwrap().unwrap();
        let cache = ProjectCache::default();
        let proven =
            crate::project_cache::Proven::Content(pascal_cache::SourceContent::from_bytes(b"{A}"));
        cache.record_witness(&file, stamp.clone(), proven.clone());
        assert_eq!(cache.witness(&file, &stamp), Some(proven));
        apply_watch_event(&cache, WatchEvent::Overflow);
        assert_eq!(cache.witness(&file, &stamp), None);
    }
}

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use lsp_types::Url;
use notify::event::ModifyKind;
use notify::{RecursiveMode, Watcher};

use crate::project_cache::ProjectCache;

pub(crate) const WATCH_EVENT_CAPACITY: usize = 1024;
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

pub(crate) struct WatchEventSender {
    sender: Sender<WatchEvent>,
    overflowed: Arc<AtomicBool>,
}

pub(crate) struct WatchEvents {
    receiver: Receiver<WatchEvent>,
    overflowed: Arc<AtomicBool>,
}

impl WatchEvents {
    pub(crate) fn channel() -> (WatchEventSender, Self) {
        let (sender, receiver) = bounded(WATCH_EVENT_CAPACITY);
        let overflowed = Arc::new(AtomicBool::new(false));
        (
            WatchEventSender {
                sender,
                overflowed: Arc::clone(&overflowed),
            },
            Self {
                receiver,
                overflowed,
            },
        )
    }

    pub(crate) fn drain(&self, cache: &ProjectCache, limit: usize) -> Vec<Url> {
        let mut affected = Vec::new();
        let mut remaining = limit;
        if remaining > 0 && self.overflowed.swap(false, Ordering::AcqRel) {
            apply_watch_event(cache, WatchEvent::Overflow);
            remaining -= 1;
        }
        for _ in 0..remaining {
            let event = match self.receiver.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            affected.extend(apply_watch_event(cache, event));
        }
        affected
    }
}

fn enqueue(sender: &Sender<WatchEvent>, overflowed: &AtomicBool, event: WatchEvent) {
    if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = sender.try_send(event) {
        overflowed.store(true, Ordering::Release);
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
            enqueue(&events.sender, &events.overflowed, event);
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
            for path in paths {
                let invalidated = if include_parent {
                    cache.invalidate_path(&path)
                } else {
                    cache.invalidate_file_contents(&path)
                };
                for uri in invalidated {
                    if !affected.contains(&uri) {
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
                resolved: pascal_core::ResolvedImports {
                    bindings: vec![],
                    dependencies: vec![],
                    complete: true,
                },
                report: pascal_core::ResolutionReport {
                    observations: vec![],
                    warnings: vec![],
                    complete: true,
                    incomplete_reasons: vec![],
                },
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
        let event = events
            .receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("event");
        match event {
            WatchEvent::Modified(paths) | WatchEvent::Changed(paths) => {
                assert!(
                    paths.iter().any(|path| path.ends_with("New.pas")),
                    "{paths:?}"
                );
            }
            WatchEvent::Overflow => {}
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
        assert!(
            events
                .receiver
                .recv_timeout(Duration::from_millis(500))
                .is_err()
        );
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
            .receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("unrelated file write must reach the watcher");
        let mut affected = apply_watch_event(&cache, event);
        affected.extend(events.drain(&cache, MAX_WATCH_EVENTS_PER_TURN));

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
                .receiver
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
            resolved: pascal_core::ResolvedImports {
                bindings: vec![],
                dependencies: vec![],
                complete: true,
            },
            report: pascal_core::ResolutionReport {
                observations: vec![],
                warnings: vec![],
                complete: true,
                incomplete_reasons: vec![],
            },
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

    #[test]
    fn full_watch_event_queue_flags_overflow_and_drops_imports() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ProjectCache::new(1 << 20);
        let main = Url::from_file_path(temp.path().join("Main.pas")).unwrap();
        store_import(&cache, &main, vec![]);
        let (sender, events) = WatchEvents::channel();

        for _ in 0..WATCH_EVENT_CAPACITY {
            enqueue(
                &sender.sender,
                &sender.overflowed,
                WatchEvent::Changed(Vec::new()),
            );
        }
        enqueue(
            &sender.sender,
            &sender.overflowed,
            WatchEvent::Changed(Vec::new()),
        );
        assert!(sender.overflowed.load(std::sync::atomic::Ordering::Acquire));

        events.drain(&cache, MAX_WATCH_EVENTS_PER_TURN);
        assert!(!sender.overflowed.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(cache.stats().imports, 0);
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
            enqueue(
                &sender.sender,
                &sender.overflowed,
                WatchEvent::Changed(vec![uri.to_file_path().unwrap()]),
            );
        }

        assert_eq!(events.drain(&cache, 2), uris[..2]);
        assert_eq!(cache.stats().imports, 1);
        assert_eq!(events.drain(&cache, 2), vec![uris[2].clone()]);
        assert_eq!(cache.stats().imports, 0);
    }
}

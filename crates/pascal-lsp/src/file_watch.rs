use std::path::Path;
use std::path::PathBuf;

use crossbeam_channel::Sender;
use lsp_types::Url;
use notify::{RecursiveMode, Watcher};

use crate::project_cache::ProjectCache;

/// Non-recursive directory watch registration. Returns `false` when the
/// directory cannot be watched; callers then rely on stamp probes alone.
pub(crate) trait DirectoryWatch: Send {
    fn watch(&mut self, directory: &Path) -> bool;
    fn unwatch(&mut self, directory: &Path);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatchEvent {
    Changed(Vec<PathBuf>),
    Overflow,
}

pub(crate) struct NotifyWatcher {
    inner: notify::RecommendedWatcher,
}

impl NotifyWatcher {
    pub(crate) fn start(events: Sender<WatchEvent>) -> Result<Self, String> {
        let inner = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            let event = match result {
                Ok(event) if event.need_rescan() => WatchEvent::Overflow,
                Ok(event) if event.kind.is_access() => return,
                Ok(event) => WatchEvent::Changed(event.paths),
                Err(_) => WatchEvent::Overflow,
            };
            let _ = events.send(event);
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
    match event {
        WatchEvent::Changed(paths) => {
            let mut affected = Vec::new();
            for path in paths {
                for uri in cache.invalidate_path(&path) {
                    if !affected.contains(&uri) {
                        affected.push(uri);
                    }
                }
            }
            affected
        }
        WatchEvent::Overflow => {
            cache.drop_imports();
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn watcher_reports_file_created_in_watched_directory() {
        let temp = tempfile::tempdir().unwrap();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut watcher = NotifyWatcher::start(sender).expect("watcher");
        assert!(watcher.watch(temp.path()));
        std::fs::write(temp.path().join("New.pas"), "unit New;").unwrap();
        let event = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("event");
        match event {
            WatchEvent::Changed(paths) => {
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
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut watcher = NotifyWatcher::start(sender).expect("watcher");
        assert!(watcher.watch(temp.path()));
        watcher.unwatch(temp.path());
        std::fs::write(temp.path().join("New.pas"), "unit New;").unwrap();
        assert!(receiver.recv_timeout(Duration::from_millis(500)).is_err());
    }

    #[test]
    fn change_event_evicts_cache_entries_and_overflow_drops_imports() {
        use crate::project_cache::{ImportValue, Lookup, Probe, ProjectCache};

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
}

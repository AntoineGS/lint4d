use super::authorization::extension_is;
use super::context::SessionSelections;
use super::selection::push_bounded_candidate;
use super::*;

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEST_PROJECT_DISCOVERY_COUNTS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    static TEST_PROJECT_DIRECTORY_LISTING_COUNTS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_project_discovery_counts() -> (usize, usize) {
    TEST_PROJECT_DISCOVERY_COUNTS.with(std::cell::Cell::get)
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_reset_project_discovery_counts() {
    TEST_PROJECT_DISCOVERY_COUNTS.with(|counts| counts.set((0, 0)));
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn test_record_project_discovery(build: bool) {
    TEST_PROJECT_DISCOVERY_COUNTS.with(|counts| {
        let (discoveries, builds) = counts.get();
        counts.set((
            discoveries + usize::from(!build),
            builds + usize::from(build),
        ));
    });
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_project_directory_listing_counts() -> (usize, usize) {
    TEST_PROJECT_DIRECTORY_LISTING_COUNTS.with(std::cell::Cell::get)
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_reset_project_directory_listing_counts() {
    TEST_PROJECT_DIRECTORY_LISTING_COUNTS.with(|counts| counts.set((0, 0)));
}

#[cfg(any(test, feature = "test-support"))]
fn test_record_project_directory_listing(read: bool) {
    TEST_PROJECT_DIRECTORY_LISTING_COUNTS.with(|counts| {
        let (reads, stamp_checks) = counts.get();
        counts.set((reads + usize::from(read), stamp_checks + usize::from(!read)));
    });
}
#[derive(Debug, Clone, Default)]
pub(super) struct ProjectDirectoryEntries {
    pub(super) dproj: Vec<PathBuf>,
    pub(super) dpr_or_dpk: Vec<PathBuf>,
    system_pas: Option<PathBuf>,
    pub(super) candidate_overflow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectDirectoryStamp {
    bytes: u64,
    modified: Option<SystemTime>,
    // Directory mtime precision varies by filesystem. On Unix, include ctime
    // and identity so same-tick membership changes still invalidate a listing.
    // Other platforms use the strongest portable std::fs metadata available.
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

#[derive(Debug, Clone)]
struct CachedProjectDirectoryEntries {
    entries: ProjectDirectoryEntries,
    stamp: ProjectDirectoryStamp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectDiscoveryCacheStats {
    pub discoveries: usize,
    pub project_context_builds: usize,
    pub project_context_cache_hits: usize,
    pub directory_listing_reads: usize,
    pub directory_listing_hits: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ProjectOptionsCacheKey {
    pub(super) project_file: Option<PathBuf>,
    pub(super) selections: SessionSelections,
    pub(super) build_config: Option<String>,
    pub(super) platform: Option<String>,
    pub(super) source_paths: Vec<String>,
    pub(super) conditional_context: ConditionalContext,
}

impl ProjectOptionsCacheKey {
    pub(super) fn new(options: &ProjectOptions, project_file: &Path) -> Self {
        Self {
            project_file: options.project_file.clone(),
            selections: options.session_selections_for(project_file),
            build_config: options.build_config.clone(),
            platform: options.platform.clone(),
            source_paths: options.source_paths.clone(),
            conditional_context: options.conditional_context.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ProjectContextBuildCacheKey {
    pub(super) project_file: PathBuf,
    pub(super) roots: Vec<PathBuf>,
    pub(super) deleted_paths: Vec<PathBuf>,
    pub(super) option_base: PathBuf,
    pub(super) options: ProjectOptionsCacheKey,
    pub(super) overrides: EffectiveOverrides,
    pub(super) exclusions: Vec<String>,
    pub(super) warnings: Vec<String>,
    pub(super) explicit: bool,
}

#[derive(Debug, Clone)]
pub(super) struct CachedProjectContext {
    pub(super) project: DiscoveredProject,
    pub(super) observations: Vec<ProjectReadObservation>,
    pub(super) metadata_paths: Vec<PathBuf>,
    pub(super) metadata_observations: Vec<MetadataObservation>,
}

#[derive(Debug, Default)]
struct ProjectDiscoveryCache {
    directory_entries: HashMap<PathBuf, CachedProjectDirectoryEntries>,
    project_contexts: HashMap<ProjectContextBuildCacheKey, CachedProjectContext>,
    stats: ProjectDiscoveryCacheStats,
}

thread_local! {
    static PROJECT_DISCOVERY_CACHE: RefCell<Option<ProjectDiscoveryCache>> = const { RefCell::new(None) };
}

/// A request-local cache for repeated Pascal project discovery work.
///
/// The cache deliberately has no process-wide lifetime: callers hold this
/// scope for one immutable workspace snapshot, then all parsed contexts and
/// directory listings are discarded.
#[doc(hidden)]
pub struct ProjectDiscoveryCacheScope {
    previous: Option<ProjectDiscoveryCache>,
}

impl ProjectDiscoveryCacheScope {
    pub fn new() -> Self {
        let previous = PROJECT_DISCOVERY_CACHE
            .with(|cache| cache.replace(Some(ProjectDiscoveryCache::default())));
        Self { previous }
    }

    pub fn stats(&self) -> ProjectDiscoveryCacheStats {
        PROJECT_DISCOVERY_CACHE.with(|cache| {
            cache
                .borrow()
                .as_ref()
                .map_or_else(ProjectDiscoveryCacheStats::default, |cache| {
                    cache.stats.clone()
                })
        })
    }
}

impl Default for ProjectDiscoveryCacheScope {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ProjectDiscoveryCacheScope {
    fn drop(&mut self) {
        PROJECT_DISCOVERY_CACHE.with(|cache| {
            cache.replace(self.previous.take());
        });
    }
}

fn with_project_discovery_cache<T>(
    operation: impl FnOnce(&mut ProjectDiscoveryCache) -> T,
) -> Option<T> {
    PROJECT_DISCOVERY_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.as_mut().map(operation)
    })
}

pub(super) fn record_project_discovery_cache_discovery() {
    let _ = with_project_discovery_cache(|cache| {
        cache.stats.discoveries = cache.stats.discoveries.saturating_add(1);
    });
}

pub(super) fn record_project_discovery_cache_build() {
    let _ = with_project_discovery_cache(|cache| {
        cache.stats.project_context_builds = cache.stats.project_context_builds.saturating_add(1);
    });
}

pub(super) fn cache_path_list(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = paths.to_vec();
    paths.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
    paths.dedup_by(|left, right| project_paths_equal(left, right));
    paths
}

pub(super) fn project_context_cache_get(
    key: &ProjectContextBuildCacheKey,
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<Option<CachedProjectContext>, String> {
    check_project_scan_cancel(cancel)?;
    if let Some(work_budget) = work_budget {
        work_budget.check_cancelled()?;
    }
    let cached =
        with_project_discovery_cache(|cache| cache.project_contexts.get(key).cloned()).flatten();
    if let Some(cached) = &cached {
        if !project_context_cache_is_current(cached, cancel, work_budget)? {
            let _ = with_project_discovery_cache(|cache| {
                cache.project_contexts.remove(key);
            });
            return Ok(None);
        }
        let _ = with_project_discovery_cache(|cache| {
            cache.stats.project_context_cache_hits =
                cache.stats.project_context_cache_hits.saturating_add(1);
        });
        if let Some(work_budget) = work_budget {
            let visits = cached
                .observations
                .len()
                .saturating_add(cached.metadata_paths.len())
                .saturating_add(cached.metadata_observations.len());
            work_budget.charge_path_visits(visits)?;
        }
    }
    check_project_scan_cancel(cancel)?;
    if let Some(work_budget) = work_budget {
        work_budget.check_cancelled()?;
    }
    Ok(cached)
}

fn project_context_cache_is_current(
    cached: &CachedProjectContext,
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<bool, String> {
    for observation in &cached.observations {
        check_project_scan_cancel(cancel)?;
        if let Some(work_budget) = work_budget {
            work_budget.check_cancelled()?;
            work_budget.charge_path_visits(1)?;
            work_budget.charge_path_bytes(observation.path.as_os_str().len())?;
        }
        let current = path_stamp_result(&observation.path).unwrap_or(None);
        // A cached context is a long-lived proof: an unsettled read cannot
        // vouch that a same-size rewrite in the same tick did not happen.
        if !current
            .as_ref()
            .is_some_and(|current| observation.stamp.proves_unchanged(current))
        {
            return Ok(false);
        }
    }
    for observation in &cached.project.observations.metadata_observations {
        check_project_scan_cancel(cancel)?;
        if let Some(work_budget) = work_budget {
            work_budget.check_cancelled()?;
            work_budget.charge_path_visits(1)?;
            work_budget.charge_path_bytes(observation.path().as_os_str().len())?;
        }
        match observation {
            MetadataObservation::Payload { path, .. } => {
                let current = metadata_payload_is_current(
                    observation,
                    project_metadata_read_limit(path),
                    work_budget,
                )?;
                check_project_scan_cancel(cancel)?;
                if !current {
                    return Ok(false);
                }
            }
            MetadataObservation::Stat { path } => {
                let has_read_observation = cached
                    .observations
                    .iter()
                    .any(|read| project_paths_equal(&read.path, path));
                if !has_read_observation && path_stamp_result(path).unwrap_or(None).is_some() {
                    return Ok(false);
                }
            }
        }
    }
    check_project_scan_cancel(cancel)?;
    if let Some(work_budget) = work_budget {
        work_budget.check_cancelled()?;
    }
    Ok(true)
}

fn project_metadata_read_limit(path: &Path) -> u64 {
    if extension_is(path, "json") {
        MAX_LOCAL_PROJECT_CONFIG_BYTES
    } else if extension_is(path, "pas") || extension_is(path, "dpr") || extension_is(path, "dpk") {
        MAX_MAIN_SOURCE_BYTES
    } else {
        MAX_PROJECT_BYTES
    }
}

pub(super) fn project_context_cache_insert(
    key: ProjectContextBuildCacheKey,
    context: CachedProjectContext,
) {
    let _ = with_project_discovery_cache(|cache| {
        cache.project_contexts.entry(key).or_insert(context);
    });
}
pub(super) fn project_directory_entries(
    directory: &Path,
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<ProjectDirectoryEntries, String> {
    check_project_scan_cancel(cancel)?;
    if let Some(work_budget) = work_budget {
        work_budget.check_cancelled()?;
    }
    let (cache_active, cached) = PROJECT_DISCOVERY_CACHE.with(|cache| {
        cache.borrow().as_ref().map_or((false, None), |cache| {
            (true, cache.directory_entries.get(directory).cloned())
        })
    });
    if !cache_active {
        #[cfg(any(test, feature = "test-support"))]
        test_record_project_directory_listing(true);
        return read_project_directory_entries(directory, cancel, work_budget);
    }
    if let Some(cached) = cached {
        let current_stamp = project_directory_stamp(directory, cancel, work_budget)?;
        if current_stamp.as_ref() == Some(&cached.stamp) {
            if let Some(work_budget) = work_budget {
                work_budget.charge_path_visits(
                    1usize
                        .saturating_add(cached.entries.dproj.len())
                        .saturating_add(cached.entries.dpr_or_dpk.len()),
                )?;
            }
            let _ = with_project_discovery_cache(|cache| {
                cache.stats.directory_listing_hits =
                    cache.stats.directory_listing_hits.saturating_add(1);
            });
            check_project_scan_cancel(cancel)?;
            return Ok(cached.entries);
        }
        let _ = with_project_discovery_cache(|cache| {
            cache.directory_entries.remove(directory);
        });
    }

    let before_stamp = project_directory_stamp(directory, cancel, work_budget)?;
    #[cfg(any(test, feature = "test-support"))]
    test_record_project_directory_listing(true);
    let result = read_project_directory_entries(directory, cancel, work_budget);
    if let Ok(entries) = &result {
        let after_stamp = project_directory_stamp(directory, cancel, work_budget)?;
        let _ = with_project_discovery_cache(|cache| {
            cache.stats.directory_listing_reads =
                cache.stats.directory_listing_reads.saturating_add(1);
            if let (Some(before), Some(after)) = (before_stamp, after_stamp)
                && before == after
            {
                cache.directory_entries.insert(
                    directory.to_path_buf(),
                    CachedProjectDirectoryEntries {
                        entries: entries.clone(),
                        stamp: after,
                    },
                );
            }
        });
    }
    result
}

fn project_directory_stamp(
    directory: &Path,
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<Option<ProjectDirectoryStamp>, String> {
    check_project_scan_cancel(cancel)?;
    if let Some(work_budget) = work_budget {
        work_budget.check_cancelled()?;
        work_budget.charge_path_visits(1)?;
    }
    #[cfg(any(test, feature = "test-support"))]
    test_record_project_directory_listing(false);
    let stamp = match fs::metadata(directory) {
        Ok(metadata) if metadata.is_dir() => Some(ProjectDirectoryStamp {
            bytes: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
        }),
        Ok(_) => None,
        Err(_) => None,
    };
    check_project_scan_cancel(cancel)?;
    if let Some(work_budget) = work_budget {
        work_budget.check_cancelled()?;
    }
    Ok(stamp)
}

fn read_project_directory_entries(
    directory: &Path,
    cancel: Option<&AtomicBool>,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<ProjectDirectoryEntries, String> {
    check_project_scan_cancel(cancel)?;
    if let Some(work_budget) = work_budget {
        work_budget.check_cancelled()?;
        work_budget.charge_path_visits(1)?;
    }
    let mut entries = fs::read_dir(directory).map_err(|error| {
        format!(
            "could not inspect project directory {}: {error}",
            directory.display()
        )
    })?;
    let mut result = ProjectDirectoryEntries::default();
    let mut visited_entries = 0usize;
    loop {
        check_project_scan_cancel(cancel)?;
        if let Some(work_budget) = work_budget {
            work_budget.check_cancelled()?;
            work_budget.charge_path_visits(1)?;
        }
        #[cfg(any(test, feature = "test-support"))]
        run_test_project_directory_advance_hook(directory);
        let Some(entry) = entries.next() else {
            break;
        };
        visited_entries = visited_entries.saturating_add(1);
        if visited_entries > MAX_PROJECT_DIRECTORY_ENTRIES {
            return Err(format!(
                "project directory entry limit ({MAX_PROJECT_DIRECTORY_ENTRIES}) reached in {}",
                directory.display()
            ));
        }
        let entry = entry.map_err(|error| {
            format!(
                "could not inspect project directory entry under {}: {error}",
                directory.display()
            )
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "could not inspect project directory entry {}: {error}",
                path.display()
            )
        })?;
        if !file_type.is_file() {
            continue;
        }
        if result.system_pas.is_none()
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("System.pas"))
        {
            result.system_pas = Some(path.clone());
        }
        if extension_is(&path, "dproj") {
            push_bounded_candidate(&mut result.dproj, path);
            result.candidate_overflow |= result.dproj.len() > MAX_OWNERSHIP_CANDIDATES;
        } else if extension_is(&path, "dpr") || extension_is(&path, "dpk") {
            push_bounded_candidate(&mut result.dpr_or_dpk, path);
            result.candidate_overflow |= result.dpr_or_dpk.len() > MAX_OWNERSHIP_CANDIDATES;
        }
    }
    Ok(result)
}

#[cfg(any(test, feature = "test-support"))]
type ProjectDirectoryAdvanceHook = Box<dyn FnMut(&Path)>;

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEST_BEFORE_PROJECT_DIRECTORY_ADVANCE:
        std::cell::RefCell<Option<ProjectDirectoryAdvanceHook>> = std::cell::RefCell::new(None);
}

#[cfg(any(test, feature = "test-support"))]
pub struct TestProjectDirectoryAdvanceGuard(Option<ProjectDirectoryAdvanceHook>);

/// Install a test-only callback after a directory-step budget charge succeeds
/// and immediately before the iterator advances. This makes the admission
/// boundary deterministic without sleeps or filesystem timing assumptions.
#[cfg(any(test, feature = "test-support"))]
pub fn test_before_project_directory_advance(
    hook: impl FnMut(&Path) + 'static,
) -> TestProjectDirectoryAdvanceGuard {
    let previous = TEST_BEFORE_PROJECT_DIRECTORY_ADVANCE
        .with(|slot| slot.borrow_mut().replace(Box::new(hook)));
    TestProjectDirectoryAdvanceGuard(previous)
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestProjectDirectoryAdvanceGuard {
    fn drop(&mut self) {
        TEST_BEFORE_PROJECT_DIRECTORY_ADVANCE.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

#[cfg(any(test, feature = "test-support"))]
fn run_test_project_directory_advance_hook(directory: &Path) {
    TEST_BEFORE_PROJECT_DIRECTORY_ADVANCE.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(directory);
        }
    });
}

pub(super) fn check_project_scan_cancel(cancel: Option<&AtomicBool>) -> Result<(), String> {
    #[cfg(any(test, feature = "test-support"))]
    let force_cancel = cancel.is_some()
        && TEST_PROJECT_SCAN_CANCEL_AFTER_CHECKS.with(|budget| match budget.get() {
            Some(0) => {
                budget.set(None);
                true
            }
            Some(remaining) => {
                budget.set(Some(remaining.saturating_sub(1)));
                false
            }
            None => false,
        });
    #[cfg(any(test, feature = "test-support"))]
    if force_cancel && let Some(cancel) = cancel {
        cancel.store(true, Ordering::Relaxed);
    }
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
        Err("request cancelled".to_string())
    } else {
        Ok(())
    }
}

#[cfg(any(test, feature = "test-support"))]
#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEST_PROJECT_SCAN_CANCEL_AFTER_CHECKS: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}
#[cfg(any(test, feature = "test-support"))]
pub struct TestProjectScanCancellationGuard(Option<usize>);

#[cfg(any(test, feature = "test-support"))]
pub fn test_cancel_project_scan_after_checks(checks: usize) -> TestProjectScanCancellationGuard {
    let previous = TEST_PROJECT_SCAN_CANCEL_AFTER_CHECKS.with(|budget| {
        let previous = budget.get();
        budget.set(Some(checks));
        previous
    });
    TestProjectScanCancellationGuard(previous)
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestProjectScanCancellationGuard {
    fn drop(&mut self) {
        TEST_PROJECT_SCAN_CANCEL_AFTER_CHECKS.with(|budget| budget.set(self.0.take()));
    }
}
#[derive(Debug, Clone, Default)]
pub(super) struct PathResolutionDirectoryEntries {
    pub(super) names: Vec<std::ffi::OsString>,
    pub(super) case_insensitive_names: HashMap<String, Vec<usize>>,
}

#[derive(Debug, Clone)]
pub(super) enum PathResolutionDirectoryListing {
    // Shared so a cache hit does not copy a large directory per lookup.
    Entries(Arc<PathResolutionDirectoryEntries>),
    Missing,
    Error(String),
    Cancelled,
}

thread_local! {
    static PATH_RESOLUTION_DIRECTORY_CACHE: std::cell::RefCell<Option<HashMap<PathBuf, PathResolutionDirectoryListing>>> = const { std::cell::RefCell::new(None) };
}

pub(super) struct PathResolutionCacheScope {
    previous: Option<HashMap<PathBuf, PathResolutionDirectoryListing>>,
}

impl PathResolutionCacheScope {
    pub(super) fn new() -> Self {
        let previous =
            PATH_RESOLUTION_DIRECTORY_CACHE.with(|cache| cache.replace(Some(HashMap::new())));
        Self { previous }
    }
}

impl Drop for PathResolutionCacheScope {
    fn drop(&mut self) {
        PATH_RESOLUTION_DIRECTORY_CACHE.with(|cache| {
            cache.replace(self.previous.take());
        });
    }
}

/// Directory entries listed between cancellation checks.
const PATH_RESOLUTION_CANCEL_CHECK_INTERVAL: usize = 1024;

thread_local! {
    static PATH_RESOLUTION_CANCEL: std::cell::Cell<Option<std::ptr::NonNull<AtomicBool>>> = const { std::cell::Cell::new(None) };
}

/// Runs `run` with `cancel` visible to path resolution on this thread. Callers
/// of `resolve_existing_path_status` are too many and too deep to thread a token
/// through, so the walk reads it from here, like the directory cache above.
/// Without a token the enclosing scope's token stays visible.
pub(super) fn with_path_resolution_cancel<T>(
    cancel: Option<&AtomicBool>,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<std::ptr::NonNull<AtomicBool>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            PATH_RESOLUTION_CANCEL.with(|slot| slot.set(self.0));
        }
    }

    let Some(cancel) = cancel else {
        return run();
    };
    let _restore = Restore(
        PATH_RESOLUTION_CANCEL.with(|slot| slot.replace(Some(std::ptr::NonNull::from(cancel)))),
    );
    run()
}

pub(super) fn path_resolution_cancelled() -> bool {
    PATH_RESOLUTION_CANCEL.with(|slot| {
        slot.get().is_some_and(|cancel| {
            // SAFETY: the slot is only written by `with_path_resolution_cancel`,
            // which keeps its restore guard private and holds it on its own
            // stack frame for the duration of `run`, while `cancel` is borrowed
            // by that call. The guard cannot be forgotten or dropped out of
            // order, so call-stack nesting restores the previous pointer before
            // any borrow ends, and the slot only holds pointers whose borrows
            // are live. The slot is thread-local, so only the installing
            // thread reads it.
            unsafe { cancel.as_ref() }.load(Ordering::Relaxed)
        })
    })
}

pub(super) fn path_resolution_directory_listing(path: &Path) -> PathResolutionDirectoryListing {
    let cached = PATH_RESOLUTION_DIRECTORY_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .and_then(|cache| cache.get(path))
            .cloned()
    });
    if let Some(cached) = cached {
        return cached;
    }

    let listing = match read_dir_for_path_resolution(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            PathResolutionDirectoryListing::Missing
        }
        Err(error) => PathResolutionDirectoryListing::Error(error.to_string()),
        Ok(directory) => {
            let mut entries = PathResolutionDirectoryEntries::default();
            let mut error = None;
            for entry in directory {
                if entries
                    .names
                    .len()
                    .is_multiple_of(PATH_RESOLUTION_CANCEL_CHECK_INTERVAL)
                    && path_resolution_cancelled()
                {
                    return PathResolutionDirectoryListing::Cancelled;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(entry_error) => {
                        error = Some(entry_error.to_string());
                        break;
                    }
                };
                let name = entry.file_name();
                let index = entries.names.len();
                entries
                    .case_insensitive_names
                    .entry(name.to_string_lossy().to_ascii_lowercase())
                    .or_default()
                    .push(index);
                entries.names.push(name);
            }
            match error {
                Some(error) => PathResolutionDirectoryListing::Error(error),
                None => PathResolutionDirectoryListing::Entries(Arc::new(entries)),
            }
        }
    };
    PATH_RESOLUTION_DIRECTORY_CACHE.with(|cache| {
        if let Some(cache) = cache.borrow_mut().as_mut() {
            cache.insert(path.to_path_buf(), listing.clone());
        }
    });
    listing
}

#[cfg(test)]
thread_local! {
    static TEST_PATH_RESOLUTION_READ_DIR_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static TEST_USE_LEGACY_PATH_RESOLUTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn read_dir_for_path_resolution(path: &Path) -> io::Result<fs::ReadDir> {
    #[cfg(test)]
    TEST_PATH_RESOLUTION_READ_DIR_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
    fs::read_dir(path)
}

#[cfg(test)]
pub(super) fn with_path_resolution_read_dir_count<T>(run: impl FnOnce() -> T) -> (T, usize) {
    TEST_PATH_RESOLUTION_READ_DIR_CALLS.with(|calls| calls.set(0));
    let result = run();
    let count = TEST_PATH_RESOLUTION_READ_DIR_CALLS.with(std::cell::Cell::get);
    (result, count)
}

#[cfg(test)]
struct LegacyPathResolutionGuard {
    previous: bool,
}

#[cfg(test)]
impl LegacyPathResolutionGuard {
    fn new() -> Self {
        Self {
            previous: TEST_USE_LEGACY_PATH_RESOLUTION.with(|legacy| legacy.replace(true)),
        }
    }
}

#[cfg(test)]
impl Drop for LegacyPathResolutionGuard {
    fn drop(&mut self) {
        TEST_USE_LEGACY_PATH_RESOLUTION.with(|legacy| legacy.set(self.previous));
    }
}

#[cfg(test)]
fn with_legacy_path_resolution<T>(run: impl FnOnce() -> T) -> T {
    let _guard = LegacyPathResolutionGuard::new();
    run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_resolution_stops_when_cancelled_before_walking() {
        let temp = tempfile::tempdir().expect("temporary directory");
        for index in 0..3000 {
            fs::write(temp.path().join(format!("file{index}.pas")), b"").expect("entry");
        }
        fs::write(temp.path().join("Target.pas"), b"").expect("unit file");
        let requested = temp.path().join("TARGET.PAS");
        let cancel = AtomicBool::new(true);
        let mut warnings = Vec::new();

        let (status, read_dirs) = with_path_resolution_read_dir_count(|| {
            with_path_resolution_cancel(Some(&cancel), || {
                resolve_existing_path_status(&requested, &mut warnings, "unit")
            })
        });

        assert!(matches!(status, ExistingPathStatus::Unresolvable));
        assert_eq!(read_dirs, 0, "a cancelled walk must not list directories");
        assert!(warnings.is_empty());
    }

    #[test]
    fn path_resolution_listing_stops_when_cancelled_and_is_not_cached() {
        let temp = tempfile::tempdir().expect("temporary directory");
        for index in 0..3000 {
            fs::write(temp.path().join(format!("file{index}.pas")), b"").expect("entry");
        }
        let cancel = AtomicBool::new(true);
        let _cache = PathResolutionCacheScope::new();

        with_path_resolution_cancel(Some(&cancel), || {
            assert!(matches!(
                path_resolution_directory_listing(temp.path()),
                PathResolutionDirectoryListing::Cancelled
            ));
        });

        match path_resolution_directory_listing(temp.path()) {
            PathResolutionDirectoryListing::Entries(entries) => {
                assert_eq!(entries.names.len(), 3000);
            }
            other => panic!("a cancelled listing must not be cached: {other:?}"),
        }
    }

    #[test]
    fn path_resolution_cancel_scope_nests_and_restores_the_previous_token() {
        let outer = AtomicBool::new(true);
        let inner = AtomicBool::new(false);
        assert!(!path_resolution_cancelled());
        with_path_resolution_cancel(Some(&outer), || {
            assert!(path_resolution_cancelled());
            with_path_resolution_cancel(Some(&inner), || {
                assert!(!path_resolution_cancelled());
            });
            assert!(path_resolution_cancelled());
            with_path_resolution_cancel(None, || {
                assert!(path_resolution_cancelled(), "no token keeps the outer one");
            });
        });
        assert!(!path_resolution_cancelled());
    }

    #[test]
    fn project_discovery_reuses_case_insensitive_directory_listings() {
        const PATHS: usize = 24;
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path();
        let shared_parent = root.join("Tools").join("Units");
        fs::create_dir_all(&shared_parent).expect("shared unit directory");
        let mut raw_paths = Vec::new();
        let mut expected_paths = Vec::new();
        for index in 0..PATHS {
            let directory = shared_parent.join(format!("Package{index:02}"));
            fs::create_dir(&directory).expect("package unit directory");
            raw_paths.push(format!("tools/units/Package{index:02}"));
            expected_paths.push(directory);
        }
        let main = root.join("App.dpr");
        fs::write(&main, "program App; begin end.").expect("main source");
        fs::write(
            root.join("App.dproj"),
            format!(
                "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_UnitSearchPath>{}</DCC_UnitSearchPath></PropertyGroup></Project>",
                raw_paths.join(";")
            ),
        )
        .expect("project descriptor");

        let (context, read_dir_calls) = with_path_resolution_read_dir_count(|| {
            ProjectContext::discover_with_overrides(
                &main,
                &[root.to_path_buf()],
                &ProjectOptions::default(),
                &OverrideSession::new(None),
            )
        });
        let context = context.expect("discover project");

        for expected in expected_paths {
            assert!(
                context.context.search_paths().contains(&expected),
                "case-insensitive discovery should retain on-disk spelling for {}",
                expected.display()
            );
        }
        // The walk lists the filesystem root, each directory down to `root`,
        // then Tools and Units.
        let root_listings = 1 + root
            .components()
            .filter(|component| matches!(component, std::path::Component::Normal(_)))
            .count();
        assert_eq!(
            read_dir_calls,
            root_listings + 2,
            "shared case-insensitive prefixes should be listed once"
        );
    }

    #[test]
    fn project_discovery_does_not_reuse_path_listings_across_calls() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path();
        let main = root.join("App.dpr");
        fs::write(&main, "program App; begin end.").expect("main source");
        fs::write(
            root.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_UnitSearchPath>tools/units</DCC_UnitSearchPath></PropertyGroup></Project>",
        )
        .expect("project descriptor");
        let options = ProjectOptions::default();
        let overrides = OverrideSession::new(None);
        let requested = root.join("tools").join("units");
        let expected = root.join("Tools").join("Units");

        let first = ProjectContext::discover_with_overrides(
            &main,
            &[root.to_path_buf()],
            &options,
            &overrides,
        )
        .expect("first discovery")
        .context;
        assert!(first.search_paths().contains(&requested));
        fs::create_dir_all(&expected).expect("create directory between discovery calls");

        let second = ProjectContext::discover_with_overrides(
            &main,
            &[root.to_path_buf()],
            &options,
            &overrides,
        )
        .expect("second discovery")
        .context;

        assert!(
            second.search_paths().contains(&expected),
            "a later discovery must observe newly-created entries and their on-disk spelling"
        );
    }

    #[test]
    #[ignore = "requires the read-only local multidev Delphi repository"]
    fn benchmark_real_multidev_project_discovery_path_resolution() {
        let repository = std::path::Path::new("/home/a.simard@multidev.local/gits/multidev");
        let candidates = [
            repository.join("Projects/Tools/WebQueryExporter/WebQuery.dproj"),
            repository.join("Branches/Chaindrive Versions/5.7.07/Chaindrive/cd2000Light.dproj"),
            repository.join("Common/AES_Codec.dproj"),
        ];
        let projects = candidates
            .into_iter()
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        assert!(
            !projects.is_empty(),
            "no benchmark project files are available"
        );

        for project in projects {
            let options = ProjectOptions {
                project_file: Some(project.clone()),
                ..ProjectOptions::default()
            };
            let ((baseline_result, baseline_elapsed), baseline_reads) =
                with_path_resolution_read_dir_count(|| {
                    with_legacy_path_resolution(|| {
                        let start = std::time::Instant::now();
                        let result = ProjectContext::discover_with_overrides(
                            &project,
                            &[repository.to_path_buf()],
                            &options,
                            &OverrideSession::new(None),
                        );
                        (result, start.elapsed())
                    })
                });
            baseline_result.expect("legacy discovery should complete");
            println!(
                "baseline {}: {:?}, {} path-resolution read_dir calls",
                project.display(),
                baseline_elapsed,
                baseline_reads
            );

            let ((optimized_result, optimized_elapsed), optimized_reads) =
                with_path_resolution_read_dir_count(|| {
                    let start = std::time::Instant::now();
                    let result = ProjectContext::discover_with_overrides(
                        &project,
                        &[repository.to_path_buf()],
                        &options,
                        &OverrideSession::new(None),
                    );
                    (result, start.elapsed())
                });
            optimized_result.expect("optimized discovery should complete");
            println!(
                "optimized {}: {:?}, {} path-resolution read_dir calls",
                project.display(),
                optimized_elapsed,
                optimized_reads
            );
        }
    }
}

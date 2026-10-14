use super::authorization::extension_is;
use super::discovery_cache::{CachedProjectContext, check_project_scan_cancel};
use super::selection::is_deleted_path;
use super::*;

impl ReadPolicy {
    pub(super) fn read_payload_with_observation(
        &self,
        entry: &ProjectPathEntry,
        limit: u64,
    ) -> Result<(String, MetadataObservation), String> {
        let stamp = path_stamp_result(&entry.path).ok().flatten();
        let bytes = self.read_payload_bytes(entry, limit)?;
        let content_hash = content_hash_bytes(&bytes);
        let observation = MetadataObservation::Payload {
            path: entry.path.clone(),
            read_policy: self.clone(),
            path_entry: entry.clone(),
            stamp,
            content_hash,
        };
        let contents =
            String::from_utf8(bytes).map_err(|error| format!("file is not UTF-8: {error}"))?;
        Ok((contents, observation))
    }

    pub(super) fn read_pascal_payload_with_observation(
        &self,
        entry: &ProjectPathEntry,
        limit: u64,
    ) -> Result<(String, MetadataObservation), String> {
        let stamp = path_stamp_result(&entry.path).ok().flatten();
        let bytes = self.read_payload_bytes(entry, limit)?;
        let observation = MetadataObservation::Payload {
            path: entry.path.clone(),
            read_policy: self.clone(),
            path_entry: entry.clone(),
            stamp,
            content_hash: content_hash_bytes(&bytes),
        };
        Ok((decode_source_payload(&bytes), observation))
    }

    /// Read an authorized payload with strict symlink-free semantics.
    ///
    /// This is the normal payload API: configured and mapped entries, as well
    /// as legacy entries passed through this method, must be regular files
    /// without symlink components. Use [`Self::read_legacy_payload_bytes`]
    /// only for the deliberate legacy route compatibility described there.
    pub fn read_payload_bytes(
        &self,
        entry: &ProjectPathEntry,
        limit: u64,
    ) -> Result<Vec<u8>, String> {
        if !self.allows_entry(entry) {
            return Err("payload path is not authorized".to_string());
        }
        self.read_payload_bytes_after_authorization(entry, limit, false)
    }

    pub fn allows_legacy_payload_entry(&self, entry: &ProjectPathEntry) -> bool {
        self.allows_legacy_route_entry(entry)
            && fs::metadata(&entry.path).is_ok_and(|metadata| metadata.is_file())
    }

    pub fn allows_legacy_route_entry(&self, entry: &ProjectPathEntry) -> bool {
        matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
            && !self.is_excluded(&entry.path)
    }

    /// Read a legacy-native payload using the deliberate legacy symlink
    /// semantics required by older include and sibling lookup routes.
    ///
    /// Unlike [`Self::read_payload_bytes`], this permits a legacy-native
    /// route whose path contains symlink components when the resolved target
    /// is a regular file. It remains limited to `LegacyNative` provenance and
    /// exclusions still apply. Configured and mapped entries must use the
    /// strict payload API instead.
    pub fn read_legacy_payload_bytes(
        &self,
        entry: &ProjectPathEntry,
        limit: u64,
    ) -> Result<Vec<u8>, String> {
        if !self.allows_legacy_payload_entry(entry) {
            return Err("payload path is not authorized".to_string());
        }
        self.read_payload_bytes_after_authorization(entry, limit, true)
    }

    fn read_payload_bytes_after_authorization(
        &self,
        entry: &ProjectPathEntry,
        limit: u64,
        allow_legacy_symlink: bool,
    ) -> Result<Vec<u8>, String> {
        analysis_metrics::record(analysis_metrics::Metric::AuthorizedPayloadReadAttempts, 1);
        let result =
            self.read_authorized_payload_counting_bytes(entry, limit, allow_legacy_symlink);
        if result.is_err() {
            analysis_metrics::record(analysis_metrics::Metric::AuthorizedPayloadReadFailures, 1);
        }
        result
    }

    /// Bytes are counted right after `read_to_end`, so a partial or
    /// over-limit prefix is included. Other raw reads (for example content
    /// probes in the LSP project cache) are outside this counter.
    fn read_authorized_payload_counting_bytes(
        &self,
        entry: &ProjectPathEntry,
        limit: u64,
        allow_legacy_symlink: bool,
    ) -> Result<Vec<u8>, String> {
        let metadata = fs::symlink_metadata(&entry.path)
            .map_err(|error| format!("could not inspect file: {error}"))?;
        if metadata.file_type().is_symlink() && !allow_legacy_symlink {
            return Err("path is not a regular file".to_string());
        }
        if !metadata.file_type().is_symlink() && !metadata.is_file() {
            return Err("path is not a regular file".to_string());
        }
        let file = open_payload_file(&entry.path)
            .map_err(|error| format!("could not open file: {error}"))?;
        let opened_metadata = file
            .metadata()
            .map_err(|error| format!("could not stat opened file: {error}"))?;
        if !opened_metadata.is_file() {
            return Err("opened path is not a regular file".to_string());
        }
        let mut bytes = Vec::new();
        let read_result = file.take(limit.saturating_add(1)).read_to_end(&mut bytes);
        analysis_metrics::record(
            analysis_metrics::Metric::AuthorizedPayloadBytes,
            u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        );
        read_result.map_err(|error| format!("could not read file: {error}"))?;
        if bytes.len() as u64 > limit {
            return Err(format!("file exceeds the {limit} byte safety limit"));
        }
        Ok(bytes)
    }
}
/// A metadata path observed during evaluation.  Stat-only observations are
/// retained for freshness and precedence, but never authorize a later
/// payload read.  Payload observations carry the exact policy and path entry
/// that authorized the original read.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MetadataObservation {
    Stat {
        path: PathBuf,
    },
    Payload {
        path: PathBuf,
        read_policy: ReadPolicy,
        path_entry: ProjectPathEntry,
        stamp: Option<ProjectReadStamp>,
        content_hash: u64,
    },
}

/// The result of looking for `System.pas` in one ordered compiler search
/// directory. A failed lookup remains distinguishable from a complete miss so
/// freshness checks do not treat an unreadable directory as proof of absence.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SystemPasSearchObservation {
    pub directory: PathBuf,
    pub selected_path: Option<PathBuf>,
    pub complete: bool,
}

impl MetadataObservation {
    pub fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        match self {
            Self::Stat { path } => visit(path.as_os_str().len()),
            Self::Payload {
                path,
                read_policy,
                path_entry,
                ..
            } => {
                visit(path.as_os_str().len())?;
                read_policy.visit_recovery_payload(visit)?;
                path_entry.visit_recovery_payload(visit)
            }
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Stat { path } | Self::Payload { path, .. } => path,
        }
    }
}
fn open_payload_file(path: &Path) -> io::Result<fs::File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    // Linux's UAPI O_NONBLOCK prevents a replaced FIFO from blocking between
    // the stat-only check and the actual open.
    const O_NONBLOCK: i32 = 0o4000;
    OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
}

#[cfg(not(target_os = "linux"))]
fn open_payload_file(path: &Path) -> io::Result<fs::File> {
    fs::File::open(path)
}
/// One file observation captured at the read which supplied bytes to project
/// discovery. Consumers use this instead of reopening the file after parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectReadObservation {
    pub path: PathBuf,
    pub stamp: ProjectReadStamp,
    pub content_hash: u64,
    pub content_bytes: Option<Vec<u8>>,
}

impl ProjectReadObservation {
    pub fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        visit(self.path.as_os_str().len())?;
        if let Some(bytes) = &self.content_bytes {
            visit(bytes.len())?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectReadStamp {
    pub bytes: u64,
    pub modified: Option<SystemTime>,
    pub is_dir: bool,
    pub is_symlink: bool,
}

#[derive(Debug, Clone)]
pub struct ProjectDiscovery {
    pub context: Arc<ProjectContext>,
    /// Observations made while constructing `context`.
    pub context_observations: ProjectObservations,
    pub observations: Vec<ProjectReadObservation>,
    pub candidate_memberships: HashMap<PathBuf, Result<ProjectCandidateMembership, String>>,
    /// Runtime project selection resolved while evaluating this source path.
    pub project_selection: Option<(PathBuf, PathBuf)>,
    /// Nearest project-selection scope observed during discovery.
    pub project_scope: Option<PathBuf>,
}

/// Work/cancellation accounting supplied by a caller such as the LSP's
/// serialized notification worker. Project discovery uses it before each
/// directory-entry probe and metadata read, so exhaustion can stop a scan
/// before more filesystem work is started.
pub trait ProjectWorkBudget {
    fn check_cancelled(&self) -> Result<(), String>;
    fn charge_path_visits(&self, amount: usize) -> Result<(), String>;
    /// Account for bytes materialized while building path identities or sort keys.
    /// Implementations that only expose path-visit accounting can conservatively
    /// treat each byte as one unit of path work.
    fn charge_path_bytes(&self, amount: usize) -> Result<(), String> {
        self.charge_path_visits(amount)
    }
    fn ensure_file_read_fits(&self, max_bytes: usize) -> Result<(), String>;
    fn charge_file_bytes(&self, amount: usize) -> Result<(), String>;

    /// Whether an error means a read was interrupted by caller-owned transient
    /// work limits rather than a stable filesystem/configuration result.
    fn is_transient_error(&self, _error: &str) -> bool {
        false
    }
}

#[derive(Default)]
pub(super) struct ProjectReadTracker<'a> {
    pub(super) observations: Vec<ProjectReadObservation>,
    pub(super) metadata_observations: Vec<MetadataObservation>,
    pub(super) candidate_memberships: HashMap<PathBuf, Result<ProjectCandidateMembership, String>>,
    pub(super) metadata_paths: Vec<PathBuf>,
    pub(super) work_budget: Option<&'a dyn ProjectWorkBudget>,
    pub(super) deleted_paths: &'a [PathBuf],
}

impl<'a> ProjectReadTracker<'a> {
    pub(super) fn with_budget(
        work_budget: Option<&'a dyn ProjectWorkBudget>,
        deleted_paths: &'a [PathBuf],
    ) -> Self {
        Self {
            work_budget,
            deleted_paths,
            ..Self::default()
        }
    }

    pub(super) fn record(&mut self, path: &Path, stamp: ProjectReadStamp, bytes: &[u8]) {
        if self
            .observations
            .iter()
            .any(|observation| project_paths_equal(&observation.path, path))
        {
            return;
        }
        self.observations.push(ProjectReadObservation {
            path: path.to_path_buf(),
            stamp,
            content_hash: project_content_hash(bytes),
            content_bytes: (!is_pascal_source_path(path)).then(|| bytes.to_vec()),
        });
    }

    pub(super) fn record_metadata_path(&mut self, path: PathBuf) {
        if !self
            .metadata_paths
            .iter()
            .any(|existing| project_paths_equal(existing, &path))
        {
            self.metadata_paths.push(path);
        }
    }

    pub(super) fn record_metadata_observation(&mut self, observation: MetadataObservation) {
        add_metadata_observation(&mut self.metadata_observations, observation);
    }

    pub(super) fn record_candidate_membership(
        &mut self,
        path: PathBuf,
        membership: Result<ProjectCandidateMembership, String>,
    ) {
        if self
            .candidate_memberships
            .keys()
            .any(|existing| project_paths_equal(existing, &path))
        {
            return;
        }
        self.candidate_memberships.insert(path, membership);
    }

    pub(super) fn merge_project_context_reads(
        &mut self,
        cached: &CachedProjectContext,
        cancel: Option<&AtomicBool>,
        work_budget: Option<&dyn ProjectWorkBudget>,
    ) -> Result<(), String> {
        for path in &cached.metadata_paths {
            check_project_scan_cancel(cancel)?;
            if let Some(work_budget) = work_budget {
                work_budget.check_cancelled()?;
            }
            self.record_metadata_path(path.clone());
        }
        for observation in &cached.metadata_observations {
            check_project_scan_cancel(cancel)?;
            if let Some(work_budget) = work_budget {
                work_budget.check_cancelled()?;
            }
            self.record_metadata_observation(observation.clone());
        }
        for observation in &cached.observations {
            check_project_scan_cancel(cancel)?;
            if let Some(work_budget) = work_budget {
                work_budget.check_cancelled()?;
            }
            self.record_project_read_observation(observation.clone());
        }
        Ok(())
    }

    fn record_project_read_observation(&mut self, observation: ProjectReadObservation) {
        if self
            .observations
            .iter()
            .any(|existing| project_paths_equal(&existing.path, &observation.path))
        {
            return;
        }
        self.observations.push(observation);
    }

    pub(super) fn into_discovery(
        self,
        mut project: DiscoveredProject,
        project_selection: Option<(PathBuf, PathBuf)>,
    ) -> ProjectDiscovery {
        let project_scope = project
            .context
            .project_file
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| project_selection.as_ref().map(|(scope, _)| scope.clone()))
            .or_else(|| {
                self.candidate_memberships
                    .iter()
                    .filter_map(|(directory, membership)| {
                        let membership = membership.as_ref().ok()?;
                        membership
                            .paths
                            .iter()
                            .any(|path| extension_is(path, "dproj"))
                            .then(|| directory.clone())
                    })
                    .max_by_key(|directory| directory.components().count())
            });
        for path in &self.metadata_paths {
            if !project
                .observations
                .metadata_files
                .iter()
                .any(|existing| project_paths_equal(existing, path))
            {
                project.observations.metadata_files.push(path.clone());
            }
        }
        for observation in self.metadata_observations {
            add_metadata_observation(&mut project.observations.metadata_observations, observation);
        }
        ProjectDiscovery {
            context: project.context,
            context_observations: project.observations,
            observations: self.observations,
            candidate_memberships: self.candidate_memberships,
            project_selection,
            project_scope,
        }
    }
}
#[cfg(any(test, feature = "test-support"))]
type ProjectReadHook = Box<dyn FnOnce(&Path)>;

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEST_AFTER_PROJECT_READ:
        std::cell::RefCell<Option<ProjectReadHook>> = std::cell::RefCell::new(None);
    static TEST_AFTER_PROJECT_READ_AT:
        std::cell::RefCell<Option<(PathBuf, ProjectReadHook)>> = std::cell::RefCell::new(None);
    static TEST_BEFORE_PROJECT_READ_AT:
        std::cell::RefCell<Option<(PathBuf, ProjectReadHook)>> = std::cell::RefCell::new(None);
}
#[cfg(any(test, feature = "test-support"))]
pub struct TestAfterProjectReadGuard(Option<ProjectReadHook>);

#[cfg(any(test, feature = "test-support"))]
pub fn test_after_project_read(hook: impl FnOnce(&Path) + 'static) -> TestAfterProjectReadGuard {
    let previous = TEST_AFTER_PROJECT_READ.with(|slot| slot.borrow_mut().replace(Box::new(hook)));
    TestAfterProjectReadGuard(previous)
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestAfterProjectReadGuard {
    fn drop(&mut self) {
        TEST_AFTER_PROJECT_READ.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

#[cfg(any(test, feature = "test-support"))]
pub struct TestAfterProjectReadAtGuard(Option<(PathBuf, ProjectReadHook)>);

#[cfg(any(test, feature = "test-support"))]
pub struct TestBeforeProjectReadAtGuard(Option<(PathBuf, ProjectReadHook)>);

#[cfg(any(test, feature = "test-support"))]
pub fn test_before_project_read_at(
    path: PathBuf,
    hook: impl FnOnce(&Path) + 'static,
) -> TestBeforeProjectReadAtGuard {
    let previous =
        TEST_BEFORE_PROJECT_READ_AT.with(|slot| slot.borrow_mut().replace((path, Box::new(hook))));
    TestBeforeProjectReadAtGuard(previous)
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestBeforeProjectReadAtGuard {
    fn drop(&mut self) {
        TEST_BEFORE_PROJECT_READ_AT.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_after_project_read_at(
    path: PathBuf,
    hook: impl FnOnce(&Path) + 'static,
) -> TestAfterProjectReadAtGuard {
    let previous =
        TEST_AFTER_PROJECT_READ_AT.with(|slot| slot.borrow_mut().replace((path, Box::new(hook))));
    TestAfterProjectReadAtGuard(previous)
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestAfterProjectReadAtGuard {
    fn drop(&mut self) {
        TEST_AFTER_PROJECT_READ_AT.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

#[cfg(any(test, feature = "test-support"))]
fn run_after_project_read(path: &Path) {
    let hook = TEST_AFTER_PROJECT_READ.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook(path);
    }
    let targeted_hook = TEST_AFTER_PROJECT_READ_AT.with(|slot| {
        let matches = slot
            .borrow()
            .as_ref()
            .is_some_and(|(target, _)| target == path);
        matches.then(|| slot.borrow_mut().take()).flatten()
    });
    if let Some((_, hook)) = targeted_hook {
        hook(path);
    }
}

#[cfg(any(test, feature = "test-support"))]
fn run_before_project_read(path: &Path) {
    let targeted_hook = TEST_BEFORE_PROJECT_READ_AT.with(|slot| {
        let matches = slot
            .borrow()
            .as_ref()
            .is_some_and(|(target, _)| target == path);
        matches.then(|| slot.borrow_mut().take()).flatten()
    });
    if let Some((_, hook)) = targeted_hook {
        hook(path);
    }
}

#[cfg(not(any(test, feature = "test-support")))]
fn run_after_project_read(_path: &Path) {}

#[cfg(not(any(test, feature = "test-support")))]
fn run_before_project_read(_path: &Path) {}
#[derive(Debug)]
#[allow(dead_code)]
struct BoundedRead {
    stamp: ProjectReadStamp,
    bytes: Vec<u8>,
}

#[allow(dead_code)]
fn read_bounded_with_tracker(
    path: &Path,
    limit: u64,
    tracker: &mut ProjectReadTracker<'_>,
) -> Result<String, String> {
    if is_deleted_path(path, tracker.deleted_paths) {
        return Err(format!(
            "metadata path is tombstoned by a delete notification: {}",
            path.display()
        ));
    }
    if let Some(budget) = tracker.work_budget {
        budget.check_cancelled()?;
        budget.charge_path_visits(1)?;
    }
    let stamp = project_read_stamp(path)?;
    if stamp.bytes > limit {
        return Err(format!(
            "file is {} bytes, exceeding the {} byte safety limit",
            stamp.bytes, limit
        ));
    }
    if let Some(budget) = tracker.work_budget {
        let estimate = usize::try_from(stamp.bytes)
            .map_err(|_| "metadata file size does not fit the work budget".to_string())?;
        budget.ensure_file_read_fits(estimate.saturating_add(1))?;
        budget.check_cancelled()?;
    }
    run_before_project_read(path);
    let bytes = read_file_limited(path, stamp.bytes, tracker.work_budget)?;
    run_after_project_read(path);
    if let Some(budget) = tracker.work_budget {
        budget.check_cancelled()?;
    }
    let read = BoundedRead { stamp, bytes };
    let text =
        String::from_utf8(read.bytes).map_err(|error| format!("file is not UTF-8: {error}"))?;
    tracker.record(path, read.stamp, text.as_bytes());
    Ok(text)
}

#[allow(dead_code)]
fn read_bounded_bytes(path: &Path, limit: u64) -> Result<BoundedRead, String> {
    let stamp = project_read_stamp(path)?;
    if stamp.bytes > limit {
        return Err(format!(
            "file is {} bytes, exceeding the {} byte safety limit",
            stamp.bytes, limit
        ));
    }
    let bytes = read_file_limited(path, limit, None)?;
    run_after_project_read(path);
    Ok(BoundedRead { stamp, bytes })
}

fn read_file_limited(
    path: &Path,
    limit: u64,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<Vec<u8>, String> {
    let file = fs::File::open(path).map_err(|error| format!("could not read file: {error}"))?;
    if !file
        .metadata()
        .map_err(|error| format!("could not inspect opened file: {error}"))?
        .is_file()
    {
        return Err("opened metadata path is not a regular file".to_string());
    }
    let mut reader = file.take(limit.saturating_add(1));
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        if let Some(work_budget) = work_budget {
            work_budget.check_cancelled()?;
        }
        let read = reader
            .read(&mut chunk)
            .map_err(|error| format!("could not read file: {error}"))?;
        if read == 0 {
            break;
        }
        if let Some(work_budget) = work_budget {
            work_budget.charge_file_bytes(read)?;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(format!(
            "metadata file grew beyond the {limit} byte read limit"
        ));
    }
    Ok(bytes)
}

pub(super) fn read_payload_with_tracker(
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
    limit: u64,
    tracker: &mut ProjectReadTracker<'_>,
) -> Result<(String, MetadataObservation), String> {
    if is_deleted_path(&entry.path, tracker.deleted_paths) {
        return Err(format!(
            "metadata path is tombstoned by a delete notification: {}",
            entry.path.display()
        ));
    }
    if let Some(budget) = tracker.work_budget {
        budget.check_cancelled()?;
        budget.charge_path_visits(1)?;
    }
    if !read_policy.allows_entry(entry) {
        return Err("payload path is not authorized".to_string());
    }
    if let Some(budget) = tracker.work_budget {
        budget.check_cancelled()?;
        budget.charge_path_visits(1)?;
    }
    let stamp = project_read_stamp(&entry.path)?;
    let read_limit = stamp.bytes.min(limit);
    if let Some(budget) = tracker.work_budget {
        let estimate = usize::try_from(read_limit)
            .map_err(|_| "metadata file size does not fit the work budget".to_string())?;
        // The authorized stream uses `take(read_limit + 1)` to detect a
        // same-path growth race. Reserve that probe byte before opening the
        // stream so a concurrent replacement cannot exceed the shared ceiling.
        budget.ensure_file_read_fits(estimate.saturating_add(1))?;
        budget.check_cancelled()?;
    }
    let bytes = match read_policy.read_payload_bytes(entry, read_limit) {
        Ok(bytes) => bytes,
        Err(error) => {
            if let Some(budget) = tracker.work_budget {
                // A failed over-limit stream may have consumed the full
                // `limit + 1` probe. Debit the reserved maximum rather than
                // losing those bytes from the cumulative account.
                let reserved = usize::try_from(read_limit)
                    .unwrap_or(usize::MAX)
                    .saturating_add(1);
                budget.charge_file_bytes(reserved)?;
            }
            return Err(error);
        }
    };
    run_after_project_read(&entry.path);
    if let Some(budget) = tracker.work_budget {
        budget.check_cancelled()?;
        budget.charge_file_bytes(bytes.len())?;
        budget.charge_path_visits(1)?;
    }
    tracker.record(&entry.path, stamp, &bytes);
    let observation = MetadataObservation::Payload {
        path: entry.path.clone(),
        read_policy: read_policy.clone(),
        path_entry: entry.clone(),
        stamp: path_stamp_result(&entry.path).ok().flatten(),
        content_hash: project_content_hash(&bytes),
    };
    // Source payloads use the same lossless UTF-8/Latin-1 boundary as the
    // resolver.  Project discovery still records the original bytes and hash
    // above; this decoded string is only the parser coordinate representation.
    let contents = decode_source_payload(&bytes);
    Ok((contents, observation))
}

fn decode_source_payload(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => bytes.iter().map(|&byte| byte as char).collect(),
    }
}

pub(super) fn project_read_stamp(path: &Path) -> Result<ProjectReadStamp, String> {
    let link_metadata =
        fs::symlink_metadata(path).map_err(|error| format!("could not stat file: {error}"))?;
    let is_symlink = link_metadata.file_type().is_symlink();
    let metadata = if is_symlink {
        fs::metadata(path).map_err(|error| format!("could not stat file: {error}"))?
    } else {
        link_metadata
    };
    Ok(ProjectReadStamp {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
        is_dir: metadata.is_dir(),
        is_symlink,
    })
}

fn project_content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(bytes);
    hasher.write_usize(bytes.len());
    hasher.finish()
}

pub fn content_hash_bytes(bytes: &[u8]) -> u64 {
    analysis_metrics::record(
        analysis_metrics::Metric::LegacyHashBytes,
        u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    );
    project_content_hash(bytes)
}

/// Re-read one previously authorized metadata payload and verify the exact
/// bytes that discovery consumed. File metadata alone is not a freshness proof
/// because content can change while size and modification time are restored.
pub fn metadata_payload_is_current(
    observation: &MetadataObservation,
    read_limit: u64,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<bool, String> {
    let MetadataObservation::Payload {
        path,
        read_policy,
        path_entry,
        content_hash,
        ..
    } = observation
    else {
        return Ok(true);
    };
    if !project_paths_equal(path, &path_entry.path) {
        return Ok(false);
    }
    let mut tracker = ProjectReadTracker::with_budget(work_budget, &[]);
    match read_payload_with_tracker(read_policy, path_entry, read_limit, &mut tracker) {
        Ok(_) => Ok(tracker
            .observations
            .iter()
            .find(|read| project_paths_equal(&read.path, path))
            .is_some_and(|read| read.content_hash == *content_hash)),
        Err(error)
            if error == "request cancelled"
                || work_budget.is_some_and(|budget| budget.is_transient_error(&error)) =>
        {
            Err(error)
        }
        Err(_) => Ok(false),
    }
}

pub fn path_stamp_result(path: &Path) -> io::Result<Option<ProjectReadStamp>> {
    analysis_metrics::record(analysis_metrics::Metric::ProjectStampProbes, 1);
    let link_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let is_symlink = link_metadata.file_type().is_symlink();
    let metadata = if is_symlink {
        match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => link_metadata,
            Err(error) => return Err(error),
        }
    } else {
        link_metadata
    };
    Ok(Some(ProjectReadStamp {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
        is_dir: metadata.is_dir(),
        is_symlink,
    }))
}

pub(super) fn is_pascal_source_path(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("pas")
            || extension.eq_ignore_ascii_case("dpr")
            || extension.eq_ignore_ascii_case("dpk")
    })
}

#[allow(dead_code)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_read_tracker_preserves_the_first_observation() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("App.dproj");
        let first_stamp = ProjectReadStamp {
            bytes: 5,
            modified: None,
            is_dir: false,
            is_symlink: false,
        };
        let second_stamp = ProjectReadStamp {
            bytes: 6,
            modified: None,
            is_dir: false,
            is_symlink: false,
        };
        let mut tracker = ProjectReadTracker::default();
        tracker.record(&path, first_stamp.clone(), b"first");
        tracker.record(&path, second_stamp, b"second");

        let discovery = tracker.into_discovery(DiscoveredProject::default(), None);
        assert_eq!(discovery.observations.len(), 1);
        assert_eq!(discovery.observations[0].stamp, first_stamp);
        assert_eq!(
            discovery.observations[0].content_bytes,
            Some(b"first".to_vec())
        );
    }

    #[test]
    fn project_metadata_reader_rejects_growth_after_pre_read_stamp() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("App.dproj");
        fs::write(&path, b"small").expect("initial descriptor");
        // Stay below the per-file limit so this specifically proves that the
        // pre-read budget estimate, rather than only the coarse file ceiling,
        // bounds bytes consumed after a same-path growth race.
        let replacement = b"x".repeat(20);
        let replacement_for_hook = replacement.clone();
        let _hook = test_before_project_read_at(path.clone(), move |path| {
            fs::write(path, replacement_for_hook).expect("grow descriptor after stamp");
        });
        let mut tracker = ProjectReadTracker::default();

        let error = read_bounded_with_tracker(&path, 32, &mut tracker)
            .expect_err("metadata growth between stamp and read must not bypass the byte cap");

        assert!(
            error.contains("exceed") || error.contains("limit"),
            "unexpected bounded-read error: {error}"
        );
    }

    #[test]
    fn project_metadata_read_reserves_the_growth_probe_byte_before_streaming() {
        use std::cell::Cell;

        struct RecordingBudget {
            ensured: Cell<usize>,
            charged: Cell<usize>,
        }

        impl crate::ProjectWorkBudget for RecordingBudget {
            fn check_cancelled(&self) -> Result<(), String> {
                Ok(())
            }

            fn charge_path_visits(&self, _amount: usize) -> Result<(), String> {
                Ok(())
            }

            fn ensure_file_read_fits(&self, max_bytes: usize) -> Result<(), String> {
                self.ensured.set(max_bytes);
                Ok(())
            }

            fn charge_file_bytes(&self, amount: usize) -> Result<(), String> {
                self.charged.set(amount);
                Ok(())
            }
        }

        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join("App.dproj");
        fs::write(&path, b"small").expect("descriptor");
        let replacement = b"x".repeat(20);
        let replacement_for_hook = replacement.clone();
        let _hook = test_before_project_read_at(path.clone(), move |path| {
            fs::write(path, replacement_for_hook).expect("grow descriptor after reservation");
        });
        let budget = RecordingBudget {
            ensured: Cell::new(0),
            charged: Cell::new(0),
        };
        let mut tracker = ProjectReadTracker::with_budget(Some(&budget), &[]);

        let error = read_bounded_with_tracker(&path, 32, &mut tracker)
            .expect_err("same-path growth must trip the bounded stream");
        assert!(error.contains("grew beyond"), "unexpected error: {error}");
        assert_eq!(
            budget.ensured.get(),
            6,
            "reserve stat size plus the probe byte"
        );
        assert_eq!(
            budget.charged.get(),
            6,
            "charge the consumed growth-probe byte"
        );
    }

    #[test]
    fn metadata_observation_keeps_the_first_payload_for_each_path() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path().to_path_buf();
        let metadata = root.join("App.dproj");
        fs::write(&metadata, "<Project />").expect("metadata");
        let policy = ReadPolicy::new(
            std::slice::from_ref(&root),
            &[],
            &[],
            &EffectiveOverrides::default(),
        );
        let entry = ProjectPathEntry {
            path: metadata.clone(),
            provenance: ProjectPathProvenance::Configured,
        };
        let first_stamp = path_stamp_result(&metadata).expect("first metadata stamp");
        let first = MetadataObservation::Payload {
            path: metadata.clone(),
            read_policy: policy.clone(),
            path_entry: entry.clone(),
            stamp: first_stamp.clone(),
            content_hash: 1,
        };
        let replacement = MetadataObservation::Payload {
            path: metadata.clone(),
            read_policy: policy.clone(),
            path_entry: entry.clone(),
            stamp: first_stamp.clone(),
            content_hash: 2,
        };
        let mut observations = vec![MetadataObservation::Stat {
            path: metadata.clone(),
        }];
        super::add_metadata_observation(&mut observations, first.clone());
        super::add_metadata_observation(&mut observations, replacement);

        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0], first);
    }
}
